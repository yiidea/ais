# 02 — 存储引擎设计

> 对应主设计文档：第三章（数据模型）、第五章（硬件直通存储）、第八章（事务）、第九章（持久化）、第十一章（安全边界）与内核文档《02-kernel》第二、三部分。
> 本章在「Object → Logical Extent → Physical Extent → Physical Block」四层结构上做实现级展开。

## 0. 四层数据组织（内核）

```text
AIObject（逻辑身份 + 统一头 + 七类 Payload）
   │ 1 : N （对象可被切成多个逻辑块，例如大文档分块、Tensor 分片）
   ▼
Logical Extent（逻辑存储单元：连续、可压缩、可迁移、与硬件无关）
   │   Placement Manager 负责「应该放在哪一层」
   ▼
Physical Extent（物理存储单元：DRAM / CXL / GPU HBM / NVMe / HDD 上的连续区间）
   │
   ▼
Physical Block（硬件访问单元：4 KB / 64 KB / GPU 传输粒度，由硬件 Profile 决定）
```

### 0.1 不变量（Invariants）

内核实现必须始终满足以下不变量，测试以它们为断言：

| 编号 | 不变量 | 检查手段 |
|---|---|---|
| I1 | 每个已提交对象至少映射到一个可读 Physical Extent | 崩溃恢复后全量校验 |
| I2 | 提交顺序 = 日志顺序 = 版本顺序；不存在「对象已提交但索引永久缺失」 | LSN 水位检查 |
| I3 | 逻辑映射与物理映射严格分离：数据迁移不改变对象身份与版本 | 迁移测试（见 04 §3） |
| I4 | 索引可见性由 LSN 水位决定，而非构建完成时间 | 查询可见性测试 |
| I5 | 所有持久化写入先落 WAL/Semantic Log，再改内存索引 | 故障注入 |
| I6 | DRAM / GPU HBM 永远不是最终持久存储 | 设计评审 + 断电测试 |

### 0.2 Extent 尺寸与切分

| 数据类型 | 默认 Extent 尺寸 | 切分方式 |
|---|---|---|
| Document | 64 KB ~ 1 MB | 按语义块（段落/函数）切分，便于召回与摘要 |
| Vector | 4 KB（块量化，16 个向量/块） | 定长，便于 SIMD 与索引随机读 |
| Graph | 1 MB 图段（节点表 + 边表 + 邻接偏移） | 按社区/分区切分，便于图遍历局部性 |
| Tensor | 2 MB ~ 64 MB | 按 Tensor 分片；大块顺序布局，适配 GPUDirect |
| Memory | 4 KB ~ 64 KB | 按事件/记忆条目 |
| Media | 1 MB ~ 64 MB | 按文件块；元数据与内容分离 |
| Event | 64 KB | 追加即可，按时间窗切分 |

默认尺寸由硬件 `optimal_io_size` 与负载学习结果联合决定（见 04 §2.1）。

## 1. 语义对象模型

### 1.1 核心对象：SemanticObject

与主设计文档的 `AIObject` 头一一对应：`id / type / namespace / version / timestamps / flags / storage_ref / index_ref / context_ref / security_ref`。实现层用一份带 `payload_kind` 的具体化消息承载七类载荷：

```protobuf
message SemanticObject {
  bytes  id            = 1;   // ULID，时间有序 + 随机后缀
  uint64 epoch         = 2;   // 单调递增的逻辑时钟，由存储层分配
  ObjectKind kind      = 3;   // VECTOR/GRAPH/TENSOR/DOCUMENT/MEMORY/MEDIA/EVENT
  bytes  payload       = 4;   // 原始内容（不透明；Document 文本 / Tensor 分片 / Media / …）
  bytes  embedding      = 5;  // 压缩向量（见 §3.4），可空
  repeated Tag tags    = 6;   // 稀疏属性（键可重复、值可为结构体）
  Provenance provenance = 7;  // 溯源链（见 05-ai-native-features.md）
  bytes  semantic_key  = 8;   // 幂等键：内容指纹 + 语义标签蒸馏
  uint32 flags         = 9;   // 加密 / 压缩 / 冷热标记
  repeated ExtentRef extents = 10; // → Logical Extent → Physical Extent
}
```

