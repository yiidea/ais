//! 索引追赶循环与新鲜度（06 §3.1–§3.3 / §6.2 的 `fallback_full_scan`、
//! `coverage_partial_exposed`、`catchup_running_txn`、`catchup_wal_truncated`）。
//!
//! 全部用例只走冻结的三条边：F16 `ChangeFeed`、`append_derived`、F17。
//! 这里验证的是「索引追到哪」的**暴露方式**，不是索引内容本身（那是 `index_layer.rs`）。

use ains::hal::mem::MemDevice;
use ains::hal::StorageDevice;
use ains::index::{
    catch_up, CatchUpLimits, ChangeRate, CostModel, IndexFreshness, MetaIndex, SegmentManifest,
};
use ains::kernel::{
    ChangeBatchSpec, ChangeFeed, CreateObjectRequest, Kernel, KernelConfig, ObjectManager,
    Snapshot, TransactionManager, TxnCtx,
};
use ains::types::*;
use std::sync::Arc;

fn kernel(tag: &str) -> Arc<Kernel> {
    let dir = std::env::temp_dir().join(format!("ains-fresh-{tag}"));
    let _ = std::fs::remove_dir_all(&dir);
    Arc::new(
        Kernel::open(
            KernelConfig::new(dir.join("ckpt.snap")),
            Box::new(MemDevice::new(DeviceId(1), 16 << 20)) as Box<dyn StorageDevice>,
        )
        .expect("open kernel"),
    )
}

fn create(k: &Kernel, payload: &[u8]) -> ObjectId {
    let mut txn: TxnCtx = TransactionManager::begin(k, IsolationLevel::Snapshot).unwrap();
    let id = ObjectManager::create_object(
        k,
        &mut txn,
        &CreateObjectRequest {
            object_type: ObjectType::Document,
            payload: payload.to_vec(),
            component_count: 1,
        },
    )
    .unwrap();
    TransactionManager::commit(k, &mut txn).unwrap();
    id
}

fn manifest() -> SegmentManifest {
    SegmentManifest::empty(IndexId(7))
}

fn kind() -> MetaIndex {
    MetaIndex::new(IndexId(7))
}

#[test]
fn catch_up_publishes_segments_and_advances_both_watermarks() {
    let k = kernel("basic");
    for i in 0..5 {
        create(k.as_ref(), &[i as u8; 32]);
    }
    let mut m = manifest();
    let mut rate = ChangeRate::default();
    let to = ChangeFeed::object_watermark(k.as_ref());
    let report = catch_up(
        &kind(),
        k.as_ref(),
        k.wal(),
        &mut m,
        &ChangeBatchSpec::unbounded(),
        CatchUpLimits::new(to, 8),
        &mut rate,
    )
    .expect("catch up");

    assert!(report.rebuild_required.is_none());
    assert!(!report.blocked);
    assert_eq!(report.changes, 5);
    assert_eq!(m.watermark, to, "水位必须追上 object_watermark");
    assert!(m.txn_watermark.0 > 0, "xid 水位必须推进");
    assert!(!m.published_segments.is_empty());
    assert!(rate.bytes_per_change().is_some(), "本轮应留下字节/条数样本");
    // 再追一轮：已经追平，不产生新段、水位不动（A6 / A8：重复执行不变）。
    let segs = m.published_segments.len();
    let again = catch_up(
        &kind(),
        k.as_ref(),
        k.wal(),
        &mut m,
        &ChangeBatchSpec::unbounded(),
        CatchUpLimits::new(to, 8),
        &mut rate,
    )
    .expect("second catch up");
    assert!(again.segments.is_empty());
    assert_eq!(m.published_segments.len(), segs);
}

