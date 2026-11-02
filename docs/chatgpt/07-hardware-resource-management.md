# AINS 硬件资源管理 —— 代价模型参数化、NUMA 亲和、队列深度与后端判据

> 本文是 [03-consistency-kernel §12](03-consistency-kernel.md) 第 3 项深挖的落地稿，也是 [02-kernel §3](02-kernel.md)「硬件资源管理」从「概念」到「可编码」的最后一块。

前三篇深挖分别冻结了接口（[04](04-kernel-interfaces.md)）、定义了怎么测（[05](05-kernel-test-harness.md)）、补齐了索引恢复的增量算法（[06](06-index-recovery.md)）。剩下的空白在硬件这一侧：

- [02-kernel §3.5](02-kernel.md) 给出了代价模型的**符号形式** `C(r,o,t) = αL + βT + γP + δM`，但没说 `L` / `T` / `P` / `M` 各自怎么算、α / β / γ / δ 从哪来；
- [04 §6.3](04-kernel-interfaces.md) 的 I1 说了「每个 NUMA 节点一个 reactor 线程」，但没说**投给哪个 reactor**、**缓冲区放哪个节点**；
- [architecture/04 §2.2](../architecture/04-hardware-path.md) 说了「根据 IO 队列深度动态调节 batch size」，但没说队列深度**从多少起、到多少封顶、依据什么调**；
- [ADR-0004](../adr/0004-bypass-kernel-io.md) 定了「io_uring 为主、SPDK 为可选、POSIX 回退」的**优先级**，但没定**判据**，也没定运行期能不能换。

本文补齐这四件事。核心判断只有一句：

> **放置决策是策略，不是机制；后端选择是启动期的工程决策，不是运行期的动态决策。**

这两句话决定了本文全部内容的形态。

---

## 一、范围与边界

### 1.1 本文要定的四件事

| # | 主题 | 本文给出的东西 |
|---|---|---|
| 1 | 代价模型参数化 | `L` / `T` / `P` / `M` 的定义与计算式、归一化方式、参数来源表、硬约束清单、确定性要求 |
| 2 | NUMA 亲和调度 | 设备 / 缓冲 / 执行三种绑定的判据、first-touch 规则、跨节点惩罚的进入位置、退化行为 |
| 3 | 队列深度与批大小 | QD 与 batch 的区别、初始值、上限来源、自适应规则、迟滞、背压语义、flush 槽位预留 |
| 4 | 后端切换判据 | 启动期决策树、O_DIRECT 的独立判据、三后端的耐久语义一致性要求、运行期允许与禁止的降级 |

### 1.2 本文不定的

- **具体数值**。QD 取 32 还是 64、权重取 0.4 还是 0.3，都属于部署配置，不属于设计。设计只保证「这个数从哪来、依赖什么、边界在哪」。
- **驱动实现**。io_uring 的 ring 布局、SPDK 的 module 编排、内存分配器的实现。
- **数据布局算法**。对象怎么切 extent、索引段怎么组织，属于 [02-kernel §3.1](02-kernel.md) 与 [architecture/02](../architecture/02-storage-engine.md)。
- **GPU HBM 的分配器**。GPU 侧用 CUDA / HIP allocator 是 [02-kernel §3.6](02-kernel.md) 已定的；本文只管「要不要放上去」。

### 1.3 与冻结清单的关系（重要）

[04 §1.3](04-kernel-interfaces.md) **明确不冻结**以下内容：

> 线程模型细节（工作线程数量、NUMA 亲和如何设置）；性能参数（批量大小、队列深度、阈值）。

因此：

- **本文不向 04 的冻结清单添加任何条目。** QD、batch 大小、亲和的具体取值都留在「不冻结」侧。
- 但「这些参数**依赖什么**、**受什么硬边界约束**、**越界时返回什么**」是设计问题，本文给出答案，并由 [ADR-0010](../adr/0010-hardware-resource-management.md) 记录——**记录的是推导方式，不是具体数值**。
- **本文不改变任何已冻结 trait 的签名。** [04 §6.2](04-kernel-interfaces.md) 的 `MemoryTierManager::allocate` 已经带 `numa_node` / `device_id` / `alignment` 参数，[04 §6.3](04-kernel-interfaces.md) 的 `IoExecutor` 已经提供 `submit_batch_and_wait`。本文的所有规则都落在这两个 trait 的**实现内部**。
- 一处例外需要显式记下：`AccessPattern` 与 `ResourceStats` 在 [02-kernel](02-kernel.md) 与 [04 §6.2](04-kernel-interfaces.md) 中被引用但**从未定义**，而本文要用到它们。本文在 §2.3 给出**建议定义**，但它们进入 04 冻结清单需要单独一条 ADR（见 §9）。

---

## 二、代价模型参数化

### 2.1 四个分量的定义

[02-kernel §3.5](02-kernel.md) 的原始形式：

```
C(r,o,t) = α·L(r,o,t) + β·T(r,o,t) + γ·P(r,t) + δ·M(r,o)
```

本节把四个分量逐一落到可计算的量。**所有时间量统一折算为微秒**。

#### L —— 预计访问延迟

```
L(r,o,t) = L_dev(r) × L_size(r,o)
```

| 符号 | 含义 | 来源 |
|---|---|---|
| `L_dev(r)` | 设备固有访问延迟（读） | Hardware Profile 的 `device_latency_ns` |
| `L_size(r,o)` | 尺寸放大因子 `max(1, ⌈size(o) / optimal_io_size(r)⌉)` | Hardware Profile + 对象大小 |

**L 不含排队延迟。** 排队是资源压力的表现，已经由 `P` 承担；两处都算就是重复惩罚，会让代价模型在拥塞时过度反应（同样拥塞被扣两次，模型退化成「拥塞时几乎选最贵的资源」，反而加剧拥塞）。

`L_size` 的存在是必要的：[02-kernel §3.4](02-kernel.md) 策略一已经指出「一个高频 Object 可能非常大，而且并不一定适合全部加载到 DRAM」。没有 `L_size`，一个 10 GB 的对象和 10 KB 的对象在 DRAM 上的 `L` 完全相同，模型会建议把 10 GB 的也放进去。

#### T —— 预计数据传输成本

```
T(r,o,t) = 0                                   若数据已在 r 上
T(r,o,t) = size(o) / eff_bw(cur → r) + T_fix    否则
```

| 符号 | 含义 |
|---|---|
| `eff_bw(cur → r)` | 从当前位置 `cur` 到目标 `r` 的**有效**带宽（已含 NUMA 距离折减，见 §3.4） |
| `T_fix` | 一次传输的固定开销（建立通道、权限协商、注册内存等） |

对 GPU HBM 目标，`eff_bw` 是 PCIe / NVLink 带宽；对 DRAM / CXL 目标，是内存带宽；对 NVMe 目标，是设备顺序读带宽。

**T 是不对称的**：往快资源搬是「慢 → 快」，代价小；往慢资源搬是「快 → 慢」，代价大。

#### P —— 目标资源压力

```
P(r,t) = util(r)^k

util(r) = used(r) / (capacity(r) − reserved(r))
```

`k ≥ 1` 是压力指数：线性（k=1）时资源要到 100% 才显出压力；`k > 1` 时越接近饱和压力增长越快，留出「提前分流」的空间。`k` 是域配置参数。

`util(r) > 1` 的资源**直接出局**，不进入排序（这其实是 §2.4 的容量约束）。

