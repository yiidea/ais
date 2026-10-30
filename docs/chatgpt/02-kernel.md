# AI-Native Storage：核心存储内核深入设计

这次重点深入分析前面提出的三个核心问题：

1. Object、Extent、Physical Block 三层数据组织及映射关系。
2. DRAM、NVMe、GPU HBM 的统一资源管理和数据迁移机制。
3. Object Version、MVCC、WAL、Checkpoint 的一致性设计。

这三个问题实际上不是独立的三个模块，而是一个完整的存储内核问题。

它们之间存在严格的依赖关系：

- Object 决定数据以什么逻辑形式存在。
- Extent 决定数据如何组织成可以管理、分配和迁移的物理存储单元。
- Physical Block 决定数据最终如何映射到硬件。
- Hardware Manager 决定数据应该放在哪里，以及如何访问。
- MVCC 决定不同事务看到哪个数据版本。
- WAL 决定数据修改如何持久化。
- Checkpoint 决定如何建立可恢复的持久化状态。

因此，我们先确定整体设计，再分别深入每一层。

# 一、三个核心问题的整体关系

## 1.1 重新定义存储内核

传统数据库的存储路径：

```
SQL
 |
 v
Executor
 |
 v
Tuple
 |
 v
Heap Page
 |
 v
Buffer Pool
 |
 v
File
 |
 v
Disk Block
```

它的核心思想是：

数据以 Page 为中心组织，Buffer Manager 管理 Page 在内存和磁盘之间的状态。

AI-Native Storage 应该改变这个中心。

新的存储路径：

```
AI Intent
    |
    v
Execution Task
    |
    v
AI Object
    |
    v
Object Version
    |
    v
Logical Extent
    |
    v
Physical Extent
    |
    v
Physical Block
    |
    v
Hardware Resource
```

这里有一个重要区别：

传统数据库中的 Page 同时承担了数据组织、缓存管理和 I/O 单元等职责。

新架构需要把这些职责拆开。

| 传统概念          | 新架构概念               | 主要职责    |
| ------------- | ------------------- | ------- |
| Tuple         | Object              | 逻辑数据对象  |
| Heap Page     | Logical Extent      | 逻辑数据组织  |
| File Block    | Physical Extent     | 物理分配单元  |
| Disk Block    | Physical Block      | 硬件存储单元  |
| Buffer Pool   | Memory Tier Manager | 多级内存管理  |
| Page Table    | Extent Mapping      | 逻辑到物理映射 |
| Tuple Version | Object Version      | 数据版本管理  |
| WAL           | Object WAL          | 数据修改持久化 |

需要注意，Logical Extent 和 Physical Extent 是本设计定义的两个概念，不是硬件或操作系统中的既有标准术语。

## 1.2 总体内核架构

建议将核心存储内核拆成七个模块：

\#chatgpt-mermaid-\_r_b53\_{font-family:-apple-system-body,ui-sans-serif,-apple-system,system-ui,"Segoe UI",Helvetica,"Apple Color Emoji",Arial,sans-serif,"Segoe UI Emoji","Segoe UI Symbol";font-size:16px;fill:rgb(13, 13, 13);}@keyframes edge-animation-frame{from{stroke-dashoffset:0;}}@keyframes dash{to{stroke-dashoffset:0;}}#chatgpt-mermaid-\_r_b53\_ .edge-animation-slow{stroke-dasharray:9,5!important;stroke-dashoffset:900;animation:dash 50s linear infinite;stroke-linecap:round;}#chatgpt-mermaid-\_r_b53\_ .edge-animation-fast{stroke-dasharray:9,5!important;stroke-dashoffset:900;animation:dash 20s linear infinite;stroke-linecap:round;}#chatgpt-mermaid-\_r_b53\_ .error-icon{fill:rgb(243, 243, 243);}#chatgpt-mermaid-\_r_b53\_ .error-text{fill:rgb(13, 13, 13);stroke:rgb(13, 13, 13);}#chatgpt-mermaid-\_r_b53\_ .edge-thickness-normal{stroke-width:1px;}#chatgpt-mermaid-\_r_b53\_ .edge-thickness-thick{stroke-width:3.5px;}#chatgpt-mermaid-\_r_b53\_ .edge-pattern-solid{stroke-dasharray:0;}#chatgpt-mermaid-\_r_b53\_ .edge-thickness-invisible{stroke-width:0;fill:none;}#chatgpt-mermaid-\_r_b53\_ .edge-pattern-dashed{stroke-dasharray:3;}#chatgpt-mermaid-\_r_b53\_ .edge-pattern-dotted{stroke-dasharray:2;}#chatgpt-mermaid-\_r_b53\_ .marker{fill:rgb(143, 143, 143);stroke:rgb(143, 143, 143);}#chatgpt-mermaid-\_r_b53\_ .marker.cross{stroke:rgb(143, 143, 143);}#chatgpt-mermaid-\_r_b53\_ svg{font-family:-apple-system-body,ui-sans-serif,-apple-system,system-ui,"Segoe UI",Helvetica,"Apple Color Emoji",Arial,sans-serif,"Segoe UI Emoji","Segoe UI Symbol";font-size:16px;}#chatgpt-mermaid-\_r_b53\_ p{margin:0;}#chatgpt-mermaid-\_r_b53\_ .label{font-family:-apple-system-body,ui-sans-serif,-apple-system,system-ui,"Segoe UI",Helvetica,"Apple Color Emoji",Arial,sans-serif,"Segoe UI Emoji","Segoe UI Symbol";color:rgb(13, 13, 13);}#chatgpt-mermaid-\_r_b53\_ .cluster-label text{fill:rgb(13, 13, 13);}#chatgpt-mermaid-\_r_b53\_ .cluster-label span{color:rgb(13, 13, 13);}#chatgpt-mermaid-\_r_b53\_ .cluster-label span p{background-color:transparent;}#chatgpt-mermaid-\_r_b53\_ .label text,#chatgpt-mermaid-\_r_b53\_ span{fill:rgb(13, 13, 13);color:rgb(13, 13, 13);}#chatgpt-mermaid-\_r_b53\_ .node rect,#chatgpt-mermaid-\_r_b53\_ .node circle,#chatgpt-mermaid-\_r_b53\_ .node ellipse,#chatgpt-mermaid-\_r_b53\_ .node polygon,#chatgpt-mermaid-\_r_b53\_ .node path{fill:rgb(226, 242, 230);stroke:rgb(107, 198, 127);stroke-width:1px;}#chatgpt-mermaid-\_r_b53\_ .rough-node .label text,#chatgpt-mermaid-\_r_b53\_ .node .label text,#chatgpt-mermaid-\_r_b53\_ .image-shape .label,#chatgpt-mermaid-\_r_b53\_ .icon-shape .label{text-anchor:middle;}#chatgpt-mermaid-\_r_b53\_ .node .katex path{fill:#000;stroke:#000;stroke-width:1px;}#chatgpt-mermaid-\_r_b53\_ .rough-node .label,#chatgpt-mermaid-\_r_b53\_ .node .label,#chatgpt-mermaid-\_r_b53\_ .image-shape .label,#chatgpt-mermaid-\_r_b53\_ .icon-shape .label{text-align:center;}#chatgpt-mermaid-\_r_b53\_ .node.clickable{cursor:pointer;}#chatgpt-mermaid-\_r_b53\_ .root .anchor path{fill:rgb(143, 143, 143)!important;stroke-width:0;stroke:rgb(143, 143, 143);}#chatgpt-mermaid-\_r_b53\_ .arrowheadPath{fill:rgb(143, 143, 143);}#chatgpt-mermaid-\_r_b53\_ .edgePath .path{stroke:rgb(143, 143, 143);stroke-width:1px;}#chatgpt-mermaid-\_r_b53\_ .flowchart-link{stroke:rgb(143, 143, 143);fill:none;}#chatgpt-mermaid-\_r_b53\_ .edgeLabel{background-color:rgb(252, 252, 252);text-align:center;}#chatgpt-mermaid-\_r_b53\_ .edgeLabel p{background-color:rgb(252, 252, 252);}#chatgpt-mermaid-\_r_b53\_ .edgeLabel rect{opacity:0.5;background-color:rgb(252, 252, 252);fill:rgb(252, 252, 252);}#chatgpt-mermaid-\_r_b53\_ .labelBkg{background-color:rgba(252, 252, 252, 0.5);}#chatgpt-mermaid-\_r_b53\_ .cluster rect{fill:rgb(243, 243, 243);stroke:rgba(0, 0, 0, 0.1);stroke-width:1px;}#chatgpt-mermaid-\_r_b53\_ .cluster text{fill:rgb(13, 13, 13);}#chatgpt-mermaid-\_r_b53\_ .cluster span{color:rgb(13, 13, 13);}#chatgpt-mermaid-\_r_b53\_ div.mermaidTooltip{position:absolute;text-align:center;max-width:200px;padding:2px;font-family:-apple-system-body,ui-sans-serif,-apple-system,system-ui,"Segoe UI",Helvetica,"Apple Color Emoji",Arial,sans-serif,"Segoe UI Emoji","Segoe UI Symbol";font-size:12px;background:rgb(243, 243, 243);border:1px solid rgba(0, 0, 0, 0.1);border-radius:2px;pointer-events:none;z-index:100;}#chatgpt-mermaid-\_r_b53\_ .flowchartTitleText{text-anchor:middle;font-size:18px;fill:rgb(13, 13, 13);}#chatgpt-mermaid-\_r_b53\_ rect.text{fill:none;stroke-width:0;}#chatgpt-mermaid-\_r_b53\_ .icon-shape,#chatgpt-mermaid-\_r_b53\_ .image-shape{background-color:rgb(252, 252, 252);text-align:center;}#chatgpt-mermaid-\_r_b53\_ .icon-shape p,#chatgpt-mermaid-\_r_b53\_ .image-shape p{background-color:rgb(252, 252, 252);padding:2px;}#chatgpt-mermaid-\_r_b53\_ .icon-shape .label rect,#chatgpt-mermaid-\_r_b53\_ .image-shape .label rect{opacity:0.5;background-color:rgb(252, 252, 252);fill:rgb(252, 252, 252);}#chatgpt-mermaid-\_r_b53\_ .label-icon{display:inline-block;height:1em;overflow:visible;vertical-align:-0.125em;}#chatgpt-mermaid-\_r_b53\_ .node .label-icon path{fill:currentColor;stroke:revert;stroke-width:revert;}#chatgpt-mermaid-\_r_b53\_ .node .neo-node{stroke:rgb(107, 198, 127);}#chatgpt-mermaid-\_r_b53\_ [data-look="neo"].node rect,#chatgpt-mermaid-\_r_b53\_ [data-look="neo"].cluster rect,#chatgpt-mermaid-\_r_b53\_ [data-look="neo"].node polygon{stroke:url(#chatgpt-mermaid-\_r_b53\_-gradient);filter:drop-shadow( 1px 2px 2px rgba(185,185,185,1));}#chatgpt-mermaid-\_r_b53\_ [data-look="neo"].swimlane.cluster rect{filter:none;}#chatgpt-mermaid-\_r_b53\_ [data-look="neo"].node path{stroke:url(#chatgpt-mermaid-\_r_b53\_-gradient);stroke-width:1px;}#chatgpt-mermaid-\_r_b53\_ [data-look="neo"].node .outer-path{filter:drop-shadow( 1px 2px 2px rgba(185,185,185,1));}#chatgpt-mermaid-\_r_b53\_ [data-look="neo"].node .neo-line path{stroke:rgb(107, 198, 127);filter:none;}#chatgpt-mermaid-\_r_b53\_ [data-look="neo"].node circle{stroke:url(#chatgpt-mermaid-\_r_b53\_-gradient);filter:drop-shadow( 1px 2px 2px rgba(185,185,185,1));}#chatgpt-mermaid-\_r_b53\_ [data-look="neo"].node circle .state-start{fill:#000000;}#chatgpt-mermaid-\_r_b53\_ [data-look="neo"].icon-shape .icon{fill:url(#chatgpt-mermaid-\_r_b53\_-gradient);filter:drop-shadow( 1px 2px 2px rgba(185,185,185,1));}#chatgpt-mermaid-\_r_b53\_ [data-look="neo"].icon-shape .icon-neo path{stroke:url(#chatgpt-mermaid-\_r_b53\_-gradient);filter:drop-shadow( 1px 2px 2px rgba(185,185,185,1));}#chatgpt-mermaid-\_r_b53\_ .node text{font-size:14px;font-weight:600;letter-spacing:normal;fill:rgb(0, 105, 42);}#chatgpt-mermaid-\_r_b53\_ .edgeLabels text{font-size:13px;font-weight:600;letter-spacing:-0.08px;fill:rgb(0, 105, 42);}#chatgpt-mermaid-\_r_b53\_ .node tspan[font-weight="normal"],#chatgpt-mermaid-\_r_b53\_ .edgeLabels tspan[font-weight="normal"]{font-weight:600;}#chatgpt-mermaid-\_r_b53\_ .edgeLabel .label rect{opacity:1;rx:13px;ry:13px;fill:rgb(237, 250, 242);stroke:rgb(166, 211, 184);stroke-width:1px;}#chatgpt-mermaid-\_r_b53\_ .node rect,#chatgpt-mermaid-\_r_b53\_ .node circle,#chatgpt-mermaid-\_r_b53\_ .node ellipse,#chatgpt-mermaid-\_r_b53\_ .node polygon,#chatgpt-mermaid-\_r_b53\_ .node path{fill:rgb(217, 244, 228);stroke:rgba(0, 0, 0, 0.1);stroke-width:1px;}#chatgpt-mermaid-\_r_b53\_ .node rect{rx:16px;ry:16px;}#chatgpt-mermaid-\_r_b53\_ .node.mermaid-decision .label-container{fill:rgb(237, 250, 242);stroke:rgb(195, 220, 205);stroke-dasharray:2,2;}#chatgpt-mermaid-\_r_b53\_ .edgePaths .flowchart-link{stroke:rgb(143, 143, 143);stroke-width:1px;stroke-linecap:round;stroke-linejoin:round;}#chatgpt-mermaid-\_r_b53\_ .marker{fill:rgb(143, 143, 143);stroke:rgb(143, 143, 143);}#chatgpt-mermaid-\_r_b53\_ :root{--mermaid-font-family:-apple-system-body,ui-sans-serif,-apple-system,system-ui,"Segoe UI",Helvetica,"Apple Color Emoji",Arial,sans-serif,"Segoe UI Emoji","Segoe UI Symbol";}Consistency LayerHardware Resource LayerPhysical Storage LayerLogical Storage LayerLogical Data LayerMVCC ManagerWAL ManagerCheckpoint ManagerRecovery ManagerHardware Resource ManagerMemory Tier ManagerI/O ManagerPhysical Extent ManagerBlock ManagerLogical Extent ManagerExtent Mapping ManagerObject ManagerObject Version Manager

这里需要注意，图中为了突出数据访问路径，没有画出全部依赖。

例如 MVCC、WAL 和物理存储管理实际上需要相互协作，不能简单理解成单向调用关系。

# 二、问题一：Object、Extent、Physical Block 三层数据组织

这是存储引擎最基础的设计。

我的建议是采用：

Object → Logical Extent → Physical Extent → Physical Block

四层模型。

其中前三层属于存储引擎的管理抽象，最后一层是硬件访问抽象。

## 2.1 第一层：Object——逻辑数据对象

Object 是用户和 AI 系统能够理解的最小逻辑数据实体。

例如：

```
Object
  |
  +-- Document
  |
  +-- Vector
  |
  +-- Tensor
  |
  +-- Graph
  |
  +-- Memory
  |
  +-- Event
  |
  +-- Media
```

这里需要避免一个错误：

不能把 Object 简单理解为一个 Blob。

如果所有数据都使用 Blob 存储，那么系统只不过是增加了一层对象元数据的文件系统。

不同类型的数据需要不同的物理组织方式。

例如：

| Object 类型 | Payload 组织      | 典型访问方式       |
| --------- | --------------- | ------------ |
| Document  | 变长数据块           | 随机读取、全文扫描    |
| Vector    | 定长连续向量          | 批量随机读取、相似度计算 |
| Tensor    | 多维分块            | 连续读取、切片读取    |
| Graph     | Node / Edge 分区  | 邻接访问、路径遍历    |
| Memory    | 时间和语义分组         | 相似度、时间过滤     |
| Event     | Append-only     | 顺序扫描         |
| Media     | Chunk / Segment | 范围读取、流式读取    |

因此，Object 应该由统一的 Header 和专用 Payload 组成。

### Object Header 设计

建议第一版采用如下结构：

```
struct ObjectHeader {
    object_id: ObjectId,

    object_type: ObjectType,

    tenant_id: TenantId,

    logical_size: u64,

    current_version: VersionId,

    schema_id: SchemaId,

    flags: u32,

    create_ts: u64,

    metadata_ref: MetadataRef,

    payload_ref: PayloadRef,
}
```

这里有几个设计重点。

第一，Object ID 必须稳定。

对象的逻辑身份不能因为：

- NVMe 迁移到另一块 SSD。
- DRAM 中缓存位置发生变化。
- GPU HBM 中产生副本。
- Compaction 重新组织数据。

而发生改变。

因此 Object ID 不能使用物理地址。

建议采用：

```
ObjectId = DatabaseId + ObjectSequence
```

或者采用全局唯一 ID。

第二，Object Header 不应该包含全部数据版本。

当前版本只是一个便于访问的提示。

真正的版本历史应该由 Version Manager 维护。

否则每次修改对象，都需要频繁更新对象头部。

第三，Object Metadata 和 Payload 分离。

例如：

```
Object 1001
    |
    +-- Header
    |
    +-- Metadata
    |
    +-- Payload
```

Metadata 可以存放：

- 文档名称。
- 数据来源。
- 模型版本。
- Embedding 模型。
- Tensor Shape。
- Graph 类型。
- 创建时间。
- 访问权限。
- 语义标签。

Payload 则存放实际内容。

这样可以在不读取大型 Payload 的情况下完成大量对象过滤。

## 2.2 Object 与多模态数据的关系

一个逻辑 Object 可以拥有多个数据分量。

例如一个源码文件：

```
Object: PostgreSQL/reorderbuffer.c
    |
    +-- Component 1: Source Document
    |
    +-- Component 2: Embedding
    |
    +-- Component 3: Function Metadata
    |
    +-- Component 4: Function Graph
    |
    +-- Component 5: Historical Events
```

建议引入 Component 概念：

```
struct ObjectComponent {
    component_id: ComponentId,

    object_id: ObjectId,

    component_type: ComponentType,

    extent_list: Vec<ExtentId>,

    logical_length: u64,

    checksum: u64,
}
```

这样可以实现：

- 一个对象多个 Payload。
- 不同 Component 使用不同存储布局。
- 不同 Component 使用不同硬件资源。
- 不同 Component 可以独立缓存。
- 不同 Component 可以独立迁移。

例如源码文本放在 NVMe，而 Embedding 放在 DRAM。

它们仍然属于同一个逻辑 Object。

这是 AI-Native Storage 非常重要的设计能力。

## 2.3 第二层：Logical Extent——逻辑存储组织单元

Object 解决逻辑数据问题，但不能直接承担物理存储分配。

例如一个 500 MB 的 Tensor：

- 不可能每次读取都把整个 Tensor 加载进 DRAM。
- 不可能每次访问都通过 Object ID 读取完整内容。
- 不同计算任务可能只需要其中一个切片。

因此需要引入 Logical Extent。

Logical Extent 的作用是：

把一个 Object 的 Payload 切分成若干可以独立定位、读取、缓存和迁移的逻辑数据区域。

例如：

```
Tensor Object 1001
        |
        +-- Logical Extent 0
        |
        +-- Logical Extent 1
        |
        +-- Logical Extent 2
        |
        +-- Logical Extent 3
        |
        +-- Logical Extent 4
```

### Logical Extent 数据结构

```
struct LogicalExtent {
    extent_id: ExtentId,

    object_id: ObjectId,

    component_id: ComponentId,

    logical_offset: u64,

    logical_length: u64,

    layout_type: LayoutType,

    extent_version: VersionId,

    physical_mapping: MappingId,
}
```

Logical Extent 需要支持：

1. 独立读取。
2. 独立写入。
3. 独立版本管理。
4. 独立物理映射。
5. 独立缓存。
6. 独立迁移。

### 不同数据类型如何切分 Extent？

这里不能使用完全相同的切分方式。

建议设计多种 Extent Layout。

| Layout          | 适用对象     | 设计特点         |
| --------------- | -------- | ------------ |
| Fixed Extent    | Vector   | 固定大小，方便批量检索  |
| Variable Extent | Document | 支持变长内容       |
| Tensor Tile     | Tensor   | 按维度和计算访问模式分块 |
| Graph Partition | Graph    | 尽量保持邻接数据局部性  |
| Append Extent   | Event    | 顺序追加写入       |
| Media Chunk     | Media    | 按时间或内容片段组织   |

例如 Tensor：

```
Tensor: [4096, 4096]

       4096
   ┌───────────────┐
   │  T0  │  T1    │
   ├──────┼────────┤
   │  T2  │  T3    │
   └───────────────┘
        4096
```

可以把每个 Tile 作为独立 Logical Extent。

GPU 执行时只读取需要的 Tile。

这里的 Tile 大小不应该固定写死，而应该根据 GPU 显存容量、计算任务、数据访问模式进行配置。

### Extent 大小如何确定？

我建议不要规定一个全局统一的 Extent 大小。

可以分成三种粒度：

| 类型            | 初始设计范围      | 目的                |
| ------------- | ----------- | ----------------- |
| Small Extent  | 16–64 KB    | 小对象、元数据           |
| Medium Extent | 256 KB–2 MB | 向量、文档             |
| Large Extent  | 4–64 MB     | Tensor、Media、批量数据 |

这些只是初始实验参数，不是通用最优值。

还要区分：

- Logical Extent 大小。
- Physical Extent 分配大小。
- I/O Request 大小。
- Hardware Block 大小。

四者不需要相等。

例如一个 2 MB Logical Extent，可以由多个 NVMe 物理分配单元组成，也可以被拆成多个 128 KB I/O Request。

这是为了让逻辑布局不被硬件扇区、设备块大小或 I/O 队列限制。

## 2.4 第三层：Physical Extent——物理存储分配单元

Logical Extent 描述的是逻辑数据区域。

Physical Extent 则负责真正的资源分配。

例如：

```
Logical Extent 1001
        |
        v
Physical Extent 5001
        |
        v
NVMe Device 0
        |
        v
LBA Range 100000 - 103999
```

如果后续需要迁移：

```
Logical Extent 1001
        |
        +-- Old Mapping
        |      |
        |      +-- NVMe 0
        |
        +-- New Mapping
               |
               +-- DRAM Cache
               +-- NVMe 1
```

注意：这里的 DRAM Cache 可以是可丢弃副本，而 NVMe 1 可以是新的持久化位置。不能把两者视为同等可靠的存储副本。

建议定义：

```
struct PhysicalExtent {
    physical_extent_id: PhysicalExtentId,

    device_id: DeviceId,

    device_offset: u64,

    allocated_length: u64,

    resource_type: ResourceType,

    durability: DurabilityLevel,

    checksum: u64,

    allocation_state: AllocationState,
}
```

Physical Extent 需要支持：

- 空间分配。
- 空间回收。
- 设备定位。
- 对齐管理。
- 校验。
- 迁移。
- 故障检测。

### 一个关键设计：逻辑映射与物理映射必须分离

