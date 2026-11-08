//! SQL frontend: Ossie Layer 2 `AGG(measure)` surface (Appendix A.1–A.6, §7.6,
//! role-playing dimensions, includeUnmatched, A.7 CREATE VIEW).

mod common;

use common::{
    duckdb, duckdb_rows, A2_SHOP, A2_SHOP_REF, A3_SHOP, A3_SHOP_REF, LAYER2_NESTED_CASES,
};
use ossiec::{DialectType, Error, Layer2Catalog, Layer2ColumnKind, Options};

const SHOP: &str = include_str!("fixtures/shop/model.yaml");
const TWO_FACTS: &str = include_str!("fixtures/two_facts_layer2.yaml");
const FANOUT: &str = include_str!("fixtures/fanout_layer2.yaml");

fn graph() -> ossiec::SemanticGraph {
    ossiec::from_yaml(SHOP).unwrap()
}

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

/// Shop answers for §7.6. `COUNT(*)` would count null-extended or crossed rows;
/// the reference counts order ids so a preserved customer with no order adds nothing.
fn assert_join_answer(sql: &str, keyword: &str, reference: &str, naive: &str) {
    let c = ossiec::compile_layer2(&graph(), sql, &duck()).unwrap();
    assert!(
        c.sql.to_uppercase().contains(keyword),
        "expected {keyword} in {}",
        c.sql
    );
    let db = duckdb();
    let got = sorted(duckdb_rows(&db, &c.sql));
    let want = sorted(duckdb_rows(&db, reference));
    let inflated = sorted(duckdb_rows(&db, naive));
    assert_ne!(
        inflated, want,
        "reference must differ from a join that counts null-extended rows"
    );
    assert_eq!(got, want, "sql:\n{}", c.sql);
}

#[test]
fn right_join_keeps_customers_without_inflating_orders() {
    let sql = "SELECT c.id, AGG(o.order_count) AS n \
        FROM orders o RIGHT JOIN customers c ON o.customer_id = c.id \
        GROUP BY c.id";
    let reference = "SELECT c.customer_id, COUNT(o.order_id) \
        FROM orders o RIGHT JOIN {customers} c ON o.customer_id = c.customer_id \
        GROUP BY c.customer_id";
    let naive = "SELECT c.customer_id, COUNT(*) \
        FROM orders o RIGHT JOIN {customers} c ON o.customer_id = c.customer_id \
        GROUP BY c.customer_id";
    assert_join_answer(sql, "RIGHT JOIN", reference, naive);
}

#[test]
fn full_join_keeps_both_sides_without_inflating_orders() {
    let sql = "SELECT c.id, AGG(o.order_count) AS n \
        FROM orders o FULL JOIN customers c ON o.customer_id = c.id \
        GROUP BY c.id";
    let reference = "SELECT c.customer_id, COUNT(o.order_id) \
        FROM orders o FULL JOIN {customers} c ON o.customer_id = c.customer_id \
        GROUP BY c.customer_id";
    let naive = "SELECT c.customer_id, COUNT(*) \
        FROM orders o FULL JOIN {customers} c ON o.customer_id = c.customer_id \
        GROUP BY c.customer_id";
    assert_join_answer(sql, "FULL JOIN", reference, naive);
}

#[test]
fn cross_join_does_not_multiply_the_measure() {
    let sql = "SELECT AGG(o.order_count) AS n FROM orders o CROSS JOIN customers c";
    let c = ossiec::compile_layer2(&graph(), sql, &duck()).unwrap();
    assert!(c.sql.to_uppercase().contains("CROSS JOIN"), "{}", c.sql);
    let db = duckdb();
    let got = duckdb_rows(&db, &c.sql);
    let want = duckdb_rows(&db, "SELECT COUNT(*) FROM orders");
    let inflated = duckdb_rows(&db, "SELECT COUNT(*) FROM orders CROSS JOIN {customers}");
    assert_ne!(inflated, want);
    assert_eq!(got, want, "sql:\n{}", c.sql);
}

#[test]
fn cross_join_grouped_by_the_other_side_counts_each_order_once_per_group() {
    let sql = "SELECT c.region, AGG(o.order_count) AS n \
        FROM orders o CROSS JOIN customers c GROUP BY c.region";
    let reference = "SELECT region, (SELECT COUNT(*) FROM orders) \
        FROM {customers} GROUP BY region";
    let naive = "SELECT c.region, COUNT(*) \
        FROM orders o CROSS JOIN {customers} c GROUP BY c.region";
    assert_join_answer(sql, "CROSS JOIN", reference, naive);
}

#[test]
fn cross_join_with_on_is_refused() {
    let m = unsupported_msg(
        ossiec::compile_layer2(
            &graph(),
            "SELECT AGG(o.order_count) FROM orders o CROSS JOIN customers c ON o.customer_id = c.id",
            &duck(),
        )
        .unwrap_err(),
    );
    assert!(m.contains("CROSS JOIN") && m.contains("ON"), "{m}");
}

#[test]
fn derived_cross_join_does_not_multiply_the_measure() {
    let sql = "SELECT AGG(o.order_count) AS n FROM orders o \
        CROSS JOIN (SELECT region FROM customers GROUP BY region) t";
    let c = ossiec::compile_layer2(&graph(), sql, &duck()).unwrap();
    assert!(c.sql.to_uppercase().contains("CROSS JOIN"), "{}", c.sql);
    let db = duckdb();
    assert_eq!(
        duckdb_rows(&db, &c.sql),
        duckdb_rows(&db, "SELECT COUNT(*) FROM orders"),
        "sql:\n{}",
        c.sql
    );
}

#[test]
fn shell_right_and_cross_join_match_plain_sql() {
    let db = duckdb();
    let right = "SELECT d.status, COUNT(*) AS n \
        FROM (SELECT status, customer_id FROM orders GROUP BY status, customer_id) d \
        RIGHT JOIN (SELECT id FROM customers GROUP BY id) m ON d.customer_id = m.id \
        GROUP BY d.status";
    let c = ossiec::compile_layer2(&graph(), right, &duck()).unwrap();
    assert!(c.sql.to_uppercase().contains("RIGHT JOIN"), "{}", c.sql);
    let right_ref = "SELECT d.status, COUNT(*) AS n \
        FROM (SELECT status, customer_id FROM orders GROUP BY status, customer_id) d \
        RIGHT JOIN (SELECT customer_id AS id FROM {customers} GROUP BY customer_id) m \
          ON d.customer_id = m.id \
        GROUP BY d.status";
    assert_eq!(
        sorted(duckdb_rows(&db, &c.sql)),
        sorted(duckdb_rows(&db, right_ref))
    );

    let cross = "SELECT COUNT(*) AS n \
        FROM (SELECT status FROM orders GROUP BY status) d \
        CROSS JOIN (SELECT region FROM customers GROUP BY region) c";
    let c = ossiec::compile_layer2(&graph(), cross, &duck()).unwrap();
    assert!(c.sql.to_uppercase().contains("CROSS JOIN"), "{}", c.sql);
    let cross_ref = "SELECT COUNT(*) FROM (SELECT status FROM orders GROUP BY status) d \
        CROSS JOIN (SELECT region FROM {customers} GROUP BY region) c";
    assert_eq!(duckdb_rows(&db, &c.sql), duckdb_rows(&db, cross_ref));
}

