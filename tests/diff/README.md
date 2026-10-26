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
- With `ORDER BY` over projected variables, each result must be in the
  order SPARQL 1.1 §15.1 defines (unbound, blank nodes, IRIs by code point,
  literals by `<`). Where it leaves the order open, ties and values that
  `<` does not order, any order passes. With other comparators, whose
  values the results do not show, the sequences must be equal, so such
  queries should have no ties.
- With `LIMIT` or `OFFSET`, solutions only one engine returned must be
  unordered with respect to those only the other returned, as where a
  slice cuts through a tie. Without `ORDER BY`, any solutions may be cut
  off and only their number is compared.
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

## Rejected queries: outside the MVP

The MVP is SPARQL `SELECT` with basic graph patterns, `FILTER`,
`OPTIONAL`, `UNION`, `ORDER BY`, `LIMIT` and `OFFSET`. Each query recorded
as `rejected` uses a form outside it, which v12n rejects rather than
answer differently from SPARQL 1.1 or from engines that extend it:

| Query | Why |
|---|---|
| `0006-filter/date-order` | `<` on `xsd:date`: no row of the operator mapping (SPARQL 1.1 §17.3), so a type error, which engines that extend SPARQL (§17.3.1) turn into a comparison of values (Discussion #57). |
| `0006-filter/language-tag-not-equal` | `!=` on two different language-tagged literals: an error by RDFterm-equal, false where values are compared, and the negation tells the two apart (Discussion #57). |
| `0006-filter/datetime-zoned-and-local` | dateTimes with and without a time zone: their order depends on an implicit time zone that SPARQL leaves to the implementation (Discussion #57). |
| `0006-filter/regex-digit-class` | `\d` is every Unicode decimal digit in XPath, which PostgreSQL has only through the database locale (Discussion #57). |
| `0007-union/shapes-natural-and-template-literal` | A variable from a column of natural datatype and a literal template: removing duplicates would compare their datatypes, which depend on the column's SQL type (Discussion #60). |
| `0008-order/distinct-over-hidden` | `ORDER BY` over a variable that `SELECT DISTINCT` does not project: one solution stands for several that the order puts at different places (Discussion #63). |
| `w3c/R2RMLTC0012e/type-ious` | A self-join on blank nodes from a template over a `float8` column: no SQL computes the canonical `xsd:double` form (Discussion #35). Removing self-joins on keys (#28, V6) would make the comparison unnecessary. |

## Known differences from Ontop

| Queries | Issue | Difference |
|---|---|---|
| `0002-constants/prices`, `price-constant`, `price-integral-constant` | #25 | Ontop keeps the scale of `numeric` values (`12.50`); v12n writes XSD 1.1 canonical forms (`12.5`), as decided in Discussion #35. |
| `w3c/R2RMLTC0005a/owes`, `R2RMLTC0005b/amount`, `R2RMLTC0005b/type-ious`, `R2RMLTC0012a/amount`, `R2RMLTC0012e/amount`, `0009-literals/double-canonical` | #25 | Ontop writes an `xsd:double` as PostgreSQL's text output (`30`) and matches constants against it; v12n writes and matches the canonical form (`3.0E1`). |
| `0002-constants/uuid-uppercase` | #26 | Ontop matches a `uuid` constant by value; the terms differ in case. |
| `0006-filter/string-order-code-points`, `0008-order/string-accent`, `0008-order/string-hangul` | #26 | Ontop orders strings by the database collation; SPARQL orders them by code point. |
| `0002-constants/date-non-canonical`, `date-invalid` (Ontop error) | #26 | Ontop refuses ill-typed `xsd:date` constants, which match nothing. |
| `w3c/R2RMLTC0009c/name`, `R2RMLTC0009d/name`, `R2RMLTC0009d/numsport`, `R2RMLTC0019a/name` (Ontop error) | #26 | Ontop does not start with a computed `rr:sqlQuery` column, and does not resolve relative IRIs from columns. |

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
