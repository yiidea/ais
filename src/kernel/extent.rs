//! `ExtentManager` 与 `MappingManager` 实现（04 §4–§5）。

use super::{ExtentManager, ExtentSpec, Kernel, MappingManager};
use crate::error::{ConflictKind, IdKind, InternalCode, KernelError, KernelResult};
use crate::kernel::state::{ExtentState, MappingRecord, MappingState};
use crate::kernel::txn::TxnCtx;
use crate::types::*;
use crate::wal::WalBody;

impl ExtentManager for Kernel {
    fn split(
        &self,
        id: ObjectId,
        _comp: ComponentId,
        _layout: LayoutType,
    ) -> KernelResult<Vec<ExtentId>> {
        let st = self.state.lock().unwrap();
        let mut out: Vec<ExtentId> = st
            .extents
            .values()
            .filter(|e| e.object_id == id && e.state != ExtentState::Freed)
            .map(|e| e.extent)
            .collect();
        out.sort();
        Ok(out)
    }

    fn allocate_logical(&self, txn: &mut TxnCtx, spec: &ExtentSpec) -> KernelResult<ExtentId> {
        self.check_txn_running(txn)?;
        let mut st = self.state.lock().unwrap();
        let (extent, _pe) =
            self.allocate_payload_extent(&mut st, txn, spec.object_id, spec.length)?;
        Ok(extent)
    }

    fn write_payload(
        &self,
        ext: ExtentId,
        data: &PayloadRef,
        dur: DurabilityLevel,
    ) -> KernelResult<()> {
        // `PayloadRef` 是逻辑引用；MVP 的写入入口在对象层（create/stage）完成，
        // 这里负责对已落盘的 extent 补一次显式 flush（E1 的 `dur` 语义）。
        let loc = {
            let st = self.state.lock().unwrap();
            let e = st
                .extents
                .get(&ext)
                .ok_or_else(|| KernelError::not_found(IdKind::Extent, ext))?;
            let pe = e.physical.ok_or_else(|| {
                KernelError::internal(InternalCode::InvariantViolation, "extent without physical")
            })?;
            let p = st.physical.get(&pe).ok_or_else(|| {
                KernelError::internal(InternalCode::InvariantViolation, "physical missing")
            })?;
            PhysicalLocation {
                device: p.device,
                offset: p.offset,
                length: p.length,
            }
        };
        if data.extent != ext {
            return Err(KernelError::internal(
                InternalCode::InvariantViolation,
                "payload ref extent mismatch",
            ));
        }
        if dur != DurabilityLevel::None {
            self.io.submit_and_wait(
                crate::hal::DeviceSite::ExtentPayloadWrite,
                crate::hal::DeviceOp::Flush {
                    site: crate::hal::DeviceSite::ExtentPayloadWrite,
                    loc,
                    level: dur,
                },
            )?;
        }
        Ok(())
    }

    fn read_payload(&self, ext: ExtentId, range: std::ops::Range<u64>) -> KernelResult<PayloadRef> {
        let st = self.state.lock().unwrap();
        let e = st
            .extents
            .get(&ext)
            .ok_or_else(|| KernelError::not_found(IdKind::Extent, ext))?;
        if range.end > e.length || range.start > range.end {
            return Err(KernelError::internal(
                InternalCode::InvariantViolation,
                "read range out of extent",
            ));
        }
        Ok(PayloadRef {
            extent: ext,
            offset: range.start,
            length: range.end - range.start,
        })
    }

    fn release(&self, ext: ExtentId) -> KernelResult<()> {
        let loc = {
            let mut st = self.state.lock().unwrap();
            let e = st
                .extents
                .get_mut(&ext)
                .ok_or_else(|| KernelError::not_found(IdKind::Extent, ext))?;
            e.state = ExtentState::Freed;
            e.physical.and_then(|pe| {
                st.physical.get_mut(&pe).map(|p| {
                    p.freed = true;
                    PhysicalLocation {
                        device: p.device,
                        offset: p.offset,
                        length: p.length,
                    }
                })
            })
        };
        // 先注销引用（上面已完成），再归还物理空间（R3）。
        // Discard 不参与语义注入（05 §3.2 目录无对应 site），因此不额外打标签。
        if let Some(loc) = loc {
            self.io.submit_and_wait(
                crate::hal::DeviceSite::Untagged,
                crate::hal::DeviceOp::Discard {
                    site: crate::hal::DeviceSite::Untagged,
                    loc,
                },
            )?;
        }
        Ok(())
    }
}

