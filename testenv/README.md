# Test environment

PostgreSQL with the `examples/shop` schema and data, plus an
[Ontop](https://ontop-vkg.org/) SPARQL endpoint over the same database using
`examples/shop/mapping.ttl` (R2RML). Ontop is the reference for differential
tests.

## Prerequisites

- Docker with Compose v2
- `curl`, and `shasum` or `sha256sum`

## Steps

Run from this directory:

```sh
./fetch-jdbc.sh          # PostgreSQL JDBC driver for Ontop, checksum-verified, into jdbc/
docker compose up -d     # postgres, then ontop once postgres is healthy
./smoke.sh               # VIP query against Ontop must return orders 1001, 1003, 1004
docker compose down -v   # stop and drop the database volume
```

## Ports

| Service  | Address                          |
|----------|----------------------------------|
| postgres | `127.0.0.1:55432`, database `shop` |
| ontop    | `http://127.0.0.1:58080/sparql`  |

Both are bound to localhost only. The credentials in `docker-compose.yml` and
`ontop/shop.properties` (`v12n` / `v12n-dev`) are for local development only.

## Files

- `ontop/shop.properties`: JDBC connection to the `postgres` service
- `ontop/shop.ttl`: vocabulary used by the shop mapping
- `jdbc/`: downloaded driver, not committed
