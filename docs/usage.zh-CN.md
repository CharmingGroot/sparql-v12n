# 使用说明

[English](usage.md) | [한국어](usage.ko.md) | 简体中文

> 本文是 [usage.md](usage.md)（提交 8e6fa66）的翻译。如有出入，以英文原文为准。

本文介绍 `v12n` 命令、输出格式与退出码，以及 HTTP 端点及其设置。总体介绍见 [README](../README.zh-CN.md)，查询的回答规则见 [semantics.zh-CN.md](semantics.zh-CN.md)。

## `v12n query`

`v12n query mapping.ttl query.rq --db postgresql://reader@localhost/shop` 执行 SPARQL 查询，并以以下五种格式之一输出结果：

- SPARQL 1.1 Query Results JSON，使用 `--format json`。
- SPARQL Query Results XML，使用 `--format xml`。
- SPARQL 1.1 Query Results CSV，使用 `--format csv`。CSV 仅包含词法形式，不含数据类型和语言标签。
- SPARQL 1.1 Query Results TSV，使用 `--format tsv`。TSV 以 SPARQL 语法写出每个 term：IRI 用 `<>` 括起，字面量加引号并带上语言标签或数据类型，`123` 这样的数字不加引号。
- 表格，使用 `--format table`。

在 CSV、TSV 和表格中，未绑定变量为空字段。JSON 和 XML 则在解中省略该变量。除表格外，所有格式都随行到达而输出。XML 1.0 无法容纳制表符、换行符和回车符以外的控制字符，因此含有此类字符（例如 U+0001）的字面量会使 XML 输出以错误结束。`--format xml` 和 `--format tsv` 仅适用于 `v12n query`，其他命令以退出码 1 拒绝它们。

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
| `4` | 语句超时（`--timeout`）。第一行之前发生的超时与其他错误一样报告。输出开始之后发生的超时使 stdout 上的 JSON 或 XML 保持未终止状态（CSV 和 TSV 没有结束标记） |
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
- 使用 JSON 以外格式时的所有错误。

行数上限的 `warning: ...`（退出码 5）始终写入 stderr。

`v12n <command> --help` 列出每个命令的 JSON 与 CSV 结构。其中的示例在仓库根目录运行。

## 数据库连接

`--db`、`V12N_DATABASE_URL` 与 `[database] url` 接受 libpq 连接 URI（例如 `postgresql://reader@db.example.com:5432/shop?sslmode=verify-full`），或 `key=value` 形式的连接字符串（例如 `host=db.example.com dbname=shop user=reader sslmode=verify-full`）。

### TLS

`sslmode` 参数决定连接是否使用 TLS，以及如何检查服务器证书。各取值的含义与 libpq 相同：

| `sslmode` | TLS | 证书检查 |
|---|---|---|
| `disable` | 从不使用 | 无 |
| `allow` | 服务器拒绝不使用 TLS 的会话时使用 | 无；`sslrootcert` 指定文件时检查证书链 |
| `prefer`（默认） | 服务器支持时使用 | 无；`sslrootcert` 指定文件时检查证书链 |
| `require` | 始终使用 | 无；`sslrootcert` 指定文件时检查证书链 |
| `verify-ca` | 始终使用 | 证书链可追溯到受信任的 CA |
| `verify-full` | 始终使用 | 同 `verify-ca`，并且证书包含 `host` 中的主机名 |

`prefer` 与 `require` 会加密连接，但除非 `sslrootcert` 指定了文件，否则接受任何证书。因此它们能防止窃听，却不能防止冒充数据库的服务器。在不受自己控制的网络上，请使用 `verify-full`。

`allow` 先不使用 TLS 连接，仅当服务器以错误应答该连接时，才使用 TLS 打开第二个连接，例如 `pg_hba.conf` 只允许该用户通过 `hostssl` 连接的情况。网络错误不会重试。若第二个连接也失败，则报告它的错误。

`sslrootcert` 参数指定受信任的 CA：

- 未设置：使用操作系统的根证书。在 Linux 上即 OpenSSL 使用的文件，可由环境变量 `SSL_CERT_FILE` 与 `SSL_CERT_DIR` 替换。
- 路径：包含一个或多个 CA 证书的 PEM 文件，用于使用自有 CA 的服务。该文件在打开连接时读取。
- `system`：使用操作系统的根证书。此时 `sslmode` 默认为 `verify-full`，与 libpq 一样拒绝较弱的模式。

```sh
# A managed service whose certificate chains to a public CA
v12n query mapping.ttl query.rq \
  --db 'postgresql://reader@db.example.com/shop?sslmode=verify-full'

# A server with its own CA
v12n query mapping.ttl query.rq \
  --db 'postgresql://reader@db.internal/shop?sslmode=verify-full&sslrootcert=/etc/v12n/db-ca.pem'
```

`sslcert` 与 `sslkey` 参数指定客户端证书及其私钥，用于通过证书认证客户端的服务器（`pg_hba.conf` 中的 `cert`，或 `clientcert=verify-ca`、`verify-full`）：

- `sslcert`：包含客户端证书及其后所需中间证书的 PEM 文件。服务器使用自己的 CA 检查该证书。
- `sslkey`：包含未加密私钥的 PEM 文件。在 Unix 上，与 libpq 一样，该文件不得被其他用户读取：权限须为 `0600` 或更严格，文件属于 root 时须为 `0640` 或更严格。

两个参数必须同时设置。服务器请求时才发送证书。

`sslcrl` 与 `sslcrldir` 参数为服务器证书指定证书吊销列表（CRL）：

- `sslcrl`：包含一个或多个 CRL 的 PEM 文件。
- `sslcrldir`：一个目录，其中的 PEM 文件按 `openssl rehash` 的方式命名（`<hash>.r0`、`<hash>.r1` 等）。目录中的其他文件不会被读取。

