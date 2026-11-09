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

> 当前协调区只在内存里（引擎持有一份）；按 ADR-0013 落 WAL 是下一步，见
> [README 下一步](README.md)与 [usage.md](usage.md#已知限制)。

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
3. record_manifest      写 0x31 证据（ADR-0013 决策 3）
4. manifest.publish     进清单，拿到 Advance 票据
5. manifest.advance     CAS 推进 watermark / txn_watermark
```

`MaintenanceReport` 给出：`segments`、`changes`、`advanced_to`、`txn_watermark`、`blocked`、
`rebuild_required`、`cas_conflicts`。

边界：

- **屏障路径**：`Blocked { resume_at }` 时会用 `scan_changes(from, resume_at)` 把屏障之前
  已提交的变更取出来照常发布，然后结束本轮（不丢已提交的写入）。
- **批次尾**：有 `next` 用 `next`；没有 `next` 用 `object_watermark().min(to).max(from)`。
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

- 协调区不持久化；进程重启后索引从空清单重建（引擎 `rebuild_indexes` 重建的是向量 /
  文档缓存，不是 `SegmentManifest`）。
- 追赶在调用线程同步执行，没有后台线程与独立 I/O 优先级。
- 只有 `MetaIndex` 一种 `IndexKind`；向量索引仍是引擎内的暴力扫描，不是索引段。
