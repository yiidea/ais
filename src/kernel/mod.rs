//! 存储内核：对象、extent、映射、事务、检查点、恢复与回收。
//!
//! 接口按 [04-kernel-interfaces](../chatgpt/04-kernel-interfaces.md) 冻结清单实现。
//! MVP 的实现取舍：
//! - 只实现快照隔离（04 §8 T5）；
//! - 写路径用一把 writer 锁串行化，读路径只短暂持状态锁；
//! - payload I/O 不在状态锁内完成（X1），WAL append/flush 允许持锁（X1.1）。

use crate::clock::{Clock, SystemClock};
use crate::error::{
    ConflictKind, CorruptionKind, IdKind, InternalCode, KernelError, KernelResult, ResourceType,
};
use crate::hal::qd::QueueDepth;
use crate::hal::{
    DeviceOp, DeviceSite, IoExecutor, ReactorIoExecutor, StorageDevice, SyncIoExecutor,
};
use crate::types::*;
use crate::wal::{DeviceWal, DeviceWalConfig, WalBody, WalManager};
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub mod checkpoint;
pub mod feed;
pub mod gc;
pub mod live;
pub mod object;
pub mod state;
pub mod txn;

pub use feed::{ChangeBatch, ChangeBatchSpec, ChangeFeed, ObjectChange, ObjectMeta};
pub use live::{EntryVisibility, LiveSnapshotHandle, LiveSnapshotSource};
pub use state::{
    ExtentEntry, ExtentState, KernelState, MappingEntry, MappingRecord, MappingState, ObjectEntry,
    PhysicalExtentEntry, VersionEntry,
};
pub use txn::{Snapshot, TxnCtx, TxnStatusMap, WriteSet};

// ---------------------------------------------------------------------------
// 请求/响应类型
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct CreateObjectRequest {
    pub object_type: ObjectType,
    pub payload: Vec<u8>,
    pub component_count: u16,
}

#[derive(Clone, Debug)]
pub struct VersionSpec {
    pub payload: Vec<u8>,
}

#[derive(Clone, Debug)]
pub struct ExtentSpec {
    pub object_id: ObjectId,
    pub component: ComponentId,
    pub length: u64,
    pub layout: LayoutType,
}

#[derive(Clone, Debug)]
pub struct PhysicalAllocRequest {
    pub device: DeviceId,
    pub length: u64,
}

#[derive(Clone, Copy, Debug)]
pub struct CommitReceipt {
    pub xid: TransactionId,
    pub commit_lsn: Lsn,
}

#[derive(Clone, Debug)]
pub struct CheckpointInfo {
    pub id: CheckpointId,
    pub lsn: Lsn,
}

#[derive(Clone, Debug, Default)]
pub struct ReclaimPlan {
    pub extents: Vec<ExtentId>,
    pub wal_before: Lsn,
}

#[derive(Clone, Copy, Debug)]
pub struct ScanBudget {
    pub max_candidates: usize,
}

#[derive(Clone, Debug)]
pub struct ReclaimCandidate {
    pub extent: ExtentId,
}

#[derive(Clone, Debug)]
pub struct ReclaimDecision {
    pub reclaimable: bool,
    pub reasons: Vec<String>,
}

#[derive(Clone, Debug)]
pub enum VerifyScope {
    Full,
    Object(ObjectId),
}

#[derive(Clone, Debug, Default)]
pub struct VerifyReport {
    pub issues: Vec<String>,
}

impl VerifyReport {
    pub fn ok(&self) -> bool {
        self.issues.is_empty()
    }
}

#[derive(Clone, Debug)]
pub struct RecoveredState {
    pub checkpoint: CheckpointInfo,
    pub replayed_records: usize,
    pub aborted_txns: Vec<TransactionId>,
}

// ---------------------------------------------------------------------------
// 冻结的 trait 形状（04 §3–§11）
// ---------------------------------------------------------------------------