#[test]
fn exotic_join_kind_is_still_refused() {
    let m = unsupported_msg(
        ossiec::compile_layer2(
            &graph(),
            "SELECT AGG(o.order_count) FROM orders o NATURAL JOIN customers c",
            &duck(),
        )
        .unwrap_err(),
    );
    assert!(m.contains("not supported") && m.contains("INNER"), "{m}");
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

/// `customers LEFT JOIN orders LEFT JOIN tickets` keeps every customer, so
/// customer 5 (no region, nothing bought) is a NULL-region group with no sales
/// and no tickets. Layer 3 roots at each fact and has no such group; the rows
/// both return carry the same values (§10.2).
#[test]
fn a4_independent_measures_via_joins_agree_with_layer3() {
    let g = two_facts_graph();
    let opts = two_facts_opts();
    let sql = "SELECT c.region, AGG(o.order_amt), AGG(t.ticket_count) \
               FROM customers c \
               LEFT JOIN orders o ON o.customer_id = c.id \
               LEFT JOIN tickets t ON t.customer_id = c.id \
               GROUP BY c.region";
    let l2 = ossiec::compile_layer2(&g, sql, &opts).unwrap();
    assert!(l2.sql.contains("UNION ALL"), "{}", l2.sql);
    let db = two_facts_db();
    let got = sorted(duckdb_rows(&db, &l2.sql));
    let want = sorted(duckdb_rows(
        &db,
        "SELECT c.region, o.s, COALESCE(t.n, 0) \
         FROM (SELECT DISTINCT region FROM customers) c \
         LEFT JOIN (SELECT x.region, SUM(o.amount) AS s FROM orders o \
                    JOIN customers x ON o.customer_id = x.id GROUP BY x.region) o \
           ON o.region IS NOT DISTINCT FROM c.region \
         LEFT JOIN (SELECT x.region, COUNT(*) AS n FROM tickets t \
                    JOIN customers x ON t.customer_id = x.id GROUP BY x.region) t \
           ON t.region IS NOT DISTINCT FROM c.region",
    ));
    assert_eq!(got, want, "{}", l2.sql);
    assert!(got.iter().any(|r| r[0] == "NULL"), "{got:?}");
    let l3 = ossiec::compile(
        &g,
        &serde_json::from_str(
            r#"{"metrics":["orders.order_amt","tickets.ticket_count"],"dimensions":["customers.region"]}"#,
        )
        .unwrap(),
        &opts,
    )
    .unwrap();
    let l3_rows = sorted(duckdb_rows(&db, &l3.sql));
    let shared: Vec<_> = got.into_iter().filter(|r| r[0] != "NULL").collect();
    assert_eq!(l3_rows, shared);
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
fn a4_agg_ratio_expression_in_select() {
    let g = two_facts_graph();
    let opts = two_facts_opts();
    let sql = "SELECT c.region, AGG(o.order_amt) / AGG(t.ticket_count) AS sales_per_ticket \
               FROM customers c \
               LEFT JOIN orders o ON o.customer_id = c.id \
               LEFT JOIN tickets t ON t.customer_id = c.id \
               GROUP BY c.region";
    let l2 = ossiec::compile_layer2(&g, sql, &opts).unwrap();
    assert!(
        l2.sql.contains('/') || l2.sql.to_uppercase().contains("NULLIF"),
        "expected division in SQL: {}",
        l2.sql
    );
    let req = ossiec::layer2_to_request(&g, sql).unwrap();
    assert_eq!(
        req.metrics,
        vec!["orders.order_amt", "tickets.ticket_count"]
    );
}

#[test]
fn a4_agg_ratio_on_single_fact() {
    let g = graph();
    let opts = Options {
        target: Some(DialectType::DuckDB),
        ..Default::default()
    };
    let sql =
        "SELECT status, AGG(order_count) / AGG(order_count) AS ones FROM orders GROUP BY status";
    let l2 = ossiec::compile_layer2(&g, sql, &opts).unwrap();
    assert!(l2.sql.to_uppercase().contains("COUNT"), "{}", l2.sql);
    assert!(
        l2.sql.contains("ones") || l2.sql.contains("\"ones\""),
        "result should expose alias ones: {}",
        l2.sql
    );
}

#[test]
fn a5_unused_inner_join_customers_compiles() {
    let g = graph();
    let opts = Options {
        target: Some(DialectType::DuckDB),
        ..Default::default()
    };
    let sql =
        "SELECT AGG(o.order_count) FROM orders o INNER JOIN customers c ON o.customer_id = c.id";
    let l2 = ossiec::compile_layer2(&g, sql, &opts).unwrap();
    let upper = l2.sql.to_uppercase();
    assert!(upper.contains("INNER JOIN"), "{}", l2.sql);
    assert!(upper.contains("CUSTOMERS"), "{}", l2.sql);
    // FROM uses the planned dataset alias; ON must match (not the consumer `o`).
    assert!(
        upper.contains("\"ORDERS\".CUSTOMER_ID") || upper.contains("ORDERS.CUSTOMER_ID"),
        "ON must qualify the attach side with the planned alias: {}",
        l2.sql
    );
    assert!(
        !upper.contains("\"O\".CUSTOMER_ID") && !upper.contains(" O.CUSTOMER_ID"),
        "ON must not use the consumer alias `o`: {}",
        l2.sql
    );
}

#[test]
fn a5_reverse_fanout_join_uses_pk_dedup() {
    let g = ossiec::from_yaml(FANOUT).unwrap();
    let opts = Options {
        target: Some(DialectType::DuckDB),
        ..Default::default()
    };
    let sql = "SELECT AGG(o.order_count) FROM orders o JOIN order_items i ON i.order_id = o.id";
    let l2 = ossiec::compile_layer2(&g, sql, &opts).unwrap();
    let upper = l2.sql.to_uppercase();
    assert!(
        upper.contains("ORDER_ITEMS"),
        "expected join to order_items: {}",
        l2.sql
    );
    assert!(
        upper.contains("DISTINCT"),
        "expected primary-key de-duplication for fan-out (G2): {}",
        l2.sql
    );
    assert!(
        !upper.contains("\"O\".") && !upper.contains(" O."),
        "ON/FROM must use planned aliases, not consumer `o`: {}",
        l2.sql
    );
}

/// `orders RIGHT JOIN order_items` keeps every line. The plan roots at the lines,
/// the many side, and counts orders as a one-side term de-duplicated by primary
/// key: two orders, not three lines, and no FILTER.
#[test]
fn a5_right_join_to_the_many_side_of_a_fanout_dedups() {
    let g = ossiec::from_yaml(FANOUT).unwrap();
    let db = duckdb::Connection::open_in_memory().unwrap();
    db.execute_batch(
        "CREATE TABLE orders (order_id INTEGER);
         INSERT INTO orders VALUES (1), (2), (3);
         CREATE TABLE order_items (line_id INTEGER, order_id INTEGER);
         INSERT INTO order_items VALUES (1, 1), (2, 1), (3, 2), (4, 9);",
    )
    .unwrap();
    let sql = "SELECT AGG(o.order_count) FROM orders o \
               RIGHT JOIN order_items i ON i.order_id = o.id";
    let c = ossiec::compile_layer2(&g, sql, &duck()).unwrap();
    assert!(!c.sql.contains("FILTER"), "{}", c.sql);
    assert_eq!(
        duckdb_rows(&db, &c.sql),
        duckdb_rows(
            &db,
            "SELECT COUNT(DISTINCT o.order_id) FROM order_items i \
             LEFT JOIN orders o ON i.order_id = o.order_id"
        )
    );
}

// ---------------------------------------------------------------- Phase 0

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

// ---------------------------------------------------------------- A.2

#[test]
fn a2_shop_form_executes_with_top_n_semantics() {
    let c = ossiec::compile_layer2(&graph(), A2_SHOP, &duck()).unwrap();
    assert!(!c.sql.contains("__ossiec_deriv"), "{}", c.sql);
    assert!(c.sql.to_uppercase().contains("INNER JOIN"), "{}", c.sql);
    let db = duckdb();
    let got = sorted(duckdb_rows(&db, &c.sql));
    let want = sorted(duckdb_rows(&db, A2_SHOP_REF));
    assert_eq!(got, want);
    // Customers 2 (three orders) and 1 (two; tie with 3 broken by id) only:
    // 4 completed + 1 returned. Not the unrestricted 8 / 1.
    assert_eq!(got.len(), 2, "{got:?}");
    let total: f64 = got.iter().map(|r| r[1].parse::<f64>().unwrap()).sum();
    assert_eq!(total, 5.0, "{got:?}");
}

#[test]
fn a2_left_derived_join_does_not_filter() {
    let sql = A2_SHOP.replace("JOIN (SELECT", "LEFT JOIN (SELECT");
    let c = ossiec::compile_layer2(&graph(), &sql, &duck()).unwrap();
    assert!(c.sql.to_uppercase().contains("LEFT JOIN"), "{}", c.sql);
    let db = duckdb();
    assert_eq!(
        sorted(duckdb_rows(&db, &c.sql)),
        sorted(duckdb_rows(
            &db,
            "SELECT status, COUNT(*) FROM orders GROUP BY status"
        ))
    );
}

#[test]
fn a2_on_both_columns_is_unique_and_accepted() {
    let sql = "SELECT o.status, AGG(o.order_count) FROM orders o \
        JOIN (SELECT status, customer_id, AGG(order_count) AS n FROM orders GROUP BY status, customer_id) t \
          ON o.customer_id = t.customer_id AND o.status = t.status \
        GROUP BY o.status";
    let c = ossiec::compile_layer2(&graph(), sql, &duck()).unwrap();
    let db = duckdb();
    assert_eq!(
        sorted(duckdb_rows(&db, &c.sql)),
        sorted(duckdb_rows(
            &db,
            "SELECT status, COUNT(*) FROM orders WHERE customer_id IS NOT NULL GROUP BY status"
        ))
    );
}

#[test]
fn a2_refuses_non_unique_derived_key_g2() {
    let sql = "SELECT o.status, AGG(o.order_count) FROM orders o \
        JOIN (SELECT status, customer_id, AGG(order_count) AS n FROM orders GROUP BY status, customer_id) t \
          ON o.customer_id = t.customer_id \
        GROUP BY o.status";
    let m = unsupported_msg(ossiec::compile_layer2(&graph(), sql, &duck()).unwrap_err());
    assert!(m.contains("G2") && m.contains("not unique"), "{m}");
}

#[test]
fn a2_unevaluated_measure_passes_through_inner_select() {
    // §7.7 in the A.2 join form: `order_count` stays a measure through the inner
    // select and the outer `AGG(t.order_count)` evaluates it.
    let sql = "SELECT o.status, AGG(t.order_count) \
        FROM orders o \
        JOIN (SELECT customer_id, order_count FROM orders) t \
          ON o.customer_id = t.customer_id \
        GROUP BY o.status";
    let c = ossiec::compile_layer2(&graph(), sql, &duck()).unwrap();
    assert!(!c.sql.contains("__ossiec"), "{}", c.sql);
    let db = duckdb();
    let got = sorted(duckdb_rows(&db, &c.sql));
    // The derived table is only a restriction (customers who ordered); the measure
    // is evaluated by the outer stage, per status: guest checkouts drop out.
    let want = sorted(duckdb_rows(
        &db,
        "SELECT status, COUNT(*) FROM orders WHERE customer_id IS NOT NULL GROUP BY status",
    ));
    assert_eq!(got, want);
    assert_eq!(got.len(), 2, "{got:?}");
    // Passing the measure through without reading it changes nothing.
    let unused = sql.replace("AGG(t.order_count)", "AGG(o.order_count)");
    let c = ossiec::compile_layer2(&graph(), &unused, &duck()).unwrap();
    assert_eq!(sorted(duckdb_rows(&db, &c.sql)), want);
}

#[test]
fn layer2_nested_cases_match_their_references_on_duckdb() {
    // The engine tier's shop cases, checked here without Docker.
    let db = duckdb();
    let g = graph();
    for case in LAYER2_NESTED_CASES {
        let c = if case.script {
            ossiec::compile_layer2_script(&g, case.sql, &duck())
        } else {
            ossiec::compile_layer2(&g, case.sql, &duck())
        }
        .unwrap_or_else(|e| panic!("{}: {e}", case.name));
        assert_eq!(
            sorted(duckdb_rows(&db, &c.sql)),
            sorted(duckdb_rows(&db, case.reference)),
            "{}: {}",
            case.name,
            c.sql
        );
    }
}

#[test]
fn a2_refuses_inner_agg_without_alias() {
    let sql = "SELECT o.status, AGG(o.order_count) FROM orders o \
        JOIN (SELECT customer_id, AGG(order_count) FROM orders GROUP BY customer_id) t \
          ON o.customer_id = t.customer_id \
        GROUP BY o.status";
    let m = unsupported_msg(ossiec::compile_layer2(&graph(), sql, &duck()).unwrap_err());
    assert!(m.contains("explicit alias"), "{m}");
}

#[test]
fn a2_refuses_duplicate_inner_output_names() {
    let sql = "SELECT o.status, AGG(o.order_count) FROM orders o \
        JOIN (SELECT customer_id, AGG(order_count) AS customer_id FROM orders GROUP BY customer_id) t \
          ON o.customer_id = t.customer_id \
        GROUP BY o.status";
    let e = ossiec::compile_layer2(&graph(), sql, &duck()).unwrap_err();
    assert!(
        matches!(e, Error::Request(ref m) if m.contains("two output columns named")),
        "{e}"
    );
}

#[test]
fn a2_refuses_correlated_inner_query() {
    let sql = "SELECT o.status, AGG(o.order_count) FROM orders o \
        JOIN (SELECT customer_id, AGG(order_count) AS n FROM orders \
              WHERE o.status = 'completed' GROUP BY customer_id) t \
          ON o.customer_id = t.customer_id \
        GROUP BY o.status";
    let m = unsupported_msg(ossiec::compile_layer2(&graph(), sql, &duck()).unwrap_err());
    assert!(m.contains("correlated"), "{m}");
}

#[test]
fn a2_refuses_derived_column_outside_on() {
    let sql = "SELECT t.n, AGG(o.order_count) FROM orders o \
        JOIN (SELECT customer_id, AGG(order_count) AS n FROM orders GROUP BY customer_id) t \
          ON o.customer_id = t.customer_id \
        GROUP BY t.n";
    let m = unsupported_msg(ossiec::compile_layer2(&graph(), sql, &duck()).unwrap_err());
    assert!(m.contains("only appear in") && m.contains("ON"), "{m}");
}

#[test]
fn a2_refuses_set_operator_inside_derived_table() {
    {
        let inner = "SELECT customer_id FROM orders UNION SELECT customer_id FROM orders";
        let sql = format!(
            "SELECT o.status, AGG(o.order_count) FROM orders o JOIN ({inner}) t ON o.customer_id = t.customer_id GROUP BY o.status"
        );
        let e = ossiec::compile_layer2(&graph(), &sql, &duck()).unwrap_err();
        assert!(matches!(e, Error::Unsupported(_)), "{inner}: {e}");
    }
}

#[test]
fn a2_derived_join_restricts_only_the_fact_that_hosts_it() {
    // Top two customers by order amount are 1 and 2 (both EU; US customer 3 is
    // out). The join attaches to `orders` only, so ticket counts stay complete
    // and neither measure is repeated by the other fact's rows.
    let g = two_facts_graph();
    let sql = "SELECT c.region, AGG(o.order_amt), AGG(t.ticket_count) \
               FROM customers c \
               LEFT JOIN orders o ON o.customer_id = c.id \
               LEFT JOIN tickets t ON t.customer_id = c.id \
               JOIN (SELECT customer_id, AGG(order_amt) AS n FROM orders \
                     GROUP BY customer_id ORDER BY n DESC, customer_id LIMIT 2) d \
                 ON o.customer_id = d.customer_id \
               GROUP BY c.region";
    let c = ossiec::compile_layer2(&g, sql, &two_facts_opts()).unwrap();
    let db = two_facts_db();
    let got = sorted(duckdb_rows(&db, &c.sql));
    let want = sorted(duckdb_rows(
        &db,
        "SELECT region, MAX(order_amt), COALESCE(MAX(ticket_count), 0) FROM (
            SELECT c.region AS region, SUM(o.amount) AS order_amt, NULL::BIGINT AS ticket_count
            FROM orders o
            LEFT JOIN customers c ON o.customer_id = c.id
            JOIN (SELECT customer_id FROM orders GROUP BY customer_id
                  ORDER BY SUM(amount) DESC, customer_id LIMIT 2) d
              ON o.customer_id = d.customer_id
            GROUP BY c.region
            UNION ALL
            SELECT c.region, NULL, COUNT(t.customer_id)
            FROM customers c
            LEFT JOIN tickets t ON t.customer_id = c.id
            GROUP BY c.region
         ) GROUP BY region",
    ));
    assert_eq!(got, want, "{}", c.sql);
    // EU 60 / 3 tickets; US and APAC keep their tickets and contribute no orders.
    // Customer 5 (no region) is kept by `customers LEFT JOIN tickets`: no tickets.
    let row = |region: &str, amt: &str, tickets: &str| -> Vec<String> {
        vec![region.into(), amt.into(), tickets.into()]
    };
    assert_eq!(
        got,
        vec![
            row("APAC", "NULL", "1.000000"),
            row("EU", "60.000000", "3.000000"),
            row("NULL", "NULL", "0.000000"),
            row("US", "NULL", "3.000000"),
        ]
    );
    // One tree would fan orders through tickets: customer 2's amount is doubled
    // (EU orders 90) and customer 1's ticket is doubled (EU tickets 4), and the
    // inner join drops US and APAC.
    let naive = sorted(duckdb_rows(
        &db,
        "SELECT c.region, SUM(o.amount), COUNT(t.id)
         FROM customers c
         LEFT JOIN orders o ON o.customer_id = c.id
         LEFT JOIN tickets t ON t.customer_id = c.id
         JOIN (SELECT customer_id FROM orders GROUP BY customer_id
               ORDER BY SUM(amount) DESC, customer_id LIMIT 2) d
           ON o.customer_id = d.customer_id
         GROUP BY c.region",
    ));
    assert_ne!(
        got, naive,
        "derived join must not inflate across facts: {naive:?}"
    );
}

#[test]
fn nested_queries_are_not_layer3_requests() {
    let m = unsupported_msg(ossiec::layer2_to_request(&graph(), A2_SHOP).unwrap_err());
    assert!(m.contains("compile_layer2"), "{m}");
}

#[test]
fn inner_subquery_alone_matches_the_one_embedded_in_the_outer() {
    let g = graph();
    let compact = Options {
        pretty: false,
        ..duck()
    };
    let inner = ossiec::compile_layer2(
        &g,
        "SELECT customer_id AS customer_id, AGG(order_count) AS n FROM orders \
         GROUP BY customer_id ORDER BY n DESC, customer_id LIMIT 2",
        &compact,
    )
    .unwrap();
    let outer = ossiec::compile_layer2(&g, A2_SHOP, &compact).unwrap();
    assert!(
        outer.sql.contains(&inner.sql),
        "inner:\n{}\nouter:\n{}",
        inner.sql,
        outer.sql
    );
}

// ---------------------------------------------------------------- A.2 / A.3 with Ossie names

const NESTED: &str = include_str!("fixtures/layer2_nested.yaml");

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

/// Appendix A.2 (`relational_semantics.md`): top-N self-join. Spec names the fact
/// `orders` and ranks on `total_revenue` while measuring `total_quantity`; this
/// fixture uses `sales` for the same shape.
const A2_APPENDIX: &str = "SELECT s.category, AGG(s.total_quantity) \
    FROM sales s \
    JOIN (SELECT state_id, AGG(total_revenue) AS r \
          FROM sales GROUP BY state_id \
          ORDER BY r DESC, state_id LIMIT 2) t \
      ON s.state_id = t.state_id \
    GROUP BY s.category";

#[test]
fn a2_appendix_top_n_self_join() {
    let g = nested_graph();
    let c = ossiec::compile_layer2(&g, A2_APPENDIX, &duck()).unwrap();
    let db = nested_db();
    let got = sorted(duckdb_rows(&db, &c.sql));
    // Top two states by revenue: 3 (101) then 2 (35).
    let want = sorted(duckdb_rows(
        &db,
        "SELECT category, SUM(qty) FROM sales WHERE state_id IN (3, 2) GROUP BY category",
    ));
    assert_eq!(got, want);
    assert_eq!(got.len(), 2);
}

#[test]
fn a2_pk_dedup_outer_measure_keeps_the_derived_restriction() {
    // `total_population` lives on `states`, one side of the join: planned as PkDedup,
    // so the restriction must also appear in the de-duplicating sub-select.
    let g = nested_graph();
    let sql = "SELECT s.category, AGG(st.total_population) AS pop \
        FROM sales s \
        JOIN states st ON s.state_id = st.id \
        JOIN (SELECT state_id, AGG(total_revenue) AS r FROM sales GROUP BY state_id \
              ORDER BY r DESC, state_id LIMIT 2) t ON s.state_id = t.state_id \
        GROUP BY s.category";
    let c = ossiec::compile_layer2(&g, sql, &duck()).unwrap();
    assert!(c.sql.to_uppercase().contains("DISTINCT"), "{}", c.sql);
    let db = nested_db();
    let got = sorted(duckdb_rows(&db, &c.sql));
    let want = sorted(duckdb_rows(
        &db,
        "SELECT category, SUM(population) FROM (SELECT DISTINCT s.category, st.id, st.population \
           FROM sales s JOIN states st ON s.state_id = st.id WHERE st.id IN (3, 2)) GROUP BY category",
    ));
    assert_eq!(got, want);
    // A: states 2 and 3 (500); B: state 3 (300).
    assert_eq!(got.len(), 2, "{got:?}");
}

#[test]
fn a2_null_safe_derived_key_keeps_null_group() {
    let g = nested_graph();
    let q = |op: &str| {
        format!(
            "SELECT s.category, AGG(s.total_revenue) FROM sales s \
             JOIN (SELECT state_id, AGG(total_revenue) AS r FROM sales GROUP BY state_id \
                   ORDER BY r DESC, state_id LIMIT 4) t ON s.state_id {op} t.state_id \
             GROUP BY s.category"
        )
    };
    let db = nested_db();
    // Top four by revenue: states 3, 2, 1 and the NULL state (7).
    let c = ossiec::compile_layer2(&g, &q("IS NOT DISTINCT FROM"), &duck()).unwrap();
    assert_eq!(
        sorted(duckdb_rows(&db, &c.sql)),
        sorted(duckdb_rows(
            &db,
            "SELECT category, SUM(amount) FROM sales \
             WHERE state_id IN (1, 2, 3) OR state_id IS NULL GROUP BY category"
        ))
    );
    let c = ossiec::compile_layer2(&g, &q("="), &duck()).unwrap();
    assert_eq!(
        sorted(duckdb_rows(&db, &c.sql)),
        sorted(duckdb_rows(
            &db,
            "SELECT category, SUM(amount) FROM sales WHERE state_id IN (1, 2, 3) GROUP BY category"
        ))
    );
}

/// Appendix A.3: average-of-averages over states (`sales` stands in for `orders`).
const A3_APPENDIX: &str = "SELECT d.category, AVG(m.state_revenue) \
    FROM (SELECT category, state_id FROM sales GROUP BY category, state_id) d \
    JOIN (SELECT state_id, AGG(total_revenue) AS state_revenue FROM sales GROUP BY state_id) m \
      ON d.state_id IS NOT DISTINCT FROM m.state_id \
    GROUP BY d.category";

#[test]
fn a3_appendix_two_stage_aggregation() {
    let g = nested_graph();
    let c = ossiec::compile_layer2(&g, A3_APPENDIX, &duck()).unwrap();
    let db = nested_db();
    let got = sorted(duckdb_rows(&db, &c.sql));
    // A: states 1, 2, 3 and NULL → (30 + 35 + 101 + 7) / 4; B: 1, 3, 4 → (30 + 101 + 2) / 3.
    let want = sorted(duckdb_rows(
        &db,
        "SELECT category, AVG(r) FROM (SELECT DISTINCT d.category, d.state_id, m.r FROM \
           (SELECT DISTINCT category, state_id FROM sales) d JOIN \
           (SELECT state_id, SUM(amount) AS r FROM sales GROUP BY state_id) m \
           ON d.state_id IS NOT DISTINCT FROM m.state_id) GROUP BY category",
    ));
    assert_eq!(got, want);
    let a = got.iter().find(|r| r[0] == "A").unwrap();
    assert_eq!(a[1].parse::<f64>().unwrap(), 43.25, "{got:?}");
}

#[test]
fn a3_plain_equality_drops_null_keys() {
    let g = nested_graph();
    let c = ossiec::compile_layer2(
        &g,
        &A3_APPENDIX.replace("IS NOT DISTINCT FROM", "="),
        &duck(),
    )
    .unwrap();
    let db = nested_db();
    let got = sorted(duckdb_rows(&db, &c.sql));
    let a = got.iter().find(|r| r[0] == "A").unwrap();
    assert!(
        (a[1].parse::<f64>().unwrap() - 166.0 / 3.0).abs() < 1e-5,
        "{got:?}"
    );
}

// ---------------------------------------------------------------- A.3, shop form

#[test]
fn a3_shop_form_executes() {
    let c = ossiec::compile_layer2(&graph(), A3_SHOP, &duck()).unwrap();
    assert!(c.sql.contains("IS NOT DISTINCT FROM"), "{}", c.sql);
    assert!(!c.sql.contains("__ossiec_deriv"), "{}", c.sql);
    let db = duckdb();
    let got = sorted(duckdb_rows(&db, &c.sql));
    assert_eq!(got, sorted(duckdb_rows(&db, A3_SHOP_REF)));
    assert_eq!(got.len(), 2, "{got:?}");
}

#[test]
fn a3_shell_where_having_order_limit_and_aliases() {
    let sql = "SELECT d.status, COUNT(*) AS k, MAX(m.n) AS biggest, SUM(m.n) \
        FROM (SELECT status, customer_id FROM orders GROUP BY status, customer_id) d \
        LEFT JOIN (SELECT customer_id, AGG(order_count) AS n FROM orders GROUP BY customer_id) m \
          ON d.customer_id = m.customer_id \
        WHERE m.n >= 1 \
        GROUP BY d.status \
        HAVING COUNT(*) >= 1 \
        ORDER BY k DESC, status \
        LIMIT 5";
    let c = ossiec::compile_layer2(&graph(), sql, &duck()).unwrap();
    let upper = c.sql.to_uppercase();
    assert!(
        upper.contains("HAVING") && upper.contains("LIMIT 5"),
        "{}",
        c.sql
    );
    assert!(c.sql.contains("\"sum_n\""), "default name: {}", c.sql);
    let db = duckdb();
    let want = duckdb_rows(
        &db,
        "SELECT d.status, COUNT(*), MAX(m.n), SUM(m.n) \
         FROM (SELECT status, customer_id FROM orders GROUP BY status, customer_id) d \
         LEFT JOIN (SELECT customer_id, COUNT(*) AS n FROM orders GROUP BY customer_id) m \
           ON d.customer_id = m.customer_id \
         WHERE m.n >= 1 GROUP BY d.status HAVING COUNT(*) >= 1 ORDER BY 2 DESC, 1 LIMIT 5",
    );
    assert_eq!(duckdb_rows(&db, &c.sql), want);
}

#[test]
fn a3_shell_without_group_by_is_one_row() {
    let sql = "SELECT AVG(m.n), COUNT(*) FROM \
        (SELECT customer_id, AGG(order_count) AS n FROM orders GROUP BY customer_id) m";
    let c = ossiec::compile_layer2(&graph(), sql, &duck()).unwrap();
    let db = duckdb();
    assert_eq!(
        duckdb_rows(&db, &c.sql),
        duckdb_rows(
            &db,
            "SELECT AVG(n), COUNT(*) FROM (SELECT COUNT(*) AS n FROM orders GROUP BY customer_id)"
        )
    );
}

#[test]
fn a3_shells_nest() {
    let sql = "SELECT AVG(a.top) AS avg_top FROM ( \
        SELECT d.status, MAX(m.n) AS top \
        FROM (SELECT status, customer_id FROM orders GROUP BY status, customer_id) d \
        JOIN (SELECT customer_id, AGG(order_count) AS n FROM orders GROUP BY customer_id) m \
          ON d.customer_id = m.customer_id \
        GROUP BY d.status) a";
    let c = ossiec::compile_layer2(&graph(), sql, &duck()).unwrap();
    let db = duckdb();
    assert_eq!(
        duckdb_rows(&db, &c.sql),
        duckdb_rows(
            &db,
            "SELECT AVG(top) FROM (SELECT d.status, MAX(m.n) AS top \
             FROM (SELECT status, customer_id FROM orders GROUP BY status, customer_id) d \
             JOIN (SELECT customer_id, COUNT(*) AS n FROM orders GROUP BY customer_id) m \
               ON d.customer_id = m.customer_id GROUP BY d.status)"
        )
    );
}

#[test]
fn a3_refuses_agg_over_an_evaluated_column() {
    let sql = "SELECT d.status, AGG(m.n) \
        FROM (SELECT status, customer_id FROM orders GROUP BY status, customer_id) d \
        JOIN (SELECT customer_id, AGG(order_count) AS n FROM orders GROUP BY customer_id) m \
          ON d.customer_id = m.customer_id GROUP BY d.status";
    let m = unsupported_msg(ossiec::compile_layer2(&graph(), sql, &duck()).unwrap_err());
    assert!(m.contains("already evaluated"), "{m}");
}

#[test]
fn a3_refuses_raw_dataset_next_to_derived_tables() {
    let sql = "SELECT d.status, COUNT(*) \
        FROM (SELECT status, customer_id FROM orders GROUP BY status, customer_id) d \
        JOIN orders o ON d.customer_id = o.customer_id GROUP BY d.status";
    let m = unsupported_msg(ossiec::compile_layer2(&graph(), sql, &duck()).unwrap_err());
    assert!(m.contains("raw model tables"), "{m}");
}

#[test]
fn a3_refuses_group_by_mismatch_and_unknown_columns() {
    let base = "FROM (SELECT status, customer_id FROM orders GROUP BY status, customer_id) d";
    let e = ossiec::compile_layer2(
        &graph(),
        &format!("SELECT d.status, COUNT(*) {base} GROUP BY d.customer_id"),
        &duck(),
    )
    .unwrap_err();
    assert!(
        matches!(e, Error::Request(ref m) if m.contains("GROUP BY must match")),
        "{e}"
    );
    let e = ossiec::compile_layer2(
        &graph(),
        &format!("SELECT d.nope, COUNT(*) {base} GROUP BY d.nope"),
        &duck(),
    )
    .unwrap_err();
    assert!(
        matches!(e, Error::Request(ref m) if m.contains("no column `nope`")),
        "{e}"
    );
}

#[test]
fn a3_refuses_correlated_reference_in_shell_subquery() {
    let sql = "SELECT d.status, COUNT(*) \
        FROM (SELECT status, customer_id FROM orders GROUP BY status, customer_id) d \
        JOIN (SELECT customer_id, AGG(order_count) AS n FROM orders WHERE d.status = 'x' GROUP BY customer_id) m \
          ON d.customer_id = m.customer_id GROUP BY d.status";
    let m = unsupported_msg(ossiec::compile_layer2(&graph(), sql, &duck()).unwrap_err());
    assert!(m.contains("correlated"), "{m}");
}

// ---------------------------------------------------------------- limits

fn agg_inner() -> String {
    "SELECT customer_id, AGG(order_count) AS n FROM orders GROUP BY customer_id".to_string()
}

fn shell_chain(levels: usize) -> String {
    let mut q = agg_inner();
    for i in 0..levels {
        q = format!("SELECT AVG(n) AS n FROM ({q}) a{i}");
    }
    q
}

#[test]
fn nesting_depth_is_limited_to_four() {
    let g = graph();
    ossiec::compile_layer2(&g, &shell_chain(4), &duck()).unwrap();
    let m = unsupported_msg(ossiec::compile_layer2(&g, &shell_chain(5), &duck()).unwrap_err());
    assert!(m.contains("at most 4 levels"), "{m}");
}

fn many_derived(n: usize) -> String {
    let mut sql = format!("SELECT COUNT(*) AS c FROM ({}) m0", agg_inner());
    for i in 1..n {
        sql.push_str(&format!(
            " JOIN ({}) m{i} ON m0.customer_id = m{i}.customer_id",
            agg_inner()
        ));
    }
    sql
}

#[test]
fn derived_tables_per_query_are_limited_to_eight() {
    let g = graph();
    ossiec::compile_layer2(&g, &many_derived(8), &duck()).unwrap();
    let m = unsupported_msg(ossiec::compile_layer2(&g, &many_derived(9), &duck()).unwrap_err());
    assert!(m.contains("at most 8 derived tables"), "{m}");
}

// ---------------------------------------------------------------- engines (compile only)

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

fn native_null_safe(t: DialectType) -> bool {
    matches!(
        t,
        DialectType::DuckDB
            | DialectType::PostgreSQL
            | DialectType::Trino
            | DialectType::Athena
            | DialectType::Databricks
    )
}

#[test]
fn nested_queries_compile_on_every_engine() {
    let g = graph();
    for target in ENGINES {
        let opts = Options {
            target: Some(target),
            ..Default::default()
        };
        for (name, q) in [("A.2", A2_SHOP), ("A.3", A3_SHOP)] {
            let c = ossiec::compile_layer2(&g, q, &opts)
                .unwrap_or_else(|e| panic!("{name} on {target:?}: {e}"));
            assert!(!c.sql.contains("__ossiec"), "{name} {target:?}: {}", c.sql);
            polyglot_sql::parse_one(&c.sql, target)
                .unwrap_or_else(|e| panic!("{name} {target:?} does not parse: {e}\n{}", c.sql));
            if name == "A.3" {
                let native = c.sql.to_uppercase().contains("IS NOT DISTINCT FROM");
                assert_eq!(
                    native,
                    native_null_safe(target),
                    "null-safe join on {target:?}:\n{}",
                    c.sql
                );
                if !native {
                    assert!(
                        c.sql.to_uppercase().contains("IS NULL"),
                        "{target:?}: {}",
                        c.sql
                    );
                    assert!(!c.sql.contains("<=>"), "{target:?}: {}", c.sql);
                }
            }
            if target == DialectType::ClickHouse {
                assert_eq!(c.sql.matches("join_use_nulls").count(), 1, "{}", c.sql);
            }
        }
    }
}

/// Phase 1a spike: lower the outer stage with a placeholder relation, adapt each
/// stage with its own plan, substitute, render once. The derived alias and its
/// quoting must survive on every engine family we adapt for.
#[test]
fn spike_placeholder_substitution_keeps_alias_and_quoting() {
    let g = graph();
    for (target, quoted_t) in [
        (DialectType::DuckDB, "\"t\""),
        (DialectType::PostgreSQL, "\"t\""),
        (DialectType::MySQL, "`t`"),
        (DialectType::ClickHouse, "\"t\""),
    ] {
        let c = ossiec::compile_layer2(
            &g,
            A2_SHOP,
            &Options {
                target: Some(target),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(!c.sql.contains("__ossiec_deriv"), "{target:?}: {}", c.sql);
        assert!(
            c.sql.contains(&format!("AS {quoted_t}")) && c.sql.contains(&format!("{quoted_t}.")),
            "{target:?} lost the derived alias: {}",
            c.sql
        );
        polyglot_sql::parse_one(&c.sql, target)
            .unwrap_or_else(|e| panic!("{target:?} does not parse: {e}\n{}", c.sql));
    }
    // And it runs.
    let c = ossiec::compile_layer2(&g, A2_SHOP, &duck()).unwrap();
    assert!(!duckdb_rows(&duckdb(), &c.sql).is_empty());
}

#[test]
fn each_stage_is_adapted_with_its_own_plan() {
    let g = graph();
    let pg = ossiec::compile_layer2(
        &g,
        A3_SHOP,
        &Options {
            target: Some(DialectType::PostgreSQL),
            ..Default::default()
        },
    )
    .unwrap();
    // The `d` stage's string dimension takes Postgres's byte collation (its plan's
    // `string_dims`), under the consumer-visible name `status`.
    assert!(pg.sql.contains("COLLATE \"C\""), "{}", pg.sql);
    let my = ossiec::compile_layer2(
        &g,
        A3_SHOP,
        &Options {
            target: Some(DialectType::MySQL),
            ..Default::default()
        },
    )
    .unwrap();
    assert!(my.sql.contains("COLLATE utf8mb4_bin"), "{}", my.sql);
    assert!(
        my.sql.to_uppercase().contains("CAST("),
        "outer AVG: {}",
        my.sql
    );
}

// ---------------------------------------------------------------- §7.7 / TO MEASURE

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

#[test]
fn s77_from_only_folds_filter_and_projection() {
    // The spec's §7.7 shape. On shop the model-scoped `total_revenue` is not a
    // measure, so the dataset-scoped `order_count` stands in.
    let sql = "SELECT status, AGG(order_count) AS n \
        FROM (SELECT status, order_count FROM orders WHERE customer_id IS NOT NULL) t \
        WHERE status = 'completed' \
        GROUP BY status";
    let c = ossiec::compile_layer2(&graph(), sql, &duck()).unwrap();
    assert!(!c.sql.contains("__ossiec"), "{}", c.sql);
    let db = duckdb();
    let got = duckdb_rows(&db, &c.sql);
    let want = duckdb_rows(
        &db,
        "SELECT status, COUNT(*) FROM orders WHERE customer_id IS NOT NULL AND status = 'completed' GROUP BY status",
    );
    assert_eq!(got, want);
    // The inner WHERE applies: the guest checkout (order 106) is gone.
    let all = duckdb_rows(
        &db,
        "SELECT status, COUNT(*) FROM orders WHERE status = 'completed' GROUP BY status",
    );
    assert_ne!(got, all);
    // Same plan as the query written without the subquery.
    let flat = ossiec::compile_layer2(
        &graph(),
        "SELECT status, AGG(order_count) AS n FROM orders \
         WHERE customer_id IS NOT NULL AND status = 'completed' GROUP BY status",
        &duck(),
    )
    .unwrap();
    assert_eq!(c.sql, flat.sql);
}

#[test]
fn s77_from_only_with_renames() {
    let sql = "SELECT s AS st, AGG(m) \
        FROM (SELECT status AS s, order_count AS m FROM orders) t \
        GROUP BY t.s ORDER BY st";
    let c = ossiec::compile_layer2(&graph(), sql, &duck()).unwrap();
    let db = duckdb();
    assert_eq!(
        duckdb_rows(&db, &c.sql),
        duckdb_rows(
            &db,
            "SELECT status, COUNT(*) FROM orders GROUP BY status ORDER BY status"
        )
    );
}

#[test]
fn s77_from_only_spec_example_on_dataset_scoped_total_revenue() {
    let g = nested_graph();
    let sql = "SELECT category, AGG(total_revenue) \
        FROM (SELECT category, total_revenue FROM sales WHERE qty > 1) t \
        GROUP BY category";
    let c = ossiec::compile_layer2(&g, sql, &duck()).unwrap();
    let db = nested_db();
    assert_eq!(
        sorted(duckdb_rows(&db, &c.sql)),
        sorted(duckdb_rows(
            &db,
            "SELECT category, SUM(amount) FROM sales WHERE qty > 1 GROUP BY category"
        ))
    );
}

#[test]
fn s77_from_only_as_a_derived_table_of_a_larger_query() {
    // A folded stage nested in an A.3 shell: aliases name its columns.
    let sql = "SELECT AVG(r.n) AS avg_n \
        FROM (SELECT status, AGG(order_count) AS n \
              FROM (SELECT status, order_count FROM orders WHERE customer_id IS NOT NULL) t \
              GROUP BY status) r";
    let c = ossiec::compile_layer2(&graph(), sql, &duck()).unwrap();
    let db = duckdb();
    assert_eq!(
        duckdb_rows(&db, &c.sql),
        duckdb_rows(
            &db,
            "SELECT AVG(n) FROM (SELECT status, COUNT(*) AS n FROM orders WHERE customer_id IS NOT NULL GROUP BY status)"
        )
    );
}

#[test]
fn to_measure_defers_a_ratio_to_the_outer_agg() {
    let g = two_facts_graph();
    let sql = "SELECT r.region, AGG(m.sales_per_ticket) AS spt \
        FROM customers r \
        JOIN ( \
          SELECT c.region, \
                 (AGG(o.order_amt) / AGG(t.ticket_count)) TO MEASURE AS sales_per_ticket \
          FROM customers c \
          LEFT JOIN orders o ON o.customer_id = c.id \
          LEFT JOIN tickets t ON t.customer_id = c.id \
        ) m ON r.region = m.region \
        GROUP BY r.region";
    let c = ossiec::compile_layer2(&g, sql, &two_facts_opts()).unwrap();
    assert!(!c.sql.contains("__ossiec"), "{}", c.sql);
    assert!(c.sql.contains("UNION ALL"), "{}", c.sql);
    let db = two_facts_db();
    let got = rows_f64(duckdb_rows(&db, &c.sql));
    let want = rows_f64(duckdb_rows(
        &db,
        "SELECT c.region, \
                (SELECT SUM(o.amount) FROM orders o JOIN customers x ON o.customer_id = x.id WHERE x.region = c.region) * 1.0 \
                / (SELECT COUNT(*) FROM tickets t JOIN customers x ON t.customer_id = x.id WHERE x.region = c.region) \
         FROM (SELECT DISTINCT region FROM customers WHERE region IS NOT NULL) c",
    ));
    // APAC has tickets but no orders: no sales, so its ratio is NULL (-1 here).
    assert_eq!(got.len(), 3, "{got:?}");
    assert_eq!(got.len(), want.len(), "{got:?} vs {want:?}");
    for (g, w) in got.iter().zip(&want) {
        assert_eq!(g.0, w.0);
        assert!((g.1 - w.1).abs() < 1e-9, "{got:?} vs {want:?}");
    }
    assert!((got[0].1 - -1.0).abs() < 1e-9, "APAC: {got:?}");
    assert!((got[1].1 - 20.0).abs() < 1e-9, "EU: {got:?}");
    assert!((got[2].1 - 5.0 / 3.0).abs() < 1e-5, "US: {got:?}");
}

#[test]
fn to_measure_outer_agg_is_restricted_by_the_derived_table() {
    // The derived table restricts `customers` (HAVING-like top-1 by tickets) in
    // every leg; the measure itself is re-evaluated by the outer stage.
    let g = two_facts_graph();
    let sql = "SELECT r.region, AGG(m.spt) AS spt \
        FROM customers r \
        JOIN (SELECT c.region, (AGG(o.order_amt) / AGG(t.ticket_count)) TO MEASURE AS spt \
              FROM customers c \
              LEFT JOIN orders o ON o.customer_id = c.id \
              LEFT JOIN tickets t ON t.customer_id = c.id \
              WHERE c.region = 'EU') m ON r.region = m.region \
        GROUP BY r.region";
    let c = ossiec::compile_layer2(&g, sql, &two_facts_opts()).unwrap();
    let got = rows_f64(duckdb_rows(&two_facts_db(), &c.sql));
    assert_eq!(got.len(), 1, "{got:?}");
    assert_eq!(got[0].0, "EU");
    assert!((got[0].1 - 20.0).abs() < 1e-9, "{got:?}");
}

#[test]
fn to_measure_in_a_sole_derived_from_folds() {
    let g = two_facts_graph();
    let sql = "SELECT region, AGG(m.sales_per_ticket) AS spt \
        FROM (SELECT c.region, \
                     (AGG(o.order_amt) / AGG(t.ticket_count)) TO MEASURE AS sales_per_ticket \
              FROM customers c \
              LEFT JOIN orders o ON o.customer_id = c.id \
              LEFT JOIN tickets t ON t.customer_id = c.id) m \
        WHERE m.region = 'EU' \
        GROUP BY region";
    let c = ossiec::compile_layer2(&g, sql, &two_facts_opts()).unwrap();
    let got = rows_f64(duckdb_rows(&two_facts_db(), &c.sql));
    assert_eq!(got.len(), 1, "{got:?}");
    assert!((got[0].1 - 20.0).abs() < 1e-9, "{got:?}");
}

#[test]
fn to_measure_inner_alone_is_a_stage_with_a_marker_free_identity() {
    // A derived table's TO MEASURE column is physically evaluated at its own grain.
    let g = two_facts_graph();
    let outer = ossiec::compile_layer2(
        &g,
        "SELECT r.region, AGG(m.x) AS x FROM customers r JOIN (SELECT c.region, \
         (AGG(o.order_amt) / AGG(t.ticket_count)) TO MEASURE AS x FROM customers c \
         LEFT JOIN orders o ON o.customer_id = c.id LEFT JOIN tickets t ON t.customer_id = c.id) m ON r.region = m.region GROUP BY r.region",
        &two_facts_opts(),
    )
    .unwrap();
    assert!(!outer.sql.contains("__l2_tm"), "{}", outer.sql);
    // Two queries that differ only in the TO MEASURE expression are different requests.
    let other = SALES_PER_TICKET.replace("AGG(o.order_amt) /", "AGG(o.order_amt) * 2 /");
    let wrap = |inner: &str| {
        format!(
            "SELECT r.region, AGG(m.sales_per_ticket) AS s FROM customers r \
             JOIN ({inner}) m ON r.region = m.region GROUP BY r.region"
        )
    };
    let a = ossiec::compile_layer2(&g, &wrap(SALES_PER_TICKET), &two_facts_opts()).unwrap();
    let b = ossiec::compile_layer2(&g, &wrap(&other), &two_facts_opts()).unwrap();
    assert_ne!(a.request_hash, b.request_hash);
}

#[test]
fn s77_refuses_top_level_bare_measure() {
    let m = unsupported_msg(
        ossiec::compile_layer2(
            &graph(),
            "SELECT status, order_count FROM orders GROUP BY status",
            &duck(),
        )
        .unwrap_err(),
    );
    assert!(m.contains("top level") && m.contains("AGG"), "{m}");
}

#[test]
fn s77_refuses_model_scoped_metric_pass_through() {
    // `total_revenue` is model-scoped on shop: not a measure (§10.3).
    let e = ossiec::compile_layer2(
        &graph(),
        "SELECT status, AGG(total_revenue) FROM (SELECT status, total_revenue FROM orders) t GROUP BY status",
        &duck(),
    )
    .unwrap_err();
    assert!(
        matches!(e, Error::Request(ref m) if m.contains("model-scoped")),
        "{e}"
    );
    let e = ossiec::compile_layer2(
        &graph(),
        "SELECT o.status, AGG(o.order_count) FROM orders o \
         JOIN (SELECT customer_id, total_revenue FROM orders GROUP BY customer_id) t \
           ON o.customer_id = t.customer_id GROUP BY o.status",
        &duck(),
    )
    .unwrap_err();
    assert!(
        matches!(e, Error::Request(ref m) if m.contains("model-scoped")),
        "{e}"
    );
}

#[test]
fn s77_refuses_agg_over_an_evaluated_derived_column() {
    for sql in [
        // Sole derived FROM (fold path).
        "SELECT status, AGG(n) FROM (SELECT status, AGG(order_count) AS n FROM orders GROUP BY status) t GROUP BY status",
        // A.2 join form.
        "SELECT o.status, AGG(t.n) FROM orders o \
         JOIN (SELECT customer_id, AGG(order_count) AS n FROM orders GROUP BY customer_id) t \
           ON o.customer_id = t.customer_id GROUP BY o.status",
    ] {
        let m = unsupported_msg(ossiec::compile_layer2(&graph(), sql, &duck()).unwrap_err());
        assert!(m.contains("already evaluated"), "{sql}: {m}");
    }
}

#[test]
fn s77_refuses_measure_in_group_by_and_plain_sql_over_it() {
    let e = ossiec::compile_layer2(
        &graph(),
        "SELECT status, order_count FROM (SELECT status, order_count FROM orders) t GROUP BY status, order_count",
        &duck(),
    )
    .unwrap_err();
    assert!(!matches!(e, Error::Internal(_)), "{e}");
    let e = ossiec::compile_layer2(
        &graph(),
        "SELECT o.status, AGG(o.order_count) FROM orders o \
         JOIN (SELECT customer_id, order_count FROM orders GROUP BY customer_id, order_count) t \
           ON o.customer_id = t.customer_id GROUP BY o.status",
        &duck(),
    )
    .unwrap_err();
    assert!(
        matches!(e, Error::Request(ref m) if m.contains("GROUP BY cannot name measure") || m.contains("§7.2")),
        "{e}"
    );
    // A.3 shell: plain SQL cannot read an unevaluated column.
    let m = unsupported_msg(
        ossiec::compile_layer2(
            &graph(),
            "SELECT AVG(m.order_count) FROM (SELECT customer_id, order_count FROM orders) m",
            &duck(),
        )
        .unwrap_err(),
    );
    assert!(m.contains("unevaluated measure"), "{m}");
}

#[test]
fn s77_refuses_inner_queries_that_cannot_fold() {
    for (inner, what) in [
        (
            "SELECT status, order_count FROM orders ORDER BY status LIMIT 1",
            "ORDER BY / LIMIT",
        ),
        (
            "SELECT DISTINCT status, order_count FROM orders",
            "DISTINCT",
        ),
    ] {
        let sql = format!("SELECT status, AGG(order_count) FROM ({inner}) t GROUP BY status");
        let m = unsupported_msg(ossiec::compile_layer2(&graph(), &sql, &duck()).unwrap_err());
        assert!(m.contains(what), "{inner}: {m}");
    }
}

fn base_measure_rows(sql: &str) -> Vec<(String, f64)> {
    let c = ossiec::compile_layer2(&two_facts_graph(), sql, &two_facts_opts()).unwrap();
    assert!(!c.sql.contains("__l2_tm"), "{}", c.sql);
    rows_f64(duckdb_rows(&two_facts_db(), &c.sql))
}

#[test]
fn to_measure_sum_defers_to_the_outer_agg() {
    // The form that used to be refused. The outer AGG evaluates SUM(amount) at
    // the orders grain: EU 60, US 5. APAC has no orders, so it is not a group.
    let sum = "SELECT r.region, AGG(m.total_revenue) AS total_revenue \
        FROM customers r \
        JOIN (SELECT c.region, SUM(o.amount) TO MEASURE AS total_revenue \
              FROM customers c LEFT JOIN orders o ON o.customer_id = c.id) m ON r.region = m.region \
        GROUP BY r.region";
    let paren = sum.replace("SUM(o.amount) TO MEASURE", "(SUM(o.amount)) TO MEASURE");
    let want = rows_f64(duckdb_rows(
        &two_facts_db(),
        "SELECT c.region, SUM(o.amount) \
         FROM orders o JOIN customers c ON o.customer_id = c.id \
         GROUP BY c.region",
    ));
    for (label, sql) in [("SUM", sum), ("(SUM)", paren.as_str())] {
        let got = base_measure_rows(sql);
        assert_eq!(got.len(), want.len(), "{label}: {got:?} vs {want:?}");
        for (g, w) in got.iter().zip(&want) {
            assert_eq!(g.0, w.0, "{label}");
            assert!((g.1 - w.1).abs() < 1e-9, "{label}: {got:?} vs {want:?}");
        }
    }
    assert_eq!(want.len(), 2, "{want:?}");
    assert!((want.iter().find(|r| r.0 == "EU").unwrap().1 - 60.0).abs() < 1e-9);
}

#[test]
fn to_measure_avg_is_not_the_average_of_inner_averages() {
    // Inner grain is the customer. EU's per-customer averages are 15 and 30;
    // the outer AGG must average the orders (20), not those averages (22.5).
    let sql = "SELECT r.region, AGG(m.a) AS a \
        FROM customers r \
        JOIN (SELECT c.id, AVG(o.amount) TO MEASURE AS a \
              FROM customers c LEFT JOIN orders o ON o.customer_id = c.id) m ON r.id = m.id \
        GROUP BY r.region";
    let got = base_measure_rows(sql);
    let eu = got.iter().find(|r| r.0 == "EU").unwrap().1;
    assert!((eu - 20.0).abs() < 1e-9, "{got:?}");
    assert!((eu - 22.5).abs() > 1.0, "pre-aggregated: {got:?}");
}

#[test]
fn to_measure_count_star_counts_the_dataset_grain() {
    let g = graph();
    let sql = "SELECT status, AGG(n) AS n \
        FROM (SELECT status, COUNT(*) TO MEASURE AS n FROM orders \
              WHERE customer_id IS NOT NULL) t \
        WHERE status = 'completed' \
        GROUP BY status";
    let c = ossiec::compile_layer2(&g, sql, &duck()).unwrap();
    let db = duckdb();
    assert_eq!(
        duckdb_rows(&db, &c.sql),
        duckdb_rows(
            &db,
            "SELECT status, COUNT(*) FROM orders \
             WHERE customer_id IS NOT NULL AND status = 'completed' GROUP BY status"
        )
    );
    let flat = ossiec::compile_layer2(
        &g,
        "SELECT status, AGG(order_count) AS n FROM orders \
         WHERE customer_id IS NOT NULL AND status = 'completed' GROUP BY status",
        &duck(),
    )
    .unwrap();
    assert_eq!(c.sql, flat.sql);
}

#[test]
fn to_measure_min_max_and_count_distinct() {
    let g = graph();
    let sql = "SELECT status, AGG(m.lo) AS lo, AGG(m.hi) AS hi, AGG(m.buyers) AS buyers \
        FROM (SELECT status, \
                     MIN(amount_cents) TO MEASURE AS lo, \
                     MAX(amount_cents) TO MEASURE AS hi, \
                     COUNT(DISTINCT customer_id) TO MEASURE AS buyers \
              FROM orders) m \
        GROUP BY status";
    let c = ossiec::compile_layer2(&g, sql, &duck()).unwrap();
    let db = duckdb();
    assert_eq!(
        sorted(duckdb_rows(&db, &c.sql)),
        sorted(duckdb_rows(
            &db,
            "SELECT status, MIN(amount_cents), MAX(amount_cents), COUNT(DISTINCT customer_id) \
             FROM orders GROUP BY status"
        ))
    );
}

#[test]
fn to_measure_refusals() {
    let g = two_facts_graph();
    let o = two_facts_opts();
    let from = "FROM customers c LEFT JOIN orders o ON o.customer_id = c.id GROUP BY c.region";
    let wrap = |item: &str| {
        format!(
            "SELECT r.region, AGG(m.x) FROM customers r JOIN (SELECT c.region, {item} {from}) m \
             ON r.region = m.region GROUP BY r.region"
        )
    };
    // Not a base aggregate, and not a parenthesized AGG() composition.
    for item in [
        "o.amount TO MEASURE AS x",
        "(o.amount) TO MEASURE AS x",
        "AGG(o.order_amt) / AGG(o.order_amt) TO MEASURE AS x",
        "(AGG(o.order_amt) + SUM(o.amount)) TO MEASURE AS x",
        "(SUM(o.amount) + SUM(o.amount)) TO MEASURE AS x",
        "COUNT(*) TO MEASURE AS x",
    ] {
        let m = unsupported_msg(ossiec::compile_layer2(&g, &wrap(item), &o).unwrap_err());
        assert!(
            m.contains("TO MEASURE") || m.contains("AGG") || m.contains("COUNT"),
            "{item}: {m}"
        );
    }
    // Missing name.
    let e = ossiec::compile_layer2(&g, &wrap("(AGG(o.order_amt)) TO MEASURE"), &o).unwrap_err();
    assert!(
        matches!(e, Error::Request(ref m) if m.contains("AS <name>")),
        "{e}"
    );
    // Top level: nothing outside could evaluate it.
    for sql in [
        "SELECT c.region, (AGG(o.order_amt)) TO MEASURE AS x FROM customers c \
         LEFT JOIN orders o ON o.customer_id = c.id",
        "SELECT o.amount, SUM(o.amount) TO MEASURE AS x FROM orders o",
    ] {
        let m = unsupported_msg(ossiec::compile_layer2(&g, sql, &o).unwrap_err());
        assert!(m.contains("TO MEASURE"), "{sql}: {m}");
    }
    // The text is only cut where it is code: a string literal is left alone.
    let e = ossiec::compile_layer2(
        &g,
        "SELECT c.region, AGG(o.order_amt) FROM customers c \
         LEFT JOIN orders o ON o.customer_id = c.id WHERE c.region = 'x TO MEASURE AS y' GROUP BY c.region",
        &o,
    );
    assert!(e.is_ok(), "{e:?}");
}

#[test]
fn s77_and_to_measure_compile_on_every_engine() {
    let g = graph();
    let tf = two_facts_graph();
    let a2 = "SELECT o.status, AGG(t.order_count) FROM orders o \
        JOIN (SELECT customer_id, order_count FROM orders) t \
          ON o.customer_id = t.customer_id GROUP BY o.status";
    let fold = "SELECT status, AGG(order_count) AS n \
        FROM (SELECT status, order_count FROM orders WHERE customer_id IS NOT NULL) t \
        GROUP BY status";
    let tm = format!(
        "SELECT r.region, AGG(m.sales_per_ticket) AS s FROM customers r \
         JOIN ({SALES_PER_TICKET}) m ON r.region = m.region GROUP BY r.region"
    );
    let base = "SELECT status, AGG(n) AS n \
        FROM (SELECT status, COUNT(*) TO MEASURE AS n FROM orders) t \
        GROUP BY status";
    for target in ENGINES {
        let opts = Options {
            target: Some(target),
            ..Default::default()
        };
        for (name, graph, q) in [
            ("A.2", &g, a2),
            ("fold", &g, fold),
            ("TO MEASURE", &tf, &tm),
            ("base TO MEASURE", &g, base),
        ] {
            let c = ossiec::compile_layer2(graph, q, &opts)
                .unwrap_or_else(|e| panic!("{name} on {target:?}: {e}"));
            assert!(
                !c.sql.contains("__ossiec") && !c.sql.contains("__l2_tm"),
                "{name} {target:?}: {}",
                c.sql
            );
            polyglot_sql::parse_one(&c.sql, target)
                .unwrap_or_else(|e| panic!("{name} {target:?} does not parse: {e}\n{}", c.sql));
        }
    }
}

// ---------------------------------------------------------------- Copilot review regressions (PR #6)

#[test]
fn fold_refuses_columns_not_exposed_by_the_derived_table() {
    for sql in [
        // `status` is not in the derived select list.
        "SELECT status, AGG(order_count) FROM (SELECT customer_id, order_count FROM orders) t GROUP BY status",
        // Hidden base relation must not leak through the fold.
        "SELECT orders.status, AGG(order_count) FROM (SELECT customer_id, order_count FROM orders) t GROUP BY orders.status",
    ] {
        let e = ossiec::compile_layer2(&graph(), sql, &duck()).unwrap_err();
        assert!(
            matches!(e, Error::Request(ref m) if m.contains("derived table")),
            "{sql}: {e}"
        );
    }
}

#[test]
fn fold_refuses_filter_on_agg_of_to_measure() {
    let g = two_facts_graph();
    let sql = "SELECT AGG(m.x) FILTER (WHERE true) FROM (
        SELECT (AGG(o.order_amt)) TO MEASURE AS x FROM orders o
      ) m";
    let m = unsupported_msg(ossiec::compile_layer2(&g, sql, &two_facts_opts()).unwrap_err());
    assert!(m.contains("FILTER"), "{m}");
}

#[test]
fn fold_preserves_unaliased_derived_column_labels() {
    // Outer query addresses the folded stage's column as `s`, not `status`.
    let sql = "SELECT r.s FROM (
        SELECT s, AGG(m) AS n FROM (
          SELECT status AS s, order_count AS m FROM orders
        ) t GROUP BY s
      ) r";
    let c = ossiec::compile_layer2(&graph(), sql, &duck()).unwrap();
    let db = duckdb();
    assert_eq!(
        sorted(duckdb_rows(&db, &c.sql)),
        sorted(duckdb_rows(
            &db,
            "SELECT status FROM orders GROUP BY status"
        ))
    );
}

#[test]
fn fold_refuses_inner_group_by_of_a_measure() {
    let e = ossiec::compile_layer2(
        &graph(),
        "SELECT status, AGG(order_count) FROM (
           SELECT status, order_count FROM orders GROUP BY status, order_count
         ) t GROUP BY status",
        &duck(),
    )
    .unwrap_err();
    // Grouping by a measure (R1) and carrying one out of a grouped query (§7.2)
    // are both refused.
    assert!(
        matches!(e, Error::Request(ref m) if m.contains("GROUP BY cannot name measure") || m.contains("§7.2")),
        "{e}"
    );
}

