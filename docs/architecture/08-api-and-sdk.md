# 08 — API 契约与 SDK

本文定义对外 API 的语义契约（不绑定某一种序列化），SDK 无论语言都要等价实现。

## 1. API 表面

```protobuf
service SynapseStore {
  // 管理
  rpc CreateDomain(CreateDomainRequest) returns (Domain);
  rpc UpdateDomain(UpdateDomainRequest) returns (Domain);

  // 写
  rpc Write(WriteRequest) returns (WriteReceipt);
  rpc Consolidate(ConsolidateRequest) returns (ConsolidateReceipt);

  // 读
  rpc Get(GetRequest) returns (GetResponse);
  rpc Recall(RecallRequest) returns (RecallResponse);   // 意图查询

  // 溯源
  rpc Provenance(ProvenanceRequest) returns (ProvenanceResponse);

  // 流
  rpc Watch(WatchRequest) returns (stream ChangeEvent);
}
```

## 2. Write

```protobuf
message WriteRequest {
  string domain = 1;
  bytes payload = 2;
  bytes embedding = 3;
  repeated Tag tags = 4;
  Provenance provenance = 5;
  Idempotency idempotency = 6;   // strict | semantic | none
  Consistency consistency = 7;   // causal | strong | eventual
}

message WriteReceipt {
  bytes id = 1;
  uint64 epoch = 2;
  bytes causal_token = 3;
  WriteOutcome outcome = 4;      // created | merged | conflicted | rejected
  map<string,string> merge_tags = 5;
}
```

### 2.1 语义

- `Write` 必须幂等或显式声明非幂等（`none`）。
- `created`：新对象。
- `merged`：与已有的语义近邻对象合并（返回合并到哪个对象）。
- `conflicted`：检测到冲突，返回冲突组 ID。
- `rejected`：超出配额 / 校验失败。
- `causal_token` 必须原样带回，以便跨节点后读。

## 3. Recall（意图查询）

```protobuf
message RecallRequest {
  string domain = 1;
  Intent intent = 2;
  Snapshot snapshot = 3;         // 可选；不提供则取默认因果快照
  bool stream = 4;
}

message RecallResponse {
  repeated Chunk chunks = 1;
  PlanTrace plan_trace = 2;      // 命中的计划与降级原因
  bytes causal_token = 3;
  Usage usage = 4;               // candidates, tokens, latency
}
```

### 3.1 契约

- 若预算不足，`chunks` 为空而非报错，`usage.truncated = true`。
- `plan_trace` 必须包含：
  - 实际执行的算子序列；
  - 是否走了模型规划、是否回退；
  - 代价预测 vs 实际测量。
- 当 `stream = true`，按注意力密度降序流式返回。

## 4. Provenance

```protobuf
message ProvenanceRequest {
  bytes id = 1;
  uint32 depth = 2;
  Direction direction = 3;       // upstream | downstream | both
  repeated EdgeKind kinds = 4;
}

message ProvenanceResponse {
  Graph subgraph = 1;
  repeated Chain chains = 2;     // 返回到根的最短链
  Confidence confidence = 3;     // 传导后的可信度
}
```

## 5. 错误模型

| code | 含义 | 客户端行为 |
|---|---|---|
| `QUOTA_EXCEEDED` | 域配额超限 | 退避重试 |
| `PLAN_INVALID` | 规划器产生非法计划 | 记录 + 重试（走规则） |
| `PLAN_BUDGET_EXCEEDED` | 计划估算超预算 | 缩小预算/重规划 |
| `CONSISTENCY_UNSATISFIABLE` | 已声明的强一致无法满足 | 降级或重试 |
| `UNTRUSTED_SOURCE` | 意图要求信任级别但结果不满足 | 返回空 + 提示 |

## 6. SDK 设计原则

1. **同一套语义，不同语言**：Rust 核心 + 多语言绑定，跨语言行为必须通过一致性测试。
2. **本地/远程透明**：SDK 初始化时检测本地模式（嵌入式库）；远程模式对接集群，API 相同。
3. **令牌传播**：SDK 自动处理因果令牌（`with_causal_token` 可手动指定）。
4. **预算优先**：SDK 默认按 token 预算构造请求，避免超大上下文。
5. **可观测性**：每次请求返回 `plan_trace` 与 `usage`，便于客户端监控。

### 6.1 示例（Python）

```python
from synapse import Store, Intent, Constraint, Budget

store = Store.open("sse://localhost:7878", domain="agent_memory")

store.write(
    payload=b"User prefers dark theme at night",
    embedding=embed("dark theme preference"),
    provenance={"writer": "profile-bot", "source": "episode:42"},
    idempotency="semantic",
)

result = store.recall(
    Intent(
        about="用户界面偏好",
        constraints=[Constraint(min_confidence=0.7)],
        budget=Budget(max_tokens=512, max_latency_ms=80),
    )
)
for chunk in result.chunks:
    print(chunk.text, chunk.confidence, chunk.evidence)
```

## 7. 兼容性

- API 版本随语义变化升级（v1 → v2 可能破坏兼容），但 `Recall` 与 `Write` 的核心字段保持稳定。
- 规划 DSL 有自己的版本号，计划缓存绑定 DSL 版本。
- SDK 与引擎版本协商：不支持的功能返回 `UNSUPPORTED`，不静默降级。
