//! `CatchUp`（[08 §2.1]）：只做三件事——读、停、判。
//!
//! 1. **读**：`scan_changes(cursor, to, spec)`，一次一批，游标来自上一批的 `next_cursor`；
//! 2. **停**：`blocked_by_pending_txn = true` 时本轮结束，游标**不越过**未决事务；
//! 3. **判**：开扫之前检查 `watermark ≥ min_required_lsn`，不满足就转全量重建（06 §2.7）。
//!
//! 它不解析帧长、不读设备、不判定提交——这三件事分别在 `WalManager`、`IoExecutor`、
//! `TransactionManager` 里各有一份实现，复制任何一份都会分叉。
//!
//! **游标是包含式起点**：`next_cursor` 指向下一条未消费记录。追赶器把它原样存进
//! 协调区，不自己 `+1` 也不加帧长——帧头 36 字节、body 按 8 字节对齐，算错一次就落在
//! 帧中间，下一轮扫描会从帧内开始解出垃圾。
//!
//! [08 §2.1]: ../../docs/chatgpt/08-index-layer.md

use crate::error::KernelError;
use crate::kernel::{ChangeBatch, ChangeBatchSpec, ChangeFeed, ObjectChange};
use crate::types::Lsn;

/// 一轮追赶的结局。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CatchUpStep {
    /// 读到一批变更，游标前进。
    Progress {
        changes: Vec<ObjectChange>,
        /// 下一轮起点；`None` = 已追到 `to`。
        next: Option<Lsn>,
    },
    /// 撞上未决事务屏障：游标停住，本轮结束，**不是错误**（06 §2.2）。
    Blocked { resume_at: Lsn },
    /// 已追到 `to`，没有更多变更。
    CaughtUp,
}

/// 从 `cursor` 拉一批。`to` 由调用方给出（通常取 `object_watermark()`）。
///
/// 返回 `RebuildRequired` 的情形（06 §2.7）：游标早于可截断水位，区间内的记录可能
/// 已被删除。此时**必须**显式失败——扫一个空区间然后推进水位，正是「索引从此永远
/// 缺一块、且没有任何报错」的那次事故。
pub fn step(
    feed: &dyn ChangeFeed,
    cursor: Lsn,
    to: Lsn,
    spec: &ChangeBatchSpec,
) -> Result<CatchUpStep, RebuildRequired> {
    match feed.scan_changes(cursor, to, spec) {
        Ok(batch) => Ok(classify(batch, cursor)),
        Err(KernelError::Unsupported { .. }) => Err(RebuildRequired {
            reason: "change feed cursor precedes min_required_lsn",
        }),
        Err(other) => Err(RebuildRequired {
            reason: leak(other.to_string()),
        }),
    }
}

/// 需要全量重建（06 §2.7）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RebuildRequired {
    pub reason: &'static str,
}

// `RebuildRequired` 的 `reason` 是 `&'static str`，而扫描错误是动态文本；
// 只在真正需要诊断信息时延长生命周期，不影响正常路径。
fn leak(s: String) -> &'static str {
    Box::leak(s.into_boxed_str())
}

fn classify(batch: ChangeBatch, cursor: Lsn) -> CatchUpStep {
    if batch.blocked_by_pending_txn {
        // 屏障优先：游标停在屏障之前，本批已读到的变更照常应用。
        return CatchUpStep::Blocked {
            resume_at: batch.next_cursor.unwrap_or(cursor),
        };
    }
    match batch.next_cursor {
        // 空批不得推进游标（06 §2.7）：没有人变更时游标原地不动。
        None if batch.changes.is_empty() => CatchUpStep::CaughtUp,
        None => CatchUpStep::Progress {
            changes: batch.changes,
            next: None,
        },
        Some(next) => {
            if next == cursor && batch.changes.is_empty() {
                // 无进展的空批：视作追平当前水位，避免调用方空转。
                return CatchUpStep::CaughtUp;
            }
            CatchUpStep::Progress {
                changes: batch.changes,
                next: Some(next),
            }
        }
    }
}
