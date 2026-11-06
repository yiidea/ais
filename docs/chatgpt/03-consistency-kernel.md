# AINS 存储内核 —— 一致性协议、WAL 格式与崩溃恢复算法

> 本文承接 [02-kernel.md](02-kernel.md) 的第十节：那里冻结了第一版内核的设计决策，
> 本文把这五条一致性约束落实为**可直接编码的协议细节**：WAL 二进制格式、锁分级、
> 内存状态机、checkpoint / recovery 算法、迁移与 GC 的安全边界，以及故障注入测试矩阵。
>
> 状态：设计阶段 · 对应实现蓝图见 [docs/architecture/02-storage-engine.md](../architecture/02-storage-engine.md)
> 与 [ADR-0003](../adr/0003-append-only-log-source-of-truth.md)、[ADR-0005](../adr/0005-three-tier-consistency.md)

本文只讨论**单机单实例内核**。分布式复制、跨节点事务与多副本仲裁不在本文范围（见 [06-distributed.md](../architecture/06-distributed.md)）。

## 一、先确定三条总原则

在讨论任何数据结构之前，先把后面所有算法都依赖的三条原则写清楚。它们与 `02-kernel.md` 第十章的“五条一致性约束”一一对应，这里给出可执行的形式。

### 1.1 原则一：日志是唯一的顺序来源（LSN 权威）

系统里存在多个“顺序”概念，必须严格区分，禁止混用：

| 概念 | 含义 | 权威来源 | 是否持久 | 用途 |
|---|---|---|---|---|
| **LSN** | WAL 逻辑字节偏移，单调不减 | WAL Manager | 是（随 WAL） | 恢复点、映射切换、GC 边界 |
| **Epoch** | 硬件资源 / 数据放置的粗粒度代号 | HRM | 由 checkpoint 持久 | 迁移代际、内存回收 |
| **VersionId** | 对象版本号（对象内单调） | Object Version Manager | 是（随 WAL 记录） | 版本链、可见性判定 |
| **TxnId (xid)** | 事务号 | Transaction Manager | 是（随 TXN 记录） | 提交状态、可见性判定 |
| **MappingVersion** | 逻辑 Extent → 物理 Extent 映射的代次 | Extent Mapping Manager | 是（随 MAPPING 记录） | 迁移并发安全 |

硬性规则：

```
R1. LSN 只描述「写入 WAL 的先后」，不描述提交顺序。
R2. VersionId 只在 ObjectId 作用域内单调，禁止用 VersionId 比较不同对象。
R3. 可见性判定只使用 xid + txn status（以及快照），不使用 VersionId / LSN 大小。
```

R3 是 `02-kernel.md` §4.3 已经强调过的点，这里升级为内核不变量：**任何用时间戳或序号近似可见性的“优化”都必须被代码审查拒绝**。

### 1.2 原则二：地址永不进入逻辑身份（身份 / 位置分离）

```
ObjectId      := (DatabaseId, ObjectSequence)      全局稳定，永不回收复用
LogicalExtent := (ObjectId, ComponentId, ordinal)  逻辑切分，稳定
PhysicalExtent:= (DeviceId, offset, length, epoch) 物理分配，会移动
```

任何持久化结构（Header、索引、元数据页）**只能存 ObjectId / LogicalExtentId / MappingId**，绝不能存裸 LBA 或裸 VA。物理地址只允许出现在 `MappingTable` 与 `FreeSpaceMap` 内部。

这条规则带来的直接结果是：迁移、压缩、去重、副本迁移全部退化为“改映射”，不触碰上层任何数据。

### 1.3 原则三：提交成功 = 依赖已持久

“Commit OK”返回给客户端之前，必须满足：

```
Persist(CommitRecord) ∧ Persist(CommitRecord 的所有前驱)
  ∧ ∀ extents referenced: PayloadPersist(extent) ≥ required durability
```

实现上严禁出现“先返回、后刷盘”的乐观提交路径（第一版不实现 group-commit 语义上的“提交未持久即返回”的弱化模式）。

## 二、WAL 二进制格式

### 2.1 文件布局

WAL 由**固定大小段文件**组成（默认 16 MB，可配置），文件名 `WAL-<SegmentStartLSN:016x>`。

```
WAL Segment (16 MB)
┌────────────────────┬──────────────────────────────────────────────┐
│ SegmentHeader 4 KB │  Record 1 │ Record 2 │ ... │ 尾部 padding      │
└────────────────────┴──────────────────────────────────────────────┘
0                4096                                             16MB
```

