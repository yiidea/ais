# 04 — 硬件直通路径

AINS 的核心性能承诺：**让引擎直接和硬件对话，跳过一切不必要的抽象层。** 本章描述读路径、写路径和内存分层的设计，以及与硬件的接口。

## 1. 目标与约束

| 指标 | 目标 |
|---|---|
| 4 KB 冷读 P50 / P99 | < 8 µs / < 30 µs（PCIe 4.0 NVMe，本地） |
| 写入 P99 | < 200 µs（含 fsync/持久化语义） |
| 内存带宽利用 | ≥ 70% 可用带宽（向量扫描） |
| 每 GB 内存可寻址向量 | ≥ 100 万（1024 维，int8 量化） |
| CPU 占用 | 向量扫描路径无系统调用、无 malloc |

### 约束

- 不修改内核模块（保持可部署于普通 Linux 云主机）。
- 在不支持 io_uring / SPDK 的环境自动回退到 POSIX/WAL，**行为一致，性能降级**。
- 引擎本身不依赖 GPU；GPU 与专用加速器是可选的加速通道。

## 2. 存储硬件抽象层（HAL）

```text
┌────────────────────────────────────────────┐
│              Storage HAL                    │
│  async_read / async_write / async_fsync     │
│  probe_capabilities()                       │
└──────────────┬─────────────────────────────┘
               │
   ┌───────────┼───────────┬───────────────┐
   ▼           ▼           ▼               ▼
io_uring    SPDK/AIO   POSIX fallback   Remote/Object
(内核旁路)  (用户态)    (WAL + pread)    (S3 兼容)
```

### 2.1 能力探测

启动时探测：

- `io_uring` 是否可用、是否支持 `IORING_SETUP_SQPOLL`、`IORING_OP_READ_FIXED`（注册缓冲区）。
- NVMe 设备队列深度、最优 IO 大小（`optimal_io_size`）、写放大提示。
- NUMA 拓扑与 HBM/CXL 可用性（`/sys/devices/system/node`）。
- 持久内存（`/dev/pmem*`）是否可用。

根据探测结果生成 **Hardware Profile**：

```json
{
  "path": "io_uring",
  "sqpoll": true,
  "registered_buffers": true,
  "optimal_io_size": 65536,
  "numa_node": 0,
  "device_latency_ns": {"read": 8200, "write": 14000},
  "vector_isa": "avx512vnni"
}
```

### 2.2 读路径（零拷贝）

```text
[执行器] → 找到目标对象位置（shard/list, offset）
        → io_uring_prep_read_fixed(注册缓冲区)
        → 提交 SQ ring → 内核/硬件完成
        → CQ ring 上收完成 → 直接交给算子（不再 copy）
```

关键点：

- **注册缓冲区**：启动时一次性分配并注册（`IORING_REGISTER_BUFFERS`），避免每次读的内核地址映射。
- **`SQPOLL` 模式**：减少系统调用；但在虚拟化/低 CPU 配额环境自动退避。
- **批量 IO**：一次提交多条读，利用 SSD 内部并行度；引擎根据 IO 队列深度动态调节 batch size。
- **直接 IO（O_DIRECT）**：日志与索引段文件绕开 page cache，避免双重缓存。

### 2.3 写路径

1. 在内存中构造语义记录（Memtable 同时插入索引）。
2. 构造 **write batch**（多个记录合并成块），向 HAL 提交。
3. 使用 `IORING_OP_WRITE` 异步写；记录级 CRC。
4. 根据域的持久化语义：
   - `durable`：等待 `fsync` 完成（io_uring 的 `IORING_OP_FSYNC`）。
   - `relaxed`：不等持久化，靠副本冗余。
5. 完成回执后分配 epoch。

### 2.4 索引构建通道

索引构建（HNSW 增量插入、段合并）也走 HAL，但可以使用 **独立队列**，避免和前台读竞争。构建线程可在 NUMA 节点上绑定，并使用大页内存。

## 3. 内存分层

### 3.1 三层结构

