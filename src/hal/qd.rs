//! 队列深度、槽位占用与背压（07 §4，HRM7）。
//!
//! 三个量必须分开（07 §4.1–§4.2）：
//!
//! ```text
//! qd_budget(r)    = qd_usable(r) + qd_flush_reserve(r)
//! qd_effective(r) = min( qd_device(r), qd_budget(r) )
//! ```
//!
//! - `qd_device`：设备上报的硬顶，启动期读一次，运行期不变；
//! - `qd_usable`：普通读写的在途上限，**不含** flush 预留；
//! - `qd_flush_reserve`：给 flush 留出的槽位，至少 1（HRM7）。
//!
//! [`QueueDepth`] 只做纯计算（给定窗口指标 → 下一个 `qd_usable`），
//! 阈值与迟滞都不读时钟；[`Slots`] 承载在飞计数与背压判定，普通 I/O
//! 与 flush 走两套互不共享的额度。

use crate::clock::Clock;
use crate::error::{KernelError, RetryReason};
use std::sync::atomic::{AtomicU64, Ordering};

/// 设备上报的硬顶与域配置的份额（07 §4.2）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QdConfig {
    /// `qd_device`：设备能力上限（如 NVMe 的 `min(NVMOSQ, NVMOCQ)`）。
    pub device: u32,
    /// `qd_usable`：普通读写可用的在途数（域预算里的普通份额）。
    pub usable: u32,
    /// `qd_flush_reserve`：flush 预留槽位，至少 1。
    pub flush_reserve: u32,
}

impl QdConfig {
    /// 07 §4.2：`qd_budget = qd_usable + qd_flush_reserve`。
    pub fn budget(&self) -> u32 {
        self.usable.saturating_add(self.flush_reserve)
    }

    /// 07 §4.2：`qd_effective = min(qd_device, qd_budget)`。
    pub fn effective(&self) -> u32 {
        self.device.min(self.budget())
    }
}

/// QD 上限的推导结果。非法配置在启动期显式失败，不静默修正。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueueDepth {
    device: u32,
    /// 域配置的普通份额（`qd_usable` 的配置值）——自适应不能越过它。
    share: u32,
    flush_reserve: u32,
    /// 当前生效的普通在途上限（§4.3 调整的就是它，初始值见 `initial`）。
    usable: u32,
}

impl QueueDepth {
    /// 07 §4.2 与 HRM7：`qd_flush_reserve >= 1`，且 `qd_budget` 不得超过设备能力
    /// ——否则「预留」变成「超发」，普通 I/O 与 flush 合起来能超出设备。
    pub fn new(device: u32, usable: u32, flush_reserve: u32) -> Result<Self, KernelError> {
        let cfg = QdConfig {
            device,
            usable,
            flush_reserve,
        };
        if flush_reserve == 0 {
            return Err(KernelError::invalid(
                "qd_flush_reserve must be at least 1 (HRM7)",
            ));
        }
        if usable == 0 {
            return Err(KernelError::invalid("qd_usable must be at least 1"));
        }
        if cfg.budget() > device {
            return Err(KernelError::invalid(format!(
                "qd_budget {} exceeds device limit {}",
                cfg.budget(),
                device
            )));
        }
        Ok(QueueDepth {
            device,
            share: usable,
            flush_reserve,
            usable,
        })
    }

    /// 07 §4.2 的初始值：`qd_initial = min(qd_effective, 32)`。
    ///
    /// 普通 I/O 的自适应上限是 `qd_effective - qd_flush_reserve`，初始值再被它截一次。
    pub fn initial(device: u32, usable: u32, flush_reserve: u32) -> Result<Self, KernelError> {
        let qd = Self::new(device, usable, flush_reserve)?;
        let start = qd.effective().min(32).min(qd.ordinary_cap()).max(1);
        Ok(QueueDepth {
            usable: start,
            ..qd
        })
    }

    /// 域配置的普通份额：`qd_usable` 的配置值，取代自适应的当前值。
    pub fn share(&self) -> u32 {
        self.share
    }

    pub fn device_limit(&self) -> u32 {
        self.device
    }

