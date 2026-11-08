//! 基于 `IoExecutor` 的分段 WAL。
//!
//! 为什么不用 `std::fs` 直接写文件：WAL 必须经过设备 HAL，
//! 否则故障注入器（位于 `StorageDevice` 之下）无法覆盖 WAL 的 write/flush，
//! 而 WAL 的持久性正是崩溃恢复测试的核心。

use super::{
    align_up, decode_frame, encode_frame, DerivedFamily, WalManager, WalRecord, WalScan, ALIGN,
    FORMAT_VERSION, RECORD_HEADER_SIZE, SEGMENT_HEADER_SIZE, WAL_MAGIC,
};
use crate::checksum::Crc32c;
use crate::error::{CorruptionKind, InternalCode, IoOp, KernelError, KernelResult, ResourceType};
use crate::hal::{DeviceOp, DeviceSite, IoExecutor};
use crate::types::{DeviceId, DurabilityLevel, Lsn, PhysicalLocation, TransactionId};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

#[derive(Clone, Debug)]
pub struct DeviceWalConfig {
    pub device: DeviceId,
    /// WAL 区起始物理偏移。
    pub base_offset: u64,
    /// 段大小，必须是 8 字节对齐且大于段头。
    pub segment_size: u64,
    /// 段数量（WAL 区大小 = segment_size * segment_count）。
    pub segment_count: u64,
}

impl DeviceWalConfig {
    pub fn new(device: DeviceId, base_offset: u64, segment_size: u64, segment_count: u64) -> Self {
        DeviceWalConfig {
            device,
            base_offset,
            segment_size,
            segment_count,
        }
    }
}

struct WalState {
    current_segment: usize,
    /// 当前段内追加偏移（从段头之后开始）。
    append_offset: u64,
    /// 下一条记录的 LSN。
    next_lsn: u64,
    durable_lsn: u64,
    initialized: bool,
}

pub struct DeviceWal {
    io: Arc<dyn IoExecutor>,
    cfg: DeviceWalConfig,
    state: Mutex<WalState>,
    min_required: AtomicU64,
}

impl DeviceWal {
    /// 打开 WAL：扫描已有段，定位追加点。
    pub fn open(io: Arc<dyn IoExecutor>, cfg: DeviceWalConfig) -> KernelResult<Self> {
        if cfg.segment_size <= SEGMENT_HEADER_SIZE as u64 || cfg.segment_size % ALIGN != 0 {
            return Err(KernelError::internal(
                InternalCode::InvariantViolation,
                "invalid WAL segment size",
            ));
        }
        let wal = DeviceWal {
            io,
            cfg,
            state: Mutex::new(WalState {
                current_segment: 0,
                append_offset: SEGMENT_HEADER_SIZE as u64,
                next_lsn: 0,
                durable_lsn: 0,
                initialized: false,
            }),
            min_required: AtomicU64::new(0),
        };
        wal.recover_append_position()?;
        Ok(wal)
    }

    fn segment_start(&self, seg: usize) -> u64 {
        self.cfg.base_offset + seg as u64 * self.cfg.segment_size
    }

    fn location(&self, offset: u64, length: u64) -> PhysicalLocation {
        PhysicalLocation {
            device: self.cfg.device,
            offset,
            length,
        }
    }

    fn read_bytes(&self, offset: u64, len: usize) -> KernelResult<Vec<u8>> {
        let mut buf = vec![0u8; len];
        self.io.read_into(
            DeviceSite::WalFrameWrite,
            &self.location(offset, len as u64),
            &mut buf,
        )?;
        Ok(buf)
    }

    /// WAL 区的所有 write 共用 `WalFrameWrite`：
    /// 段头的 write 同样属于 `wal.frame.partial` 的注入点（ADR-0011 迁移说明）。
    fn write_bytes(&self, offset: u64, data: &[u8]) -> KernelResult<()> {
        self.io.submit_and_wait(
            DeviceSite::WalFrameWrite,
            DeviceOp::Write {
                site: DeviceSite::WalFrameWrite,
                loc: self.location(offset, data.len() as u64),
                data: data.to_vec(),
            },
        )?;
        Ok(())
    }

