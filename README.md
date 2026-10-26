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
base_iri = "http://data.example.com/"   # optional: for relative IRIs (see below)

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
| `[mapping] base_iri` | `V12N_BASE_IRI` | none: relative IRIs are data errors |
| `[limits] statement_timeout` | `V12N_STATEMENT_TIMEOUT` | `30s` |
| `[limits] row_limit` | `V12N_ROW_LIMIT` | `10000` |
| `[limits] max_query_bytes` | `V12N_MAX_QUERY_BYTES` | `65536` |
| `[limits] max_concurrent_requests` | `V12N_MAX_CONCURRENT_REQUESTS` | `16` |

`v12n query` reads the same `V12N_DATABASE_URL`, `V12N_ROW_LIMIT`,
`V12N_STATEMENT_TIMEOUT` and `V12N_BASE_IRI`, below its command-line options;
`v12n explain` reads `V12N_BASE_IRI`, and `--base-iri` also overrides the
setting for `serve`. `serve` ignores other
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

## Relative IRIs

A column or template with `rr:termType rr:IRI` can produce a relative IRI,
such as `Carlos` or `person/7`. R2RML resolves it against a base IRI given
to the processor (R2RML §4, §11.2), by plain string concatenation, not RFC
3986 resolution: with the base `http://data.example.com/`, `Carlos` becomes
`<http://data.example.com/Carlos>` and `../x` becomes
`<http://data.example.com/../x>`. Template values are percent-encoded
first (`Bob/Charles` gives `Bob%2FCharles`).

There is no default base IRI. Without `base_iri` (`--base-iri`,
`V12N_BASE_IRI`), a relative IRI is an R2RML data error that names the
setting, so the engine never invents IRIs for the data. Set it to the
namespace the data belongs to; it must be an absolute IRI.

A string counts as absolute when it starts with a scheme (`http:`,
`urn:`, ...); a fragment does not make it relative. A string with a
scheme that is not a valid IRI is a data error, where R2RML would first try
the base IRI in front of it; this only differs for values such as
`http://host:port/` with a non-numeric port, and it lets the SQL
comparisons below use the same rule as the results.

- A template whose text never yields a scheme, such as `{name}` or
  `person/{id}`, gets the base IRI in front of it when the mapping is
  loaded. A constant like `<http://data.example.com/person/7>` is then
  matched on the column as before (`id = $1`), with its index.
- A column can hold either form, so a constant matches both:
  `iri IN ($1, $2)` with `http://data.example.com/a` and `a`, which still
  uses an index on the column.
- A join on such a column, or on a template like `{scheme}:{rest}` whose
  scheme depends on the values, compares the generated IRIs in SQL
  (`CASE WHEN iri ~ '^scheme:' THEN iri ELSE $base || iri END`), which an
  index on the column does not serve.
- Duplicates are removed by column value, so a relative value and the
  absolute IRI it resolves to (`a` and `http://data.example.com/a`) still
  count twice, and `SELECT DISTINCT` over such a column is rejected as
  unsupported when a base IRI is set.

Ontop 5.5.0 puts `http://example.com/base/` (the base IRI of the W3C test
cases) in front of every template that does not start with an absolute
IRI, with no setting to change it, and fails on relative IRIs from columns.

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
instead of making PostgreSQL fail on input it cannot parse (inside an
`OPTIONAL`, only that part does not match); the query skips the database
when no branch is left. The comparison stays on the
column's own type, so its indexes are used. For column types without a
natural datatype (such as `interval`), results carry the column cast to
text, and constants are compared with that same text (`col::text = $1`),
which cannot use an index on the column.

Value comparisons across datatypes, such as `FILTER(?amount = 7.0)` on an
integer column, belong to `FILTER` (see below).

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

## Variables over columns of different types

The term maps of one pattern can read a variable from columns of
different SQL types, such as an `integer` column in one triples map and a
`text` column in another. `UNION` needs one type per result column, so
for queries over several term maps the engine also asks PostgreSQL for
the types of the selected columns and:

- keeps one result column where the types combine without changing any
  value (`int4` and `int8`, `text` and `varchar`);
- splits a literal of natural datatype into one result column per
  datatype; each branch fills its own and leaves the others `NULL`, so the
  integer 120 stays `"120"^^xsd:integer` and the text `'120'` stays
  `"120"`;
- selects the natural lexical form, computed in SQL as for joins, where
  two types can give the same term: template values (`item/{id}` over an
  `integer` key and a `text` key both give `item/1`), IRI columns,
  literals with `rr:datatype` or a language tag, and plain literals from
  `text` and `uuid` columns. The same triple from two term maps then
  still counts once;