不能直接在 Logical Extent 中硬编码 NVMe LBA。

否则设备迁移、数据压缩、Compaction、数据复制都会影响上层逻辑。

建议采用独立 Mapping Table：

```
Logical Extent ID
        |
        v
Extent Mapping Table
        |
        +-- Mapping Version 1
        |      |
        |      +-- Physical Extent A
        |
        +-- Mapping Version 2
               |
               +-- Physical Extent B
```

这样可以通过更新 Mapping 来改变数据物理位置，而不改变 Object ID。

这里还需要引入 Mapping Version，因为并发读取和迁移可能同时发生。

读取操作必须使用一个稳定的映射版本，不能在一次读取过程中发生物理地址切换。

## 2.5 第四层：Physical Block——硬件访问单元

Physical Block 是最底层的数据访问单位。

对于 NVMe SSD：

```
Physical Extent
       |
       v
NVMe Namespace
       |
       v
LBA
       |
       v
NVMe Controller
       |
       v
Flash
```

对于 DRAM：

```
Memory Extent
      |
      v
Virtual Address
      |
      v
NUMA Node
      |
      v
Physical Memory
```

对于 GPU：

```
GPU Extent
     |
     v
GPU Virtual Address
     |
     v
GPU Memory Manager
     |
     v
GPU HBM
```

这里必须明确：

不同硬件的物理地址空间完全不同，不能使用一个统一的裸地址指针表示所有物理位置。

应该采用统一描述符：

```
enum PhysicalLocation {
    Nvme {
        device_id: u32,
        namespace_id: u32,
        lba: u64,
        length: u64,
    },

    Dram {
        numa_node: u32,
        allocation_id: u64,
        offset: u64,
    },

    Gpu {
        device_id: u32,
        allocation_id: u64,
        offset: u64,
    },
}
```

这只是逻辑描述。实际访问时必须通过对应的硬件 Backend。

## 2.6 完整的数据组织关系

以一个 AI Tensor 为例：

\#chatgpt-mermaid-\_r_b84\_{font-family:-apple-system-body,ui-sans-serif,-apple-system,system-ui,"Segoe UI",Helvetica,"Apple Color Emoji",Arial,sans-serif,"Segoe UI Emoji","Segoe UI Symbol";font-size:16px;fill:rgb(13, 13, 13);}@keyframes edge-animation-frame{from{stroke-dashoffset:0;}}@keyframes dash{to{stroke-dashoffset:0;}}#chatgpt-mermaid-\_r_b84\_ .edge-animation-slow{stroke-dasharray:9,5!important;stroke-dashoffset:900;animation:dash 50s linear infinite;stroke-linecap:round;}#chatgpt-mermaid-\_r_b84\_ .edge-animation-fast{stroke-dasharray:9,5!important;stroke-dashoffset:900;animation:dash 20s linear infinite;stroke-linecap:round;}#chatgpt-mermaid-\_r_b84\_ .error-icon{fill:rgb(243, 243, 243);}#chatgpt-mermaid-\_r_b84\_ .error-text{fill:rgb(13, 13, 13);stroke:rgb(13, 13, 13);}#chatgpt-mermaid-\_r_b84\_ .edge-thickness-normal{stroke-width:1px;}#chatgpt-mermaid-\_r_b84\_ .edge-thickness-thick{stroke-width:3.5px;}#chatgpt-mermaid-\_r_b84\_ .edge-pattern-solid{stroke-dasharray:0;}#chatgpt-mermaid-\_r_b84\_ .edge-thickness-invisible{stroke-width:0;fill:none;}#chatgpt-mermaid-\_r_b84\_ .edge-pattern-dashed{stroke-dasharray:3;}#chatgpt-mermaid-\_r_b84\_ .edge-pattern-dotted{stroke-dasharray:2;}#chatgpt-mermaid-\_r_b84\_ .marker{fill:rgb(143, 143, 143);stroke:rgb(143, 143, 143);}#chatgpt-mermaid-\_r_b84\_ .marker.cross{stroke:rgb(143, 143, 143);}#chatgpt-mermaid-\_r_b84\_ svg{font-family:-apple-system-body,ui-sans-serif,-apple-system,system-ui,"Segoe UI",Helvetica,"Apple Color Emoji",Arial,sans-serif,"Segoe UI Emoji","Segoe UI Symbol";font-size:16px;}#chatgpt-mermaid-\_r_b84\_ p{margin:0;}#chatgpt-mermaid-\_r_b84\_ .label{font-family:-apple-system-body,ui-sans-serif,-apple-system,system-ui,"Segoe UI",Helvetica,"Apple Color Emoji",Arial,sans-serif,"Segoe UI Emoji","Segoe UI Symbol";color:rgb(13, 13, 13);}#chatgpt-mermaid-\_r_b84\_ .cluster-label text{fill:rgb(13, 13, 13);}#chatgpt-mermaid-\_r_b84\_ .cluster-label span{color:rgb(13, 13, 13);}#chatgpt-mermaid-\_r_b84\_ .cluster-label span p{background-color:transparent;}#chatgpt-mermaid-\_r_b84\_ .label text,#chatgpt-mermaid-\_r_b84\_ span{fill:rgb(13, 13, 13);color:rgb(13, 13, 13);}#chatgpt-mermaid-\_r_b84\_ .node rect,#chatgpt-mermaid-\_r_b84\_ .node circle,#chatgpt-mermaid-\_r_b84\_ .node ellipse,#chatgpt-mermaid-\_r_b84\_ .node polygon,#chatgpt-mermaid-\_r_b84\_ .node path{fill:rgb(226, 242, 230);stroke:rgb(107, 198, 127);stroke-width:1px;}#chatgpt-mermaid-\_r_b84\_ .rough-node .label text,#chatgpt-mermaid-\_r_b84\_ .node .label text,#chatgpt-mermaid-\_r_b84\_ .image-shape .label,#chatgpt-mermaid-\_r_b84\_ .icon-shape .label{text-anchor:middle;}#chatgpt-mermaid-\_r_b84\_ .node .katex path{fill:#000;stroke:#000;stroke-width:1px;}#chatgpt-mermaid-\_r_b84\_ .rough-node .label,#chatgpt-mermaid-\_r_b84\_ .node .label,#chatgpt-mermaid-\_r_b84\_ .image-shape .label,#chatgpt-mermaid-\_r_b84\_ .icon-shape .label{text-align:center;}#chatgpt-mermaid-\_r_b84\_ .node.clickable{cursor:pointer;}#chatgpt-mermaid-\_r_b84\_ .root .anchor path{fill:rgb(143, 143, 143)!important;stroke-width:0;stroke:rgb(143, 143, 143);}#chatgpt-mermaid-\_r_b84\_ .arrowheadPath{fill:rgb(143, 143, 143);}#chatgpt-mermaid-\_r_b84\_ .edgePath .path{stroke:rgb(143, 143, 143);stroke-width:1px;}#chatgpt-mermaid-\_r_b84\_ .flowchart-link{stroke:rgb(143, 143, 143);fill:none;}#chatgpt-mermaid-\_r_b84\_ .edgeLabel{background-color:rgb(252, 252, 252);text-align:center;}#chatgpt-mermaid-\_r_b84\_ .edgeLabel p{background-color:rgb(252, 252, 252);}#chatgpt-mermaid-\_r_b84\_ .edgeLabel rect{opacity:0.5;background-color:rgb(252, 252, 252);fill:rgb(252, 252, 252);}#chatgpt-mermaid-\_r_b84\_ .labelBkg{background-color:rgba(252, 252, 252, 0.5);}#chatgpt-mermaid-\_r_b84\_ .cluster rect{fill:rgb(243, 243, 243);stroke:rgba(0, 0, 0, 0.1);stroke-width:1px;}#chatgpt-mermaid-\_r_b84\_ .cluster text{fill:rgb(13, 13, 13);}#chatgpt-mermaid-\_r_b84\_ .cluster span{color:rgb(13, 13, 13);}#chatgpt-mermaid-\_r_b84\_ div.mermaidTooltip{position:absolute;text-align:center;max-width:200px;padding:2px;font-family:-apple-system-body,ui-sans-serif,-apple-system,system-ui,"Segoe UI",Helvetica,"Apple Color Emoji",Arial,sans-serif,"Segoe UI Emoji","Segoe UI Symbol";font-size:12px;background:rgb(243, 243, 243);border:1px solid rgba(0, 0, 0, 0.1);border-radius:2px;pointer-events:none;z-index:100;}#chatgpt-mermaid-\_r_b84\_ .flowchartTitleText{text-anchor:middle;font-size:18px;fill:rgb(13, 13, 13);}#chatgpt-mermaid-\_r_b84\_ rect.text{fill:none;stroke-width:0;}#chatgpt-mermaid-\_r_b84\_ .icon-shape,#chatgpt-mermaid-\_r_b84\_ .image-shape{background-color:rgb(252, 252, 252);text-align:center;}#chatgpt-mermaid-\_r_b84\_ .icon-shape p,#chatgpt-mermaid-\_r_b84\_ .image-shape p{background-color:rgb(252, 252, 252);padding:2px;}#chatgpt-mermaid-\_r_b84\_ .icon-shape .label rect,#chatgpt-mermaid-\_r_b84\_ .image-shape .label rect{opacity:0.5;background-color:rgb(252, 252, 252);fill:rgb(252, 252, 252);}#chatgpt-mermaid-\_r_b84\_ .label-icon{display:inline-block;height:1em;overflow:visible;vertical-align:-0.125em;}#chatgpt-mermaid-\_r_b84\_ .node .label-icon path{fill:currentColor;stroke:revert;stroke-width:revert;}#chatgpt-mermaid-\_r_b84\_ .node .neo-node{stroke:rgb(107, 198, 127);}#chatgpt-mermaid-\_r_b84\_ [data-look="neo"].node rect,#chatgpt-mermaid-\_r_b84\_ [data-look="neo"].cluster rect,#chatgpt-mermaid-\_r_b84\_ [data-look="neo"].node polygon{stroke:url(#chatgpt-mermaid-\_r_b84\_-gradient);filter:drop-shadow( 1px 2px 2px rgba(185,185,185,1));}#chatgpt-mermaid-\_r_b84\_ [data-look="neo"].swimlane.cluster rect{filter:none;}#chatgpt-mermaid-\_r_b84\_ [data-look="neo"].node path{stroke:url(#chatgpt-mermaid-\_r_b84\_-gradient);stroke-width:1px;}#chatgpt-mermaid-\_r_b84\_ [data-look="neo"].node .outer-path{filter:drop-shadow( 1px 2px 2px rgba(185,185,185,1));}#chatgpt-mermaid-\_r_b84\_ [data-look="neo"].node .neo-line path{stroke:rgb(107, 198, 127);filter:none;}#chatgpt-mermaid-\_r_b84\_ [data-look="neo"].node circle{stroke:url(#chatgpt-mermaid-\_r_b84\_-gradient);filter:drop-shadow( 1px 2px 2px rgba(185,185,185,1));}#chatgpt-mermaid-\_r_b84\_ [data-look="neo"].node circle .state-start{fill:#000000;}#chatgpt-mermaid-\_r_b84\_ [data-look="neo"].icon-shape .icon{fill:url(#chatgpt-mermaid-\_r_b84\_-gradient);filter:drop-shadow( 1px 2px 2px rgba(185,185,185,1));}#chatgpt-mermaid-\_r_b84\_ [data-look="neo"].icon-shape .icon-neo path{stroke:url(#chatgpt-mermaid-\_r_b84\_-gradient);filter:drop-shadow( 1px 2px 2px rgba(185,185,185,1));}#chatgpt-mermaid-\_r_b84\_ .node text{font-size:14px;font-weight:600;letter-spacing:normal;fill:rgb(0, 105, 42);}#chatgpt-mermaid-\_r_b84\_ .edgeLabels text{font-size:13px;font-weight:600;letter-spacing:-0.08px;fill:rgb(0, 105, 42);}#chatgpt-mermaid-\_r_b84\_ .node tspan[font-weight="normal"],#chatgpt-mermaid-\_r_b84\_ .edgeLabels tspan[font-weight="normal"]{font-weight:600;}#chatgpt-mermaid-\_r_b84\_ .edgeLabel .label rect{opacity:1;rx:13px;ry:13px;fill:rgb(237, 250, 242);stroke:rgb(166, 211, 184);stroke-width:1px;}#chatgpt-mermaid-\_r_b84\_ .node rect,#chatgpt-mermaid-\_r_b84\_ .node circle,#chatgpt-mermaid-\_r_b84\_ .node ellipse,#chatgpt-mermaid-\_r_b84\_ .node polygon,#chatgpt-mermaid-\_r_b84\_ .node path{fill:rgb(217, 244, 228);stroke:rgba(0, 0, 0, 0.1);stroke-width:1px;}#chatgpt-mermaid-\_r_b84\_ .node rect{rx:16px;ry:16px;}#chatgpt-mermaid-\_r_b84\_ .node.mermaid-decision .label-container{fill:rgb(237, 250, 242);stroke:rgb(195, 220, 205);stroke-dasharray:2,2;}#chatgpt-mermaid-\_r_b84\_ .edgePaths .flowchart-link{stroke:rgb(143, 143, 143);stroke-width:1px;stroke-linecap:round;stroke-linejoin:round;}#chatgpt-mermaid-\_r_b84\_ .marker{fill:rgb(143, 143, 143);stroke:rgb(143, 143, 143);}#chatgpt-mermaid-\_r_b84\_ :root{--mermaid-font-family:-apple-system-body,ui-sans-serif,-apple-system,system-ui,"Segoe UI",Helvetica,"Apple Color Emoji",Arial,sans-serif,"Segoe UI Emoji","Segoe UI Symbol";}Tensor Object: 1001Tensor ComponentLogical Extent 0Logical Extent 1Logical Extent 2Mapping Version 1Mapping Version 1Mapping Version 1Physical Extent APhysical Extent BPhysical Extent CNVMe 0NVMe 0NVMe 1

当 GPU 需要使用 Extent 0：

\#chatgpt-mermaid-\_r_b8b\_{font-family:-apple-system-body,ui-sans-serif,-apple-system,system-ui,"Segoe UI",Helvetica,"Apple Color Emoji",Arial,sans-serif,"Segoe UI Emoji","Segoe UI Symbol";font-size:16px;fill:rgb(13, 13, 13);}@keyframes edge-animation-frame{from{stroke-dashoffset:0;}}@keyframes dash{to{stroke-dashoffset:0;}}#chatgpt-mermaid-\_r_b8b\_ .edge-animation-slow{stroke-dasharray:9,5!important;stroke-dashoffset:900;animation:dash 50s linear infinite;stroke-linecap:round;}#chatgpt-mermaid-\_r_b8b\_ .edge-animation-fast{stroke-dasharray:9,5!important;stroke-dashoffset:900;animation:dash 20s linear infinite;stroke-linecap:round;}#chatgpt-mermaid-\_r_b8b\_ .error-icon{fill:rgb(243, 243, 243);}#chatgpt-mermaid-\_r_b8b\_ .error-text{fill:rgb(13, 13, 13);stroke:rgb(13, 13, 13);}#chatgpt-mermaid-\_r_b8b\_ .edge-thickness-normal{stroke-width:1px;}#chatgpt-mermaid-\_r_b8b\_ .edge-thickness-thick{stroke-width:3.5px;}#chatgpt-mermaid-\_r_b8b\_ .edge-pattern-solid{stroke-dasharray:0;}#chatgpt-mermaid-\_r_b8b\_ .edge-thickness-invisible{stroke-width:0;fill:none;}#chatgpt-mermaid-\_r_b8b\_ .edge-pattern-dashed{stroke-dasharray:3;}#chatgpt-mermaid-\_r_b8b\_ .edge-pattern-dotted{stroke-dasharray:2;}#chatgpt-mermaid-\_r_b8b\_ .marker{fill:rgb(143, 143, 143);stroke:rgb(143, 143, 143);}#chatgpt-mermaid-\_r_b8b\_ .marker.cross{stroke:rgb(143, 143, 143);}#chatgpt-mermaid-\_r_b8b\_ svg{font-family:-apple-system-body,ui-sans-serif,-apple-system,system-ui,"Segoe UI",Helvetica,"Apple Color Emoji",Arial,sans-serif,"Segoe UI Emoji","Segoe UI Symbol";font-size:16px;}#chatgpt-mermaid-\_r_b8b\_ p{margin:0;}#chatgpt-mermaid-\_r_b8b\_ .label{font-family:-apple-system-body,ui-sans-serif,-apple-system,system-ui,"Segoe UI",Helvetica,"Apple Color Emoji",Arial,sans-serif,"Segoe UI Emoji","Segoe UI Symbol";color:rgb(13, 13, 13);}#chatgpt-mermaid-\_r_b8b\_ .cluster-label text{fill:rgb(13, 13, 13);}#chatgpt-mermaid-\_r_b8b\_ .cluster-label span{color:rgb(13, 13, 13);}#chatgpt-mermaid-\_r_b8b\_ .cluster-label span p{background-color:transparent;}#chatgpt-mermaid-\_r_b8b\_ .label text,#chatgpt-mermaid-\_r_b8b\_ span{fill:rgb(13, 13, 13);color:rgb(13, 13, 13);}#chatgpt-mermaid-\_r_b8b\_ .node rect,#chatgpt-mermaid-\_r_b8b\_ .node circle,#chatgpt-mermaid-\_r_b8b\_ .node ellipse,#chatgpt-mermaid-\_r_b8b\_ .node polygon,#chatgpt-mermaid-\_r_b8b\_ .node path{fill:rgb(226, 242, 230);stroke:rgb(107, 198, 127);stroke-width:1px;}#chatgpt-mermaid-\_r_b8b\_ .rough-node .label text,#chatgpt-mermaid-\_r_b8b\_ .node .label text,#chatgpt-mermaid-\_r_b8b\_ .image-shape .label,#chatgpt-mermaid-\_r_b8b\_ .icon-shape .label{text-anchor:middle;}#chatgpt-mermaid-\_r_b8b\_ .node .katex path{fill:#000;stroke:#000;stroke-width:1px;}#chatgpt-mermaid-\_r_b8b\_ .rough-node .label,#chatgpt-mermaid-\_r_b8b\_ .node .label,#chatgpt-mermaid-\_r_b8b\_ .image-shape .label,#chatgpt-mermaid-\_r_b8b\_ .icon-shape .label{text-align:center;}#chatgpt-mermaid-\_r_b8b\_ .node.clickable{cursor:pointer;}#chatgpt-mermaid-\_r_b8b\_ .root .anchor path{fill:rgb(143, 143, 143)!important;stroke-width:0;stroke:rgb(143, 143, 143);}#chatgpt-mermaid-\_r_b8b\_ .arrowheadPath{fill:rgb(143, 143, 143);}#chatgpt-mermaid-\_r_b8b\_ .edgePath .path{stroke:rgb(143, 143, 143);stroke-width:1px;}#chatgpt-mermaid-\_r_b8b\_ .flowchart-link{stroke:rgb(143, 143, 143);fill:none;}#chatgpt-mermaid-\_r_b8b\_ .edgeLabel{background-color:rgb(252, 252, 252);text-align:center;}#chatgpt-mermaid-\_r_b8b\_ .edgeLabel p{background-color:rgb(252, 252, 252);}#chatgpt-mermaid-\_r_b8b\_ .edgeLabel rect{opacity:0.5;background-color:rgb(252, 252, 252);fill:rgb(252, 252, 252);}#chatgpt-mermaid-\_r_b8b\_ .labelBkg{background-color:rgba(252, 252, 252, 0.5);}#chatgpt-mermaid-\_r_b8b\_ .cluster rect{fill:rgb(243, 243, 243);stroke:rgba(0, 0, 0, 0.1);stroke-width:1px;}#chatgpt-mermaid-\_r_b8b\_ .cluster text{fill:rgb(13, 13, 13);}#chatgpt-mermaid-\_r_b8b\_ .cluster span{color:rgb(13, 13, 13);}#chatgpt-mermaid-\_r_b8b\_ div.mermaidTooltip{position:absolute;text-align:center;max-width:200px;padding:2px;font-family:-apple-system-body,ui-sans-serif,-apple-system,system-ui,"Segoe UI",Helvetica,"Apple Color Emoji",Arial,sans-serif,"Segoe UI Emoji","Segoe UI Symbol";font-size:12px;background:rgb(243, 243, 243);border:1px solid rgba(0, 0, 0, 0.1);border-radius:2px;pointer-events:none;z-index:100;}#chatgpt-mermaid-\_r_b8b\_ .flowchartTitleText{text-anchor:middle;font-size:18px;fill:rgb(13, 13, 13);}#chatgpt-mermaid-\_r_b8b\_ rect.text{fill:none;stroke-width:0;}#chatgpt-mermaid-\_r_b8b\_ .icon-shape,#chatgpt-mermaid-\_r_b8b\_ .image-shape{background-color:rgb(252, 252, 252);text-align:center;}#chatgpt-mermaid-\_r_b8b\_ .icon-shape p,#chatgpt-mermaid-\_r_b8b\_ .image-shape p{background-color:rgb(252, 252, 252);padding:2px;}#chatgpt-mermaid-\_r_b8b\_ .icon-shape .label rect,#chatgpt-mermaid-\_r_b8b\_ .image-shape .label rect{opacity:0.5;background-color:rgb(252, 252, 252);fill:rgb(252, 252, 252);}#chatgpt-mermaid-\_r_b8b\_ .label-icon{display:inline-block;height:1em;overflow:visible;vertical-align:-0.125em;}#chatgpt-mermaid-\_r_b8b\_ .node .label-icon path{fill:currentColor;stroke:revert;stroke-width:revert;}#chatgpt-mermaid-\_r_b8b\_ .node .neo-node{stroke:rgb(107, 198, 127);}#chatgpt-mermaid-\_r_b8b\_ [data-look="neo"].node rect,#chatgpt-mermaid-\_r_b8b\_ [data-look="neo"].cluster rect,#chatgpt-mermaid-\_r_b8b\_ [data-look="neo"].node polygon{stroke:url(#chatgpt-mermaid-\_r_b8b\_-gradient);filter:drop-shadow( 1px 2px 2px rgba(185,185,185,1));}#chatgpt-mermaid-\_r_b8b\_ [data-look="neo"].swimlane.cluster rect{filter:none;}#chatgpt-mermaid-\_r_b8b\_ [data-look="neo"].node path{stroke:url(#chatgpt-mermaid-\_r_b8b\_-gradient);stroke-width:1px;}#chatgpt-mermaid-\_r_b8b\_ [data-look="neo"].node .outer-path{filter:drop-shadow( 1px 2px 2px rgba(185,185,185,1));}#chatgpt-mermaid-\_r_b8b\_ [data-look="neo"].node .neo-line path{stroke:rgb(107, 198, 127);filter:none;}#chatgpt-mermaid-\_r_b8b\_ [data-look="neo"].node circle{stroke:url(#chatgpt-mermaid-\_r_b8b\_-gradient);filter:drop-shadow( 1px 2px 2px rgba(185,185,185,1));}#chatgpt-mermaid-\_r_b8b\_ [data-look="neo"].node circle .state-start{fill:#000000;}#chatgpt-mermaid-\_r_b8b\_ [data-look="neo"].icon-shape .icon{fill:url(#chatgpt-mermaid-\_r_b8b\_-gradient);filter:drop-shadow( 1px 2px 2px rgba(185,185,185,1));}#chatgpt-mermaid-\_r_b8b\_ [data-look="neo"].icon-shape .icon-neo path{stroke:url(#chatgpt-mermaid-\_r_b8b\_-gradient);filter:drop-shadow( 1px 2px 2px rgba(185,185,185,1));}#chatgpt-mermaid-\_r_b8b\_ .node text{font-size:14px;font-weight:600;letter-spacing:normal;fill:rgb(0, 105, 42);}#chatgpt-mermaid-\_r_b8b\_ .edgeLabels text{font-size:13px;font-weight:600;letter-spacing:-0.08px;fill:rgb(0, 105, 42);}#chatgpt-mermaid-\_r_b8b\_ .node tspan[font-weight="normal"],#chatgpt-mermaid-\_r_b8b\_ .edgeLabels tspan[font-weight="normal"]{font-weight:600;}#chatgpt-mermaid-\_r_b8b\_ .edgeLabel .label rect{opacity:1;rx:13px;ry:13px;fill:rgb(237, 250, 242);stroke:rgb(166, 211, 184);stroke-width:1px;}#chatgpt-mermaid-\_r_b8b\_ .node rect,#chatgpt-mermaid-\_r_b8b\_ .node circle,#chatgpt-mermaid-\_r_b8b\_ .node ellipse,#chatgpt-mermaid-\_r_b8b\_ .node polygon,#chatgpt-mermaid-\_r_b8b\_ .node path{fill:rgb(217, 244, 228);stroke:rgba(0, 0, 0, 0.1);stroke-width:1px;}#chatgpt-mermaid-\_r_b8b\_ .node rect{rx:16px;ry:16px;}#chatgpt-mermaid-\_r_b8b\_ .node.mermaid-decision .label-container{fill:rgb(237, 250, 242);stroke:rgb(195, 220, 205);stroke-dasharray:2,2;}#chatgpt-mermaid-\_r_b8b\_ .edgePaths .flowchart-link{stroke:rgb(143, 143, 143);stroke-width:1px;stroke-linecap:round;stroke-linejoin:round;}#chatgpt-mermaid-\_r_b8b\_ .marker{fill:rgb(143, 143, 143);stroke:rgb(143, 143, 143);}#chatgpt-mermaid-\_r_b8b\_ :root{--mermaid-font-family:-apple-system-body,ui-sans-serif,-apple-system,system-ui,"Segoe UI",Helvetica,"Apple Color Emoji",Arial,sans-serif,"Segoe UI Emoji","Segoe UI Symbol";}Logical Extent 0NVMe Physical ExtentI/O ManagerDRAM Staging BufferGPU HBMGPU Compute

