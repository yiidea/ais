# 06 — 索引恢复的增量算法

> 前置：[03-consistency-kernel.md](03-consistency-kernel.md)（§6.1 恢复流程第 9–10 步、§5.3 WAL 截断边界）、[02-kernel.md](02-kernel.md) §6.5（索引恢复与对象恢复分离）、[04-kernel-interfaces.md](04-kernel-interfaces.md)（内核接口冻结）、[ADR-0003](../adr/0003-append-only-log-source-of-truth.md)（日志是唯一真源）。
> 本文解决 [03 §12 第 2 项](03-consistency-kernel.md)：「日志水位 → 索引构建水位的追赶策略，以及『滞后索引的查询回退路径』的代价估计」。

## 一、问题与边界

### 1.1 四条水位线

「水位」在 AINS 里不是一个概念，而是四条独立的线。它们的推进节奏不同，也不同步——把它们混为一谈是这一块最容易出错的地方。

| 水位 | 含义 | 单位 | 由谁推进 | 存放在哪 |
|---|---|---|---|---|
| `durable_lsn` | 内核 WAL 已持久化到的位置 | LSN | 内核 WAL | WAL Manager 内存状态（段头只记 `segment_start_lsn`） |
| `object_watermark` | 对象层已应用到 WAL 的哪个位置 | LSN | 内核恢复与运行时 | 检查点元数据 + 内存 |
| `index.watermark` | **每个索引各自**声明的「我已消费到这里」 | LSN | 索引构建器 | 域协调区的索引元数据 |
| `index.txn_watermark` | **每个索引各自**声明的「我已消费完 xid ≤ X 的全部事务的对象变更」 | xid | 索引构建器 | 域协调区的索引元数据 |

前三条是 **LSN 水位**，第四条是 **xid 水位**。这个区分不是术语洁癖，而是正确性的关键：

- **LSN 水位回答「追赶落后多少」**——运维指标、段回收边界、追赶扫描的起点，都用它；
- **xid 水位回答「某个快照的结果是否完整」**——只有它能用于正确性判定。

**为什么不能用 LSN 判定快照完整性**：[03 §1.1 R3](03-consistency-kernel.md) 规定「可见性判定只使用 xid + txn status（以及快照），不使用 VersionId / LSN 大小」，[03 §3.3](03-consistency-kernel.md) 的 `visible()` 实现也只看 `xmin` / `xmax` / `active_xids` 与事务状态数组；[04 §14.3](04-kernel-interfaces.md) 把这条列为「已冻结、不得单方面变更的语义」。快照本身（[02-kernel §4.3](02-kernel.md)）里**没有 LSN 字段**，只有 `xmin` / `xmax` / `active_xids` / `snapshot_id`。

因此：**快照与索引的覆盖关系必须按 xid 判定，不能按 LSN 判定**。用 LSN 近似可见性，正是 R3 所说的「用序号近似可见性的优化」，必须被代码审查拒绝。

LSN 侧的滞后量有两个单位，名字不能混用：

```
lag_bytes(index)   = object_watermark - index.watermark    // 字节差，仅用于排序与展示
lag_records(index) = 该区间内的对象变更条数                   // 代价模型用（见 §3.2）
```

`Lsn` 是 WAL 逻辑字节偏移（[04 §2.1](04-kernel-interfaces.md)），所以 `lag_bytes` 是字节差；§1.3 讨论的「滞后 1 条 vs 滞后 100 万条」说的是条数，两者不是同一个量。

xid 侧的覆盖判定（正确性口径）见 §3.1。

### 1.2 索引不属于内核

[04 §1](04-kernel-interfaces.md) 把「索引网格」明确划在上层：「上层 Intent 执行器、索引网格、Agent 运行时都可以演进，唯独内核的接口一旦被依赖，就不能再随手改」。冻结清单（[04 §1.2](04-kernel-interfaces.md) 的 F1–F21）与实现顺序表（[04 §15](04-kernel-interfaces.md)）里**都没有索引管理器**。

这是有意的，不是遗漏：

- 索引是**派生数据**，损坏后可以重建（[02-kernel §6.5](02-kernel.md)）；
- 索引的形态（哪种索引、怎么组织段、怎么合并）演进频率远高于内核；
- 把索引写进内核冻结清单，会让「换一种索引结构」变成一次 ADR。

因此本文的核心结论是：**索引恢复不是一个内核恢复流程，而是内核之上的一个「追赶者」**。内核负责提供追赶所需的事实（水位、已提交的对象变更），索引构建器负责追赶本身。

内核侧需要的最小接口：

```rust
/// 对象变更流：索引构建器的输入。
/// 载荷只含逻辑标识符与逻辑元数据字段，不含任何物理引用（见 04 §14.1 规则 1）。
pub enum ObjectChange {
    Created   { object: ObjectId, version: VersionId, meta: ObjectMeta },
    Versioned { object: ObjectId, version: VersionId, previous: Option<VersionId>, meta: ObjectMeta },
    Deleted   { object: ObjectId, version: VersionId },
}

/// 对象的逻辑元数据：只含 schema / tenant / 类型等逻辑字段。
/// 刻意不含 `StorageRef` 一类的物理引用——元数据在 WAL 里的落盘形式是
/// `metadata_ref: StorageRef`（03 §2.3），那是内核内部的表示，
/// 越过内核边界暴露出来就违反 04 §14.1 规则 1。
pub struct ObjectMeta {
    pub object_type: u8,
    pub tenant_id: u64,
    pub schema_id: u32,
    pub component_count: u16,
}

/// 追赶所需的只读能力
pub trait ChangeFeed {
    /// 当前对象水位（LSN）
    fn object_watermark(&self) -> Lsn;

    /// 读取 (from, to] 区间内**已提交**的对象变更
    fn scan_changes(&self, from: Lsn, to: Lsn) -> KernelResult<Vec<ObjectChange>>;

    /// 枚举快照下可见的对象（回退策略 A 的全量扫描所需，见 §3.1）
    fn scan_objects(&self, snap: &Snapshot) -> KernelResult<ObjectIdIter>;
}
```

**内容从哪里来**：`scan_changes` 只给元数据。向量索引需要的 embedding 在**内核 WAL 里没有**（[03 §2.4](03-consistency-kernel.md) 明确「Payload 不进入 WAL」），因此构建器必须再走一次逻辑对象读路径：`ObjectManager::resolve_version(id, snap)` 解析出可见版本，再 `ObjectManager::read_version(id, ver)` 读出 `ObjectView`（含 payload）——两者都在 [04 §3.2](04-kernel-interfaces.md) 冻结。

