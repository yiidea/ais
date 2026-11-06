//! WAL 二进制格式（03 §2）与 `WalManager`（04 §7）。
//!
//! 段布局：`SegmentHeader(4KB) | Record | Record | ...`，段大小固定（默认 1MB，可配置）。
//! LSN 是 WAL 的逻辑字节偏移，也等于设备上的物理偏移（WAL 区独立于 payload 区）。

use crate::checksum::crc32c;
use crate::error::{CorruptionKind, KernelError, KernelResult};
use crate::types::{IndexId, Lsn, ObjectId, TransactionId, VersionId};

pub mod device;

pub use device::{DeviceWal, DeviceWalConfig};

pub const WAL_MAGIC: u32 = 0x4149_4E53; // "AINS"
pub const FORMAT_VERSION: u16 = 1;
pub const SEGMENT_HEADER_SIZE: usize = 4096;
/// 记录帧头：total_len(4) + type(1) + flags(1) + reserved(2)
/// + lsn(8) + xid(8) + prev_lsn(8) + payload_crc32c(4) = 36 字节（03 §2.2）。
pub const RECORD_HEADER_SIZE: usize = 36;
pub const ALIGN: u64 = 8;

/// 记录类型（03 §2.3）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum RecordType {
    // 0x01 Transaction
    TxnBegin = 0x01,
    TxnCommit = 0x02,
    TxnAbort = 0x03,

    // 0x10 Object / Version
    ObjectCreate = 0x10,
    ObjectVersion = 0x11,
    ObjectDelete = 0x12,

    // 0x20 Extent / Mapping
    ExtentAlloc = 0x20,
    ExtentFree = 0x21,
    MappingCreate = 0x22,
    MappingSwitch = 0x23,

    // 0x30 Index（派生数据，可重建）
    IndexDelta = 0x30,
    IndexManifest = 0x31,

    // 0x40 Maintenance
    MigrationBegin = 0x40,
    MigrationEnd = 0x41,
    CompactionBegin = 0x42,
    CompactionEnd = 0x43,
    CheckpointBegin = 0x44,
}

/// 非事务派生记录族（04 §7 W4，[ADR-0013]）。
///
/// 只有派生数据（索引构建进度、维护标记）允许走非事务通道；对象与事务族的语义
/// 全部建立在「属于某个事务」之上，混入非事务通道会绕过 03 §6.2 的 redo 规则。
///
/// [ADR-0013]: ../../docs/adr/0013-index-manifest-in-wal.md
#[non_exhaustive]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DerivedFamily {
    /// `0x30` `IndexDelta` / `0x31` `IndexManifest`
    Index,
    /// `0x40`–`0x43` 中不含提交语义的 Begin/End
    Maintenance,
    /// 保留：任何模块都不得使用，出现即 `InvalidArgument`
    None,
}

impl DerivedFamily {
    /// 族与记录类型是否匹配（ADR-0013 决策 1）。不匹配时调用方必须得到
    /// `InvalidArgument`，且**不写入任何字节**。
    pub fn accepts(self, record_type: RecordType) -> bool {
        use RecordType::*;
        match self {
            DerivedFamily::Index => matches!(record_type, IndexDelta | IndexManifest),
            DerivedFamily::Maintenance => {
                matches!(
                    record_type,
                    MigrationBegin | MigrationEnd | CompactionBegin | CompactionEnd
                )
            }
            DerivedFamily::None => false,
        }
    }
}

impl RecordType {
    pub fn from_u8(v: u8) -> Option<Self> {
        Some(match v {
            0x01 => RecordType::TxnBegin,
            0x02 => RecordType::TxnCommit,
            0x03 => RecordType::TxnAbort,
            0x10 => RecordType::ObjectCreate,
            0x11 => RecordType::ObjectVersion,
            0x12 => RecordType::ObjectDelete,
            0x20 => RecordType::ExtentAlloc,
            0x21 => RecordType::ExtentFree,
            0x22 => RecordType::MappingCreate,
            0x23 => RecordType::MappingSwitch,
            0x30 => RecordType::IndexDelta,
            0x31 => RecordType::IndexManifest,
            0x40 => RecordType::MigrationBegin,
            0x41 => RecordType::MigrationEnd,
            0x42 => RecordType::CompactionBegin,
            0x43 => RecordType::CompactionEnd,
            0x44 => RecordType::CheckpointBegin,
            _ => return None,
        })
    }
}

