//! Appendix A.1 and naming: model-qualified relations, measure names, aliases
//! and output labels, and what is not a Layer 2 query.

use super::*;

/// §11.2: a model's relations live in one schema named after the model, so
/// `shop.orders` means the same as `orders`, in any part of the query.
#[test]
fn model_qualified_relations_compile_the_same_as_bare_ones() {
    let g = graph();
    let opts = Options {
        target: Some(DialectType::DuckDB),
        ..Default::default()
    };
    for (qualified, bare) in [
        (
            "SELECT status, AGG(order_count) FROM shop.orders GROUP BY status",
            "SELECT status, AGG(order_count) FROM orders GROUP BY status",
        ),
        (
            "SELECT orders.status, AGG(orders.order_count) FROM SHOP.orders GROUP BY orders.status",
            "SELECT orders.status, AGG(orders.order_count) FROM orders GROUP BY orders.status",
        ),
        (
            "SELECT c.region, AGG(o.order_count) FROM shop.orders o \
             LEFT JOIN shop.customers c ON o.customer_id = c.id GROUP BY c.region",
            "SELECT c.region, AGG(o.order_count) FROM orders o \
             LEFT JOIN customers c ON o.customer_id = c.id GROUP BY c.region",
        ),
        (
            "SELECT t.status, AGG(t.order_count) \
             FROM (SELECT status, order_count FROM shop.orders) t GROUP BY t.status",
            "SELECT t.status, AGG(t.order_count) \
             FROM (SELECT status, order_count FROM orders) t GROUP BY t.status",
        ),
        // Three-part columns, in every clause that reads one.
        (
            "SELECT shop.orders.status, AGG(shop.orders.order_count) FROM shop.orders \
             WHERE shop.orders.status = 'completed' GROUP BY shop.orders.status \
             ORDER BY shop.orders.status",
            "SELECT orders.status, AGG(orders.order_count) FROM orders \
             WHERE orders.status = 'completed' GROUP BY orders.status \
             ORDER BY orders.status",
        ),
        (
            "SELECT SHOP.customers.region, AGG(shop.orders.order_count) FROM shop.orders \
             LEFT JOIN shop.customers ON shop.orders.customer_id = shop.customers.id \
             GROUP BY shop.customers.region",
            "SELECT customers.region, AGG(orders.order_count) FROM orders \
             LEFT JOIN customers ON orders.customer_id = customers.id \
             GROUP BY customers.region",
        ),
        (
            "SELECT t.status, AGG(t.n) FROM (SELECT shop.orders.status, \
             COUNT(shop.orders.id) TO MEASURE AS n FROM shop.orders) t GROUP BY t.status",
            "SELECT t.status, AGG(t.n) FROM (SELECT orders.status, \
             COUNT(orders.id) TO MEASURE AS n FROM orders) t GROUP BY t.status",
        ),
    ] {
        assert_eq!(
            ossiec::compile_layer2(&g, qualified, &opts).unwrap().sql,
            ossiec::compile_layer2(&g, bare, &opts).unwrap().sql,
            "{qualified}"
        );
    }
}

/// A view's body goes through the same names as a query.
#[test]
fn model_qualified_columns_work_in_a_view_body() {
    let g = graph();
    let opts = Options {
        target: Some(DialectType::DuckDB),
        ..Default::default()
    };
    let compile = |body: &str| {
        let mut catalog = Layer2Catalog::new();
        ossiec::define_layer2_view(&g, &mut catalog, &format!("CREATE VIEW v AS {body}"), &opts)
            .unwrap();
        ossiec::compile_layer2_with(
            &g,
            &catalog,
            "SELECT status, AGG(n) FROM v GROUP BY status",
            &opts,
        )
        .unwrap()
        .sql
    };
    assert_eq!(
        compile(
            "SELECT shop.orders.status, COUNT(shop.orders.id) TO MEASURE AS n \
             FROM shop.orders"
        ),
        compile(
            "SELECT orders.status, COUNT(orders.id) TO MEASURE AS n \
             FROM orders"
        ),
    );
}

/// Only the model's own name qualifies a column.
#[test]
fn columns_qualified_by_another_schema_are_refused() {
    let g = graph();
    let e = ossiec::compile_layer2(
        &g,
        "SELECT other.orders.status, AGG(order_count) FROM orders GROUP BY other.orders.status",
        &Options::default(),
    )
    .expect_err("another schema");
    assert!(
        matches!(e, Error::Unsupported(_) | Error::Request(_)),
        "{e}"
    );
}

