//! Intent API 的第一版操作原语（README §5.2）。
//!
//! AI 生成意图与操作序列，引擎验证并确定性执行。MVP 只实现最常用的四个原语：
//! SIMILARITY / FILTER / TOPK / READ。操作序列等价于一个线性流水线。

use super::Engine;
use crate::error::KernelResult;
use crate::types::ObjectId;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug)]
pub enum Op {
    /// 向量召回。
    Similarity { query: Vec<f32>, k: usize },
    /// 元数据等值过滤。
    Filter { field: String, equals: String },
    /// 截断候选集。
    TopK { k: usize },
    /// 按对象标识读取。
    Read { id: ObjectId },
}

#[derive(Clone, Debug, Default)]
pub struct IntentRequest {
    pub operations: Vec<Op>,
}

#[derive(Clone, Debug, Default)]
pub struct IntentResponse {
    pub hits: Vec<SearchHit>,
}

#[derive(Clone, Debug)]
pub struct SearchHit {
    pub id: ObjectId,
    pub score: f32,
    pub text: String,
    pub metadata: BTreeMap<String, String>,
}

pub fn execute(engine: &Engine, req: IntentRequest) -> KernelResult<IntentResponse> {
    let docs = engine.docs.lock().unwrap();
    // 候选集为 None 表示「全部对象」。
    let mut candidates: Option<Vec<ObjectId>> = None;
    let mut scores: BTreeMap<ObjectId, f32> = BTreeMap::new();

    for op in req.operations {
        match op {
            Op::Similarity { query, k } => {
                let scored = engine.vectors.lock().unwrap().search(&query, k);
                let ids: Vec<ObjectId> = scored.iter().map(|(id, _)| *id).collect();
                for (id, s) in &scored {
                    scores.insert(*id, *s);
                }
                candidates = Some(match candidates {
                    None => ids,
                    Some(prev) => {
                        let keep: BTreeSet<ObjectId> = prev.into_iter().collect();
                        ids.into_iter().filter(|id| keep.contains(id)).collect()
                    }
                });
            }
            Op::Filter { field, equals } => {
                let base: Vec<ObjectId> = match &candidates {
                    Some(v) => v.clone(),
                    None => docs.keys().copied().collect(),
                };
                candidates = Some(
                    base.into_iter()
                        .filter(|id| {
                            docs.get(id)
                                .and_then(|d| d.metadata.get(&field))
                                .map(|v| v == &equals)
                                .unwrap_or(false)
                        })
                        .collect(),
                );
            }
            Op::TopK { k } => {
                if let Some(v) = &mut candidates {
                    // 有相似度分数时按分数排，否则保持原顺序。
                    if !scores.is_empty() {
                        v.sort_by(|a, b| {
                            scores
                                .get(b)
                                .unwrap_or(&0.0)
                                .partial_cmp(scores.get(a).unwrap_or(&0.0))
                                .unwrap_or(std::cmp::Ordering::Equal)
                        });
                    }
                    v.truncate(k);
                }
            }
            Op::Read { id } => {
                candidates = Some(if docs.contains_key(&id) { vec![id] } else { vec![] });
                scores.insert(id, 1.0);
            }
        }
    }

    let ids: Vec<ObjectId> = match candidates {
        Some(v) => v,
        None => docs.keys().copied().collect(),
    };
    let hits = ids
        .into_iter()
        .filter_map(|id| {
            docs.get(&id).map(|d| SearchHit {
                id,
                score: scores.get(&id).copied().unwrap_or(1.0),
                text: d.text.clone(),
                metadata: d.metadata.clone(),
            })
        })
        .collect();
    Ok(IntentResponse { hits })
}