`SegmentHeader`：

| 字段 | 类型 | 说明 |
|---|---|---|
| `magic` | `u32` | `0x41494E53`（"AINS"） |
| `format_version` | `u16` | 当前 `1` |
| `flags` | `u16` | 压缩 / 加密标记（第一版均为 0） |
| `segment_start_lsn` | `u64` | 段起始 LSN |
| `prev_segment_start_lsn` | `u64` | 前一段，用于反向扫描 |
| `created_unix_ns` | `u64` | 便于运维定位 |
| `header_crc32c` | `u32` | 覆盖前面所有字段 |
| `padding` | — | 补齐到 4 KB |

段文件头部 4 KB 保留的原因：与 NVMe 原子写粒度、`optimal_io_size` 对齐，保证段头本身不会被部分写入撕裂。

### 2.2 记录格式

所有记录共享同一条外层帧，帧内是类型化 body：

```
Offset  Size  Field
0       4     total_len         记录总长（含帧头与尾对齐），8 字节对齐
4       1     record_type       §2.3 的类型枚举
5       1     flags             FIRST_IN_TXN / LAST_IN_TXN / FPI 等
6       2     reserved
8       8     lsn               本记录起始 LSN（可校验，防错位）
16      8     xid               所属事务，非事务记录填 0
24      8     prev_lsn          同一事务上一条记录的 LSN，0 表示首条
32      8     payload_crc32c    覆盖 body
40      N     body              类型化载荷
...     0-7   padding           对齐到 8 字节
```

记录**不再额外存长度校验字段**：扫描时以 `total_len + lsn + payload_crc32c` 三重校验判定记录完整性。任何一项不符即判定“记录尾部损坏”，扫描在此终止（这是崩溃恢复的正常情况，不是错误）。

选 8 字节对齐而非 4 KB 对齐：WAL 是顺序追加，不享受随机读的块对齐收益；4 KB 对齐会让小记录（TXN_COMMIT 仅几十字节）浪费 100 倍空间，反而在 checkpoint 重放时增加 IO 量。

### 2.3 记录类型

沿用 `02-kernel.md` §5.2 的六类，给出第一版的完整枚举与 body 结构：

```rust
#[repr(u8)]
enum RecordType {
    // 0x01 Transaction
    TxnBegin        = 0x01,
    TxnCommit       = 0x02,
    TxnAbort        = 0x03,

    // 0x10 Object / Version
    ObjectCreate    = 0x10,
    ObjectVersion   = 0x11,   // 生成新版本（COW 后的版本目录更新）
    ObjectDelete    = 0x12,   // 设置 xmax，不是物理删除；body 带被删版本

    // 0x20 Extent / Mapping
    ExtentAlloc     = 0x20,
    ExtentFree      = 0x21,
    MappingCreate   = 0x22,
    MappingSwitch   = 0x23,   // 迁移 / 重映射的原子提交点

    // 0x30 Index（派生数据，可重建）
    IndexDelta      = 0x30,
    IndexManifest   = 0x31,

    // 0x40 Maintenance（系统内部事务）
    MigrationBegin  = 0x40,
    MigrationEnd    = 0x41,
    CompactionBegin = 0x42,
    CompactionEnd   = 0x43,
    CheckpointBegin = 0x44,   // 记录 checkpoint_lsn
}
```

代表性 body：

```rust
struct TxnBeginBody   { isolation: u8, snapshot_id: u64, xmin: u64, xmax: u64,
                        active_xids_len: u32 /* 后接 active_xids */ }

struct ObjectCreateBody { object_id: [u8;16], object_type: u8, tenant_id: u64,
                          schema_id: u32, metadata_ref: StorageRef,
                          component_count: u16 }

struct ObjectVersionBody { object_id: [u8;16], new_version: u64,
                           old_version: u64 /* 0 = 新建 */,
                           component_root: ComponentRoot }

struct ObjectDeleteBody  { object_id: [u8;16],
                          version: u64 /* 墓碑目标的版本（06 §2.6） */ }

struct ExtentAllocBody  { logical_extent: LogicalExtentId, mapping_id: u64,
                          physical_extent: PhysicalExtentId,
                          device_id: u32, device_offset: u64, length: u64,
                          checksum_kind: u8, checksum: u64 }

struct MappingSwitchBody { logical_extent: LogicalExtentId,
                           old_mapping: u64, new_mapping: u64,
                           reason: u8 /* MIGRATION / COMPACTION / REPAIR */ }
```

