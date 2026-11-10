//! 内核内存状态与检查点快照编码。
//!
//! 状态本身是内存结构；检查点把它的**自洽快照**写成影子文件并原子发布（03 §5.2）。
//! 编码格式是内部的、带版本号的，不与 WAL 帧格式混用。

use crate::checksum::crc32c;
use crate::error::{CorruptionKind, InternalCode, KernelError, KernelResult};
use crate::kernel::txn::TxnStatusMap;
use crate::types::{
    CheckpointId, DeviceId, ExtentId, Lsn, MappingId, MappingVersion, ObjectId, ObjectType,
    PhysicalExtentId, TransactionId, TxnStatus, VersionId, VersionState,
};
use std::collections::BTreeMap;

pub const CHECKPOINT_MAGIC: u32 = 0x4143_4B50; // "ACKP"
pub const CHECKPOINT_VERSION: u16 = 1;

/// 版本条目。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VersionEntry {
    pub version: VersionId,
    pub xmin: TransactionId,
    pub xmax: Option<TransactionId>,
    pub state: VersionState,
    pub extent: Option<ExtentId>,
    pub created_lsn: Lsn,
}

/// 对象条目。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObjectEntry {
    pub object_type: ObjectType,
    pub created_xid: TransactionId,
    pub deleted_xid: Option<TransactionId>,
    pub versions: Vec<VersionEntry>,
}

impl ObjectEntry {
    pub fn latest_version(&self) -> VersionId {
        self.versions
            .last()
            .map(|v| v.version)
            .unwrap_or(VersionId(0))
    }
}

/// 逻辑 extent 状态（03 §4.1）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum ExtentState {
    Allocated = 0,
    Mapped = 1,
    Unmapped = 2,
    Freed = 3,
}

impl ExtentState {
    pub fn from_u8(v: u8) -> Option<Self> {
        Some(match v {
            0 => ExtentState::Allocated,
            1 => ExtentState::Mapped,
            2 => ExtentState::Unmapped,
            3 => ExtentState::Freed,
            _ => return None,
        })
    }
}

/// 逻辑 extent 条目。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExtentEntry {
    pub extent: ExtentId,
    pub object_id: ObjectId,
    pub physical: Option<PhysicalExtentId>,
    pub length: u64,
    pub alloc_xid: TransactionId,
    pub alloc_lsn: Lsn,
    pub state: ExtentState,
}

/// 物理 extent 条目（FSM 的内存视图）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PhysicalExtentEntry {
    pub id: PhysicalExtentId,
    pub device: DeviceId,
    pub offset: u64,
    pub length: u64,
    pub freed: bool,
}

/// 映射状态（03 §4.1）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum MappingState {
    Active = 0,
    Superseded = 1,
    Draining = 2,
    Retired = 3,
}

impl MappingState {
    pub fn from_u8(v: u8) -> Option<Self> {
        Some(match v {
            0 => MappingState::Active,
            1 => MappingState::Superseded,
            2 => MappingState::Draining,
            3 => MappingState::Retired,
            _ => return None,
        })
    }
}

/// 单条映射记录。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MappingRecord {
    pub mapping: MappingId,
    pub version: MappingVersion,
    pub physical: PhysicalExtentId,
    pub state: MappingState,
}

/// 一个逻辑 extent 的映射集合。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MappingEntry {
    pub extent: ExtentId,
    pub current_version: MappingVersion,
    pub current_mapping: MappingId,
    pub records: Vec<MappingRecord>,
}

/// 内核内存状态。
#[derive(Clone, Debug, Default)]
pub struct KernelState {
    pub objects: BTreeMap<ObjectId, ObjectEntry>,
    pub extents: BTreeMap<ExtentId, ExtentEntry>,
    pub physical: BTreeMap<PhysicalExtentId, PhysicalExtentEntry>,
    pub mappings: BTreeMap<ExtentId, MappingEntry>,
    pub txn_status: TxnStatusMap,

    pub xmax: u64,
    pub next_oid_seq: u64,
    pub next_extent: u64,
    pub next_physical: u64,
    pub next_mapping: u64,
    /// 每个设备的 payload 区 bump 分配水位。
    pub device_next_offset: BTreeMap<DeviceId, u64>,

    pub checkpoint_id: CheckpointId,
    pub checkpoint_lsn: Lsn,
}

impl KernelState {
    pub fn alloc_object_id(&mut self, database_id: u32) -> ObjectId {
        self.next_oid_seq += 1;
        ObjectId::new(database_id, self.next_oid_seq)
    }

    pub fn alloc_extent(&mut self) -> ExtentId {
        self.next_extent += 1;
        ExtentId(self.next_extent)
    }

    pub fn alloc_physical(&mut self) -> PhysicalExtentId {
        self.next_physical += 1;
        PhysicalExtentId(self.next_physical)
    }