pub trait ObjectManager: Send + Sync {
    fn create_object(&self, txn: &mut TxnCtx, req: &CreateObjectRequest) -> KernelResult<ObjectId>;
    fn get_header(&self, id: ObjectId) -> KernelResult<ObjectHeader>;
    fn resolve_version(&self, id: ObjectId, snap: &Snapshot) -> KernelResult<VersionId>;
    fn read_version(&self, id: ObjectId, ver: VersionId) -> KernelResult<ObjectView>;
    fn stage_version(
        &self,
        txn: &mut TxnCtx,
        id: ObjectId,
        spec: &VersionSpec,
    ) -> KernelResult<VersionId>;
    fn delete_object(&self, txn: &mut TxnCtx, id: ObjectId) -> KernelResult<()>;
}

pub trait ExtentManager: Send + Sync {
    fn split(
        &self,
        id: ObjectId,
        comp: ComponentId,
        layout: LayoutType,
    ) -> KernelResult<Vec<ExtentId>>;
    fn allocate_logical(&self, txn: &mut TxnCtx, spec: &ExtentSpec) -> KernelResult<ExtentId>;
    fn write_payload(
        &self,
        ext: ExtentId,
        data: &PayloadRef,
        dur: DurabilityLevel,
    ) -> KernelResult<()>;
    fn read_payload(&self, ext: ExtentId, range: std::ops::Range<u64>) -> KernelResult<PayloadRef>;
    fn release(&self, ext: ExtentId) -> KernelResult<()>;
}

pub trait MappingManager: Send + Sync {
    fn resolve(&self, ext: ExtentId, mv: MappingVersion) -> KernelResult<PhysicalLocation>;
    fn create_mapping(
        &self,
        txn: &mut TxnCtx,
        ext: ExtentId,
        pe: PhysicalExtentId,
    ) -> KernelResult<MappingId>;
    fn switch_mapping(
        &self,
        txn: &mut TxnCtx,
        ext: ExtentId,
        expected: MappingVersion,
        new: MappingId,
    ) -> KernelResult<MappingVersion>;
    fn current_version(&self, ext: ExtentId) -> KernelResult<MappingVersion>;
}

pub trait PhysicalExtentManager: Send + Sync {
    fn allocate(
        &self,
        txn: &mut TxnCtx,
        req: &PhysicalAllocRequest,
    ) -> KernelResult<PhysicalExtentId>;
    fn release(&self, pe: PhysicalExtentId) -> KernelResult<()>;
    fn free_space(&self, device: DeviceId) -> KernelResult<u64>;
}

pub trait TransactionManager: Send + Sync {
    fn begin(&self, mode: IsolationLevel) -> KernelResult<TxnCtx>;
    fn commit(&self, txn: &mut TxnCtx) -> KernelResult<CommitReceipt>;
    fn abort(&self, txn: &mut TxnCtx) -> KernelResult<()>;
    fn visible(&self, ver: &ObjectVersion, snap: &Snapshot) -> bool;
    fn status(&self, xid: TransactionId) -> TxnStatus;
}

pub trait CheckpointManager: Send + Sync {
    fn trigger(&self) -> KernelResult<CheckpointId>;
    fn current(&self) -> KernelResult<CheckpointInfo>;
    fn reclaim_candidates(&self) -> KernelResult<ReclaimPlan>;
}

pub trait RecoveryManager: Send + Sync {
    fn recover(&self) -> KernelResult<RecoveredState>;
    fn verify(&self, scope: VerifyScope) -> KernelResult<VerifyReport>;
}

pub trait Reclaimer: Send + Sync {
    fn scan(&self, budget: ScanBudget) -> KernelResult<Vec<ReclaimCandidate>>;
    fn judge(&self, cand: &ReclaimCandidate) -> KernelResult<ReclaimDecision>;
    fn reclaim(&self, cand: &ReclaimCandidate) -> KernelResult<()>;
}