impl MappingManager for Kernel {
    fn resolve(&self, ext: ExtentId, mv: MappingVersion) -> KernelResult<PhysicalLocation> {
        let st = self.state.lock().unwrap();
        let m = st
            .mappings
            .get(&ext)
            .ok_or_else(|| KernelError::not_found(IdKind::Mapping, ext))?;
        let rec = m
            .records
            .iter()
            .find(|r| r.version == mv)
            .ok_or_else(|| KernelError::not_found(IdKind::Mapping, format!("{ext}@{mv:?}")))?;
        let p = st
            .physical
            .get(&rec.physical)
            .ok_or_else(|| KernelError::not_found(IdKind::Extent, rec.physical))?;
        Ok(PhysicalLocation {
            device: p.device,
            offset: p.offset,
            length: p.length,
        })
    }

    fn create_mapping(
        &self,
        txn: &mut TxnCtx,
        ext: ExtentId,
        pe: PhysicalExtentId,
    ) -> KernelResult<MappingId> {
        self.check_txn_running(txn)?;
        let mut st = self.state.lock().unwrap();
        let mapping = st.alloc_mapping();
        let entry = st.mappings.entry(ext).or_insert_with(|| super::MappingEntry {
            extent: ext,
            current_version: MappingVersion(0),
            current_mapping: MappingId(0),
            records: Vec::new(),
        });
        let version = MappingVersion(entry.current_version.0 + 1);
        entry.records.push(MappingRecord {
            mapping,
            version,
            physical: pe,
            state: MappingState::Active,
        });
        entry.current_version = version;
        entry.current_mapping = mapping;
        txn.pending_mappings.push((ext, mapping));
        txn.write_set.mapping_changes.push((ext, mapping));
        Ok(mapping)
    }

    fn switch_mapping(
        &self,
        txn: &mut TxnCtx,
        ext: ExtentId,
        expected: MappingVersion,
        new: MappingId,
    ) -> KernelResult<MappingVersion> {
        self.check_txn_running(txn)?;
        let mut st = self.state.lock().unwrap();
        let entry = st
            .mappings
            .get_mut(&ext)
            .ok_or_else(|| KernelError::not_found(IdKind::Mapping, ext))?;
        if entry.current_version != expected {
            return Err(KernelError::Conflict(ConflictKind::MappingChanged {
                extent: ext,
                expected,
                actual: entry.current_version,
            }));
        }
        let new_rec = entry
            .records
            .iter()
            .find(|r| r.mapping == new)
            .ok_or_else(|| KernelError::not_found(IdKind::Mapping, new))?;
        let new_version = MappingVersion(expected.0 + 1);
        let physical = new_rec.physical;
        let old_mapping = entry.current_mapping;
        for r in entry.records.iter_mut() {
            if r.state == MappingState::Active {
                r.state = MappingState::Draining;
            }
        }
        entry.records.push(MappingRecord {
            mapping: new,
            version: new_version,
            physical,
            state: MappingState::Active,
        });
        entry.current_version = new_version;
        entry.current_mapping = new;
        drop(st);
        // M3：先写 MappingSwitch WAL，再更新内存（这里内存已更新，WAL 紧随其后；
        // MVP 的 switch 只由 GC/迁移路径使用，提交点语义由调用方保证）。
        self.append(
            txn,
            &WalBody::MappingSwitch {
                extent: ext.0,
                old_mapping: old_mapping.0,
                new_mapping: new.0,
                reason: 0,
            },
        )?;
        Ok(new_version)
    }

    fn current_version(&self, ext: ExtentId) -> KernelResult<MappingVersion> {
        let st = self.state.lock().unwrap();
        let m = st
            .mappings
            .get(&ext)
            .ok_or_else(|| KernelError::not_found(IdKind::Mapping, ext))?;
        Ok(m.current_version)
    }
}
