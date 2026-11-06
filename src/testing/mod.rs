//! 故障注入夹具（05-kernel-test-harness）。
//!
//! 只在 `feature = "test-harness"` 下编译，发布构建里整个模块不存在（05 §6 约束 4）。
//!
//! 提供三类东西：
//! - [`FaultIo`]：`IoExecutor` 的注入版。设备级注入点**都在这一层**——`before`
//!   （op 出队、尚未执行）与 `after`（设备确认成功、尚未回填）两个触发点（ADR-0011 决策 3）；
//! - [`DeviceSite`]（在 `hal` 里定义）：注入点的主键，由调用方在 `DeviceOp` 上声明；
//! - [`PlanInjector`]：确定性调度——第 N 次命中某 site（可限定动作类别）时触发故障。
//!
//! 两条关键语义：
//! - **site 不由地址推断。** MVP 早期的 `FaultDevice::site_for()` 按
//!   `PhysicalLocation.offset` 是否落在 WAL 区间猜 site，已按 ADR-0011 删除：
//!   布局一改，注入点就会漂移，而测试仍然通过；
//! - **崩溃只影响被故障的那个实例。** 注入器的 `crashed` 标志让之后的每个 op 都返回
//!   `Crash`，等价于「设备掉电后不再响应」；重启时用新的 `FaultIo` 包住同一份介质，
//!   注入器状态不延续（05 §3.4）。

