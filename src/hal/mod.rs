//! 设备 HAL 与同步 façade（04 §6）。
//!
//! 冻结要点：
//! - `StorageDevice` 是内核里唯一允许异步的 trait（D0）；
//! - 为保证 `Box<dyn StorageDevice>` 可用，方法返回手工装箱的 future（等价 `#[async_trait]`，D0.1）；
//! - `IoExecutor` 是核心内核触达设备的唯一入口，提交即等待完成（I2/I3）；
//! - 设备层不感知事务（D1），错误里不出现物理地址（C3）。

use crate::error::KernelResult;
use crate::types::{DeviceId, DurabilityLevel, PhysicalLocation};
use std::future::Future;
use std::pin::Pin;

pub mod executor;
pub mod file;
pub mod mem;
pub mod qd;
pub mod reactor;

pub use executor::SyncIoExecutor;
pub use file::FileDevice;
pub use mem::MemDevice;
pub use qd::{IoPath, QdConfig, QdDecision, QdSample, QdTuner, QueueDepth, SlotGuard, Slots};
pub use reactor::{ReactorIoExecutor, WindowSample};

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// 设备能力描述。
#[derive(Clone, Debug)]
pub struct DeviceDescriptor {
    pub id: DeviceId,
    pub name: String,
    /// 设备容量（字节）。
    pub capacity: u64,
    /// 建议 I/O 对齐（字节）。
    pub alignment: u64,
    /// 是否支持 discard / TRIM。
    pub supports_discard: bool,
}

/// 设备 HAL（04 §6.1）。
pub trait StorageDevice: Send + Sync {
    fn descriptor(&self) -> &DeviceDescriptor;

    /// 顺序写一段数据（不保证持久）。
    fn write<'a>(
        &'a self,
        loc: &'a PhysicalLocation,
        buf: &'a [u8],
    ) -> BoxFuture<'a, KernelResult<()>>;

    /// 读取一段数据。
    fn read<'a>(
        &'a self,
        loc: &'a PhysicalLocation,
        buf: &'a mut [u8],
    ) -> BoxFuture<'a, KernelResult<()>>;

    /// 刷盘到指定耐久等级。
    fn flush<'a>(
        &'a self,
        loc: &'a PhysicalLocation,
        level: DurabilityLevel,
    ) -> BoxFuture<'a, KernelResult<()>>;

    /// 归还设备空间（TRIM / deallocate）；不可用时返回 Unsupported。
    fn discard<'a>(&'a self, loc: &'a PhysicalLocation) -> BoxFuture<'a, KernelResult<()>>;

    /// 当前可达的最高耐久等级（设备降级后下降）。
    fn max_durability(&self) -> DurabilityLevel;

    /// 报告设备健康状态。
    fn health(&self) -> crate::types::DeviceHealth;
}

/// 设备级语义 site：故障注入点的主键（04 §6.3 I6，[ADR-0011]）。
///
/// 由**调用方声明**，不由 `IoExecutor` 从 `PhysicalLocation` 推断——
/// 按地址区间反推 site 会让布局调整悄悄移动注入点（[ADR-0011] 备选方案 2）。
///
/// `#[non_exhaustive]` + 穷尽 `match`：新增变体必须在 05 §3.2 的 site 目录登记。
///
/// [ADR-0011]: ../../docs/adr/0011-device-op-site-tagging.md
#[non_exhaustive]
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Default)]
pub enum DeviceSite {
    /// `wal.frame.partial` / `wal.frame.written`
    WalFrameWrite,
    /// `wal.flushed` / `wal.flush.partial`
    WalFlushStart,
    /// `extent.write.partial` / `extent.write.done`
    ExtentPayloadWrite,
    /// `ckpt.manifest.before`
    CkptManifestWrite,
    /// `ckpt.superblock.before`
    CkptSuperblockWrite,
    /// `ckpt.publish.before_rename`
    CkptShadowRename,
    /// `mapping.switch.after_log`
    MappingSwitchWrite,
    /// `migration.target.partial`
    MigrationTargetWrite,
    /// `txn.commit.before_flush`
    TxnCommitWrite,
    /// `index.segment.sealed`（索引层打点，写仍经本 `IoExecutor`）
    IndexSegmentSealed,
    /// `index.watermark.published`（索引层打点）
    IndexWatermarkCommit,
    /// 未打标签：内核代码里出现即为缺陷，注入规则定义在它上面必须让测试失败。
    #[default]
    Untagged,
}

