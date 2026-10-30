# sparql-v12n

English | [한국어](README.ko.md) | [简体中文](README.zh-CN.md)

<p align="center"><b>A virtual knowledge graph engine written in Rust. It translates SPARQL queries into SQL through R2RML mappings and runs them on the source database, without copying the data.</b></p>

<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="docs/images/how-it-works-dark.svg">
    <img alt="A caller submits a SPARQL query to v12n. The R2RML mapping guides query planning and RDF term construction, and database keys and column types inform SQL generation. PostgreSQL runs one SQL query that retrieves the results and returns rows. v12n decodes the rows, builds RDF terms and streams the SELECT bindings as JSON, XML, CSV or TSV. The source data stays in PostgreSQL; loading metadata and preparing statements may take additional database round trips." src="docs/images/how-it-works-light.svg" width="760">
  </picture>
</p>

- Each SPARQL query becomes one SQL query, which runs on the database that
  holds the data. The data is not copied.
- PostgreSQL is supported today. Other databases are on the
  [roadmap](#roadmap).
- The R2RML mapping decides which tables and columns become which classes and
  properties. Data outside the mapping cannot be queried.
- There is no ontology reasoning yet. A query for a class finds only what the
  mapping states for that class. See the [roadmap](#roadmap).
- One binary, `v12n`, provides both a command line tool and a SPARQL 1.1
  Protocol HTTP endpoint.

> Status: experimental. The supported subset below is tested against Ontop and
> the W3C R2RML test cases. Commands, output and APIs may change before 1.0.

## Purpose

v12n queries data that stays in a relational database as an RDF graph,
without loading it into a separate RDF store. The R2RML mapping defines
the graph. Each query runs as SQL on the source tables, and only the rows
of its result are turned into RDF terms. A performance goal of the
project is to convert and return large `SELECT` results efficiently, from
the database rows to the serialized response.

The engine is worth evaluating when the data is in PostgreSQL, the
queries fit the [supported subset](#supported-sparql), and the classes
and properties that the mapping states are enough without ontology
reasoning. It is experimental and supports PostgreSQL only.
[Performance](#performance) links the benchmark against Ontop.

## Install

Download the archive for your platform from the
[releases](https://github.com/CharmingGroot/sparql-v12n/releases) page and
put `v12n` on your `PATH`. Archives are built for Linux (x86_64 and aarch64,
static binaries), macOS (Apple silicon) and Windows (x86_64).

Each release has a `SHA256SUMS` file and a build provenance attestation.
Check a download with either of these commands (on macOS, use
`shasum -a 256 -c` for the first):

```sh
sha256sum --ignore-missing -c SHA256SUMS
gh attestation verify v12n-<version>-<target>.tar.gz -R CharmingGroot/sparql-v12n
```

To build from source with Rust 1.88 or later:

```sh
cargo install --locked --git https://github.com/CharmingGroot/sparql-v12n v12n-cli
```

`v12n --version` prints the version and the commit it was built from.

## Quickstart

This runs the shop example in `examples/shop` against a local PostgreSQL
in Docker. It needs Docker with Compose v2 and the `v12n` binary (see
[Install](#install)), and runs from the root of a clone of this repository. With a
build from source, use `./target/release/v12n` in place of `v12n`.

```sh
git clone https://github.com/CharmingGroot/sparql-v12n.git
cd sparql-v12n
```

1. Start PostgreSQL with the example schema and data. Only the `postgres`
   service of `testenv` starts. It listens on `127.0.0.1:55432` with local
   development credentials.

   ```sh
   docker compose -f testenv/docker-compose.yml up -d --wait postgres
   ```

2. Check the mapping.

   ```sh
   v12n check examples/shop/mapping.ttl --format table
   ```

   ```text
   ok: 2 triples maps, 2 classes, 4 properties
   ```

3. Run the query for the orders that VIP customers placed.

   ```sh
   v12n query examples/shop/mapping.ttl examples/shop/queries/vip-orders.rq \
     --db postgresql://v12n:v12n-dev@127.0.0.1:55432/shop --format table
   ```

   ```text
   ?order                           ?amount
   <http://example.org/order/1001>  "120000"^^<http://www.w3.org/2001/XMLSchema#integer>
   <http://example.org/order/1003>  "87000"^^<http://www.w3.org/2001/XMLSchema#integer>
   <http://example.org/order/1004>  "15000"^^<http://www.w3.org/2001/XMLSchema#integer>
   ```

   [How the result is built](#how-the-result-is-built) shows why these
   three orders are returned.

4. Stop the database and remove its container and network.

   ```sh
   docker compose -f testenv/docker-compose.yml down -v
   ```

[testenv/README.md](testenv/README.md) describes the
test environment, including the Ontop endpoint that the differential tests
use and that this example does not need.

### How the result is built

The example database has two tables. All their rows are shown below; the
full files are [schema.sql](examples/shop/schema.sql) and
[data.sql](examples/shop/data.sql).

`customers`

| id | name | tier |
|---|---|---|
| 7 | Kim | VIP |
| 8 | Lee | REGULAR |
| 9 | Park | VIP |

`orders`

| id | customer_id | amount |
|---|---|---|
| 1001 | 7 | 120000 |
| 1002 | 8 | 30000 |
| 1003 | 9 | 87000 |
| 1004 | 7 | 15000 |

The R2RML mapping turns each row into an IRI with properties. The excerpt
below leaves out the prefixes, `a rr:TriplesMap` and `ex:name`; the full
file is [mapping.ttl](examples/shop/mapping.ttl).

```turtle
<#CustomerMap>
  rr:logicalTable [ rr:tableName "customers" ] ;
  rr:subjectMap [ rr:template "http://example.org/customer/{id}" ; rr:class ex:Customer ] ;
  rr:predicateObjectMap [ rr:predicate ex:tier ; rr:objectMap [ rr:column "tier" ] ] .

<#OrderMap>
  rr:logicalTable [ rr:tableName "orders" ] ;
  rr:subjectMap [ rr:template "http://example.org/order/{id}" ; rr:class ex:Order ] ;
  rr:predicateObjectMap [ rr:predicate ex:amount ; rr:objectMap [ rr:column "amount" ] ] ;
  rr:predicateObjectMap [
    rr:predicate ex:placedBy ;
    rr:objectMap [ rr:template "http://example.org/customer/{customer_id}" ]
  ] .
```

- The customer row with `id` 7 becomes `<http://example.org/customer/7>`,
  an `ex:Customer` with `ex:tier "VIP"`.
- The order row with `id` 1001 becomes `<http://example.org/order/1001>`,
  an `ex:Order` with `ex:amount 120000`. The `integer` column gives an
  `xsd:integer` literal.
- `ex:placedBy` builds a customer IRI from `customer_id` with the same
  template as the customer subject. Order 1001 has `customer_id` 7, so it
  points to `<http://example.org/customer/7>`, the IRI of customer row 7.

The query [vip-orders.rq](examples/shop/queries/vip-orders.rq):

```sparql
PREFIX ex: <http://example.org/>

# Orders placed by VIP customers.
SELECT ?order ?amount WHERE {
  ?customer a ex:Customer ; ex:tier "VIP" .
  ?order ex:placedBy ?customer ; ex:amount ?amount .
}
```

`?customer` appears in both patterns, so an order matches only when its
`ex:placedBy` IRI is the IRI of a customer whose tier is `"VIP"`. In SQL
this is a join on `orders.customer_id = customers.id`. Customers 7 and 9
are VIP: customer 7 placed orders 1001 and 1004, and customer 9 placed
order 1003. Order 1002 belongs to customer 8, whose tier is `"REGULAR"`.

`v12n explain` prints the generated SQL without a database. The join
appears as `JOIN "orders" t1 ON t1."customer_id" = t0."id"`, and `"VIP"`
is passed as the parameter `$1`.

```sh
v12n explain examples/shop/mapping.ttl examples/shop/queries/vip-orders.rq
```

## Supported SPARQL

The engine answers `SELECT` queries over one R2RML mapping and one
PostgreSQL database. The table lists what is supported and what is rejected.
A rejected query, or any form not listed, fails with an error (`v12n query`
exits with 1, the HTTP endpoint answers 501). It never returns a different
answer. Each supported feature is compared with
[Ontop](https://ontop-vkg.org/) in the differential tests (`tests/diff`).

| Feature | Notes | Rejected |
|---|---|---|
| Basic graph patterns | Constant predicates and `rdf:type`; constants and shared variables match RDF terms, across column types; a triple counts once however many rows give it | Variable predicates, property paths, `GRAPH`, `FROM` and `FROM NAMED` |
| `SELECT`, `SELECT *`, `DISTINCT` | Bag semantics over the solutions | `REDUCED`, subqueries, `GROUP BY` and aggregates |
| `BIND`, `SELECT (expr AS ?v)` | A variable, a constant, or `+`, `-`, `*`, `/` on integers and decimals; an error leaves the variable unbound | Other expressions; `BIND` inside `OPTIONAL` or `UNION`; a computed variable used in a triple pattern, `FILTER`, `ORDER BY` or another expression |
| `OPTIONAL` | Nested and sequential, `FILTER` inside, unbound variables | `SELECT DISTINCT` over a variable that only several `OPTIONAL` parts bind; a `FILTER` inside `OPTIONAL` around a `UNION` with an empty side |
| `UNION` | Bag union of its sides, inside `OPTIONAL` too; term maps of different shapes for one variable | Removing duplicates across a column of natural datatype and another literal shape of the same variable |
| `FILTER` | `=`, `!=`, `<`, `<=`, `>`, `>=`, `&&`, `\|\|`, `!`, `BOUND`, `sameTerm`, `isIRI`, `isBlank`, `isLiteral`, `STR`, `LANG`, `DATATYPE`, `REGEX`, `+`, `-`, `*`, `/` on integers and decimals, with SPARQL's error semantics | Other functions, arithmetic on `xsd:double`, `IN`, `EXISTS`; comparisons SPARQL 1.1 leaves to extensions (ordering `xsd:date`, `!=` on language-tagged literals, dateTimes with and without a time zone); comparisons over `real` columns; `REGEX` with `\d`, `\w`, `\p{..}` and back-references |
| `ORDER BY`, `LIMIT`, `OFFSET` | SPARQL 1.1 order, a fixed order where it leaves one open | `ORDER BY` over a variable that `SELECT DISTINCT` does not project |
| Literals | RDF 1.1 term equality: language tags in lower case, `xsd:string` is the simple literal, XSD 1.1 canonical lexical forms | `xsd:float` and derived integer types compared by value in `FILTER` |
| Other forms | No RDFS inference: `rdf:type` matches the classes the mapping states | `ASK`, `CONSTRUCT`, `DESCRIBE`, `MINUS`, `VALUES`, `SERVICE` |

[docs/semantics.md](docs/semantics.md) gives the rules behind the table:
[constants](docs/semantics.md#constants-in-a-query), [joins](docs/semantics.md#variables-shared-across-columns), [duplicates](docs/semantics.md#duplicate-triples),
[`OPTIONAL`](docs/semantics.md#optional), [`UNION`](docs/semantics.md#union), [`FILTER`](docs/semantics.md#filter),
[`BIND` and `SELECT` expressions](docs/semantics.md#bind-and-select-expressions),
[ordering](docs/semantics.md#order-by-limit-and-offset) and [literals](docs/semantics.md#literals). `tests/diff/README.md` lists
the known differences from Ontop and why each rejected test query is
rejected.

## Usage

| Command | Use |
|---|---|
| `v12n check` | Validate a mapping |
| `v12n terms` | List the classes and properties a mapping exposes |
| `v12n explain` | Show the SQL of a query without a database |
| `v12n query` | Run a query and print its results |
| `v12n serve` | Serve the SPARQL 1.1 Protocol at `/sparql` |

Limits are on by default: a read-only database session, a statement timeout
of 30 s and a limit of 10000 solutions. The HTTP endpoint also limits a
query to 64 KiB and serves at most 16 requests at a time.

[docs/usage.md](docs/usage.md) describes each command with its output
formats and exit codes, and the HTTP endpoint with its settings.

## Performance

[bench/README.md](bench/README.md) describes the benchmark against Ontop
on the same PostgreSQL, mapping and queries, and
[bench/RESULTS.md](bench/RESULTS.md) reports the results and their
analysis. A timing counts only when the engine returns the same solutions
as Ontop.

The figures below come from one benchmark session on 2026-10-26 at
commit `57f7f63`. They are not measurements of the current main branch.
PostgreSQL 17.11 held 1,000,000 rows in `alerts`, 100,000 in `assets`,
200,000 in `links` and 2,200,000 in `observables`. v12n ran natively on
an Apple M4 (16 GiB) under macOS, and Ontop 5.5.0 (`-Xmx256m`) and
PostgreSQL ran in the Docker Desktop VM. Each time is the median of 10
runs, from sending a SPARQL 1.1 Protocol request on a new connection to
the end of the JSON response.

| Query | Result rows | v12n (ms) | Ontop (ms) |
|---|---:|---:|---:|
| `q06-union` | 145,000 | 124.5 | 1267.4 |
| `q07-order-limit` | 100 | 194.8 | 142.9 |

In that session v12n took less time than Ontop on eight of the nine
queries and more on `q07-order-limit`, which sorts by an integer and an
IRI. A later change reduced the time of that query at 100,000 alerts
([IRI sort keys after #75](bench/RESULTS.md#iri-sort-keys-after-75)),
and the 1,000,000-alert session was not repeated. The results hold for
this machine and these queries. Ontop's heap was limited to 256 MiB, and
a larger heap may change its time on large results
([caveats](bench/RESULTS.md#caveats)).

These times are end to end. They include SQL execution in PostgreSQL,
decoding the rows, building RDF terms and writing the response.
Translating SPARQL to SQL is measured separately and took 15–40 µs per
query in the report. Neither engine converts the whole database to RDF,
so the benchmark does not measure whole-dataset RDF materialization.

## Roadmap

Planned work is grouped into
[milestones](https://github.com/CharmingGroot/sparql-v12n/milestones). An
arrow means that the later milestone uses work from the earlier one.
Milestones with no path between them do not depend on each other. Dashed
boxes have no work on the main branch yet. Status refers to the main
branch, not to a release; [Supported SPARQL](#supported-sparql) lists
what works today and its limits.

```mermaid
flowchart LR
  M["M Catalog metadata"] --> O1["O1 RDFS ontologies"] --> O2["O2 OWL 2 QL ontologies"]
  M --> R4["R4 Graph patterns and paths"]
  O1 --> R4
  M --> D["D More databases"]
  R1["R1 Value expressions<br/>(partly done)"] --> R3["R3 Aggregates"]
  R2["R2 Operators and query forms<br/>(partly done)"]
  classDef planned stroke-dasharray: 5 4
  class R3,M,O1,O2,R4,D planned
```

| Milestone | On main | Remaining |
|---|---|---|
| [R1 Value expressions](https://github.com/CharmingGroot/sparql-v12n/milestone/1) | Arithmetic in `FILTER`, `BIND` and `SELECT` expressions | Conditional, RDF term, string, numeric, date and time and hash functions, casts, and the new expressions in `ORDER BY` |
| [R2 Operators and query forms](https://github.com/CharmingGroot/sparql-v12n/milestone/3) | SPARQL XML, CSV and TSV results | `VALUES`, `MINUS`, `EXISTS`, subqueries, `ASK`, `CONSTRUCT`, `DESCRIBE` |
| [R3 Aggregates](https://github.com/CharmingGroot/sparql-v12n/milestone/4) | | `GROUP BY`, `HAVING` and the aggregate functions |
| [M Catalog metadata](https://github.com/CharmingGroot/sparql-v12n/milestone/2) | | Engine and backend ports, catalog metadata loaded once and refreshed, optimizations from NOT NULL and foreign key constraints |
| [O1 RDFS ontologies](https://github.com/CharmingGroot/sparql-v12n/milestone/5) | | RDFS entailment |
| [O2 OWL 2 QL ontologies](https://github.com/CharmingGroot/sparql-v12n/milestone/7) | | OWL 2 QL |
| [R4 Graph patterns and paths](https://github.com/CharmingGroot/sparql-v12n/milestone/6) | | Variable predicates, named graphs and property paths |
| [D More databases](https://github.com/CharmingGroot/sparql-v12n/milestone/8) | PostgreSQL-specific SQL syntax behind a `Dialect` trait | A second database besides PostgreSQL |

An MCP server for AI agents is planned after these.

## Layout

| Path | Contents |
|---|---|
| `crates/v12n-core` | SPARQL parsing, mapping index, unfolding, optimization, SQL rendering. No database dependency |
| `crates/v12n-postgres` | Execution on PostgreSQL and key metadata introspection |
| `crates/v12n-server` | SPARQL 1.1 Protocol HTTP endpoint |
| `crates/v12n-cli` | The `v12n` command |
| `crates/v12n-difftest` | Differential tests against Ontop, one test per case query |
| `docs` | Usage and query semantics references, and the README diagram |
| `examples/shop` | Example schema, data, mapping and queries |
| `bench` | Benchmark against Ontop on generated data, and its results |
| `testenv` | Local PostgreSQL and the Ontop reference endpoint for tests |
| `tests/diff` | Differential test cases (`examples/` are cases too), W3C R2RML cases in `tests/diff/w3c` |
| `tests/w3c/rdb2rdf` | W3C RDB2RDF test files, unmodified subset (W3C Document License) |

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md) for how to propose a change, the
commit convention and how to run the tests.

## License

Apache-2.0. See [LICENSE](LICENSE).