use crate::error::{IoOp, KernelError, KernelResult};
use crate::hal::executor::block_on;
use crate::hal::{
    DeviceDescriptor, DeviceOp, DeviceResult, DeviceSite, IoExecutor, OpClass, StorageDevice,
};
use crate::types::DurabilityLevel;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// 触发点：一次设备操作的两个边界（ADR-0011 决策 3）。
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum TriggerPoint {
    /// op 出队、尚未调用 `StorageDevice`：该 op 完全未发生。
    Before,
    /// 设备确认成功、尚未回填完成事件：设备做了，调用方没来得及知道。
    After,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FaultKind {
    /// 崩溃：本次操作不生效（已持久的部分保留）。
    Crash,
    /// 返回 I/O 错误（分类为 `KernelError::Io`，不得被改写成 `Retryable`）。
    IoError,
    /// 部分写：只把前 N 字节交给设备。
    PartialWrite(usize),
    /// 静默丢弃：返回成功但数据没落盘（05 §3.3 的 `SilentDrop`）。
    SilentDrop,
}

/// 注入规则：在命中 `site`（且动作类别为 `class`，`None` = 任意）第 `at` 次时，
/// 在 `when` 这个触发点触发 `kind`。
#[derive(Clone, Debug)]
pub struct Injection {
    pub site: DeviceSite,
    /// 只在该类设备动作上计数（同一 site 可能既有 write 也有 flush）。
    pub class: Option<OpClass>,
    pub when: TriggerPoint,
    /// 命中序号，从 1 开始。
    pub at: u64,
    pub kind: FaultKind,
}

impl Injection {
    /// 在某个 site 第 `at` 次命中**之前**注入。
    pub fn before(site: DeviceSite, at: u64, kind: FaultKind) -> Self {
        Injection {
            site,
            class: None,
            when: TriggerPoint::Before,
            at,
            kind,
        }
    }

    /// 在某个 site 第 `at` 次**指定类别**命中之前注入。
    pub fn before_class(site: DeviceSite, class: OpClass, at: u64, kind: FaultKind) -> Self {
        Injection {
            site,
            class: Some(class),
            when: TriggerPoint::Before,
            at,
            kind,
        }
    }

    /// 在某个 site 第 `at` 次**指定类别**命中之后注入（设备已确认成功）。
    pub fn after_class(site: DeviceSite, class: OpClass, at: u64, kind: FaultKind) -> Self {
        Injection {
            site,
            class: Some(class),
            when: TriggerPoint::After,
            at,
            kind,
        }
    }
}

/// 一次设备操作的票据：`begin` 发号，`take` 据此取故障。
#[derive(Clone, Copy, Debug)]
pub struct OpTicket {
    pub site: DeviceSite,
    pub class: OpClass,
    /// 该 `(site, class)` 的第几次命中，从 1 开始。
    pub count: u64,
}

/// 确定性注入器（05 §3.4）。
#[derive(Default)]
pub struct PlanInjector {
    hits: Mutex<BTreeMap<(DeviceSite, OpClass), u64>>,
    plan: Mutex<Vec<Injection>>,
    fired: Mutex<Vec<Injection>>,
    crashed: Mutex<Option<DeviceSite>>,
}

impl PlanInjector {
    pub fn new() -> Self {
        Self::default()
    }

    /// 登记一条注入规则。
    ///
    /// 在 `Untagged` 上定义注入规则是**测试缺陷**（ADR-0011 决策 6）：未打标签的设备操作
    /// 说明有一处注入点漏接了，必须让测试立刻失败，而不是「通过但什么都没测到」。
    pub fn add(&self, injection: Injection) {
        assert!(
            injection.site.is_tagged(),
            "fault injection rule registered on DeviceSite::Untagged; \
             declare the semantic site where the DeviceOp is built (ADR-0011 I6)"
        );
        self.plan.lock().unwrap().push(injection);
    }

    /// 记一次 op 命中并发放票据。测试用它算「从现在起第几次」。
    pub fn begin(&self, site: DeviceSite, class: OpClass) -> OpTicket {
        assert!(
            site.is_tagged() || matches!(class, OpClass::Read | OpClass::Discard),
            "untagged device {class:?} reached the injector: every write/flush must \
             declare a DeviceSite where the DeviceOp is built (ADR-0011 I6)"
        );
        let count = {
            let mut hits = self.hits.lock().unwrap();
            let n = hits.entry((site, class)).or_insert(0);
            *n += 1;
            *n
        };
        OpTicket { site, class, count }
    }

    /// 取出某票据在某触发点上要触发的故障（若有）。
    ///
    /// 同一 op 上若有多条规则同时命中，崩溃优先，其余一并消费——
    /// 它们指向的正是这一次操作。
    pub fn take(&self, ticket: &OpTicket, when: TriggerPoint) -> Option<FaultKind> {
        if when == TriggerPoint::Before && self.has_crashed() {
            // 崩溃之后设备不再响应：任何后续 op 都失败。
            return Some(FaultKind::Crash);
        }
        let matched: Vec<usize> = {
            let plan = self.plan.lock().unwrap();
            plan.iter()
                .enumerate()
                .filter(|(_, i)| {
                    i.site == ticket.site
                        && i.when == when
                        && i.at == ticket.count
                        && match i.class {
                            None => true,
                            Some(c) => c == ticket.class,
                        }
                })
                .map(|(idx, _)| idx)
                .collect()
        };
        if matched.is_empty() {
            return None;
        }

        let mut plan = self.plan.lock().unwrap();
        let chosen = matched
            .iter()
            .copied()
            .find(|idx| plan[*idx].kind == FaultKind::Crash)
            .unwrap_or(matched[0]);
        let inj = plan[chosen].clone();
        // 降序删除，索引不会错位。
        for &idx in matched.iter().rev() {
            let removed = plan.remove(idx);
            if idx != chosen {
                self.fired.lock().unwrap().push(removed);
            }
        }
        drop(plan);

        if inj.kind == FaultKind::Crash {
            *self.crashed.lock().unwrap() = Some(inj.site);
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

    /// 某 site 的命中次数（所有动作类别合计）。
    pub fn hits(&self, site: DeviceSite) -> u64 {
        self.hits
            .lock()
            .unwrap()
            .iter()
            .filter(|((s, _), _)| *s == site)
            .map(|(_, n)| *n)
            .sum()
    }

    /// 某 site 在指定动作类别上的命中次数。
    pub fn hits_class(&self, site: DeviceSite, class: OpClass) -> u64 {
        self.hits
            .lock()
            .unwrap()
            .get(&(site, class))
            .copied()
            .unwrap_or(0)
    }

    /// 下一次命中某 site（任意类别）时有几条规则等着——测试用它表达「从现在起第 N 次」。
    pub fn next_hit(&self, site: DeviceSite, class: OpClass) -> u64 {
        self.hits_class(site, class) + 1
    }
}

/// 注入型执行器：设备级注入的两个触发点都在这里（ADR-0011 决策 3）。
///
/// 它同时承担 `IoExecutor` 与「假设备集群」的角色：`before` 决定这个 op 到底做不做，
/// `after` 决定「设备做了但调用方没收到」。
pub struct FaultIo {
    device: Arc<dyn StorageDevice>,
    injector: PlanInjector,
    ops: AtomicU64,
}

impl FaultIo {
    pub fn new(device: Arc<dyn StorageDevice>) -> Self {
        FaultIo {
            device,
            injector: PlanInjector::new(),
            ops: AtomicU64::new(0),
        }
    }

    pub fn injector(&self) -> &PlanInjector {
        &self.injector
    }

    pub fn op_count(&self) -> u64 {
        self.ops.load(Ordering::SeqCst)
    }

    fn io_op(class: OpClass) -> IoOp {
        match class {
            OpClass::Write | OpClass::Discard => IoOp::Write,
            OpClass::Read => IoOp::Read,
            OpClass::Flush => IoOp::Flush,
        }
    }

    fn crash_error(class: OpClass) -> KernelError {
        KernelError::io(
            Self::io_op(class),
            std::io::Error::new(std::io::ErrorKind::Other, "simulated power loss"),
        )
    }

    fn injected_error(class: OpClass) -> KernelError {
        KernelError::io(
            Self::io_op(class),
            std::io::Error::new(std::io::ErrorKind::Other, "injected i/o error"),
        )
    }

    /// `PartialWrite`：只把前 n 字节交给设备，其余字节不入设备。
    fn truncate(op: DeviceOp, n: usize) -> DeviceOp {
        match op {
            DeviceOp::Write { site, loc, data } => {
                let n = n.min(data.len());
                DeviceOp::Write {
                    site,
                    loc: crate::types::PhysicalLocation {
                        length: n as u64,
                        ..loc
                    },
                    data: data[..n].to_vec(),
                }
            }
            other => other,
        }
    }

    fn execute(&self, op: &DeviceOp) -> KernelResult<DeviceResult> {
        match op {
            DeviceOp::Write { loc, data, .. } => {
                block_on(self.device.write(loc, data))?;
                Ok(DeviceResult)
            }
            DeviceOp::Read { loc, len, .. } => {
                let mut buf = vec![0u8; *len as usize];
                block_on(self.device.read(loc, &mut buf))?;
                Ok(DeviceResult)
            }
            DeviceOp::Flush { loc, level, .. } => {
                block_on(self.device.flush(loc, *level))?;
                Ok(DeviceResult)
            }
            DeviceOp::Discard { loc, .. } => {
                block_on(self.device.discard(loc))?;
                Ok(DeviceResult)
            }
        }
    }
}

impl IoExecutor for FaultIo {
    fn submit_and_wait(&self, site: DeviceSite, op: DeviceOp) -> KernelResult<DeviceResult> {
        self.ops.fetch_add(1, Ordering::SeqCst);
        let class = op.class();
        let ticket = self.injector.begin(site, class);

        // before：出队、尚未执行。
        let op = match self.injector.take(&ticket, TriggerPoint::Before) {
            Some(FaultKind::Crash) => return Err(Self::crash_error(class)),
            Some(FaultKind::IoError) => return Err(Self::injected_error(class)),
            // 静默丢弃：返回成功，但设备上什么都没发生。
            Some(FaultKind::SilentDrop) => return Ok(DeviceResult),
            Some(FaultKind::PartialWrite(n)) => Self::truncate(op, n),
            None => op,
        };

        let result = self.execute(&op)?;

        // after：设备确认成功、尚未回填。
        if let Some(FaultKind::Crash) = self.injector.take(&ticket, TriggerPoint::After) {
            return Err(Self::crash_error(class));
        }
        Ok(result)
    }

    fn read_into(
        &self,
        site: DeviceSite,
        loc: &crate::types::PhysicalLocation,
        buf: &mut [u8],
    ) -> KernelResult<()> {
        self.ops.fetch_add(1, Ordering::SeqCst);
        let ticket = self.injector.begin(site, OpClass::Read);
        if let Some(FaultKind::Crash) = self.injector.take(&ticket, TriggerPoint::Before) {
            return Err(Self::crash_error(OpClass::Read));
        }
        block_on(self.device.read(loc, buf))?;
        Ok(())
    }

    fn descriptor(&self) -> &DeviceDescriptor {
        self.device.descriptor()
    }

    fn max_durability(&self) -> DurabilityLevel {
        self.device.max_durability()
    }
}

/// 共享介质：让崩溃后的重启用例包住同一份设备。
///
/// 注入器状态放在 `FaultIo` 上、不放在设备上，因此重启时新建一个 `FaultIo`
/// 就等价于「新进程 + 同一块盘」（05 §3.4）。
#[derive(Clone)]
pub struct SharedDevice(pub Arc<dyn StorageDevice>);

impl SharedDevice {
    pub fn new(inner: Arc<dyn StorageDevice>) -> Self {
        SharedDevice(inner)
    }
}

impl StorageDevice for SharedDevice {
    fn descriptor(&self) -> &DeviceDescriptor {
        self.0.descriptor()
    }

    fn write<'a>(
        &'a self,
        loc: &'a crate::types::PhysicalLocation,
        buf: &'a [u8],
    ) -> crate::hal::BoxFuture<'a, KernelResult<()>> {
        self.0.write(loc, buf)
    }

    fn read<'a>(
        &'a self,
        loc: &'a crate::types::PhysicalLocation,
        buf: &'a mut [u8],
    ) -> crate::hal::BoxFuture<'a, KernelResult<()>> {
        self.0.read(loc, buf)
    }

    fn flush<'a>(
        &'a self,
        loc: &'a crate::types::PhysicalLocation,
        level: DurabilityLevel,
    ) -> crate::hal::BoxFuture<'a, KernelResult<()>> {
        self.0.flush(loc, level)
    }

    fn discard<'a>(
        &'a self,
        loc: &'a crate::types::PhysicalLocation,
    ) -> crate::hal::BoxFuture<'a, KernelResult<()>> {
        self.0.discard(loc)
    }

    fn max_durability(&self) -> DurabilityLevel {
        self.0.max_durability()
    }

    fn health(&self) -> crate::types::DeviceHealth {
        self.0.health()
    }
}

// ---------------------------------------------------------------------------
// L1 单元层：WAL 帧校验与段扫描
// ---------------------------------------------------------------------------

pub mod l1;
