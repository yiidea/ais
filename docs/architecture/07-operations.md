# 07 — 运维、可观测性与演进

## 1. 可观测性

### 1.1 三大支柱

| 支柱 | 内容 |
|---|---|
| **指标（Metrics）** | 延迟分位数、IO 队列、索引召回率、token 预算达成率、冲突率、GC 速率 |
| **追踪（Tracing）** | 每次意图从分类→规划→执行→打包的完整链路（OpenTelemetry 原生） |
| **日志（Logs）** | 结构化事件：写入、合并、冲突、迁移、失败 |

### 1.2 AI 专属指标

- `intent_plan_latency_ms`：意图解析 + 规划耗时。
- `plan_fallback_rate`：回退到保守规则的比例。
- `recall_precision_at_budget`：在 token 预算内返回的采纳比例。
- `conflict_rate`：被识别为冲突组比例（高说明有事实漂移）。
- `freshness_lag_ms`：索引构建滞后。
- `consolidation_gain`：记忆固化带来的 token 节省。

## 2. 容量规划

- 存储估算：`对象数 × (平均 payload + 向量 + 索引摊销)`；索引摊销 = 向量索引约 1.4× 原始向量大小（HNSW），稀疏索引约 0.5× 文本大小。
- 内存估算：向量索引常驻内存 = `对象数 × dim × 量化字节`。例如 100 万对象 × 1024 维 × 1 字节 ≈ 1 GB 常驻。
- 写入吞吐：取决于 SSD 顺序写能力与索引合并策略。

### 2.1 自适应索引选择

引擎根据查询模式自动调优：

- 访问频率高的索引优先常驻。
- 若某个索引的查询量已低于成本，降低其构建频率或改为按需构建。
- 基于代价模型的规划器会自动从「召回优先」切换到「延迟优先」。

## 3. 资源治理

- 每个域配置 CPU / 内存 / IO 上限。
- 使用租约与令牌桶进行限流。
- `max_tokens` 预算保护上下文；`max_candidates` 保护 CPU；`max_latency` 保护 SLO。
- 超预算时：降级返回（较少候选，标注「结果被截断」）。

资源域的容量、占用、预留、NUMA 归属与健康状态来自 `MemoryTierManager::stats(resource)`；该快照的类型定义见 [07-hardware-resource-management.md §2.3](../chatgpt/07-hardware-resource-management.md)。放置侧如何用这些量做决策见同文 §2.4。

## 4. 安全与合规

- **加密**：传输 TLS 1.3；静态加密 AES-256-GCM（密钥管理器托管）。
- **访问控制**：域级 + 标签级；支持基于能力的令牌（capability token）。
- **审计**：所有写入和溯源变更写入不可变审计日志，独立于业务日志。
- **数据主权**：域可配置物理位置（区域级），引擎按域路由，不在域间混合数据。
- **删除权**：对象删除包括正本 + 索引 + 缓存三层清理；溯源中保留墓碑记录但擦除内容。

## 5. 部署形态

| 形态 | 场景 | 说明 |
|---|---|---|
| **嵌入式** | 单个 Agent 本地 | 一个静态库 + 本地目录 |
| **单机服务** | 团队 / 小集群 | systemd / 容器，本地 NVMe |
| **分布式集群** | 平台 / 多租户 | Meta Plane + 数据节点 |
| **边缘/混合** | 卫星 Agent | 边缘本地副本 + 中心同步（Eventual） |

## 6. 运维操作手册（要点）

### 6.1 常规操作

- **索引重建**：`sse index rebuild <domain> <index>`，后台执行，不阻断读。
- **记忆固化**：`sse consolidate <domain> --since <time>`，建议在低峰期执行。
- **溯源校验**：`sse provenance verify <domain>`，检查签名与 Merkle 完整性。
- **冲突处理**：`sse conflict list <domain>` → `sse conflict resolve <id> --strategy latest|highest|manual`。

### 6.2 故障处理

- **规划失败**：观察 `plan_fallback_rate`，检查规划器服务可用性；回退到规则规划不会阻断。
- **索引滞后**：`freshness_lag_ms` 上升 → 检查后台构建队列 / IO 竞争。
- **延迟尖刺**：硬件 Profile 自动降级；或临时开启「保守模式」（更少候选、更小 batch）。

### 6.3 内核错误 → 运维动作映射表

内核错误类型由 [04-kernel-interfaces.md §2](../chatgpt/04-kernel-interfaces.md) 冻结（[ADR-0008](../adr/0008-kernel-interface-freeze.md)）。本表把每个变体映射到「自动动作 / 告警级别 / 是否需要人工介入」，供 SDK 与值班手册直接引用。

