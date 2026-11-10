# 平台核对与测试夹具（`src/platform.rs`、`src/testing/*`）

两块内容放在一篇，因为它们解决同一个问题的两端：**怎么在启动时把「设备并不具备假设
中的能力」这件事变成错误**，以及**怎么在测试里把设备故意弄坏还能判定得准**。

设计依据：[05-kernel-test-harness.md](../chatgpt/05-kernel-test-harness.md)、
[07-hardware-resource-management.md](../chatgpt/07-hardware-resource-management.md)、
[ADR-0011](../adr/0011-device-op-site-tagging.md)。

## 1. 文件对照

| 文件 | 内容 |
|---|---|
| `src/platform.rs` | `Deployment`（部署声明）、`BackendCapabilities`、`IoBackend`、`FileClass`、`PlatformReport`、`check_device` / `select_backend` / `check_direct_io` |
| `src/testing/mod.rs` | `FaultKind`、`TriggerPoint`、`Injection`、`OpTicket`、`PlanInjector`、`FaultIo` |
| `src/testing/l1.rs` | `Layout`（固定设备布局）、`Harness`（可注入环境）、`wal_on` / `commit_object` / `digest` |
| `tests/fault_injection.rs` | L2 崩溃恢复组：掉电 / 部分写 / 静默丢弃 / I/O 错误 |
| `tests/platform.rs` | 启动核对组：降级设备、矛盾上报、SPDK 对齐、失败设备不得打开内核 |

整个 `src/testing` 只在 `feature = "test-harness"` 下编译，发布构建里不存在
（05 §6 约束 4）。`src/platform.rs` 无条件编译——它是生产路径上要跑的东西。

## 2. 启动期核对（`platform.rs`）

**内核本身不调用它。** 这是刻意的：`Kernel::open` 拿到的是已经构造好的
`IoExecutor`，平台核对发生在调用方**构造设备之后、打开内核之前**。`src/bin/ains-demo.rs`
就是这么用的：

```rust
let deployment = Deployment::default();
let report = deployment.check_device(&desc, device.health(), device.max_durability())?;
let backend = deployment.select_backend(&BackendCapabilities::new(false, false));
let kernel = Kernel::open(config, device)?;      // 核对通过了才走到这里
```

三条原则（文件头注释）：声明与事实分离、能力不足等于启动失败、不读时钟不探测硬件。

### 2.1 `check_device`

```rust
pub fn check_device(
    &self,
    desc: &DeviceDescriptor,
    health: DeviceHealth,
    max_durability: DurabilityLevel,
) -> KernelResult<PlatformReport>
```

判定顺序（顺序有意义，一致性核对必须在分级之前）：

| 情形 | 结果 |
|---|---|
| `health` 与 `max_durability` 矛盾（如 `Healthy` 却只能 `Write`） | `Internal(InvariantViolation)`——后端上报有 bug |
| `DeviceHealth::Failed` | `Io(Allocate)`——它不该进入任何分配器候选集 |
| `max_durability == None` | `Unsupported`——连 `Write` 都给不了的设备不承载数据 |
| 启用 SPDK 且 `alignment` 不是 512/4096 | 通过，`notes` 记一条 |
| `max_durability == Write` | 通过，`notes` 记「WAL durability is degraded」 |
| `!desc.supports_discard` | 通过，`notes` 记「回收的 extent 会泄漏空间」 |

`PlatformReport::is_clean()` 是「没有偏差」。`notes` 是给运维看的，不是错误——
「能跑，但你需要知道」和「不能跑」必须分开。

### 2.2 `select_backend`

07 §5.3 决策表逐行匹配，命中即停：

```text
SPDK   ⟸ dedicated && spdk && caps.spdk && caps.spdk_worth_it && caps.io_uring && !caps.io_uring_meets_slo
io_uring ⟸ caps.io_uring
POSIX  ⟸ 其余
```

两个容易写错的地方，代码里用默认值防住了：

- `BackendCapabilities::new(io_uring, spdk)` 把 `io_uring_meets_slo` 默认为 **true**。
  如果默认 false，「没压测过」就会被读成「压测过且不达标」，SPDK 在无证据时上位。
- `spdk_worth_it` 默认为 **false**。不测就不选（判据 3）。

