# 08 — 索引层设计（内核之外）

> 对应：[06-index-recovery.md](06-index-recovery.md) 的算法部分、[ADR-0012](../adr/0012-changefeed-freeze.md) / [ADR-0013](../adr/0013-index-manifest-in-wal.md) / [ADR-0015](../adr/0015-index-segment-reclamation.md) 的冻结边界。
> 本文只回答一个问题：**一个按 06 写的索引层，实际由哪几个部件组成、每个部件读什么写什么、崩溃时谁负责收拾。**

## 一、边界：索引层是一个内核之外的调用方

索引是派生数据（[06 §1.2](06-index-recovery.md)）：删掉全部索引段、从对象目录全量重建，结果必须与删除前等价。这决定了它与内核的关系只能是调用方——不能是内核的一部分，也不能反向持有内核的内部状态。

```
┌──────────────────── 索引层（本进程或独立进程）─────────────────────┐
│                                                                    │
│   CatchUp ──► SegmentBuilder ──► Coordinator ──► Reclaimer         │
│      │              │                 │              │             │
│      │ 变更          │ 条目 + 墓碑     │ 段清单        │ G1–G6       │
│      ▼              ▼                 ▼              ▼             │
│   [IndexKind 实现]：向量 / 倒排 / 标量 / 图 / 时间线                │
└───────────────┬────────────────────────────────────────────────────┘
                │ 三条冻结的边（04 §12.2）
     ┌──────────┴───────────────┬─────────────────────────┐
     ▼                          ▼                         ▼
ChangeFeed::scan_changes   WalBody::append_derived   LiveSnapshotSource
   （读已提交变更）          （写 0x30 / 0x31）         （活快照 + 可见性）
```

内核没有任何路径回调索引层。这条规则的直接后果是：**索引层的任何状态都必须能由它自己从内核的三条边重新推导出来**——段清单在协调区、条目在段文件里、水位在 `IndexManifest` 与协调区里。凡是「只有内存里知道」的状态，重启后都必须可重建。

## 二、五个部件

| 部件 | 输入 | 输出 | 崩溃后如何恢复 |
|---|---|---|---|
| **CatchUp**（追赶器） | `ChangeFeed::scan_changes(from, to, spec)` | 内存中的 `Vec<ObjectChange>` 分批 | 无状态；从协调区读 `index.watermark` 重新出发 |
| **IndexKind**（索引种类） | 一个 `ObjectChange` + 对象视图 | 条目 / 删除 / 墓碑动作（06 §2.3 表格） | 无状态；纯函数 |
| **SegmentBuilder**（段构建器） | 条目流 | 临时段文件 → fsync → rename | 临时段是垃圾，启动时删除 |
| **Coordinator**（协调区） | 段发布、水位推进 | 段清单 + `index.watermark` + `tx_w` | 权威来源；用 CAS 单写者保证 |
| **Reclaimer**（段回收器） | 段清单 + `LiveSnapshotSource` | 摘链 → 删文件 | 无状态；每轮重新判定 |

### 2.1 CatchUp：只做三件事

1. **读**：`scan_changes(cursor, to, ChangeBatchSpec)`，一次一批，游标来自上一批的 `next_cursor`；
2. **停**：`blocked_by_pending_txn = true` 时本轮结束，游标**不越过**未决事务；
3. **判**：开扫之前检查 `index.watermark ≥ min_required_lsn`，不满足就转全量重建（06 §2.7）。

它不解析帧长、不读设备、不判定提交——这三件事分别在 `WalManager`、`IoExecutor`、`TransactionManager` 里各有一份实现，复制任何一份都会分叉。运行期 `ChangeFeed` 内部直接用事务状态表判定，所以「Running 事务只停不丢」是内核保证的，索引层不需要知道 `TxnStatus` 长什么样。

**游标是包含式起点**：`next_cursor` 指向下一条未消费记录。追赶器把游标原样存进协调区，不要自己 `+1` 或加上帧长——帧头 36 字节、body 按 8 字节对齐，算错一次就落在帧中间，下一轮扫描会从帧内开始解出垃圾。

### 2.2 IndexKind：变更 → 动作

06 §2.3 的动作表是**按索引种类**给出的，每种索引实现同一个窄接口：

