//! 内核挂在 reactor 上（07 §4，HRM7 / 04 §6.3 I1）。
//!
//! 同步 façade 同一时刻只有一个在飞操作，QD 与背压在它上面没有意义；
//! 这里验证内核经 reactor 打开后语义不变、并发真的发生、flush 不被挤掉。

use ains::hal::mem::MemDevice;
use ains::hal::qd::QueueDepth;
use ains::hal::StorageDevice;
use ains::kernel::{
    CreateObjectRequest, Kernel, KernelConfig, ObjectManager, ReactorSpec, TransactionManager,
    TxnCtx,
};
use ains::types::*;
use std::sync::Arc;
use std::time::Duration;

fn config(tag: &str) -> KernelConfig {
    let dir = std::env::temp_dir().join(format!("ains-reactor-{tag}"));
    let _ = std::fs::remove_dir_all(&dir);
    KernelConfig::new(dir.join("ckpt.snap"))
}

fn reactor_kernel(tag: &str, usable: u32) -> Kernel {
    let qd = QueueDepth::initial(usable + 2, usable, 2).expect("valid qd");
    let mut cfg = config(tag);
    cfg.reactor = Some(ReactorSpec {
        window: Duration::from_millis(5),
        ..ReactorSpec::new(qd)
    });
    Kernel::open(
        cfg,
        Box::new(MemDevice::new(DeviceId(1), 16 << 20)) as Box<dyn StorageDevice>,
    )
    .expect("open kernel on reactor")
}

fn create(k: &Kernel, payload: &[u8]) -> ObjectId {
    let mut txn: TxnCtx = TransactionManager::begin(k, IsolationLevel::Snapshot).unwrap();
    let id = ObjectManager::create_object(
        k,
        &mut txn,
        &CreateObjectRequest {
            object_type: ObjectType::Document,
            payload: payload.to_vec(),
            component_count: 1,
        },
    )
    .unwrap();
    TransactionManager::commit(k, &mut txn).unwrap();
    id
}

#[test]
fn reactor_kernel_commits_and_reads_back() {
    let k = reactor_kernel("roundtrip", 4);
    let id = create(&k, b"through the reactor");
    let view = ObjectManager::read_version(&k, id, VersionId(1)).expect("read back");
    assert_eq!(view.header.id, id);
    assert_eq!(view.version.version, VersionId(1));
}

#[test]
fn reactor_survives_many_commits() {
    // 每个 commit 都经 WAL 写 + flush：预留槽位被反复占用与归还，
    // 若账记错了，这里会先卡在背压上。
    let k = reactor_kernel("many", 2);
    for i in 0..64u32 {
        create(&k, &i.to_le_bytes());
    }
    assert_eq!(k.all_object_ids().len() as u32, 64);
}

#[test]
fn concurrent_commits_share_the_reactor() {
    let k = Arc::new(reactor_kernel("concurrent", 4));
    let handles: Vec<_> = (0..4)
        .map(|t| {
            let k = Arc::clone(&k);
            std::thread::spawn(move || {
                for i in 0..16u8 {
                    create(&k, &[t as u8, i]);
                }
            })
        })
        .collect();
    for h in handles {
        h.join().expect("writer thread");
    }
    assert_eq!(k.all_object_ids().len(), 64);
}

#[test]
fn sync_and_reactor_paths_see_the_same_data() {
    // 同一块盘、同样的写入：两种 façade 都只是「提交即等待完成」的实现，
    // 观测到的对象必须一致（HRM8 的可观测行为一致，在这里是对象层）。
    let dir = std::env::temp_dir().join("ains-reactor-parity");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");

    let path = dir.join("disk.img");
    let sync_k = Kernel::open(
        KernelConfig::new(dir.join("sync.snap")),
        Box::new(ains::hal::FileDevice::open(DeviceId(1), &path, 16 << 20).unwrap())
            as Box<dyn StorageDevice>,
    )
    .expect("open sync");
    let id = create(&sync_k, b"parity");
    let sync_len = sync_k.all_object_ids().len();
    drop(sync_k);

    let mut cfg = KernelConfig::new(dir.join("reactor.snap"));
    cfg.reactor = Some(ReactorSpec::new(
        QueueDepth::initial(8, 6, 2).expect("valid qd"),
    ));
    let reactor_k = Kernel::open(
        cfg,
        Box::new(ains::hal::FileDevice::open(DeviceId(1), &path, 16 << 20).unwrap())
            as Box<dyn StorageDevice>,
    )
    .expect("open reactor");
    // 重启后恢复：两种路径都能从同一块盘上把对象读回来。
    assert_eq!(reactor_k.all_object_ids().len(), sync_len);
    assert!(reactor_k.all_object_ids().contains(&id));
}
