# Spec coverage

What ossiec implements of Apache Ossie `0.2.0.dev0`, document by document and section by section.

**✅** supported · **🟡** partly, with the gap named · **❌** not yet · **—** not applicable to a compiler

Each supported row is backed by tests that execute the SQL and compare it with a reference. The function table and the Layer 2 test list are generated from those tests, so they can't drift: [Functions by engine](generated/functions.md), [Layer 2 test evidence](generated/layer2-tests.md).

## Layer 1: the model ([`spec.md`](https://github.com/apache/ossie/blob/main/core-spec/spec.md))

| Feature | Status | Notes |
|---|---|---|
| Model at the document root: `version`, `name`, `description`, `ai_context`, `custom_extensions` | ✅ | Every Ossie example and compliance fixture binds |
| Dataset `source` as a table or a query | ✅ | |
| `primary_key`, `unique_keys` | ✅ | Validated |
| Fields: expressions over source columns, `datatype`, `dimension.is_time` with the spec's defaults | ✅ | |
| Relationships (`from` many → `to` one, composite keys) | ✅ | |
| Dataset-scoped metrics, and model-scoped metrics over `dataset.field` | ✅ | Bare names in model-scoped metrics are accepted with a warning when exactly one dataset has the field |
| `expression.sql` / `OSSIE_SQL_2026`, `ANSI_SQL`, vendor SQL dialects | ✅ | |
| Non-SQL dialects (`DAX`, `MDX`, `TABLEAU`, …) | — | Ignored; a field needs at least one SQL form |
| Window functions in metrics (`OVER`, `RANK`, `LAG`, running totals) | ✅ | Model-scoped ones with `deferredMetrics` |
| Pre-0.2 `semantic_model:` documents, ontology documents | — | Rejected with a clear error |

## Layer 1: expressions ([`expression_language.md`](https://github.com/apache/ossie/blob/main/core-spec/expression_language.md))

| Feature | Status | Notes |
|---|---|---|
| Operators, `CASE`, `IN`, `LIKE`/`ILIKE`, `BETWEEN`, null tests | ✅ | All six engines |
| Date and time: current date/time, `EXTRACT`, `DATE_PART`, `DATE_TRUNC`, date arithmetic, construction | ✅ | All six engines |
| Strings, math, conditional functions, null-safe comparison | 🟡 | All six engines, except `REGEXP_COUNT` on MySQL (refused) and `UPPER` of a non-ASCII column on StarRocks (wrong answer; StarRocks has no Unicode form) |
| Casts | 🟡 | `TRY_CAST` refused on Postgres and MySQL |
| Aggregates, `FILTER (WHERE …)`, `COUNT(DISTINCT …)` | 🟡 | `MEDIAN` and exact percentiles refused on MySQL and Trino; `APPROX_PERCENTILE` refused on MySQL |
| Window functions | 🟡 | `NTH_VALUE` fails with an engine error on StarRocks |
| Identifier rules, 128-character limit | ✅ | |
| `source.column` in dataset-scoped metrics | ✅ | |
| Reserved `AGG`, no subqueries | ✅ | Refused when the model loads |

The complete, generated list is [Functions by engine](generated/functions.md).

## Layer 2: SQL with `AGG()` ([`relational_semantics.md`](https://github.com/apache/ossie/blob/main/core-spec/relational_semantics.md))

