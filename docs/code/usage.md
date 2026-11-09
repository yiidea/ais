# 使用手册

面向**使用**这个库的人。实现细节见同目录的 [kernel.md](kernel.md)、[hal.md](hal.md)、
[index.md](index.md)、[engine-intent.md](engine-intent.md)、
[platform-testing.md](platform-testing.md)。

当前版本是 MVP：单节点、`MemDevice` 与 `FileDevice` 两种设备、同步 façade 默认。
API 还在演进，本文按**当前实现**写；接口一旦改，这里同步改。

## 1. 最小程序

```rust
use ains::engine::Engine;
use ains::hal::mem::MemDevice;
use ains::kernel::KernelConfig;
use ains::types::{DeviceId, ObjectType};

fn main() -> ains::KernelResult<()> {
    let device = Box::new(MemDevice::new(DeviceId(1), 64 << 20));
    let engine = Engine::open(KernelConfig::new("ckpt.snap"), device)?;

    let id = engine.put_document(
        ObjectType::Document,
        "WAL 检查点的原子发布",
        vec![0.9, 0.1, 0.0],
        Default::default(),
    )?;

    for hit in engine.search(&[1.0, 0.0, 0.0], 5) {
        println!("{:.3} {}", hit.score, hit.text);
    }
    println!("{id}");
    Ok(())
}
```

对应 `examples/quickstart.rs`（`cargo run --example quickstart`）。要点：

- `Engine::open(config, device)` 打开内核并**从对象存储重建派生索引**（向量索引、文档
  缓存）。派生数据不落盘，每次打开都重建（03 §6.1 step 9）。
- `KernelConfig::new(path)` 用默认布局：WAL `[0, 2MB)`（64KB × 32 段），payload 从
  2MB 开始。`checkpoint_path` 只是一个**路径**，`MemDevice` 下不会产生文件。
- `put_document` 是一个「开事务 → 建对象 → 提交」的完整写路径，返回后数据已确认提交。
- 忘记向量维度一致：`search` 按余弦/内积算，维度不一致不会报错，只会得到没意义的分。
  当前实现没有做维度校验，调用方自己保证。

换真实文件：把 `MemDevice::new(...)` 换成 `FileDevice::open(DeviceId(1), path, capacity)?`
（`src/hal/file.rs`）。注意 `FileDevice::open` **不截断**已有文件，容量参数只做上限检查。

更大的端到端演示（检索 → 元数据过滤 → 删除 → 检查点 → 崩溃恢复）：
`cargo run --bin ains-demo`。

## 2. 让查询带上完整性标记

默认的 `search()` 返回 `Vec<SearchHit>`，**不告诉你结果全不全**。要回答「这次结果是否
覆盖了所有已提交数据」，需要开启派生索引并读覆盖标记：

```rust
use ains::engine::{Engine, IndexCoverage};
use ains::types::IndexId;

let engine = Engine::open(config, device)?.with_index(IndexId(1))?;

let result = engine.search_with_coverage(&query, 10);
match result.coverage {
    Some(IndexCoverage::Complete) => { /* 结果是全部答案 */ }
    Some(IndexCoverage::Recovered { via }) => { /* 已先追赶补齐，答案是完整的 */ }
    Some(IndexCoverage::Partial { missing_estimate }) => { /* 缺口未补齐 */ }
    None => {}
}
// 或者只看是否完整：
if !result.is_complete() { /* 降级或拒绝回答 */ }
```

判定口径（06 §3.1 / §4.3）：

| 条件 | 结果 |
|---|---|
| 没开索引（`Engine::open` 后没调 `with_index`） | `coverage = None`，`is_complete() == true`——引擎不声称任何东西 |
| `index_freshness().txn_watermark + 1 ≥ 查询快照的 xmax` | `Complete` |
| 不完整，但 `catch_up_index()` 之后完整了 | `Recovered { via: FullScan }` |
| 追赶后仍不完整（或追赶失败） | `Partial { missing_estimate }` |

