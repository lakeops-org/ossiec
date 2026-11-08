//! SQL frontend: Ossie Layer 2 `AGG(measure)` surface (relational_semantics).
//! One module per spec area; shared fixtures and helpers live here.

#[path = "../common/mod.rs"]
mod common;

use common::{
    duckdb, duckdb_rows, A2_SHOP, A2_SHOP_REF, A3_SHOP, A3_SHOP_REF, LAYER2_NESTED_CASES,
};
use ossiec::{DialectType, Error, Layer2Catalog, Layer2ColumnKind, Options};

mod basics;
mod derived;
mod joins;
mod measures;
mod multi_fact;
mod operators;
mod persist;
mod roles_unmatched;
mod views;

const SHOP: &str = include_str!("../fixtures/shop/model.yaml");

const TWO_FACTS: &str = include_str!("../fixtures/two_facts_layer2.yaml");

const FANOUT: &str = include_str!("../fixtures/fanout_layer2.yaml");

fn graph() -> ossiec::SemanticGraph {
    ossiec::from_yaml(SHOP).unwrap()
}

fn two_facts_graph() -> ossiec::SemanticGraph {
    ossiec::from_yaml(TWO_FACTS).unwrap()
}

fn two_facts_opts() -> Options {
    Options {
        target: Some(DialectType::DuckDB),
        ..Default::default()
    }
}

fn duck() -> Options {
    Options {
        target: Some(DialectType::DuckDB),
        ..Default::default()
    }
}

fn sorted(mut rows: Vec<Vec<String>>) -> Vec<Vec<String>> {
    rows.sort();
    rows
}

fn unsupported_msg(e: Error) -> String {
    match e {
        Error::Unsupported(m) => m,
        other => panic!("expected Unsupported, got {other}"),
    }
}

const NESTED: &str = include_str!("../fixtures/layer2_nested.yaml");

fn nested_graph() -> ossiec::SemanticGraph {
    ossiec::from_yaml(NESTED).unwrap()
}

/// Revenue per state: 1 → 30, 2 → 35, 3 → 101, 4 → 2, NULL → 7.
fn nested_db() -> duckdb::Connection {
    let c = duckdb::Connection::open_in_memory().unwrap();
    c.execute_batch(
        "CREATE TABLE states (id INTEGER, population INTEGER);
         INSERT INTO states VALUES (1, 100), (2, 200), (3, 300), (4, 400);
         CREATE TABLE sales (id INTEGER, state_id INTEGER, category VARCHAR, amount INTEGER, qty INTEGER);
         INSERT INTO sales VALUES
           (1, 1, 'A', 10, 1), (2, 1, 'B', 20, 2), (3, 2, 'A', 30, 3), (4, 2, 'A', 5, 1),
           (5, 3, 'B', 100, 4), (6, 3, 'A', 1, 1), (7, NULL, 'A', 7, 1), (8, 4, 'B', 2, 1);",
    )
    .unwrap();
    c
}

const ENGINES: [DialectType; 11] = [
    DialectType::Trino,
    DialectType::Athena,
    DialectType::StarRocks,
    DialectType::ClickHouse,
    DialectType::DuckDB,
    DialectType::MySQL,
    DialectType::PostgreSQL,
    DialectType::Snowflake,
    DialectType::BigQuery,
    DialectType::Databricks,
    DialectType::Redshift,
];

fn rows_f64(rows: Vec<Vec<String>>) -> Vec<(String, f64)> {
    let mut v: Vec<(String, f64)> = rows
        .into_iter()
        // NULL (a ratio over no sales) is -1: no real value here is negative.
        .map(|r| (r[0].clone(), r[1].parse::<f64>().unwrap_or(-1.0)))
        .collect();
    v.sort_by(|a, b| a.0.cmp(&b.0));
    v
}

/// EU: amount 60 over 3 tickets (20); US: 5 over 3 tickets; APAC: no orders, one ticket.
fn two_facts_db() -> duckdb::Connection {
    let c = duckdb::Connection::open_in_memory().unwrap();
    c.execute_batch(
        "CREATE TABLE customers (id INTEGER, region VARCHAR);
         INSERT INTO customers VALUES (1, 'EU'), (2, 'EU'), (3, 'US'), (4, 'APAC'), (5, NULL);
         CREATE TABLE orders (customer_id INTEGER, amount INTEGER);
         INSERT INTO orders VALUES (1, 10), (1, 20), (2, 30), (3, 5);
         CREATE TABLE tickets (customer_id INTEGER, id INTEGER);
         INSERT INTO tickets VALUES (1, 100), (2, 101), (2, 102), (3, 103), (3, 104), (3, 105), (4, 106);",
    )
    .unwrap();
    c
}

