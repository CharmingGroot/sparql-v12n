# Benchmark results

v12n against Ontop 5.5.0 on the scaled MDR data, the same PostgreSQL and
the same mapping. How it is measured: [README.md](README.md). Raw output
of each run (`results.json`, `results.md` with every statement both
engines ran) is written to `bench/results/`; the workflow uploads it as an
artifact.

## Setup

| | |
|---|---|
| Date | 2026-10-26 |
| v12n | `57f7f63` (main), release build, rustc 1.99.0, native on macOS |
| Ontop | `ontop/ontop:5.5.0@sha256:d19a2055…`, Java 11.0.30 (Temurin), `ONTOP_JAVA_ARGS=-Xmx256m` as in `testenv/difftest.sh` |
| PostgreSQL | `postgres:17.11@sha256:b8faeff7…`, defaults (`shared_buffers` 128MB, `work_mem` 4MB, `jit` on, 2 parallel workers per gather) plus `pg_stat_statements` |
| Machine | Apple M4, 10 CPUs, 16 GiB, macOS 26.4.1; Docker Desktop 29.4.3 (aarch64 VM, 10 CPUs, 7.8 GiB) |
| v12n settings | `V12N_ROW_LIMIT=0`, `V12N_STATEMENT_TIMEOUT=0` (Ontop has neither); `v12n-row-limit` keeps the default 10000 |
| HTTP | SPARQL 1.1 Protocol `POST`, new connection per request, JSON results |
| Runs | 100k: 3 warm-up + 20 timed per query and engine, alternating; 1M: 3 + 10 |
| Data | 100k: 10k assets, 100k alerts, 20k links, 220k observables. 1M: ten times that |

Every engine returned the same solutions as Ontop for every query at both
scales (bags, or the same order with `ORDER BY`), so all timings count.
`v12n-row-limit` is cut at 10000 rows where the result is larger; those
timings are for a shorter result and are only shown, not compared.

## Results, 100k alerts

Milliseconds, median / p95 over 20 runs.

| Query | Rows | v12n | Ontop | Ontop ÷ v12n | v12n first row | Ontop first row |
|---|---:|---:|---:|---:|---:|---:|
| q01-lookup | 1 | 2.6 / 3.1 | 3.7 / 5.1 | 1.4 | 2.6 | 3.6 |
| q02-join | 2000 | 15.4 / 23.4 | 33.9 / 43.4 | 2.2 | 13.7 | 8.5 |
| q03-star | 2000 | 6.4 / 9.8 | 24.4 / 36.7 | 3.8 | 5.0 | 4.8 |
| q04-optional | 10000 | 13.5 / 17.3 | 101.5 / 189.1 | 7.5 | 5.8 | 5.5 |
| q05-filter | 10000 | 13.7 / 22.6 | 111.5 / 181.1 | 8.2 | 5.1 | 6.9 |
| q06-union | 14500 | 16.3 / 29.1 | 132.0 / 211.8 | 8.1 | 6.3 | 7.9 |
| q07-order-limit | 100 | 63.4 / 95.5 | 16.3 / 23.4 | **0.26** | 63.3 | 15.8 |
| q08-distinct-keyless | 5000 | 31.1 / 38.2 | 45.3 / 94.0 | 1.5 | 29.7 | 30.9 |
| q09-keyless-set | 4000 | 21.0 / 32.2 | 49.8 / 67.7 | 2.4 | 18.2 | 18.3 |

Per request, from `pg_stat_statements` (one login role per engine) and the
translation harness:

| Query | v12n parse + unfold (µs) | v12n SQL render (µs) | v12n statements | v12n SQL exec (ms) | Ontop statements | Ontop SQL exec (ms) | `v12n query` CLI (ms) |
|---|---:|---:|---:|---:|---:|---:|---:|
| q01-lookup | 14 | 6 | 2 | 0.05 | 3 | 0.01 | 21.6 |
| q02-join | 20 | 9 | 2 | 9.7 | 3 | 3.9 | 24.4 |
| q03-star | 14 | 5 | 2 | 1.4 | 3 | 0.6 | 16.1 |
| q04-optional | 18 | 5 | 2 | 3.8 | 3 | 2.5 | 22.9 |
| q05-filter | 16 | 7 | 2 | 3.5 | 3 | 4.0 | 32.8 |
| q06-union | 18 | 7 | 2 | 8.5 | 3 | 8.6 | 27.5 |
| q07-order-limit | 18 | 22 | 2 | 63.5 | 3 | 12.1 | 141.0 |
| q08-distinct-keyless | 11 | 4 | 1 | 28.0 | 3 | 29.1 | 32.3 |
| q09-keyless-set | 19 | 8 | 2 | 15.1 | 3 | 14.5 | 25.4 |

