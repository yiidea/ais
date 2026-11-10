//! WAL 重启后的持久水位口径。
//!
//! `durable_lsn` 在重启后必须反映「盘上已有的全部内容」：本进程没 flush 过时，
//! 打开时恢复到的日志尾就是持久的；否则只返回打开时的初始值，派生记录（协调区
//! 快照）在重启后永远读不回来。

use ains::hal::SyncIoExecutor;
use ains::types::*;
use ains::wal::{DerivedFamily, RecordType, WalBody, WalManager, WalRecord};
use std::sync::Arc;

const SEGMENT_SIZE: u64 = 64 * 1024;
const SEGMENTS: u64 = 32;

/// 在同一份内存介质上重新打开 WAL（新执行器、新状态）。
fn reopen(medium: Arc<ains::hal::mem::MemDevice>) -> ains::wal::DeviceWal {
    let io = Arc::new(SyncIoExecutor::new(Box::new(Shared(medium))));
    ains::wal::DeviceWal::open(
        io,
        ains::wal::DeviceWalConfig::new(DeviceId(1), 0, SEGMENT_SIZE, SEGMENTS),
    )
    .expect("open wal")
}

fn delta(index: u64) -> WalRecord {
    let body = WalBody::IndexDelta {
        index_id: IndexId(index),
        object_id: ObjectId::new(1, 1),
        version: VersionId(1),
        op: 0,
    };
    WalRecord::new(
        RecordType::IndexDelta,
        TransactionId(0),
        Lsn(0),
        body.encode(),
    )
}

#[test]
fn reopened_wal_reports_the_on_disk_tail_as_durable() {
    let medium = Arc::new(ains::hal::mem::MemDevice::new(DeviceId(1), 16 << 20));

    let (first, last) = {
        let wal = reopen(Arc::clone(&medium));
        let first = wal
            .append_derived(&delta(1), DerivedFamily::Index)
            .expect("append");
        let last = wal
            .append_derived(&delta(2), DerivedFamily::Index)
            .expect("append");
        wal.flush_upto(last).expect("flush");
        (first, last)
    };

    let wal = reopen(medium);
    assert!(
        wal.durable_lsn() > last,
        "盘上已有的记录在重启后就是持久的：{:?} 应大于 {:?}",
        wal.durable_lsn(),
        last
    );
    let scan = wal.scan_from(Lsn(0)).expect("scan");
    assert_eq!(scan.records.len(), 2);
    assert_eq!(scan.records[0].lsn, first);
    assert_eq!(scan.records[1].lsn, last);
}

#[test]
fn appends_after_a_restart_are_not_durable_until_flushed() {
    let medium = Arc::new(ains::hal::mem::MemDevice::new(DeviceId(1), 16 << 20));
    {
        let wal = reopen(Arc::clone(&medium));
        wal.append_derived(&delta(1), DerivedFamily::Index).unwrap();
        wal.flush_upto(Lsn(u64::MAX)).unwrap();
    }

    let wal = reopen(Arc::clone(&medium));
    // 重启后新追加的帧落在已持久位置之后；它只能被「下一次 flush」带成持久。
    let durable_at_open = wal.durable_lsn();
    let added = wal.append_derived(&delta(2), DerivedFamily::Index).unwrap();
    assert!(added >= durable_at_open);
    wal.flush_upto(added).unwrap();
    assert!(wal.durable_lsn() > added);
}

/// `Arc` 共享介质，每次重开给 WAL 一个句柄。
struct Shared(Arc<ains::hal::mem::MemDevice>);

impl ains::hal::StorageDevice for Shared {
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