```rust
pub trait IndexKind {
    /// 这条变更对某个版本条目产生什么动作。纯函数：不读设备、不取锁。
    fn plan(&self, change: &ObjectChange, view: Option<&ObjectView>) -> Vec<EntryAction>;
}

pub enum EntryAction {
    Insert { version: VersionId, created_by_xid: TransactionId, payload: Vec<u8> },
    Delete { version: VersionId, deleted_by_xid: TransactionId },
    Tombstone { version: VersionId, deleted_by_xid: TransactionId },
}
```

**条目必须带两个 xid**（06 §2.3）：`created_by_xid` / `deleted_by_xid`。它们是回收器 G3 / G4 的全部输入（见 §2.5），也是 `LiveSnapshotSource::visible` 的唯一参数。没有它们的条目无法判定可见性，只能靠「构建时间」近似——那正是 [06 §1.4](06-index-recovery.md) 明确排除的做法。

**payload 从对象读取**：`ObjectChange` 只带逻辑元数据，向量 / 文本来自 `ObjectManager::resolve_version` + `read_version` + `read_payload` 这条逻辑读路径。构建器读的是**变更指定的那个版本**，不是「当前最新版本」——否则段内容就不只由 `(from_lsn, to_lsn]` 决定，幂等前提失效（06 §2.2）。

### 2.3 SegmentBuilder：先段后水位

```
1. 临时文件写入条目流（每条带 CRC32C，段头带整段摘要）
2. fsync         ── 打点 index.segment.sealed（DeviceSite::IndexSegmentSealed）
3. 计算 segment_id = hash(index_id ‖ from_lsn ‖ to_lsn ‖ index_format_version ‖ content_hash)
4. 原子 rename 为正式段名
5. 写 IndexManifest（0x31，经 append_derived）——**证据**，不是发布
```

第 5 步写的是「这个段存在且已封存」的证据，属于 WAL；发布是协调区的动作（§2.4）。两者顺序不能反：先写清单再封段，崩溃后就会出现一条指向不存在段的清单。

**`IndexManifest` 在恢复期不参与 redo**（ADR-0013 决策 3）。它只在 T4 判定里被读——「这个 `(index_id, segment_id)` 是否已在协调区发布」，用来决定 WAL 段文件能否物理删除。把 0x31 当成发布事实会制造第二个权威来源。

### 2.4 Coordinator：唯一权威，CAS 单写者

协调区保存的是「哪些段已发布」与两个水位：

```
index_state = {
    published_segments: [ { segment_id, from_lsn, to_lsn, entry_count } ],
    watermark:     Lsn,      // 已消费到的 LSN
    txn_watermark: TransactionId,  // tx_w（06 §3.1）
    baseline_segment_id: Option<SegmentId>,  // ADR-0015 G1
}
```

发布流程严格两步，顺序不可交换：

```
1. 把新段加入 published_segments（原子：临时记录 + rename，或追加式日志）
2. CAS 推进 watermark / txn_watermark（期望值 = 本轮读到的旧值）
   CAS 失败 = 有另一个追赶者先推进了 → 丢弃本轮段，不报错
```

| 崩溃点 | 状态 | 恢复动作 |
|---|---|---|
| 1 之前 | 段文件在、清单无 | 孤儿段，回收器按「不在清单」当作垃圾删除 |
| 1 与 2 之间 | 清单有、水位旧 | 下一轮从旧水位重扫，重复应用（幂等），清单里的段继续服务 |
| 2 之后 | 一致 | — |

**方向是固定的**：「水位已推进但段缺失」这一刻是不可能出现的——它正是 06 §2.4 表格里那行「静默丢数据」。任何让水位先走的实现（例如先在协调区推水位再补段）都违反这条。

**同一索引同一时刻只有一个追赶者**：CAS 期望值就是这个约束的实现，不需要额外的租约；抢输的一方丢弃自己的段，代价是一轮白工，收益是任何时刻只有一个 writer 能推水位。

### 2.5 Reclaimer：G1–G6 在代码里的形状

```rust
fn reclaimable(&self, seg: &PublishedSegment) -> bool {
    let idx = self.coordinator.load();
    seg.id != idx.baseline_segment              // G1：基线段（有继任者才可回收）
        && seg.to_lsn <= idx.watermark          // G2：段已被已发布进度覆盖
        && seg.entries.iter().all(|e| !live_visible(e))      // G3：条目对全部活快照不可见
        && seg.tombstones.iter().all(|t| !live_visible(t))   // G4：墓碑同样单独判
        && !self.refs.is_referenced(seg.id, self.reclaim_gen) // G5：回收代次之后的读者不引用它
    // G6 不是判定条件，是执行顺序：先摘链，后删文件
}

fn live_visible(&self, e: &EntryVisibility) -> bool {
    // 唯一实现来自内核（ADR-0015 决策 4），索引层不许自写
    self.snapshots.visible_to_any_live(e).unwrap_or(true)   // 读失败 → 保守：视为可见
}
```