#[test]
fn lagging_index_forces_full_scan_and_fresh_one_does_not() {
    let k = kernel("coverage");
    let a = create(k.as_ref(), b"a");
    let xmin_of_a = visible_xmin(k.as_ref(), a);
    let mut m = manifest();
    let mut rate = ChangeRate::default();

    // 空索引的 xid 水位是 0，而快照 `xmin` 至少是 1：任何快照都判滞后（A4）。
    let before = k.fresh_snapshot();
    let stale = IndexFreshness::of_manifest(&m, ChangeFeed::object_watermark(k.as_ref()));
    assert!(!IndexFreshness::covers(&m, &before));
    assert!(!stale.covers_snapshot(&before));

    catch_up_now(k.as_ref(), &mut m, &mut rate);
    // 追平后 xid 水位 = 区间里最后出现的事务，也就是 `a` 的创建者。
    assert_eq!(
        m.txn_watermark, xmin_of_a,
        "tx_w 应停在区间里最后出现的事务上"
    );

    // A3：`tx_w ≥ S.xmin` ⇔ 覆盖。`fresh_snapshot` 不读提交状态（事务分配 xid 时
    // 就把它写进 `xmax`，见 `mod.rs` 的 `begin`），所以「刚提交完的写入的快照」
    // 用它的 xmin 直接构造——那是本次写入可见的最小快照。
    let covered = Snapshot {
        xmin: xmin_of_a,
        ..k.fresh_snapshot()
    };
    assert!(IndexFreshness::covers(&m, &covered));
    // 完整的新鲜度按快照求值：`lagging` 只由 `xmin ≤ tx_w` 决定。
    let model = CostModel {
        c_scan_nanos: 2_000,
        c_point_nanos: 500,
        filter_eval_nanos: 100,
        safety_factor_num: 3,
        safety_factor_den: 2,
    };
    let now = ChangeFeed::object_watermark(k.as_ref());
    let fresh = IndexFreshness::evaluate(&m, &covered, now, &rate, &model);
    assert!(!fresh.lagging);
    assert!(fresh.covers_snapshot(&covered));
    assert_eq!(fresh.txn_watermark, m.txn_watermark);
    // 比 `a` 更早的快照（`a` 尚未提交）：同样被覆盖。
    let early = Snapshot {
        xmin: TransactionId(1),
        ..before
    };
    assert!(IndexFreshness::evaluate(&m, &early, now, &rate, &model).covers_snapshot(&early));

    // 之后的写入让索引重新落后：新快照的 xmin 越过水位，必须判滞后。
    let b = create(k.as_ref(), b"b");
    assert!(visible_xmin(k.as_ref(), b).0 > m.txn_watermark.0);
    let newest = k.fresh_snapshot();
    assert!(
        !IndexFreshness::covers(&m, &newest),
        "索引没追到新事务，必须判滞后"
    );
    assert!(
        IndexFreshness::covers(&m, &covered),
        "旧快照不能因为新写入而失去覆盖"
    );
}

/// 读一下最新版本自己的 xmin（索引条目要写的 `created_by_xid`）。
fn visible_xmin(k: &Kernel, id: ObjectId) -> TransactionId {
    let snap = k.fresh_snapshot();
    let ver = ObjectManager::resolve_version(k, id, &snap).unwrap();
    ObjectManager::read_version(k, id, ver)
        .unwrap()
        .version
        .xmin
}

/// 追平当前水位（之后必须没有任何内核操作：`tx_w` 是追赶区间里 xid 的最大值，
/// 追赶完成后再开新事务，新 xid 就落在水位之上，直到下一轮追赶）。
fn catch_up_now(k: &Kernel, m: &mut SegmentManifest, rate: &mut ChangeRate) {
    catch_up(
        &kind(),
        k,
        k.wal(),
        m,
        &ChangeBatchSpec::unbounded(),
        CatchUpLimits::new(Lsn(u64::MAX), 8),
        rate,
    )
    .expect("catch up");
}

/// 每轮批量为 1：验证跨多轮的批次尾与水位推进（每次 `publish` 后紧跟 `advance`）。
fn catch_up_one_at_a_time(k: &Kernel, m: &mut SegmentManifest) {
    let mut rate = ChangeRate::default();
    for _ in 0..32 {
        catch_up(
            &kind(),
            k,
            k.wal(),
            m,
            &ChangeBatchSpec::new(1, u64::MAX),
            CatchUpLimits::new(Lsn(u64::MAX), 1),
            &mut rate,
        )
        .expect("catch up");
    }
}

#[test]
fn pending_txn_blocks_the_watermark_but_keeps_committed_changes() {
    let k = kernel("pending");
    let a = create(k.as_ref(), b"a");
    let mut m = manifest();
    let mut rate = ChangeRate::default();

    // 未提交事务：它之后的记录都是屏障，屏障之前的 `a` 必须发布。
    let mut pending: TxnCtx =
        TransactionManager::begin(k.as_ref(), IsolationLevel::Snapshot).unwrap();
    let _staged = ObjectManager::create_object(
        k.as_ref(),
        &mut pending,
        &CreateObjectRequest {
            object_type: ObjectType::Document,
            payload: b"uncommitted".to_vec(),
            component_count: 1,
        },
    )
    .unwrap();

    let report = catch_up(
        &kind(),
        k.as_ref(),
        k.wal(),
        &mut m,
        &ChangeBatchSpec::unbounded(),
        CatchUpLimits::new(Lsn(u64::MAX), 8),
        &mut rate,
    )
    .expect("catch up");
    assert!(report.blocked, "未决事务必须让本轮停下");
    assert_eq!(report.changes, 1, "屏障之前已提交的 `a` 照常发布");
    assert_eq!(m.published_segments.len(), 1);
    let tx_w_before = m.txn_watermark;
    assert!(
        tx_w_before.0 < pending.xid.0,
        "xid 水位不得越过未决事务（A10）"
    );
    assert_eq!(m.txn_watermark, visible_xmin(k.as_ref(), a));

    // 中止未决事务：它永远不会进索引，下一次重建的条目集合与现在一致。
    TransactionManager::abort(k.as_ref(), &mut pending).unwrap();
    catch_up_now(k.as_ref(), &mut m, &mut rate);
    // 中止事务的 xid 已判定，水位随之推进；但它没有对象变更，索引条目数不变。
    assert!(m.txn_watermark.0 >= tx_w_before.0);
}