如果支持 GPUDirect Storage，则可以在符合设备、驱动和缓冲区条件时减少 CPU 中转，但仍然需要管理 GPU 内存分配和 DMA 生命周期。

# 三、问题二：DRAM、NVMe、GPU HBM 统一资源管理与数据迁移

这一部分是 AI-Native Storage 能否真正做到硬件原生的关键。

传统数据库主要管理：

```
Buffer Pool
    |
    +-- Shared Buffer
    |
    +-- Page Replacement
    |
    +-- Dirty Page
    |
    +-- Flush
```

但是 AI 系统面对的是不同性质的内存和设备：

- DRAM 适合低延迟随机访问。
- NVMe 适合持久化和大容量数据。
- GPU HBM 适合高带宽计算。
- CXL Memory 可以作为可扩展内存层。
- CPU Cache 适合极高频的小规模数据访问。

因此不能简单地把 Buffer Pool 扩展成更大的 Buffer Pool。

应该建立统一的硬件资源管理系统。

## 3.1 统一资源管理的设计目标

我建议定义一个核心模块：

Hardware Resource Manager（HRM）

它负责回答五个问题：

1. 当前系统有哪些硬件资源？
2. 每种资源还剩多少可用容量？
3. 哪些 Object 或 Extent 应该放在哪里？
4. 不同硬件之间如何迁移数据？
5. 当资源不足或者硬件发生故障时如何处理？

整体架构：

\#chatgpt-mermaid-\_r_b8o\_{font-family:-apple-system-body,ui-sans-serif,-apple-system,system-ui,"Segoe UI",Helvetica,"Apple Color Emoji",Arial,sans-serif,"Segoe UI Emoji","Segoe UI Symbol";font-size:16px;fill:rgb(13, 13, 13);}@keyframes edge-animation-frame{from{stroke-dashoffset:0;}}@keyframes dash{to{stroke-dashoffset:0;}}#chatgpt-mermaid-\_r_b8o\_ .edge-animation-slow{stroke-dasharray:9,5!important;stroke-dashoffset:900;animation:dash 50s linear infinite;stroke-linecap:round;}#chatgpt-mermaid-\_r_b8o\_ .edge-animation-fast{stroke-dasharray:9,5!important;stroke-dashoffset:900;animation:dash 20s linear infinite;stroke-linecap:round;}#chatgpt-mermaid-\_r_b8o\_ .error-icon{fill:rgb(243, 243, 243);}#chatgpt-mermaid-\_r_b8o\_ .error-text{fill:rgb(13, 13, 13);stroke:rgb(13, 13, 13);}#chatgpt-mermaid-\_r_b8o\_ .edge-thickness-normal{stroke-width:1px;}#chatgpt-mermaid-\_r_b8o\_ .edge-thickness-thick{stroke-width:3.5px;}#chatgpt-mermaid-\_r_b8o\_ .edge-pattern-solid{stroke-dasharray:0;}#chatgpt-mermaid-\_r_b8o\_ .edge-thickness-invisible{stroke-width:0;fill:none;}#chatgpt-mermaid-\_r_b8o\_ .edge-pattern-dashed{stroke-dasharray:3;}#chatgpt-mermaid-\_r_b8o\_ .edge-pattern-dotted{stroke-dasharray:2;}#chatgpt-mermaid-\_r_b8o\_ .marker{fill:rgb(143, 143, 143);stroke:rgb(143, 143, 143);}#chatgpt-mermaid-\_r_b8o\_ .marker.cross{stroke:rgb(143, 143, 143);}#chatgpt-mermaid-\_r_b8o\_ svg{font-family:-apple-system-body,ui-sans-serif,-apple-system,system-ui,"Segoe UI",Helvetica,"Apple Color Emoji",Arial,sans-serif,"Segoe UI Emoji","Segoe UI Symbol";font-size:16px;}#chatgpt-mermaid-\_r_b8o\_ p{margin:0;}#chatgpt-mermaid-\_r_b8o\_ .label{font-family:-apple-system-body,ui-sans-serif,-apple-system,system-ui,"Segoe UI",Helvetica,"Apple Color Emoji",Arial,sans-serif,"Segoe UI Emoji","Segoe UI Symbol";color:rgb(13, 13, 13);}#chatgpt-mermaid-\_r_b8o\_ .cluster-label text{fill:rgb(13, 13, 13);}#chatgpt-mermaid-\_r_b8o\_ .cluster-label span{color:rgb(13, 13, 13);}#chatgpt-mermaid-\_r_b8o\_ .cluster-label span p{background-color:transparent;}#chatgpt-mermaid-\_r_b8o\_ .label text,#chatgpt-mermaid-\_r_b8o\_ span{fill:rgb(13, 13, 13);color:rgb(13, 13, 13);}#chatgpt-mermaid-\_r_b8o\_ .node rect,#chatgpt-mermaid-\_r_b8o\_ .node circle,#chatgpt-mermaid-\_r_b8o\_ .node ellipse,#chatgpt-mermaid-\_r_b8o\_ .node polygon,#chatgpt-mermaid-\_r_b8o\_ .node path{fill:rgb(226, 242, 230);stroke:rgb(107, 198, 127);stroke-width:1px;}#chatgpt-mermaid-\_r_b8o\_ .rough-node .label text,#chatgpt-mermaid-\_r_b8o\_ .node .label text,#chatgpt-mermaid-\_r_b8o\_ .image-shape .label,#chatgpt-mermaid-\_r_b8o\_ .icon-shape .label{text-anchor:middle;}#chatgpt-mermaid-\_r_b8o\_ .node .katex path{fill:#000;stroke:#000;stroke-width:1px;}#chatgpt-mermaid-\_r_b8o\_ .rough-node .label,#chatgpt-mermaid-\_r_b8o\_ .node .label,#chatgpt-mermaid-\_r_b8o\_ .image-shape .label,#chatgpt-mermaid-\_r_b8o\_ .icon-shape .label{text-align:center;}#chatgpt-mermaid-\_r_b8o\_ .node.clickable{cursor:pointer;}#chatgpt-mermaid-\_r_b8o\_ .root .anchor path{fill:rgb(143, 143, 143)!important;stroke-width:0;stroke:rgb(143, 143, 143);}#chatgpt-mermaid-\_r_b8o\_ .arrowheadPath{fill:rgb(143, 143, 143);}#chatgpt-mermaid-\_r_b8o\_ .edgePath .path{stroke:rgb(143, 143, 143);stroke-width:1px;}#chatgpt-mermaid-\_r_b8o\_ .flowchart-link{stroke:rgb(143, 143, 143);fill:none;}#chatgpt-mermaid-\_r_b8o\_ .edgeLabel{background-color:rgb(252, 252, 252);text-align:center;}#chatgpt-mermaid-\_r_b8o\_ .edgeLabel p{background-color:rgb(252, 252, 252);}#chatgpt-mermaid-\_r_b8o\_ .edgeLabel rect{opacity:0.5;background-color:rgb(252, 252, 252);fill:rgb(252, 252, 252);}#chatgpt-mermaid-\_r_b8o\_ .labelBkg{background-color:rgba(252, 252, 252, 0.5);}#chatgpt-mermaid-\_r_b8o\_ .cluster rect{fill:rgb(243, 243, 243);stroke:rgba(0, 0, 0, 0.1);stroke-width:1px;}#chatgpt-mermaid-\_r_b8o\_ .cluster text{fill:rgb(13, 13, 13);}#chatgpt-mermaid-\_r_b8o\_ .cluster span{color:rgb(13, 13, 13);}#chatgpt-mermaid-\_r_b8o\_ div.mermaidTooltip{position:absolute;text-align:center;max-width:200px;padding:2px;font-family:-apple-system-body,ui-sans-serif,-apple-system,system-ui,"Segoe UI",Helvetica,"Apple Color Emoji",Arial,sans-serif,"Segoe UI Emoji","Segoe UI Symbol";font-size:12px;background:rgb(243, 243, 243);border:1px solid rgba(0, 0, 0, 0.1);border-radius:2px;pointer-events:none;z-index:100;}#chatgpt-mermaid-\_r_b8o\_ .flowchartTitleText{text-anchor:middle;font-size:18px;fill:rgb(13, 13, 13);}#chatgpt-mermaid-\_r_b8o\_ rect.text{fill:none;stroke-width:0;}#chatgpt-mermaid-\_r_b8o\_ .icon-shape,#chatgpt-mermaid-\_r_b8o\_ .image-shape{background-color:rgb(252, 252, 252);text-align:center;}#chatgpt-mermaid-\_r_b8o\_ .icon-shape p,#chatgpt-mermaid-\_r_b8o\_ .image-shape p{background-color:rgb(252, 252, 252);padding:2px;}#chatgpt-mermaid-\_r_b8o\_ .icon-shape .label rect,#chatgpt-mermaid-\_r_b8o\_ .image-shape .label rect{opacity:0.5;background-color:rgb(252, 252, 252);fill:rgb(252, 252, 252);}#chatgpt-mermaid-\_r_b8o\_ .label-icon{display:inline-block;height:1em;overflow:visible;vertical-align:-0.125em;}#chatgpt-mermaid-\_r_b8o\_ .node .label-icon path{fill:currentColor;stroke:revert;stroke-width:revert;}#chatgpt-mermaid-\_r_b8o\_ .node .neo-node{stroke:rgb(107, 198, 127);}#chatgpt-mermaid-\_r_b8o\_ [data-look="neo"].node rect,#chatgpt-mermaid-\_r_b8o\_ [data-look="neo"].cluster rect,#chatgpt-mermaid-\_r_b8o\_ [data-look="neo"].node polygon{stroke:url(#chatgpt-mermaid-\_r_b8o\_-gradient);filter:drop-shadow( 1px 2px 2px rgba(185,185,185,1));}#chatgpt-mermaid-\_r_b8o\_ [data-look="neo"].swimlane.cluster rect{filter:none;}#chatgpt-mermaid-\_r_b8o\_ [data-look="neo"].node path{stroke:url(#chatgpt-mermaid-\_r_b8o\_-gradient);stroke-width:1px;}#chatgpt-mermaid-\_r_b8o\_ [data-look="neo"].node .outer-path{filter:drop-shadow( 1px 2px 2px rgba(185,185,185,1));}#chatgpt-mermaid-\_r_b8o\_ [data-look="neo"].node .neo-line path{stroke:rgb(107, 198, 127);filter:none;}#chatgpt-mermaid-\_r_b8o\_ [data-look="neo"].node circle{stroke:url(#chatgpt-mermaid-\_r_b8o\_-gradient);filter:drop-shadow( 1px 2px 2px rgba(185,185,185,1));}#chatgpt-mermaid-\_r_b8o\_ [data-look="neo"].node circle .state-start{fill:#000000;}#chatgpt-mermaid-\_r_b8o\_ [data-look="neo"].icon-shape .icon{fill:url(#chatgpt-mermaid-\_r_b8o\_-gradient);filter:drop-shadow( 1px 2px 2px rgba(185,185,185,1));}#chatgpt-mermaid-\_r_b8o\_ [data-look="neo"].icon-shape .icon-neo path{stroke:url(#chatgpt-mermaid-\_r_b8o\_-gradient);filter:drop-shadow( 1px 2px 2px rgba(185,185,185,1));}#chatgpt-mermaid-\_r_b8o\_ .node text{font-size:14px;font-weight:600;letter-spacing:normal;fill:rgb(0, 105, 42);}#chatgpt-mermaid-\_r_b8o\_ .edgeLabels text{font-size:13px;font-weight:600;letter-spacing:-0.08px;fill:rgb(0, 105, 42);}#chatgpt-mermaid-\_r_b8o\_ .node tspan[font-weight="normal"],#chatgpt-mermaid-\_r_b8o\_ .edgeLabels tspan[font-weight="normal"]{font-weight:600;}#chatgpt-mermaid-\_r_b8o\_ .edgeLabel .label rect{opacity:1;rx:13px;ry:13px;fill:rgb(237, 250, 242);stroke:rgb(166, 211, 184);stroke-width:1px;}#chatgpt-mermaid-\_r_b8o\_ .node rect,#chatgpt-mermaid-\_r_b8o\_ .node circle,#chatgpt-mermaid-\_r_b8o\_ .node ellipse,#chatgpt-mermaid-\_r_b8o\_ .node polygon,#chatgpt-mermaid-\_r_b8o\_ .node path{fill:rgb(217, 244, 228);stroke:rgba(0, 0, 0, 0.1);stroke-width:1px;}#chatgpt-mermaid-\_r_b8o\_ .node rect{rx:16px;ry:16px;}#chatgpt-mermaid-\_r_b8o\_ .node.mermaid-decision .label-container{fill:rgb(237, 250, 242);stroke:rgb(195, 220, 205);stroke-dasharray:2,2;}#chatgpt-mermaid-\_r_b8o\_ .edgePaths .flowchart-link{stroke:rgb(143, 143, 143);stroke-width:1px;stroke-linecap:round;stroke-linejoin:round;}#chatgpt-mermaid-\_r_b8o\_ .marker{fill:rgb(143, 143, 143);stroke:rgb(143, 143, 143);}#chatgpt-mermaid-\_r_b8o\_ :root{--mermaid-font-family:-apple-system-body,ui-sans-serif,-apple-system,system-ui,"Segoe UI",Helvetica,"Apple Color Emoji",Arial,sans-serif,"Segoe UI Emoji","Segoe UI Symbol";}Memory and Storage TiersHardware Resource ManagerCPU CacheDRAMCXLGPU HBMNVMeHDDHardware DiscoveryHardware TopologyCapacity ManagerPerformance MonitorPlacement PolicyMigration Manager

这里 HRM 不负责直接读取每个 Object。

它负责资源决策、容量控制和迁移协调。

具体 I/O 由 I/O Manager 执行。

## 3.2 不同硬件的资源模型

建议把硬件资源抽象为 Resource，而不是简单抽象成 Device。

因为 DRAM、GPU HBM、NVMe 的资源管理方式并不一样。

例如：

- DRAM 主要管理内存分配、NUMA 位置和页面回收。
- GPU HBM 主要管理 GPU Allocation、显存容量和设备间传输。
- NVMe 主要管理 LBA 分配、I/O 队列和持久化。
- CXL 需要考虑内存设备拓扑、访问距离和一致性机制。

建议设计：

```
struct HardwareResource {
    resource_id: ResourceId,

    resource_type: ResourceType,

    capacity: u64,

    allocated: u64,

    reserved: u64,

    available: u64,

    numa_node: Option<u32>,

    device_id: Option<u32>,

    bandwidth_estimate: u64,

    latency_estimate_ns: u64,

    durability: DurabilityLevel,

    health: ResourceHealth,
}
```

还需要区分两个概念：

Capacity 与 Performance 是两种不同的资源约束。

例如 GPU HBM 还有 20 GB 空闲，但 GPU 计算资源可能已经被其他任务占满。

因此资源管理不能只根据容量判断是否应该迁移数据。

至少需要监控：

| 指标     | DRAM      | GPU HBM     | NVMe    |
| ------ | --------- | ----------- | ------- |
| 可用容量   | 是         | 是           | 是       |
| 实际带宽   | 是         | 是           | 是       |
| 访问延迟   | 是         | 是           | 是       |
| 并发访问压力 | 是         | 是           | 是       |
| 设备利用率  | NUMA/内存压力 | GPU 利用率     | I/O 利用率 |
| 持久性    | 否         | 否           | 是       |
| 数据迁移成本 | 内存复制      | PCIe/NVLink | DMA/I/O |

## 3.3 不能把 DRAM、GPU HBM、NVMe 当成简单的缓存层级

这里有一个重要的架构判断：

DRAM 和 GPU HBM 不只是 NVMe 的缓存，它们还是不同计算任务的数据驻留空间。

例如：

一个向量对象：

```
Vector Object
    |
    +-- Persistent Copy
    |      |
    |      +-- NVMe
    |
    +-- Cached Copy
           |
           +-- DRAM
           |
           +-- GPU HBM
```

它们的职责不同：

| 位置      | 作用              |
| ------- | --------------- |
| NVMe    | 持久化副本           |
| DRAM    | CPU 检索缓存、I/O 缓冲 |
| GPU HBM | GPU 计算工作集       |

这里可以允许一个 Object 同时存在于多个设备。

但必须建立副本一致性机制。

不能简单认为三个位置的数据天然一致。

## 3.4 Placement Manager：数据应该放在哪里？

这是 HRM 的核心决策模块。

建议将数据放置分为三种策略。

### 策略一：根据访问频率放置

例如：

```
Hot Object
    |
    +-- DRAM

Warm Object
    |
    +-- NVMe

Cold Object
    |
    +-- HDD
```

但是仅靠访问频率是不够的。

因为一个高频 Object 可能非常大，而且并不一定适合全部加载到 DRAM。

### 策略二：根据数据类型放置

例如：

| Object 类型       | 默认持久化位置 | 优先工作位置         |
| --------------- | ------- | -------------- |
| Object Metadata | NVMe    | DRAM           |
| Vector          | NVMe    | DRAM / GPU HBM |
| Tensor          | NVMe    | GPU HBM        |
| Document        | NVMe    | DRAM           |
| Graph           | NVMe    | DRAM           |
| Event           | NVMe    | DRAM           |
| Memory          | NVMe    | DRAM           |

这只是默认策略，实际需要根据访问模式调整。

### 策略三：根据任务放置

这是 AI-Native Storage 与传统存储系统最大的不同之一。

假设一个 Agent 正在执行：

```
向量检索
    |
    v
读取 100 万条 Embedding
    |
    v
GPU 相似度计算
    |
    v
Top-K
```

此时，向量数据是否应该放入 GPU HBM，不应该只看它是不是热点数据。

还需要看：

- GPU 是否可用。
- 数据规模是否适合 HBM。
- PCIe 或 NVLink 传输成本。
- 检索任务预计持续多久。
- 其他任务是否也需要这部分显存。

所以 Placement Manager 应该接受 Task Scheduler 的资源需求。

例如：

```
struct PlacementRequest {
    object_id: ObjectId,

    access_pattern: AccessPattern,

    expected_access_count: u64,

    expected_duration_ms: u64,

    required_bandwidth: u64,

    preferred_resource: Option<ResourceId>,

    allow_replica: bool,
}
```

最终由 Placement Manager 选择位置。

## 3.5 数据放置成本模型

建议建立一个确定性的成本模型：

\\[ C(r,o,t)= \alpha L(r,o,t)+ \beta T(r,o,t)+ \gamma P(r,t)+ \delta M(r,o) \\]

其中：

- \\(L\\)：预计访问延迟。
- \\(T\\)：预计数据传输成本。
- \\(P\\)：目标资源压力。
- \\(M\\)：数据迁移成本。
- \\(r\\)：目标资源。
- \\(o\\)：数据对象。
- \\(t\\)：当前任务。

选择成本最低的可行资源：

\\[ r^\*=\arg\min\_{r\in R} C(r,o,t) \\]

但需要加入约束：

\\[ Used(r)+Required(o,r)\le Capacity(r)-Reserved(r) \\]

此外还要加入持久性约束。

例如，不能因为 DRAM 延迟低，就把唯一的持久化副本从 NVMe 迁移到 DRAM。

这里的模型先使用规则和统计数据就可以，不需要 LLM 参与每次资源选择。

AI 可以分析长期工作负载，调整参数或者提出数据分层策略。

## 3.6 Memory Tier Manager：统一内存管理

建议把不同资源划分为 Memory Domain。

```
Memory Tier Manager
       |
       +-- CPU Memory Domain
       |      |
       |      +-- NUMA 0
       |      +-- NUMA 1
       |
       +-- GPU Memory Domain
       |      |
       |      +-- GPU 0 HBM
       |      +-- GPU 1 HBM
       |
       +-- Persistent Domain
              |
              +-- NVMe 0
              +-- NVMe 1
```

每个 Domain 独立管理：

- 容量。
- 分配。
- 释放。
- 访问统计。
- 资源预留。
- 资源压力。
- 故障状态。

### 内存分配接口

```
struct MemoryAllocationRequest {
    size: u64,

    resource_type: ResourceType,

    numa_node: Option<u32>,

    device_id: Option<u32>,

    alignment: u64,

    access_pattern: AccessPattern,
}
```

分配结果：

```
struct MemoryAllocation {
    allocation_id: AllocationId,

    resource_id: ResourceId,

    offset: u64,

    size: u64,

    allocation_state: AllocationState,
}
```

建议统一管理分配元数据，但不要强行统一所有底层内存分配算法。

例如：

- DRAM 使用 NUMA-aware allocator。
- GPU 使用 CUDA/HIP allocator。
- NVMe 使用 Extent Allocator。
- CXL 使用对应内存设备管理接口。

统一的是管理接口和资源生命周期，而不是底层分配实现。

## 3.7 Cache 与 Replica 必须分离

这是一个容易被忽视的问题。

一个数据在 DRAM 中，可能有两种完全不同的含义：

Cache：

- 可以被淘汰。
- 不承担数据持久化职责。
- 丢失后可以从 NVMe 重新加载。

Replica：

- 是一个有版本和一致性约束的数据副本。
- 可能承担高可用或数据冗余职责。
- 不能随意淘汰。

