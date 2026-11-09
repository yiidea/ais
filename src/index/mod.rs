//! 索引层（[08]）：内核**之外**的调用方。
//!
//! 索引是派生数据（06 §1.2）：删掉全部索引段、从对象目录全量重建，结果必须与删除前
//! 等价。这决定了它与内核的关系只能是调用方——不能是内核的一部分，也不能反向持有
//! 内核的内部状态。本模块只用内核已冻结的三条边：
//!
//! - [`crate::kernel::ChangeFeed`]（F16）：读已提交对象变更，游标分页 + 未决事务屏障；
//! - [`crate::wal::WalManager::append_derived`]：写 `IndexDelta` / `IndexManifest`
//!   的两条窄口（ADR-0013）；
//! - [`crate::kernel::LiveSnapshotSource`]（F17）：存活快照与唯一的可见性实现。
//!
//! 五个部件（08 §2）：
//!
//! | 部件 | 本模块 | 崩溃后 |
//! |---|---|---|
//! | CatchUp（追赶器） | [`catchup`] | 无状态，从协调区水位重新出发 |
//! | IndexKind（索引种类） | [`kind`] | 无状态，纯函数 |
//! | SegmentBuilder（段构建器） | [`builder`] | 临时段是垃圾，启动时丢弃 |
//! | Coordinator（协调区） | [`manifest`] | 唯一权威，CAS 单写者 |
//! | Reclaimer（段回收器） | [`reclaim`] | 无状态，每轮重新判定 |
//!
//! 一条刻意保留的空隙：**段文件落在哪里，由调用方决定**。MVP 没有独立的索引段存储
//! 设备，`builder::seal` 只产出段字节与 [`manifest::SegmentId`]；落盘与原子 rename
//! 属于部署层（08 §六：索引层本身不在内核仓库内，这里是它的参考实现）。
//!
//! [08]: ../../docs/chatgpt/08-index-layer.md

pub mod builder;
pub mod catchup;
pub mod freshness;
pub mod kind;
pub mod manifest;
pub mod reclaim;
pub mod segment;
pub mod tracker;

pub use builder::{build_segment, SealError, SealedSegment};
pub use catchup::{step, CatchUpStep, RebuildRequired};
pub use freshness::{
    next_txn_watermark, ChangeRate, CostModel, FallbackCost, FallbackStrategy, IndexFreshness,
};
pub use kind::{EntryAction, EntryKey, EntryKind, IndexKind, MetaIndex, ObjectView, OwnedView};
pub use manifest::{Advance, CasConflict, PublishedSegment, SegmentId, SegmentManifest};
pub use reclaim::{reclaimable, Blocker, ReclaimVerdict};
pub use segment::{IndexEntry, Segment};
pub use tracker::{catch_up, CatchUpLimits, MaintenanceHost, MaintenanceReport};
