# Known gaps and roadmap

ossiec refuses what it can't answer the spec's way, with a message that says so. Everything below is a refusal today unless it says otherwise.

## Layer 2

- **Set operators** (`UNION`, `INTERSECT`, `EXCEPT`), even over fields only (§7.8)
- **`ROLLUP`, `CUBE`, `GROUPING SETS`** (§7.4)
- **Subqueries in `WHERE`, `HAVING` and `ORDER BY`**, including scalar subqueries that read `AGG()` (§7.3, §7.7)
- **Arithmetic on plain aggregates**, alone or with `AGG()`, and plain aggregates in `HAVING` (§7.2, §7.5)
- **`WHERE` on one of several independent facts' columns** (§7.3)
- **A fan-out written from the many side**: `order_items i LEFT JOIN orders o` reading `AGG(o.order_count)`. The same join written `orders o RIGHT JOIN order_items i` works.
- **`ASOF`, `SEMI`/`ANTI`, `NATURAL` and `LATERAL` joins** (§7.6)
- **Comma `FROM`** *(answers differently, not refused)*: SQL reads it as a cross join; ossiec reads it as independent facts sharing their dimensions
- **Measure names in quotes**: `AGG("m")` matches `m`, ignoring the quotes
- **Persisting a view** judges its joins by how they're written, so `orders RIGHT JOIN customers` is refused though it means the same as `customers LEFT JOIN orders`, which persists

## Layer 1

- **Exact percentiles on MySQL and Trino**, which have no such function: `MEDIAN`, `PERCENTILE_CONT`, `PERCENTILE_DISC`
- **`TRY_CAST`** on Postgres and MySQL, **`REGEXP_COUNT`** and **`APPROX_PERCENTILE`** on MySQL
- **StarRocks**: `UPPER` of a non-ASCII column changes ASCII letters only (a wrong answer, and the one allowed), and `NTH_VALUE` fails with an engine error

## Layer 3

- **Appendix A options not offered**: one-to-many paths, per-metric overrides, grouping sets
- **A SQL table-function binding** (Appendix A.2)
- **Nested windows** in metrics

## Semantics to know

These are by design, from the spec:

- **De-duplicated groups overlap (Layer 2).** A product sold in two regions counts in both, so per-group values don't add up to a grand total. ossiec warns with `FAN_TRAP_RISK`.
- **Window metrics see only the rows in the result.** A running total over a filtered date range starts at the range's first row.
- **In Layer 3, the groups depend on the metrics you ask for** (§6.1). In Layer 2, the query's joins decide them.
- **Time grains ignore time zones.** Truncating a `DateTimeTz` uses the engine's session time zone.

The live list is the [roadmap in the README](https://github.com/lakeops-org/ossiec#roadmap).
