# 设备层实现（`src/hal`）

设计依据：[04 §6](../chatgpt/04-kernel-interfaces.md)、
[07-hardware-resource-management.md](../chatgpt/07-hardware-resource-management.md)、
[ADR-0011](../adr/0011-device-op-site-tagging.md)。本文覆盖从 `StorageDevice`
到 reactor 与队列深度的全部实现。

## 1. 文件对照

| 文件 | 内容 |
|---|---|
| `src/hal/mod.rs` | `StorageDevice` trait、`DeviceOp`（自带 `site`）、`OpClass`、`DeviceSite`、`IoExecutor` trait、`DeviceDescriptor` |
| `src/hal/mem.rs` | `MemDevice`：测试设备，可注入掉电 / 进程崩溃 / 耐久降级 / 健康状态 |
| `src/hal/file.rs` | `FileDevice`：POSIX 实现，打开已有介质文件不截断（= 同一块盘重启） |
| `src/hal/executor.rs` | `SyncIoExecutor`：调用线程上 `block_on`，每次提交前过槽位 |
| `src/hal/qd.rs` | `QueueDepth`（QD 预算推导）、`QdTuner`（自适应）、`Slots`（普通 / flush 两套额度）、`IoPath`（批量区间） |
| `src/hal/reactor.rs` | `ReactorIoExecutor`：常驻 reactor 线程 + 窗口采样 + 驱动 `QdTuner` |

## 2. `StorageDevice` 与 `DeviceOp`

```rust
trait StorageDevice: Send + Sync {
    fn descriptor(&self) -> &DeviceDescriptor;
    fn write<'a>(&'a self, loc: &'a PhysicalLocation, buf: &'a [u8]) -> BoxFuture<'a, KernelResult<()>>;
    fn read<'a>(&'a self, loc: &'a PhysicalLocation, buf: &'a mut [u8]) -> BoxFuture<'a, KernelResult<()>>;
    fn flush<'a>(&'a self, loc: &'a PhysicalLocation, level: DurabilityLevel) -> BoxFuture<'a, KernelResult<()>>;
    fn discard<'a>(&'a self, loc: &'a PhysicalLocation) -> BoxFuture<'a, KernelResult<()>>;
    fn max_durability(&self) -> DurabilityLevel;
    fn health(&self) -> DeviceHealth;
}
```

- 手工装箱的 future（等价 `#[async_trait]`，D0.1），因为 `Box<dyn StorageDevice>` 要可用。
- **实现者必须保证 future 是 `Send`**，并按 07 §7.1 的 `qd_respects_device_limit`
  自己约束同时在飞数——`IoExecutor` 只约束**提交**，不约束设备内部。
- 每个 `DeviceOp` 自带 `DeviceSite`（ADR-0011）：故障注入的标签跟着 op 走，
  不需要平行的标签数组。`Untagged` 出现在内核代码里就是缺陷。

## 3. `IoExecutor`：内核触达设备的唯一入口

```rust
trait IoExecutor: Send + Sync {
    fn submit_and_wait(&self, site: DeviceSite, op: DeviceOp) -> KernelResult<DeviceResult>;
    fn submit_batch_and_wait(&self, ops: &[DeviceOp]) -> KernelResult<Vec<DeviceResult>> { /* 默认逐个提交 */ }
    fn read_into(&self, site: DeviceSite, loc: &PhysicalLocation, buf: &mut [u8]) -> KernelResult<()>;
    fn descriptor(&self) -> &DeviceDescriptor;
    fn max_durability(&self) -> DurabilityLevel;
}
```

两种实现：

| 实现 | 在飞上限 | 何时用 |
|---|---|---|
| `SyncIoExecutor` | 1（调用线程阻塞到完成） | 默认；`KernelConfig::reactor = None` |
| `ReactorIoExecutor` | `qd_usable`（调用方线程占槽位，reactor 执行） | `KernelConfig::reactor = Some(ReactorSpec)` |

两者都在提交前过 `Slots`；额度耗尽**立即**返回
`KernelError::Retryable(RetryReason::Backpressure)`，不阻塞等待（同步实现有 deadline，
见 §4）。

## 4. 队列深度与背压（`qd.rs`）

三个量必须分清：

