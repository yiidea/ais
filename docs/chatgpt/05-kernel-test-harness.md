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
    /// 持久介质：字节在磁盘上，掉电（= 进程被杀）后存活
    media: FileBackedMedia,
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

/// 文件支撑的持久介质：一个设备一个文件
pub struct FileBackedMedia {
    /// 测试临时目录下的介质文件
    file: File,
    /// 块目录：块地址 -> (文件内偏移, 长度)
    index: BTreeMap<BlockAddr, (u64, u32)>,
}
```

**「持久介质」必须是文件支撑的，不能是内存。** 这是整个 L2 崩溃恢复层成立的前提：崩溃后要**重启进程、用同一份目录走恢复流程**；如果介质在内存里，进程一死数据全没了，恢复流程读到的是空目录，所有 L2 用例都变成空跑。

**掉电的定义**：丢掉 `cache` 与 `pending`，`media` 保留（因为它的字节在磁盘上）。重启时重新扫描介质文件的块目录，重建 `index`。

语义：

| 操作 | 落到哪一层 | 掉电后果 |
|---|---|---|
| `write` | `pending` | **丢失** |
| `flush(None)` | 不下沉（仍在 `pending`） | **丢失** |
| `flush(Write)` | `pending` → `cache` | **丢失**（在设备缓存里，掉电即丢） |
| `flush(Durable)` | `pending` / `cache` → `media`（写入文件并 fsync） | **保留** |
| `discard` | 清除全部三层 + 释放介质空间 | 空间已归还 |
| `read` | 依次回落 `pending` → `cache` → `media` | — |

**三级耐久等级必须严格区分**：[04 §6.1 D2](04-kernel-interfaces.md) 定义了 `None` / `Write` / `Durable` 三档，[04 §4.2 E1](04-kernel-interfaces.md) 给出了对应的设备侧动作。假设备如果把三者混为一谈，「耐久等级是显式参数」这条契约就没有被测到——最典型的后果是内核以为自己 flush 到了 `Write`，实际上内核以为自己还在 `None`，掉电后丢数据。

同理，`write` 本身**不带**任何隐式持久化保证（[04 §6.1 D2](04-kernel-interfaces.md)）：`write` 之后不 flush，数据只存在于 `pending`，掉电必丢。

**注意：这里模拟的「掉电」是模型，不是真断电。** 真机上 `kill -9` 不会丢掉 OS page cache，所以「未 fsync 的数据在进程被杀后仍然存在」是真机的行为，而假设备会丢掉它——这是**有意的差异**：假设备模拟的是断电（更严格），真机 `kill -9` 模拟的是进程崩溃（更宽松）。两者的对拍方式因此不同，见 §7.3。

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
| `DeviceDegrade { to: Write }` | `max_durability()` 返回 `Write`；此后 `flush(Durable)` 返回 `Io{op: Flush}`（与 [04 §8 T2.1](04-kernel-interfaces.md) 的耐久等级不足同一写法） |
| `DeviceFail` | 所有写/读返回 `Io{op}`；`health()` 返回 `Failed` |

这条链路能测出一个关键行为：**设备降级后，提交路径必须失败而不是静默降级持久性**。内核不能悄悄把 `Durable` 降成 `Write` 继续提交——那等于把「已持久」的承诺偷走。

## 三、注入器（FaultInjector）

### 3.1 注入点的三个维度

[03 §11.3](03-consistency-kernel.md) 建议「按第 N 次调用后 kill 进程」。这个办法能用，但**脆弱**：重构改变调用次数，整个注入点就漂移到别的地方，测试可能悄悄失去覆盖。

本文采用三级表达：

| 维度 | 表达 | 作用 |
|---|---|---|
| **语义 site** | `wal.frame.partial`、`mapping.switch.after_log` | 主键：与重构无关；实现改了语义就编译不过 |
| **类别** | 设备级 / 内核级 | 决定打点机制（见 §3.1.1） |
| **调用序号** | `nth: 7`（第 7 次命中该 site） | 修饰符：用于「同一个 site 只想崩第几次」 |

**规则：语义 site 是主键，序号是修饰符。** 没有语义 site 的裸序号注入只能用于探索性测试，不能进回归用例。

#### 3.1.1 两类 site 与两套打点机制

site 分两类，**不可混用同一机制**：

| 类别 | 判定标准 | 打点机制 | 顺序由谁保证 |
|---|---|---|---|
| **设备级** | 该点对应**一次设备操作**（write / flush / rename / discard） | 由调用方在提交设备操作时**打标签**；注入器在执行该操作时（或其前后）触发 | **真实 I/O 顺序** |
| **内核级** | 该点**不对应任何设备操作**（纯内存状态、FSM 转移、分配决策） | 模块内显式打点 `fault_point!(site)`，非测试构建下编译为空 | **内核逻辑顺序** |

**为什么必须分开**：[ADR-0009](../adr/0009-fault-injection-harness.md) 的原始表述是「打点集中在 `IoExecutor` 一层」，理由是「打点顺序必须与真实 I/O 顺序一致」。但这个理由**只对设备级 site 成立**——内核级 site（如 GC 摘链、内存版本发布、事务提交点）根本不对应设备操作，把它们塞进 `IoExecutor` 是不可能的。

混用的后果是具体的：若把 `gc.unlink.before_free` 硬塞进设备路径，注入点的触发时机就变成「下一次设备操作时」，而真正的摘链动作发生在两次设备操作之间——注入点会漂移到错误的位置，测试会「通过」但什么都没测到。

因此：

- **设备级 site 由 `IoExecutor` 承载**：`submit_and_wait` 提交的操作需要携带 site 标签（或 `DeviceOp` 增加一个 `site` 字段）。这是对 [04 §6.3](04-kernel-interfaces.md) trait 形状的一个补充约束，需要一条 ADR（见 §11 第 1 项）。
- **内核级 site 在模块内显式打点**，与设备路径无关，不要求与 I/O 顺序对齐。

### 3.2 语义 site 目录

[03 §11.1](03-consistency-kernel.md) 的 I1–I10 是**故障场景**，本文把它们落到**具体的设备/内核操作边界**上。

**设备级 site**（对应一次设备操作）：

| 03 注入点 | 语义 site | 打点位置 | 期望结果 |
|---|---|---|---|
| I1 | `wal.frame.partial` | WAL 帧的 write 只写入前 N 字节（帧头 + 部分 body） | 恢复时忽略尾部半帧 |
| I2 | `wal.frame.written` | 帧写完、未 flush | 该帧及其后续全部丢失（未持久） |
| I3 | `wal.flushed` | flush 完成、内存状态未更新 | 重放该帧，幂等 |
| — | `wal.flush.partial` | flush 中途，部分帧已落盘 | 落盘部分可读，其余视为未持久 |
| I4 | `extent.write.partial` | extent payload 的 write 只写入前 N 字节 | extent 不可读；事务未提交即不可见 |
| I5 | `extent.write.done` | extent 写完、未 flush | 事务未提交即不可见；崩溃后不可读 |
| I6 | `ckpt.manifest.before` | Phase B 中途，MANIFEST 的 write 未发生 | 旧 checkpoint 仍有效 |
| I7 | `ckpt.superblock.before` | MANIFEST 写完、superblock 未更新 | 新 checkpoint 未生效，从 WAL 重放 |
| I8 | `mapping.switch.after_log` | `MappingSwitch` 的 write 返回后、内存映射更新前 | 重放后切换生效 |
| I10 | `migration.target.partial` | 迁移 M3 中途，目标 extent 的 write 只写入前 N 字节 | 回滚到源 extent（[03 §6.1 step 7](03-consistency-kernel.md)） |
| — | `txn.commit.before_flush` | `TxnCommit` 写完、未 flush | 事务不可见（未返回 Commit OK） |
| — | `txn.commit.after_flush` | flush 完成、未发布内存版本 | 重放后可见 |
| — | `ckpt.publish.before_rename` | shadow 目录写完、未执行 rename | 旧 checkpoint 仍有效 |

**内核级 site**（不对应设备操作）：

| site | 打点位置 | 期望结果 |
|---|---|---|
| I9 `gc.unlink.before_free` | GC 摘链后、释放前 | 空间泄漏（可回收），不得误删 |
| `mem.publish.before_visible` | 版本状态 `Pending → Published` 之前 | 版本不可见 |
| `extent.alloc.duplicate` | 分配出重复 LBA | 必须被断言检查捕获（见 §5.3） |

**`wal.flush.partial` 为什么必须存在**：02-kernel §11.2 建议的注入点包含「**WAL Flush 过程中**」——flush 执行到一半，部分帧已落盘、部分还没有。这与 `wal.flushed`（flush 已完成）是不同的场景：前者落盘状态是「前缀」，后者是「全部」。没有前者，就少覆盖一类真实故障。

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

- **在被测进程内**，注入器命中 `Crash` 时调用 `std::process::exit(EXIT_CRASH)`（退出码非 0）；
- **测试驱动器**用 `Command` 拉起被测进程，进程退出码非零即视为崩溃；
- 驱动器用同一份目录重新拉起进程，走恢复流程。

这与真实的 `kill -9` 在**进程语义**上等价：没有 unwind、没有 Drop、没有 flush。注意等价性只覆盖进程状态；**设备状态是另一个失效域**（§7.3）。

**确认提交日志的写入要求**（这是最容易出错的一环）：日志由测试驱动器在**内核返回 Commit OK 之后**追加，追加后必须显式 `fsync`——`process::exit` 不跑析构，Rust 的行缓冲 stdout 在管道下会被丢进未刷出的缓冲。日志格式是「追加式 + 每条记录带 CRC」，因此崩溃时可能留下一条半截的尾记录，恢复时直接丢弃尾记录即可。

这条日志写在**被测内核之外**（由驱动器持有文件句柄，或走一条独立的 IPC 通道），绝不能让内核自己写——内核写它就等于给内核开了后门，测试会永远通过。

日志丢失一条已提交的记录是可以接受的：那笔事务在判定上算「未确认」，按 §5.2 的宽松规则处理（要么不可见，要么可见且内容正确）。反过来（日志里有、恢复后读不到）才是硬失败。

### 3.5 辅助类型

上文的类型里，有几个是本文新引入的、需要在此定名（其余如 `DeviceId` / `DeviceHealth` / `DurabilityLevel` / `IoOp` 已在 [04](04-kernel-interfaces.md) 冻结）：

| 类型 | 定义 | 用途 |
|---|---|---|
| `BlockAddr` | `(device_id: u32, lba: u64)` | 假设备的块地址；4 KB 对齐 |
| `FaultSite` | 枚举，见 §3.2 的 site 目录 | 语义注入点；**穷尽 `match`** 保证新增 site 必须登记 |
| `CorruptScope` | `Block(BlockAddr)` / `Frame(usize)` / `Segment(usize)` | 数据位翻转的作用范围 |
| `FiredFault` | `{ site, nth, kind }` | 已触发注入的记录，供断言「预期的注入确实发生了」 |
| `SchedId` | 可调度单元的标识（线程 ID 或任务 ID） | 确定性调度器的调度单元 |
| `EXIT_CRASH` | 常量，退出码非 0 | 被测进程的崩溃退出码 |

## 四、假时钟与确定性调度

### 4.1 假时钟

内核中所有与时间相关的逻辑必须从**注入的时钟源**读时间，不得直接调用 `Instant::now()` / `SystemTime::now()`。这条约束已写进 [04 §2.6 C5](04-kernel-interfaces.md)（`Clock` 是冻结项 F15），由 lint 强制（§8）。

```rust
/// 生产实现：读真实时钟
pub struct SystemClock { origin: Instant }

