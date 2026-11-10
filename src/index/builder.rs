//! `SegmentBuilder`（[08 §2.3]）：先段后水位。
//!
//! ```text
//! 1. 临时文件写入条目流（每条带 CRC32C，段头带整段摘要）
//! 2. fsync          ── 打点 index.segment.sealed（DeviceSite::IndexSegmentSealed）
//! 3. 计算 segment_id = hash(index_id ‖ from_lsn ‖ to_lsn ‖ format_version ‖ content_hash)
//! 4. 原子 rename 为正式段名
//! 5. 写 IndexManifest（0x31，经 append_derived）——**证据**，不是发布
//! ```
//!
//! 第 5 步写的是「这个段存在且已封存」的证据，属于 WAL；发布是协调区的动作
//! （[`super::manifest`]）。两者顺序不能反：先写清单再封段，崩溃后就会出现一条指向
//! 不存在段的清单。
//!
//! 本文件覆盖第 1、3 步与第 5 步的**编码**；第 2、4 步（fsync、原子 rename）属于
//! 部署层的文件操作——索引层不持有文件句柄，也不引入新的 I/O 路径。
//!
//! [08 §2.3]: ../../docs/chatgpt/08-index-layer.md

use crate::checksum::crc32c;
use crate::error::KernelError;
use crate::index::kind::{EntryAction, EntryKind, IndexKind};
use crate::index::manifest::{PublishedSegment, SegmentId};
use crate::index::segment::{IndexEntry, Segment};
use crate::kernel::ObjectChange;
use crate::types::{IndexId, Lsn, TransactionId, VersionId};
use crate::wal::{DerivedFamily, RecordType, WalBody, WalManager, WalRecord};

/// 封好的段：字节 + 段标识 + 发布所需的元数据。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SealedSegment {
    /// 正式段文件的内容（写进临时文件、fsync、rename 的就是它）。
    pub bytes: Vec<u8>,
    /// 幂等键算出的段标识。
    pub segment_id: SegmentId,
    pub index_id: IndexId,
    pub from_lsn: Lsn,
    pub to_lsn: Lsn,
    pub entry_count: u64,
}

impl SealedSegment {
    /// 清单里要记的那一行。
    pub fn published(&self) -> PublishedSegment {
        PublishedSegment {
            segment_id: self.segment_id,
            from_lsn: self.from_lsn,
            to_lsn: self.to_lsn,
            entry_count: self.entry_count,
        }
    }
}

/// 构建失败：取不到变更指定的版本。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SealError {
    /// 构建器要读的版本在对象里不存在。**不能静默跳过**：跳过会让段内容不再只由
    /// `(from_lsn, to_lsn]` 决定，重建与增量从此分叉（06 §2.2）。
    MissingVersion {
        object: crate::types::ObjectId,
        version: VersionId,
    },
    /// 内核状态无法给出读视图。
    Kernel(String),
}

impl From<KernelError> for SealError {
    fn from(e: KernelError) -> Self {
        SealError::Kernel(e.to_string())
    }
}

