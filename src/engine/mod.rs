//! 面向 AI 的对象/文档/向量引擎与 Intent API。
//!
//! 这一层不做事务与持久化决策，只把内核的 ObjectManager / TransactionManager
//! 组合成「存文档 + 向量检索 + 元数据过滤」的确定性接口：
//! AI 负责意图与规划，引擎负责执行（README 核心原则）。

pub mod intent;
pub mod vector;

use crate::error::{CorruptionKind, IdKind, KernelError, KernelResult};
use crate::kernel::{
    CreateObjectRequest, Kernel, KernelConfig, ObjectManager, TransactionManager, TxnCtx,
    VersionSpec,
};
use crate::types::*;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use vector::VectorIndex;

pub use intent::{IntentRequest, IntentResponse, Op, SearchHit};

const PAYLOAD_MAGIC: u32 = 0x444F_4331; // "DOC1"

/// 文档载荷：文本 + 向量 + 元数据，一起编码进对象 payload。
#[derive(Clone, Debug, PartialEq)]
pub struct DocumentPayload {
    pub text: String,
    pub embedding: Vec<f32>,
    pub metadata: BTreeMap<String, String>,
}

impl DocumentPayload {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&PAYLOAD_MAGIC.to_le_bytes());
        let tb = self.text.as_bytes();
        out.extend_from_slice(&(tb.len() as u32).to_le_bytes());
        out.extend_from_slice(tb);
        out.extend_from_slice(&(self.embedding.len() as u32).to_le_bytes());
        for f in &self.embedding {
            out.extend_from_slice(&f.to_le_bytes());
        }
        out.extend_from_slice(&(self.metadata.len() as u32).to_le_bytes());
        for (k, v) in &self.metadata {
            out.extend_from_slice(&(k.len() as u32).to_le_bytes());
            out.extend_from_slice(k.as_bytes());
            out.extend_from_slice(&(v.len() as u32).to_le_bytes());
            out.extend_from_slice(v.as_bytes());
        }
        out
    }

    pub fn decode(buf: &[u8]) -> KernelResult<Self> {
        let mut r = Reader::new(buf);
        if r.u32()? != PAYLOAD_MAGIC {
            return Err(KernelError::corrupted(CorruptionKind::ChecksumMismatch));
        }
        let tl = r.u32()? as usize;
        let text = String::from_utf8(r.take(tl)?.to_vec())
            .map_err(|_| KernelError::corrupted(CorruptionKind::ChecksumMismatch))?;
        let dim = r.u32()? as usize;
        let mut embedding = Vec::with_capacity(dim.min(1 << 20));
        for _ in 0..dim {
            embedding.push(f32::from_le_bytes(r.take(4)?.try_into().unwrap()));
        }
        let mcount = r.u32()? as usize;
        let mut metadata = BTreeMap::new();
        for _ in 0..mcount {
            let kl = r.u32()? as usize;
            let key = String::from_utf8(r.take(kl)?.to_vec())
                .map_err(|_| KernelError::corrupted(CorruptionKind::ChecksumMismatch))?;
            let vl = r.u32()? as usize;
            let val = String::from_utf8(r.take(vl)?.to_vec())
                .map_err(|_| KernelError::corrupted(CorruptionKind::ChecksumMismatch))?;
            metadata.insert(key, val);
        }
        Ok(DocumentPayload {
            text,
            embedding,
            metadata,
        })
    }
}

pub struct Engine {
    pub(crate) kernel: Kernel,
    pub(crate) vectors: Mutex<VectorIndex>,
    pub(crate) docs: Mutex<BTreeMap<ObjectId, DocumentPayload>>,
    pub(crate) versions: Mutex<BTreeMap<ObjectId, VersionId>>,
}

impl Engine {
    /// 打开引擎并重建派生索引（向量索引与文档缓存）。
    pub fn open(
        config: KernelConfig,
        device: Box<dyn crate::hal::StorageDevice>,
    ) -> KernelResult<Self> {
        let kernel = Kernel::open(config, device)?;
        let engine = Engine {
            kernel,
            vectors: Mutex::new(VectorIndex::new()),
            docs: Mutex::new(BTreeMap::new()),
            versions: Mutex::new(BTreeMap::new()),
        };
        engine.rebuild_indexes()?;
        Ok(engine)
    }

    pub fn kernel(&self) -> &Kernel {
        &self.kernel
    }

    /// 从对象存储重建派生索引。索引是派生数据，恢复后必须可重建（03 §6.1 step 9）。
    pub fn rebuild_indexes(&self) -> KernelResult<()> {
        let snap = self.kernel.fresh_snapshot();
        let mut vectors = VectorIndex::new();
        let mut docs = BTreeMap::new();
        let mut versions = BTreeMap::new();
        for id in self.kernel.all_object_ids() {
            if let Ok(ver) = ObjectManager::resolve_version(&self.kernel, id, &snap) {
                if let Ok(payload) = self.load_payload(id, ver) {
                    vectors.upsert(id, payload.embedding.clone());
                    docs.insert(id, payload);
                    versions.insert(id, ver);
                }
            }
        }
        *self.vectors.lock().unwrap() = vectors;
        *self.docs.lock().unwrap() = docs;
        *self.versions.lock().unwrap() = versions;
        Ok(())
    }

