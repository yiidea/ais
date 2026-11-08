//! 索引协调区（[08 §2.4]）：**唯一权威**，CAS 单写者。
//!
//! 协调区保存「哪些段已发布」与两个水位：
//!
//! ```text
//! SegmentManifest {
//!     published_segments: [{ segment_id, from_lsn, to_lsn, entry_count }],
//!     watermark:     Lsn,             // 已消费到的 LSN
//!     txn_watermark: TransactionId,   // tx_w（06 §3.1）
//!     baseline_segment_id: Option<SegmentId>,
//! }
//! ```
//!
//! 发布严格两步，顺序不可交换：
//!
//! 1. 把新段加入 `published_segments`
//! 2. CAS 推进 `watermark` / `txn_watermark`（期望值 = 本轮读到的旧值）
//!
//! | 崩溃点 | 状态 | 恢复动作 |
//! |---|---|---|
//! | 1 之前 | 段文件在、清单无 | 孤儿段，回收器按「不在清单」删除 |
//! | 1 与 2 之间 | 清单有、水位旧 | 下一轮从旧水位重扫，重复应用（幂等） |
//! | 2 之后 | 一致 | — |
//!
//! **方向是固定的**：「水位已推进但段缺失」这一刻不可能出现——它正是 06 §2.4
//! 表格里那行「静默丢数据」。这里的实现把这条性质做成**类型上不可表达**：
//! 推进水位只有一个入口 [`SegmentManifest::advance`]，它要求调用方出示一张
//! [`PublishedSegment`] 票据，而票据只能由 [`SegmentManifest::publish`] 发出。
//! 于是「先推水位再补段」在这段代码里没有写法。
//!
//! [08 §2.4]: ../../docs/chatgpt/08-index-layer.md

use crate::types::{IndexId, Lsn, TransactionId};
use std::fmt;

/// 段标识：`hash(index_id ‖ from_lsn ‖ to_lsn ‖ format_version ‖ content_hash)`（08 §2.3 第 3 步）。
///
/// 它是**幂等键**：同一区间、同一内容重算得到同一个 ID，发布因此可以去重；
/// 崩溃后重扫产生的段与原段同名，不会在清单里出现两条。
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct SegmentId(pub [u8; 16]);

impl SegmentId {
    /// 由幂等键的各个分量算出段标识。
    ///
    /// 用 CRC32C 打两轮生成 128 位：这不是密码学哈希，段标识只用来去重与定名，
    /// 不作为完整性证据——完整性由段内的 CRC 与摘要负责（08 §2.3）。
    pub fn derive(index_id: IndexId, from_lsn: Lsn, to_lsn: Lsn, content_crc: u32) -> SegmentId {
        let mut seed = Vec::with_capacity(33);
        seed.extend_from_slice(b"AINS-SEG\x01");
        seed.extend_from_slice(&index_id.0.to_le_bytes());
        seed.extend_from_slice(&from_lsn.0.to_le_bytes());
        seed.extend_from_slice(&to_lsn.0.to_le_bytes());
        seed.extend_from_slice(&content_crc.to_le_bytes());

        let mut id = [0u8; 16];
        let first = crate::checksum::crc32c(&seed);
        seed.push(0xFF);
        seed.extend_from_slice(&first.to_le_bytes());
        let second = crate::checksum::crc32c(&seed);
        id[..4].copy_from_slice(&first.to_le_bytes());
        id[4..8].copy_from_slice(&second.to_le_bytes());
        // 后 8 字节混入前 8 字节，避免两轮 CRC 的位相关性把空间压窄。
        for i in 0..8 {
            id[8 + i] = id[i] ^ id[(i + 3) % 8].rotate_left((i as u32) * 7 % 32);
        }
        SegmentId(id)
    }
}

impl fmt::Debug for SegmentId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for b in &self.0[..6] {
            write!(f, "{b:02x}")?;
        }
        write!(f, "…")
    }
}

/// 清单里的一条已发布段记录。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PublishedSegment {
    pub segment_id: SegmentId,
    pub from_lsn: Lsn,
    pub to_lsn: Lsn,
    pub entry_count: u64,
}