/// 迁移与内存资源层在 MVP 中只保留 trait 形状（README §7 明确第一版不实现）。
pub trait MigrationManager: Send + Sync {
    fn plan(&self, ext: ExtentId, target: ResourceType) -> KernelResult<()>;
    fn execute(&self, ext: ExtentId) -> KernelResult<()>;
    fn status(&self, ext: ExtentId) -> KernelResult<()>;
}

pub trait MemoryTierManager: Send + Sync {
    fn allocate(&self, resource: ResourceType, bytes: u64) -> KernelResult<AllocationId>;
    fn free(&self, alloc: AllocationId) -> KernelResult<()>;
    fn stats(&self, resource: ResourceType) -> KernelResult<(u64, u64)>;
}

// ---------------------------------------------------------------------------
// Kernel 配置与结构
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct KernelConfig {
    pub database_id: u32,
    /// WAL 区起始物理偏移。
    pub wal_base_offset: u64,
    pub wal_segment_size: u64,
    pub wal_segment_count: u64,
    /// payload 区起始物理偏移（必须 ≥ WAL 区末尾）。
    pub payload_base_offset: u64,
    /// 检查点影子文件路径。
    pub checkpoint_path: PathBuf,
    pub clock: Arc<dyn Clock>,
    /// 用常驻 reactor 执行设备操作（04 §6.3 I1）。`None` = 同步 façade。
    ///
    /// 同步 façade 同一时刻至多一个在飞操作，QD 上限与背压在它上面不会生效；
    /// 要真正吃到并发与队列深度，就必须挂 reactor（[`ReactorIoExecutor`]）。
    pub reactor: Option<ReactorSpec>,
}

/// reactor 的构造参数（07 §4）。QD 由 [`QueueDepth`] 推导，不在这里直接给数。
#[derive(Clone, Debug)]
pub struct ReactorSpec {
    pub qd: QueueDepth,
    /// 该设备的延迟 SLO（纳秒），§4.3 的 `lat_p99 ≤ 0.8 × SLO` 用它。
    pub slo_nanos: u64,
    /// 监控窗口长度；QD 自适应与 `io.qd.adjustments` 都按窗口计。
    pub window: Duration,
}

impl ReactorSpec {
    /// 默认窗口 100ms、SLO 100µs——都是部署参数，按 B3 实测再调。
    pub fn new(qd: QueueDepth) -> Self {
        ReactorSpec {
            qd,
            slo_nanos: 100_000,
            window: Duration::from_millis(100),
        }
    }
}

impl KernelConfig {
    /// 常用默认：WAL 2MB，payload 从 2MB 开始。
    pub fn new(checkpoint_path: impl Into<PathBuf>) -> Self {
        let wal_base_offset = 0;
        let wal_segment_size = 64 * 1024;
        let wal_segment_count = 32;
        KernelConfig {
            database_id: 1,
            wal_base_offset,
            wal_segment_size,
            wal_segment_count,
            payload_base_offset: wal_base_offset + wal_segment_size * wal_segment_count,
            checkpoint_path: checkpoint_path.into(),
            clock: Arc::new(SystemClock::new()),
            reactor: None,
        }
    }

    pub fn with_clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.clock = clock;
        self
    }
}

pub struct Kernel {
    pub(crate) state: Mutex<KernelState>,
    pub(crate) wal: Arc<dyn WalManager>,
    pub(crate) io: Arc<dyn IoExecutor>,
    pub(crate) clock: Arc<dyn Clock>,
    pub(crate) cfg: KernelConfig,
    /// 写路径串行化（MVP 取舍；生产实现改为每对象写意向锁）。
    pub(crate) writer: Mutex<()>,
    pub(crate) checkpoint_guard: Mutex<()>,
    pub(crate) active: Mutex<BTreeSet<TransactionId>>,
    /// 存活快照集合（F17，ADR-0015 决策 4）：事务快照与交出去的
    /// [`LiveSnapshotHandle`] 都在这里登记，是索引段回收判定唯一的输入。
    pub(crate) live: Mutex<BTreeMap<SnapshotId, Snapshot>>,
    pub(crate) next_snapshot: AtomicU64,
    /// 记录 checkpoint 发布后需要保留的最小 LSN。
    pub(crate) min_required: AtomicU64,
}

