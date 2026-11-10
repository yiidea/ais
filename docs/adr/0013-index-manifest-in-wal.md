# ADR-0013：索引层写入内核 WAL，以及非事务记录的截断规则

- **状态**：已接受
- **日期**：2026-11-05

## 背景

[06 §2.4](../chatgpt/06-index-recovery.md) 的三阶段追赶把发布流程定为「段写完 → fsync → 写 `IndexManifest`（记录类型 `0x31`）→ 协调区推进水位」。其中第 2 步是**内核对外的一条新写入面**：索引层（内核之外）往内核 WAL 里追加一条记录。这件事有两处和既有冻结契约对不上：

1. **调用图里没有这条边。** [04 §12.1](../chatgpt/04-kernel-interfaces.md) 的允许调用方向里，`WalManager` 的调用方只有 `TransactionManager` 与后台组件（Checkpoint / Recovery / Reclaimer / Migration）。索引层不在图上，`WalManager::append` 的签名也没区分「事务内」与「非事务」的追加。
2. **截断规则没有覆盖非事务记录。** [03 §5.3](../chatgpt/03-consistency-kernel.md) 的 T1–T3 全部以**事务**为单位：T1 说「该段的全部记录 ≤ 某个已发布 checkpoint 的 checkpoint_lsn」，T2 说「不存在仍在执行中的事务…」，T3 说「不存在依赖该段的未完成迁移」。一条不属于任何事务的 `IndexManifest` 会不会因为 T1 被截掉，取决于它相对 checkpoint 的位置——而恰恰是这条记录告诉别人「那个索引段已经发布」，它的丢失与保留直接决定 §2.4「先段后水位」是否还能成立。

第一条不定，实现者会各写各的「往 WAL 塞一条记录」的旁路；第二条不定，会在「索引追赶很慢、检查点很勤」的组合下悄悄剪掉索引发布记录。

## 决策

1. **`WalManager` 增加一个独立的非事务追加入口，并按记录族鉴权。**

   ```rust
   /// 事务追加：xid 非 0，记录进入该事务的前驱链（prev_lsn）
   fn append(&self, rec: &WalRecord) -> KernelResult<Lsn>;

   /// 非事务追加：xid = 0，不进入任何事务的前驱链
   fn append_derived(&self, rec: &WalRecord, family: DerivedFamily) -> KernelResult<Lsn>;
   ```

   `DerivedFamily` 只允许三个取值：`Index`（`0x30` `IndexDelta`、`0x31` `IndexManifest`）、`Maintenance`（`0x40`–`0x43` 中不含提交语义的 Begin/End）、`None`（保留，任何模块都不得使用）。

   **记录族与调用方必须匹配**：`append_derived` 收到的事务族记录（`0x01`–`0x03`）或对象/映射族记录（`0x10`–`0x23`）一律返回 `InvalidArgument` 而不写入。理由：这些族的事务性语义（幂等键、redo 规则、可见性）都是从「属于某个代事务」推导出来的，以非事务方式写入会绕过 [03 §6.2](../chatgpt/03-consistency-kernel.md) 的重建规则。

   这也顺带解释了为什么**不能只是一个 `append` 加一个 `xid = 0` 的约定**：约定无法被类型系统或运行时检查守住，而没有校验的旁路正是本 ADR 要消除的东西。

2. **反查方向：`WalManager` 仍然不知道索引层。**

   `WalManager` 只负责「帧封装 + 按族校验 + 持久化」，不解析 `IndexManifest` 的 body（与 [04 §12.2](../chatgpt/04-kernel-interfaces.md) 对 WAL 的禁令一致：记录体由上层按 [03 §2.3](../chatgpt/03-consistency-kernel.md) 的类型构造）。

   索引层对内核的**写**依赖只有 `WalManager` 这一个入口（它不持有 `StorageDevice`，不碰 `IoExecutor`；设备级 site 通过 [ADR-0011](0011-device-op-site-tagging.md) 的 `DeviceSite` 声明）。索引层**不得**持有 `WalManager` 之外的任何内核写路径。

3. **`IndexManifest` 的地位是「派生事实，不是提交事实」。**

   | 它是什么 | 它不是什么 |
   |---|---|
   | 「某个索引段已经完成 fsync 并覆盖 `[from_lsn, to_lsn]`」的断言 | 对象可见性的来源 |
   | 恢复期核对索引清单的交叉证据 | 索引水位的权威（权威是域协调区，[06 §5.1](../chatgpt/06-index-recovery.md)） |

   因此 §2 的 `DerivedFamily::Index` 记录**不参与任何读者静默路径**，也**不被任何可见性判定读取**。缺少一条 `IndexManifest` 的后果最多是「这个段是否已发布无法从 WAL 交叉核对」——此时以协调区为准，并触发一次该索引的全量重建检查，而不是当成数据丢失。

4. **非事务记录的截断规则（补齐 T1–T3 的第四条件 T4）。**

   T1–T3 只决定「**事务记录**是否可以随段删除」。非事务派生记录的增加一条**合取条件**：

   ```
   T4. 对于族 Index：该段内所有 IndexManifest 记录所声明的 (index_id, segment_id)
       对**当前所有已注册索引**都已出现在各自域协调区的已发布段列表中；
       对族 Maintenance：该段内所有 Begin 记录都有对应的 End 记录，
       或被判定为「未完成的维护」（其清理语义见对应协议，03 §6.1 step 7）。

   任何一条不满足 → 该段不得删除（即使 T1–T3 全满足）。
   ```

   要点：

   - **T4 是拒绝删除，不是阻止水位推进。** `min_required_lsn` 的定义**不变**（仍是 `current_checkpoint.checkpoint_lsn`，[03 §5.3](../chatgpt/03-consistency-kernel.md)）。T4 影响的是「段文件何时可以被物理删除」，纯粹是空间回收的安全界，不改变恢复的起点。把 T4 塞进 `min_required_lsn` 会让截断边界依赖索引状态，进而让检查点频率与索引追赶速度互相耦合——那正是 [06 §5.2](../chatgpt/06-index-recovery.md) 已经指出的风险。
   - **T4 是保守的**：索引被删除（不再注册）时，它的记录不再阻塞截断——判据是「当前所有已注册索引」，不是「历史上出现过的索引」。
   - **T4 允许滞后**：协调区尚未反映的段会让 T4 在短时间内为假，导致 WAL 段**暂时**不能删。这是有意的方向选择——宁可多留几个段，不可在索引追赶慢时剪掉它要读的历史。

