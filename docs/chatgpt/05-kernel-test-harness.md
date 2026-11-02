# 05 — 内核测试夹具与故障注入体系

> 前置：[04-kernel-interfaces.md](04-kernel-interfaces.md)（接口冻结）、[03-consistency-kernel.md](03-consistency-kernel.md)（一致性协议与注入矩阵）。
> 本文解决 [04 §16](04-kernel-interfaces.md) 承诺的第一件事与第二件事：**让第一版内核在没有真实 NVMe 与 GPU 的情况下跑通「写入—崩溃—恢复」闭环，并把不变量变成常驻断言。**

## 一、为什么夹具必须先于实现

[04 §15](04-kernel-interfaces.md) 给出的是「按编译依赖排序的模块顺序」：设备 → IoExecutor → WAL → 映射 → extent → 对象 → 事务 → 检查点/恢复 → 回收/迁移。

这个顺序的问题在于：**前 6 个模块各自都能编译通过，但只有把它们串起来塞进一个「写点数据、崩掉、再读出来」的循环，才会暴露真正的问题。** 而这个循环在没有真实硬件的机器上跑不起来——需要真盘、需要断电、需要可复现的崩溃时机。

因此第一件要写的东西不是模块，是**夹具**：

| 夹具 | 替代什么 | 让什么变成可测 |
|---|---|---|
| **假设备**（FakeDevice） | 真实 NVMe / DRAM / GPU | 掉电语义、部分写、IO 错误、延迟、设备降级 |
| **假时钟**（FakeClock） | 真实时间 | GC 老化、快照过期、长事务超时、保留策略 |
| **确定性调度器**（DeterministicScheduler） | OS 线程调度 | 并发交错的复现；Wait-Die 的死锁预防 |
| **注入器**（FaultInjector） | 真实崩溃（断电 / kill -9） | 精确到「第 N 次 flush 之后」的崩溃时机 |

三条判断标准：

1. **夹具不是 mock。** Mock 验证「调用发生了」，夹具验证「崩溃后状态是对的」。假设备必须实现**掉电语义**，否则整个测试体系是空的。
2. **夹具不是一次性工具。** 它必须与内核同仓、同版本、走同一套 CI 与 `cargo public-api` 快照，否则契约变了夹具不知道，测试会假绿。
3. **夹具本身可能被测。** 夹具有 bug 会让内核 bug 被掩盖，所以夹具自身有一套「对真实设备的差分测试」（见 §7.3）。

## 二、假设备（FakeDevice）

### 2.1 掉电模型

假设备唯一不可省的能力是**区分「写到了」和「写到了并且活过了掉电」**。这对应 [03 §11.1](03-consistency-kernel.md) 的 I2 / I5 两个注入点——它们只有在假设备区分「易失缓存」与「持久介质」时才可复现。

```rust
pub struct FakeDevice {
    id: DeviceId,
    /// 持久介质：只有这里的内容能活过掉电
    media: BTreeMap<BlockAddr, Vec<u8>>,
    /// 设备易失缓存（控制器 DRAM）：已下发到设备，但掉电丢失
    cache: BTreeMap<BlockAddr, Vec<u8>>,
    /// 主机侧未提交缓冲：write 之后、flush 之前的内容
    pending: BTreeMap<BlockAddr, Vec<u8>>,
    /// 已被 discard 的块
    trimmed: BTreeSet<BlockAddr>,
    /// 设备故障状态
    health: DeviceHealth,
    max_durability: DurabilityLevel,
    /// 已分配空间账本
    allocated: BTreeSet<BlockAddr>,
}
```

语义：

| 操作 | 落到哪一层 | 掉电后果 |
|---|---|---|
| `write` | `pending` | **丢失** |
| `flush(None)` | 不下沉（仍在 `pending`） | **丢失** |
| `flush(Write)` | `pending` → `cache` | **丢失**（在设备缓存里，掉电即丢） |
| `flush(Durable)` | `pending` / `cache` → `media` | **保留** |
| `discard` | 清除全部三层 | 空间已归还 |
| `read` | 依次回落 `pending` → `cache` → `media` | — |

**三级耐久等级必须严格区分**：[04 §6.1 D2](04-kernel-interfaces.md) 定义了 `None` / `Write` / `Durable` 三档，`flush(Write)` 表示「已下发到设备缓存，掉电可能丢失」，与 `None` 完全不同。假设备如果把三者混为一谈，「耐久等级是显式参数」这条契约就没有被测到——最典型的后果是内核以为自己 flush 到了 `Write`，实际上内核以为自己还在 `None`，掉电后丢数据。

