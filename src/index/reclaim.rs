//! `Reclaimer`（[08 §2.5]）：G1–G6 在代码里的形状。
//!
//! ```text
//! seg.id != baseline                     // G1：基线段（有继任者才可回收）
//!   && seg.to_lsn <= watermark           // G2：段已被已发布进度覆盖
//!   && 所有条目对全部活快照不可见        // G3 / G4：条目与墓碑分别判
//!   && 回收代次之后的读者不引用它        // G5
//! // G6 不是判定条件，是执行顺序：先摘链，后删文件
//! ```
//!
//! 三条容易写错的细节：
//!
//! - **G3 与 G4 是两个方向**。旧快照需要「删除前的条目」，新快照需要「墓碑」；
//!   一个条目可见就保留整段。判定失败时**保留**（保守方向），因为少回收只浪费空间，
//!   多回收会让查询静默少召回。
//! - **不做部分回收**：段是不可变文件，段内存活条目占比低时走合并（写新段 + 发布 +
//!   回收旧段），复用同一条发布流程（ADR-0015 决策 3）。
//! - **G5 的引用集与回收代次**（ADR-0015 决策 5）：先推进 `reclaim_gen`、等读者确认、
//!   再采样。采样之后开始的读只能看到新清单。
//!
//! 可见性判定的唯一实现来自内核（ADR-0015 决策 4）：本文件只调用
//! [`LiveSnapshotSource::visible`]，一个字都不重写。
//!
//! [08 §2.5]: ../../docs/chatgpt/08-index-layer.md

use crate::index::manifest::SegmentManifest;
use crate::index::segment::{IndexEntry, Segment};
use crate::kernel::LiveSnapshotSource;

/// 段的回收判定结果。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReclaimVerdict {
    /// 整段是否可回收。**只有全部条件成立才为 `true`**。
    pub reclaimable: bool,
    /// 未通过的条件（便于诊断；空 = 可回收）。
    pub blockers: Vec<Blocker>,
    /// 判定为可见、因此保住整段的条目（`(object, version)` 的文本形式）。
    pub live_entries: Vec<String>,
}

/// 未通过的条件。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Blocker {
    /// G1：这是基线段。
    Baseline,
    /// G2：段尾在水位之后，尚未被已发布进度覆盖。
    NotCoveredByWatermark,
    /// G3 / G4：有条目对至少一个活快照可见。
    VisibleEntries,
    /// G5：段被仍在运行的读者引用。
    Referenced,
}

/// 判定一个段能否回收（08 §2.5）。
///
/// `live` 是内核的 `LiveSnapshotSource`（F17）。*只有* 它说了算：索引层不自己判可见性。
/// `newer_refs` 是天真的「回收代次之后的读者引用集」——调用方传入仍在读该段的读者
/// 集合；为空表示没有读者。`reclaim_gen_advanced` 是 G5 的前置条件：代次未推进时
/// 无法证明没有新读者，判定为「不可回收」。
pub fn reclaimable(
    seg: &Segment,
    manifest: &SegmentManifest,
    live: &dyn LiveSnapshotSource,
    reclaim_gen_advanced: bool,
    newer_refs: bool,
) -> ReclaimVerdict {
    let mut blockers = Vec::new();
    let mut live_entries = Vec::new();

    // G1：基线段不回收。它的继任者不存在时，删了就等于把整个索引删空。
    if manifest.baseline_segment_id
        == Some(super::manifest::SegmentId::derive(
            seg.index_id,
            seg.from_lsn,
            seg.to_lsn,
            content_crc(seg),
        ))
    {
        blockers.push(Blocker::Baseline);
    }

    // G2：段尾必须在已发布水位之内。
    if seg.to_lsn.0 > manifest.watermark.0 {
        blockers.push(Blocker::NotCoveredByWatermark);
    }

    // G3 / G4：逐个条目问内核。墓碑条目走 `EntryVisibility::tombstone`，方向相反。
    let mut any_visible = false;
    for e in &seg.entries {
        let v = e.visibility();
        // 读失败 → 保守：视为可见（08 §2.5）。
        let visible = live.visible_to_any_live(&v).unwrap_or(true);
        if visible {
            any_visible = true;
            live_entries.push(describe(e));
        }
    }
    if any_visible {
        blockers.push(Blocker::VisibleEntries);
    }

    // G5：回收代次必须已推进且没有新读者，才能证明「没人还在读这个段」。
    if !reclaim_gen_advanced || newer_refs {
        blockers.push(Blocker::Referenced);
    }

    ReclaimVerdict {
        reclaimable: blockers.is_empty(),
        blockers,
        live_entries,
    }
}

fn describe(e: &IndexEntry) -> String {
    format!("{}@{:?}", e.object, e.version)
}

/// 段的内容摘要：从编码后的字节里取，保证与段文件一致。
fn content_crc(seg: &Segment) -> u32 {
    let bytes = seg.encode();
    let at = super::segment::SEGMENT_HEADER_SIZE - 4;
    u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}
