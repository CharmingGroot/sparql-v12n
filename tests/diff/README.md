# Differential test cases

Each case runs through this engine and through [Ontop](https://ontop-vkg.org/)
over the same mapping and data, and the results must be the same bag of
solutions. `crates/v12n-difftest` turns every query into one test, and
`testenv/difftest.sh` starts PostgreSQL and one Ontop endpoint per case.

Every directory with a `mapping.ttl` or a `case.toml` under `examples/`,
`tests/diff/` or `tests/diff/w3c/` is a case. Cases here are named
`<number>-<description>`; W3C cases are named by their test id.

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
# (CLI exit code 1, HTTP 501), at planning or, for checks that need column
# types, at execution. Ontop is not asked.
[query.optional-criticality]
expect = "rejected"

# A known difference, tracked in an issue. Fails once the results match.
# Also covers v12n failing where Ontop answers.
[query.prices]
expect = "mismatch"
issue = 25

# Ontop fails on the query, or does not start with the mapping. v12n must
# return `solutions` solutions. Fails once Ontop answers, so the results
# get compared.
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

Every case needs its own Ontop JVM, so the script starts at most
`DIFF_BATCH` (8) at a time and `DIFF_SHARD=i/N` runs every Nth case, which
CI uses to split the cases over four jobs. `DIFF_CASES="0001-mdr shop"`
picks cases by name. `testenv/difftest.sh up`, `test` and `down` run the
steps separately for the selected cases, for example to rerun the tests
while editing a case.

Without `V12N_DIFF_DATABASE_URL` and `V12N_DIFF_ENDPOINTS`, `cargo test`
still runs the `rejected` checks and skips the comparisons.

## W3C RDB2RDF test cases

`tests/diff/w3c/` has one case per included R2RML test. A `case.toml`
replaces `schema.sql`, `data.sql` and `mapping.ttl` there, because the W3C
files are vendored unmodified in `tests/w3c/rdb2rdf/`:

```toml
sql = "../../../w3c/rdb2rdf/D001-1table1column1row/create.sql"
mapping = "../../../w3c/rdb2rdf/D001-1table1column1row/r2rmla.ttl"
base_iri = "http://example.com/base/"   # optional: v12n's base IRI (--base-iri)
```

`tests/diff/w3c/README.md` lists the included and skipped tests.
