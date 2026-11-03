# ossiec design

This document explains how ossiec turns an Apache Ossie model and a request into SQL, and why each rule exists. The README covers usage.

## What Ossie does and doesn't define

Ossie's core spec (`0.2.0.dev0`) defines a **model**: datasets, fields, relationships and metrics, with expressions in a portable SQL subset (`OSSIE_SQL_2026`). It does not define a query format, join resolution or SQL generation. Its draft query layers (`spec_layers.md`) are:

| Layer | What | ossiec |
|---|---|---|
| 1 | Model and expression language | Read in full |
| 2 | SQL with `AGG(measure)` (draft, `relational_semantics.md`) | Appendix A.1 via `compile_layer2` / `compile-l2` |
| 3 | Wide table: metrics + dimensions + filters in, joins resolved (draft PR #246) | **This is ossiec's request format** |

The planner follows the same guarantee Layer 2 states (spec §8.3, "G2"): **a measure is aggregated over its own source rows, never over rows a join duplicated.**

## Pipeline

```
YAML ──load──► Model ──bind──► SemanticGraph ──plan(Request)──► LogicalPlan ──lower──► AST ──render──► SQL
       model.rs        graph.rs                 plan.rs                       lower.rs        sql.rs
```

| Stage | Input → output | Fails with |
|---|---|---|
| `load` | text → serde `Model`. Unknown keys are ignored. Pre-0.2 and ontology documents are rejected. | `Error::Load` |
| `bind` | `Model` → `SemanticGraph`. Every expression is parsed and every name resolved; all problems are reported together. | `Error::InvalidModel` |
| `plan` | request → `LogicalPlan`. All semantic decisions are made here. | `Error::Request`, `Error::Unsupported` |
| `lower` | plan → polyglot AST. Mechanical. | — |
| `render` | AST → SQL in the source dialect, then transpiled to the target by polyglot | `Error::Unsupported` |

## Binding

- **Expressions.** For each field and metric, ossiec keeps every variant it can read: `sql`/`OSSIE_SQL_2026` (portable), `ANSI_SQL`, and any vendor dialect polyglot knows (`SNOWFLAKE`, `TRINO`, `STARROCKS`, ...). Non-SQL dialects (`DAX`, `MDX`, `TABLEAU`, ...) are ignored. A field with no readable variant is an error.
- **Choosing a variant**, per compile: the `prefer_dialect` entry if there is one, then portable, then ANSI, then the first vendor entry. If the chosen variants need two different vendor dialects, the request fails; one statement has one source dialect.
- **Names** follow ANSI identifier rules (spec "Name Resolution"): bare identifiers match case-insensitively, quoted ones exactly. Field expressions see source columns. Dataset-scoped metrics see their own fields, unqualified. Model-scoped metrics see `dataset.field`. A bare reference in a model-scoped metric is accepted with a warning when exactly one dataset has that field, because the compliance fixtures use that form.
- **Metrics are split** into aggregate *terms* and an *outer* expression over them. `SUM(a.x) / NULLIF(COUNT(DISTINCT b.y), 0)` becomes two terms and `t0 / NULLIF(t1, 0)`. Each term's field references are replaced by the field's SQL, qualified by its dataset alias (polyglot adds parentheses by precedence).
- **`FILTER (WHERE p)` is lowered** to `CASE WHEN p THEN x END` inside the aggregate. The expression language allows this rewrite, and several engines (StarRocks, MySQL, ClickHouse, Redshift) don't support `FILTER`.
- **Window metrics** are split differently. Each window function is set aside first, so its own aggregate (the outer `SUM` of `SUM(SUM(x)) OVER (…)`) stays part of the window. Aggregates inside it become ordinary terms. Fields it names outside an aggregate (`PARTITION BY store.id`, `ORDER BY date.d`) become *dimension references*. Window functions can't be nested.

## Planning

### The join root

Relationships go from the many side (`from`) to the one side (`to`). Following them in that direction never multiplies rows of the dataset you start from.

Requests name **nodes**: a dataset, or a dataset playing a role (`customer_first_sales.d_year` is `date_dim` reached through the `customer_first_sales` relationship). Each node gets its own alias (`date_dim`, `date_dim__customer_first_sales`), so one request can use several roles at once.

1. `R` = every node referenced by the requested dimensions, filters and metric terms. A role node needs its relationship's source.
2. Candidates = datasets from which every member of `R` is reachable along many→one edges.
3. Root = the candidate inside `R`. If there is none (for example, a one-side metric grouped by another dimension), it's the deepest candidate outside `R`: the one every other candidate reaches.
4. A plain dataset reachable more than one way takes the **unique shortest path** (from `store_sales`, `date_dim` is the one-hop sale date, not a customer date two hops away). A tie is refused with the relationship names, so the caller can name the role.
5. Joins are `LEFT JOIN`s from the root, so adding a dimension never drops fact rows with a NULL or dangling key; they land in the NULL group.

### Legs: independent facts

When no dataset reaches all of `R` but the request has measures, each term gets the join tree for its own datasets plus every dimension and filter. Terms whose trees share a root share a **leg**. Each leg contributes its own base part (and its terms' de-duplicated parts), and all parts are stacked as usual, so groups are the union across facts and a metric can combine terms from different legs (`SUM(web) / SUM(store)`). Dimensions or filters several facts could link, with no measure to choose, are refused as ambiguous.

### Dimension-only requests

A request with dimensions but no metrics lists their distinct values: one part with no terms, `SELECT dims … GROUP BY dims`. The join-root rule decides what that means. One dataset roots at itself, so it lists all of that dataset's values. Dimensions spread over several datasets root at the fact joining them, so only combinations that occur are listed.

### Term strategies

| Strategy | When | Why it's correct |
|---|---|---|
| `Inline` | The term reads the root dataset, or is duplicate-insensitive (`MIN`, `MAX`, `COUNT(DISTINCT …)`, `APPROX_COUNT_DISTINCT`, `ANY_VALUE`) | Each root row appears once after many→one joins, and duplicates can't change a distinct-insensitive aggregate |
| `PkDedup` | The term reads only one-side datasets (`SUM(store.employees)` over a `store_sales` root) | Joined rows repeat one-side values; the term is evaluated over `SELECT DISTINCT dims, <primary key>, <inputs>`, which is spec §9's reference strategy |

`PkDedup` needs a `primary_key` on the term's grain dataset (the term's dataset that reaches its others). Without one the request is refused. It always adds a `FAN_TRAP_RISK` warning: a one-side row in several groups counts in each, so the groups don't sum to a grand total.

**Unmatched rows (`includeUnmatched`).** A de-duplicated part sees only one-side rows some fact row reaches. With `includeUnmatched`, its rows also include, through `UNION ALL`, the one-side rows no fact row reaches: the grain dataset and what it joins to, under `…__unmatched` aliases, with fact-side columns NULL, kept by `NOT EXISTS` against the whole leg. Filters apply to those rows too, so a fact-side filter excludes them. Every one-side term is de-duplicated in this mode, including the duplicate-insensitive ones that are normally inline, so they can take the extra rows.

### Assembly

```text
one part:      SELECT dims, outer(base.tN) FROM (part) AS base [WHERE metric filters]
several parts: SELECT dims, outer(MAX(stacked.tN))
               FROM (part UNION ALL part ...) AS stacked GROUP BY dims [HAVING metric filters]
```

Each part groups by the same dimensions and projects `NULL` for the terms it doesn't compute. Parts are stacked, not joined: there is no join predicate to get wrong, NULL dimension values group correctly, and each part scans its tables once.

### Windows

A window runs in the final `SELECT`, over one row per group, after every part is computed and stacked:

```text
SELECT d0, SUM(base.t0) OVER (ORDER BY base.d0 ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW) AS cumulative
FROM (part) AS base
```

- Each dimension reference must be a requested dimension; the planner maps it to that output column (`base.dN`, or `stacked.dN`, which is grouped). If it isn't requested, the request fails and names the missing dimension. `describe()` lists each metric's `requires_dimensions`.
- With a time grain, the window orders by the truncated value, so a monthly running total orders by month.
- SQL evaluates windows after `WHERE` and `HAVING`. So when any output has a window, metric filters move to a wrapper, `SELECT * FROM (…) AS windowed WHERE "rank" <= 3`. That gives "top 3 of the full ranking" rather than "rank among the rows that survived a filter".
- Windows see only the groups in the result. Dimension filters shape the window input, as in plain SQL.

## SQL construction

Query skeletons are SQL templates containing only keywords, generated aliases (`d0`, `t0`, `base`) and placeholders (`__ossiec_0`). Each template is parsed by polyglot, and the placeholders are replaced with AST nodes. Model and request content never becomes SQL text: field expressions are parsed ASTs, table names become `TableRef` nodes, filter values become literal nodes, and output names become quoted identifiers.

Rendering generates the AST in the statement's source dialect (`generic` unless a vendor expression was used), keeping function-name case, then transpiles to the target with polyglot. polyglot's `transpile` also applies target-specific normalizations (integer division, `ORDER BY ALL` and more) that aren't exposed separately, so rendering text and re-parsing it is the supported path, not a shortcut.

### Dialect rules

ossiec keeps one shared generator and a thin per-dialect layer, [`src/dialect/`](../src/dialect/): one file per engine (`clickhouse.rs`, `postgres.rs`, ...), each with the same two hooks. It started empty. A rule is added only when an engine test shows polyglot's rendering runs but answers differently, and it names that test.

| Rule | Evidence |
|---|---|
| ClickHouse `LAG`/`LEAD` → `lagInFrame`/`leadInFrame(toNullable(x), n)` over the full frame | `running_total_and_change`: `LAG` doesn't resolve, and `lag` returns `0` instead of NULL for the first row |
| ClickHouse `SETTINGS join_use_nulls = 1` on the query (`finish`, after rendering, because generic SQL can't carry `SETTINGS`) | `orphans_on_a_non_nullable_column` (the guest checkout lands under segment `''`) and `integer_ratio` (`COUNT(DISTINCT customers.id)` counts the default id `0` as a customer) |
| Postgres: typed `NULL`s in stacked parts (`CAST(NULL AS NUMERIC)`, from each term's type family) | `three_parts_with_a_one_side_filter`: Postgres types `UNION` branches pairwise, so two bare `NULL`s become `text` and clash with `bigint` |
| Postgres: string dimensions output with `COLLATE "C"` | `string_ordering`: en_US collation sorts `ada` before `Bob`; every other engine sorts by byte |
| StarRocks: backslashes in string literals doubled | `string_literals_with_quote_and_backslash`: StarRocks reads `\` as an escape, and polyglot doesn't double it for StarRocks (it does for MySQL) |
| BigQuery: `DATE_TRUNC` week → `ISOWEEK` | Ossie's week starts on Monday; BigQuery's `WEEK` starts on Sunday. BigQuery isn't executed, so this is checked by golden SQL only |
| Postgres, ClickHouse, StarRocks, MySQL: backslashes in `LIKE` patterns doubled (shared helper in `dialect/mod.rs`) | `like_with_a_backslash`: these engines escape with `\` by default; SQL without `ESCAPE` (Trino, DuckDB) treats it as an ordinary character |
| MySQL: string dimensions and filters `COLLATE utf8mb4_bin` | `string_ordering` (case-insensitive grouping merged `Ada` and `ada`) and `case_sensitive_filter` (`'ada'` matched `Ada`) |
| MySQL: `AVG` argument cast to `DOUBLE` | `averages_of_integers_and_one_side_values`: `AVG` of integers rounds to 4 decimals |
| MySQL: `NULLS FIRST/LAST` → leading `CASE WHEN x IS NULL THEN 1 ELSE 0 END` key | TPC-DS `q12`: MySQL has no `NULLS LAST` syntax and polyglot drops it ([polyglot#515](https://github.com/tobilg/polyglot/issues/515)) |
| ClickHouse: `CAST(NULL AS T)` → `CAST(NULL AS Nullable(T))` (`finish`) | `unmatched_products_included`: `Cannot convert NULL to a non-nullable type`; polyglot wraps some types only ([polyglot#514](https://github.com/tobilg/polyglot/issues/514)) |

Engine-neutral rules that came out of the same tests live in binding and planning instead:
- **Groups never depend on which measures are asked for** (spec §8.4, closure). With dimensions, the base part always runs, even when it computes no terms, so groups come from every joined row. Evidence: `one_side_measure_dedup`, which used to drop a region whose orders had no product.
- **`COUNT` over no rows is 0** (spec §8.1). In stacked parts a missing group reads `COALESCE(MAX(tN), 0)` for `COUNT`-like terms. Evidence: `count_is_zero_for_a_group_without_rows`.
- A time grain on a `Date` field is cast back to `DATE`, because Postgres returns `timestamptz` and StarRocks `DATETIME` (`month_grain_with_date_range`).
- `/` in portable expressions is true division. Postgres and Trino truncate `7 / 2` to `3`, while DuckDB, StarRocks and ClickHouse return `3.5`. The numerator is cast to `DOUBLE` unless one side is a fractional literal, which keeps `amount_cents / 100.0` exact (`integer_ratio`). Vendor-dialect expressions keep their engine's own semantics.
- Division by zero is NULL: a non-literal divisor is wrapped in `NULLIF(…, 0)`. Otherwise Postgres errors and Trino returns `NaN` (TPC-DS `q12` on Trino).
- With `includeUnmatched`, one `DISTINCT` runs over the matched and unmatched rows, not one per `UNION ALL` branch. ClickHouse before 26.5 prunes a branch's `DISTINCT` columns to the ones the outer query reads, so items with equal prices collapsed (`unmatched_items_included`; fixed upstream in ClickHouse#104114).

Rules have two hooks: `adapt` rewrites the neutral AST before rendering, and `finish` edits the rendered SQL in the target's own dialect, for syntax only that dialect has.

Rules apply only when a target is given. Callers that translate downstream set no target and take the source dialect, so they should compile for the routed engine to get the rules.

## Testing

| Tier | File | Catches |
|---|---|---|
| Correctness | `tests/correctness.rs` | Wrong answers. Each case runs on DuckDB against seed data built to trigger fan-out and NULL keys, and compares with hand-written reference SQL. The fan-out cases also check that the naive join gives a *different* answer, so the test can fail. |
| Drift | `tests/golden.rs`, `tests/golden/*.sql` | Unintended SQL changes, across dialects. Re-snapshot with `UPDATE_GOLDEN=1`. |
| Model | `tests/model.rs` | Spec conformance: every vendored Ossie example binds, every compliance metric (M0–M3) compiles **and returns the expected answer on DuckDB**, one test per validation rule. |
| Engines | `tests/engines.rs` | Engine disagreements. Every correctness case runs on Postgres, Trino, StarRocks, ClickHouse and MySQL (Docker, `scripts/engines.sh up`) and must match DuckDB's reference rows exactly, including types. |
| TPC-DS | `tests/tpcds.rs` | Wrong answers at realistic scale, judged against the benchmark itself. Covers Ossie's TPC-DS example model, unchanged; multi-hop joins (`store_sales → customer → customer_address`), a composite-key relationship and composite primary-key de-duplication (`store_returns → store_sales` on item and ticket); and official queries q3, q12, q42, q43, q52, q55, q96 and q98 compared with TPC-DS's own SQL from DuckDB's `tpcds_queries()`. Runs on six engines. Each reference is transpiled from DuckDB SQL and runs on the engine under test, so data-generator differences cancel out. References avoid `LAG`, `IS NOT DISTINCT FROM` and unaliased derived tables, so they never depend on the rules being tested. |
| Fuzz | `tests/fuzz.rs` | Planner edge cases nobody thought to write. Random valid requests (metrics × dimensions × filters × grains, fixed seed) compared with an independent reference generated straight from the spec's §9 strategy: groups from all joined rows, and each measure as a correlated subquery over rows de-duplicated to its identity. 100 per run on DuckDB (`OSSIEC_FUZZ=n` for more), 40 per engine on the others. |
| Scale | `tests/scale.rs`, `tpcds::trino_sf1_performance` | Super-linear compile time (a 121-table model) and plans that are right but slow (each TPC-DS case on Trino sf=1 must run within 3× its reference). |
| Requests | `tests/requests.rs` | Error messages, refusals, 11 engines, determinism. |

`scripts/sync-ossie-fixtures.sh` re-vendors the Ossie examples, so spec drift shows up as a failing test.

## Not yet

| Feature | Shape it will take |
|---|---|
| Layer 2 consumer joins / subqueries (A.2–A.7) | Extend `layer2.rs` beyond single-relation A.1 |