#[test]
fn folded_to_measure_formulas_participate_in_request_hash() {
    let g = graph();
    let wrap = |n: &str| {
        format!(
            "SELECT AGG(t.x) FROM (SELECT (AGG(order_count) * {n}) TO MEASURE AS x FROM orders) t"
        )
    };
    let a = ossiec::compile_layer2(&g, &wrap("2"), &duck()).unwrap();
    let b = ossiec::compile_layer2(&g, &wrap("3"), &duck()).unwrap();
    assert_ne!(a.request_hash, b.request_hash);
}

#[test]
fn derived_join_attach_dataset_is_planned_for_grand_total() {
    // Measure is on orders; attach is customers. Ensure customers is in the plan.
    let sql = "SELECT AGG(m.x) FROM customers c
        JOIN (SELECT customer_id, order_amt AS x FROM orders) m
          ON c.id = m.customer_id";
    let c = ossiec::compile_layer2(&two_facts_graph(), sql, &two_facts_opts()).unwrap();
    let got = duckdb_rows(&two_facts_db(), &c.sql);
    assert_eq!(got.len(), 1, "{got:?}");
    // 10+20+30+5 = 65
    assert_eq!(got[0][0].parse::<f64>().unwrap(), 65.0);
}

// ---------------------------------------------------------------- roles + includeUnmatched
//
// The vendored Ossie flights files (`tests/fixtures/ossie/flights.yaml` is an
// ontology; `flights.semantic_model.yaml` has no relationships or metrics) are
// not a queryable role-playing model. This is the same departure/arrival shape
// as `tests/requests.rs`.

