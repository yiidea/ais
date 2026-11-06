//! 检查点、恢复与校验（03 §5–§6，04 §9）。
//!
//! MVP 的检查点用单个影子文件 + 原子 rename 发布；完整实现会拆成六个目录文件
//! （txn/object/version/extent/mapping/index），但发布语义相同：先写完整快照，
//! 再原子切换 superblock，最后推进 WAL 可截断水位。

use super::{
    CheckpointInfo, CheckpointManager, CheckpointId, Kernel, ReclaimPlan, RecoveredState,
    RecoveryManager, VerifyReport, VerifyScope,
};
use crate::error::{IdKind, KernelError, KernelResult};
use crate::kernel::state::{ExtentState, KernelState, MappingState};
use crate::types::*;
use crate::wal::{RecordType, WalBody, WalRecord};
use std::collections::BTreeSet;
use std::io::Write;

impl CheckpointManager for Kernel {
    fn trigger(&self) -> KernelResult<CheckpointId> {
        let _g = self.checkpoint_guard.lock().unwrap();
        let (id, lsn, snapshot) = {
            let mut st = self.state.lock().unwrap();
            st.checkpoint_lsn = self.wal.durable_lsn();
            st.checkpoint_id = CheckpointId(st.checkpoint_id.0 + 1);
            (st.checkpoint_id, st.checkpoint_lsn, st.encode())
        };

        // A4：追加 CheckpointBegin 并 flush（系统事务，xid = 0）。
        let rec = WalRecord::new(
            RecordType::CheckpointBegin,
            TransactionId(0),
            Lsn(0),
            WalBody::Maintenance {
                kind: RecordType::CheckpointBegin,
                extent: 0,
                old_mapping: 0,
                new_mapping: 0,
            }
            .encode(),
        );
        let begin_lsn = self.wal.append(&rec)?;
        self.wal.flush_upto(begin_lsn)?;

        // B5：影子文件 + 原子发布。
        write_atomic(&self.cfg.checkpoint_path, &snapshot)?;

        // C：推进 WAL 截断水位。
        self.wal.set_min_required_lsn(lsn);
        self.min_required
            .store(lsn.0, std::sync::atomic::Ordering::SeqCst);
        Ok(id)
    }

    fn current(&self) -> KernelResult<CheckpointInfo> {
        let st = self.state.lock().unwrap();
        Ok(CheckpointInfo {
            id: st.checkpoint_id,
            lsn: st.checkpoint_lsn,
        })
    }

    fn reclaim_candidates(&self) -> KernelResult<ReclaimPlan> {
        let st = self.state.lock().unwrap();
        let extents = st
            .extents
            .values()
            .filter(|e| {
                e.state != ExtentState::Freed
                    && e.alloc_lsn.0 <= st.checkpoint_lsn.0
                    && e.alloc_xid.0 != 0
                    && matches!(
                        st.txn_status.get(&e.alloc_xid),
                        Some(TxnStatus::Aborted)
                    )
            })
            .map(|e| e.extent)
            .collect();
        Ok(ReclaimPlan {
            extents,
            wal_before: self.wal.min_required_lsn(),
        })
    }
}