/// 一条待追加的 WAL 记录。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WalRecord {
    pub record_type: RecordType,
    pub flags: u8,
    pub xid: TransactionId,
    pub prev_lsn: Lsn,
    pub body: Vec<u8>,
}

impl WalRecord {
    pub fn new(record_type: RecordType, xid: TransactionId, prev_lsn: Lsn, body: Vec<u8>) -> Self {
        WalRecord {
            record_type,
            flags: 0,
            xid,
            prev_lsn,
            body,
        }
    }
}

/// 带 LSN 的记录（扫描结果）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WalRecordAt {
    pub lsn: Lsn,
    pub record: WalRecord,
}

/// 扫描结果。遇到损坏即终止（W2），`stopped_at_corruption` 是正常崩溃点标记。
#[derive(Clone, Debug, Default)]
pub struct WalScan {
    pub records: Vec<WalRecordAt>,
    pub last_valid_lsn: Lsn,
    pub stopped_at_corruption: bool,
}

/// `WalManager`（04 §7）。
pub trait WalManager: Send + Sync {
    /// 追加一条记录，返回其 LSN（不保证已持久）。
    fn append(&self, rec: &WalRecord) -> KernelResult<Lsn>;

    /// 非事务派生记录追加（ADR-0013 决策 1）：`xid = 0`，不进入任何事务的前驱链。
    ///
    /// 记录族与记录类型必须匹配（[`DerivedFamily::accepts`]），`xid` 必须为 0；
    /// 否则返回 `InvalidArgument` 且**不写入任何字节**。`WalManager` 不解析记录体
    /// （ADR-0013 决策 2）。
    fn append_derived(&self, rec: &WalRecord, family: DerivedFamily) -> KernelResult<Lsn>;

    /// 刷盘到目标 LSN，返回时保证 target_lsn 及之前的记录已持久。
    fn flush_upto(&self, target: Lsn) -> KernelResult<()>;

    /// 当前已确认持久的水位。
    fn durable_lsn(&self) -> Lsn;

    /// 从指定 LSN 开始扫描有效记录（恢复用；遇损坏即终止并返回终止 LSN）。
    fn scan_from(&self, from: Lsn) -> KernelResult<WalScan>;

    /// 计算可截断边界。
    fn min_required_lsn(&self) -> Lsn;

    /// 推进可截断边界（仅在 checkpoint 发布后调用）。
    fn set_min_required_lsn(&self, lsn: Lsn);
}

/// 帧编码：40 字节头 + body + 8 字节对齐填充（03 §2.2）。
///
/// 帧头里没有单独的 body_len 字段，因此 body 的尾部零字节与对齐填充不可区分。
/// 这里让 **body 长度本身取 8 字节对齐**：短 body 编码为带零填充的定长形式，
/// 解码时 body 长度固定为 `total_len - RECORD_HEADER_SIZE`，两侧规则一致。
pub fn encode_frame(record: &WalRecord, lsn: Lsn) -> Vec<u8> {
    // body 一律补齐到 8 字节；total_len 覆盖 "头 + 补齐后的 body"。
    let body_len = align_up(record.body.len() as u64, ALIGN) as usize;
    let total_len = RECORD_HEADER_SIZE + body_len;
    let mut body = record.body.clone();
    body.resize(body_len, 0);

    let mut out = Vec::with_capacity(total_len);
    out.extend_from_slice(&(total_len as u32).to_le_bytes()); // 0..4
    out.push(record.record_type as u8); // 4
    out.push(record.flags); // 5
    out.extend_from_slice(&0u16.to_le_bytes()); // 6..8
    out.extend_from_slice(&lsn.0.to_le_bytes()); // 8..16
    out.extend_from_slice(&record.xid.0.to_le_bytes()); // 16..24
    out.extend_from_slice(&record.prev_lsn.0.to_le_bytes()); // 24..32
    out.extend_from_slice(&crc32c(&body).to_le_bytes()); // 32..40
    out.extend_from_slice(&body); // 40..
    debug_assert_eq!(out.len(), total_len);
    out
}

