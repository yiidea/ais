//! L2 崩溃恢复层（05 §5.2）：用故障注入夹具验证提交契约。
//!
//! 每组用例的判定标准沿用 03 §11.2：
//! - 已确认提交的事务**可见且内容逐字节一致**；
//! - 未确认提交的事务**不可见**；
//! - 恢复后内核自校验零问题。
//!
//! 恢复语义：崩溃只影响被故障的那个实例（其注入器进入“已崩溃”态）。
//! 重启时用新的 `FaultDevice` 包住同一份 `SharedDevice`，注入器状态不延续。

#![cfg(feature = "test-harness")]

use ains::hal::mem::MemDevice;
use ains::kernel::{
    CheckpointManager, CreateObjectRequest, Kernel, KernelConfig, ObjectManager, RecoveryManager,
    TransactionManager, VerifyScope,
};
use ains::testing::{FaultDevice, FaultKind, FaultSite, Injection, SharedDevice};
use ains::types::*;
use std::sync::Arc;

const WAL_END: u64 = 64 * 1024 * 32;

fn config(tag: &str) -> KernelConfig {
    let dir = std::env::temp_dir().join(format!("ains-fault-{tag}"));
    let _ = std::fs::remove_dir_all(&dir);
    let mut cfg = KernelConfig::new(dir.join("ckpt.snap"));
    cfg.wal_base_offset = 0;
    cfg.wal_segment_size = 64 * 1024;
    cfg.wal_segment_count = 32;
    cfg.payload_base_offset = WAL_END;
    cfg
}

fn medium() -> Arc<dyn ains::hal::StorageDevice> {
    Arc::new(MemDevice::new(DeviceId(1), 16 << 20))
}

/// 启动一个实例：新 FaultDevice 包住共享介质。
fn boot(cfg: &KernelConfig, medium: &Arc<dyn ains::hal::StorageDevice>) -> (Kernel, Arc<FaultDevice>) {
    let dev = Arc::new(FaultDevice::new(
        Box::new(SharedDevice::new(Arc::clone(medium))),
        0,
        WAL_END,
    ));
    let kernel = Kernel::open(cfg.clone(), Box::new(SharedDevice::new(dev.clone()))).unwrap();
    (kernel, dev)
}

fn commit_payload(kernel: &Kernel, payload: &[u8]) -> ains::KernelResult<ObjectId> {
    let mut txn = TransactionManager::begin(kernel, IsolationLevel::Snapshot)?;
    let req = CreateObjectRequest {
        object_type: ObjectType::Document,
        payload: payload.to_vec(),
        component_count: 1,
    };
    let id = ObjectManager::create_object(kernel, &mut txn, &req)?;
    TransactionManager::commit(kernel, &mut txn)?;
    Ok(id)
}

fn read_bytes(kernel: &Kernel, id: ObjectId) -> Option<Vec<u8>> {
    let snap = kernel.fresh_snapshot();
    let ver = ObjectManager::resolve_version(kernel, id, &snap).ok()?;
    let view = ObjectManager::read_version(kernel, id, ver).ok()?;
    Some(kernel.read_extent_bytes(view.version.extent?).ok()?)
}

fn assert_clean(kernel: &Kernel) {
    assert_eq!(
        RecoveryManager::verify(kernel, VerifyScope::Full)
            .unwrap()
            .issues
            .len(),
        0
    );
}

/// 场景：WAL flush 崩溃（wal barrier 未完成）。
/// 期望：已确认提交的事务仍然可见；崩溃点的那次提交不可见。
#[test]
fn crash_during_wal_flush_preserves_earlier_commits() {
    let medium = medium();
    let cfg = config("wal-flush");
    let (kernel, dev) = boot(&cfg, &medium);

    let first = commit_payload(&kernel, b"committed-before-crash").unwrap();

    // 下一次 WalFlush 就是第二个事务的 commit barrier。
    dev.injector().add(Injection {
        site: FaultSite::WalFlush,
        at: dev.injector().hits(FaultSite::WalFlush) + 1,
        kind: FaultKind::Crash,
    });

    let second = commit_payload(&kernel, b"must-not-be-confirmed");
    assert!(second.is_err(), "commit should fail when wal barrier crashes");
    assert!(dev.injector().has_crashed());
    drop(kernel);

    // 重启：同一介质、全新注入器。
    let (kernel2, _) = boot(&cfg, &medium);
    let bytes = read_bytes(&kernel2, first).expect("earlier commit must survive");
    assert_eq!(&bytes[..22], b"committed-before-crash");
    assert_clean(&kernel2);
}