**与冻结清单的关系（必须写明，否则实现者会按「未冻结」对待）**：

- `ChangeFeed` / `ObjectChange` / `ObjectMeta` **已由 [ADR-0012](../adr/0012-changefeed-freeze.md) 冻结为 F16**（[04 §1.2](04-kernel-interfaces.md)），并纳入 §14.1 规则 3 的封闭 trait 清单——外部不可实现，只能由内核提供实现。`ObjectChange` / `ObjectMeta` 是 F16 签名的一部分，随 F16 一起冻结。
- 它遵守 [04 §14.1 规则 1](../chatgpt/04-kernel-interfaces.md)：对外暴露的 trait 全部只接受逻辑标识符。上面的定义已经做到——`ObjectMeta` 不含 `StorageRef`，`scan_changes` / `scan_objects` 都按逻辑标识符取数，不泄漏物理位置。
- 调用方向与分页形态也已冻结：`ChangeFeed → WalManager`（禁止反向）；`scan_changes` 的入参带 `ChangeBatchSpec`、返回 `ChangeBatch`（有界批 + `next_cursor` + `blocked_by_pending_txn`），游标不越过未决事务屏障、空批不得前进。详见 [ADR-0012](../adr/0012-changefeed-freeze.md) 决策 2–3。
- `IndexId` 已由 [ADR-0012](../adr/0012-changefeed-freeze.md) 决策 4 补进 [04 §1.2](04-kernel-interfaces.md) 的 F1：16 字节、由协调区分配，内核只承载标识符、不解释其内容——这不会把索引管理器拉进内核，本文 §1.2 的结论不变。

### 1.3 「滞后」是连续的量，不是一个布尔值

[03 §6.1 第 9 步](03-consistency-kernel.md) 写的是「索引声明水位 < object 水位 → 标记『滞后』」。这个二值标记对**正确性**够用（滞后就是滞后），但对**查询决策**不够用：

- 滞后 1 条记录与滞后 100 万条记录，回退路径的代价差三个数量级；
- 滞后的对象里有多少命中过滤条件，决定了「补扫缺口」是否可行；
- 是否有另一个**已就绪**的索引可以协助补齐，决定了回退策略的选择。

所以索引的水位必须**对外暴露为连续量**，并且必须能估算「缺口区间里有多少对象变更」。§3.2 给出代价模型。

### 1.4 两本日志：哪一本是追赶的输入

仓库里有两本日志，它们的分工在既有文档里没有统一过：

| 日志 | 定义处 | 记录什么 | 有没有提交事实 | 有没有 payload |
|---|---|---|---|---|
| **内核 WAL** | [03 §2](03-consistency-kernel.md) | 元数据 + 校验和（对象 / 版本 / 映射 / 事务） | 有（`TxnBegin` / `TxnCommit` / `TxnAbort`） | **没有**（[03 §2.4](03-consistency-kernel.md)） |
| **语义日志 SemLog** | [ADR-0003](../adr/0003-append-only-log-source-of-truth.md)、[architecture/02 §2.1](../architecture/02-storage-engine.md) | 完整语义对象（含 embedding / tags / provenance） | 无（记录本身即已提交事实，用 epoch 标识） | 有 |

[ADR-0003](../adr/0003-append-only-log-source-of-truth.md) 的缓解措施写的是「维护『日志水位 → 索引构建水位』」与「构建滞后期间查询走回退扫描路径」，[architecture/02 §6](../architecture/02-storage-engine.md) 的「现场重建」步骤（加载段 → 记录水位 → 从水位之后回放 → 完成前查询走回退路径）与本文的三阶段结构同源。

本文按下面的分工设计，理由是它能同时满足「提交事实」与「payload」两个要求：

- **变更通知（what changed）**取自**内核 WAL**：它是唯一带事务提交事实的日志，因此「哪些对象版本已提交」可以精确判定（§2.2 第 4 条），而且它的 LSN 持久可恢复。
- **变更内容（what it is）**取自**逻辑对象读路径**：`resolve_version` + `read_version`（[04 §3.2](04-kernel-interfaces.md)）。内核 WAL 里没有 payload，所以向量索引的 embedding 只能从这里拿。

**待收敛的接口缝隙**：SemLog 用 epoch 表达因果可见性（[architecture/02 §4](../architecture/02-storage-engine.md)），内核用 xid + 事务状态表达快照可见性（[03 §3.3](03-consistency-kernel.md)）。两套可见性模型目前没有映射关系。本文按内核的 xid 模型写；如果最终以 SemLog 作为权威重放源，本文的**输入**会变，而三阶段结构、发布顺序规则与幂等键都不变。这条缝隙需要一条 ADR，见 §8。

## 二、增量追赶算法

### 2.1 输入输出

```
输入：index（一个具体索引）、from_lsn = index.watermark、to_lsn = object_watermark
输出：新的索引段（不可变）+ 新的 index.watermark + 新的 index.txn_watermark
```

关键性质：

- **不写对象层。** 追赶不产生对象变更，因此不走事务提交流程。**唯一的内核写**是 §2.4 步骤 2 的 `IndexManifest`（`0x31`）非事务性派生记录——这是一条新的内核对外写入面，同样需要 ADR（§8）。
- **至少一次（at-least-once）语义。** 追赶中途崩溃后重来是允许的，必须幂等（见 §2.5）。
- **不阻塞前台。** 追赶在后台进行；前台查询按 §3 判断是否需要回退。
- **前置条件先行。** 追赶开始前必须检查 §2.7 的 WAL 截断条件，不满足则转入全量重建。
- **水位不得越过未决事务。** 这是 §2.2 第 4 条与 §2.4 步骤 3 的硬约束。

### 2.2 阶段一：追赶扫描（catch-up scan）

扫描 `(from_lsn, to_lsn]` 区间的 WAL 记录，筛出对象变更。

**筛出规则**（顺序重要）：

1. 只看记录类型 `ObjectCreate` / `ObjectVersion` / `ObjectDelete`（[03 §2.3](03-consistency-kernel.md) 的 `0x10`–`0x12` 三类）；
2. **跳过** `ExtentAlloc` / `ExtentFree` / `MappingCreate` / `MappingSwitch`（`0x20`–`0x23`）——它们是存储布局变更，不改变对象的逻辑身份与版本（[architecture/02 §0.1 I3](../architecture/02-storage-engine.md)）；
3. **跳过** `IndexDelta` / `IndexManifest`（`0x30`–`0x31`）——它们是**各索引各自的构建进度**，不是对象变更；
4. **按事务状态分三档处理**：
   - `Committed` → 应用；
   - `Aborted` → 丢弃；
   - `Running`（有 `TxnBegin` 无 `TxnCommit`）→ **既不应用也不丢弃**，并把该事务记为「未决」。

