use ains::engine::Engine;
use ains::hal::mem::MemDevice;
use ains::kernel::KernelConfig;
use ains::types::{DeviceId, ObjectType};

fn main() -> ains::KernelResult<()> {
    let device = Box::new(MemDevice::new(DeviceId(1), 64 << 20));
    let engine = Engine::open(KernelConfig::new("ckpt.snap"), device)?;
    let id = engine.put_document(
        ObjectType::Document,
        "WAL 检查点的原子发布",
        vec![0.9, 0.1, 0.0],
        Default::default(),
    )?;
    for hit in engine.search(&[1.0, 0.0, 0.0], 5) {
        println!("{:.3} {}", hit.score, hit.text);
    }
    println!("{id}");
    Ok(())
}
