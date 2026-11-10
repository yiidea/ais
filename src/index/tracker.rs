//! 索引追赶循环：把 [`SegmentManifest`] 从当前水位推到目标 LSN，**先段后水位**。
//!
//! 每一步严格按 08 §2.3 的顺序：
//!
//! ```text
//! 1. catchup::step      拉一批已提交变更（撞上未决事务就停）
//! 2. build_segment      把这一批构建成段（幂等键由内容算出）
//! 3. manifest_log::persist 写 0x31（协调区快照：段已发布、水位还是旧值）
//! 4. record_manifest    写 0x31 —— 段证据（ADR-0013 决策 3）
//! 5. manifest.advance   票据在手才允许推进水位（CAS）；推进成功后再 persist 一次
//! ```
//!
//! 第 3 步记的是「段已发布、水位还没推进」的那一版清单，重启时据此还原：
//! 先段后水位在重启之后依然成立。
//!
//! 第 4 步必须在第 5 步之前，且第 5 步只接受 [`Advance`](super::manifest::Advance) 票据——「水位已推进但段
//! 缺失」在这段代码里没有写法（06 §6.1 A5）。CAS 失败（另一个追赶者先推进了水位）
//! 不是错误：本轮的段已经发布、内容幂等，下一轮从新水位重新出发即可。
//!
//! 06 §2.7 / A9：扫描区间早于可截断水位时 [`catchup::step`] 返回
//! [`RebuildRequired`](super::catchup::RebuildRequired)，这里**原样上抛并且不推进任何水位**——扫空区间后推进水位，正是
//! 「索引从此永远缺一块且不报错」的那次事故。

use super::builder::{build_segment, record_manifest, KernelLike, SealError, SealedSegment};
use super::catchup::{self, CatchUpStep};
use super::freshness::{next_txn_watermark, ChangeRate};
use super::kind::IndexKind;
use super::manifest::SegmentManifest;
use crate::kernel::{ChangeBatchSpec, ChangeFeed, ObjectChange};
use crate::types::{Lsn, TransactionId, TxnStatus};
use std::collections::BTreeSet;

/// 追赶循环需要的内核能力：变更流（F16）+ 构建器要的版本 xid + 事务状态判定。
///
/// 合成一个 trait 而不是让调用方传三个参数：这三样必须来自**同一个**内核，
/// 分开传就有「用 A 的变更流、B 的事务状态」这种拼装错误的余地。
pub trait MaintenanceHost: KernelLike + ChangeFeed {
    /// 事务状态（`TransactionManager::status` 的同一份判定）。
    fn txn_status(&self, xid: TransactionId) -> TxnStatus;

    /// 变更流视图。
    ///
    /// 不能写 `self as &dyn ChangeFeed`：trait upcasting 要到 Rust 1.86 才稳定，
    /// 而本项目 MSRV 是 1.75。显式给一个访问器，效果相同且老编译器也认。
    fn change_feed(&self) -> &dyn ChangeFeed;
}

impl MaintenanceHost for crate::kernel::Kernel {
    fn txn_status(&self, xid: TransactionId) -> TxnStatus {
        <Self as crate::kernel::TransactionManager>::status(self, xid)
    }

    fn change_feed(&self) -> &dyn ChangeFeed {
        self
    }
}

/// 一轮维护的结局。
#[derive(Clone, Debug, Default)]
pub struct MaintenanceReport {
    /// 本轮发布的段（已落清单）。
    pub segments: Vec<SealedSegment>,
    /// 本轮消费的变更条数。
    pub changes: usize,
    /// 清单水位（CAS 竞争失败时可能没动）。
    pub advanced_to: Lsn,
    /// 清单的 xid 水位。
    pub txn_watermark: TransactionId,
    /// 撞上未决事务屏障而提前结束（**不是错误**，06 §2.2）；不等于没消费：
    /// 屏障之前的变更已经发布。
    pub blocked: bool,
    /// 需要全量重建的原因；非空表示本轮没有推进任何水位（A9）。
    pub rebuild_required: Option<&'static str>,
    /// CAS 竞争失败次数：段已发布、水位由别人推进，下一轮从新水位重来。
    pub cas_conflicts: u32,
}

/// 一轮追赶的两个界。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CatchUpLimits {
    /// 目标 LSN（通常 `Lsn::MAX`，由内核截到可持久水位）。
    pub to: Lsn,
    /// 最多拉几批（防死循环）。
    pub max_batches: usize,
}

impl CatchUpLimits {
    pub fn new(to: Lsn, max_batches: usize) -> Self {
        CatchUpLimits { to, max_batches }
    }

    /// 追到当前可持久水位，不限批数。
    pub fn to_watermark() -> Self {
        CatchUpLimits {
            to: Lsn(u64::MAX),
            max_batches: usize::MAX,
        }
    }
}

/// 把 `manifest` 追到 `limits.to`，最多拉 `limits.max_batches` 批。
///
/// `to` 通常取 `ChangeFeed::object_watermark()`；传 `Lsn::MAX` 会被内核截到可持久水位。
/// `rate` 是字节→条数的估计样本（06 §3.2），只喂给 [`super::freshness`]，不参与判定。
pub fn catch_up(
    kind: &dyn IndexKind,
    host: &dyn MaintenanceHost,
    wal: &dyn crate::wal::WalManager,
    manifest: &mut SegmentManifest,
    spec: &ChangeBatchSpec,
    limits: CatchUpLimits,
    rate: &mut ChangeRate,
) -> Result<MaintenanceReport, SealError> {
    catch_up_observed(kind, host, wal, manifest, spec, limits, rate, |_| {})
}

