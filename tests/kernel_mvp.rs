//! MVP 内核的端到端测试（不依赖夹具 feature）。

use ains::hal::mem::MemDevice;
use ains::kernel::{
    CheckpointManager, CreateObjectRequest, Kernel, KernelConfig, ObjectManager, Reclaimer,
    RecoveryManager, ScanBudget, TransactionManager, VerifyScope,
};
use ains::types::*;

fn config(tag: &str) -> KernelConfig {
    let dir = std::env::temp_dir().join(format!("ains-test-{tag}"));
    let _ = std::fs::remove_dir_all(&dir);
    KernelConfig::new(dir.join("checkpoint.snap"))
}

fn open(tag: &str) -> Kernel {
    Kernel::open(
        config(tag),
        Box::new(MemDevice::new(DeviceId(1), 8 * 1024 * 1024)),
    )
    .expect("open")
}

#[test]
fn create_commit_visible_after_commit_only() {
    let kernel = open("vis");
    let mut txn = TransactionManager::begin(&kernel, IsolationLevel::Snapshot).unwrap();
    let req = CreateObjectRequest {
        object_type: ObjectType::Document,
        payload: b"hello".to_vec(),
        component_count: 1,
    };
    let id = ObjectManager::create_object(&kernel, &mut txn, &req).unwrap();

    // O1：未提交的版本对其他事务不可见。
    let other = kernel.fresh_snapshot();
    assert!(ObjectManager::resolve_version(&kernel, id, &other).is_err());

    TransactionManager::commit(&kernel, &mut txn).unwrap();
    let after = kernel.fresh_snapshot();
    assert_eq!(
        ObjectManager::resolve_version(&kernel, id, &after).unwrap(),
        VersionId(1)
    );
}

#[test]
fn abort_leaves_object_invisible() {
    let kernel = open("abort");
    let mut txn = TransactionManager::begin(&kernel, IsolationLevel::Snapshot).unwrap();
    let req = CreateObjectRequest {
        object_type: ObjectType::Document,
        payload: b"gone".to_vec(),
        component_count: 1,
    };
    let id = ObjectManager::create_object(&kernel, &mut txn, &req).unwrap();
    TransactionManager::abort(&kernel, &mut txn).unwrap();
    let snap = kernel.fresh_snapshot();
    assert!(ObjectManager::resolve_version(&kernel, id, &snap).is_err());
    assert_eq!(
        RecoveryManager::verify(&kernel, VerifyScope::Full)
            .unwrap()
            .issues
            .len(),
        0
    );
}

#[test]
fn update_creates_new_version_and_hides_old() {
    let kernel = open("update");
    let mut txn = TransactionManager::begin(&kernel, IsolationLevel::Snapshot).unwrap();
    let req = CreateObjectRequest {
        object_type: ObjectType::Document,
        payload: b"v1".to_vec(),
        component_count: 1,
    };
    let id = ObjectManager::create_object(&kernel, &mut txn, &req).unwrap();
    TransactionManager::commit(&kernel, &mut txn).unwrap();

    let mut txn2 = TransactionManager::begin(&kernel, IsolationLevel::Snapshot).unwrap();
    let spec = ains::kernel::VersionSpec {
        payload: b"v2-longer-payload".to_vec(),
    };
    let ver = ObjectManager::stage_version(&kernel, &mut txn2, id, &spec).unwrap();
    assert_eq!(ver, VersionId(2));
    TransactionManager::commit(&kernel, &mut txn2).unwrap();

    let snap = kernel.fresh_snapshot();
    assert_eq!(
        ObjectManager::resolve_version(&kernel, id, &snap).unwrap(),
        VersionId(2)
    );
    let view = ObjectManager::read_version(&kernel, id, VersionId(1)).unwrap();
    assert_eq!(view.version.state, VersionState::Obsolete);
}

