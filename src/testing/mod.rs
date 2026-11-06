//! 故障注入夹具（05-kernel-test-harness）。
//!
//! 只在 `feature = "test-harness"` 下编译，发布构建里整个模块不存在（05 §6 约束 4）。
//!
//! MVP 提供四件东西：
//! - [`FaultDevice`]：包装任意 `StorageDevice`，在语义 site 上注入故障；
//! - [`FaultSite`]：设计文档 §3.2 语义 site 目录的子集；
//! - [`FaultKind`]：崩溃（静默丢弃本次写）、I/O 错误、部分写；
//! - [`PlanInjector`]：确定性调度（第 N 次命中某 site 时触发）。
//!
//! 关键语义：`power_loss` 崩溃只丢弃**本次**写，之前的写保留——
//! 这正是「掉了」与「没写」的区别，也是提交契约能否被验证的前提。

use crate::error::{IoOp, KernelError, KernelResult};
use crate::hal::{BoxFuture, DeviceDescriptor, DeviceOp, IoExecutor, StorageDevice};
use crate::types::{DeviceHealth, DurabilityLevel, PhysicalLocation};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

/// 语义 site（05 §3.2 的子集）。
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum FaultSite {
    /// WAL 记录帧写入。
    WalWrite,
    /// WAL flush（wal barrier）。
    WalFlush,
    /// payload extent 写入。
    PayloadWrite,
    /// payload flush（data barrier）。
    PayloadFlush,
    /// 任意设备写。
    DeviceWrite,
    /// 任意设备 flush。
    DeviceFlush,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FaultKind {
    /// 崩溃：本次操作不生效（已持久的部分保留）。
    Crash,
    /// 返回 I/O 错误。
    IoError,
    /// 部分写：只写入前 N 字节。
    PartialWrite(usize),
}

/// 注入规则：在命中 `site` 第 `at` 次时触发 `kind`。
#[derive(Clone, Debug)]
pub struct Injection {
    pub site: FaultSite,
    pub at: u64,
    pub kind: FaultKind,
}

/// 确定性注入器（05 §3.4）。
#[derive(Default)]
pub struct PlanInjector {
    hits: Mutex<BTreeMap<FaultSite, u64>>,
    plan: Mutex<Vec<Injection>>,
    fired: Mutex<Vec<Injection>>,
    crashed: Mutex<Option<FaultSite>>,
}

impl PlanInjector {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add(&self, injection: Injection) {
        self.plan.lock().unwrap().push(injection);
    }

    /// 记录一次 site 命中；若命中注入规则且系统尚未崩溃，返回故障。
    pub fn hit(&self, site: FaultSite) -> Option<FaultKind> {
        if self.crashed.lock().unwrap().is_some() {
            // 崩溃之后设备不再响应。
            return Some(FaultKind::Crash);
        }
        let mut hits = self.hits.lock().unwrap();
        let n = hits.entry(site).or_insert(0);
        *n += 1;
        let count = *n;
        drop(hits);

        let mut plan = self.plan.lock().unwrap();
        let pos = plan
            .iter()
            .position(|i| i.site == site && i.at == count)?;
        let inj = plan.remove(pos);
        drop(plan);

        if inj.kind == FaultKind::Crash {
            *self.crashed.lock().unwrap() = Some(site);
        }
        self.fired.lock().unwrap().push(inj.clone());
        Some(inj.kind)
    }

    pub fn has_crashed(&self) -> bool {
        self.crashed.lock().unwrap().is_some()
    }

    pub fn fired(&self) -> Vec<Injection> {
        self.fired.lock().unwrap().clone()
    }

    pub fn hits(&self, site: FaultSite) -> u64 {
        self.hits.lock().unwrap().get(&site).copied().unwrap_or(0)
    }
}

/// 包装设备：在设备 op 上定位语义 site 并注入故障。
pub struct FaultDevice {
    inner: Box<dyn StorageDevice>,
    injector: PlanInjector,
    /// WAL 区范围，用于把设备偏移归类为 WAL / payload。
    wal_base: u64,
    wal_end: u64,
    ops: AtomicU64,
}

impl FaultDevice {
    pub fn new(inner: Box<dyn StorageDevice>, wal_base: u64, wal_end: u64) -> Self {
        FaultDevice {
            inner,
            injector: PlanInjector::new(),
            wal_base,
            wal_end,
            ops: AtomicU64::new(0),
        }
    }

    pub fn injector(&self) -> &PlanInjector {
        &self.injector
    }

    pub fn op_count(&self) -> u64 {
        self.ops.load(Ordering::SeqCst)
    }

    fn site_for(&self, loc: &PhysicalLocation, write: bool) -> FaultSite {
        let in_wal = loc.offset >= self.wal_base && loc.offset < self.wal_end;
        match (write, in_wal) {
            (true, true) => FaultSite::WalWrite,
            (true, false) => FaultSite::PayloadWrite,
            (false, true) => FaultSite::WalFlush,
            (false, false) => FaultSite::PayloadFlush,
        }
    }

    fn crash_error(op: IoOp) -> KernelError {
        KernelError::io(
            op,
            std::io::Error::new(std::io::ErrorKind::Other, "simulated power loss"),
        )
    }
}

impl StorageDevice for FaultDevice {
    fn descriptor(&self) -> &DeviceDescriptor {
        self.inner.descriptor()
    }

    fn write<'a>(
        &'a self,
        loc: &'a PhysicalLocation,
        buf: &'a [u8],
    ) -> BoxFuture<'a, KernelResult<()>> {
        Box::pin(async move {
            self.ops.fetch_add(1, Ordering::SeqCst);
            let site = self.site_for(loc, true);
            match self.injector.hit(site) {
                Some(FaultKind::Crash) => Err(Self::crash_error(IoOp::Write)),
                Some(FaultKind::IoError) => Err(KernelError::io(
                    IoOp::Write,
                    std::io::Error::new(std::io::ErrorKind::Other, "injected write error"),
                )),
                Some(FaultKind::PartialWrite(n)) => {
                    let n = n.min(buf.len());
                    self.inner.write(loc, &buf[..n]).await
                }
                None => self.inner.write(loc, buf).await,
            }
        })
    }

    fn read<'a>(
        &'a self,
        loc: &'a PhysicalLocation,
        buf: &'a mut [u8],
    ) -> BoxFuture<'a, KernelResult<()>> {
        Box::pin(async move {
            self.ops.fetch_add(1, Ordering::SeqCst);
            if self.injector.has_crashed() {
                return Err(Self::crash_error(IoOp::Read));
            }
            self.inner.read(loc, buf).await
        })
    }

    fn flush<'a>(
        &'a self,
        loc: &'a PhysicalLocation,
        level: DurabilityLevel,
    ) -> BoxFuture<'a, KernelResult<()>> {
        Box::pin(async move {
            self.ops.fetch_add(1, Ordering::SeqCst);
            let site = self.site_for(loc, false);
            match self.injector.hit(site) {
                Some(FaultKind::Crash) => Err(Self::crash_error(IoOp::Flush)),
                Some(FaultKind::IoError) => Err(KernelError::io(
                    IoOp::Flush,
                    std::io::Error::new(std::io::ErrorKind::Other, "injected flush error"),
                )),
                _ => self.inner.flush(loc, level).await,
            }
        })
    }

    fn discard<'a>(&'a self, loc: &'a PhysicalLocation) -> BoxFuture<'a, KernelResult<()>> {
        Box::pin(async move { self.inner.discard(loc).await })
    }

    fn max_durability(&self) -> DurabilityLevel {
        self.inner.max_durability()
    }

    fn health(&self) -> DeviceHealth {
        if self.injector.has_crashed() {
            DeviceHealth::Failed {
                reason: "power loss".into(),
            }
        } else {
            self.inner.health()
        }
    }
}

