//! Ossie Layer 2 `AGG(measure)` frontend (Appendix A.1).

use ossiec::{DialectType, Error, Options};

const SHOP: &str = include_str!("fixtures/shop/model.yaml");

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
    let e = ossiec::layer2_to_request(&graph(), "SELECT AGG(total_revenue) FROM orders").unwrap_err();
    assert!(
        matches!(e, Error::Request(ref m) if m.contains("model-scoped")),
        "{e}"
    );
}

#[test]
fn agg_of_expression_is_refused() {
    let e = ossiec::layer2_to_request(&graph(), "SELECT AGG(order_count + 1) FROM orders").unwrap_err();
    assert!(
        matches!(e, Error::Request(ref m) if m.contains("measure column")),
        "{e}"
    );
}

#[test]
fn joins_are_not_yet_supported() {
    let e = ossiec::layer2_to_request(
        &graph(),
        "SELECT AGG(order_count) FROM orders o JOIN customers c ON o.customer_id = c.id",
    )
    .unwrap_err();
    assert!(matches!(e, Error::Unsupported(_)), "{e}");
}
