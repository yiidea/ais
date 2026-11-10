//! `FreshnessOracle`（[06 §3.3]）：把索引水位暴露成**连续量**，供查询规划器选回退策略。
//!
//! 三件事，分别在三个入口：
//!
//! 1. **覆盖判定**（06 §3.1 / A1 / A3 / A4）：只读快照 `xmin` 与索引的 xid 水位，
//!    不读 LSN、不读版本号、不读任何时间戳。`lagging = !index.covers_xmin(S.xmin)`。
//! 2. **`tx_w` 的推进规则**（06 §3.1）：
//!    `tx_w = min{ xid : 已出现在 WAL 中但状态仍非终态 } − 1`。
//!    输入是「追赶区间里出现过的 xid」——那正是「已出现在 WAL 中」的可执行形式，
//!    而终态判定走 `TransactionManager::status`，不另建状态表（ADR-0012 决策 3）。
//! 3. **代价模型**（06 §3.2）：`n_lag` 必须**估**而不是猜，估计方法是先算 LSN 字节缺口，
//!    再按本进程实测的「字节 / 变更条数」比例折算。比例只能来自实测：没有样本时
//!    返回 `None`，调用方必须显式给出上界，而不是拿一个默认常数冒充测量值。
//!
//! **估计值不得用于任何正确性判定**（06 §3.2 第 2 条）。本模块里唯一进入正确性判定的
//! 是 `lagging`，它只由 `tx_w` 与 `S.xmin` 决定；`estimated_lag_objects` 只影响策略选择
//! 与结果标注里的 `missing_estimate`。
//!
//! [06 §3.3]: ../../docs/chatgpt/06-index-recovery.md

use super::manifest::SegmentManifest;
use crate::kernel::Snapshot;
use std::collections::BTreeSet;

/// 回退策略（06 §3.1 的 A / B / C）。
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FallbackStrategy {
    /// A. 补扫缺口：扫 `(index.watermark, 已提交水位]` 的变更，内存里补入。
    FullScan,
    /// B. 稀疏协助：用**语义等价**的兄弟索引取候选集再逐个检查。
    SparseAssist,
    /// C. 等待：不返回结果，等索引追上。
    Wait,
}

/// 索引的新鲜度（06 §3.3）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IndexFreshness {
    /// 索引消费到的 xid 水位（**唯一**的正确性输入）。
    pub txn_watermark: crate::types::TransactionId,
    /// 索引消费到的 LSN 水位（只用于运维滞后与段回收，不参与判定）。
    pub lsn_watermark: crate::types::Lsn,
    /// 快照是否超出该索引的覆盖范围：`!covers_xmin(S.xmin)`。
    pub lagging: bool,
    /// 估算的缺口对象数；`None` = 没有测量样本，调用方必须自己给上界。
    pub estimated_lag_objects: Option<u64>,
}

impl IndexFreshness {
    /// 清单的 xid / LSN 水位（与快照无关的那两个数）。
    ///
    /// 覆盖判定必须按快照做（A1），所以这里的 `lagging` 保守地报 `true`；要拿到
    /// 真实结论用 [`evaluate`](Self::evaluate)。
    pub fn of_manifest(m: &SegmentManifest, _object_watermark: crate::types::Lsn) -> Self {
        IndexFreshness {
            txn_watermark: m.txn_watermark,
            lsn_watermark: m.watermark,
            lagging: true,
            estimated_lag_objects: None,
        }
    }

    /// 完整的新鲜度：两个水位 + 按**快照**判定的覆盖 + 缺口估计（06 §3.3）。
    ///
    /// `lagging` 只由 [`covers`](Self::covers) 决定（只读 `xmin` 与 xid 水位，A1）；
    /// `rate` 与 `model` 只影响 `estimated_lag_objects`，不参与任何正确性判定。
    pub fn evaluate(
        m: &SegmentManifest,
        snap: &Snapshot,
        object_watermark: crate::types::Lsn,
        rate: &ChangeRate,
        model: &CostModel,
    ) -> Self {
        let lag_bytes = object_watermark.0.saturating_sub(m.watermark.0);
        IndexFreshness {
            txn_watermark: m.txn_watermark,
            lsn_watermark: m.watermark,
            lagging: !Self::covers(m, snap),
            estimated_lag_objects: rate.estimate(lag_bytes, model),
        }
    }

    /// 按快照重判覆盖（A1：只读 `xmin` 与 xid 水位）。
    pub fn covers_snapshot(&self, snap: &Snapshot) -> bool {
        !self.lagging && snap.xmin.0 <= self.txn_watermark.0
    }

    /// 覆盖判定与「清单是谁算出来的」无关：用清单直接判（06 §3.1）。
    pub fn covers(manifest: &SegmentManifest, snap: &Snapshot) -> bool {
        manifest.covers_xmin(snap.xmin)
    }
}

/// 回退代价（06 §3.3）：全部是**估算**，不是保证。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FallbackCost {
    pub strategy: FallbackStrategy,
    pub estimated_objects: u64,
    pub estimated_latency_ms: u64,
}

