//! A.7 views persisted into the model as metrics.

use super::*;

fn persist_opts() -> Options {
    Options {
        target: Some(DialectType::DuckDB),
        ..Default::default()
    }
}

fn sorted_rows(sql: &str) -> Vec<Vec<String>> {
    let mut rows = duckdb_rows(&duckdb(), sql);
    rows.sort();
    rows
}

fn view_rows(g: &ossiec::SemanticGraph, catalog: &Layer2Catalog, sql: &str) -> Vec<Vec<String>> {
    sorted_rows(
        &ossiec::compile_layer2_with(g, catalog, sql, &persist_opts())
            .unwrap()
            .sql,
    )
}

fn persisted_rows(g: &ossiec::SemanticGraph, request: &str) -> Vec<Vec<String>> {
    let request = serde_json::from_str(request).unwrap();
    sorted_rows(&ossiec::compile(g, &request, &persist_opts()).unwrap().sql)
}

fn persist_error(model: &str, view: &str) -> Error {
    let g = ossiec::from_yaml(model).unwrap();
    let mut catalog = Layer2Catalog::new();
    let opts = persist_opts();
    let name = ossiec::define_layer2_view(&g, &mut catalog, view, &opts).unwrap();
    ossiec::persist_layer2_view(&g, &catalog, &name, &opts)
        .err()
        .unwrap_or_else(|| panic!("persisted: {view}"))
}

/// `orders` without a primary key whose first field, `customer_id`, holds NULLs
/// (guest checkouts): `COUNT(customer_id)` would undercount `COUNT(*)`.
const ORDERS_WITHOUT_PRIMARY_KEY: &str = r#"
version: "0.2.0.dev0"
name: no_primary_key
datasets:
  - name: orders
    source: orders
    fields:
      - { name: customer_id, expression: { sql: customer_id }, datatype: Integer }
      - { name: status, expression: { sql: status }, datatype: String, dimension: {} }
    metrics:
      - name: order_count
        expression: { sql: COUNT(*) }
"#;

#[test]
fn persist_view_ratio_becomes_model_metric() {
    let (g, catalog) = define_region_metrics();
    let (g2, added) =
        ossiec::persist_layer2_view(&g, &catalog, "region_metrics", &two_facts_opts()).unwrap();
    assert_eq!(added, vec!["sales_per_ticket"]);
    assert!(g2
        .model()
        .metrics
        .iter()
        .any(|m| m.name == "sales_per_ticket"));
    // Layer 3 request uses the new metric; answers match the Layer 2 view path.
    let l3 = ossiec::compile(
        &g2,
        &serde_json::from_str(
            r#"{"metrics":["sales_per_ticket"],"dimensions":["customers.region"]}"#,
        )
        .unwrap(),
        &two_facts_opts(),
    )
    .unwrap();
    assert_region_ratio(&l3.sql, false);
    // Round-trip YAML still binds.
    let yaml = ossiec::to_yaml(g2.model()).unwrap();
    let g3 = ossiec::from_yaml(&yaml).unwrap();
    assert!(g3
        .describe()
        .metrics
        .iter()
        .any(|m| m.name == "sales_per_ticket" && m.queryable));
}

#[test]
fn persist_view_base_to_measure_becomes_dataset_metric() {
    let g = graph();
    let opts = Options {
        target: Some(DialectType::DuckDB),
        ..Default::default()
    };
    let mut catalog = Layer2Catalog::new();
    ossiec::define_layer2_view(
        &g,
        &mut catalog,
        "CREATE VIEW status_counts AS \
         SELECT o.status, COUNT(*) TO MEASURE AS status_n \
         FROM orders o",
        &opts,
    )
    .unwrap();
    let (g2, added) = ossiec::persist_layer2_view(&g, &catalog, "status_counts", &opts).unwrap();
    assert_eq!(added, vec!["orders.status_n"]);
    assert!(g2
        .model()
        .datasets
        .iter()
        .any(|d| { d.name == "orders" && d.metrics.iter().any(|m| m.name == "status_n") }));
    assert_eq!(
        persisted_rows(
            &g2,
            r#"{"metrics":["orders.status_n"],"dimensions":["orders.status"]}"#
        ),
        view_rows(
            &g,
            &catalog,
            "SELECT status, AGG(status_n) FROM status_counts GROUP BY status"
        ),
    );
}

/// The model's own path from a fact to a dimension is a LEFT join, so this view
/// is the one join shape that keeps its answers once persisted.
#[test]
fn persist_view_left_join_matches_the_view() {
    let g = graph();
    let mut catalog = Layer2Catalog::new();
    ossiec::define_layer2_view(
        &g,
        &mut catalog,
        "CREATE VIEW v AS SELECT c.region, (AGG(o.order_count) * 2) TO MEASURE AS doubled \
         FROM orders o LEFT JOIN customers c ON o.customer_id = c.id",
        &persist_opts(),
    )
    .unwrap();
    let (g2, _) = ossiec::persist_layer2_view(&g, &catalog, "v", &persist_opts()).unwrap();
    assert_eq!(
        persisted_rows(
            &g2,
            r#"{"metrics":["doubled"],"dimensions":["customers.region"]}"#
        ),
        view_rows(
            &g,
            &catalog,
            "SELECT region, AGG(doubled) FROM v GROUP BY region"
        ),
    );
}

