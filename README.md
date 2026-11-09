# AINS — AI-Native Storage Engine

[![ci](https://github.com/yiidea/ais/actions/workflows/ci.yml/badge.svg)](https://github.com/yiidea/ais/actions/workflows/ci.yml)

> **一个面向 AI 的存储引擎：AI 替代 SQL，存储直接面向硬件，按 AI 的特性重新设计数据组织、执行与持久化。**

---

## 1. 一句话定义

AINS 不是一个「支持向量的关系数据库」，也不是「NL2SQL 的数据库」。它把传统数据库中的 **SQL / Table / Row / B+Tree / Volcano Executor** 五件套整体替换为：

| 传统数据库 | AINS |
|---|---|
| SQL 查询语言 | **Intent DAG**（意图有向无环任务图）+ Natural Language API |
| Table / Row / Column | **统一 AI 对象模型**：Object Header + 七种 Payload（Vector / Graph / Tensor / Document / Memory / Media / Event） |
| Heap Page + B+Tree | **Object → Logical Extent → Physical Extent → Physical Block** 四层组织 |
| Volcano Tuple-at-a-time 执行器 | **Task DAG Runtime**：Object / Batch / Tensor / Stream / Task 粒度混合执行 |
| Buffer Pool + VFS + Block Device | **Hardware-Aware Storage**：DRAM / CXL / GPU HBM / NVMe / RDMA 统一资源管理，io_uring / SPDK / GPUDirect 直通 |

核心原则（贯穿全部文档）：

> **AI 负责决策，存储引擎负责执行，硬件负责加速。**
>
> AI 理解意图、规划任务、判断语义；引擎保证确定性执行、事务一致性、并发控制、持久化与故障恢复；**LLM 永不直接操作磁盘。**

## 2. 为什么需要它

| AI 工作负载 | 真实的数据需求 | 现有方案的错配 |
|---|---|---|
| RAG | 语义检索 + 上下文组装 | 需额外向量库（Milvus / pgvector）并自行做一致性 |
| Agent | 多步骤数据探索、工具调用 | SQL 难以表达跨模态、多跳的任务意图 |
| LLM 推理 | 权重、KV Cache、上下文 | 行列页存储不是 KV/Tensor 的最佳组织 |
| AI 训练 | 大 Tensor 顺序批量读 | 事务与页缓存开销过大 |
| 多模态 | 图、音、视频、文本 | 数据类型与存储组织分裂到多个系统 |
| GraphRAG | 实体、关系、路径 | 需额外图数据库 |
| AI Memory | 短期/长期记忆、经验生命周期 | 传统表结构不表达记忆的演化与遗忘 |
| 多 Agent 协作 | 共享上下文、任务状态 | 需要新的并发与一致性模型 |

**结论**：AI 负载是「语义 + 多模态 + 长生命周期 + 高并发任务」的复合体，值得一个重新设计的存储内核，而不是在旧内核上打补丁。

## 3. 系统总览（六层）

```text
┌──────────────────────────────────────────────────────────────────┐
│ 1. AI Interface Layer      NL API / Intent API / Object API / SDK │
│                            Workflow API · Agent Tool Interface    │
├──────────────────────────────────────────────────────────────────┤
│ 2. AI Intelligence Layer   Semantic Understanding · Intent Planner │
│                            Reasoning Engine · Workload Learner    │
│                            Policy & Permission（确定性，不依赖模型）│
├──────────────────────────────────────────────────────────────────┤
│ 3. Semantic Data Layer     Object Store · Vector Store · Graph     │
│                            Tensor Store · Memory Store · Metadata  │
├──────────────────────────────────────────────────────────────────┤
│ 4. Execution Engine        Task DAG Scheduler · Retrieval Engine   │
│                            Streaming / Batch / Tensor Engine       │
│                            Transaction Manager · Cache Manager     │
├──────────────────────────────────────────────────────────────────┤
│ 5. Hardware-Aware Storage  Extent Manager · Placement Manager      │
│                            Memory Tier Manager · WAL & Recovery    │
│                            Index Engine · Compaction & GC          │
├──────────────────────────────────────────────────────────────────┤
│ 6. Hardware Abstraction    CPU/NUMA · DRAM/CXL · GPU/HBM · NVMe    │
│                            RDMA/Network · io_uring/SPDK/GPUDirect  │
└──────────────────────────────────────────────────────────────────┘
```

**关键边界**：第 2 层的 AI Intelligence Layer 只产出**可验证的执行计划**（Intent IR / DAG），不产出硬件指令；第 5、6 层全部是确定性内核。模型只出现在三个位置：意图理解、复杂规划、工作量学习——**不在每次 I/O 的路径上**。

## 4. 文档导航

### 主文档

| 文档 | 内容 |
|---|---|
| [docs/chatgpt/01-overview.md](docs/chatgpt/01-overview.md) | **主设计文档**：二十一章完整方案（设计目标、架构、对象模型、Intent DAG、硬件直通存储、索引、执行、事务、持久化、Memory、安全边界、分布式、API、代码组织、数据结构、实施路线、Benchmark、架构问题、下一步） |
| [docs/chatgpt/02-kernel.md](docs/chatgpt/02-kernel.md) | **核心存储内核深入设计**：Object / Extent / Physical Block 三层组织、DRAM/NVMe/GPU HBM 统一资源管理与迁移、Object Version / MVCC / WAL / Checkpoint 一致性 |
| [docs/chatgpt/03-consistency-kernel.md](docs/chatgpt/03-consistency-kernel.md) | **一致性协议与崩溃恢复**：WAL 二进制格式、锁分层、内存状态机、Checkpoint / Recovery 算法、迁移提交点、GC 六条件判定、故障注入矩阵 |
| [docs/chatgpt/04-kernel-interfaces.md](docs/chatgpt/04-kernel-interfaces.md) | **内核接口冻结**：Rust trait 形状、统一错误模型与可重试性、模块调用方向与依赖禁令、五层锁顺序、公共 API 表面约束、接口变更流程 |
| [docs/chatgpt/05-kernel-test-harness.md](docs/chatgpt/05-kernel-test-harness.md) | **内核测试夹具与故障注入**：假设备掉电模型、语义注入点目录、假时钟与确定性调度、一致性检查器、不变量常驻断言、CI 接线 |
| [docs/chatgpt/06-index-recovery.md](docs/chatgpt/06-index-recovery.md) | **索引恢复的增量算法**：LSN / xid 双水位、追赶三阶段与「先段后水位」发布顺序、幂等键与内容哈希、回退扫描路径与代价模型、索引段回收判定 |
| [docs/chatgpt/07-hardware-resource-management.md](docs/chatgpt/07-hardware-resource-management.md) | **硬件资源管理的实现细节**：代价模型参数化与硬约束、NUMA 亲和三规则、io_uring 队列深度与 flush 槽位预留、O_DIRECT / SPDK / POSIX 的后端切换判据 |
| [docs/chatgpt/08-index-layer.md](docs/chatgpt/08-index-layer.md) | **索引层的实现结构**（内核之外的调用方）：五个部件（CatchUp / IndexKind / SegmentBuilder / Coordinator / Reclaimer）各自读什么写什么、「先段后水位」的崩溃点表、G1–G6 在代码里的形状、覆盖判定与新鲜度、重启恢复 |

### 实现文档与使用手册

| 文档 | 内容 |
|---|---|
| [docs/code/README.md](docs/code/README.md) | 按当前代码写的实现文档索引与维护约定 |
| [docs/code/kernel.md](docs/code/kernel.md) / [hal.md](docs/code/hal.md) | 内核（对象、版本、快照、WAL、检查点、恢复、回收）与硬件路径（设备、执行器、队列深度、reactor） |
| [docs/code/index.md](docs/code/index.md) / [engine-intent.md](docs/code/engine-intent.md) | 索引层（五个部件、追赶、新鲜度与覆盖）与引擎（文档载荷、检索、Intent、覆盖标记） |
| [docs/code/platform-testing.md](docs/code/platform-testing.md) | 启动期平台核对与故障注入夹具 |
| [docs/code/usage.md](docs/code/usage.md) | 使用手册：最小程序、覆盖标记、并发与背压、崩溃恢复测试、诊断、已知限制 |

### 补充架构文档（本仓库新增，供实现落地）

| 文档 | 内容 |
|---|---|
| [docs/architecture/01-overview.md](docs/architecture/01-overview.md) | 设计目标 / 非目标、四平面架构、关键路径旅程、一致性分层、方案对比 |
| [docs/architecture/02-storage-engine.md](docs/architecture/02-storage-engine.md) | SemanticObject、Append-Only 语义日志、索引网格、意图级 MVCC、GC、恢复 |
| [docs/architecture/03-query-engine.md](docs/architecture/03-query-engine.md) | Intent IR、可插拔 Planner、执行计划 DSL、Verifier、学习型代价模型、运行时 |
| [docs/architecture/04-hardware-path.md](docs/architecture/04-hardware-path.md) | io_uring / SPDK 直通、内存分层、NUMA 亲和、SIMD 向量扫描、SLO 预算、回退矩阵 |
| [docs/architecture/05-ai-native-features.md](docs/architecture/05-ai-native-features.md) | 意图级 MVCC、语义幂等、溯源图、语义 GC 与记忆固化、注意力感知返回 |
| [docs/architecture/06-distributed.md](docs/architecture/06-distributed.md) | Meta Plane、语义路由分片、日志复制、因果租约、故障恢复 |
| [docs/architecture/07-operations.md](docs/architecture/07-operations.md) | 可观测性、容量规划、资源治理、部署形态、运维手册、内核错误映射、测试策略与测试债务清单、Roadmap |
| [docs/architecture/08-api-and-sdk.md](docs/architecture/08-api-and-sdk.md) | gRPC 契约、错误模型、SDK 设计原则与示例 |
| [docs/architecture/09-threat-model.md](docs/architecture/09-threat-model.md) | STRIDE 威胁模型、提示注入专项、审计与取证 |
| [docs/adr/](docs/adr/) | 16 篇架构决策记录（ADR），含内核接口冻结（0008）、故障注入夹具（0009）、硬件资源管理（0010）及设备级注入点、`ChangeFeed` 冻结、索引层写 WAL、SemLog 可见性对齐、索引段回收、资源类型冻结（0011–0016）。每条 ADR 都带「实现落点」一节，标明它在本仓库代码里对应哪个文件、哪些部分仍是预留 |

## 5. 概念速览

### 5.1 统一 AI 对象

```c
typedef struct AIObject {
    ObjectId    id;
    ObjectType  type;           // VECTOR / GRAPH / TENSOR / DOCUMENT / MEMORY / MEDIA / EVENT
    ObjectId    namespace_id;
    uint64_t    version;
    uint64_t    create_ts, update_ts;
    uint32_t    flags;
    StorageRef  storage_ref;    // → Logical Extent
    IndexRef    index_ref;      // → 索引网格
    ContextRef  context_ref;    // → 语义/上下文关系
    SecurityRef security_ref;
    /* payload: 由 type 决定，统一头 + 专属载荷 */
} AIObject;
```

同一对象可以同时承载多种数据视图（一个源码文件 = Document + Vector + Graph + Memory + Event），而不是建五张表。

### 5.2 Intent DAG（AI 不再生成 SQL）

```text
自然语言 → Intent Understanding → Intent IR → Semantic Validation
        → Task DAG → Cost & Resource Planning → Execution DAG
        → Storage Engine → Permission / Data Scope
```

操作原语保持小型稳定（LOCATE / RETRIEVE / FILTER / SIMILARITY / RELATE / TRAVERSE / AGGREGATE / RANK / READ / WRITE / REMEMBER / FORGET / INFER / TRANSFORM / STREAM），AI 生成、引擎验证并执行。

### 5.3 数据组织四层

```text
AIObject（逻辑对象）
   │
   ▼
Logical Extent（逻辑数据范围，与硬件无关）
   │
   ▼
Physical Extent（实际硬件存储范围：DRAM / CXL / HBM / NVMe / HDD）
   │
   ▼
Physical Block（硬件访问单元：4K / 64K / GPU 传输粒度）
```

逻辑与物理映射分离：热点 Embedding 进 DRAM、热点 Tensor 进 GPU HBM、普通文档留 NVMe、冷数据下沉。

### 5.4 双层事务

| 事务类型 | 职责 | 生命周期 |
|---|---|---|
| **Storage Transaction** | 对象一致性、版本控制、冲突检测、WAL、恢复 | 短事务 |
| **AI Workflow Transaction** | 任务状态、步骤依赖、中间结果、重试、Checkpoint、补偿 | 长事务 |

> 不因面向 AI 就放弃 ACID：底层必须是 MVCC + WAL + Object Versioning。

## 6. 快速上手

零外部依赖、可直接运行（需要 Rust 1.75+，离线可构建）：

```bash
cargo test                       # 112 项：内核契约、变更流、派生记录、存活快照、资源类型、平台核对、索引层、队列深度与背压、reactor
cargo test --features test-harness   # +10 项掉电注入下的崩溃恢复（共 122）
cargo run --example quickstart   # 下面这段代码，可直接运行
cargo run --bin ains-demo        # 写入 → 向量检索 → Intent 管线 → 删除 → 检查点 → 重启恢复
```

```rust
use ains::engine::Engine;
use ains::kernel::KernelConfig;
use ains::hal::mem::MemDevice;
use ains::types::{DeviceId, ObjectType};

let device = Box::new(MemDevice::new(DeviceId(1), 64 << 20));
let engine = Engine::open(KernelConfig::new("ckpt.snap"), device)?;

// 一个对象 = 文档 + 向量 + 元数据，而不是建三张表
let id = engine.put_document(
    ObjectType::Document,
    "WAL 检查点的原子发布",
    vec![0.9, 0.1, 0.0],
    Default::default(),
)?;
// 派生索引（向量 / 元数据）在提交之后追赶，读路径永远看到已提交状态
for hit in engine.search(&[1.0, 0.0, 0.0], 5) { println!("{:.3} {}", hit.score, hit.text); }
# Ok::<(), ains::KernelError>(())
```

概念级的用法（自然语言 → Intent DAG）：

```python
# 用意图，而不是 SQL
result = engine.query("""
  找出最近 30 天访问频率最高的 100 个 PostgreSQL 源码函数，
  分析它们之间的调用关系，并输出依赖图
""")

# 或者直接使用稳定的 Intent API
engine.intent({
  "intent": "analyze_function_access",
  "scope":  {"object_type": "document", "domain": "postgresql_source"},
  "operations": [
    {"op": "filter",       "field": "access_timestamp", "range": "last_30_days"},
    {"op": "aggregate",    "field": "function_id", "metric": "access_count"},
    {"op": "topk",         "k": 100},
    {"op": "graph_expand", "relation": "calls"},
    {"op": "visualize",    "format": "dependency_graph"}
  ]
})
```

## 7. 当前状态与下一步

仓库现有两部分：**设计文档**（可落地的架构蓝图）与 **Rust MVP 内核**（`src/`，零外部依赖）。

Rust MVP 已实现并全部测试通过：

- `src/types`、`src/error`、`src/checksum`、`src/clock`：冻结的类型、四分类 `KernelError`、CRC32C、可注入时钟；
- `src/hal`：`StorageDevice`（async 风格 + `BoxFuture`）与同步门面 `IoExecutor`，含 `MemDevice` / `FileDevice`；
- `src/wal`：固定段 WAL（4KB 段头、36 字节帧头、18 种记录、增量 CRC 扫描、崩溃尾部语义），非事务派生记录走 `append_derived` 并按记录族鉴权（ADR-0013，拒绝时不写入任何字节）；
- `src/kernel`：Object / Extent / Mapping / Transaction / Checkpoint / Recovery / Reclaimer，快照隔离 MVCC、提交顺序（payload → data barrier → WAL → commit → publish）；
- `src/kernel/feed`：F16 `ChangeFeed`（对象变更流 + 游标分页 + 未决事务屏障 + 截断水位显式失败），索引追赶者的唯一读入口；
- `src/kernel/live`：F17 `LiveSnapshotSource`（存活快照集合 + 逐条目可见性，复用内核唯一的 `version_visible`），索引段回收器 G3/G4 的唯一输入；
- `src/hal/qd`：队列深度与背压（07 §4，HRM7）——`QueueDepth` 按 `qd_effective = min(设备上限, qd_usable + qd_flush_reserve)` 推导并在启动期拒绝 `qd_flush_reserve = 0` 或预算超发；`Slots` 给普通 I/O 与 flush 两套互不共享的额度，满了等到 deadline 即返回 `Retryable(Backpressure)`（不静默截断、不缩小 batch、不自动重试），`SyncIoExecutor` 每次提交都先过槽位；`QdTuner` 实现 §4.3–§4.4 的升/降/不动三条与迟滞冷却，阈值全部相对 `qd_usable` 而非含预留的 `qd_effective`；
- `src/hal/reactor`：常驻 reactor 线程上的 `IoExecutor`（04 §6.3 I1 / 07 §4）——调用方线程先在 `Slots` 上占额度、满了立即 `Retryable(Backpressure)`，再把操作交给 reactor 并阻塞等结果，在飞操作数因此能超过 1 且始终受 `qd_usable` 约束；reactor 按窗口采样 `lat_p50`/`lat_p99` 与在飞峰值（饱和视作 `util`），驱动 `QdTuner` 调整普通槽位数并用 `Slots::set_usable` 生效；`KernelConfig::reactor`（`ReactorSpec`）启用，缺省仍是 `SyncIoExecutor`；
- `src/index` 的追赶与新鲜度：`freshness` 把索引水位暴露成连续量（`IndexFreshness`：覆盖判定只读快照 `xmin` 与 xid 水位，A1/A3/A4；代价模型里「字节缺口 → 变更条数」只按本进程实测比例折算，没有样本就返回 `None`，不拿常数冒充测量值），`tracker::catch_up` 是「拉批 → 建段 → 记证据 → 发布 → CAS 推进水位」的循环，屏障停在未决事务之前且不丢已提交变更、截断区间返回 `RebuildRequired` 且不动水位（A9/A10）；`Engine::with_index` 打开索引后，`search_with_coverage` 在回答前追赶并按查询快照给出 `Complete` / `Recovered` / `Partial`，不再把旧结果当最新结果；
- `src/platform`：启动期平台核对（07 §1.2 / §5）——部署声明与设备自报的耐久等级、对齐、discard 能力逐台比对，能力不足直接拒绝启动，降级写进 `PlatformReport` 交给运维；`select_backend` 按 §5.3 决策表在启动期定下 io_uring / SPDK / POSIX，`check_direct_io` 执行 §5.4 的对齐规则（未对齐返 `InvalidArgument`，不静默降级）；核对失败时内核根本不会被打开；
- `src/index`：索引层（08 §2 的五个部件，内核之外的调用方）——`segment` 定义自校验段格式（段头 + 逐条目 CRC32C），`manifest` 以 `Advance` 票据在类型层面保证「先发布段、再推进水位」且水位单调、CAS 单写者，`catchup` 消费 F16 并对历史截断显式返回 `RebuildRequired`，`builder` 按 06 §2.3 动作表产段并经 `append_derived` 记录发布，`reclaim` 按 ADR-0015 的 G1–G6 判定（读失败按可见处理，保守保留）；
- `src/engine`：Document 载荷、暴力向量检索、Intent 管线；
- `src/testing`（feature `test-harness`）：故障注入设备与确定性注入器，验证崩溃恢复契约；
- `examples/quickstart`：README §6 的代码，作为编译检查长期存在。

验证方式：`cargo test`（112 项）、`cargo test --features test-harness`（+10 项崩溃恢复）、`cargo run --bin ains-demo`。

CI（`.github/workflows/ci.yml`）对每个 PR 跑这些任务：`rustfmt`、`clippy`（默认与 `--all-features` 两种组合，`-D warnings`）、`test` 矩阵（默认 / 全特性）、`rustdoc`（`-D warnings`，文档链接必须能解析）、`offline build`（守住零外部依赖）、`demo`（demo 与 quickstart 必须真的跑起来）、`msrv`（1.75 编译 + 测试）。

已完成的设计冻结：

- Phase 1 内核接口（Object / Extent / Mapping / WAL / Transaction / Checkpoint / Reclaimer / Migration）→ [04-kernel-interfaces.md](docs/chatgpt/04-kernel-interfaces.md)、[ADR-0008](docs/adr/0008-kernel-interface-freeze.md)
- 故障注入夹具与不变量可执行化方案 → [05-kernel-test-harness.md](docs/chatgpt/05-kernel-test-harness.md)、[ADR-0009](docs/adr/0009-fault-injection-harness.md)
- 索引恢复的增量算法与回退代价模型 → [06-index-recovery.md](docs/chatgpt/06-index-recovery.md)，配套四条 ADR 已补齐：
  - [ADR-0012](docs/adr/0012-changefeed-freeze.md)（`ChangeFeed` 入冻结清单 F16 + `IndexId` 补进 F1 + 游标分页）
  - [ADR-0013](docs/adr/0013-index-manifest-in-wal.md)（索引层写内核 WAL 的窄口 + 非事务记录截断条件 T4）
  - [ADR-0014](docs/adr/0014-semlog-epoch-vs-xid-visibility.md)（SemLog 的 epoch 与内核 xid 的可见性对齐；内核侧已满足，`PlacementEpoch` 随迁移实现落地）
  - [ADR-0015](docs/adr/0015-index-segment-reclamation.md)（索引段回收判定 G1–G6 与归属；内核侧的 F17 `LiveSnapshotSource` 已实现）
- 索引层的实现结构 → [08-index-layer.md](docs/chatgpt/08-index-layer.md)（五部件职责、崩溃点表、G1–G6 的代码形状），已在 `src/index` 落地
- 硬件资源管理的实现细节 → [07-hardware-resource-management.md](docs/chatgpt/07-hardware-resource-management.md)、[ADR-0010](docs/adr/0010-hardware-resource-management.md)（代价模型参数化、NUMA 亲和、队列深度与后端判据）；配套两条 ADR 已补齐：
  - [ADR-0011](docs/adr/0011-device-op-site-tagging.md)（设备级注入点：site 标签放在 `DeviceOp` 上）
  - [ADR-0016](docs/adr/0016-resource-types-freeze.md)（`DeviceHealth` / `ResourceStats` / `AccessPattern` / `FaultDomainId` 入冻结清单 F18–F21）

冻结清单现为 **F1–F21**，完整对照见 [04 §1.2](docs/chatgpt/04-kernel-interfaces.md) 与 [ADR-0016 决策 2](docs/adr/0016-resource-types-freeze.md)。

下一步（按依赖顺序）：

1. **索引追赶的后台化与持久化**：`catch_up` 已能把引擎的索引追平并在查询前补扫，但协调区 `SegmentManifest` 只住在进程内存里、追赶发生在调用线程上；接下来要把协调区按 ADR-0013 写进 WAL（并接上 0x31 的交叉核对），把追赶交给后台线程，再用 07 的 `ReactorSpec`/QD 给它排队。
2. **io_uring / SPDK 后端**：`StorageDevice` 已冻结、`FileDevice` 已是 POSIX 实现，选路与对齐判据已落在 `src/platform`（07 §5），缺的是真实后端实现与 B5 压测数据。
3. **组提交与写路径并发**：MVP 用一把 writer 锁串行化写；改为每对象写意向锁 + WAL 组提交（03 §2.5）。
4. 第一版不实现：分布式事务、完整 GPU 后端、CXL、RDMA、自研模型——但接口已预留。

**最终定位**：

> **AI as the Interface · Intent as the Execution Model · Object as the Data Model · Extent as the Storage Model · Hardware as the Execution Foundation.**