impl Kernel {
    pub fn open(config: KernelConfig, device: Box<dyn StorageDevice>) -> KernelResult<Self> {
        match &config.reactor {
            None => Self::open_with_io(config, Arc::new(SyncIoExecutor::new(device))),
            Some(spec) => {
                let io = Arc::new(ReactorIoExecutor::new(
                    Arc::from(device),
                    &spec.qd,
                    Arc::clone(&config.clock),
                    spec.slo_nanos,
                    spec.window,
                ));
                Self::open_with_io(config, io)
            }
        }
    }

    /// 用外部提供的 `IoExecutor` 打开内核。
    ///
    /// 生产路径走 [`Kernel::open`]（内部建同步 façade）；测试夹具用它挂在
    /// 注入型执行器上（05 §5 的 L2 崩溃层）。这不是冻结合同的一部分：
    /// 内核对外仍然只通过 `IoExecutor` 触达设备（04 §6.3 I3），换个实现不改变任何语义。
    pub fn open_with_io(config: KernelConfig, io: Arc<dyn IoExecutor>) -> KernelResult<Self> {
        let wal = Arc::new(DeviceWal::open(
            Arc::clone(&io),
            DeviceWalConfig::new(
                io.descriptor().id,
                config.wal_base_offset,
                config.wal_segment_size,
                config.wal_segment_count,
            ),
        )?);
        let state = Self::load_checkpoint(&config)?;
        let kernel = Kernel {
            state: Mutex::new(state),
            wal,
            io,
            clock: Arc::clone(&config.clock),
            cfg: config,
            writer: Mutex::new(()),
            checkpoint_guard: Mutex::new(()),
            active: Mutex::new(BTreeSet::new()),
            live: Mutex::new(BTreeMap::new()),
            next_snapshot: AtomicU64::new(1),
            min_required: AtomicU64::new(0),
        };
        // 启动恢复：加载检查点 + 重放 WAL。
        kernel.recover()?;
        Ok(kernel)
    }

    pub fn wal(&self) -> &dyn WalManager {
        self.wal.as_ref()
    }

    pub fn io(&self) -> &dyn IoExecutor {
        self.io.as_ref()
    }

    pub fn clock(&self) -> &dyn Clock {
        self.clock.as_ref()
    }

    pub fn config(&self) -> &KernelConfig {
        &self.cfg
    }

    // ---- 供引擎使用、但不进入冻结 trait 的读路径 -------------------------

    /// 读取 extent 的 payload 字节。trait 的 `read_payload` 返回逻辑引用；
    /// 真正取数据需要映射解析 + 设备读，这里作为内核内部便捷入口。
    pub fn read_extent_bytes(&self, ext: ExtentId) -> KernelResult<Vec<u8>> {
        let loc = {
            let st = self.state.lock().unwrap();
            let e = st
                .extents
                .get(&ext)
                .ok_or_else(|| KernelError::not_found(IdKind::Extent, ext))?;
            if e.state == ExtentState::Freed {
                return Err(KernelError::not_found(IdKind::Extent, ext));
            }
            let m = st
                .mappings
                .get(&ext)
                .ok_or_else(|| KernelError::not_found(IdKind::Mapping, ext))?;
            let rec = m
                .records
                .iter()
                .find(|r| r.version == m.current_version)
                .ok_or_else(|| {
                    KernelError::internal(
                        InternalCode::InvariantViolation,
                        "mapping version missing",
                    )
                })?;
            let pe = st.physical.get(&rec.physical).ok_or_else(|| {
                KernelError::internal(InternalCode::InvariantViolation, "physical extent missing")
            })?;
            PhysicalLocation {
                device: pe.device,
                offset: pe.offset,
                length: pe.length,
            }
        };
        let mut buf = vec![0u8; loc.length as usize];
        self.io
            .read_into(DeviceSite::ExtentPayloadWrite, &loc, &mut buf)?;
        Ok(buf)
    }