    pub fn alloc_mapping(&mut self) -> MappingId {
        self.next_mapping += 1;
        MappingId(self.next_mapping)
    }

    // ---- 检查点编码 -------------------------------------------------------

    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer::default();
        w.u32(CHECKPOINT_MAGIC);
        w.u16(CHECKPOINT_VERSION);
        w.u16(0);
        w.u64(self.checkpoint_id.0);
        w.u64(self.checkpoint_lsn.0);
        w.u64(self.xmax);
        w.u64(self.next_oid_seq);
        w.u64(self.next_extent);
        w.u64(self.next_physical);
        w.u64(self.next_mapping);

        w.u32(self.objects.len() as u32);
        for (id, o) in &self.objects {
            w.bytes16(id.as_bytes());
            w.u8(o.object_type as u8);
            w.u64(o.created_xid.0);
            w.u64(o.deleted_xid.map(|x| x.0).unwrap_or(0));
            w.u32(o.versions.len() as u32);
            for v in &o.versions {
                w.u64(v.version.0);
                w.u64(v.xmin.0);
                w.u64(v.xmax.map(|x| x.0).unwrap_or(0));
                w.u8(v.state as u8);
                w.u64(v.extent.map(|e| e.0).unwrap_or(0));
                w.u64(v.created_lsn.0);
            }
        }

        w.u32(self.extents.len() as u32);
        for (id, e) in &self.extents {
            w.u64(id.0);
            w.bytes16(e.object_id.as_bytes());
            w.u64(e.physical.map(|p| p.0).unwrap_or(0));
            w.u64(e.length);
            w.u64(e.alloc_xid.0);
            w.u64(e.alloc_lsn.0);
            w.u8(e.state as u8);
        }

        w.u32(self.physical.len() as u32);
        for (id, p) in &self.physical {
            w.u64(id.0);
            w.u32(p.device.0 as u32);
            w.u64(p.offset);
            w.u64(p.length);
            w.u8(u8::from(p.freed));
        }

        w.u32(self.mappings.len() as u32);
        for (id, m) in &self.mappings {
            w.u64(id.0);
            w.u64(m.current_version.0);
            w.u64(m.current_mapping.0);
            w.u32(m.records.len() as u32);
            for r in &m.records {
                w.u64(r.mapping.0);
                w.u64(r.version.0);
                w.u64(r.physical.0);
                w.u8(r.state as u8);
            }
        }

        w.u32(self.txn_status.len() as u32);
        for (xid, s) in &self.txn_status {
            w.u64(xid.0);
            w.u8(*s as u8);
        }

        w.u32(self.device_next_offset.len() as u32);
        for (d, off) in &self.device_next_offset {
            w.u32(d.0 as u32);
            w.u64(*off);
        }