建议设计：

```
enum ResidencyRole {
    Cache,

    WorkingSet,

    Replica,

    Authoritative,
}
```

例如：

```
Vector Object 1001
    |
    +-- NVMe 0
    |      |
    |      +-- Authoritative
    |
    +-- DRAM 0
    |      |
    |      +-- Cache
    |
    +-- GPU 0 HBM
           |
           +-- WorkingSet
```

GPU HBM 中的数据可以是当前任务的工作集，而不需要被视为持久化副本。

这样才能明确缓存失效、任务取消和 GPU 故障时的处理方式。

## 3.8 数据迁移：Migration Manager

这是硬件资源管理中最重要的执行模块。

数据迁移不能简单实现为：

```
Read Old Location
       |
       v
Write New Location
       |
       v
Update Pointer
```

因为系统可能同时发生：

- 并发读取。
- 并发写入。
- 事务提交。
- 版本更新。
- 设备故障。
- 迁移中断。

必须设计一个可恢复的数据迁移协议。

### 迁移状态机

建议定义：

\#chatgpt-mermaid-\_r_bar\_{font-family:-apple-system-body,ui-sans-serif,-apple-system,system-ui,"Segoe UI",Helvetica,"Apple Color Emoji",Arial,sans-serif,"Segoe UI Emoji","Segoe UI Symbol";font-size:16px;fill:rgb(13, 13, 13);}@keyframes edge-animation-frame{from{stroke-dashoffset:0;}}@keyframes dash{to{stroke-dashoffset:0;}}#chatgpt-mermaid-\_r_bar\_ .edge-animation-slow{stroke-dasharray:9,5!important;stroke-dashoffset:900;animation:dash 50s linear infinite;stroke-linecap:round;}#chatgpt-mermaid-\_r_bar\_ .edge-animation-fast{stroke-dasharray:9,5!important;stroke-dashoffset:900;animation:dash 20s linear infinite;stroke-linecap:round;}#chatgpt-mermaid-\_r_bar\_ .error-icon{fill:rgb(243, 243, 243);}#chatgpt-mermaid-\_r_bar\_ .error-text{fill:rgb(13, 13, 13);stroke:rgb(13, 13, 13);}#chatgpt-mermaid-\_r_bar\_ .edge-thickness-normal{stroke-width:1px;}#chatgpt-mermaid-\_r_bar\_ .edge-thickness-thick{stroke-width:3.5px;}#chatgpt-mermaid-\_r_bar\_ .edge-pattern-solid{stroke-dasharray:0;}#chatgpt-mermaid-\_r_bar\_ .edge-thickness-invisible{stroke-width:0;fill:none;}#chatgpt-mermaid-\_r_bar\_ .edge-pattern-dashed{stroke-dasharray:3;}#chatgpt-mermaid-\_r_bar\_ .edge-pattern-dotted{stroke-dasharray:2;}#chatgpt-mermaid-\_r_bar\_ .marker{fill:rgb(143, 143, 143);stroke:rgb(143, 143, 143);}#chatgpt-mermaid-\_r_bar\_ .marker.cross{stroke:rgb(143, 143, 143);}#chatgpt-mermaid-\_r_bar\_ svg{font-family:-apple-system-body,ui-sans-serif,-apple-system,system-ui,"Segoe UI",Helvetica,"Apple Color Emoji",Arial,sans-serif,"Segoe UI Emoji","Segoe UI Symbol";font-size:16px;}#chatgpt-mermaid-\_r_bar\_ p{margin:0;}#chatgpt-mermaid-\_r_bar\_ defs [id$="-barbEnd"]{fill:rgb(143, 143, 143);stroke:rgb(143, 143, 143);}#chatgpt-mermaid-\_r_bar\_ g.stateGroup text{fill:rgb(107, 198, 127);stroke:none;font-size:10px;}#chatgpt-mermaid-\_r_bar\_ g.stateGroup text{fill:rgb(13, 13, 13);stroke:none;font-size:10px;}#chatgpt-mermaid-\_r_bar\_ g.stateGroup .state-title{font-weight:bolder;fill:rgb(13, 13, 13);}#chatgpt-mermaid-\_r_bar\_ g.stateGroup rect{fill:rgb(226, 242, 230);stroke:rgb(107, 198, 127);}#chatgpt-mermaid-\_r_bar\_ g.stateGroup line{stroke:rgb(143, 143, 143);stroke-width:1;}#chatgpt-mermaid-\_r_bar\_ .transition{stroke:rgb(143, 143, 143);stroke-width:1;fill:none;}#chatgpt-mermaid-\_r_bar\_ .stateGroup .composit{fill:transparent;border-bottom:1px;}#chatgpt-mermaid-\_r_bar\_ .stateGroup .alt-composit{fill:#e0e0e0;border-bottom:1px;}#chatgpt-mermaid-\_r_bar\_ .state-note{stroke:rgb(239, 139, 87);fill:rgb(243, 243, 243);}#chatgpt-mermaid-\_r_bar\_ .state-note text{fill:rgb(13, 13, 13);stroke:none;font-size:10px;}#chatgpt-mermaid-\_r_bar\_ .stateLabel .box{stroke:none;stroke-width:0;fill:rgb(226, 242, 230);opacity:0.5;}#chatgpt-mermaid-\_r_bar\_ .edgeLabel .label rect{fill:rgb(226, 242, 230);opacity:0.5;}#chatgpt-mermaid-\_r_bar\_ .edgeLabel{background-color:rgb(252, 252, 252);text-align:center;}#chatgpt-mermaid-\_r_bar\_ .edgeLabel p{background-color:rgb(252, 252, 252);}#chatgpt-mermaid-\_r_bar\_ .edgeLabel rect{opacity:0.5;background-color:rgb(252, 252, 252);fill:rgb(252, 252, 252);}#chatgpt-mermaid-\_r_bar\_ .edgeLabel .label text{fill:rgb(13, 13, 13);}#chatgpt-mermaid-\_r_bar\_ .label div .edgeLabel{color:rgb(13, 13, 13);}#chatgpt-mermaid-\_r_bar\_ .stateLabel text{fill:rgb(13, 13, 13);font-size:10px;font-weight:bold;}#chatgpt-mermaid-\_r_bar\_ .node circle.state-start{fill:rgb(143, 143, 143);stroke:rgb(143, 143, 143);}#chatgpt-mermaid-\_r_bar\_ .node .fork-join{fill:rgb(143, 143, 143);stroke:rgb(143, 143, 143);}#chatgpt-mermaid-\_r_bar\_ .node circle.state-end{fill:rgb(107, 198, 127);stroke:transparent;stroke-width:1.5;}#chatgpt-mermaid-\_r_bar\_ .end-state-inner{fill:transparent;stroke-width:1.5;}#chatgpt-mermaid-\_r_bar\_ .node rect{fill:rgb(226, 242, 230);stroke:rgb(107, 198, 127);stroke-width:1px;}#chatgpt-mermaid-\_r_bar\_ .node polygon{fill:rgb(226, 242, 230);stroke:rgb(107, 198, 127);stroke-width:1px;}#chatgpt-mermaid-\_r_bar\_ [id$="-barbEnd"]{fill:rgb(143, 143, 143);}#chatgpt-mermaid-\_r_bar\_ .statediagram-cluster rect{fill:rgb(226, 242, 230);stroke:rgb(107, 198, 127);stroke-width:1px;}#chatgpt-mermaid-\_r_bar\_ .cluster-label,#chatgpt-mermaid-\_r_bar\_ .nodeLabel{color:rgb(13, 13, 13);}#chatgpt-mermaid-\_r_bar\_ .statediagram-cluster rect.outer{rx:5px;ry:5px;}#chatgpt-mermaid-\_r_bar\_ .statediagram-state .divider{stroke:rgb(107, 198, 127);}#chatgpt-mermaid-\_r_bar\_ .statediagram-state .title-state{rx:5px;ry:5px;}#chatgpt-mermaid-\_r_bar\_ .statediagram-cluster.statediagram-cluster .inner{fill:transparent;}#chatgpt-mermaid-\_r_bar\_ .statediagram-cluster.statediagram-cluster-alt .inner{fill:rgb(243, 243, 243);}#chatgpt-mermaid-\_r_bar\_ .statediagram-cluster .inner{rx:0;ry:0;}#chatgpt-mermaid-\_r_bar\_ .statediagram-state rect.basic{rx:5px;ry:5px;}#chatgpt-mermaid-\_r_bar\_ .statediagram-state rect.divider{stroke-dasharray:10,10;fill:rgb(243, 243, 243);}#chatgpt-mermaid-\_r_bar\_ .note-edge{stroke-dasharray:5;}#chatgpt-mermaid-\_r_bar\_ .statediagram-note rect{fill:rgb(243, 243, 243);stroke:rgb(239, 139, 87);stroke-width:1px;rx:0;ry:0;}#chatgpt-mermaid-\_r_bar\_ .statediagram-note rect{fill:rgb(243, 243, 243);stroke:rgb(239, 139, 87);stroke-width:1px;rx:0;ry:0;}#chatgpt-mermaid-\_r_bar\_ .statediagram-note text{fill:rgb(13, 13, 13);}#chatgpt-mermaid-\_r_bar\_ .statediagram-note .nodeLabel{color:rgb(13, 13, 13);}#chatgpt-mermaid-\_r_bar\_ .statediagram .edgeLabel{color:red;}#chatgpt-mermaid-\_r_bar\_ [id$="-dependencyStart"],#chatgpt-mermaid-\_r_bar\_ [id$="-dependencyEnd"]{fill:rgb(143, 143, 143);stroke:rgb(143, 143, 143);stroke-width:1;}#chatgpt-mermaid-\_r_bar\_ .statediagramTitleText{text-anchor:middle;font-size:18px;fill:rgb(13, 13, 13);}#chatgpt-mermaid-\_r_bar\_ [data-look="neo"].statediagram-cluster rect{fill:rgb(226, 242, 230);stroke:url(#chatgpt-mermaid-\_r_bar\_-gradient);stroke-width:1;}#chatgpt-mermaid-\_r_bar\_ [data-look="neo"].statediagram-cluster rect.outer{rx:5px;ry:5px;filter:drop-shadow( 1px 2px 2px rgba(185,185,185,1));}#chatgpt-mermaid-\_r_bar\_ .node .neo-node{stroke:rgb(107, 198, 127);}#chatgpt-mermaid-\_r_bar\_ [data-look="neo"].node rect,#chatgpt-mermaid-\_r_bar\_ [data-look="neo"].cluster rect,#chatgpt-mermaid-\_r_bar\_ [data-look="neo"].node polygon{stroke:url(#chatgpt-mermaid-\_r_bar\_-gradient);filter:drop-shadow( 1px 2px 2px rgba(185,185,185,1));}#chatgpt-mermaid-\_r_bar\_ [data-look="neo"].swimlane.cluster rect{filter:none;}#chatgpt-mermaid-\_r_bar\_ [data-look="neo"].node path{stroke:url(#chatgpt-mermaid-\_r_bar\_-gradient);stroke-width:1px;}#chatgpt-mermaid-\_r_bar\_ [data-look="neo"].node .outer-path{filter:drop-shadow( 1px 2px 2px rgba(185,185,185,1));}#chatgpt-mermaid-\_r_bar\_ [data-look="neo"].node .neo-line path{stroke:rgb(107, 198, 127);filter:none;}#chatgpt-mermaid-\_r_bar\_ [data-look="neo"].node circle{stroke:url(#chatgpt-mermaid-\_r_bar\_-gradient);filter:drop-shadow( 1px 2px 2px rgba(185,185,185,1));}#chatgpt-mermaid-\_r_bar\_ [data-look="neo"].node circle .state-start{fill:#000000;}#chatgpt-mermaid-\_r_bar\_ [data-look="neo"].icon-shape .icon{fill:url(#chatgpt-mermaid-\_r_bar\_-gradient);filter:drop-shadow( 1px 2px 2px rgba(185,185,185,1));}#chatgpt-mermaid-\_r_bar\_ [data-look="neo"].icon-shape .icon-neo path{stroke:url(#chatgpt-mermaid-\_r_bar\_-gradient);filter:drop-shadow( 1px 2px 2px rgba(185,185,185,1));}#chatgpt-mermaid-\_r_bar\_ :root{--mermaid-font-family:-apple-system-body,ui-sans-serif,-apple-system,system-ui,"Segoe UI",Helvetica,"Apple Color Emoji",Arial,sans-serif,"Segoe UI Emoji","Segoe UI Symbol";}PlannedReservedCopyingVerifiedSwitchedReclaimableReclaimedFailedRetryAborted

每次迁移都应该有 Migration ID。

迁移元数据：

```
struct MigrationRecord {
    migration_id: MigrationId,

    extent_id: ExtentId,

    source_mapping: MappingId,

    target_resource: ResourceId,

    target_allocation: Option<AllocationId>,

    state: MigrationState,

    checksum: Option<u64>,

    start_ts: u64,
}
```

### 迁移完整流程

以 NVMe → DRAM 为例：

Mapping ManagerDRAMNVMeI/O ManagerMemory ManagerPlacement ManagerMapping ManagerDRAMNVMeI/O ManagerMemory ManagerPlacement Manager#chatgpt-mermaid-\_r_bb8\_{font-family:-apple-system-body,ui-sans-serif,-apple-system,system-ui,"Segoe UI",Helvetica,"Apple Color Emoji",Arial,sans-serif,"Segoe UI Emoji","Segoe UI Symbol";font-size:16px;fill:rgb(13, 13, 13);}@keyframes edge-animation-frame{from{stroke-dashoffset:0;}}@keyframes dash{to{stroke-dashoffset:0;}}#chatgpt-mermaid-\_r_bb8\_ .edge-animation-slow{stroke-dasharray:9,5!important;stroke-dashoffset:900;animation:dash 50s linear infinite;stroke-linecap:round;}#chatgpt-mermaid-\_r_bb8\_ .edge-animation-fast{stroke-dasharray:9,5!important;stroke-dashoffset:900;animation:dash 20s linear infinite;stroke-linecap:round;}#chatgpt-mermaid-\_r_bb8\_ .error-icon{fill:rgb(243, 243, 243);}#chatgpt-mermaid-\_r_bb8\_ .error-text{fill:rgb(13, 13, 13);stroke:rgb(13, 13, 13);}#chatgpt-mermaid-\_r_bb8\_ .edge-thickness-normal{stroke-width:1px;}#chatgpt-mermaid-\_r_bb8\_ .edge-thickness-thick{stroke-width:3.5px;}#chatgpt-mermaid-\_r_bb8\_ .edge-pattern-solid{stroke-dasharray:0;}#chatgpt-mermaid-\_r_bb8\_ .edge-thickness-invisible{stroke-width:0;fill:none;}#chatgpt-mermaid-\_r_bb8\_ .edge-pattern-dashed{stroke-dasharray:3;}#chatgpt-mermaid-\_r_bb8\_ .edge-pattern-dotted{stroke-dasharray:2;}#chatgpt-mermaid-\_r_bb8\_ .marker{fill:rgb(143, 143, 143);stroke:rgb(143, 143, 143);}#chatgpt-mermaid-\_r_bb8\_ .marker.cross{stroke:rgb(143, 143, 143);}#chatgpt-mermaid-\_r_bb8\_ svg{font-family:-apple-system-body,ui-sans-serif,-apple-system,system-ui,"Segoe UI",Helvetica,"Apple Color Emoji",Arial,sans-serif,"Segoe UI Emoji","Segoe UI Symbol";font-size:16px;}#chatgpt-mermaid-\_r_bb8\_ p{margin:0;}#chatgpt-mermaid-\_r_bb8\_ .actor{stroke:rgb(107, 198, 127);fill:rgb(226, 242, 230);stroke-width:1;}#chatgpt-mermaid-\_r_bb8\_ rect.actor.outer-path[data-look="neo"]{filter:drop-shadow( 1px 2px 2px rgba(185,185,185,1));}#chatgpt-mermaid-\_r_bb8\_ rect.note[data-look="neo"]{stroke:rgb(239, 139, 87);fill:rgb(243, 243, 243);filter:drop-shadow( 1px 2px 2px rgba(185,185,185,1));}#chatgpt-mermaid-\_r_bb8\_ text.actor>tspan{fill:rgb(13, 13, 13);stroke:none;}#chatgpt-mermaid-\_r_bb8\_ .actor-line{stroke:rgb(143, 143, 143);}#chatgpt-mermaid-\_r_bb8\_ .innerArc{stroke-width:1.5;stroke-dasharray:none;}#chatgpt-mermaid-\_r_bb8\_ .messageLine0{stroke-width:1.5;stroke-dasharray:none;stroke:rgb(143, 143, 143);}#chatgpt-mermaid-\_r_bb8\_ .messageLine1{stroke-width:1.5;stroke-dasharray:2,2;stroke:rgb(143, 143, 143);}#chatgpt-mermaid-\_r_bb8\_ [id$="-arrowhead"] path{fill:rgb(143, 143, 143);stroke:rgb(143, 143, 143);}#chatgpt-mermaid-\_r_bb8\_ .sequenceNumber{fill:#707070;}#chatgpt-mermaid-\_r_bb8\_ [id$="-sequencenumber"]{fill:rgb(143, 143, 143);}#chatgpt-mermaid-\_r_bb8\_ [id$="-crosshead"] path{fill:rgb(143, 143, 143);stroke:rgb(143, 143, 143);}#chatgpt-mermaid-\_r_bb8\_ .messageText{fill:rgb(13, 13, 13);stroke:none;}#chatgpt-mermaid-\_r_bb8\_ .labelBox{stroke:rgba(0, 0, 0, 0.1);fill:rgb(252, 252, 252);filter:none;}#chatgpt-mermaid-\_r_bb8\_ .labelText,#chatgpt-mermaid-\_r_bb8\_ .labelText>tspan{fill:rgb(13, 13, 13);stroke:none;}#chatgpt-mermaid-\_r_bb8\_ .loopText,#chatgpt-mermaid-\_r_bb8\_ .loopText>tspan{fill:rgb(13, 13, 13);stroke:none;}#chatgpt-mermaid-\_r_bb8\_ .sectionTitle,#chatgpt-mermaid-\_r_bb8\_ .sectionTitle>tspan{fill:rgb(13, 13, 13);stroke:none;}#chatgpt-mermaid-\_r_bb8\_ .loopLine{stroke-width:2px;stroke-dasharray:2,2;stroke:rgba(0, 0, 0, 0.1);fill:rgba(0, 0, 0, 0.1);}#chatgpt-mermaid-\_r_bb8\_ .note{stroke:rgb(239, 139, 87);fill:rgb(243, 243, 243);}#chatgpt-mermaid-\_r_bb8\_ .noteText,#chatgpt-mermaid-\_r_bb8\_ .noteText>tspan{fill:rgb(13, 13, 13);stroke:none;font-weight:normal;}#chatgpt-mermaid-\_r_bb8\_ .activation0{fill:rgb(243, 243, 243);stroke:hsl(0, 0%, 85.2941176471%);}#chatgpt-mermaid-\_r_bb8\_ .activation1{fill:rgb(243, 243, 243);stroke:hsl(0, 0%, 85.2941176471%);}#chatgpt-mermaid-\_r_bb8\_ .activation2{fill:rgb(243, 243, 243);stroke:hsl(0, 0%, 85.2941176471%);}#chatgpt-mermaid-\_r_bb8\_ .actorPopupMenu{position:absolute;}#chatgpt-mermaid-\_r_bb8\_ .actorPopupMenuPanel{position:absolute;fill:rgb(226, 242, 230);box-shadow:0px 8px 16px 0px rgba(0,0,0,0.2);filter:drop-shadow(3px 5px 2px rgb(0 0 0 / 0.4));}#chatgpt-mermaid-\_r_bb8\_ .actor-man circle,#chatgpt-mermaid-\_r_bb8\_ line{fill:rgb(226, 242, 230);stroke-width:2px;}#chatgpt-mermaid-\_r_bb8\_ g rect.rect{filter:drop-shadow( 1px 2px 2px rgba(185,185,185,1));stroke:rgb(107, 198, 127);}#chatgpt-mermaid-\_r_bb8\_ .node .neo-node{stroke:rgb(107, 198, 127);}#chatgpt-mermaid-\_r_bb8\_ [data-look="neo"].node rect,#chatgpt-mermaid-\_r_bb8\_ [data-look="neo"].cluster rect,#chatgpt-mermaid-\_r_bb8\_ [data-look="neo"].node polygon{stroke:url(#chatgpt-mermaid-\_r_bb8\_-gradient);filter:drop-shadow( 1px 2px 2px rgba(185,185,185,1));}#chatgpt-mermaid-\_r_bb8\_ [data-look="neo"].swimlane.cluster rect{filter:none;}#chatgpt-mermaid-\_r_bb8\_ [data-look="neo"].node path{stroke:url(#chatgpt-mermaid-\_r_bb8\_-gradient);stroke-width:1px;}#chatgpt-mermaid-\_r_bb8\_ [data-look="neo"].node .outer-path{filter:drop-shadow( 1px 2px 2px rgba(185,185,185,1));}#chatgpt-mermaid-\_r_bb8\_ [data-look="neo"].node .neo-line path{stroke:rgb(107, 198, 127);filter:none;}#chatgpt-mermaid-\_r_bb8\_ [data-look="neo"].node circle{stroke:url(#chatgpt-mermaid-\_r_bb8\_-gradient);filter:drop-shadow( 1px 2px 2px rgba(185,185,185,1));}#chatgpt-mermaid-\_r_bb8\_ [data-look="neo"].node circle .state-start{fill:#000000;}#chatgpt-mermaid-\_r_bb8\_ [data-look="neo"].icon-shape .icon{fill:url(#chatgpt-mermaid-\_r_bb8\_-gradient);filter:drop-shadow( 1px 2px 2px rgba(185,185,185,1));}#chatgpt-mermaid-\_r_bb8\_ [data-look="neo"].icon-shape .icon-neo path{stroke:url(#chatgpt-mermaid-\_r_bb8\_-gradient);filter:drop-shadow( 1px 2px 2px rgba(185,185,185,1));}#chatgpt-mermaid-\_r_bb8\_ :root{--mermaid-font-family:-apple-system-body,ui-sans-serif,-apple-system,system-ui,"Segoe UI",Helvetica,"Apple Color Emoji",Arial,sans-serif,"Segoe UI Emoji","Segoe UI Symbol";}Request DRAM allocationAllocation grantedRead source extentSubmit readDataCopy to DRAMCopy completedVerify checksumRegister cache mappingMapping publishedCommit allocation

注意，这个场景是创建缓存副本，不需要把 NVMe 的权威映射替换掉。

而 NVMe 0 → NVMe 1 的持久化迁移则需要更严格的映射切换和故障恢复协议。

### 持久化迁移协议