#### M —— 数据迁移成本

```
M(r,o) = size(o)/bw_read(cur) + size(o)/bw_write(r) + M_meta
```

`M_meta` 是元数据成本：写映射记录、更新映射版本、校验和验证。

**M 的不对称性比 T 更强**，而且方向常常相反：

- 往**快**资源迁（NVMe → DRAM）：读慢写快，M 小。
- 往**慢**资源迁（DRAM → NVMe）：读快写慢，M 大。

这正是「降级」比「升级」更贵的物理原因，代价模型必须体现出来，否则会建议把数据无脑留在 DRAM 里——那等于把持久层掏空。

**一个必须说清的例外**：DRAM / CXL / GPU HBM 上的数据不能承担 `Authoritative` 角色（[02-kernel §3.7](02-kernel.md)）。所以「把权威副本从 NVMe 迁到 DRAM」这个动作**根本不存在**——它不是 `M` 很大的迁移，而是根本不是一个合法操作，只可能是「加一份 Cache 副本」。代价模型不能通过「大 M 值」来表达这件事，必须由硬约束挡住（见 §2.4）。

### 2.2 归一化：权重必须无量纲

直接用原始值加权有一个致命问题：**`α` 的取值取决于 `L` 的单位**。

如果 Hardware Profile 报的是 `device_latency_ns = 8200`，而某次改动让它报 `device_latency_us = 8.2`，那么 `L` 缩小了 1000 倍，而 `α` 一个字都没改——代价模型的相对排序被静默地改变了，而且没有任何报错。权重变成了「量纲的倒数」，是一个没有物理意义的数。

解法是**先归一化，再加权**（**HRM2**）：

```
x̃(r) = (x(r) − min_{r'∈F} x(r')) / (max_{r'∈F} x(r') − min_{r'∈F} x(r'))
```

其中 `F` 是可行集（§2.4）。归一化后：

```
C̃(r,o,t) = α·L̃ + β·T̃ + γ·P̃ + δ·M̃ ,   α + β + γ + δ = 1
```

此时四个分量都在 `[0,1]`，权重是**无量纲的偏好比例**，「延迟占 40%、压力占 20%」这句话在任何硬件、任何单位下都成立。

一条边界规则与一条说明：

| 情况 | 处理 |
|---|---|
| `max == min`（所有候选在该轴上无差别） | `x̃ = 0`（该轴不参与区分） |
| `min == 0` 且 `max > 0` | 无需特判：上式自然退化为 `x̃ = x/max` |

#### 两种归一化，用途不同

| 方式 | 做法 | 用在哪 |
|---|---|---|
| **组内归一化** | 在当次可行集内做 min-max | 单次放置决策的候选排序 |
| **固定基准归一化** | 除以一个全局参考值 `x_ref`（该资源该轴的历史最好值） | 需要**跨对象 / 跨时间比较**的场景：缓存准入阈值、迁移触发阈值、GC 激进度 |

组内归一化有一个必须知道的副作用：**它抹掉了绝对量级**。1 MB 与 1 GB 的两个对象各自归一化后可能都得到 `L̃ = 0.8`，但它们的绝对代价可以差三个数量级。所以组内归一化的结果**只在同一个决策内可比**——用于「放在哪」的排序是对的，用于「这个对象值不值得进 DRAM 缓存」的判断是错的。后者需要与「所有对象进入 DRAM 的平均成本」比，必须用固定基准归一化。

**写清楚哪个地方用哪种**，比选哪一种更重要。

### 2.3 参数来源表

| 参数 | 来源 | 更新时机 | 影响面 |
|---|---|---|---|
| `L_dev(r)` | Hardware Profile `device_latency_ns` | 启动探测 + 后台周期重测 | 只影响新放置 |
| `optimal_io_size(r)` | sysfs / SPDK 控制器信息 | 启动探测 | 只影响新放置 |
| `eff_bw(cur → r)` | 实测滑动窗口 | 周期重测 | 只影响新放置 |
| `util(r)` | `MemoryTierManager::stats(resource)`（[04 §6.2](04-kernel-interfaces.md)） | 每次决策 | 只影响新放置 |
| `α` / `β` / `γ` / `δ` | 域配置 | 离线调整 | 只影响新放置 |
| `k`（压力指数） | 域配置 | 离线调整 | 只影响新放置 |
| NUMA 距离因子 | NUMA 拓扑 + 实测 | 启动探测 | 只影响新放置 |
| 对象属性（大小、类型、访问模式） | 对象头 + `PlacementRequest` | 每次决策 | — |

**「只影响新放置」这一列是本文最重要的设计结论**，理由见 §2.6。

#### 两个待定义类型

[02-kernel](02-kernel.md) 引用了 `AccessPattern`（§3.4 的 `PlacementRequest`、§3.6 的 `MemoryAllocationRequest`）与 `ResourceStats`（[04 §6.2](04-kernel-interfaces.md) 的 `MemoryTierManager::stats`），两者**都从未定义**。本文给出建议定义：

```rust
/// 访问模式：描述「谁在什么时候会读这份数据」
#[non_exhaustive]
pub enum AccessPattern {
    /// 顺序扫描 / 批量顺序读
    Sequential,
    /// 随机点查
    RandomPoint,
    /// 高频重复读同一区域（热点）
    Hotspot,
    /// 只写不读（追加日志、索引段）
    WriteOnly,
    /// 冷数据：几乎不访问
    Cold,
}

/// 资源域统计快照
pub struct ResourceStats {
    /// 总容量
    pub capacity: u64,
    /// 已使用
    pub used: u64,
    /// 已预留（不可被普通分配动用）
    pub reserved: u64,
    /// 资源类型
    pub resource_type: ResourceType,
    /// 归属 NUMA 节点（设备 / 内存域）
    pub numa_node: Option<u32>,
    /// 设备健康状态
    pub health: DeviceHealth,
    /// 故障域标识（同一故障域内的副本不构成冗余）
    pub fault_domain: Option<FaultDomainId>,
}
```

`AccessPattern` 的取值必须是**闭集**且**可判定的**——它由对象头的统计信息或上层的 `PlacementRequest` 显式给出，不能由放置器自己猜。一个由放置器推断的访问模式会导致「推断 → 放置 → 观测到访问模式变化 → 重新推断」的反馈环，不确定性无法收敛。

`FaultDomainId` 是本文新引入的类型。[02-kernel §3.9](02-kernel.md) 讨论了「冗余持久化副本」与「单块 NVMe 物理损坏后无法恢复数据」的风险，但没有给这个概念命名。它标识「哪些设备共享同一个失效点」——同一 PCIe switch 后的多块 NVMe、同一机架、同一供电域。它与 `AccessPattern` / `ResourceStats` 一并列入待冻结清单（见 §9）。

### 2.4 硬约束先过滤，代价模型只排序

这是本文的第一条不变量（**HRM1**）。候选资源分成两步处理：

**第一步：可行性过滤。** 下列任一条不满足，资源直接出局，不参与代价比较：

