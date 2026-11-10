# ADR-0012：`ChangeFeed` 纳入冻结清单，`IndexId` 补进 F1

- **状态**：已接受
- **日期**：2026-11-05

## 背景

[06 §1.2](../chatgpt/06-index-recovery.md) 提出了一项内核侧的只读能力 `ChangeFeed`（`object_watermark` / `scan_changes` / `scan_objects`），供索引构建器做增量追赶。它同时给出了三条必须由 ADR 回答的问题：

1. `ChangeFeed` 是否纳入 [04 §14.1 规则 3](../chatgpt/04-kernel-interfaces.md) 的封闭 trait 清单；
2. 它与 `WalManager` 的调用方向；
3. `scan_changes` 的分页形态——当前草案一次返回整个区间，缺口大时会撑爆内存。

同一节还指出：`IndexId` 有 `IdKind` 变体、架构层已经在用，但**不在 [04 §1.2 F1](../chatgpt/04-kernel-interfaces.md) 的冻结标识符清单里**（F1 目前只有 `ObjectId` / `VersionId` / `TransactionId` / `Lsn` / `ExtentId` / `MappingId` / `PhysicalExtentId` / `SnapshotId`）。06 §8 把这条列为索引落地的第一项前置条件。

不定这三件事，会出现三类具体的坏结果：

- **实现自由**：外部可以自写一个「半实现」的 `ChangeFeed`，用 LSN 近似可见性，而 [03 §1.1 R3](../chatgpt/03-consistency-kernel.md) 恰好禁止这种做法。内核无法阻止。
- **方向错误**：如果 `ChangeFeed` 的实现直接读 WAL 设备（绕过 `WalManager`），它就绕过了 WAL 的段管理、CRC 扫描与 `min_required_lsn` 边界——追赶到被截断的区间时会「扫不到任何东西却认为已经追上」（[06 §2.7](../chatgpt/06-index-recovery.md) 描述的静默缺块）。
- **内存爆炸**：一次返回整个缺口区间，恢复后首次追赶可能返回百万级 `ObjectChange`。

## 决策

1. **`ChangeFeed` 纳入冻结清单，编号 F16。**

   [04 §1.2](../chatgpt/04-kernel-interfaces.md) 增加一行：

   | 编号 | 契约对象 | 所属模块 | 冻结内容 | 变更方式 |
   |---|---|---|---|---|
   | F16 | `ChangeFeed` | 内核 → 索引层（只读） | 对象变更流的三个只读入口及其可见性口径 | ADR |

   冻结的是**可观察行为**：返回集合的可见性口径、游标的单调性、水位语义。trait 内部的分页实现、缓存结构不冻结。

2. **`ChangeFeed` 进入 §14.1 规则 3 的封闭列表（`private::Sealed`）。**

   `ChangeFeed` 的每一条输出都参与正确性判定：`txn_watermark` 决定快照是否被完整覆盖（[06 §3.1](../chatgpt/06-index-recovery.md)），`ObjectChange` 决定索引里出现哪些条目。一个外部实现只要在「哪些 xid 算终态」上有一点偏差，索引就会出现幽灵条目或永久缺块——而这两类错误**都不会报错**，只会让召回慢慢变少。

   `StorageDevice` 是封闭列表的唯一例外（[04 §6.1 D0.2](../chatgpt/04-kernel-interfaces.md)），理由不适用于 `ChangeFeed`：后端只搬运字节，不判定语义。

3. **可见性与 WAL 共用同一套判定，由内核提供，不由 `ChangeFeed` 自己实现。**

   `scan_changes` 的过滤规则取自 [06 §2.2](../chatgpt/06-index-recovery.md)：只看 `0x10`–`0x12`，跳过 `0x20`–`0x23` / `0x30`–`0x31`（除 §2.4 步骤 2 的自有 `IndexManifest`），按 `TxnStatus` 分三档，`Running` 既不应用也不丢弃。判定来源：

   - **运行期**：直接读 `TransactionManager::status(xid)`（[04 §8](../chatgpt/04-kernel-interfaces.md) 已冻结的入口）背后的内存状态数组；
   - **恢复期**：按 [03 §6.1 第 5 步](../chatgpt/03-consistency-kernel.md) 重建后同样走 `status()`。

   `ChangeFeed` **不得**自己维护第二套事务状态判定表。两套判定一旦分叉，就是 [03 §1.1 R3](../chatgpt/03-consistency-kernel.md) 所说「用序号近似可见性」的变体。