第 4 条是**追赶算法与普通日志回放的关键区别**：内核的对象恢复走 redo，也只 redo 已提交事务（[03 §6.2](03-consistency-kernel.md)）；索引追赶必须用**同一套**提交判定，否则会出现「索引里有、对象里没有」的幽灵条目。

**Running 事务为什么不能丢弃**：运行期（有 `TxnBegin` 无 `TxnCommit`）的事务是**在跑**，不是中止。[03 §6.1 第 5 步](03-consistency-kernel.md) 把「有 `TxnBegin` 无 Commit/Abort → Aborted」写进恢复流程，是因为恢复时扫描窗口结束在 `last_valid_lsn`，窗口外的事务状态无从得知——那是**恢复期**的保守判定，不能搬到**运行期**。运行期若把 Running 事务的记录丢掉，又把水位推过它，该事务提交后它的对象就**永远进不了索引**——这正是 [architecture/02 §0.1 I2](../architecture/02-storage-engine.md) 第三条禁止的「对象已提交但索引永久缺失」。

正确做法：**水位不得越过未决事务**。设未决事务的首条记录 LSN 为 `lsn_barrier`，则：

```
index.watermark ≤ lsn_barrier − 1        （有未决事务时）
index.txn_watermark ≤ min{ xid of 未决事务 } − 1
```

未决事务一旦提交或中止，下一轮追赶就能越过它；因为水位没有越过，它的记录会被重新扫到并正确处理。**两个水位因此总是停在同一个事务上**。

> 注意第 4 条的判定来源在运行期与恢复期不同：**运行期**用内存中的事务状态数组（`xid mod RING_SIZE` 的环形数组，由 `TransactionManager` 维护）；**恢复期**才需要按 [03 §6.1 第 5 步](03-consistency-kernel.md) 从 WAL 重建。追赶器在运行期直接读前者，不自己重建。

**读取的是变更记录指定的那个版本，不是「当前最新版本」**：版本不可变，因此 `ObjectChange` 里的 `version: VersionId` 唯一确定了要读的内容（`read_version(id, ver)`）。追赶过程中可能有并发写入产生 LSN > `to_lsn` 的新版本——它们的记录不在扫描区间内，不进入本段（留给下一轮）。若构建器去读「当前最新版本」，段内容就不再只由 `(from_lsn, to_lsn]` 决定，幂等前提随之失效。

**已提交但索引未消费的对象，其 payload 尚未被索引构建器读到**——扫描到的 `ObjectChange` 只带逻辑元数据，payload 由构建器通过 §1.2 的逻辑读路径按需回填。

### 2.3 阶段二：增量应用（delta apply）

把每条 `ObjectChange` 转成索引内的操作：

| 变更 | 向量索引（HNSW） | 文本索引（倒排） | 时间线索引 | 标量索引（LSM） | 溯源图索引 | 语义指纹索引（LSH） |
|---|---|---|---|---|---|---|
| `Created` | insert | insert | insert | put | append 新边 | upsert |
| `Versioned` | delete(旧) + insert(新) | delete(旧) + insert(新) | append | put(新 key) | append 新边 + 墓碑旧边 | upsert |
| `Deleted` | delete | delete | tombstone | delete | tombstone | delete |

**版本语义**：索引条目的主键是 `(object_id, version_id)`，不是 `object_id`。查询按快照选版本，因此索引里同时存在多个版本是正常状态，删除旧版本是**空间回收**而非正确性要求。

这一点很重要：如果索引只保留最新版本，快照读就会拿到错误的版本。索引必须保留快照可见性所需的历史版本，淘汰由索引自己的合并策略按「快照存活期」决定。

**条目必须携带 xid**：每条索引条目要记录它由哪个事务创建（`created_by_xid`）、由哪个事务删除（`deleted_by_xid`，未删则为无）。没有这两个字段，索引层就无法按快照做可见性判定，只能靠「构建完成时间」这种近似——那正是 [architecture/02 §0.1 I4](../architecture/02-storage-engine.md) 要排除的做法。

**溯源图不是纯 append**：版本化对象会产生新的 provenance 链，因此图索引需要「追加新边 + 墓碑被取代的旧边」，不能按时间线索引那样只追加。

### 2.4 阶段三：封段与发布（seal & publish）

索引不是原地改的，而是一组不可变的段文件（[architecture/02 §3.1](../architecture/02-storage-engine.md)）。追赶的结果先落到**新段**里，然后发布。

```
1'. 追赶变更写入临时段文件 → fsync → 原子 rename 为正式段名
    （与 03 §5.2 检查点的 shadow 目录 + 原子发布同源）
    段内每条记录带 CRC32C，段头带 blake3 摘要（architecture/02 §8），
    供 §5.1 的「校验失败则作废」使用
2.  段写完 → fsync → 写 IndexManifest 记录（RecordType 0x31）
    内容：segment_id、覆盖的 LSN 区间 [from_lsn, to_lsn]、
          消费到的 xid 水位 tx_w、条目数
3.  原子更新域协调区：把新段加入段列表，并推进 index.watermark（LSN）
    与 index.txn_watermark（xid）
    （协调区更新本身必须是原子的：写临时记录 + rename，或协调区是一条
      追加式日志；半截的协调区更新在恢复时按「未发布」处理）
4'. 同一索引的追赶必须单写者：由协调区水位的 CAS 保证同一时刻只有一个
    追赶者推进水位；CAS 失败者丢弃自己的段
```

**顺序规则：先发布段，再推进水位。** 两个水位都遵守。

| 顺序 | 崩溃后状态 | 后果 |
|---|---|---|
| 段 → 水位（正确） | 段已发布、水位未推进 | 重建时从旧水位重扫，重复应用（幂等）；多出来的段是垃圾，可回收 |
| 水位 → 段（错误） | 水位已推进、段不存在 | 查询按覆盖关系判定「这条记录在索引覆盖范围内」，但索引里没有它 → **静默丢数据** |

这是与 [03 §5.2](03-consistency-kernel.md) 检查点「shadow 目录 + 原子发布」同源的规则：**发布不可变对象在前，推进指向它的元数据在后**。

**推进幅度受 §2.2 的未决事务屏障约束**：即使 `to_lsn` 已经越过某条记录，只要那个记录所属的事务仍未决，水位就停在屏障之前。

### 2.5 幂等性与重复应用