### 2.4 Payload 不进入 WAL

`02-kernel.md` §5.3 已定：大 Payload 走独立写入路径，WAL 只记录**元数据 + 校验和**。本文补充可执行细节：

```
写入顺序（必须严格保持）：
  1. New Extent 写入设备（io_uring 写或直接 DMA）
  2. Flush 该设备 / 该 extent 范围（data barrier）
  3. WAL 追加 ObjectVersion / ExtentAlloc / MappingSwitch
  4. WAL 追加 TxnCommit
  5. Flush WAL 到 commit_lsn（wal barrier）
  6. 内存中发布新版本、对外返回成功
```

第 2 步与第 5 步是两个不同的 barrier，不允许合并成一个“全局 fsync”：

- 若把 data flush 延后到 WAL flush 之后，崩溃后可能出现“日志说 extents 已就绪但数据没落盘”的假提交。
- 若把 WAL flush 提前到 data flush 之前，则提交记录指向不可信数据。

`ExtentAllocBody.checksum` 的存在使得 recovery 可以校验“数据确实写对了”，而不只是“设备返回了成功”。第一版使用 XXH3-64；后续可切到每 4 KB 段 CRC32C 以支持部分损坏定位。

### 2.5 Flush 策略与 LSN 水位

```
WAL Manager
  ├─ append(record) -> lsn            // 只入内存 buffer，不保证持久
  ├─ flush_upto(target_lsn) -> ()     // 阻塞至 durable_lsn >= target_lsn
  └─ durable_lsn() -> Lsn             // 当前已确认持久的水位
```

不变量：

```
W1. durable_lsn ≤ append_lsn
W2. 任何 TxnCommit 的 persisted 前提是：commit_lsn ≤ durable_lsn
    （commit_lsn 定义为该 TxnCommit 记录的起始 LSN）
W3. 记录一旦写入 WAL 段，内容不可修改（只允许追加和整段删除）
```

第一版 flush 行为：每次 `TxnCommit` 后立即 `flush_upto(commit_lsn)`（同步组提交可后续优化）。这是最简单且最容易证明正确的选择；`group_commit` 作为可配置项留到有压测数据后再打开。

## 三、锁设计

### 3.1 锁分层

系统里有四类并发保护对象，性能与死锁风险完全不同，不能用一把大锁统一：

| 层 | 保护对象 | 机制 | 持有时间 |
|---|---|---|---|
| **L1 事务锁** | ObjectId（写意向） | 冲突表 + 行锁语义 | 事务级 |
| **L2 版本可见性** | Version Chain | 无锁（原子指针 RCU 风格 + Epoch） | O(1) |
| **L3 映射** | Extent → Mapping | 读写锁（每 LogicalExtent 一把） | 极短（拷贝出映射即释放） |
| **L4 元数据页** | Directory / FSM | 细粒度闩（latch），按页或按项 | 微秒级 |
| **L5 系统状态** | WAL Manager / Checkpoint / GC | 全局互斥 + 条件变量 | 毫秒级，禁止阻塞用户路径 |

关键约定：

- **L1 只在写事务结束时释放**，读事务不取 L1（依赖 Snapshot）。
- **L3 的读锁只覆盖“取映射”这一步**，绝不持有到 I/O 完成。读到 `MappingId` 后再用 `MappingId` 去找物理位置，即使同时发生迁移也不影响本次读取（这正是 MappingVersion 存在的理由）。
- 读者也可以先取 `MappingVersion`、再按该版本解析物理位置；两种写法等价。冻结的接口采用后者（[04-kernel-interfaces.md §5.2](04-kernel-interfaces.md)），因为它让「读全程不发生物理切换」直接体现在签名上。
- **L5 的持有点必须在编码时显式标注**，禁止在持锁期间做 I/O 或等待用户回调。

### 3.2 死锁预防

第一版采用 **Wait-Die**，不实现死锁检测：

```
小于等于对方 xid 的事务：等待（Wait）
大于对方 xid 的事务：中止并返回 Conflict(ConflictKind::WriteConflict)（Die）
```

> 冲突的**错误分类**由 [04-kernel-interfaces.md §2](04-kernel-interfaces.md) 统一冻结（[ADR-0008](../adr/0008-kernel-interface-freeze.md)）：写冲突一律是 `Conflict`，内核**不自动重试**，由上层工作流决定是重试整个流程、换路径还是显式暴露给用户。

