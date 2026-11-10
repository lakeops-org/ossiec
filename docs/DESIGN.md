# ossiec design

This document explains how ossiec turns an Apache Ossie model and a request into SQL, and why each rule exists. The README covers usage.

## What Ossie does and doesn't define

Ossie's core spec (`0.2.0.dev0`) defines a **model**: datasets, fields, relationships and metrics, with expressions in a portable SQL subset (`OSSIE_SQL_2026`). It does not define a query format, join resolution or SQL generation. Its draft query layers (`spec_layers.md`) are:

| Layer | What | ossiec |
|---|---|---|
| 1 | Model and expression language | Read in full. The rules are enforced (identifiers, `source.column`, reserved `AGG`, no subqueries). `tests/expression_language.rs` runs every construct on six engines, and lists what each one can't do. |
| 2 | SQL with `AGG(measure)` (draft, `relational_semantics.md`) | Appendix A.1; A.4 (multi-fact measures sharing dimensions and `AGG` arithmetic in the select list); A.5 (consumer joins, including unused many→one and reverse fan-out with G2 via `PkDedup`); A.6 (`INNER`/`LEFT`) and §7.6 (`RIGHT`/`FULL`/`CROSS`: the join is emitted as written, flipped to `RIGHT`/`LEFT` when the plan roots at the side the consumer wrote second, and a non-duplicate-insensitive measure is primary-key de-duplicated so null-extension and Cartesian products do not inflate it). A.2 (derived table joined into an `AGG` stage) and A.3 (outer `AVG`/`SUM`/`COUNT`/`MIN`/`MAX` over derived tables), as one recursive compile. §7.7 unevaluated-measure pass-through and query-local `TO MEASURE` (an ad-hoc `AGG()` composition, or a base `SUM\|COUNT\|AVG\|MIN\|MAX(...)`, in a derived table, consumed by an outer `AGG`). Role-playing dimensions (`relationship.field`) and `includeUnmatched` (an `Options` flag or `-- ossiec: includeUnmatched`), including derived joins that attach inside the unmatched subtree. A.7 `CREATE VIEW` measure relations in a session `Layer2Catalog` (`define_layer2_view` / `compile_sql_with`); `persist_layer2_view` / `persist-view` writes `TO MEASURE` items into the model YAML. `compile_sql` / `compile-sql` (aliases: `compile_layer2` / `compile-l2`); `layer2_to_request` exposes leaf measures of flat queries only (nested SQL is `Unsupported` there). |
| 3 | Wide table: metrics + dimensions + filter in, joins resolved (draft, `wide_table_semantics.md`) | Branches per home dataset, the `filter` expression, and every §8 rejection and its order (`wide_table.rs`), with ossiec's own explicit properties on top (§9). Aliases and output names (§3.3): columns are named by alias or final part, as Layer 2's are (§3.1). |

The planner follows the same guarantee Layer 2 states (spec §8.3, "G2"): **a measure is aggregated over its own source rows, never over rows a join duplicated.**

## Pipeline

```
YAML ──load──► Model ──bind──► SemanticGraph ──plan(Request)──► LogicalPlan ──lower──► AST ──render──► SQL
       model.rs        graph/                   plan/                         lower.rs        sql.rs
```

### Source layout