const FLIGHTS: &str = r#"
version: "0.2.0.dev0"
name: flights
datasets:
  - name: flights
    source: flights
    primary_key: [id]
    fields:
      - { name: id, expression: { sql: id }, datatype: Integer }
      - { name: origin, expression: { sql: origin }, datatype: String }
      - { name: dest, expression: { sql: dest }, datatype: String }
      - { name: delay, expression: { sql: delay }, datatype: Integer }
    metrics:
      - { name: total_delay, expression: { sql: SUM(delay) } }
  - name: airports
    source: airports
    primary_key: [code]
    fields:
      - { name: code, expression: { sql: code }, datatype: String }
      - { name: city, expression: { sql: city }, datatype: String }
      - { name: gates, expression: { sql: gates }, datatype: Integer }
    metrics:
      - { name: gate_count, expression: { sql: SUM(gates) } }
relationships:
  - { name: departs_from, from: flights, to: airports, from_columns: [origin], to_columns: [code] }
  - { name: arrives_at, from: flights, to: airports, from_columns: [dest], to_columns: [code] }
"#;

fn flights_graph() -> ossiec::SemanticGraph {
    ossiec::from_yaml(FLIGHTS).unwrap()
}

fn flights_db() -> duckdb::Connection {
    let c = duckdb::Connection::open_in_memory().unwrap();
    c.execute_batch(
        "CREATE TABLE airports (code VARCHAR, city VARCHAR, gates INTEGER);
         INSERT INTO airports VALUES
           ('JFK', 'New York', 4), ('LHR', 'London', 3), ('CDG', 'Paris', 2),
           ('FRA', 'Frankfurt', 5); -- never a departure or arrival
         CREATE TABLE flights (id INTEGER, origin VARCHAR, dest VARCHAR, delay INTEGER);
         INSERT INTO flights VALUES
           (1, 'JFK', 'LHR', 10),
           (2, 'JFK', 'CDG', 5),
           (3, 'LHR', 'JFK', 20),
           (4, 'CDG', 'JFK', 7);",
    )
    .unwrap();
    c
}

fn l3(g: &ossiec::SemanticGraph, json: &str) -> ossiec::Compiled {
    ossiec::compile(g, &serde_json::from_str(json).unwrap(), &duck()).unwrap()
}

#[test]
fn role_playing_dimension_matches_layer3_and_duckdb() {
    let g = flights_graph();
    // Qualifier is the relationship, and it is also the JOIN alias.
    let aliased = "SELECT departs_from.city, AGG(flights.total_delay) \
         FROM flights \
         LEFT JOIN airports AS departs_from ON flights.origin = departs_from.code \
         GROUP BY departs_from.city";
    // Same role, with no JOIN: the planner adds it, as Layer 3 does.
    let implicit = "SELECT departs_from.city, AGG(flights.total_delay) \
         FROM flights GROUP BY departs_from.city";
    let joined = ossiec::compile_layer2(&g, aliased, &duck()).unwrap();
    let bare = ossiec::compile_layer2(&g, implicit, &duck()).unwrap();
    let layer3 = l3(
        &g,
        r#"{"metrics":["flights.total_delay"],"dimensions":["departs_from.city"]}"#,
    );
    assert_eq!(joined.sql, layer3.sql);
    assert_eq!(bare.sql, layer3.sql);
    assert!(
        joined.sql.contains("airports__departs_from"),
        "role alias: {}",
        joined.sql
    );
    let db = flights_db();
    assert_eq!(
        sorted(duckdb_rows(&db, &joined.sql)),
        sorted(duckdb_rows(
            &db,
            "SELECT a.city, SUM(f.delay) FROM flights f \
             LEFT JOIN airports a ON f.origin = a.code GROUP BY a.city"
        ))
    );
}