为什么是 `S.xmax` 而不是 `S.xmin`：快照可见的版本满足 `created_by_xid < S.xmax`，
所以水位追到 `S.xmax − 1` 就覆盖了一切可见版本。`S.xmin` 是**活跃事务里最小**的 xid，
拿它比会把之后才提交的事务也算成缺口，永远报「不完整」。

`missing_estimate` 来自按实测变更速率推算的代价模型，**只用于决策，不参与正确性**
（没有样本时给 1，不猜常数）。

其它接口：

- `engine.index_freshness() -> Option<IndexFreshness>`：当前水位与滞后情况，不追。
- `engine.catch_up_index() -> KernelResult<Option<IndexFreshness>>`：同步把索引追到
  当前 `object_watermark`。返回 `Err(Internal(InvariantViolation))` 表示索引
  **必须重建**（变更已经被回收，追不上），水位不会动（A9）。
- 写路径（`put_document` / `delete_document`）提交成功后会自动尝试一次追赶；失败
  不影响写入结果。

**成本提示**：`catch_up_index` 是同步的，追多少就挡多久。大批量导入后首次查询可能
明显变慢，导入结束先显式调一次 `catch_up_index()`。后台化还没实现，见 §6。

## 3. I/O 并发与背压

默认 `KernelConfig::reactor = None` → `SyncIoExecutor`：调用线程直接做设备操作，同一
时刻至多一个在飞。**队列深度与背压在同步 façade 上不会生效。**

要用并发的设备路径：

```rust
use ains::hal::QueueDepth;
use ains::kernel::{KernelConfig, ReactorSpec};

// (qd_device, qd_usable, qd_flush_reserve)
let qd = QueueDepth::initial(device_qd, share, 1)?;
let mut config = KernelConfig::new("ckpt.snap");
config.reactor = Some(ReactorSpec::new(qd));
let engine = Engine::open(config, device)?;
```

- `QueueDepth::initial(device, usable, flush_reserve)` 在**启动期**校验：
  `flush_reserve ≥ 1`、`usable ≥ 1`、`usable + flush_reserve ≤ device`。非法直接
  `InvalidArgument`，不静默修正。
- 有效上限 `qd_effective = min(device, usable + flush_reserve)`，普通 I/O 可用
  `qd_effective − flush_reserve`。flush 有独立预留，普通 I/O 打满也不会让 checkpoint
  的 flush 排队。
- `ReactorSpec::new` 默认窗口 100ms、SLO 100µs——都是部署参数，按实测调
  （`spec.slo_nanos`、`spec.window`）。
- 自适应：每窗口采样 `lat_p50` / `lat_p99` 与在飞峰值驱动 `QdTuner`：
  - 升：饱和（`inflight ≥ 0.9 × usable`）且 `lat_p99 ≤ 0.8 × SLO`，连续 3 个窗口 → ×1.5（取 2 的幂）；
  - 降：`lat_p99 > 1.5 × 基线` → ×0.7，一个窗口即生效；
  - 调整后冷却 ≥ 1 个窗口；`util < 0.6` 时不动。flush 预留不归 tuner 管。
- **背压**：槽位满时普通 I/O 返回 `KernelError::Retryable(RetryReason::Backpressure)`。
  它不是错误，调用方应稍后重试；不要把它当致命错误处理（见 `KernelError::must_abort()`）。

需要定制执行器（测试、特殊设备）时用 `Kernel::open_with_io(config, Arc<dyn IoExecutor>)`。
这不是冻结合同，但语义上与 `Kernel::open` 完全一致。

**启动核对**：内核不会替你核对平台能力。打开内核之前自己做一次：

```rust
use ains::platform::{BackendCapabilities, Deployment};

let deployment = Deployment::default();          // 或按真实部署构造
let report = deployment.check_device(device.descriptor(), device.health(), device.max_durability())?;
let backend = deployment.select_backend(&BackendCapabilities::new(io_uring, spdk));
if !report.is_clean() { eprintln!("平台偏差: {:?}", report.notes); }
// 核对通过后才 Kernel::open / Engine::open
```

