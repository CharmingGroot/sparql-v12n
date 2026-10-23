# Differential test cases

Each case runs through this engine and through [Ontop](https://ontop-vkg.org/)
over the same mapping and data, and the results must be the same bag of
solutions. `crates/v12n-difftest` turns every query into one test, and
`testenv/difftest.sh` starts PostgreSQL and one Ontop endpoint per case.

Every directory with a `mapping.ttl` under `examples/` or `tests/diff/` is a
case. Cases here are named `<number>-<description>`.

```text
0001-mdr/
  schema.sql      tables, loaded first
  data.sql        rows
  mapping.ttl     R2RML mapping, used by both engines
  queries/*.rq    one test per query
  expect.toml     optional: queries that are not expected to match
```

## Comparison

- Solutions are compared as a bag: order is ignored unless the query has a
  top-level `ORDER BY`, and duplicates count.
- Terms are compared as RDF terms: kind, lexical form, datatype and
  language tag. `"4"^^xsd:integer`, `"04"^^xsd:integer` and `"4"` all
  differ. A literal written with `xsd:string` and one without a datatype are
  the same term (RDF 1.1). Values are never normalized.
- An unbound variable differs from every bound one.
- Variables are compared as a set; their order in `SELECT *` may differ.
- Results with blank nodes fail the check, since comparing them needs graph
  isomorphism.

A failing check prints the solutions only in Ontop, only in v12n, and pairs
that differ in some terms side by side.

## Expectations

```toml
# Outside the supported subset: v12n must fail with UnsupportedFeature
# (CLI exit code 1, HTTP 501). Ontop is not asked. Runs without a database.
[query.optional-criticality]
expect = "rejected"

# A known difference, tracked in an issue. Fails once the results match.
# Also covers v12n failing where Ontop answers.
[query.prices]
expect = "mismatch"
issue = 25

# Ontop fails on the query. v12n must return `solutions` solutions. Fails
# once Ontop answers, so the results get compared.
[query.date-invalid]
expect = "ontop-error"
issue = 26
solutions = 0
```

## Running

```sh
testenv/fetch-jdbc.sh    # once
testenv/difftest.sh      # up, test, down
```

`testenv/difftest.sh up`, `test` and `down` run the steps separately, for
example to rerun the tests while editing a case. Arguments after `test`
go to the test runner, such as a filter (`case_0001_mdr`) or
`--partition count:2/1` to shard.

Without `V12N_DIFF_DATABASE_URL` and `V12N_DIFF_ENDPOINTS`, `cargo test`
still runs the `rejected` checks and skips the comparisons.

## W3C RDB2RDF test cases

Not included yet. The [R2RML test cases](https://www.w3.org/TR/rdb2rdf-test-cases/)
fit this layout as one case per test: the test database's `create.sql`
holds both tables and rows (use it as `schema.sql` with an empty
`data.sql`), the test's mapping becomes `mapping.ttl`, and the queries
are one `SELECT` per predicate and class of the mapping, since the engine
does not support variable predicates yet. They are to be vendored with
their licence notice once the download is approved, as a subset covering
basic graph patterns on PostgreSQL; tests outside that scope are listed
with the reason they are skipped.