    /// `qd_usable`：普通读写当前的在途上限（自适应调整的就是它）。
    pub fn usable(&self) -> u32 {
        self.usable
    }

    pub fn flush_reserve(&self) -> u32 {
        self.flush_reserve
    }

    pub fn budget(&self) -> u32 {
        self.share.saturating_add(self.flush_reserve)
    }

    pub fn effective(&self) -> u32 {
        self.device.min(self.budget())
    }

    /// 07 §4.3：普通 I/O 的自适应上限 = `qd_effective - qd_flush_reserve`。
    /// 拿 `qd_effective` 做阈值会把预留槽位推给普通读写，正是 §4.6 要防的。
    pub fn ordinary_cap(&self) -> u32 {
        self.effective().saturating_sub(self.flush_reserve)
    }
}

/// 一个监控窗口内的可观测指标（07 §4.3）。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct QdSample {
    /// 窗口内普通读写的在飞峰值。
    pub inflight: u32,
    pub lat_p99: u64,
    /// 设备利用率，`[0, 1]`。
    pub util: f64,
    pub slo: u64,
    /// 该设备的滚动基线 P99（EWMA 维护，§4.3）。
    pub baseline_p99: u64,
}

impl QdSample {
    /// 07 §4.3：`baseline_p99` 的滚动更新（EWMA，`alpha` ∈ `(0, 1]`）。
    pub fn roll_baseline(baseline: u64, lat_p99: u64, alpha: f64) -> u64 {
        let alpha = alpha.clamp(0.0, 1.0);
        ((1.0 - alpha) * baseline as f64 + alpha * lat_p99 as f64).round() as u64
    }
}

/// 一次窗口判定的结果（07 §4.3–§4.4）。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct QdDecision {
    /// 新的 `qd_usable`。
    pub usable: u32,
    pub changed: bool,
}

/// 自适应调节器：纯函数式的 `sample → decision`，连升规则与迟滞状态都在调用方维护。
pub struct QdTuner {
    device: u32,
    share: u32,
    flush_reserve: u32,
    usable: u32,
    up_windows: u32,
    windows_since_adjust: u32,
    /// §4.4 冷却：两次调整之间至少隔一个监控窗口。
    cooldown_windows: u32,
    /// §4.4 迟滞：连续满足升条件才升；降只需一个窗口。
    up_threshold: u32,
}

impl QdTuner {
    pub fn new(qd: QueueDepth, cooldown_windows: u32, up_threshold: u32) -> Self {
        QdTuner {
            device: qd.device,
            share: qd.share,
            flush_reserve: qd.flush_reserve,
            usable: qd.usable,
            up_windows: 0,
            windows_since_adjust: 1 << 20,
            cooldown_windows,
            up_threshold: up_threshold.max(1),
        }
    }

    pub fn usable(&self) -> u32 {
        self.usable
    }

    /// 设备硬顶、普通份额与 flush 预留。`effective()` 取 `min(device, usable + reserve)`，
    /// 而 `usable` 就是当前的普通份额，所以它不会随自适应再把上限压回去。
    fn limit(&self) -> QueueDepth {
        QueueDepth {
            device: self.device,
            share: self.share,
            flush_reserve: self.flush_reserve,
            usable: self.usable,
        }
    }