共享部署（`dedicated == false`）永远进不了 SPDK 分支：用户态驱动独占设备，
共享部署里没有这个前提。

### 2.3 `check_direct_io`

```rust
pub fn check_direct_io(&self, offset: u64, length: u64, buf_len: usize, alignment: u64) -> KernelResult<()>
```

三者都要对齐（offset、length、缓冲区长度），`alignment == 0` 也算错。未对齐返回
`InvalidArgument`，**不静默降级成 buffered**——静默降级会让人以为走了直写路径。
`FileClass::wants_direct_io()` 给出每类文件的默认倾向：WAL / 索引段 / 大 payload 开，
`SmallOrMetadata` 交给 page cache（07 §5.4）。

## 3. 平台相关用例（`tests/platform.rs`）

| 用例 | 断言的核心事实 |
|---|---|
| `degrading_device_is_reported_with_device_and_capability` | 偏差进 `notes`，设备名与能力都在里面 |
| `spdk_alignment_expectation_only_applies_when_spdk_is_enabled` | 512/4096 之外的对齐只在 SPDK 下算偏差 |
| `a_failed_device_never_opens_the_kernel` | 核对失败 → `Kernel::open` 不执行 |
| `contradictory_device_reporting_is_an_invariant_violation` | `must_abort()` 为真 |
| `an_approved_device_opens_and_carries_a_commit` | 核对通过后能真的提交一次 |
| `default_mem_health_follows_durability` | `MemDevice` 的默认 `health` 由 `max_durability` 推出，不撒谎 |

<a id="故障注入"></a>

## 4. 故障注入（`testing/mod.rs`）

### 4.1 两个触发点

```rust
pub enum TriggerPoint { Before, After }
```

| 触发点 | 语义 | 用来验证 |
|---|---|---|
| `Before` | op 出队、尚未调用 `StorageDevice` | 这次操作**完全没发生** |
| `After` | 设备确认成功、尚未回填结果 | 设备**做了**，调用方没来得及知道 |

ADR-0011 决策 3 把设备级注入固定在这两个边界上：再往下就是设备内部，再往上就混进了
内核业务逻辑。

### 4.2 四种故障

```rust
pub enum FaultKind {
    Crash,               // 本次不生效；已持久的部分保留；之后该实例不再响应
    IoError,             // 返回 KernelError::Io，不得被改写成 Retryable
    PartialWrite(usize), // 只把前 N 字节交给设备
    SilentDrop,          // 返回成功但什么都没写
}
```

`SilentDrop` 是最难查的一类：内核侧看不出异常，只有恢复后的自校验能发现。

### 4.3 规则（`Injection`）

```rust
Injection::before(site, at, kind)                  // 任意类别
Injection::before_class(site, OpClass::Flush, at, kind)
Injection::after_class(site, OpClass::Write, at, kind)
```

- `at` 从 1 开始，按 `(site, class)` 计数。「从现在起第几次」用
  `injector.next_hit(site, class)` 算，不要硬编码——前面已经发生的 op 数量会变。
- 同一 op 上多条规则同时命中时**崩溃优先**，其余一并消费。
- 规则一旦命中就从计划里移除（确定性：同一个进程里不会重复触发）。

### 4.4 注入点由 `DeviceOp` 携带

site 是 `DeviceOp` 的字段，不是按地址推断出来的。MVP 早期按 `PhysicalLocation.offset`
落在 WAL 区间来猜 site 的做法已删（ADR-0011）：布局一改注入点就漂移，测试仍然绿。

两条断言把「漏打标签」变成测试失败而不是通过：

```rust
injector.add(Injection::before(DeviceSite::Untagged, ..))   // panic：规则不得注册在 Untagged
```

以及运行期：未打标签的 `Write` / `Flush` 到达注入器直接 panic（`Read` / `Discard`
豁免）。见 `tests/fault_injection.rs::injection_on_untagged_site_is_a_test_defect` 与
`untagged_write_reaching_injector_is_a_defect`。

### 4.5 `FaultIo`

`IoExecutor` 的注入版，同时扮演「假设备集群」：`before` 决定这次 op 做不做，
`after` 决定「设备做了但调用方没收到」。