        let crc = crc32c(&w.buf);
        w.u32(crc);
        w.finish()
    }

    pub fn decode(buf: &[u8]) -> KernelResult<Self> {
        if buf.len() < 8 {
            return Err(KernelError::corrupted(CorruptionKind::CheckpointManifest));
        }
        let mut r = Reader::new(buf);
        let magic = r.u32()?;
        let version = r.u16()?;
        let _reserved = r.u16()?;
        if magic != CHECKPOINT_MAGIC || version != CHECKPOINT_VERSION {
            return Err(KernelError::corrupted(CorruptionKind::CheckpointManifest));
        }
        let checkpoint_id = CheckpointId(r.u64()?);
        let checkpoint_lsn = Lsn(r.u64()?);
        let xmax = r.u64()?;
        let next_oid_seq = r.u64()?;
        let next_extent = r.u64()?;
        let next_physical = r.u64()?;
        let next_mapping = r.u64()?;

        let mut objects = BTreeMap::new();
        for _ in 0..r.u32()? {
            let id = ObjectId::from_bytes(r.bytes16()?);
            let object_type = ObjectType::from_u8(r.u8()?)
                .ok_or_else(|| KernelError::corrupted(CorruptionKind::CheckpointManifest))?;
            let created_xid = TransactionId(r.u64()?);
            let deleted_xid = none_if_zero_txn(r.u64()?);
            let mut versions = Vec::new();
            for _ in 0..r.u32()? {
                versions.push(VersionEntry {
                    version: VersionId(r.u64()?),
                    xmin: TransactionId(r.u64()?),
                    xmax: none_if_zero_txn(r.u64()?),
                    state: VersionState::from_u8(r.u8()?).ok_or_else(|| {
                        KernelError::corrupted(CorruptionKind::CheckpointManifest)
                    })?,
                    extent: none_if_zero_extent(r.u64()?),
                    created_lsn: Lsn(r.u64()?),
                });
            }
            objects.insert(
                id,
                ObjectEntry {
                    object_type,
                    created_xid,
                    deleted_xid,
                    versions,
                },
            );
        }

        let mut extents = BTreeMap::new();
        for _ in 0..r.u32()? {
            let id = ExtentId(r.u64()?);
            let object_id = ObjectId::from_bytes(r.bytes16()?);
            extents.insert(
                id,
                ExtentEntry {
                    extent: id,
                    object_id,
                    physical: none_if_zero_pe(r.u64()?),
                    length: r.u64()?,
                    alloc_xid: TransactionId(r.u64()?),
                    alloc_lsn: Lsn(r.u64()?),
                    state: ExtentState::from_u8(r.u8()?).ok_or_else(|| {
                        KernelError::corrupted(CorruptionKind::CheckpointManifest)
                    })?,
                },
            );
        }

        let mut physical = BTreeMap::new();
        for _ in 0..r.u32()? {
            let id = PhysicalExtentId(r.u64()?);
            physical.insert(
                id,
                PhysicalExtentEntry {
                    id,
                    device: DeviceId(r.u32()? as u64),
                    offset: r.u64()?,
                    length: r.u64()?,
                    freed: r.u8()? != 0,
                },
            );
        }

        let mut mappings = BTreeMap::new();
        for _ in 0..r.u32()? {
            let id = ExtentId(r.u64()?);
            let current_version = MappingVersion(r.u64()?);
            let current_mapping = MappingId(r.u64()?);
            let mut records = Vec::new();
            for _ in 0..r.u32()? {
                records.push(MappingRecord {
                    mapping: MappingId(r.u64()?),
                    version: MappingVersion(r.u64()?),
                    physical: PhysicalExtentId(r.u64()?),
                    state: MappingState::from_u8(r.u8()?).ok_or_else(|| {
                        KernelError::corrupted(CorruptionKind::CheckpointManifest)
                    })?,
                });
            }
            mappings.insert(
                id,
                MappingEntry {
                    extent: id,
                    current_version,
                    current_mapping,
                    records,
                },
            );
        }

        let mut txn_status = TxnStatusMap::new();
        for _ in 0..r.u32()? {
            let xid = TransactionId(r.u64()?);
            let s = TxnStatus::from_u8(r.u8()?)
                .ok_or_else(|| KernelError::corrupted(CorruptionKind::CheckpointManifest))?;
            txn_status.insert(xid, s);
        }

        let mut device_next_offset = BTreeMap::new();
        for _ in 0..r.u32()? {
            device_next_offset.insert(DeviceId(r.u32()? as u64), r.u64()?);
        }

        let body_len = r.pos;
        let expected_crc = r.u32()?;
        if crc32c(&buf[..body_len]) != expected_crc {
            return Err(KernelError::corrupted(CorruptionKind::CheckpointManifest));
        }

        Ok(KernelState {
            objects,
            extents,
            physical,
            mappings,
            txn_status,
            xmax,
            next_oid_seq,
            next_extent,
            next_physical,
            next_mapping,
            device_next_offset,
            checkpoint_id,
            checkpoint_lsn,
        })
    }
}

fn none_if_zero_txn(v: u64) -> Option<TransactionId> {
    if v == 0 {
        None
    } else {
        Some(TransactionId(v))
    }
}

fn none_if_zero_extent(v: u64) -> Option<ExtentId> {
    if v == 0 {
        None
    } else {
        Some(ExtentId(v))
    }
}

fn none_if_zero_pe(v: u64) -> Option<PhysicalExtentId> {
    if v == 0 {
        None
    } else {
        Some(PhysicalExtentId(v))
    }
}

#[derive(Default)]
struct Writer {
    buf: Vec<u8>,
}

impl Writer {
    fn u8(&mut self, v: u8) {
        self.buf.push(v);
    }
    fn u16(&mut self, v: u16) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    fn u32(&mut self, v: u32) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    fn u64(&mut self, v: u64) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    fn bytes16(&mut self, v: &[u8; 16]) {
        self.buf.extend_from_slice(v);
    }
    fn finish(self) -> Vec<u8> {
        self.buf
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
    fn take(&mut self, n: usize) -> KernelResult<&'a [u8]> {
        if self.pos + n > self.buf.len() {
            return Err(KernelError::corrupted(CorruptionKind::CheckpointManifest));
        }
        let s = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }
    fn u8(&mut self) -> KernelResult<u8> {
        Ok(self.take(1)?[0])
    }
    fn u16(&mut self) -> KernelResult<u16> {
        let b = self.take(2)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }
    fn u32(&mut self) -> KernelResult<u32> {
        let b = self.take(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }
    fn u64(&mut self) -> KernelResult<u64> {
        let b = self.take(8)?;
        Ok(u64::from_le_bytes([
            b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
        ]))
    }
    fn bytes16(&mut self) -> KernelResult<[u8; 16]> {
        let b = self.take(16)?;
        let mut out = [0u8; 16];
        out.copy_from_slice(b);
        Ok(out)
    }
}

#[allow(dead_code)]
fn invariant(msg: &str) -> KernelError {
    KernelError::internal(InternalCode::InvariantViolation, msg)
}
