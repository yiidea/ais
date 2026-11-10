# ADR-0016：四个「被引用但未定义」的类型进入冻结清单

- **状态**：已接受
- **日期**：2026-11-05

## 背景

仓库里有四个类型**被多处引用、却从未定义**：

| 类型 | 引用处 | 状态 |
|---|---|---|
| `AccessPattern` | [02-kernel §3.4](../chatgpt/02-kernel.md) 的 `PlacementRequest`、[02-kernel §3.6](../chatgpt/02-kernel.md) 的 `MemoryAllocationRequest` | 被 `MemoryAllocationRequest.access_pattern` 引用，从未定义 |
| `ResourceStats` | [04 §6.2](../chatgpt/04-kernel-interfaces.md) 的 `MemoryTierManager::stats` | 作为返回类型出现，从未定义 |
| `DeviceHealth` | [04 §6.1](../chatgpt/04-kernel-interfaces.md) 的 `health()`、[05 §2.1](../chatgpt/05-kernel-test-harness.md)（当作已冻结类型使用） | 只在散文里出现过 `Degraded` / `Failed` 两个词，值集从未定义 |
| `FaultDomainId` | [07 §2.3](../chatgpt/07-hardware-resource-management.md) 新引入 | 只有概念描述 |

[07 §9 第 2 项](../chatgpt/07-hardware-resource-management.md) 与 [ADR-0010](0010-hardware-resource-management.md) 决策 9 都把这件事列为显式待办，并指出一个**硬依赖**：07 §2.4 的「故障域」硬约束在 `FaultDomainId` 定义并接入 `ResourceStats` 之前**无法执行**——那是一条已声明但尚不可兑现的约束。

这不是文档洁癖。四个类型都会出现在冻结 trait 的签名上（`MemoryTierManager::stats`、`StorageDevice::health`、`MemoryAllocationRequest`），如果不冻结，两个实现者会各自定义出字段名不同、值集不同的版本，而**编译都能通过**——直到运行时出现「这个设备算不算 `Failed`」的分歧。

## 决策

1. **四个类型全部进入 [04 §1.2](../chatgpt/04-kernel-interfaces.md) 的冻结清单。**

   [04 §1.2](../chatgpt/04-kernel-interfaces.md) 增加四行：

   | 编号 | 契约对象 | 所属模块 | 冻结内容 | 变更方式 |
   |---|---|---|---|---|
   | F18 | `DeviceHealth` | 硬件层 | 值集与判定语义（`Healthy` / `Degraded(DegradedCapability)` / `Failed`），以及它与 `max_durability()` 的一致性要求 | ADR |
   | F19 | `ResourceStats` | 资源层 | 字段集合与语义（容量三字段、资源类型、NUMA 归属、健康、故障域） | ADR |
   | F20 | `AccessPattern` | 资源层 | 取值闭集与「不影响放置、只影响 I/O 路径」的约定 | ADR |
   | F21 | `FaultDomainId` | 资源层 | 标识符语义（哪些设备共享同一个失效点） | ADR |

   编号接续 [ADR-0012](0012-changefeed-freeze.md)（F16）与 [ADR-0015](0015-index-segment-reclamation.md)（F17），完整对照见决策 2。

2. **编号分配（以最终清单为准）。**

   | 编号 | 类型 | 来源 ADR |
   |---|---|---|
   | F16 | `ChangeFeed` | [ADR-0012](0012-changefeed-freeze.md) |
   | F17 | `LiveSnapshotSource` | [ADR-0015](0015-index-segment-reclamation.md) |
   | F18 | `DeviceHealth` | 本文 |
   | F19 | `ResourceStats` | 本文 |
   | F20 | `AccessPattern` | 本文 |
   | F21 | `FaultDomainId` | 本文 |

   上表的唯一目的是消除「同一编号被两条 ADR 占用」的可能。**任何新增冻结项都必须以本表为起点递增**，不得凭记忆编号；发现冲突时以 04 §1.2 的表格为准并回改 ADR。