    /// 消费一个监控窗口，返回本次判定。阈值全部相对 `qd_usable`（§4.3）。
    pub fn observe(&mut self, s: &QdSample) -> QdDecision {
        let prev = self.usable;
        self.windows_since_adjust = self.windows_since_adjust.saturating_add(1);
        let usable = self.usable as f64;
        let saturated = s.inflight as f64 >= 0.9 * usable;
        let headroom = s.lat_p99 as f64 <= 0.8 * s.slo as f64;
        let degrading = s.baseline_p99 > 0 && (s.lat_p99 as f64) > 1.5 * s.baseline_p99 as f64;
        let over_slo = s.lat_p99 > s.slo;

        if degrading && self.windows_since_adjust >= self.cooldown_windows {
            // 降：×0.7 向下取 2 的幂，下限 1。降晚一个窗口比升晚一个昂贵（§4.4）。
            let next = pow2_floor((usable * 0.7).floor() as u32).max(1);
            if next < self.usable {
                self.usable = next;
                self.up_windows = 0;
                self.windows_since_adjust = 0;
            }
        } else if saturated && headroom {
            self.up_windows += 1;
            // 设备没吃满就不动：延迟超标但 util < 0.6 说明瓶颈不在设备（§4.3 第三条）。
            if over_slo && s.util < 0.6 {
                self.up_windows = 0;
            } else if self.up_windows >= self.up_threshold
                && self.windows_since_adjust >= self.cooldown_windows
            {
                // 上限只看本次调整能涨到哪，不经过「恢复后的 qd_usable」——
                // 否则第一次升档就会把自己卡死。
                let scaled = (usable * 1.5).ceil() as u32;
                let next = pow2_ceil(scaled).min(self.limit().ordinary_cap());
                if next > self.usable {
                    self.usable = next;
                    self.windows_since_adjust = 0;
                }
                self.up_windows = 0;
            }
        } else {
            self.up_windows = 0;
        }
        QdDecision {
            usable: self.usable,
            changed: self.usable != prev,
        }
    }
}

fn pow2_ceil(v: u32) -> u32 {
    if v <= 1 {
        return 1;
    }
    v.next_power_of_two()
}

/// 向下取到最近的 2 的幂（io_uring 的 ring 大小必须是 2 的幂，§4.3）。
fn pow2_floor(v: u32) -> u32 {
    if v == 0 {
        return 0;
    }
    1 << (31 - v.leading_zeros())
}

/// 在飞操作的额度（07 §4.5–§4.6）：普通 I/O 与 flush 两套独立额度。
///
/// 普通 I/O 只能占 `qd_usable` 个槽位，flush 只能占 `qd_flush_reserve` 个——
/// 两者不共享，因此普通 I/O 再满也饿不死 flush（HRM7 的可测试形式）。
pub struct Slots {
    usable: u32,
    reserve: u32,
    ordinary: AtomicU64,
    flush: AtomicU64,
    deadline_nanos: u64,
    clock: Box<dyn Clock>,
}

impl Slots {
    pub fn new(qd: &QueueDepth, deadline_nanos: u64, clock: Box<dyn Clock>) -> Self {
        Slots {
            usable: qd.usable,
            reserve: qd.flush_reserve,
            ordinary: AtomicU64::new(0),
            flush: AtomicU64::new(0),
            deadline_nanos,
            clock,
        }
    }

    pub fn usable(&self) -> u32 {
        self.usable
    }

    pub fn flush_reserve(&self) -> u32 {
        self.reserve
    }

    pub fn ordinary_inflight(&self) -> u32 {
        self.ordinary.load(Ordering::SeqCst) as u32
    }

    pub fn flush_inflight(&self) -> u32 {
        self.flush.load(Ordering::SeqCst) as u32
    }

    /// 尝试占用一个普通 I/O 槽位；满了就等到 deadline，仍无空位则返回
    /// `Retryable(Backpressure)`——不会静默丢掉这次操作，也不会自动重试。
    pub fn acquire_ordinary(&self) -> Result<SlotGuard<'_>, KernelError> {
        self.acquire(&self.ordinary, self.usable, false)
    }

    /// flush 走独立额度，普通 I/O 打满时它仍能拿到槽位（§4.6）。
    pub fn acquire_flush(&self) -> Result<SlotGuard<'_>, KernelError> {
        self.acquire(&self.flush, self.reserve, true)
    }

    fn acquire<'a>(
        &'a self,
        counter: &'a AtomicU64,
        limit: u32,
        _flush: bool,
    ) -> Result<SlotGuard<'a>, KernelError> {
        let start = self.clock.monotonic_nanos();
        loop {
            let cur = counter.load(Ordering::SeqCst);
            if cur < limit as u64 {
                if counter
                    .compare_exchange(cur, cur + 1, Ordering::SeqCst, Ordering::SeqCst)
                    .is_ok()
                {
                    return Ok(SlotGuard { counter });
                }
                continue;
            }
            if self.clock.monotonic_nanos().saturating_sub(start) >= self.deadline_nanos {
                return Err(KernelError::Retryable(RetryReason::Backpressure));
            }
            std::thread::yield_now();
        }
    }
}