const SALES_PER_TICKET: &str = "SELECT c.region, \
           (AGG(o.order_amt) / AGG(t.ticket_count)) TO MEASURE AS sales_per_ticket \
    FROM customers c \
    LEFT JOIN orders o ON o.customer_id = c.id \
    LEFT JOIN tickets t ON t.customer_id = c.id";

fn l3(g: &ossiec::SemanticGraph, json: &str) -> ossiec::Compiled {
    ossiec::compile(g, &serde_json::from_str(json).unwrap(), &duck()).unwrap()
}

const REGION_METRICS: &str = "CREATE VIEW region_metrics AS \
    SELECT c.region, \
           (AGG(o.order_amt) / AGG(t.ticket_count)) TO MEASURE AS sales_per_ticket \
    FROM customers c \
    LEFT JOIN orders o ON o.customer_id = c.id \
    LEFT JOIN tickets t ON t.customer_id = c.id";

/// The ratio per customer region. With `keep_null` the NULL region is a group
/// too (customer 5 has no region and bought nothing): a view written
/// `customers LEFT JOIN ...` drops no customer. A query that inner-joins on
/// `region` drops it.
fn region_ratio_oracle(keep_null: bool) -> String {
    let filter = if keep_null {
        ""
    } else {
        " WHERE region IS NOT NULL"
    };
    format!(
        "SELECT c.region, o.s * 1.0 / NULLIF(t.n, 0) \
         FROM (SELECT DISTINCT region FROM customers{filter}) c \
         LEFT JOIN (SELECT x.region, SUM(o.amount) AS s FROM orders o \
                    JOIN customers x ON o.customer_id = x.id GROUP BY x.region) o \
           ON o.region IS NOT DISTINCT FROM c.region \
         LEFT JOIN (SELECT x.region, COUNT(*) AS n FROM tickets t \
                    JOIN customers x ON t.customer_id = x.id GROUP BY x.region) t \
           ON t.region IS NOT DISTINCT FROM c.region"
    )
}

fn assert_region_ratio(sql: &str, keep_null: bool) {
    let got = rows_f64(duckdb_rows(&two_facts_db(), sql));
    let want = rows_f64(duckdb_rows(
        &two_facts_db(),
        &region_ratio_oracle(keep_null),
    ));
    assert_eq!(got.len(), want.len(), "{got:?} vs {want:?}");
    for (g, w) in got.iter().zip(&want) {
        assert_eq!(g.0, w.0);
        assert!((g.1 - w.1).abs() < 1e-9, "{got:?} vs {want:?}");
    }
    let value = |region: &str| got.iter().find(|r| r.0 == region).map(|r| r.1);
    // APAC has a ticket and no orders: a NULL ratio (-1 here).
    assert_eq!(value("APAC"), Some(-1.0), "{got:?}");
    assert_eq!(value("EU"), Some(20.0), "{got:?}");
    assert!((value("US").unwrap() - 5.0 / 3.0).abs() < 1e-5, "{got:?}");
    assert_eq!(value("NULL"), keep_null.then_some(-1.0), "{got:?}");
}

fn define_region_metrics() -> (ossiec::SemanticGraph, Layer2Catalog) {
    let g = two_facts_graph();
    let mut catalog = Layer2Catalog::new();
    let name =
        ossiec::define_layer2_view(&g, &mut catalog, REGION_METRICS, &two_facts_opts()).unwrap();
    assert_eq!(name, "region_metrics");
    let cols = catalog.columns("region_metrics").unwrap();
    assert_eq!(cols[0].name, "region");
    assert_eq!(cols[0].kind, Layer2ColumnKind::Field);
    assert_eq!(cols[1].name, "sales_per_ticket");
    assert_eq!(cols[1].kind, Layer2ColumnKind::UnevaluatedMeasure);
    (g, catalog)
}

fn answers(sql: &str, reference: &str) {
    let g = ossiec::from_yaml(common::MODEL).unwrap();
    let db = duckdb();
    let c = ossiec::compile_sql(&g, sql, &duck()).unwrap_or_else(|e| panic!("{sql}: {e}"));
    let want = sorted(duckdb_rows(&db, reference));
    assert_eq!(sorted(duckdb_rows(&db, &c.sql)), want, "{sql}\n{}", c.sql);
    assert!(!want.is_empty(), "{sql}: proves nothing");
}