    /// 计算 payload 区中某设备的已用水位（运维/测试用）。
    pub fn device_watermark(&self, device: DeviceId) -> u64 {
        self.state
            .lock()
            .unwrap()
            .device_next_offset
            .get(&device)
            .copied()
            .unwrap_or(self.cfg.payload_base_offset)
    }

    // ---- 内部辅助 ---------------------------------------------------------

    fn load_checkpoint(config: &KernelConfig) -> KernelResult<KernelState> {
        match std::fs::read(&config.checkpoint_path) {
            // 检查点文件存在但校验失败时不允许静默回退：那会丢数据。
            // 完整实现会回退到 prev_checkpoint_id（03 §6.1 step 2），MVP 只有一个文件。
            Ok(bytes) => KernelState::decode(&bytes),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(KernelState::default()),
            Err(e) => Err(KernelError::io(crate::error::IoOp::Read, e)),
        }
    }

    pub(crate) fn append(&self, txn: &mut TxnCtx, body: &WalBody) -> KernelResult<Lsn> {
        let lsn = self.wal.append(&crate::wal::WalRecord::new(
            body.record_type(),
            txn.xid,
            txn.prev_lsn,
            body.encode(),
        ))?;
        txn.prev_lsn = lsn;
        Ok(lsn)
    }

    /// 为 payload 分配逻辑 + 物理 extent（bump 分配器）。
    pub(crate) fn allocate_payload_extent(
        &self,
        st: &mut KernelState,
        txn: &mut TxnCtx,
        object_id: ObjectId,
        length: u64,
    ) -> KernelResult<(ExtentId, PhysicalExtentId)> {
        let device = self.io.descriptor().id;
        let offset = st
            .device_next_offset
            .get(&device)
            .copied()
            .unwrap_or(self.cfg.payload_base_offset);
        if offset.saturating_add(length) > self.io.descriptor().capacity {
            return Err(KernelError::ResourceExhausted {
                resource: ResourceType::Nvme,
            });
        }
        let extent = st.alloc_extent();
        let physical = st.alloc_physical();
        st.device_next_offset.insert(device, offset + length);
        st.physical.insert(
            physical,
            PhysicalExtentEntry {
                id: physical,
                device,
                offset,
                length,
                freed: false,
            },
        );
        st.extents.insert(
            extent,
            ExtentEntry {
                extent,
                object_id,
                physical: Some(physical),
                length,
                alloc_xid: txn.xid,
                alloc_lsn: Lsn(0),
                state: ExtentState::Allocated,
            },
        );
        let mapping = st.alloc_mapping();
        st.mappings.insert(
            extent,
            MappingEntry {
                extent,
                current_version: MappingVersion(1),
                current_mapping: mapping,
                records: vec![MappingRecord {
                    mapping,
                    version: MappingVersion(1),
                    physical,
                    state: MappingState::Active,
                }],
            },
        );
        txn.pending_extents.push(extent);
        txn.pending_mappings.push((extent, mapping));
        txn.write_set.extent_allocs.push(extent);
        txn.write_set.mapping_changes.push((extent, mapping));
        Ok((extent, physical))
    }

