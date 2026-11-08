# Testing

Every claim on this site is a test that executes SQL and compares the rows with a reference.

| Suite | What it proves |
|---|---|
| `correctness` | **The numbers are right.** Requests run on DuckDB against seed data built to break naive SQL (fan-out, NULL keys, a deleted customer, a guest checkout, a product that never sold), compared with hand-written reference SQL. |
| `engines` | **Every engine agrees.** The same cases, and the Layer 2 cases, on Postgres, Trino, StarRocks, ClickHouse and MySQL in Docker, compared row by row with DuckDB as printed values. |
| `expression_language` | **Every function, on every engine.** 400+ constructs, each against a value worked out by hand. Known gaps are listed, and the test fails when one closes, so the list only shrinks. |
| `sql_front` | **Layer 2.** Every spec section and appendix, each query run on DuckDB against a plain-SQL reference. See [Layer 2 test evidence](generated/layer2-tests.md). |
| `wide_table` | **Layer 3's own examples and rejections**, on the spec's own data, including every error in its order. |
| `tpcds` | **Real data and the benchmark's own SQL.** Ossie's TPC-DS example model unchanged, and official TPC-DS queries expressed as requests, on five engines. |
| `fuzz` | **Cases nobody wrote.** Random requests, each decided independently from the model: refused ones must raise the right code, answered ones must match the spec's reference strategy, on all six engines. |
| `model`, `query_compliance` | **Spec conformance.** Every Ossie example binds; every compliance case returns its expected answer. |
| `golden` | **The SQL didn't drift**, across six dialects. |

```console
cargo test                                                        # everything on DuckDB
scripts/engines.sh up                                              # start the other five engines in Docker
cargo test --test engines -- --ignored                             # Layer 3 and Layer 2 cases on them
cargo test --test expression_language -- --ignored                 # every function on them
cargo test --test fuzz -- --ignored                                # random requests on them
cargo test --test tpcds -- --include-ignored                       # TPC-DS on all engines
OSSIEC_UPDATE_SITE=1 cargo test                                    # regenerate this site's generated pages
```

## How this site stays true

[Functions by engine](generated/functions.md) and [Layer 2 test evidence](generated/layer2-tests.md) are written by tests. Each test also fails when its committed page differs from what it would write, so a change that closes a gap or adds a test must update the site in the same commit.
