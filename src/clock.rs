//! 时间源 `Clock`（04 §2.6，F15 冻结清单）。
//!
//! 内核代码不得直接调用 `Instant::now()` / `SystemTime::now()`（C5）。
//! 内核只读相对单调纳秒；墙上时间由上层在写入时提供（C5.2）。

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

pub trait Clock: Send + Sync {
    /// 单调递增的纳秒计数，起点是内核初始化时刻。
    fn monotonic_nanos(&self) -> u64;
}

/// 生产实现：相对进程启动的单调时钟。
pub struct SystemClock {
    start: std::time::Instant,
}

impl SystemClock {
    pub fn new() -> Self {
        SystemClock {
            start: std::time::Instant::now(),
        }
    }
}

impl Default for SystemClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for SystemClock {
    fn monotonic_nanos(&self) -> u64 {
        self.start.elapsed().as_nanos() as u64
    }
}

/// 测试用可控时钟（05 §4.1）。
///
/// 放在生产模块里而不是 `testing` 模块，是因为它不依赖任何夹具能力，
/// 且内核初始化需要一个默认实现。`testing` feature 只是把它暴露得更方便。
pub struct ManualClock {
    now: AtomicU64,
}

impl ManualClock {
    pub fn new(start: u64) -> Self {
        ManualClock {
            now: AtomicU64::new(start),
        }
    }

    pub fn advance(&self, nanos: u64) {
        self.now.fetch_add(nanos, Ordering::SeqCst);
    }

    pub fn set(&self, nanos: u64) {
        self.now.store(nanos, Ordering::SeqCst);
    }
}

impl Clock for ManualClock {
    fn monotonic_nanos(&self) -> u64 {
        self.now.load(Ordering::SeqCst)
    }
}

/// 常用别名：内核持有一个共享时钟。
pub type SharedClock = Arc<dyn Clock>;
