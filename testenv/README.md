# Test environment

PostgreSQL with the `examples/shop` schema and data, plus an
[Ontop](https://ontop-vkg.org/) SPARQL endpoint over the same database using
`examples/shop/mapping.ttl` (R2RML). Ontop is the reference for differential
tests.

## Prerequisites

- Docker with Compose v2
- `curl`, and `shasum` or `sha256sum`
- `jq` and a built `v12n` (`cargo build`) for `compare.sh`

## Steps

Run from this directory:

```sh
./fetch-jdbc.sh          # PostgreSQL JDBC driver for Ontop, checksum-verified, into jdbc/
docker compose up -d     # postgres, then ontop once postgres is healthy
./smoke.sh               # VIP query against Ontop must return orders 1001, 1003, 1004
./compare.sh             # every examples/shop/queries/*.rq: Ontop vs v12n query vs v12n serve, sorted
docker compose down -v   # stop and drop the database volume
```

## Ports

| Service  | Address                          |
|----------|----------------------------------|
| postgres | `127.0.0.1:55432`, database `shop` |
| ontop    | `http://127.0.0.1:58080/sparql`  |
| `v12n serve --config v12n.toml` | `http://127.0.0.1:57878/sparql` |

All are bound to localhost only. The credentials in `docker-compose.yml`,
`ontop/shop.properties` and `v12n.toml` (`v12n` / `v12n-dev`) are for local
development only.

Database integration tests run against the same PostgreSQL when
`V12N_TEST_DATABASE_URL` is set, and are skipped otherwise:

```sh
V12N_TEST_DATABASE_URL=postgresql://v12n:v12n-dev@127.0.0.1:55432/shop cargo test
```

## Files

- `compare.sh`: differential check of `v12n query` and `v12n serve` against Ontop
- `v12n.toml`: `v12n serve` settings for this environment
- `ontop/shop.properties`: JDBC connection to the `postgres` service
- `ontop/shop.ttl`: vocabulary used by the shop mapping
- `jdbc/`: downloaded driver, not committed