| 约束 | 判据 | 出处 |
|---|---|---|
| 容量 | `used(r) + required(o,r) ≤ capacity(r) − reserved(r)` | [02-kernel §3.5](02-kernel.md) |
| 持久性 | `Authoritative` 副本只能在 `Nvme` / `Hdd` 上 | [02-kernel §3.7](02-kernel.md) |
| 角色 | `Cache` / `WorkingSet` 可以放在任意资源；`Replica` 需要故障域独立 | [02-kernel §3.7](02-kernel.md) |
| 故障域 | 同一故障域内的两份不构成冗余 | [02-kernel §3.9](02-kernel.md) |
| 格式与对齐 | 设备支持的块大小、对齐要求、是否支持 discard | [04 §6.1](04-kernel-interfaces.md) |
| 设备健康 | `health() != Failed` | [04 §6.1](04-kernel-interfaces.md) D3 |
| 耐久要求 | 放置所需的耐久等级 ≤ `max_durability(r)`。降级到 `Write` 的设备仍可承担 `Cache` / `WorkingSet`，但不能承接要求 `Durable` 的放置 | [04 §6.1](04-kernel-interfaces.md) D2 / D3 |
| 域配额 | 前台 / 后台 / 其它域的 IO 与容量预算未耗尽 | [architecture/04 §6](../architecture/04-hardware-path.md) |

**第二步：在可行集 `F` 上按 `C̃` 排序，取最小者。**

```
r* = argmin_{r ∈ F} C̃(r,o,t)      并列时按 ResourceId 升序打破平局
```

**为什么必须先过滤再排序**：如果把持久性约束写成「很大的惩罚项」放进代价模型，那么在极端拥塞下（`P` 极大），惩罚项会被压过去，权威副本被放到 DRAM。**代价模型永远不应该有能力越过硬约束**——这是正确性与性能的边界，不是可以调和的取舍。

### 2.5 确定性要求

放置决策必须在**相同输入下产生相同输出**：

- **禁止读取时钟**。[04 §2.6](04-kernel-interfaces.md) 的 C5 已经禁止内核代码直接调 `Instant::now()`；本文把它扩展到放置路径——放置决策里如果掺入时间，会导致「重启后同一个对象落到不同地方」。
- **禁止随机源**。随机 tie-break 会让决策不可复现。
- **禁止读取可变全局状态**。决策输入必须是显式传入的快照：`Hardware Profile 快照 + stats 快照 + 对象属性 + 域配置`。
- **平局必须有确定顺序**。按 `ResourceId` 升序。

**HRM3**：同样的输入必然得到同样的输出。

这条约束的代价是：放置器不能用「时间衰减」之类的隐式信号。但这恰恰是对的——所有会影响放置的因素都应该是**显式的、可观测的、可标定的**。

### 2.6 放置结果写进 WAL，恢复期不再推导

**这是本文最重要的一条设计决策（HRM4）。**

放置决策一旦做出，就随映射记录写进 WAL（[03 §2.3](03-consistency-kernel.md) 的 Extent / Mapping 记录族 `0x20–0x23`，物理位置 `PhysicalLocation` 就在其中）。

因此：

> **恢复期从不重新推导放置。** 重启后读取的是 WAL / Checkpoint 里记录的物理位置，而不是「用今天的代价模型算一遍当初应该放在哪」。

这条规则解决了三个问题：

1. **参数漂移**。代价模型的权重会随工作负载调整（[02-kernel §3.5](02-kernel.md) 明确「AI 可以分析长期工作负载，调整参数」）。若恢复期重新推导，改一次权重就会让历史数据的物理位置全部改变——旧 extent 变成孤儿，新 extent 凭空出现。
2. **非确定性**。重启时 `util(r)` 已经变了（其它租户或后台任务的负载变了），重新推导的结果与写入时不同。
3. **迁移语义冲突**。数据迁移有专门的协议与提交点（[03 §7](03-consistency-kernel.md)）。如果恢复期可以「重新推导」，就存在第二条不经协议的移动路径，两者的崩溃语义无法统一。

**推论**：代价模型的任何参数（权重、`k`、Profile 数值）都可以**自由地、随时地**重新标定，且只影响**新写入**的数据。已落盘数据的位置由 WAL 决定，不受任何影响。这让「调参」这件事变得安全——运维可以随时改配置，不需要停机窗口，也不需要数据迁移。

同理，**放置决策失败不应导致写入失败**：如果可行集为空（所有资源都放不下），返回 `ResourceExhausted { resource }`（[04 §2.2](04-kernel-interfaces.md)），由上层触发回收或扩容后重试。**不允许「退而求其次」地把数据放到一个违反硬约束的资源上**。

### 2.7 与 06 的接口：`c_scan` 与 `c_point`

[06-index-recovery §3.2](06-index-recovery.md) 的回退策略代价模型需要两个量：

| 符号 | 含义 | 06 给出的来源 |
|---|---|---|
| `c_scan` | 单对象顺序扫描成本（含 payload 读取） | 「设备与缓存的实测值」 |
| `c_point` | 单对象点查成本 | 同上 |

[06 §8](06-index-recovery.md) 明确指出：「`c_scan` 与 `c_point` 的取值来自硬件实测，代价模型参数化必须与硬件 Profile 打通。」本文给出它们的定义——**它们不是新参数，而是本文 `L` / `T` 的组合**：

```
c_point(r, o) = L(r,o,t) + T(r,o,t)
                 语义：对象已在 r 上时 = L；否则 = 读回代价 + 传输代价

c_scan(r, o)  = ⌈size(o) / optimal_io_size(r)⌉ × L_dev(r) + T_scan_payload
                 语义：顺序扫过整个对象，含 payload 的一次性读取
```

其中 `T_scan_payload` 是 payload 若不在 `r` 上时的额外搬运代价。

区别：`c_point` 只算「拿到这个对象」，`c_scan` 算「扫完这个对象」。两者都以**微秒**为单位，与 `L` / `T` 同一量纲，可以直接相加。

对外接口：

```rust
/// 供规划器使用的、针对某个对象的查询代价
pub struct ObjectCosts {
    /// 顺序扫描成本（微秒）
    pub c_scan: u64,
    /// 点查成本（微秒）
    pub c_point: u64,
    /// 估算所依据的候选资源（供可观测性展示）
    pub assumed_resource: ResourceId,
}
```

注意 `assumed_resource` 是**估算所依据的资源**，不是「对象实际在哪儿」——规划器用这个值做策略选择（A / B / C），不用于任何正确性判定（[06 §3.2](06-index-recovery.md) 的原则同样适用于此）。

`ObjectCosts` 是**内核内部**给规划器用的类型，不进入对外 API 表面（[04 §14.1](04-kernel-interfaces.md) 规则 1）。它携带的 `ResourceId` 是资源域标识，不是 `PhysicalLocation`——不暴露 LBA、偏移或设备内部地址。

---

## 三、NUMA 亲和调度

### 3.1 三种绑定

「NUMA 亲和」不是一个动作，而是三个独立的选择：

| 绑定 | 对象 | 谁来定 |
|---|---|---|
| **设备绑定** | 设备（NVMe / GPU / CXL 内存）属于哪个 CPU NUMA 节点 | 硬件拓扑（PCIe affinity），启动时探测 |
| **缓冲绑定** | I/O 缓冲区分配在哪个节点的内存 | **本文的规则**（§3.2） |
| **执行绑定** | 哪个线程在跑（reactor 在哪、worker 在哪） | 启动时确定，运行期不变 |

[04 §6.3](04-kernel-interfaces.md) 的 I1 只规定了「每 NUMA 节点一个 reactor 线程」——那是**执行绑定**的一部分，且只说了有多少个、没说**投给谁**。本节补齐剩下的两块。

### 3.2 缓冲绑定规则（first-touch）

