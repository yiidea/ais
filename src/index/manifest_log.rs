//! 协调区的持久化（[ADR-0013]）：一条 `0x31` `IndexManifest` 记下「发布之后、推进之前」的完整清单。
//!
//! 没有它，`SegmentManifest` 只活在进程内存里：崩溃或重启后水位归零，整个索引要
//! 从头重追。有了它，重启时取**最新一条**记录还原协调区——先段后水位的顺序因此
//! 在重启之后依然成立。
//!
//! 记录里的水位是**发布之后的旧水位**（这一步推进的目标），不是段尾；
//! `watermark_lsn` / `watermark_xid` 两个字段正好是 ADR-0013 已经给出的名字。
//!
//! | 崩溃点 | WAL 里 | 恢复后 |
//! |---|---|---|
//! | 记录之前 | 无这条记录 | 清单是更早的快照 |
//! | 记录之后、`advance` 之前 | 有，水位 = 旧值 | 段已发布；下一轮从旧水位重扫（幂等） |
//! | `advance` 之后 | 有，水位 = 推进后的值 | 与推进后的清单一致 |
//!
//! 记录本身**不参与 redo**（ADR-0013 决策 3 / 5）：它只为索引层自己的恢复服务；
//! 内核只把它当一条普通的派生记录，不做任何解析。
//!
//! [ADR-0013]: ../../docs/adr/0013-index-manifest-in-wal.md

use super::manifest::{SegmentId, SegmentManifest};
use crate::error::{InternalCode, KernelError};
use crate::types::{IndexId, Lsn, TransactionId};
use crate::wal::{RecordType, WalBody, WalManager};

/// 把 `manifest` 当前状态原样写成一条 `0x31`。返回该记录的 LSN。
pub fn persist(wal: &dyn WalManager, manifest: &SegmentManifest) -> crate::KernelResult<Lsn> {
    // 快照不描述任何段：`segment_id` 与两端的 LSN 都取水位本身，只用
    // `from_lsn == watermark_lsn` 与 builder 的证据记录区分（见 [`restore`]）。
    let body = WalBody::IndexManifest {
        index_id: manifest.index_id,
        segment_id: SegmentId::default().0,
        from_lsn: manifest.watermark.0,
        to_lsn: manifest.watermark.0,
        watermark_xid: manifest.txn_watermark.0,
        watermark_lsn: manifest.watermark.0,
    };
    WalBody::append_derived(wal, &body)
}

/// 记录体自洽性：`from_lsn == to_lsn == watermark_lsn`（见文件头）。
fn malformed(from_lsn: u64, to_lsn: u64, watermark_lsn: u64) -> Option<&'static str> {
    if from_lsn != to_lsn {
        Some("from_lsn differs from to_lsn")
    } else if from_lsn != watermark_lsn {
        Some("from_lsn differs from watermark_lsn")
    } else {
        None
    }
}

/// 从 WAL 还原协调区：取该索引**最新一条**可解析的 `0x31`。
///
/// - 没有记录（全新索引，或记录已被截断）→ `None`，调用方从空清单开始；
/// - 只读 `durable_lsn` 之前的记录——扫描到的是缓冲区里的字节，设备并不保证它们
///   在掉电后还在，把未持久的水位当成已持久，等于丢了那一段。
/// - 记录体不自洽（`from_lsn` / `to_lsn` / `watermark_lsn` 不相等）→ `Internal(InvariantViolation)`：
///   这是本模块自己写进去的格式，解不开就是 bug，不能当成「没发布过」静默跳过。
pub fn restore(
    wal: &dyn WalManager,
    index_id: IndexId,
) -> crate::KernelResult<Option<SegmentManifest>> {
    let durable = wal.durable_lsn();
    let scan = wal.scan_from(Lsn(0))?;
    let mut found = None;
    for at in scan.records {
        if at.lsn >= durable {
            // 从这一帧起既没有完整写入、也没有持久化确认，后面的记录同理。
            break;
        }
        if at.record.record_type != RecordType::IndexManifest {
            continue;
        }
        let Ok(WalBody::IndexManifest {
            index_id: idx,
            segment_id,
            from_lsn,
            to_lsn,
            watermark_xid,
            watermark_lsn,
        }) = WalBody::decode(at.record.record_type, &at.record.body)
        else {
            // 解不开的 0x31 不是本模块写的格式（例如 builder 只记证据的那种），跳过。
            continue;
        };
        // 本模块的快照记录满足 `from_lsn == watermark_lsn`；builder 的证据记录两者不同
        // （`from_lsn` 是段起点）。靠这一点把两类 0x31 区分开，不引入新的记录类型。
        if idx != index_id || from_lsn != watermark_lsn {
            continue;
        }
        found = Some((segment_id, from_lsn, to_lsn, watermark_xid, watermark_lsn));
    }
    found
        .map(|(segment_id, from_lsn, to_lsn, xid, watermark)| {
            if let Some(reason) = malformed(from_lsn, to_lsn, watermark) {
                return Err(KernelError::internal(
                    InternalCode::InvariantViolation,
                    format!("manifest record for {index_id:?}: {reason}"),
                ));
            }
            Ok(SegmentManifest {
                index_id,
                // 快照里没有段列表：两个水位就是全部内容（见模块头）。
                published_segments: Vec::new(),
                watermark: Lsn(watermark),
                txn_watermark: TransactionId(xid),
                baseline_segment_id: segment_id
                    .iter()
                    .any(|b| *b != 0)
                    .then_some(SegmentId(segment_id)),
            })
        })
        .transpose()
}
