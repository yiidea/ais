# AINS — AI-Native Storage Engine

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
| [docs/adr/](docs/adr/) | 10 篇架构决策记录（ADR） |

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

## 6. 快速上手（概念级）

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

当前仓库处于 **设计阶段**，文档即为可落地的架构蓝图。

已完成的设计冻结：

- Phase 1 内核接口（Object / Extent / Mapping / WAL / Transaction / Checkpoint / Reclaimer / Migration）→ [04-kernel-interfaces.md](docs/chatgpt/04-kernel-interfaces.md)、[ADR-0008](docs/adr/0008-kernel-interface-freeze.md)
- 故障注入夹具与不变量可执行化方案 → [05-kernel-test-harness.md](docs/chatgpt/05-kernel-test-harness.md)、[ADR-0009](docs/adr/0009-fault-injection-harness.md)
- 索引恢复的增量算法与回退代价模型 → [06-index-recovery.md](docs/chatgpt/06-index-recovery.md)（尚有几条待补的 ADR：`ChangeFeed` 入冻结清单、索引层写内核 WAL、两本日志的可见性对齐、索引段回收判定）
- 硬件资源管理的实现细节 → [07-hardware-resource-management.md](docs/chatgpt/07-hardware-resource-management.md)、[ADR-0010](docs/adr/0010-hardware-resource-management.md)（代价模型参数化、NUMA 亲和、队列深度与后端判据；尚有 `AccessPattern` / `ResourceStats` / `DeviceHealth` / `FaultDomainId` 入冻结清单、设备级注入点形状两条待补 ADR）

建议的下一步：

1. **夹具先行**：先实现假设备 + 注入器 + 一致性检查器，再动内核模块；
2. 用 Rust 实现 MVP：Object + Document + Vector、WAL + Checkpoint、MVCC、io_uring、HNSW + Metadata Index、Intent API；
3. 建立三类 Benchmark：基础存储、AI 工作负载、硬件效率；
4. 在第一版不实现：分布式事务、完整 GPU 后端、CXL、RDMA、自研模型——但预留扩展点。

**最终定位**：

> **AI as the Interface · Intent as the Execution Model · Object as the Data Model · Extent as the Storage Model · Hardware as the Execution Foundation.**
