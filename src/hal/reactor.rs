//! 常驻 reactor 线程上的 `IoExecutor`（04 §6.3 I1 / 07 §4）。
//!
//! [`SyncIoExecutor`](crate::hal::SyncIoExecutor) 在调用线程上 `block_on`，同一时刻至多一个在飞操作——
//! QD 上限、背压与 [`QdTuner`] 在它上面永远触发不了。这里把一个 reactor
//! 线程挂在设备前面：调用方把操作交给 reactor 后阻塞等待完成（I2 的语义
//! 不变），在飞操作数因此可以超过 1，真正受 `qd_usable` 约束。
//!
//! 普通 I/O 满了**直接**返回背压，不排队等待（deadline 为 0）：等待会把
//! 延迟藏进队列，而 §4.5 要的正是让调用方看见它。
//!
//! 采样延迟 = 排队时间 + 设备完成时间。只测设备执行时间会掩盖队列效应，
//! `lat_p99` 也就失去调节 QD 的依据。

use super::qd::{QdSample, QdTuner, QueueDepth, SlotGuard, Slots};
use super::{DeviceDescriptor, DeviceOp, DeviceResult, DeviceSite, IoExecutor, StorageDevice};
use crate::clock::Clock;
use crate::error::{InternalCode, KernelError, KernelResult};
use crate::types::{DurabilityLevel, PhysicalLocation};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// 07 §4.5：普通 I/O 满了最多等这么久，回来换下一轮尝试；
/// 等待期固定，不随调用方累计——否则「等一次背压」会变成「无限排队」。
///
/// 取 0 会让两个线程几乎永远凑不到一起（它们只会轮流拿到同一个槽位）。
const REACTOR_BACKPRESSURE_DEADLINE_NANOS: u64 = 200_000;
/// 一次最多取出的操作数（`batch ≤ QD`，07 §4.1）。
const MAX_BATCH: usize = 128;
/// 空闲时 reactor 的唤醒间隔：只影响窗口计时精度，不影响延迟。
const IDLE_TICK: Duration = Duration::from_millis(2);
/// `baseline_p99` 的 EWMA 权重（§4.3）。
const BASELINE_ALPHA: f64 = 0.2;

struct SharedClock(Arc<dyn Clock>);

/// 槽位随操作排队，被 reactor 在完成后归还。
struct Slot(SlotGuard, bool);

struct Pending {
    op: DeviceOp,
    started: Instant,
    reply: Sender<KernelResult<DeviceResult>>,
    slot: Slot,
    /// `read_into` 的数据要写进调用方的缓冲区——必须由 reactor 回填，
    /// 否则读到的字节无处可去（设备结果里不含数据）。
    backfill: Option<Arc<Backfill>>,
}

/// reactor → 调用方的数据回填通道（静态生命周期，跨线程）。
struct Backfill(Mutex<Option<Vec<u8>>>);

/// 最近一个窗口（诊断用）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WindowSample {
    pub inflight_peak: u32,
    pub lat_p50: u64,
    pub lat_p99: u64,
    pub completions: u64,
}

struct Inner {
    device: Arc<dyn StorageDevice>,
    slots: Slots,
    queue: Mutex<VecDeque<Pending>>,
    wake: Condvar,
    tuner: Mutex<QdTuner>,
    stats: Mutex<Stats>,
    stopped: AtomicBool,
    max_inflight_seen: AtomicU64,
    adjustments: AtomicU64,
}

#[derive(Default)]
struct Stats {
    lat_nanos: Vec<u64>,
    inflight_peak: u32,
    baseline_p99: u64,
    last: WindowSample,
}

impl Clock for SharedClock {
    fn monotonic_nanos(&self) -> u64 {
        self.0.monotonic_nanos()
    }
}