#[test]
fn payload_roundtrip_bytes_match() {
    let kernel = open("payload");
    let payload: Vec<u8> = (0..1000u32).map(|i| (i % 251) as u8).collect();
    let mut txn = TransactionManager::begin(&kernel, IsolationLevel::Snapshot).unwrap();
    let req = CreateObjectRequest {
        object_type: ObjectType::Vector,
        payload: payload.clone(),
        component_count: 1,
    };
    let id = ObjectManager::create_object(&kernel, &mut txn, &req).unwrap();
    TransactionManager::commit(&kernel, &mut txn).unwrap();

    let snap = kernel.fresh_snapshot();
    let ver = ObjectManager::resolve_version(&kernel, id, &snap).unwrap();
    let view = ObjectManager::read_version(&kernel, id, ver).unwrap();
    let bytes = kernel
        .read_extent_bytes(view.version.extent.unwrap())
        .unwrap();
    assert_eq!(&bytes[..payload.len()], &payload[..]);
}

#[test]
fn delete_is_mvcc_delete_not_physical() {
    let kernel = open("delete");
    let mut txn = TransactionManager::begin(&kernel, IsolationLevel::Snapshot).unwrap();
    let req = CreateObjectRequest {
        object_type: ObjectType::Document,
        payload: b"x".to_vec(),
        component_count: 1,
    };
    let id = ObjectManager::create_object(&kernel, &mut txn, &req).unwrap();
    TransactionManager::commit(&kernel, &mut txn).unwrap();

    let mut del = TransactionManager::begin(&kernel, IsolationLevel::Snapshot).unwrap();
    ObjectManager::delete_object(&kernel, &mut del, id).unwrap();
    TransactionManager::commit(&kernel, &mut del).unwrap();

    assert!(ObjectManager::resolve_version(&kernel, id, &kernel.fresh_snapshot()).is_err());
    // 版本仍然存在（不是物理删除）。
    assert!(ObjectManager::read_version(&kernel, id, VersionId(1)).is_ok());
}

#[test]
fn wal_records_are_durable_and_scannable() {
    let kernel = open("wal");
    let id = commit_object_here(&kernel);
    let durable = kernel.wal().durable_lsn();
    assert!(durable.0 > 0);
    let scan = kernel.wal().scan_from(Lsn(0)).unwrap();
    assert!(!scan.records.is_empty());
    assert!(!scan.stopped_at_corruption);
    assert!(scan.last_valid_lsn <= durable);
    let _ = id;
}

fn commit_object_here(kernel: &Kernel) -> ObjectId {
    let mut txn = TransactionManager::begin(kernel, IsolationLevel::Snapshot).unwrap();
    let req = CreateObjectRequest {
        object_type: ObjectType::Document,
        payload: b"wal-payload".to_vec(),
        component_count: 1,
    };
    let id = ObjectManager::create_object(kernel, &mut txn, &req).unwrap();
    TransactionManager::commit(kernel, &mut txn).unwrap();
    id
}

#[test]
fn snapshot_isolation_hides_concurrent_commit() {
    let kernel = open("snapiso");
    // t1 开始并建立快照。
    let mut t1 = TransactionManager::begin(&kernel, IsolationLevel::Snapshot).unwrap();
    let snap_t1 = t1.snapshot.clone();

    // t2 创建并提交对象。
    let id = commit_object_here(&kernel);

    // t1 的快照看不到 t2 提交的对象。
    assert!(ObjectManager::resolve_version(&kernel, id, &snap_t1).is_err());
    assert!(ObjectManager::resolve_version(&kernel, id, &kernel.fresh_snapshot()).is_ok());

    TransactionManager::abort(&kernel, &mut t1).unwrap();
}

