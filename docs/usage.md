# Usage

English | [한국어](usage.ko.md) | [简体中文](usage.zh-CN.md)

This document describes the `v12n` commands, their output and exit codes, and the HTTP endpoint with its settings. The [README](../README.md) gives an overview, and [semantics.md](semantics.md) describes how queries are answered.

## `v12n query`

`v12n query mapping.ttl query.rq --db postgresql://reader@localhost/shop`
runs a SPARQL query and prints its result in one of five formats:

- SPARQL 1.1 Query Results JSON, with `--format json`.
- SPARQL Query Results XML, with `--format xml`.
- SPARQL 1.1 Query Results CSV, with `--format csv`. The CSV holds lexical
  forms only, without datatypes or language tags.
- SPARQL 1.1 Query Results TSV, with `--format tsv`. The TSV holds each term
  in SPARQL syntax: IRIs in `<>`, literals quoted with their language tag or
  datatype, and numbers such as `123` without quotes.
- A table, with `--format table`.

In CSV, TSV and the table, an unbound variable is an empty field. JSON and
XML leave it out of the solution. All formats except the table are written
as rows arrive. XML 1.0 cannot hold control characters other than tab,
line feed and carriage return, so a literal with one, such as U+0001, ends
XML output with an error. `--format xml` and `--format tsv` apply to
`v12n query` only. The other commands refuse them with exit code 1.

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
| `4` | Statement timeout (`--timeout`). A timeout before the first row is reported like other errors. A timeout after output has started leaves JSON or XML on stdout unterminated (CSV and TSV have no end marker) |
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
- every error in a format other than JSON.

The row-limit `warning: ...` (exit code 5) always goes to stderr.

`v12n <command> --help` lists the JSON and CSV shapes of each command. Its
examples run from the repository root.

## Database connection

`--db`, `V12N_DATABASE_URL` and `[database] url` take a libpq connection
URI, such as `postgresql://reader@db.example.com:5432/shop?sslmode=verify-full`,
or a `key=value` connection string, such as
`host=db.example.com dbname=shop user=reader sslmode=verify-full`.

### TLS

The `sslmode` parameter sets whether the connection uses TLS and how the
server certificate is checked. The values have the meaning libpq gives
them:

| `sslmode` | TLS | Certificate check |
|---|---|---|
| `disable` | Never | None |
| `allow` | When the server refuses the session without it | None, or the chain when `sslrootcert` names a file |
| `prefer` (default) | When the server supports it | None, or the chain when `sslrootcert` names a file |
| `require` | Always | None, or the chain when `sslrootcert` names a file |
| `verify-ca` | Always | The certificate chains to a trusted CA |
| `verify-full` | Always | As `verify-ca`, and the certificate holds the host name from `host` |

`prefer` and `require` encrypt the connection but accept any certificate
unless `sslrootcert` names a file, so they protect against eavesdropping
but not against a server that impersonates the database. Use
`verify-full` over a network you do not control.

`allow` connects without TLS and opens a second connection with TLS only
when the server answers the first with an error, for example because
`pg_hba.conf` admits the user over `hostssl` only. A network error is not
retried. When the second connection fails too, its error is reported.

The `sslrootcert` parameter names the trusted CAs:

- Not set: the operating system's root certificates. On Linux these are
  the files OpenSSL uses, and the `SSL_CERT_FILE` and `SSL_CERT_DIR`
  environment variables replace them.
- A path: a PEM file with one or more CA certificates, for a service with
  its own CA. The file is read when the connection is opened.
- `system`: the operating system's root certificates. `sslmode` then
  defaults to `verify-full`, and weaker modes are rejected, as in libpq.

```sh
# A managed service whose certificate chains to a public CA
v12n query mapping.ttl query.rq \
  --db 'postgresql://reader@db.example.com/shop?sslmode=verify-full'

# A server with its own CA
v12n query mapping.ttl query.rq \
  --db 'postgresql://reader@db.internal/shop?sslmode=verify-full&sslrootcert=/etc/v12n/db-ca.pem'
```

The `sslcert` and `sslkey` parameters name a client certificate and its
private key, for a server that authenticates clients by certificate
(`cert` in `pg_hba.conf`, or `clientcert=verify-ca` or `verify-full`):

- `sslcert`: a PEM file with the client certificate, followed by any
  intermediate certificates. The server checks it against its own CA.
- `sslkey`: a PEM file with the private key, not encrypted. On Unix, as in
  libpq, the file must not be readable by others: permissions `0600` or
  less, or `0640` or less when root owns the file.