追赶是 at-least-once 的，因此必须幂等。幂等性来自两条性质：

1. **已发布段不被修改**：重复追赶写入的是**新段**，不触碰已发布段；已发布段的内容不受影响。
2. **条目按 `(object_id, version_id)` upsert**：重复应用同一条 `ObjectChange` 覆盖同一条目。

**必须说清的是：本文不保证两次重建产生逐字节相同的段。** HNSW 的图结构、LSM 的层划分都依赖插入顺序与构建参数，重建结果未必一致。所以幂等性**不能**建立在「内容相同」上，必须由**幂等键 + 发布去重**保证：

```
幂等键 = (index_id, from_lsn, to_lsn, index_format_version, content_hash)
content_hash = hash(段内全部条目按 (object_id, version_id) 排序后的序列化)
segment_id   = blake3(index_id ‖ from_lsn ‖ to_lsn ‖ index_format_version ‖ content_hash)[0..16]
```

- **必须用稳定哈希**：`segment_id` 会落盘并在重启后用于去重，因此不能用 Rust 的 `DefaultHasher`（不保证跨版本稳定）；用 blake3，与 [03 §5.2 B3](03-consistency-kernel.md) 的检查点摘要一致。
- **取 128 位**：碰撞等价于静默丢段，64 位截断的风险不可接受。
- **内容必须参与哈希**：早先的想法是 `segment_id = hash(from_lsn, to_lsn, index_id)`，靠「同名即同一段」去重。这是不安全的——同一区间两次追赶只要**内容**不同（基线段不同、变更集不同、索引结构升级过），就会得到同一个段 ID；发布时「协调区已存在同名 `segment_id` → 跳过发布」会把**新的正确段丢弃**，让旧段继续服务，索引静默停在旧水位——一个不会报错、只会慢慢少召回的缺陷。
- **`index_format_version` 必须进入键**：同一区间用不同参数重建会产生不同的段，不能被当成同名段去重。
- 发布时若协调区已存在同名 `segment_id` → 跳过发布，直接推进水位；区间重叠但内容不同 → 两段并存，段清单负责消歧（水位更高者为准）。

### 2.6 删除、墓碑与版本链

`ObjectDelete` 在对象层只是「设置 xmax，不做物理删除」（[03 §2.3](03-consistency-kernel.md)）。索引侧对应的是**墓碑（tombstone）**：标记「该 `(object_id, version_id)` 已被删除」，并带上 `deleted_by_xid`。

**两个方向的可见性必须分清**：

- **新快照必须看到墓碑**。若墓碑被丢弃而删除前的条目还在，新快照会把对象解析到那个陈旧条目上，返回一个**已删除的对象**——这就是「删除复活」。墓碑的淘汰条件是：该墓碑对**所有存活快照都不可见**，且已过索引自己的合并水位。
- **旧快照需要的是删除前的版本条目本身**，不是墓碑。旧条目由另一条规则保留：「所有存活快照都看不见它」之前不淘汰（见 §5.3 的逐条目判定）。

把这两个方向混起来会导致按错误的条件淘汰墓碑。

### 2.7 追赶所需的历史已被 WAL 截断 → 全量重建

增量追赶的前提是 `(from_lsn, to_lsn]` 区间的 WAL 记录还在。这个前提不总成立。

[03 §5.3](03-consistency-kernel.md) 规定 `min_required_lsn = current_checkpoint.checkpoint_lsn`，且 `< min_required_lsn` 的 WAL 段在 T1–T3 满足后可以删除。因此：

```
若 index.watermark < min_required_lsn  →  增量追赶不可行
```

此时区间内的对象变更记录已经不在 WAL 里，唯一出路是**从对象目录全量重建索引**：读当前对象目录与版本目录，按可见性规则重新生成全部条目，即 [02-kernel §6.5](02-kernel.md) 的「向量对象扫描 → embedding 抽取 → 索引重建」路径。

这条路径必须写进设计，否则会出现一种只在**长时间停机后**出现的不一致：索引落后很多、WAL 已经截断，追赶器「扫不到任何东西」却认为「已经追上了」（把水位推进到 `to_lsn`），索引从此永远缺一块，而且没有任何报错。

**判定必须显式**：追赶器开始前检查 `index.watermark ≥ min_required_lsn`，不满足则转入全量重建路径，**不得**扫一个空区间后推进水位。

## 三、回退扫描路径与代价估计

### 3.1 何时必须回退

查询携带快照 `S`（`xmin` / `xmax` / `active_xids` / `snapshot_id`）。参与查询的索引 `i` 有一个 xid 水位 `tx_w`。

**覆盖判定（按 xid，不按 LSN）**：

```
快照 S 被索引 i 完整覆盖  ⟺  S.xmin ≤ tx_w
```

理由：快照可见的版本，其创建事务在快照成立时必然已经提交（否则不可见），而该事务的 xid 必然 `< S.xmin`——快照成立时仍在运行的事务（`active_xids`）在快照中不可见，它们后来的提交不影响快照结果。因此只要「所有 xid < S.xmin 的事务的对象变更都已被索引消费」，快照需要的所有版本都在索引里。

`tx_w` 的推进规则：构建器在扫描 WAL 时维护「已出现在 WAL 里、但事务结果尚未确定」的 xid 集合，取其中最小者减一：

```
tx_w = min{ xid : 事务已出现在 WAL 中但状态仍非终态 } − 1
```

没有未决事务时，`tx_w` 等于已见到的最大 xid。终态判定直接用事务状态数组（[03 §4.2](03-consistency-kernel.md)），不需要重新扫描。

**删除的情况**：快照之后才提交的删除，其墓碑会进入索引——但墓碑的淘汰条件（§2.6）保证「没有存活快照能看见它」之前不丢弃，所以旧快照仍然读得到删除前的版本。

**LSN 水位在这里不参与判定**：`index.watermark` 只用于运维滞后指标（§1.1）、追赶扫描起点与段回收边界。

补齐有三种策略：

| 策略 | 做法 | 适用 | 前置条件 |
|---|---|---|---|
| **A. 补扫缺口** | 扫描缺口区间（`(index.watermark, 当前已提交水位]`）对应的对象变更，在内存里过滤后补入 | 缺口小，或过滤条件选择性高 | 需要内核提供「快照下的对象枚举」——`ChangeFeed::scan_objects`（§1.2） |
| **B. 稀疏协助** | 用另一个已覆盖该快照的索引取候选集，再对候选逐个检查 | 存在语义等价的兄弟索引 | 协助索引的候选集必须是**本次查询结果的超集** |
| **C. 等待** | 不返回结果，等索引追上 | 缺口小，或查询可容忍延迟 | 无 |