```rust
let io = Arc::new(FaultIo::new(Arc::clone(&medium)));
let kernel = Kernel::open_with_io(cfg, io.clone())?;
let inj = io.injector();
inj.add(Injection::after_class(
    DeviceSite::WalFlushStart,
    OpClass::Flush,
    inj.next_hit(DeviceSite::WalFlushStart, OpClass::Flush),
    FaultKind::Crash,
));
```

诊断接口：`injector().hits(site)` / `hits_class(site, class)` / `next_hit(..)` /
`fired()` / `has_crashed()`，以及 `FaultIo::op_count()`。

**崩溃作用域**：`crashed` 一旦置位，之后每个 op 都返回 `Crash`（设备掉电后不再响应）。
「重启」= 换一个新的 `FaultIo` 包住同一份介质，注入器状态不延续（05 §3.4）：

```rust
let h1 = Harness::boot(cfg.clone(), Arc::clone(&medium))?;   // 崩在这个实例里
// ... 注入 Crash ...
let h2 = Harness::boot(cfg, Arc::clone(&medium))?;           // 同一份介质，全新注入器
```

## 5. L1 夹具（`testing/l1.rs`）

| 项 | 作用 |
|---|---|
| `Layout` | 固定布局：WAL `[0, 2MB)`（64KB × 32 段），payload 从 `2MB` 起，容量 16MB |
| `Layout::config(ckpt)` | 按上面的布局造 `KernelConfig` |
| `Harness::boot(cfg, medium)` | 用 `FaultIo` 包住共享介质打开内核 |
| `wal_on(dev)` | 单独构造一个 `DeviceWal`（WAL 单元测试用） |
| `commit_object(kernel, payload)` | 开事务 → 建文档对象 → 提交，返回 `ObjectId` |
| `digest(bytes)` | `crc32c`，用于断言「逐字节一致」 |

介质用 `Arc<dyn StorageDevice>` 共享，这是重启测试能成立的前提：`MemDevice`
里的字节就是「盘上的字节」。

## 6. 崩溃恢复组的判定标准

沿用 03 §11.2，`tests/fault_injection.rs` 每组用例都按它断言：

1. 已确认提交的事务**可见**，且 payload 逐字节一致（`digest` 比对）；
2. 未确认提交的事务**不可见**；
3. 恢复后 `RecoveryManager::verify(&k, VerifyScope::Full)` 零问题。

| 用例 | 注入 | 期望 |
|---|---|---|
| `crash_during_wal_flush_preserves_earlier_commits` | WAL 提交 flush 崩溃 | 之前的提交可见，本次提交返回错误 |
| `crash_during_payload_flush_prevents_commit` | payload flush 崩溃 | 事务不提交 |
| `partial_wal_write_terminates_scan_without_corruption` | `PartialWrite` | 扫描在坏帧处停下，不把垃圾当记录 |
| `crash_between_commit_record_and_flush_keeps_txn_invisible` | commit 记录已写、flush 未完成 | 恢复后不可见 |
| `crash_after_commit_flush_makes_txn_visible_on_replay` | commit flush 之后（`After`） | 恢复后可见 |
| `io_error_is_propagated_as_io_not_retryable` | `IoError` | 错误分类是 `Io`，不是 `Retryable` |
| `silent_drop_payload_write_is_detected_by_recovery_verification` | `SilentDrop` | 自校验发现 |
| `checkpoint_then_crash_recovers_consistent_state` | 检查点后崩溃 | 检查点 + 重放的结果一致 |

改设备层或 WAL 格式后，这一组是最先跑的：`cargo test --features test-harness --test fault_injection`。
用法见 [usage.md](usage.md#崩溃恢复测试)。

## 7. 已知差距

- `qd_respects_device_limit`（07 §7.1）的设备侧约束没有专门夹具。
- 注入器没有「延迟 / 超时」类故障；目前四种故障都是控制流的，不是时间维度的。
- 平台核对没有绑进 `Kernel::open`：调用方忘了调用就完全跳过了核对（demo 与
  `tests/platform.rs` 示范了正确用法，但内核不会替调用方兜底）。
- `BackendCapabilities` 是输入而不是探测结果——io_uring / SPDK 的实际探测与压测
  还没落地，见 [usage.md](usage.md#已知限制)。
