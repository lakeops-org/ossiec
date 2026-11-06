//! Drift tier: the exact SQL for a fixed set of requests. Any change in output
//! shows up as a diff in `tests/golden/`. Re-snapshot with `UPDATE_GOLDEN=1 cargo test`.
//!
//! Golden files lock in output, not correctness; `tests/correctness.rs` checks answers.

use ossiec::{DialectType, Options};

const TPCDS: &str = include_str!("fixtures/ossie/tpcds_semantic_model.yaml");
const SHOP: &str = include_str!("fixtures/shop/model.yaml");
const RENAMED: &str = include_str!("fixtures/ossie/renamed_fields.yaml");

/// `request` is a JSON request (Layer 3) or AGG SQL (Layer 2).
fn golden(
    name: &str,
    model: &str,
    request: &str,
    target: Option<DialectType>,
    prefer: Option<&str>,
) {
    let graph = ossiec::from_yaml(model).unwrap();
    let opts = Options {
        target,
        prefer_dialect: prefer.map(str::to_string),
        ..Default::default()
    };
    let c = if request.starts_with('{') {
        let req: ossiec::Request = serde_json::from_str(request).unwrap();
        ossiec::compile(&graph, &req, &opts).unwrap()
    } else {
        ossiec::compile_sql(&graph, request, &opts).unwrap()
    };
    let mut got = format!(
        "-- request: {request}\n-- dialect: {} (source: {})\n",
        c.dialect, c.source_dialect
    );
    for w in &c.warnings {
        got.push_str(&format!("-- warning: {w}\n"));
    }
    got.push_str(&c.sql);
    got.push('\n');

    let path = format!("{}/tests/golden/{name}.sql", env!("CARGO_MANIFEST_DIR"));
    if std::env::var_os("UPDATE_GOLDEN").is_some() {
        std::fs::write(&path, &got).unwrap();
        return;
    }
    let want = std::fs::read_to_string(&path)
        .unwrap_or_else(|_| panic!("missing {path}; run with UPDATE_GOLDEN=1"));
    assert_eq!(
        got, want,
        "{name} drifted; if intended, re-run with UPDATE_GOLDEN=1"
    );
}

#[test]
fn tpcds_sales_by_category() {
    golden(
        "tpcds_sales_by_category",
        TPCDS,
        r#"{"metrics": ["total_sales", "total_profit"], "dimensions": ["item.i_category"]}"#,
        None,
        None,
    );
}

#[test]
fn tpcds_productivity_stacks_two_branches() {
    golden(
        "tpcds_productivity_by_state",
        TPCDS,
        r#"{"metrics": ["store_productivity", "total_sales"], "dimensions": ["store.s_state"]}"#,
        None,
        None,
    );
}

#[test]
fn tpcds_clv_bigquery() {
    golden(
        "tpcds_clv_by_first_name_bigquery",
        TPCDS,
        r#"{"metrics": ["customer_lifetime_value"], "dimensions": ["customer.c_first_name"],
            "filters": [{"field": "customer.c_last_name", "op": "IN", "values": ["Smith", "Jones"]}],
            "orderBy": [{"field": "customer_lifetime_value", "direction": "desc"}], "limit": 5}"#,
        Some(DialectType::BigQuery),
        None,
    );
}

#[test]
fn shop_filtered_aggregate_starrocks() {
    golden(
        "shop_completed_revenue_starrocks",
        SHOP,
        r#"{"metrics": ["completed_revenue"], "dimensions": ["customers.region"]}"#,
        Some(DialectType::StarRocks),
        None,
    );
}

#[test]
fn shop_filtered_aggregate_duckdb() {
    golden(
        "shop_completed_revenue_duckdb",
        SHOP,
        r#"{"metrics": ["completed_revenue"], "dimensions": ["customers.region"]}"#,
        Some(DialectType::DuckDB),
        None,
    );
}

#[test]
fn shop_month_grain_trino() {
    golden(
        "shop_month_grain_trino",
        SHOP,
        r#"{"metrics": ["total_revenue", "orders.order_count"], "dimensions": ["orders.order_date"], "timeGrain": "month",
            "filters": [{"field": "orders.order_date", "op": ">=", "value": "2026-01-01"}]}"#,
        Some(DialectType::Trino),
        None,
    );
}

#[test]
fn shop_stock_by_region_clickhouse() {
    golden(
        "shop_stock_by_region_clickhouse",
        SHOP,
        "SELECT c.region, AGG(p.stock_units), AGG(o.gross_revenue) FROM orders o \
         LEFT JOIN customers c ON o.customer_id = c.id \
         LEFT JOIN products p ON o.product_id = p.id GROUP BY c.region",
        Some(DialectType::ClickHouse),
        None,
    );
}

