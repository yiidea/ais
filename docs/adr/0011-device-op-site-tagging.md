# ADR-0011：设备级注入点的标签位置与排序

- **状态**：已接受
- **日期**：2026-11-05

## 背景

[ADR-0009](0009-fault-injection-harness.md) 决策 3 把注入 site 分成两类：**设备级** site（对应一次设备操作）由 `IoExecutor` 承载，**内核级** site 在模块内显式打点。当时留了一条尾巴：「设备级 site 在 `IoExecutor` 上的具体形状（标签放 `DeviceOp` 上还是放 `submit_and_wait` 的参数上、批量接口怎么给每个操作打标签）需要一条补充 ADR」。

[05 §11 第 1 项](../chatgpt/05-kernel-test-harness.md) 与 [07 §9 第 3 项](../chatgpt/07-hardware-resource-management.md) 都把它列为待办，[06 §6.3](../chatgpt/06-index-recovery.md) 还等着它——索引层的两个 site（`index.segment.sealed`、`index.watermark.published`）是设备级，但打点方在索引层而不是内核模块。

这个问题不定，有三条验收用例无法实现：

| 用例 | 出处 | 为什么需要设备级 site |
|---|---|---|
| `qd_backpressure_returns_retryable` | [07 §7.1](../chatgpt/07-hardware-resource-management.md) | 「队列满且等待超 deadline」必须定位到**某一次**被拒的提交，而不是「下一次设备操作」 |
| `qd_no_adjust_when_device_not_saturated` / 静默截断 | [07 §4.5](../chatgpt/07-hardware-resource-management.md) | 要断言没有任何一个 op 被静默丢弃，就必须能逐 op 观测 |
| `publish_order_crash`（索引层） | [06 §6.2](../chatgpt/06-index-recovery.md) | 崩溃点落在「段 fsync 完成」与「水位推进完成」之间，两者都是索引层发起的设备写 |

[Rust MVP](../../src/) 目前的做法正好是反例：`FaultDevice::site_for()` 按 `PhysicalLocation.offset` 是否落在 WAL 区间**猜**出 site（`WalWrite` / `PayloadWrite` / …）。它能让崩溃恢复用例跑起来，但 site 是由**地址布局**推断的，不是由调用方声明的——[05 §3.1](../chatgpt/05-kernel-test-harness.md) 要排除的「漂移」正是这样发生的：任何一次布局调整都会让注入点悄悄移到别处，测试仍然通过。

## 决策

1. **site 标签放在 `DeviceOp` 上，不放 `submit_and_wait` 的参数上。**

   ```rust
   /// 设备级语义 site（05 §3.2 目录的机器表示）
   #[non_exhaustive]
   #[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
   pub enum DeviceSite {
       WalFrameWrite,        // wal.frame.partial / wal.frame.written
       WalFlushStart,        // wal.flushed / wal.flush.partial
       ExtentPayloadWrite,   // extent.write.partial / extent.write.done
       CkptManifestWrite,    // ckpt.manifest.before
       CkptSuperblockWrite,  // ckpt.superblock.before
       CkptShadowRename,     // ckpt.publish.before_rename
       MappingSwitchWrite,   // mapping.switch.after_log
       MigrationTargetWrite, // migration.target.partial
       TxnCommitWrite,       // txn.commit.before_flush
       // 索引层（06 §6.3；打点方在索引层，写操作仍经本 IoExecutor）
       IndexSegmentSealed,   // index.segment.sealed
       IndexWatermarkCommit, // index.watermark.published
   }

   pub enum DeviceOp {
       Write   { site: DeviceSite, loc: PhysicalLocation, data: Vec<u8> },
       Read    { site: DeviceSite, loc: PhysicalLocation, len: u64 },
       Flush   { site: DeviceSite, loc: PhysicalLocation, level: DurabilityLevel },
       Discard { site: DeviceSite, loc: PhysicalLocation },
   }
   ```

   理由：

   - **op 是自描述的。** 批量接口的形状取决于这一点——见决策 2。
   - **`submit_and_wait` 的签名不再增加维度。** 参数式标签等于在每次提交时多传一个与 `DeviceOp` 生命周期不一致的值（`&[DeviceOp]` 与 `&[DeviceSite]` 需要长度一致，这类约束只能靠断言维持）。标签放进 op 后，「一个 op 一个标签」由类型保证。
   - **site 是语义，不是实现细节，必须由发起方声明。** 只有调用方知道「这一次 flush 是提交屏障还是普通刷盘」。设备层与 `IoExecutor` 都无法可靠推断——MVP 的 `site_for()` 就是推断失败的例子。
   - **路由信息（NUMA 节点、队列域）不在 `DeviceSite` 上。** 它们由 `IoExecutor` 从 `PhysicalLocation` 与域配置推导（[07 §3.3 HRM6](../chatgpt/07-hardware-resource-management.md) 要求投递到设备所在节点的 reactor），与注入 site 正交。把两者合成一个「上下文」参数会让 site 退化成日志标签，并在每次提交时多分配一个对象。

