<div align="center">

# ossiec

**The Apache Ossie compiler.**
Define a metric once. Get correct SQL for any engine.

[![CI](https://github.com/lakeops-org/ossiec/actions/workflows/ci.yml/badge.svg)](https://github.com/lakeops-org/ossiec/actions/workflows/ci.yml)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)
[![Ossie spec](https://img.shields.io/badge/Apache%20Ossie-0.2.0.dev0-orange.svg)](https://github.com/apache/ossie/blob/main/core-spec/spec.md)

</div>

---

[Apache Ossie](https://ossie.apache.org/) is the vendor-neutral standard for semantic models: datasets, fields, relationships and metrics in YAML. Ossie defines what a metric *means*. It doesn't say how to turn one into SQL.

**ossiec does that part.** Give it an Ossie model and a request ("`total_revenue` by `customers.region`, last quarter") and it returns one SQL statement, in the dialect you need, that can't double-count.

```console
$ echo '{"metrics": ["total_stock"], "dimensions": ["customers.region"]}' \
    | ossiec compile shop.yaml - --dialect snowflake
```

## Why a compiler?

Ask a person or an LLM for "units in stock by customer region" over a star schema and you'll usually get this:

```sql
SELECT c.region, SUM(p.stock)
FROM orders o
LEFT JOIN customers c ON o.customer_id = c.customer_id
LEFT JOIN products  p ON o.product_id  = p.product_id
GROUP BY c.region
```

It runs, it looks right, and it's wrong. Stock lives on `products`, but the join repeats each product once per order. On the [test shop](tests/common/mod.rs):

| region | naive SQL | ossiec | truth |
|---|---:|---:|---|
| EU | **250** | **150** | EU bought products 10 and 11: 100 + 50 units |
| US | **60** | **30** | US bought product 12, twice: 30 units |

ossiec sees that `SUM(products.stock)` reads only the "one" side of a many-to-one join, so it evaluates the sum over rows de-duplicated on `products`' primary key. The test suite runs both queries on DuckDB and checks that the naive one really is wrong, so the test can't pass for the wrong reason.

## Features

- **Reads the current Ossie spec** (`0.2.0.dev0`, model at the document root): datasets with table or query sources, fields, primary keys, relationships, dataset- and model-scoped metrics, `ai_context`, `custom_extensions`. Every [Ossie example and compliance fixture](tests/fixtures/ossie) binds.
- **No fan traps or chasm traps by construction.** Joins only go many→one from a single root. Measures from different grains are computed separately and stacked with `UNION ALL`, never joined to each other ([how](#how-it-works)).
- **Engine-native output, checked on real engines.** One AST, rendered by [polyglot-sql](https://crates.io/crates/polyglot-sql) for Trino, Athena, StarRocks, ClickHouse, DuckDB, MySQL, Postgres, Snowflake, BigQuery, Databricks, Redshift and 20+ more. Every correctness case runs on **DuckDB, Postgres, Trino, StarRocks, ClickHouse and MySQL** and must return the same rows. Where an engine answers differently, a small [dialect rule](src/dialect/) fixes it (for example, ClickHouse's `lag` returns `0` instead of NULL for the first row).
- **Window metrics:** running totals, rankings and period-over-period (`SUM(SUM(x)) OVER (…)`, `RANK() OVER (…)`, `LAG`), computed over the grouped result. `describe` tells callers which dimensions each one needs.
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
ok: `shop` — 3 datasets, 2 relationships, 13 metrics (0 not queryable yet)

$ ossiec describe shop.yaml      # metrics, dimensions, ai_context as JSON
```

Compile a request. This one mixes a fact measure with a one-side measure:

```console
$ echo '{"metrics": ["total_revenue", "total_stock"], "dimensions": ["customers.region"]}' \
    | ossiec compile shop.yaml - --dialect duckdb
warning: FAN_TRAP_RISK: `SUM("products".stock)` is computed over `products` rows de-duplicated by primary key; ...
```

```sql
SELECT
  stacked.d0 AS "customers.region",
  MAX(stacked.t0) AS "total_revenue",
  MAX(stacked.t1) AS "total_stock"
FROM (
  -- revenue at the order grain
  SELECT "customers".region AS d0, SUM("orders".amount_cents / 100.0) AS t0, NULL AS t1
  FROM orders AS "orders"
  LEFT JOIN (SELECT * FROM raw_customers WHERE NOT deleted) AS "customers"
    ON "orders".customer_id = "customers".customer_id
  LEFT JOIN products AS "products" ON "orders".product_id = "products".product_id
  GROUP BY "customers".region
  UNION ALL
  -- stock over distinct (region, product) rows
  SELECT dedup.d0 AS d0, NULL AS t0, SUM(dedup.c0) AS t1
  FROM (
    SELECT DISTINCT "customers".region AS d0, "products".product_id AS k0, "products".stock AS c0
    FROM orders AS "orders"
    LEFT JOIN (SELECT * FROM raw_customers WHERE NOT deleted) AS "customers"
      ON "orders".customer_id = "customers".customer_id
    LEFT JOIN products AS "products" ON "orders".product_id = "products".product_id
    WHERE NOT "products".product_id IS NULL
  ) AS dedup
  GROUP BY dedup.d0
) AS stacked
GROUP BY stacked.d0
```

*(Condensed and commented for the README; real output is pretty-printed without comments.)*

The same request with `--dialect bigquery`, `clickhouse` or `starrocks` renders that engine's quoting and functions. With no `--dialect`, ossiec emits the model's own source dialect, which is what a downstream translator wants.

### From Rust

```rust
let graph = ossiec::from_yaml(&std::fs::read_to_string("shop.yaml")?)?; // load + validate once
let request: ossiec::Request = serde_json::from_str(r#"{
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
println!("{:?}", out.warnings); // e.g. FAN_TRAP_RISK
```

### Layer 2 (`AGG(measure)`)

Ossie Layer 2 Appendix A.1–A.6: `AGG` over dataset-scoped measures; arithmetic on `AGG(...)` in the select list (ratios); independent facts that share dimensions; consumer joins along model relationships, including unused many→one joins and reverse fan-out (G2 via primary-key de-duplication); `INNER` / `LEFT` / `RIGHT` / `FULL` / `CROSS` join kinds (§7.6: null-extended and Cartesian rows do not inflate `AGG`); **A.2** derived tables joined into an `AGG` stage; **A.3** `AVG`/`SUM`/`COUNT`/`MIN`/`MAX` over derived tables (with `IS NOT DISTINCT FROM` join keys). Consumer aliases (`AS n`) name the output columns. `layer2_to_request` returns leaf measures of flat queries only; nested SQL is `compile_sql` / `compile-sql` (aliases: `compile_layer2` / `compile-l2`) only. **§7.7** unevaluated measures: a derived table may pass a dataset-scoped measure through its select list and an outer `AGG(t.m)` evaluates it (join form, or a sole derived `FROM`, which is folded into the outer stage). **Query-local `TO MEASURE`** in a derived table's select list, consumed by an outer `AGG(t.m)`, is implemented for both `(AGG(a) / AGG(b)) TO MEASURE AS m` and a base `SUM|COUNT|AVG|MIN|MAX(...) TO MEASURE AS m` (unevaluated until that outer `AGG`). **`CREATE VIEW` (Appendix A.7)** stores that relation in a session [`Layer2Catalog`](src/front/catalog.rs) (`define_layer2_view` / `compile_sql_with`, or a `compile-sql` script). The model YAML is not modified.

**Roles.** Qualify a column with the relationship name, the same form as a Layer 3 dimension: `departs_from.city`. That name may also be the JOIN alias (`JOIN airports AS departs_from`). A plain `airports.city` still takes the unique shortest path and is refused when departure and arrival both tie.

**`includeUnmatched`.** One-side rows no fact reaches are off unless `Options { include_unmatched: true, .. }` or a comment `-- ossiec: includeUnmatched` (or `compile-l2 --include-unmatched`). It applies to the outermost `AGG` stage, including one that joins a derived table onto the unmatched grain. Layer 3 still uses the request field; `Options.include_unmatched` does not change it.

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
      LEFT JOIN tickets t ON t.customer_id = c.id
      GROUP BY c.region) m ON r.region = m.region
GROUP BY r.region;

-- Base measure: SUM(...) stays unevaluated until the outer AGG
SELECT r.region, AGG(m.total_revenue)
FROM customers r
JOIN (SELECT c.region, SUM(o.amount) TO MEASURE AS total_revenue
      FROM customers c LEFT JOIN orders o ON o.customer_id = c.id
      GROUP BY c.region) m ON r.region = m.region
GROUP BY r.region;

-- A.7: the same relation, named. The view is session state, not a model change.
CREATE VIEW region_metrics AS
SELECT c.region,
       (AGG(o.order_amt) / AGG(t.ticket_count)) TO MEASURE AS sales_per_ticket
FROM customers c
LEFT JOIN orders o ON o.customer_id = c.id
LEFT JOIN tickets t ON t.customer_id = c.id
GROUP BY c.region;

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

A derived table joined into an `AGG` stage must be unique on its join columns (G2), is read only in `ON`, and needs an alias on every `AGG(...)`. It attaches only to legs that host its dataset: when independent facts are measured together, a join on one fact restricts that fact and leaves the others unrestricted. Nesting is limited to 4 levels and 8 derived tables per query. `includeUnmatched` cannot be combined with a derived join.

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
```

Model-scoped metrics are not measures (§10.3). `AGG(expr)` inside `AGG()` is refused; use `AGG(a) + AGG(b)`. Other join forms (`ASOF`, `SEMI`/`ANTI`, `NATURAL`, `LATERAL`, …) are not implemented yet. `CROSS JOIN` takes no `ON`.

## Requests

```jsonc
{
  "metrics": ["total_revenue", "orders.order_count"], // model metrics by name, dataset metrics as dataset.metric
  "dimensions": ["customers.region", "orders.order_date"],
  "filters": [
    {"field": "orders.status", "op": "IN", "values": ["completed", "shipped"]},
    {"field": "orders.order_date", "op": "BETWEEN", "values": ["2026-01-01", "2026-03-31"]}
  ],
  "metricFilters": [{"metric": "total_revenue", "op": ">", "value": 1000}], // HAVING
  "timeGrain": "month",                    // day | week | month | quarter | year
  "timeDimension": "orders.order_date",    // only needed when several time dimensions are requested
  "orderBy": [{"field": "total_revenue", "direction": "desc", "nulls": "last"}], // nulls: first | last (optional)
  "limit": 100,
  "includeUnmatched": false                // see below
}
```

**Roles.** When a dataset is reachable more than one way (a customer's first-sale date and the sale's own date both lead to `date_dim`), a plain `date_dim.d_year` takes the **shortest** path. To use another, name the relationship: `customer_first_sales.d_year`. A request can use several roles at once, each joined under its own alias. If two paths tie, the request is refused with the relationship names to choose from.

**Independent facts.** Measures from facts that don't join each other (store sales and web sales by item category) are computed in one join tree per fact and stacked, so groups are the union across facts: a category sold only online still appears. Every requested dimension and filter must be reachable from each fact.

**`includeUnmatched`.** By default a one-side measure only sees rows a fact reaches: stock of products that sold. With `"includeUnmatched": true`, rows no fact reaches (products never sold, customers who never ordered) are added as a group of their own, where the fact's dimensions are NULL. Filters on the fact side exclude them, as SQL NULL semantics say.

Leave out `metrics` to list **dimension values** instead: `{"dimensions": ["customers.region"]}` returns every region in `customers`. Dimensions from several datasets return only the combinations that occur, joined through the fact that links them. Agents use this to filter with real values.

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
2. **Pick a join root.** Find the dataset from which every referenced dataset is reachable along many→one relationships, and `LEFT JOIN` from it. Many→one joins never multiply root rows, and `LEFT` keeps orphan rows in the NULL group instead of dropping them.
3. **Classify each term.**
   - A term that reads the root, or that duplicates can't change (`MIN`, `MAX`, `COUNT(DISTINCT …)`), is computed **inline** over the joined rows.
   - A term that reads only one-side data (`SUM(products.stock)` over an `orders` root) is computed over rows **de-duplicated on that dataset's primary key**, which is the reference strategy in Ossie's relational spec (§9).
4. **Stack, don't join.** Each strategy becomes a part grouped by the same dimensions. Parts are combined with `UNION ALL` and `MAX` per term, so there's no join predicate between parts to get wrong.
5. **Windows last.** Window functions in a metric run in the final `SELECT`, over one row per group: `SUM(SUM(x)) OVER (ORDER BY d)` becomes `SUM(base.t0) OVER (ORDER BY base.d0)`. Fields a window partitions or orders by must be requested dimensions. When windows are present, `metricFilters` apply to the finished rows, so "rank ≤ 3" means the top three by the full ranking.
6. **Render.** Build one polyglot AST and apply the target's [dialect rules](src/dialect/) (only where a real-engine test showed polyglot's rendering answers differently). Generate it in the source dialect, then transpile to the target.

If ossiec can't make a request correct, it refuses it rather than guessing:

| Situation | Response |
|---|---|
| Dimensions or filters several facts could link, with no measure saying which (item prices by year, when store and web sales both have items and dates) | `… joined by several facts (…); … independent facts need a measure from each` |
| A dataset reachable two ways of equal length (departure *and* arrival airport) | `… in more than one way (departs_from \| arrives_at); name the one you mean as <relationship>.<field>` |
| A one-side measure on a dataset with no `primary_key` | `… declare a primary_key on 'products'` |
| A window metric without the dimension it orders by | `metric 'cumulative_revenue' is a window over 'orders.order_date'; add it to 'dimensions'` |

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
| `FILTER (WHERE …)`, `COUNT(DISTINCT …)`, ratios, `CASE`, scalar functions | ✅ |
| ANSI identifier rules (bare names case-insensitive, quoted exact) | ✅ |
| Window functions in metrics (`OVER`, `RANK`, `LAG`, running totals) | ✅ |
| Ossie ontology documents (Layer 4) | Rejected with a clear error |
| Pre-0.2 `semantic_model:` documents | Rejected with a clear error |

One leniency: Ossie requires `dataset.field` in model-scoped metrics, but the compliance fixtures include a bare form (`SUM(amount)`). ossiec accepts a bare name when exactly one dataset has that field, and warns.

## Testing

```console
cargo test                        # everything (the first run downloads DuckDB's prebuilt library)
scripts/engines.sh up && cargo test --test engines -- --ignored   # same cases on Postgres, Trino, StarRocks, ClickHouse, MySQL
cargo test --test tpcds -- --include-ignored                      # TPC-DS on all six engines (DuckDB alone needs no Docker)
cargo test --test correctness     # answers vs hand-written SQL on DuckDB
UPDATE_GOLDEN=1 cargo test --test golden   # re-snapshot SQL after an intended change
scripts/sync-ossie-fixtures.sh    # re-vendor Apache Ossie examples, then cargo test
```

| Suite | What it proves |
|---|---|
| [`correctness`](tests/correctness.rs) | **The numbers are right.** 31 requests run on DuckDB against [seed data](tests/common/mod.rs) built to break naive SQL (fan-out, NULL keys, a deleted customer, a guest checkout, a product that never sold), compared with hand-written reference SQL. |
| [`engines`](tests/engines.rs) | **Every engine agrees.** The same 31 cases compiled for and executed on Postgres 16, Trino 484, StarRocks 4.1, ClickHouse 26.8 and MySQL 8.4 in Docker, compared with the DuckDB reference answer row by row, with no type leniency (a month must come back as a `DATE` everywhere, and NULL must stay distinct from `''`). |
| [`tpcds`](tests/tpcds.rs) | **Real data, and the benchmark's own SQL.**<br>• **Apache Ossie's TPC-DS example model, unchanged,** on generated TPC-DS data: all its metric kinds, including the de-duplicated `store_productivity` and the window metrics.<br>• **Official TPC-DS queries q3, q7, q12, q42, q43, q52, q55, q96 and q98**, expressed as ossiec requests and compared row by row with TPC-DS's own query text.<br>26 cases, including multi-hop joins, composite keys, date roles, store-and-web independent facts and unmatched items, on DuckDB (sf=0.1, 288k `store_sales` rows), Trino (`tpcds.tiny`), Postgres, StarRocks and ClickHouse. Each reference runs on the same engine, so data-generator differences cancel out. |
| [`fuzz`](tests/fuzz.rs) | **Cases nobody wrote.** Random requests compared with an independent reference generated from the spec's own evaluation strategy, on all six engines. |
| [`scale`](tests/scale.rs) | **No blow-ups.** A 121-table model binds in about 0.1 s and compiles in about 10 ms per request (unoptimized build). On Trino sf=1, each TPC-DS case must run within 3× its reference. |
| [`golden`](tests/golden.rs) | **The SQL didn't drift.** Exact output for TPC-DS and shop requests across generic, BigQuery, Trino, StarRocks, ClickHouse and Snowflake, in [`tests/golden/`](tests/golden). |
| [`model`](tests/model.rs) | **Spec conformance.** Every Ossie example binds, every compliance metric (M0–M3) returns its expected answer on DuckDB, and there's one test per validation rule. |
| [`requests`](tests/requests.rs) | **Good failures.** Error messages, refusals, all 11 engines (including window requests; native `FILTER` only where supported), determinism. |
| [`sql_front`](tests/sql_front.rs) | **Layer 2 AGG.** A.1, A.2/A.3 (derived tables, executed on DuckDB, including a partial-fact join over independent facts; refusals; 11-engine compile matrix), A.4 (multi-fact + `AGG` arithmetic), A.5 (unused joins + fan-out), A.6 (`INNER`/`LEFT`), §7.6 (`RIGHT`/`FULL`/`CROSS`), §7.7 pass-through and query-local `TO MEASURE` (AGG compositions and base `SUM|COUNT|AVG|MIN|MAX`), role-playing dimensions, `includeUnmatched` (flat and derived onto the unmatched grain), A.7 `CREATE VIEW` measure relations (session catalog, DuckDB oracle). The shop forms of A.2/A.3 and §7.7 (fold, outer `WHERE`, join pass-through) also run in the engine tier (`layer2_nested_*`); `TO MEASURE` is DuckDB + compile-matrix only. |

## Known issues

**Semantics to know**
- **De-duplicated groups overlap.** A product sold in two regions counts in both, so per-group values don't sum to a grand total. ossiec warns (`FAN_TRAP_RISK`). Products that never sold aren't in any group unless the request sets `includeUnmatched`; or query the measure without dimensions for the catalogue total.
- **Window metrics see only the rows in the result.** A running total over a filtered date range starts at the range's first row, and `stock_share` is a share of the groups returned. With windows present, metric filters apply after the window.
- **Division is always true division.** `/` in portable expressions casts the numerator to `DOUBLE` unless one side is a fractional literal, so `COUNT(a) / COUNT(b)` isn't truncated on Postgres and Trino. Ratios therefore come back as floating point. `amount_cents / 100.0` stays exact.
- **NULL ordering** follows generic SQL on every engine: NULLs sort first in ascending order (`NULLS FIRST` is emitted where needed).
- **Strings compare and sort by byte on every engine**, so `'Bob'` comes before `'ada'` and `'ada'` never equals `'Ada'`. Postgres string dimensions carry `COLLATE "C"`, and MySQL string dimensions and filters `COLLATE utf8mb4_bin`, to match.
- **A backslash in a `LIKE` pattern is an ordinary character**, as in SQL without `ESCAPE`. On engines that escape with it by default (Postgres, ClickHouse, StarRocks, MySQL), ossiec doubles it.
- **Groups don't depend on the measures you ask for.** A group appears whenever some joined row has it, and a measure with no rows there reads NULL (`COUNT` reads 0).
- **Time grains ignore time zones.** Truncating a `DateTimeTz` uses the engine's session time zone.

**Refused for now:** nested windows; a dataset reached two ways of equal length unless the role is named.

**Spec leniency:** bare names in model-scoped metrics are accepted with a warning.

**Engine coverage:** answers are verified on DuckDB, Postgres, Trino, StarRocks, ClickHouse and MySQL, on the shop cases, TPC-DS and fuzzing. Athena, Snowflake, BigQuery, Databricks and Redshift are compile-checked only. See [docs/ADDING_AN_ENGINE.md](docs/ADDING_AN_ENGINE.md) to move an engine up.

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
- [x] Cross-engine test tier: Postgres, Trino, StarRocks, ClickHouse, MySQL
- [x] TPC-DS: Ossie's example model on real data, plus official queries q3, q7, q42, q52, q55, q98
- [x] Official TPC-DS queries q3, q7, q12, q42, q43, q52, q55, q96, q98
- [ ] More official TPC-DS queries (many of the 99 need constructs no semantic request expresses: correlated subqueries, cross-channel set operations)
- [x] Window metrics (cumulative, rank, period-over-period)
- [x] Independent facts across shared dimensions
- [x] Role-playing dimensions (`relationship.field`, shortest path by default)
- [x] Unmatched one-side rows (`includeUnmatched`)
- [x] NULL placement in `orderBy`
- [x] Ossie Layer 2: SQL with `AGG(measure)` (A.1–A.6 incl. nested derived tables; §7.6 `RIGHT`/`FULL`/`CROSS`; §7.7 pass-through and query-local `TO MEASURE`, including base `SUM|COUNT|AVG|MIN|MAX(...)`; role-playing dimensions and `includeUnmatched`, including a derived join on the unmatched grain; A.7 `CREATE VIEW` in a session catalog, not written into the model)
- [ ] Publish to crates.io

## Contributing

Adding or verifying an engine? Follow **[docs/ADDING_AN_ENGINE.md](docs/ADDING_AN_ENGINE.md)**.


Issues and PRs are welcome. Before opening a PR:

```console
cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test
```

New planner behaviour needs a **correctness** case (a seed row that would expose the bug, plus reference SQL), not just a golden file.

## Acknowledgements

- [Apache Ossie](https://github.com/apache/ossie): the spec and the example models vendored in `tests/fixtures/ossie` (Apache-2.0).
- [polyglot-sql](https://crates.io/crates/polyglot-sql): parsing, AST and multi-dialect generation.

## License

Apache License 2.0. See [LICENSE](LICENSE) and [NOTICE](NOTICE).