/// 测试实现：逻辑时钟
pub struct FakeClock {
    /// 相对起点的单调纳秒计数
    now_nanos: Mutex<u64>,
    /// 每次读取自动推进的步长；0 = 不自动推进
    step: u64,
}
```

**为什么用 `u64` 纳秒而不是 `Instant`**：`Instant` 无法在稳定版 Rust 上任意构造，假时钟实现不了「推进到任意时刻」；返回相对单调纳秒后，假时钟只需维护一个计数器（[04 §2.6 C5.1](04-kernel-interfaces.md)）。墙上时间（对象创建时间戳等）由上层在写入时提供，内核不读墙上时钟（C5.2）。

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
| `Reclaimer` | 六条件合取的逐条判定；对 Draining 中的 extent 还要额外检查 [03 §7.4](03-consistency-kernel.md) 的三个迁移相关条件（[04 §10 R5](04-kernel-interfaces.md)） |
| `MigrationManager` | M1–M10、`MappingSwitch` 提交点、并发写冲突 |
| `MemoryTierManager` | 分层分配、释放、容量统计、`ResourceExhausted` 的触发条件 |
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

| 检查 | 断言 | 对应规则 |
|---|---|---|
| 所有返回过成功的写入可读 | 确认日志里每个 `(id, ver)` 都能读到且内容摘要一致 | INV2（已提交版本可恢复）；[architecture/02 I1](../architecture/02-storage-engine.md) |
| 未确认的事务的可见性 | 未提交的事务**不可见**；已提交但客户端未收到确认的事务**可见且内容与参考模型逐字节一致** | S1（[03 §4.1](03-consistency-kernel.md)：Pending 版本对外不可见） |
| 目录之间引用完整 | 无 dangling 引用 | [03 §6.1 step 8](../chatgpt/03-consistency-kernel.md)；`CorruptionKind::DirectoryDangling` |
| 无重复 extent 分配 | 同一 LBA 不被两个活跃 extent 占用 | [03 §6.2](../chatgpt/03-consistency-kernel.md)（`ExtentAlloc` 以 `physical_extent_id` 为幂等键） |
| 恢复后差异只是尾部截断 | 差异集合 ⊆ {注入点之后未提交的事务} | — |
| 恢复时长 < 10s（1TB 规模） | 计时断言（假设备上放宽为逻辑时间） | — |

**「未确认的对象不可读」这条要小心**：一个事务可能「数据已写完、`TxnCommit` 已 flush、但进程在发布内存版本前崩溃」。恢复后重放 WAL 会让它**可见且内容正确**——这是允许的，因为 `TxnCommit` 已经落盘并 flush，它在事实上已经提交了，只是客户端没收到确认。

所以判定不是「未确认即不可见」，而是三分支：

> - **未提交**（无已持久化的 `TxnCommit`）→ 不可见；
> - **已提交但客户端未收到确认** → 可见，且内容与参考模型**逐字节一致**；
> - **已提交且已确认** → 可见，且内容与参考模型逐字节一致。

绝不允许「可见但内容是部分的 / 撕裂的 / 与参考模型不一致」。这条判定同时覆盖了两个方向的错误：既不允许丢已提交数据，也不允许半成品泄漏。

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
    /// 确认过但读不到的：违反 INV2（已提交版本可恢复）
    pub missing: Vec<(ObjectId, VersionId)>,
    /// 读得到但内容与参考模型不一致：违反「差异只是尾部截断」
    pub divergent: Vec<(ObjectId, VersionId)>,
    /// 未提交却可见：违反 S1（Pending 版本对外不可见）
    pub phantom: Vec<(ObjectId, VersionId)>,
    /// 悬空引用：违反 03 §6.1 step 8 的引用完整性检查
    pub dangling: Vec<ExtentId>,
    /// 重复分配：违反 03 §6.2 的幂等键约定
    pub duplicated: Vec<PhysicalExtentId>,
}
```

