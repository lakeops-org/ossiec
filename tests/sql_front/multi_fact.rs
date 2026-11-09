//! Appendix A.4: measures from independent facts, and the query as one relation
//! (§7.6, §10.2).

use super::*;

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

/// A comma `FROM` is a cross join, as in SQL (§7.6, §10.2), not a list of
/// independent facts (those are written with joins, as A.4 does). A cross join
/// spanning two facts isn't supported yet, so the comma form is refused with the
/// same message as the explicit `CROSS JOIN`, rather than answered per region.
#[test]
fn a4_comma_from_is_a_cross_join_not_independent_facts() {
    let g = two_facts_graph();
    let opts = two_facts_opts();
    let refusal = |from: &str| {
        let sql = format!(
            "SELECT c.region, AGG(o.order_amt), AGG(t.ticket_count) FROM {from} GROUP BY c.region"
        );
        unsupported_msg(ossiec::compile_layer2(&g, &sql, &opts).unwrap_err())
    };
    let comma = refusal("customers c, orders o, tickets t");
    assert!(
        comma.contains("CROSS JOIN cannot span independent facts"),
        "{comma}"
    );
    assert_eq!(
        comma,
        refusal("customers c CROSS JOIN orders o CROSS JOIN tickets t")
    );
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

/// `COUNT(*)` on two facts is the same SQL over different rows: two terms, not
/// one. Shared, every region's ticket count came out as its order count.
#[test]
fn count_star_on_two_facts_stays_two_terms() {
    let model = TWO_FACTS.replace(
        "      - { name: order_amt, expression: { sql: SUM(amount) } }",
        "      - { name: order_amt, expression: { sql: SUM(amount) } }\n      \
         - { name: order_count, expression: { sql: COUNT(*) } }",
    );
    let g = ossiec::from_yaml(&model).unwrap();
    let q = "SELECT c.region, AGG(o.order_count), AGG(t.ticket_count) FROM customers c \
             LEFT JOIN orders o ON o.customer_id = c.id \
             LEFT JOIN tickets t ON t.customer_id = c.id GROUP BY c.region";
    let c = ossiec::compile_layer2(&g, q, &two_facts_opts()).unwrap();
    let db = two_facts_db();
    assert_eq!(
        sorted(duckdb_rows(&db, &c.sql)),
        sorted(duckdb_rows(
            &db,
            "SELECT c.region, \
                    (SELECT COUNT(*) FROM orders o JOIN customers x ON o.customer_id = x.id \
                     WHERE x.region IS NOT DISTINCT FROM c.region), \
                    (SELECT COUNT(*) FROM tickets t JOIN customers x ON t.customer_id = x.id \
                     WHERE x.region IS NOT DISTINCT FROM c.region) \
             FROM (SELECT DISTINCT region FROM customers) c"
        )),
        "{}",
        c.sql
    );
}

/// The query is one relation (§7.6, §10.2), however its facts are planned: a
/// join that drops customers drops them for every measure. Each case is checked
/// against the relation itself, each measure over the rows it derives into
/// (orders by customer and amount, unique in this data; tickets by id).
#[test]
fn joins_on_one_fact_narrow_the_other() {
    let g = two_facts_graph();
    let db = two_facts_db();
    for joins in [
        // APAC has a ticket but no orders: not in either inner join.
        "customers c JOIN orders o ON o.customer_id = c.id \
         JOIN tickets t ON t.customer_id = c.id",
        "customers c JOIN orders o ON o.customer_id = c.id \
         LEFT JOIN tickets t ON t.customer_id = c.id",
        // From the fact: only customers with orders, and orders with no customer.
        "orders o LEFT JOIN customers c ON o.customer_id = c.id \
         LEFT JOIN tickets t ON t.customer_id = c.id",
        "customers c LEFT JOIN orders o ON o.customer_id = c.id \
         JOIN tickets t ON t.customer_id = c.id",
        // Nothing dropped: every customer stays.
        "customers c LEFT JOIN orders o ON o.customer_id = c.id \
         LEFT JOIN tickets t ON t.customer_id = c.id",
    ] {
        let q = format!(
            "SELECT c.region, AGG(o.order_amt), AGG(t.ticket_count) FROM {joins} GROUP BY c.region"
        );
        let c = ossiec::compile_layer2(&g, &q, &two_facts_opts())
            .unwrap_or_else(|e| panic!("{joins}: {e}"));
        let want = format!(
            "WITH r AS (SELECT c.region, o.customer_id AS ocid, o.amount, t.id AS tid FROM {joins})
             SELECT r.region,
                    (SELECT SUM(amount) FROM (SELECT DISTINCT ocid, amount FROM r x
                     WHERE x.region IS NOT DISTINCT FROM r.region AND x.amount IS NOT NULL)),
                    COUNT(DISTINCT tid)
             FROM r GROUP BY r.region"
        );
        assert_eq!(
            sorted(duckdb_rows(&db, &c.sql)),
            sorted(duckdb_rows(&db, &want)),
            "{joins}\n{}",
            c.sql
        );
    }
}

/// Narrowing across facts isn't implemented for `includeUnmatched`, whose
/// unmatched rows would need it too: refused rather than answered wrongly.
#[test]
fn narrowing_across_facts_refuses_include_unmatched() {
    let q = "-- ossiec: includeUnmatched
        SELECT c.region, AGG(o.order_amt), AGG(t.ticket_count) FROM customers c \
        JOIN orders o ON o.customer_id = c.id JOIN tickets t ON t.customer_id = c.id \
        GROUP BY c.region";
    let e = ossiec::compile_layer2(&two_facts_graph(), q, &two_facts_opts()).unwrap_err();
    assert!(
        matches!(e, Error::Unsupported(ref m) if m.contains("includeUnmatched")),
        "{e}"
    );
}

/// The query is one relation (§7.6, §10.2): the inner join to `d` keeps only
/// the top two customers' rows (1 and 2, both EU), for every measure. Tickets of
/// US and APAC customers aren't in it. Each measure still counts its own rows
/// once: the tree fans orders through tickets, but neither is inflated (G2).
#[test]
fn a2_derived_join_narrows_every_fact() {
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
    assert!(c.sql.contains("EXISTS"), "{}", c.sql);
    let db = two_facts_db();
    let got = sorted(duckdb_rows(&db, &c.sql));
    // Each measure over the rows it derives into: orders by (customer, amount),
    // which is unique in this data, and tickets by id.
    let want = sorted(duckdb_rows(
        &db,
        "WITH d AS (SELECT customer_id FROM orders GROUP BY customer_id
                    ORDER BY SUM(amount) DESC, customer_id LIMIT 2),
              r AS (SELECT c.region, o.customer_id AS ocid, o.amount, t.id AS tid
                    FROM customers c
                    LEFT JOIN orders o ON o.customer_id = c.id
                    LEFT JOIN tickets t ON t.customer_id = c.id
                    JOIN d ON o.customer_id = d.customer_id)
         SELECT r.region,
                (SELECT SUM(amount) FROM (SELECT DISTINCT ocid, amount FROM r x
                                          WHERE x.region IS NOT DISTINCT FROM r.region)),
                COUNT(DISTINCT tid)
         FROM r GROUP BY r.region",
    ));
    assert_eq!(got, want, "{}", c.sql);
    assert_eq!(
        got,
        vec![vec![
            "EU".to_string(),
            "60.000000".to_string(),
            "3.000000".to_string()
        ]]
    );
}