Both must be set. The certificate is sent when the server asks for one.

The `sslcrl` and `sslcrldir` parameters give certificate revocation lists
(CRLs) for the server certificate:

- `sslcrl`: a PEM file with one or more CRLs.
- `sslcrldir`: a directory of PEM files named as `openssl rehash` names
  them (`<hash>.r0`, `<hash>.r1`, ...). Other files in the directory are
  ignored.

Both may be set. As in libpq, every certificate in the chain below the
trusted CA must be covered by a CRL from its issuer, and a CRL past its
next update is rejected. CRLs apply wherever the chain is checked:
`verify-ca`, `verify-full`, or `sslrootcert` with a file. Without a chain
check they would have no effect, so they are rejected as a configuration
error.

```sh
# A server that authenticates the client by certificate
v12n query mapping.ttl query.rq \
  --db 'postgresql://reader@db.internal/shop?sslmode=verify-full&sslrootcert=/etc/v12n/db-ca.pem&sslcert=/etc/v12n/reader.pem&sslkey=/etc/v12n/reader.key'
```

All files are read when a connection is opened.

When the server offers `SCRAM-SHA-256-PLUS`, password authentication is
bound to the TLS session. Add `channel_binding=require` to refuse a server
that does not offer it.

A failed TLS connection is a database error (exit code 3, kind `database`).
Its message names the cause, for example `invalid peer certificate:
UnknownIssuer` when the certificate does not chain to a trusted CA,
`certificate not valid for name "..."` when it does not hold the host name,
or `invalid peer certificate: Revoked` when a CRL lists it. An unreadable
file is also a database error, and its message names the file.

Settings come only from the connection string. libpq's environment
variables (`PGSSLMODE`, `PGSSLROOTCERT`, `PGSSLCERT`, `PGSSLKEY`,
`PGSSLCRL`, `PGSSLCRLDIR`, `PGHOST` and the others) are not read, so a
variable set for `psql` does not change how v12n connects. The connection
string comes from `--db`, then `V12N_DATABASE_URL`, for `v12n query`, and
from `V12N_DATABASE_URL`, then `[database] url`, for `v12n serve`. A TLS
parameter given twice takes its last value, and a parameter not given takes
the default described in this section.

The behavior differs from libpq in these points:

- Without `sslrootcert`, libpq reads `~/.postgresql/root.crt`. v12n uses
  the operating system's root certificates instead.
- With `prefer`, libpq retries without TLS when the TLS handshake fails.
  v12n reports the error.
- Without `sslcert` and `sslkey`, libpq reads `~/.postgresql/postgresql.crt`
  and `~/.postgresql/postgresql.key` when they exist, and it skips a
  certificate file that does not exist. v12n sends a client certificate
  only when both parameters are set, and reports a missing file.
- libpq ignores `sslcrl` and `sslcrldir` when it checks no chain. v12n
  rejects them.
- Encrypted private keys (`sslpassword`) and the `PG*` environment
  variables are not supported.

## HTTP endpoint

`v12n serve --config v12n.toml` serves the SPARQL 1.1 Protocol at
`/sparql`. It also serves `/healthz` for liveness checks.

A request sends the query in one of three ways:

- `GET /sparql?query=...`;
- `POST` with `application/x-www-form-urlencoded` (`query=...`);
- `POST` with `application/sparql-query` and the query as the body.

The `Accept` header selects the results format:

| Format | Media type |
|---|---|
| SPARQL 1.1 Query Results JSON | `application/sparql-results+json` |
| SPARQL Query Results XML | `application/sparql-results+xml` |
| SPARQL 1.1 Query Results CSV | `text/csv` |
| SPARQL 1.1 Query Results TSV | `text/tab-separated-values` |

The format with the highest `q` value wins. Each format takes the `q` value
of the most specific media range that matches it, so `*/*` does not
override `text/csv;q=0`. When formats tie, the order of the table decides.
A request without `Accept`, or with `*/*`, therefore gets JSON, and
`text/*` gets CSV. A request that accepts none of these formats gets `406`.
CSV and TSV responses carry `charset=utf-8`.

Results in every format are streamed as rows arrive from PostgreSQL.

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

The status is sent before the body. An error after the first row, such as
a statement timeout or a value that XML cannot hold, therefore cannot
change the status. In that case, the server aborts the response and logs
the error. The response ends incomplete, and JSON and XML stay
unterminated.

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

TLS for HTTP clients, authentication and CORS belong to a reverse proxy
in front of the server. TLS to the database is set in the database URL
(see [TLS](#tls)).
