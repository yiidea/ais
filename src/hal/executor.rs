//! `IoExecutor` 的 MVP 实现：单线程同步 façade。
//!
//! 设计文档（04 §6.3 I1）要求每个 NUMA 节点一个常驻 reactor 线程。
//! MVP 先实现语义等价的同步版本：提交 → 在当前线程 `block_on` 设备 future → 返回。
//! 这保持了 I2「调用返回即已完成」的契约，后续替换为 reactor 线程池时接口不变。

use super::qd::{QueueDepth, Slots};
use super::{DeviceDescriptor, DeviceOp, DeviceResult, DeviceSite, IoExecutor, StorageDevice};
use crate::clock::Clock;
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

/// 没有声明 QD 预算时使用的在途上限。
///
/// 单线程同步 façade 同一时刻至多一个在飞操作，远达不到这个上限；
/// 真正生效的 QD 由 [`SyncIoExecutor::with_queue_depth`] 显式给出（07 §4）。
const DEFAULT_ORDINARY_SLOTS: u32 = 32;
const DEFAULT_FLUSH_SLOTS: u32 = 1;
const DEFAULT_BACKPRESSURE_DEADLINE_NANOS: u64 = 10_000_000;

/// 同步 façade：持有 `Box<dyn StorageDevice>`（04 §6.3 I5）。
pub struct SyncIoExecutor {
    device: Box<dyn StorageDevice>,
    slots: Slots,
}

impl SyncIoExecutor {
    pub fn new(device: Box<dyn StorageDevice>) -> Self {
        let qd = QueueDepth::new(
            DEFAULT_ORDINARY_SLOTS + DEFAULT_FLUSH_SLOTS,
            DEFAULT_ORDINARY_SLOTS,
            DEFAULT_FLUSH_SLOTS,
        )
        .expect("default queue depth is valid");
        Self::with_queue_depth(device, &qd, DEFAULT_BACKPRESSURE_DEADLINE_NANOS)
    }

    /// 用 07 §4 推导出的 QD 与本设备的背压 deadline 构造（HRM7）。
    pub fn with_queue_depth(
        device: Box<dyn StorageDevice>,
        qd: &QueueDepth,
        deadline_nanos: u64,
    ) -> Self {
        SyncIoExecutor {
            device,
            slots: Slots::new(qd, deadline_nanos, Box::new(DefaultClock::default())),
        }
    }

    pub fn device(&self) -> &dyn StorageDevice {
        self.device.as_ref()
    }

    /// 当前生效的普通读写在上限与 flush 预留（诊断用）。
    pub fn slots(&self) -> &Slots {
        &self.slots
    }
}

/// 背压 deadline 的计时源：一个实例、单调递增（每次新建会把起点清零）。
#[derive(Default)]
struct DefaultClock {
    inner: crate::clock::SystemClock,
}

impl Clock for DefaultClock {
    fn monotonic_nanos(&self) -> u64 {
        self.inner.monotonic_nanos()
    }
}

impl IoExecutor for SyncIoExecutor {
    /// 触发点（ADR-0011 决策 3）：本实现没有独立的 reactor 线程，
    /// 「出队并执行之前」= 进入本函数；「确认成功、未回填之前」= 设备调用返回之后。
    /// 将来的 reactor 版本把这两个点放在工作线程上，接口不变。
    fn submit_and_wait(&self, _site: DeviceSite, op: DeviceOp) -> KernelResult<DeviceResult> {
        // HRM7：普通 I/O 只能占 `qd_usable` 个槽位，flush 只能用预留的那几个。
        // 额度满了就在 deadline 内等，仍在等不到就返回 `Retryable(Backpressure)`——
        // 不静默截断、不缩小 batch、不自动重试（07 §4.5，04 §2.3 C1）。
        let _slot = if matches!(op, DeviceOp::Flush { .. }) {
            self.slots.acquire_flush()?
        } else {
            self.slots.acquire_ordinary()?
        };
        match op {
            DeviceOp::Write { loc, data, .. } => {
                block_on(self.device.write(&loc, &data))?;
                Ok(DeviceResult)
            }
            DeviceOp::Read { loc, len, .. } => {
                let mut buf = vec![0u8; len as usize];
                block_on(self.device.read(&loc, &mut buf))?;
                Ok(DeviceResult)
            }
            DeviceOp::Flush { loc, level, .. } => {
                block_on(self.device.flush(&loc, level))?;
                Ok(DeviceResult)
            }
            DeviceOp::Discard { loc, .. } => {
                block_on(self.device.discard(&loc))?;
                Ok(DeviceResult)
            }
        }
    }

    fn read_into(
        &self,
        _site: DeviceSite,
        loc: &PhysicalLocation,
        buf: &mut [u8],
    ) -> KernelResult<()> {
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