`DiffSet` 为空 = 通过。任何一个字段非空 = 失败，并把内容写进测试报告。

**与内核 `verify` 的分工**：[04 §9 K3](04-kernel-interfaces.md) 冻结了 `RecoveryManager::verify` 是**内核内**唯一的校验实现。外部重建器与它**不互相替代**：

| | `RecoveryManager::verify` | 外部重建器（本节） |
|---|---|---|
| 检查什么 | 内核**自认为**的状态是否自洽（引用计数、epoch、状态机） | **磁盘上实际有什么**与内核的认知是否一致 |
| 能否发现「内核自己不知道自己丢了什么」 | 否 | **是** |
| 使用者 | 运维巡检、故障注入用例 | 仅故障注入测试 |

只做前者会漏掉「内核状态自洽但与磁盘不一致」这一类缺陷；只做后者会漏掉内核内部不变量（引用计数、epoch）被破坏但磁盘看起来正常的情况。「内核态」这一路走内核的公开读路径，是**故意**的：与重建态比对才能发现「磁盘上有但内核不认」或「内核认但磁盘上没有」。

### 5.4 L3 不变量常驻层

[04 §14.2](04-kernel-interfaces.md) 把「不变量断言」列为 CI 检查项之一，但三套不变量编号来自三份文档，本节先把它们分开：

