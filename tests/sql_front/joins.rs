//! Joins (§7.6, A.5, A.6): every kind, from either side, fan-out, and G2.

use super::*;

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

/// `FROM orders o, customers c` is `orders o CROSS JOIN customers c` (§7.6):
/// the same SQL, and each order counted once per region.
#[test]
fn comma_from_is_a_cross_join() {
    let comma = "SELECT c.region, AGG(o.order_count) AS n \
        FROM orders o, customers c GROUP BY c.region";
    let cross = "SELECT c.region, AGG(o.order_count) AS n \
        FROM orders o CROSS JOIN customers c GROUP BY c.region";
    let g = graph();
    assert_eq!(
        ossiec::compile_layer2(&g, comma, &duck()).unwrap().sql,
        ossiec::compile_layer2(&g, cross, &duck()).unwrap().sql
    );
    let reference = "SELECT region, (SELECT COUNT(*) FROM orders) \
        FROM {customers} GROUP BY region";
    let naive = "SELECT c.region, COUNT(*) \
        FROM orders o, {customers} c GROUP BY c.region";
    assert_join_answer(comma, "CROSS JOIN", reference, naive);
}

/// A comma binds looser than `JOIN`: `FROM c, o LEFT JOIN p ON …` is
/// `c × (o LEFT JOIN p)`, which flattening to `c CROSS JOIN o LEFT JOIN p` keeps.
#[test]
fn comma_from_before_a_left_join_keeps_its_meaning() {
    let sql = "SELECT c.region, p.category, AGG(o.order_count) AS n \
        FROM customers c, orders o LEFT JOIN products p ON o.product_id = p.id \
        GROUP BY c.region, p.category";
    let reference = "SELECT c.region, x.category, x.n \
        FROM (SELECT DISTINCT region FROM {customers}) c, \
             (SELECT p.category, COUNT(*) AS n FROM orders o \
              LEFT JOIN products p ON o.product_id = p.product_id GROUP BY p.category) x";
    let naive = "SELECT c.region, p.category, COUNT(*) \
        FROM {customers} c, orders o LEFT JOIN products p ON o.product_id = p.product_id \
        GROUP BY c.region, p.category";
    assert_join_answer(sql, "CROSS JOIN", reference, naive);
}