    pub(crate) fn write_payload_durable(
        &self,
        pe: PhysicalExtentId,
        data: &[u8],
    ) -> KernelResult<()> {
        let loc = {
            let st = self.state.lock().unwrap();
            let p = st
                .physical
                .get(&pe)
                .ok_or_else(|| KernelError::not_found(IdKind::Extent, pe))?;
            PhysicalLocation {
                device: p.device,
                offset: p.offset,
                length: p.length,
            }
        };
        if data.len() as u64 > loc.length {
            return Err(KernelError::internal(
                InternalCode::InvariantViolation,
                "payload larger than allocated extent",
            ));
        }
        let mut padded = data.to_vec();
        padded.resize(loc.length as usize, 0);
        self.io.submit_and_wait(
            DeviceSite::ExtentPayloadWrite,
            DeviceOp::Write {
                site: DeviceSite::ExtentPayloadWrite,
                loc,
                data: padded,
            },
        )?;
        // data barrier：提交前 payload 必须达到 Durable（E1 / T2.1）。
        self.io.submit_and_wait(
            DeviceSite::ExtentPayloadWrite,
            DeviceOp::Flush {
                site: DeviceSite::ExtentPayloadWrite,
                loc,
                level: DurabilityLevel::Durable,
            },
        )?;
        Ok(())
    }

    pub(crate) fn check_txn_running(&self, txn: &TxnCtx) -> KernelResult<()> {
        if txn.state != TxnStatus::Running {
            return Err(KernelError::internal(
                InternalCode::StateMachineViolation,
                format!("transaction {} is {:?}", txn.xid, txn.state),
            ));
        }
        Ok(())
    }

    /// 版本链比较：只在同一 ObjectId 内有效（F1.2）。
    pub fn compare_versions(&self, id: ObjectId, a: VersionId, b: VersionId) -> std::cmp::Ordering {
        let _ = id; // 作用域参数：调用方保证 a/b 属于同一对象
        a.0.cmp(&b.0)
    }

    /// 不开启事务、不写 WAL 的只读快照（引擎读路径与索引重建使用）。
    pub fn fresh_snapshot(&self) -> Snapshot {
        let st = self.state.lock().unwrap();
        let active = self.active.lock().unwrap().clone();
        let xmax = st.xmax.max(1);
        let xmin = active.iter().map(|x| x.0).min().unwrap_or(xmax).min(xmax);
        Snapshot {
            id: SnapshotId(self.next_snapshot.fetch_add(1, Ordering::SeqCst)),
            xmin: TransactionId(xmin),
            xmax: TransactionId(xmax),
            active,
        }
    }

    pub fn all_object_ids(&self) -> Vec<ObjectId> {
        self.state.lock().unwrap().objects.keys().copied().collect()
    }

    /// 读**指定版本**的两个 xid（08 §2.2：索引条目的 `created_by_xid` / `deleted_by_xid`）。
    ///
    /// 与 `read_version` 的区别：它按版本号直取，不走快照——索引构建器读的是变更
    /// 指定的那个版本，不是「快照下的可见版本」（06 §2.2）。
    ///
    /// 删除方向（06 §2.6）：对象级 `deleted_xid` 可能晚于**墓碑目标版本**，
    /// 因此对变更指定的版本，优先取版本自身的 `xmax`；版本从未被取代时，
    /// 才回落到对象级删除事务——那才是「这个版本被删除」的真实事务号。
    pub fn version_xids(
        &self,
        id: ObjectId,
        ver: VersionId,
    ) -> Option<(TransactionId, Option<TransactionId>)> {
        let st = self.state.lock().unwrap();
        let o = st.objects.get(&id)?;
        let v = o.versions.iter().find(|v| v.version == ver)?;
        Some((v.xmin, v.xmax.or(o.deleted_xid)))
    }
}

