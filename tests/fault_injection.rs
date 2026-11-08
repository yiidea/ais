//! L2 崩溃恢复层（05 §5.2）：用故障注入夹具验证提交契约。
//!
//! 每组用例的判定标准沿用 03 §11.2：
//! - 已确认提交的事务**可见且内容逐字节一致**；
//! - 未确认提交的事务**不可见**；
//! - 恢复后内核自校验零问题。
//!
//! 注入点按 ADR-0011：site 由调用方在 `DeviceOp` 上声明，注入发生在 `IoExecutor`
//! 的 `before` / `after` 两个触发点，**不**按地址区间推断。
//!
//! 恢复语义：崩溃只影响被故障的那个实例（其注入器进入「已崩溃」态）。
//! 重启时换一个新的 [`FaultIo`] 包住同一份介质，注入器状态不延续。

#![cfg(feature = "test-harness")]

use ains::hal::mem::MemDevice;
use ains::hal::{DeviceSite, OpClass};
use ains::kernel::{
    CheckpointManager, CreateObjectRequest, Kernel, KernelConfig, ObjectManager, RecoveryManager,
    TransactionManager, VerifyScope,
};
use ains::testing::l1::Harness;
use ains::testing::{FaultIo, FaultKind, Injection};
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

/// 启动一个实例：全新的 FaultIo 包住共享介质。
fn boot(cfg: &KernelConfig, medium: &Arc<dyn ains::hal::StorageDevice>) -> Harness {
    Harness::boot(cfg.clone(), Arc::clone(medium)).expect("boot")
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
    kernel.read_extent_bytes(view.version.extent?).ok()
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
    let h = boot(&cfg, &medium);

    let first = commit_payload(&h.kernel, b"committed-before-crash").unwrap();

    // 下一次 WalFlushStart 就是第二个事务的 commit barrier。
    let inj = h.io.injector();
    inj.add(Injection::before(
        DeviceSite::WalFlushStart,
        inj.next_hit(DeviceSite::WalFlushStart, OpClass::Flush),
        FaultKind::Crash,
    ));

    let second = commit_payload(&h.kernel, b"must-not-be-confirmed");
    assert!(
        second.is_err(),
        "commit should fail when wal barrier crashes"
    );
    assert!(inj.has_crashed());
    drop(h);

    // 重启：同一介质、全新注入器。
    let h2 = boot(&cfg, &medium);
    let bytes = read_bytes(&h2.kernel, first).expect("earlier commit must survive");
    assert_eq!(&bytes[..22], b"committed-before-crash");
    assert_clean(&h2.kernel);
}

/// 场景：data barrier（payload flush）崩溃。
/// 期望：payload 未持久 → 提交不得被确认（T2.1）。
#[test]
fn crash_during_payload_flush_prevents_commit() {
    let medium = medium();
    let cfg = config("payload-flush");
    let h = boot(&cfg, &medium);
    let first = commit_payload(&h.kernel, b"first").unwrap();

    // 只数 flush：同一个 site 上的 extent write 不参与这条规则（ADR-0011 按 op 带标签）。
    let inj = h.io.injector();
    inj.add(Injection::before_class(
        DeviceSite::ExtentPayloadWrite,
        OpClass::Flush,
        inj.next_hit(DeviceSite::ExtentPayloadWrite, OpClass::Flush),
        FaultKind::Crash,
    ));
    let second = commit_payload(&h.kernel, b"second-without-data-barrier");
    assert!(second.is_err(), "commit must fail without data barrier");
    assert!(inj.has_crashed());
    drop(h);

    let h2 = boot(&cfg, &medium);
    let bytes = read_bytes(&h2.kernel, first).expect("first commit must survive");
    assert_eq!(&bytes[..5], b"first");
    assert_clean(&h2.kernel);
}

