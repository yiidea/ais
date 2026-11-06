//! 事务上下文、快照与可见性判定（03 §3.3，04 §8）。

use crate::types::{
    ExtentId, IsolationLevel, Lsn, MappingId, ObjectId, SnapshotId, TransactionId, TxnStatus,
    VersionId, VersionState,
};
use std::collections::{BTreeMap, BTreeSet};

/// 事务快照：`xmin / xmax / active_xids` 三元组（03 §3.3）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Snapshot {
    pub id: SnapshotId,
    pub xmin: TransactionId,
    pub xmax: TransactionId,
    pub active: BTreeSet<TransactionId>,
}

impl Snapshot {
    /// 空快照（系统内部使用）。
    pub fn empty() -> Self {
        Snapshot {
            id: SnapshotId(0),
            xmin: TransactionId(0),
            xmax: TransactionId(1),
            active: BTreeSet::new(),
        }
    }
}

/// 待提交写入：三类分开，对应三类 WAL 记录（04 §3.3）。
#[derive(Clone, Debug, Default)]
pub struct WriteSet {
    pub version_writes: Vec<(ObjectId, VersionId)>,
    pub extent_allocs: Vec<ExtentId>,
    pub mapping_changes: Vec<(ExtentId, MappingId)>,
}

/// 内核内部唯一允许跨模块传递的可变状态载体（04 §3.3）。
#[derive(Clone, Debug)]
pub struct TxnCtx {
    pub xid: TransactionId,
    pub state: TxnStatus,
    pub snapshot: Snapshot,
    pub read_set: Vec<(ObjectId, VersionId)>,
    pub write_set: WriteSet,
    pub start_lsn: Lsn,
    pub commit_lsn: Option<Lsn>,

    /// 同一事务上一条 WAL 记录的 LSN（prev_lsn 链）。
    pub(crate) prev_lsn: Lsn,
    /// 本事务创建的对象。
    pub(crate) pending_creates: Vec<(ObjectId, crate::types::ObjectType)>,
    /// 本事务创建的 Pending 版本。
    pub(crate) pending_versions: Vec<(ObjectId, VersionId)>,
    /// 本事务分配的 extent。
    pub(crate) pending_extents: Vec<ExtentId>,
    /// 本事务创建的映射。
    pub(crate) pending_mappings: Vec<(ExtentId, MappingId)>,
    /// 本事务待删除的对象，以及删除发生时可见的版本。
    ///
    /// 版本链是追加的：删除提交后仍可能有并发事务追加更高的版本号。因此墓碑的目标
    /// 版本必须在删除**发生时**确定并随 WAL 记录持久，不能到提交时再取「最新版本」
    /// （06 §2.6：墓碑标记的是 `(object_id, version)`）。
    pub(crate) pending_deletes: Vec<(ObjectId, VersionId)>,
    /// 事务的隔离级别（MVP 固定为 `Snapshot`）。
    pub isolation: IsolationLevel,
}

impl TxnCtx {
    pub fn new(
        xid: TransactionId,
        snapshot: Snapshot,
        start_lsn: Lsn,
        isolation: IsolationLevel,
    ) -> Self {
        TxnCtx {
            xid,
            state: TxnStatus::Running,
            snapshot,
            read_set: Vec::new(),
            write_set: WriteSet::default(),
            start_lsn,
            commit_lsn: None,
            prev_lsn: Lsn(0),
            pending_creates: Vec::new(),
            pending_versions: Vec::new(),
            pending_extents: Vec::new(),
            pending_mappings: Vec::new(),
            pending_deletes: Vec::new(),
            isolation,
        }
    }
}

/// 事务状态数组的只读视图。
pub type TxnStatusMap = BTreeMap<TransactionId, TxnStatus>;

/// `committed_in_snapshot`（03 §3.3）。
///
/// 只看 xid + 事务状态 + 快照三元组，不使用 VersionId / LSN 近似（R3）。
pub fn committed_in_snapshot(
    xid: TransactionId,
    snap: &Snapshot,
    status: &TxnStatusMap,
) -> bool {
    if xid.0 == 0 {
        // 系统事务/预置数据视为已提交。
        return true;
    }
    if xid.0 >= snap.xmax.0 {
        return false;
    }
    if snap.active.contains(&xid) {
        return false;
    }
    matches!(status.get(&xid), Some(TxnStatus::Committed))
}

/// 版本可见性（03 §3.3）。
pub fn version_visible(
    xmin: TransactionId,
    xmax: Option<TransactionId>,
    snap: &Snapshot,
    status: &TxnStatusMap,
) -> bool {
    if !committed_in_snapshot(xmin, snap, status) {
        return false;
    }
    match xmax {
        None => true,
        Some(x) => !committed_in_snapshot(x, snap, status),
    }
}

/// 快照下可见的最新已发布版本（`VersionId(0)` = 没有可见版本）。
///
/// `resolve_version`、删除时的墓碑判定与 `ChangeFeed` 的 `scan_objects` 共用这一处
/// 判定：可见性逻辑一旦分叉，就是 03 §1.1 R3 的「用序号近似可见性」。
pub fn latest_visible_version(
    entry: &crate::kernel::state::ObjectEntry,
    snap: &Snapshot,
    status: &TxnStatusMap,
) -> VersionId {
    entry
        .versions
        .iter()
        .rev()
        .filter(|v| v.state == VersionState::Published)
        .find(|v| version_visible(v.xmin, v.xmax, snap, status))
        .map(|v| v.version)
        .unwrap_or(VersionId(0))
}

/// 快照下对象本身是否可见：未被删除事务隐藏，且至少有一个可见版本。
pub fn object_visible(
    entry: &crate::kernel::state::ObjectEntry,
    snap: &Snapshot,
    status: &TxnStatusMap,
) -> bool {
    if let Some(del) = entry.deleted_xid {
        if committed_in_snapshot(del, snap, status) {
            return false;
        }
    }
    latest_visible_version(entry, snap, status).0 != 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(xmin: u64, xmax: u64, active: &[u64]) -> Snapshot {
        Snapshot {
            id: SnapshotId(1),
            xmin: TransactionId(xmin),
            xmax: TransactionId(xmax),
            active: active.iter().map(|x| TransactionId(*x)).collect(),
        }
    }

    #[test]
    fn committed_visibility_uses_status_not_lsn() {
        let mut status = TxnStatusMap::new();
        status.insert(TransactionId(5), TxnStatus::Committed);
        status.insert(TransactionId(6), TxnStatus::Aborted);
        let s = snap(1, 10, &[]);
        assert!(committed_in_snapshot(TransactionId(5), &s, &status));
        assert!(!committed_in_snapshot(TransactionId(6), &s, &status));
        // 快照之后才开始的事务不可见。
        assert!(!committed_in_snapshot(TransactionId(10), &s, &status));
    }

    #[test]
    fn active_xid_is_invisible_even_if_committed_later() {
        let mut status = TxnStatusMap::new();
        status.insert(TransactionId(5), TxnStatus::Committed);
        let s = snap(1, 10, &[5]);
        assert!(!committed_in_snapshot(TransactionId(5), &s, &status));
    }

    #[test]
    fn xmax_hides_old_version() {
        let mut status = TxnStatusMap::new();
        status.insert(TransactionId(5), TxnStatus::Committed);
        status.insert(TransactionId(6), TxnStatus::Committed);
        let s = snap(1, 10, &[]);
        assert!(!version_visible(
            TransactionId(5),
            Some(TransactionId(6)),
            &s,
            &status
        ));
        assert!(version_visible(TransactionId(5), None, &s, &status));
    }
}
