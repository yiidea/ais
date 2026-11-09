# 引擎与 Intent API（`src/engine`）

面向调用方的一层：文档载荷 + 向量检索 + 元数据过滤 + Intent 管线，并在开启索引时
给出查询结果的完整性标记。它不做事务与持久化决策，只组合内核的公开 trait。

## 1. 文件对照

| 文件 | 内容 |
|---|---|
| `src/engine/mod.rs` | `Engine`、`DocumentPayload`、`IndexCoverage`、`SearchResult`、`DEFAULT_COST_MODEL` |
| `src/engine/vector.rs` | `VectorIndex`：暴力余弦相似度（精确，不是近似索引） |
| `src/engine/intent.rs` | `IntentRequest` / `Op` / `IntentResponse` / `SearchHit` 与线性流水线执行 |

## 2. 文档载荷

```rust
pub struct DocumentPayload {
    pub text: String,
    pub embedding: Vec<f32>,
    pub metadata: BTreeMap<String, String>,
}
```

编码格式：`MAGIC("DOC1") ‖ text_len ‖ text ‖ dim ‖ f32[dim] ‖ meta_count ‖ (k_len‖k‖v_len‖v)*`。
整个 payload 存进对象 payload；`DocumentPayload::decode` 对截断与错误 magic 返回
`Corrupted(ChecksumMismatch)`。

> 载荷自带向量，意味着同一份数据存两份（对象 payload + 向量索引）。这是 MVP 的取舍：
> 向量索引可丢弃重建。

## 3. 写路径

```rust
engine.put_document(ObjectType::Document, "text", vec![0.9, 0.1], meta)?;  // -> ObjectId
engine.update_document(id, "new", vec![0.8, 0.2], meta)?;                 // -> VersionId
engine.delete_document(id)?;
```

每次写入都是「开事务 → 改对象 → 提交 → 更新派生缓存（向量 / 文档 / 版本号）→ 追赶索引」。
提交失败会 `abort` 并返回内核错误；索引追赶失败**不影响**写入结果（索引是派生数据）。

## 4. 读路径

```rust
engine.get_document(id)?;                 // 按最新快照取载荷
engine.search(&query, k);                 // Vec<SearchHit>，无覆盖声明
engine.search_with_coverage(&query, k);   // SearchResult { hits, coverage }
engine.execute(IntentRequest { operations });  // -> IntentResponse { hits }
```

`IntentResponse` 目前不带 `coverage` 字段；需要覆盖声明时用 `search_with_coverage`。

<a id="覆盖标记"></a>

### 覆盖标记

```rust
pub enum IndexCoverage {
    Complete,
    Recovered { via: FallbackStrategy },
    Partial { missing_estimate: u64 },
}
```

只在 `Engine::with_index(id)` 打开索引后出现（否则 `coverage = None`）。判定流程
（`Engine::coverage_for_query`）：

```text
1. 取查询快照 S（fresh_snapshot）
2. 完整 ⟺ tx_w + 1 ≥ S.xmax        // 快照可见版本的 created_by_xid < S.xmax
3. 不完整 → catch_up_index() 补扫缺口
     补扫后完整   → Recovered { via: FullScan }
     补扫后仍不完整 → Partial { missing_estimate }
     补扫失败      → Partial { missing_estimate }
```

为什么不用 `S.xmin ≤ tx_w`：`fresh_snapshot` 的 `xmin` 是活跃事务的下界，新取的快照
`xmin` 总比 `tx_w` 大 1（`tx_w` 只追到区间里最后出现的 xid），按字面判定会把所有查询
都报成滞后。`S.xmax` 才是「本查询需要覆盖到的上界」。`IndexFreshness::covers` 仍保留
06 §3.1 的字面形式，供精确掌握快照构造的调用方使用。

候选集是暴力扫描得到的，是查询结果的**超集**，因此补扫后可以报 `Recovered` 而不是
`Partial`（06 §3.1 策略 B 的超集条件在这里天然满足）。

## 5. Intent 管线

四个原语，按书写顺序线性执行，候选集从「全部对象」开始：

| `Op` | 行为 |
|---|---|
| `Similarity { query, k }` | 向量 top-k（暴力余弦），与已有候选集取交 |
| `Filter { field, equals }` | 元数据等值过滤 |
| `TopK { k }` | 有分数时按分数降序截断，否则保持原顺序 |
| `Read { id }` | 候选集收窄为单个对象（存在时） |

第一版**不做**语义规划：AI 生成操作序列，引擎确定性执行。计划器的接入点（`FreshnessOracle`
→ `FallbackStrategy` 选择）见 [index.md](index.md#新鲜度与覆盖)。

## 6. 检索精度

`VectorIndex::search` 是精确余弦相似度（无 ANN 误差），因此「索引结果 vs 暴力扫描」的差集
只可能是滞后造成的，不会是近似误差（06 §6.1 A7 的容差集在此为空）。

## 7. 已知差距

- 向量索引不是索引段：`Engine::rebuild_indexes` 从对象目录全量重建，内存里保存整份载荷。
- `IntentResponse` 没有 `coverage` 字段，Intent 查询无法自带完整性标记。
- `DEFAULT_COST_MODEL` 的系数是占位值，未接 07 §7.2 的 B7 实测。
- 写路径与索引追赶都在调用线程同步执行。
