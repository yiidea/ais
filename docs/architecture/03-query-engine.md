# 03 — 查询引擎：意图管道与模型规划器

## 1. 为什么是「意图」，不是 SQL

SQL 的核心价值是把「意图」压成「可验证的有界计划」，其代价是表达了大量与 AI 无关的语法、类型、集合语义。AI 负载的意图更接近：

```
我希望能在一周内，找到最近对「用户偏好」的可靠陈述；
来源必须是人工或高置信 Agent；
总上下文不得超过 512 token；
如有冲突，优先显示最新证据并标注冲突。
```

这段意图含**多个约束维度**（时间、置信、来源、token 预算、冲突语义），用 SQL 表达既冗长又会丢失约束。因此AINS 采用：

- **声明式意图**：结构化的字段 + 自由文本。
- **规划器**：把意图翻译成执行计划，而不是翻译成 SQL。
- **DSL**：执行计划用有界 DSL 表达，可验证、可缓存、可优化。

### 1.1 意图结构

```protobuf
message Intent {
  string kind            = 1;  // recall / write / traverse / watch / consolidate
  string about           = 2;  // 自然语言描述
  bytes  about_embedding = 3;  // 可选：已提供的查询向量
  repeated Constraint constraints = 4;
  Budget budget          = 5;
  OutputSpec output      = 6;  // 认知块格式、引用要求
}

message Constraint {
  oneof kind {
    TimeRange time = 1;
    double min_confidence = 2;
    string source_filter = 3;
    TagFilter tags = 4;
    string must_include = 5;
  }
}

message Budget {
  uint32 max_tokens = 1;
  uint32 max_latency_ms = 2;
  uint32 max_candidates = 3;
}
```

### 1.2 意图分类器

在进入规划器之前，先用轻量分类器把意图分成几类：

| 类型 | 示例 | 默认路由 |
|---|---|---|
| **Lookup** | 有 ID / 精确键 | 直接读对齐路径，不调用 LLM |
| **Recall** | 语义召回 | 稀疏+向量召回 → 融合 |
| **Traverse** | 溯源多跳 | 图遍历 + 过滤 |
| **Write** | 新事实、反馈 | 写路径 + 幂等检查 |
| **Watch** | 订阅变化 | 事件流 |
| **Consolidate** | 摘要/合并记忆 | 后台批处理 |

分类器本身可以用小型蒸馏模型（< 10 MB），也可用规则实现；这是避免「每次都调用大模型」的关键成本控制点。

## 2. 规划器接口

```text
interface Planner {
  Plan propose(Intent intent, PlanningContext ctx) -> Plan
}

PlanningContext {
  catalog:         // 可用域、索引、统计信息
  cost_model:      // 学习型代价预测
  budget:          // 延迟/算力上限
  history:         // 相似意图的历史计划（缓存）
}
```

可插拔实现：

- **LLMPlanner**：把意图 + 上下文摘要 + 可用算子列表 + 少量示例放入 prompt。
- **RulePlanner**：确定性规则，覆盖 Lookup/Write/Watch 等简单路径。
- **HybridPlanner**：先用 RulePlanner，若无法满足预算再升级到 LLMPlanner。
- **CachePlanner**：意图哈希命中历史计划直接返回。

### 2.1 规划上下文摘要

规划器的输入必须**有界**，否则随着数据增长 prompt 失控。摘要内容：

- 可用域 + 索引清单（名字 + 类型 + 覆盖范围，不含原始数据）。
- 统计信息：各索引的规模、延迟分布分位数、召回率。
- 历史计划：最近相同意图类型的 3 个成功计划。
- 硬约束：可用算子白名单。

### 2.2 输出：执行计划 DSL

规划器输出有界 DSL（不是代码，不是 SQL）。例如：

```yaml
plan:
  - op: sparse_recall
    domain: agent_memory
    query: "${intent.about}"
    k: 50
  - op: vector_recall
    domain: agent_memory
    vector: "${intent.about_embedding}"
    k: 50
  - op: fuse
    strategy: rrf
    inputs: [0, 1]
  - op: filter
    conditions:
      - field: confidence >= 0.7
      - field: time within "${intent.constraints.time}"
  - op: dedup
    strategy: semantic
    threshold: 0.92
  - op: pack
    budget_tokens: "${budget.max_tokens}"
  - op: attach_provenance
    depth: 1
```