#[test]
fn both_roles_and_a_derived_join_on_one_of_them() {
    let g = flights_graph();
    // `dep` / `arr` are not the relationship names; the select list and the
    // derived ON qualify by role. `code <> 'CDG'` keeps JFK and LHR, so
    // CDG→JFK drops and JFK→CDG (arrival Paris) stays. Attaching the derived
    // table to `arrives_at` would do the opposite.
    let sql =
        "SELECT departs_from.city AS origin, arrives_at.city AS destination, AGG(f.total_delay) \
         FROM flights f \
         LEFT JOIN airports dep ON f.origin = dep.code \
         LEFT JOIN airports arr ON f.dest = arr.code \
         JOIN (SELECT code FROM airports WHERE code <> 'CDG' GROUP BY code) t \
           ON departs_from.code = t.code \
         GROUP BY departs_from.city, arrives_at.city";
    let c = ossiec::compile_layer2(&g, sql, &duck()).unwrap();
    assert!(
        c.sql.contains("airports__departs_from") && c.sql.contains("airports__arrives_at"),
        "{}",
        c.sql
    );
    let db = flights_db();
    let got = sorted(duckdb_rows(&db, &c.sql));
    let want = sorted(duckdb_rows(
        &db,
        "SELECT d.city, a.city, SUM(f.delay) \
         FROM flights f \
         LEFT JOIN airports d ON f.origin = d.code \
         LEFT JOIN airports a ON f.dest = a.code \
         JOIN (SELECT code FROM airports WHERE code <> 'CDG' GROUP BY code) t \
           ON d.code = t.code \
         GROUP BY d.city, a.city",
    ));
    assert_eq!(got, want, "sql:\n{}", c.sql);
    // CDG departures are gone; JFK→CDG remains (arrival city Paris).
    assert!(got.iter().any(|r| r[0] == "New York" && r[1] == "Paris"));
    assert!(got.iter().all(|r| r[0] != "Paris"));
}

#[test]
fn unnamed_role_tie_is_refused() {
    let e = ossiec::compile_layer2(
        &flights_graph(),
        "SELECT airports.city, AGG(flights.total_delay) \
         FROM flights LEFT JOIN airports ON flights.origin = airports.code \
         GROUP BY airports.city",
        &duck(),
    )
    .unwrap_err();
    let m = e.to_string();
    assert!(
        m.contains("departs_from") && m.contains("arrives_at"),
        "{m}"
    );
}

const STOCK: &str = r#"
version: "0.2.0.dev0"
name: stock
datasets:
  - name: orders
    source: orders
    primary_key: [id]
    fields:
      - { name: id, expression: { sql: id }, datatype: Integer }
      - { name: product_id, expression: { sql: product_id }, datatype: Integer }
      - { name: region, expression: { sql: region }, datatype: String }
  - name: products
    source: products
    primary_key: [id]
    fields:
      - { name: id, expression: { sql: id }, datatype: Integer }
      - { name: stock, expression: { sql: stock }, datatype: Integer }
    metrics:
      - { name: stock_units, expression: { sql: SUM(stock) } }
relationships:
  - { name: orders_to_products, from: orders, to: products, from_columns: [product_id], to_columns: [id] }
"#;

fn stock_graph() -> ossiec::SemanticGraph {
    ossiec::from_yaml(STOCK).unwrap()
}

fn stock_db() -> duckdb::Connection {
    let c = duckdb::Connection::open_in_memory().unwrap();
    // Product 3 (stock 7) and 4 (stock 15) were never ordered. 1 was ordered twice.
    c.execute_batch(
        "CREATE TABLE products (id INTEGER, stock INTEGER);
         INSERT INTO products VALUES (1, 10), (2, 20), (3, 7), (4, 15);
         CREATE TABLE orders (id INTEGER, product_id INTEGER, region VARCHAR);
         INSERT INTO orders VALUES (1, 1, 'EU'), (2, 1, 'EU'), (3, 2, 'US');",
    )
    .unwrap();
    c
}

fn stock_sql() -> &'static str {
    "SELECT o.region, AGG(p.stock_units) \
     FROM orders o \
     LEFT JOIN products p ON o.product_id = p.id \
     GROUP BY o.region"
}

fn unmatched_opts() -> Options {
    Options {
        include_unmatched: true,
        ..duck()
    }
}

#[test]
fn include_unmatched_is_layer2_only_and_matches_duckdb() {
    let g = stock_graph();
    let l2 = ossiec::compile_layer2(&g, stock_sql(), &unmatched_opts()).unwrap();
    // Layer 3 metrics read their home dataset's rows, so there are none to add.
    let e = ossiec::compile(
        &g,
        &serde_json::from_str(
            r#"{"metrics":["products.stock_units"],"dimensions":["products.category"],"includeUnmatched":true}"#,
        )
        .unwrap(),
        &duck(),
    )
    .expect_err("includeUnmatched is Layer 2's");
    assert_eq!(e.code(), Some(ossiec::ErrorCode::InvalidQuery), "{e}");
    let req =
        ossiec::layer2_to_request(&g, &format!("-- ossiec: includeUnmatched\n{}", stock_sql()))
            .unwrap();
    assert!(req.include_unmatched);
    assert_eq!(req.dimensions, vec!["orders.region"]);
    let via_pragma = ossiec::compile_layer2(
        &g,
        &format!("-- ossiec: includeUnmatched\n{}", stock_sql()),
        &duck(),
    )
    .unwrap();
    assert_eq!(via_pragma.sql, l2.sql);
    let db = stock_db();
    let got = sorted(duckdb_rows(&db, &l2.sql));
    let want = sorted(duckdb_rows(
        &db,
        "WITH matched AS (
            SELECT DISTINCT o.region, p.id, p.stock
            FROM orders o
            LEFT JOIN products p ON o.product_id = p.id
            WHERE p.id IS NOT NULL
         ), unmatched AS (
            SELECT CAST(NULL AS VARCHAR) AS region, p.id, p.stock
            FROM products p
            WHERE NOT EXISTS (SELECT 1 FROM orders o WHERE o.product_id = p.id)
         ), r AS (
            SELECT * FROM matched UNION ALL SELECT * FROM unmatched
         ), g AS (
            SELECT DISTINCT region FROM (
              SELECT o.region AS region FROM orders o
              UNION ALL
              SELECT region FROM unmatched
            ) u
         )
         SELECT g.region, s.stock
         FROM g
         LEFT JOIN (SELECT region, SUM(stock) AS stock FROM r GROUP BY region) s
           ON g.region IS NOT DISTINCT FROM s.region",
    ));
    assert_eq!(got, want);
    // The unsold products share the NULL region: 7 + 15.
    assert!(got.iter().any(|r| r[0] == "NULL" && r[1] == "22.000000"));
}

#[test]
fn include_unmatched_with_derived_join_on_the_grain() {
    let g = stock_graph();
    // stock >= 10 keeps products 1, 2 and 4. Product 3 (stock 7, never ordered) drops.
    // Product 4 was never ordered and still qualifies, so it is the NULL group.
    let sql = "SELECT o.region, AGG(p.stock_units) \
         FROM orders o \
         LEFT JOIN products p ON o.product_id = p.id \
         JOIN (SELECT id FROM products WHERE stock >= 10 GROUP BY id) t ON p.id = t.id \
         GROUP BY o.region";
    let c = ossiec::compile_layer2(&g, sql, &unmatched_opts()).unwrap();
    assert!(
        c.sql.contains("__unmatched"),
        "unmatched branch should rename aliases:\n{}",
        c.sql
    );
    let db = stock_db();
    let got = sorted(duckdb_rows(&db, &c.sql));
    let want = sorted(duckdb_rows(
        &db,
        "WITH keep AS (SELECT id FROM products WHERE stock >= 10 GROUP BY id),
         matched AS (
            SELECT DISTINCT o.region, p.id, p.stock
            FROM orders o
            LEFT JOIN products p ON o.product_id = p.id
            JOIN keep t ON p.id = t.id
            WHERE p.id IS NOT NULL
         ), unmatched AS (
            SELECT CAST(NULL AS VARCHAR) AS region, p.id, p.stock
            FROM products p
            JOIN keep t ON p.id = t.id
            WHERE NOT EXISTS (SELECT 1 FROM orders o WHERE o.product_id = p.id)
         ), r AS (
            SELECT * FROM matched UNION ALL SELECT * FROM unmatched
         ), g AS (
            SELECT DISTINCT o.region
            FROM orders o
            LEFT JOIN products p ON o.product_id = p.id
            JOIN keep t ON p.id = t.id
            UNION ALL SELECT DISTINCT region FROM unmatched
         )
         SELECT g.region, s.stock
         FROM g
         LEFT JOIN (SELECT region, SUM(stock) AS stock FROM r GROUP BY region) s
           ON g.region IS NOT DISTINCT FROM s.region",
    ));
    assert_eq!(got, want, "sql:\n{}", c.sql);
    assert!(
        got.iter().any(|r| r[0] == "NULL" && r[1].starts_with("15")),
        "{got:?}\n{}",
        c.sql
    );
    assert!(
        got.iter()
            .all(|r| !r[1].starts_with('7') && !r[1].starts_with("22")),
        "{got:?}"
    );
}

// ---------------------------------------------------------------- A.7 CREATE VIEW

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

#[test]
fn create_view_agg_of_measure_matches_inline_ratio() {
    let (g, catalog) = define_region_metrics();
    let via = "SELECT region, AGG(sales_per_ticket) AS spt \
        FROM region_metrics GROUP BY region";
    let inline = format!(
        "SELECT region, AGG(m.sales_per_ticket) AS spt \
         FROM ({SALES_PER_TICKET}) m GROUP BY region"
    );
    let c = ossiec::compile_layer2_with(&g, &catalog, via, &two_facts_opts()).unwrap();
    let direct = ossiec::compile_layer2(&g, &inline, &two_facts_opts()).unwrap();
    assert_eq!(c.sql, direct.sql);
    assert!(
        !c.sql.contains("__l2_tm") && !c.sql.contains("__ossiec"),
        "{}",
        c.sql
    );
    assert!(c.sql.contains("UNION ALL"), "{}", c.sql);
    assert_region_ratio(&c.sql, true);
    // The catalog is session state: compile_layer2 does not see the view.
    let missing = ossiec::compile_layer2(&g, via, &two_facts_opts()).unwrap_err();
    assert!(
        matches!(missing, Error::Request(ref m) if m.contains("region_metrics")),
        "{missing}"
    );
}

#[test]
fn create_view_join_form_matches_derived_table() {
    let (g, catalog) = define_region_metrics();
    let via = "SELECT r.region, AGG(m.sales_per_ticket) AS spt \
        FROM customers r \
        JOIN region_metrics m ON r.region = m.region \
        GROUP BY r.region";
    let inline = format!(
        "SELECT r.region, AGG(m.sales_per_ticket) AS spt \
         FROM customers r JOIN ({SALES_PER_TICKET}) m ON r.region = m.region \
         GROUP BY r.region"
    );
    let c = ossiec::compile_layer2_with(&g, &catalog, via, &two_facts_opts()).unwrap();
    let direct = ossiec::compile_layer2(&g, &inline, &two_facts_opts()).unwrap();
    assert_eq!(c.sql, direct.sql);
    assert_region_ratio(&c.sql, false);
}

#[test]
fn create_view_outer_filter_keeps_one_region() {
    let (g, catalog) = define_region_metrics();
    let c = ossiec::compile_layer2_with(
        &g,
        &catalog,
        "SELECT region, AGG(sales_per_ticket) AS spt FROM region_metrics \
         WHERE region = 'EU' GROUP BY region",
        &two_facts_opts(),
    )
    .unwrap();
    let got = rows_f64(duckdb_rows(&two_facts_db(), &c.sql));
    assert_eq!(got.len(), 1, "{got:?}");
    assert_eq!(got[0].0, "EU");
    assert!((got[0].1 - 20.0).abs() < 1e-9, "{got:?}");
}

#[test]
fn create_view_dimension_only_and_pass_through() {
    let g = two_facts_graph();
    let mut catalog = Layer2Catalog::new();
    ossiec::define_layer2_view(
        &g,
        &mut catalog,
        "CREATE VIEW eu_ids AS SELECT c.id FROM customers c WHERE c.region = 'EU'",
        &two_facts_opts(),
    )
    .unwrap();
    let cols = catalog.columns("eu_ids").unwrap();
    assert_eq!(cols.len(), 1);
    assert_eq!(cols[0].kind, Layer2ColumnKind::Field);
    let listed =
        ossiec::compile_layer2_with(&g, &catalog, "SELECT id FROM eu_ids", &two_facts_opts())
            .unwrap();
    assert_eq!(
        sorted(duckdb_rows(&two_facts_db(), &listed.sql)),
        sorted(duckdb_rows(
            &two_facts_db(),
            "SELECT id FROM customers WHERE region = 'EU'"
        ))
    );
    // A second view reads the first. AGG of the passed-through measure is the EU total.
    ossiec::define_layer2_view(
        &g,
        &mut catalog,
        "CREATE VIEW eu_orders AS \
         SELECT o.customer_id, o.order_amt FROM orders o \
         JOIN eu_ids e ON o.customer_id = e.id",
        &two_facts_opts(),
    )
    .unwrap();
    let summed = ossiec::compile_layer2_with(
        &g,
        &catalog,
        "SELECT AGG(order_amt) AS amt FROM eu_orders",
        &two_facts_opts(),
    )
    .unwrap();
    assert_eq!(
        duckdb_rows(&two_facts_db(), &summed.sql),
        duckdb_rows(
            &two_facts_db(),
            "SELECT SUM(amount) FROM orders WHERE customer_id IN (1, 2)"
        )
    );
}

#[test]
fn create_view_script_splits_on_semicolons_outside_strings() {
    let g = two_facts_graph();
    let script = format!(
        "{REGION_METRICS};\n\
         -- a comment;\n\
         SELECT region, AGG(sales_per_ticket) AS spt FROM region_metrics GROUP BY region;"
    );
    let c = ossiec::compile_layer2_script(&g, &script, &two_facts_opts()).unwrap();
    assert_region_ratio(&c.sql, true);
    // A semicolon inside a string is not a statement boundary.
    let mut catalog = Layer2Catalog::new();
    ossiec::define_layer2_view(
        &g,
        &mut catalog,
        "CREATE VIEW labeled AS SELECT c.region FROM customers c WHERE c.region = 'a;b'",
        &two_facts_opts(),
    )
    .unwrap();
    let c = ossiec::compile_layer2_with(
        &g,
        &catalog,
        "SELECT region FROM labeled",
        &two_facts_opts(),
    )
    .unwrap();
    assert!(duckdb_rows(&two_facts_db(), &c.sql).is_empty());
}

#[test]
fn create_view_refusals() {
    let g = two_facts_graph();
    let opts = two_facts_opts();
    let body = "SELECT c.region FROM customers c";
    for sql in [
        "CREATE OR REPLACE VIEW region_metrics AS SELECT c.region FROM customers c",
        "CREATE OR ALTER VIEW region_metrics AS SELECT c.region FROM customers c",
        "CREATE MATERIALIZED VIEW region_metrics AS SELECT c.region FROM customers c",
        "CREATE VIEW region_metrics (region) AS SELECT c.region FROM customers c",
    ] {
        let e = ossiec::define_layer2_view(&g, &mut Layer2Catalog::new(), sql, &opts).unwrap_err();
        assert!(
            matches!(e, Error::Unsupported(ref m) if m.contains("CREATE VIEW") || m.contains("OR REPLACE") || m.contains("OR ALTER")),
            "{sql}: {e}"
        );
    }
    let e = ossiec::define_layer2_view(
        &g,
        &mut Layer2Catalog::new(),
        "DROP VIEW region_metrics",
        &opts,
    )
    .unwrap_err();
    assert!(
        matches!(e, Error::Unsupported(ref m) if m.contains("DROP VIEW")),
        "{e}"
    );
    let e = ossiec::compile_layer2(&g, "DROP VIEW region_metrics", &opts).unwrap_err();
    assert!(
        matches!(e, Error::Unsupported(ref m) if m.contains("DROP VIEW")),
        "{e}"
    );
    let e = ossiec::compile_layer2(&g, REGION_METRICS, &opts).unwrap_err();
    assert!(
        matches!(e, Error::Unsupported(ref m) if m.contains("define_layer2_view")),
        "{e}"
    );
    let e = ossiec::define_layer2_view(
        &g,
        &mut Layer2Catalog::new(),
        &format!("CREATE VIEW customers AS {body}"),
        &opts,
    )
    .unwrap_err();
    assert!(
        matches!(e, Error::Request(ref m) if m.contains("collides") && m.contains("customers")),
        "{e}"
    );
    let mut catalog = Layer2Catalog::new();
    ossiec::define_layer2_view(&g, &mut catalog, REGION_METRICS, &opts).unwrap();
    let e = ossiec::define_layer2_view(&g, &mut catalog, REGION_METRICS, &opts).unwrap_err();
    assert!(
        matches!(e, Error::Request(ref m) if m.contains("already defined")),
        "{e}"
    );
    let e = ossiec::define_layer2_view(
        &g,
        &mut Layer2Catalog::new(),
        "CREATE VIEW self_view AS SELECT c.region FROM self_view c",
        &opts,
    )
    .unwrap_err();
    assert!(
        matches!(e, Error::Request(ref m) if m.contains("refers to itself")),
        "{e}"
    );
    let e = ossiec::compile_layer2_script(&g, REGION_METRICS, &opts).unwrap_err();
    assert!(
        matches!(e, Error::Request(ref m) if m.contains("no SELECT")),
        "{e}"
    );
}

#[test]
fn create_view_compiles_on_every_engine() {
    let g = two_facts_graph();
    let query = "SELECT region, AGG(sales_per_ticket) AS spt FROM region_metrics GROUP BY region";
    for target in ENGINES {
        let opts = Options {
            target: Some(target),
            ..Default::default()
        };
        let mut catalog = Layer2Catalog::new();
        ossiec::define_layer2_view(&g, &mut catalog, REGION_METRICS, &opts)
            .unwrap_or_else(|e| panic!("define on {target:?}: {e}"));
        let c = ossiec::compile_layer2_with(&g, &catalog, query, &opts)
            .unwrap_or_else(|e| panic!("compile on {target:?}: {e}"));
        assert!(
            !c.sql.contains("__ossiec") && !c.sql.contains("__l2_tm"),
            "{target:?}: {}",
            c.sql
        );
        polyglot_sql::parse_one(&c.sql, target)
            .unwrap_or_else(|e| panic!("{target:?} does not parse: {e}\n{}", c.sql));
    }
}

