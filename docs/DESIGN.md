# ossiec design

This document explains how ossiec turns an Apache Ossie model and a request into SQL, and why each rule exists. The README covers usage.

## What Ossie does and doesn't define

Ossie's core spec (`0.2.0.dev0`) defines a **model**: datasets, fields, relationships and metrics, with expressions in a portable SQL subset (`OSSIE_SQL_2026`). It does not define a query format, join resolution or SQL generation. Its draft query layers (`spec_layers.md`) are:

| Layer | What | ossiec |
|---|---|---|
| 1 | Model and expression language | Read in full |
| 2 | SQL with `AGG(measure)` (draft, `relational_semantics.md`) | Not yet |
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

1. `R` = every dataset referenced by the requested dimensions, filters and metric terms.
2. Candidates = datasets from which every member of `R` is reachable along many→one edges.
3. Root = the candidate inside `R`. If there is none (for example, a one-side metric grouped by another dimension), it's the deepest candidate outside `R`: the one every other candidate reaches.
4. Each member of `R` must be reachable from the root by **exactly one** path. Two paths (role-playing dimensions, such as departure and arrival airport) are refused with both paths named.
5. Joins are `LEFT JOIN`s from the root, so adding a dimension never drops fact rows with a NULL or dangling key; they land in the NULL group.

No candidate means the request spans independent facts. That is refused, not silently cross-joined.

### Dimension-only requests

A request with dimensions but no metrics lists their distinct values: one part with no terms, `SELECT dims … GROUP BY dims`. The join-root rule decides what that means. One dataset roots at itself, so it lists all of that dataset's values. Dimensions spread over several datasets root at the fact joining them, so only combinations that occur are listed.

### Term strategies

| Strategy | When | Why it's correct |
|---|---|---|
| `Inline` | The term reads the root dataset, or is duplicate-insensitive (`MIN`, `MAX`, `COUNT(DISTINCT …)`, `APPROX_COUNT_DISTINCT`, `ANY_VALUE`) | Each root row appears once after many→one joins, and duplicates can't change a distinct-insensitive aggregate |
| `PkDedup` | The term reads only one-side datasets (`SUM(store.employees)` over a `store_sales` root) | Joined rows repeat one-side values; the term is evaluated over `SELECT DISTINCT dims, <primary key>, <inputs>`, which is spec §9's reference strategy |

`PkDedup` needs a `primary_key` on the term's grain dataset (the term's dataset that reaches its others). Without one the request is refused. It always adds a `FAN_TRAP_RISK` warning: a one-side row in several groups counts in each, so the groups don't sum to a grand total.

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

ossiec keeps one shared generator and a thin per-dialect layer, [`src/dialect.rs`](../src/dialect.rs), which starts empty. A rule is added only when an engine test shows polyglot's rendering runs but answers differently, and it names that test.

| Rule | Evidence |
|---|---|
| ClickHouse `LAG`/`LEAD` → `lagInFrame`/`leadInFrame(toNullable(x), n)` over the full frame | `running_total_and_change`: `LAG` doesn't resolve, and `lag` returns `0` instead of NULL for the first row |
| ClickHouse `SETTINGS join_use_nulls = 1` on the query (`finish`, after rendering, because generic SQL can't carry `SETTINGS`) | `orphans_on_a_non_nullable_column` (the guest checkout lands under segment `''`) and `integer_ratio` (`COUNT(DISTINCT customers.id)` counts the default id `0` as a customer) |

Engine-neutral rules that came out of the same tests live in binding and planning instead:
- A time grain on a `Date` field is cast back to `DATE`, because Postgres returns `timestamptz` and StarRocks `DATETIME` (`month_grain_with_date_range`).
- `/` in portable expressions is true division. Postgres and Trino truncate `7 / 2` to `3`, while DuckDB, StarRocks and ClickHouse return `3.5`. The numerator is cast to `DOUBLE` unless one side is a fractional literal, which keeps `amount_cents / 100.0` exact (`integer_ratio`). Vendor-dialect expressions keep their engine's own semantics.

Rules have two hooks: `adapt` rewrites the neutral AST before rendering, and `finish` edits the rendered SQL in the target's own dialect, for syntax only that dialect has.

Rules apply only when a target is given. Callers that translate downstream set no target and take the source dialect, so they should compile for the routed engine to get the rules.

## Testing

| Tier | File | Catches |
|---|---|---|
| Correctness | `tests/correctness.rs` | Wrong answers. Each case runs on DuckDB against seed data built to trigger fan-out and NULL keys, and compares with hand-written reference SQL. The fan-out cases also check that the naive join gives a *different* answer, so the test can fail. |
| Drift | `tests/golden.rs`, `tests/golden/*.sql` | Unintended SQL changes, across dialects. Re-snapshot with `UPDATE_GOLDEN=1`. |
| Model | `tests/model.rs` | Spec conformance: every vendored Ossie example binds, every compliance metric compiles, one test per validation rule. |
| Engines | `tests/engines.rs` | Engine disagreements. Every correctness case runs on Postgres, Trino, StarRocks and ClickHouse (Docker, `scripts/engines.sh up`) and must match DuckDB's reference rows exactly, including types. |
| Requests | `tests/requests.rs` | Error messages, refusals, 11 engines, determinism. |

`scripts/sync-ossie-fixtures.sh` re-vendors the Ossie examples, so spec drift shows up as a failing test.

## Not yet

| Feature | Shape it will take |
|---|---|
| Independent facts (orders and tickets by region) | One stacked part per root, reusing the `UNION ALL` combine |
| Role-playing dimensions | Path selection in the request; one alias per path |
| "Parents without children" for `PkDedup` | Add unmatched one-side rows as their own group |
| Layer 2 `AGG()` SQL | A frontend that produces the same plan |
