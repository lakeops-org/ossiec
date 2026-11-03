//! Ossie Layer 2 `AGG(measure)` frontend (Appendix A.1, A.4, and consumer joins A.5–A.6).

use ossiec::{DialectType, Error, Options};

const SHOP: &str = include_str!("fixtures/shop/model.yaml");
const TWO_FACTS: &str = include_str!("fixtures/two_facts_layer2.yaml");

fn graph() -> ossiec::SemanticGraph {
    ossiec::from_yaml(SHOP).unwrap()
}

#[test]
fn a1_status_order_count_matches_layer3() {
    let g = graph();
    let l2 = ossiec::compile_layer2(
        &g,
        "SELECT status, AGG(order_count) FROM orders GROUP BY status",
        &Options {
            target: Some(DialectType::DuckDB),
            ..Default::default()
        },
    )
    .unwrap();
    let l3 = ossiec::compile(
        &g,
        &serde_json::from_str(
            r#"{"metrics":["orders.order_count"],"dimensions":["orders.status"]}"#,
        )
        .unwrap(),
        &Options {
            target: Some(DialectType::DuckDB),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(l2.sql, l3.sql);
    assert!(l2.sql.to_uppercase().contains("COUNT"));
}

#[test]
fn qualified_measure_and_alias() {
    let g = graph();
    let req = ossiec::layer2_to_request(
        &g,
        "SELECT o.status, AGG(o.order_count) AS n FROM orders AS o GROUP BY o.status ORDER BY n DESC LIMIT 5",
    )
    .unwrap();
    assert_eq!(req.metrics, vec!["orders.order_count"]);
    assert_eq!(req.dimensions, vec!["orders.status"]);
    assert_eq!(req.limit, Some(5));
    assert_eq!(req.order_by[0].field, "orders.order_count");
}

#[test]
fn measure_synonym_and_where() {
    let g = graph();
    let req = ossiec::layer2_to_request(
        &g,
        "SELECT MEASURE(order_count) FROM orders WHERE status = 'completed'",
    )
    .unwrap();
    assert_eq!(req.metrics, vec!["orders.order_count"]);
    assert_eq!(req.filters.len(), 1);
    assert_eq!(req.filters[0].field, "orders.status");
}

#[test]
fn model_scoped_metric_is_refused() {
    let e =
        ossiec::layer2_to_request(&graph(), "SELECT AGG(total_revenue) FROM orders").unwrap_err();
    assert!(
        matches!(e, Error::Request(ref m) if m.contains("model-scoped")),
        "{e}"
    );
}

#[test]
fn agg_of_expression_is_refused() {
    let e =
        ossiec::layer2_to_request(&graph(), "SELECT AGG(order_count + 1) FROM orders").unwrap_err();
    assert!(
        matches!(e, Error::Request(ref m) if m.contains("measure column")),
        "{e}"
    );
}

#[test]
fn inner_join_customers_emits_inner_join() {
    let g = graph();
    let opts = Options {
        target: Some(DialectType::DuckDB),
        ..Default::default()
    };
    let l2 = ossiec::compile_layer2(
        &g,
        "SELECT c.region, AGG(o.order_count) FROM orders o INNER JOIN customers c ON o.customer_id = c.id GROUP BY c.region",
        &opts,
    )
    .unwrap();
    assert!(l2.sql.to_uppercase().contains("INNER JOIN"), "{}", l2.sql);
    let l3 = ossiec::compile(
        &g,
        &serde_json::from_str(
            r#"{"metrics":["orders.order_count"],"dimensions":["customers.region"]}"#,
        )
        .unwrap(),
        &opts,
    )
    .unwrap();
    assert_ne!(l2.sql, l3.sql);
    assert!(l3.sql.to_uppercase().contains("LEFT JOIN"));
}

#[test]
fn left_join_customers_matches_layer3_agg_shape() {
    let g = graph();
    let opts = Options {
        target: Some(DialectType::DuckDB),
        ..Default::default()
    };
    let sql = "SELECT c.region, AGG(o.order_count) FROM orders o LEFT JOIN customers c ON o.customer_id = c.id GROUP BY c.region";
    let l2 = ossiec::compile_layer2(&g, sql, &opts).unwrap();
    assert!(l2.sql.to_uppercase().contains("LEFT JOIN"), "{}", l2.sql);
    let l3 = ossiec::compile(
        &g,
        &serde_json::from_str(
            r#"{"metrics":["orders.order_count"],"dimensions":["customers.region"]}"#,
        )
        .unwrap(),
        &opts,
    )
    .unwrap();
    assert_eq!(l2.sql, l3.sql);
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

#[test]
fn a4_independent_measures_via_joins_match_layer3() {
    let g = two_facts_graph();
    let opts = two_facts_opts();
    let sql = "SELECT c.region, AGG(o.order_amt), AGG(t.ticket_count) \
               FROM customers c \
               LEFT JOIN orders o ON o.customer_id = c.id \
               LEFT JOIN tickets t ON t.customer_id = c.id \
               GROUP BY c.region";
    let l2 = ossiec::compile_layer2(&g, sql, &opts).unwrap();
    assert!(l2.sql.contains("UNION ALL"), "{}", l2.sql);
    let l3 = ossiec::compile(
        &g,
        &serde_json::from_str(
            r#"{"metrics":["orders.order_amt","tickets.ticket_count"],"dimensions":["customers.region"]}"#,
        )
        .unwrap(),
        &opts,
    )
    .unwrap();
    assert_eq!(l2.sql, l3.sql);
}

#[test]
fn a4_comma_from_lists_independent_facts() {
    let g = two_facts_graph();
    let opts = two_facts_opts();
    let sql = "SELECT c.region, AGG(o.order_amt), AGG(t.ticket_count) \
               FROM customers c, orders o, tickets t \
               GROUP BY c.region";
    let req = ossiec::layer2_to_request(&g, sql).unwrap();
    assert_eq!(
        req.metrics,
        vec!["orders.order_amt", "tickets.ticket_count"]
    );
    assert_eq!(req.dimensions, vec!["customers.region"]);
    let l2 = ossiec::compile_layer2(&g, sql, &opts).unwrap();
    let l3 = ossiec::compile(&g, &req, &opts).unwrap();
    assert_eq!(l2.sql, l3.sql);
}

#[test]
fn a4_refuses_dimension_not_shared_across_facts() {
    let g = two_facts_graph();
    let sql = "SELECT o.amount, AGG(o.order_amt), AGG(t.ticket_count) \
               FROM customers c \
               LEFT JOIN orders o ON o.customer_id = c.id \
               LEFT JOIN tickets t ON t.customer_id = c.id \
               GROUP BY o.amount";
    let e = ossiec::compile_layer2(&g, sql, &two_facts_opts()).unwrap_err();
    assert!(
        matches!(e, Error::Unsupported(ref m) if m.contains("independent facts sharing")),
        "{e}"
    );
}

#[test]
fn a2_a3_derived_table_in_from_is_unsupported() {
    let g = graph();
    let e = ossiec::compile_layer2(
        &g,
        "SELECT status, AGG(order_count) FROM (SELECT * FROM orders) AS o GROUP BY status",
        &Options {
            target: Some(DialectType::DuckDB),
            ..Default::default()
        },
    )
    .unwrap_err();
    assert!(
        matches!(e, Error::Unsupported(ref m) if m.contains("A.2") && m.contains("subquer")),
        "{e}"
    );
}