/// 槽位占用凭据：`Drop` 即归还，因此 panic 展开路径也不会泄漏额度。
pub struct SlotGuard<'a> {
    counter: &'a AtomicU64,
}

impl Drop for SlotGuard<'_> {
    fn drop(&mut self) {
        self.counter.fetch_sub(1, Ordering::SeqCst);
    }
}

/// 每个路径的 batch 初值与区间（07 §4.2 表、§4.7）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IoPath {
    /// 前台读：吞吐优先。
    ForegroundRead,
    /// 前台写：延迟优先。
    ForegroundWrite,
    /// 后台重建（索引段合并等）：吞吐优先，可暂停。
    BackgroundRebuild,
    /// WAL / flush：屏障，一次一个。
    Flush,
}

impl IoPath {
    /// 07 §4.2：`batch_initial(p)` 与允许区间 `(min, max)`。
    pub fn batch_initial(self) -> u32 {
        match self {
            IoPath::ForegroundRead => 16,
            IoPath::ForegroundWrite => 4,
            IoPath::BackgroundRebuild => 32,
            IoPath::Flush => 1,
        }
    }

    pub fn batch_range(self) -> (u32, u32) {
        match self {
            IoPath::ForegroundRead => (16, 64),
            IoPath::ForegroundWrite => (1, 4),
            IoPath::BackgroundRebuild => (32, 128),
            IoPath::Flush => (1, 1),
        }
    }

    /// 07 §4.2：`batch_initial(p) = min(batch_initial(p), qd_usable(p))`，
    /// 并且不得越出该路径自己的区间。`qd_usable` 是普通份额；flush 走预留槽位。
    pub fn batch_within(self, qd_usable: u32) -> u32 {
        let (lo, hi) = self.batch_range();
        self.batch_initial()
            .min(qd_usable)
            .clamp(lo.min(qd_usable), hi)
    }
}

