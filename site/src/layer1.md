# Layer 1: the expression language

The SQL inside a model's fields and metrics is Ossie's portable expression language ([`expression_language.md`](https://github.com/apache/ossie/blob/main/core-spec/expression_language.md)): operators, functions and aggregates with a meaning the spec fixes, independent of any engine.

Engines disagree on many of these functions. `LENGTH` counts bytes on MySQL, `DAYOFWEEK` starts the week on a different day, `CONCAT` treats NULL differently, and some engines have no exact percentile at all. A translator that maps names one to one gets those wrong silently. So ossiec gives every Ossie function an implementation per engine ([`src/functions.rs`](https://github.com/lakeops-org/ossiec/blob/main/src/functions.rs)):

- **A standard form** in core SQL, used wherever it means the same thing.
- **An engine's own form** where it needs one: `CHAR_LENGTH` on MySQL, the UTF-8 string functions on ClickHouse, a `CAST(… AS BIGINT)` around MySQL's unsigned `YEAR`.
- **A refusal** where the engine has no form that gives the same answer. A query never gets a different answer silently.

The result, construct by construct and engine by engine, is on [Functions by engine](generated/functions.md). That page is generated from the test suite, which runs every construct on every engine against a value worked out by hand.

## Rules enforced when a model loads

| Rule | What ossiec does |
|---|---|
| Identifiers | Regular names are upper-cased and quoted ones kept, so `"ID"` matches `id` and `"id"` doesn't. At most 128 characters. |
| `source.column` | A dataset-scoped metric may read a source column directly. |
| `AGG` is reserved | A portable expression can't call a function named `AGG`. |
| No subqueries | A portable expression can't contain one. |
| Division | `/` is true division on every engine: `COUNT(a) / COUNT(b)` isn't truncated on Postgres and Trino. |
| `FILTER (WHERE …)` | Kept on engines that run it (DuckDB, Postgres, Trino, Athena, Databricks), and rewritten to `CASE` elsewhere. |
| `STDDEV`, `VARIANCE` | The sample forms, on every engine. |
| Vendor dialects | A field may carry `SNOWFLAKE`, `TRINO`, … variants beside the portable one; `--prefer SNOWFLAKE` uses the native one. A vendor expression keeps its engine's meaning and isn't rewritten. |

## Engine behaviours ossiec normalizes

- **Strings compare and sort by byte on every engine.** Postgres string dimensions carry `COLLATE "C"`, and MySQL ones `COLLATE utf8mb4_bin`.
- **NULLs sort first in ascending order**, as in generic SQL; `NULLS FIRST` is emitted where an engine defaults otherwise.
- **A backslash in a `LIKE` pattern is an ordinary character.** On engines that treat it as an escape (Postgres, ClickHouse, StarRocks, MySQL), ossiec doubles it.
- **ClickHouse's `lag`** returns 0 instead of NULL for the first row; ossiec's dialect rule restores NULL.
