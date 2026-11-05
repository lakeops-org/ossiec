//! Layer 3, `wide_table_semantics.md`: the spec's own worked data and results
//! (§1.3, §3.4, §5.2, §6.1, Appendix B), run on DuckDB, and every row of §8.

mod common;

use common::duckdb_rows;
use duckdb::Connection;
use ossiec::{DialectType, ErrorCode, Layer2Catalog, Options, SemanticGraph};

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

/// Appendix B.5.
#[test]
fn the_filter_examples() {
    assert_eq!(
        query(
            r#"{"metrics": ["orders.total_revenue"], "filter": "customers.region = 'EAST'"}"#,
            ""
        ),
        rows(&[&["350"]])
    );
    assert_eq!(
        query(
            r#"{"dimensions": ["customers.region"], "metrics": ["orders.total_revenue"], "filter": "orders.amount > 100"}"#,
            ""
        ),
        rows(&[&["EAST", "200"], &["WEST", "300"]])
    );
    assert_eq!(
        query(
            r#"{"dimensions": ["orders.order_id", "customers.region"], "filter": "orders.amount > 100"}"#,
            ""
        ),
        rows(&[&["O2", "EAST"], &["O4", "WEST"]])
    );
    assert_eq!(
        query(
            r#"{"dimensions": ["customers.region"], "metrics": ["orders.total_revenue"], "filter": "FALSE"}"#,
            ""
        ),
        rows(&[])
    );
    assert_eq!(
        query(
            r#"{"metrics": ["orders.total_revenue"], "filter": "FALSE"}"#,
            ""
        ),
        rows(&[&["NULL"]])
    );
    // Doubled parentheses at the start are still one expression.
    assert_eq!(
        query(
            r#"{"metrics": ["orders.total_revenue"], "filter": "((orders.amount > 100)) AND orders.amount < 300"}"#,
            ""
        ),
        rows(&[&["200"]])
    );
    // Appendix B.1: the groups kept are those present before filtering.
    assert_eq!(
        query(
            r#"{"dimensions": ["customers.region"], "filter": "customers.region <> 'EAST'"}"#,
            ""
        ),
        rows(&[&["NORTH"], &["WEST"]])
    );
}

/// §3.4: for an order with no customer, `<>` removes it and `IS DISTINCT FROM` keeps it.
#[test]
fn a_filter_sees_unmatched_rows_as_null() {
    let orphan = "INSERT INTO orders VALUES ('O5', 'C9', 40);";
    let by = |filter: &str| {
        query(
            &format!(
                r#"{{"dimensions": ["customers.region"], "metrics": ["orders.total_revenue"], "filter": "{filter}"}}"#
            ),
            orphan,
        )
    };
    assert_eq!(by("customers.region <> 'WEST'"), rows(&[&["EAST", "350"]]));
    assert_eq!(
        by("customers.region IS DISTINCT FROM 'WEST'"),
        rows(&[&["EAST", "350"], &["NULL", "40"]])
    );
}