| Module | Concern |
|---|---|
| `model.rs` | The Ossie YAML as serde types |
| `graph/` | A bound model: `bind.rs` (validation, sources, dialect variants), `metric.rs` (metrics into terms), `describe.rs` (the catalog API), `mod.rs` (types and accessors) |
| `request.rs` | The semantic request and its JSON (Layer 3's query) |
| `plan/` | Decisions: `mod.rs` (`plan()`: names, terms, strategies, outputs), `legs.rs` (join roots and trees) |
| `protect.rs` | Keeping measures duplicate-free over joins that add rows (§7.6, G2) |
| `wide_table.rs` | Layer 3 rejections (§8) |
| `lower.rs` | Plan → SQL AST, mechanically |
| `sql.rs` | AST helpers: templates, quoting, aggregate `FILTER` |
| `dialect/` | Per-engine adaptation; `filter.rs` rewrites `FILTER` for engines without it |
| `functions.rs`, `portable.rs` | Layer 1 functions per engine, and the portable expression parser |
| `front/` | Layer 2 SQL: `mod.rs` (entry points, shared types), `stage.rs` (one `AGG` stage), `from.rs` (`FROM`/`JOIN`, relationship matching), `resolve.rs` (naming), `measures.rs` (what is a measure), `formula.rs` (`AGG` arithmetic), `clauses.rs` (`WHERE`/`HAVING`/`ORDER BY`), `joins.rs` (consumer joins on the plan), `nested.rs` (derived tables), `fold.rs`, `shell.rs`, `cte.rs`, `to_measure.rs`, `catalog.rs` (A.7 views), `persist.rs` |

| Stage | Input → output | Fails with |
|---|---|---|
| `load` | text → serde `Model`. Unknown keys are ignored. Pre-0.2 and ontology documents are rejected. | `Error::Load` |
| `bind` | `Model` → `SemanticGraph`. Every expression is parsed and every name resolved; all problems are reported together. | `Error::InvalidModel` |
| `plan` | request → `LogicalPlan`. All semantic decisions are made here. A Layer 3 request first passes `wide_table::check`, §8 in order. | `Error::Query` (Layer 3, with its code), `Error::Request` (Layer 2), `Error::Unsupported` |
| `lower` | plan → polyglot AST. Mechanical. | — |
| `render` | AST → SQL in the source dialect, then transpiled to the target by polyglot | `Error::Unsupported` |

## Binding

- **Expressions.** For each field and metric, ossiec keeps every variant it can read: `sql`/`OSSIE_SQL_2026` (portable), `ANSI_SQL`, and any vendor dialect polyglot knows (`SNOWFLAKE`, `TRINO`, `STARROCKS`, ...). Non-SQL dialects (`DAX`, `MDX`, `TABLEAU`, ...) are ignored. A field with no readable variant is an error.
- **Choosing a variant**, per compile: the `prefer_dialect` entry if there is one, then portable, then ANSI, then the first vendor entry. If the chosen variants need two different vendor dialects, the request fails; one statement has one source dialect.
- **Names** match by their normalized form (expression language, "Comparison Table"): a regular identifier is upper-cased and a quoted one kept, so `"ID"` matches `id` and `"id"` doesn't. A model declares a quoted name as `name: '"Exact"'`. Identifiers are at most 128 characters. Field expressions see source columns. Dataset-scoped metrics see their own fields, unqualified, and `source.column`, which becomes a hidden field so the rest of the compiler only sees fields. A request can't name a hidden field, and neither can a window, which reads the grouped result. Model-scoped metrics see `dataset.field`. A bare reference in a model-scoped metric is accepted with a warning when exactly one dataset has that field, because the compliance fixtures use that form.
- **Metrics are split** into aggregate *terms* and an *outer* expression over them. `SUM(a.x) / NULLIF(COUNT(DISTINCT b.y), 0)` becomes two terms and `t0 / NULLIF(t1, 0)`. Each term's field references are replaced by the field's SQL, qualified by its dataset alias (polyglot adds parentheses by precedence).
- **Every rewrite visits every node.** polyglot's `transform` skips the arguments of many typed functions, so a field inside `YEAR(d)` or `CONTAINS(s, x)` would keep its name instead of its SQL. `sql::transform` uses `transform_all`.
- **Ossie's functions have their own implementations** (`functions.rs`). polyglot translates SQL between dialects, and ossiec keeps it for that, but Ossie defines these functions, and Generic SQL doesn't. Each has a form in core SQL (`standard`), and an engine that needs its own has an override, or refuses when it has no form that means the same (`on_engine`). `tests/expression_language.rs` runs them all on six engines, and a gap can't be a wrong answer, with one known exception: `UPPER` on a StarRocks column only changes ASCII letters, and the engine has no Unicode form. Only a statement written entirely in the portable language gets these rules.

  Where the spec is silent on an edge, the rule follows its first design principle, that core functions work identically: `CONCAT` with a NULL is NULL, `SPLIT_PART` past the last part is an empty string, string functions count characters rather than bytes, `DAYOFWEEK` starts at 0 on Sunday, `WEEK` is ISO, and `DATEDIFF` counts boundaries crossed.

  Ibis, Malloy, MetricFlow and Cube all divide the work this way: the project owns its functions per engine, with explicit lists of what isn't supported, and generates the rest. We measured the alternatives on the same test: reading Ossie as Snowflake, generating from the AST without polyglot's transpile, its `UnsupportedLevel::Raise` mode and polyglot 0.14.0 each left the failures at least as high, and all but 0.14.0 broke existing tests.
- **The portable language is checked on the way in** (`portable.rs`), only for `sql` and `OSSIE_SQL_2026` expressions: no subqueries, `AGG` is reserved, `STDDEV` and `VARIANCE` are the sample forms (the same names mean population on MySQL and StarRocks), `TIMESTAMP_NTZ` is `TIMESTAMP`, and the part in `DATEADD(day, ...)` is a keyword rather than a column.
- **A window reads the grouped result.** Inside or beside one, a field is a column of that result, so a request must include it: `SUM(amount) OVER (ORDER BY day)` needs both `amount` and `day`. To total what each group adds up to, write `SUM(SUM(amount)) OVER (...)`. A metric with no aggregate of its own (`ROW_NUMBER() OVER (...)`) gets a `COUNT(*)` no expression reads, which gives it a home dataset to run over.
- **`FILTER (WHERE p)` stays on the bound AST** until compile. Targets that run standard aggregate FILTER (DuckDB, Postgres, Trino, Athena, Spark, Databricks) keep it; others get the allowed rewrite to `CASE WHEN p THEN x END` (StarRocks, MySQL, ClickHouse, Redshift, BigQuery, Snowflake, and `target: None`).
- **Window metrics** are split differently. Each window function is set aside first, so its own aggregate (the outer `SUM` of `SUM(SUM(x)) OVER (…)`) stays part of the window. Aggregates inside it become ordinary terms. Fields it names outside an aggregate (`PARTITION BY store.id`, `ORDER BY date.d`) become *dimension references*. Window functions can't be nested.

## Planning

### The join root

Relationships go from the many side (`from`) to the one side (`to`). Following them in that direction never multiplies rows of the dataset you start from.

A Layer 3 request with measures has one root per home dataset (see "Legs"), and one without measures roots at its anchor. The rules below pick the root in Layer 2, whose query has one tree from its own joins, or one per independent fact.

Requests name **nodes**: a dataset, or a dataset playing a role (`sale_date.year` is `dates` reached through the `sale_date` relationship). Each node gets its own alias (`dates`, `dates__sale_date`), so one request can use several roles at once.

1. `R` = every node referenced by the requested dimensions, filters and metric terms. A role node needs its relationship's source.
2. Candidates = datasets from which every member of `R` is reachable along many→one edges.
3. Root = the candidate inside `R`. If there is none (for example, a one-side metric grouped by another dimension), it's the deepest candidate outside `R`: the one every other candidate reaches.
4. A plain dataset reachable more than one way takes the **unique shortest path** in Layer 2. A tie is refused with the relationship names, so the caller can name the role. Layer 3 refuses any second path with `E_AMBIGUOUS_PATH` (§4).
5. Joins are `LEFT JOIN`s from the root, so adding a dimension never drops fact rows with a NULL or dangling key; they land in the NULL group.

### Legs: home datasets and independent facts

**One relation across legs (Layer 2).** A Layer 2 query is one relation (§7.6, §10.2), even when its independent facts are planned as separate legs. A leg whose own written joins can drop rows it shares with the others (an `INNER` join, a `LEFT` join from the fact, a derived table joined other than `LEFT`/`FULL`, that the other legs don't also carry) gets `Leg::narrows_on`: the aliases of the datasets the consumer bound (`mark_narrowing_legs`). `lower::from_where` then adds `EXISTS (SELECT 1 FROM <that leg's tree, aliases suffixed __semi> WHERE <shared rows match>)` to every other leg, matching each shared row by its primary key, or else its join columns (`shared_rows_survive`). Joins the planner added to reach a dimension narrow nothing, and Layer 3 never marks a leg: its independent facts stay independent. `includeUnmatched` with a narrowing leg is refused. A `WHERE` on a dataset only one leg's fact reaches (`o.amount > 10` beside `tickets`) can't go on every leg: when planning with every filter fails, `legs::scope_filters` plans the legs without them, gives such a filter to the one leg that reaches it (`LogicalPlan::filter_legs`), and that leg narrows the others, its scoped filters repeated inside the `EXISTS` on its `__semi` tree. That is the relation's meaning only for a filter that rejects NULLs, so only a structured filter (a comparison, `IN`, `NOT IN`, `BETWEEN` or `LIKE` against literals, built by `clauses::where_to_filters`) on a field that is a plain column qualifies (`Expression::Column` after `field_sql_as`); `IS NULL`, a field defined as an expression (`COALESCE(amount, 0) = 0` holds on a padded row) and free-form predicates there are refused.

A Layer 3 term is evaluated over its home dataset's rows (wide table §5.2, §5.4): `SUM(orders.amount) / COUNT(DISTINCT customers.id)` counts customers with no orders too. Terms with the same home share a **leg**, joined to every dimension and filter along the one path `wide_table::check` has confirmed. The root is the home even when relationships run both ways. Layer 2 never does this: its joins decide the rows, and a measure must not add groups (relational semantics §8.4; wide table §6.1).

With `deferredMetrics`, a term reading several datasets has as its home the one that reaches the others, and its leg also joins what the term reads.

In Layer 2, when no dataset reaches all of `R` but the query has measures, each term gets the join tree for its own datasets plus every dimension and filter, and terms whose trees share a root share a leg. Dimensions or filters several facts could link, with no measure to choose, are refused as ambiguous.

Either way, each leg contributes its own base part (and in Layer 2 its terms' de-duplicated parts), and all parts are stacked as usual, so groups are the union across legs and a metric can combine terms from different ones (`SUM(web) / SUM(store)`).

### Layer 3 rejections

`wide_table::check` runs §8's rows in order before planning, since the first that holds decides the code. A metric's rejections (rows 10 to 12) are computed by `metric_rejections`, per metric and at the query, because §5.2 rejects a query naming the metric, not the model: `bind` only marks such a metric not queryable. Names that don't resolve are still an invalid model. One reading of the spec: **row 10 doesn't count a window's own clauses.** Read literally, `SUM(orders.amount) OVER (ORDER BY orders.order_date)` has a field outside every aggregate, but §5.2 gives it as the example of `E_DEFERRED_WINDOWED_METRIC`.

`filter` is read as the `WHERE` clause of `SELECT 1 WHERE …`, and anything but that clause coming out is `E_INVALID_QUERY`. polyglot can't parse `((a)) AND b` as a bare expression, and a role's output name is its final part like any other, so one field in two roles needs aliases.

Properties §9 allows because a query asks for them: `relationship.field`, `filters` entries across datasets (each must still be reachable from every metric), `deferredMetrics`, `timeGrain`, `metricFilters`, `orderBy` and `limit`.

### Dimension-only requests

A request with dimensions but no metrics lists their distinct values: one part with no terms, `SELECT dims … GROUP BY dims`. In Layer 3 it roots at the anchor (§5.6), a dimension's dataset that reaches the others, so one dataset lists all its values and several list the combinations the anchor links. In Layer 2 the join-root rule decides.

### Term strategies

| Strategy | When | Why it's correct |
|---|---|---|
| `Inline` | The term reads the root dataset, or is duplicate-insensitive (`MIN`, `MAX`, `COUNT(DISTINCT …)`, `APPROX_COUNT_DISTINCT`, `ANY_VALUE`) | Each root row appears once after many→one joins, and duplicates can't change a distinct-insensitive aggregate |
| `PkDedup` (Layer 2 only) | The term reads only one-side datasets (`SUM(store.employees)` over a `store_sales` root) | Joined rows repeat one-side values; the term is evaluated over `SELECT DISTINCT dims, <primary key>, <inputs>`, which is spec §9's reference strategy |

`PkDedup` needs a `primary_key` on the term's grain dataset (the term's dataset that reaches its others). Without one the request is refused. It always adds a `FAN_TRAP_RISK` warning: a one-side row in several groups counts in each, so the groups don't sum to a grand total.

**A join written from the one side.** The plan joins from the fact, so `customers LEFT JOIN orders` becomes the planned step `orders RIGHT JOIN customers` (`JoinKind::mirrored`): it keeps every customer and drops the orders no customer matches. A forward `RIGHT` step along a many-to-one relationship repeats no row; it only adds a row for each unmatched right-side row, with everything to its left NULL. Those rows aren't any measure's source rows (§8.1), so instead of de-duplicating, each inline aggregate on that leg gets `FILTER (WHERE <each such step's left-side key> IS NOT NULL)` (`fact_row_present`, `filter_inline_terms`, `sql::with_aggregate_filter`, in `protect.rs`). Every matched row has those keys, and every NULL-extended row lacks one, at any depth of the chain. No primary key is needed. A `FULL` join can't use the filter, because a fact row whose key is NULL is a real row there, and a `RIGHT` step to the many side fans rows out; both take the primary-key path as before.

**A fan-out written from the many side.** `order_items i LEFT JOIN orders o` reading `AGG(o.order_count)` plans a leg rooted at `orders` with no `order_items` on it, so the join has nowhere to attach as written. It is the relation `orders o RIGHT JOIN order_items i`, so `joins::written_from_the_other_side` swaps its sides and mirrors its kind, and it attaches as that reverse (fan-out) step, with the measure de-duplicated by `orders`' primary key. This happens only when no leg has the side written first under any alias, one holds the other, and the `ON` names no role. A dataset on a leg under two roles (departure and arrival airports) is there, just ambiguous, so a role-less join from it is refused rather than turned around onto a new copy of it.

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

## Layer 2 nested queries (A.2–A.3)

Consumer SQL with derived tables compiles by **recursion in Layer 2** ([`src/front/nested.rs`](../src/front/nested.rs)); there is no nested `Request` or `LogicalPlan`. Every `AGG` stage is the flat pipeline (request → `plan` → `lower` → FILTER lowering → dialect `adapt`) run with its own plan. This matters because `adapt` reads the plan (Postgres `string_dims`, MySQL `string_exprs`): adapting once over an assembled tree would have no single plan, and adapting an already-adapted inner AST again would apply the rules twice.

1. **Placeholder relations.** A derived table in an `AGG` stage becomes a `DerivedJoin` on each planner `Leg` that hosts the attach dataset (`from_alias`, `(dataset, field)` pairs, `rel_placeholder`, `rel_alias`, `rel_cols`, `null_safe`, `kind`). A leg that does not read that dataset is not given the join, but it is still narrowed: see "One relation across legs" below. If no leg hosts the dataset, the compile is refused. `lower`'s `tree_sql` emits `JOIN __ossiec_derivN AS "t" ON ...` after the relationship joins, from field indexes and column names (never prebuilt ASTs, because `tree_sql` renames aliases per part). Base, stacked and `PkDedup` parts of a leg that carries the join all build their `FROM` there, so the restriction reaches every part of that leg. A post-lower rewrite of "the" `FROM` would be unsound: `lower` repeats the leg tree inside `DISTINCT` sub-selects.
2. **Adapt per stage, substitute, finish once.** Each stage is adapted with its own plan, then placeholders are replaced bottom-up with the inner (already adapted) ASTs; the whole is rendered once and the text-level `finish` (ClickHouse `SETTINGS join_use_nulls`) runs once, on the outermost statement. One `Ctx` is threaded through the tree: one source-dialect decision, merged warnings.
3. **Output names.** Layer 2 honors consumer aliases (`AS n`) in the compiled column names, ordering, and `string_dims`. An un-aliased column takes the final part of its name, as standard SQL does (§3.1): `o.status` is `status` and `AGG(o.order_count)` is `order_count`. Two output columns with one name are refused. In a derived table, every `AGG(...)` needs an alias.
4. **G2 for derived tables.** In a stage that evaluates `AGG()`, an equijoin to a derived table must be **unique on its join columns** (`Node::unique_key` ⊆ join columns): a dimension-grouped stage is unique on its dimensions, an aggregate with no dimensions has one row. Otherwise the join would repeat measure input rows and the compile is refused with a G2 message. (A "join keys equal the inner `GROUP BY`" rule would wrongly refuse A.3's `d`, grouped by `category, state_id` and joined on `state_id` only; the outer shell evaluates no `AGG()`, so no uniqueness rule applies there.) `INNER` derived joins filter before `AGG` (G1); `LEFT` does not. `RIGHT` and `FULL` are emitted as written: null-extended rows stay in the groups, and a measure that duplication would change is primary-key de-duplicated so those rows do not inflate it (§7.6). A comma `FROM` is read as `CROSS JOIN`s (`from::comma_items_as_cross_joins`), ahead of the written joins; with a `RIGHT` or `FULL` join that flattening would change the meaning, so it is refused. `CROSS JOIN` has no `ON` (an `ON` is `Unsupported`); it is not subject to the join-column uniqueness check, and the same de-duplication keeps each source row once per group.
5. **A.3 outer shell** ([`src/front/shell.rs`](../src/front/shell.rs)). Every `FROM` item is a derived table; the select list is derived columns and `AVG|SUM|COUNT|MIN|MAX` over one derived column (or `COUNT(*)`), `WHERE`/`HAVING` against literals, `GROUP BY` the plain columns. It is assembled with `Tpl` outside `plan`/`lower`. `AGG()` there is an error: the column is already a scalar. Join keys may use `IS NOT DISTINCT FROM`, emitted natively for DuckDB, Postgres, Trino, Athena, Spark, Databricks (and for `target: None`, whose generic SQL is translated downstream) and as `(a = b OR (a IS NULL AND b IS NULL))` everywhere else (`dialect::supports_null_safe_eq`; the list grows from engine evidence only). Derived-table keys in an A.2 `AGG` stage use the expanded form always, since that stage is lowered before the target is consulted.
6. **Unevaluated measures (§7.7).** `OutKind::UnevaluatedMeasure { metric, formula }` marks a derived-table column that is a *measure*, not a value. In a derived table's select list a bare dataset-scoped measure column (never in `GROUP BY`) passes through as metadata only: it adds nothing to the inner `Request` or SQL. An outer `AGG(t.m)` (`resolve_agg`) resolves to the canonical metric (or, for `TO MEASURE`, the formula's leaves, composed like A.4) and is evaluated by the *outer* stage. Derived tables are compiled before the outer select list is parsed so their schemas are known. A derived join is attached to every leg that hosts its dataset, so independent facts that share the attach dataset are all restricted. A leg that does not host the dataset is narrowed through the shared rows ("One relation across legs"). Top-level bare measures, model-scoped metrics, `GROUP BY` of a measure, `AGG` over an evaluated column and plain SQL (the A.3 shell) reading an unevaluated column are refused.
7. **Sole derived `FROM` + outer `AGG` is folded** ([`src/front/fold.rs`](../src/front/fold.rs)). `SELECT region, AGG(m) FROM (SELECT region, m FROM v WHERE ...) t GROUP BY region` is rewritten syntactically to `SELECT region, AGG(m) FROM v WHERE ... GROUP BY region` (inner `FROM`/`JOIN`/`WHERE`, outer select list/`WHERE`/`GROUP BY`/`ORDER BY`/`LIMIT`, derived columns replaced by the inner expressions they name) and compiled as an ordinary `AGG` stage, so the inner filter applies to the measure's source rows. Inners with `HAVING`/`DISTINCT`/`ORDER BY`/`LIMIT`/`WITH` or a derived `FROM` are `Unsupported`.
8. **Query-local `TO MEASURE`** ([`src/front/to_measure.rs`](../src/front/to_measure.rs)). Cut out of the SQL text before parsing (the parser does not know it) and replaced by a marker column `__l2_tmN AS m`. Two forms, only in a derived table's select list (or a `CREATE VIEW` body), both unevaluated until an outer `AGG(t.m)`:
   - `(AGG(a) / AGG(b)) TO MEASURE AS m` — a parenthesized composition of `AGG(measure)`. The inner stage still evaluates it physically at its own grain (so its leaves are planned together, A.4) and exposes it as an unevaluated measure; the outer `AGG` re-evaluates the leaves.
   - `SUM|COUNT|AVG|MIN|MAX(...) TO MEASURE AS m` — a base measure (§5.1). `(SUM(x))` is the same thing. Columns must be fields of one dataset; `COUNT(*)` counts that dataset's grain and needs a single-dataset `FROM`. The compiler installs an ephemeral dataset-scoped metric with that SQL body on a clone of the graph (the model is not mutated) and passes the name through like §7.7, so the outer `AGG` plans it. Mixing `AGG(measure)` with a raw aggregate, or compounding several raw aggregates, is `Unsupported`. A bare `x TO MEASURE` is refused.
9. **Limits.** Nesting depth ≤ 4, derived tables per query ≤ 8, with structured `Unsupported` messages for: top-level bare measure, derived column outside `ON`, correlated references, `WITH`/`DISTINCT`/set operators in a derived table, and a derived join whose attach dataset is on no planned leg.
10. **Role-playing dimensions.** A column qualified by a relationship name is Layer 3's `relationship.field` (`departs_from.city`), whether or not that name is also the JOIN alias. The planner then joins the target under `dataset__relationship` (`airports__departs_from`). A plain `dataset.field` still takes the unique shortest path and is refused when two paths tie. `planned_alias_on_leg` takes the role, so a derived join onto one of several roles uses that role's alias.
11. **`includeUnmatched`.** `Options.include_unmatched` or a `-- ossiec: includeUnmatched` comment sets `Request.include_unmatched` on the outermost `AGG` stage (a flat query, a folded §7.7 query, or an `AGG` stage that joins derived tables). It does not apply to an A.3 shell or to inner stages. `unmatched_rows` already renames every subtree alias with `…__unmatched`, including `dataset__relationship`. Derived joins whose `from_alias` sits in that subtree are emitted there under the same rename; a derived join on the fact side stays out of the unmatched `FROM` and still applies inside the `NOT EXISTS` probe. Layer 3 is unchanged: it reads the request field and ignores `Options.include_unmatched`.
12. **Named measure relations (`CREATE VIEW`, Appendix A.7)** ([`src/front/catalog.rs`](../src/front/catalog.rs)). `define_layer2_view` parses `CREATE VIEW name AS SELECT ...` (polyglot `CreateView`), refuses `OR REPLACE` / `DROP` / column lists / materialized views, and refuses a name that matches a model dataset. The body is checked as a derived table and stored on a `Layer2Catalog`: output columns (fields, evaluated measures, unevaluated measures with their formulas) plus the defining `SELECT` and its `TO MEASURE` expressions. `compile_sql_with` / `compile_with` rewrites a `FROM` / `JOIN` of that name into the defining select before the usual compile, so `AGG(view.measure)` is the §7.7 fold or A.2 derived join. `compile_sql` keeps an empty catalog. `compile_sql_script` / `compile-sql` accept `CREATE VIEW` statements followed by one `SELECT`.
13. **Persist A.7 into the model** ([`src/front/persist.rs`](../src/front/persist.rs)). `persist_layer2_view` copies a session view's `TO MEASURE` items into the bound model and re-binds: a base `SUM|COUNT|AVG|MIN|MAX(...) TO MEASURE AS name` becomes a dataset-scoped metric; an `AGG` composition becomes a model-scoped metric whose SQL inlines the leaf measures (dataset-scoped `COUNT(*)` rewritten to `COUNT(dataset.key)`, where the key must be a primary-key column; a dataset without one is refused, since `COUNT` skips NULLs in any other column). Pass-through columns are skipped. A metric carries only the aggregate, so a view with `WHERE`, `LIMIT`/`OFFSET` or a join other than `LEFT JOIN` is refused rather than persisted with different answers. `to_yaml` / `persist-view` write the updated document. The session catalog is unchanged.

**Why not `ChildAggregate`.** duckdb-semantic_layer expands to plain SQL at bind time, which ossiec shares, but its nested LOD is model-side `AVG(SUM(...))` through correlated scalar subqueries, and its query-by-name rewrite bails out on non-base `FROM`, so it does not implement Appendix A consumer shapes. A.2–A.3 are consumer SQL whose derived tables already evaluated `AGG`; the outer `AVG` in A.3 is ordinary SQL over scalars.

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
| MySQL: `AVG` argument cast to `DOUBLE` (also for the A.3 outer shell, `adapt_shell`) | `averages_of_integers_and_one_side_values`: `AVG` of integers rounds to 4 decimals |

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
| Wide table | `tests/wide_table.rs` | Layer 3's worked examples, on the spec's own data. Unlike Layer 2, a metric can add groups (§6.1), so `Interface` in `plan.rs` keeps branching out of `AGG` queries. |
| Expression language | `tests/expression_language.rs` | Every construct in `expression_language.md`, run on an engine against a value worked out by hand. `gaps` lists what each engine can't do. Apart from one marked engine limit, a gap can't be a wrong answer, and the test fails when one closes, so the list only shrinks. |
| Query compliance | `tests/query_compliance.rs` | Ossie's own `compliance/query` suite (Layer 2, SCOTT data), every vendored case on DuckDB, compared by the suite's rules. |
| Engines | `tests/engines.rs` | Engine disagreements. Every correctness case runs on Postgres, Trino, StarRocks, ClickHouse and MySQL and Spark, Athena's SQL on Trino, and Databricks' (rendered as Spark's) on Spark (Docker, `scripts/engines.sh up`) and must return DuckDB's reference rows, compared as printed text: numbers rounded to six decimals, NULL distinct from `''`, column types not compared. |
| TPC-DS | `tests/tpcds.rs` | Wrong answers at realistic scale, judged against the benchmark itself. Covers Ossie's TPC-DS example model, unchanged; multi-hop joins (`store_sales → customer → customer_address`), a composite-key relationship and composite primary-key de-duplication (`store_returns → store_sales` on item and ticket); and official queries q3, q7, q12, q26, q42, q43, q48, q52, q55, q96 and q98 compared with TPC-DS's own SQL from DuckDB's `tpcds_queries()`. Runs on six engines. Each reference is transpiled from DuckDB SQL and runs on the engine under test, so data-generator differences cancel out. References avoid `LAG`, `IS NOT DISTINCT FROM` and unaliased derived tables, so they never depend on the rules being tested. |
| Fuzz | `tests/fuzz.rs` | Planner edge cases nobody thought to write. Random valid requests (metrics × dimensions × filters × grains, fixed seed) compared with an independent reference generated straight from the spec: each measure as a correlated subquery over its home dataset's rows de-duplicated to its identity (relational semantics §9), and the groups as the union of the branches' (wide table §5.5). 100 per run on DuckDB (`OSSIEC_FUZZ=n` for more), 40 per engine on the others. |
| Scale | `tests/scale.rs`, `tpcds::trino_sf1_performance` | Super-linear compile time (a 121-table model) and plans that are right but slow (each TPC-DS case on Trino sf=1 must run within 3× its reference). |
| Requests | `tests/requests.rs` | Error messages, refusals, 11 engines, determinism. |

`scripts/sync-ossie-fixtures.sh` re-vendors the Ossie examples, so spec drift shows up as a failing test.

## Not yet

| Feature | Shape it will take |
|---|---|
| Layer 2 operators (§7) | Plain aggregates (§7.2) in a select item, `HAVING` or `ORDER BY` are collected by `measures::RawAggs`: each `SUM`, `COUNT`, `AVG`, `MIN` or `MAX` becomes an ephemeral metric marked `raw` (`MetricInfo::raw`, `Term::raw`): its term is always inline, never de-duplicated, null-extension-filtered or fan-out-protected, so it reads the joined rows as SQL does, and terms never merge across `raw`. It is a leaf of the same `L2Formula` as `AGG()` leaves, so the two combine with arithmetic (`+`, `-`, `*`, `/`) and `NULLIF`. `COUNT(*)` sits on the first dataset in `FROM`. A query with independent-fact legs refuses it, since no leg holds the joined rows. `GROUP BY ALL` is expanded to the selected non-aggregates before anything else reads the query (§7.4). An aggregated query (`GROUP BY`, `HAVING` or an aggregate in its select list) can't return an unevaluated measure, passed through or defined there (§7.2); a measure definition's own `AGG()` doesn't make its query aggregated (§4.2), so a measure relation is written without `GROUP BY`, as in Appendix A.7. `DISTINCT` refuses an unevaluated measure (§7.8), and changes nothing in an `AGG` stage, because `GROUP BY` must name exactly the selected dimensions; an outer query over derived tables emits it, since its rows can repeat. `fold` folds a derived table whose own `FROM` is a sole derived table first, innermost out, so a measure passes through any number of levels; a grouping innermost is refused. Refused today: set operators over fields, `ROLLUP`, subqueries in `WHERE`, `HAVING` and `ORDER BY`, and a window metric in `HAVING` or an `ORDER BY` expression. Other `WHERE` predicates go through `Request.filter`, with each column as its `dataset.field`. `HAVING` conjuncts that read only grouping columns go to `WHERE`; the rest become metric filters over placeholders, with the measures they read planned as hidden terms. An `ORDER BY` key that isn't an output column works the same way, as `OrderKey::Expr`. A string dimension inside such an expression sorts by the engine's collation, not by byte. `WITH` expands to derived tables before anything else reads the query (`front/cte.rs`). |
| Writing A.7 views into the model | Done: `persist_layer2_view` / `persist-view` / `to_yaml`. |
| `ASOF`, `SEMI`/`ANTI`, `NATURAL`, `LATERAL`, and the other non-equijoin kinds | Refused. §7.6 covers `INNER`, `LEFT`, `RIGHT`, `FULL`, and `CROSS` (`CROSS` with `ON` is refused). |
| Model-side nested aggregates (`AVG(SUM(x))`) | Not Layer 2; see "Why not `ChildAggregate`" under Layer 2 nested queries. |