- rejects the query as unsupported when that form cannot be computed in
  SQL (`float8`, `real` with `double precision`, date/time, `char(n)`).

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
tables (not views) through a single combination of term maps (in some
side of a `UNION`), the engine reads their unique keys from the catalog;
when every table has a key among the columns the solution fixes, no two
rows can give the same solution and the SQL keeps all rows without
`DISTINCT`. Otherwise
PostgreSQL has to deduplicate, which costs a hash or sort over the rows
and, unless an index delivers them in order, reads all of them before the
first one is returned. `v12n explain` has no database and always shows the
`DISTINCT`.

Duplicates are compared by column values, so two different values that a
template renders as the same string (`{a}-{b}` with `x-`, `y` and `x`,
`-y`) still count twice. A variable that is not selected is compared by
its columns' type and text output, so equal terms from columns of
different types also count twice where SQL cannot compute their lexical
form, or when the types combine (`varchar` and `text`) or the text differs
(`numeric` 1.0 and 1.00).

## OPTIONAL

`OPTIONAL` follows the SPARQL algebra's LeftJoin: every solution of the
left side is kept, and extended by each compatible solution of the
optional part where there are some. The SQL is a `LEFT JOIN` per optional
part:

- The part's tables are joined among themselves (in parentheses when
  there are several), and the `ON` clause holds both the part's own
  conditions and the link to the left side. A nested `OPTIONAL` is a
  `LEFT JOIN` inside the outer one, and patterns written after an
  `OPTIONAL` are joined after it.
- Shared variables are compared as RDF terms, as in joins, so typed
  columns and template decomposition work the same way and indexes are
  used where both sides are plain columns.
- An unbound variable is `NULL` in SQL and left out of the JSON binding.
  Where a shared variable may be unbound on one side (bound only by an
  earlier `OPTIONAL`, or by one nested in the part), it is compatible with
  any value: the condition is `(left IS NULL OR left = right)`, which no
  index on the column serves.
- Each part is translated on its own first, as the algebra evaluates it,
  so a condition that links a nested part to the outer left side is
  checked in the outer part's `ON` clause, never inside the nested one.
- A variable that only `OPTIONAL` parts bind gets columns from each of
  them, and its value is the first one bound in the row. A constant term
  of an optional part comes with a flag column that tells whether the
  part matched.
- Duplicate triples count once inside each part, as in any basic graph
  pattern.

Not supported yet, and rejected as unsupported: an optional part that
more than one combination of triples maps, or more than one side of a
`UNION` inside it, can match (it needs a `UNION` inside the `LEFT JOIN`);
`SELECT DISTINCT` over a variable that only several `OPTIONAL` parts bind.

## UNION

`{ A } UNION { B }` has the solutions of both sides as a bag: a solution
that both sides give counts twice, and once under `SELECT DISTINCT`. A
variable that only one side binds is unbound in the other side's
solutions.

- Joins, `FILTER`s and `OPTIONAL` distribute over a union on their left
  side, so the engine unfolds each arm, a choice of one side of every
  `UNION`, on its own, and combines the arms with `UNION ALL`. Each arm is
  translated as any other pattern: duplicate triples count once inside
  it (`DISTINCT`, or `UNION` across its term maps, or nothing when keys
  prove there are no duplicates, now checked per arm), and the arms are
  never compared with each other. The number of arms times the term map
  combinations of each is limited to 64, like the branches of one
  pattern.
- A `FILTER` in one side applies to that side's solutions only, and a
  `FILTER` of the group around the union to the solutions of both, where
  a variable of the other side is unbound.
- A `UNION` inside `OPTIONAL` does not distribute over the left join. It
  works where only one of its sides can match the left side, and is
  rejected otherwise (see `OPTIONAL`).

A variable can take its values from term maps of different shapes in
different sides or term maps: a template in one and a column or another
template in the other, constants, IRIs and literals, or literals of
different datatypes or language tags. The results rebuild each value
from the term map of the row's branch:

- Terms of shapes that can never be the same get separate result
  columns, and each branch fills its own: `http://example.org/customer/{id}`
  and `http://example.org/wholesale/{code}` (different fixed text, or text
  that IRI-safe encoding always escapes in the other's values), an IRI and
  a literal, two different constants, or two language tags. Removing
  duplicates on the columns stays exact.
- Shapes that can give the same term and whose duplicates are compared,
  in one arm or under `SELECT DISTINCT`, share one result column with the
  term's string, computed in SQL as for joins (the IRI-safe template, the
  IRI resolved against the base IRI, the natural lexical form), so the
  same triple from a template and from an IRI column counts once. Such a
  column cannot use an index and computes the string for every row. It is
  rejected as unsupported where SQL cannot compute the lexical form (as
  for joins), and for literals where one of the shapes is a column of
  natural datatype, whose datatype depends on the SQL type.

## FILTER

