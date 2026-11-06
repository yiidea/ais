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

pub use executor::SyncIoExecutor;
pub use file::FileDevice;
pub use mem::MemDevice;

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
    fn write<'a>(&'a self, loc: &'a PhysicalLocation, buf: &'a [u8]) -> BoxFuture<'a, KernelResult<()>>;

    /// 读取一段数据。
    fn read<'a>(&'a self, loc: &'a PhysicalLocation, buf: &'a mut [u8]) -> BoxFuture<'a, KernelResult<()>>;

    /// 刷盘到指定耐久等级。
    fn flush<'a>(&'a self, loc: &'a PhysicalLocation, level: DurabilityLevel) -> BoxFuture<'a, KernelResult<()>>;

    /// 归还设备空间（TRIM / deallocate）；不可用时返回 Unsupported。
    fn discard<'a>(&'a self, loc: &'a PhysicalLocation) -> BoxFuture<'a, KernelResult<()>>;

    /// 当前可达的最高耐久等级（设备降级后下降）。
    fn max_durability(&self) -> DurabilityLevel;

    /// 报告设备健康状态。
    fn health(&self) -> crate::types::DeviceHealth;
}

/// 提交给 reactor 的设备操作单位（04 §6.3）。
#[derive(Clone, Debug)]
pub enum DeviceOp {
    Write {
        loc: PhysicalLocation,
        data: Vec<u8>,
    },
    Read {
        loc: PhysicalLocation,
        len: u64,
    },
    Flush {
        loc: PhysicalLocation,
        level: DurabilityLevel,
    },
    Discard {
        loc: PhysicalLocation,
    },
}

/// 设备操作的结果（读取结果通过调用方提供的缓冲区回填）。
#[derive(Clone, Debug, Default)]
pub struct DeviceResult;

/// 同步 façade（04 §6.3）。
pub trait IoExecutor: Send + Sync {
    /// 提交一次设备操作并等��完成（阻塞调用方）。
    fn submit_and_wait(&self, op: DeviceOp) -> KernelResult<DeviceResult>;

    /// 批量提交并等待全部完成。
    fn submit_batch_and_wait(&self, ops: &[DeviceOp]) -> KernelResult<Vec<DeviceResult>> {
        ops.iter().map(|op| self.submit_and_wait(op.clone())).collect()
    }

    /// 读取并回填缓冲区。这是 `DeviceOp::Read` 的便捷形式，
    /// 避免调用方为了拿回数据再实现一次 reactor 协议。
    fn read_into(&self, loc: &PhysicalLocation, buf: &mut [u8]) -> KernelResult<()>;

    /// 设备描述（供放置与诊断使用）。
    fn descriptor(&self) -> &DeviceDescriptor;

    fn max_durability(&self) -> DurabilityLevel;
}