`check_device` 返回 `Err` 就**不要打开内核**：`Failed` 设备、连 `Write` 都给不了的设备、
自相矛盾的上报都在这里拦下。详见 [platform-testing.md](platform-testing.md)。

<a id="崩溃恢复测试"></a>

## 4. 崩溃恢复测试


需要 `feature = "test-harness"`：

```bash
cargo test --features test-harness --test fault_injection
cargo test --features test-harness
```

两种「弄坏设备」的手段：

1. **`MemDevice` 自带**：`power_loss()`（丢掉未持久的部分）、`process_crash()`
   （模拟进程崩溃）、`set_max_durability(level)`、`set_health(..)`。用于「介质状态」
   层面的模拟。
2. **`FaultIo` 故障注入**：精确到「第几次某个设备操作」。用于「时序」层面的模拟。

注入一个崩溃并观察恢复：

```rust
use ains::hal::{DeviceSite, OpClass};
use ains::kernel::ObjectManager;
use ains::testing::l1::{self, Harness};
use ains::testing::{FaultKind, Injection};
use std::sync::Arc;

let medium = Arc::new(ains::hal::mem::MemDevice::new(DeviceId(1), 16 << 20));
let cfg = l1::Layout::config("ckpt.snap");

// 第一次启动：正常写入一件事务，然后让 WAL flush 崩溃。
let h1 = Harness::boot(cfg.clone(), Arc::clone(&medium))?;
let first = l1::commit_object(&h1.kernel, b"committed-before-crash")?;
let inj = h1.io.injector();
let at = inj.next_hit(DeviceSite::WalFlushStart, OpClass::Flush);
inj.add(Injection::before_class(DeviceSite::WalFlushStart, OpClass::Flush, at, FaultKind::Crash));
assert!(l1::commit_object(&h1.kernel, b"dies-with-the-crash").is_err());
assert!(inj.has_crashed());

// 第二次启动：同一份介质，全新的注入器状态（内核恢复正常）。
let h2 = Harness::boot(cfg, Arc::clone(&medium))?;
let snap = h2.kernel.fresh_snapshot();
let ver = ObjectManager::resolve_version(&h2.kernel, first, &snap)?;
let view = ObjectManager::read_version(&h2.kernel, first, ver)?;
let bytes = h2.kernel.read_extent_bytes(view.version.extent.unwrap())?;
assert_eq!(l1::digest(&bytes), l1::digest(b"committed-before-crash"));
```