`FILTER` follows SPARQL's evaluation rules, errors included. An error is
neither true nor false: `error || true` is true, `error && false` is
false, and `!error` is still an error. The SQL uses `NULL` for an error,
which SQL's `AND`, `OR` and `NOT` treat the same way, and `WHERE` keeps a
row only where the filter is true. A comparison that SPARQL makes an
error is `NULL` in the SQL, never `false`, so `FILTER(!(?x = 5))` drops
the rows where `?x` is unbound or a string.

Where a filter applies:

- A `FILTER` in a group applies to the whole group, wherever it is
  written, and sees only the group's variables: in `{ ?s ex:name ?n {
  ?s ex:qty ?q FILTER(?n = "x") } }` the filter is an error, since `?n`
  is not bound inside the nested group.
- A `FILTER` inside `OPTIONAL` goes into the `ON` clause of the part's
  `LEFT JOIN`: it decides whether the part matches, never which rows of
  the left side are kept, and it sees the left side's variables. Where
  it is an error the part does not match, as SQL's `LEFT JOIN` does. A
  `FILTER` after the `OPTIONAL` is in `WHERE` and drops rows.
- An unbound variable is an error for every function except `BOUND`. A
  variable that only `OPTIONAL` parts bind is read from the part that
  matched (`CASE WHEN part matched THEN ... END`).

Comparisons follow the SPARQL operator mapping:

| Operands | `=`, `!=`, `<`, `<=`, `>`, `>=` |
|---|---|
| Numbers (`xsd:integer`, `xsd:decimal`, `xsd:double`) | By value, with XPath type promotion: `7.0` equals the integer 7, `-0` equals `0`, and NaN equals nothing. |
| Strings (simple literals, `xsd:string`) | By code point (`COLLATE "C"`, which needs a UTF-8 database). |
| `xsd:boolean` | `false` < `true`. |
| `xsd:dateTime` | By instant when both have a time zone or neither has one; comparing one with a time zone and one without depends on an implicit time zone and is rejected. |
| IRIs, blank nodes, and literals of other types | `=` and `!=` only, as RDF terms (RDFterm-equal); `<` and the others are an error. Two different literals are an error. |

A constant compared with a column is converted to the column's type
where that gives the same answer (`?qty > 5` is `qty > $1::int8`, `?qty =
7.0` is `qty = $1::int8`), so an index on the column can be used.
Otherwise the column is cast to the type XPath promotes to (`?qty < 5.5`
casts the integer column to `numeric`, `?qty < 5.5e0` to `float8`), which
no index on the column serves.

`sameTerm`, `BOUND`, `isIRI`, `isBlank`, `isLiteral`, `STR`, `LANG` and
`DATATYPE` are supported, and a term where a boolean is expected stands
for its effective boolean value (an empty string and zero are false).
`STR` and `DATATYPE` of a column work where its lexical form or natural
datatype is known in SQL, as for joins; `date` and `timestamp` infinities
are plain literals, so their `DATATYPE` is `xsd:string`.

`REGEX` translates the XPath regular expression into a PostgreSQL pattern
with the same matches, applied with `~` under `COLLATE "C"`:

| XPath | PostgreSQL pattern |
|---|---|
| `.` (without `s`) | any character but newline and carriage return, `[^\u000A\u000D]` |
| `^`, `$` | the start and end of the string, as PostgreSQL's defaults |
| `^`, `$` with `m` | at line boundaries, but not after a final newline (XPath 3.1), with lookaround |
| `i` | each character expanded to its case variants (`lower-case` or `upper-case` equal), so `i` also matches `ı` and `k` the Kelvin sign, whatever the database locale |
| `\s` | exactly space, tab, newline, carriage return |
| `x` | whitespace outside character classes removed |
| other characters | written as `\uXXXX` escapes unless ASCII letters or digits |

Rejected as unsupported: `\d`, `\w`, `\i`, `\c`, `\p{..}` and their
complements (Unicode classes that PostgreSQL only has through the locale),
back-references, character class subtraction, counts above 255, `\S`
inside a class, the `q` flag, and patterns or flags that are not
constants. An invalid pattern or flag is an error, as in XPath.

Rejected as unsupported rather than answered differently from engines
that extend SPARQL:

- ordering or comparing two literals of the same datatype outside the
  table, such as `xsd:date`, and two language-tagged literals except with
  `=` outside a negation;
- comparisons over `real` columns, values of a column whose `rr:datatype`
  is a numeric, boolean or dateTime datatype other than the column's
  natural one, `xsd:float` and the derived integer types, and dateTime
  constants outside the years 1 to 9999 or finer than a microsecond;
- `sameTerm` over `real` or `double precision` columns (PostgreSQL finds
  `-0` and `0` equal);
- other functions, arithmetic, `IN`, `EXISTS`, and expressions other
  than terms (variables, constants, and `STR`, `LANG` or `DATATYPE` of
  them) as operands;
- string ordering and `REGEX` in a database whose encoding is not UTF-8.

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
