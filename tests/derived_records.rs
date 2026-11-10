//! `WalManager::append_derived` 的行为测试（ADR-0013 决策 1）。
//!
//! 只断言写进契约里的性质：
//! 1. 记录族与记录类型必须匹配，`DerivedFamily::None` 永不接受；
//! 2. `xid` / `prev_lsn` 非零的派生记录被拒绝——它们不属于任何事务前驱链；
//! 3. 任何被拒绝的调用**不写入字节**（水位与后续 LSN 都不变）；
//! 4. 派生记录进入 WAL 后，`ChangeFeed` 不把它们当成对象变更。

use ains::hal::mem::MemDevice;
use ains::hal::StorageDevice;
use ains::kernel::{
    ChangeBatchSpec, ChangeFeed, CreateObjectRequest, Kernel, KernelConfig, ObjectManager,
    TransactionManager,
};
use ains::types::*;
use ains::wal::{DerivedFamily, RecordType, WalBody, WalRecord};

fn kernel(tag: &str) -> Kernel {
    let dir = std::env::temp_dir().join(format!("ains-derived-{tag}"));
    let _ = std::fs::remove_dir_all(&dir);
    Kernel::open(
        KernelConfig::new(dir.join("ckpt.snap")),
        Box::new(MemDevice::new(DeviceId(1), 16 << 20)) as Box<dyn StorageDevice>,
    )
    .expect("open kernel")
}

fn derived(record_type: RecordType, body: &WalBody) -> WalRecord {
    WalRecord::new(record_type, TransactionId(0), Lsn(0), body.encode())
}

const ALL_TYPES: [RecordType; 16] = [
    RecordType::TxnBegin,
    RecordType::TxnCommit,
    RecordType::TxnAbort,
    RecordType::ObjectCreate,
    RecordType::ObjectVersion,
    RecordType::ObjectDelete,
    RecordType::ExtentAlloc,
    RecordType::ExtentFree,
    RecordType::MappingCreate,
    RecordType::MappingSwitch,
    RecordType::IndexDelta,
    RecordType::IndexManifest,
    RecordType::MigrationBegin,
    RecordType::MigrationEnd,
    RecordType::CompactionBegin,
    RecordType::CompactionEnd,
];

#[test]
fn family_membership_is_exact() {
    for t in ALL_TYPES {
        let index = matches!(t, RecordType::IndexDelta | RecordType::IndexManifest);
        let maintenance = matches!(
            t,
            RecordType::MigrationBegin
                | RecordType::MigrationEnd
                | RecordType::CompactionBegin
                | RecordType::CompactionEnd
        );
        assert_eq!(DerivedFamily::Index.accepts(t), index, "{t:?}");
        assert_eq!(DerivedFamily::Maintenance.accepts(t), maintenance, "{t:?}");
        assert!(!DerivedFamily::None.accepts(t), "{t:?}");
    }
    // CheckpointBegin 有发布语义，不走非事务通道（ADR-0013 决策 1）。
    assert!(!DerivedFamily::Maintenance.accepts(RecordType::CheckpointBegin));
}

#[test]
fn accepted_records_are_appended_and_scannable() {
    let k = kernel("accept");
    let delta = WalBody::IndexDelta {
        index_id: IndexId(1),
        object_id: ObjectId::new(1, 7),
        version: VersionId(3),
        op: 1,
    };
    // `durable_lsn` 是「下一条记录的 LSN」，因此首条记录的 LSN 恰好等于它。
    let start = k.wal().durable_lsn();
    let lsn = k
        .wal()
        .append_derived(
            &derived(RecordType::IndexDelta, &delta),
            DerivedFamily::Index,
        )
        .expect("append index delta");
    assert_eq!(lsn, start);
    k.wal().flush_upto(lsn).expect("flush");

    let scan = k.wal().scan_from(start).expect("scan");
    let hit = scan
        .records
        .iter()
        .find(|r| r.lsn == lsn)
        .expect("record present");
    assert_eq!(hit.record.record_type, RecordType::IndexDelta);
    assert_eq!(hit.record.xid, TransactionId(0));
    assert_eq!(hit.record.prev_lsn, Lsn(0));
    assert_eq!(
        WalBody::decode(hit.record.record_type, &hit.record.body).unwrap(),
        delta
    );

    let manifest = WalBody::IndexManifest {
        index_id: IndexId(1),
        segment_id: [7u8; 16],
        from_lsn: 100,
        to_lsn: 200,
        watermark_xid: 12,
        watermark_lsn: 34,
    };
    k.wal()
        .append_derived(
            &derived(RecordType::IndexManifest, &manifest),
            DerivedFamily::Index,
        )
        .expect("append manifest");
    let maintenance = WalBody::Maintenance {
        kind: RecordType::CompactionBegin,
        extent: 1,
        old_mapping: 0,
        new_mapping: 0,
    };
    k.wal()
        .append_derived(
            &derived(RecordType::CompactionBegin, &maintenance),
            DerivedFamily::Maintenance,
        )
        .expect("append maintenance");
}