/// 同 [`catch_up`]，但每发布一批段就调用一次 `on_publish`（参数是该批的
/// [`SealedSegment`]）——部署层用它在批次边界落盘段文件（08 §2.3 第 2/4 步）。
#[allow(clippy::too_many_arguments)]
pub fn catch_up_observed(
    kind: &dyn IndexKind,
    host: &dyn MaintenanceHost,
    wal: &dyn crate::wal::WalManager,
    manifest: &mut SegmentManifest,
    spec: &ChangeBatchSpec,
    limits: CatchUpLimits,
    rate: &mut ChangeRate,
    mut on_publish: impl FnMut(&SealedSegment),
) -> Result<MaintenanceReport, SealError> {
    let CatchUpLimits { to, max_batches } = limits;
    let mut report = MaintenanceReport::default();
    for _ in 0..max_batches {
        let from = manifest.watermark;
        let (changes, next, blocked) = match catchup::step(host.change_feed(), from, to, spec) {
            Ok(CatchUpStep::Progress { changes, next }) => (changes, next, false),
            Ok(CatchUpStep::Blocked { resume_at }) => {
                // 屏障路径：`resume_at` 之前的变更要照常应用（否则每次撞上未决事务
                // 都会把已提交的那部分再丢掉一次），用同一个投影重新取一批。
                let batch = host.change_feed().scan_changes(from, resume_at, spec)?;
                (batch.changes, None, true)
            }
            Ok(CatchUpStep::CaughtUp) => (Vec::new(), None, false),
            Err(rebuild) => {
                // A9：不推进水位。区间里的记录可能已被截断，增量追赶不再成立。
                report.rebuild_required = Some(rebuild.reason);
                break;
            }
        };
        if !changes.is_empty() {
            // 批次尾 = 下一批起点；没有下一条记录时取**本批最后一条记录**的 LSN——
            // 不能取调用方给的目标水位：`to` 可能是 `Lsn::MAX` 或 `object_watermark`，
            // 但内核只在 `to` 处截断，而水位越过 `object_watermark` 就会把还没持久化的
            // 记录算作已消费（06 §2.7 的反面）。`next` 正是内核给出的记录边界。
            let to_lsn = next.unwrap_or(host.change_feed().object_watermark().min(to).max(from));
            publish_batch(
                kind,
                host,
                wal,
                manifest,
                from,
                to_lsn,
                &changes,
                &mut report,
            )?;
            if let Some(sealed) = report.segments.last() {
                on_publish(sealed);
            }
            report.changes += changes.len();
            rate.observe(from, to_lsn, changes.len());
        }
        if blocked {
            report.blocked = true;
            break;
        }
        if next.is_none() {
            break;
        }
    }
    report.advanced_to = manifest.watermark;
    report.txn_watermark = manifest.txn_watermark;
    Ok(report)
}

/// 构建 → 记证据 → 发布 → 推进水位。**顺序不可交换**（08 §2.3 / A5）。
#[allow(clippy::too_many_arguments)]
fn publish_batch(
    kind: &dyn IndexKind,
    host: &dyn MaintenanceHost,
    wal: &dyn crate::wal::WalManager,
    manifest: &mut SegmentManifest,
    from: Lsn,
    to: Lsn,
    changes: &[ObjectChange],
    report: &mut MaintenanceReport,
) -> Result<(), SealError> {
    let sealed = build_segment(kind, host, 1, from, to, changes)?;
    let mut next = manifest.clone();
    let ticket = next.publish(sealed.published());
    // 持久化在推进之前：WAL 里存的是「段已发布、水位还是旧值」的那一版清单
    // （崩溃在这一步与下一步之间时，重启后从旧水位重扫，重复应用是幂等的）。
    super::manifest_log::persist(wal, &next)?;
    record_manifest(wal, &sealed, manifest.txn_watermark)?;
    // 06 §3.1：`tx_w` = 出现过的 xid 里「非终态」的最小者减一。
    let txn_watermark = next_txn_watermark(&xids_of(host, changes), |x| host.txn_status(x));
    let expected = manifest.watermark;
    if let Err(conflict) = next.advance(&ticket, expected, txn_watermark) {
        // 另一个追赶者先推进了水位：段已发布（幂等键保证不会重复），本轮放弃推进。
        debug_assert_eq!(conflict.expected, expected);
        report.cas_conflicts += 1;
    } else {
        // 把推进后的水位也落盘：重启后的起点就是下一次发布的起点。
        let _ = super::manifest_log::persist(wal, &next);
    }
    *manifest = next;
    report.segments.push(sealed);
    Ok(())
}

/// 一批变更涉及到的 xid（`created_by_xid` / `deleted_by_xid`）。
///
/// `ObjectChange` 本身不带 xid（ADR-0012 决策 7 允许将来加可选字段），因此这里回内核
/// 按 `(object, version)` 取——那正是索引条目要写的两个 xid，取自同一处不会分叉。
pub fn xids_of(host: &dyn MaintenanceHost, changes: &[ObjectChange]) -> BTreeSet<TransactionId> {
    let mut out = BTreeSet::new();
    for change in changes {
        if let Some((xmin, xmax)) = host.version_xids(change.object(), change.version()) {
            out.insert(xmin);
            if let Some(xmax) = xmax {
                out.insert(xmax);
            }
        }
    }
    out
}