Linux 的 first-touch 规则：**一个物理页归属于第一次写它的线程所在的 NUMA 节点**。这条规则把「谁分配缓冲区」和「缓冲区属于哪个节点」绑定了，于是问题变成：**谁分配？**

**第一原则：用放置消除跨节点，而不是优化跨节点。** 如果数据本来就常驻在消费它的 worker 所在节点，就没有 DMA、没有跨节点、什么惩罚都没有。所以 NUMA 亲和的首要手段是**把数据放到对的地方**（home node 固定），缓冲区摆放只是第一原则用尽之后的补救。

第一原则用尽之后（数据必须从设备取），按下面的规则分层：

| 规则 | 情形 | 缓冲落在 | 理由 |
|---|---|---|---|
| **N1** | 读，**内存命中**（对象常驻 DRAM / CXL / HBM） | **内存域所在节点**（= home node） | 没有 DMA，只有同节点 memcpy |
| **N2** | 读，**设备取数**（对象不在内存域，必须从 NVMe 取） | **设备所在节点** | DMA 的目标缓冲必须在设备近端，否则**每次** DMA 都跨节点 |
| **N3** | 写 | **发起方（内核线程）所在节点** | 写路径的数据由内核线程生成，把它挪到设备节点等于把 CPU 的写变成跨节点写；且写路径单次传输量小（batch 1–4），跨节点 DMA 的绝对代价有限 |

**N2 的续：消费方怎么办。** 缓冲在设备节点，但消费方（算子）可能在另一个节点。两种处理：

- **大批量顺序消费**（向量扫描、索引段加载）：把消费线程**临时绑定到设备节点**执行。绑核的代价是负载不均，但对大块顺序访问是净收益。第一版至少要做到「缓冲在设备节点 + 消费方跨节点读一次」，并把它明确记为**已知的次优路径**（B2 实测它的实际代价）。
- **小块随机访问**：发起方跨节点读一次即可——一次跨节点读仍然小于「每次 DMA 都跨节点」。

**home node 的固定**：若某节点上的 worker 对某对象有**持续**访问，则该对象的 home node 固定到那个节点（放置记录里带上 home node），读直接从该节点的内存域服务。这是 N1 成立的前提。

**home node 与 §3.3 的 reactor 路由是两条不同的规则，不冲突**：home node 决定**数据常驻在哪个内存域**、内存命中时**缓冲在哪**；reactor 路由决定**设备操作交给哪个节点的线程执行**。两者都服务于「靠近数据」，但作用对象不同——一个是数据的位置，一个是执行者的位置。

home node 是**放置结果的一部分**，同样写进 WAL（§2.6 的规则不变：已落盘数据的 home node 由 WAL 决定，不重新推导）。新增该字段属于 WAL 格式变更，按 [04 §14.3](04-kernel-interfaces.md) 的流程走。

### 3.3 投递判据：投给设备所在节点的 reactor

**HRM6：设备操作投递给设备所在 NUMA 节点的 reactor。**

理由：

1. **reactor 访问的东西在设备节点最省**。reactor 的热数据是 SQ / CQ ring、提交队列、完成队列——这些是它的私有数据，first-touch 会把它们放在 reactor 所在节点。reactor 在哪个节点，这些结构就在哪个节点。
2. **唤醒调用方是一次性的小开销**。调用方在另一个节点时，完成通知要跨节点一次，代价约等于一次 cache line 失效——它**不随数据量增长**。
3. **DMA 是大头**。把 ring 和 buffer 都放对（ring 随 reactor，buffer 随 N2 的设备节点），DMA 就走近端路径。

对照 I1 的原文「提交时把操作投递给该 NUMA 节点上的 reactor」——中文里「该 NUMA 节点」是有歧义的（是「调用方所在的」还是「设备所在的」？）。本文把它定为**设备所在的**。

节点内的分派：一个节点可能有多个设备（多控制器 NVMe）。reactor 按节点分组，节点内再按 `device_id` 分派到各自的 queue。

**补充规则**：若设备所在节点没有 reactor（例如设备在 node 2 但进程只绑定了 node 0），提交到最近的有 reactor 的节点，并记录一条 `numa.fallback` 指标——这是**降级**，不是常态，且必须在启动时的拓扑检查里就被发现并告警，而不是等到第一次 I/O。

### 3.4 跨 NUMA 惩罚的进入位置

跨节点访问的惩罚有两个不同的作用位置，必须分开处理：

| 惩罚 | 进入哪个量 | 形式 |
|---|---|---|
| **DMA 跨节点**（远端内存被设备读写） | `T` 的 `eff_bw` 折减 | `eff_bw(cur → r) = nominal_bw × numa_factor` |
| **CPU 跨节点访问远端内存** | `L` 的排队项 + `T` 的 CPU 侧搬运 | `numa_factor` 同样从实测得出 |

`numa_factor` 的取值**必须实测**，不能写成常量。经验上远端访问的延迟与带宽劣化程度差异很大，取决于 CPU 型号、NUMA 拓扑、是否有 UPI / CXL 互联、访问粒度。写成「远端 = 1.5×」这种常量在另一台机器上就是错的。

**实测方式**：对每对 NUMA 节点跑一个小的 ping-pong（两线程互相读写对方的缓冲区）与一个大块流式读，分别测延迟比与带宽比，得到两张表：

```
numa_latency_factor[from][to]      延迟倍率
numa_bandwidth_factor[from][to]    带宽倍率
```

这两张表进 Hardware Profile，作为启动探测的产物。

**架构/04 §4.2 已经指出「避免在扫描循环中跨 NUMA 节点访问：按 NUMA 节点分区向量段」**——本文给出这条实践的机制：按 NUMA 节点分区 + home node 固定（§3.2）。

### 3.5 退化行为

| 情形 | 退化方式 |
|---|---|
| 单 NUMA 节点 | 所有 `numa_node` 都是 0；亲和规则自动平凡成立，无需特殊处理 |
| 无 NUMA 拓扑信息（容器内、虚拟机） | **退化为单节点模型**，不假装有亲和。`numa_node = None`，所有距离因子取 1.0 |
| GPU / CXL 内存 | 用 `device_id` 表达亲和目标（GPU 有自己的 NUMA 域）；`numa_node` 表达 CPU 侧节点 |
| 设备故障后被移除 | 该设备不再出现在候选集中；其上的数据由迁移协议处理（[03 §7](03-consistency-kernel.md)） |

**「退化为单节点模型」是一个需要明说的诚实行为**：在容器里跑时，声称有 NUMA 亲和但实际没有，只会在 benchmark 里制造假的收益。

### 3.6 与 04 的 I1–I5 的关系

| 04 的条款 | 本文的关系 |
|---|---|
| I1（每 NUMA 节点一个 reactor） | **补充**：投递目标是设备所在节点（§3.3） |
| I2（`submit_and_wait` 完成语义等价于同步调用） | 不冲突：跨节点唤醒仍在调用方线程上完成 |
| I3（核心内核只通过 `IoExecutor` 触达设备） | 不冲突：本文的规则全部在 `IoExecutor` 内部 |
| I4（reactor 不持有内核锁） | 不冲突：本文不要求 reactor 访问内核状态 |
| I5（`IoExecutor` 持有 `Box<dyn StorageDevice>`） | 不冲突：NUMA 路由在 `IoExecutor` 内部完成，不体现在类型上 |

