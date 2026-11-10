//! 面向 AI 的对象/文档/向量引擎与 Intent API。
//!
//! 这一层不做事务与持久化决策，只把内核的 ObjectManager / TransactionManager
//! 组合成「存文档 + 向量检索 + 元数据过滤」的确定性接口：
//! AI 负责意图与规划，引擎负责执行（README 核心原则）。

pub mod intent;
pub mod vector;

use crate::error::InternalCode;
use crate::error::{CorruptionKind, IdKind, KernelError, KernelResult};
use crate::index::{
    CatchUpLimits, ChangeRate, CostModel, FallbackStrategy, IndexFreshness, IndexKind,
    MaintenanceHost, MetaIndex, SegmentManifest,
};
use crate::kernel::{
    ChangeBatchSpec, ChangeFeed, CreateObjectRequest, Kernel, KernelConfig, ObjectManager,
    TransactionManager, TxnCtx, VersionSpec,
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

/// 查询结果的完整性标记（06 §4.3）：索引不覆盖这个快照时必须显式暴露。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IndexCoverage {
    /// 快照内的全部写入都已进索引，结果完整。
    Complete,
    /// 结果经回退补齐后完整（仅限候选集是超集的回退策略）。
    Recovered { via: FallbackStrategy },
    /// 结果部分且未能补齐——`missing_estimate` 是估计值，不是上界。
    Partial { missing_estimate: u64 },
}

#[derive(Clone, Debug, Default)]
pub struct SearchResult {
    pub hits: Vec<SearchHit>,
    pub coverage: Option<IndexCoverage>,
}

impl SearchResult {
    /// 结果是否完整（`None` = 引擎没有开启索引，不做覆盖声明）。
    pub fn is_complete(&self) -> bool {
        !matches!(self.coverage, Some(IndexCoverage::Partial { .. }))
    }
}

pub struct Engine {
    pub(crate) kernel: Kernel,
    pub(crate) vectors: Mutex<VectorIndex>,
    pub(crate) docs: Mutex<BTreeMap<ObjectId, DocumentPayload>>,
    pub(crate) versions: Mutex<BTreeMap<ObjectId, VersionId>>,
    /// 开了索引时：追赶进度（06 §3）与字节→条数估计样本。
    pub(crate) index: Option<Mutex<IndexState>>,
}

/// 引擎内索引的追赶状态。
pub(crate) struct IndexState {
    kind: Box<dyn IndexKind>,
    manifest: SegmentManifest,
    rate: ChangeRate,
    model: CostModel,
}