Mapping ManagerTarget NVMeSource NVMeWAL ManagerMigration ManagerMapping ManagerTarget NVMeSource NVMeWAL ManagerMigration Manager#chatgpt-mermaid-\_r_bbf\_{font-family:-apple-system-body,ui-sans-serif,-apple-system,system-ui,"Segoe UI",Helvetica,"Apple Color Emoji",Arial,sans-serif,"Segoe UI Emoji","Segoe UI Symbol";font-size:16px;fill:rgb(13, 13, 13);}@keyframes edge-animation-frame{from{stroke-dashoffset:0;}}@keyframes dash{to{stroke-dashoffset:0;}}#chatgpt-mermaid-\_r_bbf\_ .edge-animation-slow{stroke-dasharray:9,5!important;stroke-dashoffset:900;animation:dash 50s linear infinite;stroke-linecap:round;}#chatgpt-mermaid-\_r_bbf\_ .edge-animation-fast{stroke-dasharray:9,5!important;stroke-dashoffset:900;animation:dash 20s linear infinite;stroke-linecap:round;}#chatgpt-mermaid-\_r_bbf\_ .error-icon{fill:rgb(243, 243, 243);}#chatgpt-mermaid-\_r_bbf\_ .error-text{fill:rgb(13, 13, 13);stroke:rgb(13, 13, 13);}#chatgpt-mermaid-\_r_bbf\_ .edge-thickness-normal{stroke-width:1px;}#chatgpt-mermaid-\_r_bbf\_ .edge-thickness-thick{stroke-width:3.5px;}#chatgpt-mermaid-\_r_bbf\_ .edge-pattern-solid{stroke-dasharray:0;}#chatgpt-mermaid-\_r_bbf\_ .edge-thickness-invisible{stroke-width:0;fill:none;}#chatgpt-mermaid-\_r_bbf\_ .edge-pattern-dashed{stroke-dasharray:3;}#chatgpt-mermaid-\_r_bbf\_ .edge-pattern-dotted{stroke-dasharray:2;}#chatgpt-mermaid-\_r_bbf\_ .marker{fill:rgb(143, 143, 143);stroke:rgb(143, 143, 143);}#chatgpt-mermaid-\_r_bbf\_ .marker.cross{stroke:rgb(143, 143, 143);}#chatgpt-mermaid-\_r_bbf\_ svg{font-family:-apple-system-body,ui-sans-serif,-apple-system,system-ui,"Segoe UI",Helvetica,"Apple Color Emoji",Arial,sans-serif,"Segoe UI Emoji","Segoe UI Symbol";font-size:16px;}#chatgpt-mermaid-\_r_bbf\_ p{margin:0;}#chatgpt-mermaid-\_r_bbf\_ .actor{stroke:rgb(107, 198, 127);fill:rgb(226, 242, 230);stroke-width:1;}#chatgpt-mermaid-\_r_bbf\_ rect.actor.outer-path[data-look="neo"]{filter:drop-shadow( 1px 2px 2px rgba(185,185,185,1));}#chatgpt-mermaid-\_r_bbf\_ rect.note[data-look="neo"]{stroke:rgb(239, 139, 87);fill:rgb(243, 243, 243);filter:drop-shadow( 1px 2px 2px rgba(185,185,185,1));}#chatgpt-mermaid-\_r_bbf\_ text.actor>tspan{fill:rgb(13, 13, 13);stroke:none;}#chatgpt-mermaid-\_r_bbf\_ .actor-line{stroke:rgb(143, 143, 143);}#chatgpt-mermaid-\_r_bbf\_ .innerArc{stroke-width:1.5;stroke-dasharray:none;}#chatgpt-mermaid-\_r_bbf\_ .messageLine0{stroke-width:1.5;stroke-dasharray:none;stroke:rgb(143, 143, 143);}#chatgpt-mermaid-\_r_bbf\_ .messageLine1{stroke-width:1.5;stroke-dasharray:2,2;stroke:rgb(143, 143, 143);}#chatgpt-mermaid-\_r_bbf\_ [id$="-arrowhead"] path{fill:rgb(143, 143, 143);stroke:rgb(143, 143, 143);}#chatgpt-mermaid-\_r_bbf\_ .sequenceNumber{fill:#707070;}#chatgpt-mermaid-\_r_bbf\_ [id$="-sequencenumber"]{fill:rgb(143, 143, 143);}#chatgpt-mermaid-\_r_bbf\_ [id$="-crosshead"] path{fill:rgb(143, 143, 143);stroke:rgb(143, 143, 143);}#chatgpt-mermaid-\_r_bbf\_ .messageText{fill:rgb(13, 13, 13);stroke:none;}#chatgpt-mermaid-\_r_bbf\_ .labelBox{stroke:rgba(0, 0, 0, 0.1);fill:rgb(252, 252, 252);filter:none;}#chatgpt-mermaid-\_r_bbf\_ .labelText,#chatgpt-mermaid-\_r_bbf\_ .labelText>tspan{fill:rgb(13, 13, 13);stroke:none;}#chatgpt-mermaid-\_r_bbf\_ .loopText,#chatgpt-mermaid-\_r_bbf\_ .loopText>tspan{fill:rgb(13, 13, 13);stroke:none;}#chatgpt-mermaid-\_r_bbf\_ .sectionTitle,#chatgpt-mermaid-\_r_bbf\_ .sectionTitle>tspan{fill:rgb(13, 13, 13);stroke:none;}#chatgpt-mermaid-\_r_bbf\_ .loopLine{stroke-width:2px;stroke-dasharray:2,2;stroke:rgba(0, 0, 0, 0.1);fill:rgba(0, 0, 0, 0.1);}#chatgpt-mermaid-\_r_bbf\_ .note{stroke:rgb(239, 139, 87);fill:rgb(243, 243, 243);}#chatgpt-mermaid-\_r_bbf\_ .noteText,#chatgpt-mermaid-\_r_bbf\_ .noteText>tspan{fill:rgb(13, 13, 13);stroke:none;font-weight:normal;}#chatgpt-mermaid-\_r_bbf\_ .activation0{fill:rgb(243, 243, 243);stroke:hsl(0, 0%, 85.2941176471%);}#chatgpt-mermaid-\_r_bbf\_ .activation1{fill:rgb(243, 243, 243);stroke:hsl(0, 0%, 85.2941176471%);}#chatgpt-mermaid-\_r_bbf\_ .activation2{fill:rgb(243, 243, 243);stroke:hsl(0, 0%, 85.2941176471%);}#chatgpt-mermaid-\_r_bbf\_ .actorPopupMenu{position:absolute;}#chatgpt-mermaid-\_r_bbf\_ .actorPopupMenuPanel{position:absolute;fill:rgb(226, 242, 230);box-shadow:0px 8px 16px 0px rgba(0,0,0,0.2);filter:drop-shadow(3px 5px 2px rgb(0 0 0 / 0.4));}#chatgpt-mermaid-\_r_bbf\_ .actor-man circle,#chatgpt-mermaid-\_r_bbf\_ line{fill:rgb(226, 242, 230);stroke-width:2px;}#chatgpt-mermaid-\_r_bbf\_ g rect.rect{filter:drop-shadow( 1px 2px 2px rgba(185,185,185,1));stroke:rgb(107, 198, 127);}#chatgpt-mermaid-\_r_bbf\_ .node .neo-node{stroke:rgb(107, 198, 127);}#chatgpt-mermaid-\_r_bbf\_ [data-look="neo"].node rect,#chatgpt-mermaid-\_r_bbf\_ [data-look="neo"].cluster rect,#chatgpt-mermaid-\_r_bbf\_ [data-look="neo"].node polygon{stroke:url(#chatgpt-mermaid-\_r_bbf\_-gradient);filter:drop-shadow( 1px 2px 2px rgba(185,185,185,1));}#chatgpt-mermaid-\_r_bbf\_ [data-look="neo"].swimlane.cluster rect{filter:none;}#chatgpt-mermaid-\_r_bbf\_ [data-look="neo"].node path{stroke:url(#chatgpt-mermaid-\_r_bbf\_-gradient);stroke-width:1px;}#chatgpt-mermaid-\_r_bbf\_ [data-look="neo"].node .outer-path{filter:drop-shadow( 1px 2px 2px rgba(185,185,185,1));}#chatgpt-mermaid-\_r_bbf\_ [data-look="neo"].node .neo-line path{stroke:rgb(107, 198, 127);filter:none;}#chatgpt-mermaid-\_r_bbf\_ [data-look="neo"].node circle{stroke:url(#chatgpt-mermaid-\_r_bbf\_-gradient);filter:drop-shadow( 1px 2px 2px rgba(185,185,185,1));}#chatgpt-mermaid-\_r_bbf\_ [data-look="neo"].node circle .state-start{fill:#000000;}#chatgpt-mermaid-\_r_bbf\_ [data-look="neo"].icon-shape .icon{fill:url(#chatgpt-mermaid-\_r_bbf\_-gradient);filter:drop-shadow( 1px 2px 2px rgba(185,185,185,1));}#chatgpt-mermaid-\_r_bbf\_ [data-look="neo"].icon-shape .icon-neo path{stroke:url(#chatgpt-mermaid-\_r_bbf\_-gradient);filter:drop-shadow( 1px 2px 2px rgba(185,185,185,1));}#chatgpt-mermaid-\_r_bbf\_ :root{--mermaid-font-family:-apple-system-body,ui-sans-serif,-apple-system,system-ui,"Segoe UI",Helvetica,"Apple Color Emoji",Arial,sans-serif,"Segoe UI Emoji","Segoe UI Symbol";}Log migration intentDurable intentAllocate target extentRead source extentSource dataWrite target dataFlush target dataVerify checksumLog mapping switchDurable switch recordPublish new mappingSwitch completedLog migration completeReclaim source after safe point

这里的核心原则：

先保证目标数据持久化，再发布新映射，最后回收旧数据。

如果系统在 Mapping Switch 前崩溃：

- 旧映射仍然有效。
- 新分配的目标 Extent 可以在恢复时回收。

如果在 Mapping Switch 后崩溃：

- WAL 可以确认新映射已经生效。
- 旧 Extent 不能立即回收，需要确认没有旧映射读者。

### 如何避免迁移和并发写入冲突？

这是实际实现时必须解决的问题。

我建议第一版采用：

迁移期间固定源版本 + 版本化映射 + 最终短暂切换。

过程如下：

1. 获取当前 Extent 的版本和 Mapping Version。
2. 为目标分配新空间。
3. 复制源版本数据。
4. 记录迁移期间发生的增量修改，或者通过写入重定向让新修改进入新的版本。
5. 校验目标数据。
6. 使用原子映射切换发布新 Mapping。
7. 等待旧 Mapping 的读者退出。
8. 回收旧空间。

对于第一版，建议选择更简单的写时复制（COW）方案：

- 迁移时读取稳定的旧版本。
- 并发写入创建新的 Extent Version。
- 切换时重新检查源版本。
- 如果源版本已经变化，则重新同步或者放弃本次迁移。

这样避免实现复杂的双写系统。

## 3.9 硬件故障如何处理？

需要区分三类故障。

| 故障      | 处理方式                        |
| ------- | --------------------------- |
| DRAM 丢失 | 重新从持久化副本加载                  |
| GPU 故障  | 取消相关任务，释放工作集，从 NVMe/DRAM 重建 |
| NVMe 故障 | 使用冗余持久化副本恢复，单副本系统必须报告不可恢复风险 |

这意味着系统必须为每个 Extent 记录：

- 是否存在持久化副本。
- 持久化副本数量。
- 副本版本。
- 校验和。
- 所属设备。
- 最近一次成功 Flush 的位置。

尤其要强调：

一个只有单块 NVMe 的系统，即使有完整 WAL，也无法保证在这块 NVMe 物理损坏后恢复数据。

WAL 解决的是逻辑故障恢复和崩溃一致性，不等于硬件冗余。

# 四、问题三：Object Version、MVCC、WAL、Checkpoint 一致性设计

这是整个系统最难的部分。

前面两个问题主要解决数据如何组织、如何访问和如何迁移。

第三个问题解决的是：

在并发读写、数据迁移、进程崩溃和硬件故障的情况下，如何保证数据仍然正确。

我建议第一版采用：

- Object-level MVCC。
- Immutable Object Version。
- Copy-on-Write Extent。
- Append-only WAL。
- Checkpoint。
- Mapping Version。
- Epoch-based Reclamation。

这几个机制组合起来，比直接对 Extent 原地修改更容易实现可靠的一致性模型。

## 4.1 为什么需要 Object Version？

假设一个 Agent 正在读取一份 Document。

与此同时，另一个 Agent 更新了这份 Document。

如果直接覆盖原数据，就可能出现：

```
Reader:
    Read Header
    Read Old Payload

Writer:
    Update Payload
    Update Header

Reader:
    Read Remaining Payload
```

Reader 最终可能拿到新旧数据混合的内容。

这就是典型的数据一致性问题。

因此需要版本机制。

例如：

```
Object 1001
    |
    +-- Version 1
    |
    +-- Version 2
    |
    +-- Version 3
```

每次更新产生新版本。

读事务根据自己的 Snapshot 选择可见版本。

## 4.2 Object Version 数据结构

建议设计：

```
struct ObjectVersion {
    object_id: ObjectId,

    version_id: VersionId,

    xmin: TransactionId,

    xmax: Option<TransactionId>,

    create_lsn: Lsn,

    commit_lsn: Option<Lsn>,

    previous_version: Option<VersionId>,

    component_root: ComponentRoot,

    state: VersionState,
}
```

这里有一个设计重点：

不要让 Object Header 本身保存整个版本历史。

应该独立维护 Version Chain。

```
Object Directory
       |
       v
Object 1001
       |
       v
Version Directory
       |
       +-- Version 3
       |
       +-- Version 2
       |
       +-- Version 1
```

每个 Version 对应一个完整的逻辑对象状态。

但不要求每个版本都复制完整 Payload。

可以采用：

- 元数据 Copy-on-Write。
- Extent 级别共享。
- 修改的 Extent 创建新版本。
- 未修改的 Extent 继续共享。

例如：

```
Version 1
    |
    +-- Extent A
    +-- Extent B
    +-- Extent C

Version 2
    |
    +-- Extent A  (共享)
    +-- Extent B2 (新版本)
    +-- Extent C  (共享)
```

这可以大幅降低更新大型对象时的写放大。

## 4.3 MVCC 的核心：Snapshot

建议采用事务级 Snapshot。

例如：

```
Transaction T1:
    snapshot = 100

Transaction T2:
    snapshot = 105
```

T1 开始后，T2 更新了 Object 1001。

T1 仍然应该读取它开始时可见的旧版本。

### 版本可见性

建议使用类似传统 MVCC 的规则：

- 版本创建事务已经提交，且在 Snapshot 中可见。
- 版本删除事务尚未提交，或者在 Snapshot 中不可见。
- 创建和删除事务状态需要能够从事务状态管理器中查询。

不要仅凭 Object Version ID 大小判断可见性。

Version ID 是对象版本标识，不等于全局事务提交顺序。

例如：

```
Object 1001

Version 1:
    xmin = T10
    xmax = T20

Version 2:
    xmin = T20
    xmax = T30

Version 3:
    xmin = T30
    xmax = NULL
```

如果 T15 的事务 Snapshot 在 T20 提交之前建立，它仍然看到 Version 1。

如果 T25 的 Snapshot 在 T20 提交之后、T30 提交之前建立，它看到 Version 2。

这就是 MVCC 的核心。

## 4.4 事务管理器设计

建议拆分为四个内部模块：

```
Transaction Manager
       |
       +-- Transaction Registry
       |
       +-- Snapshot Manager
       |
       +-- Commit Manager
       |
       +-- Version Visibility Manager
```

核心事务结构：

```
struct StorageTransaction {
    xid: TransactionId,

    state: TransactionState,

    snapshot: Snapshot,

    read_set: ReadSet,

    write_set: WriteSet,

    start_lsn: Lsn,

    commit_lsn: Option<Lsn>,
}
```

这里有一个非常重要的实现建议：

第一版不要同时引入全局 CSN、复杂的分布式事务时间戳和多版本提交协议。

先采用清晰的事务 ID、Snapshot 和 Commit Status 体系。

如果未来需要全局提交序列，再单独引入 CSN。

### Snapshot 数据结构

```
struct Snapshot {
    xmin: TransactionId,

    xmax: TransactionId,

    active_xids: Vec<TransactionId>,

    snapshot_id: SnapshotId,
}
```

Snapshot 的关键职责是：

确定当前事务可以看到哪些已经提交的版本。

## 4.5 写入流程：Copy-on-Write

以修改一个 Tensor 为例。

初始状态：

```
Object 1001
    |
    +-- Version 1
           |
           +-- Extent A
           +-- Extent B
           +-- Extent C
```

事务 T20 修改 Extent B。

不直接覆盖 Extent B。

而是：

```
Object 1001
    |
    +-- Version 1
    |      |
    |      +-- Extent A
    |      +-- Extent B
    |      +-- Extent C
    |
    +-- Version 2
           |
           +-- Extent A (共享)
           +-- Extent B2 (新建)
           +-- Extent C (共享)
```

完整流程：

Storage DeviceWAL ManagerExtent ManagerObject ManagerMVCC ManagerTransactionStorage DeviceWAL ManagerExtent ManagerObject ManagerMVCC ManagerTransaction#chatgpt-mermaid-\_r_bdl\_{font-family:-apple-system-body,ui-sans-serif,-apple-system,system-ui,"Segoe UI",Helvetica,"Apple Color Emoji",Arial,sans-serif,"Segoe UI Emoji","Segoe UI Symbol";font-size:16px;fill:rgb(13, 13, 13);}@keyframes edge-animation-frame{from{stroke-dashoffset:0;}}@keyframes dash{to{stroke-dashoffset:0;}}#chatgpt-mermaid-\_r_bdl\_ .edge-animation-slow{stroke-dasharray:9,5!important;stroke-dashoffset:900;animation:dash 50s linear infinite;stroke-linecap:round;}#chatgpt-mermaid-\_r_bdl\_ .edge-animation-fast{stroke-dasharray:9,5!important;stroke-dashoffset:900;animation:dash 20s linear infinite;stroke-linecap:round;}#chatgpt-mermaid-\_r_bdl\_ .error-icon{fill:rgb(243, 243, 243);}#chatgpt-mermaid-\_r_bdl\_ .error-text{fill:rgb(13, 13, 13);stroke:rgb(13, 13, 13);}#chatgpt-mermaid-\_r_bdl\_ .edge-thickness-normal{stroke-width:1px;}#chatgpt-mermaid-\_r_bdl\_ .edge-thickness-thick{stroke-width:3.5px;}#chatgpt-mermaid-\_r_bdl\_ .edge-pattern-solid{stroke-dasharray:0;}#chatgpt-mermaid-\_r_bdl\_ .edge-thickness-invisible{stroke-width:0;fill:none;}#chatgpt-mermaid-\_r_bdl\_ .edge-pattern-dashed{stroke-dasharray:3;}#chatgpt-mermaid-\_r_bdl\_ .edge-pattern-dotted{stroke-dasharray:2;}#chatgpt-mermaid-\_r_bdl\_ .marker{fill:rgb(143, 143, 143);stroke:rgb(143, 143, 143);}#chatgpt-mermaid-\_r_bdl\_ .marker.cross{stroke:rgb(143, 143, 143);}#chatgpt-mermaid-\_r_bdl\_ svg{font-family:-apple-system-body,ui-sans-serif,-apple-system,system-ui,"Segoe UI",Helvetica,"Apple Color Emoji",Arial,sans-serif,"Segoe UI Emoji","Segoe UI Symbol";font-size:16px;}#chatgpt-mermaid-\_r_bdl\_ p{margin:0;}#chatgpt-mermaid-\_r_bdl\_ .actor{stroke:rgb(107, 198, 127);fill:rgb(226, 242, 230);stroke-width:1;}#chatgpt-mermaid-\_r_bdl\_ rect.actor.outer-path[data-look="neo"]{filter:drop-shadow( 1px 2px 2px rgba(185,185,185,1));}#chatgpt-mermaid-\_r_bdl\_ rect.note[data-look="neo"]{stroke:rgb(239, 139, 87);fill:rgb(243, 243, 243);filter:drop-shadow( 1px 2px 2px rgba(185,185,185,1));}#chatgpt-mermaid-\_r_bdl\_ text.actor>tspan{fill:rgb(13, 13, 13);stroke:none;}#chatgpt-mermaid-\_r_bdl\_ .actor-line{stroke:rgb(143, 143, 143);}#chatgpt-mermaid-\_r_bdl\_ .innerArc{stroke-width:1.5;stroke-dasharray:none;}#chatgpt-mermaid-\_r_bdl\_ .messageLine0{stroke-width:1.5;stroke-dasharray:none;stroke:rgb(143, 143, 143);}#chatgpt-mermaid-\_r_bdl\_ .messageLine1{stroke-width:1.5;stroke-dasharray:2,2;stroke:rgb(143, 143, 143);}#chatgpt-mermaid-\_r_bdl\_ [id$="-arrowhead"] path{fill:rgb(143, 143, 143);stroke:rgb(143, 143, 143);}#chatgpt-mermaid-\_r_bdl\_ .sequenceNumber{fill:#707070;}#chatgpt-mermaid-\_r_bdl\_ [id$="-sequencenumber"]{fill:rgb(143, 143, 143);}#chatgpt-mermaid-\_r_bdl\_ [id$="-crosshead"] path{fill:rgb(143, 143, 143);stroke:rgb(143, 143, 143);}#chatgpt-mermaid-\_r_bdl\_ .messageText{fill:rgb(13, 13, 13);stroke:none;}#chatgpt-mermaid-\_r_bdl\_ .labelBox{stroke:rgba(0, 0, 0, 0.1);fill:rgb(252, 252, 252);filter:none;}#chatgpt-mermaid-\_r_bdl\_ .labelText,#chatgpt-mermaid-\_r_bdl\_ .labelText>tspan{fill:rgb(13, 13, 13);stroke:none;}#chatgpt-mermaid-\_r_bdl\_ .loopText,#chatgpt-mermaid-\_r_bdl\_ .loopText>tspan{fill:rgb(13, 13, 13);stroke:none;}#chatgpt-mermaid-\_r_bdl\_ .sectionTitle,#chatgpt-mermaid-\_r_bdl\_ .sectionTitle>tspan{fill:rgb(13, 13, 13);stroke:none;}#chatgpt-mermaid-\_r_bdl\_ .loopLine{stroke-width:2px;stroke-dasharray:2,2;stroke:rgba(0, 0, 0, 0.1);fill:rgba(0, 0, 0, 0.1);}#chatgpt-mermaid-\_r_bdl\_ .note{stroke:rgb(239, 139, 87);fill:rgb(243, 243, 243);}#chatgpt-mermaid-\_r_bdl\_ .noteText,#chatgpt-mermaid-\_r_bdl\_ .noteText>tspan{fill:rgb(13, 13, 13);stroke:none;font-weight:normal;}#chatgpt-mermaid-\_r_bdl\_ .activation0{fill:rgb(243, 243, 243);stroke:hsl(0, 0%, 85.2941176471%);}#chatgpt-mermaid-\_r_bdl\_ .activation1{fill:rgb(243, 243, 243);stroke:hsl(0, 0%, 85.2941176471%);}#chatgpt-mermaid-\_r_bdl\_ .activation2{fill:rgb(243, 243, 243);stroke:hsl(0, 0%, 85.2941176471%);}#chatgpt-mermaid-\_r_bdl\_ .actorPopupMenu{position:absolute;}#chatgpt-mermaid-\_r_bdl\_ .actorPopupMenuPanel{position:absolute;fill:rgb(226, 242, 230);box-shadow:0px 8px 16px 0px rgba(0,0,0,0.2);filter:drop-shadow(3px 5px 2px rgb(0 0 0 / 0.4));}#chatgpt-mermaid-\_r_bdl\_ .actor-man circle,#chatgpt-mermaid-\_r_bdl\_ line{fill:rgb(226, 242, 230);stroke-width:2px;}#chatgpt-mermaid-\_r_bdl\_ g rect.rect{filter:drop-shadow( 1px 2px 2px rgba(185,185,185,1));stroke:rgb(107, 198, 127);}#chatgpt-mermaid-\_r_bdl\_ .node .neo-node{stroke:rgb(107, 198, 127);}#chatgpt-mermaid-\_r_bdl\_ [data-look="neo"].node rect,#chatgpt-mermaid-\_r_bdl\_ [data-look="neo"].cluster rect,#chatgpt-mermaid-\_r_bdl\_ [data-look="neo"].node polygon{stroke:url(#chatgpt-mermaid-\_r_bdl\_-gradient);filter:drop-shadow( 1px 2px 2px rgba(185,185,185,1));}#chatgpt-mermaid-\_r_bdl\_ [data-look="neo"].swimlane.cluster rect{filter:none;}#chatgpt-mermaid-\_r_bdl\_ [data-look="neo"].node path{stroke:url(#chatgpt-mermaid-\_r_bdl\_-gradient);stroke-width:1px;}#chatgpt-mermaid-\_r_bdl\_ [data-look="neo"].node .outer-path{filter:drop-shadow( 1px 2px 2px rgba(185,185,185,1));}#chatgpt-mermaid-\_r_bdl\_ [data-look="neo"].node .neo-line path{stroke:rgb(107, 198, 127);filter:none;}#chatgpt-mermaid-\_r_bdl\_ [data-look="neo"].node circle{stroke:url(#chatgpt-mermaid-\_r_bdl\_-gradient);filter:drop-shadow( 1px 2px 2px rgba(185,185,185,1));}#chatgpt-mermaid-\_r_bdl\_ [data-look="neo"].node circle .state-start{fill:#000000;}#chatgpt-mermaid-\_r_bdl\_ [data-look="neo"].icon-shape .icon{fill:url(#chatgpt-mermaid-\_r_bdl\_-gradient);filter:drop-shadow( 1px 2px 2px rgba(185,185,185,1));}#chatgpt-mermaid-\_r_bdl\_ [data-look="neo"].icon-shape .icon-neo path{stroke:url(#chatgpt-mermaid-\_r_bdl\_-gradient);filter:drop-shadow( 1px 2px 2px rgba(185,185,185,1));}#chatgpt-mermaid-\_r_bdl\_ :root{--mermaid-font-family:-apple-system-body,ui-sans-serif,-apple-system,system-ui,"Segoe UI",Helvetica,"Apple Color Emoji",Arial,sans-serif,"Segoe UI Emoji","Segoe UI Symbol";}Begin transactionSnapshotRead ObjectResolve visible versionVersion 1Create new ExtentAllocate and write Extent B2Log object version changesWAL LSNCommit transactionAppend COMMITCommit LSNPublish committed versionCommit success