/// 代价模型系数（06 §3.2）。
///
/// 三个量都是**实测**的（07 §2.7 / §7.2 的 B7 是 `c_scan` / `c_point` 的唯一来源），
/// 因此没有 `Default`：用错一个系数只会让规划器选错策略，而选错策略不会报错。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CostModel {
    /// 单对象顺序扫描成本（微秒，含 payload 读取）。
    pub c_scan_nanos: u64,
    /// 单对象点查成本（微秒）。
    pub c_point_nanos: u64,
    /// 过滤表达式求值成本（微秒，按对象计）。
    pub filter_eval_nanos: u64,
    /// 估计的上界安全系数：`n_lag ≤ n_lag_estimate × safety_factor`（06 §3.2）。
    pub safety_factor_num: u32,
    pub safety_factor_den: u32,
}

impl CostModel {
    /// 上界安全系数（06 §3.2 的 `safety_factor`）。
    pub fn safety_factor(&self) -> f64 {
        if self.safety_factor_den == 0 {
            return 1.0;
        }
        self.safety_factor_num as f64 / self.safety_factor_den as f64
    }
}

/// 本进程实测的「WAL 字节 / 变更条数」比例，用于把 LSN 字节缺口折算成变更条数。
///
/// 06 §3.1 指出 `Lsn` 是**字节偏移**，而 §1.3 的「滞后 N 条」说的是**条数**——
/// 两者不是同一个量。内核的「每 LSN 区间变更数直方图」还没有落地，所以第一版用
/// 观测到的平均记录体积折算：`n_lag ≈ lag_bytes / bytes_per_change`。这是估计，
/// 不是上界（06 §3.2 第 1 条）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ChangeRate {
    pub observed_bytes: u64,
    pub observed_changes: u64,
}

impl ChangeRate {
    /// 把这批变更记进样本。字节量取「游标差」（LSN 是字节偏移），条数取变更条数。
    pub fn observe(&mut self, from: crate::types::Lsn, to: crate::types::Lsn, changes: usize) {
        let bytes = to.0.saturating_sub(from.0);
        if bytes == 0 || changes == 0 {
            return;
        }
        self.observed_bytes += bytes;
        self.observed_changes += changes as u64;
    }

    /// 每条变更的平均字节数；没有样本时 `None`（**不允许**拿默认常数代替）。
    pub fn bytes_per_change(&self) -> Option<u64> {
        if self.observed_changes == 0 {
            return None;
        }
        Some((self.observed_bytes / self.observed_changes).max(1))
    }

    /// 缺口字节数 → 估计变更条数，并乘上安全系数。
    pub fn estimate(&self, lag_bytes: u64, model: &CostModel) -> Option<u64> {
        let per = self.bytes_per_change()?;
        // 缺口不足一条也要估成 1：0 会让规划器以为「立刻追平」。
        let raw = lag_bytes.max(1).div_ceil(per);
        Some((raw as f64 * model.safety_factor()).ceil() as u64)
    }
}

/// `tx_w`：追赶区间里出现过的 xid 中，「非终态」的最小者减一；没有未决事务时
/// 取已见最大 xid（06 §3.1）。
///
/// `status` 是内核的事务状态判定（`TransactionManager::status`）。`InProgress`
/// （未知）按未决处理：**保守停住水位**比越过一个可能中途崩溃的事务安全。
///
/// 这是**下界**：不在区间里的更小 xid 若仍未决，真正的 `tx_w` 还会更低。内核状态表
/// 目前不提供「枚举未决事务」，因此 `covers_snapshot` 在它与实际未决集合一致时才是精确的。
pub fn next_txn_watermark<S>(
    seen: &BTreeSet<crate::types::TransactionId>,
    status: S,
) -> crate::types::TransactionId
where
    S: Fn(crate::types::TransactionId) -> crate::types::TxnStatus,
{
    let mut max_seen = crate::types::TransactionId(0);
    let mut lowest_pending: Option<crate::types::TransactionId> = None;
    for &xid in seen {
        // 系统事务（xid = 0）不是对象事务，与 `ChangeFeed` 的口径一致。
        if xid.0 == 0 {
            continue;
        }
        max_seen = max_seen.max(xid);
        let terminal = matches!(
            status(xid),
            crate::types::TxnStatus::Committed | crate::types::TxnStatus::Aborted
        );
        if !terminal && lowest_pending.map(|p| xid < p).unwrap_or(true) {
            lowest_pending = Some(xid);
        }
    }
    match lowest_pending {
        // `xid − 1`：水位表示「所有 ≤ tx_w 的 xid 都已判定」，屏障本身不算。
        Some(pending) => crate::types::TransactionId(pending.0.saturating_sub(1)),
        // 没有未决事务时 `tx_w` 等于**已见到**的最大 xid。`max_seen` 是对区间里实际
        // 出现的 xid 取的最大值：当区间里没有对象变更却有维护记录（检查点等）时它
        // 会停在旧值——保守，不会越过任何未判定事务。
        None => max_seen,
    }
}