两个参数可以同时设置。与 libpq 一样，受信任 CA 之下证书链中的每个证书都必须有其签发者的 CRL，超过下次更新时间的 CRL 会被拒绝。CRL 在检查证书链时生效：`verify-ca`、`verify-full`，或 `sslrootcert` 指定文件时。不检查证书链时 CRL 不起作用，因此作为配置错误拒绝。

```sh
# A server that authenticates the client by certificate
v12n query mapping.ttl query.rq \
  --db 'postgresql://reader@db.internal/shop?sslmode=verify-full&sslrootcert=/etc/v12n/db-ca.pem&sslcert=/etc/v12n/reader.pem&sslkey=/etc/v12n/reader.key'
```

所有文件都在打开连接时读取。

当服务器提供 `SCRAM-SHA-256-PLUS` 时，密码认证会绑定到 TLS 会话。添加 `channel_binding=require` 可拒绝不提供该机制的服务器。

TLS 连接失败属于数据库错误（退出码 3，kind 为 `database`）。错误消息会指出原因，例如证书链无法追溯到受信任的 CA 时为 `invalid peer certificate: UnknownIssuer`，证书不包含主机名时为 `certificate not valid for name "..."`，证书被 CRL 列出时为 `invalid peer certificate: Revoked`。无法读取文件同样属于数据库错误，错误消息会指出该文件。

设置只从连接字符串读取。不读取 libpq 的环境变量（`PGSSLMODE`、`PGSSLROOTCERT`、`PGSSLCERT`、`PGSSLKEY`、`PGSSLCRL`、`PGSSLCRLDIR`、`PGHOST` 等），因此为 `psql` 设置的变量不会改变 v12n 的连接方式。连接字符串在 `v12n query` 中依次取自 `--db`、`V12N_DATABASE_URL`，在 `v12n serve` 中依次取自 `V12N_DATABASE_URL`、`[database] url`。同一个 TLS 参数出现两次时使用最后一个值，未给出的参数使用本节所述的默认值。

与 libpq 的不同之处如下：

- 未设置 `sslrootcert` 时，libpq 读取 `~/.postgresql/root.crt`，v12n 则使用操作系统的根证书。
- 使用 `prefer` 时，若 TLS 握手失败，libpq 会不使用 TLS 重试，v12n 则报告错误。
- 未设置 `sslcert` 与 `sslkey` 时，libpq 会在 `~/.postgresql/postgresql.crt` 与 `~/.postgresql/postgresql.key` 存在时读取它们，并跳过不存在的证书文件。v12n 仅在两个参数都设置时发送客户端证书，并报告缺失的文件。
- 不检查证书链时，libpq 忽略 `sslcrl` 与 `sslcrldir`，v12n 则拒绝它们。
- 不支持加密的私钥（`sslpassword`）与 `PG*` 环境变量。

## HTTP 端点

`v12n serve --config v12n.toml` 在 `/sparql` 提供 SPARQL 1.1 Protocol 服务。它还在 `/healthz` 提供存活检查。

请求以下列三种方式之一发送查询：

- `GET /sparql?query=...`；
- 使用 `application/x-www-form-urlencoded`（`query=...`）的 `POST`；
- 使用 `application/sparql-query` 并以查询作为请求体的 `POST`。

`Accept` 请求头选择结果格式：

| 格式 | 媒体类型 |
|---|---|
| SPARQL 1.1 Query Results JSON | `application/sparql-results+json` |
| SPARQL Query Results XML | `application/sparql-results+xml` |
| SPARQL 1.1 Query Results CSV | `text/csv` |
| SPARQL 1.1 Query Results TSV | `text/tab-separated-values` |

`q` 值最高的格式胜出。每种格式采用与其匹配的最具体媒体范围的 `q` 值，因此 `*/*` 不会覆盖 `text/csv;q=0`。`q` 值相同时，按表中顺序决定。因此，不带 `Accept` 或带 `*/*` 的请求得到 JSON，`text/*` 得到 CSV。不接受上述任何格式的请求返回 `406`。CSV 和 TSV 响应带有 `charset=utf-8`。

所有格式的结果都随行从 PostgreSQL 到达而以流式发送。

参数 `default-graph-uri` 与 `named-graph-uri` 返回 `400`，因为数据集即映射所定义的数据集。

| 状态码 | 情形 |
|---|---|
| `200` | 结果。适用行数上限且结果被截断时，响应带有 `V12N-Truncated: <row limit>`，并恰好包含该数量的解。同样情况下，`v12n query` 以 5 退出 |
| `400` | 查询语法错误、缺失或重复的 `query`、数据集参数，或展开后 SQL 分支过多的查询 |
| `406` / `413` / `414` / `415` | `406`：未提供的格式。`413`：`POST` 中超过 `max_query_bytes` 的查询。`414`：`GET` 中超过 `max_query_bytes` 的查询。`415`：其他任何内容类型 |
| `501` | 尚不支持的 SPARQL 功能 |
| `503` | 第一行之前发生的语句超时（此时 `v12n query` 以 4 退出），或达到 `max_concurrent_requests`（响应带有 `Retry-After`） |
| `500` | 数据库错误或其他内部错误 |

状态码在响应体之前发送。因此，第一行之后发生的错误（例如语句超时，或 XML 无法容纳的值）无法改变状态码。此时，服务器中止响应并记录该错误。响应以不完整的状态结束，JSON 和 XML 保持未终止状态。

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

面向 HTTP 客户端的 TLS、认证与 CORS 由服务器前置的反向代理负责。与数据库之间的 TLS 在数据库 URL 中设置（参见 [TLS](#tls)）。