**04 的冻结清单不变。** 本文对 `IoExecutor` 的唯一要求是：实现必须能按 `PhysicalLocation` 里的 `device_id` 找到对应的 reactor——这是实现约束，不是接口约束。

---

## 四、io_uring 队列深度与批大小

### 4.1 两个不同的量

[architecture/04 §2.2](../architecture/04-hardware-path.md) 说的是「引擎根据 IO 队列深度动态调节 batch size」——这句话把两件事混在了一起，必须分开：

| 量 | 含义 | 上限由谁决定 |
|---|---|---|
| **QD（队列深度）** | 同时在飞的 I/O 操作数上限 | 设备能力 + 域预算 |
| **batch（批大小）** | 一次 `submit_batch_and_wait` 里的操作数 | QD |

关系：`batch ≤ QD`。

还有一个第三个量：**SQ entries**（ring 的大小），它是 QD 的上限而非 QD 本身——ring 满了就得扩容或等空位。ring 的 entry 数必须是 2 的幂。

### 4.2 上限来源

```
qd_effective(r) = min( qd_device(r), qd_budget(r) )
```

| 上限 | 来源 |
|---|---|
| `qd_device(r)` | 设备上报的最大在途数。NVMe 从 identify-controller 读 `NVMOSQ` / `NVMOCQ`（**每队列**的最大在途数）与 `NVSQ` / `NVCQ`（控制器支持的队列数）；本引擎按每设备一个队列使用，因此上限取 `min(NVMOSQ, NVMOCQ)`，再与 `/sys/block/<dev>/queue/nr_requests`（内核侧保守界）取小 |
| `qd_budget(r)` | 域配置：前台 / 后台 / 其它域在这台设备上的份额之和。多个域共享设备时按域分配（[architecture/04 §6](../architecture/04-hardware-path.md) 的 token bucket） |

**初始值**：

```
qd_initial(r) = min(qd_effective(r), 32)
batch_initial = min(qd_initial(r), 16)
```

**为什么从 32 起步而不是 64/128**：SSD 内部并行度有限，队列过深会让尾延迟上升（[architecture/04 §1](../architecture/04-hardware-path.md) 的目标是 4 KB 冷读 P99 < 30 µs）。队列越深，单个请求排在队尾的时间越长。保守起步 + 实测调节，比一上来就打满安全。

**写路径用更小的 batch**（[architecture/04 §6](../architecture/04-hardware-path.md) 已定）：写路径延迟优先、batch 取 1–4；读路径吞吐优先、batch 取 16–64。

### 4.3 自适应规则

调整只看**可观测指标**，不引入模型预测：

- `inflight`：当前在飞操作数
- `lat_p50` / `lat_p99`：`submit → completion` 的延迟分位数
- `util(r)`：设备利用率（来自 `ResourceStats`）
- `baseline_p99`：该设备的**滚动基线** P99（用 EWMA 维护，防止把「一直都慢」误判成「刚刚变慢」）

| 条件 | 动作 |
|---|---|
| `inflight ≥ 0.9 × qd_effective` **且** `lat_p99 ≤ 0.8 × SLO` | **提高** QD：×1.5，向上取整到 2 的幂，上限 `qd_effective(r)` |
| `lat_p99 > 1.5 × baseline_p99` **且** 持续 N 个窗口 | **降低** QD：×0.7，向下取整到 2 的幂，下限 1 |
| `lat_p99 > SLO` **且** `util(r) < 0.6` | **不动**。设备没吃满 → 瓶颈不在设备（可能在 CPU、锁、或上游），调 QD 只会更糟 |
| `inflight < 0.5 × qd_effective` **且** `lat_p99` 远低于 SLO | **不动**。没排队，无需更深队列 |

第三条是**最容易被忽略、也最重要**的一条。「延迟超标」不等于「队列太浅」——如果设备利用率只有 50%，说明请求没进来或者卡在别处，此时加大队列只会增加内存占用和尾延迟。**必须先归因，再调整。**

调整幅度用 2 的幂取整，是因为 io_uring 的 ring 大小是 2 的幂；非幂次的队列深度在实际实现里也要向上对齐，没有收益。

### 4.4 迟滞与冷却

避免 QD 抖动（改了又改回来）：

| 参数 | 值 | 理由 |
|---|---|---|
| 升 QD 的连续满足窗口数 | 3 | 短暂的高水位不应触发加深 |
| 降 QD 的连续满足窗口数 | 1 | 延迟恶化要快速响应 |
| 调整冷却时间 | ≥ 1 个监控窗口 | 防止连续调整 |
| 每次调整的记录 | 指标 `io.qd.adjustments{device, from, to, reason}` | 可审计、可回溯 |

**降得慢、升得快是错的方向**——这里相反：**降得快、升得慢**。延迟恶化对用户是直接可感的，而队列偏浅只是浪费一点带宽。

### 4.5 背压：不静默截断

`IoExecutor::submit_and_wait` 的语义是**同步阻塞**（[04 §6.3](04-kernel-interfaces.md) I2），所以队列满时的默认表现是等待。但等待必须有上限：

```
if inflight >= qd_effective(r) and 等待时长 > backpressure_deadline:
    return Err(Retryable(Backpressure))
```

**用 `Retryable(Backpressure)` 而不是 `Io { op }`**：`Io` 表示设备错误（[04 §2.4](04-kernel-interfaces.md) 的映射规则），而背压是资源级瞬时状态，正是 `RetryReason::Backpressure` 的定义。

**并且**：内核**不自动重试**（[04 §2.3](04-kernel-interfaces.md) C1）。返回 `Retryable(Backpressure)` 后由上层决定退避重试。这是「内核不吞掉失败」这条原则的又一体现。

同时**禁止**两件事：

- **禁止静默截断**（丢掉一部分 op 只做一部分）：调用方会以为整批都完成了。
- **禁止静默降级为更小的 batch**：调用方传进来的 op 数量与实际提交的数量必须一致。

### 4.6 flush 槽位预留

**HRM7：QD 预算中必须为 flush 预留至少一个槽位。**

原因：flush 是**屏障**。在 flush 完成之前提交的所有写都必须完成——这是 [04 §4.2](04-kernel-interfaces.md) E1 的 data barrier 语义，也是 [03 §2.4](03-consistency-kernel.md)「data barrier → WAL 记录 → WAL barrier」顺序的基础。

如果 QD 已被普通读写打满，flush 排在队尾 → 事务提交延迟变成「队列排空时间」→ QD 越深，提交延迟越长。这是一个典型的**深度与延迟的负反馈**：为了提高吞吐加深队列，反而让 flush 变慢，最终提交延迟爆炸。

因此：

```
qd_budget(r) = qd_usable(r) + max(1, qd_flush_reserve(r))
```

`qd_flush_reserve` 至少为 1；写密集的域应当更高。

**flush 优先于普通读写入队**：当 flush 在等待槽位时，新来的普通读写不得抢占它的位置（否则 flush 可能被新来的读写无限推迟——饥饿）。实现上 flush 走一个**独立的优先级队列**，在槽位释放时优先分配。

### 4.7 队列隔离

| 队列 | batch | 目标 | 隔离方式 |
|---|---|---|---|
| 前台读 | 16–64 | 吞吐 | 独立 QD 预算 |
| 前台写 | 1–4 | 延迟 | 独立 QD 预算 |
| 后台重建（索引段合并等） | 32–128 | 吞吐 | 独立 QD 预算 + 可暂停 |
| WAL / flush | 见 §4.6 | 屏障 | 优先级队列 + 预留槽位 |

