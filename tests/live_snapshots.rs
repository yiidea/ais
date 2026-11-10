//! F17 `LiveSnapshotSource` 的行为测试（ADR-0015 决策 4）。
//!
//! 断言的是索引段回收判定（G3 / G4）依赖的那几条性质：
//! 1. 快照的存活期 = 句柄的存活期，且 `live_snapshots` 按 `SnapshotId` 排序；
//! 2. `visible` 与 `TransactionManager::visible` 是同一份判定（同 xid、同快照、同结果）；
//! 3. 可见性只看 xid 与快照：快照开始之后才提交的 xid 不可见，即使它已经提交；
//! 4. 墓碑用「执行删除的事务」表达，判定方向与条目的 `deleted_by_xid` 一致——
//!    这是 G4「墓碑不可见性独立判定」的前提；
//! 5. 事务结束后其快照不再是存活快照。

use ains::hal::mem::MemDevice;
use ains::hal::StorageDevice;
use ains::kernel::{
    CreateObjectRequest, EntryVisibility, Kernel, KernelConfig, LiveSnapshotHandle,
    LiveSnapshotSource, ObjectManager, TransactionManager,
};
use ains::types::*;
use std::sync::Arc;

fn kernel(tag: &str) -> Arc<Kernel> {
    let dir = std::env::temp_dir().join(format!("ains-live-{tag}"));
    let _ = std::fs::remove_dir_all(&dir);
    Arc::new(
        Kernel::open(
            KernelConfig::new(dir.join("ckpt.snap")),
            Box::new(MemDevice::new(DeviceId(1), 16 << 20)) as Box<dyn StorageDevice>,
        )
        .expect("open kernel"),
    )
}

fn create(kernel: &Kernel, txn: &mut ains::kernel::TxnCtx, payload: &[u8]) -> ObjectId {
    ObjectManager::create_object(
        kernel,
        txn,
        &CreateObjectRequest {
            object_type: ObjectType::Document,
            payload: payload.to_vec(),
            component_count: 1,
        },
    )
    .expect("create")
}

#[test]
fn handle_lifetime_is_liveness_and_order_is_stable() {
    let k = kernel("lifetime");
    assert!(k.live_snapshots().expect("live").is_empty());

    let a = LiveSnapshotHandle::capture(&k);
    let b = LiveSnapshotHandle::capture(&k);
    let live = k.live_snapshots().expect("live");
    assert_eq!(live.len(), 2);
    assert!(live.windows(2).all(|w| w[0].id < w[1].id));
    assert!(live.iter().any(|s| s.id == a.id()));
    assert!(live.iter().any(|s| s.id == b.id()));

    a.release();
    let live = k.live_snapshots().expect("live");
    assert_eq!(live.len(), 1);
    assert_eq!(live[0].id, b.id());

    drop(b);
    assert!(k.live_snapshots().expect("live").is_empty());
}

#[test]
fn txn_snapshot_is_live_only_while_the_txn_runs() {
    let k = kernel("txn");
    let mut txn = TransactionManager::begin(&*k, IsolationLevel::Snapshot).expect("begin");
    let live = k.live_snapshots().expect("live");
    assert_eq!(live.len(), 1);
    assert_eq!(live[0].id, txn.snapshot.id);

    let id = create(&k, &mut txn, b"x");
    assert!(!id.as_bytes().is_empty());
    TransactionManager::commit(&*k, &mut txn).expect("commit");
    assert!(k.live_snapshots().expect("live").is_empty());

    let mut txn = TransactionManager::begin(&*k, IsolationLevel::Snapshot).expect("begin");
    TransactionManager::abort(&*k, &mut txn).expect("abort");
    assert!(k.live_snapshots().expect("live").is_empty());
}

#[test]
fn visible_matches_transaction_manager() {
    let k = kernel("parity");
    let mut txn = TransactionManager::begin(&*k, IsolationLevel::Snapshot).expect("begin");
    let id = create(&k, &mut txn, b"v1");
    TransactionManager::commit(&*k, &mut txn).expect("commit");

    let snap = k.fresh_snapshot();
    let ver = ObjectManager::resolve_version(&*k, id, &snap).expect("resolve");
    let view = ObjectManager::read_version(&*k, id, ver).expect("read");

    // 同一个 xid / 快照，两个入口必须同结论。
    let entry = EntryVisibility::new(view.version.xmin, view.version.xmax);
    assert_eq!(
        TransactionManager::visible(&*k, &view.version, &snap),
        LiveSnapshotSource::visible(&*k, &entry, &snap)
    );
    assert!(LiveSnapshotSource::visible(
        &*k,
        &EntryVisibility::new(view.version.xmin, None),
        &snap
    ));
}

#[test]
fn xids_committed_after_the_snapshot_are_invisible() {
    let k = kernel("after");
    let early = LiveSnapshotHandle::capture(&k);

    // 快照之后才提交的条目：对早期快照不可见（范围关系），不是「未提交」。
    let mut txn = TransactionManager::begin(&*k, IsolationLevel::Snapshot).expect("begin");
    let late_xid = txn.xid;
    create(&k, &mut txn, b"late");
    TransactionManager::commit(&*k, &mut txn).expect("commit");
    assert_eq!(k.status(late_xid), TxnStatus::Committed);

    let entry = EntryVisibility::new(late_xid, None);
    assert!(
        !LiveSnapshotSource::visible(&*k, &entry, &early.snapshot),
        "born after the snapshot"
    );
    assert!(
        LiveSnapshotSource::visible(&*k, &entry, &k.fresh_snapshot()),
        "visible to a newer snapshot"
    );
    // 唯一活着的快照还是那个早期句柄：它看不见新条目。
    assert!(!k.visible_to_any_live(&entry).expect("any live"));

    drop(early);
    // 没有存活快照时任何人都看不见它——但那是「没有读者」，不是「不存在」。
    assert!(!k.visible_to_any_live(&entry).expect("any live"));
}

#[test]
fn tombstone_direction_is_opposite_to_the_entry() {
    let k = kernel("tombstone");
    let mut txn = TransactionManager::begin(&*k, IsolationLevel::Snapshot).expect("begin");
    let id = create(&k, &mut txn, b"to-delete");
    TransactionManager::commit(&*k, &mut txn).expect("commit");
    let created_xid = {
        let snap = k.fresh_snapshot();
        let ver = ObjectManager::resolve_version(&*k, id, &snap).expect("resolve");
        ObjectManager::read_version(&*k, id, ver)
            .expect("read")
            .version
            .xmin
    };

    let reader = LiveSnapshotHandle::capture(&k);

    let mut txn = TransactionManager::begin(&*k, IsolationLevel::Snapshot).expect("begin");
    let delete_xid = txn.xid;
    ObjectManager::delete_object(&*k, &mut txn, id).expect("delete");
    TransactionManager::commit(&*k, &mut txn).expect("commit");

    let entry = EntryVisibility::new(created_xid, Some(delete_xid));
    let tomb = EntryVisibility::tombstone(delete_xid);

    // 旧读者：条目仍可见（需要它），墓碑不可见（不需要它）。
    assert!(LiveSnapshotSource::visible(&*k, &entry, &reader.snapshot));
    assert!(!LiveSnapshotSource::visible(&*k, &tomb, &reader.snapshot));

    // 新读者：条目不可见，墓碑可见。两者都不可回收的方向正好相反（G4）。
    let fresh = k.fresh_snapshot();
    assert!(!LiveSnapshotSource::visible(&*k, &entry, &fresh));
    assert!(LiveSnapshotSource::visible(&*k, &tomb, &fresh));
}