| 编号 | 出处 | 含义 |
|---|---|---|
| `INV1`–`INV5` | [03 §10](03-consistency-kernel.md) | 五条硬性不变量（可测试形式） |
| `架构 I1`–`I6` | [architecture/02 §0.1](../architecture/02-storage-engine.md) | 存储引擎不变量 |
| `注入 I1`–`I10` | [03 §11.1](03-consistency-kernel.md) | 故障注入点（§3.2 已映射） |

三者的性质不同：

- **`INV1`–`INV5` 是结构性不变量**：逻辑身份不依赖物理地址、已提交版本可恢复、映射切换不改变进行中的读取、旧 extent 引用清零才回收、非持久化存储不是唯一权威副本。
- **`架构 I1` / `I5` / `I6` 依赖具体设备行为**（architecture/02 给出的检查手段分别是「崩溃恢复后全量校验」「故障注入」「**设计评审 + 断电测试**」），主要由崩溃注入用例在假设备上覆盖；其中 `架构 I6` **另外必须由 §7.3 的真机断电对拍兜底**，不能只靠假设备断言——假设备本身就是被测模型，只用它验证自己等于没验证。
- **`架构 I2` / `I3` / `I4` 是结构性检查**，可以在属性测试里断言。

**常驻口径**（[04 §16 第 2 项](04-kernel-interfaces.md) 与 [ADR-0009](../adr/0009-fault-injection-harness.md) 决策 5 一致）：**全部 11 条（`INV1`–`INV5` + `架构 I1`–`I6`）都在 `feature = "invariants"` 下常驻**——既在每次 CI 中断言，也在长时间运行的 fuzz / soak 中持续断言。其中依赖设备行为的 `INV2`、`INV5`、`架构 I1`/`I5`/`I6` **额外**由每一个崩溃注入用例断言：它们既是常驻检查项，也是 L2 用例的通过条件。