同理，`write` 本身**不带**任何隐式持久化保证（[04 §6.1 D2](04-kernel-interfaces.md)）：`write` 之后不 flush，数据只存在于 `pending`，掉电必丢。

### 2.2 寻址与分配

`PhysicalLocation`（[02-kernel §2.5](02-kernel.md)）的 `Nvme` 变体带 `device_id / namespace_id / lba / length`。假设备按 **4 KB 块**寻址：

```rust
pub const BLOCK_SIZE: u64 = 4096;
```

写入必须**按块对齐并做跨块拆分**——`length` 通常不是 4 KB 的整数倍，跨块写是常态。假设备要正确处理跨块的部分写（注入点 I4「Extent 写入一半」经常落在块中间）。

`Allocate` 不是 `StorageDevice` 的方法（见 [04 §6.1](04-kernel-interfaces.md)），而是由上层的 `PhysicalExtentManager` 分配。假设备只需要提供空间账本并在 `discard` 时归还；分配逻辑本身属于内核，应有独立的单元测试。

### 2.3 设备降级

[04 §6.1 D3](04-kernel-interfaces.md) 要求降级可见：`max_durability()` 下降、`health()` 报告 `Degraded`/`Failed`。假设备要支持注入降级：

| 注入 | 效果 |
|---|---|
| `DeviceDegrade { to: Write }` | `max_durability()` 返回 `Write`；此后 `flush(Durable)` 返回 `Io{op: Fsync}` |
| `DeviceFail` | 所有写/读返回 `Io{op}`；`health()` 返回 `Failed` |

这条链路能测出一个关键行为：**设备降级后，提交路径必须失败而不是静默降级持久性**。内核不能悄悄把 `Durable` 降成 `Write` 继续提交——那等于把「已持久」的承诺偷走。

## 三、注入器（FaultInjector）

### 3.1 两种注入点：语义 site 优先

[03 §11.3](03-consistency-kernel.md) 建议「按第 N 次调用后 kill 进程」。这个办法能用，但**脆弱**：重构改变调用次数，整个注入点就漂移到别的地方，测试可能悄悄失去覆盖。

本文采用两级注入点：

| 级别 | 表达 | 稳定性 |
|---|---|---|
| **语义 site** | `wal.frame.partial`、`mapping.switch.after_log` | 与重构无关；实现改了语义就编译不过 |
| **调用序号** | `nth: 7`（第 7 次命中该 site） | 兜底，用于「同一个 site 只想崩第几次」 |

**规则：语义 site 是主键，序号是修饰符。** 没有语义 site 的裸序号注入只能用于探索性测试，不能进回归用例。

### 3.2 语义 site 目录

[03 §11.1](03-consistency-kernel.md) 的 I1–I10 是**故障场景**，本文把它们落到**具体的设备/内核操作边界**上。site 由内核在关键位置显式打点（一个 `#[cfg(feature = "fault-injection")]` 的轻量标记），而不是靠拦截设备调用猜测。

| 03 注入点 | 语义 site | 打点位置 | 期望结果 |
|---|---|---|---|
| I1 | `wal.frame.partial` | WAL 帧头写完、body 未写 | 恢复时忽略尾部半帧 |
| I2 | `wal.frame.written` | 帧写完、未 flush | 该帧及其后续全部丢失（未持久） |
| I3 | `wal.flushed` | flush 完成、内存状态未更新 | 重放该帧，幂等 |
| I4 | `extent.write.partial` | extent payload 写一半 | extent 不可读；事务未提交即不可见 |
| I5 | `extent.write.done` | extent 写完、未 flush | 事务未提交即不可见；崩溃后不可读 |
| I6 | `ckpt.manifest.before` | Phase B 中途，MANIFEST 未写 | 旧 checkpoint 仍有效 |
| I7 | `ckpt.superblock.before` | MANIFEST 写完、superblock 未更新 | 新 checkpoint 未生效，从 WAL 重放 |
| I8 | `mapping.switch.after_log` | `MappingSwitch` 已落盘、内存映射未更新 | 重放后切换生效 |
| I9 | `gc.unlink.before_free` | GC 摘链后、释放前 | 空间泄漏（可回收），不得误删 |
| I10 | `migration.target.partial` | 迁移 M3 中途，目标 extent 部分写入 | 回滚到源 extent（[03 §6.1 step 7](03-consistency-kernel.md)） |

补充几个内核级 site（不出现在 I1–I10，但必测）：