**策略 B 的超集条件不能省**：不同种类的索引（例如稀疏倒排协助向量 kNN）产生的是由**另一种相似度 / 选择函数**定义的集合，不是查询结果的超集。若不做这个约束，`IndexCoverage::Recovered` 里的「完整」就是假的，查询会静默丢掉真正的近邻——与 §4.2 警告的「取最大水位」是同一类错误。

### 3.2 代价模型

规划器需要的是**三个可比较的数**（每个策略一个），而不是把它们混在一起的一个数：

```
cost(FullScan)     = n_lag × selectivity_estimate(filter) × c_scan + filter_eval_cost
cost(SparseAssist) = K × c_point + filter_eval_cost
cost(Wait)         = 等待时间估计（由追赶速率推算）
```

其中：

| 符号 | 含义 | 来源 |
|---|---|---|
| `n_lag` | 缺口区间内**已提交的对象变更条数**（估计） | 见下 |
| `K` | 协助索引返回的候选数 | 兄弟索引的实测返回量 |
| `c_scan` | 单对象顺序扫描成本（含 payload 读取） | 设备与缓存的实测值 |
| `c_point` | 单对象点查成本 | 同上 |
| `filter_eval_cost` | 过滤表达式求值成本 | 按过滤器类型估算 |

关键中间量 `n_lag` 需要估算。**这个数不能靠猜**：内核在推进 `object_watermark` 时维护一个「每 LSN 区间对象变更数」的直方图（**含 create / version / delete，不能只数新增对象**，否则会低估索引工作量与回退代价），构建器据此估算：

```
n_lag ≈ Σ hist[lsn]  for lsn ∈ (index.watermark, object_watermark]
```

精确计数需要按 LSN 采样；第一版用**速率估计 + 上界**即可：

```
n_lag ≤ n_lag_estimate × safety_factor
```

必须说清两件事：

1. **这是估计，不是上界。** 缺口区间里同一个对象可能被反复更新（版本数 ≫ 对象数），所以按「变更条数」估、按「对象数」用时要乘以平均版本数——宁可高估。
2. **估计值不得用于任何正确性判定。** 它只影响策略选择（A / B / C）与结果标注里的 `missing_estimate`。正确性判定只由 `tx_w` 与 `S.xmin` 决定（§3.1）。

规划器据此比较三个代价：

```
if cost(FullScan) 最小 且 过滤条件选择性高   → A
elif 存在语义等价且已覆盖 S.xmin 的兄弟索引  → B
else                                         → C（等待 / 降级返回）
```

### 3.3 给规划器的接口

```rust
/// 回退策略（第一版三种）
#[non_exhaustive]
pub enum FallbackStrategy {
    FullScan,
    SparseAssist,
    Wait,
}

/// 索引的新鲜度
pub struct IndexFreshness {
    /// 索引消费到的 xid 水位（覆盖判定用）
    pub txn_watermark: TransactionId,
    /// 索引消费到的 LSN 水位（运维与回收用）
    pub lsn_watermark: Lsn,
    /// 查询快照是否超出该索引的覆盖范围
    pub lagging: bool,
    /// 估算的缺口对象数（估计值，不是上界）
    pub estimated_lag_objects: u64,
}

/// 回退代价
pub struct FallbackCost {
    pub strategy: FallbackStrategy,
    pub estimated_objects: u64,
    pub estimated_latency_ms: u64,
}

/// 供查询规划器查询新鲜度
pub trait FreshnessOracle {
    fn freshness(&self, index: &IndexId, snapshot: &Snapshot) -> IndexFreshness;
    fn fallback_cost(&self, index: &IndexId, snapshot: &Snapshot, filter: &FilterSpec)
        -> FallbackCost;
}
```

`FreshnessOracle` 属于**执行层**（[architecture/03-query-engine.md](../architecture/03-query-engine.md)），不属于内核。内核只负责提供 `object_watermark` 与变更流，水位与代价模型由索引层提供。

**一个必须写进接口契约的约定**：`fallback_cost` 返回的是**估算**，不是保证。规划器可以据此选择策略，但不能据此承诺延迟 SLO；查询结果必须带上「该索引在此快照下是完整的还是部分的」标记，让调用方知道拿到的答案有多完整。

## 四、索引网格中的水位矩阵

### 4.1 逐索引水位

一个域有多个索引（Vector / Sparse / Timeline / Provenance / Scalar / Fingerprint，见 [architecture/02 §3.2](../architecture/02-storage-engine.md)），**每个索引有各自的水位**。

这意味着同一时刻，不同索引的滞后量可以差几个数量级：刚重建过的向量索引可能落后几万条，而只增量更新的元数据索引可能只落后几十条。

### 4.2 混合查询的部分滞后

一个查询可能同时用到向量索引与稀疏索引：

```
query: vector_search(k=10) + sparse_filter(terms)
       └── 索引 A（向量，水位 tx_w_A）
       └── 索引 B（稀疏，水位 tx_w_B）
快照 S
```

三种水位合并策略：

| 策略 | 做法 | 问题 |
|---|---|---|
| **取最小公共水位** | 所有索引都退到 `min(tx_w_A, tx_w_B)` | 过度保守：只查询 B 的请求被 A 的滞后拖慢 |
| **取最大水位** | 统一到 `max(tx_w_A, tx_w_B)` | **不安全**：A 并没有覆盖到那个位置 |
| **逐索引判定**（采纳） | 每个索引独立判断是否滞后、分别选择策略 | 结果可能来自不同覆盖范围 |

采用**逐索引判定**。结果是：**每个索引独立判定是否滞后，各自选择回退策略**；查询结果的完整性标记也随之变成「按索引标注」而不是一个全局布尔值。

「取最大水位」这一列要特别强调：它不是「乐观」而是「错」，会把「索引没覆盖到的对象」当成「对象不存在」，返回静默少召回的结果。这比报错更危险，因为 Agent 无法察觉。**这一列对 xid 水位同样成立**——`max(tx_w_A, tx_w_B)` 一样不安全。

### 4.3 完整性标记

查询结果应携带每个索引的覆盖状态：

```rust
pub enum IndexCoverage {
    /// 索引 txn 水位 ≥ 快照 xmin，结果完整
    Complete,
    /// 索引落后，结果经回退补齐后完整
    /// （仅当回退策略能保证候选集是查询结果的超集时才能给出本变体；
    ///   否则必须给 Partial）
    Recovered { via: FallbackStrategy, extra_latency_ms: u64 },
    /// 结果部分且未能补齐（回退代价过高）
    Partial { missing_estimate: u64 },
}
```