这个流程还需要补充一个关键条件：

事务提交成功的前提，是对应 WAL 已经持久化。

而且如果采用直接数据写入 NVMe 的方式，新数据和对应元数据也必须满足崩溃恢复协议。

## 4.6 Object Version 与 Extent Version 的关系

这里建议明确区分两个版本体系：

| 版本              | 管理对象        | 用途           |
| --------------- | ----------- | ------------ |
| Object Version  | 逻辑对象整体      | 事务可见性        |
| Extent Version  | 物理数据区域的内容版本 | 数据共享、COW 和回收 |
| Mapping Version | 物理位置映射      | 迁移和重映射       |
| Index Version   | 索引状态        | 检索可见性        |

这四种版本不能简单合并。

例如：

Object Version 3 可能仍然使用 Extent Version 5，但它的 Mapping Version 从 1 变成了 2。

这意味着：

- 逻辑内容没有变化。
- 物理存储位置发生变化。
- Object Version 不一定需要增加。
- Mapping Version 必须更新。

这种分离是实现无感数据迁移的关键。

# 五、WAL 设计：从 Page WAL 转向 Object WAL

## 5.1 传统 WAL 的设计问题

传统数据库 WAL 通常记录物理或逻辑层面的数据变化。

例如：

```
UPDATE HEAP PAGE
    |
    +-- Block Number
    +-- Offset Number
    +-- Tuple Data
```

它适合 Page-oriented Storage。

但是新系统需要管理：

- Object 创建和更新。
- Object Version。
- Extent 分配。
- Mapping 切换。
- Index 更新。
- 事务提交。
- 数据迁移。

因此建议采用：

Object-aware WAL + Physical Allocation WAL。

注意，这不是说所有 WAL 都只记录逻辑 Object 操作。

对于 Extent 分配、元数据结构和底层恢复，仍然需要物理层面的恢复信息。

## 5.2 WAL 记录类型

建议分成六类。

| 类型          | WAL Record                          | 作用     |
| ----------- | ----------------------------------- | ------ |
| Transaction | TXN_BEGIN / TXN_COMMIT / TXN_ABORT  | 事务状态   |
| Object      | OBJECT_CREATE / OBJECT_VERSION      | 对象生命周期 |
| Extent      | EXTENT_ALLOC / EXTENT_FREE          | 空间管理   |
| Mapping     | MAPPING_CREATE / MAPPING_SWITCH     | 物理位置   |
| Index       | INDEX_DELTA / INDEX_CHECKPOINT      | 索引恢复   |
| Maintenance | MIGRATION / COMPACTION / CHECKPOINT | 后台维护   |

示例：

```
enum WalRecord {
    TxnBegin {
        xid: TransactionId,
    },

    ObjectCreate {
        object_id: ObjectId,
        version_id: VersionId,
    },

    ObjectVersionCreate {
        object_id: ObjectId,
        old_version: Option<VersionId>,
        new_version: VersionId,
    },

    ExtentAllocate {
        extent_id: ExtentId,
        physical_extent_id: PhysicalExtentId,
    },

    MappingSwitch {
        extent_id: ExtentId,
        old_mapping: MappingId,
        new_mapping: MappingId,
    },

    IndexDelta {
        index_id: IndexId,
        object_id: ObjectId,
        version_id: VersionId,
    },

    TxnCommit {
        xid: TransactionId,
    },

    TxnAbort {
        xid: TransactionId,
    },
}
```

实际实现还需要为每种 WAL 记录增加：

- Record Length。
- Record Type。
- Transaction ID。
- LSN。
- Checksum。
- 版本号。
- 必要的依赖信息。

## 5.3 WAL 与数据 Payload 分离

这是 AI-Native Storage 非常重要的设计。

假设一个 Tensor 大小为 10 GB。

如果每次更新 Tensor 都把完整数据写入 WAL，那么系统会产生极大的写放大。

因此建议：

```
Tensor Update
      |
      +-- Allocate New Extent
      |
      +-- Write Tensor Data
      |
      +-- Flush Tensor Data
      |
      +-- WAL: Version Metadata
      |
      +-- WAL: Mapping Changes
      |
      +-- WAL: Commit
```

这里可以将 Payload 作为独立的数据写入路径。

WAL 记录对象版本、Extent 分配、数据校验和以及映射变化。

但是需要严格遵守恢复顺序。

例如：

1. 新数据必须在对应提交记录持久化前达到协议要求的持久化状态。
2. Mapping Switch 必须可恢复。
3. Commit 不能早于它依赖的数据和元数据持久化。

如果使用异步写入，还需要 Flush Barrier 和 LSN 依赖管理。

## 5.4 WAL 的 LSN 管理

建议统一使用 LSN：

```
LSN
 |
 +-- WAL Record Position
 |
 +-- Transaction Commit Position
 |
 +-- Object Version Creation Position
 |
 +-- Mapping Switch Position
 |
 +-- Checkpoint Position
```

LSN 的核心作用是：

- 判断 WAL 是否已经持久化。
- 判断某个对象版本对应的修改是否可以恢复。
- 判断 Checkpoint 可以安全推进到哪里。
- 判断旧 Extent 是否可以回收。

需要注意：

LSN 表示 WAL 顺序，不等于事务提交顺序，也不等于对象版本顺序。

不能把这三个概念混用。

# 六、Checkpoint 与 Recovery：如何保证崩溃后恢复

WAL 解决了修改记录问题，但如果系统运行数月，WAL 会不断增长。

因此需要 Checkpoint。

## 6.1 Checkpoint 的目标

Checkpoint 主要完成四件事：

1. 固化已经提交的 Object Version。
2. 固化 Extent 和 Mapping 元数据。
3. 固化索引的可恢复状态。
4. 确定一个可以安全截断旧 WAL 的恢复边界。

建议采用：

Metadata Snapshot + WAL LSN Barrier + Manifest。

不要简单地把所有内存状态一次性刷盘。

因为系统可能存在大量并发事务和后台迁移任务。

## 6.2 Checkpoint 数据结构

建议设计：

```
struct CheckpointManifest {
    checkpoint_id: CheckpointId,

    checkpoint_lsn: Lsn,

    transaction_state_ref: StorageRef,

    object_directory_ref: StorageRef,

    version_directory_ref: StorageRef,

    extent_directory_ref: StorageRef,

    mapping_directory_ref: StorageRef,

    index_manifest_ref: StorageRef,

    checksum: u64,
}
```

Checkpoint Manifest 是恢复时的重要入口。

它描述了一个完整、可验证的元数据快照。

### 为什么需要 Manifest？

假设系统有：

```
Object Directory
Version Directory
Extent Directory
Mapping Directory
Index Directory
```

如果这些数据分别写入磁盘，中间发生崩溃，就可能出现：

- Object Directory 已更新。
- Extent Directory 尚未更新。
- Mapping Directory 仍然是旧版本。

这样恢复时会出现引用关系不一致。

因此需要一个统一的提交入口。

建议采用：

```
Checkpoint Files
       |
       v
Checkpoint Manifest
       |
       v
Atomic Manifest Publication
```

所有文件先写入临时位置，完成持久化和校验后，再通过原子发布 Manifest 的方式使 Checkpoint 生效。

实际发布时需要考虑文件系统原子重命名和目录持久化语义，不能只依赖 rename 返回成功。

## 6.3 Checkpoint 执行流程

\#chatgpt-mermaid-\_r_bf2\_{font-family:-apple-system-body,ui-sans-serif,-apple-system,system-ui,"Segoe UI",Helvetica,"Apple Color Emoji",Arial,sans-serif,"Segoe UI Emoji","Segoe UI Symbol";font-size:16px;fill:rgb(13, 13, 13);}@keyframes edge-animation-frame{from{stroke-dashoffset:0;}}@keyframes dash{to{stroke-dashoffset:0;}}#chatgpt-mermaid-\_r_bf2\_ .edge-animation-slow{stroke-dasharray:9,5!important;stroke-dashoffset:900;animation:dash 50s linear infinite;stroke-linecap:round;}#chatgpt-mermaid-\_r_bf2\_ .edge-animation-fast{stroke-dasharray:9,5!important;stroke-dashoffset:900;animation:dash 20s linear infinite;stroke-linecap:round;}#chatgpt-mermaid-\_r_bf2\_ .error-icon{fill:rgb(243, 243, 243);}#chatgpt-mermaid-\_r_bf2\_ .error-text{fill:rgb(13, 13, 13);stroke:rgb(13, 13, 13);}#chatgpt-mermaid-\_r_bf2\_ .edge-thickness-normal{stroke-width:1px;}#chatgpt-mermaid-\_r_bf2\_ .edge-thickness-thick{stroke-width:3.5px;}#chatgpt-mermaid-\_r_bf2\_ .edge-pattern-solid{stroke-dasharray:0;}#chatgpt-mermaid-\_r_bf2\_ .edge-thickness-invisible{stroke-width:0;fill:none;}#chatgpt-mermaid-\_r_bf2\_ .edge-pattern-dashed{stroke-dasharray:3;}#chatgpt-mermaid-\_r_bf2\_ .edge-pattern-dotted{stroke-dasharray:2;}#chatgpt-mermaid-\_r_bf2\_ .marker{fill:rgb(143, 143, 143);stroke:rgb(143, 143, 143);}#chatgpt-mermaid-\_r_bf2\_ .marker.cross{stroke:rgb(143, 143, 143);}#chatgpt-mermaid-\_r_bf2\_ svg{font-family:-apple-system-body,ui-sans-serif,-apple-system,system-ui,"Segoe UI",Helvetica,"Apple Color Emoji",Arial,sans-serif,"Segoe UI Emoji","Segoe UI Symbol";font-size:16px;}#chatgpt-mermaid-\_r_bf2\_ p{margin:0;}#chatgpt-mermaid-\_r_bf2\_ .label{font-family:-apple-system-body,ui-sans-serif,-apple-system,system-ui,"Segoe UI",Helvetica,"Apple Color Emoji",Arial,sans-serif,"Segoe UI Emoji","Segoe UI Symbol";color:rgb(13, 13, 13);}#chatgpt-mermaid-\_r_bf2\_ .cluster-label text{fill:rgb(13, 13, 13);}#chatgpt-mermaid-\_r_bf2\_ .cluster-label span{color:rgb(13, 13, 13);}#chatgpt-mermaid-\_r_bf2\_ .cluster-label span p{background-color:transparent;}#chatgpt-mermaid-\_r_bf2\_ .label text,#chatgpt-mermaid-\_r_bf2\_ span{fill:rgb(13, 13, 13);color:rgb(13, 13, 13);}#chatgpt-mermaid-\_r_bf2\_ .node rect,#chatgpt-mermaid-\_r_bf2\_ .node circle,#chatgpt-mermaid-\_r_bf2\_ .node ellipse,#chatgpt-mermaid-\_r_bf2\_ .node polygon,#chatgpt-mermaid-\_r_bf2\_ .node path{fill:rgb(226, 242, 230);stroke:rgb(107, 198, 127);stroke-width:1px;}#chatgpt-mermaid-\_r_bf2\_ .rough-node .label text,#chatgpt-mermaid-\_r_bf2\_ .node .label text,#chatgpt-mermaid-\_r_bf2\_ .image-shape .label,#chatgpt-mermaid-\_r_bf2\_ .icon-shape .label{text-anchor:middle;}#chatgpt-mermaid-\_r_bf2\_ .node .katex path{fill:#000;stroke:#000;stroke-width:1px;}#chatgpt-mermaid-\_r_bf2\_ .rough-node .label,#chatgpt-mermaid-\_r_bf2\_ .node .label,#chatgpt-mermaid-\_r_bf2\_ .image-shape .label,#chatgpt-mermaid-\_r_bf2\_ .icon-shape .label{text-align:center;}#chatgpt-mermaid-\_r_bf2\_ .node.clickable{cursor:pointer;}#chatgpt-mermaid-\_r_bf2\_ .root .anchor path{fill:rgb(143, 143, 143)!important;stroke-width:0;stroke:rgb(143, 143, 143);}#chatgpt-mermaid-\_r_bf2\_ .arrowheadPath{fill:rgb(143, 143, 143);}#chatgpt-mermaid-\_r_bf2\_ .edgePath .path{stroke:rgb(143, 143, 143);stroke-width:1px;}#chatgpt-mermaid-\_r_bf2\_ .flowchart-link{stroke:rgb(143, 143, 143);fill:none;}#chatgpt-mermaid-\_r_bf2\_ .edgeLabel{background-color:rgb(252, 252, 252);text-align:center;}#chatgpt-mermaid-\_r_bf2\_ .edgeLabel p{background-color:rgb(252, 252, 252);}#chatgpt-mermaid-\_r_bf2\_ .edgeLabel rect{opacity:0.5;background-color:rgb(252, 252, 252);fill:rgb(252, 252, 252);}#chatgpt-mermaid-\_r_bf2\_ .labelBkg{background-color:rgba(252, 252, 252, 0.5);}#chatgpt-mermaid-\_r_bf2\_ .cluster rect{fill:rgb(243, 243, 243);stroke:rgba(0, 0, 0, 0.1);stroke-width:1px;}#chatgpt-mermaid-\_r_bf2\_ .cluster text{fill:rgb(13, 13, 13);}#chatgpt-mermaid-\_r_bf2\_ .cluster span{color:rgb(13, 13, 13);}#chatgpt-mermaid-\_r_bf2\_ div.mermaidTooltip{position:absolute;text-align:center;max-width:200px;padding:2px;font-family:-apple-system-body,ui-sans-serif,-apple-system,system-ui,"Segoe UI",Helvetica,"Apple Color Emoji",Arial,sans-serif,"Segoe UI Emoji","Segoe UI Symbol";font-size:12px;background:rgb(243, 243, 243);border:1px solid rgba(0, 0, 0, 0.1);border-radius:2px;pointer-events:none;z-index:100;}#chatgpt-mermaid-\_r_bf2\_ .flowchartTitleText{text-anchor:middle;font-size:18px;fill:rgb(13, 13, 13);}#chatgpt-mermaid-\_r_bf2\_ rect.text{fill:none;stroke-width:0;}#chatgpt-mermaid-\_r_bf2\_ .icon-shape,#chatgpt-mermaid-\_r_bf2\_ .image-shape{background-color:rgb(252, 252, 252);text-align:center;}#chatgpt-mermaid-\_r_bf2\_ .icon-shape p,#chatgpt-mermaid-\_r_bf2\_ .image-shape p{background-color:rgb(252, 252, 252);padding:2px;}#chatgpt-mermaid-\_r_bf2\_ .icon-shape .label rect,#chatgpt-mermaid-\_r_bf2\_ .image-shape .label rect{opacity:0.5;background-color:rgb(252, 252, 252);fill:rgb(252, 252, 252);}#chatgpt-mermaid-\_r_bf2\_ .label-icon{display:inline-block;height:1em;overflow:visible;vertical-align:-0.125em;}#chatgpt-mermaid-\_r_bf2\_ .node .label-icon path{fill:currentColor;stroke:revert;stroke-width:revert;}#chatgpt-mermaid-\_r_bf2\_ .node .neo-node{stroke:rgb(107, 198, 127);}#chatgpt-mermaid-\_r_bf2\_ [data-look="neo"].node rect,#chatgpt-mermaid-\_r_bf2\_ [data-look="neo"].cluster rect,#chatgpt-mermaid-\_r_bf2\_ [data-look="neo"].node polygon{stroke:url(#chatgpt-mermaid-\_r_bf2\_-gradient);filter:drop-shadow( 1px 2px 2px rgba(185,185,185,1));}#chatgpt-mermaid-\_r_bf2\_ [data-look="neo"].swimlane.cluster rect{filter:none;}#chatgpt-mermaid-\_r_bf2\_ [data-look="neo"].node path{stroke:url(#chatgpt-mermaid-\_r_bf2\_-gradient);stroke-width:1px;}#chatgpt-mermaid-\_r_bf2\_ [data-look="neo"].node .outer-path{filter:drop-shadow( 1px 2px 2px rgba(185,185,185,1));}#chatgpt-mermaid-\_r_bf2\_ [data-look="neo"].node .neo-line path{stroke:rgb(107, 198, 127);filter:none;}#chatgpt-mermaid-\_r_bf2\_ [data-look="neo"].node circle{stroke:url(#chatgpt-mermaid-\_r_bf2\_-gradient);filter:drop-shadow( 1px 2px 2px rgba(185,185,185,1));}#chatgpt-mermaid-\_r_bf2\_ [data-look="neo"].node circle .state-start{fill:#000000;}#chatgpt-mermaid-\_r_bf2\_ [data-look="neo"].icon-shape .icon{fill:url(#chatgpt-mermaid-\_r_bf2\_-gradient);filter:drop-shadow( 1px 2px 2px rgba(185,185,185,1));}#chatgpt-mermaid-\_r_bf2\_ [data-look="neo"].icon-shape .icon-neo path{stroke:url(#chatgpt-mermaid-\_r_bf2\_-gradient);filter:drop-shadow( 1px 2px 2px rgba(185,185,185,1));}#chatgpt-mermaid-\_r_bf2\_ .node text{font-size:14px;font-weight:600;letter-spacing:normal;fill:rgb(0, 105, 42);}#chatgpt-mermaid-\_r_bf2\_ .edgeLabels text{font-size:13px;font-weight:600;letter-spacing:-0.08px;fill:rgb(0, 105, 42);}#chatgpt-mermaid-\_r_bf2\_ .node tspan[font-weight="normal"],#chatgpt-mermaid-\_r_bf2\_ .edgeLabels tspan[font-weight="normal"]{font-weight:600;}#chatgpt-mermaid-\_r_bf2\_ .edgeLabel .label rect{opacity:1;rx:13px;ry:13px;fill:rgb(237, 250, 242);stroke:rgb(166, 211, 184);stroke-width:1px;}#chatgpt-mermaid-\_r_bf2\_ .node rect,#chatgpt-mermaid-\_r_bf2\_ .node circle,#chatgpt-mermaid-\_r_bf2\_ .node ellipse,#chatgpt-mermaid-\_r_bf2\_ .node polygon,#chatgpt-mermaid-\_r_bf2\_ .node path{fill:rgb(217, 244, 228);stroke:rgba(0, 0, 0, 0.1);stroke-width:1px;}#chatgpt-mermaid-\_r_bf2\_ .node rect{rx:16px;ry:16px;}#chatgpt-mermaid-\_r_bf2\_ .node.mermaid-decision .label-container{fill:rgb(237, 250, 242);stroke:rgb(195, 220, 205);stroke-dasharray:2,2;}#chatgpt-mermaid-\_r_bf2\_ .edgePaths .flowchart-link{stroke:rgb(143, 143, 143);stroke-width:1px;stroke-linecap:round;stroke-linejoin:round;}#chatgpt-mermaid-\_r_bf2\_ .marker{fill:rgb(143, 143, 143);stroke:rgb(143, 143, 143);}#chatgpt-mermaid-\_r_bf2\_ :root{--mermaid-font-family:-apple-system-body,ui-sans-serif,-apple-system,system-ui,"Segoe UI",Helvetica,"Apple Color Emoji",Arial,sans-serif,"Segoe UI Emoji","Segoe UI Symbol";}Checkpoint TriggerCapture Checkpoint LSNCapture Consistent MetadataSnapshotWrite Object DirectoryWrite Version DirectoryWrite Extent and MappingWrite Index ManifestFlush Checkpoint FilesWrite Checkpoint ManifestFlush ManifestPublish ManifestAdvance WAL RetentionBoundary

这里有一个关键问题：

Checkpoint 期间可能还有事务继续提交。

因此 Checkpoint 不能简单地假定所有数据都停留在 Checkpoint 开始时的状态。

正确做法是：

- 在一个确定的 LSN 边界建立元数据快照。
- Checkpoint 期间产生的新 WAL 继续正常记录。
- 恢复时从 Checkpoint LSN 之后继续重放 WAL。

这类似传统数据库的 Checkpoint 原理，但需要将对象版本、映射和硬件资源状态纳入一致性范围。

## 6.4 崩溃恢复流程

假设系统运行期间：

```
LSN 100: Object Create
LSN 120: Object Version Update
LSN 150: Transaction Commit
LSN 180: Checkpoint
LSN 200: Object Update
LSN 220: Extent Migration
LSN 250: Transaction Commit
LSN 270: System Crash
```

恢复过程：