| site | 打点位置 | 期望结果 |
|---|---|---|
| `txn.commit.before_flush` | TxnCommit 写完、未 flush | 事务不可见（未返回 Commit OK） |
| `txn.commit.after_flush` | flush 完成、未发布内存版本 | 重放后可见 |
| `ckpt.publish.before_rename` | shadow 目录写完、未 rename | 旧 checkpoint 仍有效 |
| `mem.publish.before_visible` | 版本状态 Pending→Published 之前 | 版本不可见 |
| `extent.alloc.duplicate` | 分配出重复 LBA | 必须被断言检查捕获（见 §5.3） |

### 3.3 故障类型

```rust
pub enum FaultKind {
    /// 模拟进程 kill -9：不 unwind、不 flush、不 drop
    Crash,
    /// 返回 KernelError::Io { op }
    IoError { op: IoOp },
    /// 部分写：只写入前 N 字节
    ShortWrite { bytes: u64 },
    /// 假时钟推进 N 个 tick
    Delay { ticks: u64 },
    /// 数据位翻转
    Corrupt { scope: CorruptScope },
    /// 设备降级 / 故障
    DeviceDegrade { to: DurabilityLevel },
    DeviceFail,
    /// 静默丢弃这次写（返回成功但不落盘）——最恶劣的一种
    SilentDrop,
}
```

`SilentDrop` 单独列出，因为它是最容易被「重试掩盖」的故障：设备返回成功，数据没写进去。它能测出「内核是否校验了写入结果」——若内核只信任返回值，损坏会一直静默传播到恢复阶段。

### 3.4 注入器结构

```rust
pub struct FaultRule {
    pub site: FaultSite,
    /// 命中第几次（第 0 次 = 第一次命中）
    pub nth: u32,
    pub kind: FaultKind,
    /// 命中多少次后失效；0 = 永久
    pub times: u32,
}

pub struct FaultInjector {
    rules: Vec<FaultRule>,
    counters: HashMap<FaultSite, u32>,
    seed: u64,
    /// 已触发的注入记录，供测试断言
    fired: Vec<FiredFault>,
}
```

**`Crash` 的实现方式必须是真的「不回溯」**：不能用 `panic!`（会 unwrap、跑 Drop、可能把锁释放掉），也不能用 `std::process::abort`（会留 core dump 但测试进程就死了）。做法是让 `Crash` 变成一个**受控的进程退出**：

- 在测试里，`Crash` 触发一次 `std::process::exit(EXIT_CRASH)`；
- 测试驱动器用 `Command` 拉起被测进程，进程退出码非零即视为崩溃；
- 重启用同一份目录，验证恢复。

这与真实的 `kill -9` 在语义上等价：没有 unwind、没有 Drop、没有 flush。

**确认提交日志的写入要求**（这是最容易出错的一环）：日志由测试驱动器在**内核返回 Commit OK 之后**追加，追加后必须显式 `fsync`——`process::exit` 不跑析构，Rust 的行缓冲 stdout 在管道下会被丢进未刷出的缓冲。日志格式是「追加式 + 每条记录带 CRC」，因此崩溃时可能留下一条半截的尾记录，恢复时直接丢弃尾记录即可。

这条日志写在**被测内核之外**（由驱动器持有文件句柄，或走一条独立的 IPC 通道），绝不能让内核自己写——内核写它就等于给内核开了后门，测试会永远通过。

日志丢失一条已提交的记录是可以接受的：那笔事务在判定上算「未确认」，按 §5.2 的宽松规则处理（要么不可见，要么可见且内容正确）。反过来（日志里有、恢复后读不到）才是硬失败。

## 四、假时钟与确定性调度

### 4.1 假时钟

内核中所有与时间相关的逻辑必须从**注入的时钟源**读时间，不得直接调用 `Instant::now()` / `SystemTime::now()`。这是一条需要写进 lint 的约束（§8）。

```rust
pub trait Clock: Send + Sync {
    fn now(&self) -> Instant;
    fn monotonic_nanos(&self) -> u64;
}

pub struct FakeClock {
    current: Mutex<Instant>,
    /// 每次调用 now() 自动推进的步长；0 = 不自动推进
    step: u64,
}
```

假时钟要能测的场景：

| 场景 | 依赖的时间 | 怎么测 |
|---|---|---|
| GC 老化与保留策略 | 对象年龄 | 推进时钟过 TTL，触发一次 GC，断言该回收的被回收 |
| 快照过期 | `Snapshot` 的存活期 | 推进时钟后旧快照失效，断言读路径回退 |
| 长事务超时 | 事务时长 | 推进时钟触发 `Retryable(ResourceBusy)` |
| Checkpoint 周期 | 触发间隔 | 推进时钟触发检查点，断言 WAL 截断边界 |
| 迁移退避 | 退避窗口 | 推进时钟断言重排时机 |

