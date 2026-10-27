# Benchmarks

v12n against [Ontop](https://ontop-vkg.org/) on the same PostgreSQL, mapping
and queries. Measured numbers and their analysis are in
[RESULTS.md](RESULTS.md).

```sh
testenv/fetch-jdbc.sh                  # once: the JDBC driver Ontop loads
bench/run.sh                           # 100k alerts, about 4 minutes
BENCH_ALERTS=1000000 bench/run.sh      # 1M alerts, about 7 minutes
```

`run.sh` builds `v12n` in release mode, starts PostgreSQL and two Ontop
containers under the Compose project `v12n-bench`, loads the data, runs
`bench.py`, and removes everything (`docker compose down -v`) when it ends
or fails. Results go to `bench/results/<alerts>-<time>/` (not committed):
`results.md`, `results.json`, the generated `compose.yml` and the v12n logs.
The settings are listed at the top of `run.sh`.

It needs Docker with Compose v2, `python3` (standard library only), a Rust
toolchain and `testenv/jdbc/`. The images are the pinned ones of
`testenv/difftest.sh`; nothing else is downloaded. `pgbench` on the host is
optional (one round-trip figure).

The `Benchmark` workflow runs the same script on `workflow_dispatch` and
uploads the results directory as an artifact. It never runs on pull
requests.

## Data

`mdr/` is the MDR case of `tests/diff/0001-mdr` (assets, alerts, links)
scaled by `generate.sql`, plus `observables`, a table without a key where
the same row can appear twice. The generator uses no `random()`, so every
run loads the same rows. Per `N` alerts: `N/10` assets, about `N/5` links
and `2.2 N` observables. Indexes are the keys plus one per foreign key and
filtered column, created after loading, followed by `VACUUM ANALYZE`.

## Queries

| Query | What it exercises |
|---|---|
| `q01-lookup` | BGP with a constant subject: one row by primary key |
| `q02-join` | join through a template object, with a constant |
| `q03-star` | two properties of one table: a self-join on the key unless eliminated |
| `q04-optional` | `OPTIONAL` over every asset |
| `q05-filter` | `FILTER` with a comparison on an integer column |
| `q06-union` | `UNION` of two selections |
| `q07-order-limit` | `ORDER BY` on an integer and an IRI, `LIMIT 100` |
| `q08-distinct-keyless` | `DISTINCT` over the table without a key |
| `q09-keyless-set` | no `DISTINCT`, but duplicate rows must count once (set semantics) |

## What is measured

- **Correctness first.** Each engine's result for each query must be the
  same solutions as Ontop's (a bag, or the same sequence with `ORDER BY`)
  before its timings count. A mismatch is reported and the engine's
  timings for that query are dropped.
- **HTTP**, for every engine: a SPARQL 1.1 Protocol `POST` on a new
  connection, after warm-up runs; median and p95 of the time to the first
  solution and to the end of the response. Runs alternate between the
  engines.
  - `v12n`: `v12n serve`, native, row limit off (`V12N_ROW_LIMIT=0`) and
    no statement timeout, as Ontop has neither.
  - `v12n-row-limit`: the same with the default row limit of 10000, which
    adds a `count(*) OVER ()` so that a truncated result is known before
    the first row.
  - `ontop`: the Ontop container, reaching PostgreSQL on the Compose
    network, with `-Xmx256m` as in `testenv/difftest.sh`.
  - `ontop-host`: the same Ontop reaching PostgreSQL through the published
    port, the path the native v12n takes.
- **CLI**: `v12n query` per run, so process start, mapping parsing and
  the connection count too.
- **Translation without a database**: `cargo bench -p v12n-server --bench
  translate` times parsing and unfolding, and SQL rendering, per query.
- **Database side**: `pg_stat_statements`, one login role per engine, gives
  the statements each request runs and their execution and planning time,
  and the most expensive statement (the SQL each engine generated).
- **Round trips**: a third `v12n serve` reaches PostgreSQL through a
  proxy in `bench.py` that counts the requests v12n waits on per HTTP
  request (pipelined requests count once; prepare-only steps count too,
  which `pg_stat_statements` does not show). It is not timed.
- **Start-up**: from launching the process or starting the container to
  the first answered query, several times. For Ontop, its own
  "Started ... in" time is recorded too.
- **Memory**: resident set size at idle, after the run, and the peak
  (`VmHWM` on Linux; `time -l` for v12n and `VmHWM` in the container on
  macOS), plus the largest sampled value while several clients send
  requests at once (requests per second and latency are reported for that
  phase too).

## Caveats

- On macOS, Ontop runs in the Docker Desktop VM and v12n natively. Every
  v12n round trip to PostgreSQL crosses the VM boundary, as do HTTP
  requests to Ontop. `ontop-host` and the `SELECT 1` round-trip times in
  the results show what that costs. On Linux (the workflow) both paths are
  local.
- The client is a Python process on the same machine as the engines and
  the database; numbers are for comparing engines on one machine, not
  absolute capacity.