2. **`submit_batch_and_wait` 逐个 op 带标签，不额外传标签数组。**

   ```rust
   fn submit_and_wait(&self, op: DeviceOp) -> KernelResult<DeviceResult>;
   fn submit_batch_and_wait(&self, ops: &[DeviceOp]) -> KernelResult<Vec<DeviceResult>>;
   ```

   签名**不变**；批量里每个 op 自带 `site`。理由：标签数组与 ops 数组的长度一致性无法用类型表达，而错的长度只会让注入点静默错位——比编译失败更糟。

3. **崩溃注入的触发条件是「设备出队并执行该 op 之前」，一次注入只作用于一个 op。**

   | 时序 | 触发点 | 典型用途 |
   |---|---|---|
   | `before` | reactor 从 SQ 取出该 op、尚未调用 `StorageDevice` | `*_partial` 类 site（该 op 完全未发生） |
   | `after` | 该 op 被 `StorageDevice` 确认成功、尚未回填完成事件给调用方 | `after_log`、`*_done` 类 site |

   这些点**都在 `IoExecutor` 内**，且都在「一次设备操作」的边界上。

   一条边界规则：设备级 site 的**触发点**由本 ADR 定，但**期望结果**仍由 site 目录（[05 §3.2](../chatgpt/05-kernel-test-harness.md)）定。典型例子是 `txn.commit.after_flush`——它对应 `TxnCommit` 帧那次 flush 的 `after` 触发点，但「flush 完成、内存版本尚未发布」这个状态是事务层的语义，设备层只负责在正确的设备操作边界上停下来。设备层不得为自己的方便重排 `after` 与调用方内存更新的先后。

4. **注入器只认 `DeviceSite`，不得从 `PhysicalLocation` 推断 site。**

   `DeviceSite` 进入假设备与注入器的公共输入类型，**穷尽 `match`**：新增一个变体而没有在 site 目录登记，编译即失败（[05 §8](../chatgpt/05-kernel-test-harness.md) 的编译期校验）。MVP 的 `site_for()` 必须删除，替换为调用方声明——这是本 ADR 对现有代码的直接要求。

5. **内核级 site 不进入 `DeviceSite`。** 它们仍由模块内 `fault_point!` 打点。两套机制不可混用（[ADR-0009](0009-fault-injection-harness.md) 决策 3 不变）。

6. **`DeviceSite` 有默认值 `DeviceSite::Untagged`。**

   它不是合法 site，命中 `Untagged` 的注入规则必须是**测试失败**而不是静默跳过——内核代码里出现未打标签的设备操作，说明有一处注入点漏接了。这条规则把「漏打标签」从「测不到」变成「立刻红」。

7. **分页/快照语义：`DeviceOp` 只按值传递。**

   `DeviceSite` 是 `Copy` 的小枚举，三个内核 trait 的其它方法都可保持原样；`DeviceOp` 的所有权随提交转入 reactor，不做借用——这与 [04 §6.1 E1](../chatgpt/04-kernel-interfaces.md) 「`write` 不隐含 flush」是同一套所有权风格，也和 `submit_and_wait(op: DeviceOp)` 的冻结形状一致（`DeviceOp` 按值）。字段是新增的，不是改的，调用方只需要给现有构造点补一个 `site:`。

## 备选方案

