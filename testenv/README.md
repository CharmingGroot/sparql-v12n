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

## Differential tests

`difftest.sh` runs every case in `examples/` and `tests/diff/` through
v12n and Ontop and compares the results (see `tests/diff/README.md`). It
starts its own PostgreSQL and one Ontop container per case from a
generated Compose file in `target/difftest/`, under the Compose project
`v12n-difftest`, so it does not touch the environment above.

```sh
./fetch-jdbc.sh
./difftest.sh            # up, test, down -v
```

## Ports

| Service  | Address                          |
|----------|----------------------------------|
| postgres | `127.0.0.1:55432`, database `shop` |
| ontop    | `http://127.0.0.1:58080/sparql`  |
| `v12n serve --config v12n.toml` | `http://127.0.0.1:57878/sparql` |
| `difftest.sh` postgres | `127.0.0.1:55433` (`DIFF_PG_PORT`), one database `diff_<case>` per case |
| `tls.sh` postgres | `127.0.0.1:55434` (`TLS_PG_PORT`), TLS on, database `shop` |
| `difftest.sh` ontop | `http://127.0.0.1:58100/sparql` and up, one per case (`DIFF_ONTOP_PORT`) |

All are bound to localhost only. The credentials in `docker-compose.yml`,
`ontop/shop.properties` and `v12n.toml` (`v12n` / `v12n-dev`) are for local
development only.

Database integration tests run against the same PostgreSQL when
`V12N_TEST_DATABASE_URL` is set, and are skipped otherwise:

```sh
V12N_TEST_DATABASE_URL=postgresql://v12n:v12n-dev@127.0.0.1:55432/shop cargo test
```

## TLS tests

`tls.sh up` generates a test CA, a server certificate for the host name
`localhost` signed by it, and a second CA that signed nothing, into
`target/tls/`. It then starts a PostgreSQL container with `ssl=on` and the
shop data, and prints the environment variables that enable the TLS tests
(`crates/v12n-postgres/tests/tls.rs` and one test in
`crates/v12n-cli/tests/cli.rs`):

```sh
eval "$(./tls.sh up)"    # sets V12N_TEST_TLS_DATABASE_URL and V12N_TEST_TLS_CERTS
cargo test -p v12n-postgres --test tls
./tls.sh down            # stop and remove the container
```

The tests are skipped unless both variables are set. The container is
named `v12n-tls` and listens on `127.0.0.1:55434`; `TLS_PG_NAME` and
`TLS_PG_PORT` change them. The certificates are valid for seven days and
are generated again on every `up`.

CI starts the same server in the `test` job of `.github/workflows/ci.yml`,
from the image of its PostgreSQL service, and runs the TLS tests with the
other PostgreSQL tests.

## Files

- `difftest.sh`: differential tests of every case against Ontop
- `tls.sh`: PostgreSQL with TLS and test certificates for the TLS tests
- `compare.sh`: differential check of `v12n query` and `v12n serve` against Ontop
- `v12n.toml`: `v12n serve` settings for this environment
- `ontop/shop.properties`: JDBC connection to the `postgres` service
- `ontop/shop.ttl`: vocabulary used by the shop mapping
- `jdbc/`: downloaded driver, not committed
