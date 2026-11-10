//! `IndexKind`：把一条 `ObjectChange` 变成索引内的动作（[08 §2.2]）。
//!
//! 纯函数：不读设备、不取锁、不读时钟。同一 `(change, view)` 永远得到同一组动作——
//! 重建段内容只由 `(from_lsn, to_lsn]` 决定，这条幂等前提（[06 §2.2]）靠的就是它。
//!
//! 动作必须携带两个 xid：`created_by_xid` / `deleted_by_xid`。它们是回收器 G3 / G4
//! 的全部输入，也是 `LiveSnapshotSource::visible` 的唯一参数——没有它们，索引层只能
//! 靠「构建完成时间」近似可见性，而 [06 §1.4] 已明确排除这种做法。
//!
//! [08 §2.2]: ../../docs/chatgpt/08-index-layer.md
//! [06 §2.2]: ../../docs/chatgpt/06-index-recovery.md
//! [06 §1.4]: ../../docs/chatgpt/06-index-recovery.md

use crate::kernel::{ObjectChange, ObjectMeta};
use crate::types::{IndexId, ObjectId, TransactionId, VersionId};

/// 一条变更对某个版本条目产生的动作（06 §2.3 的三种）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EntryAction {
    /// `Created` / `Versioned` 的新版本。
    Insert {
        version: VersionId,
        created_by_xid: TransactionId,
        payload: Vec<u8>,
    },
    /// 被新版本取代的旧版本：索引条目改挂 `deleted_by_xid`，不再删除条目本身
    /// （06 §2.3：删除旧版本是空间回收，不是正确性要求）。
    Delete {
        version: VersionId,
        deleted_by_xid: TransactionId,
    },
    /// `Deleted`：留下墓碑。旧快照需要删除前的条目，新快照需要墓碑——两个方向都要。
    Tombstone {
        version: VersionId,
        deleted_by_xid: TransactionId,
    },
    /// 命中了一个**不存在的版本**（墓碑指向 `VersionId(0)`，或版本已被回收）。
    ///
    /// 索引层必须能把这种情况说清楚：静默返回空动作，会让段内容不再只由 LSN 区间
    /// 决定（06 §2.2），重建与增量从此分叉。由构建器决定是报错还是跳过。
    Absent { version: VersionId },
}

/// 索引种类（08 §2.2）。
///
/// `view` 是构建器按变更**指定的那个版本**读出的对象视图（06 §2.2），
/// 不是「当前最新版本」。两个 xid 只能从这里取。
pub trait IndexKind: Send + Sync {
    /// 协调区与 WAL 记录里的索引标识（ADR-0012 决策 4：内核只承载、不解释）。
    fn index_id(&self) -> IndexId;

    /// 这条变更产生哪些动作。`None` 的 `view` 表示构建器没提供视图（例如删除后的对象），
    /// 此时实现只能依赖 `change` 自身携带的信息。
    fn plan(&self, change: &ObjectChange, view: Option<&ObjectView>) -> Vec<EntryAction>;
}

/// 构建器交给 `IndexKind::plan` 的那部分对象视图。
///
/// 刻意只保留 `IndexKind` 能用的字段：`xmin` / `xmax` 与元数据。物理位置不在其中——
/// 索引条目不持有物理引用（04 §14.1 规则 1）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ObjectView<'a> {
    /// 创建该版本的事务。
    pub xmin: TransactionId,
    /// 把该版本标为已删除的事务。
    pub xmax: Option<TransactionId>,
    /// 变更自带的逻辑元数据。
    pub meta: &'a ObjectMeta,
}

/// [`ObjectView`] 的持有版：构建器先把元数据搬到这里，再借出去给 `plan`。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OwnedView {
    pub xmin: TransactionId,
    pub xmax: Option<TransactionId>,
    pub meta: ObjectMeta,
}

/// 按对象类型分桶的元数据索引。
///
/// 选它作为 MVP 的第一种 `IndexKind`，是因为它不需要读 payload：动作只依赖
/// 元数据与两个 xid，因此「段内容只由 LSN 区间决定」这条性质能在没有 payload
/// 读路径的情况下被真正测到。向量 / 文本索引接进来时只需要换一个 `IndexKind`。
pub struct MetaIndex {
    id: IndexId,
}

impl MetaIndex {
    pub const fn new(id: IndexId) -> Self {
        MetaIndex { id }
    }

    fn bucket(meta: &ObjectMeta) -> Vec<u8> {
        // 桶键 = (object_type, schema_id)。租户不进键：MVP 单库，`tenant_id` 由
        // `database_id` 推导，恒等于索引所属的库。
        let mut out = Vec::with_capacity(5);
        out.push(meta.object_type);
        out.extend_from_slice(&meta.schema_id.to_le_bytes());
        out
    }
}

impl IndexKind for MetaIndex {
    fn index_id(&self) -> IndexId {
        self.id
    }

    fn plan(&self, change: &ObjectChange, view: Option<&ObjectView>) -> Vec<EntryAction> {
        match change {
            ObjectChange::Created { version, meta, .. } => match view {
                // 没有 `xmin` 就不能建条目：猜一个 xid 会让 G3 判错，宁可让构建器
                // 在上游失败（`catch_up` 在取不到视图时返回错误，不做静默跳过）。
                None => Vec::new(),
                Some(v) => vec![EntryAction::Insert {
                    version: *version,
                    created_by_xid: v.xmin,
                    payload: Self::bucket(meta),
                }],
            },
            ObjectChange::Versioned {
                version,
                previous,
                meta,
                ..
            } => {
                let Some(v) = view else {
                    return Vec::new();
                };
                let mut actions = Vec::with_capacity(2);
                // 06 §2.3：`Versioned` = delete(旧) + insert(新)。旧版本不存在时只插入。
                if let Some(prev) = previous {
                    actions.push(EntryAction::Delete {
                        version: *prev,
                        deleted_by_xid: v.xmin,
                    });
                }
                actions.push(EntryAction::Insert {
                    version: *version,
                    created_by_xid: v.xmin,
                    payload: Self::bucket(meta),
                });
                actions
            }
            // 墓碑的 `deleted_by_xid` 来自视图：删除记录自己不带事务号，而事务号
            // 只能由状态给出（ADR-0012 决策 3：可见性判定只有一套来源）。
            ObjectChange::Deleted { version, .. } => match view.and_then(|v| v.xmax) {
                // `VersionId(0)`：删除发生时没有可见版本（典型是对象刚创建就被删）。
                // 没有条目可删，也没有墓碑可立——把这种情况显式说出来。
                None if version.0 == 0 => vec![EntryAction::Absent { version: *version }],
                None => Vec::new(),
                Some(xid) => vec![EntryAction::Tombstone {
                    version: *version,
                    deleted_by_xid: xid,
                }],
            },
        }
    }
}

/// 索引条目的主键（06 §2.3）：`(object_id, version_id)`，不是 `object_id`。
///
/// 查询按快照选版本，因此索引里同时存在多个版本是正常状态。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EntryKey {
    pub object: ObjectId,
    pub version: VersionId,
}

/// 条目在段里的形态：`Live` 仍有效，`Tombstone` 描述一次删除。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntryKind {
    Live,
    Tombstone,
}