/// Appendix B.4: unconnected metrics answer together without a dimension.
#[test]
fn unconnected_metrics_answer_together() {
    let g = rejections_graph();
    let db = Connection::open_in_memory().unwrap();
    db.execute_batch(DATA).unwrap();
    db.execute_batch(
        "CREATE TABLE tickets (ticket_id VARCHAR); INSERT INTO tickets VALUES ('T1'), ('T2');
         CREATE TABLE stores (store_id VARCHAR, opened VARCHAR);
         CREATE TABLE dates (date_id VARCHAR, year INTEGER);
         CREATE TABLE sales (sale_id VARCHAR, store_id VARCHAR, sold VARCHAR, amount INTEGER);",
    )
    .unwrap();
    let run = |json: &str| {
        let req = serde_json::from_str(json).unwrap();
        let sql = ossiec::compile(&g, &req, &opts()).unwrap().sql;
        duckdb_rows(&db, &sql)
    };
    assert_eq!(
        run(r#"{"metrics": ["orders.total_revenue", "tickets.ticket_count"]}"#),
        rows(&[&["650", "2"]])
    );
    assert_eq!(
        run(r#"{"metrics": ["orders.total_revenue", "tickets.ticket_count"], "filter": "FALSE"}"#),
        rows(&[&["NULL", "0"]])
    );
}

/// Appendix B.1 and B.3 and the §5.2 examples, with a metric for each rejection
/// and two paths from `sales` to `dates`. These metrics don't stop the model loading.
const REJECTIONS: &str = r#"
version: "0.2.0.dev0"
name: rejections
datasets:
  - name: customers
    source: customers
    primary_key: [customer_id]
    fields:
      - { name: customer_id, expression: { sql: customer_id } }
      - { name: region, expression: { sql: region } }
      - { name: rate, expression: { sql: "1" } }
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
      - { name: total_amount, expression: { sql: "SUM(amount)" } }
  - name: returns
    source: returns
    primary_key: [return_id]
    fields:
      - { name: return_id, expression: { sql: return_id } }
      - { name: customer_id, expression: { sql: customer_id } }
      - { name: amount, expression: { sql: amount } }
    metrics:
      - { name: total_amount, expression: { sql: "SUM(amount)" } }
  - name: tickets
    source: tickets
    primary_key: [ticket_id]
    fields:
      - { name: ticket_id, expression: { sql: ticket_id } }
      - { name: AMOUNT, expression: { sql: amount } }
    metrics:
      - { name: ticket_count, expression: { sql: "COUNT(*)" } }
  - name: sales
    source: sales
    primary_key: [sale_id]
    fields:
      - { name: sale_id, expression: { sql: sale_id } }
      - { name: store_id, expression: { sql: store_id } }
      - { name: sold, expression: { sql: sold } }
      - { name: amount, expression: { sql: amount } }
    metrics:
      - { name: total_sales, expression: { sql: "SUM(amount)" } }
  - name: stores
    source: stores
    primary_key: [store_id]
    fields:
      - { name: store_id, expression: { sql: store_id } }
      - { name: opened, expression: { sql: opened } }
  - name: dates
    source: dates
    primary_key: [date_id]
    fields:
      - { name: date_id, expression: { sql: date_id } }
      - { name: year, expression: { sql: year } }
relationships:
  - { name: orders_to_customers, from: orders, to: customers, from_columns: [customer_id], to_columns: [customer_id] }
  - { name: returns_to_customers, from: returns, to: customers, from_columns: [customer_id], to_columns: [customer_id] }
  - { name: sales_to_stores, from: sales, to: stores, from_columns: [store_id], to_columns: [store_id] }
  - { name: sale_date, from: sales, to: dates, from_columns: [sold], to_columns: [date_id] }
  - { name: store_opening, from: stores, to: dates, from_columns: [opened], to_columns: [date_id] }
metrics:
  - { name: revenue_per_customer, expression: { sql: "SUM(orders.amount) / COUNT(DISTINCT customers.customer_id)" } }
  - { name: unaggregated, expression: { sql: "SUM(orders.amount) * customers.rate" } }
  - { name: only_a_field, expression: { sql: "orders.amount + 1" } }
  - { name: running, expression: { sql: "SUM(orders.amount) OVER (ORDER BY orders.order_id)" } }
  - { name: ranked, expression: { sql: "RANK() OVER (ORDER BY SUM(orders.amount))" } }
  - { name: two_datasets, expression: { sql: "SUM(orders.amount * LENGTH(customers.region))" } }
  - { name: foreign_filter, expression: { sql: "SUM(orders.amount) FILTER (WHERE customers.region = 'WEST')" } }
  - { name: count_star, expression: { sql: "COUNT(*)" } }
  - { name: count_star_filtered, expression: { sql: "COUNT(*) FILTER (WHERE orders.amount > 1)" } }
  - { name: sales_by_year, expression: { sql: "SUM(sales.amount * dates.year)" } }
"#;

fn rejections_graph() -> SemanticGraph {
    ossiec::from_yaml(REJECTIONS).unwrap()
}

/// The error of a request against [`REJECTIONS`].
fn refused(json: &str) -> ossiec::Error {
    let req = serde_json::from_str(json).unwrap();
    ossiec::compile(&rejections_graph(), &req, &opts())
        .expect_err(&format!("expected {json} to be refused"))
}

/// Every condition in §8, and the code and spelling it reports.
#[test]
fn every_row_of_section_8() {
    use ErrorCode::*;
    let cases: &[(ErrorCode, &str, &str)] = &[
        (
            InvalidQuery,
            "E_INVALID_QUERY",
            r#"{"metrics": ["orders.total_revenue"], "filter": "orders.amount >"}"#,
        ),
        (
            InvalidQuery,
            "E_INVALID_QUERY",
            r#"{"metrics": ["orders.total_revenue"], "filter": "orders.amount IN (SELECT 1)"}"#,
        ),
        (
            InvalidQuery,
            "E_INVALID_QUERY",
            r#"{"metrics": ["orders.total_revenue"], "filters": [{"field": "orders.amount", "op": "BETWEEN", "values": [1]}]}"#,
        ),
        (
            InvalidQuery,
            "E_INVALID_QUERY",
            r#"{"metrics": ["orders.total_revenue"], "filter": "orders.amount > 1 ORDER BY 1"}"#,
        ),
        (
            InvalidQuery,
            "E_INVALID_QUERY",
            r#"{"metrics": ["orders.total_revenue"], "filter": "TRUE UNION SELECT 1"}"#,
        ),
        (
            InvalidQuery,
            "E_INVALID_QUERY",
            r#"{"metrics": ["orders.total_revenue"], "filter": "TRUE; SELECT 1"}"#,
        ),
        (
            InvalidQuery,
            "E_INVALID_QUERY",
            r#"{"metrics": ["orders.total_revenue"], "includeUnmatched": true}"#,
        ),
        (
            InvalidQuery,
            "E_INVALID_QUERY",
            r#"{"metrics": ["orders.total_revenue"], "orderBy": [{"field": "customers.region"}]}"#,
        ),
        (EmptyAggregationQuery, "E_EMPTY_AGGREGATION_QUERY", "{}"),
        (
            NameNotFound,
            "E_NAME_NOT_FOUND",
            r#"{"dimensions": ["customers.nope"]}"#,
        ),
        (
            NameNotFound,
            "E_NAME_NOT_FOUND",
            r#"{"dimensions": ["region"]}"#,
        ),
        (NameNotFound, "E_NAME_NOT_FOUND", r#"{"metrics": ["nope"]}"#),
        (
            NameNotFound,
            "E_NAME_NOT_FOUND",
            r#"{"metrics": ["orders.total_revenue"], "filters": [{"field": "orders.nope", "op": "=", "value": 1}]}"#,
        ),
        (
            NameNotFound,
            "E_NAME_NOT_FOUND",
            r#"{"metrics": ["orders.total_revenue"], "filter": "orders.nope = 1"}"#,
        ),
        (
            NameNotFound,
            "E_NAME_NOT_FOUND",
            r#"{"metrics": ["orders.total_revenue"], "filter": "amount = 1"}"#,
        ),
        (
            MetricInDimensions,
            "E_METRIC_IN_DIMENSIONS",
            r#"{"dimensions": ["orders.total_revenue"]}"#,
        ),
        (
            MetricInDimensions,
            "E_METRIC_IN_DIMENSIONS",
            r#"{"dimensions": ["revenue_per_customer"]}"#,
        ),
        (
            FieldInMetrics,
            "E_FIELD_IN_METRICS",
            r#"{"metrics": ["customers.region"]}"#,
        ),
        (
            AggregateInWhere,
            "E_AGGREGATE_IN_WHERE",
            r#"{"metrics": ["orders.total_revenue"], "filter": "SUM(orders.amount) > 1"}"#,
        ),
        (
            AggregateInWhere,
            "E_AGGREGATE_IN_WHERE",
            r#"{"metrics": ["orders.total_revenue"], "filter": "orders.total_revenue > 1"}"#,
        ),
        (
            AggregateInWhere,
            "E_AGGREGATE_IN_WHERE",
            r#"{"metrics": ["orders.total_revenue"], "filter": "revenue_per_customer > 1"}"#,
        ),
        (
            AggregateInWhere,
            "E_AGGREGATE_IN_WHERE",
            r#"{"metrics": ["orders.total_revenue"], "filters": [{"field": "orders.total_revenue", "op": ">", "value": 1}]}"#,
        ),
        (
            WindowInWhere,
            "E_WINDOW_IN_WHERE",
            r#"{"metrics": ["orders.total_revenue"], "filter": "RANK() OVER (ORDER BY orders.amount) = 1"}"#,
        ),
        (
            CrossDatasetFilter,
            "E_CROSS_DATASET_FILTER",
            r#"{"metrics": ["orders.total_revenue"], "filter": "orders.amount > 1 AND customers.region = 'WEST'"}"#,
        ),
        (
            NameCollision,
            "E_NAME_COLLISION",
            r#"{"metrics": ["orders.total_amount", "returns.total_amount"]}"#,
        ),
        (
            NameCollision,
            "E_NAME_COLLISION",
            r#"{"dimensions": ["orders.amount", "ORDERS.AMOUNT"]}"#,
        ),
        // Output names compare under identifier normalization, whatever their spelling.
        (
            NameCollision,
            "E_NAME_COLLISION",
            r#"{"dimensions": ["orders.amount", "tickets.AMOUNT"]}"#,
        ),
        (
            NameCollision,
            "E_NAME_COLLISION",
            r#"{"dimensions": ["orders.customer_id", "customers.customer_id"]}"#,
        ),
        (
            UnaggregatedFinerGrainReference,
            "E_UNAGGREGATED_FINER_GRAIN_REFERENCE",
            r#"{"metrics": ["unaggregated"]}"#,
        ),
        (
            UnaggregatedFinerGrainReference,
            "E_UNAGGREGATED_FINER_GRAIN_REFERENCE",
            r#"{"metrics": ["only_a_field"]}"#,
        ),
        (
            DeferredWindowedMetric,
            "E_DEFERRED_WINDOWED_METRIC",
            r#"{"metrics": ["running"]}"#,
        ),
        (
            DeferredWindowedMetric,
            "E_DEFERRED_WINDOWED_METRIC",
            r#"{"metrics": ["ranked"]}"#,
        ),
        (
            AmbiguousMeasureGrain,
            "E_AMBIGUOUS_MEASURE_GRAIN",
            r#"{"metrics": ["two_datasets"]}"#,
        ),
        (
            AmbiguousMeasureGrain,
            "E_AMBIGUOUS_MEASURE_GRAIN",
            r#"{"metrics": ["foreign_filter"]}"#,
        ),
        (
            AmbiguousMeasureGrain,
            "E_AMBIGUOUS_MEASURE_GRAIN",
            r#"{"metrics": ["count_star"]}"#,
        ),
        (
            AmbiguousMeasureGrain,
            "E_AMBIGUOUS_MEASURE_GRAIN",
            r#"{"metrics": ["count_star_filtered"]}"#,
        ),
        (
            NoPath,
            "E_NO_PATH",
            r#"{"metrics": ["customers.customer_count"], "dimensions": ["stores.opened"]}"#,
        ),
        (
            NoPath,
            "E_NO_PATH",
            r#"{"metrics": ["customers.customer_count"], "dimensions": ["orders.amount"]}"#,
        ),
        // A metric reaches a dataset only if every home dataset does (§5.3).
        (
            NoPath,
            "E_NO_PATH",
            r#"{"metrics": ["revenue_per_customer"], "dimensions": ["orders.order_id"]}"#,
        ),
        (
            PartialDimensionReachability,
            "E_PARTIAL_DIMENSION_REACHABILITY",
            r#"{"metrics": ["orders.total_revenue", "tickets.ticket_count"], "dimensions": ["customers.region"]}"#,
        ),
        (
            NoAnchor,
            "E_NO_ANCHOR",
            r#"{"dimensions": ["customers.region", "stores.opened"]}"#,
        ),
        (
            UnsupportedFilterPath,
            "E_UNSUPPORTED_FILTER_PATH",
            r#"{"dimensions": ["customers.region"], "metrics": ["orders.total_revenue", "returns.total_amount"], "filter": "orders.amount > 100"}"#,
        ),
        (
            UnsupportedFilterPath,
            "E_UNSUPPORTED_FILTER_PATH",
            r#"{"dimensions": ["customers.region"], "filter": "orders.amount > 100"}"#,
        ),
        (
            UnsupportedFilterPath,
            "E_UNSUPPORTED_FILTER_PATH",
            r#"{"dimensions": ["customers.region"], "filters": [{"field": "orders.amount", "op": ">", "value": 100}]}"#,
        ),
        (
            AmbiguousPath,
            "E_AMBIGUOUS_PATH",
            r#"{"metrics": ["sales.total_sales"], "dimensions": ["dates.year"]}"#,
        ),
        (
            AmbiguousPath,
            "E_AMBIGUOUS_PATH",
            r#"{"metrics": ["sales.total_sales"], "filter": "dates.year = 2024"}"#,
        ),
        (
            AmbiguousPath,
            "E_AMBIGUOUS_PATH",
            r#"{"dimensions": ["sales.sale_id", "dates.year"]}"#,
        ),
        // What a deferred aggregate reads counts, even with no dimension.
        (
            AmbiguousPath,
            "E_AMBIGUOUS_PATH",
            r#"{"metrics": ["sales_by_year"], "deferredMetrics": true}"#,
        ),
    ];
    for (code, spelled, json) in cases {
        let e = refused(json);
        assert_eq!(e.code(), Some(*code), "{json}: {e}");
        assert!(e.to_string().starts_with(&format!("{spelled}: ")), "{e}");
    }
}

/// §8: the first row whose condition holds wins. Each request meets two adjacent rows.
#[test]
fn the_first_row_wins() {
    use ErrorCode::*;
    let cases: &[(ErrorCode, &str)] = &[
        (InvalidQuery, r#"{"filter": "orders.amount >"}"#),
        (EmptyAggregationQuery, r#"{"filter": "orders.nope = 1"}"#),
        // Row 1 needs no name to resolve, so a bad value goes before a bad name.
        (
            InvalidQuery,
            r#"{"dimensions": ["orders.nope"], "filters": [{"field": "orders.amount", "op": "BETWEEN", "values": [1]}]}"#,
        ),
        (
            NameNotFound,
            r#"{"dimensions": ["orders.total_revenue", "orders.nope"]}"#,
        ),
        (
            MetricInDimensions,
            r#"{"dimensions": ["orders.total_revenue"], "metrics": ["customers.region"]}"#,
        ),
        (
            FieldInMetrics,
            r#"{"metrics": ["customers.region"], "filter": "orders.total_revenue > 1"}"#,
        ),
        (
            AggregateInWhere,
            r#"{"metrics": ["orders.total_revenue"], "filter": "orders.total_revenue > 1 AND RANK() OVER (ORDER BY orders.amount) = 1"}"#,
        ),
        (
            WindowInWhere,
            r#"{"metrics": ["orders.total_revenue"], "filter": "RANK() OVER (ORDER BY orders.amount) = 1 AND customers.region = 'WEST'"}"#,
        ),
        (
            CrossDatasetFilter,
            r#"{"metrics": ["orders.total_amount", "returns.total_amount"], "filter": "orders.amount > 1 AND customers.region = 'WEST'"}"#,
        ),
        (
            NameCollision,
            r#"{"metrics": ["unaggregated"], "dimensions": ["customers.region", "customers.REGION"]}"#,
        ),
        // Across metrics, and in either order.
        (
            UnaggregatedFinerGrainReference,
            r#"{"metrics": ["running", "unaggregated"]}"#,
        ),
        (
            DeferredWindowedMetric,
            r#"{"metrics": ["count_star", "running"]}"#,
        ),
        (
            AmbiguousMeasureGrain,
            r#"{"metrics": ["count_star"], "dimensions": ["stores.opened"]}"#,
        ),
        // A later dimension no metric reaches goes before an earlier partial one.
        (
            NoPath,
            r#"{"metrics": ["orders.total_revenue", "customers.customer_count"], "dimensions": ["orders.order_id", "stores.opened"]}"#,
        ),
        (
            PartialDimensionReachability,
            r#"{"metrics": ["orders.total_revenue", "returns.total_amount"], "dimensions": ["orders.order_id"], "filter": "tickets.ticket_id = 'T1'"}"#,
        ),
        (
            NoAnchor,
            r#"{"dimensions": ["customers.region", "stores.opened"], "filter": "tickets.ticket_id = 'T1'"}"#,
        ),
        (
            UnsupportedFilterPath,
            r#"{"metrics": ["sales.total_sales", "orders.total_revenue"], "filter": "dates.year = 2024"}"#,
        ),
    ];
    for (code, json) in cases {
        let e = refused(json);
        assert_eq!(e.code(), Some(*code), "{json}: {e}");
    }
}

/// §8: these must identify the dimension or filter, and the metric or anchor.
#[test]
fn reachability_errors_name_both_sides() {
    let cases: &[(&str, &[&str])] = &[
        (
            r#"{"metrics": ["customers.customer_count"], "dimensions": ["orders.amount"]}"#,
            &["orders.amount", "customers.customer_count"],
        ),
        (
            r#"{"metrics": ["orders.total_revenue", "tickets.ticket_count"], "dimensions": ["customers.region"]}"#,
            &["customers.region", "tickets.ticket_count"],
        ),
        (
            r#"{"metrics": ["orders.total_revenue", "returns.total_amount"], "filter": "orders.amount > 100"}"#,
            &["orders.amount > 100", "returns.total_amount"],
        ),
        (
            r#"{"dimensions": ["customers.region"], "filter": "orders.amount > 100"}"#,
            &["orders.amount > 100", "anchor `customers`"],
        ),
    ];
    for (json, names) in cases {
        let e = refused(json).to_string();
        for name in *names {
            assert!(e.contains(name), "want `{name}` in: {e}");
        }
    }
}

/// §5.2 rejects a query naming the metric, so the rest of the model stays usable.
#[test]
fn a_metric_the_spec_rejects_does_not_stop_the_model_loading() {
    let c = rejections_graph().describe();
    let doc = |name: &str| c.metrics.iter().find(|m| m.name == name).unwrap();
    assert!(!doc("unaggregated").queryable);
    assert!(!doc("count_star").queryable);
    assert!(doc("ranked").queryable && doc("ranked").requires_deferred_metrics);
    assert!(doc("two_datasets").queryable && doc("two_datasets").requires_deferred_metrics);
    assert!(doc("revenue_per_customer").queryable);
    assert!(!doc("revenue_per_customer").requires_deferred_metrics);
    // A name that resolves to nothing is still an invalid model.
    let bad = REJECTIONS.replace("customers.rate\" }", "customers.nope\" }");
    assert_ne!(bad, REJECTIONS);
    let e = ossiec::from_yaml(&bad).err().expect("an invalid model");
    assert!(e.to_string().contains("`nope` is not a field"), "{e}");
}

/// §9: a query may ask for what the spec defers, and only that.
#[test]
fn deferred_metrics_are_answered_when_asked_for() {
    let db = Connection::open_in_memory().unwrap();
    db.execute_batch(DATA).unwrap();
    let g = rejections_graph();
    let run = |json: &str| {
        let req = serde_json::from_str(json).unwrap();
        let mut got = duckdb_rows(&db, &ossiec::compile(&g, &req, &opts()).unwrap().sql);
        got.sort();
        got
    };
    assert_eq!(
        run(
            r#"{"metrics": ["ranked"], "dimensions": ["customers.region"], "deferredMetrics": true}"#
        ),
        rows(&[&["EAST", "2"], &["WEST", "1"]])
    );
    assert_eq!(
        run(
            r#"{"metrics": ["two_datasets"], "dimensions": ["customers.region"], "deferredMetrics": true}"#
        ),
        rows(&[&["EAST", "1400"], &["WEST", "1200"]])
    );
    // Its home joins to the other dataset it reads, though nothing else needs it.
    assert_eq!(
        run(r#"{"metrics": ["two_datasets"], "deferredMetrics": true}"#),
        rows(&[&["2600"]])
    );
    // Not deferred: the property changes nothing for these.
    for (metric, code) in [
        ("unaggregated", ErrorCode::UnaggregatedFinerGrainReference),
        ("foreign_filter", ErrorCode::AmbiguousMeasureGrain),
        ("count_star", ErrorCode::AmbiguousMeasureGrain),
    ] {
        let e = refused(&format!(
            r#"{{"metrics": ["{metric}"], "deferredMetrics": true}}"#
        ));
        assert_eq!(e.code(), Some(code), "{metric}: {e}");
    }
    // Asking for deferred metrics doesn't relax reachability.
    let e = refused(
        r#"{"metrics": ["two_datasets"], "dimensions": ["stores.opened"], "deferredMetrics": true}"#,
    );
    assert_eq!(e.code(), Some(ErrorCode::NoPath), "{e}");
}

/// §9 and Appendix A.1: naming a relationship picks the path §4 won't guess.
#[test]
fn a_named_role_picks_the_path() {
    let db = Connection::open_in_memory().unwrap();
    db.execute_batch(
        "CREATE TABLE sales (sale_id VARCHAR, store_id VARCHAR, sold VARCHAR, amount INTEGER);
         INSERT INTO sales VALUES ('S1', 'A', 'D1', 10), ('S2', 'A', 'D2', 20), ('S3', 'B', 'D2', 40);
         CREATE TABLE stores (store_id VARCHAR, opened VARCHAR);
         INSERT INTO stores VALUES ('A', 'D1'), ('B', 'D1');
         CREATE TABLE dates (date_id VARCHAR, year INTEGER);
         INSERT INTO dates VALUES ('D1', 1999), ('D2', 2024);
         CREATE TABLE customers (customer_id VARCHAR, region VARCHAR);
         CREATE TABLE orders (order_id VARCHAR, customer_id VARCHAR, amount INTEGER);
         CREATE TABLE returns (return_id VARCHAR, customer_id VARCHAR, amount INTEGER);
         CREATE TABLE tickets (ticket_id VARCHAR);",
    )
    .unwrap();
    let g = rejections_graph();
    let run = |json: &str| {
        let req = serde_json::from_str(json).unwrap();
        let mut got = duckdb_rows(&db, &ossiec::compile(&g, &req, &opts()).unwrap().sql);
        got.sort();
        got
    };
    assert_eq!(
        run(r#"{"metrics": ["sales.total_sales"], "dimensions": ["sale_date.year"]}"#),
        rows(&[&["1999", "10"], &["2024", "60"]])
    );
    assert_eq!(
        run(r#"{"metrics": ["sales.total_sales"], "dimensions": ["store_opening.year"]}"#),
        rows(&[&["1999", "70"]])
    );
    assert_eq!(
        run(r#"{"metrics": ["sales.total_sales"], "dimensions": [
                {"field": "sale_date.year", "alias": "sold"},
                {"field": "store_opening.year", "alias": "opened"}
            ]}"#),
        rows(&[&["1999", "1999", "10"], &["2024", "1999", "60"]])
    );
    assert_eq!(
        run(r#"{"metrics": ["sales.total_sales"], "filter": "sale_date.year = 2024"}"#),
        rows(&[&["60"]])
    );
    // One field in two roles has one output name, unless aliased (§3.3).
    let e = refused(
        r#"{"metrics": ["sales.total_sales"], "dimensions": ["sale_date.year", "store_opening.year"]}"#,
    );
    assert_eq!(e.code(), Some(ErrorCode::NameCollision), "{e}");
    // Two roles are two datasets to `filter`, and a role can be out of reach.
    let e = refused(
        r#"{"metrics": ["sales.total_sales"], "filter": "sale_date.year = 2024 AND store_opening.year = 1999"}"#,
    );
    assert_eq!(e.code(), Some(ErrorCode::CrossDatasetFilter), "{e}");
    let e = refused(r#"{"metrics": ["orders.total_revenue"], "dimensions": ["sale_date.year"]}"#);
    assert_eq!(e.code(), Some(ErrorCode::NoPath), "{e}");
}

/// §4: a path has no repeated dataset, so relationships both ways are one path each
/// way, and the metric's home is the root whichever way the others lean.
#[test]
fn relationships_both_ways_are_not_ambiguous_for_a_metric() {
    let model = r#"
version: "0.2.0.dev0"
name: cycle
datasets:
  - name: a
    source: a
    primary_key: [id]
    fields:
      - { name: id, expression: { sql: id } }
      - { name: b_id, expression: { sql: b_id } }
      - { name: label_a, expression: { sql: label } }
    metrics:
      - { name: a_rows, expression: { sql: "COUNT(*)" } }
  - name: b
    source: b
    primary_key: [id]
    fields:
      - { name: id, expression: { sql: id } }
      - { name: a_id, expression: { sql: a_id } }
      - { name: label_b, expression: { sql: label } }
    metrics:
      - { name: b_rows, expression: { sql: "COUNT(*)" } }
relationships:
  - { name: a_to_b, from: a, to: b, from_columns: [b_id], to_columns: [id] }
  - { name: b_to_a, from: b, to: a, from_columns: [a_id], to_columns: [id] }
"#;
    let db = Connection::open_in_memory().unwrap();
    db.execute_batch(
        "CREATE TABLE a (id INTEGER, b_id INTEGER, label VARCHAR);
         INSERT INTO a VALUES (1, 1, 'x'), (2, 1, 'y');
         CREATE TABLE b (id INTEGER, a_id INTEGER, label VARCHAR);
         INSERT INTO b VALUES (1, 2, 'p');",
    )
    .unwrap();
    let g = ossiec::from_yaml(model).unwrap();
    let compile = |json: &str| {
        let req = serde_json::from_str(json).unwrap();
        ossiec::compile(&g, &req, &opts())
    };
    let sql = compile(r#"{"metrics": ["a.a_rows"], "dimensions": ["b.label_b"]}"#)
        .unwrap()
        .sql;
    assert_eq!(duckdb_rows(&db, &sql), rows(&[&["p", "2"]]));
    let sql = compile(r#"{"metrics": ["b.b_rows"], "dimensions": ["a.label_a"]}"#)
        .unwrap()
        .sql;
    assert_eq!(duckdb_rows(&db, &sql), rows(&[&["y", "1"]]));
    // Each reaches the other, so without a metric both qualify as the anchor (§5.6).
    let e = compile(r#"{"dimensions": ["a.label_a", "b.label_b"]}"#).expect_err("two anchors");
    assert_eq!(e.code(), Some(ErrorCode::AmbiguousPath), "{e}");
}

/// §3.7: the JSON binding's malformed queries.
#[test]
fn malformed_json_is_an_invalid_query() {
    for json in [
        "not json",
        r#"{"metrics": ["a"], "dimension": ["b.c"]}"#,
        r#"{"metrics": null}"#,
        r#"{"metrics": ["a"], "filter": null}"#,
        r#"{"metrics": "a"}"#,
    ] {
        let e = ossiec::Request::from_json(json).expect_err(json);
        assert_eq!(e.code(), Some(ErrorCode::InvalidQuery), "{json}: {e}");
    }
    assert!(ossiec::Request::from_json(r#"{"metrics": ["a"], "filter": "TRUE"}"#).is_ok());
}

/// `filter` is the same predicate as a structured filter, down to each engine's
/// string collation and the field's own SQL.
#[test]
fn a_filter_expression_compiles_like_a_structured_filter() {
    let g = ossiec::from_yaml(common::MODEL).unwrap();
    for target in [
        DialectType::MySQL,
        DialectType::PostgreSQL,
        DialectType::ClickHouse,
        DialectType::Trino,
        DialectType::StarRocks,
        DialectType::DuckDB,
    ] {
        let opts = Options {
            target: Some(target),
            ..Default::default()
        };
        for (structured, expression) in [
            (
                r#"{"field": "customers.name", "op": "=", "value": "ada"}"#,
                "customers.name = 'ada'",
            ),
            (
                r#"{"field": "orders.revenue", "op": ">", "value": 20}"#,
                "orders.revenue > 20",
            ),
        ] {
            let compile = |part: String| {
                let json = format!(r#"{{"metrics": ["total_revenue"], {part}}}"#);
                let req = serde_json::from_str(&json).unwrap();
                ossiec::compile(&g, &req, &opts).unwrap().sql
            };
            assert_eq!(
                compile(format!(r#""filters": [{structured}]"#)),
                compile(format!(r#""filter": "{expression}""#)),
                "{target:?}"
            );
        }
    }
}

/// `/` in a filter is true division, as in every other portable expression.
#[test]
fn a_filter_divides_like_any_expression() {
    let req = serde_json::from_str(
        r#"{"metrics": ["orders.total_revenue"], "filter": "orders.amount / orders.amount = 1"}"#,
    )
    .unwrap();
    let opts = Options {
        target: Some(DialectType::Trino),
        ..Default::default()
    };
    let sql = ossiec::compile(&graph(), &req, &opts).unwrap().sql;
    assert!(sql.contains("CAST") && sql.contains("NULLIF"), "{sql}");
}

/// The column names a request gets on DuckDB.
fn columns(json: &str) -> Vec<String> {
    let db = Connection::open_in_memory().unwrap();
    db.execute_batch(DATA).unwrap();
    let req = serde_json::from_str(json).unwrap();
    let sql = ossiec::compile(&rejections_graph(), &req, &opts())
        .unwrap()
        .sql;
    let mut st = db.prepare(&sql).unwrap();
    drop(st.query([]).unwrap());
    st.column_names()
}

/// §3.3 and §3.6: dimensions first, then metrics, by alias or final part, in the
/// model's spelling.
#[test]
fn output_names_are_the_alias_or_the_final_part() {
    assert_eq!(
        columns(
            r#"{"metrics": ["orders.total_revenue", "revenue_per_customer"],
                "dimensions": ["customers.region", "CUSTOMERS.CUSTOMER_ID"]}"#
        ),
        [
            "region",
            "customer_id",
            "total_revenue",
            "revenue_per_customer"
        ]
    );
    // §3.7's example.
    assert_eq!(
        columns(
            r#"{"dimensions": [{"field": "customers.region", "alias": "Area"}],
                "metrics": [{"metric": "orders.total_amount", "alias": "order_amount"},
                            {"metric": "returns.total_amount", "alias": "return_amount"}]}"#
        ),
        ["Area", "order_amount", "return_amount"]
    );
}

/// §3.3: an alias takes part in collisions, and clears them.
#[test]
fn aliases_decide_collisions() {
    use ErrorCode::*;
    for (code, json) in [
        (
            NameCollision,
            r#"{"dimensions": ["customers.region", {"field": "customers.customer_id", "alias": "REGION"}]}"#,
        ),
        (
            NameCollision,
            r#"{"metrics": [{"metric": "orders.total_revenue", "alias": "x"}], "dimensions": [{"field": "customers.region", "alias": "x"}]}"#,
        ),
        // No longer a collision, so the next row that applies is reported.
        (
            NoAnchor,
            r#"{"dimensions": ["orders.amount", {"field": "tickets.AMOUNT", "alias": "ticket_amount"}]}"#,
        ),
    ] {
        let e = refused(json);
        assert_eq!(e.code(), Some(code), "{json}: {e}");
    }
    // The filter reads fields, not output names (§3.3).
    let e = refused(
        r#"{"dimensions": [{"field": "customers.region", "alias": "area"}], "filter": "area = 'EAST'"}"#,
    );
    assert_eq!(e.code(), Some(NameNotFound), "{e}");
}

/// §3.7: malformed entries, and a request's JSON survives a round trip.
#[test]
fn entries_are_a_reference_or_an_object() {
    for json in [
        r#"{"dimensions": [{"metric": "orders.total_revenue"}]}"#,
        r#"{"metrics": [{"field": "customers.region"}]}"#,
        r#"{"dimensions": [{"field": "customers.region", "alias": null}]}"#,
        r#"{"dimensions": [{"field": "customers.region", "alias": ""}]}"#,
        r#"{"dimensions": [{"field": "customers.region", "label": "x"}]}"#,
        r#"{"dimensions": [{"alias": "x"}]}"#,
        r#"{"dimensions": [null]}"#,
        r#"{"dimensions": [1]}"#,
    ] {
        let e = ossiec::Request::from_json(json).expect_err(json);
        assert_eq!(e.code(), Some(ErrorCode::InvalidQuery), "{json}: {e}");
    }
    let req = ossiec::Request::from_json(
        r#"{"dimensions": ["a.b", {"field": "a.c", "alias": "d"}], "metrics": [{"metric": "m", "alias": "n"}]}"#,
    )
    .unwrap();
    let back = serde_json::to_string(&req).unwrap();
    let again = ossiec::Request::from_json(&back).unwrap();
    assert_eq!(again.dimensions, req.dimensions);
    assert_eq!(again.metrics, req.metrics);
    assert!(back.contains(r#""a.b""#), "{back}");
}

/// `orderBy` and `metricFilters` follow an aliased metric.
#[test]
fn request_properties_see_aliases() {
    let by = |order: &str| {
        query(
            &format!(
                r#"{{"dimensions": ["customers.region"],
                     "metrics": [{{"metric": "orders.total_revenue", "alias": "rev"}}],
                     "metricFilters": [{{"metric": "orders.total_revenue", "op": ">", "value": 310}}],
                     "orderBy": [{{"field": "{order}"}}]}}"#
            ),
            "",
        )
    };
    let want = rows(&[&["EAST", "350"]]);
    assert_eq!(by("rev"), want);
    assert_eq!(by("orders.total_revenue"), want);
    assert_eq!(by("region"), want);
    assert_eq!(by("customers.region"), want);
}