    /// 写一帧记录：site 由记录类型决定（`TxnCommit` 帧是 `txn.commit.before_flush`，
    /// 其余帧是 `wal.frame.partial` / `wal.frame.written`）。
    fn write_frame(&self, offset: u64, frame: &[u8], site: DeviceSite) -> KernelResult<()> {
        self.io.submit_and_wait(
            site,
            DeviceOp::Write {
                site,
                loc: self.location(offset, frame.len() as u64),
                data: frame.to_vec(),
            },
        )?;
        Ok(())
    }

    fn flush_segment(&self, seg: usize) -> KernelResult<()> {
        self.io.submit_and_wait(
            DeviceSite::WalFlushStart,
            DeviceOp::Flush {
                site: DeviceSite::WalFlushStart,
                loc: self.location(self.segment_start(seg), self.cfg.segment_size),
                level: DurabilityLevel::Durable,
            },
        )?;
        Ok(())
    }

    fn encode_segment_header(&self, seg: usize) -> Vec<u8> {
        let start = self.segment_start(seg);
        let prev = if seg == 0 {
            0
        } else {
            self.segment_start(seg - 1)
        };
        let mut h = vec![0u8; SEGMENT_HEADER_SIZE];
        h[0..4].copy_from_slice(&WAL_MAGIC.to_be_bytes());
        h[4..6].copy_from_slice(&FORMAT_VERSION.to_be_bytes());
        h[6..8].copy_from_slice(&0u16.to_be_bytes());
        h[8..16].copy_from_slice(&start.to_be_bytes());
        h[16..24].copy_from_slice(&prev.to_be_bytes());
        h[24..32].copy_from_slice(&0u64.to_be_bytes()); // created_unix_ns（内核不读墙钟）
        let crc = crate::checksum::crc32c(&h[0..32]);
        h[32..36].copy_from_slice(&crc.to_be_bytes());
        h
    }

    fn init_segment(&self, seg: usize) -> KernelResult<()> {
        let header = self.encode_segment_header(seg);
        self.write_bytes(self.segment_start(seg), &header)?;
        self.flush_segment(seg)
    }

    fn valid_segment_header(buf: &[u8]) -> bool {
        if buf.len() < 36 {
            return false;
        }
        let magic = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]);
        if magic != WAL_MAGIC {
            return false;
        }
        let crc = u32::from_be_bytes([buf[32], buf[33], buf[34], buf[35]]);
        let mut c = Crc32c::new();
        c.update(&buf[0..32]);
        c.finish() == crc
    }

    /// 扫描全部段，找到最后一条有效记录的末尾作为追加点。
    fn recover_append_position(&self) -> KernelResult<()> {
        let mut st = self.state.lock().unwrap();
        let mut last_seg = 0usize;
        let mut last_end = SEGMENT_HEADER_SIZE as u64;
        let mut saw_valid = false;

        for seg in 0..self.cfg.segment_count as usize {
            let start = self.segment_start(seg);
            let header = self.read_bytes(start, SEGMENT_HEADER_SIZE)?;
            if !Self::valid_segment_header(&header) {
                break;
            }
            saw_valid = true;
            last_seg = seg;
            let mut off = SEGMENT_HEADER_SIZE;
            let mut last_valid_in_seg = SEGMENT_HEADER_SIZE as u64;
            // 只在段内扫描到最后一个有效帧。
            loop {
                if off as u64 + super::RECORD_HEADER_SIZE as u64 > self.cfg.segment_size {
                    break;
                }
                let remaining = (self.cfg.segment_size - off as u64) as usize;
                let chunk = self.read_bytes(start + off as u64, remaining)?;
                let expected = Lsn(start + off as u64);
                match decode_frame(&chunk, 0, expected)? {
                    Some(f) => {
                        off += f.total_len;
                        last_valid_in_seg = off as u64;
                    }
                    None => break,
                }
            }
            last_end = last_valid_in_seg;
        }

        if !saw_valid {
            // 全新设备：初始化第 0 段。
            self.init_segment(0)?;
            st.current_segment = 0;
            st.append_offset = SEGMENT_HEADER_SIZE as u64;
            st.next_lsn = self.segment_start(0) + SEGMENT_HEADER_SIZE as u64;
            st.durable_lsn = st.next_lsn;
            st.initialized = true;
            return Ok(());
        }

        // 如果最后一段已写满，滚动到下一段。
        let mut seg = last_seg;
        let mut off = last_end;
        if off + super::RECORD_HEADER_SIZE as u64 + 8 > self.cfg.segment_size {
            let next = seg + 1;
            if next >= self.cfg.segment_count as usize {
                return Err(KernelError::ResourceExhausted {
                    resource: ResourceType::Nvme,
                });
            }
            self.init_segment(next)?;
            seg = next;
            off = SEGMENT_HEADER_SIZE as u64;
        }
        st.current_segment = seg;
        st.append_offset = off;
        st.next_lsn = self.segment_start(seg) + off;
        st.durable_lsn = st.next_lsn;
        st.initialized = true;
        Ok(())
    }

    fn roll_segment(&self, st: &mut WalState, need: u64) -> KernelResult<()> {
        let next = st.current_segment + 1;
        if next >= self.cfg.segment_count as usize {
            return Err(KernelError::ResourceExhausted {
                resource: ResourceType::Nvme,
            });
        }
        // 老段必须在滚动前持久，否则 flush_upto 的语义会漏掉旧段。
        self.flush_segment(st.current_segment)?;
        self.init_segment(next)?;
        st.current_segment = next;
        st.append_offset = SEGMENT_HEADER_SIZE as u64;
        st.next_lsn = self.segment_start(next) + st.append_offset;
        if st.append_offset + need > self.cfg.segment_size {
            return Err(KernelError::ResourceExhausted {
                resource: ResourceType::Nvme,
            });
        }
        Ok(())
    }
}

