//! 索引层验收用例（[08 §5]，与 06 §6.2 对齐）。
//!
//! 全部用例只走内核冻结的三条边（F16 `ChangeFeed`、`append_derived`、F17
//! `LiveSnapshotSource`），不碰内核内部状态——索引层是调用方，不是内核的一部分。
//!
//! [08 §5]: ../../docs/chatgpt/08-index-layer.md

use ains::hal::mem::MemDevice;
use ains::hal::StorageDevice;
use ains::index::{
    build_segment, catchup, reclaimable, Blocker, CatchUpStep, EntryKind, IndexEntry, MetaIndex,
    SealError, Segment, SegmentId, SegmentManifest,
};
use ains::kernel::{
    ChangeBatchSpec, ChangeFeed, CreateObjectRequest, EntryVisibility, Kernel, KernelConfig,
    LiveSnapshotHandle, LiveSnapshotSource, ObjectManager, TransactionManager, TxnCtx,
};
use ains::types::*;
use std::sync::Arc;

fn kernel(tag: &str) -> Arc<Kernel> {
    let dir = std::env::temp_dir().join(format!("ains-index-{tag}"));
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

fn add_version(k: &Kernel, id: ObjectId, payload: &[u8]) -> VersionId {
    let mut txn: TxnCtx = TransactionManager::begin(k, IsolationLevel::Snapshot).unwrap();
    let v = ObjectManager::stage_version(
        k,
        &mut txn,
        id,
        &ains::kernel::VersionSpec {
            payload: payload.to_vec(),
        },
    )
    .unwrap();
    TransactionManager::commit(k, &mut txn).unwrap();
    v
}

fn delete(k: &Kernel, id: ObjectId) {
    let mut txn: TxnCtx = TransactionManager::begin(k, IsolationLevel::Snapshot).unwrap();
    ObjectManager::delete_object(k, &mut txn, id).unwrap();
    TransactionManager::commit(k, &mut txn).unwrap();
}

const KIND: MetaIndex = MetaIndex::new(IndexId(1));

/// 追赶到 `to`，返回所有段。
fn catch_up_all(k: &Kernel, mut cursor: Lsn, to: Lsn) -> Vec<ains::index::SealedSegment> {
    let spec = ChangeBatchSpec::unbounded();
    let mut segments = Vec::new();
    loop {
        match catchup::step(k as &dyn ChangeFeed, cursor, to, &spec).expect("scan") {
            CatchUpStep::CaughtUp => return segments,
            CatchUpStep::Blocked { resume_at } => {
                assert_eq!(resume_at, cursor, "blocked cursor must not advance");
                return segments;
            }
            CatchUpStep::Progress { changes, next } => {
                // 批次可能恰好收尾在 `to`：此时 `next` 为 `None`，段尾就是 `to`。
                let next = next.unwrap_or(to);
                segments.push(
                    build_segment(&KIND, k, 1, cursor, next, &changes).expect("build segment"),
                );
                // 收尾或没有进展都结束本轮（空批不得推进游标，06 §2.7）。
                if (cursor >= next && changes.is_empty()) || next >= to {
                    return segments;
                }
                cursor = next;
            }
        }
    }
}

fn fetch_changes(k: &Kernel) -> Vec<ains::kernel::ObjectChange> {
    catchup_all_changes(k, Lsn(0), ChangeFeed::object_watermark(k))
}

fn catchup_all_changes(k: &Kernel, mut cursor: Lsn, to: Lsn) -> Vec<ains::kernel::ObjectChange> {
    let spec = ChangeBatchSpec::unbounded();
    let mut out = Vec::new();
    loop {
        match catchup::step(k, cursor, to, &spec).expect("scan") {
            CatchUpStep::CaughtUp | CatchUpStep::Blocked { .. } => return out,
            CatchUpStep::Progress { changes, next } => {
                cursor = next.unwrap_or(to);
                out.extend(changes);
                if cursor >= to {
                    return out;
                }
            }
        }
    }
}

/// 按主键取最后一次动作（跨段折叠成逻辑索引状态）。
fn latest_per_key(
    segments: &[ains::index::SealedSegment],
) -> Vec<(ObjectId, VersionId, EntryKind)> {
    let mut map: std::collections::BTreeMap<(u64, u64), EntryKind> = Default::default();
    for s in segments {
        for e in Segment::decode(&s.bytes).expect("decode").entries {
            map.insert((e.object.sequence(), e.version.0), e.kind);
        }
    }
    map.into_iter()
        .map(|((o, v), k)| (ObjectId::new(1, o), VersionId(v), k))
        .collect()
}

// ---------------------------------------------------------------------------
// 段格式
// ---------------------------------------------------------------------------

#[test]
fn segment_roundtrip_is_strict() {
    let seg = Segment {
        index_id: IndexId(1),
        from_lsn: Lsn(10),
        to_lsn: Lsn(20),
        entries: vec![
            IndexEntry {
                kind: EntryKind::Live,
                object: ObjectId::new(1, 7),
                version: VersionId(1),
                created_by_xid: TransactionId(3),
                deleted_by_xid: None,
                payload: vec![4, 0, 0, 0, 0],
            },
            IndexEntry {
                kind: EntryKind::Tombstone,
                object: ObjectId::new(1, 7),
                version: VersionId(1),
                created_by_xid: TransactionId(5),
                deleted_by_xid: None,
                payload: Vec::new(),
            },
        ],
    };
    let bytes = seg.encode();
    assert_eq!(Segment::decode(&bytes).expect("decode"), seg);

    // 截断必须失败，且是 `Corrupted`。
    let short = &bytes[..bytes.len() - 3];
    assert!(Segment::decode(short).is_err());

    // 翻一个 bit：整段摘要先炸。
    let mut flipped = bytes.clone();
    let at = flipped.len() - 8;
    flipped[at] ^= 0xFF;
    assert!(Segment::decode(&flipped).is_err());

    // 尾部多余字节同样不接受。
    let mut trailing = bytes.clone();
    trailing.push(0);
    assert!(Segment::decode(&trailing).is_err());

    // 段头识别字。
    let mut bad_magic = bytes.clone();
    bad_magic[0] ^= 0xFF;
    assert!(Segment::decode(&bad_magic).is_err());
}

#[test]
fn segment_id_is_an_idempotent_key() {
    let a = SegmentId::derive(IndexId(1), Lsn(10), Lsn(20), 0xDEAD_BEEF);
    let b = SegmentId::derive(IndexId(1), Lsn(10), Lsn(20), 0xDEAD_BEEF);
    assert_eq!(a, b, "same key must give the same segment id");

    // 任一分量变化都会改变标识。
    assert_ne!(
        a,
        SegmentId::derive(IndexId(2), Lsn(10), Lsn(20), 0xDEAD_BEEF)
    );
    assert_ne!(
        a,
        SegmentId::derive(IndexId(1), Lsn(11), Lsn(20), 0xDEAD_BEEF)
    );
    assert_ne!(
        a,
        SegmentId::derive(IndexId(1), Lsn(10), Lsn(21), 0xDEAD_BEEF)
    );
    assert_ne!(
        a,
        SegmentId::derive(IndexId(1), Lsn(10), Lsn(20), 0xDEAD_BEEE)
    );
}

// ---------------------------------------------------------------------------
// 追赶
// ---------------------------------------------------------------------------

#[test]
fn watermark_never_over_pending_txn() {
    let k = kernel("pending");
    let _committed = create(k.as_ref(), b"first");

    // 开一个未提交事务：它的记录成为屏障。
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

    let to = ChangeFeed::object_watermark(k.as_ref());
    let spec = ChangeBatchSpec::unbounded();
    match catchup::step(k.as_ref(), Lsn(0), to, &spec).expect("scan") {
        CatchUpStep::Blocked { resume_at } => {
            // 游标停在屏障之前：只消费了已提交的那部分。
            assert!(resume_at.0 > 0);
            assert!(resume_at.0 <= to.0);
        }
        other => panic!("expected a barrier, got {other:?}"),
    }
}

#[test]
fn blocked_cursor_moves_after_the_txn_terminates() {
    let k = kernel("pending-resolve");
    let _first = create(k.as_ref(), b"first");

    let mut pending: TxnCtx =
        TransactionManager::begin(k.as_ref(), IsolationLevel::Snapshot).unwrap();
    // 未决事务的 `TxnBegin` 也在区间里：一次扫码可能直接停在屏障处（没有已提交的变更
    // 排在它前面），也可能先返回已提交的批次。两种都合法，这里只要求「停住或前进」。
    let blocked_at = {
        let to = ChangeFeed::object_watermark(k.as_ref());
        match catchup::step(k.as_ref(), Lsn(0), to, &ChangeBatchSpec::unbounded()).expect("scan") {
            CatchUpStep::Progress { next, .. } => next.unwrap_or(to),
            CatchUpStep::Blocked { resume_at } => resume_at,
            CatchUpStep::CaughtUp => panic!("there is a pending txn, cannot be caught up"),
        }
    };
    // 屏障之前已经消费完；从屏障处再拉应该立刻撞上。
    match catchup::step(
        k.as_ref(),
        blocked_at,
        ChangeFeed::object_watermark(k.as_ref()),
        &ChangeBatchSpec::unbounded(),
    )
    .expect("scan")
    {
        CatchUpStep::Blocked { .. } => {}
        other => panic!("expected blocked, got {other:?}"),
    }

    TransactionManager::abort(k.as_ref(), &mut pending).unwrap();
    match catchup::step(
        k.as_ref(),
        blocked_at,
        ChangeFeed::object_watermark(k.as_ref()),
        &ChangeBatchSpec::unbounded(),
    )
    .expect("scan")
    {
        // 中止事务的记录被丢弃，游标可以越过。
        CatchUpStep::CaughtUp | CatchUpStep::Progress { .. } => {}
        other => panic!("cursor must pass an aborted txn, got {other:?}"),
    }
}

#[test]
fn rewind_when_wal_truncated_requires_rebuild() {
    let k = kernel("truncated");
    let _ = create(k.as_ref(), b"first");
    let to = ChangeFeed::object_watermark(k.as_ref());

    // 模拟 WAL 已截断：可截断水位推进到区间起点之后。
    k.wal().set_min_required_lsn(Lsn(to.0 + 1));
    let err = catchup::step(k.as_ref(), Lsn(0), to, &ChangeBatchSpec::unbounded())
        .expect_err("cursor before min_required must fail");
    assert!(err.reason.contains("min_required_lsn"));
}

// ---------------------------------------------------------------------------
// 构建
// ---------------------------------------------------------------------------

#[test]
fn build_reads_the_version_named_by_the_change() {
    let k = kernel("build");
    let id = create(k.as_ref(), b"v1");
    let v2 = add_version(k.as_ref(), id, b"v2");
    assert_eq!(v2, VersionId(2));

    let to = ChangeFeed::object_watermark(k.as_ref());
    let segments = catch_up_all(k.as_ref(), Lsn(0), to);
    assert!(!segments.is_empty());

    let decoded: Vec<IndexEntry> = segments
        .iter()
        .flat_map(|s| Segment::decode(&s.bytes).unwrap().entries)
        .collect();
    // 两个版本都在索引里：删除旧版本是空间回收，不是正确性要求（06 §2.3）。
    assert!(decoded
        .iter()
        .any(|e| e.object == id && e.version == VersionId(1)));
    assert!(decoded
        .iter()
        .any(|e| e.object == id && e.version == VersionId(2)));
    // 每个条目都带 `created_by_xid`：没有它就判不了可见性。
    assert!(decoded.iter().all(|e| e.created_by_xid.0 != 0));
}

#[test]
fn missing_version_is_an_error_not_a_silent_skip() {
    let k = kernel("missing-version");
    let to = ChangeFeed::object_watermark(k.as_ref());
    let real = create(k.as_ref(), b"payload");
    let _ = to;

    // 构造一条指向不存在版本的变更：元数据取自真实变更，只把版本号改坏。
    let real_change = fetch_changes(k.as_ref())
        .into_iter()
        .find(|c| c.object() == real)
        .expect("created change");
    let change = match real_change {
        ains::kernel::ObjectChange::Created { object, meta, .. } => {
            ains::kernel::ObjectChange::Created {
                object,
                version: VersionId(99),
                meta,
            }
        }
        other => panic!("expected Created, got {other:?}"),
    };
    let err = build_segment(&KIND, &*k, 1, Lsn(0), Lsn(10), &[change])
        .expect_err("missing version must fail");
    assert!(matches!(err, SealError::MissingVersion { .. }));
}

#[test]
fn tombstone_direction_is_opposite_to_the_entry() {
    let k = kernel("tombstone");
    let id = create(k.as_ref(), b"to-be-deleted");
    // 删除前冻结的旧快照：删除提交之后它仍然必须读得到创建时的条目。
    let old = LiveSnapshotHandle::capture(&k);
    let to_before = ChangeFeed::object_watermark(k.as_ref());
    delete(k.as_ref(), id);

    let entries: Vec<IndexEntry> =
        catch_up_all(k.as_ref(), Lsn(0), ChangeFeed::object_watermark(k.as_ref()))
            .iter()
            .flat_map(|s| Segment::decode(&s.bytes).unwrap().entries)
            .collect();
    assert!(ChangeFeed::object_watermark(k.as_ref()) > to_before);

    // 被删版本（创建后立刻删除，版本 1）只留一条墓碑，指向执行删除的事务。
    let tombstone = entries
        .iter()
        .find(|e| e.object == id && e.kind == EntryKind::Tombstone)
        .expect("deleting a created version leaves a tombstone");
    assert_eq!(tombstone.version, VersionId(1));
    assert!(!entries
        .iter()
        .any(|e| e.object == id && e.kind == EntryKind::Live));

    // 方向相反：删除提交**之后**冻结的新快照看不见被删版本（删除不复活），
    // 删除提交**之前**冻结的旧快照看得见它（G3 —— 旧条目仍要保留）。
    let (xmin, xmax) = k.version_xids(id, VersionId(1)).expect("version exists");
    let entry = EntryVisibility::new(xmin, xmax);
    let tomb = tombstone.visibility();
    assert!(xmax.is_some());
    // 旧快照（冻结在删除之前）看得见删除前的版本——它看不见删除事务，因此
    // 条目仍然对它有效。这正是「旧条目不能因为删除就被丢」的直接来源（G3）。
    assert!(LiveSnapshotSource::visible(
        k.as_ref(),
        &entry,
        &old.snapshot
    ));
    // 反过来，删除之后的快照看不见这个版本；墓碑则反过来可见（G4）。
    let fresh = LiveSnapshotHandle::capture(&k);
    assert!(!LiveSnapshotSource::visible(
        k.as_ref(),
        &entry,
        &fresh.snapshot
    ));
    assert!(LiveSnapshotSource::visible_to_any_live(k.as_ref(), &tomb).unwrap());
    fresh.release();
    old.release();
}

/// 有可见版本时的删除：墓碑指向那个版本。
#[test]
fn tombstone_targets_the_visible_version() {
    let k = kernel("tombstone-visible");
    let id = create(k.as_ref(), b"v1");
    let old = LiveSnapshotHandle::capture(&k);
    delete(k.as_ref(), id);

    let entries: Vec<IndexEntry> =
        catch_up_all(k.as_ref(), Lsn(0), ChangeFeed::object_watermark(k.as_ref()))
            .iter()
            .flat_map(|s| Segment::decode(&s.bytes).unwrap().entries)
            .collect();
    let tombstone = entries
        .iter()
        .find(|e| e.kind == EntryKind::Tombstone && e.object == id)
        .expect("a delete with a visible version leaves a tombstone");
    assert_eq!(tombstone.version, VersionId(1));
    // 墓碑描述的删除由 `created_by_xid` 承载，`deleted_by_xid` 为空——
    // 这正是 `EntryVisibility::tombstone` 的形状（删除对该快照是否可见由 G4 判）。
    assert!(tombstone.deleted_by_xid.is_none());
    assert_ne!(tombstone.created_by_xid.0, 0);
    // 同一主键上了墓碑：活条目被删除取代，新快照不会解析到陈旧条目。
    assert!(entries
        .iter()
        .all(|e| !(e.object == id && e.kind == EntryKind::Live)));

    let (xmin, xmax) = k.version_xids(id, VersionId(1)).expect("version exists");
    assert!(xmax.is_some());
    assert!(LiveSnapshotSource::visible(
        k.as_ref(),
        &EntryVisibility::new(xmin, xmax),
        &old.snapshot
    ));

    // 读失败、判定为可见时整段保留：这是 G3 / G4 的保守方向。
    let seg = Segment {
        index_id: IndexId(1),
        from_lsn: Lsn(0),
        to_lsn: ChangeFeed::object_watermark(k.as_ref()),
        entries: entries.clone(),
    };
    let mut m = SegmentManifest::empty(IndexId(1));
    m.watermark = seg.to_lsn;
    let live = LiveSnapshotHandle::capture(&k);
    let verdict = reclaimable(&seg, &m, k.as_ref(), true, false);
    assert!(verdict.blockers.contains(&Blocker::VisibleEntries));
    live.release();

    // 没有活快照之后，墓碑不再阻断回收。
    let verdict = reclaimable(&seg, &m, k.as_ref(), true, false);
    assert!(!verdict.blockers.contains(&Blocker::VisibleEntries));
}

// ---------------------------------------------------------------------------
// 协调区
// ---------------------------------------------------------------------------

#[test]
fn publish_is_monotonic_and_deduplicated() {
    let mut m = SegmentManifest::empty(IndexId(1));
    let a = SegmentId::derive(IndexId(1), Lsn(0), Lsn(10), 1);
    let b = SegmentId::derive(IndexId(1), Lsn(10), Lsn(20), 2);

    let t1 = m.publish(ains::index::PublishedSegment {
        segment_id: a,
        from_lsn: Lsn(0),
        to_lsn: Lsn(10),
        entry_count: 3,
    });
    m.advance(&t1, Lsn(0), TransactionId(5)).expect("first");
    assert_eq!(m.watermark, Lsn(10));
    assert_eq!(m.txn_watermark, TransactionId(5));
    assert_eq!(m.baseline_segment_id, Some(a));

    // 同一段重复发布：去重，不重复计。
    let t1_again = m.publish(ains::index::PublishedSegment {
        segment_id: a,
        from_lsn: Lsn(0),
        to_lsn: Lsn(10),
        entry_count: 3,
    });
    assert_eq!(m.published_segments.len(), 1);
    m.advance(&t1_again, Lsn(10), TransactionId(5))
        .expect("idempotent");
    assert_eq!(m.watermark, Lsn(10));

    let t2 = m.publish(ains::index::PublishedSegment {
        segment_id: b,
        from_lsn: Lsn(10),
        to_lsn: Lsn(20),
        entry_count: 4,
    });
    m.advance(&t2, Lsn(10), TransactionId(9)).expect("second");
    assert_eq!(m.watermark, Lsn(20));
    assert_eq!(
        m.baseline_segment_id,
        Some(a),
        "baseline never moves forward"
    );
}

#[test]
fn cas_conflict_loses_the_race_without_corrupting_state() {
    let mut m = SegmentManifest::empty(IndexId(1));
    let seg = ains::index::PublishedSegment {
        segment_id: SegmentId::derive(IndexId(1), Lsn(0), Lsn(10), 1),
        from_lsn: Lsn(0),
        to_lsn: Lsn(10),
        entry_count: 1,
    };
    let ticket = m.publish(seg);
    m.advance(&ticket, Lsn(0), TransactionId(2))
        .expect("winner");

    // 第二个追赶者拿着过期的期望值：CAS 必须失败，且不改变任何状态。
    let before = m.clone();
    let conflict = m
        .advance(&ticket, Lsn(0), TransactionId(3))
        .expect_err("stale expectation");
    assert_eq!(conflict.expected, Lsn(0));
    assert_eq!(conflict.actual, Lsn(10));
    assert_eq!(m, before, "a lost race changes nothing");
}

#[test]
fn publish_order_makes_watermark_ahead_of_segments_unrepresentable() {
    // 结构性质：推进水位必须出示一张 `Advance` 票据，而票据只能由 `publish` 发出。
    // 因此「水位已推进、段缺失」在这套 API 上没有写法——这里是它的行为证据：
    // 一个没有任何已发布段的清单，水位恒为 0。
    let m = SegmentManifest::empty(IndexId(7));
    assert!(m.published_segments.is_empty());
    assert_eq!(m.watermark, Lsn(0));
    assert!(!m.is_published(SegmentId::derive(IndexId(7), Lsn(0), Lsn(1), 0)));
}

#[test]
fn orphan_segments_are_discarded_on_recovery() {
    let mut m = SegmentManifest::empty(IndexId(1));
    let published = SegmentId::derive(IndexId(1), Lsn(0), Lsn(10), 1);
    let orphan = SegmentId::derive(IndexId(1), Lsn(10), Lsn(20), 2);
    let ticket = m.publish(ains::index::PublishedSegment {
        segment_id: published,
        from_lsn: Lsn(0),
        to_lsn: Lsn(10),
        entry_count: 1,
    });
    m.advance(&ticket, Lsn(0), TransactionId(1)).unwrap();

    let discarded = m.discard_orphans([&published, &orphan]);
    assert_eq!(discarded, vec![orphan]);
    assert_eq!(m.watermark, Lsn(10), "watermark does not move backwards");
}

// ---------------------------------------------------------------------------
// 回收（G1–G5）
// ---------------------------------------------------------------------------

fn published_seg(entries: Vec<IndexEntry>) -> Segment {
    Segment {
        index_id: IndexId(1),
        from_lsn: Lsn(0),
        to_lsn: Lsn(10),
        entries,
    }
}

fn manifest_with(seg: &Segment) -> SegmentManifest {
    let mut m = SegmentManifest::empty(IndexId(1));
    let built = build_segment(&KIND, &DummyKernel, 1, seg.from_lsn, seg.to_lsn, &[])
        .expect("empty segment");
    let _ = built;
    // 用段自身算出的 ID 作为「已发布」，但**不**设为基线，以便单独测 G2–G5。
    let id = SegmentId::derive(seg.index_id, seg.from_lsn, seg.to_lsn, 0);
    let ticket = m.publish(ains::index::PublishedSegment {
        segment_id: id,
        from_lsn: seg.from_lsn,
        to_lsn: seg.to_lsn,
        entry_count: seg.entries.len() as u64,
    });
    m.advance(&ticket, Lsn(0), TransactionId(100)).unwrap();
    // 清掉基线段标记，让 G1 不作为本次判定的阻断项。
    m.baseline_segment_id = None;
    m
}

struct DummyKernel;

impl ains::index::builder::KernelLike for DummyKernel {
    fn version_xids(
        &self,
        _: ObjectId,
        _: VersionId,
    ) -> Option<(TransactionId, Option<TransactionId>)> {
        None
    }
}

#[test]
fn baseline_is_never_reclaimed() {
    let k = kernel("g1");
    let id = create(k.as_ref(), b"x");
    let seg = published_seg(vec![IndexEntry {
        kind: EntryKind::Live,
        object: id,
        version: VersionId(1),
        created_by_xid: TransactionId(1),
        deleted_by_xid: Some(TransactionId(2)),
        payload: vec![],
    }]);
    let mut m = manifest_with(&seg);
    // 把段设成基线。
    // 与 `reclaimable` 用同一个口径（段字节的整段摘要）算基线段标识。
    let bytes = seg.encode();
    let at = ains::index::segment::SEGMENT_HEADER_SIZE - 4;
    let content = u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]]);
    m.baseline_segment_id = Some(SegmentId::derive(
        seg.index_id,
        seg.from_lsn,
        seg.to_lsn,
        content,
    ));

    let verdict = reclaimable(&seg, &m, &*k, true, false);
    assert!(!verdict.reclaimable);
    assert!(verdict.blockers.contains(&Blocker::Baseline));
}