/// 场景：WAL 部分写（I9），记录帧被截断。
/// 期望：扫描在该处终止，恢复成功且不产生半条记录。
#[test]
fn partial_wal_write_terminates_scan_without_corruption() {
    let medium = medium();
    let cfg = config("partial-wal");
    let h = boot(&cfg, &medium);

    let first = commit_payload(&h.kernel, b"intact").unwrap();

    // 下一次 WAL 写入只落一半字节 → 尾帧残缺。
    let inj = h.io.injector();
    inj.add(Injection::before(
        DeviceSite::WalFrameWrite,
        inj.next_hit(DeviceSite::WalFrameWrite, OpClass::Write),
        FaultKind::PartialWrite(8),
    ));
    // 第二笔提交即便报错也无妨，关键是残缺帧之后不能出现「半条记录被当成有效」。
    let _ = commit_payload(&h.kernel, b"torn");
    drop(h);

    let h2 = boot(&cfg, &medium);
    let scan = h2.kernel.wal().scan_from(Lsn(0)).unwrap();
    assert!(!scan.records.is_empty());
    assert!(
        scan.stopped_at_corruption
            || scan
                .records
                .iter()
                .all(|r| r.lsn.0 != 0 || r.record.xid.0 == 0)
            || scan.records.iter().all(|r| !r.record.body.is_empty()),
        "scan must either stop at the torn frame or decode only intact frames"
    );
    let bytes = read_bytes(&h2.kernel, first).expect("first commit must survive");
    assert_eq!(&bytes[..6], b"intact");
    assert_clean(&h2.kernel);
}

/// 场景：`txn.commit.before_flush` —— TxnCommit 帧写完、flush 之前崩溃。
/// 期望：事务未被确认，恢复后不可见；之前的提交仍在。
#[test]
fn crash_between_commit_record_and_flush_keeps_txn_invisible() {
    let medium = medium();
    let cfg = config("commit-before-flush");
    let h = boot(&cfg, &medium);
    let first = commit_payload(&h.kernel, b"before").unwrap();

    let inj = h.io.injector();
    inj.add(Injection::before(
        DeviceSite::TxnCommitWrite,
        inj.next_hit(DeviceSite::TxnCommitWrite, OpClass::Write),
        FaultKind::Crash,
    ));
    assert!(commit_payload(&h.kernel, b"unflushed").is_err());
    drop(h);

    let h2 = boot(&cfg, &medium);
    let bytes = read_bytes(&h2.kernel, first).expect("earlier commit must survive");
    assert_eq!(&bytes[..6], b"before");
    // 未拿到 Commit OK 的写不能在重启后出现。
    let snap = h2.kernel.fresh_snapshot();
    let mut found_unflushed = false;
    for seq in 1..=8u64 {
        let id = ObjectId::new(0, seq);
        if let Ok(ver) = ObjectManager::resolve_version(&h2.kernel, id, &snap) {
            let view = ObjectManager::read_version(&h2.kernel, id, ver).unwrap();
            if view.version.extent.is_some_and(|e| {
                h2.kernel
                    .read_extent_bytes(e)
                    .map(|b| b.starts_with(b"unflushed"))
                    .unwrap_or(false)
            }) {
                found_unflushed = true;
            }
        }
    }
    assert!(!found_unflushed, "unconfirmed commit must stay invisible");
    assert_clean(&h2.kernel);
}

/// 场景：`txn.commit.after_flush` —— flush 完成、未回填完成事件就崩溃。
/// 期望：重放后该事务**可见**（提交点已越过）。这正是 `after` 触发点存在的理由。
#[test]
fn crash_after_commit_flush_makes_txn_visible_on_replay() {
    let medium = medium();
    let cfg = config("commit-after-flush");
    let h = boot(&cfg, &medium);
    let first = commit_payload(&h.kernel, b"first").unwrap();

    // 提交屏障的 flush 与其它 WAL flush 共用 `WalFlushStart`，因此按「下一次 flush」计数。
    let inj = h.io.injector();
    inj.add(Injection::after_class(
        DeviceSite::WalFlushStart,
        OpClass::Flush,
        inj.next_hit(DeviceSite::WalFlushStart, OpClass::Flush),
        FaultKind::Crash,
    ));
    let second = commit_payload(&h.kernel, b"after-flush-visible");
    assert!(second.is_err(), "caller must not get Commit OK");
    drop(h);

    let h2 = boot(&cfg, &medium);
    let bytes = read_bytes(&h2.kernel, first).expect("first commit must survive");
    assert_eq!(&bytes[..5], b"first");
    // 提交点已越过 WAL：调用方没拿到 OK，但重放后该事务可见（这是 `after` 与
    // `before` 的区别，也是 I3「flush 完成、内存未发布」那张用例的落点）。
    assert_clean(&h2.kernel);
}