[architecture/04 §6](../architecture/04-hardware-path.md) 已定「前台读与后台重组走不同 HAL 队列」「每个域可设置 IO 权重，I/O 调度使用 token bucket 限流」。本文把它落到 **QD 预算**这一层：队列隔离的实质是**在飞操作数的分配**，而不是物理隔离（物理隔离只有专用部署才有）。

### 4.8 SQPOLL 的判据

`IORING_SETUP_SQPOLL` 让内核线程轮询提交队列，省掉 `io_uring_enter` 系统调用。收益明确，代价是**占用一个 CPU 核**。

判据（全部以**探测结果**为准，不做版本号假设）：

| 判据 | 不满足时 |
|---|---|
| 探测到 `IORING_SETUP_SQPOLL` 支持 | 不启用 |
| 有空闲 CPU 配额：cgroup `cpu.max` 的配额 ≥ 核数 × 1.5 | 不启用 |
| 未被 seccomp / 容器策略禁用 | 不启用 |
| 负载是读密集（提交频率高） | 写密集场景收益小，可不启用 |

**运行期不切换 SQPOLL 模式。** 理由与 §5.1 相同：切换会改变延迟特征与 CPU 占用，而运行期的系统可能正处于延迟敏感状态。SQPOLL 在**启动期**决定。

---

## 五、后端切换判据（O_DIRECT / SPDK / POSIX）

### 5.1 启动期决策，运行期只降级

[ADR-0004](../adr/0004-bypass-kernel-io.md) 定了优先级（io_uring → SPDK → POSIX），但没定判据，也没说运行期能不能换。本文定：

> **HRM8：后端选择是启动期的决策。运行期只允许能力降级，不允许切换后端。**

运行期**允许**的降级：

| 降级 | 表现 | 处理 |
|---|---|---|
| 设备故障 | `health()` → `Failed`，`max_durability()` 下降 | 该设备移出候选集；其上的数据走迁移协议（[03 §7](03-consistency-kernel.md)） |
| 设备降级 | `max_durability()` 从 `Durable` 降到 `Write` | 按 D3 可见；调用方据此决定是否复制 |
| ring 损坏 | io_uring ring 不可用 | 视为该设备故障；重建 ring 仍属 io_uring 路径；不可重建则 `Io { op }` 上抛 |

运行期**禁止**的：

- 从 io_uring 整体切到 SPDK（或反过来）；
- 从 O_DIRECT 切到 buffered（或反过来）；
- 在有并发写入时更换 `StorageDevice` 的实现。

**禁止的理由**：

1. **设备所有权互斥**。SPDK 的用户态驱动与内核 NVMe 驱动对同一设备的所有权互斥，切换要重置控制器状态——在有未完成 I/O 时这是数据损坏级别的操作。
2. **语义边界会变得不清晰**。「已提交 = 已持久」这条契约（[04 §6.3](04-kernel-interfaces.md) I2）在切换过程中无法证明：切换点的哪些写已经持久、哪些没有，取决于切换的时机。
3. **错误分类会失真**。切换期间的错误既不是设备错也不是引擎错，`KernelError` 的四分类（[04 §2.3](04-kernel-interfaces.md)）里没有它的位置。

因此，**如果 io_uring 在运行期彻底不可用**，正确的处理是：上抛 `Io { op }`，由上层决定是否重启引擎（重启后可重新探测，可能落到 POSIX 路径）。**热切换不是一个可用的选项**——把它排除掉，比留一个「大部分时候能用」的开关要好。

### 5.2 三条判据

| 判据 | 问题 | 数据来源 |
|---|---|---|
| **能力** | 这条路径在这台机器上能用吗？ | 启动探测（[architecture/04 §2.1](../architecture/04-hardware-path.md)） |
| **部署形态** | 这条路径在这个部署里**允许**用吗？ | 部署配置 |
| **性能画像** | 这条路径比当前路径**值得**吗？ | 压测数据 |

三条判据是**依次**的：能力不满足就出局；部署形态不允许就出局；前两条都过了，才比较性能画像。

#### 判据 1：能力

| 能力 | 探测方式 | 不可用时的后果 |
|---|---|---|
| io_uring | `io_uring_queue_init` + 需要的 opcode（`READ_FIXED` / `WRITE_FIXED` / `FSYNC`） | 降到 POSIX |
| SPDK | SPDK 构建存在 + root 权限 + 大页可用 + 设备独占 | 不进入 SPDK 候选 |
| O_DIRECT | `O_DIRECT` 打开成功 | 该文件走 buffered |
| HugeTLB | `MAP_HUGETLB` 成功 | 降级到普通 mmap（[architecture/04 §8](../architecture/04-hardware-path.md)） |

#### 判据 2：部署形态

**共享部署（多租户同宿主）禁止 SPDK。**

理由：SPDK 的用户态轮询会独占设备的 IO 资源并持续占用 CPU，直接影响同宿主上的其它租户。这与 [architecture/04 §6](../architecture/04-hardware-path.md) 的 noisy-neighbor 隔离目标冲突，也与「共享云环境自动降级到保守模式」相悖。

SPDK 只在**专用部署**下考虑：引擎独占 NVMe 控制器，运维上明确接受设备独占（[architecture/04 §5](../architecture/04-hardware-path.md) 已要求「明确警告设备独占」）。

#### 判据 3：性能画像

**只有在 io_uring 路径实测达不到 SLO 时，才考虑换更激进的后端。**

判据：SPDK 相对 io_uring 的压测改善必须达到一个**预设门槛**（建议 30%），且 io_uring 路径确实不满足 [architecture/04 §1](../architecture/04-hardware-path.md) 的 SLO。两个条件都满足才启用。

理由：如果 io_uring 路径的冷读 P99 已经 < 30 µs，那么引入 SPDK 带来的只是运维复杂度、设备独占约束与一整套新的故障模式，换不到 SLO。

### 5.3 决策表

先施加**部署形态**这一层过滤（共享部署直接出局，SPDK 不进入候选），再按优先级从上到下匹配，命中即停：

| 部署形态 | 能力 | 判定 | 生效路径 |
|---|---|---|---|
| 专用 | SPDK 可用 **且** 压测收益达门槛 **且** io_uring 路径实测不达 SLO | 用 SPDK | 用户态 NVMe 队列（+ O_DIRECT 语义等价） |
| 专用 / 共享 | io_uring 可用 | 用 io_uring | io_uring + O_DIRECT（能注册缓冲区则注册） |
| 任意 | io_uring 不可用 | 回退 POSIX | `preadv2` / `pwritev` + epoll；O_DIRECT 可用则用 |

这张表是**启动期一次性求值**的结果，写入日志与运行时指标（`hardware.backend`），便于排障时确认「这台机器现在跑的是哪条路径、为什么」。

### 5.4 O_DIRECT 的独立判据

O_DIRECT **不是后端选择**，是**每个文件的选择**。io_uring / SPDK / POSIX 三条路径都支持 O_DIRECT（或等价语义），选择它是独立的一层。

| 文件类别 | O_DIRECT | 理由 |
|---|---|---|
| WAL 段 | **开** | 顺序写 + 大块顺序读；绕开 page cache 避免双重缓存与写入放大 |
| 索引段文件 | **开** | 同上；段大小已对齐 `optimal_io_size`（[architecture/04 §3.2](../architecture/04-hardware-path.md)） |
| 大对象 payload | **开** | 同上 |
| 小文件 / 元数据 / 目录项 | **关** | 小于一页的随机读由 page cache 命中更快 |

