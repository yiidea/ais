//! 启动期平台核对（07 §1.2 / §5，F18）的端到端用例。
//!
//! `Kernel::open` 只拿 `IoExecutor`，不给平台核对留插入点。核对因此是**打开内核之前**
//! 的一道调用方动作：失败就根本不 boot。这里验证的正是这条接线——先核对、再 open。

use ains::hal::mem::MemDevice;
use ains::hal::StorageDevice;
use ains::kernel::{
    CreateObjectRequest, Kernel, KernelConfig, ObjectManager, RecoveryManager, TransactionManager,
    VerifyScope,
};
use ains::platform::Deployment;
use ains::types::{
    DegradedCapability, DeviceHealth, DeviceId, DurabilityLevel, IsolationLevel, ObjectType,
};
use ains::KernelResult;

/// 部署侧统一入口：逐台核对，全部通过才消费设备去打开内核。
///
/// 设备只能核对通过后 move 进内核（`Box<dyn StorageDevice>` 交给 `open` 就要不回来了），
/// 核对失败时一个 `Kernel` 都不会被构造出来。
fn boot_guarded<const N: usize>(
    cfg: &KernelConfig,
    deployment: &Deployment,
    devices: [Box<dyn StorageDevice>; N],
) -> KernelResult<Vec<Kernel>> {
    for dev in &devices {
        deployment.check_device(dev.descriptor(), dev.health(), dev.max_durability())?;
    }
    devices
        .map(|dev| Kernel::open(cfg.clone(), dev).expect("open"))
        .into_iter()
        .map(Ok)
        .collect()
}

fn config(tag: &str) -> KernelConfig {
    let dir = std::env::temp_dir().join(format!("ains-platform-{tag}"));
    let _ = std::fs::remove_dir_all(&dir);
    KernelConfig::new(dir.join("ckpt.snap"))
}

#[test]
fn degrading_device_is_reported_with_device_and_capability() {
    let mut dev = MemDevice::new(DeviceId(1), 16 << 20);
    dev.set_max_durability(DurabilityLevel::Write);

    let report = Deployment::default()
        .check_device(dev.descriptor(), dev.health(), dev.max_durability())
        .expect("write-only device can still hold data");

    // 报告不是布尔：运维要能看出「哪台设备、缺哪种能力」。
    assert!(!report.is_clean());
    assert!(report.notes.iter().any(|n| n.contains("cannot fsync")));
}

#[test]
fn spdk_alignment_expectation_only_applies_when_spdk_is_enabled() {
    let dev = MemDevice::new(DeviceId(1), 16 << 20);
    // MemDevice 的 alignment 是 1：只要 SPDK 路径不开就不该被抱怨。
    let plain = Deployment::default()
        .check_device(dev.descriptor(), dev.health(), dev.max_durability())
        .expect("posix path tolerates unaligned devices");
    assert!(plain.is_clean());

    let strict = Deployment {
        spdk: true,
        ..Deployment::default()
    };
    let report = strict
        .check_device(dev.descriptor(), dev.health(), dev.max_durability())
        .expect("still openable");
    assert!(report
        .notes
        .iter()
        .any(|n| n.contains("SPDK path expects 512/4096")));
}

#[test]
fn a_failed_device_never_opens_the_kernel() {
    let mut dev = MemDevice::new(DeviceId(1), 16 << 20);
    dev.set_max_durability(DurabilityLevel::Write);
    dev.set_health(DeviceHealth::Failed);

    let err = match boot_guarded(&config("failed"), &Deployment::default(), [Box::new(dev)]) {
        Ok(_) => panic!("Failed device must not boot"),
        Err(e) => e,
    };
    assert!(matches!(err, ains::KernelError::Io { .. }));
}

#[test]
fn contradictory_device_reporting_is_an_invariant_violation() {
    // 设备说 Healthy，却给不出 Durable：两边至少有一边在撒谎（F18）。
    let mut dev = MemDevice::new(DeviceId(1), 16 << 20);
    dev.set_max_durability(DurabilityLevel::Write);
    dev.set_health(DeviceHealth::Healthy);

    let err = match boot_guarded(
        &config("contradictory"),
        &Deployment::default(),
        [Box::new(dev)],
    ) {
        Ok(_) => panic!("contradiction must abort"),
        Err(e) => e,
    };
    assert!(matches!(err, ains::KernelError::Internal { .. }));
    assert!(err.must_abort());
}

#[test]
fn an_approved_device_opens_and_carries_a_commit() {
    let dev = MemDevice::new(DeviceId(1), 16 << 20);
    let mut kernels = boot_guarded(&config("approved"), &Deployment::default(), [Box::new(dev)])
        .expect("healthy durable device boots");
    let kernel = kernels.remove(0);

    let mut txn = TransactionManager::begin(&kernel, IsolationLevel::Snapshot).unwrap();
    let id = ObjectManager::create_object(
        &kernel,
        &mut txn,
        &CreateObjectRequest {
            object_type: ObjectType::Document,
            payload: b"guarded boot".to_vec(),
            component_count: 1,
        },
    )
    .unwrap();
    TransactionManager::commit(&kernel, &mut txn).unwrap();

    assert!(ObjectManager::get_header(&kernel, id).is_ok());
    assert!(RecoveryManager::verify(&kernel, VerifyScope::Full)
        .unwrap()
        .ok());
}

#[test]
fn default_mem_health_follows_durability() {
    let mut dev = MemDevice::new(DeviceId(1), 1 << 20);
    assert_eq!(dev.health(), DeviceHealth::Healthy);
    dev.set_max_durability(DurabilityLevel::Write);
    assert_eq!(
        dev.health(),
        DeviceHealth::Degraded(DegradedCapability::DurabilityReduced)
    );

    // 显式覆盖优先，且 `None` 能恢复推导。
    dev.set_health(DeviceHealth::Failed);
    assert_eq!(dev.health(), DeviceHealth::Failed);
    dev.set_health(None);
    assert_eq!(
        dev.health(),
        DeviceHealth::Degraded(DegradedCapability::DurabilityReduced)
    );
}