理由：AI 工作负载的冲突概率低于 OLTP（写入多为 append-only 的新对象），死锁检测器的复杂度和运行时开销不值得；Wait-Die 的“老事务优先”也天然对齐 MVCC 的快照老幼关系。

预留扩展点：如果未来出现大量同 xid 区间冲突（例如批处理场景），可以开启基于时间窗的 `Wait-Die / Wound-Wait` 混合策略，接口不变。

### 3.3 可见性判定（不取任何锁）

```
fn visible(version, snapshot, txn_status) -> bool:
    let xmin = version.xmin;
    let xmax = version.xmax;
    if !committed_in_snapshot(xmin, snapshot, txn_status): return false;
    if xmax.is_none(): return true;
    let x = xmax.unwrap();
    return !committed_in_snapshot(x, snapshot, txn_status);
```

`committed_in_snapshot` 依赖事务状态数组（`xid -> Running/Committed/Aborted`），并用 `xmin/xmax/active_xids` 三元组做快照裁剪。这段逻辑**必须纯函数、无 IO、缓存友好**，因为它是读路径上唯一每版本都要执行的热点。

## 四、内存状态机

### 4.1 对象与版本

```
ObjectState:   Live → Deleted(committed xmax) → Reclaimable
VersionState:  Pending → Published → Obsolete → Reclaimable
               Pending → Aborted              （事务中止时标记，见 §6.3）
ExtentState:   Allocated → Mapped → Unmapped → Freed
MappingState:  Active → Superseded → Draining → Retired
```

转移规则中最重要的三条：

```
S1. Pending 版本对外不可见，即使它已经写入 WAL。（可见性由 xid 状态决定，不由 WAL 存在决定）
S2. Obsolete 不代表可回收：还需要满足 §8 的六条件判定。
S3. Superseded Mapping 必须经过 Draining（等所有旧读者退出，见 §3.1 的 Epoch）才能 Retired。
S4. Aborted 版本永不可见，也不进入 Reclaimable；其物理 extent 进入待回收集合（见 §6.3）。
```

### 4.2 事务状态数组

```
TxnStatus 数组（内存，周期性与状态一并 checkpoint）
  索引：xid mod RING_SIZE（环形复用，配合 xmin 推进）
  取值：Running / Committed / Aborted / InProgress
```

`xmin` 之前的槽位全部确定为 Committed/Aborted，可以安全复用。环形数组的大小决定了“最老可复用 xid”，即 `SELECT xmin, xmax, active_xids` 的可回溯窗口的上界；如果这个窗口被长事务反复撑爆，应增加环大小或拒绝开始新事务（返回资源繁忙），而不是放宽可见性规则。

## 五、Checkpoint 算法

### 5.1 目标与失败模型

Checkpoint 的目标：得到一个**自洽且可验证的元数据快照**，并推进 WAL 可截断边界。

失败模型：checkpoint 过程中任意时刻崩溃，系统必须能够回退到**上一个完整 checkpoint** 并重放 WAL。因此 checkpoint 必须是“影子文件 + 原子发布”，不能原地修改。

### 5.2 三阶段算法

```
Phase A —— Capture（短暂，持 L5 锁）
  A1. 取 checkpoint_lsn := WAL.durable_lsn()      // 后续所有元数据变更 ≥ 该 LSN
  A2. 记录当前 txns: xmin / xmax / active_xids / 各 xid 状态
  A3. 记录 extent_epoch 与 mapping_epoch 水位
  A4. 追加 CheckpointBegin(checkpoint_lsn) 到 WAL，flush
  A5. 释放 L5 锁

Phase B —— Snapshot & Write（长时间，不持锁）
  B1. 对 6 个目录分别做 versioned 拷贝：
        Transaction Status
        Object Directory
        Version Directory
        Extent Directory
        Mapping Directory
        Index Manifest
      拷贝语义：读取时用 MVCC 快照过滤掉 < checkpoint_lsn 未提交的条目
  B2. 每个目录写成新文件（copy-on-write，不覆盖旧文件）：
        CKPT-<id>/txn.status      CKPT-<id>/object.dir
        CKPT-<id>/version.dir     CKPT-<id>/extent.dir
        CKPT-<id>/mapping.dir     CKPT-<id>/index.manifest
  B3. 每个文件写完后 flush + 计算 blake3 摘要
  B4. 写 CKPT-<id>/MANIFEST，包含：
        checkpoint_id, checkpoint_lsn, 各文件相对路径 + 大小 + 摘要,
        prev_checkpoint_id, format_version, manifest_crc32c
  B5. flush MANIFEST 所在文件与目录

Phase C —— Publish（短暂，持 L5 锁）
  C1. 写 superblock 的 current_checkpoint_id（单次原子写，4 KB 对齐）
  C2. flush superblock 与所在目录
  C3. 释放 L5 锁
  C4. 异步删除比 current 早 2 代以上的 CKPT-* 目录（保留一代用于回退）
```