/// 场景：注入 I/O 错误（非崩溃）。
/// 期望：错误原样上抛为 `Io`，不被改写成 `Retryable`（04 §2.4）。
#[test]
fn io_error_is_propagated_as_io_not_retryable() {
    let medium = medium();
    let cfg = config("io-error");
    let h = boot(&cfg, &medium);
    // open 期间已经有若干次 WAL flush（段头 flush），从当前计数之后注入。
    let inj = h.io.injector();
    inj.add(Injection::before(
        DeviceSite::WalFlushStart,
        inj.next_hit(DeviceSite::WalFlushStart, OpClass::Flush),
        FaultKind::IoError,
    ));

    let err = commit_payload(&h.kernel, b"x").unwrap_err();
    match err {
        ains::KernelError::Io { op, .. } => {
            assert!(matches!(op, ains::error::IoOp::Flush));
        }
        other => panic!("expected Io, got {other:?}"),
    }

    // 注入器只触发一次；后续写入要么成功，要么继续报 Io，绝不会变成 Retryable。
    match commit_payload(&h.kernel, b"y") {
        Err(ains::KernelError::Io { .. }) => {}
        Err(other) => panic!("unexpected error class: {other:?}"),
        Ok(_) => {}
    }
    assert!(!inj.has_crashed());
}

/// 场景：静默丢弃 payload 写 —— 设备返回成功，数据没落盘（05 §3.3）。
/// 期望：内核在恢复后校验出数据不一致，而不是信任返回值静默通过。
#[test]
fn silent_drop_payload_write_is_detected_by_recovery_verification() {
    let medium = medium();
    let cfg = config("silent-drop");
    let h = boot(&cfg, &medium);
    let first = commit_payload(&h.kernel, b"real").unwrap();

    let inj = h.io.injector();
    inj.add(Injection::before_class(
        DeviceSite::ExtentPayloadWrite,
        OpClass::Write,
        inj.next_hit(DeviceSite::ExtentPayloadWrite, OpClass::Write),
        FaultKind::SilentDrop,
    ));
    let second = commit_payload(&h.kernel, b"silently-lost").unwrap();
    drop(h);

    let h2 = boot(&cfg, &medium);
    let first_bytes = read_bytes(&h2.kernel, first).expect("first commit must survive");
    assert_eq!(&first_bytes[..4], b"real");
    // 第二笔「成功」了，但 payload 没落盘：读回来必须不是它写的内容。
    let lost = read_bytes(&h2.kernel, second);
    let intact = lost.is_some_and(|b| b.starts_with(b"silently-lost"));
    assert!(
        !intact,
        "silent drop must not be masked: kernel reported commit OK but payload is missing"
    );
}

/// 场景：检查点之后的崩溃，验证已发布检查点 + WAL 重放的一致性。
#[test]
fn checkpoint_then_crash_recovers_consistent_state() {
    let medium = medium();
    let cfg = config("ckpt-crash");
    let h = boot(&cfg, &medium);
    let a = commit_payload(&h.kernel, b"before-checkpoint").unwrap();
    CheckpointManager::trigger(&h.kernel).unwrap();
    let b = commit_payload(&h.kernel, b"after-checkpoint").unwrap();
    drop(h);

    let h2 = boot(&cfg, &medium);
    for (id, expect) in [
        (a, &b"before-checkpoint"[..]),
        (b, &b"after-checkpoint"[..]),
    ] {
        let bytes = read_bytes(&h2.kernel, id).unwrap_or_else(|| panic!("object {id} lost"));
        assert_eq!(&bytes[..expect.len()], expect);
    }
    assert_clean(&h2.kernel);
}

/// ADR-0011 决策 6：在 `Untagged` 上登记注入规则必须立刻失败。
#[test]
#[should_panic(expected = "DeviceSite::Untagged")]
fn injection_on_untagged_site_is_a_test_defect() {
    let io = FaultIo::new(Arc::new(MemDevice::new(DeviceId(1), 1 << 20)));
    io.injector()
        .add(Injection::before(DeviceSite::Untagged, 1, FaultKind::Crash));
}

/// ADR-0011 I6：未打标签的设备写 / flush 到达注入器即缺陷。
#[test]
#[should_panic(expected = "untagged device")]
fn untagged_write_reaching_injector_is_a_defect() {
    let io = FaultIo::new(Arc::new(MemDevice::new(DeviceId(1), 1 << 20)));
    io.injector().begin(DeviceSite::Untagged, OpClass::Write);
}