`Partial` 是一个**必须暴露给调用方**的状态，不能静默返回。理由与 [architecture/07 §3](../architecture/07-operations.md) 的资源治理口径一致：上层工作流需要知道「这次写入失败过 / 这次结果不完整」，才能决定重试、降级还是交给用户。

## 五、崩溃、并发与回收

### 5.1 追赶过程中崩溃

| 崩溃时机 | 恢复后状态 | 处理 |
|---|---|---|
| 段未写完 | 半个段文件，无 manifest | 段作废，重新追赶 |
| 段写完未 fsync | 段内容可能不完整 | 摘要/CRC 校验失败则作废重扫 |
| 段已 fsync，manifest 未写 | 孤儿段 | 作废重扫 |
| manifest 已写，协调区未更新 | 段存在但未发布 | 重新追赶；因幂等键含内容哈希，同键段被去重 |
| 协调区已更新（段 + 两个水位） | 正常 | 无需处理 |

**结论：追赶过程中崩溃不需要任何「恢复」逻辑**，只需要「未发布的段一律作废、重扫」这一条规则 + 幂等键去重。

**恢复期的权威来源**：索引水位的权威是**域协调区**（活文件），检查点里的 `Index Manifest` 副本只是 `checkpoint_lsn` 时刻的快照。恢复时若二者不一致，以协调区为准（它可能包含了检查点之后发布的段）；但水位不得越过 `min_required_lsn`（§2.7）。

非事务性 `IndexManifest` 记录不参与 [03 §5.3](03-consistency-kernel.md) T1–T3 的事务判定——那三条规则全部以事务为单位，没有「非事务记录何时可删」的规则。这需要补一条规则（§8）。

### 5.2 与 Checkpoint 的关系

检查点**不固化索引内容，但固化索引元数据**。

[03 §5.2](03-consistency-kernel.md) 的 Phase B 对**六个目录**做 versioned 拷贝，其中包含 `Index Manifest`（`CKPT-<id>/index.manifest`）；[03 §6.1](03-consistency-kernel.md) 恢复时也加载这六个目录；[02-kernel §6.2](02-kernel.md) 的 `CheckpointManifest` 里也有 `index_manifest_ref`。所以：

| 内容 | 检查点是否覆盖 | 说明 |
|---|---|---|
| 索引**段**（segment 文件本体） | 否 | 派生数据，可重建（[02-kernel §6.5](02-kernel.md)） |
| 索引**清单**（段列表 + 两个水位） | 是 | 属于「元数据快照」，随检查点持久化 |

因此：

- 崩溃恢复后，已发布的段列表与水位来自检查点或协调区（§5.1）；**未发布的段一律作废**；
- 检查点完成后，`object_watermark` 前移到检查点覆盖的 LSN；
- 索引水位**不随检查点移动**——它由自己的段发布推进；
- 结果：**对象恢复可以走得比索引恢复远得多**。恢复完成后索引立刻处于滞后状态，需要后台追赶。

但这里有一个方向性的后果必须写明：检查点把 `object_watermark` 推高，同时把 WAL 截断边界推到同一个位置。索引若落后太多（`index.watermark < min_required_lsn`），增量追赶就不可能了，必须走 §2.7 的全量重建。**检查点频率越高，这个风险越大**——检查点间隔必须不短于索引追赶一轮的时间，否则索引会反复被推入「必须全量重建」的境地。

这条关系必须在运维上体现：`freshness_lag_ms`（[architecture/07 §1.2](../architecture/07-operations.md)）在每次崩溃恢复后必然先冲高再回落，这是预期行为，不是故障。

### 5.3 与 GC 的交互

索引段本身占空间，且段是不可变的，因此旧的段可以整段回收。但**回收判定必须逐条目做，不能只看段的 LSN 区间**：

> **回收条件**：
> - 段内每一条目 `(object_id, version_id)` 对**所有存活快照都不可见**（或为已不可见的墓碑）；
> - 段不是当前索引的「基线段」。

**为什么不能只看段的 LSN 区间**：段覆盖的 LSN 区间只说明「这段是哪次追赶产出的」，不说明「段里的对象是否还活着」。一个创建于 LSN 100、此后从未更新的对象，其唯一条目就在覆盖 `[0, 500]` 的段里；若按区间回收，LSN ≥ 500 的快照查询会**静默丢失**它，而且 §3.1 的覆盖判定（`S.xmin ≤ tx_w`）仍然会说「索引覆盖了快照需要的所有变更 → 结果完整」。新建索引时全部条目都落在同一个基线段里，「基线段」这条豁免正是这个问题的补丁，但它本身没有定义清楚。

因此**索引段的回收不能复用对象的 GC 六条件**（[03 §8](03-consistency-kernel.md)）：那六条件是针对「对象 extent」设计的（快照引用、epoch 引用、检查点读集、迁移、WAL 恢复依赖）。索引段需要的是另一组条件——**快照可见性 + 段自身的水位区间**。如果强行复用六条件，结果是索引空间永远回收不掉（因为没有「epoch 引用」「检查点读集」这类东西，条件永远不满足）。

**索引段的回收需要单独的一套判定，且必须写进索引层而不是内核的 `Reclaimer`。** 内核的 `Reclaimer`（[04 §10](04-kernel-interfaces.md)）管对象 extent；索引段归索引层。

### 5.4 与迁移的交互

迁移不改变对象的逻辑身份与版本（[architecture/02 §0.1 I3](../architecture/02-storage-engine.md)），因此**迁移对索引透明**：索引里仍是同一个 `(object_id, version_id)`，构建器不需要感知 `MappingSwitch`。

但有一个例外需要写明：**迁移中途崩溃回滚**（[03 §7.3](03-consistency-kernel.md)）时，目标 extent 被丢弃、源 extent 保留。索引不引用物理位置，所以不需要回滚任何索引内容。

## 六、不变量与验收

### 6.1 I4 的可执行化

[architecture/02 §0.1 I4](../architecture/02-storage-engine.md)：「索引可见性由 LSN 水位决定，而非构建完成时间 | 查询可见性测试」。

**先修正一句**：I4 原文用的是「LSN 水位」，而按 [03 §1.1 R3](03-consistency-kernel.md) 与 [03 §3.3](03-consistency-kernel.md)，快照可见性只由 xid + 事务状态决定，快照里也没有 LSN 字段。因此 I4 在索引层的可执行形式应读作：**索引覆盖判定由「索引的水位 vs 快照的 xid 窗口」决定，而不是由「索引什么时候构建完」决定**。LSN 水位只用于运维滞后与段回收。