// ---------------------------------------------------------------- combinations (recent merges)

#[test]
fn create_view_with_base_sum_to_measure() {
    // A.7 body with a base §5.1 measure, not only an AGG composition.
    let g = two_facts_graph();
    let mut catalog = Layer2Catalog::new();
    ossiec::define_layer2_view(
        &g,
        &mut catalog,
        "CREATE VIEW region_revenue AS \
         SELECT c.region, SUM(o.amount) TO MEASURE AS total_revenue \
         FROM customers c LEFT JOIN orders o ON o.customer_id = c.id",
        &two_facts_opts(),
    )
    .unwrap();
    let via = "SELECT region, AGG(total_revenue) AS total_revenue \
        FROM region_revenue GROUP BY region";
    let fold = "SELECT region, AGG(total_revenue) AS total_revenue FROM ( \
         SELECT c.region, SUM(o.amount) TO MEASURE AS total_revenue \
         FROM customers c LEFT JOIN orders o ON o.customer_id = c.id) m GROUP BY region";
    let c = ossiec::compile_layer2_with(&g, &catalog, via, &two_facts_opts()).unwrap();
    let direct = ossiec::compile_layer2(&g, fold, &two_facts_opts()).unwrap();
    assert_eq!(
        c.sql, direct.sql,
        "view expands to the same fold as an inline derived FROM"
    );
    let got = rows_f64(duckdb_rows(&two_facts_db(), &c.sql));
    let want = rows_f64(duckdb_rows(
        &two_facts_db(),
        "SELECT c.region, SUM(o.amount) \
         FROM customers c LEFT JOIN orders o ON o.customer_id = c.id \
         GROUP BY c.region",
    ));
    assert_eq!(got.len(), want.len(), "{got:?} vs {want:?}");
    for (a, b) in got.iter().zip(&want) {
        assert_eq!(a.0, b.0);
        assert!((a.1 - b.1).abs() < 1e-9, "{got:?} vs {want:?}");
    }
}

#[test]
fn include_unmatched_with_role_played_derived_attach() {
    // One relationship so `airports.gate_count` is unambiguous, but the dimension
    // still uses the role form (`departs_from.city`) and the derived ON attaches
    // through that role alias. FRA never hosts a departure; the derived join keeps
    // FRA/JFK/LHR (drops CDG). With includeUnmatched, Frankfurt's gates appear
    // under a NULL city group and the derived attach rides `…__unmatched`.
    let model = FLIGHTS.replace(
        "  - { name: arrives_at, from: flights, to: airports, from_columns: [dest], to_columns: [code] }\n",
        "",
    );
    let g = ossiec::from_yaml(&model).unwrap();
    let sql = "-- ossiec: includeUnmatched\n\
         SELECT departs_from.city, AGG(airports.gate_count) \
         FROM flights f \
         LEFT JOIN airports dep ON f.origin = dep.code \
         JOIN (SELECT code FROM airports WHERE code <> 'CDG' GROUP BY code) t \
           ON departs_from.code = t.code \
         GROUP BY departs_from.city";
    let c = ossiec::compile_layer2(&g, sql, &duck()).unwrap();
    assert!(
        c.sql.contains("airports__departs_from") && c.sql.contains("__unmatched"),
        "{}",
        c.sql
    );
    let db = flights_db();
    let got = sorted(duckdb_rows(&db, &c.sql));
    let want = sorted(duckdb_rows(
        &db,
        "WITH keep AS (SELECT code FROM airports WHERE code <> 'CDG' GROUP BY code),
         matched AS (
            SELECT DISTINCT a.city, a.code, a.gates
            FROM flights f
            LEFT JOIN airports a ON f.origin = a.code
            JOIN keep t ON a.code = t.code
            WHERE a.code IS NOT NULL
         ), unmatched AS (
            SELECT CAST(NULL AS VARCHAR) AS city, a.code, a.gates
            FROM airports a
            JOIN keep t ON a.code = t.code
            WHERE NOT EXISTS (SELECT 1 FROM flights f WHERE f.origin = a.code)
         ), r AS (
            SELECT * FROM matched UNION ALL SELECT * FROM unmatched
         ), g AS (
            SELECT DISTINCT city FROM (
              SELECT a.city FROM flights f
              LEFT JOIN airports a ON f.origin = a.code
              JOIN keep t ON a.code = t.code
              UNION ALL SELECT city FROM unmatched
            ) u
         )
         SELECT g.city, s.gates
         FROM g
         LEFT JOIN (SELECT city, SUM(gates) AS gates FROM r GROUP BY city) s
           ON g.city IS NOT DISTINCT FROM s.city",
    ));
    assert_eq!(got, want, "sql:\n{}", c.sql);
    assert!(
        got.iter().any(|r| r[0] == "NULL" && r[1].starts_with('5')),
        "unused FRA (5 gates) should appear unmatched: {got:?}\n{}",
        c.sql
    );
    assert!(got.iter().all(|r| r[0] != "Paris"), "{got:?}");
}

#[test]
fn a4_ratio_with_partial_fact_derived_join() {
    // Derived join hosts orders only: ticket counts stay complete across every
    // region, and the ratio does not fan either fact through the other.
    let g = two_facts_graph();
    let sql = "SELECT c.region, \
                      AGG(o.order_amt) / AGG(t.ticket_count) AS sales_per_ticket \
               FROM customers c \
               LEFT JOIN orders o ON o.customer_id = c.id \
               LEFT JOIN tickets t ON t.customer_id = c.id \
               JOIN (SELECT customer_id, AGG(order_amt) AS n FROM orders \
                     GROUP BY customer_id ORDER BY n DESC, customer_id LIMIT 2) d \
                 ON o.customer_id = d.customer_id \
               GROUP BY c.region";
    let c = ossiec::compile_layer2(&g, sql, &two_facts_opts()).unwrap();
    assert!(c.sql.contains("UNION ALL"), "{}", c.sql);
    let db = two_facts_db();
    let got = rows_f64(duckdb_rows(&db, &c.sql));
    let want = rows_f64(duckdb_rows(
        &db,
        "SELECT region, \
                CASE WHEN order_amt IS NULL THEN NULL \
                     ELSE order_amt * 1.0 / ticket_count END \
         FROM (
            SELECT region, MAX(order_amt) AS order_amt, COALESCE(MAX(ticket_count), 0) AS ticket_count FROM (
                SELECT c.region AS region, SUM(o.amount) AS order_amt, NULL::BIGINT AS ticket_count
                FROM orders o
                LEFT JOIN customers c ON o.customer_id = c.id
                JOIN (SELECT customer_id FROM orders GROUP BY customer_id
                      ORDER BY SUM(amount) DESC, customer_id LIMIT 2) d
                  ON o.customer_id = d.customer_id
                GROUP BY c.region
                UNION ALL
                SELECT c.region, NULL, COUNT(t.customer_id)
                FROM customers c
                LEFT JOIN tickets t ON t.customer_id = c.id
                GROUP BY c.region
             ) GROUP BY region
         )",
    ));
    assert_eq!(got, want, "{}", c.sql);
    let eu = got.iter().find(|r| r.0 == "EU").unwrap();
    assert!((eu.1 - 20.0).abs() < 1e-9, "60/3 = 20: {got:?}");
    // US / APAC keep unrestricted tickets and a NULL numerator → NULL ratio.
    // Customer 5 (no region, nothing bought) is a group too, with a NULL ratio.
    assert!(got.iter().any(|r| r.0 == "US" && r.1 < 0.0), "{got:?}");
    assert!(got.iter().any(|r| r.0 == "NULL" && r.1 < 0.0), "{got:?}");
    assert!(got.iter().any(|r| r.0 == "APAC" && r.1 < 0.0), "{got:?}");
}

/// Appendix A.7: conformed-dimension bridge as a session `CREATE VIEW`.
#[test]
fn a7_appendix_conformed_dimension_bridge() {
    let (g, catalog) = define_region_metrics();
    let via = "SELECT region, AGG(sales_per_ticket) AS sales_per_ticket \
        FROM region_metrics GROUP BY region";
    let c = ossiec::compile_layer2_with(&g, &catalog, via, &two_facts_opts()).unwrap();
    assert_region_ratio(&c.sql, true);
}

// -------------------------------------------------------- A.7 persist into model

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