| 不变量 | 可执行化方式 | 载体 |
|---|---|---|
| `INV1` 逻辑身份不依赖物理地址 | 静态检查：读取路径中不出现按物理地址的比较分支 | lint + 人工评审 |
| `INV2` 已提交版本可恢复 | 崩溃注入后逐个确认已提交对象可读 | 常驻 + L2（每个用例都断言） |
| `INV3` 映射切换不改变进行中的读取 | 迁移与读并发，读到的内容前后一致 | 常驻 + 确定性调度器 |
| `INV4` 旧 extent 引用清零才回收 | 回收日志逐条检查引用计数 | 常驻 + 检查器 |
| `INV5` 非持久化存储不是唯一权威副本 | 每个可读对象的权威副本位置 ∈ {NVMe, HDD} | 常驻 + 检查器 |
| `架构 I1` 已提交对象至少映射到一个可读物理 extent | 崩溃恢复后全量校验：确认日志中的每个对象都能解析出可读 extent | 常驻 + L2 + 检查器 |
| `架构 I2` 提交顺序 = 日志顺序；版本顺序由 xid 决定 | **同一提交路径内** `commit_lsn` 大于其全部依赖记录的 LSN；版本链按 xid 单调。**不**断言「全局提交顺序 = LSN 顺序」——LSN 只描述写入 WAL 的先后，不描述提交顺序（[03 §1.1 R1](03-consistency-kernel.md)） | 常驻 + 检查器 |
| `架构 I3` 逻辑映射与物理映射分离 | 迁移前后 `ObjectId` / `VersionId` 不变 | 常驻 + L1 迁移测试 |
| `架构 I4` 索引可见性由 LSN 水位决定 | 水位未到的索引不可见 | 常驻 + L1 |
| `架构 I5` 持久化写入先落 WAL 再改内存索引 | 注入点断言：内存状态变化必晚于对应 WAL flush | 常驻 + L2 |
| `架构 I6` DRAM / GPU HBM 不是最终持久存储 | 掉电后 DRAM/GPU 上的数据必须从 NVMe 重建得到 | 常驻 + L2 + **真机断电对拍**（§7.3） |