/// 把一批变更构建成一个段。
///
/// `from_lsn` / `to_lsn` 是本批覆盖的区间（游标原样传递，**不自行推算帧长**）。
pub fn build_segment<K: KernelLike + ?Sized>(
    kind: &dyn IndexKind,
    kernel: &K,
    index_format_version: u16,
    from_lsn: Lsn,
    to_lsn: Lsn,
    changes: &[ObjectChange],
) -> Result<SealedSegment, SealError> {
    let mut entries: Vec<IndexEntry> = Vec::new();
    for change in changes {
        // 读的必须是变更**指定的那个版本**（06 §2.2），不是当前最新版本。
        let xids = kernel.version_xids(change.object(), change.version());
        let view = xids.map(|(xmin, xmax)| super::kind::OwnedView {
            xmin,
            xmax,
            meta: change_meta(change),
        });
        let borrowed = view.as_ref().map(|v| super::kind::ObjectView {
            xmin: v.xmin,
            xmax: v.xmax,
            meta: &v.meta,
        });
        let actions = kind.plan(change, borrowed.as_ref());
        if actions.is_empty() && borrowed.is_none() {
            return Err(SealError::MissingVersion {
                object: change.object(),
                version: change.version(),
            });
        }
        for action in actions {
            // 不存在的版本：没有条目可改，跳过。它不贡献段内容，因此不破坏
            // 「段内容只由 `(from_lsn, to_lsn]` 决定」。
            if matches!(action, EntryAction::Absent { .. }) {
                continue;
            }
            let mut entry = to_entry(change.object(), action);
            // `Delete` 动作只带「谁删的」，不带「谁建的」。被删的版本可能落在**上一个段**
            // 里（段按 LSN 区间切），所以创建者必须回内核按版本号取，不能靠本批里的
            // 前一条动作推断——否则跨段的条目会丢掉 `created_by_xid`，G3 从此判错。
            if entry.kind == EntryKind::Live && entry.created_by_xid.0 == 0 {
                let (xmin, _) = kernel.version_xids(change.object(), entry.version).ok_or(
                    SealError::MissingVersion {
                        object: change.object(),
                        version: entry.version,
                    },
                )?;
                entry.created_by_xid = xmin;
            }
            // 同一条目的多条动作要**合并**，不是相互覆盖：`Versioned` 先对旧版本
            // 发 `Delete`、再对新版本发 `Insert`，两者主键不同（版本不同），
            // 而同一版本在同一批里出现两次时（分页边界造成的重复应用），
            // 后到的动作只补上它带来的字段，不能把先到的 `created_by_xid` 抹成 0。
            if let Some(slot) = entries
                .iter_mut()
                .find(|e| e.object == entry.object && e.version == entry.version)
            {
                merge(slot, entry);
            } else {
                entries.push(entry);
            }
        }
    }

    let segment = Segment {
        index_id: kind.index_id(),
        from_lsn,
        to_lsn,
        entries,
    };
    let bytes = segment.encode();
    let content_crc = content_crc_of(&bytes);
    let segment_id = SegmentId::derive(
        kind.index_id(),
        from_lsn,
        to_lsn,
        version_salt(content_crc, index_format_version),
    );
    Ok(SealedSegment {
        entry_count: segment.entries.len() as u64,
        bytes,
        segment_id,
        index_id: kind.index_id(),
        from_lsn,
        to_lsn,
    })
}

/// 把 `segment_id` 的幂等键各分量混进内容摘要（08 §2.3 第 3 步）。
fn version_salt(content_crc: u32, index_format_version: u16) -> u32 {
    let mut buf = Vec::with_capacity(6);
    buf.extend_from_slice(&index_format_version.to_le_bytes());
    buf.extend_from_slice(&content_crc.to_le_bytes());
    crc32c(&buf)
}

