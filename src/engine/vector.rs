//! 向量索引：MVP 用暴力余弦检索；接口留出 HNSW 替换点。
//!
//! README 的 MVP 目标是 HNSW + Metadata Index。第一版先保证「对象 + 文档 + 向量」
//! 端到端可用与可恢复，索引只作为**派生数据**存在：崩溃后从对象 payload 重建。

use crate::types::ObjectId;

#[derive(Default)]
pub struct VectorIndex {
    entries: Vec<(ObjectId, Vec<f32>)>,
}

impl VectorIndex {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn upsert(&mut self, id: ObjectId, embedding: Vec<f32>) {
        if let Some(slot) = self.entries.iter_mut().find(|(oid, _)| *oid == id) {
            slot.1 = embedding;
        } else {
            self.entries.push((id, embedding));
        }
    }

    pub fn remove(&mut self, id: ObjectId) {
        self.entries.retain(|(oid, _)| *oid != id);
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// 返回按余弦相似度降序的 (ObjectId, score)。
    pub fn search(&self, query: &[f32], k: usize) -> Vec<(ObjectId, f32)> {
        if query.is_empty() || k == 0 {
            return Vec::new();
        }
        let qnorm = norm(query);
        if qnorm == 0.0 {
            return Vec::new();
        }
        let mut scored: Vec<(ObjectId, f32)> = self
            .entries
            .iter()
            .filter(|(_, v)| v.len() == query.len())
            .map(|(id, v)| {
                let vnorm = norm(v);
                let score = if vnorm == 0.0 {
                    0.0
                } else {
                    dot(query, v) / (qnorm * vnorm)
                };
                (*id, score)
            })
            .collect();
        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        scored.truncate(k);
        scored
    }
}

fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b.iter()).map(|(x, y)| x * y).sum()
}

fn norm(a: &[f32]) -> f32 {
    dot(a, a).sqrt()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cosine_ranking() {
        let mut idx = VectorIndex::new();
        idx.upsert(ObjectId::new(1, 1), vec![1.0, 0.0]);
        idx.upsert(ObjectId::new(1, 2), vec![0.0, 1.0]);
        idx.upsert(ObjectId::new(1, 3), vec![0.7, 0.7]);
        let r = idx.search(&[1.0, 0.0], 2);
        assert_eq!(r[0].0, ObjectId::new(1, 1));
        assert_eq!(r[1].0, ObjectId::new(1, 3));
    }
}