\#chatgpt-mermaid-\_r_bfe\_{font-family:-apple-system-body,ui-sans-serif,-apple-system,system-ui,"Segoe UI",Helvetica,"Apple Color Emoji",Arial,sans-serif,"Segoe UI Emoji","Segoe UI Symbol";font-size:16px;fill:rgb(13, 13, 13);}@keyframes edge-animation-frame{from{stroke-dashoffset:0;}}@keyframes dash{to{stroke-dashoffset:0;}}#chatgpt-mermaid-\_r_bfe\_ .edge-animation-slow{stroke-dasharray:9,5!important;stroke-dashoffset:900;animation:dash 50s linear infinite;stroke-linecap:round;}#chatgpt-mermaid-\_r_bfe\_ .edge-animation-fast{stroke-dasharray:9,5!important;stroke-dashoffset:900;animation:dash 20s linear infinite;stroke-linecap:round;}#chatgpt-mermaid-\_r_bfe\_ .error-icon{fill:rgb(243, 243, 243);}#chatgpt-mermaid-\_r_bfe\_ .error-text{fill:rgb(13, 13, 13);stroke:rgb(13, 13, 13);}#chatgpt-mermaid-\_r_bfe\_ .edge-thickness-normal{stroke-width:1px;}#chatgpt-mermaid-\_r_bfe\_ .edge-thickness-thick{stroke-width:3.5px;}#chatgpt-mermaid-\_r_bfe\_ .edge-pattern-solid{stroke-dasharray:0;}#chatgpt-mermaid-\_r_bfe\_ .edge-thickness-invisible{stroke-width:0;fill:none;}#chatgpt-mermaid-\_r_bfe\_ .edge-pattern-dashed{stroke-dasharray:3;}#chatgpt-mermaid-\_r_bfe\_ .edge-pattern-dotted{stroke-dasharray:2;}#chatgpt-mermaid-\_r_bfe\_ .marker{fill:rgb(143, 143, 143);stroke:rgb(143, 143, 143);}#chatgpt-mermaid-\_r_bfe\_ .marker.cross{stroke:rgb(143, 143, 143);}#chatgpt-mermaid-\_r_bfe\_ svg{font-family:-apple-system-body,ui-sans-serif,-apple-system,system-ui,"Segoe UI",Helvetica,"Apple Color Emoji",Arial,sans-serif,"Segoe UI Emoji","Segoe UI Symbol";font-size:16px;}#chatgpt-mermaid-\_r_bfe\_ p{margin:0;}#chatgpt-mermaid-\_r_bfe\_ .label{font-family:-apple-system-body,ui-sans-serif,-apple-system,system-ui,"Segoe UI",Helvetica,"Apple Color Emoji",Arial,sans-serif,"Segoe UI Emoji","Segoe UI Symbol";color:rgb(13, 13, 13);}#chatgpt-mermaid-\_r_bfe\_ .cluster-label text{fill:rgb(13, 13, 13);}#chatgpt-mermaid-\_r_bfe\_ .cluster-label span{color:rgb(13, 13, 13);}#chatgpt-mermaid-\_r_bfe\_ .cluster-label span p{background-color:transparent;}#chatgpt-mermaid-\_r_bfe\_ .label text,#chatgpt-mermaid-\_r_bfe\_ span{fill:rgb(13, 13, 13);color:rgb(13, 13, 13);}#chatgpt-mermaid-\_r_bfe\_ .node rect,#chatgpt-mermaid-\_r_bfe\_ .node circle,#chatgpt-mermaid-\_r_bfe\_ .node ellipse,#chatgpt-mermaid-\_r_bfe\_ .node polygon,#chatgpt-mermaid-\_r_bfe\_ .node path{fill:rgb(226, 242, 230);stroke:rgb(107, 198, 127);stroke-width:1px;}#chatgpt-mermaid-\_r_bfe\_ .rough-node .label text,#chatgpt-mermaid-\_r_bfe\_ .node .label text,#chatgpt-mermaid-\_r_bfe\_ .image-shape .label,#chatgpt-mermaid-\_r_bfe\_ .icon-shape .label{text-anchor:middle;}#chatgpt-mermaid-\_r_bfe\_ .node .katex path{fill:#000;stroke:#000;stroke-width:1px;}#chatgpt-mermaid-\_r_bfe\_ .rough-node .label,#chatgpt-mermaid-\_r_bfe\_ .node .label,#chatgpt-mermaid-\_r_bfe\_ .image-shape .label,#chatgpt-mermaid-\_r_bfe\_ .icon-shape .label{text-align:center;}#chatgpt-mermaid-\_r_bfe\_ .node.clickable{cursor:pointer;}#chatgpt-mermaid-\_r_bfe\_ .root .anchor path{fill:rgb(143, 143, 143)!important;stroke-width:0;stroke:rgb(143, 143, 143);}#chatgpt-mermaid-\_r_bfe\_ .arrowheadPath{fill:rgb(143, 143, 143);}#chatgpt-mermaid-\_r_bfe\_ .edgePath .path{stroke:rgb(143, 143, 143);stroke-width:1px;}#chatgpt-mermaid-\_r_bfe\_ .flowchart-link{stroke:rgb(143, 143, 143);fill:none;}#chatgpt-mermaid-\_r_bfe\_ .edgeLabel{background-color:rgb(252, 252, 252);text-align:center;}#chatgpt-mermaid-\_r_bfe\_ .edgeLabel p{background-color:rgb(252, 252, 252);}#chatgpt-mermaid-\_r_bfe\_ .edgeLabel rect{opacity:0.5;background-color:rgb(252, 252, 252);fill:rgb(252, 252, 252);}#chatgpt-mermaid-\_r_bfe\_ .labelBkg{background-color:rgba(252, 252, 252, 0.5);}#chatgpt-mermaid-\_r_bfe\_ .cluster rect{fill:rgb(243, 243, 243);stroke:rgba(0, 0, 0, 0.1);stroke-width:1px;}#chatgpt-mermaid-\_r_bfe\_ .cluster text{fill:rgb(13, 13, 13);}#chatgpt-mermaid-\_r_bfe\_ .cluster span{color:rgb(13, 13, 13);}#chatgpt-mermaid-\_r_bfe\_ div.mermaidTooltip{position:absolute;text-align:center;max-width:200px;padding:2px;font-family:-apple-system-body,ui-sans-serif,-apple-system,system-ui,"Segoe UI",Helvetica,"Apple Color Emoji",Arial,sans-serif,"Segoe UI Emoji","Segoe UI Symbol";font-size:12px;background:rgb(243, 243, 243);border:1px solid rgba(0, 0, 0, 0.1);border-radius:2px;pointer-events:none;z-index:100;}#chatgpt-mermaid-\_r_bfe\_ .flowchartTitleText{text-anchor:middle;font-size:18px;fill:rgb(13, 13, 13);}#chatgpt-mermaid-\_r_bfe\_ rect.text{fill:none;stroke-width:0;}#chatgpt-mermaid-\_r_bfe\_ .icon-shape,#chatgpt-mermaid-\_r_bfe\_ .image-shape{background-color:rgb(252, 252, 252);text-align:center;}#chatgpt-mermaid-\_r_bfe\_ .icon-shape p,#chatgpt-mermaid-\_r_bfe\_ .image-shape p{background-color:rgb(252, 252, 252);padding:2px;}#chatgpt-mermaid-\_r_bfe\_ .icon-shape .label rect,#chatgpt-mermaid-\_r_bfe\_ .image-shape .label rect{opacity:0.5;background-color:rgb(252, 252, 252);fill:rgb(252, 252, 252);}#chatgpt-mermaid-\_r_bfe\_ .label-icon{display:inline-block;height:1em;overflow:visible;vertical-align:-0.125em;}#chatgpt-mermaid-\_r_bfe\_ .node .label-icon path{fill:currentColor;stroke:revert;stroke-width:revert;}#chatgpt-mermaid-\_r_bfe\_ .node .neo-node{stroke:rgb(107, 198, 127);}#chatgpt-mermaid-\_r_bfe\_ [data-look="neo"].node rect,#chatgpt-mermaid-\_r_bfe\_ [data-look="neo"].cluster rect,#chatgpt-mermaid-\_r_bfe\_ [data-look="neo"].node polygon{stroke:url(#chatgpt-mermaid-\_r_bfe\_-gradient);filter:drop-shadow( 1px 2px 2px rgba(185,185,185,1));}#chatgpt-mermaid-\_r_bfe\_ [data-look="neo"].swimlane.cluster rect{filter:none;}#chatgpt-mermaid-\_r_bfe\_ [data-look="neo"].node path{stroke:url(#chatgpt-mermaid-\_r_bfe\_-gradient);stroke-width:1px;}#chatgpt-mermaid-\_r_bfe\_ [data-look="neo"].node .outer-path{filter:drop-shadow( 1px 2px 2px rgba(185,185,185,1));}#chatgpt-mermaid-\_r_bfe\_ [data-look="neo"].node .neo-line path{stroke:rgb(107, 198, 127);filter:none;}#chatgpt-mermaid-\_r_bfe\_ [data-look="neo"].node circle{stroke:url(#chatgpt-mermaid-\_r_bfe\_-gradient);filter:drop-shadow( 1px 2px 2px rgba(185,185,185,1));}#chatgpt-mermaid-\_r_bfe\_ [data-look="neo"].node circle .state-start{fill:#000000;}#chatgpt-mermaid-\_r_bfe\_ [data-look="neo"].icon-shape .icon{fill:url(#chatgpt-mermaid-\_r_bfe\_-gradient);filter:drop-shadow( 1px 2px 2px rgba(185,185,185,1));}#chatgpt-mermaid-\_r_bfe\_ [data-look="neo"].icon-shape .icon-neo path{stroke:url(#chatgpt-mermaid-\_r_bfe\_-gradient);filter:drop-shadow( 1px 2px 2px rgba(185,185,185,1));}#chatgpt-mermaid-\_r_bfe\_ .node text{font-size:14px;font-weight:600;letter-spacing:normal;fill:rgb(0, 105, 42);}#chatgpt-mermaid-\_r_bfe\_ .edgeLabels text{font-size:13px;font-weight:600;letter-spacing:-0.08px;fill:rgb(0, 105, 42);}#chatgpt-mermaid-\_r_bfe\_ .node tspan[font-weight="normal"],#chatgpt-mermaid-\_r_bfe\_ .edgeLabels tspan[font-weight="normal"]{font-weight:600;}#chatgpt-mermaid-\_r_bfe\_ .edgeLabel .label rect{opacity:1;rx:13px;ry:13px;fill:rgb(237, 250, 242);stroke:rgb(166, 211, 184);stroke-width:1px;}#chatgpt-mermaid-\_r_bfe\_ .node rect,#chatgpt-mermaid-\_r_bfe\_ .node circle,#chatgpt-mermaid-\_r_bfe\_ .node ellipse,#chatgpt-mermaid-\_r_bfe\_ .node polygon,#chatgpt-mermaid-\_r_bfe\_ .node path{fill:rgb(217, 244, 228);stroke:rgba(0, 0, 0, 0.1);stroke-width:1px;}#chatgpt-mermaid-\_r_bfe\_ .node rect{rx:16px;ry:16px;}#chatgpt-mermaid-\_r_bfe\_ .node.mermaid-decision .label-container{fill:rgb(237, 250, 242);stroke:rgb(195, 220, 205);stroke-dasharray:2,2;}#chatgpt-mermaid-\_r_bfe\_ .edgePaths .flowchart-link{stroke:rgb(143, 143, 143);stroke-width:1px;stroke-linecap:round;stroke-linejoin:round;}#chatgpt-mermaid-\_r_bfe\_ .marker{fill:rgb(143, 143, 143);stroke:rgb(143, 143, 143);}#chatgpt-mermaid-\_r_bfe\_ :root{--mermaid-font-family:-apple-system-body,ui-sans-serif,-apple-system,system-ui,"Segoe UI",Helvetica,"Apple Color Emoji",Arial,sans-serif,"Segoe UI Emoji","Segoe UI Symbol";}System StartupRead SuperblockLoad Latest Valid ManifestLoad Metadata SnapshotLocate Checkpoint LSNScan WAL From CheckpointRedo Committed OperationsRecover Transaction StateResolve Incomplete MigrationValidate Mapping and ExtentsRebuild Derived IndexStart Service

对于未提交事务：

- 不允许其新版本对外可见。
- 不完整的数据分配需要回收。
- 如果已经有部分物理写入，需要根据 WAL 判断其是否属于可恢复状态。

对于已经提交的事务：

- 必须恢复其 Object Version。
- 必须恢复其对应的 Extent 引用。
- 必须恢复其有效 Mapping。
- 索引可以从可靠的基础状态重建或者增量恢复。

## 6.5 索引恢复应该和对象恢复分离

例如 Vector Index。

如果索引文件损坏：

```
Object Store
    |
    +-- Vector Payload
    |
    +-- Object Metadata
```

这些数据仍然可以保留。

然后通过：

```
Vector Object Scan
       |
       v
Embedding Extraction
       |
       v
Vector Index Rebuild
```

重新构建索引。

因此建议：

- Object 数据是核心数据。
- Version 和 Mapping 是核心一致性元数据。
- Index 是可恢复的派生数据结构。
- AI 生成的缓存和中间结果属于可丢弃数据。

这会大幅简化崩溃恢复的复杂度。

# 七、三个问题交汇的关键：GC、版本回收与空间复用

前面已经有了 Object Version、Extent Mapping、Memory Tier 和 WAL。

但还有一个非常重要的机制：

什么时候可以真正删除旧版本、旧 Extent 和旧 Mapping？

这是 MVCC、Checkpoint 和 Migration 的交汇点。

## 7.1 为什么不能立即删除旧版本？

例如：

```
Object 1001

Version 1
Version 2
Version 3
```

当前版本是 Version 3。

但事务 T10 可能仍然在读取 Version 1。

如果直接删除 Version 1，就会破坏 T10 的 Snapshot 一致性。

同样：

迁移过程中，旧 Mapping 可能仍然被某个读取任务使用。

因此必须区分：

- 逻辑上已经过期。
- 当前已经不可见。
- 已经没有任何读者引用。
- 已经可以回收物理空间。

这四者不是一回事。

## 7.2 引入 Epoch-based Reclamation

建议采用：

MVCC Visibility + Epoch-based Reclamation。

可以定义全局 Epoch：

```
Epoch 100
    |
    +-- Reader T1
    |
    +-- Reader T2

Epoch 101
    |
    +-- Reader T3

Epoch 102
    |
    +-- New Object Version
```

当某个旧版本不再被任何活动 Snapshot 引用，并且相关 Mapping 读者已经退出，就可以回收。

例如：

```
Version 1
    |
    +-- No Active Snapshot References
    |
    +-- No Active Reader References
    |
    +-- WAL Recovery No Longer Needs It
    |
    v
Reclaimable
```

需要注意，Epoch 不能只根据时间判断。

必须考虑：

- 最老活动 Snapshot。
- 活跃读者的 Mapping 引用。
- 尚未完成的迁移。
- Checkpoint 恢复边界。
- 备份和复制系统的保留需求（如果实现这些功能）。

## 7.3 GC 的完整流程

\#chatgpt-mermaid-\_r_bge\_{font-family:-apple-system-body,ui-sans-serif,-apple-system,system-ui,"Segoe UI",Helvetica,"Apple Color Emoji",Arial,sans-serif,"Segoe UI Emoji","Segoe UI Symbol";font-size:16px;fill:rgb(13, 13, 13);}@keyframes edge-animation-frame{from{stroke-dashoffset:0;}}@keyframes dash{to{stroke-dashoffset:0;}}#chatgpt-mermaid-\_r_bge\_ .edge-animation-slow{stroke-dasharray:9,5!important;stroke-dashoffset:900;animation:dash 50s linear infinite;stroke-linecap:round;}#chatgpt-mermaid-\_r_bge\_ .edge-animation-fast{stroke-dasharray:9,5!important;stroke-dashoffset:900;animation:dash 20s linear infinite;stroke-linecap:round;}#chatgpt-mermaid-\_r_bge\_ .error-icon{fill:rgb(243, 243, 243);}#chatgpt-mermaid-\_r_bge\_ .error-text{fill:rgb(13, 13, 13);stroke:rgb(13, 13, 13);}#chatgpt-mermaid-\_r_bge\_ .edge-thickness-normal{stroke-width:1px;}#chatgpt-mermaid-\_r_bge\_ .edge-thickness-thick{stroke-width:3.5px;}#chatgpt-mermaid-\_r_bge\_ .edge-pattern-solid{stroke-dasharray:0;}#chatgpt-mermaid-\_r_bge\_ .edge-thickness-invisible{stroke-width:0;fill:none;}#chatgpt-mermaid-\_r_bge\_ .edge-pattern-dashed{stroke-dasharray:3;}#chatgpt-mermaid-\_r_bge\_ .edge-pattern-dotted{stroke-dasharray:2;}#chatgpt-mermaid-\_r_bge\_ .marker{fill:rgb(143, 143, 143);stroke:rgb(143, 143, 143);}#chatgpt-mermaid-\_r_bge\_ .marker.cross{stroke:rgb(143, 143, 143);}#chatgpt-mermaid-\_r_bge\_ svg{font-family:-apple-system-body,ui-sans-serif,-apple-system,system-ui,"Segoe UI",Helvetica,"Apple Color Emoji",Arial,sans-serif,"Segoe UI Emoji","Segoe UI Symbol";font-size:16px;}#chatgpt-mermaid-\_r_bge\_ p{margin:0;}#chatgpt-mermaid-\_r_bge\_ .label{font-family:-apple-system-body,ui-sans-serif,-apple-system,system-ui,"Segoe UI",Helvetica,"Apple Color Emoji",Arial,sans-serif,"Segoe UI Emoji","Segoe UI Symbol";color:rgb(13, 13, 13);}#chatgpt-mermaid-\_r_bge\_ .cluster-label text{fill:rgb(13, 13, 13);}#chatgpt-mermaid-\_r_bge\_ .cluster-label span{color:rgb(13, 13, 13);}#chatgpt-mermaid-\_r_bge\_ .cluster-label span p{background-color:transparent;}#chatgpt-mermaid-\_r_bge\_ .label text,#chatgpt-mermaid-\_r_bge\_ span{fill:rgb(13, 13, 13);color:rgb(13, 13, 13);}#chatgpt-mermaid-\_r_bge\_ .node rect,#chatgpt-mermaid-\_r_bge\_ .node circle,#chatgpt-mermaid-\_r_bge\_ .node ellipse,#chatgpt-mermaid-\_r_bge\_ .node polygon,#chatgpt-mermaid-\_r_bge\_ .node path{fill:rgb(226, 242, 230);stroke:rgb(107, 198, 127);stroke-width:1px;}#chatgpt-mermaid-\_r_bge\_ .rough-node .label text,#chatgpt-mermaid-\_r_bge\_ .node .label text,#chatgpt-mermaid-\_r_bge\_ .image-shape .label,#chatgpt-mermaid-\_r_bge\_ .icon-shape .label{text-anchor:middle;}#chatgpt-mermaid-\_r_bge\_ .node .katex path{fill:#000;stroke:#000;stroke-width:1px;}#chatgpt-mermaid-\_r_bge\_ .rough-node .label,#chatgpt-mermaid-\_r_bge\_ .node .label,#chatgpt-mermaid-\_r_bge\_ .image-shape .label,#chatgpt-mermaid-\_r_bge\_ .icon-shape .label{text-align:center;}#chatgpt-mermaid-\_r_bge\_ .node.clickable{cursor:pointer;}#chatgpt-mermaid-\_r_bge\_ .root .anchor path{fill:rgb(143, 143, 143)!important;stroke-width:0;stroke:rgb(143, 143, 143);}#chatgpt-mermaid-\_r_bge\_ .arrowheadPath{fill:rgb(143, 143, 143);}#chatgpt-mermaid-\_r_bge\_ .edgePath .path{stroke:rgb(143, 143, 143);stroke-width:1px;}#chatgpt-mermaid-\_r_bge\_ .flowchart-link{stroke:rgb(143, 143, 143);fill:none;}#chatgpt-mermaid-\_r_bge\_ .edgeLabel{background-color:rgb(252, 252, 252);text-align:center;}#chatgpt-mermaid-\_r_bge\_ .edgeLabel p{background-color:rgb(252, 252, 252);}#chatgpt-mermaid-\_r_bge\_ .edgeLabel rect{opacity:0.5;background-color:rgb(252, 252, 252);fill:rgb(252, 252, 252);}#chatgpt-mermaid-\_r_bge\_ .labelBkg{background-color:rgba(252, 252, 252, 0.5);}#chatgpt-mermaid-\_r_bge\_ .cluster rect{fill:rgb(243, 243, 243);stroke:rgba(0, 0, 0, 0.1);stroke-width:1px;}#chatgpt-mermaid-\_r_bge\_ .cluster text{fill:rgb(13, 13, 13);}#chatgpt-mermaid-\_r_bge\_ .cluster span{color:rgb(13, 13, 13);}#chatgpt-mermaid-\_r_bge\_ div.mermaidTooltip{position:absolute;text-align:center;max-width:200px;padding:2px;font-family:-apple-system-body,ui-sans-serif,-apple-system,system-ui,"Segoe UI",Helvetica,"Apple Color Emoji",Arial,sans-serif,"Segoe UI Emoji","Segoe UI Symbol";font-size:12px;background:rgb(243, 243, 243);border:1px solid rgba(0, 0, 0, 0.1);border-radius:2px;pointer-events:none;z-index:100;}#chatgpt-mermaid-\_r_bge\_ .flowchartTitleText{text-anchor:middle;font-size:18px;fill:rgb(13, 13, 13);}#chatgpt-mermaid-\_r_bge\_ rect.text{fill:none;stroke-width:0;}#chatgpt-mermaid-\_r_bge\_ .icon-shape,#chatgpt-mermaid-\_r_bge\_ .image-shape{background-color:rgb(252, 252, 252);text-align:center;}#chatgpt-mermaid-\_r_bge\_ .icon-shape p,#chatgpt-mermaid-\_r_bge\_ .image-shape p{background-color:rgb(252, 252, 252);padding:2px;}#chatgpt-mermaid-\_r_bge\_ .icon-shape .label rect,#chatgpt-mermaid-\_r_bge\_ .image-shape .label rect{opacity:0.5;background-color:rgb(252, 252, 252);fill:rgb(252, 252, 252);}#chatgpt-mermaid-\_r_bge\_ .label-icon{display:inline-block;height:1em;overflow:visible;vertical-align:-0.125em;}#chatgpt-mermaid-\_r_bge\_ .node .label-icon path{fill:currentColor;stroke:revert;stroke-width:revert;}#chatgpt-mermaid-\_r_bge\_ .node .neo-node{stroke:rgb(107, 198, 127);}#chatgpt-mermaid-\_r_bge\_ [data-look="neo"].node rect,#chatgpt-mermaid-\_r_bge\_ [data-look="neo"].cluster rect,#chatgpt-mermaid-\_r_bge\_ [data-look="neo"].node polygon{stroke:url(#chatgpt-mermaid-\_r_bge\_-gradient);filter:drop-shadow( 1px 2px 2px rgba(185,185,185,1));}#chatgpt-mermaid-\_r_bge\_ [data-look="neo"].swimlane.cluster rect{filter:none;}#chatgpt-mermaid-\_r_bge\_ [data-look="neo"].node path{stroke:url(#chatgpt-mermaid-\_r_bge\_-gradient);stroke-width:1px;}#chatgpt-mermaid-\_r_bge\_ [data-look="neo"].node .outer-path{filter:drop-shadow( 1px 2px 2px rgba(185,185,185,1));}#chatgpt-mermaid-\_r_bge\_ [data-look="neo"].node .neo-line path{stroke:rgb(107, 198, 127);filter:none;}#chatgpt-mermaid-\_r_bge\_ [data-look="neo"].node circle{stroke:url(#chatgpt-mermaid-\_r_bge\_-gradient);filter:drop-shadow( 1px 2px 2px rgba(185,185,185,1));}#chatgpt-mermaid-\_r_bge\_ [data-look="neo"].node circle .state-start{fill:#000000;}#chatgpt-mermaid-\_r_bge\_ [data-look="neo"].icon-shape .icon{fill:url(#chatgpt-mermaid-\_r_bge\_-gradient);filter:drop-shadow( 1px 2px 2px rgba(185,185,185,1));}#chatgpt-mermaid-\_r_bge\_ [data-look="neo"].icon-shape .icon-neo path{stroke:url(#chatgpt-mermaid-\_r_bge\_-gradient);filter:drop-shadow( 1px 2px 2px rgba(185,185,185,1));}#chatgpt-mermaid-\_r_bge\_ .node text{font-size:14px;font-weight:600;letter-spacing:normal;fill:rgb(0, 105, 42);}#chatgpt-mermaid-\_r_bge\_ .edgeLabels text{font-size:13px;font-weight:600;letter-spacing:-0.08px;fill:rgb(0, 105, 42);}#chatgpt-mermaid-\_r_bge\_ .node tspan[font-weight="normal"],#chatgpt-mermaid-\_r_bge\_ .edgeLabels tspan[font-weight="normal"]{font-weight:600;}#chatgpt-mermaid-\_r_bge\_ .edgeLabel .label rect{opacity:1;rx:13px;ry:13px;fill:rgb(237, 250, 242);stroke:rgb(166, 211, 184);stroke-width:1px;}#chatgpt-mermaid-\_r_bge\_ .node rect,#chatgpt-mermaid-\_r_bge\_ .node circle,#chatgpt-mermaid-\_r_bge\_ .node ellipse,#chatgpt-mermaid-\_r_bge\_ .node polygon,#chatgpt-mermaid-\_r_bge\_ .node path{fill:rgb(217, 244, 228);stroke:rgba(0, 0, 0, 0.1);stroke-width:1px;}#chatgpt-mermaid-\_r_bge\_ .node rect{rx:16px;ry:16px;}#chatgpt-mermaid-\_r_bge\_ .node.mermaid-decision .label-container{fill:rgb(237, 250, 242);stroke:rgb(195, 220, 205);stroke-dasharray:2,2;}#chatgpt-mermaid-\_r_bge\_ .edgePaths .flowchart-link{stroke:rgb(143, 143, 143);stroke-width:1px;stroke-linecap:round;stroke-linejoin:round;}#chatgpt-mermaid-\_r_bge\_ .marker{fill:rgb(143, 143, 143);stroke:rgb(143, 143, 143);}#chatgpt-mermaid-\_r_bge\_ :root{--mermaid-font-family:-apple-system-body,ui-sans-serif,-apple-system,system-ui,"Segoe UI",Helvetica,"Apple Color Emoji",Arial,sans-serif,"Segoe UI Emoji","Segoe UI Symbol";}GC ScannerFind Obsolete ObjectVersionsCheck MVCC VisibilityCheck Active ReadersCheck Mapping ReferencesCheck WAL RetentionMark ReclaimableRelease ExtentUpdate Free Space MapStill VisibleObsoleteReferencedUnreferencedReferencedUnreferencedRequired by RecoverySafe

