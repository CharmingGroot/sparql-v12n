# sparql-v12n

Query PostgreSQL with SPARQL through R2RML mappings, without copying the data.

sparql-v12n is a virtualization (virtual knowledge graph, OBDA) engine written in Rust.
It rewrites a SPARQL query into SQL using an R2RML mapping, runs the SQL on the
source database, and returns SPARQL results. The mapping also defines exactly
which tables and columns can be queried.

> Status: early development. Not usable yet.

## Planned interfaces

| Interface | Use |
|---|---|
| `v12n` CLI | `check`, `terms`, `explain`, `query`, `serve` |
| HTTP | SPARQL 1.1 Protocol endpoint at `/sparql` |
| MCP | tools for AI agents (later) |

## HTTP endpoint

`v12n serve --config v12n.toml` serves the SPARQL 1.1 Protocol at `/sparql`
(and `/healthz` for liveness):

- `GET /sparql?query=...`, `POST` with `application/x-www-form-urlencoded`
  (`query=...`), or `POST` with `application/sparql-query` and the query as
  the body.
- Results are SPARQL 1.1 Query Results JSON (`application/sparql-results+json`),
  streamed as rows arrive from PostgreSQL. Other formats get `406`.
- `default-graph-uri` and `named-graph-uri` get `400`: the dataset is the one
  the mapping defines.

| Status | When |
|---|---|
| `200` | Results. With a row limit, a cut-off result carries `V12N-Truncated: <row limit>` and holds exactly that many solutions |
| `400` | Query syntax, a missing or repeated `query`, dataset parameters, a query that unfolds into too many SQL branches |
| `406` / `413` / `414` / `415` | Format not offered / query over `max_query_bytes` in a `POST` / in a `GET` / other content type |
| `501` | A SPARQL feature not supported yet |
| `503` | Statement timeout before the first row, or `max_concurrent_requests` reached (with `Retry-After`) |
| `500` | Database or other internal errors |

The status is sent before the body, so an error after the first row cannot
change it: the server aborts the response (the JSON stays unterminated) and
logs the error. Whether a result is truncated is known before the first row,
because PostgreSQL counts the rows up to the limit before sending them.

Settings come from a TOML file and `V12N_<SECTION>__<KEY>` environment
variables, which win over the file:

```toml
[server]
address = "127.0.0.1"          # V12N_SERVER__ADDRESS
port = 7878                     # V12N_SERVER__PORT

[database]
url = "postgresql://reader@localhost/shop"   # V12N_DATABASE__URL
password_env = "SHOP_DB_PASSWORD"            # optional: read the password from this variable

[mapping]
path = "mapping.ttl"            # relative to this file; V12N_MAPPING__PATH

[limits]
statement_timeout = "30s"       # 0 disables
row_limit = 10000               # 0 disables
max_query_bytes = 65536
max_concurrent_requests = 16    # more are refused with 503, not queued
```

Connect as a database user that can only read; sessions are also set to
read-only transactions. The database URL and password are never logged. Request
logs (stderr, level from `RUST_LOG`) carry the generated SQL and timings, not
result values. TLS, authentication and CORS belong to a reverse proxy in front.

## Layout

| Path | Contents |
|---|---|
| `crates/v12n-core` | SPARQL parsing, mapping index, unfolding, optimization, SQL rendering. No database dependency |
| `crates/v12n-postgres` | Execution on PostgreSQL and key metadata introspection |
| `crates/v12n-server` | SPARQL 1.1 Protocol HTTP endpoint |
| `crates/v12n-cli` | The `v12n` command |
| `examples/shop` | Example schema, data, mapping and queries |
| `testenv` | Local PostgreSQL (and later the reference endpoint) for tests |
| `tests/diff` | Differential test cases |

## License

Apache-2.0. See [LICENSE](LICENSE).
