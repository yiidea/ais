//! 错误模型（04 §2.2–§2.4，F2 冻结清单）。

use crate::types::{DeviceId, ExtentId, MappingVersion, ObjectId, VersionId};

/// 资源类别（与 02-kernel §3.6 对应）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ResourceType {
    Dram,
    Cxl,
    GpuHbm,
    Nvme,
    Hdd,
    Internal,
}
use std::fmt;

pub type KernelResult<T> = Result<T, KernelError>;

/// 被查找对象的类别。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum IdKind {
    Object,
    Version,
    Extent,
    Mapping,
    Component,
    Index,
    Checkpoint,
}

/// 冲突类别。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ConflictKind {
    /// 与并发写事务冲突（Wait-Die 的 Die 分支）。
    WriteConflict {
        object: ObjectId,
        expected: VersionId,
        actual: VersionId,
    },
    /// 映射代次已被他人切换（迁移放弃的唯一原因）。
    MappingChanged {
        extent: ExtentId,
        expected: MappingVersion,
        actual: MappingVersion,
    },
    /// 语义幂等冲突：同一 fingerprint 对应不同内容。
    IntentConflict { object: ObjectId },
}

/// 可重试原因。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RetryReason {
    DeviceBusy,
    Backpressure,
    ResourceBusy,
}

/// 损坏类别。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CorruptionKind {
    WalRecordCrc,
    CheckpointManifest,
    ChecksumMismatch,
    VersionChainBroken,
    DirectoryDangling,
    /// 索引段：长度不足，读不到完整段头或完整条目。
    SegmentTruncated,
    /// 索引段：段头识别字不符——这不是本引擎写的段。
    SegmentBadMagic,
    /// 索引段：格式版本不认识。
    SegmentBadVersion {
        found: u16,
    },
    /// 索引段：整段摘要不符（06 §5.1 的「段文件校验失败」）。
    SegmentContentCrc,
    /// 索引段：某一条的 CRC 不符，能定位到具体条目。
    SegmentEntryCrc,
    /// 索引段：条目形态字节不在 `{0, 1}` 内。
    SegmentBadEntryKind {
        found: u8,
    },
    /// 索引段：`entry_count` 之外的剩余字节。容忍它等于接受来源不明的数据。
    SegmentTrailingBytes,
}

/// 内部错误码：出现即内核 bug。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum InternalCode {
    InvariantViolation,
    StateMachineViolation,
    Unreachable,
}

/// I/O 操作类别（故障注入按此定位）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum IoOp {
    Read,
    Write,
    Flush,
    Fsync,
    Rename,
    Allocate,
    Discard,
}

/// 顶层错误类型（`#[non_exhaustive]` 是有意为之，04 §2.2）。
#[derive(Debug)]
#[non_exhaustive]
pub enum KernelError {
    NotFound {
        kind: IdKind,
        id: String,
    },
    AlreadyExists {
        kind: IdKind,
        id: String,
    },
    Conflict(ConflictKind),
    Retryable(RetryReason),
    Corrupted(CorruptionKind),
    ResourceExhausted {
        resource: ResourceType,
    },
    Unsupported {
        feature: &'static str,
    },
    /// 调用参数自相矛盾，且**没有写入任何东西**（ADR-0013 决策 1：记录族与记录类型
    /// 不匹配、非事务通道收到事务族记录等）。与 `Conflict`（并发导致的失败）不同，
    /// 重试同一个调用只会得到同一个结果。
    InvalidArgument {
        detail: String,
    },
    Io {
        op: IoOp,
        source: std::io::Error,
    },
    Internal {
        code: InternalCode,
        detail: String,
    },
}

impl KernelError {
    pub fn not_found(kind: IdKind, id: impl fmt::Display) -> Self {
        KernelError::NotFound {
            kind,
            id: id.to_string(),
        }
    }

    pub fn internal(code: InternalCode, detail: impl Into<String>) -> Self {
        KernelError::Internal {
            code,
            detail: detail.into(),
        }
    }

    pub fn corrupted(kind: CorruptionKind) -> Self {
        KernelError::Corrupted(kind)
    }

    pub fn io(op: IoOp, source: std::io::Error) -> Self {
        KernelError::Io { op, source }
    }

    pub fn invalid(detail: impl Into<String>) -> Self {
        KernelError::InvalidArgument {
            detail: detail.into(),
        }
    }

    /// 04 §2.3：调用方是否可以自动重试。
    ///
    /// `InvalidArgument` 不在其列：参数错与并发无关，重试同一调用只会得到同一结果。
    pub fn is_retryable(&self) -> bool {
        match self {
            KernelError::Retryable(_) => true,
            KernelError::Io { op, .. } => {
                matches!(op, IoOp::Read | IoOp::Write | IoOp::Flush | IoOp::Fsync)
            }
            _ => false,
        }
    }

    /// 04 §2.4 C4：`Internal` 必须在公共 API 边界变成 panic/abort。
    pub fn must_abort(&self) -> bool {
        matches!(self, KernelError::Internal { .. })
    }
}

impl fmt::Display for KernelError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            KernelError::NotFound { kind, id } => write!(f, "not found: {kind:?} {id}"),
            KernelError::AlreadyExists { kind, id } => write!(f, "already exists: {kind:?} {id}"),
            KernelError::Conflict(c) => write!(f, "conflict: {c:?}"),
            KernelError::Retryable(r) => write!(f, "retryable: {r:?}"),
            KernelError::Corrupted(c) => write!(f, "corrupted: {c:?}"),
            KernelError::ResourceExhausted { resource } => {
                write!(f, "resource exhausted: {resource:?}")
            }
            KernelError::Unsupported { feature } => write!(f, "unsupported: {feature}"),
            KernelError::InvalidArgument { detail } => write!(f, "invalid argument: {detail}"),
            KernelError::Io { op, source } => write!(f, "io error during {op:?}: {source}"),
            KernelError::Internal { code, detail } => {
                write!(f, "internal error {code:?}: {detail}")
            }
        }
    }
}

impl std::error::Error for KernelError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            KernelError::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

impl From<std::io::Error> for KernelError {
    fn from(source: std::io::Error) -> Self {
        KernelError::io(IoOp::Read, source)
    }
}

/// 04 §2.4 C4：公共 API 边界把 `Internal` 升级为 panic。
pub fn boundary<T>(r: KernelResult<T>) -> T {
    match r {
        Ok(v) => v,
        Err(e) if e.must_abort() => panic!("kernel invariant violated: {e}"),
        Err(e) => panic!("kernel error at public boundary: {e}"),
    }
}

// 便于在测试里构造物理位置相关的诊断，避免 DeviceId 未使用告警。
#[allow(dead_code)]
pub(crate) fn device_label(d: DeviceId) -> String {
    d.to_string()
}