拆成可执行断言：

| 编号 | 断言 |
|---|---|
| A1 | 索引覆盖判定只读「快照 `xmin` vs 索引 xid 水位」与事务状态数组，不读 LSN、不读 VersionId、不读任何时间戳（[03 §1.1 R3](03-consistency-kernel.md)） |
| A2 | 每条索引条目带 `created_by_xid` 与 `deleted_by_xid`，可参与快照可见性判定；条目主键仍是 `(object_id, version_id)` |
| A3 | 快照 `xmin ≤ index.txn_watermark` 时，索引对快照**完整**（不需回退）；完整性以「该快照下所有可见对象都能在索引中定位」判定 |
| A4 | 快照 `xmin > index.txn_watermark` 时，结果要么经回退补齐后完整，要么显式标注 `Partial` |
| A5 | 发布顺序恒为「段先于水位」；不存在「水位已推进但段不存在」的持久状态（LSN 水位与 xid 水位都遵守） |
| A6 | 追赶重复执行不修改已发布段；同一幂等键的重复发布被去重，内容不同的重复追赶不会被误去重 |
| A7 | 同一快照下，索引结果与暴力扫描结果的差集 ⊆ （未消费事务产生的对象 ∪ 近似索引的容差集） |
| A8 | 水位的推进不依赖任何时间戳或构建完成时间：同一批 WAL 记录重放两次得到相同的水位 |
| A9 | `index.watermark < min_required_lsn` 时必须转入全量重建，**不得**扫描空区间后推进水位 |
| A10 | 水位不得越过未决事务：存在 Running 事务时，`index.watermark` 不越过其首条记录的 LSN，`index.txn_watermark` 不越过其 xid |

**A3 / A7 的比较对象是「快照可见版本集合」，不是「索引中的全部条目」**——索引按 §2.3 保留多版本，直接做集合比较会把不可见版本算进差集。

**A7 里的「容差集」**：向量 HNSW / IVF-PQ 是**近似**索引（[architecture/02 §3.2](../architecture/02-storage-engine.md)），它与暴力扫描的差集包含近似误差，不可能落在滞后区间内。因此：

- 对**精确**索引（标量 / 倒排 / 时间线），容差集为空；
- 对**近似**索引（向量），容差集按 `recall@k` 阈值定义（例如 `recall@10 ≥ 0.95`）。

没有这条容差集，A7 作为「最终裁判」会在系统最主要的索引类型上恒失败。

### 6.2 验收用例

| 用例 | 场景 | 期望 |
|---|---|---|
| `catchup_basic` | 写入 N 个对象后追赶 | 索引条目数 = N，两个水位都推进到最新 |
| `catchup_idempotent` | 同一区间追赶两次 | 段数不增（幂等键去重），已发布段内容不变 |
| `catchup_content_differs` | 同一区间两次追赶但内容不同 | **产生两个不同段**，不误去重（§2.5） |
| `catchup_running_txn` | 追赶区间内有 Running 事务 | 该事务的记录**不丢弃**；水位停在其屏障之前；该事务提交后能被下一轮扫到（A10） |
| `catchup_crash_before_publish` | 段写完未更新协调区时崩溃 | 重启后重扫，最终条目数 = N |
| `catchup_aborted_txn` | 追赶区间内含 Aborted 事务 | 该事务的对象**不在**索引里 |
| `catchup_concurrent_write` | 追赶期间并发写入新版本 | 新版本不进入本段（LSN > to_lsn），下一轮才进 |
| `catchup_wal_truncated` | `index.watermark < min_required_lsn` | 转入全量重建路径，**不**推进水位（§2.7 / A9） |
| `fallback_full_scan` | 落后 + 无兄弟索引 | 走补扫缺口，结果完整，延迟增加 |
| `fallback_sparse_assist` | 落后 + **语义等价**的兄弟索引就绪 | 走稀疏协助，结果完整（`Recovered`），`K` 不超过补扫的对象数 |
| `coverage_partial_exposed` | 回退代价过高 | 结果标注 `Partial { missing_estimate }`，不是静默截断 |
| `snapshot_history` | 保留旧快照后追赶 | 旧快照读到的仍是旧版本，不是最新版本 |
| `delete_tombstone` | 删除对象后用**新**快照读 | 读不到该对象（墓碑生效，删除不复活）；**旧**快照读得到删除前版本 |
| `publish_order_crash` | 水位推进前崩溃 | 恢复后无「水位已推进但段缺失」 |
| `visibility_no_lsn` | 快照可见性判定路径 | 只读 xid 与事务状态，不读 LSN / VersionId / 时间戳（A1） |
| `approx_recall` | 向量索引与暴力扫描对比 | 差集 ⊆ 容差集，且 `recall@10` ≥ 阈值（A7） |

### 6.3 与测试夹具的关系

本文的验收用例分两层落地：

- **L1（索引层）**：索引构建器自身的纯逻辑（段生成、幂等键去重、发布顺序）。[05](05-kernel-test-harness.md) §5.1 的 L1 模块表目前只覆盖内核模块（ObjectManager…IoExecutor），**没有索引模块**；05 也没有假协调区、没有假索引存储、没有索引层的崩溃重启模型。这部分夹具需要新增。
- **L2（崩溃层）**：段 fsync 与水位推进之间崩溃。需要在 [05 §3.2](05-kernel-test-harness.md) 的 site 目录新增两个 site：`index.segment.sealed`（段 fsync 完成）与 `index.watermark.published`（协调区水位推进完成）。这两个 site 对应**设备写操作**，因此属于 05 §3.1.1 的**设备级**——打点方在索引层，写操作仍然经过内核的 `IoExecutor`。该机制已由 [ADR-0011](../adr/0011-device-op-site-tagging.md) 定下：site 标签是 `DeviceOp` 的字段，由调用方在提交时声明、`IoExecutor` 在两处触发点透传，索引层因此可以给自己的写操作打标签。
- 但 **05 §5.1 的 L1 模块表与 §3.5 的 `FaultSite` 枚举（要求穷尽 `match`）都要同步扩展**，否则新增 site 未登记即编译失败（05 §8 的编译期校验）。这一步是 05 与本文的接口改动，不是本文单方面能决定的。
- 「段先于水位」这条顺序保证由**索引层自己的逻辑**提供，不由 `IoExecutor` 提供——因此对应的断言是「崩溃后不存在『水位已推进但段缺失』」，而不是「两个 site 的相对顺序」。

## 七、与既有文档的关系