**假时钟不得自动推进**，除非显式设定 `step`。自动推进会让「推进了多久」变得不确定，测试就不确定了。

### 4.2 确定性调度器

并发交错是内核最容易出错的地方（Wait-Die、锁顺序 L1–L5、迁移与 GC 竞争）。靠 `sleep` + 反复重跑来「碰运气」是行不通的：偶发失败复现不出来，等于没有测试。

做法：把并发部分改成**协作式调度**——每个可调度单元（线程 / 任务）在每一步只被允许推进一小段，推进权由测试驱动器决定。

```rust
pub struct Scheduler {
    /// 每个可调度单元的「剩余步数」，归零即让出
    budgets: Mutex<HashMap<SchedId, u32>>,
    /// 当前持步者
    holder: Mutex<Option<SchedId>>,
    /// 全局步计数，注入点可以用它做「第 N 步后崩溃」
    steps: AtomicU64,
}
```

调度契约：

1. 每次「让出」必须发生在**可观测的边界**上（进入内核 API、获取锁之后、I/O 提交之后），而不是随机位置——否则调度点会随实现重构漂移。
2. 调度点由内核显式标记（与注入 site 同一套标记机制），不在内核代码里撒 `thread::yield_now()`。
3. 调度器的目的是**复现特定交错**，不是穷举所有交错。穷举交错用性质测试（§5.4）。

第一版的范围控制：确定性调度器只用于**锁与可见性相关的模块**（Object / Transaction / Mapping / Reclaimer），WAL 与 Checkpoint 的路径本身是单线程的，不需要。

## 五、四层测试与判定

### 5.1 L1 单元层

每个模块的纯逻辑，用假设备但不注入故障。

| 模块 | 必须覆盖 |
|---|---|
| `ObjectManager` | 创建、读头、版本解析、快照下可见性、删除标记 |
| `ExtentManager` | 切分、写入、payload 读回、跨块边界 |
| `MappingManager` | 解析、切换、`MappingVersion` 单调、冲突返回 `Conflict(MappingChanged)` |
| `WalManager` | 记录编解码、CRC32C、LSN 分配、段切换、`flush_upto` 语义 |
| `TransactionManager` | 状态机、可见性判定、提交六步顺序、冲突中止 |
| `CheckpointManager` | 三阶段、shadow 目录、MANIFEST、原子发布 |
| `RecoveryManager` | 重放、幂等、尾部截断、迁移回滚 |
| `Reclaimer` | 六条件合取的逐条判定 |
| `MigrationManager` | M1–M10、`MappingSwitch` 提交点、并发写冲突 |
| `IoExecutor` | 批量提交、错误原样上抛 |

**每个错误路径必须有一条测试**（[04 §15](04-kernel-interfaces.md) 的要求）。错误断言要检查变体本身，不能只检查 `is_err()`——`KernelError` 是 `#[non_exhaustive]` 的，粒度太粗的断言在变体增加时会失去意义。

### 5.2 L2 崩溃恢复层

这是整套体系的核心。每个用例是一个**三元组**：

```
(seed, 注入序列, 期望差异集)
```

执行流程：

```
1. 在临时目录启动被测进程，跑一个固定 workload（写入若干对象、提交若干事务）
2. workload 中每个成功的 commit，把「对象 id + 版本 + 内容摘要」写进
   外部确认日志（被测进程之外，测试驱动器持有）
3. 注入器在指定 site 触发 Crash
4. 进程以非零码退出
5. 用同一目录重启进程，走恢复流程
6. 一致性检查器三方比对：重建态 / 内核态 / 确认日志
```

判定标准沿用 [03 §11.2](03-consistency-kernel.md)，这里写成可执行断言：

| 检查 | 断言 | 对应不变量 |
|---|---|---|
| 所有返回过成功的写入可读 | 确认日志里每个 `(id, ver)` 都能读到且内容摘要一致 | INV2 / 架构 I1 |
| 未返回成功的写入不对外可见 | 工作负载中未确认的对象不可读 | 架构 I2 |
| 目录之间引用完整 | 无 dangling 引用 | 架构 I3 |
| 无重复 extent 分配 | 同一 LBA 不被两个活跃 extent 占用 | 架构 I4 |
| 恢复后差异只是尾部截断 | 差异集合 ⊆ {注入点之后未提交的事务} | — |
| 恢复时长 < 10s（1TB 规模） | 计时断言（假设备上放宽为逻辑时间） | — |

