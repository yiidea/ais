//! AINS — AI-Native Storage Engine, MVP kernel.
//!
//! 本 crate 落地设计文档中已冻结的契约：
//!
//! - 标识符与错误模型：[`types`]、[`error`]（04 §2）
//! - 设备 HAL 与同步 façade：[`hal`]（04 §6）
//! - WAL 二进制格式与扫描：[`wal`]（03 §2）
//! - 内核模块：[`kernel`]（04 §3–§11）
//! - 启动期平台核对：[`platform`]（07 §1.2 / §5）
//! - 面向 AI 的对象/向量/意图 API：[`engine`]
//! - 故障注入夹具（feature = `test-harness`）：模块 `testing`（05）
//!
//! 设计边界（MVP）：
//! - 只支持快照隔离（04 §8 T5）；
//! - 单机单实例，不含分布式事务、CXL/RDMA、完整 GPU 后端（README §7）；
//! - 迁移与 MemoryTier 只保留 trait 形状，MVP 不启用。

pub mod checksum;
pub mod clock;
pub mod engine;
pub mod error;
pub mod hal;
pub mod kernel;
pub mod platform;
pub mod types;
pub mod wal;

#[cfg(feature = "test-harness")]
pub mod testing;

pub use error::{KernelError, KernelResult};