1. **标签放在 `submit_and_wait(op, site)` 的参数上** → `submit_batch_and_wait` 需要平行的 `&[DeviceSite]`，长度一致性只能靠断言；调用方构造批量时最容易出错的位置恰好是重试路径（重排 ops 后忘记重排 sites）。改为放进 `DeviceOp`。
2. **沿用 MVP 的 `site_for(loc)` 按地址区间推断** → site 由布局决定而不是由语义决定，任何布局调整都会让注入点漂移而测试仍然通过。这正是 [05 §3.1](../chatgpt/05-kernel-test-harness.md) 要排除的情况。改为调用方显式声明。
3. **一个通用「注入上下文」结构体（site + NUMA 节点 + 队列域 + 调用理由）** → 把三种正交的意图混在一个对象里：site 决定**观测**、NUMA 与队列域决定**路由与并发**。混起来之后，`IoExecutor` 的实现为了拿到路由信息就得读测试字段，测试字段一旦进生产 struct 就必然被读错。改为：`DeviceSite` 只表达注入语义，路由由 `IoExecutor` 自行推导。
4. **设备层自己解析记录内容判断 site** → 设备层不知道对象语义（[04 §6.1 D1](../chatgpt/04-kernel-interfaces.md)：设备层不感知事务），解析 WAL 帧属于越界。
5. **site 直接用 `&'static str` 字符串** → 拼写错误不会编译失败，穷尽 `match` 的编译期校验失效。改为 `#[non_exhaustive]` 枚举。
6. **允许未打标签（缺省为无注入）** → 漏接的注入点会让用例「通过但什么都没测到」。改为 `Untagged` 触发即失败（决策 6）。
7. **把索引层两个 site 拒绝出 `DeviceSite`** → 索引层的写操作同样经过内核 `IoExecutor`（[06 §6.3](../chatgpt/06-index-recovery.md)），若它们的 site 不能出现在 `DeviceOp` 上，索引层就只能退回按地址推断。改为纳入，并由索引层自己声明。
8. **把 `DeviceSite` 挂在 `StorageDevice` 的方法参数上** → `StorageDevice` 是实现方可替换的 trait（[04 §6.1 D0.2](../chatgpt/04-kernel-interfaces.md)），把测试语义压进设备接口会让后端实现承担与它无关的责任，且违反 [04 §14.1 规则 1](../chatgpt/04-kernel-interfaces.md) 的边界。

## 后果

**正面**

- 注入点由调用方声明，与地址布局解耦：布局调整不会让 site 漂移。
- 批量接口的形状不需要第二个数组，长度不一致这类错误不可表达。
- `DeviceSite` 的穷尽 `match` 把「新增 site 未登记」变成编译错误，与 [05 §8](../chatgpt/05-kernel-test-harness.md) 已有的 CI 检查一致。
- 索引层的两个 site 有了落点，[06 §6.3](../chatgpt/06-index-recovery.md) 指出的「索引构建器缺夹具」只剩夹具本身要写。

**负面**

- 每个设备操作的构造点都要多写一个 `site` 字段；内核里构造 `DeviceOp` 的地方都必须知道自己在做什么。这是有意的：写 WAL 帧的调用点本来就知道自己在写 WAL 帧。
- `DeviceOp` 的公开面增加了一个字段。这是对 [04 §6.3](../chatgpt/04-kernel-interfaces.md) 冻结契约的**补充**，按 [04 §14.3](../chatgpt/04-kernel-interfaces.md) 必须走 ADR——即本 ADR。
- `DeviceSite` 的取值会随 site 目录增长；`#[non_exhaustive]` 暴露到 crate 外会让外部后端在 `match` 上被迫加通配分支。缓解：`DeviceSite` 不进入对外 API 表面，`DeviceOp` 与 `IoExecutor` 本来就不对 SDK 导出（[04 §14.1 规则 1](../chatgpt/04-kernel-interfaces.md)）。

**缓解**

- MVP 的迁移路径很短：删除 `FaultDevice::site_for()`，在 `wal/` 与 `kernel/` 构造 `DeviceOp` 的几处补 `site`，`FaultDevice` 改为读 `op.site()`。现有 5 个崩溃恢复用例的语义不变。
- `DeviceSite` 枚举与 site 目录（[05 §3.2](../chatgpt/05-kernel-test-harness.md) 表）由同一条检查守护：CI diff 两边名字集合不一致即失败，防止枚举与目录各走各的。
