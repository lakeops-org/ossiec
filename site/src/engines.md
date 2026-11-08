# Engines

ossiec builds one SQL syntax tree and renders it for the target with [polyglot-sql](https://crates.io/crates/polyglot-sql), then applies its own dialect rules where a real engine was shown to answer differently.

## Verified by execution

These engines run every test tier, in Docker, and must return the same rows as the DuckDB reference. Values are compared as the engine prints them, not by column type, and NULL stays distinct from `''`. In the Layer 3, Layer 2 and TPC-DS cases, numbers are rounded to six decimal places and must agree to one part in a million, and dates must print identically. The expression-language tier also accepts a date that comes back as a midnight timestamp, and numbers within one part in 100,000.

| Engine | Version tested | Layer 3 cases | Layer 2 cases | Expression language | TPC-DS | Fuzzing |
|---|---|:-:|:-:|:-:|:-:|:-:|
| DuckDB | 1.5 | ✅ | ✅ | ✅ all | ✅ sf=0.1 | ✅ |
| Postgres | 16 | ✅ | ✅ | 🟡 [gaps](generated/functions.md#gaps) | ✅ | ✅ |
| Trino | 484 | ✅ | ✅ | 🟡 [gaps](generated/functions.md#gaps) | ✅ `tpcds.tiny` | ✅ |
| StarRocks | 4.1 | ✅ | ✅ | 🟡 [gaps](generated/functions.md#gaps) | ✅ | ✅ |
| ClickHouse | 26.8 | ✅ | ✅ | ✅ all | ✅ | ✅ |
| MySQL | 8.4 | ✅ | ✅ | 🟡 [gaps](generated/functions.md#gaps) | — | ✅ |

The expression-language gaps are refusals (ossiec won't compile what the engine can't express faithfully), apart from one engine error on StarRocks and one known wrong answer there: `UPPER` of a non-ASCII column, since StarRocks has no Unicode form. See [Functions by engine](generated/functions.md).

## Compile-checked

Snowflake, BigQuery, Databricks, Athena and Redshift get engine-native SQL and are checked for what ossiec must never emit (`UNION ALL BY NAME`, `QUALIFY`, `GROUP BY ALL`, an aggregate `FILTER` where the engine lacks it), but their answers aren't executed in CI. polyglot renders 20+ more dialects. [Adding an engine](adding-an-engine.md) explains how to move one up to verified.

**Known risks on engines not in the executed set:** Doris has StarRocks' backslash-literal gap, and Redshift probably has Postgres' untyped-`NULL` union issue. Neither has a dialect rule yet.

## Dialect rules

Each rule exists because an engine test showed polyglot's rendering answering differently, and names that test as its evidence:

| Engine | Difference | Rule |
|---|---|---|
| ClickHouse | No standard `LAG`/`LEAD`; its `lag` returns 0, not NULL, for the first row | `lagInFrame(toNullable(x), n)` over the whole partition |
| ClickHouse | An unmatched `LEFT JOIN` column is the type's default (0, `''`), not NULL, so orphan rows merge into real groups | `SETTINGS join_use_nulls = 1` on the query |
| Postgres | `UNION` branches are typed pairwise, so two bare `NULL`s become `text` and clash with a number | Each `NULL` takes its term's type |
| Postgres | Locale collation sorts `'ada'` before `'Bob'` | `COLLATE "C"` on string dimensions |
| MySQL | Default collation compares case-insensitively, merging `Ada` and `ada` | `COLLATE utf8mb4_bin` on string dimensions and filters |
| MySQL | `AVG` of integers is rounded to 4 decimal places | The argument is cast to `DOUBLE` |
| StarRocks | `\` in a string literal is an escape, and polyglot doesn't double it | Double it |
| Postgres, ClickHouse, StarRocks, MySQL | `\` in a `LIKE` pattern is an escape | Double it, so it matches itself |
| BigQuery | `DATE_TRUNC(d, WEEK)` starts weeks on Sunday; Ossie's start on Monday | `ISOWEEK` (checked by golden SQL only) |
| Engines without aggregate `FILTER` | `FILTER (WHERE …)` would be passed through | `CASE WHEN` inside the aggregate; also on every engine for statistical aggregates, whose `FILTER` polyglot drops |

The rules live in [`src/dialect/`](https://github.com/lakeops-org/ossiec/tree/main/src/dialect). The function implementations per engine are in [`src/functions.rs`](https://github.com/lakeops-org/ossiec/blob/main/src/functions.rs).