#[test]
fn mismatched_family_writes_nothing() {
    let k = kernel("mismatch");
    let delta = WalBody::IndexDelta {
        index_id: IndexId(1),
        object_id: ObjectId::new(1, 7),
        version: VersionId(1),
        op: 0,
    };
    let maintenance = WalBody::Maintenance {
        kind: RecordType::MigrationBegin,
        extent: 0,
        old_mapping: 0,
        new_mapping: 0,
    };
    let before_lsn = k.wal().durable_lsn();
    let before_scan = k.wal().scan_from(Lsn(0)).expect("scan").last_valid_lsn;

    let cases = [
        (&delta, DerivedFamily::Maintenance),
        (&maintenance, DerivedFamily::Index),
        (&delta, DerivedFamily::None),
    ];
    for (body, family) in cases {
        let err = k
            .wal()
            .append_derived(&derived(body.record_type(), body), family)
            .expect_err("family mismatch must fail");
        assert!(
            matches!(err, ains::KernelError::InvalidArgument { .. }),
            "expected InvalidArgument, got {err:?}"
        );
        assert!(!err.is_retryable());
    }

    // 对象记录走派生通道同样被拒。
    let obj = WalBody::ObjectCreate {
        object_id: ObjectId::new(1, 9),
        object_type: 1,
        component_count: 1,
    };
    assert!(k
        .wal()
        .append_derived(
            &derived(RecordType::ObjectCreate, &obj),
            DerivedFamily::Index
        )
        .is_err());

    assert_eq!(
        k.wal().durable_lsn(),
        before_lsn,
        "rejected call advanced durable_lsn"
    );
    assert_eq!(
        k.wal().scan_from(Lsn(0)).expect("scan").last_valid_lsn,
        before_scan,
        "rejected call wrote bytes"
    );

    // 之后合法追加仍从原来的边界开始，说明没有留下半截帧。
    let lsn = k
        .wal()
        .append_derived(
            &derived(RecordType::IndexDelta, &delta),
            DerivedFamily::Index,
        )
        .expect("append index delta");
    let scan = k.wal().scan_from(before_scan).expect("scan");
    assert_eq!(scan.records.first().map(|r| r.lsn), Some(lsn));
    assert!(!scan.stopped_at_corruption);
}

#[test]
fn transactional_fields_are_rejected() {
    let k = kernel("txn-fields");
    let delta = WalBody::IndexDelta {
        index_id: IndexId(1),
        object_id: ObjectId::new(1, 7),
        version: VersionId(1),
        op: 0,
    };
    let before = k.wal().scan_from(Lsn(0)).expect("scan").last_valid_lsn;

    let with_xid = WalRecord::new(
        RecordType::IndexDelta,
        TransactionId(5),
        Lsn(0),
        delta.encode(),
    );
    let err = k
        .wal()
        .append_derived(&with_xid, DerivedFamily::Index)
        .expect_err("xid must be zero");
    assert!(matches!(err, ains::KernelError::InvalidArgument { .. }));

    let with_prev = WalRecord::new(
        RecordType::IndexDelta,
        TransactionId(0),
        Lsn(4096),
        delta.encode(),
    );
    let err = k
        .wal()
        .append_derived(&with_prev, DerivedFamily::Index)
        .expect_err("prev_lsn must be zero");
    assert!(matches!(err, ains::KernelError::InvalidArgument { .. }));

    assert_eq!(
        k.wal().scan_from(Lsn(0)).expect("scan").last_valid_lsn,
        before
    );
}

#[test]
fn change_feed_ignores_derived_records() {
    let k = kernel("feed");
    let mut txn = TransactionManager::begin(&k, IsolationLevel::Snapshot).expect("begin");
    let id = ObjectManager::create_object(
        &k,
        &mut txn,
        &CreateObjectRequest {
            object_type: ObjectType::Document,
            payload: b"payload".to_vec(),
            component_count: 1,
        },
    )
    .expect("create");
    TransactionManager::commit(&k, &mut txn).expect("commit");

    let delta = WalBody::IndexDelta {
        index_id: IndexId(1),
        object_id: id,
        version: VersionId(1),
        op: 2,
    };
    let lsn = k
        .wal()
        .append_derived(
            &derived(RecordType::IndexDelta, &delta),
            DerivedFamily::Index,
        )
        .expect("append");
    k.wal().flush_upto(lsn).expect("flush");

    let from = Lsn(0);
    let to = k.object_watermark();
    let batch = k
        .scan_changes(from, to, &ChangeBatchSpec::unbounded())
        .expect("scan_changes");
    assert!(!batch.blocked_by_pending_txn);
    assert_eq!(
        batch.changes.len(),
        1,
        "only the create is an object change"
    );
    assert_eq!(batch.changes[0].object(), id);
}

