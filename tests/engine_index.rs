//! 引擎读路径的索引覆盖标记（06 §4.3）：查询结果必须说清「索引在这份快照上
//! 是否完整」，而不是把旧结果当最新结果静默返回。

use ains::engine::{Engine, IndexCoverage};
use ains::hal::mem::MemDevice;
use ains::hal::StorageDevice;
use ains::index::FallbackStrategy;
use ains::kernel::{KernelConfig, TransactionManager, TxnCtx};
use ains::types::*;
use std::collections::BTreeMap;

fn engine(tag: &str) -> Engine {
    let dir = std::env::temp_dir().join(format!("ains-engine-index-{tag}"));
    let _ = std::fs::remove_dir_all(&dir);
    Engine::open(
        KernelConfig::new(dir.join("ckpt.snap")),
        Box::new(MemDevice::new(DeviceId(1), 16 << 20)) as Box<dyn StorageDevice>,
    )
    .unwrap()
}

fn put(engine: &Engine, text: &str, embedding: Vec<f32>) -> ObjectId {
    engine
        .put_document(ObjectType::Document, text, embedding, BTreeMap::new())
        .unwrap()
}

#[test]
fn without_index_there_is_no_coverage_claim() {
    let e = engine("plain");
    put(&e, "a", vec![1.0, 0.0]);
    let r = e.search_with_coverage(&[1.0, 0.0], 3);
    assert_eq!(r.coverage, None);
    assert!(r.is_complete(), "没开索引时不声称部分");
    assert_eq!(r.hits.len(), 1);
}

#[test]
fn index_reports_complete_after_writes() {
    let e = engine("complete").with_index(IndexId(1)).unwrap();
    put(&e, "a", vec![1.0, 0.0]);
    put(&e, "b", vec![0.0, 1.0]);
    let fresh = e.index_freshness().expect("index on");
    // `lagging` 是「按一个新快照重判」的保守标记：本次写入的 xid 恰好等于快照的
    // `xmax`，`tx_w` 只到 `xmax − 1`。查询按**自己的**快照判定，因此是完整的。
    assert_eq!(fresh.txn_watermark, TransactionId(2));
    let r = e.search_with_coverage(&[1.0, 0.0], 2);
    assert_eq!(r.coverage, Some(IndexCoverage::Complete));
    assert!(r.is_complete());
    assert_eq!(r.hits.len(), 2);
}

#[test]
fn lagging_index_is_recovered_by_catching_up_before_answering() {
    let e = engine("recover").with_index(IndexId(1)).unwrap();
    put(&e, "a", vec![1.0, 0.0]);

    // 绕过引擎直接写内核：索引不知道这次提交（模拟后台追赶还没跑 / 进程重启后
    // 的索引状态）。引擎的写入路径之外还有大量这样的写者。
    {
        let k = e.kernel();
        let mut txn: TxnCtx = TransactionManager::begin(k, IsolationLevel::Snapshot).unwrap();
        ains::kernel::ObjectManager::create_object(
            k,
            &mut txn,
            &ains::kernel::CreateObjectRequest {
                object_type: ObjectType::Document,
                payload: ains::engine::DocumentPayload {
                    text: "outside".into(),
                    embedding: vec![0.0, 1.0],
                    metadata: BTreeMap::new(),
                }
                .encode(),
                component_count: 1,
            },
        )
        .unwrap();
        TransactionManager::commit(k, &mut txn).unwrap();
    }
    let stale = e.index_freshness().unwrap();
    assert!(stale.lagging, "内核里的事务已越过索引 xid 水位");

    // 查询前补扫缺口：仍然是暴力扫描的候选集（超集），因此可以报 Recovered。
    let r = e.search_with_coverage(&[1.0, 0.0], 5);
    assert_eq!(
        r.coverage,
        Some(IndexCoverage::Recovered {
            via: FallbackStrategy::FullScan
        })
    );
    assert!(r.is_complete());
    // 补扫后 xid 水位已追到内核最后分配的事务——这正是「查询快照可以报到」的位置。
    let after = e.index_freshness().unwrap();
    assert_eq!(
        after.txn_watermark,
        TransactionId(stale.txn_watermark.0 + 1)
    );
}