### 5.3 WAL 截断边界

只在 Phase C 完成后才推进 `WAL.min_required_lsn`：

```
min_required_lsn = current_checkpoint.checkpoint_lsn
```

判定：任何 `< min_required_lsn` 的 WAL 段，在以下条件都满足后可以删除：

```
T1. 该段的全部记录 ≤ 某个已发布 checkpoint 的 checkpoint_lsn
T2. 不存在仍在执行中的事务，其某个 WAL 记录位于该段
T3. 不存在依赖该段的未完成迁移（MigrationRecord.source_mapping 未切换完）
T4. 非事务派生记录已兑现：族 Index 的记录所声明的 (index_id, segment_id)
    对当前所有已注册索引都已出现在各自域协调区的已发布段列表中；
    族 Maintenance 的记录都有对应的 End，或被判定为未完成的维护。
```

**T2 是必须的**：长事务可能在其开始后申请了大量 LSN，如果贸然截断它很早的记录，恢复时就无法判定该事务的起始状态。这一类 bug 在开源数据库中出过多次，必须显式处理。

**T4 是** [ADR-0013](../adr/0013-index-manifest-in-wal.md) **补的**：T1–T3 全部以事务为单位，而 `IndexManifest`（`0x31`）不属于任何事务——它恰恰是「某个索引段已经发布」的证据。把它随段截掉，就等于在索引追赶慢、检查点勤的组合下剪掉了索引的发布记录。

T4 的两个边界必须写清：

- **T4 不改 `min_required_lsn`。** 上式 `min_required_lsn = current_checkpoint.checkpoint_lsn` 不变；T4 只影响「该 WAL 段文件何时可以被物理删除」，是空间回收的安全界，不是恢复起点。若把 T4 并进 `min_required_lsn`，截断边界就会依赖索引状态，检查点频率与索引追赶速度互相绑死。
- **T4 是保守的。** 索引尚未消费到协调区时，相关 WAL 段**暂时**不能删（`wal_segments_blocked_by_index` 上升是运维信号）——宁可多留几个段，不可让索引无历史可读。

### 5.4 Checkpoint 的并发复杂度

checkpoint 不阻塞用户路径的几点保障：

- Phase B 只做只读快照，不做写回。
- WAL 截断、旧目录删除都在 C4 后台进行，不影响写入。
- 若 Phase B 期间写放大过大，触发内存淘汰，淘汰只影响缓存（Cache 角色），不改变持久状态。这就是 `02-kernel.md` §3.7 “Cache 与 Replica 必须分离” 的直接收益。

## 六、崩溃恢复算法

### 6.1 总流程

```
Startup
  │
  ├─ 1. Read superblock（双份 + CRC，取校验通过的较大 checkpoint_id）
  │
  ├─ 2. 校验并加载 CKPT-<id>/MANIFEST；校验失败 → 回退到 prev_checkpoint_id
  │
  ├─ 3. 加载六个目录到内存（构建目录树与 hash 索引）
  │
  ├─ 4. 定位 checkpoint_lsn；从该 LSN 起扫描 WAL
  │       ├─ 段头 CRC 不通过 → 该段视作未创建（正常情况）
  │       ├─ 记录帧校验不通过 → 该段扫描在此终止，记录 last_valid_lsn
  │       └─ 只保留 [checkpoint_lsn, last_valid_lsn] 区间记录
  │
  ├─ 5. 重建事务状态：
  │       ├─ 有 TxnCommit 且完整持久 → Committed
  │       ├─ 有 TxnBegin 无 Commit/Abort → Aborted（回滚其效果）
  │       └─ 无 TxnBegin（扫描窗口外起始）→ 依赖 checkpoint 中的状态，未知则保守 Aborted
  │
  ├─ 6. Redo（只 redo 已提交事务的元数据变更，见 §6.2）
  │
  ├─ 7. 解析未完成迁移（MigrationBegin 无 MigrationEnd）：
  │       ├─ 无 MappingSwitch → 丢弃目标 extent，回滚迁移
  │       └─ 有 MappingSwitch → 迁移已完成，补记 MigrationEnd，回收源 extent
  │
  ├─ 8. 校验 Extent / Mapping 引用完整性（每个映射指向合法 allocated extent）
  │
  ├─ 9. 索引水位检查：索引声明水位 < object 水位 → 标记“滞后”，后台增量重建
  │
  └─ 10. 打开服务；滞后索引在重建完成前走回退扫描路径
```

