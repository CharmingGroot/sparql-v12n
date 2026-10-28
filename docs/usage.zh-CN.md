# 使用说明

[English](usage.md) | [한국어](usage.ko.md) | 简体中文

> 本文是 [usage.md](usage.md)（提交 ffa5953）的翻译。如有出入，以英文原文为准。

本文介绍 `v12n` 命令、输出格式与退出码，以及 HTTP 端点及其设置。总体介绍见 [README](../README.zh-CN.md)，查询的回答规则见 [semantics.zh-CN.md](semantics.zh-CN.md)。

## `v12n query`

`v12n query mapping.ttl query.rq --db postgresql://reader@localhost/shop` 执行 SPARQL 查询，并以以下三种格式之一输出结果：

- SPARQL 1.1 Query Results JSON。
- SPARQL 1.1 Query Results CSV，使用 `--format csv`。CSV 仅包含词法形式，未绑定变量为空字段。
- 表格，使用 `--format table`。

查询可以来自三处：

- 文件，例如上例中的 `query.rq`；
- 标准输入，以 `-` 代替文件名；
- 命令行，使用 `--query '<SPARQL>'`。

这些格式均无处将结果标记为部分结果。因此，该命令在结果之外报告上限，即写入 stderr 并体现在退出码中：

| 退出码 | 情形 |
|---|---|
| `0` | 成功，包括恰好为 `--row-limit` 行的结果 |
| `1` | 无效输入：参数、文件、查询语法、不支持的 SPARQL |
| `2` | 无效映射 |
| `3` | 数据库错误，或无法转换为 RDF term 的值 |
| `4` | 语句超时（`--timeout`）。第一行之前发生的超时与其他错误一样报告。输出开始之后发生的超时使 stdout 上的 JSON 保持未终止状态（CSV 没有结束标记） |
| `5` | 达到行数上限（`--row-limit`）。stdout 包含前 `--row-limit` 个解，构成完整的结果，stderr 中有一条警告 |

stderr 中的这一行会指明原因与上限。例如，结果被截断时输出 `warning: result truncated at the row limit of 10000; more rows exist ...`，超时时输出 `error: statement timeout of 30s reached after 120 rows; ...`。

## 面向智能体的 CLI

该 CLI 既可由 shell 工具驱动，也可手动使用。它遵循以下规则：

- 从不提示输入。
- 只要 stdout 不是终端就输出 JSON。`--format json` 强制输出 JSON。
- 仅在终端上使用颜色，且设置了 `NO_COLOR` 时从不使用颜色。
- 所有命令均使用上面列出的退出码。

调用方通过三个步骤从映射得到答案。

1. `v12n terms mapping.ttl` 列出映射公开的类与属性。其他类或属性均无法匹配。

   ```json
   {"prefixes":{"ex":"http://example.org/"},
    "classes":[{"iri":"http://example.org/Customer","prefixed_name":"ex:Customer","triples_maps":["<#CustomerMap>"]}, ...],
    "properties":[{"iri":"http://example.org/name","prefixed_name":"ex:name","triples_maps":["<#CustomerMap>"]}, ...]}
   ```

2. `v12n explain mapping.ttl --query '<SPARQL>'` 在没有数据库的情况下检查查询。语法错误或不支持的功能使其以 1 退出。成功时，它输出 SQL、其绑定参数以及投影的变量。

   ```json
   {"sql":"SELECT DISTINCT t0.\"id\" AS \"customer\", ...","params":["VIP"],
    "variables":[{"name":"customer","term_type":"iri","columns":["customer"]}, ...]}
   ```

3. `v12n query mapping.ttl --query '<SPARQL>'` 执行查询并输出 SPARQL 1.1 Query Results JSON。数据库来自 `--db` 或 `V12N_DATABASE_URL`。行数上限、语句超时与只读会话默认开启。

在产生任何输出之前发生的错误，会以一个 JSON 对象代替结果写到 stdout。因此，调用方先检查退出码，然后在两种情况下都读取 stdout：

```json
{"ok":false,"error":{"kind":"query","message":"query syntax error: ...","exit_code":1}}
```

下表列出每种错误 `kind`、其退出码及其 `details`（如有）。

| `kind` | 退出码 | `details` |
|---|---|---|
| `input`、`query`、`unsupported`、`output`、`runtime` | 1 | 无法读取文件时为 `{"path": ...}` |
| `mapping` | 2 | |
| `database`、`data` | 3 | |
| `timeout` | 4 | `{"timeout_ms": ..., "rows": ...}` |

部分错误以一行 `error: ...` 写入 stderr，而不是写入 stdout：

- 结果开始输出之后发生的错误，因为它已无法再改变 stdout；
- 使用 `--format csv` 或 `--format table` 时的所有错误。