/// With a `RIGHT` or `FULL` join, `a, b RIGHT JOIN c` pairs `c`'s padded rows
/// with every `a` row, which flattening can't express: refused, by name.
#[test]
fn comma_from_with_a_right_or_full_join_is_refused() {
    for kind in ["RIGHT", "FULL"] {
        let sql = format!(
            "SELECT c.region, AGG(o.order_count) FROM customers c, orders o \
             {kind} JOIN products p ON o.product_id = p.id GROUP BY c.region"
        );
        let m = unsupported_msg(ossiec::compile_layer2(&graph(), &sql, &duck()).unwrap_err());
        assert!(
            m.contains("comma-separated FROM") && m.contains("CROSS JOIN"),
            "{m}"
        );
    }
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

/// A fan-out written from the many side (§7.6): `order_items i LEFT JOIN orders o`
/// is the relation `orders o RIGHT JOIN order_items i`, so it counts orders as
/// that form does, de-duplicated by primary key. Grouped by the line's order id,
/// line 4's order 9 doesn't exist: `LEFT` keeps its group with no orders, `INNER`
/// drops it, and `FULL` also keeps order 3, which has no lines.
#[test]
fn a5_fanout_written_from_the_many_side_dedups() {
    let g = ossiec::from_yaml(FANOUT).unwrap();
    let db = duckdb::Connection::open_in_memory().unwrap();
    db.execute_batch(
        "CREATE TABLE orders (order_id INTEGER);
         INSERT INTO orders VALUES (1), (2), (3);
         CREATE TABLE order_items (line_id INTEGER, order_id INTEGER);
         INSERT INTO order_items VALUES (1, 1), (2, 1), (3, 2), (4, 9);",
    )
    .unwrap();
    for (join, want) in [
        (
            "LEFT JOIN",
            vec![vec!["1", "1"], vec!["2", "1"], vec!["9", "0"]],
        ),
        ("JOIN", vec![vec!["1", "1"], vec!["2", "1"]]),
        (
            "FULL JOIN",
            vec![
                vec!["1", "1"],
                vec!["2", "1"],
                vec!["9", "0"],
                vec!["NULL", "1"],
            ],
        ),
    ] {
        let sql = format!(
            "SELECT i.order_id, AGG(o.order_count) FROM order_items i \
             {join} orders o ON i.order_id = o.id GROUP BY i.order_id"
        );
        let c = ossiec::compile_layer2(&g, &sql, &duck()).unwrap_or_else(|e| panic!("{sql}: {e}"));
        let reference = format!(
            "SELECT i.order_id, COUNT(DISTINCT o.order_id) FROM order_items i \
             {join} orders o ON i.order_id = o.order_id GROUP BY i.order_id"
        );
        let got = sorted(duckdb_rows(&db, &c.sql));
        assert_eq!(
            got,
            sorted(duckdb_rows(&db, &reference)),
            "{join}\n{}",
            c.sql
        );
        let mut want: Vec<Vec<String>> = want
            .into_iter()
            .map(|r| r.into_iter().map(String::from).collect())
            .collect();
        want.sort();
        let got: Vec<Vec<String>> = got
            .into_iter()
            .map(|r| {
                r.into_iter()
                    .map(|v| v.trim_end_matches(".000000").to_string())
                    .collect()
            })
            .collect();
        assert_eq!(got, want, "{join}\n{}", c.sql);
    }
    // The same relation written either way compiles to the same SQL.
    let from_lines = ossiec::compile_layer2(
        &g,
        "SELECT AGG(o.order_count) FROM order_items i LEFT JOIN orders o ON i.order_id = o.id",
        &duck(),
    )
    .unwrap();
    let from_orders = ossiec::compile_layer2(
        &g,
        "SELECT AGG(o.order_count) FROM orders o RIGHT JOIN order_items i ON i.order_id = o.id",
        &duck(),
    )
    .unwrap();
    assert_eq!(from_lines.sql, from_orders.sql);
}

/// A join is turned around only when no leg has its first side at all. Here
/// `airports` is on the leg twice (departure and arrival) and `countries` once,
/// so `dep.country = kd.code` can't say which airport it means; turned around, it
/// would join a new copy of `airports` to the carrier's country. It's refused.
#[test]
fn a_join_from_a_dataset_with_two_roles_is_not_turned_around() {
    let g = ossiec::from_yaml(
        r#"
version: "0.2.0.dev0"
name: flights
datasets:
  - name: flights
    source: flights
    primary_key: [id]
    fields:
      - { name: id, expression: { sql: id } }
      - { name: origin, expression: { sql: origin } }
      - { name: dest, expression: { sql: dest } }
      - { name: carrier, expression: { sql: carrier } }
      - { name: delay, expression: { sql: delay } }
    metrics:
      - { name: total_delay, expression: { sql: SUM(delay) } }
  - name: airports
    source: airports
    primary_key: [code]
    fields:
      - { name: code, expression: { sql: code } }
      - { name: country, expression: { sql: country } }
  - name: countries
    source: countries
    primary_key: [code]
    fields:
      - { name: code, expression: { sql: code } }
      - { name: name, expression: { sql: name } }
relationships:
  - { name: departs_from, from: flights, to: airports, from_columns: [origin], to_columns: [code] }
  - { name: arrives_at, from: flights, to: airports, from_columns: [dest], to_columns: [code] }
  - { name: registered_in, from: flights, to: countries, from_columns: [carrier], to_columns: [code] }
  - { name: located_in, from: airports, to: countries, from_columns: [country], to_columns: [code] }
"#,
    )
    .unwrap();
    let sql = "SELECT k.name, AGG(f.total_delay) FROM flights f \
               JOIN airports dep ON f.origin = dep.code \
               JOIN airports arr ON f.dest = arr.code \
               JOIN countries k ON f.carrier = k.code \
               JOIN countries kd ON dep.country = kd.code GROUP BY k.name";
    let m = unsupported_msg(ossiec::compile_layer2(&g, sql, &duck()).unwrap_err());
    assert!(m.contains("cannot attach"), "{m}");
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

/// The join kind is part of a query's identity: an `INNER` and a `LEFT` join
/// compile to different SQL, so they must not share a `request_hash`.
#[test]
fn join_kind_changes_the_request_hash() {
    let g = ossiec::from_yaml(common::MODEL).unwrap();
    let compile = |join: &str| {
        let sql = format!(
            "SELECT c.region, AGG(o.order_count) FROM orders o {join} customers c \
             ON o.customer_id = c.id GROUP BY c.region"
        );
        ossiec::compile_sql(&g, &sql, &duck()).unwrap()
    };
    let (inner, left) = (compile("JOIN"), compile("LEFT JOIN"));
    assert_ne!(inner.sql, left.sql);
    assert_ne!(inner.request_hash, left.request_hash);
    assert_eq!(left.request_hash, compile("LEFT JOIN").request_hash);
}

/// One alias names one relation, however the second one is joined: a comma
/// item, a `CROSS JOIN` or a relationship join. Refused, never resolved to one.
#[test]
fn a_repeated_alias_is_refused_in_every_join_form() {
    for from in [
        "orders o, customers o",
        "orders o CROSS JOIN customers o",
        "orders o JOIN customers o ON o.customer_id = o.id",
    ] {
        let sql = format!("SELECT AGG(o.order_count) FROM {from}");
        let e = ossiec::compile_layer2(&graph(), &sql, &duck())
            .expect_err(&sql)
            .to_string();
        assert!(e.contains("duplicate relation alias `o`"), "{from}: {e}");
    }
}
