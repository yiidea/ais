//! L1 单元层与 L2 崩溃恢复层的共享辅助。

use crate::checksum::crc32c;
use crate::error::KernelResult;
use crate::hal::StorageDevice;
use crate::kernel::{
    CommitReceipt, Kernel, KernelConfig, ObjectManager, TransactionManager, TxnCtx,
};
use crate::testing::FaultIo;
use crate::types::*;
use crate::wal::{DeviceWal, DeviceWalConfig};
use std::path::PathBuf;
use std::sync::Arc;

/// 固定设备布局：WAL 在 [0, 2MB)，payload 从 2MB 开始。
pub struct Layout;

impl Layout {
    pub const WAL_SEGMENT_SIZE: u64 = 64 * 1024;
    pub const WAL_SEGMENTS: u64 = 32;
    pub const WAL_END: u64 = Self::WAL_SEGMENT_SIZE * Self::WAL_SEGMENTS;
    pub const PAYLOAD_BASE: u64 = Self::WAL_END;
    pub const CAPACITY: u64 = 16 * 1024 * 1024;

    pub fn config(ckpt: impl Into<PathBuf>) -> KernelConfig {
        let mut cfg = KernelConfig::new(ckpt);
        cfg.wal_base_offset = 0;
        cfg.wal_segment_size = Self::WAL_SEGMENT_SIZE;
        cfg.wal_segment_count = Self::WAL_SEGMENTS;
        cfg.payload_base_offset = Self::PAYLOAD_BASE;
        cfg
    }
}

/// 用共享内存设备建一个可注入的引擎环境。
///
/// 设备通过 `Arc` 共享：崩溃重启时复用同一份「介质」，换一个新的 [`FaultIo`]。
pub struct Harness {
    pub kernel: Kernel,
    pub io: Arc<FaultIo>,
}

impl Harness {
    /// 打开（或重启）内核：同一份介质、全新的注入器状态。
    pub fn boot(cfg: KernelConfig, medium: Arc<dyn StorageDevice>) -> KernelResult<Self> {
        let io = Arc::new(FaultIo::new(medium));
        let kernel = Kernel::open_with_io(cfg, io.clone())?;
        Ok(Harness { kernel, io })
    }
}

/// 构造一个包装在内存设备上的 WAL（L1 用例用）。
pub fn wal_on(dev: Box<dyn StorageDevice>) -> KernelResult<DeviceWal> {
    let io = Arc::new(crate::hal::SyncIoExecutor::new(dev));
    DeviceWal::open(
        io,
        DeviceWalConfig::new(
            DeviceId(1),
            0,
            Layout::WAL_SEGMENT_SIZE,
            Layout::WAL_SEGMENTS,
        ),
    )
}

/// 一个提交了 N 个文档对象的简单写事务，返回 (xid, receipt)。
pub fn commit_object(kernel: &Kernel, payload: &[u8]) -> KernelResult<ObjectId> {
    let mut txn: TxnCtx = TransactionManager::begin(kernel, IsolationLevel::Snapshot)?;
    let req = crate::kernel::CreateObjectRequest {
        object_type: ObjectType::Document,
        payload: payload.to_vec(),
        component_count: 1,
    };
    let id = ObjectManager::create_object(kernel, &mut txn, &req)?;
    let _r: CommitReceipt = TransactionManager::commit(kernel, &mut txn)?;
    Ok(id)
}

/// 校验和辅助：测试用它断言 payload 逐字节一致。
pub fn digest(bytes: &[u8]) -> u32 {
    crc32c(bytes)
}