impl RecoveryManager for Kernel {
    fn recover(&self) -> KernelResult<RecoveredState> {
        let checkpoint = {
            let st = self.state.lock().unwrap();
            CheckpointInfo {
                id: st.checkpoint_id,
                lsn: st.checkpoint_lsn,
            }
        };
        let scan = self.wal.scan_from(checkpoint.lsn)?;
        let mut st = self.state.lock().unwrap();
        let replayed = scan.records.len();

        // Pass 1：重建事务状态。checkpoint 中的终态优先。
        for rec in &scan.records {
            if let Ok(body) = WalBody::decode(rec.record.record_type, &rec.record.body) {
                match body {
                    WalBody::TxnBegin { .. } => {
                        st.txn_status
                            .entry(rec.record.xid)
                            .or_insert(TxnStatus::Running);
                    }
                    WalBody::TxnCommit => {
                        st.txn_status
                            .insert(rec.record.xid, TxnStatus::Committed);
                    }
                    WalBody::TxnAbort => {
                        st.txn_status.insert(rec.record.xid, TxnStatus::Aborted);
                    }
                    _ => {}
                }
                st.xmax = st.xmax.max(rec.record.xid.0 + 1);
            }
        }

        // 扫描窗口内没有 Commit/Abort 的事务一律 Aborted（03 §6.1 step 5）。
        let aborted: Vec<TransactionId> = st
            .txn_status
            .iter()
            .filter(|(_, s)| **s == TxnStatus::Running)
            .map(|(x, _)| *x)
            .collect();
        for x in &aborted {
            st.txn_status.insert(*x, TxnStatus::Aborted);
        }

        // Pass 2：幂等 redo。
        for rec in &scan.records {
            let Ok(body) = WalBody::decode(rec.record.record_type, &rec.record.body) else {
                continue;
            };
            let committed = rec.record.xid.0 == 0
                || matches!(
                    st.txn_status.get(&rec.record.xid),
                    Some(TxnStatus::Committed)
                );
            apply_record(&mut st, rec.lsn, rec.record.xid, committed, &body);
        }

        // 未提交事务的物理影响：逻辑 extent 标记 Unmapped，交给 GC（03 §6.3）。
        let status: Vec<(TransactionId, TxnStatus)> =
            st.txn_status.iter().map(|(k, v)| (*k, *v)).collect();
        let committed = |xid: TransactionId| -> bool {
            matches!(
                status.iter().find(|(k, _)| *k == xid).map(|(_, v)| *v),
                Some(TxnStatus::Committed)
            )
        };
        for e in st.extents.values_mut() {
            if e.alloc_xid.0 != 0 && !committed(e.alloc_xid) && e.state != ExtentState::Freed {
                e.state = ExtentState::Unmapped;
            }
        }
        for o in st.objects.values_mut() {
            for v in o.versions.iter_mut() {
                if v.state == VersionState::Pending && !committed(v.xmin) {
                    v.state = VersionState::Aborted;
                }
            }
        }

        Ok(RecoveredState {
            checkpoint,
            replayed_records: replayed,
            aborted_txns: aborted,
        })
    }

    fn verify(&self, scope: VerifyScope) -> KernelResult<VerifyReport> {
        let st = self.state.lock().unwrap();
        let mut report = VerifyReport::default();

        let objects: Vec<ObjectId> = match scope {
            VerifyScope::Full => st.objects.keys().copied().collect(),
            VerifyScope::Object(id) => vec![id],
        };

        for id in objects {
            let Some(o) = st.objects.get(&id) else {
                report.issues.push(format!("object {id} missing"));
                continue;
            };
            let mut last = VersionId(0);
            for v in &o.versions {
                if v.version <= last {
                    report
                        .issues
                        .push(format!("object {id} version chain not monotonic"));
                }
                last = v.version;
                if let Some(ext) = v.extent {
                    match st.extents.get(&ext) {
                        None => report.issues.push(format!(
                            "object {id} version {} references missing extent {ext}",
                            v.version.0
                        )),
                        Some(e) if e.state == ExtentState::Freed => report.issues.push(format!(
                            "object {id} version {} references freed extent {ext}",
                            v.version.0
                        )),
                        Some(_) => {}
                    }
                }
            }
        }

        for (ext, e) in &st.extents {
            if e.state == ExtentState::Freed {
                continue;
            }
            if let Some(pe) = e.physical {
                if !st.physical.contains_key(&pe) {
                    report
                        .issues
                        .push(format!("extent {ext} references missing physical {pe}"));
                }
            } else {
                report.issues.push(format!("extent {ext} has no physical"));
            }
            match st.mappings.get(ext) {
                None => report.issues.push(format!("extent {ext} has no mapping")),
                Some(m) => {
                    let rec = m
                        .records
                        .iter()
                        .find(|r| r.version == m.current_version);
                    match rec {
                        None => report
                            .issues
                            .push(format!("extent {ext} current mapping version missing")),
                        Some(r) if !st.physical.contains_key(&r.physical) => report
                            .issues
                            .push(format!("extent {ext} mapping points to missing physical")),
                        Some(_) => {}
                    }
                }
            }
        }

        for (ext, m) in &st.mappings {
            if !st.extents.contains_key(ext) {
                report
                    .issues
                    .push(format!("mapping for missing extent {ext}"));
            }
            for r in &m.records {
                if r.state == MappingState::Active && r.version != m.current_version {
                    report.issues.push(format!(
                        "extent {ext} has multiple active mappings ({:?} vs {:?})",
                        r.version, m.current_version
                    ));
                }
            }
        }

        Ok(report)
    }
}