**`架构 I2` 的第三个子句也要测**：[architecture/02 §0.1](../architecture/02-storage-engine.md) 的 I2 完整表述是「提交顺序 = 日志顺序；版本顺序由 xid 决定，不由 VersionId 决定；**不存在「对象已提交但索引永久缺失」**」。第三子句的载体是 `架构 I4` 的水位检查（恢复后索引水位落后于对象水位即标记滞后并触发后台增量重建，[03 §6.1 step 9](03-consistency-kernel.md)），由检查器在每次恢复后断言。

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

[02-kernel §11.2](02-kernel.md) 另外建议的六个崩溃注入点，全部落在本文的 site 目录里：

| 02-kernel 的注入点 | 对应 site | 类别 |
|---|---|---|
| WAL Record 写入过程中 | `wal.frame.partial` | 设备级 |
| WAL Flush 过程中 | `wal.flush.partial`（flush 中途，部分帧落盘） | 设备级 |
| Extent 写入过程中 | `extent.write.partial` | 设备级 |
| Checkpoint Manifest 发布过程中 | `ckpt.manifest.before`、`ckpt.publish.before_rename` | 设备级 |
| Mapping Switch 过程中 | `mapping.switch.after_log` | 设备级 |
| Transaction Commit 过程中 | `txn.commit.before_flush`、`txn.commit.after_flush` | 设备级 |

前三项由 [03 §11.1](03-consistency-kernel.md) 的 `注入 I1`–`I10` 直接覆盖；第四、六项由 `注入 I6`/`I7`/`I8` 覆盖；第五项由 `注入 I8` 覆盖；第六项（Transaction Commit）由本文 §3.2 补充的 `txn.commit.*` 两个 site 覆盖。因此 [02-kernel §11.2](02-kernel.md) 的注入场景与 [03 §11.1](03-consistency-kernel.md) 的注入矩阵不是两套东西，而是**场景 → 矩阵**的关系。

## 六、夹具必须遵守的约束

夹具是测试代码，但它处在内核的信任边界上——**一个撒谎的夹具比没有夹具更危险**。因此：