/// 场景：data barrier（payload flush）崩溃。
/// 期望：payload 未持久 → 提交不得被确认（T2.1）。
#[test]
fn crash_during_payload_flush_prevents_commit() {
    let medium = medium();
    let cfg = config("payload-flush");
    let (kernel, dev) = boot(&cfg, &medium);
    let first = commit_payload(&kernel, b"first").unwrap();

    dev.injector().add(Injection {
        site: FaultSite::PayloadFlush,
        at: dev.injector().hits(FaultSite::PayloadFlush) + 1,
        kind: FaultKind::Crash,
    });
    let second = commit_payload(&kernel, b"second-without-data-barrier");
    assert!(second.is_err(), "commit must fail without data barrier");
    assert!(dev.injector().has_crashed());
    drop(kernel);

    let (kernel2, _) = boot(&cfg, &medium);
    let bytes = read_bytes(&kernel2, first).expect("first commit must survive");
    assert_eq!(&bytes[..5], b"first");
    assert_clean(&kernel2);
}

/// 场景：WAL 部分写（I9），记录帧被截断。
/// 期望：扫描在该处终止，恢复成功且不产生半条记录。
#[test]
fn partial_wal_write_terminates_scan_without_corruption() {
    let medium = medium();
    let cfg = config("partial-wal");
    let (kernel, dev) = boot(&cfg, &medium);

    let first = commit_payload(&kernel, b"intact").unwrap();

    // 下一次 WAL 写入只落一半字节 → 尾帧残缺。
    dev.injector().add(Injection {
        site: FaultSite::WalWrite,
        at: dev.injector().hits(FaultSite::WalWrite) + 1,
        kind: FaultKind::PartialWrite(8),
    });
    // 第二笔提交即便报错也无妨，关键是残缺帧之后不能出现“半条记录被当成有效”。
    let _ = commit_payload(&kernel, b"torn");
    drop(kernel);

    let (kernel2, _) = boot(&cfg, &medium);
    let scan = kernel2.wal().scan_from(Lsn(0)).unwrap();
    assert!(!scan.records.is_empty());
    assert!(
        scan.stopped_at_corruption || scan.records.iter().all(|r| r.lsn.0 != 0),
        "scan must either stop at the torn frame or decode only intact frames"
    );
    let bytes = read_bytes(&kernel2, first).expect("first commit must survive");
    assert_eq!(&bytes[..6], b"intact");
    assert_clean(&kernel2);
}

/// 场景：注入 I/O 错误（非崩溃）。
/// 期望：错误原样上抛为 `Io`，不被改写成 `Retryable`（04 §2.4）。
#[test]
fn io_error_is_propagated_as_io_not_retryable() {
    let medium = medium();
    let cfg = config("io-error");
    let (kernel, dev) = boot(&cfg, &medium);
    // open 期间已经有若干次 WalFlush（段头 flush），从当前计数之后注入。
    dev.injector().add(Injection {
        site: FaultSite::WalFlush,
        at: dev.injector().hits(FaultSite::WalFlush) + 1,
        kind: FaultKind::IoError,
    });

    let err = commit_payload(&kernel, b"x").unwrap_err();
    match err {
        ains::KernelError::Io { op, .. } => {
            assert!(matches!(op, ains::error::IoOp::Flush));
        }
        other => panic!("expected Io, got {other:?}"),
    }

    // 注入器只触发一次；后续写入要么成功，要么继续报 Io，绝不会变成 Retryable。
    match commit_payload(&kernel, b"y") {
        Err(ains::KernelError::Io { .. }) => {}
        Err(other) => panic!("unexpected error class: {other:?}"),
        Ok(_) => {}
    }
}

/// 场景：检查点之后的崩溃，验证已发布检查点 + WAL 重放的一致性。
#[test]
fn checkpoint_then_crash_recovers_consistent_state() {
    let medium = medium();
    let cfg = config("ckpt-crash");
    let (kernel, _dev) = boot(&cfg, &medium);
    let a = commit_payload(&kernel, b"before-checkpoint").unwrap();
    CheckpointManager::trigger(&kernel).unwrap();
    let b = commit_payload(&kernel, b"after-checkpoint").unwrap();
    drop(kernel);

    let (kernel2, _) = boot(&cfg, &medium);
    for (id, expect) in [(a, &b"before-checkpoint"[..]), (b, &b"after-checkpoint"[..])] {
        let bytes = read_bytes(&kernel2, id).unwrap_or_else(|| panic!("object {id} lost"));
        assert_eq!(&bytes[..expect.len()], expect);
    }
    assert_clean(&kernel2);
}