/// 常驻 reactor 线程的同步 façade。
///
/// `Drop` 会停掉线程；未完成的操作由 reactor 在退出前回填错误，
/// 调用方不会因为执行器消失而永久阻塞。
pub struct ReactorIoExecutor {
    inner: Arc<Inner>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl ReactorIoExecutor {
    /// 按设备的 QD 配置起一个 reactor；每 `window` 收一次窗口交给 [`QdTuner`]。
    pub fn new(
        device: Arc<dyn StorageDevice>,
        qd: &QueueDepth,
        clock: Arc<dyn Clock>,
        slo_nanos: u64,
        window: Duration,
    ) -> Self {
        let slots = Slots::new(
            qd,
            REACTOR_BACKPRESSURE_DEADLINE_NANOS,
            Arc::new(SharedClock(Arc::clone(&clock))),
        );
        let inner = Arc::new(Inner {
            device,
            slots,
            queue: Mutex::new(VecDeque::new()),
            wake: Condvar::new(),
            tuner: Mutex::new(QdTuner::new(qd.clone(), 1, 3)),
            stats: Mutex::new(Stats::default()),
            stopped: AtomicBool::new(false),
            max_inflight_seen: AtomicU64::new(0),
            adjustments: AtomicU64::new(0),
        });
        let worker = std::thread::Builder::new()
            .name("ains-reactor".into())
            .spawn({
                let inner = Arc::clone(&inner);
                move || run(inner, slo_nanos, window.max(Duration::from_millis(1)))
            })
            .expect("spawn reactor thread");
        ReactorIoExecutor {
            inner,
            worker: Some(worker),
        }
    }

    pub fn device(&self) -> &dyn StorageDevice {
        self.inner.device.as_ref()
    }

    /// 当前生效的普通在途额度 `qd_usable`。
    pub fn qd_usable(&self) -> u32 {
        self.inner.slots.usable()
    }

    /// 观测到的最大在飞操作数：并发确实发生过，QD 才有意义。
    pub fn max_inflight_seen(&self) -> u64 {
        self.inner.max_inflight_seen.load(Ordering::SeqCst)
    }

    /// 窗口发生过的 QD 调整次数（`io.qd.adjustments`，§4.4）。
    pub fn adjustments(&self) -> u64 {
        self.inner.adjustments.load(Ordering::SeqCst)
    }

    /// 最近一次窗口采样。
    pub fn last_window(&self) -> WindowSample {
        self.inner.stats.lock().unwrap().last
    }
}

impl Drop for ReactorIoExecutor {
    fn drop(&mut self) {
        self.inner.stopped.store(true, Ordering::SeqCst);
        self.inner.wake.notify_all();
        if let Some(w) = self.worker.take() {
            let _ = w.join();
        }
    }
}

impl IoExecutor for ReactorIoExecutor {
    fn submit_and_wait(&self, _site: DeviceSite, op: DeviceOp) -> KernelResult<DeviceResult> {
        // 槽位在调用方线程上占用：满了在这里就返回背压，操作根本没进队列（§4.5）。
        let slot = if matches!(op, DeviceOp::Flush { .. }) {
            Slot(self.inner.slots.acquire_flush()?, false)
        } else {
            Slot(self.inner.slots.acquire_ordinary()?, true)
        };
        let (tx, rx) = mpsc::channel();
        {
            let mut q = self.inner.queue.lock().unwrap();
            if self.inner.stopped.load(Ordering::SeqCst) {
                return Err(KernelError::internal(
                    InternalCode::InvariantViolation,
                    "reactor is stopping",
                ));
            }
            q.push_back(Pending {
                op,
                started: Instant::now(),
                reply: tx,
                slot,
                backfill: None,
            });
        }
        self.inner.wake.notify_one();
        rx.recv().unwrap_or_else(|_| {
            Err(KernelError::internal(
                InternalCode::InvariantViolation,
                "reactor dropped a submitted op",
            ))
        })
    }

    /// 同步读：读到的字节由 reactor 直接写进调用方的缓冲区。
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
        // 必须经 reactor：绕过它会让 I/O 顺序与槽位记账分叉。
        let (tx, rx) = mpsc::channel();
        let slot = Slot(self.inner.slots.acquire_ordinary()?, true);
        let backfill = Arc::new(Backfill(Mutex::new(None)));
        {
            let mut q = self.inner.queue.lock().unwrap();
            if self.inner.stopped.load(Ordering::SeqCst) {
                return Err(KernelError::internal(
                    InternalCode::InvariantViolation,
                    "reactor is stopping",
                ));
            }
            q.push_back(Pending {
                op: DeviceOp::Read {
                    site: DeviceSite::Untagged,
                    loc: *loc,
                    len: loc.length,
                },
                started: Instant::now(),
                reply: tx,
                slot,
                backfill: Some(Arc::clone(&backfill)),
            });
        }
        self.inner.wake.notify_one();
        match rx.recv() {
            Ok(Ok(_)) => {
                // 数据由 reactor 读进自己的缓冲区，这里拷进调用方的 `buf`。
                match backfill.0.lock().unwrap().take() {
                    Some(data) if data.len() == buf.len() => {
                        buf.copy_from_slice(&data);
                        Ok(())
                    }
                    _ => Err(KernelError::internal(
                        InternalCode::InvariantViolation,
                        "reactor completed a read without data",
                    )),
                }
            }
            Ok(Err(e)) => Err(e),
            Err(_) => Err(KernelError::internal(
                InternalCode::InvariantViolation,
                "reactor dropped a submitted read",
            )),
        }
    }