fn apply_record(
    st: &mut KernelState,
    lsn: Lsn,
    xid: TransactionId,
    committed: bool,
    body: &WalBody,
) {
    match body {
        WalBody::ExtentAlloc {
            extent,
            physical_extent,
            device_id,
            device_offset,
            length,
            ..
        } => {
            let pe = PhysicalExtentId(*physical_extent);
            st.physical.entry(pe).or_insert(super::PhysicalExtentEntry {
                id: pe,
                device: DeviceId(*device_id as u64),
                offset: *device_offset,
                length: *length,
                freed: false,
            });
            let ext = ExtentId(*extent);
            st.extents.entry(ext).or_insert(super::ExtentEntry {
                extent: ext,
                object_id: ObjectId::ZERO,
                physical: Some(pe),
                length: *length,
                alloc_xid: xid,
                alloc_lsn: lsn,
                state: ExtentState::Allocated,
            });
            let next = device_offset.saturating_add(*length);
            let d = DeviceId(*device_id as u64);
            let cur = st.device_next_offset.entry(d).or_insert(next);
            *cur = (*cur).max(next);
            st.next_extent = st.next_extent.max(*extent);
            st.next_physical = st.next_physical.max(*physical_extent);
        }
        WalBody::MappingCreate {
            extent,
            mapping_id,
            physical_extent,
        } => {
            let ext = ExtentId(*extent);
            let mid = MappingId(*mapping_id);
            let pe = PhysicalExtentId(*physical_extent);
            let entry = st.mappings.entry(ext).or_insert_with(|| super::MappingEntry {
                extent: ext,
                current_version: MappingVersion(0),
                current_mapping: MappingId(0),
                records: Vec::new(),
            });
            if !entry.records.iter().any(|r| r.mapping == mid) {
                let version = MappingVersion(entry.current_version.0 + 1);
                entry.records.push(super::MappingRecord {
                    mapping: mid,
                    version,
                    physical: pe,
                    state: MappingState::Active,
                });
                entry.current_version = version;
                entry.current_mapping = mid;
            }
            st.next_mapping = st.next_mapping.max(mid.0);
        }
        WalBody::ObjectCreate {
            object_id,
            object_type,
            ..
        } => {
            if committed {
                if let Some(ty) = ObjectType::from_u8(*object_type) {
                    st.objects.entry(*object_id).or_insert(super::ObjectEntry {
                        object_type: ty,
                        created_xid: xid,
                        deleted_xid: None,
                        versions: Vec::new(),
                    });
                }
            }
            st.next_oid_seq = st.next_oid_seq.max(object_id.sequence());
        }
        WalBody::ObjectVersion {
            object_id,
            new_version,
            old_version,
            extent,
        } => {
            if !committed {
                return;
            }
            let o = st.objects.entry(*object_id).or_insert_with(|| super::ObjectEntry {
                object_type: ObjectType::Document,
                created_xid: xid,
                deleted_xid: None,
                versions: Vec::new(),
            });
            if o.versions.iter().any(|v| v.version == *new_version) {
                return;
            }
            if old_version.0 != 0 {
                if let Some(prev) = o.versions.iter_mut().find(|v| v.version == *old_version) {
                    if prev.xmax.is_none() {
                        prev.xmax = Some(xid);
                    }
                    prev.state = VersionState::Obsolete;
                }
            }
            o.versions.push(super::VersionEntry {
                version: *new_version,
                xmin: xid,
                xmax: None,
                state: VersionState::Published,
                extent: none_if_zero(*extent),
                created_lsn: lsn,
            });
            o.versions.sort_by_key(|v| v.version.0);
        }
        WalBody::ObjectDelete { object_id, .. } => {
            if committed {
                if let Some(o) = st.objects.get_mut(object_id) {
                    o.deleted_xid = Some(xid);
                }
            }
        }
        WalBody::MappingSwitch {
            extent,
            new_mapping,
            ..
        } => {
            if !committed {
                return;
            }
            let ext = ExtentId(*extent);
            let target = MappingId(*new_mapping);
            if let Some(m) = st.mappings.get_mut(&ext) {
                let found = m
                    .records
                    .iter()
                    .find(|r| r.mapping == target)
                    .map(|r| (r.mapping, r.version));
                if let Some((mapping, version)) = found {
                    m.current_mapping = mapping;
                    m.current_version = version;
                    for x in m.records.iter_mut() {
                        if x.version != version && x.state == MappingState::Active {
                            x.state = MappingState::Draining;
                        }
                    }
                }
            }
        }
        WalBody::ExtentFree { extent } => {
            if let Some(e) = st.extents.get_mut(&ExtentId(*extent)) {
                e.state = ExtentState::Freed;
                if let Some(pe) = e.physical {
                    if let Some(p) = st.physical.get_mut(&pe) {
                        p.freed = true;
                    }
                }
            }
        }
        WalBody::TxnBegin { .. }
        | WalBody::TxnCommit
        | WalBody::TxnAbort
        | WalBody::IndexDelta { .. }
        | WalBody::IndexManifest { .. }
        | WalBody::Maintenance { .. } => {}
    }
}

