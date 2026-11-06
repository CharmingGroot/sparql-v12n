# Changelog

All notable changes are listed here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions
follow [Semantic Versioning](https://semver.org/). Before 1.0, any release
may change behavior; such changes are marked **Breaking**.
Design decisions are recorded in the `Decision:` Discussions.

## [Unreleased]

### Added

- `YEAR`, `MONTH`, `DAY`, `HOURS`, `MINUTES`, `SECONDS`, `TIMEZONE`, `TZ`
  and `NOW` in `FILTER`, and `YEAR` to `SECONDS` also in `BIND` and
  `SELECT (expr AS ?v)`. They read `xsd:dateTime` values from `timestamp`
  and `timestamptz` columns, constants and `NOW`, and agree with the
  lexical forms the engine writes: a `timestamptz` gives its components
  in UTC and the time zone `Z`, and a `timestamp` has no time zone.
  `NOW` is one value per query, sent as a parameter. The functions are
  rejected over `xsd:date` and `xsd:time` values and in `ORDER BY`.
- `ABS`, `ROUND`, `CEIL` and `FLOOR` in `FILTER`, `BIND` and
  `SELECT (expr AS ?v)`, with a result of the argument's datatype.
  `ROUND` rounds halves towards positive infinity, as `fn:round` does,
  for decimals and doubles. `RAND` is rejected as unsupported.
- `STRLEN`, `SUBSTR`, `UCASE`, `LCASE`, `STRSTARTS`, `STRENDS`, `CONTAINS`
  and `CONCAT` in `FILTER`, and `STRLEN` in `BIND` and `SELECT (expr AS
  ?v)`, with the argument compatibility and language tag rules of SPARQL
  1.1 §17.4.3. Lengths and positions count code points, and `UCASE` and
  `LCASE` use Unicode's full case mappings through the ICU root locale,
  whatever the database locale. `STRSTARTS`, `STRENDS` and `CONTAINS` with
  a constant are `LIKE` with escaped wildcards under `COLLATE "C"`, which
  an index on the column under that collation can serve.
- `ASK` queries. The SQL reads at most one row and removes no duplicates.
  The answer is the JSON or XML boolean result from the endpoint and from
  `v12n query`, which prints `true` or `false` in the table format and
  exits with 0 for both. CSV and TSV, which define no boolean result, are
  refused (exit code 1, HTTP 406), and so are `LIMIT` and `OFFSET` in `ASK`.
- `v12n check` warns about triples maps over different logical tables
  whose subject templates can produce the same IRI. Each warning is a
  `warning: ...` line on stderr and an entry of `warnings` in JSON, and
  the exit code stays 0.
- `IF`, `COALESCE`, `IN` and `NOT IN` in `FILTER`, with the error rules of
  SPARQL 1.1 §17.4.1, and `IF` and `COALESCE` over numbers in `BIND` and
  `SELECT (expr AS ?v)`. `IN` over constants keeps a comparison of the
  column that an index can serve.
- `BIND` and `SELECT (expr AS ?v)` over variables, constants and arithmetic
  on integers and decimals. An error leaves the variable unbound.
- `VALUES` inside a group and after the query, with `UNDEF` (#113). Each
  row becomes an alternative of a disjunction whose constants are compared
  with their variables' columns and checked like triple pattern constants.
  The `VALUES` blocks of a query hold at most 1000 values; a larger query
  fails with `Error::TooManyValues`, HTTP 400.
- `MINUS` (#114), as an anti-join on the shared variables compared as RDF
  terms. A solution of the right side removes a left solution only when
  the two are compatible and share a variable that both bind, so
  variables that `OPTIONAL` leaves unbound on either side follow
  SPARQL 1.1 §18.5. `MINUS` inside `OPTIONAL`, `UNION` or another `MINUS`,
  and `BIND` or `VALUES` inside `MINUS`, are rejected.
- Arithmetic on `xsd:double` values in `FILTER`, `BIND` and `SELECT`, with
  IEEE 754 results: infinities and signed zeros where PostgreSQL's
  `float8` operators raise errors, and NaN neither equal to nor ordered
  with anything.
- Arithmetic (`+`, `-`, `*`, `/`) on integers and decimals in `FILTER`,
  with XPath's type promotion and SPARQL's error semantics.
- SPARQL Query Results XML, CSV and TSV from the endpoint, chosen by
  `Accept`, and from `v12n query --format`.
- TLS for database connections through rustls: `sslmode` (`disable`,
  `prefer`, `require`, `verify-ca`, `verify-full`) and `sslrootcert` in
  the database URL, with libpq's meaning and its default `prefer`.
  Certificates are checked against the system's root certificates or a CA
  file.
- Client certificates (`sslcert`, `sslkey`), `sslmode=allow`, and
  certificate revocation lists (`sslcrl`, `sslcrldir`) in the database
  URL, with libpq's meaning. libpq's `PG*` environment variables are not
  read.
- `v12n check --db URL` also checks that every table and column the
  mapping reads exists and can be read, and exits with 2 listing what the
  database lacks.

### Changed

- An `OPTIONAL` part that can only match the row of a table on its left,
  joined on a `NOT NULL` unique key, reads that row's columns instead of
  a `LEFT JOIN` of the table with itself; they are `NULL` where the
  part's other conditions do not hold.
- The generated SQL leaves out `IS NOT NULL` on term columns that a
  `NOT NULL` constraint of a table keeps from being `NULL`, except where a
  `LEFT JOIN` can make them `NULL`.
- A scan of a referenced table is removed when a validated foreign key,
  to a unique key of the same column types and under deterministic
  collations, joins it and the query reads only the key columns from it, as for the
  parent of a referencing object map whose subject the query needs.
  The terms are built from the referencing columns (#112). A
  referencing column that may be `NULL` is required not to be, as the
  join required (#198).
- Keys, column types and resolved column names come from a catalog
  snapshot instead of being read on every query (Discussion #187).
  `v12n serve` reads it at startup; each query checks a fingerprint of
  the catalog entries of its tables in the round trip that prepares the
  statement, and reads them again when the database changed. A query
  whose catalog changed again while it was planned anew gets HTTP 503.
- A table or column that the mapping reads and the database lacks fails
  the query with an error that names it, before the statement is
  prepared.

### Fixed

- An `xsd:decimal` compared with an `xsd:double` becomes the nearest
  double without raising an error beyond the range of doubles, and a
  `numeric` NaN or infinity is an error there, as in other comparisons.

## [0.0.1] - 2026-10-28

First public version: a SPARQL-to-SQL virtualization engine over R2RML
mappings and PostgreSQL. See the README for the supported subset.

### Added

- R2RML mappings: parsing, validation (`v12n check`), the classes and
  properties a mapping exposes (`v12n terms`), and relative IRIs resolved
  against a configured base IRI.
- SPARQL `SELECT` with basic graph patterns, `OPTIONAL`, `UNION`,
  `FILTER` with SPARQL's error semantics, `DISTINCT`, `ORDER BY`, `LIMIT`
  and `OFFSET`, unfolded into one SQL query.
- RDF 1.1 term equality in constants, joins and duplicate removal, and
  XSD 1.1 canonical lexical forms in results.
- `v12n explain` and `v12n query` on the command line, with JSON, CSV and
  table output and exit codes for each kind of failure.
- `v12n serve`: a SPARQL 1.1 Protocol endpoint that streams JSON results.
- Bound parameters for every constant, a read-only session, a statement
  timeout, a row limit, a query size limit and a limit on concurrent
  requests, on by default.
- Differential tests against Ontop and the W3C R2RML test cases.
- Release binaries for Linux (x86_64 and aarch64), macOS (Apple silicon)
  and Windows (x86_64), with SHA-256 checksums and build provenance
  attestations; `v12n --version` names the commit of the build.

[Unreleased]: https://github.com/CharmingGroot/sparql-v12n/compare/v0.0.1...HEAD
[0.0.1]: https://github.com/CharmingGroot/sparql-v12n/releases/tag/v0.0.1