| 量 | 含义 | 来源 |
|---|---|---|
| `qd_device` | 设备硬顶（NVMe 的 `min(NVMOSQ, NVMOCQ)`） | `device_qd_limit(...)` |
| `qd_usable`（配置 `share`） | 域配置给普通 I/O 的份额 | 部署配置 |
| 当前 `usable` | **自适应后**生效的普通在途上限 | `QdTuner` 调整 |

推导（07 §4.2 / HRM7）：

```text
qd_budget    = share + qd_flush_reserve
qd_effective = min(qd_device, qd_budget)
qd_initial   = min(qd_effective, 32)            // 再被 ordinary_cap 截一次
ordinary_cap = qd_effective - qd_flush_reserve  // 自适应上限
```

```rust
let qd = QueueDepth::initial(dev_limit, 64, 2)?;   // flush_reserve=0 或 budget>device → InvalidArgument
let slots = Slots::new(&qd, deadline_nanos, clock);
let _g = slots.acquire_ordinary()?;                // 满了 → Retryable(Backpressure)
let _f = slots.acquire_flush()?;                   // flush 走独立额度，不会被普通 I/O 饿死
```

- **预留必须从设备额度里划出**，不是额外加：`budget ≤ device` 在 `QueueDepth::initial`
  （与 `new`）里检查，非法配置启动期失败。
- `QdTuner::observe(&QdSample)` 返回 `QdDecision`：
  - **降**：`lat_p99 > 1.5 × 基线` 且过了冷却 → ×0.7 取 2 的幂，一个窗口即生效；
  - **升**：饱和（`inflight ≥ 0.9 × usable`）、`lat_p99 ≤ 0.8 × SLO`、且 `lat_p99 > SLO`
    不成立（`util < 0.6` 时不动）→ ×1.5 取 2 的幂，连续 3 个窗口；
  - **不动**：其余情况；调整后冷却 ≥ 1 个窗口。
- `IoPath::{ForegroundRead, ForegroundWrite, BackgroundRebuild, Flush}` 给出批量初值与
  允许区间，`batch_within(qd_usable)` 做截断。

## 5. reactor（`reactor.rs`）

```rust
let mut config = KernelConfig::new(path);
config.reactor = Some(ReactorSpec::new(qd));       // 100µs SLO、100ms 窗口；要换窗口就改字段
let kernel = Kernel::open(config, device)?;
```

- 调用方线程先 `acquire_ordinary` / `acquire_flush`（满了立即背压），再把 `DeviceOp`
  入队、`recv()` 等结果。语义仍是「提交即等待」（I2），只是在飞数可以 > 1。
- reactor 线程每轮最多取 128 个 op 执行；读操作通过 `Backfill` 把数据拷回调用方缓冲区。
- 每过一个窗口（默认 100ms）统计 `lat_p50` / `lat_p99` / 在飞峰值，饱和视作 `util`
  喂给 `QdTuner`，并把结果写回 `Slots::set_usable`。
- 诊断：`reactor.qd_usable()`、`reactor.last_window()`、`reactor.max_inflight_seen()`、
  `reactor.adjustments()`。
- `Drop` 时停线程并 join；残留 op 收到错误而不是悬挂。

**限制（实现侧事实）**：单个 reactor 线程顺序执行设备操作，所以设备侧的并发只有在
设备自身异步时才会出现（`MemDevice` / `FileDevice` 是同步的）。reactor 的价值在真实
异步后端（io_uring）落地后才完全兑现，见 [usage.md](usage.md#io-并发与背压)。

## 6. 设备与耐久

| 设备 | 用途 | 注意 |
|---|---|---|
| `MemDevice::new(id, capacity)` | 测试 / 演示 | `power_loss()` 丢掉未 flush 的写，`process_crash()` 只丢缓存 |
| `FileDevice::open(id, path, capacity)` | 持久化 | 不截断已有文件；`check_direct_io` 要求对齐 |

## 7. 已知差距

- `qd_respects_device_limit` 的设备侧约束还没有专门夹具（现有 reactor 用例只验证
  调用方侧的槽位与背压）。
- io_uring / SPDK 后端未实现，`Src` 选择在 [platform-testing.md](platform-testing.md)。
- 槽位用可注入时钟判 deadline，一旦返回背压，调用方必须自行退避重试——
  `IoExecutor` 不自动重试。
- `Slots::new(qd, deadline_nanos, clock)`：`SyncIoExecutor` 默认 10ms deadline，
  reactor 默认 200µs（`REACTOR_BACKPRESSURE_DEADLINE_NANOS`）。