#[test]
fn visible_entry_keeps_the_whole_segment() {
    let k = kernel("g3");
    let id = create(k.as_ref(), b"alive");
    let xids = k.version_xids(id, VersionId(1)).expect("version exists");
    let seg = published_seg(vec![IndexEntry {
        kind: EntryKind::Live,
        object: id,
        version: VersionId(1),
        created_by_xid: xids.0,
        deleted_by_xid: None,
        payload: vec![],
    }]);
    let m = manifest_with(&seg);

    // 有一个活快照，条目对它是可见的 → 整段保留（不做部分回收）。
    let live = LiveSnapshotHandle::capture(&k);
    let verdict = reclaimable(&seg, &m, &*k, true, false);
    assert!(!verdict.reclaimable);
    assert!(verdict.blockers.contains(&Blocker::VisibleEntries));
    assert!(!verdict.live_entries.is_empty());
    live.release();

    // 没有活快照时，可见性条件不再阻断（其余条件仍可能挡）。
    let verdict = reclaimable(&seg, &m, &*k, true, false);
    assert!(!verdict.blockers.contains(&Blocker::VisibleEntries));
}

#[test]
fn uncovered_segment_is_not_reclaimed() {
    let k = kernel("g2");
    let id = create(k.as_ref(), b"future");
    let seg = Segment {
        index_id: IndexId(1),
        from_lsn: Lsn(100),
        to_lsn: Lsn(200),
        entries: vec![IndexEntry {
            kind: EntryKind::Live,
            object: id,
            version: VersionId(1),
            created_by_xid: TransactionId(1),
            deleted_by_xid: Some(TransactionId(2)),
            payload: vec![],
        }],
    };
    let mut m = SegmentManifest::empty(IndexId(1));
    m.watermark = Lsn(50); // 段尾 200 > 水位 50
    m.baseline_segment_id = None;
    let verdict = reclaimable(&seg, &m, &*k, true, false);
    assert!(!verdict.reclaimable);
    assert!(verdict.blockers.contains(&Blocker::NotCoveredByWatermark));
}