### 2.3 验证器（Plan Verifier）

规划器输出在进入执行前必须通过：

1. **语法与相位**：DSL 合法，步骤引用正确。
2. **算子白名单**：只允许注册过的算子。
3. **资源上限**：总估算成本 ≤ 预算（token / 延迟 / 内存）。
4. **域权限**：调用方只能访问授权域。
5. **无副作用保证**：只读意图不允许产生写算子。
6. **死循环检测**：图遍历/流水线有深度上限。

任何一步失败 → 进入重规划或返回确定性的保守回退计划。

## 3. 学习型代价模型

### 3.1 代价分解

每次候选计划的代价由以下部分预测：

```
cost = w1 * predicted_p50_latency
     + w2 * predicted_p99_latency
     + w3 * predicted_token_cost
     + w4 * predicted_reranker_cost
     + w5 * predicted_cpu_cycles
```

### 3.2 训练数据

- 每个计划执行的历史滑动窗口：类型、参数规模、实际各阶段耗时、召回率、硬件桶（CPU 型号 / SSD 类型）。
- **上下文感知**：代价模型输入包括索引规模、碎片率、当前 IO 队列深度，而不只是计划形态。

### 3.3 冷启动策略

- 用硬件基准面板 + 静态公式做初始代价。
- 一段时间内建议规划器走保守路径（召回更多候选、再精排）。
- 反馈学习（用户点击/采纳率）作为弱监督。

### 3.4 多目标优化

AI 请求的优化目标不是单一的。可选目标：

- **Latency-first**：Agent 交互场景。
- **Recall-first**：知识库构建。
- **Token-first**：上下文稀缺场景。
- **Cost-first**：离线批处理。

意图可声明偏好，代价模型按偏好加权。

## 4. 执行运行时

### 4.1 计划 DAG

执行计划会被编译成 DAG：

- 算子间通过 **共享内存环（Shared Ring Buffer）** 传递批次，避免序列化。
- 每批为 **Batch<T>**（可变长），与硬件 IO 的异步完成直接对接。

### 4.2 执行算子清单

| 算子 | 说明 |
|---|---|
| `sparse_recall` | 文本倒排召回 |
| `vector_recall` | ANN 召回 |
| `timeline_scan` | 时间范围检索 |
| `graph_traverse` | 溯源图游走 |
| `filter` | 谓词过滤 |
| `fuse` | RRF / 加权融合 |
| `dedup` | 语义去重 |
| `rerank` | 重排序（可调用外部模型或引擎内轻量模型） |
| `summarize` | 子图摘要（可选，模型调用） |
| `pack` | 将候选打包成认知块，控制 token 预算 |
| `attach_provenance` | 附加引用来源 |
| `emit` | 输出结果 |

### 4.3 背压与流式

- 召回阶段产生流式候选，`pack` 阶段做 **滑动窗口选择**：当预算已满，后续候选只有显著优于当前最小密度时才替换（类似在线 top-k）。
- 支持 **流式返回**：先给用户一个高置信的认知块，后续结果追加。

### 4.4 执行计划的缓存与版本化

- 计划 DSL 与「索引版本 + 统计摘要」绑定。索引布局变化 → 缓存失效。
- 历史计划可用于在线 A/B 对比（不同策略在不同用户/域上）。

## 5. 反馈回路

每次查询返回后，应用可选择回传：

- **采纳**：本次返回被用于生成答案的条目。
- **修正**：错误/过时/冲突说明。
- **延迟与成本**：真实测量值。

反馈驱动：

1. 代价模型再训练。
2. 溯源置信度更新（被采纳的证据置信度提升）。
3. 与 `consolidate` 意图结合，将高价值认知块固化到更高层（见 05）。

## 6. 与 SQL 的兼容层

不是所有人都能立即抛弃 SQL。引擎提供一个 **SQL 兼容翻译层**：

- 把 SQL 查询翻译成对应的召回算子组合，作为「规则型意图」使用。
- 支持 `SELECT ... WHERE tags, vector <-> $1` 的 pgvector 式语法。
- 这是过渡层，不保证复杂 SQL 的完整语义。