| 层 | 介质 | 内容 | 淘汰策略 |
|---|---|---|---|
| L0 热 | HBM/DRAM（大页） | Memtable、HNSW 上层图节点、高频索引、缓存块 | LRU + 访问频率 |
| L1 温 | CXL（如有）/ 本地 NVMe | 完整索引段、最近检索对象 | ARC |
| L2 冷 | 远程对象存储 | 原始对象、冷索引段 | 按需 |

### 3.2 索引常驻策略

- 索引的「上层图」与「热段」常驻内存；底层段按需加载。
- **路径感知预载**：对高访问的语义子空间预取索引段（学习访问模式）。
- 段的大小按硬件 `optimal_io_size` 对齐（默认 64 KB），实现一次 IO 取一个完整段块。

### 3.3 大页与内存治理

- 所有内存储备使用 `mmap(MAP_HUGETLB)` 分配 2 MB 大页，降低 TLB miss。
- 内存管理为 **arena + slab**，避免请求路径上的 malloc。
- 最大内存占用量由域配置限制；超限触发背压（拒写而非 OOM）。

## 4. 向量计算加速

### 4.1 指令级优化

- 距离函数直接使用 `AVX-512 VNNI` / `NEON dotprod` / Apple `AMX`。
- 量化（PQ/SQ）解码与距离计算融合在一个循环。
- **批量扫描**：一次扫描 16 个量化向量，使用 SIMD 累积器。

### 4.2 内存带宽友好的布局

- 向量按 **SoA（struct of arrays）** 存储，而不是 AoS，便于 SIMD。
- 对 int8 量化向量，采用 32 byte 对齐，单次 cache line 取 2 个向量的片段。
- 避免在扫描循环中跨 NUMA 节点访问：按 NUMA 节点分区向量段。

### 4.3 可选加速器

- 若检测到 GPU / NPU，可通过插件提供 **copy-once** 的批量距离计算（一次上传，批量查询），适合高 QPS 场景。
- 设计上不强制依赖，作为可选后端。

## 5. 内核旁路与安全

- **io_uring 安全**：只允许引擎进程使用，所有缓冲区注册在引擎内部；使用 `IORING_REGISTER_RESTRICTIONS` 限制可操作的 opcode。
- **SPDK**：仅在专用部署（独占 NVMe 队列）时启用，需明确警告设备独占。
- **直接 IO 的缓存一致性**：所有共享索引数据通过引擎自身的内存版本管理，避免依赖内核页缓存的隐式一致性。

## 6. 隔离与 QoS

- 前台读与后台重组走不同 HAL 队列。
- 每个域可设置 **IO 权重**；I/O 调度使用 token bucket 限流。
- 读写分离：写路径使用更小的 batch、立即持久化；读路径可以更大 batch、允许适度乱序。
- 在共享云环境中，检测 noisy neighbor（延迟 P99 上升）自动降级到保守模式。

## 7. 性能预算（建议 SLO）

| 路径 | v1 预算 | 优化方向 |
|---|---|---|
| 意图分类 | 5 ms（本地） | 蒸馏模型 / 缓存 |
| 规划（规则） | 1 ms | 计划缓存 |
| 规划（LLM） | 150 ms | 本地小模型、异步预规划 |
| 稀疏召回（100 万文档） | 15 ms | SSD 批量 IO |
| 向量召回（1000 万，HNSW） | 30 ms | 索引常驻 + batch |
| 融合 + 去重 | 5 ms | SIMD |
| 打包认知块 | 2 ms | 流式 |
| 合计（Recall 路径） | ~60 ms（不含 LLM） | v3 目标 < 30 ms |

## 8. 回退路径矩阵

| 能力 | 有 | 无时回退 |
|---|---|---|
| io_uring | 全功能 | `preadv2/pwritev` + epoll |
| SPDK | 用户态队列 | io_uring |
| HugeTLB | 大页 | 普通 mmap |
| AVX-512 | VNNI 距离 | AVX2 / NEON / 标量 |
| PIM/CXL | 内存语义扩展 | 普通 DRAM |
| 本地 NVMe | 直连 | 本地 NVMe 走 POSIX / 远程 |

**原则**：回退时行为一致，仅性能降级；所有功能都必须能在纯软环境里验证。