#[test]
fn renamed_fields_substitute_field_expressions() {
    golden(
        "renamed_fields_revenue_by_segment",
        RENAMED,
        r#"{"metrics": ["total_revenue"], "dimensions": ["customers.segment"]}"#,
        None,
        None,
    );
}

#[test]
fn vendor_dialect_expression_is_used_when_preferred() {
    let model = r#"
version: "0.2.0.dev0"
name: vendor
datasets:
  - name: events
    source: analytics.events
    fields:
      - name: day
        expression:
          sql: CAST(event_ts AS DATE)
          dialects:
            - { dialect: SNOWFLAKE, expression: "TO_DATE(event_ts)" }
        datatype: Date
      - { name: user_id, expression: { sql: user_id } }
metrics:
  - { name: users, expression: { sql: COUNT(DISTINCT events.user_id) } }
"#;
    let req = r#"{"metrics": ["users"], "dimensions": ["events.day"]}"#;
    golden("vendor_portable", model, req, None, None);
    golden(
        "vendor_prefer_snowflake",
        model,
        req,
        None,
        Some("SNOWFLAKE"),
    );
}

#[test]
fn tpcds_window_metrics() {
    golden(
        "tpcds_cumulative_sales_snowflake",
        TPCDS,
        r#"{"metrics": ["cumulative_sales", "total_sales"], "dimensions": ["date_dim.d_date"], "deferredMetrics": true, "orderBy": [{"field": "date_dim.d_date"}]}"#,
        Some(DialectType::Snowflake),
        None,
    );
    golden(
        "tpcds_top3_brands_per_store_trino",
        TPCDS,
        r#"{"metrics": ["brand_rank_in_store", "total_sales"], "dimensions": ["store.s_store_sk", "item.i_brand"], "deferredMetrics": true,
            "metricFilters": [{"metric": "brand_rank_in_store", "op": "<=", "value": 3}],
            "orderBy": [{"field": "store.s_store_sk"}, {"field": "brand_rank_in_store"}]}"#,
        Some(DialectType::Trino),
        None,
    );
    golden(
        "tpcds_monthly_sales_change",
        TPCDS,
        r#"{"metrics": ["monthly_sales_change"], "dimensions": ["date_dim.d_year", "date_dim.d_moy"], "deferredMetrics": true}"#,
        None,
        None,
    );
}

#[test]
fn clickhouse_lag_keeps_standard_null_semantics() {
    golden(
        "shop_revenue_change_clickhouse",
        SHOP,
        r#"{"metrics": ["revenue_change"], "dimensions": ["orders.order_date"], "timeGrain": "month", "deferredMetrics": true}"#,
        Some(DialectType::ClickHouse),
        None,
    );
}

#[test]
fn clickhouse_lead_keeps_standard_null_semantics() {
    golden(
        "shop_revenue_change_to_next_clickhouse",
        SHOP,
        r#"{"metrics": ["revenue_change_to_next"], "dimensions": ["orders.order_date"], "timeGrain": "month", "deferredMetrics": true}"#,
        Some(DialectType::ClickHouse),
        None,
    );
}

#[test]
fn mysql_collates_string_dimensions_and_filters() {
    golden(
        "shop_string_collation_mysql",
        SHOP,
        r#"{"metrics": ["total_revenue"], "dimensions": ["customers.segment"],
            "filters": [{"field": "customers.region", "op": "IN", "values": ["EU", "US"]}]}"#,
        Some(DialectType::MySQL),
        None,
    );
}

#[test]
fn bigquery_week_starts_on_monday() {
    golden(
        "shop_week_grain_bigquery",
        SHOP,
        r#"{"metrics": ["total_revenue"], "dimensions": ["orders.order_date"], "timeGrain": "week"}"#,
        Some(DialectType::BigQuery),
        None,
    );
}

#[test]
fn starrocks_doubles_backslashes_in_literals() {
    golden(
        "shop_backslash_literal_starrocks",
        SHOP,
        r#"{"dimensions": ["customers.name"], "filters": [{"field": "customers.name", "op": "IN", "values": ["O'Brien", "C:\\temp"]}]}"#,
        Some(DialectType::StarRocks),
        None,
    );
}

#[test]
fn postgres_types_stacked_nulls_and_collates_strings() {
    golden(
        "shop_two_branches_postgres",
        SHOP,
        r#"{"metrics": ["total_revenue", "total_stock"], "dimensions": ["products.category"],
            "orderBy": [{"field": "products.category"}]}"#,
        Some(DialectType::PostgreSQL),
        None,
    );
}
