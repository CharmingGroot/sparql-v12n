# Usage

English | [한국어](usage.ko.md) | [简体中文](usage.zh-CN.md)

This document describes the `v12n` commands, their output and exit codes, and the HTTP endpoint with its settings. The [README](../README.md) gives an overview, and [semantics.md](semantics.md) describes how queries are answered.

## `v12n query`

`v12n query mapping.ttl query.rq --db postgresql://reader@localhost/shop`
runs a SPARQL query and prints its result in one of three formats:

- SPARQL 1.1 Query Results JSON.
- SPARQL 1.1 Query Results CSV, with `--format csv`. The CSV holds lexical
  forms only, and an unbound variable is an empty field.
- A table, with `--format table`.

The query can come from three places:

- a file, such as `query.rq` in the example above;
- stdin, with `-` in place of the file name;
- the command line, with `--query '<SPARQL>'`.

None of these formats has a place to mark a result as partial. The command
therefore reports limits outside the result, on stderr and in the exit code:

| Exit code | When |
|---|---|
| `0` | Success, including a result of exactly `--row-limit` rows |
| `1` | Invalid input: arguments, files, query syntax, unsupported SPARQL |
| `2` | Invalid mapping |
| `3` | Database error, or a value that cannot become an RDF term |
| `4` | Statement timeout (`--timeout`). A timeout before the first row is reported like other errors. A timeout after output has started leaves the JSON on stdout unterminated (CSV has no end marker) |
| `5` | Row limit reached (`--row-limit`). Stdout holds the first `--row-limit` solutions as a complete result, and stderr has a warning |

The stderr line names the cause and the limit. For example, a truncated
result prints
`warning: result truncated at the row limit of 10000; more rows exist ...`,
and a timeout prints
`error: statement timeout of 30s reached after 120 rows; ...`.

## CLI for agents

The CLI is meant to be driven by a shell tool as well as by hand. It
follows these rules:

- It never prompts for input.
- It prints JSON whenever stdout is not a terminal. `--format json` forces
  JSON output.
- It uses colour only on a terminal, and never when `NO_COLOR` is set.
- Every command uses the exit codes listed above.

A caller goes from a mapping to an answer in three steps.

1. `v12n terms mapping.ttl` lists the classes and properties that the
   mapping exposes. No other class or property can match.

   ```json
   {"prefixes":{"ex":"http://example.org/"},
    "classes":[{"iri":"http://example.org/Customer","prefixed_name":"ex:Customer","triples_maps":["<#CustomerMap>"]}, ...],
    "properties":[{"iri":"http://example.org/name","prefixed_name":"ex:name","triples_maps":["<#CustomerMap>"]}, ...]}
   ```

2. `v12n explain mapping.ttl --query '<SPARQL>'` checks the query without a
   database. A syntax error or an unsupported feature makes it exit with 1.
   On success, it prints the SQL, its bind parameters and the projected
   variables.

   ```json
   {"sql":"SELECT DISTINCT t0.\"id\" AS \"customer\", ...","params":["VIP"],
    "variables":[{"name":"customer","term_type":"iri","columns":["customer"]}, ...]}
   ```

3. `v12n query mapping.ttl --query '<SPARQL>'` runs the query and prints
   SPARQL 1.1 Query Results JSON. The database comes from `--db` or
   `V12N_DATABASE_URL`. The row limit, the statement timeout and the
   read-only session are on by default.

An error before any output is printed as one JSON object on stdout, in
place of the result. A caller therefore checks the exit code and then reads
stdout in both cases:

```json
{"ok":false,"error":{"kind":"query","message":"query syntax error: ...","exit_code":1}}
```

The table below lists each error `kind`, its exit code and its `details`,
if any.

| `kind` | Exit code | `details` |
|---|---|---|
| `input`, `query`, `unsupported`, `output`, `runtime` | 1 | `{"path": ...}` when a file cannot be read |
| `mapping` | 2 | |
| `database`, `data` | 3 | |
| `timeout` | 4 | `{"timeout_ms": ..., "rows": ...}` |