这里要特别注意：

GC 不能简单根据 `xmax` 是否已提交就释放 Extent。

它必须同时考虑 Snapshot、引用关系、迁移状态和恢复依赖。

# 八、完整一致性协议：一次 Object 更新到底发生了什么？

前面分别讨论了 Object、Extent、硬件资源、MVCC、WAL 和 Checkpoint。

现在把它们串起来。

假设：

- Object ID：1001。
- 当前版本：Version 1。
- 当前数据位于 NVMe 0。
- 用户要更新其中一个 Tensor Tile。
- 新数据需要写入 NVMe。
- 同时允许其他事务读取旧版本。

## 8.1 完整更新流程

Mapping ManagerNVMeI/O ManagerWAL ManagerExtent ManagerObject ManagerTransaction ManagerAI ApplicationMapping ManagerNVMeI/O ManagerWAL ManagerExtent ManagerObject ManagerTransaction ManagerAI Application#chatgpt-mermaid-\_r_bgl\_{font-family:-apple-system-body,ui-sans-serif,-apple-system,system-ui,"Segoe UI",Helvetica,"Apple Color Emoji",Arial,sans-serif,"Segoe UI Emoji","Segoe UI Symbol";font-size:16px;fill:rgb(13, 13, 13);}@keyframes edge-animation-frame{from{stroke-dashoffset:0;}}@keyframes dash{to{stroke-dashoffset:0;}}#chatgpt-mermaid-\_r_bgl\_ .edge-animation-slow{stroke-dasharray:9,5!important;stroke-dashoffset:900;animation:dash 50s linear infinite;stroke-linecap:round;}#chatgpt-mermaid-\_r_bgl\_ .edge-animation-fast{stroke-dasharray:9,5!important;stroke-dashoffset:900;animation:dash 20s linear infinite;stroke-linecap:round;}#chatgpt-mermaid-\_r_bgl\_ .error-icon{fill:rgb(243, 243, 243);}#chatgpt-mermaid-\_r_bgl\_ .error-text{fill:rgb(13, 13, 13);stroke:rgb(13, 13, 13);}#chatgpt-mermaid-\_r_bgl\_ .edge-thickness-normal{stroke-width:1px;}#chatgpt-mermaid-\_r_bgl\_ .edge-thickness-thick{stroke-width:3.5px;}#chatgpt-mermaid-\_r_bgl\_ .edge-pattern-solid{stroke-dasharray:0;}#chatgpt-mermaid-\_r_bgl\_ .edge-thickness-invisible{stroke-width:0;fill:none;}#chatgpt-mermaid-\_r_bgl\_ .edge-pattern-dashed{stroke-dasharray:3;}#chatgpt-mermaid-\_r_bgl\_ .edge-pattern-dotted{stroke-dasharray:2;}#chatgpt-mermaid-\_r_bgl\_ .marker{fill:rgb(143, 143, 143);stroke:rgb(143, 143, 143);}#chatgpt-mermaid-\_r_bgl\_ .marker.cross{stroke:rgb(143, 143, 143);}#chatgpt-mermaid-\_r_bgl\_ svg{font-family:-apple-system-body,ui-sans-serif,-apple-system,system-ui,"Segoe UI",Helvetica,"Apple Color Emoji",Arial,sans-serif,"Segoe UI Emoji","Segoe UI Symbol";font-size:16px;}#chatgpt-mermaid-\_r_bgl\_ p{margin:0;}#chatgpt-mermaid-\_r_bgl\_ .actor{stroke:rgb(107, 198, 127);fill:rgb(226, 242, 230);stroke-width:1;}#chatgpt-mermaid-\_r_bgl\_ rect.actor.outer-path[data-look="neo"]{filter:drop-shadow( 1px 2px 2px rgba(185,185,185,1));}#chatgpt-mermaid-\_r_bgl\_ rect.note[data-look="neo"]{stroke:rgb(239, 139, 87);fill:rgb(243, 243, 243);filter:drop-shadow( 1px 2px 2px rgba(185,185,185,1));}#chatgpt-mermaid-\_r_bgl\_ text.actor>tspan{fill:rgb(13, 13, 13);stroke:none;}#chatgpt-mermaid-\_r_bgl\_ .actor-line{stroke:rgb(143, 143, 143);}#chatgpt-mermaid-\_r_bgl\_ .innerArc{stroke-width:1.5;stroke-dasharray:none;}#chatgpt-mermaid-\_r_bgl\_ .messageLine0{stroke-width:1.5;stroke-dasharray:none;stroke:rgb(143, 143, 143);}#chatgpt-mermaid-\_r_bgl\_ .messageLine1{stroke-width:1.5;stroke-dasharray:2,2;stroke:rgb(143, 143, 143);}#chatgpt-mermaid-\_r_bgl\_ [id$="-arrowhead"] path{fill:rgb(143, 143, 143);stroke:rgb(143, 143, 143);}#chatgpt-mermaid-\_r_bgl\_ .sequenceNumber{fill:#707070;}#chatgpt-mermaid-\_r_bgl\_ [id$="-sequencenumber"]{fill:rgb(143, 143, 143);}#chatgpt-mermaid-\_r_bgl\_ [id$="-crosshead"] path{fill:rgb(143, 143, 143);stroke:rgb(143, 143, 143);}#chatgpt-mermaid-\_r_bgl\_ .messageText{fill:rgb(13, 13, 13);stroke:none;}#chatgpt-mermaid-\_r_bgl\_ .labelBox{stroke:rgba(0, 0, 0, 0.1);fill:rgb(252, 252, 252);filter:none;}#chatgpt-mermaid-\_r_bgl\_ .labelText,#chatgpt-mermaid-\_r_bgl\_ .labelText>tspan{fill:rgb(13, 13, 13);stroke:none;}#chatgpt-mermaid-\_r_bgl\_ .loopText,#chatgpt-mermaid-\_r_bgl\_ .loopText>tspan{fill:rgb(13, 13, 13);stroke:none;}#chatgpt-mermaid-\_r_bgl\_ .sectionTitle,#chatgpt-mermaid-\_r_bgl\_ .sectionTitle>tspan{fill:rgb(13, 13, 13);stroke:none;}#chatgpt-mermaid-\_r_bgl\_ .loopLine{stroke-width:2px;stroke-dasharray:2,2;stroke:rgba(0, 0, 0, 0.1);fill:rgba(0, 0, 0, 0.1);}#chatgpt-mermaid-\_r_bgl\_ .note{stroke:rgb(239, 139, 87);fill:rgb(243, 243, 243);}#chatgpt-mermaid-\_r_bgl\_ .noteText,#chatgpt-mermaid-\_r_bgl\_ .noteText>tspan{fill:rgb(13, 13, 13);stroke:none;font-weight:normal;}#chatgpt-mermaid-\_r_bgl\_ .activation0{fill:rgb(243, 243, 243);stroke:hsl(0, 0%, 85.2941176471%);}#chatgpt-mermaid-\_r_bgl\_ .activation1{fill:rgb(243, 243, 243);stroke:hsl(0, 0%, 85.2941176471%);}#chatgpt-mermaid-\_r_bgl\_ .activation2{fill:rgb(243, 243, 243);stroke:hsl(0, 0%, 85.2941176471%);}#chatgpt-mermaid-\_r_bgl\_ .actorPopupMenu{position:absolute;}#chatgpt-mermaid-\_r_bgl\_ .actorPopupMenuPanel{position:absolute;fill:rgb(226, 242, 230);box-shadow:0px 8px 16px 0px rgba(0,0,0,0.2);filter:drop-shadow(3px 5px 2px rgb(0 0 0 / 0.4));}#chatgpt-mermaid-\_r_bgl\_ .actor-man circle,#chatgpt-mermaid-\_r_bgl\_ line{fill:rgb(226, 242, 230);stroke-width:2px;}#chatgpt-mermaid-\_r_bgl\_ g rect.rect{filter:drop-shadow( 1px 2px 2px rgba(185,185,185,1));stroke:rgb(107, 198, 127);}#chatgpt-mermaid-\_r_bgl\_ .node .neo-node{stroke:rgb(107, 198, 127);}#chatgpt-mermaid-\_r_bgl\_ [data-look="neo"].node rect,#chatgpt-mermaid-\_r_bgl\_ [data-look="neo"].cluster rect,#chatgpt-mermaid-\_r_bgl\_ [data-look="neo"].node polygon{stroke:url(#chatgpt-mermaid-\_r_bgl\_-gradient);filter:drop-shadow( 1px 2px 2px rgba(185,185,185,1));}#chatgpt-mermaid-\_r_bgl\_ [data-look="neo"].swimlane.cluster rect{filter:none;}#chatgpt-mermaid-\_r_bgl\_ [data-look="neo"].node path{stroke:url(#chatgpt-mermaid-\_r_bgl\_-gradient);stroke-width:1px;}#chatgpt-mermaid-\_r_bgl\_ [data-look="neo"].node .outer-path{filter:drop-shadow( 1px 2px 2px rgba(185,185,185,1));}#chatgpt-mermaid-\_r_bgl\_ [data-look="neo"].node .neo-line path{stroke:rgb(107, 198, 127);filter:none;}#chatgpt-mermaid-\_r_bgl\_ [data-look="neo"].node circle{stroke:url(#chatgpt-mermaid-\_r_bgl\_-gradient);filter:drop-shadow( 1px 2px 2px rgba(185,185,185,1));}#chatgpt-mermaid-\_r_bgl\_ [data-look="neo"].node circle .state-start{fill:#000000;}#chatgpt-mermaid-\_r_bgl\_ [data-look="neo"].icon-shape .icon{fill:url(#chatgpt-mermaid-\_r_bgl\_-gradient);filter:drop-shadow( 1px 2px 2px rgba(185,185,185,1));}#chatgpt-mermaid-\_r_bgl\_ [data-look="neo"].icon-shape .icon-neo path{stroke:url(#chatgpt-mermaid-\_r_bgl\_-gradient);filter:drop-shadow( 1px 2px 2px rgba(185,185,185,1));}#chatgpt-mermaid-\_r_bgl\_ :root{--mermaid-font-family:-apple-system-body,ui-sans-serif,-apple-system,system-ui,"Segoe UI",Helvetica,"Apple Color Emoji",Arial,sans-serif,"Segoe UI Emoji","Segoe UI Symbol";}BeginSnapshotUpdate Object 1001Resolve visible Version 1Allocate new ExtentWrite new payloadWrite completedFlush new payloadDurableAppend Object Version RecordLSN 200Append Extent Mapping RecordLSN 220CommitAppend Commit RecordCommit LSN 250Flush through LSN 250DurablePublish new Object VersionPublishedCommit Success

这里的顺序是设计上的安全基线。

实际实现中，Object Version、Mapping 和事务提交记录可以在同一个 WAL Commit Group 中组织，但必须保证所有依赖记录都在提交记录之前，并且数据持久化屏障满足要求。

## 8.2 更新过程中发生崩溃怎么办？

| 崩溃位置                   | 恢复结果                          |
| ---------------------- | ----------------------------- |
| 新 Extent 分配前           | 原版本不变                         |
| 新数据写入期间                | 原版本不变，孤立分配回收                  |
| 新数据写完但未 Flush          | 原版本不变，未确认的新数据不允许发布            |
| 数据 Flush 完成但未写 Commit  | 原版本不变，新数据可回收                  |
| Commit WAL 已写但未持久化     | 根据 WAL 是否完整持久化判断，未确认提交不对外承诺成功 |
| Commit WAL 已持久化但尚未发布版本 | Recovery 重做提交，恢复新版本           |
| 新版本发布后                 | 恢复新版本                         |

最关键的原则：

提交成功的事务必须在崩溃恢复后仍然可见。

因此 Commit Success 不能仅仅意味着内存里的状态已经切换。

必须意味着提交记录以及它依赖的数据已经达到规定的持久化条件。

# 九、三个核心模块的接口边界

为了避免实现时出现模块职责混乱，我建议最终确定下面的接口关系。

## 9.1 Object / Extent / Mapping

| 模块                      | 负责什么       | 不负责什么   |
| ----------------------- | ---------- | ------- |
| Object Manager          | 对象创建、读取、更新 | 硬件分配    |
| Version Manager         | 版本链、版本引用   | 数据 I/O  |
| Extent Manager          | 逻辑数据切分     | 设备选择    |
| Mapping Manager         | 逻辑到物理映射    | 数据迁移决策  |
| Physical Extent Manager | 物理空间分配     | AI 语义理解 |
| Block Manager           | 硬件块定位和分配   | 事务可见性   |

## 9.2 Hardware Resource Manager

| 模块                  | 负责什么                |
| ------------------- | ------------------- |
| Hardware Discovery  | 发现硬件                |
| Topology Manager    | 管理 NUMA、GPU、NVMe 拓扑 |
| Capacity Manager    | 管理容量                |
| Performance Monitor | 采集性能                |
| Placement Manager   | 决定数据位置              |
| Migration Manager   | 执行数据迁移              |
| Memory Tier Manager | 管理内存资源              |
| I/O Manager         | 执行实际读写              |

## 9.3 Consistency Manager

| 模块                  | 负责什么     |
| ------------------- | -------- |
| Transaction Manager | 事务生命周期   |
| Snapshot Manager    | Snapshot |
| Version Manager     | 版本可见性    |
| WAL Manager         | 修改记录和持久化 |
| Checkpoint Manager  | 元数据快照    |
| Recovery Manager    | 崩溃恢复     |
| GC Manager          | 旧版本和空间回收 |

这三个模块组需要通过稳定接口交互。

例如：

```
Object Manager
    |
    +-- Version Manager
    |
    +-- Extent Manager
    |
    +-- Transaction Manager
    |
    +-- WAL Manager
```

但不能让 Object Manager 直接操作 NVMe LBA。

它只能通过 Extent Manager 和 I/O Manager 访问物理数据。

# 十、第一版内核必须确定的设计决策

经过前面分析，我建议将第一版的核心设计冻结为以下方案。

| 设计领域       | 推荐方案                         | 选择理由       |
| ---------- | ---------------------------- | ---------- |
| 逻辑数据模型     | Object + Component           | 支持多模态对象    |
| 逻辑存储组织     | Logical Extent               | 解耦对象与物理布局  |
| 物理分配       | Physical Extent              | 支持硬件资源管理   |
| 硬件定位       | Physical Location Descriptor | 避免绑定裸地址    |
| 物理映射       | 独立 Mapping Table             | 支持迁移和重映射   |
| 数据更新       | Copy-on-Write                | 减少并发覆盖问题   |
| 对象版本       | Object-level MVCC            | 保证事务可见性    |
| Extent 版本  | Immutable Extent Version     | 支持共享和版本回收  |
| 映射版本       | Mapping Version              | 支持并发迁移     |
| 内存管理       | Memory Tier Manager          | 统一管理异构内存   |
| 数据放置       | Cost-based Placement         | 根据工作负载选择位置 |
| 数据迁移       | Versioned Mapping Switch     | 保证迁移安全     |
| 持久化        | Object-aware WAL             | 支持对象级恢复    |
| Checkpoint | Manifest + WAL Barrier       | 建立一致恢复边界   |
| 垃圾回收       | MVCC + Epoch Reclamation     | 安全回收旧版本    |

## 10.1 第一版最重要的五条一致性约束

这些约束应该成为内核实现时的硬性不变量。

约束一：逻辑身份不依赖物理地址。

```
Object ID != Physical Address
```

约束二：已经提交的 Object Version 必须能够恢复。

```
Committed Version
    =>
Recoverable Version
```

约束三：Mapping 切换不能导致读取过程中物理位置变化。

```
Reader
    =>
Stable Mapping Reference
```

约束四：旧 Extent 只有在所有引用消失后才能回收。

```
Reclaim Extent
    =>
No Snapshot Reference
    &&
No Reader Reference
    &&
No Recovery Dependency
```

约束五：非持久化缓存不能成为唯一有效数据副本。

```
Authoritative Data
    =>
Durable Storage
```

这五条约束比第一版支持多少种 Object、多少种索引更重要。

# 十一、建议的内核实现顺序

虽然三个问题相互关联，但实际编码不能同时开始。

我建议按下面的依赖顺序开发。

## Storage Kernel Development Roadmap

1. 第一步：Object + Extent

   建立最基本的数据组织体系。
   - Object Header
   - Object Component
   - Logical Extent
   - Physical Extent
   - Mapping Table
2. 第二步：NVMe Storage Backend

   让对象真正落盘。
   - Device Discovery
   - Block Allocator
   - I/O Manager
   - io_uring
   - Checksum
3. 第三步：WAL + Recovery

   先解决崩溃安全，再引入复杂并发。
   - WAL Record
   - WAL Flush
   - Checkpoint Manifest
   - Recovery
4. 第四步：Object Version + MVCC

   建立并发读写和快照机制。
   - Transaction Manager
   - Snapshot Manager
   - Version Chain
   - Copy-on-Write
   - Visibility
5. 第五步：Memory Tier Manager

   支持 DRAM 和 NVMe 的协同管理。
   - Object Cache
   - NUMA-aware Allocation
   - Cache Eviction
   - Data Placement
   - Migration
6. 第六步：GPU HBM Backend

   实现面向 AI 计算的数据驻留和访问。
   - GPU Allocation
   - HBM Working Set
   - GPU Data Transfer
   - GPU-aware Scheduler

## 11.2 每个阶段的验收条件

| 阶段              | 必须通过的测试             |
| --------------- | ------------------- |
| Object + Extent | 对象切分、重组、随机读取正确      |
| NVMe Backend    | 并发读写、I/O 错误处理       |
| WAL + Recovery  | 任意 WAL 写入边界崩溃恢复     |
| MVCC            | 并发事务读取正确版本          |
| Memory Tier     | 缓存淘汰、迁移、并发读取        |
| GPU HBM         | GPU 数据正确性、设备故障和资源释放 |

特别建议为第三、第四阶段编写 Crash Injection 测试。

模拟在以下位置强制终止进程：

- WAL Record 写入过程中。
- WAL Flush 过程中。
- Extent 写入过程中。
- Checkpoint Manifest 发布过程中。
- Mapping Switch 过程中。
- Transaction Commit 过程中。

重启后验证 Object、Version、Extent 和 Mapping 是否满足一致性约束。

# 十二、我认为还需要特别注意的一个问题：不要过早把 Page 完全删除

前面的设计将 Object 和 Extent 作为核心抽象，但这不代表物理层完全不需要固定大小的数据块。

我的建议是：

逻辑层彻底摆脱 Page 依赖，物理层保留 Block/Page 作为一种实现方式。

例如：

```
Object Layer
     |
Logical Extent Layer
     |
Physical Extent Layer
     |
Block Allocator
     |
NVMe Block / Memory Page
```

原因是：

- 内存管理本身仍然需要页面和对齐。
- NVMe 仍然需要满足设备和 I/O 对齐要求。
- GPU 内存管理也存在分配粒度和对齐限制。
- 大型数据对象需要拆分成设备能够处理的 I/O 请求。

真正需要改变的是 Page 在架构中的地位。

传统数据库：

```
Page 是核心数据组织单位。
```

新系统：

```
Object 是核心逻辑数据单位。
Extent 是核心存储管理单位。
Block 是底层物理访问单位。
```

这样既能保留硬件访问的效率，也能避免上层数据模型被 Page 限制。

# 十三、最终结论

这三个问题实际上定义了 AI-Native Storage 的内核基础。

# AI-Native Storage Kernel

Object Manager

逻辑数据、组件、版本

Extent & Mapping Manager

逻辑切分、物理映射、空间分配

Hardware Resource Manager

DRAM、GPU HBM、NVMe、NUMA、数据迁移

Hardware Backend

CPU Memory / GPU / NVMe

贯穿整个内核的 Consistency Plane

MVCC

版本和可见性

WAL

持久化和恢复

Checkpoint

一致性快照

GC

版本和空间回收

我的最终建议是：

先把 AI 从存储内核中拿开，先实现一个面向 AI 数据形态和异构硬件的独立存储内核。

因为 AI 可以更换，模型可以更换，Intent Planner 可以更换，但 Object、Extent、Mapping、MVCC、WAL 和硬件资源管理构成的内核必须长期稳定。

这也是这个项目真正有技术壁垒的地方。

下一步最值得展开的是：将上述设计进一步落实成 PostgreSQL 内核级别的详细设计深度——包括模块内部数据结构、函数调用链、状态机、锁设计、WAL Record 格式、崩溃恢复算法，以及并发时序图。 这样就可以从架构方案进入真正的存储引擎内核设计与编码阶段。