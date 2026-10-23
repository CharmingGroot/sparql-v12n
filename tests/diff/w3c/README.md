# W3C R2RML test cases

One case per R2RML test of the [W3C RDB2RDF test cases](https://www.w3.org/TR/rdb2rdf-test-cases/).
The W3C files are vendored unmodified in `tests/w3c/rdb2rdf/` (see its
README for the licence); each case here has a `case.toml` that points at the
test's `create.sql` and mapping, and our own queries.

The tests define expected output as a whole graph (`mapped*.nq`). The engine
does not support variable predicates and the harness does not compare blank
nodes, so each query asks for one predicate or class. The queries were
derived from the triples in `mapped*.nq`: `SELECT ?s ?o` per predicate and
`SELECT ?s` per class, leaving out variables bound to blank nodes. As
everywhere, the results are compared with Ontop, not with `mapped*.nq`.

Ontop runs with its default settings. It needs the `pgcrypto` extension for
blank nodes on PostgreSQL, which `testenv/difftest.sh` creates in every case
database.

## Included: 34 of 63 R2RML tests

R2RMLTC0000, 0001a, 0001b, 0002a, 0002b, 0002d, 0002i, 0002j, 0003b, 0003c,
0004a, 0005a, 0005b, 0007a, 0007c, 0007d, 0008b, 0008c, 0009a, 0009c, 0009d,
0010a, 0010b, 0010c, 0011a, 0011b, 0012a, 0012b, 0012e, 0013a, 0014d, 0015a,
0018a, 0020a. Known differences are in each case's `expect.toml`.

## Skipped: 29

| Tests | Reason |
|---|---|
| R2RMLTC0002c, 0002e, 0002f, 0002g, 0002h, 0003a, 0004b, 0007h, 0012c, 0012d, 0015b, 0019b, 0020b | The test expects the mapping to be rejected (no expected output), so there are no answers to compare. |
| R2RMLTC0006a, 0007b, 0007e, 0007f, 0007g, 0008a, 0009b | Named graphs (`rr:graph`, `rr:graphMap`): not supported, and `GRAPH` queries are outside the supported subset. |
| R2RMLTC0014a, 0014b, 0014c | `rr:inverseExpression`: not supported. |
| R2RMLTC0016a, 0016b, 0016c, 0016d, 0016e | `create.sql` uses `VARBINARY`, which PostgreSQL rejects. |
| R2RMLTC0019a | Relative IRI from a column: Ontop fails (HTTP 500) and v12n has no base IRI (#33), so neither answers. |
