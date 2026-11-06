//! `ObjectManager` 实现（04 §3）。
//!
//! 写路径：先在状态锁内分配 extent 与版本槽位，再在锁外写 payload + data barrier，
//! 最后回到锁内挂上 Pending 版本。真正的 WAL 记录在 `TransactionManager::commit` 生成，
//! 保证「先持久 data，再持久 WAL commit」（T2）。

use super::{
    CreateObjectRequest, Kernel, ObjectManager, VersionSpec,
};
use crate::error::{IdKind, InternalCode, KernelError, KernelResult};
use crate::kernel::state::{ObjectEntry, VersionEntry};
use crate::kernel::txn::{Snapshot, TxnCtx};
use crate::types::*;

impl ObjectManager for Kernel {
    fn create_object(&self, txn: &mut TxnCtx, req: &CreateObjectRequest) -> KernelResult<ObjectId> {
        self.check_txn_running(txn)?;
        let _writer = self.writer.lock().unwrap();

        let (object_id, extent, physical) = {
            let mut st = self.state.lock().unwrap();
            let object_id = st.alloc_object_id(self.cfg.database_id);
            let (extent, physical) = self.allocate_payload_extent(
                &mut st,
                txn,
                object_id,
                req.payload.len() as u64,
            )?;
            (object_id, extent, physical)
        };

        // data barrier：payload 在 WAL commit 之前必须持久（T2.1）。
        self.write_payload_durable(physical, &req.payload)?;

        {
            let mut st = self.state.lock().unwrap();
            let version = VersionId(1);
            st.objects.insert(
                object_id,
                ObjectEntry {
                    object_type: req.object_type,
                    created_xid: txn.xid,
                    deleted_xid: None,
                    versions: vec![VersionEntry {
                        version,
                        xmin: txn.xid,
                        xmax: None,
                        state: VersionState::Pending,
                        extent: Some(extent),
                        created_lsn: Lsn(0),
                    }],
                },
            );
            if let Some(e) = st.extents.get_mut(&extent) {
                e.state = crate::kernel::state::ExtentState::Mapped;
            }
            txn.pending_creates.push((object_id, req.object_type));
            txn.pending_versions.push((object_id, version));
            txn.write_set.version_writes.push((object_id, version));
        }
        Ok(object_id)
    }

    fn get_header(&self, id: ObjectId) -> KernelResult<ObjectHeader> {
        let st = self.state.lock().unwrap();
        let o = st
            .objects
            .get(&id)
            .ok_or_else(|| KernelError::not_found(IdKind::Object, id))?;
        Ok(ObjectHeader {
            id,
            object_type: o.object_type,
            latest_version: o.latest_version(),
            created_xid: o.created_xid,
            deleted_xid: o.deleted_xid,
        })
    }

    fn resolve_version(&self, id: ObjectId, snap: &Snapshot) -> KernelResult<VersionId> {
        let st = self.state.lock().unwrap();
        let o = st
            .objects
            .get(&id)
            .ok_or_else(|| KernelError::not_found(IdKind::Object, id))?;
        // 删除也受快照可见性约束：删除事务在快照之后提交则仍然可见。
        if let Some(del) = o.deleted_xid {
            if crate::kernel::txn::committed_in_snapshot(del, snap, &st.txn_status) {
                return Err(KernelError::not_found(IdKind::Object, id));
            }
        }
        // 从新到旧找第一个可见的已发布版本。
        for v in o.versions.iter().rev() {
            if v.state != VersionState::Published {
                continue;
            }
            if crate::kernel::txn::version_visible(v.xmin, v.xmax, snap, &st.txn_status) {
                return Ok(v.version);
            }
        }
        Err(KernelError::not_found(IdKind::Object, id))
    }

    fn read_version(&self, id: ObjectId, ver: VersionId) -> KernelResult<ObjectView> {
        let st = self.state.lock().unwrap();
        let o = st
            .objects
            .get(&id)
            .ok_or_else(|| KernelError::not_found(IdKind::Object, id))?;
        let v = o
            .versions
            .iter()
            .find(|v| v.version == ver)
            .ok_or_else(|| KernelError::not_found(IdKind::Version, format!("{id}@{ver:?}")))?;
        Ok(ObjectView {
            header: ObjectHeader {
                id,
                object_type: o.object_type,
                latest_version: o.latest_version(),
                created_xid: o.created_xid,
                deleted_xid: o.deleted_xid,
            },
            version: ObjectVersion {
                object_id: id,
                version: v.version,
                xmin: v.xmin,
                xmax: v.xmax,
                state: v.state,
                extent: v.extent,
                metadata: Default::default(),
            },
        })
    }

    fn stage_version(
        &self,
        txn: &mut TxnCtx,
        id: ObjectId,
        spec: &VersionSpec,
    ) -> KernelResult<VersionId> {
        self.check_txn_running(txn)?;
        let _writer = self.writer.lock().unwrap();

        let (new_version, extent, physical) = {
            let mut st = self.state.lock().unwrap();
            let o = st
                .objects
                .get(&id)
                .ok_or_else(|| KernelError::not_found(IdKind::Object, id))?;
            if o.deleted_xid.is_some() {
                return Err(KernelError::not_found(IdKind::Object, id));
            }
            let old = o.latest_version();
            let new_version = VersionId(old.0 + 1);
            let (extent, physical) =
                self.allocate_payload_extent(&mut st, txn, id, spec.payload.len() as u64)?;
            (new_version, extent, physical)
        };

        self.write_payload_durable(physical, &spec.payload)?;

        {
            let mut st = self.state.lock().unwrap();
            let o = st
                .objects
                .get_mut(&id)
                .ok_or_else(|| KernelError::not_found(IdKind::Object, id))?;
            o.versions.push(VersionEntry {
                version: new_version,
                xmin: txn.xid,
                xmax: None,
                state: VersionState::Pending,
                extent: Some(extent),
                created_lsn: Lsn(0),
            });
            if let Some(e) = st.extents.get_mut(&extent) {
                e.state = crate::kernel::state::ExtentState::Mapped;
            }
            txn.pending_versions.push((id, new_version));
            txn.write_set.version_writes.push((id, new_version));
        }
        Ok(new_version)
    }

    fn delete_object(&self, txn: &mut TxnCtx, id: ObjectId) -> KernelResult<()> {
        self.check_txn_running(txn)?;
        let st = self.state.lock().unwrap();
        if !st.objects.contains_key(&id) {
            return Err(KernelError::not_found(IdKind::Object, id));
        }
        drop(st);
        if !txn.pending_deletes.contains(&id) {
            txn.pending_deletes.push(id);
        }
        Ok(())
    }
}

#[allow(dead_code)]
fn broken_chain() -> KernelError {
    KernelError::internal(InternalCode::InvariantViolation, "version chain broken")
}