    fn descriptor(&self) -> &DeviceDescriptor {
        self.inner.device.descriptor()
    }

    fn max_durability(&self) -> DurabilityLevel {
        self.inner.device.max_durability()
    }
}

fn run(inner: Arc<Inner>, slo_nanos: u64, window: Duration) {
    let mut last_window = Instant::now();
    loop {
        let batch = {
            let mut q = inner.queue.lock().unwrap();
            while q.is_empty() && !inner.stopped.load(Ordering::SeqCst) {
                let (guard, _) = inner.wake.wait_timeout(q, IDLE_TICK).unwrap();
                q = guard;
            }
            let cap = (inner.slots.usable() as usize).clamp(1, MAX_BATCH);
            (0..cap).filter_map(|_| q.pop_front()).collect::<Vec<_>>()
        };
        if inner.stopped.load(Ordering::SeqCst) && batch.is_empty() {
            break;
        }
        inner
            .max_inflight_seen
            .fetch_max(inner.slots.ordinary_inflight() as u64, Ordering::SeqCst);

        for p in batch {
            let started = p.started;
            let result = match (&p.backfill, &p.op) {
                (Some(backfill), DeviceOp::Read { loc, len, .. }) => {
                    let mut data = vec![0u8; *len as usize];
                    super::executor::block_on(inner.device.read(loc, &mut data)).map(|()| {
                        *backfill.0.lock().unwrap() = Some(data);
                        DeviceResult
                    })
                }
                _ => execute(inner.device.as_ref(), &p.op),
            };
            let elapsed = started.elapsed().as_nanos() as u64;
            let ordinary = p.slot.1;
            p.slot.0.release();
            if ordinary {
                inner.record(elapsed);
            }
            // 先确认成功，再回填完成事件（ADR-0011 的 after 触发点）。
            let _ = p.reply.send(result);
        }

        if last_window.elapsed() >= window {
            inner.close_window(slo_nanos);
            last_window = Instant::now();
        }
    }
    // 停机时把队列里的操作都回填失败，而不是让调用方永久阻塞。
    let leftovers: Vec<Pending> = inner.queue.lock().unwrap().drain(..).collect();
    for p in leftovers {
        let _ = p.reply.send(Err(KernelError::internal(
            InternalCode::InvariantViolation,
            "reactor stopped before completing the op",
        )));
    }
}

impl Inner {
    fn record(&self, latency_nanos: u64) {
        let mut s = self.stats.lock().unwrap();
        s.lat_nanos.push(latency_nanos);
        s.inflight_peak = s.inflight_peak.max(self.slots.ordinary_inflight());
    }

    /// 07 §4.3–§4.4：一个窗口收一次，交给 `QdTuner`；调整后写回 `Slots`。
    fn close_window(&self, slo: u64) {
        let (sample, baseline) = {
            let mut s = self.stats.lock().unwrap();
            let mut lat = std::mem::take(&mut s.lat_nanos);
            lat.sort_unstable();
            let pct = |p: f64| -> u64 {
                if lat.is_empty() {
                    return 0;
                }
                lat[((lat.len() - 1) as f64 * p).round() as usize]
            };
            let win = WindowSample {
                inflight_peak: s.inflight_peak,
                lat_p50: pct(0.50),
                lat_p99: pct(0.99),
                completions: lat.len() as u64,
            };
            s.inflight_peak = 0;
            s.baseline_p99 = if s.baseline_p99 == 0 {
                win.lat_p99
            } else {
                QdSample::roll_baseline(s.baseline_p99, win.lat_p99, BASELINE_ALPHA)
            };
            s.last = win;
            (win, s.baseline_p99)
        };
        // 没有完成的普通 I/O 时没有指标：窗口作废，不把「没流量」当成「没饱和」。
        if sample.completions == 0 || slo == 0 {
            return;
        }
        let usable = self.slots.usable();
        let saturated = sample.inflight_peak * 10 >= usable * 9;
        let qd_sample = QdSample {
            inflight: sample.inflight_peak,
            lat_p99: sample.lat_p99,
            // 峰值逼近额度即视为设备吃满；真正的 util 来自 `ResourceStats`，MVP 无采样源。
            util: if saturated { 1.0 } else { 0.0 },
            slo,
            baseline_p99: baseline,
        };
        if self.tuner.lock().unwrap().observe(&qd_sample).changed {
            let next = self.tuner.lock().unwrap().usable();
            self.slots.set_usable(next);
            self.adjustments.fetch_add(1, Ordering::SeqCst);
        }
    }
}