/// Each view narrows or reshapes its rows in a way a metric cannot carry.
/// Persisting would silently change its answers, so it must be refused.
#[test]
fn persist_view_refuses_what_a_metric_cannot_carry() {
    let ratio = "(AGG(o.order_count) * 1) TO MEASURE AS r";
    for (clause, view) in [
        (
            "WHERE",
            "CREATE VIEW v AS SELECT o.status, COUNT(*) TO MEASURE AS n \
             FROM orders o WHERE o.status = 'completed'"
                .to_string(),
        ),
        (
            "WHERE",
            format!(
                "CREATE VIEW v AS SELECT o.status, {ratio} \
                 FROM orders o WHERE o.status = 'completed'"
            ),
        ),
        (
            "LIMIT",
            "CREATE VIEW v AS SELECT o.status, COUNT(*) TO MEASURE AS n \
             FROM orders o ORDER BY o.status LIMIT 1"
                .to_string(),
        ),
        (
            "other than LEFT",
            format!(
                "CREATE VIEW v AS SELECT c.region, {ratio} \
                 FROM orders o JOIN customers c ON o.customer_id = c.id"
            ),
        ),
        (
            "other than LEFT",
            format!(
                "CREATE VIEW v AS SELECT c.region, {ratio} \
                 FROM orders o RIGHT JOIN customers c ON o.customer_id = c.id"
            ),
        ),
        (
            "other than LEFT",
            format!(
                "CREATE VIEW v AS SELECT c.region, {ratio} \
                 FROM orders o FULL JOIN customers c ON o.customer_id = c.id"
            ),
        ),
    ] {
        let e = persist_error(SHOP, &view);
        assert!(
            matches!(e, Error::Unsupported(ref m) if m.contains(clause) && m.contains("would not carry")),
            "{view}: {e}"
        );
    }
}

/// A filter inside a nested relation must not slip past the check on the outer
/// `SELECT`, whether it comes from a referenced view or an inline derived table.
#[test]
fn persist_view_refuses_a_filter_in_a_nested_relation() {
    let g = graph();
    let opts = persist_opts();
    let mut catalog = Layer2Catalog::new();
    ossiec::define_layer2_view(
        &g,
        &mut catalog,
        "CREATE VIEW a AS SELECT status, order_count FROM orders \
         WHERE status = 'completed'",
        &opts,
    )
    .unwrap();
    ossiec::define_layer2_view(
        &g,
        &mut catalog,
        "CREATE VIEW over_view AS SELECT o.status, (AGG(a.order_count) * 2) TO MEASURE AS d \
         FROM orders o LEFT JOIN a ON o.status = a.status",
        &opts,
    )
    .unwrap();
    ossiec::define_layer2_view(
        &g,
        &mut catalog,
        "CREATE VIEW over_derived AS SELECT o.status, (AGG(t.order_count) * 2) TO MEASURE AS d \
         FROM orders o LEFT JOIN (SELECT status, order_count FROM orders \
         WHERE status = 'completed') t ON o.status = t.status",
        &opts,
    )
    .unwrap();
    for view in ["over_view", "over_derived"] {
        let e = ossiec::persist_layer2_view(&g, &catalog, view, &opts)
            .err()
            .unwrap_or_else(|| panic!("persisted {view}"));
        assert!(
            matches!(e, Error::Unsupported(ref m) if m.contains("a WHERE filter")),
            "{view}: {e}"
        );
    }
}