第 9–10 步的「滞后判定 + 后台增量重建 + 回退扫描路径」在 [06-index-recovery.md](06-index-recovery.md) 中被展开为三阶段追赶算法与代价模型。其中一条对本文的补充：追赶的**水位判定必须按 xid 而非 LSN**（[03 §1.1 R3](03-consistency-kernel.md)），因此索引侧除 LSN 水位外还维护一条 xid 水位（[06 §1.1](06-index-recovery.md)）。

### 6.2 Redo 的幂等性

WAL redo 必须**幂等**，因为崩溃可能发生在“redo 过程中再次崩溃”。三种记录的处理：

| 记录 | Redo 语义 | 幂等实现 |
|---|---|---|
| `ObjectVersion` | 把版本链前进到指定 version | 以 `version_id` 为幂等键，已存在则跳过 |
| `ExtentAlloc` | 在 FSM 中标记 extent 已占用 | 以 `physical_extent_id` 为键，重复分配是 no-op |
| `MappingSwitch` | 把逻辑 extent 的当前映射改为 new | 以 `new_mapping` 为键，已是 new 则跳过 |

不能用“LSN 比当前记录大的才重做”这类逻辑：LSN 是 WAL 偏移，恢复时内存状态里没有对应的“最后应用 LSN”，除了按 LSN 顺序应用之外没有别的选择。这里采用最朴素的方式：**从 checkpoint_lsn 顺序重放到 last_valid_lsn，靠幂等键保证正确性**。

### 6.3 Undo 与“未提交事务的物理影响”

第一版**不做 undo**：未提交事务的数据本身仍然可以通过 WAL 判定为不可见（因为 xmin 对应 xid 未 Committed），不需要把物理数据抹掉。但需要在恢复时做三件事：

1. 把该事务的 `xid` 状态置为 Aborted（写入下一个 checkpoint）。
2. 把该事务新分配的 `ExtentAlloc` 加入“待回收”集合（下次 GC 处理）。
3. 把该事务创建的 `VersionState::Pending` 版本标记为 Aborted。

这样避免了 undo 日志与 redo 日志共存带来的复杂度，代价是“垃圾产生后靠 GC 清理”。这是一次有意的取舍，见 §9.3。

### 6.4 恢复时间上限

第一版必须能把恢复时间作为可度量 SLA：

```
T_recover ≈ T_load(ckpt) + T_scan(WAL) + T_redo(committed_txns_in_window)
```

因此需要把 checkpoint 频率作为自适应参数：
- 目标：`T_scan(WAL) ≤ 2 × T_load(ckpt)`，否则提高 checkpoint 频率。
- 目标：`T_recover < 10s`（1 TB 数据规模），这是第一版的硬性验收标准。

## 七、迁移（Migration）的完整协议

`02-kernel.md` §3.8 已经给出状态机。本文给出**可以并发安全的精确时序与恢复规则**。

### 7.1 协议

```
Migration(extent E, target T):
  M1. 在 L3 读锁下读取 E 的 (MappingVersion v, PhysicalExtent P)     [锁内 O(1)]
  M2. 在 T 上分配空间 P'，写 MigrationBegin(E, v, P, P')
  M3. 复制 Payload: read P (以 v 为稳定视图) → write P'
  M4. 计算 P' 的校验和，与 ExtentAllocBody.checksum 比较
        ├─ 不符 → MigrationEnd(ABORTED)，释放 P'，结束
        └─ 相符 → 继续
  M5. flush T（data barrier）
  M6. 在 L3 写锁下：
        ├─ 若 E 的当前 MappingVersion ≠ v → 源版本已变化，放弃（见 7.2）
        └─ 否则 WAL 追加 MappingSwitch(E, v, v+1)，flush
  M7. 更新内存映射表（E → v+1 → P'），并让 v 进入 Draining
  M8. 等待所有持有 v 的读者退出（Epoch 推进）
  M9. WAL 追加 MigrationEnd(E, v+1)，flush
  M10. 释放 P
```