/// §7.3: `WHERE` is an ordinary row predicate, on any column the query can read.
#[test]
fn where_accepts_any_row_predicate() {
    let g = ossiec::from_yaml(common::MODEL).unwrap();
    let db = duckdb();
    let by = |predicate: &str, reference: &str| {
        let sql = format!(
            "SELECT status, AGG(order_count) FROM orders WHERE {predicate} GROUP BY status"
        );
        let compiled =
            ossiec::compile_sql(&g, &sql, &duck()).unwrap_or_else(|e| panic!("{predicate}: {e}"));
        let mut got = duckdb_rows(&db, &compiled.sql);
        got.sort();
        let mut want = duckdb_rows(
            &db,
            &format!("SELECT status, COUNT(*) FROM orders WHERE {reference} GROUP BY status"),
        );
        want.sort();
        assert_eq!(got, want, "{predicate}\n{}", compiled.sql);
        assert!(!want.is_empty(), "{predicate}: proves nothing");
    };
    for (predicate, reference) in [
        (
            "status IN ('completed', 'x')",
            "status IN ('completed', 'x')",
        ),
        ("status NOT IN ('completed')", "status NOT IN ('completed')"),
        ("status LIKE 'comp%'", "status LIKE 'comp%'"),
        ("id BETWEEN 100 AND 103", "order_id BETWEEN 100 AND 103"),
        (
            "status = 'returned' OR id = 100",
            "status = 'returned' OR order_id = 100",
        ),
        ("NOT status = 'returned'", "NOT status = 'returned'"),
        ("id + 1 = 101", "order_id + 1 = 101"),
        (
            "customer_id = product_id - 9",
            "customer_id = product_id - 9",
        ),
        (
            "status = 'completed' AND id IN (100, 101)",
            "status = 'completed' AND order_id IN (100, 101)",
        ),
    ] {
        by(predicate, reference);
    }
    // §7.3 rejects these, or they aren't done yet: none is answered wrongly.
    for (predicate, want) in [
        ("AGG(order_count) > 1", "aggregate"),
        ("SUM(id) > 1", "aggregate"),
        ("id IN (SELECT 1)", "subqueries"),
        ("nope = 1 OR id = 1", "nope"),
        ("order_count = 1 OR id = 1", "order_count"),
    ] {
        let sql = format!(
            "SELECT status, AGG(order_count) FROM orders WHERE {predicate} GROUP BY status"
        );
        let e = ossiec::compile_sql(&g, &sql, &duck())
            .expect_err(predicate)
            .to_string();
        assert!(e.contains(want), "{predicate}: {e}");
    }
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

fn answers(sql: &str, reference: &str) {
    let g = ossiec::from_yaml(common::MODEL).unwrap();
    let db = duckdb();
    let c = ossiec::compile_sql(&g, sql, &duck()).unwrap_or_else(|e| panic!("{sql}: {e}"));
    let want = sorted(duckdb_rows(&db, reference));
    assert_eq!(sorted(duckdb_rows(&db, &c.sql)), want, "{sql}\n{}", c.sql);
    assert!(!want.is_empty(), "{sql}: proves nothing");
}

fn refuses(sql: &str, want: &str) {
    let g = ossiec::from_yaml(common::MODEL).unwrap();
    let e = ossiec::compile_sql(&g, sql, &duck())
        .expect_err(sql)
        .to_string();
    assert!(e.contains(want), "want `{want}` for {sql}: {e}");
}

/// §7.5: `HAVING` may contain `AGG()` and scalar expressions over it.
#[test]
fn having_keeps_or_drops_finished_groups() {
    answers(
        "SELECT status FROM orders GROUP BY status HAVING AGG(order_count) > 1",
        "SELECT status FROM orders GROUP BY status HAVING COUNT(*) > 1",
    );
    answers(
        "SELECT status, AGG(order_count) FROM orders GROUP BY status HAVING COALESCE(AGG(order_count), 0) * 2 >= 4 AND NOT AGG(order_count) = 100",
        "SELECT status, COUNT(*) FROM orders GROUP BY status HAVING COUNT(*) * 2 >= 4",
    );
    // A grouping column and a measure in one condition stay over the groups.
    answers(
        "SELECT status, AGG(order_count) FROM orders GROUP BY status HAVING status = 'returned' OR AGG(order_count) > 5",
        "SELECT status, COUNT(*) FROM orders GROUP BY status HAVING status = 'returned' OR COUNT(*) > 5",
    );
    // With no GROUP BY there's one group, kept or dropped.
    answers(
        "SELECT AGG(order_count) FROM orders HAVING AGG(order_count) > 1",
        "SELECT COUNT(*) FROM orders",
    );
    let g = ossiec::from_yaml(common::MODEL).unwrap();
    let c = ossiec::compile_sql(
        &g,
        "SELECT AGG(order_count) FROM orders HAVING AGG(order_count) > 1000",
        &duck(),
    )
    .unwrap();
    assert!(duckdb_rows(&duck_db(), &c.sql).is_empty(), "{}", c.sql);
    let c = ossiec::compile_sql(
        &g,
        "SELECT AGG(order_count) FROM orders HAVING 1 = 0",
        &duck(),
    )
    .unwrap();
    assert!(duckdb_rows(&duck_db(), &c.sql).is_empty(), "{}", c.sql);
    // Through a sole derived table, and only as a filter on the result's rows.
    answers(
        "SELECT status, AGG(n) FROM (SELECT status, order_count AS n FROM orders WHERE customer_id IS NOT NULL) t GROUP BY status HAVING AGG(n) > 1",
        "SELECT status, COUNT(*) FROM orders WHERE customer_id IS NOT NULL GROUP BY status HAVING COUNT(*) > 1",
    );
    refuses(
        "SELECT status, AGG(order_count) FROM orders GROUP BY status HAVING customer_id > 1",
        "not in GROUP BY",
    );
    refuses(
        "SELECT status FROM orders HAVING AGG(order_count) > 1",
        "need GROUP BY",
    );
    refuses(
        "SELECT status, AGG(order_count) FROM orders GROUP BY status HAVING order_count > 1",
        "order_count",
    );
    refuses(
        "SELECT status, AGG(order_count) FROM orders GROUP BY status HAVING SUM(id) > 1",
        "plain aggregate",
    );
    refuses(
        "SELECT status, AGG(order_count) FROM orders GROUP BY status HAVING AGG(order_count) > (SELECT 1)",
        "subqueries",
    );
    refuses(
        "SELECT status, AGG(order_count) FROM orders GROUP BY status HAVING AGG(nope) > 1",
        "nope",
    );
    // A request has nowhere to put it.
    let e = ossiec::sql_to_request(
        &g,
        "SELECT status FROM orders GROUP BY status HAVING AGG(order_count) > 1",
    )
    .expect_err("no HAVING in a request")
    .to_string();
    assert!(e.contains("HAVING"), "{e}");
    // Two HAVINGs, one hash each.
    let hash = |h: &str| {
        let sql = format!("SELECT status, AGG(order_count) FROM orders GROUP BY status HAVING {h}");
        ossiec::compile_sql(&g, &sql, &duck()).unwrap().request_hash
    };
    assert_ne!(hash("AGG(order_count) > 1"), hash("AGG(order_count) > 2"));
}

fn duck_db() -> duckdb::Connection {
    duckdb()
}

/// §7.7: a CTE is a derived table with a name.
#[test]
fn with_names_derived_tables() {
    // The same query, written with and without it.
    let g = ossiec::from_yaml(common::MODEL).unwrap();
    let plain = "SELECT status, AGG(order_count) FROM orders GROUP BY status";
    let with = "WITH o AS (SELECT status, order_count FROM orders) SELECT status, AGG(order_count) FROM o GROUP BY status";
    assert_eq!(
        ossiec::compile_sql(&g, plain, &duck()).unwrap().sql,
        ossiec::compile_sql(&g, with, &duck()).unwrap().sql
    );
    // A CTE's body reads the dataset of its own name, and the query reads the CTE.
    answers(
        "WITH orders AS (SELECT status, order_count FROM orders WHERE status = 'returned') \
         SELECT status, AGG(order_count) FROM orders GROUP BY status",
        "SELECT status, COUNT(*) FROM orders WHERE status = 'returned' GROUP BY status",
    );
    // An alias on the reference.
    answers(
        "WITH b AS (SELECT status, order_count FROM orders WHERE customer_id IS NOT NULL) \
         SELECT x.status, AGG(x.order_count) FROM b x GROUP BY x.status",
        "SELECT status, COUNT(*) FROM orders WHERE customer_id IS NOT NULL GROUP BY status",
    );
    // Two levels of pass-through aren't folded yet, with or without WITH.
    refuses(
        "WITH a AS (SELECT status, customer_id, order_count FROM orders), \
              b AS (SELECT status, order_count FROM a WHERE customer_id IS NOT NULL) \
         SELECT status, AGG(order_count) FROM b GROUP BY status",
        "a derived table as its FROM",
    );
    // Over a plain aggregate outer query (A.3), inside a derived table, used twice.
    answers(
        "WITH per AS (SELECT status, AGG(order_count) AS n FROM orders GROUP BY status) \
         SELECT AVG(n) FROM per",
        "SELECT AVG(n) FROM (SELECT status, COUNT(*) AS n FROM orders GROUP BY status) t",
    );
    answers(
        "SELECT AVG(n) FROM (WITH per AS (SELECT status, AGG(order_count) AS n FROM orders GROUP BY status) SELECT n FROM per) t",
        "SELECT AVG(n) FROM (SELECT status, COUNT(*) AS n FROM orders GROUP BY status) t",
    );
    answers(
        "WITH per AS (SELECT customer_id, AGG(order_count) AS n FROM orders GROUP BY customer_id) \
         SELECT o.status, AGG(o.order_count) FROM orders o \
         JOIN per a ON o.customer_id = a.customer_id \
         JOIN per b ON o.customer_id = b.customer_id GROUP BY o.status",
        "SELECT status, COUNT(*) FROM orders WHERE customer_id IS NOT NULL GROUP BY status",
    );
    // A qualified name is the dataset, whatever the CTEs are called.
    answers(
        "WITH orders AS (SELECT status, order_count FROM orders WHERE status = 'returned') \
         SELECT status, AGG(order_count) FROM shop.orders GROUP BY status",
        "SELECT status, COUNT(*) FROM orders GROUP BY status",
    );
    // In a view's body too.
    let script = "CREATE VIEW v AS WITH k AS (SELECT status, AGG(order_count) AS n FROM orders WHERE customer_id IS NOT NULL GROUP BY status) SELECT status, n FROM k; \
        SELECT status, n FROM v";
    let c = ossiec::compile_sql_script(&g, script, &duck()).unwrap();
    assert_eq!(
        sorted(duckdb_rows(&duck_db(), &c.sql)),
        sorted(duckdb_rows(
            &duck_db(),
            "SELECT status, COUNT(*) FROM orders WHERE customer_id IS NOT NULL GROUP BY status"
        ))
    );
    refuses(
        "WITH RECURSIVE r AS (SELECT status FROM orders) SELECT status FROM r",
        "RECURSIVE",
    );
    refuses(
        "WITH r(s) AS (SELECT status FROM orders) SELECT s FROM r",
        "column list",
    );
    refuses(
        "WITH r AS (SELECT status FROM orders), R AS (SELECT status FROM orders) SELECT status FROM r",
        "twice",
    );
    refuses(
        "WITH r AS (SELECT status FROM orders UNION ALL SELECT status FROM orders) SELECT status FROM r",
        "single SELECT",
    );
    // A later CTE isn't in scope for an earlier one.
    refuses(
        "WITH a AS (SELECT status FROM b), b AS (SELECT status FROM orders) SELECT status FROM a",
        "b",
    );
}

/// A view can't define a measure and also aggregate with HAVING (§7.2), so there's
/// no such view to persist.
#[test]
fn a_view_defining_a_measure_cannot_have_having() {
    let g = ossiec::from_yaml(common::MODEL).unwrap();
    let mut catalog = Layer2Catalog::new();
    for view in [
        "CREATE VIEW v AS SELECT status, COUNT(*) TO MEASURE AS n FROM orders GROUP BY status HAVING AGG(order_count) > 1",
        "CREATE VIEW v AS SELECT status, COUNT(*) TO MEASURE AS n FROM orders HAVING AGG(order_count) > 1",
    ] {
        let e = ossiec::define_layer2_view(&g, &mut catalog, view, &duck())
            .expect_err(view)
            .to_string();
        assert!(e.contains("§7.2") || e.contains("HAVING"), "{view}: {e}");
    }
}

/// What a target's SQL has to carry, which DuckDB's answers can't show.
#[test]
fn having_keeps_each_engines_rules() {
    let g = ossiec::from_yaml(common::MODEL).unwrap();
    let compile = |sql: &str, target: DialectType| {
        let opts = Options {
            target: Some(target),
            ..Default::default()
        };
        ossiec::compile_sql(&g, sql, &opts).unwrap().sql
    };
    // A grouping column compares as in WHERE: by byte on MySQL.
    let sql = compile(
        "SELECT status, AGG(order_count) FROM orders GROUP BY status HAVING status = 'ada'",
        DialectType::MySQL,
    );
    let where_at = sql.find("WHERE").expect("the condition moved to WHERE");
    assert!(where_at < sql.find("GROUP BY").unwrap(), "{sql}");
    assert!(sql[where_at..].contains("utf8mb4_bin"), "{sql}");
    // `/` is true division, as in every other portable expression.
    let sql = compile(
        "SELECT status, AGG(order_count) FROM orders GROUP BY status HAVING AGG(order_count) / AGG(order_count) > 0.5",
        DialectType::Trino,
    );
    assert!(sql.contains("NULLIF") && sql.contains("DOUBLE"), "{sql}");
}

/// A window runs after HAVING, so one in a measure can't be compared there.
#[test]
fn having_refuses_a_window_metric() {
    let model = common::MODEL.replace(
        "      - { name: gross_revenue, expression: { sql: SUM(revenue) } }\n",
        "      - { name: gross_revenue, expression: { sql: SUM(revenue) } }\n      - { name: running, expression: { sql: \"SUM(SUM(revenue)) OVER (ORDER BY status)\" } }\n",
    );
    assert_ne!(model, common::MODEL);
    let g = ossiec::from_yaml(&model).unwrap();
    let e = ossiec::compile_sql(
        &g,
        "SELECT status FROM orders GROUP BY status HAVING AGG(running) > 1",
        &duck(),
    )
    .expect_err("a window in HAVING")
    .to_string();
    assert!(e.contains("window metric"), "{e}");
}

/// §7.5: `ORDER BY` may contain `AGG()` and scalar expressions over it.
#[test]
fn order_by_reads_measures_that_are_not_selected() {
    let ordered = |sql: &str, reference: &str| {
        let g = ossiec::from_yaml(common::MODEL).unwrap();
        let db = duckdb();
        let c = ossiec::compile_sql(&g, sql, &duck()).unwrap_or_else(|e| panic!("{sql}: {e}"));
        let want = duckdb_rows(&db, reference);
        assert_eq!(duckdb_rows(&db, &c.sql), want, "{sql}\n{}", c.sql);
        assert!(want.len() > 1, "{sql}: proves nothing");
    };
    ordered(
        "SELECT status FROM orders GROUP BY status ORDER BY AGG(order_count) DESC",
        "SELECT status FROM orders GROUP BY status ORDER BY COUNT(*) DESC",
    );
    ordered(
        "SELECT status, AGG(order_count) AS n FROM orders GROUP BY status ORDER BY AGG(order_count) * -1",
        "SELECT status, COUNT(*) FROM orders GROUP BY status ORDER BY COUNT(*) * -1",
    );
    ordered(
        "SELECT status, AGG(order_count) / AGG(order_count) AS one FROM orders GROUP BY status ORDER BY AGG(order_count) DESC",
        "SELECT status, 1.0 FROM orders GROUP BY status ORDER BY COUNT(*) DESC",
    );
    // A second key, around a named one, in the order written.
    ordered(
        "SELECT status, AGG(order_count) AS n FROM orders GROUP BY status ORDER BY AGG(gross_revenue) * 0, status DESC",
        "SELECT status, COUNT(*) FROM orders GROUP BY status ORDER BY status DESC",
    );
    ordered(
        "SELECT status, AGG(order_count) AS n FROM orders GROUP BY status ORDER BY n DESC, LENGTH(status)",
        "SELECT status, COUNT(*) AS n FROM orders GROUP BY status ORDER BY n DESC, LENGTH(status)",
    );
    // A measure on the one side is de-duplicated before it sorts: EU has 150 units
    // from two products; summed over its joined rows it would be 250.
    ordered(
        "SELECT c.region FROM orders o \
         LEFT JOIN customers c ON o.customer_id = c.id \
         LEFT JOIN products p ON o.product_id = p.id \
         GROUP BY c.region ORDER BY AGG(p.stock_units) DESC NULLS LAST, c.region",
        "SELECT region FROM (
           SELECT g.region, s.stock FROM (SELECT DISTINCT c.region FROM orders o LEFT JOIN {customers} c ON o.customer_id = c.customer_id) g
           LEFT JOIN (SELECT region, SUM(stock) AS stock FROM (
             SELECT DISTINCT c.region, p.product_id, p.stock FROM orders o
             LEFT JOIN {customers} c ON o.customer_id = c.customer_id
             LEFT JOIN products p ON o.product_id = p.product_id
             WHERE p.product_id IS NOT NULL) GROUP BY region) s
           ON g.region IS NOT DISTINCT FROM s.region
         ) ORDER BY stock DESC NULLS LAST, region NULLS FIRST",
    );
    // The Top-N it's for: a derived table with only its join column selected.
    answers(
        "SELECT o.status, AGG(o.order_count) FROM orders o \
         JOIN (SELECT customer_id FROM orders GROUP BY customer_id ORDER BY AGG(order_count) DESC, customer_id LIMIT 2) t \
           ON o.customer_id = t.customer_id GROUP BY o.status",
        "SELECT o.status, COUNT(*) FROM orders o \
         JOIN (SELECT customer_id FROM orders GROUP BY customer_id ORDER BY COUNT(*) DESC, customer_id NULLS FIRST LIMIT 2) t \
           ON o.customer_id = t.customer_id GROUP BY o.status",
    );
    // Through a sole derived table.
    ordered(
        "SELECT status FROM (SELECT status, order_count AS n FROM orders) t GROUP BY status ORDER BY AGG(n) DESC",
        "SELECT status FROM orders GROUP BY status ORDER BY COUNT(*) DESC",
    );
    refuses(
        "SELECT status FROM orders GROUP BY status ORDER BY 1",
        "position",
    );
    refuses(
        "SELECT status FROM orders ORDER BY AGG(order_count)",
        "need GROUP BY",
    );
    refuses(
        "SELECT status FROM orders GROUP BY status ORDER BY AGG(nope)",
        "nope",
    );
    refuses(
        "SELECT status FROM orders GROUP BY status ORDER BY LENGTH(customer_id)",
        "not in GROUP BY",
    );
    refuses(
        "SELECT status FROM orders GROUP BY status ORDER BY SUM(id)",
        "plain aggregate",
    );
    refuses(
        "SELECT status FROM orders GROUP BY status ORDER BY (SELECT 1)",
        "subqueries",
    );
    let g = ossiec::from_yaml(common::MODEL).unwrap();
    let sql = "SELECT status FROM orders GROUP BY status ORDER BY AGG(order_count)";
    let e = ossiec::sql_to_request(&g, sql)
        .expect_err("no such key in a request")
        .to_string();
    assert!(e.contains("ORDER BY"), "{e}");
    let hash = |o: &str| {
        let sql = format!("SELECT status FROM orders GROUP BY status ORDER BY {o}");
        ossiec::compile_sql(&g, &sql, &duck()).unwrap().request_hash
    };
    assert_ne!(hash("AGG(order_count)"), hash("AGG(order_count) DESC"));
}

/// §7.4: `GROUP BY ALL` is the list of selected non-aggregates.
#[test]
fn group_by_all_groups_by_the_selected_fields() {
    let g = ossiec::from_yaml(common::MODEL).unwrap();
    let sql = |q: &str| ossiec::compile_sql(&g, q, &duck()).map(|c| c.sql);
    for (all, explicit) in [
        (
            "SELECT status, AGG(order_count) AS n FROM orders GROUP BY ALL",
            "SELECT status, AGG(order_count) AS n FROM orders GROUP BY status",
        ),
        (
            "SELECT status AS s, customer_id, AGG(order_count) FROM orders GROUP BY ALL",
            "SELECT status AS s, customer_id, AGG(order_count) FROM orders GROUP BY status, customer_id",
        ),
        // Nothing to group by: a grand total.
        (
            "SELECT AGG(order_count) FROM orders GROUP BY ALL",
            "SELECT AGG(order_count) FROM orders",
        ),
        // A listing of fields.
        (
            "SELECT status FROM orders GROUP BY ALL",
            "SELECT status FROM orders GROUP BY status",
        ),
        // In a derived table, a CTE and an outer query.
        (
            "SELECT status, AGG(order_count) FROM (SELECT status FROM orders GROUP BY ALL) t JOIN orders o ON o.status = t.status GROUP BY ALL",
            "SELECT status, AGG(order_count) FROM (SELECT status FROM orders GROUP BY status) t JOIN orders o ON o.status = t.status GROUP BY status",
        ),
        (
            "WITH t AS (SELECT status, AGG(order_count) AS n FROM orders GROUP BY ALL) SELECT status, SUM(n) FROM t GROUP BY ALL",
            "WITH t AS (SELECT status, AGG(order_count) AS n FROM orders GROUP BY status) SELECT status, SUM(n) FROM t GROUP BY status",
        ),
    ] {
        match (sql(all), sql(explicit)) {
            (Ok(a), Ok(e)) => assert_eq!(a, e, "{all}"),
            (Err(a), Err(e)) => assert_eq!(a.to_string(), e.to_string(), "{all}"),
            (a, e) => panic!("{all}: {a:?} but {explicit}: {e:?}"),
        }
    }
    // A definition isn't grouped by, so a grouped one is refused as its explicit
    // form is (§7.2), and a later CTE's body expands too.
    let joined = |group: &str| {
        format!("SELECT o.status, AGG(t.n) FROM orders o JOIN (SELECT customer_id, COUNT(*) TO MEASURE AS n FROM orders GROUP BY {group}) t ON o.customer_id = t.customer_id GROUP BY o.status")
    };
    let view = |group: &str| {
        let q = format!("CREATE VIEW v AS SELECT status, COUNT(*) TO MEASURE AS n FROM orders GROUP BY {group}; SELECT status, AGG(n) FROM v GROUP BY status");
        ossiec::compile_sql_script(&g, &q, &duck()).map(|c| c.sql)
    };
    for (all, explicit) in [
        (sql(&joined("ALL")), sql(&joined("customer_id"))),
        (view("ALL"), view("status")),
    ] {
        let (all, explicit) = (
            all.unwrap_err().to_string(),
            explicit.unwrap_err().to_string(),
        );
        assert!(
            all.contains("§7.2") && explicit.contains("§7.2"),
            "{all}\n{explicit}"
        );
    }
    assert_eq!(
        sql("SELECT status, AGG(n) FROM (SELECT status, COUNT(*) TO MEASURE AS n FROM orders) t GROUP BY ALL").unwrap(),
        sql("SELECT status, AGG(n) FROM (SELECT status, COUNT(*) TO MEASURE AS n FROM orders) t GROUP BY status").unwrap(),
    );
    assert_eq!(
        sql("WITH a AS (SELECT status FROM orders GROUP BY status), b AS (SELECT status FROM a GROUP BY ALL) SELECT status FROM b").unwrap(),
        sql("WITH a AS (SELECT status FROM orders GROUP BY status), b AS (SELECT status FROM a GROUP BY status) SELECT status FROM b").unwrap(),
    );
    // A literal isn't a column to group by, and is refused as it is without ALL.
    assert!(sql("SELECT 1, AGG(order_count) FROM orders GROUP BY ALL").is_err());
    // §7.4: a bare measure has nothing to group by.
    let e =
        sql("SELECT status, x FROM (SELECT status, order_count AS x FROM orders GROUP BY ALL) t")
            .unwrap_err()
            .to_string();
    assert!(e.contains("cannot name measure"), "{e}");
    let e = sql("SELECT status, order_count FROM orders GROUP BY ALL")
        .unwrap_err()
        .to_string();
    assert!(e.contains("measure"), "{e}");
    let e = sql("SELECT * FROM orders GROUP BY ALL")
        .unwrap_err()
        .to_string();
    assert!(e.contains("not `*`"), "{e}");
    let e = sql("SELECT status, AGG(order_count) FROM orders GROUP BY ALL status")
        .map(|_| String::new())
        .unwrap_or_else(|e| e.to_string());
    assert!(e.contains("GROUP BY") || e.contains("Parse"), "{e}");
}

/// §7.8: `DISTINCT` is valid over fields and evaluated measures.
#[test]
fn distinct_keeps_one_of_each_row() {
    let g = ossiec::from_yaml(common::MODEL).unwrap();
    let db = common::duckdb();
    let rows = |q: &str| {
        let c = ossiec::compile_sql(&g, q, &duck()).unwrap_or_else(|e| panic!("{q}: {e}"));
        let mut r = common::duckdb_rows(&db, &c.sql);
        r.sort();
        r
    };
    let reference = |q: &str| {
        let mut r = common::duckdb_rows(&db, q);
        r.sort();
        r
    };
    assert_eq!(
        rows("SELECT DISTINCT status FROM orders"),
        reference("SELECT DISTINCT status FROM orders")
    );
    assert_eq!(
        rows("SELECT DISTINCT status, AGG(order_count) AS n FROM orders GROUP BY status"),
        reference("SELECT status, COUNT(*) FROM orders GROUP BY status")
    );
    // An outer query's rows can repeat, so it needs the real thing.
    let outer = |d: &str| {
        format!("SELECT {d}t.status FROM (SELECT status, customer_id FROM orders GROUP BY status, customer_id) t")
    };
    let all = rows(&outer(""));
    let one_each = rows(&outer("DISTINCT "));
    assert!(all.len() > one_each.len(), "{all:?}");
    assert_eq!(one_each, reference("SELECT DISTINCT status FROM orders"));
    // In a derived table, a CTE, and through an outer aggregate.
    assert_eq!(
        rows("SELECT COUNT(*) FROM (SELECT DISTINCT status FROM orders) t"),
        reference("SELECT COUNT(*) FROM (SELECT DISTINCT status FROM orders) t")
    );
    assert_eq!(
        rows("WITH s AS (SELECT DISTINCT status FROM orders) SELECT status FROM s"),
        reference("SELECT DISTINCT status FROM orders")
    );
    // Joined in, without repeating the rows it restricts.
    assert_eq!(
        rows("SELECT o.status, AGG(o.order_count) FROM orders o JOIN (SELECT DISTINCT customer_id FROM orders) t ON o.customer_id = t.customer_id GROUP BY o.status"),
        reference("SELECT status, COUNT(*) FROM orders WHERE customer_id IS NOT NULL GROUP BY status")
    );
    // A sort by what it selects.
    let c = ossiec::compile_sql(
        &g,
        "SELECT DISTINCT status FROM orders ORDER BY status DESC LIMIT 1",
        &duck(),
    )
    .unwrap();
    assert_eq!(
        common::duckdb_rows(&db, &c.sql),
        reference("SELECT MAX(status) FROM orders")
    );
}

