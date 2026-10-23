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

## `v12n query`

`v12n query mapping.ttl query.rq --db postgresql://reader@localhost/shop`
prints SPARQL 1.1 Query Results JSON (or a table with `--format table`).
SPARQL results JSON has no place to mark a partial result, so limits are
reported outside it, on stderr and in the exit code:

| Exit code | When |
|---|---|
| `0` | Success, including a result of exactly `--row-limit` rows |
| `1` | Invalid input: arguments, files, query syntax, unsupported SPARQL |
| `2` | Invalid mapping |
| `3` | Database error, or a value that cannot become an RDF term |
| `4` | Statement timeout (`--timeout`). Before the first row it is reported like other errors; after output has started, stdout is left unterminated |
| `5` | Row limit reached (`--row-limit`): stdout holds the first `--row-limit` solutions as a complete result, and stderr has a warning |

The stderr line names the cause and the limit, for example
`warning: result truncated at the row limit of 10000; more rows exist ...` or
`error: statement timeout of 30s reached after 120 rows; ...`.

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
| `200` | Results. With a row limit, a cut-off result carries `V12N-Truncated: <row limit>` and holds exactly that many solutions (`v12n query` exits with 5 in the same case) |
| `400` | Query syntax, a missing or repeated `query`, dataset parameters, a query that unfolds into too many SQL branches |
| `406` / `413` / `414` / `415` | Format not offered / query over `max_query_bytes` in a `POST` / in a `GET` / other content type |
| `501` | A SPARQL feature not supported yet |
| `503` | Statement timeout before the first row (`v12n query` exits with 4), or `max_concurrent_requests` reached (with `Retry-After`) |
| `500` | Database or other internal errors |

The status is sent before the body, so an error after the first row cannot
change it: the server aborts the response (the JSON stays unterminated) and
logs the error. Whether a result is truncated is known before the first row,
because PostgreSQL counts the rows up to the limit before sending them.

Settings come from a TOML file (`--config`); environment variables override it:

```toml
[server]
address = "127.0.0.1"
port = 7878

[database]
url = "postgresql://reader@localhost/shop"
password_env = "SHOP_DB_PASSWORD"   # optional: read the password from this variable

[mapping]
path = "mapping.ttl"                # relative to this file

[limits]
statement_timeout = "30s"           # 0 disables
row_limit = 10000                   # 0 disables
max_query_bytes = 65536
max_concurrent_requests = 16        # more are refused with 503, not queued
```

| Setting | Environment variable | Default |
|---|---|---|
| `[server] address` | `V12N_ADDRESS` | `127.0.0.1` |
| `[server] port` | `V12N_PORT` | `7878` |
| `[database] url` | `V12N_DATABASE_URL` | required |
| `[database] password_env` | (file only) | none |
| `[mapping] path` | `V12N_MAPPING` | required |
| `[limits] statement_timeout` | `V12N_STATEMENT_TIMEOUT` | `30s` |
| `[limits] row_limit` | `V12N_ROW_LIMIT` | `10000` |
| `[limits] max_query_bytes` | `V12N_MAX_QUERY_BYTES` | `65536` |
| `[limits] max_concurrent_requests` | `V12N_MAX_CONCURRENT_REQUESTS` | `16` |

`v12n query` reads the same `V12N_DATABASE_URL`, `V12N_ROW_LIMIT` and
`V12N_STATEMENT_TIMEOUT`, below its command-line options. `serve` ignores other
`V12N_*` variables and logs a warning for each, so a misspelled name is
visible.

Connect as a database user that can only read; sessions are also set to
read-only transactions. The database URL and password are never logged. Request
logs (stderr, level from `RUST_LOG`) carry the generated SQL and timings, not
result values. TLS, authentication and CORS belong to a reverse proxy in front.

## Column names

Column names in `rr:column`, templates and join conditions are SQL
identifiers (R2RML §5). A delimited name (`"\"Name\""`) is taken as
written. A regular name is resolved as PostgreSQL resolves it unquoted,
which folds it to lower case: `rr:column "StudentId"` reads the column
that `SELECT ... AS StudentId` produces (`studentid`), and `rr:column
"Name"` does not match a column created as `"Name"`. Before running a
query that uses such a name, the engine prepares a probe statement in
which PostgreSQL resolves it, and then quotes the resolved name in the
generated SQL. Names of lower-case letters, digits and `_` need no probe.

## Lexical forms of column values

