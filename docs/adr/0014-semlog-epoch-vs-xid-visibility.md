# ADR-0014：SemLog 的 epoch 可见性与内核的 xid 可见性如何对齐

- **状态**：已接受
- **日期**：2026-11-05

## 背景

仓库里有两本日志，它们对「什么算已提交、什么对一个读者可见」的表述不同：

| 日志 | 定义处 | 可见性模型 | 有没有 payload |
|---|---|---|---|
| **内核 WAL** | [03 §2](../chatgpt/03-consistency-kernel.md) | xid + `TxnStatus` + 快照（`xmin` / `xmax` / `active_xids`） | 没有（[03 §2.4](../chatgpt/03-consistency-kernel.md)） |
| **语义日志 SemLog** | [ADR-0003](0003-append-only-log-source-of-truth.md)、[architecture/02 §2.1](../architecture/02-storage-engine.md) | 每条记录带 `epoch`，「记录本身即已提交事实」；读者的可见集由 `{ writer_id, seen_epoch_by_writer }`（因果快照）决定（[architecture/05 §1.2](../architecture/05-ai-native-features.md)） | 有 |

[06 §1.4](../chatgpt/06-index-recovery.md) 按内核 xid 模型写了追赶算法，并明确留下一条缝隙：两套可见性模型**没有映射关系**。这条缝隙不补，会出现三种具体后果：

1. **同一对象两种答案**：索引追赶按 xid 判定「已提交」，而架构层的语义日志回放按 epoch 判定「可见」。两者对一条记录给出相反结论时，索引与对象层会长期不一致，且两边各自都「自洽」。
2. **追赶输入不确定**：若实现者理解成「以 SemLog 为权威重放源」，06 的三阶段算法输入就变了，而它的幂等键、发布顺序、屏障规则全部是按 xid 写的。
3. **`epoch` 一词三个含义**：[03 §1.1](../chatgpt/03-consistency-kernel.md) 的 `Epoch` 是**硬件资源 / 放置代际**，[architecture/02 §1.1](../architecture/02-storage-engine.md) 的 `epoch` 是**语义日志的因果时钟**，[architecture/05 §1.2](../architecture/05-ai-native-features.md) 的 `epoch` 是**按 writer 计数的因果令牌**。三者在文档里同名，实现期必然互相串用。

## 决策

1. **就对象「是否已提交」这一判定：内核 xid 模型是唯一权威。**

   SemLog 的 epoch 与因果快照回答的是**另一个问题**：一个 Agent 应该看见**哪些已提交**的内容（可见性的**范围**）。它不回答「这条内容提交了没有」。两者是「范围」与「存在」的关系，不是两套彼此竞争的真值。

   形式化：

   ```
   ∃ 已提交事实 F（由内核 xid + TxnStatus 判定，03 §1.1 R3）
        ↓  ★ 层：F 被分配到 epoch 并追加进 SemLog
   ∃ SemLog 记录 R（携带 epoch）
        ↓  ☆ 层：因果快照决定该 Agent 能否看见 R
   Agent 的结果集
   ```

   ★ 层**只有一个来自内核的入口**：对象提交路径（`TransactionManager::commit` → `ObjectManager` 发布 → SemLog 追加）。`SemLog` 只可能看到「已提交」的对象，不存在「SemLog 里有、内核里没有」的记录。这一条把 06 §2.2 第 4 条（按 `TxnStatus` 分三档）确立为**唯一**的提交判定。

2. **追加顺序定死：WAL 提交记录先持久，SemLog 追加在后。**

   ```
   1. payload → data barrier            （04 §4.2 E1）
   2. WAL: TxnCommit → flush             （提交事实在这里产生）
   3. SemLog: 追加语义记录（含 epoch）    （★ 层，崩溃时可能缺失）
   4. 返回 Commit OK
   ```

   第 3 步发生在 `TxnCommit` 已持久之后、返回客户端之前。理由是崩溃语义：若 SemLog 追加在 `TxnCommit` 之前，崩溃可能留下「SemLog 有、WAL 无」的记录——那是一条**无法被撤销**的幽灵事实（SemLog 是 append-only，且按 ADR-0003「记录本身即已提交事实」，没有任何机制能标记它无效）。反向的情况（WAL 有、SemLog 无）是安全的：对象已提交、可见，只是语义日志缺一条，可由内核 WAL + payload 重建补齐。