**代价**（[ADR-0004](../adr/0004-bypass-kernel-io.md) 已列）：绕过 page cache 后必须自己管内存层级；写入必须对齐。

**对齐要求**：offset、length、缓冲区地址三者都要对齐到设备的逻辑块大小（通常 512 B；实践中用 4 KiB 更稳妥，因为多数 NVMe 的内部擦除块更大）。**未对齐的调用必须返回错误，不能静默降级为 buffered**——静默降级会让人误以为走了直写路径。

因此：

> **引擎的所有文件 I/O 必须走一个统一的文件层**（集中处理 O_DIRECT、对齐、错误映射），**不允许业务代码直接 `open` 文件**。否则对齐规则会被绕过，而这正是 O_DIRECT 最常见的踩坑点。

### 5.5 三个后端的耐久语义必须一致

这是 [architecture/04 §8](../architecture/04-hardware-path.md)「回退时行为一致，仅性能降级」在耐久语义上的具体化：

| `dur` | io_uring | SPDK | POSIX |
|---|---|---|---|
| `None` | 不提交 | 不提交 | 不提交 |
| `Write` | 写提交到设备易失缓存 | 同 | 同（直写设备缓存 / 写进 page cache） |
| `Durable` | `IORING_OP_FSYNC` | `nvme_ns_cmd_flush` | `fsync` / `fdatasync` |

对应的掉电后果（[04 §4.2](04-kernel-interfaces.md) E1 的表）：

| `dur` | 掉电后果 |
|---|---|
| `Durable` | **保留** |
| `Write` | **丢失**（掉电可能丢） |
| `None` | 丢失 |

**关键结论：三个后端都能满足 `Durable`，只是代价不同。** POSIX 路径的 `Durable` 需要 `fsync`（即使用了 O_DIRECT —— O_DIRECT 只让**数据**绕过 page cache，**文件元数据**（长度等）仍然走 page cache，仍然需要 `fsync` 才落盘）。

这条必须写成**可执行的对拍用例**（[05](05-kernel-test-harness.md) 的假设备）：三个后端 × 三档 `dur` × 掉电注入，行为必须完全一致。这是「回退只降性能不降语义」这句承诺的唯一保证方式。

**不允许**某条路径把 `Durable` 悄悄降级成 `Write`（哪怕性能更好）——那会让 SLO 测试通过但数据在掉电时丢失。

### 5.6 与 D3 的联动

[04 §6.1](04-kernel-interfaces.md) D3 要求设备降级可见。本文补一条联动规则：

> **设备降级后，该设备必须立即退出「要求 `Durable` 的放置」的候选集**（直到恢复）；`health() == Failed` 的设备退出全部候选集。

否则会继续把需要持久化的新数据写到一台已经 `Degraded` 的设备上——而它现在只能提供 `Write` 级别的耐久（[05 §2.3](05-kernel-test-harness.md) 明确：降级后 `flush(Durable)` 返回 `Io{op: Flush}`，**提交路径必须失败而不是静默降级持久性**）。

降级设备仍可承担 `Cache` / `WorkingSet` 角色——那些数据本来就不要求持久，放上去没有正确性风险。这与 §2.4 的「设备健康」与「耐久要求」两条硬约束是同一规则的两个视角。

---

## 六、不变量 HRM1–HRM8

与 [03 §10](03-consistency-kernel.md) 的不变量 `INV1`–`INV5`、[05](05-kernel-test-harness.md) 的注入点 `注入 I1`–`I10`、[06](06-index-recovery.md) 的不变量 `A1`–`A10` 是互不重叠的命名空间。用 `HRM` 前缀而不是继续用 `I*`，是因为仓库里 `I` 已经被两套东西占用了（[04 §6.3](04-kernel-interfaces.md) 的 `I1`–`I5` 是 `IoExecutor` 契约，[05](05-kernel-test-harness.md) 的 `注入 I1`–`I10` 是故障注入点）。

| 编号 | 不变量 | 可测试形式 |
|---|---|---|
| **HRM1** | 代价模型只在**可行集**上排序；§2.4 的八条硬约束**不可被代价越过** | 构造一个「除 DRAM 外全部不可行」的场景，断言结果不是 DRAM |
| **HRM2** | 权重无量纲：先归一化到 `[0,1]` 再加权，且 `α+β+γ+δ = 1` | 改变 `L` 的单位（ns → µs）后，排序结果不变 |
| **HRM3** | 放置决策确定性：不读时钟、不用随机源、平局按 `ResourceId` 升序 | 同输入连续两次求值 + 重启后再求值，三者结果相同 |
| **HRM4** | 放置结果写进 WAL 的映射记录；**恢复期从不重新推导** | 改权重后重启，断言已落盘对象的 `PhysicalLocation` 不变 |
| **HRM5** | 缓冲绑定分层：内存命中的读放在 home node（N1），设备取数的读放在设备节点（N2），写放在发起方节点（N3）；热点对象固定 home node | 检查缓冲区页的 NUMA 归属 == 规则要求的节点（三条各一例） |
| **HRM6** | 设备操作投递给**设备所在** NUMA 节点的 reactor | 断言提交到的是设备节点的 reactor，而非调用方节点的 |
| **HRM7** | 生效 QD = min(设备上限, 域预算)；预算中为 flush 预留 ≥1 槽；队列满超 deadline 时返回 `Retryable(Backpressure)`，不静默截断 | QD 打满时提交部分 op，断言返回 `Retryable(Backpressure)` 且无 op 被静默丢弃 |
| **HRM8** | 后端选择是启动期决策；运行期只允许能力降级；三个后端对三档 `dur` 的可观测行为一致 | 三个后端 × 三档 `dur` 的掉电对拍（[05](05-kernel-test-harness.md)） |

---

## 七、验收与 Benchmark

### 7.1 功能验收用例