/// The ratio over a relation the derived join narrows: only EU's top customers
/// are in it, for orders and tickets alike (§7.6, §10.2): 60 / 3.
#[test]
fn a4_ratio_with_a_derived_join_on_one_fact() {
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
    let got = rows_f64(duckdb_rows(&two_facts_db(), &c.sql));
    assert_eq!(got, vec![("EU".to_string(), 20.0)], "{}", c.sql);
}

/// §7.3, §10.2: a `WHERE` on one fact's columns filters the one relation, so it
/// keeps that fact's rows that pass and the customers that still have one; the
/// other fact keeps only those customers' rows. Orders over 10 are customer 1's
/// 20 and customer 2's 30, both in EU, whose customers have tickets 100–102.
#[test]
fn a4_where_on_one_fact_narrows_the_other() {
    let g = two_facts_graph();
    let db = two_facts_db();
    let run = |predicate: &str| {
        let sql = format!(
            "SELECT c.region, AGG(o.order_amt) AS amt, AGG(t.ticket_count) AS n FROM customers c \
             LEFT JOIN orders o ON o.customer_id = c.id \
             LEFT JOIN tickets t ON t.customer_id = c.id \
             WHERE {predicate} GROUP BY c.region"
        );
        let c = ossiec::compile_layer2(&g, &sql, &two_facts_opts())
            .unwrap_or_else(|e| panic!("{sql}: {e}"));
        (sorted(duckdb_rows(&db, &c.sql)), c.sql)
    };
    let (got, sql) = run("o.amount > 10");
    assert_eq!(got, vec![vec!["EU", "50.000000", "3.000000"]], "{sql}");
    // On the other fact: tickets from 103 on are customer 3's (US, 5 of orders)
    // and customer 4's (APAC, no orders: a group with no order rows).
    let (got, sql) = run("t.id >= 103");
    assert_eq!(
        got,
        vec![
            vec!["APAC", "NULL", "1.000000"],
            vec!["US", "5.000000", "3.000000"],
        ],
        "{sql}"
    );
    // With a filter on the shared dimension too.
    let (got, sql) = run("c.region = 'EU' AND o.amount > 10");
    assert_eq!(got, vec![vec!["EU", "50.000000", "3.000000"]], "{sql}");
}

