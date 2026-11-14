# Known gaps and roadmap

ossiec refuses what it can't answer the spec's way, with a message that says so. Everything below is a refusal today unless it says otherwise.

## Layer 2

- **Set operators** (`UNION`, `INTERSECT`, `EXCEPT`), even over fields only (§7.8)
- **`ROLLUP`, `CUBE`, `GROUPING SETS`** (§7.4)
- **`SELECT *`** (§7.1)
- **Subqueries in the select list, `HAVING` and `ORDER BY`**, including scalar subqueries that read `AGG()`, and **correlated subqueries** anywhere (§7.3, §7.7)
- **`WHERE` on one of several independent facts' columns**, beyond a comparison, `IN`, `BETWEEN` or `LIKE` on a plain column: `IS NULL`, `NOT BETWEEN`, `NOT LIKE` and other predicates there, one on a field defined as an expression (`COALESCE(amount, 0)`), and one that reads two facts (§7.3)
- **`ASOF`, `SEMI`/`ANTI`, `NATURAL` and `LATERAL` joins** (§7.6)
- **Quoted references to a derived table's columns** *(lenient, not refused)*: `t."order_count"` matches the derived column `order_count` regardless of its quotes. Two columns of a relation can't differ by case alone, so it never finds a different column; a strict reading would refuse it
- **A `CROSS JOIN` spanning independent facts**, written out or as a comma `FROM`, and a comma `FROM` combined with a `RIGHT` or `FULL` join (§7.6)
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