1. **假设备必须实现 `StorageDevice` 的完整语义**，不得为了「让测试好过」而简化。特别是掉电语义（§2.1）与耐久等级（`flush(None)` ≠ `flush(Write)` ≠ `flush(Durable)`）。
2. **夹具不得绕过内核的锁与校验。** 假设备只能通过设备接口影响内核，不得直接改内核的内存状态。检查器是只读的，且是独立重建而非复用内核状态（§5.3）。
3. **夹具不得自动重试。** 内核「不自动重试」这条契约（[04 §2.3 C1](04-kernel-interfaces.md)）只有在夹具会稳定地产生 `Io` / `Retryable` 错误时才测得到。夹具一旦「贴心」地重试，这条契约就永远测不到。
4. **夹具必须在 crate 内部，不进公共 API 快照。** 假设备实现的是 `StorageDevice`（而 [04 §14.1 规则 1](04-kernel-interfaces.md) 允许 `StorageDevice` / `IoExecutor` 携带 `PhysicalLocation`），所以它本身不违反规则 1；但把 `FakeDevice` / `FakeClock` 导出到公共表面会让测试专用的类型成为 SDK 的一部分。解决办法是放在 `#[doc(hidden)] pub mod testing` 下，并用 `#[cfg(feature = "test-harness")]` 门控——发布构建里整个模块不存在，自然也不进 `cargo public-api` 快照。
5. **夹具不得泄露 panic 到内核的错误路径。** 假设备内部出错必须返回 `KernelError::Io`，而不是 panic——否则测试会把「内核 panic」误判成「夹具 panic」。
6. **夹具必须可确定性复现。** 同一个 `seed` + 同一注入序列 → 同一结果。夹具里禁止使用真实随机源、真实时钟、哈希迭代顺序依赖。
7. **夹具不得用真实磁盘路径作为「状态模型」的存储。** 这条约束的对象是**假设备的模型状态**（哪些块被分配、哪些是易失的），它必须由假设备自己在内存里维护，不得让内核直接读写真实磁盘路径。**但这不禁止真实磁盘**：假设备的持久介质本来就是文件支撑的（§2.1），WAL 段与 checkpoint 目录也都落在测试临时目录下——它们正是「要落盘的东西」。这条同样不禁止 §7.3 的真机对拍：对拍跑在真机上，用的是真盘，与本条无关。

## 七、CI 接线

### 7.1 分层

| 流水线 | 内容 | 时长目标 | 触发 |
|---|---|---|---|
| PR | L1 + L2 的固定用例集（每 site 至少 1 个）+ L3 的快速版 | < 3 min | 每次 push |
| 每夜 | L2 全量 seed 空间（seed 0–1023）+ L3 完整版 + 进程崩溃语义对拍（§7.3） | < 60 min | nightly |
| 每周 | L2 扩展 seed + 掉电语义对拍（§7.3，仅实验机） | 数小时 | weekly |
| 发布前 | 全量 + 长时 soak（不变量常驻开启）+ **全部对拍通过**（§7.3） | 数小时 | tag |

**对拍是发布的前置条件**（[ADR-0009](../adr/0009-fault-injection-harness.md) 决策 1）：对拍未通过则不得发布。

### 7.2 失败复现

失败报告里必须包含：

```
seed: 12345
workload: <workload 名>
fault: <site> nth=<n> kind=<kind>
diff: <DiffSet 的非空字段>
```

CI 提供 `cargo test -- --seed 12345 --workload wal_crash --fault wal.flushed` 的单点复现命令。**没有可复现 seed 的并发 / 崩溃测试等于没有测试**。

**这条命令要求自定义测试入口**：`libtest` 会拒绝未知的 `--` 参数，因此崩溃注入与性质测试必须用 `harness = false` 的自定义 `main`（自己解析 `--seed` / `--workload` / `--fault`），或者把参数经环境变量传入。这不是可选项——参数解析不到就没法复现。

### 7.3 夹具自身的对拍

假设备必须与真实设备对拍，否则「夹具验证了错误的世界」。

但**进程崩溃与掉电是两个不同的失效域，必须分开对拍**——用错方法会得到「假设备是对的」的错误结论：

| 对拍项 | 失效域 | 方法 | 频率 |
|---|---|---|---|
| **进程崩溃语义** | 进程 | 真机上 `write → kill -9 → 重启`，验证：无 unwind、无 `Drop` 执行、无部分状态 | 每夜 |
| **掉电语义** | 设备 | 真断电（或设备级故障注入：关闭设备写缓存、`echo 1 > /sys/block/nvme0n1/queue/write_cache`），验证 `flush(Write)` 丢失、`flush(Durable)` 保留 | 每周（实验机） |
| 块对齐与跨块写 | 设备 | 真机写入非 4 KB 对齐长度的数据，验证读回与假设备一致 | 每夜 |
| TRIM / discard | 设备 | 真机上验证 discard 后空间确实归还 | 每周 |

**为什么不能只做进程崩溃对拍**：`kill -9` 不会丢掉 OS page cache，未 `fsync` 的数据在进程被杀后**仍然存在**。真机会显示「数据在」，而假设备（§2.1）会丢掉它——两者不一致，但**不是假设备错了**：假设备模拟的是断电（更严格），真机 `kill -9` 模拟的是进程崩溃（更宽松）。这是两个不同的失效域，不能用同一个方法验证。