#[test]
fn body_family_mapping_is_authoritative() {
    // 族是从记录体推出来的，不是调用方口头声明的：这样「把这个记录标成那个族」
    // 这种自相矛盾的调用在类型层面就写不出来。
    let delta = WalBody::IndexDelta {
        index_id: IndexId(1),
        object_id: ObjectId::new(1, 7),
        version: VersionId(1),
        op: 0,
    };
    assert_eq!(delta.family(), Some(DerivedFamily::Index));

    let maintenance = WalBody::Maintenance {
        kind: RecordType::CompactionEnd,
        extent: 0,
        old_mapping: 0,
        new_mapping: 0,
    };
    assert_eq!(maintenance.family(), Some(DerivedFamily::Maintenance));

    let obj = WalBody::ObjectCreate {
        object_id: ObjectId::new(1, 9),
        object_type: 1,
        component_count: 1,
    };
    assert_eq!(obj.family(), None, "object records are transactional");
}

#[test]
fn append_derived_helper_refuses_transactional_bodies() {
    let k = kernel("helper");
    let before = k.wal().scan_from(Lsn(0)).expect("scan").last_valid_lsn;

    let txn_body = WalBody::TxnCommit;
    let err = WalBody::append_derived(k.wal(), &txn_body).expect_err("must refuse");
    assert!(matches!(err, ains::KernelError::InvalidArgument { .. }));

    let delta = WalBody::IndexDelta {
        index_id: IndexId(1),
        object_id: ObjectId::new(1, 7),
        version: VersionId(2),
        op: 1,
    };
    let lsn = WalBody::append_derived(k.wal(), &delta).expect("derived append");
    k.wal().flush_upto(lsn).expect("flush");
    let scan = k.wal().scan_from(before).expect("scan");
    let hit = scan.records.iter().find(|r| r.lsn == lsn).expect("present");
    assert_eq!(hit.record.xid, TransactionId(0));
    assert_eq!(
        WalBody::decode(hit.record.record_type, &hit.record.body).unwrap(),
        delta
    );
}

/// `Arc` 共享介质：同一份字节上「重启」出新内核。
struct SharedMedium(std::sync::Arc<MemDevice>);

impl StorageDevice for SharedMedium {
    fn descriptor(&self) -> &ains::hal::DeviceDescriptor {
        self.0.descriptor()
    }
    fn write<'a>(
        &'a self,
        loc: &'a PhysicalLocation,
        buf: &'a [u8],
    ) -> ains::hal::BoxFuture<'a, ains::KernelResult<()>> {
        self.0.write(loc, buf)
    }
    fn read<'a>(
        &'a self,
        loc: &'a PhysicalLocation,
        buf: &'a mut [u8],
    ) -> ains::hal::BoxFuture<'a, ains::KernelResult<()>> {
        self.0.read(loc, buf)
    }
    fn flush<'a>(
        &'a self,
        loc: &'a PhysicalLocation,
        level: DurabilityLevel,
    ) -> ains::hal::BoxFuture<'a, ains::KernelResult<()>> {
        self.0.flush(loc, level)
    }
    fn discard<'a>(
        &'a self,
        loc: &'a PhysicalLocation,
    ) -> ains::hal::BoxFuture<'a, ains::KernelResult<()>> {
        self.0.discard(loc)
    }
    fn max_durability(&self) -> DurabilityLevel {
        self.0.max_durability()
    }
    fn health(&self) -> DeviceHealth {
        self.0.health()
    }
}

#[test]
fn derived_records_survive_a_restart() {
    // 重启后盘上已有的记录必须重新可读：`durable_lsn` 在「本进程没 flush 过」时
    // 取日志尾，而不是打开时的初始值 0——否则派生记录永远读不回来。
    let medium = std::sync::Arc::new(MemDevice::new(DeviceId(1), 16 << 20));
    let dir = std::env::temp_dir().join("ains-derived-restart");
    let _ = std::fs::remove_dir_all(&dir);
    let cfg = || KernelConfig::new(dir.join("ckpt.snap"));

    let lsn = {
        let k = Kernel::open(cfg(), Box::new(SharedMedium(medium.clone()))).expect("open");
        let delta = WalBody::IndexDelta {
            index_id: IndexId(5),
            object_id: ObjectId::new(1, 1),
            version: VersionId(1),
            op: 0,
        };
        let lsn = k
            .wal()
            .append_derived(
                &derived(RecordType::IndexDelta, &delta),
                DerivedFamily::Index,
            )
            .expect("append");
        k.wal().flush_upto(lsn).expect("flush");
        lsn
    };

    let k = Kernel::open(cfg(), Box::new(SharedMedium(medium))).expect("reopen");
    // 重启后的第一次扫描只到「已持久」水位为止：没修正的话它是打开时的初始值，
    // 结果是一条记录都读不到。
    let scan = k.wal().scan_from(Lsn(0)).expect("scan");
    assert!(
        scan.last_valid_lsn > lsn,
        "重启后的扫描要覆盖盘上已有的记录"
    );
    assert!(k.wal().durable_lsn() >= lsn, "盘上已有的记录是持久的");
    assert!(
        scan.records
            .iter()
            .any(|r| r.lsn == lsn && r.record.record_type == RecordType::IndexDelta),
        "重启后派生记录仍在"
    );
}
