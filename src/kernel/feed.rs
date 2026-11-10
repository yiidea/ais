//! `ChangeFeed`（F16，[ADR-0012]）：索引追赶者读取「已提交对象变更」的唯一入口。
//!
//! 三条边界，全部来自 ADR-0012：
//!
//! 1. **只经 `WalManager` 读日志**（`ChangeFeed → WalManager` 单向）。段边界、CRC 终止
//!    语义与 `min_required_lsn` 都在日志层，这里不复制扫描逻辑，也不自己推算帧长。
//! 2. **可见性判定只有一套实现**：`Committed` 应用、`Aborted` 丢弃、`Running`
//!    既不应用也不丢弃，而是把游标停在它之前（未决事务屏障，06 §2.2）。判定来源是
//!    `TransactionManager` 背后的同一张状态表。
//! 3. **空批不得推进游标**（06 §2.7）：追赶器「扫不到东西」不能等价于「追上了」。
//!
//! [ADR-0012]: ../../../docs/adr/0012-changefeed-freeze.md

use super::{Kernel, Snapshot, TransactionManager};
use crate::error::{CorruptionKind, KernelError, KernelResult};
use crate::types::{Lsn, ObjectId, TransactionId, TxnStatus, VersionId};
use crate::wal::{RecordType, WalBody};

/// 对象的逻辑元数据（06 §1.2）。**不含任何物理引用**（04 §14.1 规则 1）。
///
/// ADR-0012 决策 7：字段可增不可改；后续新增字段必须是可选字段 + `#[non_exhaustive]`。
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ObjectMeta {
    pub object_type: u8,
    pub tenant_id: u64,
    pub schema_id: u32,
    pub component_count: u16,
}

/// 一条已提交的对象变更（06 §1.2）。
///
/// 索引侧的动作表见 06 §2.3：`Created` 与 `Versioned` 的差别只有 `previous`，
/// 但墓碑需要「删的是哪个版本」，因此 `Deleted` 也必须带版本。
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ObjectChange {
    Created {
        object: ObjectId,
        version: VersionId,
        meta: ObjectMeta,
    },
    Versioned {
        object: ObjectId,
        version: VersionId,
        previous: Option<VersionId>,
        meta: ObjectMeta,
    },
    Deleted {
        object: ObjectId,
        version: VersionId,
    },
}

impl ObjectChange {
    /// 变更涉及的对象（按对象聚合 / 去重时使用）。
    pub fn object(&self) -> ObjectId {
        match self {
            ObjectChange::Created { object, .. }
            | ObjectChange::Versioned { object, .. }
            | ObjectChange::Deleted { object, .. } => *object,
        }
    }

    /// 构建器要读的版本：`read_version(object, version)`。
    ///
    /// 06 §2.2：读的必须是记录**指定的那个版本**，不是「当前最新版本」——
    /// 版本不可变，只有这样段内容才只由 `(from_lsn, to_lsn]` 决定。
    pub fn version(&self) -> VersionId {
        match self {
            ObjectChange::Created { version, .. }
            | ObjectChange::Versioned { version, .. }
            | ObjectChange::Deleted { version, .. } => *version,
        }
    }

    /// 估算体积（字节），供 `max_bytes` 软上限使用。
    fn estimate_bytes(&self) -> u64 {
        // 两个定长标识 + 少量元数据。软上限只约束内存，不表达语义（ADR-0012 决策 5）。
        64
    }
}

/// 一次拉取的粒度上限（ADR-0012 决策 5）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChangeBatchSpec {
    /// 本批最多多少条变更。
    pub max_records: u32,
    /// 本批变更的估算总量上限（**软**上限：单条超限的记录整条返回，不在记录中间分页）。
    pub max_bytes: u64,
}

impl ChangeBatchSpec {
    pub fn new(max_records: u32, max_bytes: u64) -> Self {
        ChangeBatchSpec {
            max_records,
            max_bytes,
        }
    }

    /// 不限量的单批（小型追赶与测试用）；生产调用方应给出有界值。
    pub fn unbounded() -> Self {
        ChangeBatchSpec {
            max_records: u32::MAX,
            max_bytes: u64::MAX,
        }
    }
}

/// 一次拉取的结果。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ChangeBatch {
    /// 本批已提交变更，顺序 = WAL 顺序。
    pub changes: Vec<ObjectChange>,
    /// 下一批的起点（**记录边界**）；`None` 表示已到达 `to_lsn`。
    pub next_cursor: Option<Lsn>,
    /// 游标因未决事务屏障而停下（06 §2.2）。
    pub blocked_by_pending_txn: bool,
}

