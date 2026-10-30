# sparql-v12n

[English](README.md) | [한국어](README.ko.md) | 简体中文

> 本文是 [README.md](README.md)（提交 8c0e55a）的翻译。如有出入，以英文原文为准。

<p align="center"><b>用 Rust 编写的虚拟知识图谱引擎。它通过 R2RML 映射将 SPARQL 查询翻译为 SQL，并在源数据库上执行，无需复制数据。</b></p>

<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="docs/images/how-it-works-dark.svg">
    <img alt="调用方将 SPARQL 查询提交给 v12n。R2RML 映射用于查询规划和 RDF 项构造，数据库的键和列类型用于生成 SQL。PostgreSQL 执行一条获取结果的 SQL 查询并返回行。v12n 解码这些行、构造 RDF 项，并以 JSON、XML、CSV 或 TSV 流式输出 SELECT 绑定。源数据保留在 PostgreSQL 中；加载元数据和准备语句可能需要额外的数据库往返。" src="docs/images/how-it-works-light.svg" width="760">
  </picture>
</p>

- 每条 SPARQL 查询都会成为一条 SQL 查询，在存放数据的数据库上执行。数据不会被复制。
- 目前支持 PostgreSQL。其他数据库已列入[路线图](#路线图)。
- 哪些表和列对应哪些类和属性，由 R2RML 映射决定。映射之外的数据无法查询。
- 目前尚不支持本体推理。查询某个类时，只会得到映射中声明为该类的数据。参见[路线图](#路线图)。
- 单个可执行文件 `v12n` 同时提供命令行工具和 SPARQL 1.1 Protocol HTTP 端点。

> 状态：实验性。下文所列的支持范围已与 Ontop 及 W3C R2RML 测试用例对照测试。1.0 之前，命令、输出与 API 可能发生变化。

## 目的

v12n 将保留在关系数据库中的数据作为 RDF 图进行查询，无需将数据加载到单独的 RDF 存储中。该图由 R2RML 映射定义。每条查询都以 SQL 在源表上执行，只有其结果中的行才会被转换为 RDF 项。本项目的性能目标之一是高效地转换并返回大规模 `SELECT` 结果，涵盖从数据库行到序列化响应的全过程。

当数据位于 PostgreSQL 中、查询属于[支持的范围](#支持的-sparql)、并且无需本体推理、仅凭映射中声明的类和属性即可满足需求时，值得评估本引擎。本引擎目前处于实验阶段，仅支持 PostgreSQL。与 Ontop 对比的基准测试见[性能](#性能)。

## 安装

从[发布](https://github.com/CharmingGroot/sparql-v12n/releases)页面下载与平台对应的压缩包，并将 `v12n` 放入 `PATH`。压缩包提供 Linux（x86_64 与 aarch64，静态二进制）、macOS（Apple silicon）和 Windows（x86_64）版本。

每个发布都附有 `SHA256SUMS` 文件和构建来源证明（attestation）。可用以下任一命令校验下载的文件（在 macOS 上，第一条命令请改用 `shasum -a 256 -c`）：

```sh
sha256sum --ignore-missing -c SHA256SUMS
gh attestation verify v12n-<version>-<target>.tar.gz -R CharmingGroot/sparql-v12n
```

使用 Rust 1.88 或更高版本从源码构建：

```sh
cargo install --locked --git https://github.com/CharmingGroot/sparql-v12n v12n-cli
```

`v12n --version` 输出版本以及构建所用的提交。

## 快速开始

本示例在 Docker 中的本地 PostgreSQL 上运行 `examples/shop` 中的商店示例。需要支持 Compose v2 的 Docker 和 `v12n` 可执行文件（见[安装](#安装)一节），并在本仓库克隆目录的根目录中执行。若从源码构建，请用 `./target/release/v12n` 代替 `v12n`。

```sh
git clone https://github.com/CharmingGroot/sparql-v12n.git
cd sparql-v12n
```

1. 启动包含示例数据库模式和数据的 PostgreSQL。只会启动 `testenv` 的 `postgres` 服务，它使用本地开发凭据在 `127.0.0.1:55432` 上监听。

   ```sh
   docker compose -f testenv/docker-compose.yml up -d --wait postgres
   ```

2. 检查映射。

   ```sh
   v12n check examples/shop/mapping.ttl --format table
   ```

   ```text
   ok: 2 triples maps, 2 classes, 4 properties
   ```

3. 执行查询 VIP 客户所下订单的查询。

   ```sh
   v12n query examples/shop/mapping.ttl examples/shop/queries/vip-orders.rq \
     --db postgresql://v12n:v12n-dev@127.0.0.1:55432/shop --format table
   ```

   ```text
   ?order                           ?amount
   <http://example.org/order/1001>  "120000"^^<http://www.w3.org/2001/XMLSchema#integer>
   <http://example.org/order/1003>  "87000"^^<http://www.w3.org/2001/XMLSchema#integer>
   <http://example.org/order/1004>  "15000"^^<http://www.w3.org/2001/XMLSchema#integer>
   ```

   返回这三个订单的原因见[结果的构成](#结果的构成)。

4. 停止数据库，并删除其容器和网络。

   ```sh
   docker compose -f testenv/docker-compose.yml down -v
   ```

测试环境见 [testenv/README.md](testenv/README.md)，其中也介绍了差分测试使用的 Ontop 端点；本示例不需要它。

### 结果的构成

示例数据库有两张表。下面列出两张表的全部行。完整文件见 [schema.sql](examples/shop/schema.sql) 和 [data.sql](examples/shop/data.sql)。

`customers`

| id | name | tier |
|---|---|---|
| 7 | Kim | VIP |
| 8 | Lee | REGULAR |
| 9 | Park | VIP |

`orders`

| id | customer_id | amount |
|---|---|---|
| 1001 | 7 | 120000 |
| 1002 | 8 | 30000 |
| 1003 | 9 | 87000 |
| 1004 | 7 | 15000 |

R2RML 映射将每一行转换为一个 IRI 及其属性。下面的摘录省略了前缀声明、`a rr:TriplesMap` 和 `ex:name`。完整文件见 [mapping.ttl](examples/shop/mapping.ttl)。

```turtle
<#CustomerMap>
  rr:logicalTable [ rr:tableName "customers" ] ;
  rr:subjectMap [ rr:template "http://example.org/customer/{id}" ; rr:class ex:Customer ] ;
  rr:predicateObjectMap [ rr:predicate ex:tier ; rr:objectMap [ rr:column "tier" ] ] .

<#OrderMap>
  rr:logicalTable [ rr:tableName "orders" ] ;
  rr:subjectMap [ rr:template "http://example.org/order/{id}" ; rr:class ex:Order ] ;
  rr:predicateObjectMap [ rr:predicate ex:amount ; rr:objectMap [ rr:column "amount" ] ] ;
  rr:predicateObjectMap [
    rr:predicate ex:placedBy ;
    rr:objectMap [ rr:template "http://example.org/customer/{customer_id}" ]
  ] .
```

- `id` 为 7 的客户行成为 `<http://example.org/customer/7>`，它是 `ex:Customer`，并具有 `ex:tier "VIP"`。
- `id` 为 1001 的订单行成为 `<http://example.org/order/1001>`，它是 `ex:Order`，并具有 `ex:amount 120000`。由于该列是 `integer`，值为 `xsd:integer` 字面量。
- `ex:placedBy` 使用与客户主语相同的模板，由 `customer_id` 构造客户 IRI。订单 1001 的 `customer_id` 为 7，因此它指向 `<http://example.org/customer/7>`，即客户行 7 的 IRI。

查询 [vip-orders.rq](examples/shop/queries/vip-orders.rq)：

```sparql
PREFIX ex: <http://example.org/>

# Orders placed by VIP customers.
SELECT ?order ?amount WHERE {
  ?customer a ex:Customer ; ex:tier "VIP" .
  ?order ex:placedBy ?customer ; ex:amount ?amount .
}
```

`?customer` 同时出现在两个模式中，因此只有当订单的 `ex:placedBy` IRI 等于等级为 `"VIP"` 的客户的 IRI 时，该订单才会出现在结果中。在 SQL 中，这是 `orders.customer_id = customers.id` 的连接。VIP 客户是 7 和 9：客户 7 的订单是 1001 和 1004，客户 9 的订单是 1003。订单 1002 属于等级为 `"REGULAR"` 的客户 8。

`v12n explain` 在不连接数据库的情况下输出生成的 SQL。连接显示为 `JOIN "orders" t1 ON t1."customer_id" = t0."id"`，`"VIP"` 作为参数 `$1` 传递。

```sh
v12n explain examples/shop/mapping.ttl examples/shop/queries/vip-orders.rq
```

## 支持的 SPARQL

引擎回答针对一个 R2RML 映射和一个 PostgreSQL 数据库的 `SELECT` 查询。下表列出支持与拒绝的内容。被拒绝的查询以及表中未列出的形式会以错误结束（`v12n query` 退出码为 1，HTTP 端点返回 501），不会返回不同的答案。每项受支持的功能都在差分测试（`tests/diff`）中与 [Ontop](https://ontop-vkg.org/) 进行比较。

| 功能 | 说明 | 拒绝 |
|---|---|---|
| 基本图模式 | 常量谓词与 `rdf:type`；常量与共享变量按 RDF term 匹配，可跨越不同的列类型；一个三元组无论由多少行产生都只计一次 | 变量谓词、属性路径、`GRAPH`、`FROM` 与 `FROM NAMED` |
| `SELECT`、`SELECT *`、`DISTINCT` | 对解采用 bag 语义 | `REDUCED`、子查询、`GROUP BY` 与聚合 |
| `BIND`、`SELECT (expr AS ?v)` | 变量、常量，或整数与 decimal 的 `+`、`-`、`*`、`/`；发生错误时变量保持未绑定 | 其他表达式；`OPTIONAL` 或 `UNION` 内部的 `BIND`；在三元组模式、`FILTER`、`ORDER BY` 或其他表达式中使用计算得到的变量 |
| `OPTIONAL` | 嵌套与顺序排列、内部的 `FILTER`、未绑定变量 | 对仅由多个 `OPTIONAL` 部分绑定的变量使用 `SELECT DISTINCT`；`OPTIONAL` 内部包围一侧为空的 `UNION` 的 `FILTER` |
| `UNION` | 两侧的 bag 并集，在 `OPTIONAL` 内部同样适用；同一变量可对应不同形态的 term map | 在同一变量的自然数据类型列与另一种字面量形态之间去重 |
| `FILTER` | `=`、`!=`、`<`、`<=`、`>`、`>=`、`&&`、`\|\|`、`!`、`BOUND`、`sameTerm`、`isIRI`、`isBlank`、`isLiteral`、`STR`、`LANG`、`DATATYPE`、`REGEX`、整数与 decimal 的 `+`、`-`、`*`、`/`，遵循 SPARQL 的错误语义 | 其他函数、`xsd:double` 的算术、`IN`、`EXISTS`；SPARQL 1.1 留给扩展处理的比较（`xsd:date` 的排序、带语言标签字面量上的 `!=`、带时区与不带时区的 dateTime 之间的比较）；`real` 列上的比较；含 `\d`、`\w`、`\p{..}` 与反向引用的 `REGEX` |
| `ORDER BY`、`LIMIT`、`OFFSET` | SPARQL 1.1 的顺序，在其未规定之处采用固定顺序 | 对 `SELECT DISTINCT` 未投影的变量使用 `ORDER BY` |
| 字面量 | RDF 1.1 的 term 相等：语言标签为小写，`xsd:string` 即简单字面量，XSD 1.1 规范词法形式 | 在 `FILTER` 中按值比较 `xsd:float` 与派生整数类型 |
| 其他形式 | 不进行 RDFS 推理：`rdf:type` 匹配映射中声明的类 | `ASK`、`CONSTRUCT`、`DESCRIBE`、`MINUS`、`VALUES`、`SERVICE` |

表中各项背后的规则见 [docs/semantics.zh-CN.md](docs/semantics.zh-CN.md)：[常量](docs/semantics.zh-CN.md#查询中的常量)、[连接](docs/semantics.zh-CN.md#跨列共享的变量)、[去重](docs/semantics.zh-CN.md#重复的三元组)、[`OPTIONAL`](docs/semantics.zh-CN.md#optional)、[`UNION`](docs/semantics.zh-CN.md#union)、[`FILTER`](docs/semantics.zh-CN.md#filter)、[`BIND` 与 `SELECT` 表达式](docs/semantics.zh-CN.md#bind-与-select-表达式)、[排序](docs/semantics.zh-CN.md#order-bylimit-与-offset) 与[字面量](docs/semantics.zh-CN.md#字面量)。`tests/diff/README.md` 列出了与 Ontop 的已知差异，以及每个被拒绝的测试查询被拒绝的原因。

## 使用说明

| 命令 | 用途 |
|---|---|
| `v12n check` | 校验映射 |
| `v12n terms` | 列出映射公开的类和属性 |
| `v12n explain` | 不连接数据库，显示查询的 SQL |
| `v12n query` | 执行查询并输出结果 |
| `v12n serve` | 在 `/sparql` 提供 SPARQL 1.1 Protocol 端点 |

默认启用的限制为只读数据库会话、30 秒语句超时，以及最多 10000 个解。HTTP 端点还将查询大小限制为 64 KiB，并且最多同时处理 16 个请求。

各命令的输出格式与退出码，以及 HTTP 端点及其设置，见 [docs/usage.zh-CN.md](docs/usage.zh-CN.md)。

## 性能

[bench/README.md](bench/README.md) 说明在相同的 PostgreSQL、映射和查询上与 Ontop 对比的基准测试，[bench/RESULTS.md](bench/RESULTS.md) 给出测量结果及其分析。只有当引擎返回与 Ontop 相同的解时，其计时才计入结果。

以下数据来自 2026-10-26 在提交 `57f7f63` 上进行的一次基准测试，并非当前 main 分支的测量值。PostgreSQL 17.11 中载入了 `alerts` 1,000,000 行、`assets` 100,000 行、`links` 200,000 行和 `observables` 2,200,000 行。v12n 在 Apple M4（16 GiB）的 macOS 上原生运行，Ontop 5.5.0（`-Xmx256m`）和 PostgreSQL 在 Docker Desktop 虚拟机中运行。每个时间为 10 次运行的中位数，从在新连接上发送 SPARQL 1.1 Protocol 请求开始，到 JSON 响应结束为止。

| 查询 | 结果行数 | v12n（毫秒） | Ontop（毫秒） |
|---|---:|---:|---:|
| `q06-union` | 145,000 | 124.5 | 1267.4 |
| `q07-order-limit` | 100 | 194.8 | 142.9 |

在该次测试中，v12n 在九条查询中的八条上用时少于 Ontop，而在按整数和 IRI 排序的 `q07-order-limit` 上用时更长。此后的一项修改缩短了该查询在 `alerts` 为 100,000 行时的用时（[#75 之后的 IRI 排序键](bench/RESULTS.md#iri-sort-keys-after-75)），`alerts` 为 1,000,000 行的测试没有重新运行。上述结果仅适用于该机器和这些查询。Ontop 的堆被限制为 256 MiB，更大的堆可能改变其在大结果上的用时（[注意事项](bench/RESULTS.md#caveats)）。

这些时间是端到端时间，包括 PostgreSQL 中的 SQL 执行、行解码、RDF 项构造以及响应写出。SPARQL 到 SQL 的翻译时间单独测量，报告中每条查询为 15–40 µs。两个引擎都不会将整个数据库转换为 RDF，因此该基准测试不测量整个数据集的 RDF 物化。

## 路线图

计划中的工作按[里程碑](https://github.com/CharmingGroot/sparql-v12n/milestones)分组。箭头表示后一个里程碑使用前一个里程碑的工作。没有路径相连的里程碑彼此不依赖。虚线框表示 main 分支上尚无已合并工作的里程碑。状态以 main 分支为准，而非发布版本；当前支持的范围与限制见[支持的 SPARQL](#支持的-sparql)。

```mermaid
flowchart LR
  M["M 目录元数据"] --> O1["O1 RDFS 本体"] --> O2["O2 OWL 2 QL 本体"]
  M --> R4["R4 图模式与路径"]
  O1 --> R4
  M --> D["D 更多数据库"]
  R1["R1 值表达式<br/>（部分支持）"] --> R3["R3 聚合"]
  R2["R2 运算符与查询形式<br/>（部分支持）"]
  classDef planned stroke-dasharray: 5 4
  class R3,M,O1,O2,R4,D planned
```

| 里程碑 | main 上已有 | 剩余工作 |
|---|---|---|
| [R1 值表达式](https://github.com/CharmingGroot/sparql-v12n/milestone/1) | `FILTER` 中的算术、`BIND` 与 `SELECT` 表达式 | 条件函数、RDF 项函数、字符串、数值、日期时间与哈希函数、类型转换，以及 `ORDER BY` 中的新表达式 |
| [R2 运算符与查询形式](https://github.com/CharmingGroot/sparql-v12n/milestone/3) | SPARQL XML、CSV、TSV 结果 | `VALUES`、`MINUS`、`EXISTS`、子查询、`ASK`、`CONSTRUCT`、`DESCRIBE` |
| [R3 聚合](https://github.com/CharmingGroot/sparql-v12n/milestone/4) | | `GROUP BY`、`HAVING` 与聚合函数 |
| [M 目录元数据](https://github.com/CharmingGroot/sparql-v12n/milestone/2) | | 引擎与后端端口、一次加载并可刷新的目录元数据、利用 NOT NULL 与外键约束的优化 |
| [O1 RDFS 本体](https://github.com/CharmingGroot/sparql-v12n/milestone/5) | | RDFS 蕴涵 |
| [O2 OWL 2 QL 本体](https://github.com/CharmingGroot/sparql-v12n/milestone/7) | | OWL 2 QL |
| [R4 图模式与路径](https://github.com/CharmingGroot/sparql-v12n/milestone/6) | | 变量谓词、命名图与属性路径 |
| [D 更多数据库](https://github.com/CharmingGroot/sparql-v12n/milestone/8) | 将 PostgreSQL 专用 SQL 语法分离到 `Dialect` trait | PostgreSQL 之外的第二个数据库 |

面向 AI 智能体的 MCP 服务器计划在这些里程碑之后实现。

## 目录结构

[docs/architecture.zh-CN.md](docs/architecture.zh-CN.md) 介绍各 crate 之间的依赖关系，以及查询从 SPARQL 文本到结果的路径。

| 路径 | 内容 |
|---|---|
| `crates/v12n-core` | SPARQL 解析、映射索引、展开、优化、SQL 渲染。不依赖数据库 |
| `crates/v12n-postgres` | 在 PostgreSQL 上执行，以及键元数据内省 |
| `crates/v12n-server` | SPARQL 1.1 Protocol HTTP 端点 |
| `crates/v12n-cli` | `v12n` 命令 |
| `crates/v12n-difftest` | 针对 Ontop 的差分测试，每个用例查询对应一个测试 |
| `docs` | [使用说明](docs/usage.zh-CN.md)、[查询语义](docs/semantics.zh-CN.md)与[架构](docs/architecture.zh-CN.md)参考文档，以及 README 中的图 |
| `examples/shop` | 示例数据库模式、数据、映射与查询 |
| `bench` | 在生成数据上与 Ontop 对比的基准测试及其结果 |
| `testenv` | 用于测试的本地 PostgreSQL 与 Ontop 参考端点 |
| `tests/diff` | 差分测试用例（`examples/` 也是用例），W3C R2RML 用例位于 `tests/diff/w3c` |
| `tests/w3c/rdb2rdf` | W3C RDB2RDF 测试文件，未经修改的子集（W3C Document License） |

## 参与贡献

关于如何提出修改、提交约定以及如何运行测试，请参阅 [CONTRIBUTING.md](CONTRIBUTING.md)。

## 许可证

Apache-2.0。参见 [LICENSE](LICENSE)。