| Section | Status | Notes |
|---|---|---|
| §3 Data model: relations, measures, row identity | ✅ | Model datasets are relations; dataset-scoped metrics are their measures |
| §4 Type discipline (R1, R2) | ✅ | A measure is read only through `AGG()`; `AGG(a + b)` is refused |
| §5.1 Base measures | 🟡 | Query-local `SUM\|COUNT\|AVG\|MIN\|MAX(…) TO MEASURE`; one base aggregate per definition |
| §5.2 Derived measures | ✅ | `(AGG(a) / AGG(b)) TO MEASURE AS m` |
| §5.3 Evaluation controls | 🟡 | Windows through model metrics; no other controls (out of the spec's scope) |
| §6 `AGG()` | ✅ | `MEASURE()` accepted as a synonym |
| §7.1 Unaggregated `SELECT` | 🟡 | Measures pass through derived tables, renamed or not; a top-level unevaluated measure is refused (the spec leaves it open, §13.1). `SELECT *` is refused |
| §7.2 Aggregated `SELECT` | 🟡 | `AGG()` arithmetic, plain aggregates beside it, and arithmetic mixing the two (`+ - * /`, `NULLIF`). Plain aggregates beside independent facts, or with `includeUnmatched`, are refused |
| §7.3 `WHERE` | 🟡 | Any row predicate; on one of several independent facts' columns, a comparison, which narrows the other facts too. Subqueries, and other predicates on one of several facts, are refused |
| §7.4 `GROUP BY` | 🟡 | `GROUP BY` and `GROUP BY ALL`. `ROLLUP`, `CUBE` and `GROUPING SETS` are refused |
| §7.5 `HAVING`, `ORDER BY` | 🟡 | `AGG()`, plain aggregates and expressions over them, selected or not. Subqueries are refused |
| §7.6 `JOIN` | 🟡 | `INNER`, `LEFT`, `RIGHT`, `FULL`, `CROSS` from either side, with filtering, duplication and null-extension handled. A comma `FROM` is a `CROSS JOIN`. `ASOF`, `SEMI`/`ANTI`, `NATURAL`, `LATERAL`, a fan-out written from the many side, and a `CROSS JOIN` spanning independent facts are refused |
| §7.7 Subqueries and CTEs | 🟡 | Derived tables and `WITH`, nested to any depth for pass-through (4 levels, 8 derived tables per query). Scalar subqueries are refused |
| §7.8 `DISTINCT` | ✅ | Over fields and evaluated measures; refused over an unevaluated measure |
| §7.8 Set operators | ❌ | Refused, even over fields only |
| §8.1 Evaluation semantics | ✅ | Every query is one relation; each measure is evaluated over its own input set |
| §8.2 G1, slice and dice | ✅ | |
| §8.3 G2, no duplication | ✅ | Primary-key de-duplication (§9's reference strategy), or a NULL-extension filter where no key is needed |
| §8.4 Closure | ✅ | A.7 views keep every dimension member, with the empty-input value |
| §9 Reference evaluation strategy | ✅ | Results equal it; it is one of ossiec's own strategies |
| §10.3 Model-scoped metrics aren't measures | ✅ | `AGG()` of one is refused |
| §11.1 Measure discovery | 🟡 | Through `describe()` and a view catalog's column kinds. ossiec is a compiler, so there is no `information_schema` |
| §11.2 Model boundary | ✅ | A model's relations form one schema: `shop.orders` is `orders` |
| §11.3 Relationships | ✅ | Joins are matched to model relationships, including roles. Relationship DDL is an engine concern (—) |
| Appendix A.1–A.7 | ✅ | Each worked example has a test; A.7 views can also be persisted into the model |

## Layer 3: semantic requests ([`wide_table_semantics.md`](https://github.com/apache/ossie/blob/main/core-spec/wide_table_semantics.md))

| Section | Status | Notes |
|---|---|---|
| §3 The query: references, parts, aliases, `filter`, validation, result, JSON binding | ✅ | Plus ossiec's own properties (§9 allows them on request): `filters`, `metricFilters`, `orderBy`, `limit`, `timeGrain`, `deferredMetrics` |
| §4 Paths | ✅ | Roles by relationship name; `E_AMBIGUOUS_PATH` otherwise |
| §5 Lowering: metric components, reachability, branches, combining, dimension-only queries | ✅ | One branch per home dataset, stacked with `UNION ALL` |
| §6 Guarantees, §6.1 groups depend on the metrics | ✅ | |
| §7 Using Layer 2 directly | ✅ | |
| §8 Errors | ✅ | Every code, in the spec's order |
| Appendix A.1 Future options | 🟡 | Explicit role selection, sorting and limits, metric predicates and result windows are offered as explicit properties. One-to-many paths, per-metric overrides and grouping sets are not |
| Appendix A.2 SQL table function | ❌ | |
| Appendix B worked examples | ✅ | Each has a test, on the spec's own data |