#[test]
fn relations_outside_the_model_are_refused() {
    let g = graph();
    let opts = Options::default();
    for sql in [
        "SELECT AGG(order_count) FROM other.orders",
        "SELECT AGG(order_count) FROM prod.shop.orders",
    ] {
        let e = ossiec::compile_layer2(&g, sql, &opts)
            .err()
            .unwrap_or_else(|| panic!("accepted: {sql}"));
        assert!(
            matches!(e, Error::Unsupported(ref m) if m.contains("`shop.dataset`")),
            "{sql}: {e}"
        );
    }
    let e = ossiec::compile_layer2(&g, "SELECT AGG(order_count) FROM shop.nope", &opts)
        .expect_err("unknown dataset");
    assert!(
        matches!(e, Error::Request(ref m) if m.contains("unknown Layer 2 relation")),
        "{e}"
    );
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
fn consumer_aliases_name_output_columns() {
    let g = graph();
    let sql = "SELECT status AS st, AGG(order_count) AS n FROM orders GROUP BY status ORDER BY n DESC, st";
    let c = ossiec::compile_layer2(&g, sql, &duck()).unwrap();
    assert!(c.sql.contains("AS \"st\""), "{}", c.sql);
    assert!(c.sql.contains("AS \"n\""), "{}", c.sql);
    assert!(!c.sql.contains("orders.status"), "{}", c.sql);
    let db = duckdb();
    let got = duckdb_rows(&db, &c.sql);
    let want = duckdb_rows(
        &db,
        "SELECT status, COUNT(*) AS n FROM orders GROUP BY status ORDER BY n DESC, status",
    );
    assert_eq!(got, want);
}

#[test]
fn group_by_without_agg_equals_dimension_listing() {
    let g = graph();
    let with =
        ossiec::compile_layer2(&g, "SELECT status FROM orders GROUP BY status", &duck()).unwrap();
    let without = ossiec::compile_layer2(&g, "SELECT status FROM orders", &duck()).unwrap();
    assert_eq!(with.sql, without.sql);
    let e = ossiec::compile_layer2(
        &g,
        "SELECT status FROM orders GROUP BY customer_id",
        &duck(),
    )
    .unwrap_err();
    assert!(
        matches!(e, Error::Request(ref m) if m.contains("must match the selected dimensions")),
        "{e}"
    );
}

#[test]
fn nested_queries_are_not_layer3_requests() {
    let m = unsupported_msg(ossiec::layer2_to_request(&graph(), A2_SHOP).unwrap_err());
    assert!(m.contains("compile_layer2"), "{m}");
}

/// §3.1: columns behave as in standard SQL, named by their final part, and a
/// relation's column names are unique.
#[test]
fn columns_are_named_as_in_standard_sql() {
    let g = ossiec::from_yaml(common::MODEL).unwrap();
    let db = duckdb();
    let columns = |sql: &str| {
        let compiled = ossiec::compile_sql(&g, sql, &duck()).unwrap();
        let mut st = db.prepare(&compiled.sql).unwrap();
        drop(st.query([]).unwrap());
        st.column_names()
    };
    assert_eq!(
        columns("SELECT o.status, AGG(o.order_count) FROM orders o GROUP BY o.status"),
        ["status", "order_count"]
    );
    assert_eq!(
        columns("SELECT status AS s, AGG(order_count) AS n FROM orders GROUP BY status"),
        ["s", "n"]
    );
    assert_eq!(
        columns("SELECT status FROM orders GROUP BY status"),
        ["status"]
    );
    // ORDER BY takes the short name, the qualified column, or the alias.
    for order in ["status", "o.status", "order_count", "AGG(o.order_count)"] {
        let sql = format!(
            "SELECT o.status, AGG(o.order_count) FROM orders o GROUP BY o.status ORDER BY {order}"
        );
        assert_eq!(columns(&sql), ["status", "order_count"], "{order}");
    }
    // Two columns can't share a name, spelled either way.
    for sql in [
        "SELECT o.id, c.id FROM orders o LEFT JOIN customers c ON o.customer_id = c.id GROUP BY o.id, c.id",
        "SELECT status, status FROM orders GROUP BY status",
        "SELECT status AS n, AGG(order_count) AS N FROM orders GROUP BY status",
    ] {
        let e = ossiec::compile_sql(&g, sql, &duck()).expect_err(sql).to_string();
        assert!(e.contains("two output columns named"), "{sql}: {e}");
    }
    assert_eq!(
        columns(
            "SELECT o.id, c.id AS customer FROM orders o LEFT JOIN customers c ON o.customer_id = c.id GROUP BY o.id, c.id"
        ),
        ["id", "customer"]
    );
}