三条容易被写错的细节：

- **G3 与 G4 是两个方向**。旧快照需要「删除前的条目」，新快照需要「墓碑」；一个条目可见就保留整段。判定失败时**保留**（保守方向），因为少回收只浪费空间，多回收会让查询静默少召回。
- **不做部分回收**：段是不可变文件，段内存活条目占比低时走合并（写新段 + 发布 + 回收旧段），复用同一条发布流程（ADR-0015 决策 3）。
- **G5 的引用集与回收代次**（ADR-0015 决策 5）：先推进 `reclaim_gen`、等读者确认、再采样。采样之后开始的读只能看到新清单。

## 三、覆盖判定与新鲜度

查询携带快照 `S`，索引有 `tx_w`。**唯一**的覆盖判定是 `S.xmin ≤ tx_w`（06 §3.1），按 xid 不按 LSN。不覆盖时有三种回退（06 §3.2）：补扫缺口 / 换已覆盖的索引 / 等待；选择依据是代价估计，`n_lag` 来自「缺口区间内已提交变更条数」。

对调用方暴露的新鲜度是一个**连续量**（`(watermark, 内核 object_watermark)` 的距离与 `tx_w` 与 `S.xmin` 的差），不是布尔值——布尔值无法让规划器在「补扫 3 条」与「回退到全表扫描」之间做选择。

## 四、崩溃与重启

| 状态 | 重启后 |
|---|---|
| 临时段文件 | 删除（未 rename 的一律作废，06 §5.1） |
| 已 rename、未进清单的段 | 垃圾；回收器按「不在清单」删除 |
| 清单已更新、水位未推进 | 从旧水位重扫；重复应用幂等 |
| 段文件校验失败 | 该段作废，水位回退到它之前的段尾，重新构建（06 §5.1） |
| WAL 已截断到水位之后 | 全量重建（06 §2.7） |

**重建不是「重放同一批 WAL 得到逐字节相同的段」**：HNSW 图结构、LSM 层划分依赖插入顺序。幂等性由段 ID 的幂等键与发布去重给出，不由内容一致给出（06 §2.5）。因此验收用例要比对**条目集合**，不要比对段字节。

## 五、验收用例（与 06 §6.2 对齐）

| 用例 | 断言 |
|---|---|
| `watermark_never_over_pending` | 未决事务存在时水位不超过其首条记录；事务终结后能越过 |
| `rewind_when_wal_truncated` | 水位 < `min_required_lsn` 时转全量重建，不推进水位 |
| `publish_order_crash` | 任意注入点崩溃后都不存在「水位已推进、段缺失」 |
| `baseline_kept` / `no_partial_reclaim` | G1 不被回收；段内任一条目可见则整段保留 |
| `tombstone_direction` | 旧快照可见条目、新快照可见墓碑（ADR-0015 实现落点 3） |
| `rebuild_equals_incremental` | 全量重建的条目集合 = 增量追赶到同一水位后的条目集合（按 `(object_id, version_id)` 比较） |
| `visibility_no_lsn` | 可见性判定只读 xid 与事务状态 |

## 六、与内核已有实现的关系

本仓库的内核已经提供了本文依赖的全部三条边：

- `ChangeFeed`（F16，`src/kernel/feed.rs`）：游标分页、未决事务屏障、截断水位显式失败；
- `WalBody::append_derived`（ADR-0013，`src/wal/mod.rs` + `src/wal/device.rs`）：族与记录体一致、`xid = 0`、拒绝时零写入；
- `LiveSnapshotSource`（F17，`src/kernel/live.rs`）：存活快照集合与唯一的 `visible` 实现。

`0x30` / `0x31` 的 body 已带 `index_id`（[03 §2.3](03-consistency-kernel.md)），段发布所需字段（`segment_id`、`[from_lsn, to_lsn]`、`tx_w`）已全部落进 `IndexManifestBody`。索引层本身不在内核仓库内——它是本文描述的一个独立调用方。