3. **`epoch` 不参与任何内核可见性判定，也不进入索引的覆盖判定。**

   - [03 §1.1 R3](../chatgpt/03-consistency-kernel.md)（「可见性判定只使用 xid + txn status」）**不变**，不因本 ADR 而放宽。
   - [06 §3.1](../chatgpt/06-index-recovery.md) 的覆盖判定 `S.xmin ≤ index.txn_watermark` **不变**；不引入 `epoch` 参与。
   - 索引条目带 `created_by_xid` / `deleted_by_xid`（[06 §2.3](../chatgpt/06-index-recovery.md)），**不带** epoch。epoch 只存在于 SemLog 记录与因果快照里，属于执行层。

4. **两个术语改名，`epoch` 一词不再有三义。**

   | 现名 | 新名 | 含义 | 生效位置 |
   |---|---|---|---|
   | [03 §1.1](../chatgpt/03-consistency-kernel.md) 的 `Epoch` | **`PlacementEpoch`** | 硬件资源 / 数据放置的粗粒度代际 | 内核（迁移代际、内存回收） |
   | [architecture/02 §1.1](../architecture/02-storage-engine.md) 的 `epoch` | **`sem_epoch`** | SemLog 记录的因果时钟，由存储层分配 | SemLog 记录头（已有 8 字节字段） |
   | [architecture/05 §1.2](../architecture/05-ai-native-features.md) 的 `epoch` | **`causal_token`** | 每个 writer 的因果计数，构成 `seen_epoch_by_writer` 的取值 | 执行层因果快照 |

   **改名不是纯文档动作**：`PlacementEpoch` 已经是冻结清单 F1 之外的内核概念（[03 §1.1](../chatgpt/03-consistency-kernel.md) 表中「由 checkpoint 持久」），需要在代码里同步；`sem_epoch` 就是 [architecture/02 §2.2](../architecture/02-storage-engine.md) 记录头里那个已经存在的 8 字节 `epoch` 字段，**布局与偏移都不变**，只是改名字（该字段的**值**在决策 6 里另有 `commit_xid` 伴随字段，那是新增而非重排）；`causal_token` 只在执行层出现。

5. **「以 SemLog 为权威重放源」被明确否决。**

   若 SemLog 是权威重放源，需要它同时满足「有提交事实」与「崩溃后可判定完整边界」。它两条都不满足：没有 `TxnCommit`（ADR-0003 的设计前提就是「记录本身即已提交」），也没有 WAL 那种「帧 + CRC + 尾截断」的可判定末尾（[03 §2.2](../chatgpt/03-consistency-kernel.md) 的扫描终止语义）。因此：

   - **索引追赶的权威输入**是内核 WAL + `ChangeFeed`（[ADR-0012](0012-changefeed-freeze.md)），不是 SemLog；
   - **SemLog 仍是索引重建时 payload 的一个来源**（它能给出完整对象内容），但它与 payload 存储是**并列**的候选来源，不是唯一来源；
   - 两者冲突时以「内核对象层实际读到的版本」为准（`resolve_version` + `read_version`，[04 §3.2](../chatgpt/04-kernel-interfaces.md)），因为那才是查询会返回的东西。

6. **`sem_epoch` 与 xid 的关联方式：记录但不推导。**

   SemLog 记录头**增加一个可选字段 `commit_xid`**（写入时由内核事务上下文提供，非事务写入填 0）。它只用于**诊断与审计**（「这条语义记录属于哪次提交」），**不用于可见性判定**——任何用 `commit_xid` 反推可见性的实现都违反决策 3。

   版本兼容沿用 [03 §2.2](../chatgpt/03-consistency-kernel.md) 的同一套做法：记录头带 `format_version` 与 `flags`（[architecture/02 §2.2](../architecture/02-storage-engine.md) 的布局里已经有 `flags` 2 字节），`commit_xid` 是否存在由 `flags` 位标记。v1 记录没有该字段，读取时按 0 处理；v2 写入的记录在 v1 读取器眼里是「未知标志位 → 拒绝读取」而不是静默忽略——SemLog 记录自带 CRC，宁可拒绝也不猜。

## 实现落点（MVP）

1. **`PlacementEpoch` 在 MVP 内核里没有落成类型**。MVP 没有迁移、也没有分代内存回收（[README §7](../../README.md) 第一版不做迁移），放置代际的唯一消费者 `Reclaimer` 的第 3 条判定直接视为满足（`src/kernel/gc.rs`）。因此代码里不存在「名不副实」的 `Epoch` 类型可改——改名生效的位置在文档与将来的迁移实现里。这与决策 4「改名不是纯文档动作」不冲突：**一旦**迁移实现落地，它必须新建 `PlacementEpoch`，而不是沿用裸 `epoch` 这个名字。