Every non-string value is written in the canonical form of XML Schema 1.1
Part 2, the version R2RML references, both in literals and in template
values: `numeric` 12.50 and 3.00 become `"12.5"^^xsd:decimal` and
`"3"^^xsd:decimal` (an integral decimal has no decimal point), `float8` 30
becomes `"3.0E1"^^xsd:double`, `boolean` gives `true`/`false`, and `bytea`
upper-case hexadecimal. Results therefore do not depend on a column's
declared scale. Ontop writes PostgreSQL's text output instead (`12.50`,
`"30"^^xsd:double`); the differential tests record these as known
differences (issue #25).

One exception: a `time` value of `24:00:00` is written as is, although the
XSD 1.1 canonical form is `00:00:00` (issue #37).

## Constants in a query

Constants in triple patterns are matched as RDF terms, as SPARQL requires:
the lexical form and the datatype must both be equal. A column value takes
part through its R2RML natural mapping (`integer` columns give
`xsd:integer` in canonical form, `numeric` gives canonical `xsd:decimal`,
and so on). So, for an integer column holding 7:

| Constant | Matches | Why |
|---|---|---|
| `7`, `<…/customer/7>` | yes | |
| `"7"`, `"7"^^xsd:decimal`, `7.0` | no | Different datatype |
| `"07"^^xsd:integer`, `<…/customer/007>`, `<…/customer/+7>` | no | Not the canonical form the mapping produces |
| `99999999999`, `<…/customer/99999999999>` | no | Out of range for the column, so no row holds it |

Constants are sent to PostgreSQL as bound parameters, never inside the
SQL text. Before binding, the engine checks each one against the
parameter type PostgreSQL infers from the column: it reads the constant
as that type and formats it back with the same code that builds result
terms, and only an identical round trip is sent, in binary where the type
allows. A constant that cannot match empties its own `UNION` branch
instead of making PostgreSQL fail on input it cannot parse; the query
skips the database when no branch is left. The comparison stays on the
column's own type, so its indexes are used. For column types without a
natural datatype (such as `interval`), results carry the column cast to
text, and constants are compared with that same text (`col::text = $1`),
which cannot use an index on the column.

Value comparisons across datatypes, such as `FILTER(?amount = 7.0)` on an
integer column, belong to `FILTER`, which is not supported yet. When it
is, the plan is to convert the constant to the column's type in the
engine when the value is exactly representable (`7.0` becomes `7`) and to
treat the equality as false otherwise (`7.5`), rather than casting the
column, which in
PostgreSQL turns `id = 7.0` into `id::numeric = 7.0` and stops it from
using an index on `id`.

## Variables shared across columns

A variable that occurs twice is a join, and it also compares RDF terms,
not SQL values. Before running such a query, the engine asks PostgreSQL
for the types of the columns involved (by preparing a probe statement)
and then:

- compares columns of the same kind (integers, text, dates, ...) with
  `=`, so indexes are used;
- compares other pairs through the natural lexical form computed in SQL
  (`int_col::text = text_col::text`) after checking the natural datatypes:
  an `integer` column holding 7 joins a text column only if that column
  has `rr:datatype xsd:integer`, and never a `numeric` 7.0. A `numeric`
  value's canonical form is computed with `trim_scale(col)::text`
  (PostgreSQL 13 or later), which is tested to equal the engine's own
  formatting;
- percent-encodes template values as IRIs do (R2RML IRI-safe version)
  when templates or IRI columns can only be compared as strings;
- rejects the query as unsupported (exit code 1, HTTP 501) when the
  lexical form cannot be computed in SQL, for example a `float8` or
  date/time template value against a text column, instead of failing in
  PostgreSQL or returning wrong rows.

## Duplicate triples

An RDF graph is a set of triples, so a triple counts once however many
rows produce it: duplicate rows of a table without a key, an R2RML view
that repeats rows, or two term maps that produce the same triple. A basic
graph pattern has one solution per distinct binding of all its variables,
and only then does `SELECT` project, so `SELECT ?city WHERE { ?p ex:city
?city }` still returns a city once per person.

The generated SQL removes duplicates with `DISTINCT` (or `UNION` across
term maps) over the columns of every variable of the pattern, in a
subquery when some are not selected. Before running a query that reads
tables (not views) through a single combination of term maps, the engine
reads their unique keys from the catalog; when every table has a key
among the columns the solution fixes, no two rows can give the same
solution and the SQL keeps all rows without `DISTINCT`. Otherwise
PostgreSQL has to deduplicate, which costs a hash or sort over the rows
and, unless an index delivers them in order, reads all of them before the
first one is returned. `v12n explain` has no database and always shows the
`DISTINCT`.

Duplicates are compared by column values, so two different values that a
template renders as the same string (`{a}-{b}` with `x-`, `y` and `x`,
`-y`), or columns of different types that give the same term in two term
maps, still count twice.

## Layout

| Path | Contents |
|---|---|
| `crates/v12n-core` | SPARQL parsing, mapping index, unfolding, optimization, SQL rendering. No database dependency |
| `crates/v12n-postgres` | Execution on PostgreSQL and key metadata introspection |
| `crates/v12n-server` | SPARQL 1.1 Protocol HTTP endpoint |
| `crates/v12n-cli` | The `v12n` command |
| `crates/v12n-difftest` | Differential tests against Ontop, one test per case query |
| `examples/shop` | Example schema, data, mapping and queries |
| `testenv` | Local PostgreSQL and the Ontop reference endpoint for tests |
| `tests/diff` | Differential test cases (`examples/` are cases too), W3C R2RML cases in `tests/diff/w3c` |
| `tests/w3c/rdb2rdf` | W3C RDB2RDF test files, unmodified subset (W3C Document License) |

## License

Apache-2.0. See [LICENSE](LICENSE).