fn execute(dev: &dyn StorageDevice, op: &DeviceOp) -> KernelResult<DeviceResult> {
    match op {
        DeviceOp::Write { loc, data, .. } => super::executor::block_on(dev.write(loc, data))?,
        DeviceOp::Read { loc, len, .. } => {
            let mut buf = vec![0u8; *len as usize];
            super::executor::block_on(dev.read(loc, &mut buf))?;
        }
        DeviceOp::Flush { loc, level, .. } => super::executor::block_on(dev.flush(loc, *level))?,
        DeviceOp::Discard { loc, .. } => super::executor::block_on(dev.discard(loc))?,
    }
    Ok(DeviceResult)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hal::mem::MemDevice;
    use crate::types::DeviceId;
    use std::sync::Mutex as StdMutex;

    /// 每次操作都慢一点的设备：让调用方有足够时间把额度占满。
    struct SlowDevice {
        inner: MemDevice,
        busy: Arc<StdMutex<u32>>,
        delay: Duration,
    }

    impl SlowDevice {
        fn new(delay: Duration) -> Arc<Self> {
            Arc::new(SlowDevice {
                inner: MemDevice::new(DeviceId(1), 1 << 20),
                busy: Arc::new(StdMutex::new(0)),
                delay,
            })
        }

        fn enter(&self) {
            // 计数先落盘再睡：持着锁 sleep 会把设备本身变成串行的。
            *self.busy.lock().unwrap() += 1;
            std::thread::sleep(self.delay);
            *self.busy.lock().unwrap() -= 1;
        }
    }

    impl StorageDevice for SlowDevice {
        fn descriptor(&self) -> &DeviceDescriptor {
            self.inner.descriptor()
        }
        fn write<'a>(
            &'a self,
            loc: &'a PhysicalLocation,
            buf: &'a [u8],
        ) -> super::super::BoxFuture<'a, KernelResult<()>> {
            Box::pin(async move {
                self.enter();
                StorageDevice::write(&self.inner, loc, buf).await
            })
        }
        fn read<'a>(
            &'a self,
            loc: &'a PhysicalLocation,
            buf: &'a mut [u8],
        ) -> super::super::BoxFuture<'a, KernelResult<()>> {
            Box::pin(async move {
                self.enter();
                StorageDevice::read(&self.inner, loc, buf).await
            })
        }
        fn flush<'a>(
            &'a self,
            loc: &'a PhysicalLocation,
            level: DurabilityLevel,
        ) -> super::super::BoxFuture<'a, KernelResult<()>> {
            Box::pin(async move {
                self.enter();
                StorageDevice::flush(&self.inner, loc, level).await
            })
        }
        fn discard<'a>(
            &'a self,
            loc: &'a PhysicalLocation,
        ) -> super::super::BoxFuture<'a, KernelResult<()>> {
            Box::pin(async move {
                self.enter();
                StorageDevice::discard(&self.inner, loc).await
            })
        }
        fn max_durability(&self) -> DurabilityLevel {
            self.inner.max_durability()
        }
        fn health(&self) -> crate::types::DeviceHealth {
            self.inner.health()
        }
    }

    fn write_op(offset: u64) -> DeviceOp {
        DeviceOp::Write {
            site: DeviceSite::ExtentPayloadWrite,
            loc: PhysicalLocation {
                device: DeviceId(1),
                offset,
                length: 4096,
            },
            data: vec![0xAB; 4096],
        }
    }

    fn reactor(dev: Arc<SlowDevice>, usable: u32) -> Arc<ReactorIoExecutor> {
        let qd = QueueDepth::new(usable + 2, usable, 2).expect("valid");
        Arc::new(ReactorIoExecutor::new(
            Arc::new(Shared(dev)),
            &qd,
            Arc::new(crate::clock::SystemClock::new()),
            100_000_000,
            Duration::from_millis(2),
        ))
    }

    /// 共享底层设备：`ReactorIoExecutor` 要独占盒子，这里让测试仍能观察到它。
    struct Shared(Arc<SlowDevice>);

    impl StorageDevice for Shared {
        fn descriptor(&self) -> &DeviceDescriptor {
            self.0.descriptor()
        }
        fn write<'a>(
            &'a self,
            loc: &'a PhysicalLocation,
            buf: &'a [u8],
        ) -> super::super::BoxFuture<'a, KernelResult<()>> {
            self.0.write(loc, buf)
        }
        fn read<'a>(
            &'a self,
            loc: &'a PhysicalLocation,
            buf: &'a mut [u8],
        ) -> super::super::BoxFuture<'a, KernelResult<()>> {
            self.0.read(loc, buf)
        }
        fn flush<'a>(
            &'a self,
            loc: &'a PhysicalLocation,
            level: DurabilityLevel,
        ) -> super::super::BoxFuture<'a, KernelResult<()>> {
            self.0.flush(loc, level)
        }
        fn discard<'a>(
            &'a self,
            loc: &'a PhysicalLocation,
        ) -> super::super::BoxFuture<'a, KernelResult<()>> {
            self.0.discard(loc)
        }
        fn max_durability(&self) -> DurabilityLevel {
            self.0.max_durability()
        }
        fn health(&self) -> crate::types::DeviceHealth {
            self.0.health()
        }
    }

    #[test]
    fn concurrent_callers_share_the_reactor() {
        let dev = SlowDevice::new(Duration::from_millis(20));
        let ex = reactor(Arc::clone(&dev), 4);
        // 四个提交方用栅栏对齐。设备本身在单个 reactor 线程上执行，因此设备侧
        // 峰值恒为 1——这里断言的是**在飞额度**被用满，而不是设备内部并行。
        let start = Arc::new(std::sync::Barrier::new(4));
        let handles: Vec<_> = (0..4)
            .map(|i| {
                let ex = Arc::clone(&ex);
                let start = Arc::clone(&start);
                std::thread::spawn(move || {
                    start.wait();
                    ex.submit_and_wait(DeviceSite::ExtentPayloadWrite, write_op(i * 4096))
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap().expect("write completes");
        }
        assert!(
            ex.max_inflight_seen() >= 2,
            "ops shared the reactor: {}",
            ex.max_inflight_seen()
        );
    }

    #[test]
    fn saturating_the_budget_returns_backpressure() {
        let dev = SlowDevice::new(Duration::from_millis(30));
        let ex = reactor(Arc::clone(&dev), 2);
        // 两个普通槽位被占住：第三个普通写必须拿到背压，而不是排队等（§4.5）。
        let held: Vec<_> = (0..2)
            .map(|_| ex.inner.slots.acquire_ordinary().expect("slot"))
            .collect();
        let err = ex
            .submit_and_wait(DeviceSite::ExtentPayloadWrite, write_op(0))
            .expect_err("ordinary budget is exhausted");
        assert!(matches!(
            err,
            KernelError::Retryable(crate::error::RetryReason::Backpressure)
        ));
        // flush 用预留槽位，不受普通额度影响（§4.6）。
        let flush = DeviceOp::Flush {
            site: DeviceSite::WalFlushStart,
            loc: PhysicalLocation {
                device: DeviceId(1),
                offset: 0,
                length: 4096,
            },
            level: DurabilityLevel::Durable,
        };
        ex.submit_and_wait(DeviceSite::WalFlushStart, flush)
            .expect("flush uses the reserve");
        drop(held);
        // 额度归还后普通写又能通过：没有静默丢额度。
        ex.submit_and_wait(DeviceSite::ExtentPayloadWrite, write_op(4096))
            .expect("budget returned");
    }

    #[test]
    fn read_into_fills_the_buffer_through_the_reactor() {
        let dev = SlowDevice::new(Duration::from_millis(0));
        let ex = reactor(Arc::clone(&dev), 2);
        let loc = PhysicalLocation {
            device: DeviceId(1),
            offset: 8192,
            length: 512,
        };
        ex.submit_and_wait(
            DeviceSite::ExtentPayloadWrite,
            DeviceOp::Write {
                site: DeviceSite::ExtentPayloadWrite,
                loc,
                data: vec![7u8; 512],
            },
        )
        .expect("write");
        let mut buf = vec![0u8; 512];
        ex.read_into(DeviceSite::ExtentPayloadWrite, &loc, &mut buf)
            .expect("read");
        assert_eq!(buf, vec![7u8; 512]);
    }
}