impl DeviceSite {
    /// 是否为合法 site（`Untagged` 不是）。
    pub fn is_tagged(self) -> bool {
        !matches!(self, DeviceSite::Untagged)
    }
}

/// 提交给 reactor 的设备操作单位（04 §6.3）。
///
/// 每个变体自带 `site`：标签跟着 op 走，批量接口因此不需要平行的标签数组，
/// 「数组长度不一致导致注入点错位」这类错误不可表达（ADR-0011 决策 2）。
#[derive(Clone, Debug)]
pub enum DeviceOp {
    Write {
        site: DeviceSite,
        loc: PhysicalLocation,
        data: Vec<u8>,
    },
    Read {
        site: DeviceSite,
        loc: PhysicalLocation,
        len: u64,
    },
    Flush {
        site: DeviceSite,
        loc: PhysicalLocation,
        level: DurabilityLevel,
    },
    Discard {
        site: DeviceSite,
        loc: PhysicalLocation,
    },
}

impl DeviceOp {
    /// 该操作的语义 site。
    pub fn site(&self) -> DeviceSite {
        match self {
            DeviceOp::Write { site, .. }
            | DeviceOp::Read { site, .. }
            | DeviceOp::Flush { site, .. }
            | DeviceOp::Discard { site, .. } => *site,
        }
    }

    /// 该操作落在哪一类设备动作上（注入器用它区分同一 site 的 write / flush）。
    pub fn class(&self) -> OpClass {
        match self {
            DeviceOp::Write { .. } => OpClass::Write,
            DeviceOp::Read { .. } => OpClass::Read,
            DeviceOp::Flush { .. } => OpClass::Flush,
            DeviceOp::Discard { .. } => OpClass::Discard,
        }
    }

    /// 物理位置（只有 HAL 与内核内部可见，04 §14.1 规则 1）。
    pub fn location(&self) -> &PhysicalLocation {
        match self {
            DeviceOp::Write { loc, .. }
            | DeviceOp::Read { loc, .. }
            | DeviceOp::Flush { loc, .. }
            | DeviceOp::Discard { loc, .. } => loc,
        }
    }
}

/// 设备动作类别。读 / discard 不参与语义注入，但同样是自描述 op 的一部分。
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum OpClass {
    Write,
    Read,
    Flush,
    Discard,
}

/// 设备操作的结果（读取结果通过调用方提供的缓冲区回填）。
#[derive(Clone, Debug, Default)]
pub struct DeviceResult;

/// 同步 façade（04 §6.3）。
pub trait IoExecutor: Send + Sync {
    /// 提交一次设备操作并等待完成（阻塞调用方）。
    ///
    /// `site` 由调用方声明（ADR-0011）；`Untagged` 是缺陷，实现可拒绝或忽略。
    fn submit_and_wait(&self, site: DeviceSite, op: DeviceOp) -> KernelResult<DeviceResult>;

    /// 批量提交并等待全部完成。每个 op 自带 `site`，不传平行的标签数组（ADR-0011 决策 2）。
    fn submit_batch_and_wait(&self, ops: &[DeviceOp]) -> KernelResult<Vec<DeviceResult>> {
        ops.iter()
            .map(|op| self.submit_and_wait(op.site(), op.clone()))
            .collect()
    }

    /// 读取并回填缓冲区。这是 `DeviceOp::Read` 的便捷形式，
    /// 避免调用方为了拿回数据再实现一次 reactor 协议。
    fn read_into(
        &self,
        site: DeviceSite,
        loc: &PhysicalLocation,
        buf: &mut [u8],
    ) -> KernelResult<()>;

    /// 设备描述（供放置与诊断使用）。
    fn descriptor(&self) -> &DeviceDescriptor;

    fn max_durability(&self) -> DurabilityLevel;
}