#[test]
fn checkpoint_and_recovery_preserve_committed_state() {
    let dir = std::env::temp_dir().join("ains-test-recover");
    let _ = std::fs::remove_dir_all(&dir);
    let mut cfg = KernelConfig::new(dir.join("ckpt.snap"));
    cfg.database_id = 1;

    // 第一个实例：写对象并发布检查点。
    let id;
    let commit_lsn;
    {
        let kernel =
            Kernel::open(cfg.clone(), Box::new(MemDevice::new(DeviceId(1), 8 << 20))).unwrap();
        let mut txn = TransactionManager::begin(&kernel, IsolationLevel::Snapshot).unwrap();
        let req = CreateObjectRequest {
            object_type: ObjectType::Document,
            payload: b"before-checkpoint".to_vec(),
            component_count: 1,
        };
        id = ObjectManager::create_object(&kernel, &mut txn, &req).unwrap();
        let receipt = TransactionManager::commit(&kernel, &mut txn).unwrap();
        commit_lsn = receipt.commit_lsn;
        CheckpointManager::trigger(&kernel).unwrap();
        assert_eq!(kernel.current().unwrap().lsn, kernel.wal().durable_lsn());
    }

    // 第二个实例：检查点文件应恢复出同一对象。
    {
        let kernel =
            Kernel::open(cfg.clone(), Box::new(MemDevice::new(DeviceId(1), 8 << 20))).unwrap();
        let view = ObjectManager::read_version(&kernel, id, VersionId(1)).unwrap();
        assert_eq!(view.version.state, VersionState::Published);
        assert!(commit_lsn.0 > 0);
        assert_eq!(
            RecoveryManager::verify(&kernel, VerifyScope::Full)
                .unwrap()
                .issues
                .len(),
            0
        );
    }
}

#[test]
fn corrupted_checkpoint_is_not_silently_ignored() {
    let dir = std::env::temp_dir().join("ains-test-corrupt-ckpt");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("ckpt.snap");
    std::fs::write(&path, b"definitely not a checkpoint").unwrap();

    let cfg = KernelConfig::new(&path);
    let err = match Kernel::open(cfg, Box::new(MemDevice::new(DeviceId(1), 1 << 20))) {
        Ok(_) => panic!("corrupted checkpoint must not open silently"),
        Err(e) => e,
    };
    assert!(
        matches!(err, ains::KernelError::Corrupted(_)),
        "got {err:?}"
    );
}

#[test]
fn gc_refuses_to_reclaim_live_extent() {
    let kernel = open("gc-live");
    let id = commit_object_here(&kernel);
    let snap = kernel.fresh_snapshot();
    let ver = ObjectManager::resolve_version(&kernel, id, &snap).unwrap();
    let ext = ObjectManager::read_version(&kernel, id, ver)
        .unwrap()
        .version
        .extent
        .unwrap();

    let cand = ains::kernel::ReclaimCandidate { extent: ext };
    let decision = Reclaimer::judge(&kernel, &cand).unwrap();
    assert!(!decision.reclaimable);
    assert!(Reclaimer::reclaim(&kernel, &cand).is_err());
}

#[test]
fn gc_reclaims_after_checkpoint_and_obsolete_versions() {
    let kernel = open("gc-obsolete");
    let mut txn = TransactionManager::begin(&kernel, IsolationLevel::Snapshot).unwrap();
    let req = CreateObjectRequest {
        object_type: ObjectType::Document,
        payload: b"v1".to_vec(),
        component_count: 1,
    };
    let id = ObjectManager::create_object(&kernel, &mut txn, &req).unwrap();
    TransactionManager::commit(&kernel, &mut txn).unwrap();
    let ext_v1 = ObjectManager::read_version(&kernel, id, VersionId(1))
        .unwrap()
        .version
        .extent
        .unwrap();

    let mut txn2 = TransactionManager::begin(&kernel, IsolationLevel::Snapshot).unwrap();
    ObjectManager::stage_version(
        &kernel,
        &mut txn2,
        id,
        &ains::kernel::VersionSpec {
            payload: b"v2".to_vec(),
        },
    )
    .unwrap();
    TransactionManager::commit(&kernel, &mut txn2).unwrap();
    CheckpointManager::trigger(&kernel).unwrap();

    let candidates = Reclaimer::scan(&kernel, ScanBudget { max_candidates: 16 }).unwrap();
    let cand = candidates
        .into_iter()
        .find(|c| c.extent == ext_v1)
        .expect("obsolete extent should be a candidate");

    let decision = Reclaimer::judge(&kernel, &cand).unwrap();
    assert!(decision.reclaimable, "reasons: {:?}", decision.reasons);
    Reclaimer::reclaim(&kernel, &cand).unwrap();
    assert_eq!(
        RecoveryManager::verify(&kernel, VerifyScope::Full)
            .unwrap()
            .issues
            .len(),
        0
    );
}