| 错误 | 内核已做的 | 自动动作 | 告警 | 人工介入 |
|---|---|---|---|---|
| `NotFound` | 无 | 无 | 无 | 否（业务语义） |
| `AlreadyExists` | 无 | 无 | 无 | 否 |
| `Conflict(WriteConflict)` | Wait-Die 判定，事务中止 | 上层工作流可重试整个流程 | `conflict_rate` | 否（持续偏高则调优） |
| `Conflict(MappingChanged)` | 迁移放弃，源版本不变 | Placement 退避后重排 | 无 | 否 |
| `Conflict(IntentConflict)` | 无 | 由冲突查询 API 显式暴露 | `conflict_rate` | 否（由业务决定取舍） |
| `Retryable(DeviceBusy)` | 无 | 上层指数退避 | IO 队列深度 | 否 |
| `Retryable(Backpressure)` | 无 | 上层降低并发 | 背压指标 | 否 |
| `Retryable(ResourceBusy)` | 拒绝开始新事务 | 等待长事务结束 | 长事务数告警 | 是（长事务通常是应用问题） |
| `Corrupted(WalRecordCrc)` | 扫描终止于损坏点 | 触发恢复，隔离损坏段 | P1 | 是 |
| `Corrupted(CheckpointManifest)` | 回退到上一个 checkpoint | 从 WAL 重放 | P1 | 是 |
| `Corrupted(ChecksumMismatch)` | 拒绝发布该版本 | 标记对象不可读 | P1 | 是 |
| `Corrupted(VersionChainBroken)` | 恢复中止 | 从上一个 checkpoint 重放 | P0 | 是 |
| `Corrupted(DirectoryDangling)` | 校验失败 | 记录并跳过 | P1 | 是 |
| `ResourceExhausted` | 无 | 触发 GC 或扩容 | 容量水位 | 否（长期则扩容） |
| `Unsupported` | 无 | 走降级路径 | 无 | 否 |
| `Io{op}` | 设备标记降级 | 按 `op` 分派，见下表 | P0 / P1 | 视 `op` |
| `Internal` | 模块层返回，公共 API 边界转成 panic / abort（[04 §2.3 C4](../chatgpt/04-kernel-interfaces.md)） | 收集现场 dump | P0 | 是（内核 bug） |

`Io` 按操作类型的处置差异很大，值班时按 `op` 分派。「上层可重试」列指**上层能否重新发起同一逻辑操作**，不是指内核会不会自动重试——内核在任何情况下都不自动重试（[04 §2.4 C1](../chatgpt/04-kernel-interfaces.md)）。

| `op` | 含义 | 处置 | 上层可重试 | 对外承诺 |
|---|---|---|---|---|
| `Read` | 读失败 | 返回错误；**禁止**返回脏数据或静默跳过 | 是（读是幂等的，退避后重发） | 无（未读到即无承诺） |
| `Write` | 写失败 | 事务中止，未提交版本不可见 | 是，但只能以**整个事务**为单位重发（半帧由 CRC 检出并跳过） | 无 |
| `Flush` / `Fsync` | 落盘失败 | **提交失败**：未返回 Commit OK 的事务不算成功 | 否（重试落盘修不好已经撕裂的帧；正确做法是让事务整体失败、由上层重新提交） | 无 |
| `Rename` | 检查点发布失败 | 保留上一个完整检查点，后台重试 | 否（rename 不可重放；重做的是整个发布步骤，由内核后台流程负责） | 无 |
| `Allocate` | 空间分配失败 | 触发 GC / 扩容 | 否 | 无 |
| `Discard` | 空间归还失败 | 仅影响空间回收率，不影响正确性 | 否（重复归还无意义，回收由 GC 流程重试） | 无 |

关键口径：**只有返回了 Commit OK 的事务才有「已持久」的承诺**。任何 `Io` 出现在提交路径上，都意味着该事务没有对外承诺成功，不需要也不应该做数据补偿。

另一条口径：`Io` 永远不被改写为 `Retryable`（[04 §2.4](../chatgpt/04-kernel-interfaces.md)）。`Io` 表示「这次设备操作失败了」，`Retryable` 表示「内核判定这是资源级瞬时故障」；`TransactionManager::commit` 的 WAL 落盘失败必须以 `Io{op: Flush | Fsync}` 原样上抛，上层才能区分「盘掉了」和「暂时挤」。

配套指标：

```
kernel_error_total{variant}          按变体计数
kernel_io_error_total{op,device}     按 op 与设备计数
kernel_conflict_total{kind}          按冲突类型计数
kernel_recovery_duration_s           恢复耗时（对应 §6 的恢复时间目标）
wal_durable_lag_bytes                WAL 持久水位落后量（判断设备是否拖慢提交）
```

