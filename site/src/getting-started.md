# Getting started

## Install

```console
cargo install --git https://github.com/lakeops-org/ossiec
```

or as a library:

```toml
[dependencies]
ossiec = { git = "https://github.com/lakeops-org/ossiec" }
```

## A model

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
    metrics:
      - { name: order_count, expression: { sql: COUNT(*) } }
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
    metrics:
      - { name: stock_units, expression: { sql: SUM(stock) } }
relationships:
  - { name: orders_to_customers, from: orders, to: customers, from_columns: [customer_id], to_columns: [id] }
  - { name: orders_to_products, from: orders, to: products, from_columns: [product_id], to_columns: [id] }
metrics:
  - name: total_revenue
    expression: { sql: SUM(orders.revenue) }
  - name: total_stock
    expression: { sql: SUM(products.stock) }
```

The full test model is [`tests/fixtures/shop/model.yaml`](https://github.com/lakeops-org/ossiec/blob/main/tests/fixtures/shop/model.yaml).

## Check it

```console
$ ossiec validate shop.yaml
ok: `shop` — 3 datasets, 2 relationships, 4 metrics (0 not queryable yet)

$ ossiec describe shop.yaml      # metrics, dimensions and ai_context, as JSON
```

## Ask a question (Layer 3)

Name metrics and dimensions; ossiec chooses the joins.

```console
$ echo '{"metrics": ["total_revenue", "total_stock"], "dimensions": ["products.category"]}' \
    | ossiec compile shop.yaml - --dialect duckdb
```

Each metric reads its own home dataset in its own branch, and the branches are stacked, never joined to each other. So `total_stock` counts every product, sold or not:

```sql
SELECT stacked.d0 AS "category", MAX(stacked.t0) AS "total_revenue", MAX(stacked.t1) AS "total_stock"
FROM (
  SELECT "products".category AS d0, SUM("orders".amount_cents / 100.0) AS t0, NULL AS t1
  FROM orders AS "orders"
  LEFT JOIN products AS "products" ON "orders".product_id = "products".product_id
  GROUP BY "products".category
  UNION ALL
  SELECT "products".category AS d0, NULL AS t0, SUM("products".stock) AS t1
  FROM products AS "products"
  GROUP BY "products".category
) AS stacked
GROUP BY stacked.d0
```

Change `--dialect` to `bigquery`, `clickhouse`, `snowflake`, `trino`, … for that engine's SQL. With no `--dialect`, ossiec emits the model's own source dialect, for a downstream translator. More in [Layer 3: semantic requests](layer3.md).

## Write the SQL yourself (Layer 2)

Write the joins; read measures with `AGG()`:

```console
$ echo "SELECT c.region, AGG(o.order_count), AGG(p.stock_units)
        FROM orders o
        LEFT JOIN customers c ON o.customer_id = c.id
        LEFT JOIN products p ON o.product_id = p.id
        GROUP BY c.region" \
    | ossiec compile-sql shop.yaml - --dialect postgres
```

`AGG(p.stock_units)` sums each product's stock once per region, however many orders repeat it. ossiec also prints a warning:

```text
warning: FAN_TRAP_RISK: `SUM("products".stock)` is computed over `products` rows de-duplicated by primary key; a `products` row that belongs to several groups counts in each, so groups do not add up to a grand total
```

A product bought in two regions counts in both, which is right per region, but the regions don't add up to the total stock. More in [Layer 2: SQL with AGG()](layer2.md).

## From Rust

```rust
let graph = ossiec::from_yaml(&std::fs::read_to_string("shop.yaml")?)?; // load and validate once

// Layer 3
let request = ossiec::Request::from_json(r#"{
    "metrics": ["total_revenue"],
    "dimensions": ["orders.order_date"],
    "timeGrain": "month",
    "filters": [{"field": "orders.order_date", "op": ">=", "value": "2026-01-01"}]
}"#)?;
let opts = ossiec::Options { target: Some(ossiec::DialectType::Trino), ..Default::default() };
let out = ossiec::compile(&graph, &request, &opts)?;
println!("{}", out.sql);

// Layer 2
let out = ossiec::compile_sql(&graph, "SELECT status, AGG(order_count) FROM orders GROUP BY status", &opts)?;
```

A rejected Layer 3 request is `Error::Query { code, message }`; `error.code()` gives the spec's code.
