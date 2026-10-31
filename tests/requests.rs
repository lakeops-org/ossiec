//! Request validation, unsupported shapes, dialect coverage and determinism.

use ossiec::{DialectType, Error, Options, Request};

const SHOP: &str = include_str!("fixtures/shop/model.yaml");

fn req(json: &str) -> Request {
    serde_json::from_str(json).unwrap()
}

fn compile_err(model: &str, json: &str) -> Error {
    let g = ossiec::from_yaml(model).unwrap();
    ossiec::compile(&g, &req(json), &Options::default()).expect_err("expected an error")
}

#[test]
fn errors_name_the_problem_and_the_choices() {
    let cases = [
        (r#"{"metrics": []}"#, "at least one metric or dimension"),
        (
            r#"{"metrics": ["revenue"]}"#,
            "unknown metric `revenue`; available: orders.order_count, total_revenue",
        ),
        (
            r#"{"metrics": ["total_revenue"], "dimensions": ["orders.nope"]}"#,
            "`nope` is not a field of `orders`; fields: id, customer_id",
        ),
        (
            r#"{"metrics": ["total_revenue"], "dimensions": ["region"]}"#,
            "use `dataset.field`",
        ),
        (
            r#"{"metrics": ["total_revenue"], "dimensions": ["customers.region"], "timeGrain": "month"}"#,
            "timeGrain needs a requested time dimension",
        ),
        (
            r#"{"metrics": ["total_revenue"], "orderBy": [{"field": "customers.region"}]}"#,
            "must be a requested dimension or metric",
        ),
        (
            r#"{"metrics": ["total_revenue"], "metricFilters": [{"metric": "total_stock", "op": ">", "value": 1}]}"#,
            "must also be a requested metric",
        ),
        (
            r#"{"metrics": ["total_revenue"], "filters": [{"field": "orders.status", "op": "BETWEEN", "values": [1]}]}"#,
            "BETWEEN needs exactly two",
        ),
        (
            r#"{"metrics": ["total_revenue"], "filters": [{"field": "orders.status", "op": "=", "value": null}]}"#,
            "IS NULL",
        ),
    ];
    for (json, want) in cases {
        let got = compile_err(SHOP, json).to_string();
        assert!(got.contains(want), "want `{want}` in: {got}");
    }
}

#[test]
fn unknown_request_keys_are_rejected() {
    let e =
        serde_json::from_str::<Request>(r#"{"metrics": ["x"], "dimension": ["a.b"]}"#).unwrap_err();
    assert!(e.to_string().contains("unknown field `dimension`"), "{e}");
}

const TWO_FACTS: &str = r#"
version: "0.2.0.dev0"
name: two_facts
datasets:
  - { name: orders, source: orders, fields: [{name: customer_id, expression: {sql: customer_id}}, {name: amount, expression: {sql: amount}}] }
  - { name: tickets, source: tickets, fields: [{name: customer_id, expression: {sql: customer_id}}, {name: id, expression: {sql: id}}] }
  - { name: customers, source: customers, fields: [{name: id, expression: {sql: id}}, {name: region, expression: {sql: region}}] }
relationships:
  - { name: o_c, from: orders, to: customers, from_columns: [customer_id], to_columns: [id] }
  - { name: t_c, from: tickets, to: customers, from_columns: [customer_id], to_columns: [id] }
metrics:
  - { name: revenue, expression: { sql: SUM(orders.amount) } }
  - { name: tickets, expression: { sql: COUNT(tickets.id) } }
"#;

#[test]
fn independent_facts_are_stacked_not_joined() {
    // No dataset reaches both orders and tickets: one leg per fact, stacked.
    let g = ossiec::from_yaml(TWO_FACTS).unwrap();
    let c = ossiec::compile(
        &g,
        &req(r#"{"metrics": ["revenue", "tickets"], "dimensions": ["customers.region"]}"#),
        &Options::default(),
    )
    .unwrap();
    assert!(c.sql.contains("UNION ALL"), "{}", c.sql);
    assert!(
        !c.sql.contains("JOIN tickets") || !c.sql.contains("JOIN orders"),
        "facts must not be joined to each other:\n{}",
        c.sql
    );
}

#[test]
fn dimension_only_request_across_facts_is_ambiguous() {
    let e = compile_err(
        TWO_FACTS,
        r#"{"dimensions": ["customers.region"], "filters": [{"field": "orders.amount", "op": ">", "value": 0}, {"field": "tickets.id", "op": ">", "value": 0}]}"#,
    );
    assert!(matches!(e, Error::Unsupported(_)), "{e}");
}

#[test]
fn role_playing_ties_are_refused_and_roles_can_be_named() {
    let model = r#"
version: "0.2.0.dev0"
name: flights
datasets:
  - { name: flights, source: flights, fields: [{name: origin, expression: {sql: origin}}, {name: dest, expression: {sql: dest}}, {name: delay, expression: {sql: delay}}] }
  - { name: airports, source: airports, primary_key: [code], fields: [{name: code, expression: {sql: code}}, {name: city, expression: {sql: city}}] }
relationships:
  - { name: departs_from, from: flights, to: airports, from_columns: [origin], to_columns: [code] }
  - { name: arrives_at, from: flights, to: airports, from_columns: [dest], to_columns: [code] }
metrics:
  - { name: total_delay, expression: { sql: SUM(flights.delay) } }
"#;
    let e = compile_err(
        model,
        r#"{"metrics": ["total_delay"], "dimensions": ["airports.city"]}"#,
    )
    .to_string();
    assert!(
        e.contains("departs_from") && e.contains("arrives_at"),
        "{e}"
    );
}

#[test]
fn one_side_measure_without_primary_key_is_refused() {
    let model = SHOP.replace(
        "    source: products\n    primary_key: [id]\n",
        "    source: products\n",
    );
    let e = compile_err(
        &model,
        r#"{"metrics": ["total_stock"], "dimensions": ["customers.region"]}"#,
    );
    assert!(
        matches!(e, Error::Unsupported(ref m) if m.contains("declare a primary_key on `products`")),
        "{e}"
    );
}

#[test]
fn compiles_for_every_engine_with_portable_constructs_only() {
    let g = ossiec::from_yaml(SHOP).unwrap();
    let requests = [
        r#"{"metrics": ["completed_revenue", "orders.order_count"], "dimensions": ["products.category"]}"#,
        r#"{"metrics": ["total_stock", "total_revenue"], "dimensions": ["customers.region"]}"#,
        r#"{"metrics": ["total_revenue"], "dimensions": ["orders.order_date"], "timeGrain": "quarter", "orderBy": [{"field": "orders.order_date"}], "limit": 10}"#,
        r#"{"metrics": ["cumulative_revenue", "revenue_change"], "dimensions": ["orders.order_date"], "timeGrain": "month"}"#,
        r#"{"metrics": ["revenue_rank_in_region", "stock_share"], "dimensions": ["customers.region", "products.category"], "metricFilters": [{"metric": "revenue_rank_in_region", "op": "<=", "value": 2}]}"#,
    ];
    for target in [
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
    ] {
        for r in requests {
            let opts = Options {
                target: Some(target),
                ..Default::default()
            };
            let c =
                ossiec::compile(&g, &req(r), &opts).unwrap_or_else(|e| panic!("{target:?}: {e}"));
            // Constructs polyglot passes through, or emulates incorrectly, on engines that
            // lack them (verified on polyglot-sql 0.13.4). ossiec must never emit them.
            let upper = c.sql.to_uppercase();
            for construct in ["FILTER(", "QUALIFY", "BY NAME", "GROUP BY ALL"] {
                assert!(
                    !upper.contains(construct),
                    "{target:?} emitted {construct}:\n{}",
                    c.sql
                );
            }
        }
    }
}

#[test]
fn same_input_same_sql_and_hash() {
    let g = ossiec::from_yaml(SHOP).unwrap();
    let r =
        req(r#"{"metrics": ["total_stock", "total_revenue"], "dimensions": ["customers.region"]}"#);
    let a = ossiec::compile(&g, &r, &Options::default()).unwrap();
    let b = ossiec::compile(&ossiec::from_yaml(SHOP).unwrap(), &r, &Options::default()).unwrap();
    assert_eq!((a.sql, a.request_hash), (b.sql, b.request_hash));
}

#[test]
fn describe_lists_metrics_dimensions_and_context() {
    let c = ossiec::from_yaml(SHOP).unwrap().describe();
    let rev = c
        .metrics
        .iter()
        .find(|m| m.name == "total_revenue")
        .unwrap();
    assert_eq!(rev.ai_context.as_ref().unwrap()["synonyms"][0], "sales");
    let date = c
        .dimensions
        .iter()
        .find(|d| d.name == "orders.order_date")
        .unwrap();
    assert!(date.is_time && date.declared_dimension);
    assert!(c.metrics.iter().any(|m| m.name == "orders.order_count"));
    let rank = c
        .metrics
        .iter()
        .find(|m| m.name == "revenue_rank_in_region")
        .unwrap();
    assert_eq!(rank.requires_dimensions, ["customers.region"]);
}