fn body_len_from_total(total_len: usize) -> usize {
    total_len - RECORD_HEADER_SIZE
}

/// 帧解码结果。
pub struct DecodedFrame {
    pub lsn: Lsn,
    pub record: WalRecord,
    pub total_len: usize,
}

/// 从缓冲区 `buf` 的 `offset` 处解码一帧。返回 `Ok(None)` 表示尾部结束/损坏。
pub fn decode_frame(buf: &[u8], offset: usize, expected_lsn: Lsn) -> KernelResult<Option<DecodedFrame>> {
    if offset + RECORD_HEADER_SIZE > buf.len() {
        return Ok(None);
    }
    let h = &buf[offset..offset + RECORD_HEADER_SIZE];
    let total_len = u32::from_le_bytes([h[0], h[1], h[2], h[3]]) as usize;
    if total_len == 0 {
        return Ok(None);
    }
    // body 本身 8 字节对齐，但帧头 36 字节不是 8 的倍数，
    // 因此 total_len 只要求「body 段对齐」，不能整体按 ALIGN 校验。
    if total_len < RECORD_HEADER_SIZE
        || (total_len - RECORD_HEADER_SIZE) % ALIGN as usize != 0
    {
        return Ok(None);
    }
    let Some(record_type) = RecordType::from_u8(h[4]) else {
        return Ok(None);
    };
    let flags = h[5];
    let lsn = Lsn(u64::from_le_bytes([
        h[8], h[9], h[10], h[11], h[12], h[13], h[14], h[15],
    ]));
    if lsn != expected_lsn {
        return Ok(None);
    }
    let xid = TransactionId(u64::from_le_bytes([
        h[16], h[17], h[18], h[19], h[20], h[21], h[22], h[23],
    ]));
    let prev_lsn = Lsn(u64::from_le_bytes([
        h[24], h[25], h[26], h[27], h[28], h[29], h[30], h[31],
    ]));
    let crc = u32::from_le_bytes([h[32], h[33], h[34], h[35]]);
    if offset + total_len > buf.len() {
        return Ok(None);
    }
    // 帧尾的对齐填充不计入 body（CRC 只覆盖 body，03 §2.2）。
    let body_len = body_len_from_total(total_len);
    let body = &buf[offset + RECORD_HEADER_SIZE..offset + RECORD_HEADER_SIZE + body_len];
    if crc32c(body) != crc {
        return Ok(None);
    }
    Ok(Some(DecodedFrame {
        lsn,
        record: WalRecord {
            record_type,
            flags,
            xid,
            prev_lsn,
            body: body.to_vec(),
        },
        total_len,
    }))
}

pub fn align_up(v: u64, a: u64) -> u64 {
    (v + a - 1) / a * a
}

// ---------------------------------------------------------------------------
// 类型化 body 编解码
// ---------------------------------------------------------------------------