fn none_if_zero(v: u64) -> Option<ExtentId> {
    if v == 0 {
        None
    } else {
        Some(ExtentId(v))
    }
}

fn write_atomic(path: &std::path::Path, bytes: &[u8]) -> KernelResult<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| KernelError::io(crate::error::IoOp::Write, e))?;
    }
    let tmp = path.with_extension("tmp");
    {
        let mut f = std::fs::File::create(&tmp)
            .map_err(|e| KernelError::io(crate::error::IoOp::Write, e))?;
        f.write_all(bytes)
            .map_err(|e| KernelError::io(crate::error::IoOp::Write, e))?;
        f.sync_all()
            .map_err(|e| KernelError::io(crate::error::IoOp::Fsync, e))?;
    }
    std::fs::rename(&tmp, path)
        .map_err(|e| KernelError::io(crate::error::IoOp::Rename, e))?;
    // 发布后 fsync 父目录，保证 rename 本身持久。
    if let Some(parent) = path.parent() {
        if let Ok(dir) = std::fs::File::open(parent) {
            let _ = dir.sync_all();
        }
    }
    Ok(())
}

#[allow(dead_code)]
fn dangling() -> KernelError {
    KernelError::corrupted(crate::error::CorruptionKind::DirectoryDangling)
}

#[allow(dead_code)]
fn missing(kind: IdKind, id: impl std::fmt::Display) -> KernelError {
    KernelError::not_found(kind, id)
}

#[allow(dead_code)]
fn unique<T: Ord>(v: Vec<T>) -> BTreeSet<T> {
    v.into_iter().collect()
}