设计要点：

- **ID 时间有序**：ULID 前缀是毫秒级时间，天然适合时间线检索，也适合分层存储的「时间路由」。
- **payload 完全不透明**：引擎不需要理解内容；所有语义分析都由索引构建器 / 规划器消费。
- **embedding 可空**：文本索引、标量检索、图检索不依赖 embedding，embedding 只是「又一种索引维度」。
- **tags 是稀疏字典**：键不预先声明；同一键可以出现多次（表示多值或冲突，由冲突策略决定）。
- **provenance 必填**：所有对象都必须能回答「这是谁写的、基于什么写的」。

### 1.2 数据域（Domain）

数据域是逻辑隔离单元，相当于「命名空间 + 物理路由桶」。每个域有自己的索引网格、保留策略、规划器偏好。域之间没有强 schema 约束，但可以声明可选模板以优化索引布局。

```protobuf
message Domain {
  string  name          = 1;
  uint32  shard_count   = 2;
  Retention retention   = 3;
  repeated IndexSpec indexes = 4;  // 可在任意时刻增删，异步构建
  string  planner_hint  = 5;       // 优化器提示：偏好本地小模型/远程模型
}
```

## 2. 存储分层：Append-Only 语义日志 + 多索引

### 2.1 唯一真理源：Semantic Log

所有写入先形成一条**语义日志记录（Semantic Record）**，追加到日志中。日志是唯一的持久化真理源；所有索引都是日志的派生视图，可随时重建。

```
Semantic Log (按 shard 分段的追加日志)
┌─────────────────────────────────────────────┐
│ Segment 0000 │ Segment 0001 │ Segment 0002… │
│  对象1、对象2、…（连续追加）                    │
└─────────────────────────────────────────────┘
          │ 派生（异步，可并行）
          ├──► 向量索引（HNSW/IVF-PQ/自研SIE）
          ├──► 稀疏文本倒排
          ├──► 时间线索引
          ├──► 溯源图索引
          └──► 标量/标签索引
```

优势：

- **写入路径极短**：只需追加一段变长记录 + 一个 epoch 分配。
- **崩溃恢复简单**：校验 CRC → 重建索引即可。
- **索引独立演进**：新增索引只需从日志回放构建，不影响写入路径。
- **多副本复制**：复制日志而非复制索引，节省带宽。

### 2.2 记录布局

```
+--------+--------+--------+--------+--------+--------+-------+
| magic  | len    | crc32c | epoch  | flags  | payload|  ...  |
|  4B    |  4B    |  4B    |  8B    |  2B    |  var   |       |
+--------+--------+--------+--------+--------+--------+-------+
```

- 变长记录、对齐到 4 KB 块边界（可配置 64 KB 大块，减少 SD 卡写放大）。
- 支持 **原子追加**：io_uring 的 `IORING_OP_WRITE` 一次提交整个记录，保证记录级原子性。
- 记录里的 payload 已含 embedding / tags / provenance；不分离存，保持「单记录即完整对象」。

### 2.3 Memtable + 不可变段

日志写入时同时维护一个内存表（Memtable，见 §3.3）。当 Memtable 达到阈值，冻结并写入新段，后台异步刷写。

## 3. 索引网格（Index Mesh）

### 3.1 索引即对象

每个索引是一组段文件（Segment），段文件也存放在 Object Store 中。索引元数据（段列表、构建水位）保存在每个域的小型协调区。

### 3.2 索引类型

| 索引 | 用途 | 构建方式 | 查询方式 |
|---|---|---|---|
| Vector HNSW | 语义近邻 | 增量，后台合并 | 近似 kNN |
| Vector IVF-PQ | 大规规模低内存 | 批量训练 | 粗量化 + 精排 |
| Sparse Inverted | 关键词 + BM25 | 增量 | 倒排 + BM25 |
| Timeline | 时间点 / 范围查询 | 追加即有序 | 跳表 / 二分 |
| Provenance Graph | 溯源多跳 | 异步图合并 | BFS / 图游走 |
| Scalar/Tag | 等值 + 范围 | 增量 | LSM 有序 |
| Semantic Fingerprint | 语义幂等检测 | 增量 | LSH / 局部敏感哈希 |