4. **调用方向：`ChangeFeed` 只负责对象语义，读取动作经 `WalManager::scan_from`；`WalManager` 不得回头调用 `ChangeFeed`。**

   ```
   Intent Executor / 索引构建器
        │
        ▼
   ChangeFeed ──(读 WAL 记录)──► WalManager
        │                          │
        │                          ▼
        │                      IoExecutor
        ├──(事务状态)──► TransactionManager::status
        └──(payload 回填)──► ObjectManager::read_version   （不在本 ADR 内，见 06 §1.2）
   ```

   - `ChangeFeed` **不能**直接持有 `StorageDevice` 或自己扫 WAL 段文件：段边界、CRC 终止语义与 `min_required_lsn` 都在 `WalManager` 里（[04 §7 W2](../chatgpt/04-kernel-interfaces.md)），绕过它等于复制一份会漂移的扫描逻辑。
   - `WalManager → ChangeFeed` 的反向边**禁止**（并入 [04 §12.2](../chatgpt/04-kernel-interfaces.md) 的依赖禁令表）：日志层不得回调任何上层。
   - `ChangeFeed` 不是新的锁层：它只读 WAL 与事务状态，不引入新锁，遵守既有的 L5 → L1 顺序（[04 §12.3](../chatgpt/04-kernel-interfaces.md)）。特别地，**禁止持 L5 扫描整个缺口区间**。

5. **`scan_changes` 改为游标 + 有界分页。**

   ```rust
   /// 一次拉取的粒度上限（条数与字节数各一档，取先到者）
   pub struct ChangeBatchSpec {
       pub max_records: u32,
       pub max_bytes: u64,
   }

   /// 一次拉取的结果
   pub struct ChangeBatch {
       /// 本批已提交变更（顺序 = WAL 顺序）
       pub changes: Vec<ObjectChange>,
       /// 下一批的起点；`None` 表示已到达 `to_lsn`
       pub next_cursor: Option<Lsn>,
       /// 本批之前是否遇到未决事务屏障（06 §2.2）；为真时 next_cursor 停在屏障之前
       pub blocked_by_pending_txn: bool,
   }

   // 取代 06 §1.2 草案的 scan_changes(from, to) -> Vec<ObjectChange>
   fn scan_changes(&self, from: Lsn, to: Lsn, spec: &ChangeBatchSpec)
       -> KernelResult<ChangeBatch>;
   ```

   关键约定：

   - **游标单调不减，且不越过未决事务屏障**。`next_cursor` 的最小值是 `min{ 未决事务首条记录 LSN }`（06 §2.2 的 `lsn_barrier`），不是 `to_lsn`。
   - **分页不改变可见性结果**：同一区间用不同 `max_records` 拉取，把各批 `changes` 按序拼接后，结果必须**逐条相同**（这是可测试的性质，见 [06 §6.2](../chatgpt/06-index-recovery.md) 的 `catchup_*` 用例族）。
   - **`max_bytes` 是软上限**：单条记录超过 `max_bytes` 时仍返回它（一条记录不可拆），不在记录中间分页。软上限只用于限制内存，不表达任何语义。
   - **不允许返回空批且 `next_cursor` 前进**：空批只允许出现在「已达 `to_lsn`」（`next_cursor = None`）或「遇屏障」（`blocked_by_pending_txn = true` 且 `next_cursor = lsn_barrier`）。这条禁止了 06 §2.7 警告的「扫一个空区间后推进水位」。

6. **`IndexId` 补进 F1。**

   F1 的标识符清单增加 `IndexId`。理由：它已经出现在 `IdKind` 的变体里，并被架构层使用（[architecture/02 §3.1](../architecture/02-storage-engine.md) 的索引段与协调区元数据），而 F1 的作用正是「同一个标识符在整个系统里只有一个语义」。缺了它，索引层与规划器各自定义 `IndexId` 只是时间问题。

   **`IndexId` 补进 F1 不改变 06 §1.2 的结论「索引不在内核冻结清单内」**：标识符的语义被冻结，不等于索引管理器进入内核。索引段如何组织、如何合并、如何回收仍归索引层（[06 §5.3](../chatgpt/06-index-recovery.md)）。

   `IndexId` 的语义与 `ObjectId` 同构：16 字节定长、全局稳定、永不复用；由**域协调区**分配，不由内核分配——内核只在 WAL 记录里**承载**它（`0x30` / `0x31`）。