/// 索引协调区（08 §2.4）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SegmentManifest {
    pub index_id: IndexId,
    pub published_segments: Vec<PublishedSegment>,
    pub watermark: Lsn,
    /// `tx_w`：已消费到的最大已提交事务号（06 §3.1 的覆盖判定按 xid 不按 LSN）。
    pub txn_watermark: TransactionId,
    /// 基线段（ADR-0015 G1）：有继任者之前不可回收。
    pub baseline_segment_id: Option<SegmentId>,
}

/// CAS 失败：期望值与当前值不一致，说明另一个追赶者先推进了水位。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CasConflict {
    pub expected: Lsn,
    pub actual: Lsn,
}

impl SegmentManifest {
    /// 空协调区：没有任何段，水位为 0。
    pub fn empty(index_id: IndexId) -> Self {
        SegmentManifest {
            index_id,
            published_segments: Vec::new(),
            watermark: Lsn(0),
            txn_watermark: TransactionId(0),
            baseline_segment_id: None,
        }
    }

    /// 某个段是否已在清单里（T4 判定：这个 `(index_id, segment_id)` 是否已发布）。
    pub fn is_published(&self, id: SegmentId) -> bool {
        self.published_segments.iter().any(|s| s.segment_id == id)
    }

    /// 发布一个段，返回 [`Advance`] 票据。
    ///
    /// 去重：同一个 `segment_id` 重复发布只保留一条（崩溃后重扫会走到这里）。
    /// 先发布的段成为基线段。
    pub fn publish(&mut self, seg: PublishedSegment) -> Advance {
        if !self.is_published(seg.segment_id) {
            self.published_segments.push(PublishedSegment { ..seg });
            self.published_segments
                .sort_by_key(|s| (s.from_lsn, s.to_lsn));
            if self.baseline_segment_id.is_none() {
                self.baseline_segment_id = Some(seg.segment_id);
            }
        } else if let Some(existing) = self
            .published_segments
            .iter_mut()
            .find(|s| s.segment_id == seg.segment_id)
        {
            // 同 ID 必然同内容（ID 由幂等键算出）；这里刷新计数以防上游补报。
            existing.entry_count = seg.entry_count;
        }
        Advance { segment: seg }
    }

    /// CAS 推进两个水位（08 §2.4 第 2 步）。
    ///
    /// 期望值来自本轮开始时读到的 `watermark`。推进目标是**已发布段**的
    /// `to_lsn` 与清单里的 `tx_w`，不允许越过它们。
    pub fn advance(
        &mut self,
        ticket: &Advance,
        expected_watermark: Lsn,
        txn_watermark: TransactionId,
    ) -> Result<(), CasConflict> {
        if self.watermark != expected_watermark {
            return Err(CasConflict {
                expected: expected_watermark,
                actual: self.watermark,
            });
        }
        // 票据与清单必须一致：段没在清单里，说明调用方绕过了 `publish`。
        debug_assert!(self.is_published(ticket.segment.segment_id));
        // 水位单调只增：目标 = max(当前水位, 段尾)。
        self.watermark = Lsn(self.watermark.0.max(ticket.segment.to_lsn.0));
        self.txn_watermark = TransactionId(self.txn_watermark.0.max(txn_watermark.0));
        Ok(())
    }

    /// 覆盖判定（06 §3.1）：快照的 `xmin ≤ tx_w` 才算被索引覆盖。
    pub fn covers_xmin(&self, xmin: TransactionId) -> bool {
        xmin.0 <= self.txn_watermark.0
    }

    /// 崩溃恢复：丢弃「不在清单」的段（孤儿段），水位保持原值。
    ///
    /// 返回被丢弃的段 ID，供回收器删文件用。**不改水位**：清单里有、水位旧，
    /// 下一轮从旧水位重扫即可——重复应用是幂等的。
    pub fn discard_orphans<'a, I>(&self, candidates: I) -> Vec<SegmentId>
    where
        I: IntoIterator<Item = &'a SegmentId>,
    {
        candidates
            .into_iter()
            .copied()
            .filter(|id| !self.is_published(*id))
            .collect()
    }
}

/// 发布票据：证明「这个段已经在清单里」。
///
/// 它没有公开构造函数，只能由 [`SegmentManifest::publish`] 发出——推进水位因此
/// 不可能发生在发布之前。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Advance {
    segment: PublishedSegment,
}

impl Advance {
    /// 本次发布覆盖的 LSN 区间。
    pub fn segment(&self) -> &PublishedSegment {
        &self.segment
    }
}