impl DeviceWal {
    /// 追加的实现：`append` 与 `append_derived` 共用。
    fn append_frame(&self, rec: &WalRecord) -> KernelResult<Lsn> {
        let mut st = self.state.lock().unwrap();
        // 帧长必须与 `encode_frame` 完全一致：头固定 36 字节，body 单独补齐到 8 字节。
        // 若这里用 align_up(头 + body)，会多算 4 字节，导致后续帧的 LSN 与扫描期望错位。
        let frame_len = super::RECORD_HEADER_SIZE as u64 + align_up(rec.body.len() as u64, ALIGN);
        if frame_len > self.cfg.segment_size - SEGMENT_HEADER_SIZE as u64 {
            return Err(KernelError::ResourceExhausted {
                resource: ResourceType::Nvme,
            });
        }
        if st.append_offset + frame_len > self.cfg.segment_size {
            self.roll_segment(&mut st, frame_len)?;
        }
        let lsn = Lsn(self.segment_start(st.current_segment) + st.append_offset);
        let frame = encode_frame(rec, lsn);
        let phys = self.segment_start(st.current_segment) + st.append_offset;
        self.write_frame(phys, &frame, frame_site(rec.record_type))?;
        st.append_offset += frame_len;
        st.next_lsn = self.segment_start(st.current_segment) + st.append_offset;
        Ok(lsn)
    }
}

impl WalManager for DeviceWal {
    fn append(&self, rec: &WalRecord) -> KernelResult<Lsn> {
        self.append_frame(rec)
    }

    fn append_derived(&self, rec: &WalRecord, family: DerivedFamily) -> KernelResult<Lsn> {
        // 校验全部在任何写入之前完成（ADR-0013 决策 1）：不匹配的调用必须零副作用。
        if !family.accepts(rec.record_type) {
            return Err(KernelError::invalid(format!(
                "{:?} family may not carry {:?} records",
                family, rec.record_type
            )));
        }
        if rec.xid != TransactionId(0) {
            return Err(KernelError::invalid(format!(
                "derived records are non-transactional, got xid {}",
                rec.xid
            )));
        }
        if rec.prev_lsn != Lsn(0) {
            return Err(KernelError::invalid(
                "derived records do not join a transaction's prev_lsn chain",
            ));
        }
        self.append_frame(rec)
    }

