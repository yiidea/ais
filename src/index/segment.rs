//! 索引段格式（[08 §2.3]）：不可变、自校验、逐条带 CRC32C。
//!
//! ```text
//! 段头（magic ‖ format_version ‖ index_id ‖ from_lsn ‖ to_lsn ‖ entry_count ‖ content_crc）
//! 条目 × entry_count（kind ‖ object_id[16] ‖ version ‖ created_by_xid ‖
//!                   deleted_by_xid(+1 位存在标记) ‖ payload_len ‖ payload ‖ crc32c）
//! ```
//!
//! 两层校验各管一件事（与 06 §2.4 一致）：
//! - **逐条 CRC**：定位到哪一条坏掉，解码时立刻失败；
//! - **段头 content_crc**：整段摘要，用来在打开时判断「这一整段是否可信」，
//!   不必逐条扫完才知道。
//!
//! 解码是**严格**的：长度越界、尾部多余字节、CRC 不符都返回 `Corrupted`，
//! 不「尽量解析」。索引段是派生数据，坏段的正确处置是丢弃重建（06 §5.1），
//! 容忍半截只会把损坏带进查询结果。
//!
//! [08 §2.3]: ../../docs/chatgpt/08-index-layer.md

use crate::checksum::crc32c;
use crate::error::{CorruptionKind, KernelError, KernelResult};
use crate::index::kind::EntryKind;
use crate::types::{IndexId, Lsn, ObjectId, TransactionId, VersionId};

const MAGIC: [u8; 8] = *b"AINSIDX\x01";
const FORMAT_VERSION: u16 = 1;
/// 段头长度；`content_crc` 覆盖的是段头之后的全部字节。
pub const SEGMENT_HEADER_SIZE: usize = 8 + 2 + 8 + 8 + 8 + 8 + 4;

/// 一个索引条目（06 §2.3：主键是 `(object_id, version_id)`）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexEntry {
    pub kind: EntryKind,
    pub object: ObjectId,
    pub version: VersionId,
    pub created_by_xid: TransactionId,
    /// `None` = 仍有效；`Some(xid)` = 被 `xid` 删除。墓碑条目在这里是 `None`，
    /// 它描述的删除记在 `created_by_xid`（见 `EntryVisibility::tombstone`）。
    pub deleted_by_xid: Option<TransactionId>,
    pub payload: Vec<u8>,
}

impl IndexEntry {
    /// 交给回收器 / 可见性判定的形态（F17）。
    pub fn visibility(&self) -> crate::kernel::EntryVisibility {
        match self.kind {
            EntryKind::Live => {
                crate::kernel::EntryVisibility::new(self.created_by_xid, self.deleted_by_xid)
            }
            EntryKind::Tombstone => crate::kernel::EntryVisibility::tombstone(self.created_by_xid),
        }
    }

    fn encode_into(&self, out: &mut Vec<u8>) {
        let start = out.len();
        out.push(match self.kind {
            EntryKind::Live => 0,
            EntryKind::Tombstone => 1,
        });
        out.extend_from_slice(self.object.as_bytes());
        out.extend_from_slice(&self.version.0.to_le_bytes());
        out.extend_from_slice(&self.created_by_xid.0.to_le_bytes());
        match self.deleted_by_xid {
            None => out.extend_from_slice(&0u64.to_le_bytes()),
            Some(x) => {
                // 存在标记：0 表示「无删除事务」。xid 从 1 开始分配（`st.xmax.max(1)`），
                // 因此 0 不会被真实事务占用。
                out.extend_from_slice(&x.0.to_le_bytes());
            }
        }
        out.extend_from_slice(&(self.payload.len() as u32).to_le_bytes());
        out.extend_from_slice(&self.payload);
        let crc = crc32c(&out[start..]);
        out.extend_from_slice(&crc.to_le_bytes());
    }
}

/// 一个已封存的段。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Segment {
    pub index_id: IndexId,
    pub from_lsn: Lsn,
    pub to_lsn: Lsn,
    pub entries: Vec<IndexEntry>,
}

impl Segment {
    /// 编码为段字节（含段头与整段摘要）。
    pub fn encode(&self) -> Vec<u8> {
        let mut body = Vec::new();
        for e in &self.entries {
            e.encode_into(&mut body);
        }
        let mut out = Vec::with_capacity(SEGMENT_HEADER_SIZE + body.len());
        out.extend_from_slice(&MAGIC);
        out.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
        out.extend_from_slice(&self.index_id.0.to_le_bytes());
        out.extend_from_slice(&self.from_lsn.0.to_le_bytes());
        out.extend_from_slice(&self.to_lsn.0.to_le_bytes());
        out.extend_from_slice(&(self.entries.len() as u64).to_le_bytes());
        out.extend_from_slice(&crc32c(&body).to_le_bytes());
        out.extend_from_slice(&body);
        out
    }

