//! 文件设备：MVP 的持久后端。
//!
//! 语义与 `StorageDevice` 对齐：`write` 只写到页缓存；`flush(Durable)` 才 `sync_data`。
//! 这允许故障注入夹具区分「写到了」与「写到了并且活过了掉电」。

use super::{BoxFuture, DeviceDescriptor, StorageDevice};
use crate::error::{IoOp, KernelError, KernelResult};
use crate::types::{DeviceHealth, DurabilityLevel, PhysicalLocation};
use std::fs::{File, OpenOptions};
use std::os::unix::fs::FileExt;
use std::path::Path;
use std::sync::Mutex;

pub struct FileDevice {
    descriptor: DeviceDescriptor,
    file: Mutex<File>,
    max_durability: DurabilityLevel,
}

impl FileDevice {
    pub fn open(
        id: crate::types::DeviceId,
        path: impl AsRef<Path>,
        capacity: u64,
    ) -> std::io::Result<Self> {
        let path = path.as_ref();
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(path)?;
        file.set_len(capacity)?;
        Ok(FileDevice {
            descriptor: DeviceDescriptor {
                id,
                name: path.display().to_string(),
                capacity,
                alignment: 4096,
                supports_discard: true,
            },
            file: Mutex::new(file),
            max_durability: DurabilityLevel::Durable,
        })
    }

    fn check(&self, loc: &PhysicalLocation) -> KernelResult<()> {
        if loc.device != self.descriptor.id {
            return Err(KernelError::internal(
                crate::error::InternalCode::InvariantViolation,
                "device mismatch",
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
}

impl StorageDevice for FileDevice {
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
            let f = self.file.lock().unwrap();
            f.write_all_at(buf, loc.offset)
                .map_err(|e| KernelError::io(IoOp::Write, e))
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
            let f = self.file.lock().unwrap();
            f.read_exact_at(buf, loc.offset)
                .map_err(|e| KernelError::io(IoOp::Read, e))
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
                let f = self.file.lock().unwrap();
                f.sync_data().map_err(|e| KernelError::io(IoOp::Fsync, e))?;
            }
            Ok(())
        })
    }

    fn discard<'a>(&'a self, loc: &'a PhysicalLocation) -> BoxFuture<'a, KernelResult<()>> {
        Box::pin(async move {
            self.check(loc)?;
            let f = self.file.lock().unwrap();
            // MVP 用零填充模拟 TRIM；真实后端会调用 fallocate(PUNCH_HOLE)。
            let zeros = vec![0u8; loc.length as usize];
            f.write_all_at(&zeros, loc.offset)
                .map_err(|e| KernelError::io(IoOp::Discard, e))
        })
    }

    fn max_durability(&self) -> DurabilityLevel {
        self.max_durability
    }

    fn health(&self) -> DeviceHealth {
        DeviceHealth::Healthy
    }
}