/// 共享介质：让崩溃后用“新 FaultDevice + 新注入器”重新包住同一份设备。
/// 注入器状态不会泄漏到重启后的实例，这正是 05 §3.4 的语义。
#[derive(Clone)]
pub struct SharedDevice(pub std::sync::Arc<dyn StorageDevice>);

impl SharedDevice {
    pub fn new(inner: std::sync::Arc<dyn StorageDevice>) -> Self {
        SharedDevice(inner)
    }
}

impl StorageDevice for SharedDevice {
    fn descriptor(&self) -> &DeviceDescriptor {
        self.0.descriptor()
    }

    fn write<'a>(
        &'a self,
        loc: &'a PhysicalLocation,
        buf: &'a [u8],
    ) -> BoxFuture<'a, KernelResult<()>> {
        self.0.write(loc, buf)
    }

    fn read<'a>(
        &'a self,
        loc: &'a PhysicalLocation,
        buf: &'a mut [u8],
    ) -> BoxFuture<'a, KernelResult<()>> {
        self.0.read(loc, buf)
    }

    fn flush<'a>(
        &'a self,
        loc: &'a PhysicalLocation,
        level: DurabilityLevel,
    ) -> BoxFuture<'a, KernelResult<()>> {
        self.0.flush(loc, level)
    }

    fn discard<'a>(&'a self, loc: &'a PhysicalLocation) -> BoxFuture<'a, KernelResult<()>> {
        self.0.discard(loc)
    }

    fn max_durability(&self) -> DurabilityLevel {
        self.0.max_durability()
    }

    fn health(&self) -> DeviceHealth {
        self.0.health()
    }
}

/// 把注入后续的 fork / 重启用例包装得更顺手的执行器。
pub struct FaultIo {
    inner: crate::hal::SyncIoExecutor,
}

impl FaultIo {
    pub fn new(device: Box<dyn StorageDevice>) -> Self {
        FaultIo {
            inner: crate::hal::SyncIoExecutor::new(device),
        }
    }
}

impl IoExecutor for FaultIo {
    fn submit_and_wait(&self, op: DeviceOp) -> KernelResult<crate::hal::DeviceResult> {
        self.inner.submit_and_wait(op)
    }

    fn read_into(&self, loc: &PhysicalLocation, buf: &mut [u8]) -> KernelResult<()> {
        self.inner.read_into(loc, buf)
    }

    fn descriptor(&self) -> &DeviceDescriptor {
        self.inner.descriptor()
    }

    fn max_durability(&self) -> DurabilityLevel {
        self.inner.max_durability()
    }
}

// ---------------------------------------------------------------------------
// L1 单元层：WAL 帧校验与段扫描
// ---------------------------------------------------------------------------

pub mod l1;