5. **恢复期：`IndexManifest` 不参与 redo。**

   [03 §6.2](../chatgpt/03-consistency-kernel.md) 的 redo 规则表**不增加** `0x31` 的条目。理由：`IndexManifest` 描述的是内核之外的索引段文件的发布状态，而索引段不在内核的 checkpoint 覆盖范围内（[06 §5.2](../chatgpt/06-index-recovery.md) 的表格：段不覆盖，清单覆盖）。让我在恢复时根据 WAL 记录「重新应用」一条索引发布，等于让内核替索引层决定它的段已生效——而索引段文件本身可能已损坏。

   恢复时 `0x31` 只做**只读交叉核对**：与协调区的段列表比对，不一致时以协调区为准并记录告警。

6. **`IndexDelta`（`0x30`）的定位同属派生事实，随 ADR 不在 `ChangeFeed` 的输入里。**

   `ChangeFeed::scan_changes` 显式跳过 `0x30`/`0x31`（[06 §2.2 第 3 条](../chatgpt/06-index-recovery.md)），本 ADR 确认这条不变，并补充理由：它们描述的是「各索引各自的构建进度」，与对象变更正交。

## 备选方案

1. **索引层绕过 `WalManager` 直接写 WAL 设备** → 段边界、CRC 终止语义、`durable_lsn` 记账全部要复制一份，两份逻辑必然分叉；且违反 [04 §6.3 I3](../chatgpt/04-kernel-interfaces.md)「核心内核只通过 `IoExecutor` 触达设备」的对称要求。改为新增窄口 `append_derived`。
2. **复用 `append`，约定 `xid = 0` 表示非事务** → 没有校验的旁路；写错的记录族会绕过 redo 规则。改为独立入口 + 族校验。
3. **让 `WalManager` 接收任意记录并只校验 `record_type` 落在 `0x30`–`0x43`** → `0x01`–`0x23` 若混进来就是灾难；按族（而非按单个类型）校验能让新增类型的默认落入已有族。
4. **把 `IndexManifest` 做成事务性记录（由内核代开一个内部事务）** → 索引追赶会产生用户不可见的 xid，污染 `xmin` 记账与快照集合；且追赶是 at-least-once 的，「索引段已发布」比「事务提交」弱得多。改为非事务派生记录。
5. **把 `IndexManifest` 纳入 `min_required_lsn` 的计算** → 截断边界从此依赖索引状态，检查点频率与索引追赶速度互相绑死；`min_required_lsn` 也不再是「与检查点一条线对齐」的简单定义。改为只影响段文件的物理删除（T4）。
6. **不增加 T4，让 `IndexManifest` 随段一起被截断** → 索引追赶慢 + 检查点勤时，发布记录被剪掉，恢复期无法交叉核对「这个段到底发布过没有」，只能回退到「不信 WAL，只信协调区」。改为补 T4。
7. **T4 里要求「所有历史索引」** → 索引被删除后它的记录永远阻塞截断，空间泄漏。改为只看当前已注册索引。
8. **恢复期 redo `IndexManifest`** → 内核替索引层下了「段已生效」的结论，而段文件可能损坏。改为只做只读核对。

## 后果

**正面**

- 索引层对内核的依赖被收窄到一个明确的、可校验的入口，`WalManager` 的职责边界没有被扩大成「什么都往里塞的日志服务」。
- 非事务记录的截断规则被写死（T4），「检查点把历史剪掉导致索引发布记录丢失」这一类只在慢追赶组合下出现的缺陷被提前消除。
- 恢复了 `IndexManifest` 的定位：它是交叉证据，不是权威。恢复期的行为因此是确定的（以协调区为准 + 告警）。
- 派生记录与对象可见性彻底隔离：任何「用 `0x30`/`0x31` 判断对象是否可见」的实现都会被本 ADR 直接否决。

**负面**

- `WalManager` 的冻结签名增加了一个方法（F7 的变更），并且 `DerivedFamily` 是一个新的公共类型。
- 调用图上多了一条「索引层 → `WalManager`」的边，`WalManager` 现在同时服务内核事务路径与索引构建器；`append_derived` 的写入速率不受事务限流约束，需要独立限流。
- T4 是保守条件，会导致部分 WAL 段在索引落后时暂时无法回收（空间占用上升）。

**缓解**

- `append_derived` 的写入速率由索引追赶自己的 QD 份额约束（[07 §4.7](../chatgpt/07-hardware-resource-management.md) 的后台重建路径），不需要 WAL 侧再发明一套限流。
- T4 的滞后有可观测指标：`wal_segments_blocked_by_index`（被 T4 阻塞的段数）。该值持续增长说明索引追赶落后于检查点频率，是 06 §5.2 已经警告过的运维信号。
- `DerivedFamily` 与 `FaultSite`（[ADR-0011](0011-device-op-site-tagging.md)）一样用 `#[non_exhaustive]` 枚举并穷尽 `match`，新增族必须同时更新本 ADR 与 [04 §12](../chatgpt/04-kernel-interfaces.md)。
