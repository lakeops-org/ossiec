# Adding an engine

An engine is *compile-checked* once polyglot renders its dialect, and *verified* once every test tier runs on it and agrees with DuckDB. Moving one up takes four steps:

1. **Start it.** Add the image and a readiness check to `scripts/engines.sh`.
2. **Teach the harness the engine.** Add an `Engine` variant in `tests/common/engines.rs`, its DDL differences in `seed_sql` (`tests/common/shop.rs`), and a runner in `run_in` that prints NULL differently from an empty string.
3. **Run the tiers and fix what differs.** Every difference becomes a dialect rule in `src/dialect/`, with the failing case as its evidence, or a refusal in `src/functions.rs` when the engine has no faithful form. Never a speculative rule.
4. **Record the gaps.** Add the engine's expression-language gaps to `gaps()` in `tests/expression_language.rs`, add it to `SITE_ENGINES` there, and regenerate the [Functions by engine](generated/functions.md) page (`OSSIEC_UPDATE_SITE=1 cargo test --test expression_language`) to give it a column.

The full guide is [ADDING_AN_ENGINE.md](https://github.com/lakeops-org/ossiec/blob/main/docs/ADDING_AN_ENGINE.md).
