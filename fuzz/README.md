# Fuzzing

[cargo-fuzz](https://rust-fuzz.github.io/book/cargo-fuzz.html) targets for
the inputs that reach v12n from outside: SPARQL text from the endpoint and
the CLI, and R2RML mappings in Turtle.

| Target | Input | Path |
|---|---|---|
| `query_parse` | SPARQL text | `sparql::parse` |
| `mapping_parse` | Turtle | `Mapping::from_turtle`, then `Planner::new` |
| `query_plan` | SPARQL text | `Planner::plan` and `SqlPlan::to_sql` against four fixed mappings, the path of `v12n explain` |

The crate is outside the repository's workspace, so the stable builds,
the MSRV check and `cargo deny` do not build it. It needs a nightly
toolchain and `cargo-fuzz`:

```sh
rustup toolchain install nightly
cargo install cargo-fuzz
fuzz/seed.sh                                  # queries and mappings of tests/diff and examples
cargo +nightly fuzz run query_plan -- -max_total_time=600
```

The `Fuzz` workflow runs every target for ten minutes each week, and for
one minute on pull requests that change `fuzz/`. A crash fails the job
and its input is uploaded as the artifact `fuzz-<target>`.

`seed.sh` copies the test queries and mappings into `fuzz/corpus/`, and
crashes go to `fuzz/artifacts/`; neither is committed. A panic, or an
input that exceeds a time or memory bound, is a bug: fix it and add the
input as a regression test next to the code it reaches.
