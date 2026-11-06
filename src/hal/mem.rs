//! 内存设备：区分「易失缓存」与「持久介质」，用于掉电语义测试（05 §2.1）。

use super::{BoxFuture, DeviceDescriptor, StorageDevice};
use crate::error::{IoOp, KernelError, KernelResult};
use crate::types::{DeviceHealth, DurabilityLevel, PhysicalLocation};
use std::collections::BTreeMap;
use std::sync::Mutex;

#[derive(Default)]
struct MemState {
    /// 已落到持久介质的数据（掉电后保留）。
    media: BTreeMap<u64, u8>,
    /// 设备易失缓存（掉电后丢失）。
    cache: BTreeMap<u64, u8>,
    /// 是否已经历掉电。
    power_lost: bool,
}

pub struct MemDevice {
    descriptor: DeviceDescriptor,
    state: Mutex<MemState>,
    max_durability: DurabilityLevel,
}

impl MemDevice {
    pub fn new(id: crate::types::DeviceId, capacity: u64) -> Self {
        MemDevice {
            descriptor: DeviceDescriptor {
                id,
                name: "mem".to_string(),
                capacity,
                alignment: 1,
                supports_discard: true,
            },
            state: Mutex::new(MemState::default()),
            max_durability: DurabilityLevel::Durable,
        }
    }

    fn check(&self, loc: &PhysicalLocation) -> KernelResult<()> {
        if loc.device != self.descriptor.id {
            return Err(KernelError::internal(
                crate::error::InternalCode::InvariantViolation,
                format!("device mismatch: {:?} vs {:?}", loc.device, self.descriptor.id),
            ));
        }
        if loc.offset.saturating_add(loc.length) > self.descriptor.capacity {
            return Err(KernelError::io(
                IoOp::Write,
                std::io::Error::new(std::io::ErrorKind::Other, "out of device capacity"),
            ));
        }
        Ok(())
    }

    /// 模拟掉电：清空易失缓存，保留持久介质。
    pub fn power_loss(&self) {
        let mut st = self.state.lock().unwrap();
        st.cache.clear();
        st.power_lost = true;
    }

    /// 模拟「进程崩溃但设备缓存仍在」（Write 级耐久）。
    pub fn process_crash(&self) {
        // 进程崩溃不影响设备侧缓存；这里保留 cache 与 media。
    }

    pub fn set_max_durability(&mut self, level: DurabilityLevel) {
        self.max_durability = level;
    }

    pub fn persistent_bytes(&self) -> usize {
        self.state.lock().unwrap().media.len()
    }

    pub fn cached_bytes(&self) -> usize {
        self.state.lock().unwrap().cache.len()
    }
}

impl StorageDevice for MemDevice {
    fn descriptor(&self) -> &DeviceDescriptor {
        &self.descriptor
    }

    fn write<'a>(
        &'a self,
        loc: &'a PhysicalLocation,
        buf: &'a [u8],
    ) -> BoxFuture<'a, KernelResult<()>> {
        Box::pin(async move {
            self.check(loc)?;
            if buf.len() as u64 != loc.length {
                return Err(KernelError::internal(
                    crate::error::InternalCode::InvariantViolation,
                    "write length mismatch",
                ));
            }
            let mut st = self.state.lock().unwrap();
            if st.power_lost {
                return Err(KernelError::io(
                    IoOp::Write,
                    std::io::Error::new(std::io::ErrorKind::Other, "device is powered off"),
                ));
            }
            for (i, b) in buf.iter().enumerate() {
                st.cache.insert(loc.offset + i as u64, *b);
            }
            Ok(())
        })
    }

    fn read<'a>(
        &'a self,
        loc: &'a PhysicalLocation,
        buf: &'a mut [u8],
    ) -> BoxFuture<'a, KernelResult<()>> {
        Box::pin(async move {
            self.check(loc)?;
            if buf.len() as u64 != loc.length {
                return Err(KernelError::internal(
                    crate::error::InternalCode::InvariantViolation,
                    "read length mismatch",
                ));
            }
            let st = self.state.lock().unwrap();
            for (i, slot) in buf.iter_mut().enumerate() {
                let off = loc.offset + i as u64;
                *slot = st
                    .cache
                    .get(&off)
                    .or_else(|| st.media.get(&off))
                    .copied()
                    .unwrap_or(0);
            }
            Ok(())
        })
    }

    fn flush<'a>(
        &'a self,
        loc: &'a PhysicalLocation,
        level: DurabilityLevel,
    ) -> BoxFuture<'a, KernelResult<()>> {
        Box::pin(async move {
            self.check(loc)?;
            if level > self.max_durability {
                return Err(KernelError::io(
                    IoOp::Flush,
                    std::io::Error::new(
                        std::io::ErrorKind::Other,
                        "requested durability exceeds device capability",
                    ),
                ));
            }
            if level == DurabilityLevel::Durable {
                // 持久化语义：把范围内**脏缓存**写到介质，其余偏移保持原样。
                // 不能对未命中缓存的偏移写 0——那会把此前已经持久化的段头/记录覆盖掉。
                // 从未写过的偏移在 read 时回退为 0，等价于"介质上的空洞"。
                let mut st = self.state.lock().unwrap();
                let end = loc.offset + loc.length;
                let dirty: Vec<(u64, u8)> = st
                    .cache
                    .range(loc.offset..end)
                    .map(|(k, v)| (*k, *v))
                    .collect();
                for (off, v) in dirty {
                    st.cache.remove(&off);
                    st.media.insert(off, v);
                }
            }
            Ok(())
        })
    }

    fn discard<'a>(&'a self, loc: &'a PhysicalLocation) -> BoxFuture<'a, KernelResult<()>> {
        Box::pin(async move {
            self.check(loc)?;
            let mut st = self.state.lock().unwrap();
            for off in loc.offset..loc.offset + loc.length {
                st.cache.remove(&off);
                st.media.remove(&off);
            }
            Ok(())
        })
    }

    fn max_durability(&self) -> DurabilityLevel {
        self.max_durability
    }

    fn health(&self) -> DeviceHealth {
        if self.max_durability == DurabilityLevel::None {
            DeviceHealth::Degraded {
                reason: "durability degraded to None".to_string(),
            }
        } else {
            DeviceHealth::Healthy
        }
    }
}