行数上限的 `warning: ...`（退出码 5）始终写入 stderr。

`v12n <command> --help` 列出每个命令的 JSON 与 CSV 结构。其中的示例在仓库根目录运行。

## HTTP 端点

`v12n serve --config v12n.toml` 在 `/sparql` 提供 SPARQL 1.1 Protocol 服务。它还在 `/healthz` 提供存活检查。

请求以下列三种方式之一发送查询：

- `GET /sparql?query=...`；
- 使用 `application/x-www-form-urlencoded`（`query=...`）的 `POST`；
- 使用 `application/sparql-query` 并以查询作为请求体的 `POST`。

结果为 SPARQL 1.1 Query Results JSON（`application/sparql-results+json`）。结果随行从 PostgreSQL 到达而以流式发送。请求其他任何格式时返回 `406`。

参数 `default-graph-uri` 与 `named-graph-uri` 返回 `400`，因为数据集即映射所定义的数据集。

| 状态码 | 情形 |
|---|---|
| `200` | 结果。适用行数上限且结果被截断时，响应带有 `V12N-Truncated: <row limit>`，并恰好包含该数量的解。同样情况下，`v12n query` 以 5 退出 |
| `400` | 查询语法错误、缺失或重复的 `query`、数据集参数，或展开后 SQL 分支过多的查询 |
| `406` / `413` / `414` / `415` | `406`：未提供的格式。`413`：`POST` 中超过 `max_query_bytes` 的查询。`414`：`GET` 中超过 `max_query_bytes` 的查询。`415`：其他任何内容类型 |
| `501` | 尚不支持的 SPARQL 功能 |
| `503` | 第一行之前发生的语句超时（此时 `v12n query` 以 4 退出），或达到 `max_concurrent_requests`（响应带有 `Retry-After`） |
| `500` | 数据库错误或其他内部错误 |

状态码在响应体之前发送。因此，第一行之后发生的错误无法改变状态码。此时，服务器中止响应并记录该错误，JSON 保持未终止状态。

结果是否被截断在第一行之前即可确定。PostgreSQL 在发送行之前会先统计不超过上限的行数。

设置来自以 `--config` 指定的 TOML 文件，例如：

```toml
[server]
address = "127.0.0.1"
port = 7878

[database]
url = "postgresql://reader@localhost/shop"
password_env = "SHOP_DB_PASSWORD"   # optional: read the password from this variable

[mapping]
path = "mapping.ttl"                # relative to this file
base_iri = "http://data.example.com/"   # optional: for relative IRIs (see semantics.md)

[limits]
statement_timeout = "30s"           # 0 disables
row_limit = 10000                   # 0 disables
max_query_bytes = 65536
max_concurrent_requests = 16        # more are refused with 503, not queued
```

环境变量会覆盖文件中的设置。下表列出每项设置及其环境变量与默认值。

| 设置 | 环境变量 | 默认值 |
|---|---|---|
| `[server] address` | `V12N_ADDRESS` | `127.0.0.1` |
| `[server] port` | `V12N_PORT` | `7878` |
| `[database] url` | `V12N_DATABASE_URL` | 必填 |
| `[database] password_env` | （仅限文件） | 无 |
| `[mapping] path` | `V12N_MAPPING` | 必填 |
| `[mapping] base_iri` | `V12N_BASE_IRI` | 无：相对 IRI 视为数据错误 |
| `[limits] statement_timeout` | `V12N_STATEMENT_TIMEOUT` | `30s` |
| `[limits] row_limit` | `V12N_ROW_LIMIT` | `10000` |
| `[limits] max_query_bytes` | `V12N_MAX_QUERY_BYTES` | `65536` |
| `[limits] max_concurrent_requests` | `V12N_MAX_CONCURRENT_REQUESTS` | `16` |

其他命令读取部分相同的变量：

- `v12n query` 读取 `V12N_DATABASE_URL`、`V12N_ROW_LIMIT`、`V12N_STATEMENT_TIMEOUT` 与 `V12N_BASE_IRI`。其命令行选项优先于这些变量。
- `v12n explain` 读取 `V12N_BASE_IRI`。

对于 `serve`，`--base-iri` 选项也会覆盖基础 IRI 设置。`serve` 忽略其他 `V12N_*` 变量。它为每个此类变量记录一条警告，以便发现拼写错误的变量名。

请以只能读取的数据库用户连接。引擎也会将会话设为只读事务。

数据库 URL 和密码从不写入日志。请求日志写入 stderr，级别由 `RUST_LOG` 决定。日志包含生成的 SQL 与耗时，但不包含结果值。

TLS、认证与 CORS 由服务器前置的反向代理负责。