/// 记录 body 的类型化视图（03 §2.3）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WalBody {
    TxnBegin {
        isolation: u8,
        snapshot_id: u64,
        xmin: u64,
        xmax: u64,
        active_xids: Vec<u64>,
    },
    TxnCommit,
    TxnAbort,
    ObjectCreate {
        object_id: ObjectId,
        object_type: u8,
        component_count: u16,
    },
    ObjectVersion {
        object_id: ObjectId,
        new_version: VersionId,
        old_version: VersionId,
        extent: u64,
    },
    ObjectDelete {
        object_id: ObjectId,
        /// 被删除的版本（墓碑的目标版本）。
        ///
        /// 03 §2.3 的 `ObjectDeleteBody` 原先只有 `object_id`，但 `ObjectChange::Deleted`
        /// 需要 `version`，而索引侧墓碑的目标是 `(object_id, version)` 而不是对象——
        /// 一个对象被删除时，哪些版本已经不可见由删除事务的快照决定。这里补上该字段
        /// （03 §2.3 已同步），让变更流不必靠「猜最新版本」来构造墓碑。
        version: VersionId,
    },
    ExtentAlloc {
        extent: u64,
        mapping_id: u64,
        physical_extent: u64,
        device_id: u32,
        device_offset: u64,
        length: u64,
    },
    ExtentFree {
        extent: u64,
    },
    MappingCreate {
        extent: u64,
        mapping_id: u64,
        physical_extent: u64,
    },
    MappingSwitch {
        extent: u64,
        old_mapping: u64,
        new_mapping: u64,
        reason: u8,
    },
    /// 索引条目变更（06 §2.3）。`index_id` 是决定幂等的必填字段：一份 WAL 上可能同时
    /// 有多个索引的追赶记录，缺了它，「按 `(object_id, version_id)` 去重」就会跨索引
    /// 覆盖（见 03 §2.3 的字段说明）。
    IndexDelta {
        index_id: IndexId,
        object_id: ObjectId,
        version: VersionId,
        op: u8,
    },
    /// 索引段发布（06 §2.4）。`segment_id` / `from_lsn` / `to_lsn` 与协调区一起构成
    /// ADR-0013 决策 4 的 T4 核对依据；**不作为恢复期 redo 输入**（ADR-0013 决策 3）。
    IndexManifest {
        index_id: IndexId,
        segment_id: [u8; 16],
        from_lsn: u64,
        to_lsn: u64,
        watermark_xid: u64,
        watermark_lsn: u64,
    },
    Maintenance {
        kind: RecordType,
        extent: u64,
        old_mapping: u64,
        new_mapping: u64,
    },
}

impl WalBody {
    pub fn record_type(&self) -> RecordType {
        match self {
            WalBody::TxnBegin { .. } => RecordType::TxnBegin,
            WalBody::TxnCommit => RecordType::TxnCommit,
            WalBody::TxnAbort => RecordType::TxnAbort,
            WalBody::ObjectCreate { .. } => RecordType::ObjectCreate,
            WalBody::ObjectVersion { .. } => RecordType::ObjectVersion,
            WalBody::ObjectDelete { .. } => RecordType::ObjectDelete,
            WalBody::ExtentAlloc { .. } => RecordType::ExtentAlloc,
            WalBody::ExtentFree { .. } => RecordType::ExtentFree,
            WalBody::MappingCreate { .. } => RecordType::MappingCreate,
            WalBody::MappingSwitch { .. } => RecordType::MappingSwitch,
            WalBody::IndexDelta { .. } => RecordType::IndexDelta,
            WalBody::IndexManifest { .. } => RecordType::IndexManifest,
            WalBody::Maintenance { kind, .. } => *kind,
        }
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer::default();
        match self {
            WalBody::TxnBegin {
                isolation,
                snapshot_id,
                xmin,
                xmax,
                active_xids,
            } => {
                w.u8(*isolation);
                w.u64(*snapshot_id);
                w.u64(*xmin);
                w.u64(*xmax);
                w.u32(active_xids.len() as u32);
                for x in active_xids {
                    w.u64(*x);
                }
            }
            WalBody::TxnCommit | WalBody::TxnAbort => {}
            WalBody::ObjectCreate {
                object_id,
                object_type,
                component_count,
            } => {
                w.bytes16(object_id.as_bytes());
                w.u8(*object_type);
                w.u16(*component_count);
            }
            WalBody::ObjectVersion {
                object_id,
                new_version,
                old_version,
                extent,
            } => {
                w.bytes16(object_id.as_bytes());
                w.u64(new_version.0);
                w.u64(old_version.0);
                w.u64(*extent);
            }
            WalBody::ObjectDelete { object_id, version } => {
                w.bytes16(object_id.as_bytes());
                w.u64(version.0);
            }
            WalBody::ExtentAlloc {
                extent,
                mapping_id,
                physical_extent,
                device_id,
                device_offset,
                length,
            } => {
                w.u64(*extent);
                w.u64(*mapping_id);
                w.u64(*physical_extent);
                w.u32(*device_id);
                w.u64(*device_offset);
                w.u64(*length);
            }
            WalBody::ExtentFree { extent } => w.u64(*extent),
            WalBody::MappingCreate {
                extent,
                mapping_id,
                physical_extent,
            } => {
                w.u64(*extent);
                w.u64(*mapping_id);
                w.u64(*physical_extent);
            }
            WalBody::MappingSwitch {
                extent,
                old_mapping,
                new_mapping,
                reason,
            } => {
                w.u64(*extent);
                w.u64(*old_mapping);
                w.u64(*new_mapping);
                w.u8(*reason);
            }
            WalBody::IndexDelta {
                index_id,
                object_id,
                version,
                op,
            } => {
                w.u64(index_id.0);
                w.bytes16(object_id.as_bytes());
                w.u64(version.0);
                w.u8(*op);
            }
            WalBody::IndexManifest {
                index_id,
                segment_id,
                from_lsn,
                to_lsn,
                watermark_xid,
                watermark_lsn,
            } => {
                w.u64(index_id.0);
                w.bytes16(segment_id);
                w.u64(*from_lsn);
                w.u64(*to_lsn);
                w.u64(*watermark_xid);
                w.u64(*watermark_lsn);
            }
            WalBody::Maintenance {
                extent,
                old_mapping,
                new_mapping,
                ..
            } => {
                w.u64(*extent);
                w.u64(*old_mapping);
                w.u64(*new_mapping);
            }
        }
        w.finish()
    }