7. **`ChangeFeed` 的 `ObjectChange` / `ObjectMeta` 形状冻结，字段语义冻结，字段可增不可改。**

   沿用 [06 §1.2](../chatgpt/06-index-recovery.md) 的定义。`ObjectMeta` **不含** `StorageRef` 等物理引用（[04 §14.1 规则 1](../chatgpt/04-kernel-interfaces.md)）。后续若要给 `ObjectMeta` 加字段（例如 07 的 home node），必须是可选字段并走 `#[non_exhaustive]`，不得复用已有字段表达新含义。

## 备选方案

1. **`ChangeFeed` 不进冻结清单，留作「实现自由」** → 外部实现可以在可见性判定上与内核分叉，产生不会报错的幽灵条目与永久缺块。改为冻结（F16）并封闭实现。
2. **`ChangeFeed` 与 `StorageDevice` 一样允许外部实现** → 后端只需要搬运字节；索引追赶需要精确的提交判定，外部实现没有义务与内核一致，也无法被内核验证。改为封闭。
3. **`ChangeFeed` 直接读 WAL 设备，不经过 `WalManager`** → 复制段管理、CRC 终止与截断边界逻辑，两份逻辑迟早分叉；`min_required_lsn` 检查会被漏掉。改为经 `WalManager::scan_from`。
4. **`WalManager` 直接提供 `scan_changes`（不引入新 trait）** → 把对象语义塞进日志层，违反 [04 §12.2](../chatgpt/04-kernel-interfaces.md) 的「`WalManager` → 对象语义」禁令；而且 `WalManager` 不该知道 `ObjectChange`。改为独立 trait，单向依赖 `WalManager`。
5. **保留一次返回整区间的 `scan_changes(from, to) -> Vec<ObjectChange>`** → 恢复后首次追赶的缺口可能到百万条，单次分配即 OOM；且调用方无法边拉边建段。改为游标 + 有界分页。
6. **分页用 `(from, limit)` 而不带游标** → 调用方要自己推算下一批起点，而屏障会让「推算出的下一个 LSN」与「实际安全起点」不一致。改为内核返回 `next_cursor`。
7. **分页时遇未决事务直接跳过并前进游标** → 该事务提交后其对象永远进不了索引（[architecture/02 §0.1 I2](../architecture/02-storage-engine.md) 第三条禁止）。改为游标停在屏障之前。
8. **`max_bytes` 做成硬上限，超限记录截断** → 截断的 `ObjectChange` 会生成错误条目。改为软上限：超限记录整条返回。
9. **`IndexId` 留给索引层自己定义** → 内核的 `0x30` / `0x31` 记录要携带它，`IdKind` 已有变体，两处定义迟早不一致。改为补进 F1，同时明确它不把索引管理器拉进内核。
10. **既然 `IndexId` 进 F1，顺带把索引管理器也冻结** → 索引形态演进频率远高于内核，冻结它会让「换一种索引结构」变成一次 ADR（[06 §1.2](../chatgpt/06-index-recovery.md)）。只冻结标识符语义。

## 后果

**正面**

- 索引追赶有了一个**不可被外部改写**的输入契约：可见性口径与内核一致，不再有第二套提交判定。
- 分页让首次追赶的内存占用有上界，并且「遇屏障停游标」这条规则被写进了类型（`blocked_by_pending_txn`），而不是靠实现者记得。
- 空批不前进这条禁止，直接封住了 06 §2.7 的静默缺块。
- `IndexId` 有了唯一语义，索引层与规划器可以共享同一个类型。

**负面**

- 冻结清单从 F1–F15 变成 F1–F16，`ChangeFeed` 的签名今后改动都要走 ADR。
- 分页接口比一次返回更难用：调用方（索引构建器）必须写循环并处理屏障。
- `ChangeFeed` 现在处在「对象层语义 + 日志层读取」的中间位置，它同时依赖 `WalManager` 与 `TransactionManager`，是 04 §12.1 调用图上的一个新节点（下图已补）。

**缓解**

- 调用方的循环可以直接封装成索引层的一个迭代器适配器（`ChangeFeedIter`），三阶段算法（[06 §2](../chatgpt/06-index-recovery.md)）按批处理本来就更自然。
- 「分页不改变可见性」做成性质用例：同一区间用 `max_records ∈ {1, 7, 1000}` 拉取，拼接结果必须相同——这条用例比任何文档描述都更能守住接口。
- `ChangeFeed` 依赖面变大这一点，由 §12.2 的依赖禁令守住：它只能**向下**依赖 `WalManager` / `TransactionManager` / `ObjectManager`，不得被它们反过来调用。
