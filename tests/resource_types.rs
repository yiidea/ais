//! F18–F21 冻结资源类型的行为测试（ADR-0016）。
//!
//! 这些类型是「被引用但从未定义」的四个洞：`DeviceHealth` / `ResourceStats` /
//! `AccessPattern` / `FaultDomainId`。本文件验证它们的**不可变语义**，
//! 而不是字段布局：
//! - 降级必须可枚举，且与 `max_durability()` 一致；
//! - `available` 是导出值，不允许出现三字段互相矛盾；
//! - 未声明故障域时所有设备同域（保守，不假装有冗余）。

use ains::error::ResourceType;
use ains::hal::mem::MemDevice;
use ains::hal::StorageDevice;
use ains::types::{
    AccessPattern, DegradedCapability, DeviceHealth, DurabilityLevel, FaultDomainId, ResourceStats,
};

#[test]
fn healthy_device_must_report_full_durability() {
    let dev = MemDevice::new(ains::types::DeviceId(1), 1 << 20);
    let health = <MemDevice as StorageDevice>::health(&dev);
    assert_eq!(health, DeviceHealth::Healthy);
    assert!(health.is_consistent_with(dev.max_durability()));
}

#[test]
fn durability_degradation_is_consistent_with_max_durability() {
    let mut dev = MemDevice::new(ains::types::DeviceId(1), 1 << 20);
    dev.set_max_durability(DurabilityLevel::Write);

    let health = <MemDevice as StorageDevice>::health(&dev);
    assert_eq!(
        health,
        DeviceHealth::Degraded(DegradedCapability::DurabilityReduced),
        "degradation must name what was lost, not carry a free-form reason"
    );
    assert!(health.is_consistent_with(dev.max_durability()));
    // 反方向：声称 Durable 却报告降级同样是矛盾状态。
    assert!(!health.is_consistent_with(DurabilityLevel::Durable));
}

#[test]
fn degraded_for_non_durability_reasons_is_allowed_at_full_durability() {
    let health = DeviceHealth::Degraded(DegradedCapability::QueueDepthReduced);
    assert!(health.is_consistent_with(DurabilityLevel::Durable));
}

#[test]
fn resource_stats_derive_available_instead_of_storing_it() {
    let stats = ResourceStats {
        capacity: 1_000,
        used: 400,
        reserved: 100,
        resource_type: ResourceType::Nvme,
        numa_node: Some(1),
        health: DeviceHealth::Healthy,
        fault_domain: None,
    };
    assert_eq!(stats.available(), 500);
    // 记账错误（used + reserved > capacity）不得下溢成天文数字。
    let over = ResourceStats {
        used: 900,
        ..stats
    };
    assert_eq!(over.available(), 0);
}

#[test]
fn mem_device_stats_match_its_descriptor() {
    let dev = MemDevice::new(ains::types::DeviceId(7), 1 << 20);
    let stats = dev.stats();
    assert_eq!(stats.capacity, StorageDevice::descriptor(&dev).capacity);
    assert_eq!(stats.used, 0);
    assert_eq!(stats.resource_type, ResourceType::Nvme);
    assert!(stats.health.is_consistent_with(dev.max_durability()));
}

#[test]
fn undeclared_fault_domain_is_conservative() {
    assert_eq!(FaultDomainId::default(), FaultDomainId::UNDECLARED);
    assert_eq!(FaultDomainId::UNDECLARED, FaultDomainId(0));
    // 两个未声明的资源落在同一域——意味着「任何两份副本都不构成冗余」。
    let a = FaultDomainId::UNDECLARED;
    let b = FaultDomainId::UNDECLARED;
    assert_eq!(a, b);
    assert_ne!(a, FaultDomainId(1));
}

#[test]
fn access_pattern_is_a_closed_judgeable_set() {
    // 闭集：每个取值互相可区分、可哈希（放置器与队列选择按它查表）。
    let patterns = [
        AccessPattern::Sequential,
        AccessPattern::RandomPoint,
        AccessPattern::Hotspot,
        AccessPattern::WriteOnly,
        AccessPattern::Cold,
    ];
    let unique: std::collections::BTreeSet<u8> =
        patterns.iter().map(|p| discriminant(*p)).collect();
    assert_eq!(unique.len(), patterns.len());
}

fn discriminant(p: AccessPattern) -> u8 {
    match p {
        AccessPattern::Sequential => 0,
        AccessPattern::RandomPoint => 1,
        AccessPattern::Hotspot => 2,
        AccessPattern::WriteOnly => 3,
        AccessPattern::Cold => 4,
        _ => 255,
    }
}