### 7.2 并发写冲突的处理

M6 的“源版本已变化”是设计上的正常路径，不是错误：

- 若 E 在迁移期间被写入（COW 生成了新版本），则旧版本数据已经没有任何读者会用到，**整个迁移可以直接放弃**，不需要双写。
- 这对应 `02-kernel.md` 中的建议：“采用 COW 方案，切换时重新检查源版本，若已变化则重新同步或放弃”。

这条规则让第一版不需要实现任何增量同步机制，代价是“频繁写的热点 Extent 迁移可能反复失败”，由 Placement Manager 做退避（最多 N 次后放弃并记录原因）。

### 7.3 崩溃点恢复矩阵

| 崩溃点 | 恢复处理 |
|---|---|
| M2 之后，M3 未完成 | MigrationBegin 无 End 且无 MappingSwitch → 丢弃 P'，回滚 |
| M3-M5 完成，M6 未执行 | 同上（Payload 已完整但映射未切换，数据作废） |
| M6 WAL 写入但未 flush | MappingSwitch 不存在于有效 WAL → 视为未切换，丢弃 P' |
| M6 flush 完成，M7-M9 未完成 | 有 MappingSwitch 无 End → 补记 End，把 P 加入待回收 |
| M9 完成后崩溃 | 迁移完成，正常继续 |
| M10 完成后崩溃 | 完成，P 已回收（释放操作幂等） |

**关键安全点**：`MappingSwitch` 的持久化是唯一的“提交点”。在此之前一切可回滚，在此之后一切必须完成。

### 7.4 与 GC 的交互

迁移进入 Draining 的旧 Mapping，其对应 PhysicalExtent 除了要通过 §8 的六条件判定外，还必须额外满足**三个迁移相关条件**：

1. 已完成 M8（无旧读者）；
2. 有持久化新映射（`MappingSwitch` 已持久）；
3. 该 extent 不在任何 checkpoint 的“正在被读取”集合中。

第 3 条容易被忽略：如果 checkpoint 正在从旧 Mapping 读取，提前回收会导致 checkpoint 产出损坏文件。实现上通过“GC 与 checkpoint 共享同一个 Epoch 注册表”解决：checkpoint 注册为长周期 reader，GC 必须等它退出对应 Epoch。

## 八、GC 判定：六条件合取

`02-kernel.md` §7.3 已经给出流程图，这里给出精确判定，它必须是一个**合取且无捷径**的表达式：

```
reclaimable(x) :=
    (1) ∀ v ∈ versions(x): v.state == Obsolete
    (2) ∀ s ∈ active_snapshots: ¬references(s, x)
    (3) ∀ e ∈ active_epochs: ¬references(e, x.mapping)
    (4) ⌊x⌋ 不在任何 checkpoint 的 read set 中
    (5) x 不在任何未完成迁移的 source 或 target 集合中
    (6) WAL 侧无恢复依赖（x 的 ExtentAlloc 已被某个已发布 checkpoint 覆盖）
```

实现要防止“时序竞态”：判定必须在一个**快照式视图**上做，否则判定通过的瞬间可能有新读者加入。第一版的解法：

- GC 在“空闲 Epoch”执行：先推进一次全局 Epoch，等待所有进程确认，再采样活动快照集合；
- 任何在采样后开始的读者只能看到“当前内存映射”，不会引用被标记回收的 mapping；
- 回收动作本身先摘链（从映射表、版本链、FSM 中移除引用），再释放。

这里的顺序不能反过来：**先注销引用、再释放物理空间**。否则会出现“读者拿到的指针指向已释放空间”的 use-after-free。

## 九、第一版实现边界与取舍

### 9.1 必须实现

| 项 | 原因 |
|---|---|
| WAL 帧格式与 CRC | 崩溃恢复的前提 |
| Checkpoint 影子目录 + 原子发布 | 恢复点唯一自洽入口 |
| 前滚重放 + 幂等键 | 保证“已提交必须恢复” |
| MappingSwitch 作为迁移提交点 | 迁移正确性的唯一关键 |
| 六条件 GC | 防止悬挂引用 |

### 9.2 第一版不实现（明确列进设计债务表）