3. **`DeviceHealth` 的定义与判定语义。**

   ```rust
   /// 设备的健康状态（F18）。由设备/后端被动上报，内核不主动探测。
   #[non_exhaustive]
   #[derive(Clone, Copy, PartialEq, Eq, Debug)]
   pub enum DeviceHealth {
       /// 全部能力可用
       Healthy,
       /// 能力下降：可继续承载数据，但能力集合收缩
       Degraded(DegradedCapability),
       /// 不可用：所有 I/O 返回 Io{op}，不再进入任何候选集
       Failed,
   }

   /// 降级时**具体**失去了什么。必须可枚举，不能只是一个字符串。
   #[non_exhaustive]
   #[derive(Clone, Copy, PartialEq, Eq, Debug)]
   pub enum DegradedCapability {
       /// max_durability() 下降（例如 Durable → Write）
       DurabilityReduced,
       /// 队列深度上限下降
       QueueDepthReduced,
       /// discard 不再可用
       DiscardUnavailable,
       /// 写缓存被禁用（掉电即丢，等价于只能提供 Write）
       WriteCacheDisabled,
   }
   ```

   三条约束，都是 [04 §6.1 D3](../chatgpt/04-kernel-interfaces.md)「降级必须可见」的可执行化：

   - **`DurabilityReduced` 与 `max_durability()` 必须一致**：报告 `Degraded(DurabilityReduced)` 而 `max_durability()` 仍返回 `Durable` 是矛盾状态，必须被断言检查捕获（[05 §2.3](../chatgpt/05-kernel-test-harness.md) 的假设备提供这条用例）。
   - **`Failed` 是终态的一种**：[07 §5.6](../chatgpt/07-hardware-resource-management.md) 的规则「`health() == Failed` 的设备退出全部候选集」直接读这个值。
   - **不得用字符串表达原因。** 早期草案里的 `Degraded { reason: String }` 会让「降级到什么程度」无法被程序判断——`reason` 里写「队列深度下降」还是「写缓存关闭」取决于实现者的措辞，放置器无法据此决定是否仍可承接 `Cache` 角色。改为枚举 `DegradedCapability`。

4. **`ResourceStats` 的定义。**

   ```rust
   /// 资源域统计快照（F19）。一次采样，不带时间戳。
   pub struct ResourceStats {
       /// 总容量（字节）
       pub capacity: u64,
       /// 已使用（字节）；对应 02-kernel §3.2 的 `allocated`
       pub used: u64,
       /// 已预留、不可被普通分配动用（字节）
       pub reserved: u64,
       /// 资源类型
       pub resource_type: ResourceType,
       /// 归属 NUMA 节点；`None` = 无 NUMA 信息（容器 / VM）
       pub numa_node: Option<u32>,
       /// 设备健康
       pub health: DeviceHealth,
       /// 故障域；`None` = 未知
       pub fault_domain: Option<FaultDomainId>,
   }
   ```

   - **`used` 而不是 `allocated`**：[07 §2.3](../chatgpt/07-hardware-resource-management.md) 的映射表已定「名字不同、含义相同」，本文确认以 `used` 为准，02-kernel 的 `allocated` 作为别名在文档中保留说明。
   - **不存 `available`**：由 `capacity − used − reserved` 导出，存下来就会有「三个字段互相矛盾」的状态。
   - **不含设备 ID**：资源以 `ResourceId` 标识，设备归属由资源注册表维护（[07 §2.3](../chatgpt/07-hardware-resource-management.md) 已论证）。
   - **不含时间戳**：放置决策禁止读时钟（[07 §2.5](../chatgpt/07-hardware-resource-management.md) HRM3），统计快照由调用方在决策点显式采样。

5. **`AccessPattern` 的定义，以及它的作用边界。**

   ```rust
   /// 访问模式（F20）。闭集、可判定，由对象头统计或 PlacementRequest 显式给出。
   #[non_exhaustive]
   #[derive(Clone, Copy, PartialEq, Eq, Debug)]
   pub enum AccessPattern {
       Sequential,
       RandomPoint,
       Hotspot,
       WriteOnly,
       Cold,
   }
   ```

   **冻结的不只是取值，还有一条边界**：`AccessPattern` **不影响放置**，只影响 I/O 路径（队列选择、batch 大小，[07 §4.7](../chatgpt/07-hardware-resource-management.md)）。

   理由是 07 §2.3 已经写明的：首次放置时 `T̃ ≡ 0`、`M̃` 为常数，真正参与排序的 `L` 与 `P` 都与访问模式无关。若将来要让访问模式进入 `C̃`，那是改变代价模型的定义，**需要另一条 ADR**——本文把这条边界冻结下来，避免实现者「顺手」把它加进代价模型。

   另一条同样重要的约定：**不允许放置器自行推断访问模式**。由放置器推断会出现「推断 → 放置 → 观测到访问模式变化 → 重新推断」的反馈环，不确定性无法收敛（[07 §2.3](../chatgpt/07-hardware-resource-management.md)）。

6. **`FaultDomainId` 的定义，以及它对 07 §2.4 硬约束的解锁。**

   ```rust
   /// 故障域标识（F21）：共享同一失效点的一组设备。
   /// 同一故障域内的两个副本不构成冗余。
   pub struct FaultDomainId(pub u32);
   ```

   语义：

   - **同一故障域 ⟺ 共享同一个失效点**：同一 PCIe switch 后的多块 NVMe、同一机架、同一供电域、同一 CXL 主机桥。
   - **域 ID 的分配是部署配置，不是探测结果。** 内核无法从设备属性可靠推断「是否共用一个 PSW」，只能由部署方在 Hardware Profile 里声明；未声明时全部设备落在同一个域 `FaultDomainId(0)`。
   - **未声明 = 保守**：所有设备同域意味着「任何两份副本都不构成冗余」，因此第一版单副本形态不会被误判为高可用。这与 07 §3.5「无 NUMA 拓扑信息时退化为单节点模型，不假装有亲和」是同一种诚实行为。

   这条定义**兑现了 07 §2.4 的故障域硬约束**：`ResourceStats.fault_domain` 有值之后，「同一故障域内的两份不构成冗余」可以被放置器实际执行。ADR-0010 决策 9 里「依赖 `FaultDomainId`，在该类型定义并接入 `ResourceStats` 之前无法执行」这句话在本文生效后失效。

