//! 协调区的持久化（ADR-0013）：每次发布都经 `append_derived` 把清单写进 WAL，
//! 重启时取最新一条还原——先段后水位在重启之后依然成立。

use ains::hal::mem::MemDevice;
use ains::hal::StorageDevice;
use ains::index::{
    catch_up, catch_up_observed, manifest_log, CatchUpLimits, ChangeRate, MetaIndex, SealedSegment,
    SegmentManifest,
};
use ains::kernel::{
    ChangeBatchSpec, ChangeFeed, CreateObjectRequest, Kernel, KernelConfig, ObjectManager,
    TransactionManager, TxnCtx,
};
use ains::types::*;

fn kernel(tag: &str) -> Kernel {
    let dir = std::env::temp_dir().join(format!("ains-manifest-{tag}"));
    let _ = std::fs::remove_dir_all(&dir);
    Kernel::open(
        KernelConfig::new(dir.join("ckpt.snap")),
        Box::new(MemDevice::new(DeviceId(1), 16 << 20)) as Box<dyn StorageDevice>,
    )
    .expect("open kernel")
}

fn create(k: &Kernel) -> ObjectId {
    let mut txn: TxnCtx = TransactionManager::begin(k, IsolationLevel::Snapshot).unwrap();
    let id = ObjectManager::create_object(
        k,
        &mut txn,
        &CreateObjectRequest {
            object_type: ObjectType::Document,
            payload: b"payload".to_vec(),
            component_count: 1,
        },
    )
    .unwrap();
    TransactionManager::commit(k, &mut txn).unwrap();
    id
}

fn catch_up_all(k: &Kernel, m: &mut SegmentManifest) -> ChangeRate {
    let mut rate = ChangeRate::default();
    catch_up(
        &MetaIndex::new(IndexId(3)),
        k,
        k.wal(),
        m,
        &ChangeBatchSpec::unbounded(),
        CatchUpLimits::new(ChangeFeed::object_watermark(k), 8),
        &mut rate,
    )
    .expect("catch up");
    rate
}

#[test]
fn restore_without_records_starts_empty() {
    let k = kernel("empty");
    create(&k);
    assert_eq!(manifest_log::restore(k.wal(), IndexId(3)).unwrap(), None);
}

/// 重启后只看「已持久」的水位：`persist` 之后显式 flush 一次（真实部署里由下一个
/// 检查点或提交的 flush 覆盖它），模拟「记录已落盘」。
fn flush(k: &Kernel) {
    k.wal().flush_upto(Lsn(u64::MAX)).expect("flush");
}

#[test]
fn restore_returns_the_published_watermark_and_ignores_other_indexes() {
    let k = kernel("restore");
    create(&k);
    create(&k);
    let mut m = SegmentManifest::empty(IndexId(3));
    let to = ChangeFeed::object_watermark(&k);
    catch_up_all(&k, &mut m);
    flush(&k);
    assert_eq!(m.watermark, to);

    let restored = manifest_log::restore(k.wal(), IndexId(3))
        .unwrap()
        .expect("record persisted");
    // 最后一条快照是「推进之后」写的：重启后的起点就是下一次发布的起点。
    // `txn_watermark` 例外——它在 `advance` 里由本轮 xid 算出，不能从记录反推，
    // 只能取记录里的值（同样不超过当前值）。
    assert_eq!(restored.watermark, m.watermark);
    assert!(restored.txn_watermark.0 <= m.txn_watermark.0);

    // 另一个索引没有写过协调区：不能借到这条记录。
    assert_eq!(manifest_log::restore(k.wal(), IndexId(4)).unwrap(), None);
}

#[test]
fn restore_picks_the_latest_record() {
    let k = kernel("latest");
    let mut m = SegmentManifest::empty(IndexId(3));
    let mut first = None;
    for _ in 0..3 {
        create(&k);
        catch_up_all(&k, &mut m);
        first.get_or_insert(m.watermark);
    }
    flush(&k);
    let restored = manifest_log::restore(k.wal(), IndexId(3)).unwrap().unwrap();
    assert_eq!(
        restored.watermark,
        m.published_segments.last().unwrap().to_lsn
    );
    assert!(restored.watermark > first.unwrap(), "还原的是最新一条");
}

#[test]
fn persisted_record_holds_the_post_publish_pre_advance_watermark() {
    let k = kernel("order");
    create(&k);
    let mut m = SegmentManifest::empty(IndexId(3));
    let mut observed = Vec::new();
    let mut rate = ChangeRate::default();
    catch_up_observed(
        &MetaIndex::new(IndexId(3)),
        &k,
        k.wal(),
        &mut m,
        &ChangeBatchSpec::unbounded(),
        CatchUpLimits::new(ChangeFeed::object_watermark(&k), 8),
        &mut rate,
        |sealed: &SealedSegment| observed.push(sealed.segment_id),
    )
    .expect("catch up");
    assert_eq!(observed.len(), m.published_segments.len());
    flush(&k);

    // 记录在推进之前写入：它记的是「段已发布、水位还是旧值」，于是恢复后
    // 水位不会越过「已持久」的边界。
    let restored = manifest_log::restore(k.wal(), IndexId(3)).unwrap().unwrap();
    let durable = k.wal().durable_lsn();
    assert!(restored.watermark <= durable);
    // 每批写两次快照（推进前 + 推进后），还原的是推进后的那条。
    assert_eq!(restored.watermark, m.watermark);
}

#[test]
fn unflushed_manifest_records_are_not_restored() {
    let k = kernel("unflushed");
    create(&k);
    let mut m = SegmentManifest::empty(IndexId(3));
    catch_up_all(&k, &mut m);
    // 记录写了但**没有**在它之后 fsync：`durable_lsn` 仍停在最后一次提交的
    // flush 上。还原到的是已持久的最后一版（批次开始前的水位），比内存里的旧，
    // 但不会把未持久的水位当成已持久。
    let restored = manifest_log::restore(k.wal(), IndexId(3)).unwrap();
    match restored {
        None => assert!(k.wal().durable_lsn() <= m.watermark),
        Some(r) => assert!(r.watermark < m.watermark),
    }
}
