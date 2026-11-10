# 内核实现（`src/kernel`、`src/wal`、`src/types.rs`、`src/error.rs`）

设计依据：[04-kernel-interfaces.md](../chatgpt/04-kernel-interfaces.md)、
[03-consistency-kernel.md](../chatgpt/03-consistency-kernel.md)。本文只写**代码怎么落**。

## 1. 文件对照

| 文件 | 内容 |
|---|---|
| `src/types.rs` | 冻结的标识符（`ObjectId` / `VersionId` / `TransactionId` / `Lsn` / …）、`Snapshot`、`ObjectView`、`DeviceHealth`、`ResourceStats` |
| `src/error.rs` | 四分类 `KernelError`（NotFound / Conflict / Retryable / Corrupted）+ `InvalidArgument` / `Io` / `Internal` / `ResourceExhausted` / `Unsupported`、`ConflictKind`、`CorruptionKind` |
| `src/wal/mod.rs` | `RecordType`（0x01–0x03 事务、0x10–0x12 对象、0x20–0x23 布局、0x30–0x31 索引、0x40+ 维护）、`WalBody`、`WalManager` trait |
| `src/wal/device.rs` | `DeviceWal`：固定 4KB 段头、36 字节帧头、增量 CRC 扫描、崩溃尾部语义 |
| `src/kernel/mod.rs` | `Kernel`、各 trait 的 impl（ObjectManager / TransactionManager / CheckpointManager / RecoveryManager / Reclaimer）、`KernelConfig`、`ReactorSpec` |
| `src/kernel/state.rs` | `KernelState`（对象表、事务状态表、extent 表、检查点编解码） |
| `src/kernel/txn.rs` | `Snapshot`、`TxnCtx`、`version_visible`、`object_visible` |
| `src/kernel/object.rs` / `extent.rs` | 对象与 extent 的读写 |
| `src/kernel/feed.rs` | F16 `ChangeFeed`：游标分页 + 未决事务屏障 + 截断水位显式失败 |
| `src/kernel/live.rs` | F17 `LiveSnapshotSource`：存活快照集合 + 唯一一份可见性判定 |
| `src/kernel/checkpoint.rs` | 检查点发布与加载 |
| `src/kernel/gc.rs` | 对象回收器的六条件 |

## 2. 打开内核

```rust
let kernel = Kernel::open(KernelConfig::new("ckpt.snap"), device)?;   // 内部建 SyncIoExecutor
let kernel = Kernel::open(config_with_reactor, device)?;             // config.reactor.is_some()
let kernel = Kernel::open_with_io(config, io)?;                      // 测试夹具挂注入执行器
```

`Kernel::open` 之后依次做：打开 WAL → 加载检查点 → **重放 WAL**（`RecoveryManager::recover`）。
重放时未提交事务被中止，其版本状态置 `Aborted`，不会被任何快照看到。

`KernelConfig` 字段：