/// §7.3: `IN`, `NOT IN`, `BETWEEN` and `LIKE` on a plain column are never true on
/// NULL, so like a comparison they go on the one fact and narrow the other.
/// Orders: customer 1 has 10 ('web') and 20 ('store'), customer 2 has 30 ('web'),
/// customer 3 has 5 ('webshop'). Tickets: customer 1 has one, 2 has two, 3 has three.
#[test]
fn a4_where_in_between_like_on_one_fact_narrow_the_other() {
    let g = ossiec::from_yaml(&TWO_FACTS.replace(
        "      - { name: amount, expression: { sql: amount } }\n",
        "      - { name: amount, expression: { sql: amount } }\n      \
         - { name: channel, expression: { sql: channel } }\n",
    ))
    .unwrap();
    let db = two_facts_db();
    db.execute_batch(
        "ALTER TABLE orders ADD COLUMN channel VARCHAR;
         UPDATE orders SET channel = CASE amount WHEN 10 THEN 'web' WHEN 20 THEN 'store'
           WHEN 30 THEN 'web' ELSE 'webshop' END;",
    )
    .unwrap();
    let run = |predicate: &str| {
        let sql = format!(
            "SELECT c.region, AGG(o.order_amt) AS amt, AGG(t.ticket_count) AS n FROM customers c \
             LEFT JOIN orders o ON o.customer_id = c.id \
             LEFT JOIN tickets t ON t.customer_id = c.id \
             WHERE {predicate} GROUP BY c.region"
        );
        let c = ossiec::compile_layer2(&g, &sql, &two_facts_opts())
            .unwrap_or_else(|e| panic!("{sql}: {e}"));
        (sorted(duckdb_rows(&db, &c.sql)), c.sql)
    };
    let eu_us = |eu: &'static str, eu_n: &'static str| {
        vec![vec!["EU", eu, eu_n], vec!["US", "5.000000", "3.000000"]]
    };
    // 10 and 30: customers 1 and 2, both EU, with three tickets between them.
    let (got, sql) = run("o.amount IN (10, 30)");
    assert_eq!(got, vec![vec!["EU", "40.000000", "3.000000"]], "{sql}");
    // 20 and 5: customer 1 (one ticket) and customer 3.
    let (got, sql) = run("o.amount NOT IN (10, 30)");
    assert_eq!(got, eu_us("20.000000", "1.000000"), "{sql}");
    // 10, 20 and 5: customer 1 and customer 3.
    let (got, sql) = run("o.amount BETWEEN 5 AND 20");
    assert_eq!(got, eu_us("30.000000", "1.000000"), "{sql}");
    // 'web', 'web' and 'webshop': 10 and 30 in EU (three tickets), 5 in US.
    let (got, sql) = run("o.channel LIKE 'web%'");
    assert_eq!(got, eu_us("40.000000", "3.000000"), "{sql}");
}