/// 对象变更流（F16）。封闭 trait：只有内核能实现。
pub trait ChangeFeed: private::Sealed {
    /// 当前对象水位：最后一条**已持久**的 WAL 位置。
    ///
    /// 这不是「索引已消费到哪」——那是索引自己的水位（`IndexManifest`，0x31）。
    /// 内核只声明「到哪为止的记录不会因掉电丢失」。
    fn object_watermark(&self) -> Lsn;

    /// 读取 `(from, to]` 区间内**已提交**的对象变更，有界分页。
    ///
    /// 关键约定（ADR-0012 决策 5）：
    /// - `next_cursor` 单调不减，且**不越过未决事务屏障**；
    /// - 分页不改变可见性结果：同一区间用不同 `max_records` 拉取，拼接后逐条相同；
    /// - 空批只允许出现在「已到 `to_lsn`」（`next_cursor = None`）或「遇屏障」
    ///   （`blocked_by_pending_txn = true`，`next_cursor` 停在屏障之前）两种情形。
    ///
    /// `from` 必须是本函数返回过的游标（或 `Lsn(0)`）：游标永远落在记录边界上，
    /// 边界由日志层给出，调用方不得自行推算帧长。
    fn scan_changes(&self, from: Lsn, to: Lsn, spec: &ChangeBatchSpec)
        -> KernelResult<ChangeBatch>;

    /// 枚举快照下可见的对象（全量重建路径，06 §3.1）。
    ///
    /// 与 `resolve_version` / 删除判定共用 [`super::txn::object_visible`]，
    /// 不另写一套可见性规则。
    fn scan_objects(&self, snap: &Snapshot) -> KernelResult<Vec<ObjectId>>;
}

mod private {
    pub trait Sealed {}
    impl Sealed for crate::kernel::Kernel {}
}

impl ChangeFeed for Kernel {
    fn object_watermark(&self) -> Lsn {
        self.wal().durable_lsn()
    }

    fn scan_changes(
        &self,
        from: Lsn,
        to: Lsn,
        spec: &ChangeBatchSpec,
    ) -> KernelResult<ChangeBatch> {
        // 读取动作经 `WalManager::scan_from`（ADR-0012 决策 4），不直接触达设备。
        let scan = self.wal().scan_from(from)?;
        // 水位之上的记录不保证持久，不能承诺给追赶者（否则崩溃后索引会多出条目）。
        // 但屏障判定要看见停在 `to` 上的那个未决事务（游标正是停在其首条记录的 LSN），
        // 否则下一批从屏障处重新出发时看不见它，就会悄悄越过屏障。
        let to = to.min(self.object_watermark());
        if to < from {
            return Ok(ChangeBatch::default());
        }
        // 06 §2.7：区间起点早于可截断水位时，区间内的记录可能已被删除。
        // 此处**必须显式失败**——扫一个空区间然后推进水位，正是那次「索引从此永远
        // 缺一块、且没有任何报错」的事故。
        if from < self.wal().min_required_lsn() {
            return Err(KernelError::Unsupported {
                feature: "change feed cursor precedes min_required_lsn: rebuild the index from a snapshot (06 §2.7)",
            });
        }

        let mut changes: Vec<ObjectChange> = Vec::new();
        let mut bytes = 0u64;
        let mut last_consumed: Option<usize> = None;
        let mut pending_barrier: Option<Lsn> = None;
        let mut full = false;

        for (idx, at) in scan.records.iter().enumerate() {
            if at.lsn > to {
                break;
            }
            let xid = at.record.xid;
            // 判定来源是 `TransactionManager::status`，不另建状态表（ADR-0012 决策 3）。
            // 系统事务（xid = 0：检查点等维护记录）不是对象事务，按已提交跳过。
            //
            // 状态判定必须在「是不是对象记录」之前：屏疑来自事务状态而不是记录类型，
            // 未决事务的**首条**记录通常是 `TxnBegin`，它本身就是屏障（06 §2.2）。
            let status = if xid.0 == 0 {
                TxnStatus::Committed
            } else {
                self.status(xid)
            };
            match status {
                TxnStatus::Committed => {}
                TxnStatus::Aborted => continue,
                // `Running`（在跑）与 `InProgress`（未知，保守）都不应用、不丢弃。
                _ => {
                    // 停在被截断后的 `to` 上：调用方拿这个游标重新拉取时，
                    // 扫描从屏障处开始，必须再次判定到同一个事务。
                    pending_barrier = Some(at.lsn.min(to));
                    break;
                }
            }
            if changes.len() as u32 >= spec.max_records
                || (bytes >= spec.max_bytes && !changes.is_empty())
            {
                // 容量已满：这条记录留给下一批，游标停在它之前。
                full = true;
                break;
            }
            let Some(body) = decode_object_body(at.record.record_type, &at.record.body)? else {
                // 本批已满之前的非对象记录照常跳过（不影响 `changes` 与游标）。
                last_consumed = Some(idx);
                continue;
            };
            // 被折叠的记录也算「已消费」：下一批不会回到它前面，结果因此与分页无关。
            last_consumed = Some(idx);
            let Some(change) = change_from_body(body) else {
                continue;
            };
            // 新建对象会写两条记录（`ObjectCreate` + 首个 `ObjectVersion`），但只有一条
            // 逻辑变更：`Created.version` 就是那个版本（06 §2.3：Created → insert 一次）。
            if let ObjectChange::Versioned {
                object,
                previous: None,
                ..
            } = &change
            {
                if self.created_by(*object, xid) {
                    continue;
                }
            }
            bytes += change.estimate_bytes();
            changes.push(change);
        }

        if scan.stopped_at_corruption {
            // 窗口内出现残缺帧：后续记录不可读，不能宣称追赶完成。
            return Err(KernelError::corrupted(CorruptionKind::WalRecordCrc));
        }

        Ok(ChangeBatch {
            changes,
            next_cursor: match pending_barrier {
                // 屏障优先于容量：下一批必须从屏障前重新扫，哪怕本批还没满。
                Some(barrier) => Some(barrier),
                // 游标 = 后一条记录的 LSN（记录边界来自日志层的扫描结果，不自行推算帧长）。
                // 一条也没放得下时游标原地不动（空批不得推进，06 §2.7）。
                None if full => Some(
                    last_consumed
                        .and_then(|i| scan.records.get(i + 1))
                        .map(|r| r.lsn)
                        .unwrap_or(from),
                ),
                None => None,
            },
            blocked_by_pending_txn: pending_barrier.is_some(),
        })
    }

