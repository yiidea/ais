# 01 — 总体架构与设计目标

> **阅读顺序**：主设计文档 [../chatgpt/01-overview.md](../chatgpt/01-overview.md)（二十一章全景）与 [../chatgpt/02-kernel.md](../chatgpt/02-kernel.md)（内核三层深入）是权威来源；本目录 01–09 是同一套方案在**实现层面的展开与细化**：接口、不变量、指标、回退策略、ADR。两份文档冲突时，以主设计文档的原则 + 本目录的 ADR 为准（ADR 记录了取舍理由）。

## 0. 本文与主设计文档的术语对齐

| 本文（实现层） | 主设计文档（原始设计层） | 说明 |
|---|---|---|
| Semantic Object（语义对象） | AIObject + Payload 变体 | 同一概念：统一头 + 七类载荷 |
| Domain（数据域） | Namespace / ObjectType 作用域 | 逻辑隔离单元 |
| Semantic Log（语义日志） | WAL + Object Store | 追加日志为唯一真源，对象由日志派生 |
| Intent + 执行计划 DSL | Intent IR + Task DAG | DSL 是 DAG 的序列化形式，二者等价 |
| Index Mesh | 多模态索引（Vector / Graph / 混合） | 并列多索引，共用真源 |
| 认知块（Chunk） | Context 聚合结果 | 按 token 预算打包的返回单元 |
| 三层内存分层 | Memory Tier Manager（DRAM/CXL/HBM/NVMe/HDD） | 本文补充了 CXL/HBM 的落地细节 |
| 三级一致性 | Storage Txn（ACID）+ Workflow Txn + 因果语义 | 见 §6 与 ADR-0005 |

## 1. 设计目标

| 编号 | 目标 | 可度量指标（v1 目标 → v3 目标） |
|---|---|---|
| G1 | **语义写入**：无 Schema，原文+向量+溯源一体写入 | 单次语义写 < 200 µs（4 KB） → < 50 µs |
| G2 | **意图查询**：用「我想…」+ 约束代替 SQL | 意图→可执行计划 < 150 ms（本地小模型） → < 40 ms |
| G3 | **硬件直通**：绕过内核页缓存和通用 VFS | 冷读 P99 < 30 µs（NVMe） → < 10 µs |
| G4 | **自适应索引**：系统决定建什么索引、怎么建 | 写入放大 < 2.5× → < 1.5× |
| G5 | **可溯源**：所有字节可回溯到来源 | 溯源图查询 < 10 ms（深度 5） |
| G6 | **注意力友好**：结果按「认知块」返回 | 命中率 ≥ 90% @ 512 token 预算 |
| G7 | **线性扩展**：分布式水平扩展，不阻塞单体 | 3 节点 3 倍吞吐，P99 抖动 < 20% |
| G8 | **自治运维**：自学习调优、自愈合 | 无需人工调参即可维持 SLA |

## 2. 非目标

- **不是通用 OLTP 数据库**：不追求 TPC-C 级别的精确事务吞吐与强约束；但**绝不放弃 ACID**——底层 Storage Transaction 仍是 MVCC + WAL + Object Versioning（见 §6 与 ADR-0005）。
- **不是向量库的薄封装**：向量只是索引网格中的一种索引，不是核心存储。
- **不替代应用逻辑**：引擎不负责编排 Agent 的推理循环，只提供存储与召回原语。
- **不做图灵完备的查询语言**：意图 DSL 保持有界、可验证，规划器不能执行任意代码。
- **不让 LLM 进入 I/O 路径**：模型只出现在意图理解、复杂规划、工作负载学习三处；块级读写、索引访问、事务、I/O 调度全部由确定性内核完成。
- **不在 v1 支持分布式事务**（见 §6、ADR-0005 与 Roadmap）。

## 3. 为什么 AI 负载与关系型负载的本质不同

| 维度 | 关系型负载 | AI 负载（Agent / RAG / 记忆） |
|---|---|---|
| 数据形态 | 结构化、严格要求类型 | 半结构化 + 语义 + 多模态 |
| 写入模式 | 事务内小批写 | 流式追加、高并发、并发写入 |
| 查询 | 精确谓词、聚合 | 语义相似、多跳、开放式召回 |
| 结果形态 | 行、列、聚合值 | 上下文块、证据、引用 |
| 一致性需求 | 强 ACID | 因果一致、最终收敛、冲突需解释 |
| 失败模式 | 锁等待、死锁 | 幻觉写、语义重复、偏差漂移 |
| 最有价值的信息 | 存在性与唯一性 | 来源、时间、为何如此 |

**结论**：AI 负载的核心张力在于「模糊性与可审计性同时存在」。AINS 用「无模式对象存储 + 多索引并行 + 溯源图」同时承认这两点，而关系型数据库把它俩都压扁了。

## 4. 四平面架构（与六层架构的映射）

AINS 在实现上分为四个平面，每个平面有清晰的接口边界，可独立演进与替换；它们与主设计文档的六层一一对应：

