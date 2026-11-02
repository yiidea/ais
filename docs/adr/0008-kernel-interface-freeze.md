# ADR-0008：内核接口冻结与统一错误模型

- **状态**：已接受
- **日期**：2026-11-02

## 背景

内核是整个系统里唯一「错了就丢数据」的部分。Intent 执行器、索引网格、Agent 运行时都可以迭代重写，但内核的 trait 形状、错误语义与模块调用方向一旦被依赖就无法随意改动：

- 改一个 trait 的返回类型，会同时波及 Object / Extent / Mapping / WAL / Recovery 与全部测试夹具；
- 改一个错误的分类，会同时改变客户端的重试策略与运维告警的分级；
- 改一处锁的持有范围，会同时改变读延迟与死锁风险。

在实现开始之前，这些契约必须被显式冻结并写下来，而不是散落在各篇设计文档的叙述里。

另外还存在一个具体问题：此前各篇文档对「冲突」的处理口径不一致——有的地方说「可重试」，有的地方说「由上层决定」。如果内核自己悄悄重试写冲突，AI 工作流层就无法知道这次写入失败过，也无法决定是重试、换路径还是交给用户。

## 决策

1. **在实现前冻结内核契约**，写入 [docs/chatgpt/04-kernel-interfaces.md](../chatgpt/04-kernel-interfaces.md)：
   - 标识符类型的作用域与单调性规则；
   - `ObjectManager` / `ExtentManager` / `MappingManager` / `StorageDevice` / `IoExecutor` / `WalManager` / `TransactionManager` / `CheckpointManager` / `RecoveryManager` / `Reclaimer` / `MigrationManager` / `MemoryTierManager` 各自的职责边界；
   - 模块调用方向、依赖禁令与五层锁顺序；
   - 已冻结且不得单方面变更的语义清单。

2. **采用统一的 `KernelError` 错误模型**，要点：
   - `#[non_exhaustive]` 枚举，允许后续新增变体而不破坏调用方匹配；
   - 错误按「可自动重试 / 需上层决策 / 永久失败 / 内部 bug」四类划分，**内核内部不自动重试写冲突**，冲突原样返回给上层工作流；
   - 每个模块只允许返回与自身职责相关的错误子集，设备错误不得穿透到对象层；
   - 写冲突统一命名为 `Conflict(ConflictKind::WriteConflict)`，取代 03 §3.2 中的 `RetryableConflict`；
   - 错误中禁止携带物理地址，诊断需要物理位置时由调用方自行二次查询映射表。

3. **核心内核同步，设备 HAL 是唯一的 `async` 边界**。核心模块（Object / Extent / Mapping / Transaction / WAL / Checkpoint / Reclaimer / Migration / MemoryTier）全部使用同步 trait；`StorageDevice` 保持 [ADR-0004](0004-bypass-kernel-io.md) 与 [01-overview §15.3](../chatgpt/01-overview.md) 已确定的 async 形态（io_uring / SPDK 天生是完成通知模型），两者由一个同步 façade `IoExecutor` 桥接：每 NUMA 节点一个 reactor 线程，提交设备操作后阻塞等待完成，使核心内核保持「调用返回即已完成」的语义。

   理由：WAL 的持久化契约与临界区语义在同步模型下才容易证明；而设备 I/O 是真正需要并发的地方，不应该为了内核的简单性而放弃 io_uring 的批量提交能力。

   来源说明：`StorageDevice` 的 async 形态来自 [01-overview §15.3](../chatgpt/01-overview.md)，Storage HAL 的后端选型（io_uring / SPDK / POSIX 回退）来自 [ADR-0004](0004-bypass-kernel-io.md)；ADR-0004 本身没有定义 trait 形态。

4. **公共 API 表面不得暴露物理类型**：物理标识符只允许出现在 `pub(crate)` 接口中。

5. **变更接口必须走 ADR**，并更新契约文档与 CI 的公共 API 快照。

## 备选方案

1. **不冻结，边写边定** → 模块之间互相依赖对方的临时约定，改一个签名要在五个模块里同时改，最后没人说得清哪条是契约。
2. **用 async trait 表达全部内核接口** → 锁语义与「调用返回即已持久」的契约被 async 化稀释，崩溃恢复的正确性更难证明。
3. **内核自动重试冲突** → 上层丢失「写入失败过」这一信息，AI 工作流无法正确决定重试或降级。
4. **错误类型按模块分散定义** → 每层各自包装，错误链越来越长，客户端无法做统一的重试与告警分级。
5. **直接沿用 `std::io::Error` + `anyhow`** → 丢失「可重试 / 需上层决策」这类语义信息，运维侧无法据此分级。

## 后果

**正面**

- 实现可以并行开工：模块之间有明确的接口边界与调用方向。
- 契约集中在一处，评审与变更都有落点。
- 错误分类可直接驱动客户端重试策略与运维告警分级。
- 同步内核让持久化语义与锁语义可以被严格推理。

**负面**

- 前期多写一份契约文档，且实现中常会发现契约需要微调。
- 错误模型与模块边界的调整成本变高（需要走 ADR）。
- 公共 API 表面不能直接暴露物理位置，某些调试工具需要额外的内部接口。

**缓解**

- 契约文档标注「冻结 / 可演进」与变更方式，区分「改实现」与「改契约」。
- 提供 `cargo public-api` 快照与自定义 lint，把「公共表面不含物理标识符」变成自动检查而非评审约定。
- 允许新增错误变体（`#[non_exhaustive]`）与带默认实现的 trait 方法，避免小改动也要写 ADR。
- 第一版只冻结单机单实例内核，分布式与多副本仲裁留到后续 ADR，避免过早冻结尚未验证的语义。
