//! `Reclaimer`：六条件合取判定与空间回收（03 §8，04 §10）。
//!
//! 判定与执行分离（R1）：`judge` 只读，`reclaim` 才有副作用。
//! 顺序不可颠倒（R3）：先注销引用，再释放物理空间。

use super::{Kernel, ReclaimCandidate, ReclaimDecision, Reclaimer, ScanBudget};
use crate::error::{InternalCode, KernelError, KernelResult};
use crate::kernel::state::ExtentState;
use crate::types::*;
use crate::wal::{RecordType, WalBody, WalRecord};

impl Reclaimer for Kernel {
    fn scan(&self, budget: ScanBudget) -> KernelResult<Vec<ReclaimCandidate>> {
        let st = self.state.lock().unwrap();
        let mut out = Vec::new();
        for e in st.extents.values() {
            if out.len() >= budget.max_candidates {
                break;
            }
            if e.state == ExtentState::Freed {
                continue;
            }
            let all_obsolete = st
                .objects
                .values()
                .flat_map(|o| o.versions.iter())
                .filter(|v| v.extent == Some(e.extent))
                .all(|v| matches!(v.state, VersionState::Obsolete | VersionState::Aborted));
            let object_deleted = st
                .objects
                .values()
                .find(|o| o.versions.iter().any(|v| v.extent == Some(e.extent)))
                .map(|o| o.deleted_xid.is_some())
                .unwrap_or(false);
            if e.state == ExtentState::Unmapped || all_obsolete || object_deleted {
                out.push(ReclaimCandidate { extent: e.extent });
            }
        }
        Ok(out)
    }

    fn judge(&self, cand: &ReclaimCandidate) -> KernelResult<ReclaimDecision> {
        let st = self.state.lock().unwrap();
        let mut reasons = Vec::new();

        let Some(e) = st.extents.get(&cand.extent) else {
            return Ok(ReclaimDecision {
                reclaimable: false,
                reasons: vec!["extent missing".into()],
            });
        };

        // (1) 所有引用该 extent 的版本都已 Obsolete / Aborted。
        let versions: Vec<&super::VersionEntry> = st
            .objects
            .values()
            .flat_map(|o| o.versions.iter())
            .filter(|v| v.extent == Some(cand.extent))
            .collect();
        let c1 = versions
            .iter()
            .all(|v| matches!(v.state, VersionState::Obsolete | VersionState::Aborted));
        if !c1 {
            reasons.push("condition 1: live version still references extent".into());
        }

        // (2) 没有活动快照引用。MVP 保守处理：任何活动事务都阻止回收。
        let c2 = self.active.lock().unwrap().is_empty();
        if !c2 {
            reasons.push("condition 2: active snapshot may reference extent".into());
        }

        // (3) 没有活动 epoch 引用该映射。MVP 无 epoch，视为满足。
        let c3 = true;

        // (4) 不在任何 checkpoint 的读取集合中。MVP：checkpoint 已覆盖即满足。
        let c4 = e.alloc_lsn.0 <= st.checkpoint_lsn.0;
        if !c4 {
            reasons.push("condition 4: extent not covered by a published checkpoint".into());
        }

        // (5) 不在未完成迁移的 source/target 集合中。MVP 无迁移。
        let c5 = true;

        // (6) WAL 侧无恢复依赖：ExtentAlloc 已被 checkpoint 覆盖。
        let c6 = e.alloc_lsn.0 <= st.checkpoint_lsn.0;
        if !c6 {
            reasons.push("condition 6: WAL still holds recovery dependency".into());
        }

        Ok(ReclaimDecision {
            reclaimable: c1 && c2 && c3 && c4 && c5 && c6,
            reasons,
        })
    }

    fn reclaim(&self, cand: &ReclaimCandidate) -> KernelResult<()> {
        let decision = self.judge(cand)?;
        if !decision.reclaimable {
            return Err(KernelError::internal(
                InternalCode::InvariantViolation,
                format!("reclaim judgement failed: {:?}", decision.reasons),
            ));
        }
        let _writer = self.writer.lock().unwrap();

        // 先注销引用：从对象版本链、映射表、FSM 中移除。
        let loc = {
            let mut st = self.state.lock().unwrap();
            let Some(e) = st.extents.get(&cand.extent) else {
                return Ok(());
            };
            let pe = e.physical;
            let loc = pe
                .and_then(|pe| st.physical.get(&pe))
                .map(|p| PhysicalLocation {
                    device: p.device,
                    offset: p.offset,
                    length: p.length,
                });
            for o in st.objects.values_mut() {
                o.versions.retain(|v| v.extent != Some(cand.extent));
            }
            st.mappings.remove(&cand.extent);
            st.extents.remove(&cand.extent);
            if let Some(pe) = pe {
                st.physical.remove(&pe);
            }
            loc
        };

        // ExtentFree 是系统维护记录，xid = 0。
        let rec = WalRecord::new(
            RecordType::ExtentFree,
            TransactionId(0),
            Lsn(0),
            WalBody::ExtentFree {
                extent: cand.extent.0,
            }
            .encode(),
        );
        let lsn = self.wal.append(&rec)?;
        self.wal.flush_upto(lsn)?;

        // 再释放物理空间（R3）。Discard 不参与语义注入，site 保持 `Untagged`。
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
