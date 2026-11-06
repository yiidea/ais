//! `IoExecutor` 的 MVP 实现：单线程同步 façade。
//!
//! 设计文档（04 §6.3 I1）要求每个 NUMA 节点一个常驻 reactor 线程。
//! MVP 先实现语义等价的同步版本：提交 → 在当前线程 `block_on` 设备 future → 返回。
//! 这保持了 I2「调用返回即已完成」的契约，后续替换为 reactor 线程池时接口不变。

use super::{DeviceOp, DeviceResult, DeviceDescriptor, IoExecutor, StorageDevice};
use crate::error::{InternalCode, KernelError, KernelResult};
use crate::types::{DurabilityLevel, PhysicalLocation};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};

/// 极简 `block_on`：设备 future 在 MVP 中都是立即完成的（文件 I/O 同步执行），
/// 因此一个 noop waker + 轮询即可；若未来接入真正的 io_uring reactor，
/// 这里会替换为 reactor 的完成队列等待。
pub fn block_on<F: Future>(future: F) -> F::Output {
    let waker = noop_waker();
    let mut cx = Context::from_waker(&waker);
    let mut future = Box::pin(future);
    loop {
        match Pin::new(&mut future).poll(&mut cx) {
            Poll::Ready(v) => return v,
            Poll::Pending => std::thread::yield_now(),
        }
    }
}

fn noop_waker() -> Waker {
    unsafe fn clone(_: *const ()) -> RawWaker {
        RawWaker::new(std::ptr::null(), &VTABLE)
    }
    unsafe fn wake(_: *const ()) {}
    unsafe fn wake_by_ref(_: *const ()) {}
    unsafe fn drop(_: *const ()) {}

    static VTABLE: RawWakerVTable = RawWakerVTable::new(clone, wake, wake_by_ref, drop);
    // SAFETY: noop waker 不携带任何数据，四个函数均为空操作。
    unsafe { Waker::from_raw(RawWaker::new(std::ptr::null(), &VTABLE)) }
}

/// 同步 façade：持有 `Box<dyn StorageDevice>`（04 §6.3 I5）。
pub struct SyncIoExecutor {
    device: Box<dyn StorageDevice>,
}

impl SyncIoExecutor {
    pub fn new(device: Box<dyn StorageDevice>) -> Self {
        SyncIoExecutor { device }
    }

    pub fn device(&self) -> &dyn StorageDevice {
        self.device.as_ref()
    }
}

impl IoExecutor for SyncIoExecutor {
    fn submit_and_wait(&self, op: DeviceOp) -> KernelResult<DeviceResult> {
        match op {
            DeviceOp::Write { loc, data } => {
                block_on(self.device.write(&loc, &data))?;
                Ok(DeviceResult)
            }
            DeviceOp::Read { loc, len } => {
                let mut buf = vec![0u8; len as usize];
                block_on(self.device.read(&loc, &mut buf))?;
                Ok(DeviceResult)
            }
            DeviceOp::Flush { loc, level } => {
                block_on(self.device.flush(&loc, level))?;
                Ok(DeviceResult)
            }
            DeviceOp::Discard { loc } => {
                block_on(self.device.discard(&loc))?;
                Ok(DeviceResult)
            }
        }
    }

    fn read_into(&self, loc: &PhysicalLocation, buf: &mut [u8]) -> KernelResult<()> {
        if buf.len() as u64 != loc.length {
            return Err(KernelError::internal(
                InternalCode::InvariantViolation,
                format!(
                    "read_into length mismatch: buffer {} vs location {}",
                    buf.len(),
                    loc.length
                ),
            ));
        }
        block_on(self.device.read(loc, buf))
    }

    fn descriptor(&self) -> &DeviceDescriptor {
        self.device.descriptor()
    }

    fn max_durability(&self) -> DurabilityLevel {
        self.device.max_durability()
    }
}

/// 便于在 `Arc<dyn IoExecutor>` 与具体类型之间转换。
pub type SharedIoExecutor = Arc<dyn IoExecutor>;
