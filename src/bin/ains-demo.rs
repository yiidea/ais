//! MVP 端到端演示：对象写入 → 向量检索 → 元数据过滤 → 删除 → 恢复。
//!
//! 运行：`cargo run --bin ains-demo`

use ains::engine::intent::{IntentRequest, Op};
use ains::engine::Engine;
use ains::hal::mem::MemDevice;
use ains::kernel::{CheckpointManager, KernelConfig, RecoveryManager, VerifyScope};
use ains::types::{DeviceId, ObjectType};
use std::collections::BTreeMap;

fn main() {
    let device = Box::new(MemDevice::new(DeviceId(1), 64 * 1024 * 1024));
    let dir = std::env::temp_dir().join("ains-demo-checkpoint");
    let _ = std::fs::remove_file(&dir);
    let config = KernelConfig::new(dir);
    let engine = Engine::open(config, device).expect("open engine");

    let mut docs: Vec<(&str, Vec<f32>)> = vec![
        ("postgres buffer pool 的换页策略", vec![1.0, 0.0, 0.0]),
        ("向量索引 HNSW 的分层构建", vec![0.0, 1.0, 0.0]),
        ("WAL 检查点的原子发布", vec![0.9, 0.1, 0.0]),
        ("GPU HBM 的 NUMA 亲和分配", vec![0.0, 0.0, 1.0]),
    ];

    let mut ids = Vec::new();
    for (i, (text, emb)) in docs.drain(..).enumerate() {
        let mut md = BTreeMap::new();
        md.insert("domain".to_string(), "storage".to_string());
        md.insert("rank".to_string(), i.to_string());
        let id = engine
            .put_document(ObjectType::Document, text, emb, md)
            .expect("put document");
        ids.push(id);
        println!("写入对象 {id}（{}）", text);
    }

    println!("\n--- 向量检索 query=[1.0, 0.0, 0.0] top 2 ---");
    for hit in engine.search(&[1.0, 0.0, 0.0], 2) {
        println!("  {:.4}  {}  [{}]", hit.score, hit.text, hit.id);
    }

    println!("\n--- Intent 管线：向量召回 → 过滤 domain=storage → topk 3 ---");
    let resp = engine
        .execute(IntentRequest {
            operations: vec![
                Op::Similarity {
                    query: vec![0.95, 0.05, 0.0],
                    k: 4,
                },
                Op::Filter {
                    field: "domain".into(),
                    equals: "storage".into(),
                },
                Op::TopK { k: 3 },
            ],
        })
        .expect("execute intent");
    for (i, hit) in resp.hits.iter().enumerate() {
        println!("  #{} {:.4} {}", i + 1, hit.score, hit.text);
    }

    let victim = ids[1];
    println!("\n删除对象 {victim}");
    engine.delete_document(victim).expect("delete");

    println!("\n--- 检查点发布 ---");
    let ckpt = CheckpointManager::trigger(engine.kernel()).expect("trigger checkpoint");
    println!("  检查点 {ckpt:?}");

    let report = RecoveryManager::verify(engine.kernel(), VerifyScope::Full).expect("verify");
    println!("  内核自校验：{} 项问题", report.issues.len());

    // 模拟进程重启：新开一个空设备，只靠检查点 + WAL 恢复。
    // 老数据的 payload 随内存设备一起消失，因此这里演示的是恢复路径本身。
    let dir2 = std::env::temp_dir().join("ains-demo-checkpoint-2");
    let _ = std::fs::remove_file(&dir2);
    let config2 = KernelConfig::new(dir2);
    let device2 = Box::new(MemDevice::new(DeviceId(1), 64 * 1024 * 1024));
    let engine2 = Engine::open(config2, device2).expect("open engine 2");
    let recovered = RecoveryManager::recover(engine2.kernel()).expect("recover");
    println!(
        "\n--- 重启恢复：重放 {} 条 WAL，放弃 {} 个未提交事务，派生索引重建 {} 条 ---",
        recovered.replayed_records,
        recovered.aborted_txns.len(),
        engine2.indexed_count()
    );
}