v12n's two statements are the table-key lookup in the catalog and the
query; prepare-only steps (the column-type probe and the prepare of the
query) do not show in `pg_stat_statements` but are round trips (see
below). Ontop's three are `BEGIN`, the query and `COMMIT`.

Start-up and memory:

| | v12n | Ontop |
|---|---:|---:|
| Launch to first answered query | 10 ms (first launch 556 ms) | 5.4 s (runs 6.6, 4.7, 5.4 s) |
| Resident memory at idle | 7.5 MiB | 344 MiB |
| Resident memory, peak | 11.9 MiB | 520 MiB |

Load, 8 clients sending q01, q02, q03 and q07 in turn for 15 s: v12n 137
requests/s (median 21.7 ms, p95 205 ms), Ontop 131 requests/s (median
54.4 ms, p95 151 ms). An earlier run on the same machine gave 79 and 28
requests/s, so this phase varies too much on Docker Desktop to rank the
engines; the q07 requests dominate v12n's tail.

## Results, 1M alerts

Milliseconds, median over 10 runs (p95 in `results.json`).

| Query | Rows | v12n | Ontop | Ontop ÷ v12n | v12n SQL exec | Ontop SQL exec |
|---|---:|---:|---:|---:|---:|---:|
| q01-lookup | 1 | 4.3 | 5.8 | 1.3 | 0.1 | 0.02 |
| q02-join | 20000 | 97.6 | 313.4 | 3.2 | 86.5 | 50.4 |
| q03-star | 20000 | 34.4 | 309.0 | 9.0 | 20.9 | 8.5 |
| q04-optional | 100000 | 120.9 | 1470.3 | 12.2 | 63.8 | 38.4 |
| q05-filter | 100000 | 120.2 | 1516.7 | 12.6 | 84.3 | 104.8 |
| q06-union | 145000 | 124.5 | 1267.4 | 10.2 | 100.6 | 146.8 |
| q07-order-limit | 100 | 194.8 | 142.9 | **0.73** | 214.4 | 131.1 |
| q08-distinct-keyless | 50000 | 263.4 | 732.4 | 2.8 | 270.8 | 434.0 |
| q09-keyless-set | 40000 | 267.5 | 826.9 | 3.1 | 198.9 | 237.1 |

Start-up 11 ms against 7.4 s; memory 7.5 / 12.2 MiB against 322 / 527 MiB
(idle / peak). Load: 28.5 against 9.5 requests/s.

## IRI sort keys after #75