    pub fn put_document(
        &self,
        object_type: ObjectType,
        text: impl Into<String>,
        embedding: Vec<f32>,
        metadata: BTreeMap<String, String>,
    ) -> KernelResult<ObjectId> {
        let payload = DocumentPayload {
            text: text.into(),
            embedding,
            metadata,
        };
        let bytes = payload.encode();
        let mut txn = TransactionManager::begin(&self.kernel, IsolationLevel::Snapshot)?;
        let req = CreateObjectRequest {
            object_type,
            payload: bytes,
            component_count: 1,
        };
        let id = match ObjectManager::create_object(&self.kernel, &mut txn, &req) {
            Ok(id) => id,
            Err(e) => {
                let _ = TransactionManager::abort(&self.kernel, &mut txn);
                return Err(e);
            }
        };
        if let Err(e) = TransactionManager::commit(&self.kernel, &mut txn) {
            let _ = TransactionManager::abort(&self.kernel, &mut txn);
            return Err(e);
        }
        self.vectors
            .lock()
            .unwrap()
            .upsert(id, payload.embedding.clone());
        self.docs.lock().unwrap().insert(id, payload);
        self.versions.lock().unwrap().insert(id, txn_version(&txn));
        Ok(id)
    }

    pub fn update_document(
        &self,
        id: ObjectId,
        text: impl Into<String>,
        embedding: Vec<f32>,
        metadata: BTreeMap<String, String>,
    ) -> KernelResult<VersionId> {
        let payload = DocumentPayload {
            text: text.into(),
            embedding,
            metadata,
        };
        let mut txn = TransactionManager::begin(&self.kernel, IsolationLevel::Snapshot)?;
        let spec = VersionSpec {
            payload: payload.encode(),
        };
        let ver = match ObjectManager::stage_version(&self.kernel, &mut txn, id, &spec) {
            Ok(v) => v,
            Err(e) => {
                let _ = TransactionManager::abort(&self.kernel, &mut txn);
                return Err(e);
            }
        };
        if let Err(e) = TransactionManager::commit(&self.kernel, &mut txn) {
            let _ = TransactionManager::abort(&self.kernel, &mut txn);
            return Err(e);
        }
        self.vectors
            .lock()
            .unwrap()
            .upsert(id, payload.embedding.clone());
        self.docs.lock().unwrap().insert(id, payload);
        self.versions.lock().unwrap().insert(id, ver);
        Ok(ver)
    }

    pub fn delete_document(&self, id: ObjectId) -> KernelResult<()> {
        let mut txn = TransactionManager::begin(&self.kernel, IsolationLevel::Snapshot)?;
        if let Err(e) = ObjectManager::delete_object(&self.kernel, &mut txn, id) {
            let _ = TransactionManager::abort(&self.kernel, &mut txn);
            return Err(e);
        }
        if let Err(e) = TransactionManager::commit(&self.kernel, &mut txn) {
            let _ = TransactionManager::abort(&self.kernel, &mut txn);
            return Err(e);
        }
        self.vectors.lock().unwrap().remove(id);
        self.docs.lock().unwrap().remove(&id);
        self.versions.lock().unwrap().remove(&id);
        Ok(())
    }

    pub fn get_document(&self, id: ObjectId) -> KernelResult<DocumentPayload> {
        let snap = self.kernel.fresh_snapshot();
        let ver = ObjectManager::resolve_version(&self.kernel, id, &snap)?;
        self.load_payload(id, ver)
    }

    /// 派生的向量索引规模（诊断/测试用）。
    pub fn indexed_count(&self) -> usize {
        self.vectors.lock().unwrap().len()
    }

    pub fn search(&self, query: &[f32], k: usize) -> Vec<SearchHit> {
        let scored = self.vectors.lock().unwrap().search(query, k);
        let docs = self.docs.lock().unwrap();
        scored
            .into_iter()
            .filter_map(|(id, score)| {
                docs.get(&id).map(|d| SearchHit {
                    id,
                    score,
                    text: d.text.clone(),
                    metadata: d.metadata.clone(),
                })
            })
            .collect()
    }

    pub fn execute(&self, req: IntentRequest) -> KernelResult<IntentResponse> {
        intent::execute(self, req)
    }

    fn load_payload(&self, id: ObjectId, ver: VersionId) -> KernelResult<DocumentPayload> {
        let view = ObjectManager::read_version(&self.kernel, id, ver)?;
        let ext = view
            .version
            .extent
            .ok_or_else(|| KernelError::not_found(IdKind::Extent, format!("{id}@{ver:?}")))?;
        let bytes = self.kernel.read_extent_bytes(ext)?;
        DocumentPayload::decode(&bytes)
    }
}

fn txn_version(txn: &TxnCtx) -> VersionId {
    txn.pending_versions
        .last()
        .map(|(_, v)| *v)
        .unwrap_or(VersionId(1))
}

struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Reader { buf, pos: 0 }
    }
    fn take(&mut self, n: usize) -> KernelResult<&'a [u8]> {
        if self.pos + n > self.buf.len() {
            return Err(KernelError::corrupted(CorruptionKind::ChecksumMismatch));
        }
        let s = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }
    fn u32(&mut self) -> KernelResult<u32> {
        let b = self.take(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }
}

#[allow(dead_code)]
fn shared<T>(v: T) -> Arc<Mutex<T>> {
    Arc::new(Mutex::new(v))
}