impl TransactionManager for Kernel {
    fn begin(&self, mode: IsolationLevel) -> KernelResult<TxnCtx> {
        if mode != IsolationLevel::Snapshot {
            return Err(KernelError::Unsupported {
                feature: "only snapshot isolation in v1",
            });
        }
        let mut st = self.state.lock().unwrap();
        let xid = TransactionId(st.xmax.max(1));
        st.xmax = xid.0 + 1;
        st.txn_status.insert(xid, TxnStatus::Running);

        let snapshot = {
            // 快照必须在加入本事务之前采样。
            let active = self.active.lock().unwrap().clone();
            let xmin = active.iter().map(|x| x.0).min().unwrap_or(xid.0).min(xid.0);
            Snapshot {
                id: SnapshotId(self.next_snapshot.fetch_add(1, Ordering::SeqCst)),
                xmin: TransactionId(xmin),
                xmax: TransactionId(xid.0),
                active: active.clone(),
            }
        };
        let mut txn = TxnCtx::new(xid, snapshot, Lsn(0), mode);
        let body = WalBody::TxnBegin {
            isolation: 0,
            snapshot_id: txn.snapshot.id.0,
            xmin: txn.snapshot.xmin.0,
            xmax: txn.snapshot.xmax.0,
            active_xids: txn.snapshot.active.iter().map(|x| x.0).collect(),
        };
        let lsn = self.append(&mut txn, &body)?;
        txn.start_lsn = lsn;
        self.active.lock().unwrap().insert(xid);
        // 快照在事务存活期间一直算「存活快照」（F17）。
        self.live
            .lock()
            .unwrap()
            .insert(txn.snapshot.id, txn.snapshot.clone());
        Ok(txn)
    }

    fn commit(&self, txn: &mut TxnCtx) -> KernelResult<CommitReceipt> {
        self.check_txn_running(txn)?;
        let _writer = self.writer.lock().unwrap();
        let mut st = self.state.lock().unwrap();

        // 1. 依赖记录：ExtentAlloc / MappingCreate。
        for ext in txn.pending_extents.clone() {
            let e = st
                .extents
                .get(&ext)
                .ok_or_else(|| KernelError::not_found(IdKind::Extent, ext))?;
            let pe_id = e.physical.ok_or_else(|| {
                KernelError::internal(InternalCode::InvariantViolation, "extent without physical")
            })?;
            let p = st.physical.get(&pe_id).ok_or_else(|| {
                KernelError::internal(InternalCode::InvariantViolation, "physical missing")
            })?;
            let body = WalBody::ExtentAlloc {
                extent: ext.0,
                mapping_id: st
                    .mappings
                    .get(&ext)
                    .map(|m| m.current_mapping.0)
                    .unwrap_or(0),
                physical_extent: pe_id.0,
                device_id: p.device.0 as u32,
                device_offset: p.offset,
                length: p.length,
            };
            let lsn = self.append(txn, &body)?;
            if let Some(e) = st.extents.get_mut(&ext) {
                e.alloc_lsn = lsn;
            }
        }
        for (ext, mapping) in txn.pending_mappings.clone() {
            let pe = st
                .mappings
                .get(&ext)
                .and_then(|m| m.records.iter().find(|r| r.mapping == mapping))
                .map(|r| r.physical.0)
                .unwrap_or(0);
            self.append(
                txn,
                &WalBody::MappingCreate {
                    extent: ext.0,
                    mapping_id: mapping.0,
                    physical_extent: pe,
                },
            )?;
        }
        // 2. ObjectCreate
        for (oid, ty) in txn.pending_creates.clone() {
            self.append(
                txn,
                &WalBody::ObjectCreate {
                    object_id: oid,
                    object_type: ty as u8,
                    component_count: 1,
                },
            )?;
        }
        // 3. ObjectVersion
        for (oid, ver) in txn.pending_versions.clone() {
            let old = st
                .objects
                .get(&oid)
                .and_then(|o| {
                    o.versions
                        .iter()
                        .filter(|v| v.version < ver && v.state == VersionState::Published)
                        .map(|v| v.version)
                        .max()
                })
                .unwrap_or(VersionId(0));
            let ext = st
                .objects
                .get(&oid)
                .and_then(|o| o.versions.iter().find(|v| v.version == ver))
                .and_then(|v| v.extent)
                .map(|e| e.0)
                .unwrap_or(0);
            let lsn = self.append(
                txn,
                &WalBody::ObjectVersion {
                    object_id: oid,
                    new_version: ver,
                    old_version: old,
                    extent: ext,
                },
            )?;
            if let Some(o) = st.objects.get_mut(&oid) {
                if let Some(v) = o.versions.iter_mut().find(|v| v.version == ver) {
                    v.created_lsn = lsn;
                }
            }
        }
        // 4. ObjectDelete
        for (oid, ver) in txn.pending_deletes.clone() {
            self.append(
                txn,
                &WalBody::ObjectDelete {
                    object_id: oid,
                    version: ver,
                },
            )?;
        }
        // 5. TxnCommit + wal barrier
        let commit_lsn = self.append(txn, &WalBody::TxnCommit)?;
        self.wal.flush_upto(commit_lsn)?;
        txn.commit_lsn = Some(commit_lsn);

        // 6. 发布内存版本（Pending → Published）。
        st.txn_status.insert(txn.xid, TxnStatus::Committed);
        for (oid, ver) in txn.pending_versions.clone() {
            if let Some(o) = st.objects.get_mut(&oid) {
                // 旧版本被本事务标记删除。
                for v in o.versions.iter_mut() {
                    if v.version < ver && v.state == VersionState::Published {
                        v.xmax = Some(txn.xid);
                        v.state = VersionState::Obsolete;
                    }
                }
                if let Some(v) = o.versions.iter_mut().find(|v| v.version == ver) {
                    v.state = VersionState::Published;
                }
            }
        }
        for (oid, _) in txn.pending_deletes.clone() {
            if let Some(o) = st.objects.get_mut(&oid) {
                o.deleted_xid = Some(txn.xid);
            }
        }
        for ext in txn.pending_extents.clone() {
            if let Some(e) = st.extents.get_mut(&ext) {
                e.state = ExtentState::Mapped;
            }
        }
        drop(st);
        txn.state = TxnStatus::Committed;
        self.active.lock().unwrap().remove(&txn.xid);
        self.live.lock().unwrap().remove(&txn.snapshot.id);
        Ok(CommitReceipt {
            xid: txn.xid,
            commit_lsn,
        })
    }