#[test]
fn reclaim_requires_the_generation_to_have_advanced() {
    let k = kernel("g5");
    let id = create(k.as_ref(), b"x");
    let seg = published_seg(vec![IndexEntry {
        kind: EntryKind::Live,
        object: id,
        version: VersionId(1),
        created_by_xid: TransactionId(1),
        deleted_by_xid: Some(TransactionId(2)),
        payload: vec![],
    }]);
    let m = manifest_with(&seg);

    // 回收代次未推进 → 无法证明没有新读者。
    let verdict = reclaimable(&seg, &m, &*k, false, false);
    assert!(verdict.blockers.contains(&Blocker::Referenced));

    // 已有读者引用 → 同样不可回收。
    let verdict = reclaimable(&seg, &m, &*k, true, true);
    assert!(verdict.blockers.contains(&Blocker::Referenced));
}

// ---------------------------------------------------------------------------
// 重建 vs 增量
// ---------------------------------------------------------------------------

#[test]
fn rebuild_equals_incremental() {
    let k = kernel("rebuild");
    let a = create(k.as_ref(), b"a");
    let b = create(k.as_ref(), b"b");
    let _v2 = add_version(k.as_ref(), a, b"a2");
    delete(k.as_ref(), b);

    let to = ChangeFeed::object_watermark(k.as_ref());
    // 增量：一次追完。
    let incremental = catch_up_all(k.as_ref(), Lsn(0), to);

    // 「重建」：从 0 开始分很多小批重新追（模拟全量重建的条目集合）。
    let spec = ChangeBatchSpec::new(1, u64::MAX);
    let mut cursor = Lsn(0);
    let mut rebuilt = Vec::new();
    loop {
        match catchup::step(k.as_ref(), cursor, to, &spec).expect("scan") {
            CatchUpStep::CaughtUp => break,
            CatchUpStep::Blocked { .. } => break,
            CatchUpStep::Progress { changes, next } => {
                let next = next.expect("cursor");
                rebuilt.push(build_segment(&KIND, &*k, 1, cursor, next, &changes).expect("build"));
                cursor = next;
            }
        }
    }

    // 验收比对的是**条目集合**，不是段字节（06 §2.5：HNSW / LSM 依赖插入顺序）。
    // 也必须按主键去重：段是按 LSN 区间切的，同一 `(object_id, version_id)` 出现在
    // 两个相邻段里是正常的——索引对键的语义是「最后一次动作生效」。
    assert_eq!(latest_per_key(&incremental), latest_per_key(&rebuilt));
}