**「未确认的对象不可读」这条要小心**：一个事务可能「数据已写完、flush 已完成、但进程在发布内存版本前崩溃」。恢复后重放 WAL 会让它**可见且内容正确**——这是允许的，因为 `TxnCommit` 已经落盘并 flush，它在事实上已经提交了，只是客户端没收到确认。

所以判定不是「未确认即不可见」，而是：

> 未确认的事务，恢复后要么不可见，**要么可见且内容与参考模型完全一致**。绝不允许「可见但内容是部分的 / 撕裂的 / 与参考模型不一致」。

这条判定同时覆盖了两个方向的错误：既不允许丢已提交数据，也不允许半成品泄漏。

### 5.3 一致性检查器

检查器是一个**独立于内核实现**的只读遍历器。它必须**重新扫描底层存储**（WAL 段、checkpoint 目录、映射表）重建出对象视图，而不是调用内核的 API 去问「你觉得你有多少对象」——用内核自己的 API 重建状态，验证出的只是「内核自洽」。

检查器做三方比对，三方各有独立来源：

| 视角 | 来源 | 回答的问题 |
|---|---|---|
| **重建态**（rebuilt） | 直接扫 WAL 段 / checkpoint 目录 / 映射表 | 磁盘上实际有什么 |
| **内核态**（live） | 走内核公开读路径 | 恢复后内核对外展示什么 |
| **确认日志**（acked） | 测试驱动器持有，记录每次返回 Commit OK 的事务 | 客户端被承诺了什么 |

```rust
pub trait ConsistencyChecker {
    /// 从底层存储重新构造出对象视图（不经过内核）
    fn rebuild(&self, dir: &Path) -> Result<RebuiltState>;

    /// 三方比对
    fn diff(
        &self,
        rebuilt: &RebuiltState,
        live: &LiveState,
        acked: &AckLog,
    ) -> DiffSet;
}

pub struct DiffSet {
    /// 确认过但读不到的：违反 INV2 / 架构 I1
    pub missing: Vec<(ObjectId, VersionId)>,
    /// 读得到但内容与参考模型不一致：违反「差异只是尾部截断」
    pub divergent: Vec<(ObjectId, VersionId)>,
    /// 不该可见却可见：违反 I2
    pub phantom: Vec<(ObjectId, VersionId)>,
    /// 悬空引用：违反 I3
    pub dangling: Vec<ExtentId>,
    /// 重复分配：违反 I4
    pub duplicated: Vec<PhysicalExtentId>,
}
```

`DiffSet` 为空 = 通过。任何一个字段非空 = 失败，并把内容写进测试报告。

「内核态」这一路走的是内核的公开读路径，这是**故意**的：它检验的是「内核对外展示了什么」，与重建态比对才能发现「磁盘上有但内核不认」或「内核认但磁盘上没有」。而重建态这一路完全独立于内核，才能发现「内核自己也不知道自己丢了什么」。

### 5.4 L3 不变量常驻层

[04 §14.2](04-kernel-interfaces.md) 把「不变量断言」列为 CI 检查项之一，但三套不变量编号来自三份文档，本节先把它们分开：

| 编号 | 出处 | 含义 |
|---|---|---|
| `INV1`–`INV5` | [03 §10](03-consistency-kernel.md) | 五条硬性不变量（可测试形式） |
| `架构 I1`–`I6` | [architecture/02 §0.1](../architecture/02-storage-engine.md) | 存储引擎不变量 |
| `注入 I1`–`I10` | [03 §11.1](03-consistency-kernel.md) | 故障注入点（§3.2 已映射） |

三者的性质不同：

- **`INV1`–`INV5` 是结构性不变量**：逻辑身份不依赖物理地址、已提交版本可恢复、映射切换不改变进行中的读取、旧 extent 引用清零才回收、非持久化存储不是唯一权威副本。它们可以在每次 CI 中跑，但更重要的一点是——它们应该是**常驻的**，即在长时间运行的模糊测试（fuzz / soak）中持续被断言，而不是只在 CI 跑一次就结束。
- **`架构 I1` / `I5` / `I6` 依赖具体设备行为**（architecture/02 给出的检查手段分别是「崩溃恢复后全量校验」「故障注入」「断电测试」），只能在假设备上断言；`架构 I2` / `I3` / `I4` 是结构性检查，可以在属性测试里断言。