    fn scan_objects(&self, snap: &Snapshot) -> KernelResult<Vec<ObjectId>> {
        let st = self.state.lock().unwrap();
        Ok(st
            .objects
            .iter()
            .filter(|(_, o)| super::txn::object_visible(o, snap, &st.txn_status))
            .map(|(id, _)| *id)
            .collect())
    }
}

impl Kernel {
    /// `object` 是否由 `xid` 创建（用于把「创建 + 首个版本」折叠成一条变更）。
    ///
    /// 判定读内核状态而不是「本批里前面那条记录」，因此**与分页边界无关**。
    fn created_by(&self, object: ObjectId, xid: TransactionId) -> bool {
        let st = self.state.lock().unwrap();
        st.objects
            .get(&object)
            .map(|o| o.created_xid == xid)
            .unwrap_or(false)
    }
}

/// 解出对象记录 body；`Ok(None)` = 非对象变更记录（06 §2.2 前三条筛出规则）。
fn decode_object_body(record_type: RecordType, body: &[u8]) -> KernelResult<Option<WalBody>> {
    match record_type {
        RecordType::ObjectCreate | RecordType::ObjectVersion | RecordType::ObjectDelete => {}
        // Extent / Mapping（0x20–0x23）是存储布局变更，不改变对象身份与版本；
        // Index / Maintenance（0x30–0x31、0x40–0x44）是各自的构建进度与系统事务。
        _ => return Ok(None),
    }
    Ok(Some(WalBody::decode(record_type, body)?))
}

/// 对象记录 → 变更。`None` = 该记录不产生变更（删除时目标版本为 0，即删除发生时
/// 没有可见版本：对象在快照之外本就不存在，索引无需墓碑）。
fn change_from_body(body: WalBody) -> Option<ObjectChange> {
    Some(match body {
        WalBody::ObjectCreate {
            object_id,
            object_type,
            component_count,
        } => ObjectChange::Created {
            object: object_id,
            version: VersionId(1),
            meta: ObjectMeta {
                object_type,
                // MVP 只有单库：`database_id` 就是逻辑租户；schema 由上层填充。
                tenant_id: object_id.database_id() as u64,
                schema_id: 0,
                component_count,
            },
        },
        WalBody::ObjectVersion {
            object_id,
            new_version,
            old_version,
            extent,
        } => ObjectChange::Versioned {
            object: object_id,
            version: new_version,
            // `old_version == 0`：版本链从头开始（通常是被折叠的 `Created` 首个版本）。
            previous: (old_version.0 != 0).then_some(old_version),
            meta: ObjectMeta {
                object_type: 0,
                tenant_id: object_id.database_id() as u64,
                schema_id: 0,
                component_count: u16::from(extent != 0),
            },
        },
        WalBody::ObjectDelete { object_id, version } => {
            if version.0 == 0 {
                return None;
            }
            ObjectChange::Deleted {
                object: object_id,
                version,
            }
        }
        _ => unreachable!("change_from_body 只接收 0x10–0x12 三类记录"),
    })
}
