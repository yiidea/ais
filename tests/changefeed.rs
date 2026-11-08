//! F16 `ChangeFeed` 的行为测试（ADR-0012）。
//!
//! 只测 ADR 与 06 §2.2 写进类型里的那几条性质，不测实现细节：
//! 1. 只看见已提交变更：未决事务的写入不出现，已中止的不出现；
//! 2. 游标**不越过未决事务屏障**，事务结局确定后下一轮能越过它；
//! 3. 分页不改变可见性结果：任意 `max_records` 下拼接结果逐条相同；
//! 4. 空批不推进游标；
//! 5. `scan_objects` 与 `object_watermark` 的口径；
//! 6. 区间起点早于可截断水位时**显式失败**（06 §2.7）。

use ains::hal::mem::MemDevice;
use ains::hal::StorageDevice;
use ains::kernel::{
    ChangeBatchSpec, ChangeFeed, CheckpointManager, CreateObjectRequest, Kernel, KernelConfig,
    ObjectChange, ObjectManager, TransactionManager, TxnCtx, VersionSpec,
};
use ains::types::*;

fn kernel(tag: &str) -> Kernel {
    let dir = std::env::temp_dir().join(format!("ains-feed-{tag}"));
    let _ = std::fs::remove_dir_all(&dir);
    let cfg = KernelConfig::new(dir.join("ckpt.snap"));
    Kernel::open(
        cfg,
        Box::new(MemDevice::new(DeviceId(1), 16 << 20)) as Box<dyn StorageDevice>,
    )
    .expect("open kernel")
}