| 不变量 | 可执行化方式 | 载体 |
|---|---|---|
| `INV1` 逻辑身份不依赖物理地址 | 静态检查：读取路径中不出现按物理地址的比较分支 | lint + 人工评审 |
| `INV2` 已提交版本可恢复 | 崩溃注入后逐个确认已提交对象可读 | L2（每个用例都断言） |
| `INV3` 映射切换不改变进行中的读取 | 迁移与读并发，读到的内容前后一致 | 确定性调度器 + 属性测试 |
| `INV4` 旧 extent 引用清零才回收 | 回收日志逐条检查引用计数 | L1 + 检查器 |
| `INV5` 非持久化存储不是唯一权威副本 | 每个可读对象的权威副本位置 ∈ {NVMe, HDD} | 检查器 |
| `架构 I1` 已提交对象至少映射到一个可读物理 extent | 崩溃恢复后全量校验：确认日志中的每个对象都能解析出可读 extent | L2 + 检查器 |
| `架构 I2` 提交顺序 = 日志顺序；版本顺序由 xid 决定 | 检查提交顺序与 LSN 顺序一致；版本链按 xid 单调 | 检查器 |
| `架构 I3` 逻辑映射与物理映射分离 | 迁移前后 `ObjectId` / `VersionId` 不变 | L1 迁移测试 |
| `架构 I4` 索引可见性由 LSN 水位决定 | 水位未到的索引不可见 | L1 |
| `架构 I5` 持久化写入先落 WAL 再改内存索引 | 注入点断言：内存状态变化必晚于对应 WAL flush | L2 |
| `架构 I6` DRAM / GPU HBM 不是最终持久存储 | 掉电后 DRAM/GPU 上的数据必须从 NVMe 重建得到 | L2 |

**「常驻」的含义**：这些断言编进内核的可选特性里（`feature = "invariants"`），在模糊测试与 soak 测试中长期开启，每次操作后检查一次不变量（用 epoch-based 的增量检查，而不是每次全量扫描）。

### 5.5 L4 性质测试（可选，第二版）

用 `proptest` 之类的性质测试框架生成随机的操作序列（create / update / delete / read / snapshot / migrate / gc），断言的是**性质**而不是具体结果：

- 读到的版本一定符合快照语义；
- 已提交对象最终可读；
- 引用计数最终归零；
- 任意交错下不出现死锁（调度超时即失败）。

性质测试能覆盖手写用例想不到的交错，代价是运行时间长、需要可复现的 seed。第一版可以先不做，但夹具（假设备 + 确定性调度器）要为此留好接口。

### 5.6 与内核实现阶段的对应

[02-kernel §11.2](02-kernel.md) 给出了六个阶段的验收条件，这里逐条映射到测试层——每一条验收条件都必须对应到至少一个可运行的用例，而不是一句口号。

| 阶段 | 验收条件 | 载体 | 对应用例 |
|---|---|---|---|
| Object + Extent | 对象切分、重组、随机读取正确 | L1 | 切分边界用例、跨块重组用例、随机读回比对 |
| NVMe Backend | 并发读写、IO 错误处理 | L1 + L2 | `IoError{op}` 注入、并发读写用例 |
| WAL + Recovery | 任意 WAL 写入边界崩溃恢复 | L2 | `注入 I1`–`I3` 全部 site |
| MVCC | 并发事务读取正确版本 | L1 + 确定性调度器 | 快照可见性用例、并发事务交错 |
| Memory Tier | 缓存淘汰、迁移、并发读取 | L1 + L2 | `迁移 target.partial`、缓存淘汰用例 |
| GPU HBM | GPU 数据正确性、设备故障和资源释放 | L1 + L2 | 设备降级注入、GPU 数据重建用例 |

[02-kernel §11.2](02-kernel.md) 另外建议的六个崩溃注入点，与本文的 site 目录一一对应：

| 02-kernel 的注入点 | 对应 site |
|---|---|
| WAL Record 写入过程中 | `wal.frame.partial` |
| WAL Flush 过程中 | `wal.flushed` |
| Extent 写入过程中 | `extent.write.partial` |
| Checkpoint Manifest 发布过程中 | `ckpt.manifest.before`、`ckpt.publish.before_rename` |
| Mapping Switch 过程中 | `mapping.switch.after_log` |
| Transaction Commit 过程中 | `txn.commit.before_flush`、`txn.commit.after_flush` |

前五项由 [03 §11.1](03-consistency-kernel.md) 的 `注入 I1`–`I10` 直接覆盖；第六项（Transaction Commit）由本文 §3.2 补充的 `txn.commit.*` 两个 site 覆盖。因此 [02-kernel §11.2](02-kernel.md) 的注入场景与 [03 §11.1](03-consistency-kernel.md) 的注入矩阵不是两套东西，而是**场景 → 矩阵**的关系。

