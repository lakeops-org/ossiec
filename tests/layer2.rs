//! Ossie Layer 2 `AGG(measure)` frontend (Appendix A.1–A.6, §7.6 join kinds,
//! role-playing dimensions, includeUnmatched).

mod common;

use common::{
    duckdb, duckdb_rows, A2_SHOP, A2_SHOP_REF, A3_SHOP, A3_SHOP_REF, LAYER2_NESTED_CASES,
};
use ossiec::{DialectType, Error, Options};

const SHOP: &str = include_str!("fixtures/shop/model.yaml");
const TWO_FACTS: &str = include_str!("fixtures/two_facts_layer2.yaml");
const FANOUT: &str = include_str!("fixtures/fanout_layer2.yaml");

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
fn unaliased_output_names_are_unchanged() {
    let c = ossiec::compile_layer2(
        &graph(),
        "SELECT status, AGG(order_count) FROM orders GROUP BY status",
        &duck(),
    )
    .unwrap();
    assert!(c.sql.contains("\"orders.status\""), "{}", c.sql);
    assert!(c.sql.contains("\"orders.order_count\""), "{}", c.sql);
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
        JOIN (SELECT customer_id, order_count FROM orders GROUP BY customer_id) t \
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
    // The engine tier's shop cases (A.2, A.3, §7.7), checked here without Docker.
    let db = duckdb();
    for (name, layer2, reference) in LAYER2_NESTED_CASES {
        let c = ossiec::compile_layer2(&graph(), layer2, &duck()).unwrap();
        assert_eq!(
            sorted(duckdb_rows(&db, &c.sql)),
            sorted(duckdb_rows(&db, reference)),
            "{name}: {}",
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
fn a2_refuses_set_operator_and_distinct_inside_derived_table() {
    for inner in [
        "SELECT customer_id FROM orders UNION SELECT customer_id FROM orders",
        "SELECT DISTINCT customer_id FROM orders",
    ] {
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
            SELECT c.region, NULL, COUNT(*)
            FROM tickets t
            LEFT JOIN customers c ON t.customer_id = c.id
            GROUP BY c.region
         ) GROUP BY region",
    ));
    assert_eq!(got, want, "{}", c.sql);
    // EU 60 / 3 tickets; US and APAC keep their tickets and contribute no orders.
    let row = |region: &str, amt: &str, tickets: &str| -> Vec<String> {
        vec![region.into(), amt.into(), tickets.into()]
    };
    assert_eq!(
        got,
        vec![
            row("APAC", "NULL", "1.000000"),
            row("EU", "60.000000", "3.000000"),
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

const A2_OSSIE: &str = "SELECT s.category, AGG(s.total_revenue) AS revenue \
    FROM sales s \
    JOIN (SELECT state_id, AGG(total_revenue) AS state_revenue \
          FROM sales GROUP BY state_id \
          ORDER BY state_revenue DESC, state_id LIMIT 2) t \
      ON s.state_id = t.state_id \
    GROUP BY s.category";

#[test]
fn a2_ossie_names_execute() {
    let g = nested_graph();
    let c = ossiec::compile_layer2(&g, A2_OSSIE, &duck()).unwrap();
    let db = nested_db();
    let got = sorted(duckdb_rows(&db, &c.sql));
    let want = sorted(duckdb_rows(
        &db,
        "SELECT category, SUM(amount) FROM sales WHERE state_id IN (3, 2) GROUP BY category",
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

const A3_OSSIE: &str = "SELECT d.category, AVG(m.state_revenue) \
    FROM (SELECT category, state_id FROM sales GROUP BY category, state_id) d \
    JOIN (SELECT state_id, AGG(total_revenue) AS state_revenue FROM sales GROUP BY state_id) m \
      ON d.state_id IS NOT DISTINCT FROM m.state_id \
    GROUP BY d.category";

#[test]
fn a3_ossie_names_execute_with_null_safe_keys() {
    let g = nested_graph();
    let c = ossiec::compile_layer2(&g, A3_OSSIE, &duck()).unwrap();
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
    let c = ossiec::compile_layer2(&g, &A3_OSSIE.replace("IS NOT DISTINCT FROM", "="), &duck())
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
    assert!(my.sql.contains("utf8mb4_bin"), "{}", my.sql);
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
    LEFT JOIN tickets t ON t.customer_id = c.id \
    GROUP BY c.region";

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
fn s77_from_only_with_inner_group_by_and_renames() {
    let sql = "SELECT s AS st, AGG(m) \
        FROM (SELECT status AS s, order_count AS m FROM orders GROUP BY status) t \
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
          GROUP BY c.region \
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
              WHERE c.region = 'EU' \
              GROUP BY c.region) m ON r.region = m.region \
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
              LEFT JOIN tickets t ON t.customer_id = c.id \
              GROUP BY c.region) m \
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
         LEFT JOIN orders o ON o.customer_id = c.id LEFT JOIN tickets t ON t.customer_id = c.id \
         GROUP BY c.region) m ON r.region = m.region GROUP BY r.region",
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
        matches!(e, Error::Request(ref m) if m.contains("GROUP BY cannot name measure")),
        "{e}"
    );
    // A.3 shell: plain SQL cannot read an unevaluated column.
    let m = unsupported_msg(
        ossiec::compile_layer2(
            &graph(),
            "SELECT AVG(m.order_count) FROM (SELECT customer_id, order_count FROM orders GROUP BY customer_id) m",
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
              FROM customers c LEFT JOIN orders o ON o.customer_id = c.id \
              GROUP BY c.region) m ON r.region = m.region \
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
              FROM customers c LEFT JOIN orders o ON o.customer_id = c.id \
              GROUP BY c.id) m ON r.id = m.id \
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
              WHERE customer_id IS NOT NULL GROUP BY status) t \
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
              FROM orders GROUP BY status) m \
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
         LEFT JOIN orders o ON o.customer_id = c.id GROUP BY c.region",
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
        JOIN (SELECT customer_id, order_count FROM orders GROUP BY customer_id) t \
          ON o.customer_id = t.customer_id GROUP BY o.status";
    let fold = "SELECT status, AGG(order_count) AS n \
        FROM (SELECT status, order_count FROM orders WHERE customer_id IS NOT NULL) t \
        GROUP BY status";
    let tm = format!(
        "SELECT r.region, AGG(m.sales_per_ticket) AS s FROM customers r \
         JOIN ({SALES_PER_TICKET}) m ON r.region = m.region GROUP BY r.region"
    );
    let base = "SELECT status, AGG(n) AS n \
        FROM (SELECT status, COUNT(*) TO MEASURE AS n FROM orders GROUP BY status) t \
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
    assert!(
        matches!(e, Error::Request(ref m) if m.contains("GROUP BY cannot name measure")),
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
        JOIN (SELECT customer_id, order_amt AS x FROM orders GROUP BY customer_id) m
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
        "CREATE TABLE airports (code VARCHAR, city VARCHAR);
         INSERT INTO airports VALUES ('JFK', 'New York'), ('LHR', 'London'), ('CDG', 'Paris');
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
    let sql = "SELECT departs_from.city, arrives_at.city, AGG(f.total_delay) \
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
fn include_unmatched_matches_layer3_and_duckdb() {
    let g = stock_graph();
    let l2 = ossiec::compile_layer2(&g, stock_sql(), &unmatched_opts()).unwrap();
    let layer3 = ossiec::compile(
        &g,
        &serde_json::from_str(
            r#"{"metrics":["products.stock_units"],"dimensions":["orders.region"],"includeUnmatched":true}"#,
        )
        .unwrap(),
        &duck(),
    )
    .unwrap();
    assert_eq!(l2.sql, layer3.sql);
    let req =
        ossiec::layer2_to_request(&g, &format!("-- ossiec: includeUnmatched\n{}", stock_sql()))
            .unwrap();
    assert!(req.include_unmatched);
    assert_eq!(req.dimensions, vec!["orders.region".to_string()]);
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
