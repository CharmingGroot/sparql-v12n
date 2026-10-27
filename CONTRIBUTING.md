# Contributing

Thanks for looking into sparql-v12n. This guide covers how changes are
proposed, what CI checks, and how to test a change locally.

## What the project is

The engine does one thing: given an R2RML mapping, a SPARQL query and a
PostgreSQL database, it answers the query by translating it to SQL. It
stores, loads and caches nothing. A few rules follow from that and from
the correctness goal:

- **Answers follow the specifications.** SPARQL 1.1 Query, R2RML and XML
  Schema 1.1 decide what a correct answer is. Where engines differ, the
  choice and its reasons are written down in a Discussion titled
  `Decision: …` (category Announcements) before or with the change.
- **Unsupported means rejected.** A query or mapping outside the supported
  subset fails with an `UnsupportedFeature` error (CLI exit code 1, HTTP
  501). It is never answered differently from what the specification says.
- **Constants are bound parameters.** No value from a query is written
  into SQL text; identifiers come only from the mapping and are quoted.
- **Every change keeps the results checked.** Query results are compared
  with [Ontop](https://ontop-vkg.org/) and the W3C R2RML test cases (see
  [tests/diff/README.md](tests/diff/README.md)).

## Proposing a change

1. **Open an issue first**, for bugs and features alike, so the scope can
   be agreed before code is written. For a design question with several
   reasonable answers, say which options you see.
2. **Branch** as `<type>/<issue>-<short-name>`, for example
   `fix/37-time-midnight`.
3. **Open a pull request** for that one issue, with `Closes #N` (or
   `Refs #N`) in the body, and fill in the template. Draft pull requests
   are welcome for early feedback.
4. Pull requests are **squash merged** by a maintainer. The pull request
   title becomes the commit on `main`.

Issues, pull requests, commit messages and code comments are written in
English. A translation below a `---` line is welcome.

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

A new behavior usually comes with a case in `tests/diff/`: a schema, data,
a mapping and one `.rq` file per query. When v12n deliberately differs
from Ontop, or rejects a query, record it in the case's `expect.toml`
with the issue that explains it, and add it to the tables in
[tests/diff/README.md](tests/diff/README.md).

### SQL snapshots

Generated SQL is pinned with [insta](https://insta.rs/) snapshots
(`snapshots/` directories under `crates/`). When a change alters the SQL, review the new
snapshots (`cargo insta review`, or run the tests with
`INSTA_UPDATE=always` and read the diff) and commit them with the change.

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

Please do not open a public issue for a vulnerability, for example SQL
built from query input or a way around the resource limits. Use GitHub's
private vulnerability reporting on this repository instead.

## License

By contributing, you agree that your contributions are licensed under the
Apache License 2.0, as the rest of the project ([LICENSE](LICENSE)).
