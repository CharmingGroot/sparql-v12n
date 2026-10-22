# sparql-v12n

Query PostgreSQL with SPARQL through R2RML mappings, without copying the data.

sparql-v12n is a virtualization (virtual knowledge graph, OBDA) engine written in Rust.
It rewrites a SPARQL query into SQL using an R2RML mapping, runs the SQL on the
source database, and returns SPARQL results. The mapping also defines exactly
which tables and columns can be queried.

> Status: early development. Not usable yet.

## Planned interfaces

| Interface | Use |
|---|---|
| `v12n` CLI | `check`, `terms`, `explain`, `query`, `serve` |
| HTTP | SPARQL 1.1 Protocol endpoint at `/sparql` |
| MCP | tools for AI agents (later) |

## Layout

| Path | Contents |
|---|---|
| `crates/v12n-core` | SPARQL parsing, mapping index, unfolding, optimization, SQL rendering. No database dependency |
| `crates/v12n-postgres` | Execution on PostgreSQL and key metadata introspection |
| `crates/v12n-server` | SPARQL 1.1 Protocol HTTP endpoint |
| `crates/v12n-cli` | The `v12n` command |
| `examples/shop` | Example schema, data, mapping and queries |
| `testenv` | Local PostgreSQL (and later the reference endpoint) for tests |
| `tests/diff` | Differential test cases |

## License

Apache-2.0. See [LICENSE](LICENSE).