    fn abort(&self, txn: &mut TxnCtx) -> KernelResult<()> {
        self.check_txn_running(txn)?;
        let _writer = self.writer.lock().unwrap();
        let mut st = self.state.lock().unwrap();
        self.append(txn, &WalBody::TxnAbort)?;
        self.wal.flush_upto(txn.prev_lsn)?;
        st.txn_status.insert(txn.xid, TxnStatus::Aborted);
        for (oid, ver) in txn.pending_versions.clone() {
            if let Some(o) = st.objects.get_mut(&oid) {
                if let Some(v) = o.versions.iter_mut().find(|v| v.version == ver) {
                    v.state = VersionState::Aborted;
                }
            }
        }
        for ext in txn.pending_extents.clone() {
            if let Some(e) = st.extents.get_mut(&ext) {
                e.state = ExtentState::Unmapped;
            }
        }
        drop(st);
        txn.state = TxnStatus::Aborted;
        self.active.lock().unwrap().remove(&txn.xid);
        self.live.lock().unwrap().remove(&txn.snapshot.id);
        Ok(())
    }

    fn visible(&self, ver: &ObjectVersion, snap: &Snapshot) -> bool {
        let st = self.state.lock().unwrap();
        txn::version_visible(ver.xmin, ver.xmax, snap, &st.txn_status)
    }

    fn status(&self, xid: TransactionId) -> TxnStatus {
        self.state
            .lock()
            .unwrap()
            .txn_status
            .get(&xid)
            .copied()
            .unwrap_or(TxnStatus::InProgress)
    }
}

#[allow(dead_code)]
fn conflict_write(object: ObjectId, expected: VersionId, actual: VersionId) -> KernelError {
    KernelError::Conflict(ConflictKind::WriteConflict {
        object,
        expected,
        actual,
    })
}

#[allow(dead_code)]
fn corrupted_wal() -> KernelError {
    KernelError::corrupted(CorruptionKind::WalRecordCrc)
}
