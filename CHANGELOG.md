# Changelog

All notable changes are listed here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions
will follow [Semantic Versioning](https://semver.org/) once released.
Design decisions are recorded in the `Decision:` Discussions.

## [Unreleased]

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
