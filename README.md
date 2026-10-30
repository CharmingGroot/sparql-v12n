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

4. Stop the database and remove its container and network.

   ```sh
   docker compose -f testenv/docker-compose.yml down -v
   ```

`v12n explain` with the same mapping and query shows the generated SQL
without a database. [testenv/README.md](testenv/README.md) describes the
test environment, including the Ontop endpoint that the differential tests
use and that this example does not need.

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

## Roadmap

The supported subset grows in the order below. Each step is a
[milestone](https://github.com/CharmingGroot/sparql-v12n/milestones), and
an arrow means the later step builds on the earlier one.

```mermaid
flowchart LR
  R1["1. Value expressions"] --> R2["3. Operators and query forms"] --> R3["4. Aggregates"]
  M["2. Catalog metadata"] --> O1["5. RDFS ontologies"] --> O2["7. OWL 2 QL ontologies"]
  M --> R4["6. Graph patterns and paths"]
  O1 --> R4
  D["8. More databases"]
```

1. Value expressions: `BIND`, expressions in `SELECT`, arithmetic and the
   SPARQL 1.1 functions
2. Catalog metadata loaded once and refreshed, with optimizations from
   NOT NULL and foreign key constraints
3. `VALUES`, `MINUS`, `EXISTS`, subqueries, `ASK`, `CONSTRUCT`,
   `DESCRIBE`, and more result formats
4. `GROUP BY` and aggregates
5. Ontologies: RDFS entailment
6. Variable predicates, named graphs and property paths
7. Ontologies: OWL 2 QL
8. Databases other than PostgreSQL

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