/// 07 §4.2：`qd_device` 的上限来源——按每设备一个队列使用，
/// 取 `min(NVMOSQ, NVMOCQ)` 再与内核侧保守界（`nr_requests`）取小。
pub fn device_qd_limit(max_ops_per_queue: u32, max_queues: u32, nr_requests: u32) -> u32 {
    max_ops_per_queue.min(max_queues).min(nr_requests).max(1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::ManualClock;
    use crate::error::{KernelError, RetryReason};
    use crate::types::{DeviceId, DurabilityLevel, PhysicalLocation};
    use std::sync::Arc;

    fn sample(inflight: u32, lat_p99: u64, util: f64, slo: u64, baseline: u64) -> QdSample {
        QdSample {
            inflight,
            lat_p99,
            util,
            slo,
            baseline_p99: baseline,
        }
    }

    #[test]
    fn flush_slot_reserved_and_budget_never_exceeds_device() {
        let qd = QueueDepth::new(16, 12, 4).expect("valid");
        assert_eq!(qd.budget(), 16);
        assert_eq!(qd.effective(), 16);
        assert_eq!(qd.ordinary_cap(), 12);
        // 设备额度越紧，预留对普通 I/O 的挤压越明显，但不能超发（§4.6）。
        let tight = QueueDepth::new(8, 4, 4).expect("valid");
        assert_eq!(tight.effective(), 8);
    }

    #[test]
    fn zero_flush_reserve_is_rejected() {
        let err = QueueDepth::new(64, 64, 0).expect_err("HRM7 requires a reserve");
        assert!(matches!(err, KernelError::InvalidArgument { .. }));
    }

    #[test]
    fn budget_above_device_is_rejected() {
        let err = QueueDepth::new(16, 14, 4).expect_err("budget > device must fail");
        assert!(matches!(err, KernelError::InvalidArgument { .. }));
    }

    #[test]
    fn qd_respects_device_limit_and_starts_at_32() {
        // 假设备上报一个很小的队列深度上限（07 §7.1）。
        let small = QueueDepth::initial(6, 4, 2).expect("valid");
        assert!(small.effective() <= 6);
        assert_eq!(small.usable(), 4);

        let big = QueueDepth::initial(1024, 1000, 8).expect("valid");
        assert_eq!(big.usable(), 32);
        let mid = QueueDepth::initial(64, 48, 4).expect("valid");
        assert_eq!(mid.usable(), 32);
    }

    fn tuner(share: u32) -> QdTuner {
        let qd = QueueDepth::initial(256, share, 4).expect("valid");
        // 冷却取 0，让三个连续窗口的升档规则本身可见；冷却另有单独用例。
        QdTuner::new(qd, 0, 3)
    }

    #[test]
    fn qd_grows_when_saturated_with_headroom() {
        let mut t = tuner(64);
        assert_eq!(t.usable(), 32, "qd_initial = min(qd_effective, 32)");
        // 饱和与 SLO 之间留出余量：`0.8 × SLO = 80`，取 75。
        let s = sample(31, 75, 0.95, 100, 70);
        assert!(!t.observe(&s).changed, "one window is a spike, not a trend");
        assert!(!t.observe(&s).changed);
        assert!(t.observe(&s).changed);
        assert_eq!(t.usable(), 64);
    }

    #[test]
    fn qd_needs_three_windows_and_a_cooldown_before_growing_again() {
        // 07 §4.4：升 QD 要连续 3 个窗口满足，且两次调整之间至少隔一个窗口。
        let qd = QueueDepth::initial(256, 64, 4).expect("valid");
        let mut t = QdTuner::new(qd, 1, 3);
        let s = sample(31, 75, 0.95, 100, 70);
        assert!(!t.observe(&s).changed, "one window is a spike, not a trend");
        assert!(!t.observe(&s).changed);
        assert!(t.observe(&s).changed);
        assert_eq!(t.usable(), 64);
        // 升档之后的第一个窗口处于冷却期，即使条件满足也不动。
        assert!(!t.observe(&s).changed);
    }

    #[test]
    fn qd_never_exceeds_ordinary_cap() {
        // 上限 = qd_effective - qd_flush_reserve = 40，取 2 的幂后 32 不涨到 64 以上。
        let mut t = tuner(40);
        for _ in 0..12 {
            t.observe(&sample(40, 75, 0.99, 100, 70));
        }
        assert!(t.usable() <= 40);
    }

    #[test]
    fn qd_drops_in_one_window() {
        let mut t = tuner(64);
        let d = t.observe(&sample(1, 200, 0.9, 100, 50));
        assert!(d.changed);
        assert!(t.usable() < 64);
    }

    #[test]
    fn qd_no_adjust_when_device_not_saturated() {
        // lat_p99 > SLO 且 util < 0.6：瓶颈不在设备，调 QD 只会更糟（§4.3）。
        let mut t = tuner(32);
        for _ in 0..10 {
            assert!(!t.observe(&sample(32, 120, 0.5, 100, 100)).changed);
        }
        assert_eq!(t.usable(), 32);
    }

    #[test]
    fn qd_does_not_ride_on_noise() {
        let mut t = tuner(32);
        t.observe(&sample(31, 75, 0.95, 100, 70));
        t.observe(&sample(10, 75, 0.5, 100, 70));
        t.observe(&sample(31, 75, 0.95, 100, 70));
        assert_eq!(t.usable(), 32, "a gap between windows resets the streak");
    }

    #[test]
    fn backpressure_is_retryable_and_never_silently_truncates() {
        let qd = QueueDepth::new(4, 2, 2).expect("valid");
        let clock = Arc::new(ManualClock::new(0));
        let slots = Slots::new(&qd, 0, Box::new(SharedClock(clock.clone())));
        let a = slots.acquire_ordinary().expect("first");
        let b = slots.acquire_ordinary().expect("second");
        // deadline 为 0：满额后立即超时，返回 `Retryable(Backpressure)`（HRM7）。
        let err = slots.acquire_ordinary().err().expect("queue is full");
        assert!(matches!(
            err,
            KernelError::Retryable(RetryReason::Backpressure)
        ));
        drop(a);
        drop(b);
        assert_eq!(slots.ordinary_inflight(), 0);
        // 归还后可以重新拿到，说明没有静默丢额度。
        slots.acquire_ordinary().expect("slot returned");
    }

    #[test]
    fn flush_is_never_starved_by_ordinary_io() {
        let qd = QueueDepth::new(8, 4, 1).expect("valid");
        let clock = Arc::new(ManualClock::new(0));
        let slots = Slots::new(&qd, 0, Box::new(SharedClock(clock)));
        let mut held = Vec::new();
        for _ in 0..4 {
            held.push(slots.acquire_ordinary().expect("ordinary slot"));
        }
        // 普通 I/O 打满 `qd_usable`，flush 仍拿得到预留槽位（§4.6）。
        let f = slots.acquire_flush().expect("reserved slot");
        let second = slots
            .acquire_flush()
            .err()
            .expect("only the reserve, no more");
        assert!(matches!(
            second,
            KernelError::Retryable(RetryReason::Backpressure)
        ));
        drop(f);
        assert_eq!(slots.flush_inflight(), 0);
        // 归还后预留槽位仍可用。
        slots.acquire_flush().expect("reserve returned");
        drop(held);
    }

    struct SharedClock(Arc<ManualClock>);

    impl Clock for SharedClock {
        fn monotonic_nanos(&self) -> u64 {
            self.0.monotonic_nanos()
        }
    }

    #[test]
    fn batch_initial_differs_per_path() {
        assert_eq!(IoPath::ForegroundRead.batch_within(32), 16);
        assert_eq!(IoPath::ForegroundWrite.batch_within(32), 4);
        assert_eq!(IoPath::BackgroundRebuild.batch_within(32), 32);
        assert_eq!(IoPath::Flush.batch_within(32), 1);
        // 不得越出区间：浅队列时读路径也降到 16 以下。
        assert_eq!(IoPath::ForegroundRead.batch_within(8), 8);
    }

    #[test]
    fn device_limit_takes_the_minimum_bound() {
        assert_eq!(device_qd_limit(1024, 64, 128), 64);
        assert_eq!(device_qd_limit(128, 64, 4096), 64);
        assert_eq!(device_qd_limit(1024, 1024, 256), 256);
    }

    #[test]
    fn baseline_tracks_slowdown_with_ewma() {
        assert_eq!(QdSample::roll_baseline(100, 200, 0.5), 150);
        assert_eq!(QdSample::roll_baseline(100, 100, 0.2), 100);
    }

    #[test]
    fn executor_returns_backpressure_instead_of_dropping_ops() {
        use crate::hal::mem::MemDevice;
        use crate::hal::StorageDevice;
        use crate::hal::{DeviceOp, DeviceSite, IoExecutor, SyncIoExecutor};

        // 用最小预算把普通槽位占满，再提交一条普通写：必须拿到背压而不是被静默丢弃。
        let dev: Box<dyn StorageDevice> = Box::new(MemDevice::new(DeviceId(1), 1 << 20));
        let qd = QueueDepth::new(2, 1, 1).expect("valid");
        let ex = SyncIoExecutor::with_queue_depth(dev, &qd, 0);
        let guard = ex.slots().acquire_ordinary().expect("the only slot");
        let op = DeviceOp::Write {
            site: DeviceSite::WalFrameWrite,
            loc: PhysicalLocation {
                device: DeviceId(1),
                offset: 0,
                length: 512,
            },
            data: vec![0u8; 512],
        };
        let err = ex
            .submit_and_wait(DeviceSite::WalFrameWrite, op)
            .expect_err("ordinary budget is full");
        assert!(matches!(
            err,
            KernelError::Retryable(RetryReason::Backpressure)
        ));
        // 同样的状态下 flush 仍能完成：预留槽位没有被普通 I/O 吃掉。
        let flush = DeviceOp::Flush {
            site: DeviceSite::WalFlushStart,
            loc: PhysicalLocation {
                device: DeviceId(1),
                offset: 0,
                length: 512,
            },
            level: DurabilityLevel::Durable,
        };
        ex.submit_and_wait(DeviceSite::WalFlushStart, flush)
            .expect("flush uses the reserve");
        drop(guard);
    }
}
