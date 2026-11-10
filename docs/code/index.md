# 索引层实现（`src/index`）

设计依据：[06-index-recovery.md](../chatgpt/06-index-recovery.md)、
[08-index-layer.md](../chatgpt/08-index-layer.md)、[ADR-0012](../adr/0012-changefeed-freeze.md)、
[ADR-0013](../adr/0013-index-manifest-in-wal.md)、[ADR-0015](../adr/0015-index-segment-reclamation.md)。

索引是**内核之外的调用方**：删光所有段从对象目录重建，结果必须等价。

## 1. 部件与文件

| 部件 | 文件 | 一句话 |
|---|---|---|
| CatchUp | `catchup.rs` | 读一批、撞屏障停、截断则要求重建 |
| IndexKind | `kind.rs` | 把一条 `ObjectChange` 变成动作，纯函数 |
| SegmentBuilder | `builder.rs` | 建段（自校验）、算幂等键、写 0x31 证据 |
| Coordinator | `manifest.rs` | 已发布段 + 两个水位，CAS 单写者 |
| Reclaimer | `reclaim.rs` | G1–G6 逐条目判定 |
| Freshness | `freshness.rs` | 覆盖判定、代价模型、`tx_w` 推进规则 |
| Tracker | `tracker.rs` | 把上面五个串成一轮追赶循环 |
| ManifestLog | `manifest_log.rs` | 协调区落 WAL（0x31）与重启还原 |

## 2. 数据形态

```rust
pub struct IndexEntry {
    pub kind: EntryKind,                 // Live | Tombstone
    pub object: ObjectId,
    pub version: VersionId,
    pub created_by_xid: TransactionId,
    pub deleted_by_xid: Option<TransactionId>,
    pub payload: Vec<u8>,                // 由 IndexKind 决定（MetaIndex = 类型分桶）
}

pub struct Segment { pub index_id: IndexId, pub from_lsn: Lsn, pub to_lsn: Lsn, pub entries: Vec<IndexEntry> }
```

- 主键是 `(object, version)`，**多版本共存**是正常状态；`Segment::decode` 校验逐条目
  CRC 与整段摘要，失败返回 `Corrupted`。
- 墓碑的删除事务记在 `created_by_xid`（`EntryVisibility::tombstone(xid)`），
  `deleted_by_xid = None`——两个方向（旧快照看活条目、新快照看墓碑）由两个条目表达，
  不能合并成一个。
- `SegmentId::derive(index_id, from_lsn, to_lsn, content_crc)` 是幂等键：同区间同内容
  重算得到同一个 ID，崩溃后重扫不会在清单里出现两条。

## 3. 协调区（`manifest.rs`）

```rust
pub struct SegmentManifest {
    pub index_id: IndexId,
    pub published_segments: Vec<PublishedSegment>,
    pub watermark: Lsn,                 // 已消费到的 LSN（运维 / 回收用）
    pub txn_watermark: TransactionId,   // tx_w（覆盖判定用）
    pub baseline_segment_id: Option<SegmentId>,
}
```

- **先段后水位**做成类型上不可表达的反例：推进水位只有 `advance(&Advance, expected, txw)`，
  而 `Advance` 票据只能由 `publish()` 发出。
- `advance` 是 CAS：`expected` 不等于当前 `watermark` 返回 `CasConflict`，水位只增不减。
- `publish` 去重；先发布的段成为基线段。`discard_orphans` 给出「不在清单」的段供回收。

### 3.1 持久化（`manifest_log.rs`）

协调区每发布一批段写**两次** `0x31`，并在推进后再写一次，记的分别是：

```text
persist(wal, &manifest) -> KernelResult<Lsn>              // 经 append_derived，DerivedFamily::Index
restore(wal, index_id) -> KernelResult<Option<SegmentManifest>>
```

| 写入时机 | 记录里的水位 | 崩溃在这里之后 |
|---|---|---|
| 段已发布、水位未推进 | 发布前的旧水位 | 重启后从旧水位重扫，重复应用（幂等） |
| `advance` 之后 | 推进后的水位 | 重启后的起点就是下一次发布的起点 |

