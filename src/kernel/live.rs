//! `LiveSnapshotSource`（F17，[ADR-0015] 决策 4）：内核提供给索引层回收器的
//! 唯一可见性输入。
//!
//! 索引段的回收判定（G3 / G4）要回答两个问题：**有哪些快照还活着**，以及
//! **某个条目对某个快照是否可见**。第二个问题必须只有一份实现——索引层若自己
//! 复制一份，日积月累就会与内核分叉，而分叉的后果是「索引静默少一块」或
//! 「墓碑被误删导致删除复活」，两者都不会报错。
//!
//! 三条边界：
//!
//! 1. **只读、单向**：`LiveSnapshotSource` 只被内核之外的索引层调用（04 §12.2），
//!    内核没有任何路径回调它。
//! 2. **判定复用** `TransactionManager::visible` 背后的同一函数
//!    （[`super::txn::version_visible`]）：不读 LSN、不读 `VersionId`、不读时钟
//!    （06 §6.1 A1）。`EntryVisibility` 里没有这些字段，所以想违反也无从下手。
//! 3. **存活 = 持有句柄**：快照的寿命由 [`LiveSnapshotHandle`] 的 `Drop` 决定，
//!    而不是由「事务是否还在跑」推断。索引层的全量重建不需要事务，却同样需要一个
//!    在整个重建期间都算数的快照。
//!
//! [ADR-0015]: ../../../docs/adr/0015-index-segment-reclamation.md

use super::{Kernel, Snapshot};
use crate::error::KernelResult;
use crate::types::{SnapshotId, TransactionId};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};

/// 一个索引条目参与可见性判定所需的全部信息（ADR-0015 决策 4）。
///
/// 只有两个 xid：谁创建了它、谁删除了它。payload、物理位置、LSN、版本号
/// 都不在这里——它们要么与可见性无关，要么正是 06 §6.1 A1 禁止使用的近似量。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct EntryVisibility {
    pub created_by_xid: TransactionId,
    /// `None` = 条目仍有效；`Some(xid)` = 被 `xid` 删除。
    pub deleted_by_xid: Option<TransactionId>,
}

impl EntryVisibility {
    pub fn new(created_by_xid: TransactionId, deleted_by_xid: Option<TransactionId>) -> Self {
        EntryVisibility {
            created_by_xid,
            deleted_by_xid,
        }
    }

    /// 墓碑条目：它**描述的删除**由 `deleted_by_xid` 执行。
    ///
    /// 回收器要判的是「这个删除对某个快照是否可见」。可见的旧快照仍需要删除前的
    /// 条目，因此墓碑本身反而不可回收（G4）。用同一条判定表达：墓碑的「创建者」
    /// 就是执行删除的事务，且它没有后继删除。
    pub fn tombstone(deleted_by_xid: TransactionId) -> Self {
        EntryVisibility {
            created_by_xid: deleted_by_xid,
            deleted_by_xid: None,
        }
    }
}

/// 存活快照集合 + 逐条目可见性（F17）。封闭 trait：只有内核能实现。
pub trait LiveSnapshotSource: private::Sealed {
    /// 当前存活的快照，按 `SnapshotId` 排序（判定因此可复现）。
    fn live_snapshots(&self) -> KernelResult<Vec<Snapshot>>;

    /// 条目 `entry` 对 `snap` 是否可见。
    ///
    /// 与 `TransactionManager::visible` 是同一份实现（ADR-0015 决策 4）。
    fn visible(&self, entry: &EntryVisibility, snap: &Snapshot) -> bool;

    /// 条目对**至少一个**存活快照可见（G3 / G4 的直接形态）。
    ///
    /// 没有存活快照时返回 `false`：没有读者需要它，这正是回收器要的答案。
    fn visible_to_any_live(&self, entry: &EntryVisibility) -> KernelResult<bool> {
        Ok(self
            .live_snapshots()?
            .iter()
            .any(|s| self.visible(entry, s)))
    }
}

mod private {
    pub trait Sealed {}
    impl Sealed for crate::kernel::Kernel {}
}

impl LiveSnapshotSource for Kernel {
    fn live_snapshots(&self) -> KernelResult<Vec<Snapshot>> {
        let mut out: Vec<Snapshot> = self.live.lock().unwrap().values().cloned().collect();
        out.sort_by_key(|s| s.id);
        Ok(out)
    }

    fn visible(&self, entry: &EntryVisibility, snap: &Snapshot) -> bool {
        let st = self.state.lock().unwrap();
        super::txn::version_visible(
            entry.created_by_xid,
            entry.deleted_by_xid,
            snap,
            &st.txn_status,
        )
    }
}

/// 交出去的一"份"存活快照：持有即存活，`Drop` 即释放。
///
/// 只携带快照值，不携带任何写能力——索引层拿不到 `Kernel` 的其余入口。
#[derive(Debug)]
pub struct LiveSnapshotHandle {
    /// 冻结下来的快照。
    pub snapshot: Snapshot,
    kernel: Weak<Kernel>,
    registered: AtomicBool,
}

impl LiveSnapshotHandle {
    /// 在 `kernel` 上冻结一个当前时刻的快照，并在它被释放前计入 `live_snapshots`。
    pub fn capture(kernel: &Arc<Kernel>) -> Self {
        let snapshot = kernel.fresh_snapshot();
        kernel
            .live
            .lock()
            .unwrap()
            .insert(snapshot.id, snapshot.clone());
        LiveSnapshotHandle {
            snapshot,
            kernel: Arc::downgrade(kernel),
            registered: AtomicBool::new(true),
        }
    }

    /// 快照标识。
    pub fn id(&self) -> SnapshotId {
        self.snapshot.id
    }

    /// 立即释放（与 `Drop` 等价，便于提前归还）。
    pub fn release(&self) {
        if self.registered.swap(false, Ordering::SeqCst) {
            if let Some(k) = self.kernel.upgrade() {
                k.live.lock().unwrap().remove(&self.snapshot.id);
            }
        }
    }
}

impl Drop for LiveSnapshotHandle {
    fn drop(&mut self) {
        self.release();
    }
}
