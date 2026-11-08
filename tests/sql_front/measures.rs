//! Measures through queries: §7.2 aggregated queries and plain aggregates, §7.7
//! pass-through, query-local `TO MEASURE`, and the fold of a sole derived table.

use super::*;

fn base_measure_rows(sql: &str) -> Vec<(String, f64)> {
    let c = ossiec::compile_layer2(&two_facts_graph(), sql, &two_facts_opts()).unwrap();
    assert!(!c.sql.contains("__l2_tm"), "{}", c.sql);
    rows_f64(duckdb_rows(&two_facts_db(), &c.sql))
}

/// §7.2: an ordinary aggregate over fields beside measures. It is plain SQL over
/// the query's joined rows: repeats and NULL-extended rows count, while each
/// measure still counts its own source rows once.
#[test]
fn s72_plain_aggregates_beside_measures() {
    let g = graph();
    let db = duckdb();
    for (q, reference) in [
        (
            "SELECT status, MAX(order_date) AS last, AGG(order_count) AS n \
             FROM orders GROUP BY status",
            "SELECT status, MAX(order_date), COUNT(*) FROM orders GROUP BY status",
        ),
        (
            "SELECT status, MAX(order_date) FROM orders GROUP BY status",
            "SELECT status, MAX(order_date) FROM orders GROUP BY status",
        ),
        (
            // Every order, one row each: COUNT(*) and the measure agree.
            "SELECT c.region, COUNT(*) AS rows_, AGG(o.order_count) AS n \
             FROM orders o LEFT JOIN customers c ON o.customer_id = c.id GROUP BY c.region",
            "SELECT c.region, COUNT(*), COUNT(*) FROM orders o \
             LEFT JOIN {customers} c ON o.customer_id = c.customer_id GROUP BY c.region",
        ),
        (
            // A customer without orders is one NULL-extended row: COUNT(*) counts
            // it, COUNT(o.id) and the measure don't.
            "SELECT c.region, COUNT(*) AS rows_, COUNT(o.id) AS ids, AGG(o.order_count) AS n \
             FROM customers c LEFT JOIN orders o ON o.customer_id = c.id GROUP BY c.region",
            "SELECT c.region, COUNT(*), COUNT(o.order_id), COUNT(o.order_id) \
             FROM {customers} c LEFT JOIN orders o ON o.customer_id = c.customer_id \
             GROUP BY c.region",
        ),
        (
            // FULL: the measure is de-duplicated by primary key; COUNT(*) still
            // counts every joined row.
            "SELECT c.region, COUNT(*) AS rows_, AGG(o.order_count) AS n \
             FROM customers c FULL JOIN orders o ON o.customer_id = c.id GROUP BY c.region",
            "SELECT c.region, COUNT(*), COUNT(o.order_id) \
             FROM {customers} c FULL JOIN orders o ON o.customer_id = c.customer_id \
             GROUP BY c.region",
        ),
        (
            // A product's stock repeats once per order in plain SQL; the measure
            // counts each product once (G2).
            "SELECT o.status, SUM(p.stock) AS repeated, AGG(p.stock_units) AS once \
             FROM orders o JOIN products p ON o.product_id = p.id GROUP BY o.status",
            "SELECT o.status, SUM(p.stock), \
                    (SELECT SUM(stock) FROM products x WHERE x.product_id IN \
                       (SELECT product_id FROM orders y WHERE y.status = o.status)) \
             FROM orders o JOIN products p ON o.product_id = p.product_id GROUP BY o.status",
        ),
    ] {
        let c = ossiec::compile_layer2(&g, q, &duck()).unwrap_or_else(|e| panic!("{q}: {e}"));
        assert_eq!(
            sorted(duckdb_rows(&db, &c.sql)),
            sorted(duckdb_rows(&db, reference)),
            "{q}\n{}",
            c.sql
        );
    }
}

/// A fan-out: COUNT(*) counts the joined lines, the measure counts orders once.
#[test]
fn s72_plain_count_over_a_fanout_counts_lines() {
    let g = ossiec::from_yaml(FANOUT).unwrap();
    let db = duckdb::Connection::open_in_memory().unwrap();
    db.execute_batch(
        "CREATE TABLE orders (order_id INTEGER);
         INSERT INTO orders VALUES (1), (2), (3);
         CREATE TABLE order_items (line_id INTEGER, order_id INTEGER);
         INSERT INTO order_items VALUES (1, 1), (2, 1), (3, 2), (4, 9);",
    )
    .unwrap();
    let q = "SELECT COUNT(*) AS lines, AGG(o.order_count) AS orders_ FROM orders o \
             JOIN order_items i ON i.order_id = o.id";
    let c = ossiec::compile_layer2(&g, q, &duck()).unwrap();
    assert_eq!(
        duckdb_rows(&db, &c.sql),
        vec![vec!["3.000000".to_string(), "2.000000".to_string()]],
        "{}",
        c.sql
    );
}