    pub fn decode(record_type: RecordType, body: &[u8]) -> KernelResult<Self> {
        let mut r = Reader::new(body);
        let decoded = match record_type {
            RecordType::TxnBegin => WalBody::TxnBegin {
                isolation: r.u8()?,
                snapshot_id: r.u64()?,
                xmin: r.u64()?,
                xmax: r.u64()?,
                active_xids: {
                    let n = r.u32()? as usize;
                    let mut v = Vec::with_capacity(n.min(1 << 20));
                    for _ in 0..n {
                        v.push(r.u64()?);
                    }
                    v
                },
            },
            RecordType::TxnCommit => WalBody::TxnCommit,
            RecordType::TxnAbort => WalBody::TxnAbort,
            RecordType::ObjectCreate => {
                let object_id = ObjectId::from_bytes(r.bytes16()?);
                let object_type = r.u8()?;
                let component_count = r.u16()?;
                WalBody::ObjectCreate {
                    object_id,
                    object_type,
                    component_count,
                }
            }
            RecordType::ObjectVersion => WalBody::ObjectVersion {
                object_id: ObjectId::from_bytes(r.bytes16()?),
                new_version: VersionId(r.u64()?),
                old_version: VersionId(r.u64()?),
                extent: r.u64()?,
            },
            RecordType::ObjectDelete => WalBody::ObjectDelete {
                object_id: ObjectId::from_bytes(r.bytes16()?),
                version: VersionId(r.u64()?),
            },
            RecordType::ExtentAlloc => WalBody::ExtentAlloc {
                extent: r.u64()?,
                mapping_id: r.u64()?,
                physical_extent: r.u64()?,
                device_id: r.u32()?,
                device_offset: r.u64()?,
                length: r.u64()?,
            },
            RecordType::ExtentFree => WalBody::ExtentFree { extent: r.u64()? },
            RecordType::MappingCreate => WalBody::MappingCreate {
                extent: r.u64()?,
                mapping_id: r.u64()?,
                physical_extent: r.u64()?,
            },
            RecordType::MappingSwitch => WalBody::MappingSwitch {
                extent: r.u64()?,
                old_mapping: r.u64()?,
                new_mapping: r.u64()?,
                reason: r.u8()?,
            },
            RecordType::IndexDelta => WalBody::IndexDelta {
                index_id: IndexId(r.u64()?),
                object_id: ObjectId::from_bytes(r.bytes16()?),
                version: VersionId(r.u64()?),
                op: r.u8()?,
            },
            RecordType::IndexManifest => WalBody::IndexManifest {
                index_id: IndexId(r.u64()?),
                segment_id: r.bytes16()?,
                from_lsn: r.u64()?,
                to_lsn: r.u64()?,
                watermark_xid: r.u64()?,
                watermark_lsn: r.u64()?,
            },
            RecordType::MigrationBegin
            | RecordType::MigrationEnd
            | RecordType::CompactionBegin
            | RecordType::CompactionEnd
            | RecordType::CheckpointBegin => WalBody::Maintenance {
                kind: record_type,
                extent: r.u64()?,
                old_mapping: r.u64()?,
                new_mapping: r.u64()?,
            },
        };
        Ok(decoded)
    }
}