| 文档 | 本文对其做了什么 |
|---|---|
| [03 §6.1 第 9–10 步](03-consistency-kernel.md) | 把「标记滞后 + 后台增量重建」细化为三阶段追赶算法、xid 水位与发布顺序规则 |
| [03 §5.3](03-consistency-kernel.md) | 补充 WAL 截断对增量追赶的约束：落后太多时必须全量重建（§2.7） |
| [03 §12 第 2 项](03-consistency-kernel.md) | 本文的直接来源，已完成 |
| [02-kernel §6.5](02-kernel.md) | 「索引恢复与对象恢复分离」的原则保留；本文给出分离后的具体算法与代价模型 |
| [architecture/02 §0.1 I4](../architecture/02-storage-engine.md) | 从一句话不变式变成 A1–A10 十条可执行断言；并把 I4 原文的「LSN 水位」修正为「xid 窗口」 |
| [architecture/02 §3](../architecture/02-storage-engine.md) | 段文件与构建水位模型细化；补充「先段后水位」的发布规则与幂等键 |
| [architecture/03](../architecture/03-query-engine.md) | 提供 `FreshnessOracle` 与 `FallbackCost`，供规划器在索引落后时选策略 |
| [architecture/07 §1.2](../architecture/07-operations.md) | `freshness_lag_ms` 的语义明确：崩溃恢复后必然先冲高 |
| [04](04-kernel-interfaces.md) | 确认索引不在内核冻结清单内；**本文提出**内核侧的 `ChangeFeed` 只读能力草案（尚未冻结，需 ADR，见 §1.2） |
| [ADR-0003](../adr/0003-append-only-log-source-of-truth.md) | 「日志是唯一真源」在索引层的落地；两本日志的分工见 §1.4 |
| [architecture/02 §6](../architecture/02-storage-engine.md) | 「现场重建」的四个步骤与本文的三阶段结构对齐 |

## 八、下一步

本文把索引追赶定到了可编码的粒度。原本还剩四处需要 ADR，**现已全部落定**：

1. **`ChangeFeed` 纳入内核冻结清单**（§1.2）→ 已完成，见 [ADR-0012](../adr/0012-changefeed-freeze.md)。决定：`ChangeFeed` 进冻结清单（F16）并纳入 [04 §14.1 规则 3](../chatgpt/04-kernel-interfaces.md) 的封闭列表；调用方向为 `ChangeFeed → WalManager`（禁止反向）；`scan_changes` 改为**游标 + 有界分页**，游标不越过未决事务屏障、空批不得前进；`IndexId` 补进 F1（冻结标识符语义，但不把索引管理器拉进内核，本文 §1.2 的结论不变）。
2. **索引层写入内核 WAL**（§2.4 步骤 2）→ 已完成，见 [ADR-0013](../adr/0013-index-manifest-in-wal.md)。决定：新增非事务入口 `WalManager::append_derived(rec, family)` 并按记录族鉴权；`IndexManifest`（`0x31`）是**派生事实，不是提交事实**；WAL 截断补第四条件 **T4**（非事务记录不阻塞 `min_required_lsn`，只阻塞段文件的物理删除）；恢复期 `0x31` 不参与 redo，只做与协调区的只读交叉核对。
3. **两本日志的可见性模型对齐**（§1.4）→ 已完成，见 [ADR-0014](../adr/0014-semlog-epoch-vs-xid-visibility.md)。决定：**「是否已提交」只由内核 xid 判定**，「能否看见」由因果快照判定，两者是「存在」与「范围」的关系而非两套真值；SemLog 追加固定在 `TxnCommit` 持久**之后**（反向会产生不可撤销的幽灵事实）；三个同名 `epoch` 分别改名为 `PlacementEpoch` / `sem_epoch` / `causal_token`；本文的权威输入是内核 WAL，**不改**。
4. **索引段回收的判定条件**（§5.3）→ 已完成，见 [ADR-0015](../adr/0015-index-segment-reclamation.md)。决定：回收器归索引层，**不改** [04 §10](04-kernel-interfaces.md) 的 `Reclaimer` 契约；判定为 G1–G6 六条合取（含基线段及其继任者条件、逐条目快照可见性、墓碑、引用集、先摘链后删文件）；不做部分回收，部分保留走合并；内核新增 `LiveSnapshotSource`（F17）提供唯一的可见性实现，禁止索引层自写判定。

另有 [03 §12 第 3 项](03-consistency-kernel.md)（硬件资源管理的实现细节）会直接影响本文的代价模型：`c_scan` 与 `c_point` 的取值来自硬件实测，代价模型参数化必须与硬件 Profile 打通。 → 已完成，见 [07-hardware-resource-management.md](07-hardware-resource-management.md)；该文 §2.7 给出这两个量的定义（它们不是新参数，而是该文 `L` / `T` 的组合，以微秒为单位；点查只取回**一个逻辑 extent**，扫描读完全部 extent——两者的差距是本文 §3.2 的 A / B 策略可区分的前提），§7.2 的 B7 是这两个数的**唯一实测来源**（设备取数与缓存命中各一组），§7.3 要求两处引用同一份来源。

**本文因此产生的、仍需实现侧对齐的两处**（不是新的设计问题）：

- §6.3 说的「`05` 的 L1 模块表与 `FaultSite` 枚举要加索引模块与两个索引层 site」——site 的承载机制已由 [ADR-0011](../adr/0011-device-op-site-tagging.md) 定为 `DeviceSite` 枚举，两个索引层 site 已列入其中；`05` 的模块表补索引构建器仍是夹具侧的具体工作。
- §2.5 的幂等键与 §5.1 的「未发布的段一律作废」在 [ADR-0013](../adr/0013-index-manifest-in-wal.md) 之后多了一条可核对路径（`0x31` 与协调区交叉核对），但结论不变：协调区仍是权威。

另有 [03 §12 第 3 项](03-consistency-kernel.md)（硬件资源管理的实现细节）会直接影响本文的代价模型：`c_scan` 与 `c_point` 的取值来自硬件实测，代价模型参数化必须与硬件 Profile 打通。 → 已完成，见 [07-hardware-resource-management.md](07-hardware-resource-management.md)；该文 §2.7 给出这两个量的定义（它们不是新参数，而是该文 `L` / `T` 的组合，以微秒为单位；点查只取回**一个逻辑 extent**，扫描读完全部 extent——两者的差距是本文 §3.2 的 A / B 策略可区分的前提），§7.2 的 B7 是这两个数的**唯一实测来源**（设备取数与缓存命中各一组），§7.3 要求两处引用同一份来源。
