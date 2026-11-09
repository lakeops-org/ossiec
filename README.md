<div align="center">

# ossiec

**The Apache Ossie compiler.**
Define a metric once. Get correct SQL for any engine.

[![CI](https://github.com/lakeops-org/ossiec/actions/workflows/ci.yml/badge.svg)](https://github.com/lakeops-org/ossiec/actions/workflows/ci.yml)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)
[![Ossie spec](https://img.shields.io/badge/Apache%20Ossie-0.2.0.dev0-orange.svg)](https://github.com/apache/ossie/blob/main/core-spec/spec.md)
[![Docs](https://img.shields.io/badge/docs-lakeops--org.github.io%2Fossiec-informational.svg)](https://lakeops-org.github.io/ossiec/)

</div>

---

[Apache Ossie](https://ossie.apache.org/) is the vendor-neutral standard for semantic models: datasets, fields, relationships and metrics in YAML. Ossie defines what a metric *means*. It doesn't say how to turn one into SQL.

**ossiec does that part.** Give it an Ossie model and a request ("`total_revenue` by `customers.region`, last quarter") and it returns one SQL statement, in the dialect you need, that can't double-count.

```console
$ echo '{"metrics": ["total_revenue", "total_stock"], "dimensions": ["products.category"]}' \
    | ossiec compile shop.yaml - --dialect snowflake
```

**[Documentation](https://lakeops-org.github.io/ossiec/)**: what ossiec supports of the spec, [section by section](https://lakeops-org.github.io/ossiec/spec-coverage.html) and [engine by engine](https://lakeops-org.github.io/ossiec/generated/functions.html).

## Why a compiler?

Ask a person or an LLM for "units in stock by customer region" over a star schema and you'll usually get this:

```sql
SELECT c.region, SUM(p.stock)
FROM orders o
LEFT JOIN customers c ON o.customer_id = c.customer_id
LEFT JOIN products  p ON o.product_id  = p.product_id
GROUP BY c.region
```

It runs, it looks right, and it's wrong. Stock lives on `products`, but the join repeats each product once per order. On the [test shop](tests/common/shop.rs):

| region | naive SQL | ossiec | truth |
|---|---:|---:|---|
| EU | **250** | **150** | EU bought products 10 and 11: 100 + 50 units |
| US | **60** | **30** | US bought product 12, twice: 30 units |

ossiec gets there two ways, both from the Ossie spec:

- **Ask with metrics and dimensions (Layer 3)** and ossiec chooses the joins. Stock by customer region has no safe answer there, since `products` can't reach `customers` along many-to-one relationships, so it refuses with `E_NO_PATH` rather than guess.
- **Write the joins yourself with `AGG(products.stock_units)` (Layer 2)** and the sum runs over rows de-duplicated on `products`' primary key, which gives the column above. The test suite runs both queries on DuckDB and checks that the naive one really is wrong, so the test can't pass for the wrong reason.

## Features

- **Reads the current Ossie spec** (`0.2.0.dev0`, model at the document root): datasets with table or query sources, fields, primary keys, relationships, dataset- and model-scoped metrics, `ai_context`, `custom_extensions`. Every [Ossie example and compliance fixture](tests/fixtures/ossie) binds.
- **No fan traps or chasm traps by construction.** Joins only go many→one from a single root. Measures from different grains are computed separately and stacked with `UNION ALL`, never joined to each other ([how](#how-it-works)).
- **Engine-native output, checked on real engines.** One AST, rendered by [polyglot-sql](https://crates.io/crates/polyglot-sql) for Trino, Athena, StarRocks, ClickHouse, DuckDB, MySQL, Postgres, Snowflake, BigQuery, Databricks, Redshift and 20+ more. Every correctness case runs on **DuckDB, Postgres, Trino, StarRocks, ClickHouse and MySQL** and must return the same rows. Where an engine answers differently, a small [dialect rule](src/dialect/) fixes it (for example, ClickHouse's `lag` returns `0` instead of NULL for the first row).
- **Window metrics:** running totals, rankings and period-over-period (`SUM(SUM(x)) OVER (…)`, `RANK() OVER (…)`, `LAG`), computed over the grouped result. The spec defers them for model-scoped metrics, so a request asks with `"deferredMetrics": true`. `describe` tells callers which metrics need that, and which dimensions each one needs.
- **Refuses with the spec's error codes.** A Layer 3 request the spec rejects raises its `E_…` code, first matching row first, instead of an answer that might be wrong ([list](#what-gets-refused)).
- **Uses vendor expressions when you want them.** A field can carry a portable expression plus `SNOWFLAKE`, `TRINO`, … variants; `--prefer SNOWFLAKE` uses the native one.
- **Answers in the model's language.** Errors name the problem and the valid choices (`unknown metric 'revenue'; available: total_revenue, …`), so an agent can correct itself. `describe` returns metrics, dimensions and `ai_context` as JSON.
- **Safe SQL construction.** Model and request content become AST nodes, never SQL text.
- **Deterministic.** The same model, request and options give byte-identical SQL and the same `request_hash`.
- **Small and embeddable.** A Rust library plus CLI with no runtime, no server and no Python. Five dependencies.

## Install

```console
cargo install --git https://github.com/lakeops-org/ossiec
```

or as a library:

```toml
[dependencies]
ossiec = { git = "https://github.com/lakeops-org/ossiec" }
```

## Quick start

A model ([full file](tests/fixtures/shop/model.yaml)):

```yaml
version: "0.2.0.dev0"
name: shop
datasets:
  - name: orders
    source: orders
    primary_key: [id]
    fields:
      - { name: id, expression: { sql: order_id } }
      - { name: customer_id, expression: { sql: customer_id } }
      - { name: product_id, expression: { sql: product_id } }
      - { name: order_date, expression: { sql: order_date }, datatype: Date }
      - { name: revenue, expression: { sql: amount_cents / 100.0 } }
      - { name: status, expression: { sql: status } }
  - name: customers
    source: SELECT * FROM raw_customers WHERE NOT deleted   # query sources work
    primary_key: [id]
    fields:
      - { name: id, expression: { sql: customer_id } }
      - { name: region, expression: { sql: region } }
  - name: products
    source: products
    primary_key: [id]
    fields:
      - { name: id, expression: { sql: product_id } }
      - { name: category, expression: { sql: category } }
      - { name: stock, expression: { sql: stock } }
relationships:
  - { name: orders_to_customers, from: orders, to: customers, from_columns: [customer_id], to_columns: [id] }
  - { name: orders_to_products, from: orders, to: products, from_columns: [product_id], to_columns: [id] }
metrics:
  - name: total_revenue
    expression: { sql: SUM(orders.revenue) }
  - name: completed_revenue
    expression: { sql: "SUM(orders.revenue) FILTER (WHERE orders.status = 'completed')" }
  - name: total_stock
    expression: { sql: SUM(products.stock) }
```

Check it, then see what it offers:

```console
$ ossiec validate shop.yaml
ok: `shop` — 3 datasets, 2 relationships, 24 metrics (0 not queryable yet)

$ ossiec describe shop.yaml      # metrics, dimensions, ai_context as JSON
```

Compile a request. Each metric reads its own home dataset in its own branch, so `total_stock` counts every product, sold or not:

```console
$ echo '{"metrics": ["total_revenue", "total_stock"], "dimensions": ["products.category"]}' \
    | ossiec compile shop.yaml - --dialect duckdb
```

```sql
SELECT
  stacked.d0 AS "category",
  MAX(stacked.t0) AS "total_revenue",
  MAX(stacked.t1) AS "total_stock"
FROM (
  -- revenue from the orders' rows
  SELECT "products".category AS d0, SUM("orders".amount_cents / 100.0) AS t0, NULL AS t1
  FROM orders AS "orders"
  LEFT JOIN products AS "products" ON "orders".product_id = "products".product_id
  GROUP BY "products".category
  UNION ALL
  -- stock from the products' rows
  SELECT "products".category AS d0, NULL AS t0, SUM("products".stock) AS t1
  FROM products AS "products"
  GROUP BY "products".category
) AS stacked
GROUP BY stacked.d0
```

*(Condensed and commented for the README; real output is pretty-printed without comments.)*

The same request with `--dialect bigquery`, `clickhouse` or `starrocks` renders that engine's quoting and functions. With no `--dialect`, ossiec emits the model's own source dialect, which is what a downstream translator wants.

### From Rust

```rust
let graph = ossiec::from_yaml(&std::fs::read_to_string("shop.yaml")?)?; // load + validate once
let request = ossiec::Request::from_json(r#"{
    "metrics": ["completed_revenue"],
    "dimensions": ["orders.order_date"],
    "timeGrain": "month",
    "filters": [{"field": "orders.order_date", "op": ">=", "value": "2026-01-01"}]
}"#)?;
let out = ossiec::compile(&graph, &request, &ossiec::Options {
    target: Some(ossiec::DialectType::Trino),
    ..Default::default()
})?;
println!("{}", out.sql);       // the statement
println!("{:?}", out.warnings);
```

A rejected request is `Error::Query { code, message }`, and `error.code()` gives the [`ErrorCode`](src/wide_table.rs).

### Layer 2 (`AGG(measure)`)

Ossie Layer 2 Appendix A.1–A.6: `AGG` over dataset-scoped measures; arithmetic on `AGG(...)` in the select list (ratios); independent facts that share dimensions; consumer joins along model relationships, including unused many→one joins and reverse fan-out (G2 via primary-key de-duplication); `INNER` / `LEFT` / `RIGHT` / `FULL` / `CROSS` join kinds (§7.6: null-extended and Cartesian rows do not inflate `AGG`); **A.2** derived tables joined into an `AGG` stage; **A.3** `AVG`/`SUM`/`COUNT`/`MIN`/`MAX` over derived tables (with `IS NOT DISTINCT FROM` join keys). Consumer aliases (`AS n`) name the output columns, and an unaliased column takes its final part, as in standard SQL (`SELECT o.status` gives `status`). `layer2_to_request` returns leaf measures of flat queries only; nested SQL is `compile_sql` / `compile-sql` (aliases: `compile_layer2` / `compile-l2`) only. **§7.7** unevaluated measures: a derived table may pass a dataset-scoped measure through its select list and an outer `AGG(t.m)` evaluates it (join form, or a sole derived `FROM`, which is folded into the outer stage). **Query-local `TO MEASURE`** in a derived table's select list, consumed by an outer `AGG(t.m)`, is implemented for both `(AGG(a) / AGG(b)) TO MEASURE AS m` and a base `SUM|COUNT|AVG|MIN|MAX(...) TO MEASURE AS m` (unevaluated until that outer `AGG`). **`CREATE VIEW` (Appendix A.7)** stores that relation in a session [`Layer2Catalog`](src/front/catalog.rs) (`define_layer2_view` / `compile_sql_with`, or a `compile-sql` script). The session catalog leaves the model YAML unchanged until `persist_layer2_view` / `persist-view` runs.

**Roles.** Qualify a column with the relationship name, the same form as a Layer 3 dimension: `departs_from.city`. That name may also be the JOIN alias (`JOIN airports AS departs_from`). In Layer 2 a plain `airports.city` takes the unique shortest path and is refused when departure and arrival both tie.

**`HAVING`** (§7.5) takes `AGG()`, expressions over it and selected columns, and the measure needn't be selected. It's computed in its own part like any measure, so a one-side measure is de-duplicated before it's compared. A plain aggregate there (`HAVING COUNT(*) > 5`) reads the group's joined rows, as in plain SQL; a window in `HAVING` is refused.

**Plain aggregates** (§7.2) sit beside `AGG()` in the select list: `SELECT region, MAX(order_date), AGG(total_revenue) … GROUP BY region`. Each is ordinary SQL over the query's joined rows, so repeats and NULL-extended rows count (`COUNT(*)` over `customers LEFT JOIN orders` counts a customer without orders once), while each measure still counts its own source rows once. Each `SUM|COUNT|AVG|MIN|MAX(...)` reads one dataset's fields, and select items, `HAVING` and `ORDER BY` combine them with each other and with `AGG()` (`+ - * /`, `NULLIF`): `COUNT(*) - AGG(o.order_count)`. Nested aggregates, plain aggregates beside measures from independent facts, and `includeUnmatched` are refused.

**Nested pass-through** (§7.7): a measure passes through any number of plain projections and filters (`FROM (SELECT … FROM (SELECT … FROM orders WHERE …) a WHERE …) b`, or `WITH` levels reading each other) and is evaluated by the outer `AGG()`.

**`ORDER BY`** (§7.5) takes a selected column or measure by name, or `AGG()` and expressions over it, selected or not, so a Top-N can select only its join column. `ORDER BY 1` is refused, since it means a position in SQL.

**Measure relations** (§7.2, §4.2) are written without `GROUP BY`, as in Appendix A.7: a query that aggregates can't return an unevaluated measure, whether it passes one through or defines it with `TO MEASURE`.

**`GROUP BY ALL`** (§7.4) groups by every selected column that isn't an aggregate, and refuses a bare measure. **`ROLLUP`**, **`CUBE`** and **`GROUPING SETS`** report each measure at every level they name, with NULL in a subtotal row's grouping columns, as SQL does. **`DISTINCT`** (§7.8) works over fields and evaluated measures, and refuses an unevaluated one.

**`WITH`** (§7.7) is a named derived table, so a measure passes through a CTE as it does through a subquery. `WITH RECURSIVE`, a column list and a set operator in a CTE are refused.

**`WHERE`** is any row predicate over the query's columns (§7.3): comparisons, `IN`, `LIKE`, `BETWEEN`, `OR`, `NOT` and expressions, across datasets. Aggregates and windows are refused, and subqueries aren't supported yet.

**`includeUnmatched`.** One-side rows no fact reaches are off unless `Options { include_unmatched: true, .. }` or a comment `-- ossiec: includeUnmatched` (or `compile-l2 --include-unmatched`). It applies to the outermost `AGG` stage, including one that joins a derived table onto the unmatched grain. Layer 3 has no such rows to add, since each metric reads all of its home dataset, so it raises `E_INVALID_QUERY` for the request field and ignores the option.

```sql
-- A.2: restrict an AGG stage to the top two customers
SELECT o.status, AGG(o.order_count)
FROM orders o
JOIN (SELECT customer_id, AGG(order_count) AS n
      FROM orders GROUP BY customer_id
      ORDER BY n DESC, customer_id LIMIT 2) t
  ON o.customer_id = t.customer_id
GROUP BY o.status;

-- §7.7: the measure stays a measure through the subquery; the outer AGG evaluates it
SELECT region, AGG(order_count) FROM (SELECT region, order_count FROM orders WHERE ...) t GROUP BY region;

-- Query-local TO MEASURE: a deferred ratio, evaluated by the outer AGG
SELECT r.region, AGG(m.sales_per_ticket)
FROM customers r
JOIN (SELECT c.region, (AGG(o.order_amt) / AGG(t.ticket_count)) TO MEASURE AS sales_per_ticket
      FROM customers c
      LEFT JOIN orders o ON o.customer_id = c.id
      LEFT JOIN tickets t ON t.customer_id = c.id) m ON r.region = m.region
GROUP BY r.region;

-- Base measure: SUM(...) stays unevaluated until the outer AGG
SELECT r.region, AGG(m.total_revenue)
FROM customers r
JOIN (SELECT c.region, SUM(o.amount) TO MEASURE AS total_revenue
      FROM customers c LEFT JOIN orders o ON o.customer_id = c.id) m ON r.region = m.region
GROUP BY r.region;

-- A.7: the same relation, named (session catalog). Persist into the model with
-- `persist_layer2_view` / `ossiec persist-view`.
CREATE VIEW region_metrics AS
SELECT c.region,
       (AGG(o.order_amt) / AGG(t.ticket_count)) TO MEASURE AS sales_per_ticket
FROM customers c
LEFT JOIN orders o ON o.customer_id = c.id
LEFT JOIN tickets t ON t.customer_id = c.id;

SELECT region, AGG(sales_per_ticket) AS sales_per_ticket
FROM region_metrics
GROUP BY region;

-- A.3: ordinary aggregates over derived tables
SELECT d.status, AVG(m.n)
FROM (SELECT status, customer_id FROM orders GROUP BY status, customer_id) d
JOIN (SELECT customer_id, AGG(order_count) AS n FROM orders GROUP BY customer_id) m
  ON d.customer_id IS NOT DISTINCT FROM m.customer_id
GROUP BY d.status;
```

A derived table joined into an `AGG` stage must be unique on its join columns (G2), is read only in `ON`, and needs an alias on every `AGG(...)`. When independent facts are measured together the query is still one relation (§7.6, §10.2): a derived join, an `INNER` join or a `LEFT` join from one fact also narrows the others to the shared rows that survive it. Nesting is limited to 4 levels and 8 derived tables per query. `includeUnmatched` cannot be combined with a derived join.

```bash
echo "SELECT status, AGG(order_count) FROM orders GROUP BY status" \
  | ossiec compile-l2 shop.yaml - --dialect duckdb
```

```rust
let out = ossiec::compile_sql(&graph, "SELECT AGG(order_count) FROM orders", &opts)?;

let mut views = ossiec::Layer2Catalog::new();
ossiec::define_layer2_view(&graph, &mut views, "CREATE VIEW region_metrics AS SELECT ...", &opts)?;
let out = ossiec::compile_sql_with(&graph, &views, "SELECT region, AGG(sales_per_ticket) FROM region_metrics GROUP BY region", &opts)?;
// compile-l2 accepts the same script: CREATE VIEW ...; SELECT ...
// Promote TO MEASURE items into the model (and optionally write YAML):
let (graph, added) = ossiec::persist_layer2_view(&graph, &views, "region_metrics", &opts)?;
let yaml = ossiec::to_yaml(graph.model())?;
```

Persisting refuses a view with `WHERE`, `LIMIT`/`OFFSET` or a join other than `LEFT JOIN`, including inside a referenced view or derived table, because a metric would not carry them and could answer differently from the view. A `LEFT JOIN` written from the one side (`customers LEFT JOIN orders`, as in A.7) is persisted, but the metric is queried over the model's own `orders -> customers` path: it has no group for a customer with no orders, and puts orders without a customer under NULL. The groups both answers share agree. Composing a dataset's `COUNT(*)` into a model-scoped metric also needs that dataset to declare a primary key.

`to_yaml` and `persist-view` write a re-serialized model, so unknown keys and comments in the source file are not preserved. Write to a new file and review the diff before replacing the original.

Model-scoped metrics are not measures (§10.3). `AGG(expr)` inside `AGG()` is refused; use `AGG(a) + AGG(b)`. Other join forms (`ASOF`, `SEMI`/`ANTI`, `NATURAL`, `LATERAL`, …) are not implemented yet. `CROSS JOIN` takes no `ON`, and a comma `FROM` is a `CROSS JOIN`, as in SQL. A `CROSS JOIN` spanning independent facts, and a comma `FROM` combined with a `RIGHT` or `FULL` join, are refused.

## Requests

```jsonc
{
  "metrics": ["total_revenue", {"metric": "orders.order_count", "alias": "orders"}], // model metrics by name, dataset metrics as dataset.metric
  "dimensions": ["customers.region", {"field": "orders.order_date", "alias": "day"}],
  "filter": "orders.status IN ('completed', 'shipped')", // one expression over one dataset
  "filters": [                             // structured, and may read several datasets
    {"field": "orders.order_date", "op": "BETWEEN", "values": ["2026-01-01", "2026-03-31"]}
  ],
  "metricFilters": [{"metric": "total_revenue", "op": ">", "value": 1000}], // HAVING
  "timeGrain": "month",                    // day | week | month | quarter | year
  "timeDimension": "orders.order_date",    // only needed when several time dimensions are requested
  "orderBy": [{"field": "total_revenue", "direction": "desc", "nulls": "last"}], // nulls: first | last (optional)
  "limit": 100,
  "deferredMetrics": false                 // see below
}
```

**Output columns** are named by the alias if given, or the final part of the reference: `customers.region` comes back as `region`. Two entries with the same name raise `E_NAME_COLLISION`, so `orders.id` and `customers.id` need an alias on at least one. `orderBy` takes an output name or a reference.

The spec's query is `metrics`, `dimensions` and `filter`. The others are ossiec's own properties, which the spec allows when a query asks for them explicitly (§9).

**Roles.** When a dataset is reachable more than one way (a sale's own date, and its store's opening date), a plain `dates.year` raises `E_AMBIGUOUS_PATH`, even if one path is shorter. Name the relationship to choose: `sale_date.year`. A request can use several roles at once, each joined under its own alias.

**Home datasets.** Metrics from datasets that don't join each other (store sales and web sales by item category) are computed in one branch per home dataset and stacked, so groups are the union across them: a category sold only online still appears. Every requested dimension and filter must be reachable from every metric's home datasets.

**`deferredMetrics`.** The spec defers two kinds of model-scoped metric: window functions, and aggregates reading several datasets (`SUM(sales.price * dates.year)`). With `"deferredMetrics": true` they're answered, the second from the dataset that reaches the others. Without it they raise `E_DEFERRED_WINDOWED_METRIC` and `E_AMBIGUOUS_MEASURE_GRAIN`.

Leave out `metrics` to list **dimension values** instead: `{"dimensions": ["customers.region"]}` returns every region in `customers`. Dimensions from several datasets need one of those datasets to reach the others, and return the combinations it links. Agents use this to filter with real values.

| Filter `op` | Value |
|---|---|
| `=` `!=` `<` `<=` `>` `>=` `LIKE` | `value` |
| `IN` `NOT IN` | `values` (non-empty) |
| `BETWEEN` | `values`: exactly two |
| `IS NULL` `IS NOT NULL` | none |

String values compared with `Date` and `DateTime` fields are cast (`CAST('2026-01-01' AS DATE)`), so they work on strict engines like Trino. Unknown request keys are rejected, so a typo like `dimension` fails loudly instead of being ignored.

## How it works

```
YAML ─load─► Model ─bind─► SemanticGraph ─plan─► LogicalPlan ─lower─► AST ─render─► SQL
```

1. **Bind** (once per model). Parse every expression and resolve every name with ANSI identifier rules. Split each metric into aggregate *terms* and an outer formula: `SUM(a.x) / NULLIF(SUM(b.y), 0)` becomes two terms and `t0 / NULLIF(t1, 0)`. All problems are reported together.
2. **Pick the join roots.** Each aggregate reads its home dataset (`SUM(orders.x) / COUNT(DISTINCT customers.id)` counts customers with no orders too), and gets a branch that `LEFT JOIN`s from there to every dimension and filter. Many→one joins never multiply home rows, and `LEFT` keeps orphan rows in the NULL group instead of dropping them. A request where a home can't reach a dimension or filter is refused.
3. **Classify each term.**
   - A term that reads the root, or that duplicates can't change (`MIN`, `MAX`, `COUNT(DISTINCT …)`), is computed **inline** over the joined rows. In Layer 3 every term reads its root.
   - In a Layer 2 query, whose joins decide the rows, a term that reads only one-side data (`SUM(products.stock)` over an `orders` root) is computed over rows **de-duplicated on that dataset's primary key**, which is the reference strategy in Ossie's relational spec (§9).
4. **Stack, don't join.** Each branch and strategy becomes a part grouped by the same dimensions. Parts are combined with `UNION ALL` and `MAX` per term, so there's no join predicate between parts to get wrong. The groups are every part's groups, so asking for a metric can add groups (wide table §6.1).
5. **Windows last.** Window functions in a metric run in the final `SELECT`, over one row per group: `SUM(SUM(x)) OVER (ORDER BY d)` becomes `SUM(base.t0) OVER (ORDER BY base.d0)`. Fields a window partitions or orders by must be requested dimensions. When windows are present, `metricFilters` apply to the finished rows, so "rank ≤ 3" means the top three by the full ranking.
6. **Render.** Build one polyglot AST and apply the target's [dialect rules](src/dialect/) (only where a real-engine test showed polyglot's rendering answers differently). Generate it in the source dialect, then transpile to the target.

### What gets refused

A Layer 3 request the spec rejects raises one of `wide_table_semantics.md` §8's codes. When several apply, the first in this order wins:

| Code | Condition |
|---|---|
| `E_INVALID_QUERY` | Malformed, or a property Layer 3 doesn't support (`includeUnmatched`) |
| `E_EMPTY_AGGREGATION_QUERY` | No dimensions or metrics |
| `E_NAME_NOT_FOUND` | A reference doesn't resolve |
| `E_METRIC_IN_DIMENSIONS`, `E_FIELD_IN_METRICS` | An entry names the wrong kind of thing |
| `E_AGGREGATE_IN_WHERE`, `E_WINDOW_IN_WHERE` | A filter reads an aggregate, a metric or a window |
| `E_CROSS_DATASET_FILTER` | `filter` reads more than one dataset |
| `E_NAME_COLLISION` | Two entries have the same output name, like `orders.id` and `customers.id` without an alias |
| `E_UNAGGREGATED_FINER_GRAIN_REFERENCE` | A model-scoped metric reads a field outside every aggregate |
| `E_DEFERRED_WINDOWED_METRIC` | A model-scoped metric has a window, without `deferredMetrics` |
| `E_AMBIGUOUS_MEASURE_GRAIN` | An aggregate reads several datasets (without `deferredMetrics`), has a `FILTER` on another dataset, or reads none (`COUNT(*)` at model scope) |
| `E_NO_PATH` | No metric reaches a dimension's dataset |
| `E_PARTIAL_DIMENSION_REACHABILITY` | Some metrics reach a dimension's dataset, others don't |
| `E_NO_ANCHOR` | No metrics, and no dimension's dataset reaches the others |
| `E_UNSUPPORTED_FILTER_PATH` | A metric or the anchor can't reach a filter's dataset |
| `E_AMBIGUOUS_PATH` | Two paths lead to a dataset the query needs, or two datasets qualify as the anchor |

Metrics the spec rejects don't stop a model loading: only a query that names one is refused, and `describe` marks it.

Two refusals are ossiec's own: a window metric without the dimension it orders by (`metric 'cumulative_revenue' is a window over 'orders.order_date'; add it to 'dimensions'`, as `E_INVALID_QUERY`), and, in Layer 2, a one-side measure on a dataset with no `primary_key`.

The full rationale is in **[docs/DESIGN.md](docs/DESIGN.md)**.

## Ossie support

| Spec feature | Status |
|---|---|
| Model at document root, `version`, `name`, `description`, `ai_context` | ✅ |
| Dataset `source` as a table reference or a query | ✅ |
| `primary_key`, `unique_keys` (validated) | ✅ |
| Field expressions over source columns, `datatype`, `dimension.is_time` with spec defaults | ✅ |
| Relationships (`from` many → `to` one, composite keys) | ✅ |
| Dataset-scoped metrics (unqualified names) and model-scoped metrics (`dataset.field`) | ✅ |
| `expression.sql` / `OSSIE_SQL_2026`, `ANSI_SQL`, vendor SQL dialects | ✅ |
| Non-SQL dialects (`DAX`, `MDX`, `TABLEAU`, …) | Ignored (a field needs at least one SQL form) |
| `FILTER (WHERE …)`, `COUNT(DISTINCT …)`, ratios, `CASE` | ✅ |
| Scalar and aggregate functions in `expression_language.md` | ✅ DuckDB and ClickHouse. Other engines refuse what they can't express, and [`gaps`](tests/expression_language.rs) is the full list: `MEDIAN`, exact percentiles on MySQL and Trino, `APPROX_PERCENTILE` and `REGEXP_COUNT` on MySQL, `TRY_CAST` on MySQL and Postgres. `NTH_VALUE` fails with an engine error on StarRocks, whose `UPPER` also only changes ASCII letters in a column |
| Identifier rules (regular names upper-cased, quoted kept, so `"ID"` matches `id` and `"id"` doesn't; 128 characters at most) | ✅ |
| `source.column` in dataset-scoped metrics | ✅ |
| Reserved `AGG`, and no subqueries, in portable expressions | ✅ Refused when the model loads |
| Window functions in metrics (`OVER`, `RANK`, `LAG`, running totals) | ✅ Dataset-scoped directly, model-scoped with `deferredMetrics` |
| Ossie ontology documents (Layer 4) | Rejected with a clear error |
| Pre-0.2 `semantic_model:` documents | Rejected with a clear error |

One leniency: Ossie requires `dataset.field` in model-scoped metrics, but the compliance fixtures include a bare form (`SUM(amount)`). ossiec accepts a bare name when exactly one dataset has that field, and warns.

## Testing

```console
cargo test                        # everything (the first run downloads DuckDB's prebuilt library)
scripts/engines.sh up && cargo test --test engines -- --ignored   # same cases on Postgres, Trino, StarRocks, ClickHouse, MySQL, and Athena's SQL on Trino
cargo test --test tpcds -- --include-ignored                      # TPC-DS on all six engines (DuckDB alone needs no Docker)
cargo test --test correctness     # answers vs hand-written SQL on DuckDB
UPDATE_GOLDEN=1 cargo test --test golden   # re-snapshot SQL after an intended change
scripts/sync-ossie-fixtures.sh    # re-vendor Apache Ossie examples, then cargo test
```

| Suite | What it proves |
|---|---|
| [`correctness`](tests/correctness.rs) | **The numbers are right.** 30 requests run on DuckDB against [seed data](tests/common/shop.rs) built to break naive SQL (fan-out, NULL keys, a deleted customer, a guest checkout, a product that never sold), compared with hand-written reference SQL. |
| [`engines`](tests/engines.rs) | **Every engine agrees.** The same 30 cases compiled for and executed on Postgres 16, Trino 484, StarRocks 4.1, ClickHouse 26.8 and MySQL 8.4 in Docker, compared with the DuckDB reference answer row by row, with no type leniency (a month must come back as a `DATE` everywhere, and NULL must stay distinct from `''`). |
| [`tpcds`](tests/tpcds.rs) | **Real data, and the benchmark's own SQL.**<br>• **Apache Ossie's TPC-DS example model, unchanged,** on generated TPC-DS data: all its metric kinds, including the two-branch `store_productivity` and the window metrics.<br>• **Official TPC-DS queries q3, q7, q12, q26, q42, q43, q48, q52, q55, q96 and q98**, expressed as ossiec requests and compared row by row with TPC-DS's own query text.<br>26 cases, including multi-hop joins, composite keys, date roles, store-and-web independent facts and unmatched items, on DuckDB (sf=0.1, 288k `store_sales` rows), Trino (`tpcds.tiny`), Postgres, StarRocks and ClickHouse. Each reference runs on the same engine, so data-generator differences cancel out. |
| [`fuzz`](tests/fuzz.rs) | **Cases nobody wrote.** Random requests, each decided independently from the model's relationships: refused ones must raise the right code, and answered ones must match a reference generated from the spec's own evaluation strategy, on all six engines. |
| [`scale`](tests/scale.rs) | **No blow-ups.** A 121-table model binds in about 0.1 s and compiles in about 10 ms per request (unoptimized build). On Trino sf=1, each TPC-DS case must run within 3× its reference. |
| [`golden`](tests/golden.rs) | **The SQL didn't drift.** Exact output for TPC-DS and shop requests across generic, BigQuery, Trino, StarRocks, ClickHouse and Snowflake, in [`tests/golden/`](tests/golden). |
| [`model`](tests/model.rs) | **Spec conformance.** Every Ossie example binds, every compliance metric (M0–M3) returns its expected answer on DuckDB, and there's one test per validation rule. |
| [`wide_table`](tests/wide_table.rs) | **Layer 3's own examples and rejections.** The data and results from `wide_table_semantics.md` (§1.3, §3.4, §5.2, §6.1, Appendix B), every row of §8 and the order between them. |
| [`query_compliance`](tests/query_compliance.rs) | **Ossie's own query suite.** Every vendored `compliance/query` case returns its expected result on DuckDB, compared by the suite's rules. |
| [`requests`](tests/requests.rs) | **Good failures.** Error messages, refusals, all 11 engines (including window requests; native `FILTER` only where supported), determinism. |
| [`sql_front`](tests/sql_front/) | **Layer 2 AGG.** A.1, A.2/A.3 (derived tables, executed on DuckDB, including a derived join on one of several independent facts, which narrows them all; refusals; 11-engine compile matrix), A.4 (multi-fact + `AGG` arithmetic, including joins on one fact narrowing the others), A.5 (unused joins + fan-out), A.6 (`INNER`/`LEFT`), §7.6 (`RIGHT`/`FULL`/`CROSS`), §7.7 pass-through and query-local `TO MEASURE` (AGG compositions and base `SUM\|COUNT\|AVG\|MIN\|MAX`), role-playing dimensions, `includeUnmatched` (flat, derived onto the unmatched grain, and role-played attach), A.7 `CREATE VIEW` (AGG compositions and base `SUM(...) TO MEASURE`) and `persist_layer2_view` into the model. Spec appendix A.2/A.3/A.7 shapes are pinned. The shop forms (A.2/A.3, §7.7, base `TO MEASURE`, unmatched+derived, A.7 script) also run in the engine tier (`layer2_nested_*`). |

## Known issues

**Semantics to know**
- **De-duplicated groups overlap (Layer 2).** A product sold in two regions counts in both, so per-group values don't sum to a grand total. ossiec warns (`FAN_TRAP_RISK`). Products that never sold aren't in any group unless the query sets `includeUnmatched`.
- **Window metrics see only the rows in the result.** A running total over a filtered date range starts at the range's first row, and `stock_share` is a share of the groups returned. With windows present, metric filters apply after the window.
- **Division is always true division.** `/` in portable expressions casts the numerator to `DOUBLE` unless one side is a fractional literal, so `COUNT(a) / COUNT(b)` isn't truncated on Postgres and Trino. Ratios therefore come back as floating point. `amount_cents / 100.0` stays exact.
- **NULL ordering** follows generic SQL on every engine: NULLs sort first in ascending order (`NULLS FIRST` is emitted where needed).
- **Strings compare and sort by byte on every engine**, so `'Bob'` comes before `'ada'` and `'ada'` never equals `'Ada'`. Postgres string dimensions carry `COLLATE "C"`, and MySQL string dimensions and filters `COLLATE utf8mb4_bin`, to match.
- **A backslash in a `LIKE` pattern is an ordinary character**, as in SQL without `ESCAPE`. On engines that escape with it by default (Postgres, ClickHouse, StarRocks, MySQL), ossiec doubles it.
- **In Layer 3, groups depend on the metrics you ask for** (spec §6.1). Each metric brings its home dataset's groups, and a metric with no rows in a group takes its empty-input value: NULL, or 0 for `COUNT`. In Layer 2 the query's joins decide the groups.
- **Columns are named by their alias or final part in both layers** (`customers.region` comes back as `region`). Two with the same name are refused until one has an alias. An unaliased expression like `AGG(a) / AGG(b)` gets a generated name, `__l2_1`, so give it an `AS`.
- **Time grains ignore time zones.** Truncating a `DateTimeTz` uses the engine's session time zone.

**Refused for now:** nested windows.

**Spec leniency:** bare names in model-scoped metrics are accepted with a warning.

**Engine coverage:** answers are verified on DuckDB, Postgres, Trino, StarRocks, ClickHouse and MySQL, on the shop cases, TPC-DS and fuzzing. Athena's SQL is Trino's (its engine v3 is Trino), and it runs on the Trino container for every tier except TPC-DS. Snowflake, BigQuery, Databricks and Redshift are compile-checked only. See [docs/ADDING_AN_ENGINE.md](docs/ADDING_AN_ENGINE.md) to move an engine up.

**Downstream translators:** if you emit `generic` SQL (no `--dialect`) and translate it elsewhere, ossiec's [dialect rules](src/dialect/) don't apply. For example, a translator that maps `LAG` to ClickHouse's `lag` gets `0` instead of NULL for the first row. Compile for the engine instead.

**Engines not in the executed set, with known risks:** Doris has StarRocks' backslash-literal gap; Redshift probably has Postgres' untyped-`NULL` union issue. Neither is covered by dialect rules yet.

**polyglot-sql 0.13.5 notes, still guarded where needed:**
- it still passes `UNION ALL BY NAME` and `FILTER (WHERE …)` through to engines that lack them;
- its generic generator upper-cases function names.

ossiec never emits `BY NAME` / `QUALIFY` / `GROUP BY ALL` (checked for 11 engines). Aggregate `FILTER` is kept only for engines that run it (DuckDB, Postgres, Trino, Athena, Databricks) and rewritten to `CASE` elsewhere. Function case is preserved. `CAST(NULL AS …)` for ClickHouse and MySQL `NULLS FIRST/LAST` are fixed upstream ([polyglot#514](https://github.com/tobilg/polyglot/issues/514), [polyglot#515](https://github.com/tobilg/polyglot/issues/515)).

## Using ossiec in a query engine

ossiec is a library first, built to be embedded in query engines, SQL proxies and agent tool servers.

- **Compile for the engine you'll run on.** Resolve the target first, then call `compile` with `Options { target: Some(..), .. }`. You get engine-ready SQL with ossiec's [dialect rules](src/dialect/) applied.
- **Or translate downstream.** With `target: None`, you get SQL in the model's source dialect (`Compiled::source_dialect`) to hand to your own translator. ossiec's dialect rules don't apply on that path.
- **Apply your own governance.** Row filters, column masks and permissions work on the generated SQL like on any other query, since it references the model's physical tables.
- **For agents,** `describe()` returns metrics, dimensions and `ai_context` as JSON, and a request without metrics lists dimension values for filters.

## Roadmap

- [x] Ossie `0.2` loading, validation and identifier rules
- [x] Join-root planning, inline and primary-key de-duplicated terms, `UNION ALL` stacking
- [x] `FILTER` kept where supported / CASE elsewhere, time grains, metric filters, vendor-dialect preference
- [x] CLI: `validate`, `describe`, `compile`, `compile-sql`
- [x] Cross-engine test tier: Postgres, Trino, StarRocks, ClickHouse, MySQL, and Athena's SQL on Trino
- [x] TPC-DS: Ossie's example model on real data, plus official queries q3, q7, q26, q42, q48, q52, q55, q98
- [x] Official TPC-DS queries q3, q7, q12, q26, q42, q43, q48, q52, q55, q96, q98
- [ ] More official TPC-DS queries (many of the 99 need constructs no semantic request expresses: correlated subqueries, cross-channel set operations, ROLLUP)
- [x] Window metrics (cumulative, rank, period-over-period)
- [x] Independent facts across shared dimensions
- [x] Role-playing dimensions (`relationship.field`)
- [x] Unmatched one-side rows in Layer 2 (`includeUnmatched`)
- [x] Layer 3 rejections with the spec's error codes (`wide_table_semantics.md` §8), and the `filter` expression
- [x] Layer 3 aliases and short output names
- [x] Layer 2 `WHERE` as any row predicate (`IN`, `OR`, `LIKE`, `BETWEEN`, `NOT`, expressions)
- [x] Layer 2 `HAVING` and `WITH`
- [x] Layer 2 `ORDER BY` on an unselected `AGG()` or an expression over it (§7.5)
- [x] Layer 1 rules: identifiers, `source.column`, reserved `AGG`, no subqueries
- [x] Layer 1 functions on six engines, each implemented or refused (`functions.rs`)
- [ ] Exact percentiles on MySQL and Trino, which have no such function
- [x] Layer 2 `GROUP BY ALL` and `DISTINCT`
- [x] Layer 2 plain aggregates beside `AGG()` (§7.2), and a measure passed through nested derived tables (§7.7)
- [x] Layer 2 arithmetic on plain aggregates, alone or with `AGG()`, and plain aggregates in `HAVING` and `ORDER BY`
- [x] Fix: a LEFT JOIN written from the one side (`customers LEFT JOIN orders`) counted fact rows that match nothing
- [x] Fix: a join on one of several independent facts left the others unrestricted, and two `COUNT(*)` metrics on different facts shared one answer
- [x] Fix: a Layer 2 comma `FROM` (`FROM customers c, orders o, tickets t`) was read as independent facts; it is a `CROSS JOIN`, as in SQL
- [x] Fix: a quoted Layer 2 measure name (`AGG("m")`) ignored its quotes; it now follows the identifier rules
- [x] Fix: Athena SQL used polyglot's Athena dialect and gave wrong answers (`DATEDIFF`, `TO_CHAR`, day of week, `SPLIT_PART`, exact percentiles as `APPROX_PERCENTILE`, `CAST(… AS STRING)`); it is now Trino's, run on Trino in every tier but TPC-DS
- [ ] Layer 2 `CROSS JOIN` (or comma `FROM`) spanning independent facts: refused today
- [ ] Layer 2 quoted references to a derived table's columns follow the identifier rules (today they match regardless of quotes)
- [x] Layer 2 `WHERE` comparison, `IN`, `NOT IN`, `BETWEEN` or `LIKE` on one of several independent facts' plain-column fields, narrowing the others
- [ ] Layer 2 `WHERE` on one of several independent facts' columns beyond a comparison, `IN`, `BETWEEN` or `LIKE` on a plain column (`IS NULL`, `NOT BETWEEN`, `NOT LIKE`, a field defined as an expression such as `COALESCE(amount, 0)`, a predicate over two facts): refused today
- [ ] Layer 2 fan-out written from the many side (`order_items i LEFT JOIN orders o` with `AGG(o.order_count)`): refused today; `orders o RIGHT JOIN order_items i` works
- [x] Layer 2 `ROLLUP`, `CUBE` and `GROUPING SETS` over columns, each grouping set compiled at its own level
- [ ] Layer 2 subqueries in `WHERE`, the `GROUPING()` function, set operators, and the `MEASURE` marker
- [x] NULL placement in `orderBy`
- [x] Ossie Layer 2: SQL with `AGG(measure)` (A.1–A.6 incl. nested derived tables; §7.6 `RIGHT`/`FULL`/`CROSS`; §7.7 pass-through and query-local `TO MEASURE`, including base `SUM|COUNT|AVG|MIN|MAX(...)`; role-playing dimensions and `includeUnmatched`, including a derived join on the unmatched grain; A.7 `CREATE VIEW` in a session catalog; `persist_layer2_view` / `persist-view` writes measures into the model)
- [ ] Publish to crates.io

## Contributing

Adding or verifying an engine? Follow **[docs/ADDING_AN_ENGINE.md](docs/ADDING_AN_ENGINE.md)**.


Issues and PRs are welcome. Before opening a PR:

```console
cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test
```

New planner behaviour needs a **correctness** case (a seed row that would expose the bug, plus reference SQL), not just a golden file.

**PR titles** follow [Conventional Commits](https://www.conventionalcommits.org): `type(scope): summary`, lower case, saying what changes for the user. A workflow checks the title against [.github/pr-title-checker-config.json](.github/pr-title-checker-config.json).

- **Types:** `feat` (new behaviour), `fix` (wrong results or a bug), `docs`, `test`, `refactor`, `perf`, `ci`, `chore` (dependencies, tooling). Add `!` for a breaking change: `feat(layer2)!: …`.
- **Scopes** (optional): `layer1`, `layer2`, `layer3`, `engines`, `cli`, `site`, `deps`.
- **Branches** match the title: `feat/layer2-plain-aggregate-arithmetic`, `fix/duplicate-join-alias`.

For example: `feat(layer2): arithmetic on plain aggregates`, `fix(layer2): refuse a repeated alias in every join form`.

## Acknowledgements

- [Apache Ossie](https://github.com/apache/ossie): the spec and the example models vendored in `tests/fixtures/ossie` (Apache-2.0).
- [polyglot-sql](https://crates.io/crates/polyglot-sql): parsing, AST and multi-dialect generation.

## License

Apache License 2.0. See [LICENSE](LICENSE) and [NOTICE](NOTICE).