/// A plain aggregate in a derived table, read by an outer query.
#[test]
fn s72_plain_aggregate_in_a_derived_table() {
    let db = duckdb();
    let c = ossiec::compile_layer2(
        &graph(),
        "SELECT AVG(n), MAX(last) FROM \
         (SELECT status, COUNT(*) AS n, MAX(order_date) AS last FROM orders GROUP BY status) t",
        &duck(),
    )
    .unwrap();
    assert_eq!(
        duckdb_rows(&db, &c.sql),
        duckdb_rows(
            &db,
            "SELECT AVG(n), MAX(last) FROM \
             (SELECT status, COUNT(*) AS n, MAX(order_date) AS last FROM orders GROUP BY status) t"
        ),
        "{}",
        c.sql
    );
}

#[test]
fn s72_plain_aggregate_refusals() {
    let two = two_facts_graph();
    for (g, q, why) in [
        (
            graph(),
            "SELECT status, MAX(order_id) + 1, AGG(order_count) FROM orders GROUP BY status",
            "one SUM|COUNT|AVG|MIN|MAX",
        ),
        (
            graph(),
            "SELECT status, AGG(order_count) / COUNT(*) FROM orders GROUP BY status",
            "not supported",
        ),
        (
            two.clone(),
            "SELECT c.region, COUNT(*), AGG(o.order_amt), AGG(t.ticket_count) FROM customers c \
             LEFT JOIN orders o ON o.customer_id = c.id \
             LEFT JOIN tickets t ON t.customer_id = c.id GROUP BY c.region",
            "independent facts",
        ),
    ] {
        let e = ossiec::compile_layer2(&g, q, &duck()).expect_err(q);
        assert!(
            matches!(e, Error::Unsupported(ref m) if m.contains(why)),
            "{q}: {e}"
        );
    }
    let e = ossiec::compile_layer2(
        &graph(),
        "-- ossiec: includeUnmatched
         SELECT p.category, COUNT(*), AGG(o.order_count) FROM orders o \
         LEFT JOIN products p ON o.product_id = p.id GROUP BY p.category",
        &duck(),
    )
    .unwrap_err();
    assert!(
        matches!(e, Error::Unsupported(ref m) if m.contains("includeUnmatched")),
        "{e}"
    );
    let e = ossiec::layer2_to_request(
        &graph(),
        "SELECT status, MAX(order_date), AGG(order_count) FROM orders GROUP BY status",
    )
    .unwrap_err();
    assert!(
        matches!(e, Error::Unsupported(ref m) if m.contains("compile_sql")),
        "{e}"
    );
}

/// §7.7: a measure passes unchanged through any number of plain projections and
/// filters. Each level's WHERE applies; the outer AGG evaluates it at the top.
#[test]
fn s77_measure_passes_through_nested_derived_tables() {
    let g = graph();
    let db = duckdb();
    for (q, reference) in [
        (
            "SELECT status, AGG(order_count) FROM \
             (SELECT status, order_count FROM (SELECT status, order_count FROM orders) a) b \
             GROUP BY status",
            "SELECT status, COUNT(*) FROM orders GROUP BY status",
        ),
        (
            "SELECT b.state, AGG(b.n) FROM \
             (SELECT a.st AS state, a.n FROM \
                (SELECT status AS st, order_count AS n FROM orders \
                 WHERE customer_id IS NOT NULL) a \
              WHERE a.st <> 'cancelled') b \
             WHERE b.state <> 'pending' GROUP BY b.state",
            "SELECT status, COUNT(*) FROM orders WHERE customer_id IS NOT NULL \
             AND status <> 'cancelled' AND status <> 'pending' GROUP BY status",
        ),
        (
            "SELECT status, AGG(order_count) FROM \
             (SELECT status, order_count FROM \
               (SELECT status, order_count FROM \
                 (SELECT status, order_count FROM orders WHERE status <> 'pending') a) b) c \
             GROUP BY status",
            "SELECT status, COUNT(*) FROM orders WHERE status <> 'pending' GROUP BY status",
        ),
        (
            "SELECT status, AGG(m) FROM \
             (SELECT status, m FROM \
               (SELECT status, COUNT(*) TO MEASURE AS m FROM orders) a) b \
             GROUP BY status",
            "SELECT status, COUNT(*) FROM orders GROUP BY status",
        ),
    ] {
        let c = ossiec::compile_layer2(&g, q, &duck()).unwrap_or_else(|e| panic!("{q}: {e}"));
        assert_eq!(
            sorted(duckdb_rows(&db, &c.sql)),
            sorted(duckdb_rows(&db, reference)),
            "{q}\n{}",
            c.sql
        );
    }
}

/// The innermost query groups, and folding would drop its GROUP BY: refused.
#[test]
fn s77_nested_fold_refuses_an_aggregating_innermost() {
    let q = "SELECT status, AGG(order_count) FROM \
             (SELECT status, order_count FROM \
               (SELECT status, MAX(order_date) AS d FROM orders GROUP BY status) a) b \
             GROUP BY status";
    let e = ossiec::compile_layer2(&graph(), q, &duck()).unwrap_err();
    assert!(
        matches!(e, Error::Unsupported(ref m) if m.contains("aggregates")),
        "{e}"
    );
}

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
