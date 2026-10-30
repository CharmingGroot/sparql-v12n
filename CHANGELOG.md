# Changelog

All notable changes are listed here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions
follow [Semantic Versioning](https://semver.org/). Before 1.0, any release
may change behavior; such changes are marked **Breaking**.
Design decisions are recorded in the `Decision:` Discussions.

## [Unreleased]

### Added

- `BIND` and `SELECT (expr AS ?v)` over variables, constants and arithmetic
  on integers and decimals. An error leaves the variable unbound.
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