### 3.3 Memtable 与索引合并策略

- 每个索引有独立的 Memtable，按写入速率动态扩容。
- 达到阈值后冻结为不可变内存段（Immutable Memtable），再批量写入。
- **稀疏性自适应**：引擎统计每个索引的更新频率，决定哪些索引应当合并构建（降低随机 IO），哪些应增量维护（降低读延迟）。

```text
写入 → 全局 SemLog 追加
         ├─ 向量 Memtable（按 shard）
         ├─ 文本倒排 Memtable
         └─ 标签 LSM Memtable
              │ 冻结 + 合并（LSM 风格）
              ▼
         层 0 段 → 层 1 段 → … → 由容量触发的全局重建
```

### 3.4 向量索引与压缩

- 默认 **1024 维 float16 → PQ(int8)** 的混合表示；可配置。
- 存储侧对向量按 **块量化**（block-wise quantization），每个 4 KB 块包含 16 个向量，独立缩放因子。
- 查询时使用同一硬件向量指令（AVX-512 / NEON / AMX），避免解码开销。
- 早期版本优先实现 HNSW（较简单、可增量），后续演进到语义感知的 SIE（Semantic Index Engine，按 embedding 分布自动分区）。

### 3.5 多模态扩展

embedding 字段是变长字节数组，索引层面按 `modality` 标签区分（文本 / 图像 / 音频）：同一向量空间或不同向量空间由域配置决定。这样，多模态并不需要一个独立的存储引擎。

## 4. MVCC：从「行版本」到「意图版本」

传统 MVCC 是「行级快照隔离」；AINS 的并发可见性语义围绕 **意图** 而不是页面：

- 每次写入一个语义对象，携带 `epoch`。
- 读者拿到一个 **IntentSnapshot**（起点 epoch + 已知收敛集）。
- **因果可见性**：一个读取只能看到「对该 Agent 因果先序的写入」，而不是全局最新。
- 如果读取方是另一个 Agent，则用传播进来的因果令牌决定可见集。

详见 [05-ai-native-features.md §2](05-ai-native-features.md)。

## 5. GC 与保留策略

- 对象级保留：按域配置，可写 TTL（例如记忆域 90 天）、按引用计数（被某个证据链引用则不删）、按热度。
- **语义 GC**：对「语义等价且低置信度」的对象合并或淘汰（如两个重复事实）。判定标准可配置。
- **段级 GC**：段内所有对象过期 → 整段删除，避免细粒度随机 IO。
- 后台垃圾回收器与前台写入解耦，优先删除旧冷段。

## 6. 现场重建（Recovery）

1. 加载各 shard 的最新日志段并校验 CRC。
2. 对每个索引加载已持久化的段元数据，记录其水位。
3. 从日志水位之后回放，增量构建缺失索引。
4. 完成前，查询走回退路径（全量扫描 / 稀疏检索），不阻塞外部服务。

## 7. 存储层接口（对外暴露的原子操作）

```text
append(domain, object) -> id, epoch
get(domain, id) -> object
scan_timeline(domain, from, to) -> iterator
search_vector(domain, query_emb, k, filter) -> candidates
search_sparse(domain, terms, k) -> candidates
traverse_provenance(domain, id, depth, direction) -> graph
upsert_semantic(domain, fingerprint, object) -> id | conflict
```

## 8. 存储格式细节与安全

- **完整性**：每条记录 CRC32C，段级 Merkle 哈希，便于检测损坏与重放攻击。
- **加密**：对象可标记加密；引擎把加密算法和密钥 ID 放入 flags，加密由存储层完成（见安全章节）。
- **不可变性**：已落盘记录不可变，写入的是新 epoch 的新记录。审计友好。
- **来源完整性**：provenance 链中记录签名（签名可由写入 Agent 提供，引擎做验签），防止伪造来源。
