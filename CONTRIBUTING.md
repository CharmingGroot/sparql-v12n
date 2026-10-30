# Contributing

How to propose a change, what CI checks, and how to test locally.

## Scope and rules

sparql-v12n answers a SPARQL query over an R2RML mapping by translating it
to SQL and running it on PostgreSQL. It does not store, load or cache
data. Changes keep to these rules:

- SPARQL 1.1 Query, R2RML and XML Schema 1.1 define the correct answer.
  Where engines differ, the choice and its reasons go into a Discussion
  titled `Decision: …` (category Announcements), before or with the
  change.
- A query or mapping outside the supported subset fails with an
  `UnsupportedFeature` error (CLI exit code 1, HTTP 501) instead of an
  answer that differs from the specification.
- Values from a query reach PostgreSQL only as bound parameters.
  Identifiers come only from the mapping and are quoted.
- Query results are compared with [Ontop](https://ontop-vkg.org/) and the
  W3C R2RML test cases ([tests/diff/README.md](tests/diff/README.md)).

## Proposing a change

1. Open an issue first, for bugs and features alike, to agree on the
   scope before writing code. For a design question, list the options
   you considered.
2. Name the branch `<type>/<issue>-<short-name>`, for example
   `fix/37-time-midnight`.
3. Open one pull request per issue, with `Closes #N` (or `Refs #N`) in
   the body, and fill in the template. Open it as a draft to get early
   feedback.
4. A maintainer squash merges the pull request; its title becomes the
   commit on `main`.

Write issues, pull requests, commit messages and code comments in
English. A translation may follow below a `---` line.

## Translations

The README and the documents in `docs/` have Korean (`.ko.md`) and
Chinese (`.zh-CN.md`) translations. The English document is the source,
and each translation names the commit of the English text it follows.

A change to supported features, examples or limits in an English
document updates the translations in the same pull request, or lists the
translations left to update and the issue that tracks them. A pull
request that changes only translations does not need the PostgreSQL or
differential tests.

## Commit and title convention

CI checks the pull request title, the branch name and the sign-off.

```text
<type>(<scope>)!: <subject>
```

- `type`: `feat`, `fix`, `perf`, `refactor`, `test`, `docs`, `build`,
  `ci`, `chore`, `revert`
- `scope` (optional): `core`, `postgres`, `server`, `cli`, `mapping`,
  `sparql`, `unfold`, `optimize`, `sql`, `deps`, `release`
- `subject`: English, imperative, starting in lower case, no period at
  the end; the whole title at most 72 characters
- `!` after the type or scope marks a breaking change

The commit body, if any, says why the change is made.

### Sign-off (DCO)

Every commit carries a `Signed-off-by` line matching its author, which
certifies the [Developer Certificate of Origin](https://developercertificate.org/):

```sh
git commit -s
```

## Testing

Before opening a pull request:

```sh
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

CI also checks the minimum supported Rust version (1.88) and the
dependency policy (`cargo deny check`, see [deny.toml](deny.toml)).

### PostgreSQL tests

Tests that need a database are skipped unless `V12N_TEST_DATABASE_URL` is
set. [testenv/README.md](testenv/README.md) starts a local PostgreSQL with
development credentials:

```sh
cd testenv && docker compose up -d postgres && cd ..
V12N_TEST_DATABASE_URL=postgresql://v12n:v12n-dev@127.0.0.1:55432/shop cargo test --workspace
```

### Differential tests against Ontop

Run them when a change can alter query results:

```sh
testenv/fetch-jdbc.sh    # once: the PostgreSQL JDBC driver for Ontop
testenv/difftest.sh      # every case; DIFF_CASES="0011-time" picks cases
```

A change in behavior needs a case in `tests/diff/`: a schema, data, a
mapping and one `.rq` file per query. When v12n deliberately differs
from Ontop, or rejects a query, record it in the case's `expect.toml`
with the issue that explains it, and add it to the tables in
[tests/diff/README.md](tests/diff/README.md).

### SQL snapshots

Generated SQL is pinned with [insta](https://insta.rs/) snapshots in the
`snapshots/` directories under `crates/`. When a change alters the SQL,
review the new snapshots (`cargo insta review`, or run the tests with
`INSTA_UPDATE=always` and read the diff) and commit them with the
change.

## Code

- `v12n-core` has no database or async runtime dependency; PostgreSQL code
  lives in `v12n-postgres`.
- No `unsafe`. Each crate has one `Error` enum.
- Add a dependency only with a reason, and it must pass `cargo deny`.
- Comments say why, not what: the specification section a rule comes
  from, an invariant, or the reason for a choice. Public items get a short
  `///` comment.
- New source files start with `// SPDX-License-Identifier: Apache-2.0`.

## Reporting a security issue

Report vulnerabilities privately, as [SECURITY.md](SECURITY.md)
describes, not in a public issue.

## License

Contributions are licensed under the Apache License 2.0, like the rest
of the project ([LICENSE](LICENSE)).