[#75](https://github.com/CharmingGroot/sparql-v12n/issues/75) builds the
IRI of a template without encoding values whose characters IRI-safe
encoding keeps: the key of `alert/{id}` is now `$1 || t0."id"::text`, as
in Ontop. Two 100k runs on the same machine, one after the other on
2026-10-27, `52681cf` (main) against the change, every result matching
Ontop; milliseconds, median / p95 over 20 runs:

| q07-order-limit | Before | After |
|---|---:|---:|
| v12n | 62.0 / 76.8 | 3.6 / 4.6 |
| Ontop | 15.4 / 22.9 | 12.9 / 14.1 |
| Ontop ÷ v12n | 0.25 | 3.6 |
| `v12n-row-limit` | 80.2 / 103.2 | 3.5 / 4.1 |
| v12n SQL exec | 58.8 | 0.69 |
| Ontop SQL exec | 11.2 | 9.7 |
| `v12n query` CLI | 132.7 | 11.8 |

v12n now runs below Ontop: its `ORDER BY "o1_exact" DESC, "o2_text"`
reads the index on `severity_id` backwards and sorts only the rows of
the highest severity by the key (`Incremental Sort` over `Index Scan
Backward`), where Ontop's `DESC NULLS LAST` does not match the index
order and sorts every row (`Seq Scan`, top-N heapsort), as `EXPLAIN
ANALYZE` of both statements on 100,000 rows shows. Before, the same plan
encoded the id of every row of that severity. The other queries stayed
within the run-to-run variation. 1M was not re-run.

## Row-limit window after #77

[#77](https://github.com/CharmingGroot/sparql-v12n/issues/77) runs a
query whose own `LIMIT` is at or below the row limit as the plan's own
SQL, without `count(*) OVER ()` and the `LIMIT n+1` sub-selects, since
such a result cannot be truncated. Two 100k runs on the same machine on
2026-10-27, `8828ae3` (main) against the change, every result matching
Ontop; milliseconds, median / p95 over 20 runs:

| q07-order-limit | Before | After |
|---|---:|---:|
| `v12n-row-limit` | 3.4 / 3.9 | 3.5 / 4.2 |
| `v12n-row-limit` SQL exec | 0.60 | 0.61 |
| v12n (no row limit) | 3.6 / 4.2 | 3.7 / 4.7 |

The statement `v12n-row-limit` runs for q07 is now the same as without
a row limit. After #75 the window over q07's 100 sorted rows no longer
cost anything measurable (the 63 → 81 ms of the issue was measured
before #75), so the times stay within run-to-run variation. q07 is the
only benchmark query with a `LIMIT`; the others still run with the
window, unchanged.

## Self-joins after #28

[#28](https://github.com/CharmingGroot/sparql-v12n/issues/28) merges two
scans of one table when the query's conditions pin both to the same row
by a `NOT NULL` unique key. q03's `assets t0 JOIN assets t1 ON t1.id =
t0.id` is one scan of `assets`, q02 reads `alerts` once, and q01 is one
key lookup instead of a `CROSS JOIN` of two. q04's `LEFT JOIN` of
`assets` with itself is not merged: the optional side is `NULL` where
the part does not match. Runs one after the other on 2026-10-27, `8828ae3`
(main) against the change, every result matching Ontop; v12n SQL exec is
the mean per request from `pg_stat_statements`, end to end the median /
p95 (100k: 20 runs, 1M: 10 runs). The machine was faster than on
2026-10-26, so compare within this table.

| Query | Alerts | v12n SQL exec before | after | Ontop SQL exec (after run) | v12n end to end before | after |
|---|---|---:|---:|---:|---:|---:|
| q01-lookup | 100k | 0.047 | 0.046 | 0.006 | 2.3 / 3.1 | 2.3 / 3.9 |
| | 1M | 0.049 | 0.048 | 0.007 | 2.1 / 2.8 | 2.4 / 3.7 |
| q02-join | 100k | 4.75 | 1.50 | 1.69 | 9.9 / 11.8 | 5.5 / 7.8 |
| | 1M | 32.0 | 18.4 | 24.6 | 33.4 / 40.1 | 26.8 / 38.4 |
| q03-star | 100k | 0.86 | 0.34 | 0.34 | 4.1 / 7.2 | 4.1 / 7.8 |
| | 1M | 9.5 | 4.3 | 4.6 | 15.3 / 17.5 | 18.2 / 21.7 |
| q04-optional | 100k | 2.01 | 2.08 | 1.14 | 8.8 / 10.6 | 9.4 / 15.2 |
| | 1M | 34.7 | 29.9 | 14.4 | 72.4 / 102.6 | 86.2 / 94.3 |

PostgreSQL now runs q02 and q03 as long as Ontop's SQL or shorter
(q03 0.34 ms both at 100k; q02 18 against 25 ms at 1M). End to end, q02
is 45% faster at 100k; q03's request time is dominated by the round trips
and moved within the run-to-run variation (its first row came at 3.1 ms
against 3.2 at 100k, 4.2 against 5.1 at 1M). q01's two key lookups cost
little either way. The other queries have no self-join and stayed within
the variation. The number of statements per request is unchanged: the
keys come from the catalog query that already ran.

## Round trips after #76

[#76](https://github.com/CharmingGroot/sparql-v12n/issues/76) sends
everything that depends on the plan alone in one pipelined round trip:
the key query (prepared once per connection), one statement that resolves
column names and reports column types, and the statements prepared for
the same plan last time on that connection, which are used only when they
are exactly the statement the fresh keys and types give. Nothing read
from the catalog is cached
(Discussion [#87](https://github.com/CharmingGroot/sparql-v12n/discussions/87)). A
request then takes two round trips: that one and running the statement;
the first request of a plan on a connection takes three. Round trips are
counted by a proxy in `bench.py` (see README.md); Ontop takes two (`BEGIN`
with the query, then `COMMIT`).

Two 100k runs one after the other on 2026-10-27, `ce8a9f1` (main) against
the change, every result matching Ontop; milliseconds, median / p95 over
20 runs (first row: median). `SELECT 1` from the host took 0.21 and
0.19 ms in the two runs.

| Query | Round trips | v12n before | after | First row before | after | Ontop before | after | `v12n query` CLI before | after |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|
| q01-lookup | 4 → 2 | 2.6 / 3.7 | 1.2 / 2.4 | 2.6 | 1.2 | 3.7 | 3.2 | 10.4 | 9.8 |
| q02-join | 5 → 2 | 6.8 / 9.3 | 5.2 / 8.2 | 4.4 | 3.0 | 16.1 | 15.8 | 13.0 | 12.5 |
| q03-star | 4 → 2 | 3.6 / 5.4 | 2.6 / 3.8 | 2.8 | 1.7 | 13.5 | 13.0 | 12.7 | 11.6 |
| q04-optional | 5 → 2 | 9.7 / 12.9 | 8.1 / 14.8 | 4.2 | 2.5 | 59.4 | 60.6 | 17.8 | 17.3 |
| q05-filter | 5 → 2 | 8.5 / 14.9 | 7.2 / 14.4 | 3.5 | 2.0 | 60.1 | 59.1 | 17.1 | 16.9 |
| q06-union | 5 → 2 | 9.6 / 16.9 | 8.6 / 16.4 | 3.8 | 2.3 | 56.4 | 54.9 | 19.2 | 16.6 |
| q07-order-limit | 5 → 2 | 4.2 / 5.4 | 2.6 / 3.2 | 4.2 | 2.5 | 13.2 | 12.3 | 12.0 | 11.4 |
| q08-distinct-keyless | 3 → 2 | 22.0 / 23.1 | 21.4 / 23.9 | 19.1 | 18.5 | 29.4 | 29.2 | 28.4 | 28.1 |
| q09-keyless-set | 5 → 2 | 15.0 / 16.6 | 15.9 / 16.9 | 12.0 | 10.5 | 32.1 | 31.0 | 23.9 | 23.4 |

The first row now comes 0.6–1.7 ms earlier, and q01 takes half the time:
its SQL runs for 0.07 ms, so the round trips were most of it. Queries that
spend longer in the database or in streaming rows gain the same
milliseconds, a smaller share. The SQL each engine runs and its execution
time did not change (`pg_stat_statements`: v12n still runs two statements
per request, the key query and the query; prepare-only steps do not show
there). The CLI opens a new connection per query, so it has no earlier
statements to reuse and takes three round trips instead of four or five;
its time is mostly process start and connecting. 1M was not re-run.

## Analysis

**Where v12n is faster.** On every query that returns many rows, by 7–8×
at 100k and 10–12× at 1M end to end, although both engines send
PostgreSQL much the same SQL and its execution time is close (q05, q06,
q08, q09). The difference is after the database: v12n decodes and writes
rows as they arrive (first row at 5–6 ms for 10000-row results), while
Ontop spends about 100 ms per 10000 rows of its own time. Start-up is
three orders of magnitude shorter (a native process that parses the
mapping and opens one connection, against a JVM and Spring Boot) and
resident memory about 40× smaller; v12n's peak stays at 12 MiB with
145000-row results because it streams.

**Where v12n is slower.**

1. **IRI sort keys** ([#75](https://github.com/CharmingGroot/sparql-v12n/issues/75),
   fixed, see above). q07 was 4× slower at 100k. To sort by the IRI of
   `alert/{id}`, v12n percent-encoded the id text one character at a
   time in a correlated subquery per row; Ontop sorts by
   `prefix || CAST(id AS TEXT)`. The text of an integer never needs
   encoding.
2. **Self-joins** ([#28](https://github.com/CharmingGroot/sparql-v12n/issues/28),
   fixed except `OPTIONAL`, see above). Ontop merges scans of the same
   table on the primary key; v12n joined `alerts` with `alerts` (q02) and
   `assets` with `assets` (q03, q04, and q01 as a `CROSS JOIN` of two key
   lookups), which PostgreSQL ran 1.5–2.5× longer (q02: 9.7 against 3.9 ms
   at 100k, 86 against 50 ms at 1M). q04's `LEFT JOIN` on the key remains.
3. **Round trips before the first row** ([#76](https://github.com/CharmingGroot/sparql-v12n/issues/76),
   fixed, see above). Counted from the PostgreSQL log: the key lookup
   (prepare and execute), the column-type probe (prepare), the prepare and
   the execution of the query, so 4–5 round trips per request against
   Ontop's 2. q01's SQL runs for 0.05 ms but the request took 2.6 ms, most
   of it waiting on the network. Now 2, as Ontop.
4. **Row-limit window** ([#77](https://github.com/CharmingGroot/sparql-v12n/issues/77),
   fixed for queries whose `LIMIT` fits, see above). With the default row
   limit, `count(*) OVER ()` and, for ordered or sliced queries, two more
   sorted sub-selects were added even when the query's own `LIMIT` made
   truncation impossible: q07 went from 63 to 81 ms before #75. On
   results under the limit without such a `LIMIT`, the window still holds
   the first row until all rows are computed (q04: 9.0 against 5.8 ms);
   that is the price of the truncation header.

**What the correctness mechanisms cost.**

- Translation (parse, unfold, render) is 15–40 µs per query, below 1% of
  any request; the largest render is q07's encoding expression.
- Term-key comparisons and computed term columns cost nothing measurable
  in these queries except the IRI sort key above (before #75), the one
  place a term is computed per row in SQL.
- Set semantics: reading the keys costs one catalog query (0.05–0.1 ms of
  execution, in the round trip shared with the column probe since #76) and
  lets v12n drop `DISTINCT` on every query
  whose tables have keys (q01–q07 run without it). On the table without a
  key, both engines use `DISTINCT` and take the same time (q08, q09).
- The column-type probe was one prepare-only round trip; since #76 it
  goes with the key query.

**`v12n query` (CLI)** costs 16–22 ms for a one-row query: process start,
mapping parsing, connecting and setting up the session, then the same
round trips. It suits scripts and agents that run a few queries; repeated
queries belong on `v12n serve`.

## Caveats

- **Docker Desktop.** Ontop and PostgreSQL run in the Docker Desktop VM,
  v12n natively on macOS. Measured `SELECT 1` round trips (pgbench,
  extended protocol): 0.08 ms on the Compose network (Ontop), 0.44 ms from
  the host (v12n), 0.65 ms from a container through the published port.
  v12n pays about 0.36 ms more per round trip than Ontop: with the 4
  round trips before #76, about 1.4 ms of q01's 2.6 ms. `ontop-host` (Ontop through the
  published port, a slower path than v12n's) was 4.7 ms on q01 and 4–17%
  slower than `ontop` elsewhere, so the conclusions do not depend on the
  network path. Ontop's
  HTTP responses also cross the VM boundary; v12n's do not.
- A Linux build of v12n to run in a container would have needed new
  downloads (a Rust image or a cross toolchain), so it was not done. The
  `Benchmark` workflow runs everything on one Linux host, where both
  engines reach PostgreSQL locally.
- Ontop runs with `-Xmx256m`, as in the differential tests. Its peak RSS
  (520 MiB) is well above the heap, and a larger heap may change its time
  on large results; that is worth a run before drawing conclusions about
  Ontop's per-row cost.
- The client is Python on the same machine. Start-up and load figures
  vary between runs on Docker Desktop (Ontop's start-up ranged from 4.5 to
  18 s across runs of the same day). Between two 100k runs, v12n's
  per-query medians agreed within about 20% and Ontop's within 50% (q06,
  q09); every ratio kept its side of 1.
