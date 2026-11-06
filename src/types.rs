//! 标识符、对象模型与枚举（04 §2.1，F1 冻结清单）。

use std::fmt;

/// 逻辑对象标识：16 字节定长不可变值。
///
/// 内部编码 `(database_id: u32 BE, sequence: u64 BE, reserved: u32)`。
/// `database_id` / `sequence` 是逻辑视图，不是存储布局（04 §2.1）。
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ObjectId([u8; 16]);

impl ObjectId {
    pub const ZERO: ObjectId = ObjectId([0u8; 16]);

    pub fn new(database_id: u32, sequence: u64) -> Self {
        let mut raw = [0u8; 16];
        raw[0..4].copy_from_slice(&database_id.to_be_bytes());
        raw[4..12].copy_from_slice(&sequence.to_be_bytes());
        ObjectId(raw)
    }

    pub fn from_bytes(raw: [u8; 16]) -> Self {
        ObjectId(raw)
    }

    pub fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }

    pub fn database_id(&self) -> u32 {
        u32::from_be_bytes([self.0[0], self.0[1], self.0[2], self.0[3]])
    }

    pub fn sequence(&self) -> u64 {
        let mut b = [0u8; 8];
        b.copy_from_slice(&self.0[4..12]);
        u64::from_be_bytes(b)
    }
}

impl fmt::Debug for ObjectId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ObjectId({}:{})", self.database_id(), self.sequence())
    }
}

impl fmt::Display for ObjectId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:08x}{:016x}", self.database_id(), self.sequence())
    }
}

macro_rules! id_type {
    ($name:ident, $doc:literal) => {
        #[doc = $doc]
        #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Default)]
        pub struct $name(pub u64);

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}={}", stringify!($name), self.0)
            }
        }
    };
}

id_type!(VersionId, "对象内单调的版本号；禁止跨 ObjectId 比较（F1.2）。");
id_type!(TransactionId, "全局单调事务号；回收的 xid 槽位不得复用（F1.5）。");
id_type!(Lsn, "WAL 逻辑字节偏移；只描述写入先后（F1.3）。");
id_type!(ExtentId, "逻辑 extent 标识。");
id_type!(PhysicalExtentId, "物理 extent 标识。");
id_type!(MappingId, "映射记录标识。");
id_type!(MappingVersion, "单个 Extent 内部的映射代次（F1.4）。");
id_type!(SnapshotId, "快照标识。");
id_type!(CheckpointId, "检查点标识。");
id_type!(AllocationId, "内存/设备分配标识。");
id_type!(DeviceId, "设备标识。");

/// 组件标识（对象内部数据划分）。
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Default)]
pub struct ComponentId(pub u32);

/// 统一 AI 对象模型的七种载荷（README §5.1）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum ObjectType {
    Vector = 1,
    Graph = 2,
    Tensor = 3,
    Document = 4,
    Memory = 5,
    Media = 6,
    Event = 7,
}

impl ObjectType {
    pub fn from_u8(v: u8) -> Option<Self> {
        Some(match v {
            1 => ObjectType::Vector,
            2 => ObjectType::Graph,
            3 => ObjectType::Tensor,
            4 => ObjectType::Document,
            5 => ObjectType::Memory,
            6 => ObjectType::Media,
            7 => ObjectType::Event,
            _ => return None,
        })
    }
}

/// Extent 布局类型（02-kernel §2.3，第一版固定六种）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum LayoutType {
    Fixed = 0,
    Variable = 1,
    TensorTile = 2,
    GraphPartition = 3,
    Append = 4,
    MediaChunk = 5,
}

/// 耐久等级（04 §6.1 D2）。
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
#[repr(u8)]
pub enum DurabilityLevel {
    /// 仅在内存/易失设备上，不保证持久。
    None = 0,
    /// 已下发到设备缓存，掉电可能丢失。
    Write = 1,
    /// 已落到持久介质，掉电不丢。
    Durable = 2,
}

impl DurabilityLevel {
    pub fn from_u8(v: u8) -> Option<Self> {
        Some(match v {
            0 => DurabilityLevel::None,
            1 => DurabilityLevel::Write,
            2 => DurabilityLevel::Durable,
            _ => return None,
        })
    }
}

/// 设备的健康状态（F18，[ADR-0016]）。由设备/后端被动上报，内核不主动探测。
///
/// [ADR-0016]: ../../docs/adr/0016-resource-types-freeze.md
#[non_exhaustive]
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum DeviceHealth {
    /// 全部能力可用。
    Healthy,
    /// 能力下降：可继续承载数据，但能力集合收缩。
    Degraded(DegradedCapability),
    /// 不可用：所有 I/O 返回 `Io{op}`，不再进入任何候选集（F18）。
    Failed,
}

/// 降级时**具体**失去了什么（F18）。
///
/// 必须可枚举，不能只是一个字符串：放置器要据此判断设备是否仍可承接 `Cache` 角色，
/// 而不是解析「队列深度下降」还是「写缓存关闭」这类措辞（ADR-0016 决策 3 的约束）。
#[non_exhaustive]
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum DegradedCapability {
    /// `max_durability()` 下降（例如 `Durable` → `Write`）。
    DurabilityReduced,
    /// 队列深度上限下降。
    QueueDepthReduced,
    /// discard 不再可用。
    DiscardUnavailable,
    /// 写缓存被禁用（掉电即丢，等价于只能提供 `Write`）。
    WriteCacheDisabled,
}

