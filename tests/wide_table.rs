//! Layer 3, `wide_table_semantics.md`: the spec's own worked data and results
//! (§1.3, §5.2, §6.1, Appendix B), run on DuckDB.

mod common;

use common::duckdb_rows;
use duckdb::Connection;
use ossiec::{DialectType, Layer2Catalog, Options, SemanticGraph};

/// Appendix B.1 and B.2. Metrics are dataset-scoped except `revenue_per_customer`,
/// the §5.2 example.
const MODEL: &str = r#"
version: "0.2.0.dev0"
name: appendix_b
datasets:
  - name: customers
    source: customers
    primary_key: [customer_id]
    fields:
      - { name: customer_id, expression: { sql: customer_id } }
      - { name: region, expression: { sql: region } }
    metrics:
      - { name: customer_count, expression: { sql: "COUNT(*)" } }
  - name: orders
    source: orders
    primary_key: [order_id]
    fields:
      - { name: order_id, expression: { sql: order_id } }
      - { name: customer_id, expression: { sql: customer_id } }
      - { name: amount, expression: { sql: amount } }
    metrics:
      - { name: total_revenue, expression: { sql: "SUM(amount)" } }
  - name: returns
    source: returns
    primary_key: [return_id]
    fields:
      - { name: return_id, expression: { sql: return_id } }
      - { name: customer_id, expression: { sql: customer_id } }
      - { name: amount, expression: { sql: amount } }
    metrics:
      - { name: total_returns, expression: { sql: "SUM(amount)" } }
  - name: visits
    source: visits
    primary_key: [visit_id]
    fields:
      - { name: visit_id, expression: { sql: visit_id } }
      - { name: customer_id, expression: { sql: customer_id } }
    metrics:
      - { name: visit_count, expression: { sql: "COUNT(*)" } }
relationships:
  - { name: orders_to_customers, from: orders, to: customers, from_columns: [customer_id], to_columns: [customer_id] }
  - { name: returns_to_customers, from: returns, to: customers, from_columns: [customer_id], to_columns: [customer_id] }
  - { name: visits_to_customers, from: visits, to: customers, from_columns: [customer_id], to_columns: [customer_id] }
metrics:
  - name: revenue_per_customer
    expression: { sql: "SUM(orders.amount) / COUNT(DISTINCT customers.customer_id)" }
"#;

const DATA: &str = "
CREATE TABLE customers (customer_id VARCHAR, region VARCHAR);
INSERT INTO customers VALUES ('C1', 'EAST'), ('C2', 'EAST'), ('C3', 'WEST'), ('C4', 'NORTH');
CREATE TABLE orders (order_id VARCHAR, customer_id VARCHAR, amount INTEGER);
INSERT INTO orders VALUES ('O1', 'C1', 100), ('O2', 'C1', 200), ('O3', 'C2', 50), ('O4', 'C3', 300);
CREATE TABLE returns (return_id VARCHAR, customer_id VARCHAR, amount INTEGER);
INSERT INTO returns VALUES ('R1', 'C1', 25), ('R2', 'C4', 10);
CREATE TABLE visits (visit_id VARCHAR, customer_id VARCHAR);
INSERT INTO visits VALUES ('V1', 'C4'), ('V2', 'C4'), ('V3', 'C3');
";

fn graph() -> SemanticGraph {
    ossiec::from_yaml(MODEL).unwrap()
}

fn opts() -> Options {
    Options {
        target: Some(DialectType::DuckDB),
        ..Default::default()
    }
}

fn sorted_rows(sql: &str, extra: &str) -> Vec<Vec<String>> {
    let db = Connection::open_in_memory().unwrap();
    db.execute_batch(DATA).unwrap();
    db.execute_batch(extra).unwrap();
    let mut rows = duckdb_rows(&db, sql);
    rows.sort();
    rows
}

fn query(request: &str, extra: &str) -> Vec<Vec<String>> {
    let req = serde_json::from_str(request).unwrap();
    sorted_rows(
        &ossiec::compile(&graph(), &req, &opts()).unwrap().sql,
        extra,
    )
}

fn rows(want: &[&[&str]]) -> Vec<Vec<String>> {
    let mut want: Vec<Vec<String>> = want
        .iter()
        .map(|r| {
            r.iter()
                .map(|c| match c.parse::<f64>() {
                    Ok(n) => format!("{n:.6}"),
                    Err(_) => c.to_string(),
                })
                .collect()
        })
        .collect();
    want.sort();
    want
}