    fn flush_upto(&self, target: Lsn) -> KernelResult<()> {
        let mut st = self.state.lock().unwrap();
        if target <= Lsn(st.durable_lsn) {
            return Ok(());
        }
        // 当前段一次 fsync 覆盖本段全部追加；已滚动段在 roll 时已 flush。
        self.flush_segment(st.current_segment)?;
        st.durable_lsn = st.next_lsn;
        Ok(())
    }

    fn durable_lsn(&self) -> Lsn {
        Lsn(self.state.lock().unwrap().durable_lsn)
    }

    fn scan_from(&self, from: Lsn) -> KernelResult<WalScan> {
        // 不持状态锁做 I/O（读路径）。
        let mut out = WalScan {
            records: Vec::new(),
            last_valid_lsn: from,
            stopped_at_corruption: false,
        };
        if from.0 < self.cfg.base_offset {
            return Ok(out);
        }
        let first_seg = ((from.0 - self.cfg.base_offset) / self.cfg.segment_size) as usize;
        for seg in first_seg..self.cfg.segment_count as usize {
            let start = self.segment_start(seg);
            let header = self.read_bytes(start, SEGMENT_HEADER_SIZE)?;
            if !Self::valid_segment_header(&header) {
                // 尚未初始化的段（段头全零）是正常的 WAL 尾部，不是损坏。
                let uninitialized = header.iter().all(|b| *b == 0);
                if !uninitialized {
                    out.stopped_at_corruption = true;
                }
                break;
            }
            // begin 是段内偏移；只有当 from 落在本段内时才跳过前缀。
            let begin = if start < from.0 {
                (from.0 - start) as usize
            } else {
                SEGMENT_HEADER_SIZE
            };
            if begin as u64 >= self.cfg.segment_size {
                continue;
            }
            let chunk = self.read_bytes(
                start + begin as u64,
                (self.cfg.segment_size - begin as u64) as usize,
            )?;
            let mut off = 0usize;
            loop {
                let expected = Lsn(start + begin as u64 + off as u64);
                match decode_frame(&chunk, off, expected)? {
                    Some(f) => {
                        off += f.total_len;
                        out.last_valid_lsn = Lsn(expected.0 + f.total_len as u64);
                        out.records.push(super::WalRecordAt {
                            lsn: expected,
                            record: f.record,
                        });
                    }
                    None => {
                        // 全零 = 段尾（正常）。但若剩余空间足够一帧、且首 4 字节像
                        // 一个非零长度，就说明读到了残缺帧，这才是真损坏。
                        let rest = &chunk[off..];
                        let looks_like_torn_frame =
                            rest.len() >= RECORD_HEADER_SIZE && rest[0..4].iter().any(|b| *b != 0);
                        if looks_like_torn_frame {
                            out.stopped_at_corruption = true;
                        }
                        break;
                    }
                }
            }
            if out.stopped_at_corruption {
                break;
            }
        }
        Ok(out)
    }

    fn min_required_lsn(&self) -> Lsn {
        Lsn(self.min_required.load(Ordering::SeqCst))
    }

    fn set_min_required_lsn(&self, lsn: Lsn) {
        self.min_required.store(lsn.0, Ordering::SeqCst);
    }
}

/// 记录类型 → 写入该帧时的设备级 site（05 §3.2 目录）。
///
/// 只有 `TxnCommit` 帧对应 `txn.commit.before_flush`；其余记录（含段头）都是
/// `wal.frame.partial` / `wal.frame.written`。索引层记录 0x30 / 0x31 由索引层
/// 自己的写路径打标签，但仍经过同一个 `IoExecutor`（06 §6.3）。
fn frame_site(record_type: super::RecordType) -> DeviceSite {
    match record_type {
        super::RecordType::TxnCommit => DeviceSite::TxnCommitWrite,
        _ => DeviceSite::WalFrameWrite,
    }
}

#[allow(dead_code)]
fn corrupted() -> KernelError {
    KernelError::corrupted(CorruptionKind::WalRecordCrc)
}

#[allow(dead_code)]
fn io_op(op: IoOp) -> IoOp {
    op
}
