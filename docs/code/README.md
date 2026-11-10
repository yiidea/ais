# 代码实现文档

本目录记录**当前实现**（而不是设计意图）：每篇对应源码里的一个模块或一组紧密相关的
模块，写清入口、契约、失败方式与「为什么这样做」。设计文档在
[`docs/chatgpt`](../chatgpt/) 与 [`docs/architecture`](../architecture/)，
冻结决策在 [`docs/adr`](../adr/)；这里不重复论证，只说明代码落在哪里、怎么用、
哪些地方与设计有已知差距。

配套的[使用手册](usage.md)面向调用方，从「打开引擎」一路讲到诊断与测试。

## 阅读顺序

| 文档 | 对应源码 | 一句话 |
|---|---|---|
| [kernel.md](kernel.md) | `src/kernel/*`、`src/wal/*`、`src/types.rs`、`src/error.rs` | 对象 / 版本 / 事务 / 检查点 / 恢复 / 回收 |
| [index.md](index.md) | `src/index/*` | 追赶、段、协调区、新鲜度、回收判定 |
| [hal.md](hal.md) | `src/hal/*` | 设备 trait、同步 façade、reactor、QD 与背压 |
| [engine-intent.md](engine-intent.md) | `src/engine/*` | 文档载荷、向量检索、Intent 管线、覆盖标记 |
| [platform-testing.md](platform-testing.md) | `src/platform.rs`、`src/testing/*` | 启动期平台核对与故障注入夹具 |
| [usage.md](usage.md) | — | 使用手册：最小程序、各场景配方、诊断方法、测试命令 |

## 分层的硬边界（实现侧）

```text
engine (src/engine)          文档 / 向量 / Intent；只组合内核的公开 trait
  └── index (src/index)      内核算不到的派生层；只走三条冻结边
        └── kernel (src/kernel)  Object / Extent / Mapping / Txn / Checkpoint / Recovery
              ├── wal (src/wal)  日志格式、扫描、追加
              └── hal (src/hal)  设备 trait、IoExecutor、reactor 与 QD
```

- 索引层**不是**内核的一部分：它只用 `ChangeFeed`（F16）、`WalManager::append_derived`
  与 `LiveSnapshotSource`（F17）；内核不知道索引存在。
- 内核只用 `IoExecutor` 触达设备（04 §6.3 I3），不直接持有 `StorageDevice`。
- 引擎对索引的依赖是「可选」的：不开索引时不做任何覆盖声明（见
  [engine-intent.md](engine-intent.md#覆盖标记)）。

## 维护约定（新增代码时同步这里）

1. **改接口就改文档**：任何 `pub` 签名变化、错误语义变化、失败模式变化，都要落到
   对应篇目；文档里引用源码用 `src/路径.rs:行号` 的写法，方便点开。
2. **已知差距要写明**：不能在文档里把「尚未实现」写成「已实现」。当前差距集中在
   [usage.md](usage.md#已知限制)与各篇的「已知差距」小节。
3. **数字要实测**：测试计数、水位语义、错误码这类可验证的事实，以本地
   `cargo test` / `cargo run --bin ains-demo` 的结果为准，不引用旧会话里的数字。
4. 文档本身要能被 `grep` 到：跨篇引用用相对链接，不用「上文」这类指代。

## 一分钟速查

| 我想…… | 去哪里 |
|---|---|
| 写文档 + 向量检索，最少几行？ | [usage.md](usage.md#最小程序) |
| 让查询知道索引是否追上了？ | [usage.md](usage.md#让查询带上完整性标记)、[index.md](index.md#新鲜度与覆盖) |
| 调大 I/O 并发、处理背压？ | [hal.md](hal.md#reactor)、[usage.md](usage.md#io-并发与背压) |
| 带掉电故障跑一致性测试？ | [platform-testing.md](platform-testing.md#故障注入)、[usage.md](usage.md#崩溃恢复测试) |
| 知道哪些设计还没落地？ | [usage.md](usage.md#已知限制) |