/// 段字节里的整段摘要（段头最后 4 字节）。
fn content_crc_of(bytes: &[u8]) -> u32 {
    let at = super::segment::SEGMENT_HEADER_SIZE - 4;
    u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

/// 同一主键上的两条动作合并成一条（重复应用幂等的落点）。
///
/// 规则只有三条，都是「保留更具体的那一边」：
/// 1. `created_by_xid` 取非零的那个（`Delete` 动作不携带创建者）；
/// 2. `deleted_by_xid` 一旦被某次动作写上就不再清空；
/// 3. 墓碑优先于活条目——删除是最终状态，之后到达的 `Insert` 不会让它复活。
fn merge(slot: &mut IndexEntry, incoming: IndexEntry) {
    if slot.created_by_xid.0 == 0 {
        slot.created_by_xid = incoming.created_by_xid;
    }
    if incoming.deleted_by_xid.is_some() {
        slot.deleted_by_xid = incoming.deleted_by_xid;
    }
    if incoming.kind == EntryKind::Tombstone {
        slot.kind = EntryKind::Tombstone;
        slot.created_by_xid = incoming.created_by_xid;
        slot.deleted_by_xid = None;
    }
    if !incoming.payload.is_empty() {
        slot.payload = incoming.payload;
    }
}

fn to_entry(object: crate::types::ObjectId, action: EntryAction) -> IndexEntry {
    match action {
        EntryAction::Insert {
            version,
            created_by_xid,
            payload,
        } => IndexEntry {
            kind: EntryKind::Live,
            object,
            version,
            created_by_xid,
            deleted_by_xid: None,
            payload,
        },
        EntryAction::Delete {
            version,
            deleted_by_xid,
        } => IndexEntry {
            kind: EntryKind::Live,
            object,
            version,
            created_by_xid: TransactionId(0),
            deleted_by_xid: Some(deleted_by_xid),
            payload: Vec::new(),
        },
        EntryAction::Tombstone {
            version,
            deleted_by_xid,
        } => IndexEntry {
            kind: EntryKind::Tombstone,
            object,
            version,
            created_by_xid: deleted_by_xid,
            deleted_by_xid: None,
            payload: Vec::new(),
        },
        // 由 `build_segment` 在进入本函数之前筛掉。
        EntryAction::Absent { version } => IndexEntry {
            kind: EntryKind::Tombstone,
            object,
            version,
            created_by_xid: TransactionId(0),
            deleted_by_xid: None,
            payload: Vec::new(),
        },
    }
}

fn change_meta(change: &ObjectChange) -> crate::kernel::ObjectMeta {
    match change {
        ObjectChange::Created { meta, .. } | ObjectChange::Versioned { meta, .. } => *meta,
        // 删除不改变元数据：墓碑只需要版本与 xid。
        ObjectChange::Deleted { .. } => crate::kernel::ObjectMeta {
            object_type: 0,
            tenant_id: 0,
            schema_id: 0,
            component_count: 0,
        },
    }
}

/// 构建器需要的那一点点内核能力（08 §2.2：`ObjectManager` 的逻辑读路径）。
///
/// 只声明一个方法，而不是收 `&dyn ObjectManager`：构建器要的是「指定版本的两个 xid」，
/// 不是版本解析（那是快照语义，不是构建语义）。
pub trait KernelLike {
    fn version_xids(
        &self,
        id: crate::types::ObjectId,
        ver: VersionId,
    ) -> Option<(TransactionId, Option<TransactionId>)>;
}

impl KernelLike for crate::kernel::Kernel {
    fn version_xids(
        &self,
        id: crate::types::ObjectId,
        ver: VersionId,
    ) -> Option<(TransactionId, Option<TransactionId>)> {
        Self::version_xids(self, id, ver)
    }
}

/// 把已封存的段记为 WAL 证据（08 §2.3 第 5 步）。
///
/// **不是发布**（ADR-0013 决策 3）：`IndexManifest` 在恢复期不参与 redo，只在 T4 判定里
/// 被读——「这个 `(index_id, segment_id)` 是否已在协调区发布」，用来决定 WAL 段文件
/// 能否物理删除。把 0x31 当成发布事实会制造第二个权威来源。
pub fn record_manifest(
    wal: &dyn WalManager,
    sealed: &SealedSegment,
    txn_watermark: TransactionId,
) -> crate::KernelResult<Lsn> {
    let body = WalBody::IndexManifest {
        index_id: sealed.index_id,
        segment_id: sealed.segment_id.0,
        from_lsn: sealed.from_lsn.0,
        to_lsn: sealed.to_lsn.0,
        watermark_xid: txn_watermark.0,
        watermark_lsn: sealed.to_lsn.0,
    };
    let record = WalRecord::new(
        RecordType::IndexManifest,
        TransactionId(0),
        Lsn(0),
        body.encode(),
    );
    wal.append_derived(&record, DerivedFamily::Index)
}