| 字段 | 作用 |
|---|---|
| `database_id` | `ObjectId` 的逻辑库号 |
| `wal_base_offset` / `wal_segment_size` / `wal_segment_count` | WAL 区位置与容量 |
| `payload_base_offset` | 对象 payload 区起点（必须 ≥ WAL 区末尾） |
| `checkpoint_path` | 检查点影子文件 |
| `clock` | 可注入时钟（内核读它，不读系统时钟） |
| `reactor` | `Option<ReactorSpec>`：`Some` 时用 reactor 执行设备操作，见 [hal.md](hal.md#reactor) |

## 3. 对象与版本

```rust
let mut txn = TransactionManager::begin(&k, IsolationLevel::Snapshot)?;
let id = ObjectManager::create_object(&k, &mut txn, &CreateObjectRequest {
    object_type: ObjectType::Document,
    payload: bytes,
    component_count: 1,
})?;
TransactionManager::commit(&k, &mut txn)?;
```

- **版本**：同一对象每次更新产生一个新 `VersionId`，旧版本保留（MVCC）。`stage_version`
  只把新版本挂成 `Pending`，提交后才可见。
- **删除**：`delete_object` 给可见版本设 `xmax = 本事务 xid`，不物理删除版本。
  旧快照仍能看到删除前的版本，新快照看不到——两个方向由同一段 `version_visible`
  实现，没有第二份判定。
- **读**：`resolve_version(id, snap)` 取快照下可见版本；`read_version(id, ver)` 取指定版本
  的视图（索引构建器用它，**不走快照**）。
- **提交顺序**：payload → 数据 barrier → WAL → commit 记录 → 发布（04 §8）。

## 4. 快照语义（实现细节，重要）

`fresh_snapshot()`（`src/kernel/mod.rs:589` 附近）：

```text
xmax = state.xmax.max(1)                 // 下一个要分配的 xid
xmin = min(active xids)  或  xmax（没有活跃事务）
active = 当前所有 Running 事务的 xid
```

- `begin()` 在**取到状态锁的第一行**就推进 `xmax = xid + 1`，然后才采样快照
  `xmax = xid`。这样「分配了 xid 但没走到 begin 结尾」的事务不会落在后续快照的
  `xmax` 之下（它永不出现在变更流里，落在下面会让覆盖判定失真）。
- 快照 `xmax` 只声明「`xid < xmax` 的事务都已在 WAL 里出现过」，不声明「都已提交」。
  所以**不能**用新快照的 `xmin` 直接判「索引是否滞后」：`xmin` 是活跃事务的下界，
  不是已提交事务的上界。索引侧的口径见 [index.md](index.md#新鲜度与覆盖)。
- 事务存活期间，其快照登记在 `live`（F17）；`LiveSnapshotHandle::capture` 交出去的
  快照同样登记，`release` / `Drop` 时注销。

## 5. 变更流（F16）

```rust
trait ChangeFeed {
    fn object_watermark(&self) -> Lsn;                       // 最后一条已持久记录
    fn scan_changes(&self, from: Lsn, to: Lsn, spec: &ChangeBatchSpec) -> KernelResult<ChangeBatch>;
    fn scan_objects(&self, snap: &Snapshot) -> KernelResult<Vec<ObjectId>>;
}
```

`object_watermark()` 就是 `WalManager::durable_lsn()`。它的口径（`src/wal/device.rs`）：

- 本进程 flush 过 → 返回最近一次 `flush_upto` 覆盖到的位置；
- 本进程没 flush 过（刚打开 / 刚重启）→ 返回**打开时恢复到的日志尾**。盘上已有的
  字节本来就是持久的；返回初始值 0 会让重启后读不到任何派生记录。

实现要点（`src/kernel/feed.rs`）：

- 游标是**包含式起点**，永远是记录边界（`scan_from` 给出），调用方不得自行推算帧长。
- `Committed` 应用；`Aborted` 跳过；`Running` / `InProgress` **停住游标**并
  `blocked_by_pending_txn = true`（屏障优先于容量，哪怕本批还没满）。
- 空批不推进游标；`from < min_required_lsn` 返回 `Unsupported`——这就是「必须全量重建」
  的信号，绝不允许扫空区间后推进。
- 一次创建产生的 `ObjectCreate` + 首个 `ObjectVersion` 折叠为一条 `Created`（否则
  每个新对象会在索引里先 insert 再「删旧 + insert」）。
- `scan_objects` 与 `resolve_version` 共用 `txn::object_visible`，没有第二套可见性。

## 6. 检查点、恢复与校验

```rust
let ckpt = CheckpointManager::trigger(&k)?;          // 发布检查点
let info = CheckpointManager::current(&k)?;
let rec = RecoveryManager::recover(&k)?;             // 重启时由 open 自动调用
let report = RecoveryManager::verify(&k, VerifyScope::Full)?;
```

`RecoveredState { checkpoint, replayed_records, aborted_txns }`。检查点把
`object_watermark` 前移到覆盖位置，进而可能把 `min_required_lsn` 推高——索引追不上
时就会出现 [index.md](index.md#must-rebuild) 说的全量重建。

## 7. 回收

`Reclaimer::scan(budget)` 找候选，`judge` 按六条件判定，`reclaim` 执行。
索引段的回收**不走这里**，它在索引层（[index.md](index.md#段回收)）。

## 8. 已知差距

- 写路径一把 `writer` 锁串行化（03 §2.5 的组提交与每对象意向锁未实现）。
- `TransactionManager` 没有「枚举未决事务」的接口，索引 `tx_w` 因此只是下界
  （见 [index.md](index.md#新鲜度与覆盖)）。
- 迁移与 `MemoryTierManager` 只有 trait 形状。