- 记录体复用 ADR-0013 的 `IndexManifest { index_id, segment_id, from_lsn, to_lsn,
  watermark_xid, watermark_lsn }`。快照记录的 `segment_id` 全零、三个 LSN 字段都等于
  **水位本身**；builder 的证据记录 `from_lsn` 是段起点。`restore` 据此区分两类记录，
  不引入新的记录类型。
- `restore` 只认 `durable_lsn` 之前的记录，取该索引最新一条；没有记录返回 `None`，
  调用方从空清单开始。记录体不自洽（三个 LSN 不相等）返回 `Internal(InvariantViolation)`。
- 还原结果只含两个水位与 `baseline_segment_id`，**不含段列表**。
- 记录**不是发布事实**（ADR-0013 决策 3）：内核不解析它，恢复期不 redo。它只让
  **索引层自己**在重启后接着追，见 [usage.md](usage.md#让查询带上完整性标记)。
- 持久化要等下一次 WAL flush。引擎在 `catch_up_index` 末尾主动 flush 一次，让刚写的
  快照成为可还原的水位；在它之前掉电，WAL 里最多回到上一条已持久的记录——水位落后，
  不会越界。

> `durable_lsn` 在**本进程没 flush 过**时取日志尾（打开时恢复到的位置）：盘上已有的
> 字节本来就是持久的。修正前它取打开时的初始值，重启后读不到任何派生记录。

## 4. 构建（`builder.rs`、`kind.rs`）

```rust
build_segment(&kind, &kernel, 1, from_lsn, to_lsn, &changes) -> Result<SealedSegment, SealError>
record_manifest(wal, &sealed, tx_w) -> KernelResult<Lsn>       // 写 0x31，只作证据
```

- 构建器读的是**变更指定的那个版本**（`version_xids`），不是最新版本；取不到视图返回
  `SealError::MissingVersion`，不静默跳过。
- `Delete` 动作不带创建者，跨段时回内核补 `created_by_xid`，否则 G3 判错。
- 同一主键多条动作**合并**：保留非零的创建者、`deleted_by_xid` 写过后不清空、墓碑优先。
- 0x31 是派生事实，恢复期不参与 redo，只用于 T4（WAL 段能否物理删除）。

## 5. 追赶循环（`tracker.rs`）

```rust
let mut manifest = SegmentManifest::empty(IndexId(7));
let mut rate = ChangeRate::default();
let report = catch_up(&kind, &kernel as &dyn MaintenanceHost, kernel.wal(),
                      &mut manifest, &ChangeBatchSpec::unbounded(),
                      CatchUpLimits::to_watermark(), &mut rate)?;
```

每批五步，顺序不可交换：

```text
1. catchup::step        读一批已提交变更（撞屏障 → Blocked，返回本批 + blocked=true）
2. build_segment        建段
3. persist              协调区快照（发布后、推进前）——`manifest_log::persist`
4. record_manifest      段证据——`record_manifest`
5. advance              CAS 推进 watermark / txn_watermark，成功后再 persist 一次
```

第 3–5 步在代码里是：`clone` 清单 → `publish` 得到票据 → `persist` 写快照 →
`record_manifest` 写证据 → `advance`（CAS）→ 成功则再 `persist` 一次。任何一步崩掉，
WAL 里都不会出现「水位已推进、段却没记下」的状态。

`catch_up_observed` 与 `catch_up` 相同，额外在每批发布后回调 `on_publish(&SealedSegment)`：
部署层用它把段字节落盘并原子 rename（08 §2.3 第 2/4 步）。本仓库的 `engine` 只
在内存里消费段，不调用它。

`MaintenanceReport` 给出：`segments`、`changes`、`advanced_to`、`txn_watermark`、`blocked`、
`rebuild_required`、`cas_conflicts`。

边界：

- **屏障路径**：`Blocked { resume_at }` 时会用 `scan_changes(from, resume_at)` 把屏障之前
  已提交的变更取出来照常发布，然后结束本轮（不丢已提交的写入）。
- **批次尾**：有 `next` 用 `next`；没有 `next` 用 `object_watermark().min(to).max(from)`。
  注意 `object_watermark()` 是**本批发布前**读到的持久水位，批次尾因此不会越过它。
  **不能**直接用调用方给的 `to`——`to` 可能是 `Lsn::MAX`，越过持久水位就会把没落盘的记录
  算作已消费。
- <a id="must-rebuild"></a>**截断**：`catchup::step` 遇到 `from < min_required_lsn` 返回
  `RebuildRequired`，`catch_up` 把它放进 `report.rebuild_required` 并**不推进任何水位**
  （A9）。调用方必须走全量重建。
- `MaintenanceHost` = `KernelLike`（取版本 xid）+ `ChangeFeed` + `txn_status` + `change_feed()`。
  最后一个是回避 trait upcasting 的（MSRV 1.75 不支持 `&dyn Sub` → `&dyn Super`）。

<a id="新鲜度与覆盖"></a>
## 6. 新鲜度与覆盖（`freshness.rs`）

```rust
pub enum FallbackStrategy { FullScan, SparseAssist, Wait }

pub struct IndexFreshness {
    pub txn_watermark: TransactionId,
    pub lsn_watermark: Lsn,
    pub lagging: bool,
    pub estimated_lag_objects: Option<u64>,
}
```

三个入口，语义不同：

| 函数 | 结论 | 何时用 |
|---|---|---|
| `IndexFreshness::covers(&manifest, &snap)` | `snap.xmin ≤ txn_watermark` | 06 §3.1 的字面判定（A1），只读 xid |
| `of_manifest(&m, _)` | 保守：`lagging = true` | 只想要两个水位时 |
| `evaluate(&m, &snap, object_watermark, &rate, &model)` | `lagging = !covers`，并给出缺口估计 | 需要完整新鲜度时 |

**口径提醒**（实现侧的重要事实）：内核 `fresh_snapshot` 的 `xmin` 是「活跃事务里最小的
xid」，没有活跃事务时等于 `xmax`——它**不是**已提交事务的上界。所以：

- 查询侧的精确判定用 `tx_w + 1 ≥ snap.xmax`（快照可见版本的 `created_by_xid < S.xmax`），
  见 [engine-intent.md](engine-intent.md#覆盖标记)；
- 不要拿刚追平之后新取的快照的 `xmin` 去判 `lagging`，它总会比 `tx_w` 大 1。原因：`tx_w`
  只推进到区间里最后出现的 xid，而快照的 `xmax` 在事务分配 xid 时就前进。

`tx_w` 推进规则（`next_txn_watermark`）：区间里出现过的 xid 中，取「非终态」的最小者减一；
没有未决事务时取已见最大 xid。`InProgress`（未知）按未决处理。**这是下界**：不在区间里的
更小未决 xid 不会被看到，因为内核不提供枚举未决事务的接口。

代价模型（`CostModel` + `ChangeRate`）：

```text
n_lag ≈ ceil( lag_bytes / bytes_per_change × safety_factor )
bytes_per_change = 本进程观测的 Σ区间字节 / Σ变更条数
```

没有样本时 `estimate` 返回 `None`——**不许**用默认常数冒充测量值。估计值只影响策略选择与
`missing_estimate`，不参与任何正确性判定（06 §3.2）。

<a id="段回收"></a>

## 7. 段回收（`reclaim.rs`）

```rust
let verdict = reclaimable(&segment, &manifest, &live_source, gen_advanced, newer_refs);
```

G1–G6 逐条目判定（含基线段与继任者、逐条目快照可见性、墓碑、引用集、代次、先摘链后删文件）。
读失败按「可见」处理（保守保留）。回收器归索引层，**不复用**内核 `Reclaimer`。

<a id="已知差距"></a>
## 8. 已知差距

- 协调区还原出来的清单**不含段列表**：对追赶与覆盖判定够用（只用两个水位），
  段回收要用完整列表时得另外重建。
- 每批写两次快照记录，WAL 里 `0x31` 的数量是段数的两倍；T4 判定读的是全部 `0x31`，
  快照记录的 `segment_id` 全零不对应任何段，不应计入。
- `persist` 不 fsync：重启后可能回退到上一条已持久的快照（安全，但会重扫一段）。
- 引擎不落盘段文件：`Engine` 的内存段在重启后不还原，重启时 `with_index` 只恢复
  清单，若再需要段材料要走全量重建。
- 追赶在调用线程同步执行，没有后台线程与独立 I/O 优先级。
- 只有 `MetaIndex` 一种 `IndexKind`；向量索引仍是引擎内的暴力扫描，不是索引段。