## 7. 测试策略

- **功能一致性**：所有硬件回退路径必须通过同一套语义测试。
- **性能基准**：固定硬件面板（不同 SSD / CPU）输出可对比的基准报告。
- **故障注入**：断电、IO 延迟注入、节点失联、日志损坏。
- **语义正确性**：随机生成意图与对象，断言因果可见性、幂等性、溯源完整性。
- **对抗测试**：提示注入污染存储后是否能被隔离；伪造溯源签名能否被拒绝。

内核层的故障注入与不变量断言由 [05-kernel-test-harness.md](../chatgpt/05-kernel-test-harness.md) 落地：假设备 + 注入器 + 一致性检查器，覆盖 [03 §11](../chatgpt/03-consistency-kernel.md) 的 I1–I10 注入矩阵。

### 7.1 测试债务清单

[03 §9.3](../chatgpt/03-consistency-kernel.md) 要求「暂不支持」与「实现了但有 bug」在 CI 上可区分。第一版明确**不实现**的部分各有一条 xfail 测试，风险描述与 [03 §9.2](../chatgpt/03-consistency-kernel.md) 原文一致：

| 债务 | 出处 | 替代方案 | 风险 | xfail 用例 | 解除条件 |
|---|---|---|---|---|---|
| Undo 日志 | 03 §9.2 | 依赖 MVCC 可见性 + GC 清理 | 恢复后空间暂时未回收，需 GC 补偿 | `xfail_recovery_no_undo_space_reclaimed` | 引入 undo 日志，或 GC 补偿路径稳定 |
| 事务组提交 | 03 §9.2 | 每 commit 单独 flush | 吞吐受限，但语义正确 | `xfail_group_commit_throughput` | 实现 group commit |
| 死锁检测 | 03 §9.2 | Wait-Die | 偶发的事务中止需要应用层重试 | `xfail_deadlock_detected_not_reported` | 引入死锁检测 |
| 增量迁移同步 | 03 §9.2 | M6 版本检查失败即放弃 | 热点数据迁移成功率下降 | `xfail_incremental_migration_sync` | 实现增量同步 |
| 分布式副本仲裁 | 03 §9.2 / §10 | 单机单副本 | 无法应对单盘物理损坏 | `xfail_replica_arbitration` | 实现副本仲裁 |

xfail 用例的含义是「**已知会失败，且失败原因必须是预期的那一条**」。若 xfail 用例因为别的原因失败（例如断言写错导致 panic），CI 仍然失败——这是 xfail 相对于「注释掉这个测试」的关键区别。

**解除条件在每次发布时逐条复核**：CI 只保证 xfail 用例「以预期原因失败」，不负责判断债务是否已解除——那是发布清单上的人工检查项。债务解除后必须同步删除对应的 xfail 用例，否则这条用例会长期以「预期失败」通过，掩盖同名的真实回归。

## 8. 演进路线（Roadmap）

### v0 — 设计验证（当前）
- 文档、原型接口、HAL 骨架、本地 Append-Only 日志原型。

### v1 — 单机可用
- 语义对象日志 + 向量/稀疏/时间线三类索引。
- 规则规划器 + 意图 DSL。
- io_uring 读路径（回退 POSIX）。
- 溯源基础、因果快照、语义幂等。

### v2 — AI 特性完整
- LLM 规划器 + 学习型代价模型。
- 溯源图可信度传导、语义 GC、记忆固化。
- 注意力感知返回（认知块 + 密度打包）。
- 主从复制。

### v3 — 分布式
- Meta Plane + 语义路由 + 虚拟分片重平衡。
- 三级一致性模型、因果租约。
- 分布式索引聚合、布隆过滤去重。
- 性能目标：Recall 路径 < 30 ms P99，冷读 < 10 µs。

### v4 — 硬件前沿
- SPDK 队列、CXL 内存分层、GPU 向量批处理。
- 多区域拓扑与数据主权路由。

## 9. 开源与生态

- **协议**：Apache-2.0。
- **SDK**：Rust 核心 + Python/TypeScript/Go 绑定。
- **集成**：LangChain / LlamaIndex / MCP server 适配器，使任意 Agent 框架可直接使用。
- **插件**：规划器、reranker、编码器都可以作为插件注册。

## 10. 成功判据

- 单一 SDK 同时支持「本地嵌入」与「分布式集群」，切换零成本。
- 在真实 Agent 记忆场景中，相比 PostgreSQL+pgvector 减少 ≥ 60% 的 token 消耗（相同任务完成率）。
- 冷读 P99 达到 NVMe 理论值的 70% 以上。
- 溯源可回答「这条记忆从哪来、被谁修改、是否还成立」。