2. **内核侧无需改动即可满足决策 3**：索引条目携带的 `created_by_xid` / `deleted_by_xid` 已经由 F16 的 `ObjectChange` 与 F17 的 `EntryVisibility` 承载（`EntryVisibility` 只有这两个字段，见 [ADR-0015](0015-index-segment-reclamation.md) 实现落点 2），没有任何途径把一个 `sem_epoch` 混进可见性判定。

3. **`sem_epoch` 与 `commit_xid` 属于 SemLog，不属于本仓库的内核 MVP**。SemLog 的格式（[architecture/02 §2.2](../architecture/02-storage-engine.md)）与 `flags` 位兼容规则就绪前，内核不引入这两个字段——决策 6 是给 SemLog 写入方的契约，不是内核 WAL 的格式变更。

4. **追加速率的边界**：内核只承诺「`TxnCommit` 持久后才返回 `CommitReceipt`」（[04 §8 T2](../chatgpt/04-kernel-interfaces.md) 第 5 步）。SemLog 追加发生在 `commit` 返回之后，由执行层负责；`semlog_lag_records` 的可观测口径因此也归执行层。

## 备选方案

1. **以 SemLog 为权威重放源，内核 WAL 降为「元数据缓存」** → SemLog 没有提交事实，也没有可判定的持久末尾；索引追赶会失去「哪些事务已提交」这个前提。改为内核 WAL 权威（决策 5）。
2. **把 `epoch` 映射成 xid 的一个函数（例如 `epoch = f(xid)`）** → 强行把「因果范围」压成「提交顺序」，[architecture/05 §1.2](../architecture/05-ai-native-features.md) 的「Agent A 不必看见 Agent B 的写入」就无法表达；且函数一旦被依赖，再改 `SemLog` 的分配策略就是破坏性变更。改为两者不映射，只承认「存在」与「范围」两层（决策 1）。
3. **在索引条目上同时存 `epoch` 与 xid** → 两个可见性维度同时进索引，[06 §3.1](../chatgpt/06-index-recovery.md) 的覆盖判定要重新推导，且与 [03 §1.1 R3](../chatgpt/03-consistency-kernel.md) 冲突。改为只有 xid（决策 3）。
4. **SemLog 追加放在 `TxnCommit` flush 之前（写入更快）** → 崩溃可能留下无法撤销的幽灵事实。改为之后（决策 2）。
5. **SemLog 追加与提交做成同一事务（两阶段）** → 需要跨两本日志的提交协议，第一版单机单副本不值得，且它会把 [03 §1.3](../chatgpt/03-consistency-kernel.md)「提交成功 = 依赖已持久」的定义扩大到「两本日志都持久」。改为「WAL 先行、SemLog 可补」，补齐路径由重建提供（决策 2 的后果）。
6. **保留 `epoch` 一词，靠上下文区分** → 三处同名不同义已经在文档里造成过串用（06 §1.1 就区分了四条水位线以避免同类问题）。改为改名（决策 4）。
7. **`commit_xid` 用于可见性判定** → 与决策 3 直接冲突，会让内核出现第二套可见性。改为仅诊断用途，并在代码评审中作为拒绝项。
8. **不记录 `commit_xid`** → 出现「SemLog 与对象层不一致」时无法定位是哪次提交造成的，排障只能靠时间戳猜测。改为记录但不用。

## 后果

**正面**

- 「已提交」只有一个判定（xid），「能否看见」只有一个判定（因果快照），两者不再互相冒充。
- 06 的三阶段追赶算法、幂等键、屏障规则**全部不变**——本 ADR 确认而不是改写它们。
- 崩溃语义是明确的：SemLog 可能缺记录（可补），永不会多出记录（不可撤销）。
- 三个 `epoch` 改了名，实现期不会再把放置代际写进因果令牌。

**负面**

- WAL 与 SemLog 之间仍有一个窗口（`TxnCommit` 持久了、SemLog 追加未完成）。这个窗口内的记录只能靠重建补齐，不是「两本日志严格一致」。
- 改名涉及内核代码（`PlacementEpoch`）与架构文档，是一次横跨多文件的重命名。
- `commit_xid` 是 SemLog 格式的一个新字段，需要版本兼容处理。

**缓解**

- 窗口的可观测化：`semlog_lag_records`（已提交但尚未追加进 SemLog 的对象数）作为运维指标，持续增长说明 ★ 层出了故障。
- 回放式补齐只在「索引全量重建」与「审计核对」两条路径上发生，不在正常读路径上，因此不引入稳态开销。
- `commit_xid` 用 `flags` 位标记存在性：老记录按 0 处理，新记录被老读取器遇到时拒绝而不是猜。代价是升级期的读取兼容要显式验证一次。