7. **与 [04 §14.1 规则 1](../chatgpt/04-kernel-interfaces.md)（公共 API 表面不得出现物理类型）的界限。**

   - `DeviceHealth` / `ResourceStats` / `AccessPattern` / `FaultDomainId` **都是逻辑类型**，不含 `PhysicalLocation` / LBA / 偏移，因此可以出现在公共表面上，不违反规则 1。
   - `ResourceStats` 会被 [architecture/07 §2–§3](../architecture/07-operations.md) 的容量规划与资源治理消费（运维 API），因此它**是**公共表面的一部分。这一点在本文里明确：它不是内部类型。

8. **实现方必须对齐的一份单一实现。**

   四个类型由内核 crate 定义一次，索引层、规划器、夹具、运维 API **全部引用同一个定义**。`ResourceStats` 与 02-kernel `HardwareResource` 的字段映射（[07 §2.3](../chatgpt/07-hardware-resource-management.md) 的表）保留为文档层的对照，不作为两套类型存在。

## 备选方案

1. **保持「未定义」，让实现者各自定义** → 同一 trait 签名在不同实现里含义不同，编译能过、运行会分歧（「这台设备算不算 `Failed`」）。改为冻结。
2. **`DeviceHealth` 用字符串 reason**（早期草案形态）→ 程序无法判断降级后的实际能力，放置器只能按「`Degraded` 一律不进候选集」处理，白白放弃仍可承载 `Cache` 的设备。改为枚举 + 明确的 `DegradedCapability`。
3. **`Degraded` 只表示「持久性下降」** → 队列深度下降、discard 不可用、写缓存关闭都属于降级，但处理方式不同（前两者影响性能，后者影响耐久）。改为按能力枚举。
4. **`ResourceStats` 存 `available`** → 三个容量字段之间可能出现矛盾（`available ≠ capacity − used − reserved`），放置器读到哪个就用哪个。改为导出。
5. **`ResourceStats` 带采样时间戳** → 放置决策禁止读时钟；带时间戳会诱导实现者做「时间衰减」这类隐式信号。改为不带。
6. **`AccessPattern` 交给放置器推断** → 反馈环不收敛（07 §2.3）。改为显式传入。
7. **让 `AccessPattern` 参与 `C̃`** → 需要重新标定权重，且与「首次放置时 `T̃ ≡ 0`」的推导冲突。改为冻结边界：只影响 I/O 路径。
8. **`FaultDomainId` 由内核探测推断** → 内核拿不到「是否同一 PSW / 同一机架」这类部署信息，探测只能靠 PCI 拓扑猜，猜错会让独立的设备被当成同域（可用性降低）或反之（误以为有冗余）。改为部署配置声明，未声明则保守同域。
9. **`FaultDomainId` 用字符串（如 `"rack-3"`）** → 可读但不可比较、不可索引，且拼写变体（`rack3` / `Rack-3`）会让「同域」判定失效。改为 `u32` 标识 + 部署侧的映射表。
10. **把四个类型放在上层 crate（非内核）** → 它们出现在冻结 trait 的签名上（`MemoryTierManager::stats`、`StorageDevice::health`），上层定义会让内核依赖上层，违反 [04 §12.2](../chatgpt/04-kernel-interfaces.md) 的依赖禁令。改为内核定义、全部引用。

## 后果

**正面**

- 「被引用但从未定义」这一类缝隙被关掉：四个类型的字段与值集有了唯一说法。
- 07 §2.4 的故障域硬约束从「已声明但不可执行」变成可执行——这是 ADR-0010 遗留的唯一一条**正确性**缺口。
- `DeviceHealth` 的枚举化让「降级后能否继续承接 `Cache` 角色」变成程序可判定的事实，而不是读字符串猜。
- 编号分配表（决策 2）把「同一编号被两条 ADR 占用」变成一次可检查的对比，而不是到实现期才发现。

**负面**

- 冻结清单从 F1–F16（含 ADR-0012）扩展到 F1–F21，今后改这四个类型的字段都要走 ADR。
- `DeviceHealth` 的 `DegradedCapability` 是 `#[non_exhaustive]` 的，外部 match 必须带通配分支；即便它很少被外部使用。
- `FaultDomainId` 的默认「全部同域」会让容量规划看到「零冗余」的悲观结论，需要运维侧显式声明才能变好。

**缓解**

- 四个类型都很小，ADR 成本主要体现在「改字段要想清楚」这一点上，而这正是冻结的目的。
- `DegradedCapability` 的外部 `match` 负担可以用一个 `is_durable_capable()` 之类的辅助方法缓解，避免调用方逐变体匹配。
- 部署配置模板里给 `fault_domain` 一个必填项（缺省时启动告警），而不是静默取 0——默认值只用于测试与单机开发。