impl DeviceHealth {
    /// F18 的一致性要求：报告 `DurabilityReduced` 时 `max_durability()` 必须真的低于
    /// `Durable`，反之亦然。矛盾状态必须被断言捕获（ADR-0016 决策 3）。
    pub fn is_consistent_with(&self, max_durability: DurabilityLevel) -> bool {
        match self {
            DeviceHealth::Healthy => max_durability == DurabilityLevel::Durable,
            DeviceHealth::Degraded(DegradedCapability::DurabilityReduced) => {
                max_durability < DurabilityLevel::Durable
            }
            // 其它降级能力（队列深度、discard、写缓存）与耐久等级无关。
            DeviceHealth::Degraded(_) => true,
            // 不可用设备不承诺任何耐久等级；但设备能返回 Durable 却说 Failed 同样是矛盾。
            DeviceHealth::Failed => max_durability < DurabilityLevel::Durable,
        }
    }
}

/// 资源域统计快照（F19，[ADR-0016]）。一次采样，不带时间戳，不含设备 ID。
///
/// [ADR-0016]: ../../docs/adr/0016-resource-types-freeze.md
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ResourceStats {
    /// 总容量（字节）。
    pub capacity: u64,
    /// 已使用（字节）；对应 02-kernel §3.2 的 `allocated`。
    pub used: u64,
    /// 已预留、不可被普通分配动用（字节）。
    pub reserved: u64,
    /// 资源类型。
    pub resource_type: crate::error::ResourceType,
    /// 归属 NUMA 节点；`None` = 无 NUMA 信息（容器 / VM）。
    pub numa_node: Option<u32>,
    /// 设备健康。
    pub health: DeviceHealth,
    /// 故障域；`None` = 未知。
    pub fault_domain: Option<FaultDomainId>,
}

impl ResourceStats {
    /// 可分配余量。**导出而非存储**：存下来就会出现「三字段互相矛盾」的状态（F19）。
    pub fn available(&self) -> u64 {
        self.capacity.saturating_sub(self.used).saturating_sub(self.reserved)
    }
}

/// 访问模式（F20，[ADR-0016]）。闭集、可判定，由对象头统计或 `PlacementRequest` 显式给出。
///
/// 冻结的不只是取值，还有一条边界：它**不影响放置**，只影响 I/O 路径
/// （队列选择、batch 大小，07 §4.7）。放置器不得自行推断访问模式。
///
/// [ADR-0016]: ../../docs/adr/0016-resource-types-freeze.md
#[non_exhaustive]
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum AccessPattern {
    Sequential,
    RandomPoint,
    Hotspot,
    WriteOnly,
    Cold,
}

/// 故障域标识（F21，[ADR-0016]）：共享同一失效点的一组设备。
///
/// 同一故障域内的两个副本**不构成冗余**。域 ID 是部署配置而不是探测结果；
/// 未声明意味着所有设备同域 `FaultDomainId(0)`——保守，不假装有冗余。
///
/// [ADR-0016]: ../../docs/adr/0016-resource-types-freeze.md
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Default)]
pub struct FaultDomainId(pub u32);

impl FaultDomainId {
    /// 未声明故障域时所有设备落入的域。
    pub const UNDECLARED: FaultDomainId = FaultDomainId(0);
}

/// 隔离级别：MVP 只有快照隔离（04 §8 T5）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum IsolationLevel {
    Snapshot,
}

/// 事务状态（03 §4.2）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum TxnStatus {
    InProgress = 0,
    Running = 1,
    Committed = 2,
    Aborted = 3,
}

impl TxnStatus {
    pub fn from_u8(v: u8) -> Option<Self> {
        Some(match v {
            0 => TxnStatus::InProgress,
            1 => TxnStatus::Running,
            2 => TxnStatus::Committed,
            3 => TxnStatus::Aborted,
            _ => return None,
        })
    }
}

/// 版本状态机（03 §4.1）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum VersionState {
    Pending = 0,
    Published = 1,
    Obsolete = 2,
    Reclaimable = 3,
    Aborted = 4,
}

impl VersionState {
    pub fn from_u8(v: u8) -> Option<Self> {
        Some(match v {
            0 => VersionState::Pending,
            1 => VersionState::Published,
            2 => VersionState::Obsolete,
            3 => VersionState::Reclaimable,
            4 => VersionState::Aborted,
            _ => return None,
        })
    }
}

/// 对象头（不含 payload）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObjectHeader {
    pub id: ObjectId,
    pub object_type: ObjectType,
    pub latest_version: VersionId,
    pub created_xid: TransactionId,
    pub deleted_xid: Option<TransactionId>,
}

/// 版本的元数据视图。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObjectVersion {
    pub object_id: ObjectId,
    pub version: VersionId,
    /// 创建该版本的事务。
    pub xmin: TransactionId,
    /// 删除该版本的事务（版本被更新后设置）。
    pub xmax: Option<TransactionId>,
    pub state: VersionState,
    /// payload 所在的逻辑 extent。
    pub extent: Option<ExtentId>,
    pub metadata: std::collections::BTreeMap<String, String>,
}

/// 对象视图：头 + 指定版本的元数据（payload 另经 ExtentManager 读取）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObjectView {
    pub header: ObjectHeader,
    pub version: ObjectVersion,
}

/// 逻辑 payload 引用（不含物理地址，F14）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PayloadRef {
    pub extent: ExtentId,
    pub offset: u64,
    pub length: u64,
}

/// 物理位置：只允许出现在 HAL 与内核内部 trait 中（04 §14.1）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PhysicalLocation {
    pub device: DeviceId,
    pub offset: u64,
    pub length: u64,
}