| 不实现 | 替代方案 | 风险 |
|---|---|---|
| Undo 日志 | 依赖 MVCC 可见性 + GC 清理 | 恢复后空间暂时未回收，需 GC 补偿 |
| 事务组提交 | 每 commit 单独 flush | 吞吐受限，但语义正确 |
| 死锁检测 | Wait-Die | 偶发的事务中止需要应用层重试 |
| 增量迁移同步 | M6 版本检查失败即放弃 | 热点数据迁移成功率下降 |
| 分布式副本仲裁 | 单机单副本 | 无法应对单盘物理损坏（见 §10） |

### 9.3 设计债务的验收

每条债务都需要在测试中有对应的**已知缺口测试**（xfail 测试），并记录在 `docs/architecture/07-operations.md §7` 的测试债务清单里。这样“暂不支持”与“实现了但有 bug”在 CI 上是可区分的。

## 十、五条硬性不变量（可测试形式）

直接引用 `02-kernel.md` §10.1 的五条，并给出**可自动化的断言**：

```
INV1  逻辑身份不依赖物理地址
      assert: 任意对象读取路径中，不出现物理地址比较/比较结果决定身份

INV2  已提交版本可恢复
      assert: 崩溃注入后，所有返回过 Commit OK 的对象必能读取到对应版本

INV3  映射切换不改变进行中的读取
      assert: 在迁移并发下读到的结果与迁移前一致

INV4  旧 extent 引用清零才回收
      assert: 回收日志中每个 extent 的引用计数在回收时刻为 0

INV5  非持久化存储不是唯一权威副本
      assert: 任意时刻，每个可读对象的权威副本位置 ∈ {NVMe, HDD}
```

## 十一、故障注入测试矩阵

`02-kernel.md` §11.2 建议对关键路径做崩溃注入。本文给出第一版必须覆盖的矩阵。

### 11.1 注入点

```
I1. WAL 帧写入一半（帧头写、body 未写）
I2. WAL 帧写入完成但未 flush（模拟掉电 / 进程 kill -9）
I3. WAL flush 后、内存状态更新前
I4. New Extent 数据写入一半
I5. New Extent 写完但未 flush
I6. Checkpoint Phase B 中途（某个目录文件写完、MANIFEST 未写）
I7. Checkpoint MANIFEST 写完、superblock 未更新
I8. MappingSwitch 写入后、内存映射更新前
I9. GC 摘链后、释放前
I10. 迁移 M3 中途（目标 extent 部分写入）
```

### 11.2 判定标准

对每个注入点，恢复后必须满足：

| 检查 | 期望 |
|---|---|
| 所有返回过成功的写入可读 | 是 |
| 未返回成功的写入不对外可见 | 是 |
| 目录之间引用完整（无 dangling） | 是 |
| 无重复 extent 分配 | 是 |
| 恢复后数据与“注入前状态”的差异只能是尾部截断 | 是 |
| 恢复时长 < 10s（1TB） | 是 |

### 11.3 测试实现建议

- 用一个 `FaultInjector` 包装所有 `write / flush / rename`，按“第 N 次调用后 kill 进程”注入。
- 崩溃后重启，用“运行一致性检查器”而不是靠人眼对比。
- 同一个测试种子可复现（`seed → 注入序列 → 期望差异集`），便于 CI 回归。

以上三条已在 [05-kernel-test-harness.md](05-kernel-test-harness.md) 中落成具体方案：注入点改为**语义 site 为主键、调用序号为修饰符**（§3.1–§3.2），假设备的掉电模型见 §2.1，一致性检查器见 §5.3，四层测试与判定标准见 §5。

## 十二、接下来还要做的三个深挖

本文把“一致性协议与崩溃安全”落到了可编码的粒度。下一步建议按相同标准继续：

1. **函数调用链与模块接口冻结**：Object Manager / Extent Manager / Mapping Manager / WAL Manager / GC 之间的精确 Rust trait 与错误类型。 → 已完成，见 [04-kernel-interfaces.md](04-kernel-interfaces.md)。
2. **索引恢复的增量算法**：日志水位 → 索引构建水位的追赶策略，以及“滞后索引的查询回退路径”的代价估计。 → 已完成，见 [06-index-recovery.md](06-index-recovery.md)。
3. **硬件资源管理的实现细节**：Placement Manager 的代价模型参数化、NUMA 亲和调度、io_uring 队列深度与 O_DIRECT / SPDK 的切换判据。 → 已完成，见 [07-hardware-resource-management.md](07-hardware-resource-management.md)、[ADR-0010](../adr/0010-hardware-resource-management.md)。

这三项完成后，实现阶段就不再需要“边写边想”。