**掉电语义对拍的现实约束**：真断电需要实验机（可远程断电的机器或可关写缓存的设备），不是每台 CI 机器都能做。因此：

- 掉电语义对拍**每周在实验机上跑**，其结果是**发布前置条件**（§7.1）；
- 在掉电语义对拍首次通过之前，`flush` 等级差异（`None` / `Write` / `Durable`）这一项**处于未验证状态**，必须在发布说明里写明。

对拍失败 = 假设备的语义模型错了，所有基于它的结论都要作废。因此对拍是**先决条件**：在假设备与真机对拍通过之前，L2 的结果只能当作「对假设备的结论」，不能当作「对内核的结论」。

## 八、编译期与 CI 检查清单

| 检查 | 工具 | 失败条件 |
|---|---|---|
| 公共 API 不含夹具类型 | `cargo public-api` diff | 快照出现 `FakeDevice` / `FakeClock` |
| 内核代码不直接读真实时钟 | 自定义 lint | 内核 crate 里出现 `Instant::now` / `SystemTime::now`（[04 §2.6 C5](04-kernel-interfaces.md)） |
| 注入 site 编译期校验 | 枚举 + `match` 穷尽 | 新增 site 未登记到 site 目录 |
| 崩溃注入用例覆盖 | 用例清单 diff | 每个 site 至少 1 个用例 |
| 设计债务 xfail 清单 | 用例清单 diff | [architecture/07 §7.1](../architecture/07-operations.md) 的每条债务都有对应 xfail 用例；债务解除后该 xfail 必须已移除 |
| 不变量断言 | `feature = "invariants"` 常驻检查 | 任一 `INV1`–`INV5` / `架构 I1`–`I6` 失败 |
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
| [architecture/07 §7](../architecture/07-operations.md) | 故障注入章节的落地形态；测试债务清单（§7.1）由 §8 的 CI 检查守住 |
| [ADR-0009](../adr/0009-fault-injection-harness.md) | 本文的决策记录：夹具形态、语义 site 优先、site 两级分类、不变量常驻、夹具受公共 API 约束 |

## 十一、下一步

本文把「怎么测」定了下来。相关决策已记录在 [ADR-0009](../adr/0009-fault-injection-harness.md)（夹具形态、语义 site 优先、site 分设备级与内核级两级、不变量常驻、夹具受公共 API 约束）。

剩下的设计工作有三项：

1. **把设备级 site 的打点机制回填到 [04 §6.3](04-kernel-interfaces.md) 的 `IoExecutor` 契约里。** §3.1.1 已经把 site 分成两级：**设备级** site 需要 `IoExecutor` 能携带 site 标签（或 `DeviceOp` 增加 `site` 字段），**内核级** site 在模块内显式打点、不进设备路径。剩下要定的是设备级那一半的具体形状——标签放在 `DeviceOp` 上还是放在 `submit_and_wait` 的参数上，两者的公共 API 影响不同；以及 `submit_batch_and_wait` 的批量场景怎么给每个操作打标签。这是对 [04](04-kernel-interfaces.md) 冻结契约的补充，需要一条 ADR。
2. **索引恢复的增量算法**（[03 §12 第 2 项](03-consistency-kernel.md)）——「日志水位 → 索引构建水位」的追赶策略，以及「滞后索引的查询回退路径」的代价估计。 → 已完成，见 [06-index-recovery.md](06-index-recovery.md)。该文的 §6.3 要求本文件的夹具扩展一层：L1 模块表与 §3.5 的 `FaultSite` 枚举都要加入索引构建器与两个索引层 site。
3. **硬件资源管理的实现细节**（[03 §12 第 3 项](03-consistency-kernel.md)）——参数化 Placement 代价模型、定义 NUMA 亲和调度与 io_uring 队列深度的切换判据。这一项会反过来影响假设备要模拟多少设备差异（§2.3 的降级模型是否够用）。