/// §5.2: each aggregate reads its home dataset, so `c2` counts C4, who has no
/// orders. A plan over one join from `orders` gives 650 / 3.
#[test]
fn model_scoped_metric_reads_each_home_dataset() {
    assert_eq!(
        query(r#"{"metrics": ["revenue_per_customer"]}"#, ""),
        rows(&[&["162.5"]])
    );
}

/// §5.5: the keys are the union of the branches', so NORTH is a group even though
/// no order reaches it, and its revenue takes SUM's empty-input value.
#[test]
fn model_scoped_metric_groups_come_from_every_branch() {
    assert_eq!(
        query(
            r#"{"metrics": ["revenue_per_customer"], "dimensions": ["customers.region"]}"#,
            ""
        ),
        rows(&[&["EAST", "175"], &["NORTH", "NULL"], &["WEST", "300"]])
    );
}

/// §5.4: the filter applies in every branch.
#[test]
fn a_filter_applies_to_every_branch() {
    assert_eq!(
        query(
            r#"{"metrics": ["revenue_per_customer"],
                "filters": [{"field": "customers.region", "op": "=", "value": "EAST"}]}"#,
            ""
        ),
        rows(&[&["175"]])
    );
}

/// §5.4 and §5.5: an order with no match stays, in a NULL group from its own
/// branch. The customers branch has no row for that group, so its COUNT(*) takes
/// the empty-input value, 0.
#[test]
fn an_unmatched_home_row_is_a_null_group() {
    let orphan = "INSERT INTO orders VALUES ('O5', 'C9', 40);";
    assert_eq!(
        query(
            r#"{"metrics": ["orders.total_revenue", "customers.customer_count"],
                "dimensions": ["customers.region"]}"#,
            orphan
        ),
        rows(&[
            &["EAST", "350", "2"],
            &["NORTH", "NULL", "1"],
            &["NULL", "40", "0"],
            &["WEST", "300", "1"],
        ])
    );
}

/// §1.3 and Appendix B.1.
#[test]
fn two_branches() {
    assert_eq!(
        query(
            r#"{"dimensions": ["customers.region"],
                "metrics": ["orders.total_revenue", "returns.total_returns"]}"#,
            ""
        ),
        rows(&[
            &["EAST", "350", "25"],
            &["NORTH", "NULL", "10"],
            &["WEST", "300", "NULL"],
        ])
    );
}

/// Appendix B.2: EAST has no visits, so COUNT(*) takes its empty-input value, and
/// NORTH, missing from `orders`, is still one row.
#[test]
fn three_branches() {
    assert_eq!(
        query(
            r#"{"dimensions": ["customers.region"],
                "metrics": ["orders.total_revenue", "returns.total_returns", "visits.visit_count"]}"#,
            ""
        ),
        rows(&[
            &["EAST", "350", "25", "0"],
            &["NORTH", "NULL", "10", "2"],
            &["WEST", "300", "NULL", "1"],
        ])
    );
}

/// §6.1: the groups returned depend on the metrics requested.
#[test]
fn groups_depend_on_the_metrics() {
    let dim = r#""dimensions": ["customers.region"]"#;
    assert_eq!(
        query(
            &format!(r#"{{{dim}, "metrics": ["orders.total_revenue"]}}"#),
            ""
        ),
        rows(&[&["EAST", "350"], &["WEST", "300"]])
    );
    assert_eq!(
        query(
            &format!(r#"{{{dim}, "metrics": ["returns.total_returns"]}}"#),
            ""
        ),
        rows(&[&["EAST", "25"], &["NORTH", "10"]])
    );
    assert_eq!(
        query(&format!("{{{dim}}}"), ""),
        rows(&[&["EAST"], &["NORTH"], &["WEST"]])
    );
}

/// Layer 2 keeps closure (relational semantics §8.4): the query's own joins decide
/// the rows, so no measure brings in a group of its own.
#[test]
fn layer2_is_not_branched() {
    let g = graph();
    let catalog = Layer2Catalog::new();
    let sql = "SELECT c.region, AGG(o.total_revenue), AGG(c.customer_count) \
               FROM orders o LEFT JOIN customers c ON o.customer_id = c.customer_id \
               GROUP BY c.region";
    let compiled = ossiec::compile_layer2_with(&g, &catalog, sql, &opts()).unwrap();
    assert_eq!(
        sorted_rows(&compiled.sql, ""),
        rows(&[&["EAST", "350", "2"], &["WEST", "300", "1"]])
    );
}