| 本文平面 | 主文档层次 |
|---|---|
| API 平面 | 1. AI Interface Layer |
| 规划平面 | 2. AI Intelligence Layer（含 Policy 校验） |
| 执行平面 | 3. Semantic Data Layer + 4. Execution Engine |
| 存储平面 | 5. Hardware-Aware Storage + 6. Hardware Abstraction |

### 4.1 API 平面

负责与外部 Agent、应用、SDK 交互。

- **Intent Submit**：接受「意图 + 约束」结构（文本、embedding、结构化参数字段混合）。
- **Pipeline Builder**：把多意图串成流水线（写→读→反馈）。
- **Schema-Free Templates**：可选模板，仅用于提示与索引优化，不参与强制约束。
- **协议**：gRPC + JSON over HTTP + 原生 SDK（Python / Rust / TS）。

### 4.2 规划平面

引擎的「大脑」，把意图翻译成执行计划。

- **Planner Interface**：统一接口，任何规划器都能插拔。
- **LLM Planner**：将意图 + 数据分布摘要 + 代价模型提示放入 prompt，输出执行计划 DSL。
- **Rule Planner**：确定性回退，覆盖常见意图（精确召回、时间范围、标签过滤）。
- **Learned Cost Model**：离线用历史执行数据训练，预测候选计划的代价。
- **Plan Verifier**：对 DSL 进行静态检查（无危险操作、资源上限、白名单算子），再放入沙箱执行。

### 4.3 执行平面

- **Execution Runtime**：DAG 调度器，面向批 + 流混合。
- **Retrieval Services**：向量检索、稀疏文本检索、图检索、标量过滤并行执行。
- **Reranker / Dedup**：语义去重与重排序。
- **Semantic Join / Fusion**：多路召回结果融合成认知块。

### 4.4 存储平面

- **Semantic Router**：根据意图的 embedding + 元数据决定数据域与物理分片。
- **Layer Tiering**：热层（HBM/DRAM）、温层（NVMe）、冷层（对象存储）。
- **Object Store**：Append-Only 语义对象日志（核心持久化）。
- **Index Mesh**：多索引并行更新，索引本身也是存放于 Object Store 中的对象。
- **Hardware Adapter**：io_uring / SPDK / 未来 CXL。

## 5. 关键路径：一次「回忆」的完整旅程

```
用户/Agent: recall "用户的 UI 偏好"  budget=512 tokens
      │
      ▼
[API] 构造 Intent {about, constraints}
      │
      ▼
[Planner] 小模型生成执行计划：先稀疏召回，再向量召回，去重，rerank
      │
      ▼
[Verifier] 检查算子白名单 & 资源上限 → 通过
      │
      ▼
[Runtime] 并行调度 3 个 Retrieval Services
      │         │
      │         ├─ Host: io_uring 读热点索引（命中 LSM memtable）
      │         ├─ NVMe: 异步读向量图
      │         └─ NVMe: 读时间线 + 溯源
      ▼
[Fusion] 合并、去重、按注意力密度排序 → 选 512 token 的认知块
      │
      ▼
返回：认知块 + 置信度 + 溯源引用
```

## 6. 一致性模型分层

AINS 不提供单一的一致性等级，而是提供服务端可选的三个等级：

| 等级 | 语义 | 适用 |
|---|---|---|
| **Causal**（默认） | 单个 Agent 内因果序 + 跨 Agent 最终收敛 | Agent 记忆、RAG |
| **Strong** | 线性一致，写入可见 | 策略库、凭证、配置 |
| **Eventual** | 最终一致，可容忍冲突 | 多模态大对象、日志 |

详见 [06-distributed.md](06-distributed.md) 与 ADR-0005。

## 7. 与常见方案的对比

| 需求 | PostgreSQL+pgvector | Milvus | AINS |
|---|---|---|---|
| 语义写入 | 需强 schema + 额外 vector 列 | 需 collection schema | 原生 |
| 意图查询 | 需手写 SQL + 应用层编排 | 需手写检索流程 | 模型即规划器 |
| 硬件直通 | 否 | 部分 | 是 |
| 溯源 | 需自建 | 无 | 内置 |
| 意图的并发控制 | 事务 | 无 | 意图级 MVCC |
| 本地单机部署 | 较重 | 重 | 轻 |

## 8. 架构决策记录索引

| ADR | 标题 |
|---|---|
| 0001 | 无模式语义对象 vs 强 schema |
| 0002 | 模型作为查询规划器 |
| 0003 | Append-Only 日志作为唯一真源 |
| 0004 | 绕过内核：io_uring + SPDK 双通道 |
| 0005 | 三级一致性模型 |
| 0006 | 索引网格而非单一索引 |
| 0007 | 认知块作为返回单位 |
| 0008 | 内核接口冻结与统一错误模型 |
| 0009 | 故障注入夹具与不变量可执行化 |