    /// 严格解码；任何不一致都返回 `Corrupted`。
    pub fn decode(bytes: &[u8]) -> KernelResult<Segment> {
        if bytes.len() < SEGMENT_HEADER_SIZE {
            return Err(KernelError::corrupted(CorruptionKind::SegmentTruncated));
        }
        let mut r = Reader::new(bytes);
        let magic = r.take(8)?;
        if magic != MAGIC {
            return Err(KernelError::corrupted(CorruptionKind::SegmentBadMagic));
        }
        let format_version = r.u16()?;
        if format_version != FORMAT_VERSION {
            return Err(KernelError::corrupted(CorruptionKind::SegmentBadVersion {
                found: format_version,
            }));
        }
        let index_id = IndexId(r.u64()?);
        let from_lsn = Lsn(r.u64()?);
        let to_lsn = Lsn(r.u64()?);
        let entry_count = r.u64()?;
        let content_crc = r.u32()?;
        let body = &bytes[SEGMENT_HEADER_SIZE..];
        if crc32c(body) != content_crc {
            return Err(KernelError::corrupted(CorruptionKind::SegmentContentCrc));
        }

        let mut entries = Vec::new();
        for _ in 0..entry_count {
            // 条目起始位置：逐条 CRC 覆盖的就是「从这里到 crc 字段之前」。
            let entry_start = r.pos;
            let kind = match r.u8()? {
                0 => EntryKind::Live,
                1 => EntryKind::Tombstone,
                v => {
                    return Err(KernelError::corrupted(
                        CorruptionKind::SegmentBadEntryKind { found: v },
                    ))
                }
            };
            let object = ObjectId::from_bytes(r.array16()?);
            let version = VersionId(r.u64()?);
            let created_by_xid = TransactionId(r.u64()?);
            let deleted = r.u64()?;
            let payload_len = r.u32()? as usize;
            let payload = r.slice(payload_len)?.to_vec();
            let crc = r.u32()?;
            let expect = crc32c(&bytes[entry_start..r.pos - 4]);
            if crc != expect {
                return Err(KernelError::corrupted(CorruptionKind::SegmentEntryCrc));
            }
            entries.push(IndexEntry {
                kind,
                object,
                version,
                created_by_xid,
                deleted_by_xid: (deleted != 0).then_some(TransactionId(deleted)),
                payload,
            });
        }
        if r.pos != SEGMENT_HEADER_SIZE + body.len() {
            return Err(KernelError::corrupted(CorruptionKind::SegmentTrailingBytes));
        }
        if entries.len() as u64 != entry_count {
            return Err(KernelError::corrupted(CorruptionKind::SegmentTruncated));
        }
        Ok(Segment {
            index_id,
            from_lsn,
            to_lsn,
            entries,
        })
    }
}

struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Reader { buf, pos: 0 }
    }

    fn slice(&mut self, n: usize) -> KernelResult<&'a [u8]> {
        let end = self.pos.saturating_add(n);
        if end > self.buf.len() {
            return Err(KernelError::corrupted(CorruptionKind::SegmentTruncated));
        }
        let s = &self.buf[self.pos..end];
        self.pos = end;
        Ok(s)
    }

    fn take(&mut self, n: usize) -> KernelResult<&'a [u8]> {
        self.slice(n)
    }

    fn array16(&mut self) -> KernelResult<[u8; 16]> {
        let s = self.slice(16)?;
        let mut out = [0u8; 16];
        out.copy_from_slice(s);
        Ok(out)
    }

    fn u8(&mut self) -> KernelResult<u8> {
        Ok(self.slice(1)?[0])
    }

    fn u16(&mut self) -> KernelResult<u16> {
        let s = self.slice(2)?;
        Ok(u16::from_le_bytes([s[0], s[1]]))
    }

    fn u32(&mut self) -> KernelResult<u32> {
        let s = self.slice(4)?;
        Ok(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
    }

    fn u64(&mut self) -> KernelResult<u64> {
        let s = self.slice(8)?;
        let mut b = [0u8; 8];
        b.copy_from_slice(s);
        Ok(u64::from_le_bytes(b))
    }
}