/// A predicate a padded row can pass, on one fact, keeps rows no leg has; and a
/// predicate across both facts has no one leg to go on. Both refused, by name.
#[test]
fn a4_where_on_one_fact_refusals() {
    let g = two_facts_graph();
    let refusal = |predicate: &str| {
        let sql = format!(
            "SELECT c.region, AGG(o.order_amt), AGG(t.ticket_count) FROM customers c \
             LEFT JOIN orders o ON o.customer_id = c.id \
             LEFT JOIN tickets t ON t.customer_id = c.id \
             WHERE {predicate} GROUP BY c.region"
        );
        unsupported_msg(ossiec::compile_layer2(&g, &sql, &two_facts_opts()).unwrap_err())
    };
    for predicate in [
        "o.amount IS NULL",
        // Not structured yet: each stays a free-form predicate.
        "o.amount NOT BETWEEN 10 AND 20",
        "NOT o.amount IN (10, 30)",
        // SQL's NULL in a list: `NOT IN (10, NULL)` keeps no row.
        "o.amount NOT IN (10, NULL)",
    ] {
        let m = refusal(predicate);
        // It names what works, and tells NULL-passing tests from forms not yet handled.
        for part in [
            "one of several independent facts",
            "IN, NOT IN, BETWEEN and LIKE",
            "IS NULL",
            "aren't implemented yet",
        ] {
            assert!(m.contains(part), "{predicate}: {m}");
        }
    }
    let m = refusal("o.amount > t.id");
    assert!(m.contains("independent facts"), "{m}");
}

/// A field that is an expression can map NULL to a value, so a comparison on it
/// can keep the NULL-padded rows of a fact the other facts don't share:
/// `COALESCE(amount, 0) = 0` holds for a customer without orders. Scoping it to
/// the orders leg would drop that customer from the tickets leg, so it's refused.
/// A comparison on a plain column is never true on NULL, and is scoped.
#[test]
fn a4_where_on_an_expression_field_is_refused() {
    let yaml = TWO_FACTS.replace(
        "      - { name: amount, expression: { sql: amount } }\n",
        "      - { name: amount, expression: { sql: amount } }\n      \
         - { name: amount_or_zero, expression: { sql: \"COALESCE(amount, 0)\" } }\n      \
         - { name: always_one, expression: { sql: \"1\" } }\n",
    );
    assert_ne!(yaml, TWO_FACTS, "fixture changed: update the replacement");
    let g = ossiec::from_yaml(&yaml).unwrap();
    let compile = |predicate: &str| {
        let sql = format!(
            "SELECT c.region, AGG(o.order_amt), AGG(t.ticket_count) FROM customers c \
             LEFT JOIN orders o ON o.customer_id = c.id \
             LEFT JOIN tickets t ON t.customer_id = c.id \
             WHERE {predicate} GROUP BY c.region"
        );
        ossiec::compile_layer2(&g, &sql, &two_facts_opts())
    };
    for predicate in [
        "o.amount_or_zero = 0",
        "o.amount_or_zero >= 0",
        "o.always_one = 1",
    ] {
        let m = unsupported_msg(compile(predicate).unwrap_err());
        assert!(
            m.contains("one of several independent facts"),
            "{predicate}: {m}"
        );
    }
    compile("o.amount >= 0").expect("a comparison on a plain column is scoped");
}