## 六、夹具必须遵守的约束

夹具是测试代码，但它处在内核的信任边界上——**一个撒谎的夹具比没有夹具更危险**。因此：

1. **假设备必须实现 `StorageDevice` 的完整语义**，不得为了「让测试好过」而简化。特别是掉电语义（§2.1）与耐久等级（`flush(None)` ≠ `flush(Write)` ≠ `flush(Durable)`）。
2. **夹具不得绕过内核的锁与校验。** 假设备只能通过设备接口影响内核，不得直接改内核的内存状态。检查器是只读的，且是独立重建而非复用内核状态（§5.3）。
3. **夹具不得自动重试。** 内核「不自动重试」这条契约（[04 §2.3 C1](04-kernel-interfaces.md)）只有在夹具会稳定地产生 `Io` / `Retryable` 错误时才测得到。夹具一旦「贴心」地重试，这条契约就永远测不到。
4. **夹具必须在 crate 内部，不进公共 API 快照。** `FakeDevice` 的接口天然涉及 `PhysicalLocation`，与 [04 §14.1 规则 1](04-kernel-interfaces.md) 冲突——解决办法是把它放在 `#[doc(hidden)] pub mod testing` 下，并从 `cargo public-api` 快照中排除（用 `#[cfg(feature = "test-harness")]` 门控，发布构建里整个模块不存在）。
5. **夹具不得泄露 panic 到内核的错误路径。** 假设备内部出错必须返回 `KernelError::Io`，而不是 panic——否则测试会把「内核 panic」误判成「夹具 panic」。
6. **夹具必须可确定性复现。** 同一个 `seed` + 同一注入序列 → 同一结果。夹具里禁止使用真实随机源、真实时钟、哈希迭代顺序依赖。
7. **夹具不得使用真实磁盘路径作为状态存储。** 假设备的「持久介质」是内存里的 `BTreeMap`；夹具只把「真实要落盘的东西」（WAL 段、checkpoint 目录）写到临时目录。

## 七、CI 接线

### 7.1 分层

| 流水线 | 内容 | 时长目标 | 触发 |
|---|---|---|---|
| PR | L1 + L2 的固定用例集（每 site 至少 1 个）+ L3 的快速版 | < 3 min | 每次 push |
| 每夜 | L2 全量 seed 空间（seed 0–1023）+ L3 完整版 | < 60 min | nightly |
| 每周 | L2 扩展 seed + 差分测试（§7.3） | 数小时 | weekly |
| 发布前 | 全量 + 长时 soak（不变量常驻开启） | 数小时 | tag |

### 7.2 失败复现

失败报告里必须包含：

```
seed: 12345
workload: <workload 名>
fault: <site> nth=<n> kind=<kind>
diff: <DiffSet 的非空字段>
```

CI 提供 `cargo test -- --seed 12345 --workload wal_crash --fault wal.flushed` 的单点复现命令。**没有可复现 seed 的并发 / 崩溃测试等于没有测试**。

### 7.3 夹具自身的对拍

假设备必须与真实设备对拍，否则「夹具验证了错误的世界」：

| 对拍项 | 方法 | 频率 |
|---|---|---|
| 耐久语义 | 在真机上执行 `write → kill -9 → 重启`，验证落盘内容与假设备模拟一致 | 每夜 |
| 块对齐与跨块写 | 真机写入非 4 KB 对齐长度的数据，验证读回与假设备一致 | 每夜 |
| `flush` 等级差异 | 真机上验证 `flush(Write)` 掉电丢失、`flush(Durable)` 掉电保留 | 每周 |
| TRIM / discard | 真机上验证 discard 后空间确实归还 | 每周 |

对拍失败 = 假设备的语义模型错了，所有基于它的结论都要作废。因此对拍是**先决条件**：在假设备与真机对拍通过之前，L2 的结果只能当作「对假设备的结论」，不能当作「对内核的结论」。

## 八、编译期与 CI 检查清单

| 检查 | 工具 | 失败条件 |
|---|---|---|
| 公共 API 不含夹具类型 | `cargo public-api` diff | 快照出现 `FakeDevice` / `FakeClock` |
| 内核代码不直接读真实时钟 | 自定义 lint | 内核 crate 里出现 `Instant::now` / `SystemTime::now` |
| 注入 site 编译期校验 | 枚举 + `match` 穷尽 | 新增 site 未登记到 site 目录 |
| 崩溃注入用例覆盖 | 用例清单 diff | 每个 site 至少 1 个用例 |
| 不变量断言 | 单元测试 | 任一 `INV1`–`INV5` / `I1`–`I6` 失败 |
| 公共 API 表面变更 | `cargo public-api` diff | 快照未更新（沿用 [04 §14.2](04-kernel-interfaces.md)） |