impl WalBody {
    /// 记录族（ADR-0013 决策 1）。`None` = 该记录只能走事务路径 `append`。
    ///
    /// 这里维护的是「记录类型 → 族」的唯一映射；`append_derived` 用它校验
    /// 「调用方声明的族」与「记录体想去的族」一致，避免出现
    /// `IndexDelta { .. }` 被标成 `Maintenance` 这类自相矛盾的调用。
    pub fn family(&self) -> Option<DerivedFamily> {
        match self.record_type() {
            RecordType::IndexDelta | RecordType::IndexManifest => Some(DerivedFamily::Index),
            RecordType::MigrationBegin
            | RecordType::MigrationEnd
            | RecordType::CompactionBegin
            | RecordType::CompactionEnd => Some(DerivedFamily::Maintenance),
            _ => None,
        }
    }

    /// 经非事务通道追加。封装「编码 + `append_derived`」两步，调用方只需给出 `body`。
    pub fn append_derived(wal: &dyn WalManager, body: &WalBody) -> KernelResult<Lsn> {
        let family = body.family().ok_or_else(|| {
            KernelError::invalid(format!(
                "{:?} is a transactional record and cannot use append_derived (ADR-0013)",
                body.record_type()
            ))
        })?;
        wal.append_derived(
            &WalRecord::new(body.record_type(), TransactionId(0), Lsn(0), body.encode()),
            family,
        )
    }
}

/// 语义化追加辅助：把 `WalBody` 编码后交给 `WalManager`。
pub fn append_body(
    wal: &dyn WalManager,
    xid: TransactionId,
    prev_lsn: Lsn,
    body: &WalBody,
) -> KernelResult<Lsn> {
    wal.append(&WalRecord::new(
        body.record_type(),
        xid,
        prev_lsn,
        body.encode(),
    ))
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
            return Err(KernelError::corrupted(CorruptionKind::WalRecordCrc));
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_roundtrip() {
        let body = WalBody::ObjectCreate {
            object_id: ObjectId::new(7, 42),
            object_type: 4,
            component_count: 1,
        };
        let rec = WalRecord::new(
            body.record_type(),
            TransactionId(9),
            Lsn(0),
            body.encode(),
        );
        let frame = encode_frame(&rec, Lsn(4096));
        let decoded = decode_frame(&frame, 0, Lsn(4096)).unwrap().unwrap();
        assert_eq!(decoded.record.record_type, RecordType::ObjectCreate);
        assert_eq!(
            WalBody::decode(decoded.record.record_type, &decoded.record.body).unwrap(),
            body
        );
    }

    #[test]
    fn corrupted_body_is_detected() {
        let rec = WalRecord::new(RecordType::TxnCommit, TransactionId(1), Lsn(0), vec![1, 2, 3]);
        let mut frame = encode_frame(&rec, Lsn(0));
        frame[RECORD_HEADER_SIZE] ^= 0xff;
        assert!(decode_frame(&frame, 0, Lsn(0)).unwrap().is_none());
    }

    #[test]
    fn corrupted_crc_is_detected() {
        let rec = WalRecord::new(RecordType::TxnCommit, TransactionId(1), Lsn(0), vec![1, 2, 3]);
        let mut frame = encode_frame(&rec, Lsn(0));
        frame[32] ^= 0xff;
        assert!(decode_frame(&frame, 0, Lsn(0)).unwrap().is_none());
    }

    #[test]
    fn mismatched_lsn_is_detected() {
        let rec = WalRecord::new(RecordType::TxnCommit, TransactionId(1), Lsn(0), vec![]);
        let frame = encode_frame(&rec, Lsn(4096));
        assert!(decode_frame(&frame, 0, Lsn(8192)).unwrap().is_none());
    }
}