Some errors go to stderr as one `error: ...` line instead of to stdout:

- an error after results have started, because it cannot change stdout any
  more;
- every error with `--format csv` or `--format table`.

The row-limit `warning: ...` (exit code 5) always goes to stderr.

`v12n <command> --help` lists the JSON and CSV shapes of each command. Its
examples run from the repository root.

## HTTP endpoint

`v12n serve --config v12n.toml` serves the SPARQL 1.1 Protocol at
`/sparql`. It also serves `/healthz` for liveness checks.

A request sends the query in one of three ways:

- `GET /sparql?query=...`;
- `POST` with `application/x-www-form-urlencoded` (`query=...`);
- `POST` with `application/sparql-query` and the query as the body.

Results are SPARQL 1.1 Query Results JSON
(`application/sparql-results+json`). They are streamed as rows arrive from
PostgreSQL. A request for any other format gets `406`.

The parameters `default-graph-uri` and `named-graph-uri` get `400`, because
the dataset is the one that the mapping defines.

| Status | When |
|---|---|
| `200` | Results. When a row limit applies and the result is cut off, the response carries `V12N-Truncated: <row limit>` and holds exactly that many solutions. In the same case, `v12n query` exits with 5 |
| `400` | A query syntax error, a missing or repeated `query`, dataset parameters, or a query that unfolds into too many SQL branches |
| `406` / `413` / `414` / `415` | `406`: a format that is not offered. `413`: a query over `max_query_bytes` in a `POST`. `414`: a query over `max_query_bytes` in a `GET`. `415`: any other content type |
| `501` | A SPARQL feature that is not supported yet |
| `503` | A statement timeout before the first row (`v12n query` exits with 4 in this case), or `max_concurrent_requests` reached (the response carries `Retry-After`) |
| `500` | A database error or another internal error |

The status is sent before the body. An error after the first row therefore
cannot change the status. In that case, the server aborts the response and
logs the error, and the JSON stays unterminated.

Whether a result is truncated is known before the first row. PostgreSQL
counts the rows up to the limit before it sends them.

Settings come from a TOML file given with `--config`, such as the
following:

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

Environment variables override the settings in the file. The table lists
each setting with its environment variable and its default.

| Setting | Environment variable | Default |
|---|---|---|
| `[server] address` | `V12N_ADDRESS` | `127.0.0.1` |
| `[server] port` | `V12N_PORT` | `7878` |
| `[database] url` | `V12N_DATABASE_URL` | required |
| `[database] password_env` | (file only) | none |
| `[mapping] path` | `V12N_MAPPING` | required |
| `[mapping] base_iri` | `V12N_BASE_IRI` | none: relative IRIs are data errors |
| `[limits] statement_timeout` | `V12N_STATEMENT_TIMEOUT` | `30s` |
| `[limits] row_limit` | `V12N_ROW_LIMIT` | `10000` |
| `[limits] max_query_bytes` | `V12N_MAX_QUERY_BYTES` | `65536` |
| `[limits] max_concurrent_requests` | `V12N_MAX_CONCURRENT_REQUESTS` | `16` |

The other commands read some of the same variables:

- `v12n query` reads `V12N_DATABASE_URL`, `V12N_ROW_LIMIT`,
  `V12N_STATEMENT_TIMEOUT` and `V12N_BASE_IRI`. Its command-line options
  take precedence over these variables.
- `v12n explain` reads `V12N_BASE_IRI`.

For `serve`, the `--base-iri` option also overrides the base IRI setting.
`serve` ignores other `V12N_*` variables. It logs a warning for each one,
so a misspelled name is visible.

Connect as a database user that can only read. The engine also sets
sessions to read-only transactions.

The database URL and the password are never logged. Request logs go to
stderr, at the level set by `RUST_LOG`. They carry the generated SQL and
timings, but not result values.

TLS, authentication and CORS belong to a reverse proxy in front of the
server.