fn create(kernel: &Kernel, txn: &mut TxnCtx, payload: &[u8]) -> ObjectId {
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

fn commit_create(kernel: &Kernel, payload: &[u8]) -> ObjectId {
    let mut txn = TransactionManager::begin(kernel, IsolationLevel::Snapshot).expect("begin");
    let id = create(kernel, &mut txn, payload);
    TransactionManager::commit(kernel, &mut txn).expect("commit");
    id
}

/// 从 `from` 开始按 `spec` 一直拉到 `to`（或遇屏障），返回拼接结果与批次数。
fn drain(
    kernel: &Kernel,
    from: Lsn,
    to: Lsn,
    spec: &ChangeBatchSpec,
) -> (Vec<ObjectChange>, usize) {
    let mut all = Vec::new();
    let mut cursor = from;
    let mut batches = 0usize;
    loop {
        let b = kernel.scan_changes(cursor, to, spec).expect("scan_changes");
        batches += 1;
        assert!(
            batches < 10_000,
            "cursor failed to advance: stuck at {cursor:?}"
        );
        all.extend(b.changes);
        if b.blocked_by_pending_txn {
            break;
        }
        match b.next_cursor {
            None => break,
            Some(next) => {
                assert!(
                    next > cursor,
                    "cursor must strictly advance ({cursor:?} -> {next:?})"
                );
                cursor = next;
            }
        }
    }
    (all, batches)
}

#[test]
fn create_and_version_are_reported_in_wal_order() {
    let k = kernel("create");
    let a = commit_create(&k, b"a");

    let mut txn = TransactionManager::begin(&k, IsolationLevel::Snapshot).unwrap();
    ObjectManager::stage_version(
        &k,
        &mut txn,
        a,
        &VersionSpec {
            payload: b"a-v2".to_vec(),
        },
    )
    .unwrap();
    TransactionManager::commit(&k, &mut txn).unwrap();

    let to = ChangeFeed::object_watermark(&k);
    let (changes, _) = drain(&k, Lsn(0), to, &ChangeBatchSpec::unbounded());
    assert_eq!(
        changes.len(),
        2,
        "one create + one version, got {changes:?}"
    );
    match &changes[0] {
        ObjectChange::Created {
            object,
            version,
            meta,
        } => {
            assert_eq!(*object, a);
            assert_eq!(*version, VersionId(1));
            assert_eq!(meta.object_type, ObjectType::Document as u8);
            assert_eq!(meta.component_count, 1);
        }
        other => panic!("first change must be Created, got {other:?}"),
    }
    match &changes[1] {
        ObjectChange::Versioned {
            object,
            version,
            previous,
            ..
        } => {
            assert_eq!(*object, a);
            assert_eq!(*version, VersionId(2));
            assert_eq!(*previous, Some(VersionId(1)), "old_version 取自 WAL 记录");
        }
        other => panic!("second change must be Versioned, got {other:?}"),
    }
}

#[test]
fn delete_reports_the_tombstone_version() {
    let k = kernel("delete");
    let a = commit_create(&k, b"x");

    let mut txn = TransactionManager::begin(&k, IsolationLevel::Snapshot).unwrap();
    ObjectManager::delete_object(&k, &mut txn, a).unwrap();
    TransactionManager::commit(&k, &mut txn).unwrap();

    let (changes, _) = drain(
        &k,
        Lsn(0),
        ChangeFeed::object_watermark(&k),
        &ChangeBatchSpec::unbounded(),
    );
    match changes.last().expect("changes") {
        ObjectChange::Deleted { object, version } => {
            assert_eq!(*object, a);
            assert_eq!(*version, VersionId(1), "墓碑的目标是删除时可见的那个版本");
        }
        other => panic!("last change must be Deleted, got {other:?}"),
    }
    // 删除后对象不再出现在快照枚举中。
    let snap = k.fresh_snapshot();
    assert!(!ChangeFeed::scan_objects(&k, &snap).unwrap().contains(&a));
}

#[test]
fn aborted_txn_never_appears() {
    let k = kernel("abort");
    let keep = commit_create(&k, b"keep");

    let mut txn = TransactionManager::begin(&k, IsolationLevel::Snapshot).unwrap();
    let dropped = create(&k, &mut txn, b"dropped");
    TransactionManager::abort(&k, &mut txn).unwrap();

    let (changes, _) = drain(
        &k,
        Lsn(0),
        ChangeFeed::object_watermark(&k),
        &ChangeBatchSpec::unbounded(),
    );
    let objects: Vec<ObjectId> = changes.iter().map(|c| c.object()).collect();
    assert!(objects.contains(&keep));
    assert!(
        !objects.contains(&dropped),
        "aborted writes must not produce changes: {changes:?}"
    );
    let snap = k.fresh_snapshot();
    assert!(!ChangeFeed::scan_objects(&k, &snap)
        .unwrap()
        .contains(&dropped));
}

#[test]
fn cursor_stops_before_pending_txn_and_resumes_after_it_resolves() {
    let k = kernel("barrier");
    let before = commit_create(&k, b"before");

    // 未决事务：`TxnBegin` 已在 WAL 里，尚无 Commit。
    let mut txn = TransactionManager::begin(&k, IsolationLevel::Snapshot).unwrap();
    let pending_obj = create(&k, &mut txn, b"pending");
    let watermark_during = ChangeFeed::object_watermark(&k);

    let batch = k
        .scan_changes(Lsn(0), watermark_during, &ChangeBatchSpec::unbounded())
        .unwrap();
    assert!(batch.blocked_by_pending_txn, "must stop at the pending txn");
    assert_eq!(
        batch.changes.len(),
        1,
        "only the committed create is visible"
    );
    assert_eq!(batch.changes[0].object(), before);
    let cursor = batch.next_cursor.expect("屏障必须给出游标");
    assert!(cursor > Lsn(0) && cursor <= watermark_during);

    // 再拉一次：仍停在同一个屏障，游标不动、不返回任何东西。
    let again = k
        .scan_changes(cursor, watermark_during, &ChangeBatchSpec::unbounded())
        .unwrap();
    assert!(again.blocked_by_pending_txn);
    assert_eq!(
        again.changes,
        vec![],
        "nothing may be applied past the barrier"
    );
    assert_eq!(again.next_cursor, Some(cursor));

    // 事务提交后，下一轮追赶越过屏障，拿到它的对象。
    TransactionManager::commit(&k, &mut txn).unwrap();
    let (rest, _) = drain(
        &k,
        cursor,
        ChangeFeed::object_watermark(&k),
        &ChangeBatchSpec::unbounded(),
    );
    assert!(
        rest.iter().any(|c| c.object() == pending_obj),
        "pending txn visible now: {rest:?}"
    );
}

#[test]
fn paging_does_not_change_the_result() {
    let k = kernel("paging");
    for i in 0..6u8 {
        commit_create(&k, &[i]);
    }
    let to = ChangeFeed::object_watermark(&k);

    let (whole, n_whole) = drain(&k, Lsn(0), to, &ChangeBatchSpec::unbounded());
    assert_eq!(n_whole, 1);
    assert_eq!(whole.len(), 6);

    for max_records in [1u32, 2, 3, 5] {
        let (paged, n) = drain(&k, Lsn(0), to, &ChangeBatchSpec::new(max_records, u64::MAX));
        assert_eq!(paged, whole, "max_records={max_records} 不得改变结果");
        assert!(n >= 6 / max_records as usize, "确实分了页");
    }

    // `max_bytes` 是软上限：单条记录超限也整条返回，不会丢变更。
    let (tiny, _) = drain(&k, Lsn(0), to, &ChangeBatchSpec::new(100, 1));
    assert_eq!(tiny, whole);
}

#[test]
fn empty_batch_does_not_advance_the_cursor() {
    let k = kernel("empty");
    commit_create(&k, b"x");
    let to = ChangeFeed::object_watermark(&k);

    // 容量为 0：一条也放不下，游标必须原地不动，且不能声称已到顶。
    let b = k
        .scan_changes(Lsn(0), to, &ChangeBatchSpec::new(0, u64::MAX))
        .unwrap();
    assert!(b.changes.is_empty());
    assert!(!b.blocked_by_pending_txn);
    assert_eq!(b.next_cursor, Some(Lsn(0)), "empty batch may not advance");

    // 已到顶：`None`。
    let done = k
        .scan_changes(to, to, &ChangeBatchSpec::unbounded())
        .unwrap();
    assert!(done.changes.is_empty());
    assert_eq!(done.next_cursor, None);
}

#[test]
fn watermark_bounds_the_feed() {
    let k = kernel("watermark");
    let a = commit_create(&k, b"a");
    let wm = ChangeFeed::object_watermark(&k);
    assert_eq!(wm, k.wal().durable_lsn());

    // 水位之上的记录不保证持久，不得承诺给追赶者。
    let b = k
        .scan_changes(Lsn(0), Lsn(wm.0 + 1_000_000), &ChangeBatchSpec::unbounded())
        .unwrap();
    assert_eq!(b.changes.len(), 1);
    assert_eq!(b.changes[0].object(), a);
    assert_eq!(b.next_cursor, None);
}

#[test]
fn cursor_before_min_required_lsn_is_an_explicit_error() {
    let k = kernel("truncated");
    commit_create(&k, b"a");
    CheckpointManager::trigger(&k).unwrap();
    let min = k.wal().min_required_lsn();
    assert!(min > Lsn(0), "checkpoint advances the truncation boundary");

    // 06 §2.7：区间可能已被截断，必须显式失败，而不是扫空区间后推进水位。
    let err = k
        .scan_changes(
            Lsn(0),
            ChangeFeed::object_watermark(&k),
            &ChangeBatchSpec::unbounded(),
        )
        .unwrap_err();
    assert!(
        matches!(err, ains::KernelError::Unsupported { .. }),
        "{err:?}"
    );

    // 从合法水位开始则正常（该区间内只有检查点标记，没有对象变更）。
    let ok = k
        .scan_changes(
            min,
            ChangeFeed::object_watermark(&k),
            &ChangeBatchSpec::unbounded(),
        )
        .unwrap();
    assert!(ok.changes.is_empty());
    assert_eq!(ok.next_cursor, None);
}

#[test]
fn scan_objects_lists_only_snapshot_visible_objects() {
    let k = kernel("scan-objects");
    let keep = commit_create(&k, b"keep");
    let gone = commit_create(&k, b"gone");

    let before_delete = k.fresh_snapshot();
    let mut txn = TransactionManager::begin(&k, IsolationLevel::Snapshot).unwrap();
    ObjectManager::delete_object(&k, &mut txn, gone).unwrap();
    TransactionManager::commit(&k, &mut txn).unwrap();

    let now = ChangeFeed::scan_objects(&k, &k.fresh_snapshot()).unwrap();
    assert!(now.contains(&keep));
    assert!(!now.contains(&gone));
    assert_eq!(now.len(), 1);

    // 快照隔离：删除提交之前的快照仍然看得见该对象（回退扫描要用到这一点）。
    let then = ChangeFeed::scan_objects(&k, &before_delete).unwrap();
    assert!(then.contains(&gone));
}

#[test]
fn changes_carry_the_ids_from_the_wal_not_a_guess() {
    let k = kernel("ids");
    let a = commit_create(&k, b"a");
    let b = commit_create(&k, b"b");
    assert_ne!(a, b);

    let (changes, _) = drain(
        &k,
        Lsn(0),
        ChangeFeed::object_watermark(&k),
        &ChangeBatchSpec::unbounded(),
    );
    let created: Vec<ObjectId> = changes
        .iter()
        .filter(|c| matches!(c, ObjectChange::Created { .. }))
        .map(|c| c.object())
        .collect();
    assert_eq!(created, vec![a, b]);
    assert!(changes.iter().all(|c| c.version() == VersionId(1)));
}
