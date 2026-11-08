# Adding an engine

ossiec renders SQL with [polyglot-sql](https://github.com/tobilg/polyglot), so compiling for a new engine usually needs no code. *Supporting* an engine means proving that its answers match. This guide covers that.

## How support is defined

| Level | Meaning | Engines today |
|---|---|---|
| **Executed** | Every correctness case runs on the engine in `tests/engines.rs` and returns exactly DuckDB's reference rows, including types and NULLs | DuckDB, Postgres, Trino, StarRocks, ClickHouse, MySQL |
| **Compiled** | Requests compile and contain no constructs the engine lacks (`tests/requests.rs`), but nothing has run there | Athena, Snowflake, BigQuery, Databricks, Redshift |
| **Renderable** | polyglot has the dialect; nothing in ossiec checks it | about 25 more |

Only "executed" is a claim about correctness.

## Case A: a polyglot dialect that runs in a container

This takes roughly half a day to a day, mostly spent on the test harness.

1. **Start it.** Add the image and a readiness check to [`scripts/engines.sh`](../scripts/engines.sh). Name the container `ossiec-<engine>`.
2. **Teach the harness the engine.**
   - In [`tests/common/engines.rs`](../tests/common/engines.rs), add an `Engine` variant mapped to polyglot's `DialectType`.
   - In `seed_sql` ([`tests/common/shop.rs`](../tests/common/shop.rs)), handle the engine's DDL differences: column types, table engine or properties, and date literal syntax. Keep columns non-nullable where the engine distinguishes them and the data has no NULLs; real tables look like that, and it's how the ClickHouse `lag` bug was found.
   - In `run_in` ([`tests/common/engines.rs`](../tests/common/engines.rs)), add a runner that uses the engine's own CLI inside the container. Its output **must print NULL differently from an empty string**. Trino's TSV doesn't, so its runner reads JSON.
   - Add a `#[test] #[ignore]` that calls `sweep(Engine::New)`.
3. **Run it.**
   ```console
   scripts/engines.sh up
   cargo test --test engines -- --ignored <engine>
   ```
4. **Triage each failure.**

   | What failed | Where the fix goes |
   |---|---|
   | The seed or the output format | The harness. Not a product bug. |
   | The engine differs, and the fix is right on every engine | The planner, for example the `CAST(DATE_TRUNC(...) AS DATE)` on `Date` grains, or true division |
   | The engine differs, and only it needs the fix | A rule in that engine's file, [`src/dialect/<engine>.rs`](../src/dialect/) (create it if needed): `adapt` before rendering, `finish` after. Add its arms in `src/dialect/mod.rs` |
   | polyglot renders it wrong | A rule or a guard in ossiec, **and** an upstream issue |

   Every rule's doc comment names the engine case that proves it's needed.
5. **Make sure the test can fail.** Disable your fix and check that the case fails. A case that passes either way proves nothing; that's how the gaps in the ClickHouse `join_use_nulls` evidence were caught.
6. **Run TPC-DS.** Add the engine to `load()` in [`tests/tpcds.rs`](../tests/tpcds.rs) (CSV from DuckDB, then the engine's bulk loader) and a `sweep(Engine::New)` test. If a reference fails rather than ossiec's SQL, make the reference portable instead of special-casing the engine.
7. **Fuzz it.** Add a `fuzz(Engine::New, count(40))` test in [`tests/fuzz.rs`](../tests/fuzz.rs).
8. **Lock it in.**
   - Add a golden for a representative request in [`tests/golden.rs`](../tests/golden.rs).
   - Add the dialect to the compile sweep in [`tests/requests.rs`](../tests/requests.rs).
   - Add the container to the CI `engines` job if the image is a reasonable size.
   - Move the engine to "Executed" in the README.

## Case B: cloud-only engines

Snowflake, BigQuery, Databricks, Redshift and Athena have no container. The steps are the same, except:

- The runner uses the engine's CLI or HTTP API, with credentials from environment variables (for example `OSSIEC_SNOWFLAKE_ACCOUNT`). The test skips with a clear message when they're missing.
- The seed creates tables in a scratch schema and drops them afterwards.
- Run these nightly, not on every PR: they cost money and are slower.
- Emulators exist for some of them (for example a community BigQuery emulator), but their fidelity is limited. Treat them as a smoke test, not as evidence.

## Case C: a dialect polyglot doesn't have

1. **Preferred:** contribute the dialect to polyglot upstream. Every polyglot user benefits.
2. **Otherwise:** polyglot's `CustomDialectBuilder` defines a dialect from the closest existing one plus overrides. `Options.target` currently takes polyglot's built-in `DialectType`, so this needs a small API change to accept a custom dialect handle. Open an issue first.

## Model-side vendor expressions

These need no work. An Ossie `dialects:` entry named after any polyglot dialect (`STARROCKS`, `TRINO`, `CLICKHOUSE`, ...) is recognized, and `--prefer <NAME>` uses it.

## Checklist

- [ ] Container and readiness check in `scripts/engines.sh`
- [ ] `Engine` variant, seed DDL and a runner that keeps NULL distinct from `''`
- [ ] Every case passes in `cargo test --test engines -- --ignored <engine>`
- [ ] TPC-DS passes in `cargo test --test tpcds -- --ignored <engine>`
- [ ] Each fix lives in the planner, `src/dialect/<engine>.rs`, or upstream, and names its evidence
- [ ] Each fix was disabled once to confirm its case fails
- [ ] Golden file, compile-sweep entry, CI job, README support level