每条用例都要按 03 §11.2 断言三件事：已提交可见且逐字节一致、未提交不可见、
恢复后自校验零问题（`RecoveryManager::verify`）。完整用例表见
[platform-testing.md#故障注入](platform-testing.md#故障注入)。

**自己定义注入点**不按地址猜：在构造 `DeviceOp` 时声明 `DeviceSite`。注册到
`DeviceSite::Untagged` 会在 `add()` 时 panic——这是测试缺陷，不是可选项（ADR-0011）。


| 想知道 | 怎么拿 |
|---|---|
| 索引是否新鲜 | `engine.index_freshness()` |
| 这次结果是否完整 | `search_with_coverage(..).coverage` |
| 派生的向量索引规模 | `engine.indexed_count()` |
| 设备自报的耐久等级 / 健康 | `device.max_durability()` / `device.health()` / `device.descriptor()` |
| 启动期偏差 | `Deployment::check_device(..)?.notes` |
| 内存设备上真实落盘的字节数 | `MemDevice::persistent_bytes()` / `stats()` |
| 注入器命中情况 | `FaultIo::injector().hits(..)` / `fired()` / `has_crashed()` |
| 恢复自校验 | `RecoveryManager::verify(&kernel, VerifyScope::Full)` |

内核错误分类：`KernelError::must_abort()` 为真的错误（内部不变量违反、校验失败等）
不应重试；`Retryable(Backpressure)` 应重试；`Io` 视场景。`IoError` 注入**不会**被
改写成 `Retryable`——这是测试里专门守住的一条。

## 6. 测试命令

```bash
cargo test                                   # 默认特性（112 项）
cargo test --all-features                    # 含 test-harness（122 项）
cargo clippy --all-targets --all-features -- -D warnings
cargo fmt --check
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --all-features
cargo build --offline --locked               # 零外部依赖
cargo +1.75 build --all-targets --locked     # MSRV（不能依赖 trait upcasting）
cargo run --bin ains-demo
cargo run --example quickstart
```

CI 由 `.github/workflows/ci.yml` 的 8 个任务覆盖以上全部，全局 `RUSTFLAGS: -D warnings`。
测试按层组织：`tests/` 下按主题分组（`kernel_*`、`wal`、`fault_injection`、`platform`、
`freshness`、`engine_index` ……），夹具在 `tests/common/` 与 `src/testing/l1.rs`。

## 7. 已知限制

按「会影响你做什么」排序。这些是当前实现的真实边界，不是待办口号。

1. **单机、单进程。** 没有集群、没有复制、没有跨节点事务。`Deployment::fabric_lanes`
   只是声明字段，不参与任何行为。
2. **派生索引每次打开都全量重建。** 向量索引与文档缓存不落盘；`Engine::open` 的
   成本与对象总数成正比。段索引（`IndexState`）也只在内存里，`with_index` 每次从空
   清单开始追赶。
3. **索引追赶是同步的，且没有后台化 / 持久化。** `catch_up_index` 会挡住调用方；
   进程重启后清单丢失，重新追。这是 README「下一步」第 1 项。
4. **未决事务会让水位停住。** 覆盖判定按 xid，长事务会让 `txn_watermark` 卡在它之前
   （A10：水位不越过未决事务）。同时要知道：`next_txn_watermark` 在「没有未决事务」
   时取已见最大 xid，这是**下界**——内核没有枚举未决事务的接口，所以这个值偏保守，
   不会乐观。
5. **索引一旦被截断就必须重建。** 变更被回收（`RebuildRequired`）时 `catch_up_index`
   返回错误而不是把水位推到「看起来追上了」。重建路径要调用方自己走。
6. **设备只有 `MemDevice` 与 `FileDevice`。** 没有 io_uring 实现、没有 SPDK、没有真实
   NVMe 队列；`BackendCapabilities` 是**输入**而不是探测结果。
7. **同步 façade 是默认。** 不挂 reactor 就没有并发、没有 QD 自适应、没有背压。
8. **槽位不是无限等待**：`Slots` 带 deadline（同步 10ms、reactor 200µs），等不到就返回
   `Retryable(Backpressure)`；它按注入的 `Clock` 判定。
9. **平台能力不是内建强制的。** 内核不会替你调 `Deployment::check_device`；
   忘了调就等于没有核对。
10. **快照隔离的边界**：`fresh_snapshot` 里 `xmax` 在 `begin()` 分配 xid 时即推进，
    `xmin` 是活跃事务最小 xid（无活跃时为 `xmax`）。`Engine` 的读路径都用
    `fresh_snapshot()`，没有提供「读到旧快照」的接口。
11. **检索精度**：向量检索是暴力余弦（或内积），没有 ANN；`Filter` 是元数据等值匹配，
    没有范围查询。Intent 管线只有 `Similarity` / `Filter` / `TopK` / `Read` 四个原语。
12. **单个 `Engine` 的写路径加锁粒度**：向量索引、文档缓存、段索引各一把 `Mutex`，
    高并发写入会串行。MVP 目标是正确性，不是写吞吐。

设计上的取舍与原因见 [06-index-recovery.md](../chatgpt/06-index-recovery.md)、
[07-hardware-resource-management.md](../chatgpt/07-hardware-resource-management.md)，
决策记录见 [docs/adr/](../adr/)。
