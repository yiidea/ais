//! 平台适配：核对设备描述与部署声明，并在后端能力不足时**拒绝启动**。
//!
//! 解决的是 07 §1.2 / §5 里最容易被静默放过的一类问题：内核按一个假设在跑，
//! 而设备按另一套参数在跑，两边都不报错，只有 SLO 慢慢变差，或者在一次掉电后
//! 才发现「以为已持久的数据其实只到了页缓存」。
//!
//! 三条原则：
//!
//! 1. **声明与事实分离**：`Deployment` 是部署方声明的（链路条数、是否 SPDK、
//!    持久内存窗口），`DeviceDescriptor` 是设备自报的。两者之间能被核对的部分，
//!    在这里核对一次，而不是在每次 I/O 上重新推断。
//! 2. **能力不足 = 启动失败**：需要 `Durable` 而设备最高只能到 `Write`，不是
//!    「降级运行」，是「写了会丢」。这类错误必须在打开内核之前暴露。
//! 3. **不读时钟、不探测硬件**：`Deployment` 是输入，不是探测结果（内核不直接
//!    读时钟、不主动探测健康，见 04 §2.6 C5、F18）。

use crate::error::{IoOp, KernelError, KernelResult};
use crate::hal::DeviceDescriptor;
use crate::types::{DegradedCapability, DeviceHealth, DurabilityLevel};

/// 部署方声明的平台能力（07 §1.2 的域配置中，需要参与启动核对的那一部分）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Deployment {
    /// 是否启用 SPDK 直通后端。关闭时 `FileDevice` 的 4096 对齐只能保证「按页写」，
    /// 不能保证绕过页缓存（07 §5.1）。
    pub spdk: bool,
    /// 到持久层的链路条数；`None` = 未声明（容器 / VM）。
    pub fabric_lanes: Option<u16>,
    /// 持久内存（CXL / pmem）窗口的可用区间；`None` = 没有。
    pub persistent_window: Option<(u64, u64)>,
}

/// 启动核对的结果：可以继续，但需要在运维上显式记录的偏差。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PlatformReport {
    /// 启动不失败、但需要运维注意的偏差（例如设备已降级）。
    pub notes: Vec<String>,
}

impl PlatformReport {
    pub fn is_clean(&self) -> bool {
        self.notes.is_empty()
    }
}

impl Deployment {
    /// 核对一台设备的描述，返回它真正能提供的耐久等级。
    ///
    /// 失败情形（全部在打开内核之前）：
    /// - 需要持久化而设备连 `Write` 都给不了（`None`）→ 该设备不能承载数据；
    /// - 设备已 `Failed` —— 它不应进入任何分配器的候选集；
    /// - 设备自报 `Durable` 却伴随 `DurabilityReduced` 或 `Failed` 这类矛盾状态
    ///   （F18 的一致性要求），说明后端上报有 bug，按不变量违反处理。
    pub fn check_device(
        &self,
        desc: &DeviceDescriptor,
        health: DeviceHealth,
        max_durability: DurabilityLevel,
    ) -> KernelResult<PlatformReport> {
        if !health.is_consistent_with(max_durability) {
            return Err(KernelError::internal(
                crate::error::InternalCode::InvariantViolation,
                format!(
                    "device {} reports {health:?} but max_durability {max_durability:?}",
                    desc.name
                ),
            ));
        }
        match health {
            DeviceHealth::Failed => {
                return Err(KernelError::io(
                    IoOp::Allocate,
                    std::io::Error::other(format!(
                        "device {} is Failed and cannot take data",
                        desc.name
                    )),
                ))
            }
            DeviceHealth::Degraded(DegradedCapability::DurabilityReduced) => {
                // 能进来就说明 max_durability < Durable（一致性已核对）。
            }
            _ => {}
        }
        if max_durability == DurabilityLevel::None {
            return Err(KernelError::Unsupported {
                feature: "device cannot provide any durability level; refusing to hold WAL or data",
            });
        }

        let mut report = PlatformReport::default();
        // 对齐：WAL 帧与 extent 都按 8 字节对齐写，但直接 I/O 需要按设备对齐单位写。
        // 未启用 SPDK 时 4096 对齐只是「友好」，不是硬要求。
        if self.spdk && desc.alignment != 4096 && desc.alignment != 512 {
            report.notes.push(format!(
                "device {} reports alignment {}; SPDK path expects 512/4096",
                desc.name, desc.alignment
            ));
        }
        if max_durability == DurabilityLevel::Write {
            report.notes.push(format!(
                "device {} cannot fsync (Write only): WAL durability is degraded",
                desc.name
            ));
        }
        if !desc.supports_discard {
            report.notes.push(format!(
                "device {} does not support discard: reclaimed extents leak space",
                desc.name
            ));
        }
        Ok(report)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::DeviceId;

    fn desc() -> DeviceDescriptor {
        DeviceDescriptor {
            id: DeviceId(1),
            name: "nvme0".into(),
            capacity: 1 << 30,
            alignment: 4096,
            supports_discard: true,
        }
    }

    #[test]
    fn healthy_durable_device_passes_clean() {
        let r = Deployment::default()
            .check_device(&desc(), DeviceHealth::Healthy, DurabilityLevel::Durable)
            .expect("ok");
        assert!(r.is_clean());
    }

    #[test]
    fn write_only_device_is_allowed_but_reported() {
        // 设备自报 Healthy 却给不出 Durable 是矛盾状态（F18），必须先被一致性核对拦下。
        let contradictory = Deployment::default()
            .check_device(&desc(), DeviceHealth::Healthy, DurabilityLevel::Write)
            .expect_err("Healthy claims Durable");
        assert!(matches!(contradictory, KernelError::Internal { .. }));

        // 诚实上报 Write 的设备可以承载数据，但 WAL 耐久性降级必须写进报告。
        let r = Deployment::default()
            .check_device(
                &desc(),
                DeviceHealth::Degraded(DegradedCapability::DurabilityReduced),
                DurabilityLevel::Write,
            )
            .expect("ok");
        assert!(r.notes.iter().any(|n| n.contains("cannot fsync")));
    }

    #[test]
    fn failed_or_none_durability_targets_are_rejected() {
        let mut d = desc();
        d.name = "bad0".into();
        let err = Deployment::default()
            .check_device(&d, DeviceHealth::Failed, DurabilityLevel::Write)
            .expect_err("Failed device");
        assert!(matches!(err, KernelError::Io { .. }));

        let err = Deployment::default()
            .check_device(
                &d,
                DeviceHealth::Degraded(DegradedCapability::DurabilityReduced),
                DurabilityLevel::None,
            )
            .expect_err("no durability");
        assert!(matches!(err, KernelError::Unsupported { .. }));
    }

    #[test]
    fn contradictory_health_is_an_invariant_violation() {
        let err = Deployment::default()
            .check_device(&desc(), DeviceHealth::Healthy, DurabilityLevel::Write)
            .expect_err("Healthy claims Durable");
        assert!(err.must_abort() || matches!(err, KernelError::Internal { .. }));
    }
}