/// `products LEFT JOIN orders` keeps product 13, which never sold, as a group
/// with no orders (`COUNT(*)` on the nullable side skips the NULL-extended row),
/// and drops the order with no product. The persisted metric is queried over the
/// model's `orders -> products` path, which does the opposite: no product 13, and
/// that order under a NULL product. The products both return agree.
#[test]
fn persist_view_left_join_from_the_one_side_agrees_on_shared_groups() {
    let g = graph();
    let mut catalog = Layer2Catalog::new();
    ossiec::define_layer2_view(
        &g,
        &mut catalog,
        "CREATE VIEW v AS SELECT p.id, (AGG(o.order_count) * 1) TO MEASURE AS d \
         FROM products p LEFT JOIN orders o ON o.product_id = p.id",
        &persist_opts(),
    )
    .unwrap();
    let view = view_rows(&g, &catalog, "SELECT id, AGG(d) FROM v GROUP BY id");
    let want = sorted_rows(
        "SELECT p.product_id, COUNT(o.order_id) FROM products p \
         LEFT JOIN orders o ON o.product_id = p.product_id GROUP BY p.product_id",
    );
    assert_eq!(view, want);
    let unsold = vec!["13.000000".to_string(), "0.000000".to_string()];
    assert!(view.contains(&unsold), "{view:?}");
    assert!(view.iter().all(|r| r[0] != "NULL"), "{view:?}");

    let (g2, _) = ossiec::persist_layer2_view(&g, &catalog, "v", &persist_opts()).unwrap();
    let persisted = persisted_rows(&g2, r#"{"metrics":["d"],"dimensions":["products.id"]}"#);
    assert!(persisted.iter().any(|r| r[0] == "NULL"), "{persisted:?}");
    let shared = |rows: &[Vec<String>], other: &[Vec<String>]| -> Vec<Vec<String>> {
        rows.iter()
            .filter(|r| other.iter().any(|o| o[0] == r[0]))
            .cloned()
            .collect()
    };
    let in_both = shared(&view, &persisted);
    assert_eq!(in_both.len(), 3, "{view:?} vs {persisted:?}");
    assert_eq!(in_both, shared(&persisted, &view));
}

#[test]
fn persist_view_composing_count_star_needs_a_primary_key() {
    let e = persist_error(
        ORDERS_WITHOUT_PRIMARY_KEY,
        "CREATE VIEW v AS SELECT status, (AGG(order_count) * 1) TO MEASURE AS c \
         FROM orders",
    );
    assert!(
        matches!(e, Error::Unsupported(ref m) if m.contains("no primary key")),
        "{e}"
    );
    // A field that happens to be called `id` is not a primary key either.
    let e = persist_error(
        &SHOP.replacen("    primary_key: [id]\n", "", 1),
        "CREATE VIEW v AS SELECT status, (AGG(order_count) * 1) TO MEASURE AS c \
         FROM orders",
    );
    assert!(
        matches!(e, Error::Unsupported(ref m) if m.contains("no primary key")),
        "{e}"
    );
}

/// The refusal above tells the user to persist at dataset scope: that must work
/// and must keep the rows with a NULL first field.
#[test]
fn persist_view_count_star_without_a_primary_key_works_at_dataset_scope() {
    let g = ossiec::from_yaml(ORDERS_WITHOUT_PRIMARY_KEY).unwrap();
    let mut catalog = Layer2Catalog::new();
    ossiec::define_layer2_view(
        &g,
        &mut catalog,
        "CREATE VIEW v AS SELECT status, COUNT(*) TO MEASURE AS n FROM orders",
        &persist_opts(),
    )
    .unwrap();
    let (g2, added) = ossiec::persist_layer2_view(&g, &catalog, "v", &persist_opts()).unwrap();
    assert_eq!(added, vec!["orders.n"]);
    assert_eq!(
        persisted_rows(
            &g2,
            r#"{"metrics":["orders.n"],"dimensions":["orders.status"]}"#
        ),
        view_rows(&g, &catalog, "SELECT status, AGG(n) FROM v GROUP BY status"),
    );
}

#[test]
fn persist_view_refuses_a_leaf_without_portable_sql() {
    let dialect_only = SHOP.replacen(
        "expression: { sql: COUNT(*) }\n        description: Number of orders",
        "expression:\n          dialects:\n            - { dialect: DUCKDB, expression: \"COUNT(*)\" }",
        1,
    );
    assert_ne!(dialect_only, SHOP, "fixture no longer has this metric");
    let e = persist_error(
        &dialect_only,
        "CREATE VIEW v AS SELECT status, (AGG(order_count) * 1) TO MEASURE AS c \
         FROM orders",
    );
    assert!(
        matches!(e, Error::Unsupported(ref m) if m.contains("no portable `sql` expression")),
        "{e}"
    );
}

#[test]
fn persist_view_refusals() {
    let (g, catalog) = define_region_metrics();
    let opts = two_facts_opts();
    let e = ossiec::persist_layer2_view(&g, &catalog, "missing", &opts)
        .err()
        .expect("missing view");
    assert!(
        matches!(e, Error::Request(ref m) if m.contains("not defined")),
        "{e}"
    );
    let (g2, _) = ossiec::persist_layer2_view(&g, &catalog, "region_metrics", &opts).unwrap();
    let e = ossiec::persist_layer2_view(&g2, &catalog, "region_metrics", &opts)
        .err()
        .expect("duplicate metric");
    assert!(
        matches!(e, Error::Request(ref m) if m.contains("already exists")),
        "{e}"
    );
    let mut cat = Layer2Catalog::new();
    ossiec::define_layer2_view(
        &g,
        &mut cat,
        "CREATE VIEW eu_ids AS SELECT c.region FROM customers c WHERE c.region = 'EU'",
        &opts,
    )
    .unwrap();
    let e = ossiec::persist_layer2_view(&g, &cat, "eu_ids", &opts)
        .err()
        .expect("no TO MEASURE");
    assert!(
        matches!(e, Error::Request(ref m) if m.contains("no TO MEASURE")),
        "{e}"
    );
}