## 九、实现顺序与工作量

按 [04 §15](04-kernel-interfaces.md) 的模块顺序，夹具的依赖关系是：

| 顺序 | 交付物 | 依赖 | 预估 |
|---|---|---|---|
| 1 | `FakeDevice`（掉电语义 + 块对齐 + `Io` 错误） | 无 | 3–5 天 |
| 2 | `FaultInjector` + 语义 site 打点机制 | 1 | 2–3 天 |
| 3 | 崩溃测试驱动器（进程重启 + 外部确认日志） | 1、2 | 3–4 天 |
| 4 | 一致性检查器（独立重建 + `DiffSet`） | 3 | 4–6 天 |
| 5 | `FakeClock` + 内部 lint | 1 | 1–2 天 |
| 6 | 确定性调度器 | 2 | 5–8 天（工作量最大的一个） |
| 7 | L3 不变量常驻检查 | 4 | 3–4 天 |

**关键路径是 1 → 2 → 3 → 4**。前四项完成后，L2 崩溃恢复测试就能跑起来，这已经是 [02-kernel §11.2](02-kernel.md) 第三阶段（WAL + Recovery）与第四阶段（MVCC）的验收手段。调度器（6）可以推迟——先用「单线程 workload + 少量并发用例」把 L2 跑起来，再投入确定性调度器。

## 十、与既有文档的关系

| 文档 | 本文对其做了什么 |
|---|---|
| [03 §10](03-consistency-kernel.md) `INV1`–`INV5` | 从「可测试形式」变成「常驻断言」，并说明各自的可执行化方式 |
| [03 §11.1](03-consistency-kernel.md) `注入 I1`–`I10` | 从「注入点清单」变成「语义 site 目录」，每条给出打点位置与期望结果 |
| [architecture/02 §0.1](../architecture/02-storage-engine.md) `架构 I1`–`I6` | 逐条给出可执行化方式；`架构 I2` 的表述以 [04 §16](04-kernel-interfaces.md) 的修订为准（提交顺序 = 日志顺序；版本顺序由 xid 决定） |
| [02-kernel §11.2](02-kernel.md) | 六个阶段的验收条件全部映射到本文的测试层 |
| [04 §16](04-kernel-interfaces.md) | 兑现第一件事（夹具）与第二件事（不变量可执行化）；第三件事（错误码映射）见 [architecture/07 §6.3](../architecture/07-operations.md) |
| [architecture/07 §7](../architecture/07-operations.md) | 故障注入章节的落地形态 |
| [ADR-0009](../adr/0009-fault-injection-harness.md) | 本文的决策记录：夹具形态、语义 site 优先、打点集中、不变量常驻 |

## 十一、下一步

本文把「怎么测」定了下来。相关决策已记录在 [ADR-0009](../adr/0009-fault-injection-harness.md)（夹具形态、语义 site 优先、打点集中在 `IoExecutor`、不变量常驻、夹具受公共 API 约束）。

剩下的设计工作有三项：

1. **把 site 打点机制回填到 [04](04-kernel-interfaces.md) 的实现约束里。** ADR-0009 决策 3 要求打点集中在 `IoExecutor` 一层，但 [04 §6.3](04-kernel-interfaces.md) 的 `IoExecutor` trait 签名目前没有表达「打点」的位置。实现前需要决定：是给 `IoExecutor` 加一个 `#[cfg(feature = "fault-injection")]` 的旁路钩子，还是让内核在调用 `submit_and_wait` 前显式打点。两种做法对 trait 形状的影响不同，需要一条补充 ADR。
2. **索引恢复的增量算法**（[03 §12 第 2 项](03-consistency-kernel.md)）——「日志水位 → 索引构建水位」的追赶策略，以及「滞后索引的查询回退路径」的代价估计。这一项与本文件的 §5.4 `架构 I4` 直接相关：回退路径的代价估计必须落到查询规划器能用的形式上。
3. **硬件资源管理的实现细节**（[03 §12 第 3 项](03-consistency-kernel.md)）——参数化 Placement 代价模型、定义 NUMA 亲和调度与 io_uring 队列深度的切换判据。这一项会反过来影响假设备要模拟多少设备差异（§2.3 的降级模型是否够用）。