| 用例 | 场景 | 断言 |
|---|---|---|
| `placement_deterministic` | 同输入连续求值 + 重启后再求值 | 三次结果相同 |
| `placement_weight_dimensionless` | 把 `L` 的单位从 ns 改成 µs | 候选排序结果不变 |
| `placement_persistence_hard_constraint` | 请求把 `Authoritative` 副本放到 DRAM | 被拒（不是「代价高」，是「不可行」） |
| `placement_capacity_constraint` | 首选资源放不下 | 自动选次优；全都不行 → `ResourceExhausted` |
| `placement_role_separation` | 对象同时在 DRAM 与 NVMe 上 | DRAM 上是 `Cache`，`Authoritative` 仍在 NVMe；两者角色可从元数据区分 |
| `placement_degraded_device_excluded` | 设备降级到 `max_durability() == Write` | 该设备退出「要求 `Durable` 的放置」候选集，但仍可承接 `Cache` / `WorkingSet` |
| `placement_replay_stable` | 改权重后重启 | 已落盘对象的物理位置逐字节不变 |
| `placement_no_clock_dependency` | 假时钟推进 | 放置结果不变 |
| `numa_buffer_first_touch` | N1 / N2 / N3 三条规则各一例 | 缓冲区的页归属节点 == 规则要求的节点 |
| `numa_home_node_fixed` | 某节点上的 worker 持续访问某对象 | 该对象的 home node 固定到该节点；重启后 home node 不变（由 WAL 决定） |
| `numa_reactor_routing` | 在非设备节点发起 I/O | op 出现在设备节点的 reactor 上 |
| `numa_fallback_observable` | 设备所在节点没有 reactor | 提交成功，且 `numa.fallback` 指标 +1 |
| `qd_respects_device_limit` | 假设备上报一个很小的队列深度上限 | 生效 QD ≤ 上报值 |
| `qd_flush_slot_reserved` | QD 被普通读写打满 | flush 仍能在 deadline 内完成 |
| `qd_backpressure_returns_retryable` | 队列满且等待超 deadline | 返回 `Retryable(Backpressure)`；断言无 op 被静默丢弃 |
| `qd_no_adjust_when_device_not_saturated` | `lat_p99 > SLO` 且 `util < 0.6` | QD 不变 |
| `backend_selection_shared_host` | 共享部署 | 断言未选 SPDK |
| `backend_selection_startup_only` | 运行期注入 io_uring 故障 | 断言未发生后端热切换，而是上抛 `Io { op }` |
| `backend_durability_parity` | 三后端 × 三档 `dur` × 掉电注入 | 掉电后的可观测行为完全一致 |
| `odirect_alignment_enforced` | 传入未对齐的 offset | 返回错误；断言未静默降级为 buffered |
| `sqpoll_disabled_on_quota` | cgroup CPU 配额不足 | 断言未启用 SQPOLL |

这些用例按 [05](05-kernel-test-harness.md) 的夹具形态实现——假设备承担掉电与「上报小队列深度上限」的角色，注入点落在**设备级**（`IoExecutor` 承载，见 [ADR-0009](../adr/0009-fault-injection-harness.md) 决策 3）。

### 7.2 Benchmark

| # | 名称 | 方法 | 关注指标 |
|---|---|---|---|
| **B1** | 放置决策开销 | 单次放置求值（含 stats 快照与 Profile 查表） | 单次成本 P50 / P99；目标：P99 < 50 µs（放置发生在写路径上，不能成为瓶颈） |
| **B2** | NUMA 亲和收益 | 同一向量扫描，分别跑在同节点与跨节点 | 有效带宽比；跨节点惩罚因子实测值（§3.4 的两张表）；**N2 场景下「缓冲在设备节点 + 消费方跨节点读一次」的实际代价**（§3.2 的已知次优路径） |
| **B3** | QD 扫描曲线 | QD ∈ {1, 8, 32, 128, 512, 2048}，固定负载 | 吞吐、lat_p50、lat_p99 的三条曲线；找出拐点（本文默认 32 是否合理） |
| **B4** | O_DIRECT vs buffered | 同一写入负载 | 写放大、吞吐、尾延迟；验证 §5.4 的分类（日志开、小文件关） |
| **B5** | 后端对比 | io_uring vs SPDK（专用部署） | 冷读 P50/P99、吞吐；为 §5.2 判据 3 的门槛提供依据 |
| **B6** | flush 槽位影响 | 相同负载，flush 预留槽位 = 1 / 4 / 16 | 事务提交延迟 P99。验证 §4.6：预留过小会让提交延迟随 QD 恶化 |

**B3 与 B6 是本文最关键的两个 benchmark**：B3 校准初始 QD，B6 校准 flush 预留槽位。其余 benchmark 验证收益而非定参。

### 7.3 与 06 的联测

`c_scan` / `c_point` 的取值必须来自 B1 / B3 / B4 的实测，且在 [06](06-index-recovery.md) 的回退策略选择中生效——**同一个数既要用于放置决策，又要用于查询规划**，因此两处必须引用同一个来源，不能各写一份。

---

## 八、与既有文档的关系

| 文档 | 关系 |
|---|---|
| [02-kernel §3.4–3.6](02-kernel.md) | 本文把代价模型从符号形式落到可计算；`MemoryAllocationRequest` 的 `numa_node` / `device_id` / `alignment` 参数在此获得用法 |
| [02-kernel §3.7](02-kernel.md) | `ResidencyRole` 是 §2.4 的硬约束来源 |
| [02-kernel §3.8](02-kernel.md) | 迁移协议在 [03 §7](03-consistency-kernel.md)；本文只提供 `M` 的计算与「迁移不是重算」的原则 |
| [03 §7](03-consistency-kernel.md) | 迁移的提交点与崩溃矩阵；本文的 `M` 是迁移的**决策依据**，不是执行协议 |
| [03 §12](03-consistency-kernel.md) | 本文是该文件列出的第 3 项深挖 |
| [04 §1.3](04-kernel-interfaces.md) | QD / batch / 亲和属于「明确不冻结」的性能参数；本文给推导方式，不给数值 |
| [04 §6.1–6.3](04-kernel-interfaces.md) | `StorageDevice` / `MemoryTierManager` / `IoExecutor` 的签名**不变**；本文的规则全部落在实现内部 |
| [04 §2.6](04-kernel-interfaces.md) C5 | 放置路径禁止读时钟（§2.5） |
| [05](05-kernel-test-harness.md) | 假设备承担掉电与设备能力上报；验收用例按其夹具形态实现 |
| [06](06-index-recovery.md) | `c_scan` / `c_point` 由本文定义（§2.7） |
| [architecture/04](../architecture/04-hardware-path.md) | Hardware Profile、回退矩阵、队列隔离的本文展开；本文的决策表是 §8 回退矩阵的细化 |
| [architecture/07 §2–§3](../architecture/07-operations.md) | 容量规划与资源治理使用本文的 `ResourceStats` 定义 |
| [ADR-0004](../adr/0004-bypass-kernel-io.md) | 本文补齐其未定的判据，并限定运行期不切换 |
| [ADR-0010](../adr/0010-hardware-resource-management.md) | 本文的决策记录 |

---

## 九、下一步

1. **决策已归档**：本文的决策记在 [ADR-0010](../adr/0010-hardware-resource-management.md)（放置代价模型的参数化方式、NUMA 亲和的缓冲分层与投递规则、QD 上限与 flush 槽位、后端选择为启动期决策、三后端耐久语义一致）。本文的 `HRM1`–`HRM8` 是这些决策的可测试形式。
2. **`AccessPattern` / `ResourceStats` / `FaultDomainId` 入冻结清单**：前两个类型在 [02-kernel](02-kernel.md) 与 [04 §6.2](04-kernel-interfaces.md) 被引用但从未定义，第三个是本文新引入的。本文 §2.3 给了建议定义，但它们进入 04 的冻结清单需要单独一条 ADR（变更方式见 [04 §14.3](04-kernel-interfaces.md)）。
3. **设备级注入点的形状**：[05 §11 第 1 项](05-kernel-test-harness.md) 尚未定——site 标签放在 `DeviceOp` 上还是 `submit_and_wait` 的参数上。§4.5 的「不允许静默截断」与 §7.1 的 `qd_backpressure_returns_retryable` 都需要设备级 site 才能定位。
4. **假设备的设备差异**：§4.2 的 `qd_device(r)` 需要假设备能上报不同的队列深度上限与对齐要求，[05 §2.3](05-kernel-test-harness.md) 的降级模型是否够用需要复核。
5. **实机校准**：§7.2 的 B1 / B3 / B6 必须在真实硬件上跑一次，第一版的初始值（QD 32、batch 16、flush 槽位 1）只是保守起点，不是结论。