/// 代价模型系数（微秒）。这些都是 B7 的实测来源，默认值只是让引擎开箱可用，
/// 部署时必须按设备实测替换（07 §2.7）。
pub const DEFAULT_COST_MODEL: CostModel = CostModel {
    c_scan_nanos: 2_000,
    c_point_nanos: 500,
    filter_eval_nanos: 100,
    safety_factor_num: 3,
    safety_factor_den: 2,
};

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
            index: None,
        };
        engine.rebuild_indexes()?;
        Ok(engine)
    }

    /// 开启索引（`MetaIndex` 按对象类型分桶，06 §3 的追赶与覆盖判定）。
    ///
    /// 开启后 `search_with_coverage` 会先追赶、再按快照报覆盖；不开启时引擎不做
    /// 任何覆盖声明（`coverage = None`）。
    pub fn with_index(mut self, id: IndexId) -> KernelResult<Self> {
        // 协调区从 WAL 还原（ADR-0013）：重启后不必从头重追。记录已被截断时
        // 还原不到，从空清单开始——追到 `min_required_lsn` 之前会显式要求重建。
        let manifest = crate::index::manifest_log::restore(self.kernel.wal(), id)
            .map_err(|e| {
                KernelError::internal(
                    InternalCode::InvariantViolation,
                    format!("index manifest restore: {e}"),
                )
            })?
            .unwrap_or_else(|| SegmentManifest::empty(id));
        let state = IndexState {
            kind: Box::new(MetaIndex::new(id)),
            manifest,
            rate: ChangeRate::default(),
            model: DEFAULT_COST_MODEL,
        };
        self.index = Some(Mutex::new(state));
        self.catch_up_index()?;
        Ok(self)
    }

    /// 把索引追到当前 `object_watermark`（同步调用，06 §6.2 的 `fallback_full_scan`
    /// 即在本函数返回后执行）。
    pub fn catch_up_index(&self) -> KernelResult<Option<IndexFreshness>> {
        let Some(state) = &self.index else {
            return Ok(None);
        };
        let mut state = state.lock().unwrap();
        let IndexState {
            kind,
            manifest,
            rate,
            ..
        } = &mut *state;
        let report = crate::index::catch_up(
            kind.as_ref(),
            &self.kernel as &dyn MaintenanceHost,
            self.kernel.wal(),
            manifest,
            &ChangeBatchSpec::unbounded(),
            CatchUpLimits::to_watermark(),
            rate,
        )
        .map_err(|e| {
            KernelError::internal(
                InternalCode::InvariantViolation,
                format!("index catch-up: {e:?}"),
            )
        })?;
        // 协调区记录只追加、不 fsync；在这里刷一次，让它成为可还原的水位
        // （ADR-0013：索引水位在重启后必须能接着用，而不是从零重追）。
        if report.advanced_to > Lsn(0) {
            self.kernel
                .wal()
                .flush_upto(self.kernel.wal().durable_lsn())?;
        }
        if let Some(reason) = report.rebuild_required {
            // A9：水位没动，必须显式要求全量重建，而不是把「没追」当「追完了」。
            return Err(KernelError::internal(
                InternalCode::InvariantViolation,
                format!("index requires rebuild: {reason}"),
            ));
        }
        let snap = self.kernel.fresh_snapshot();
        let model = state.model;
        let rate = state.rate;
        Ok(Some(IndexFreshness::evaluate(
            &state.manifest,
            &snap,
            self.kernel.object_watermark(),
            &rate,
            &model,
        )))
    }

    /// 当前索引的新鲜度；没开索引时为 `None`。
    pub fn index_freshness(&self) -> Option<IndexFreshness> {
        let state = self.index.as_ref()?;
        let state = state.lock().unwrap();
        let snap = self.kernel.fresh_snapshot();
        Some(IndexFreshness::evaluate(
            &state.manifest,
            &snap,
            self.kernel.object_watermark(),
            &state.rate,
            &state.model,
        ))
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
        self.index_after_write();
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
        self.index_after_write();
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
        self.index_after_write();
        Ok(())
    }

    /// 写入之后的追赶：把新提交的变更发布出去。
    ///
    /// 失败不影响写入结果——索引是派生数据，写已经提交；缺口会在下一次
    /// `catch_up_index` 或查询前的追赶里补上，覆盖判定会如实报滞后。
    fn index_after_write(&self) {
        let _ = self.catch_up_index();
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
        self.search_with_coverage(query, k).hits
    }

    /// 同 [`search`](Self::search)，但附带完整性标记（06 §4.3）。
    ///
    /// 开索引时先追赶，再按**查询快照**判覆盖：
    /// - 覆盖 → `Complete`，结果就是全部答案；
    /// - 不覆盖 → 候选集来自向量索引对文档缓存的暴力扫描，它是**超集**，因此先补扫
    ///   缺口再回答（策略 A，`Recovered`）；补不上（追赶失败）时显式报 `Partial`，
    ///   给出缺口估计，不把旧结果当最新结果静默返回。
    pub fn search_with_coverage(&self, query: &[f32], k: usize) -> SearchResult {
        let coverage = self.coverage_for_query();
        let scored = self.vectors.lock().unwrap().search(query, k);
        let docs = self.docs.lock().unwrap();
        let hits = scored
            .into_iter()
            .filter_map(|(id, score)| {
                docs.get(&id).map(|d| SearchHit {
                    id,
                    score,
                    text: d.text.clone(),
                    metadata: d.metadata.clone(),
                })
            })
            .collect();
        SearchResult { hits, coverage }
    }

    fn coverage_for_query(&self) -> Option<IndexCoverage> {
        let before = self.index_freshness()?;
        // 覆盖判定按**本次查询的快照**（06 §3.1 的精确形式）：
        // `完整 ⟺ 快照可见版本的 created_by_xid 都已在索引里`。而快照可见版本的
        // `created_by_xid < S.xmax`，所以 `tx_w ≥ S.xmax − 1` 即完整；`snap.xmin`
        // 是「活跃事务里最小的 xid」，用它会把**之后的**事务也算进缺口。
        let snap = self.kernel.fresh_snapshot();
        let complete = |fw: &IndexFreshness| fw.txn_watermark.0.saturating_add(1) >= snap.xmax.0;
        if complete(&before) {
            return Some(IndexCoverage::Complete);
        }
        match self.catch_up_index() {
            Ok(Some(after)) if complete(&after) => Some(IndexCoverage::Recovered {
                via: FallbackStrategy::FullScan,
            }),
            Ok(Some(after)) => Some(IndexCoverage::Partial {
                missing_estimate: after
                    .estimated_lag_objects
                    .or(before.estimated_lag_objects)
                    .unwrap_or(1),
            }),
            _ => Some(IndexCoverage::Partial {
                missing_estimate: before.estimated_lag_objects.unwrap_or(1),
            }),
        }
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