#[test]
fn many_small_batches_converge_like_one_big_batch() {
    let k = kernel("batch");
    for i in 0..6 {
        let id = create(k.as_ref(), &[i as u8; 16]);
        if i % 2 == 0 {
            let mut txn: TxnCtx =
                TransactionManager::begin(k.as_ref(), IsolationLevel::Snapshot).unwrap();
            ObjectManager::stage_version(
                k.as_ref(),
                &mut txn,
                id,
                &ains::kernel::VersionSpec {
                    payload: vec![0xEE; 16],
                },
            )
            .unwrap();
            TransactionManager::commit(k.as_ref(), &mut txn).unwrap();
        }
    }

    let mut one = manifest();
    let mut many = manifest();
    catch_up_now(k.as_ref(), &mut one, &mut ChangeRate::default());
    catch_up_one_at_a_time(k.as_ref(), &mut many);

    // 水位在**提交记录自身**的 LSN 处停下：提交之后的记录要么不属于本区间，
    // 要么是未判定的写入事务（A10），两者都不能算已消费。
    let exact = k
        .wal()
        .scan_from(Lsn(0))
        .expect("scan")
        .records
        .iter()
        .filter(|r| matches!(r.record.record_type, ains::wal::RecordType::TxnCommit))
        // 游标指向**下一条未消费记录**：最后一条已消费记录之后的那一条。
        .map(|r| r.lsn.0)
        .max()
        .map(|lsn| {
            k.wal()
                .scan_from(Lsn(lsn))
                .expect("scan")
                .records
                .iter()
                .filter(|r| r.lsn.0 > lsn)
                .map(|r| r.lsn.0)
                .min()
                .unwrap_or(lsn)
        })
        .expect("至少有一次提交");
    assert_eq!(
        one.watermark,
        Lsn(exact),
        "单批追赶应停在最后一条提交记录之后"
    );
    // 小批量时最后一批可能只装下非对象记录（`TxnCommit` 本身不产生变更），此时
    // 那一批被整体跳过——水位停在此前最后一条**产生了变更**的记录之后。两种做法
    // 都不越过未判定的写入事务，只是终点不同。
    let mut to = one.watermark;
    while to < many.watermark {
        let batch = ChangeFeed::scan_changes(
            k.as_ref(),
            to,
            many.watermark,
            &ChangeBatchSpec::unbounded(),
        )
        .expect("scan tail");
        assert!(
            batch.changes.is_empty() && !batch.blocked_by_pending_txn,
            "差的只能是非对象记录"
        );
        to = batch.next_cursor.unwrap_or(many.watermark);
    }
    assert_eq!(many.txn_watermark, one.txn_watermark);
    let count =
        |m: &SegmentManifest| -> u64 { m.published_segments.iter().map(|s| s.entry_count).sum() };
    assert!(
        count(&many) >= count(&one),
        "拆批只会让同一区间产出更多段，条目不会少"
    );
    assert!(one.published_segments.len() <= many.published_segments.len());

    // 再次单批追赶：已经追平，不产生新段（A6）。
    let segs = many.published_segments.len();
    catch_up_now(k.as_ref(), &mut many, &mut ChangeRate::default());
    assert_eq!(many.published_segments.len(), segs);
}

#[test]
fn fallback_estimate_needs_a_measured_rate() {
    let k = kernel("estimate");
    for _ in 0..4 {
        create(k.as_ref(), &[7u8; 64]);
    }
    let model = CostModel {
        c_scan_nanos: 2_000,
        c_point_nanos: 500,
        filter_eval_nanos: 100,
        safety_factor_num: 3,
        safety_factor_den: 2,
    };
    let mut m = manifest();
    let mut rate = ChangeRate::default();

    // 没有样本时**不许猜**：返回 None，调用方必须自己给上界。
    assert_eq!(rate.estimate(1 << 20, &model), None);

    catch_up(
        &kind(),
        k.as_ref(),
        k.wal(),
        &mut m,
        &ChangeBatchSpec::unbounded(),
        CatchUpLimits::new(Lsn(u64::MAX), 8),
        &mut rate,
    )
    .expect("catch up");
    let per = rate.bytes_per_change().expect("sample");

    // 缺口 = 一倍样本量 + 乘上安全系数 → 估计 ≥ 实测条数 × 1.5。
    let est = rate.estimate(per * 4, &model).expect("estimate");
    assert!(est >= 6, "4 条 × 1.5 的安全系数应给到 ≥ 6，实际 {est}");
    // 缺口不足一条也要估成 1，否则规划器会以为「立刻追平」。
    assert_eq!(
        rate.estimate(1, &model),
        Some(2),
        "1 字节 → 1 条 → ×1.5 取整"
    );
}