/// §7.8: an unevaluated measure has no value to compare.
#[test]
fn distinct_refuses_unevaluated_measures() {
    let g = ossiec::from_yaml(common::MODEL).unwrap();
    let refused = |q: &str, why: &str| {
        let e = ossiec::compile_sql(&g, q, &duck())
            .map(|c| c.sql)
            .expect_err(q)
            .to_string();
        assert!(e.contains(why), "{q}: {e}");
    };
    // Passed through a derived table, as a measure or a new one.
    refused(
        "SELECT status, AGG(x) FROM (SELECT DISTINCT status, order_count AS x FROM orders) t GROUP BY status",
        "DISTINCT",
    );
    refused(
        "SELECT o.status, AGG(o.order_count) FROM orders o JOIN (SELECT DISTINCT status, order_count AS x FROM orders) t ON o.status = t.status GROUP BY o.status",
        "DISTINCT can't compare unevaluated measure `x`",
    );
    refused(
        "SELECT status, AGG(x) FROM (SELECT DISTINCT status, COUNT(*) TO MEASURE AS x FROM orders) t GROUP BY status",
        "DISTINCT",
    );
    // Read by an outer query.
    refused(
        "SELECT DISTINCT t.x FROM (SELECT status, order_count AS x FROM orders) t",
        "unevaluated measure",
    );
    // At the top, where it was never allowed.
    refused(
        "SELECT DISTINCT status, order_count FROM orders",
        "bare measure",
    );
    // A sort by what it doesn't select, which SQL refuses too.
    refused(
        "SELECT DISTINCT status FROM orders GROUP BY status ORDER BY AGG(order_count)",
        "only sort by columns it selects",
    );
    // What isn't standard SQL stays refused.
    refused(
        "SELECT DISTINCT ON (status) status FROM orders",
        "DISTINCT ON",
    );
    refused(
        "SELECT DISTINCT ON (t.status) t.status FROM (SELECT status FROM orders GROUP BY status) t",
        "DISTINCT ON",
    );
    refused(
        "SELECT DISTINCT ON (status) status, AGG(x) FROM (SELECT status, order_count AS x FROM orders) t GROUP BY status",
        "DISTINCT ON",
    );
}

/// §7.2: an aggregated query reports values, so it can't carry an unevaluated
/// measure out, whether passed through or defined there. §4.2: a measure definition
/// doesn't aggregate, so Appendix A.7's ungrouped view is the form that works.
#[test]
fn aggregated_queries_refuse_unevaluated_measures() {
    let g = ossiec::from_yaml(common::MODEL).unwrap();
    let sql = |q: &str| {
        if q.contains("CREATE VIEW") {
            ossiec::compile_sql_script(&g, q, &duck())
        } else {
            ossiec::compile_sql(&g, q, &duck())
        }
    };
    for (grouped, ungrouped) in [
        // A measure passed through a joined derived table.
        (
            "SELECT o.status, AGG(t.order_count) FROM orders o JOIN (SELECT customer_id, order_count FROM orders GROUP BY customer_id) t ON o.customer_id = t.customer_id GROUP BY o.status",
            "SELECT o.status, AGG(t.order_count) FROM orders o JOIN (SELECT customer_id, order_count FROM orders) t ON o.customer_id = t.customer_id GROUP BY o.status",
        ),
        // ... through the sole derived table.
        (
            "SELECT status, AGG(m) FROM (SELECT status, order_count AS m FROM orders GROUP BY status) t GROUP BY status",
            "SELECT status, AGG(m) FROM (SELECT status, order_count AS m FROM orders) t GROUP BY status",
        ),
        // A base measure defined in one.
        (
            "SELECT status, AGG(n) FROM (SELECT status, COUNT(*) TO MEASURE AS n FROM orders GROUP BY status) t GROUP BY status",
            "SELECT status, AGG(n) FROM (SELECT status, COUNT(*) TO MEASURE AS n FROM orders) t GROUP BY status",
        ),
        // A derived measure, in a joined derived table and in a view.
        (
            "SELECT c.region, AGG(m.x) FROM customers c JOIN (SELECT o.customer_id, (AGG(o.order_count) * 2) TO MEASURE AS x FROM orders o GROUP BY o.customer_id) m ON c.id = m.customer_id GROUP BY c.region",
            "SELECT c.region, AGG(m.x) FROM customers c JOIN (SELECT o.customer_id, (AGG(o.order_count) * 2) TO MEASURE AS x FROM orders o) m ON c.id = m.customer_id GROUP BY c.region",
        ),
        (
            "CREATE VIEW v AS SELECT c.region, (AGG(o.order_count) * 2) TO MEASURE AS x FROM customers c LEFT JOIN orders o ON o.customer_id = c.id GROUP BY c.region; SELECT region, AGG(x) FROM v GROUP BY region",
            "CREATE VIEW v AS SELECT c.region, (AGG(o.order_count) * 2) TO MEASURE AS x FROM customers c LEFT JOIN orders o ON o.customer_id = c.id; SELECT region, AGG(x) FROM v GROUP BY region",
        ),
        // HAVING makes a query aggregated too.
        (
            "SELECT status, AGG(n) FROM (SELECT status, COUNT(*) TO MEASURE AS n FROM orders GROUP BY status HAVING AGG(order_count) > 1) t GROUP BY status",
            "SELECT status, AGG(n) FROM (SELECT status, COUNT(*) TO MEASURE AS n FROM orders) t GROUP BY status",
        ),
    ] {
        let e = sql(grouped).map(|c| c.sql).expect_err(grouped).to_string();
        assert!(e.contains("§7.2"), "{grouped}: {e}");
        sql(ungrouped).unwrap_or_else(|e| panic!("{ungrouped}: {e}"));
    }
    // Without GROUP BY a query still aggregates through HAVING, or an aggregate of
    // its own in the select list.
    for q in [
        "SELECT AGG(n) FROM (SELECT COUNT(*) TO MEASURE AS n FROM orders HAVING AGG(order_count) > 1) t",
        "SELECT AGG(n) FROM (SELECT AGG(order_count) AS a, COUNT(*) TO MEASURE AS n FROM orders) t",
    ] {
        let e = sql(q).map(|c| c.sql).expect_err(q).to_string();
        assert!(e.contains("§7.2"), "{q}: {e}");
    }
    sql("SELECT AGG(n) FROM (SELECT COUNT(*) TO MEASURE AS n FROM orders) t").unwrap();
    // What a definition reads with AGG() doesn't make its query aggregated, but an
    // AGG() of its own beside it does.
    let e = sql("SELECT status, x FROM (SELECT status, AGG(order_count) AS n, (AGG(order_count) * 2) TO MEASURE AS x FROM orders GROUP BY status) t")
        .map(|c| c.sql)
        .unwrap_err()
        .to_string();
    assert!(e.contains("§7.2") || e.contains("measure"), "{e}");
}

/// Appendix A.7, as the spec writes it: the view has no `GROUP BY`, and gives the
/// answer of the same measure in a plain query. Regions only: the NULL region also
/// holds orders without a customer, which `left_join_from_the_one_side...` covers.
#[test]
fn appendix_a7_ungrouped_view_answers() {
    let g = ossiec::from_yaml(common::MODEL).unwrap();
    let db = duckdb();
    let q = "CREATE VIEW v AS SELECT c.region, (AGG(o.order_count) * 2) TO MEASURE AS x \
             FROM customers c LEFT JOIN orders o ON o.customer_id = c.id; \
             SELECT region, AGG(x) FROM v WHERE region IS NOT NULL GROUP BY region";
    let c = ossiec::compile_sql_script(&g, q, &duck()).unwrap();
    let reference = "SELECT c.region, 2 * COUNT(o.order_id) FROM {customers} c \
                     LEFT JOIN orders o ON o.customer_id = c.customer_id \
                     WHERE c.region IS NOT NULL GROUP BY c.region";
    assert_eq!(
        sorted(duckdb_rows(&db, &c.sql)),
        sorted(duckdb_rows(&db, reference))
    );
}

/// Two-facts `orders` has no primary key, so null-extended customers are kept out
/// of `SUM` by a FILTER on the join key rather than by de-duplication. `tickets`
/// has one, and its `COUNT(*)` must not count a customer without tickets.
#[test]
fn join_from_either_side_without_a_fact_key() {
    let g = two_facts_graph();
    let db = two_facts_db();
    let joins = [
        "customers c LEFT JOIN {f}",
        "{f} RIGHT JOIN customers c",
        "customers c RIGHT JOIN {f}",
        "{f} LEFT JOIN customers c",
        "customers c JOIN {f}",
        "customers c FULL JOIN {f}",
    ];
    let facts = [
        ("orders o", "AGG(o.order_amt)", "SUM(o.amount)", "o"),
        ("tickets t", "AGG(t.ticket_count)", "COUNT(t.id)", "t"),
    ];
    for ((fact, agg, plain, a), join) in
        facts.iter().flat_map(|f| joins.iter().map(move |j| (f, j)))
    {
        let join = join.replace("{f}", fact);
        let on = format!("ON {a}.customer_id = c.id GROUP BY c.region");
        let reference = format!("SELECT c.region, {plain} FROM {join} {on}");
        let q = format!("SELECT c.region, {agg} FROM {join} {on}");
        let compiled = ossiec::compile_layer2(&g, &q, &two_facts_opts());
        if join == "customers c FULL JOIN orders o" {
            // An order whose `customer_id` is NULL is a real row of a FULL join,
            // so a filter on the key can't stand in for the missing primary key.
            let e = compiled.expect_err("FULL JOIN without a key compiled");
            assert!(
                matches!(e, Error::Unsupported(ref m) if m.contains("primary_key")),
                "{e}"
            );
            continue;
        }
        let c = compiled.unwrap_or_else(|e| panic!("{join}: {e}"));
        assert_eq!(
            sorted(duckdb_rows(&db, &c.sql)),
            sorted(duckdb_rows(&db, &reference)),
            "{join}\n{}",
            c.sql
        );
    }
}

/// A composite metric is planned as one aggregate per term, and each takes the
/// FILTER: `COUNT(*)` counts no orders for a customer that has none, so its
/// ratio is NULL rather than divided by the one null-extended row.
#[test]
fn join_from_the_one_side_filters_each_term_of_a_composite_metric() {
    let model = TWO_FACTS.replace(
        "      - { name: order_amt, expression: { sql: SUM(amount) } }",
        "      - { name: order_amt, expression: { sql: SUM(amount) } }\n      \
         - { name: amt_per_order, expression: { sql: SUM(amount) / COUNT(*) } }",
    );
    assert_ne!(model, TWO_FACTS);
    let g = ossiec::from_yaml(&model).unwrap();
    let q = "SELECT c.region, AGG(o.amt_per_order) FROM customers c \
             LEFT JOIN orders o ON o.customer_id = c.id GROUP BY c.region";
    let c = ossiec::compile_layer2(&g, q, &two_facts_opts()).unwrap();
    assert!(c.sql.contains("FILTER"), "{}", c.sql);
    let db = two_facts_db();
    let got = rows_f64(duckdb_rows(&db, &c.sql));
    let want = rows_f64(duckdb_rows(
        &db,
        "SELECT c.region, SUM(o.amount) * 1.0 / NULLIF(COUNT(o.customer_id), 0) \
         FROM customers c LEFT JOIN orders o ON o.customer_id = c.id GROUP BY c.region",
    ));
    assert_eq!(got.len(), want.len(), "{got:?} vs {want:?}");
    for (g, w) in got.iter().zip(&want) {
        assert_eq!(g.0, w.0);
        assert!((g.1 - w.1).abs() < 1e-9, "{got:?} vs {want:?}");
    }
    assert!(got.contains(&("APAC".to_string(), -1.0)), "{got:?}");
}

/// Aggregates of every shape take the FILTER, ANDed with a FILTER of their own,
/// so a fact without a primary key needs no de-duplication for any of them.
#[test]
fn join_from_the_one_side_filters_every_aggregate_shape() {
    let metrics = [
        (
            "filtered",
            "SUM(amount) FILTER (WHERE amount > 10)",
            "SUM(o.amount) FILTER (WHERE o.amount > 10)",
        ),
        (
            "median",
            "PERCENTILE_CONT(0.5) WITHIN GROUP (ORDER BY amount)",
            "PERCENTILE_CONT(0.5) WITHIN GROUP (ORDER BY o.amount)",
        ),
        (
            "big_median",
            "PERCENTILE_CONT(0.5) WITHIN GROUP (ORDER BY amount) FILTER (WHERE amount > 10)",
            "PERCENTILE_CONT(0.5) WITHIN GROUP (ORDER BY o.amount) FILTER (WHERE o.amount > 10)",
        ),
        (
            "biggest",
            "ARG_MAX(amount, amount)",
            "ARG_MAX(o.amount, o.amount)",
        ),
    ];
    for (name, metric, plain) in metrics {
        let model = TWO_FACTS.replace(
            "      - { name: order_amt, expression: { sql: SUM(amount) } }",
            &format!(
                "      - {{ name: order_amt, expression: {{ sql: SUM(amount) }} }}\n      \
                 - {{ name: {name}, expression: {{ sql: \"{metric}\" }} }}"
            ),
        );
        let g = ossiec::from_yaml(&model).unwrap();
        let q = format!(
            "SELECT c.region, AGG(o.{name}) FROM customers c \
             LEFT JOIN orders o ON o.customer_id = c.id GROUP BY c.region"
        );
        let c = ossiec::compile_layer2(&g, &q, &two_facts_opts())
            .unwrap_or_else(|e| panic!("{name}: {e}"));
        let db = two_facts_db();
        assert_eq!(
            sorted(duckdb_rows(&db, &c.sql)),
            sorted(duckdb_rows(
                &db,
                &format!(
                    "SELECT c.region, {plain} FROM customers c \
                     LEFT JOIN orders o ON o.customer_id = c.id GROUP BY c.region"
                )
            )),
            "{name}\n{}",
            c.sql
        );
    }
}

/// orders -> customers -> regions, `orders` without a primary key. A RIGHT join
/// below the root null-extends the whole chain above it, so the filter on that
/// join's own key keeps those rows out too.
#[test]
fn right_joins_along_a_chain_without_a_fact_key() {
    let g = ossiec::from_yaml(
        r#"
version: "0.2.0.dev0"
name: chain
datasets:
  - name: orders
    source: orders
    fields:
      - { name: customer_id, expression: { sql: customer_id } }
      - { name: amount, expression: { sql: amount } }
    metrics:
      - { name: amt, expression: { sql: SUM(amount) } }
      - { name: n, expression: { sql: COUNT(*) } }
  - name: customers
    source: customers
    fields:
      - { name: id, expression: { sql: id } }
      - { name: region_id, expression: { sql: region_id } }
  - name: regions
    source: regions
    fields:
      - { name: id, expression: { sql: id } }
      - { name: name, expression: { sql: name }, dimension: {} }
relationships:
  - { name: o_c, from: orders, to: customers, from_columns: [customer_id], to_columns: [id] }
  - { name: c_r, from: customers, to: regions, from_columns: [region_id], to_columns: [id] }
"#,
    )
    .unwrap();
    let db = duckdb::Connection::open_in_memory().unwrap();
    db.execute_batch(
        "CREATE TABLE regions (id INTEGER, name VARCHAR);
         INSERT INTO regions VALUES (1, 'EU'), (2, 'US'), (3, 'APAC');
         CREATE TABLE customers (id INTEGER, region_id INTEGER);
         INSERT INTO customers VALUES (10, 1), (11, 1), (12, 2), (13, NULL);
         CREATE TABLE orders (customer_id INTEGER, amount INTEGER);
         INSERT INTO orders VALUES (10, 5), (10, 7), (12, 3), (13, 4), (NULL, 9), (99, 1);",
    )
    .unwrap();
    for joins in [
        "regions r LEFT JOIN customers c ON c.region_id = r.id \
         LEFT JOIN orders o ON o.customer_id = c.id",
        "orders o LEFT JOIN customers c ON o.customer_id = c.id \
         RIGHT JOIN regions r ON c.region_id = r.id",
        "orders o RIGHT JOIN customers c ON o.customer_id = c.id \
         RIGHT JOIN regions r ON c.region_id = r.id",
    ] {
        let q = format!("SELECT r.name, AGG(o.amt), AGG(o.n) FROM {joins} GROUP BY r.name");
        let c = ossiec::compile_layer2(&g, &q, &duck()).unwrap_or_else(|e| panic!("{joins}: {e}"));
        let want =
            format!("SELECT r.name, SUM(o.amount), COUNT(o.amount) FROM {joins} GROUP BY r.name");
        assert_eq!(
            sorted(duckdb_rows(&db, &c.sql)),
            sorted(duckdb_rows(&db, &want)),
            "{joins}\n{}",
            c.sql
        );
    }
}

/// §7.6, §8.1: `customers LEFT JOIN orders` keeps every customer and the orders that
/// match one. An order with no customer isn't in that join, so `AGG(order_count)`
/// mustn't count it (it once did, under the NULL region: 3 instead of 1). The plan
/// roots at `orders`, so the join it writes runs the other way: LEFT and RIGHT
/// trade places, and a FULL join stays FULL.
#[test]
fn left_join_from_the_one_side_drops_unmatched_fact_rows() {
    let g = ossiec::from_yaml(common::MODEL).unwrap();
    let db = duckdb();
    let every_customer = "SELECT c.region, COUNT(o.order_id) FROM {customers} c \
                          LEFT JOIN orders o ON o.customer_id = c.customer_id GROUP BY c.region";
    let every_order = "SELECT c.region, COUNT(o.order_id) FROM orders o \
                       LEFT JOIN {customers} c ON o.customer_id = c.customer_id GROUP BY c.region";
    let both = "SELECT c.region, COUNT(o.order_id) FROM orders o \
                FULL JOIN {customers} c ON o.customer_id = c.customer_id GROUP BY c.region";
    for (join, reference) in [
        ("customers c LEFT JOIN orders o", every_customer),
        ("orders o RIGHT JOIN customers c", every_customer),
        ("customers c RIGHT JOIN orders o", every_order),
        ("orders o LEFT JOIN customers c", every_order),
        ("customers c FULL JOIN orders o", both),
        ("orders o FULL JOIN customers c", both),
    ] {
        let q = format!(
            "SELECT c.region, AGG(o.order_count) FROM {join} \
             ON o.customer_id = c.id GROUP BY c.region"
        );
        let c = ossiec::compile_sql(&g, &q, &duck()).unwrap();
        assert_eq!(
            sorted(duckdb_rows(&db, &c.sql)),
            sorted(duckdb_rows(&db, reference)),
            "{join}\n{}",
            c.sql
        );
    }
    // The two answers differ: the guard above is not vacuous.
    assert_ne!(
        sorted(duckdb_rows(&db, every_customer)),
        sorted(duckdb_rows(&db, every_order))
    );
}
