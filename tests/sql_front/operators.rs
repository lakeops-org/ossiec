//! Query operators (§7.3–§7.8): `WHERE`, `HAVING`, `ORDER BY`, `GROUP BY ALL`,
//! `DISTINCT` and `WITH`.

use super::*;

fn refuses(sql: &str, want: &str) {
    let g = ossiec::from_yaml(common::MODEL).unwrap();
    let e = ossiec::compile_sql(&g, sql, &duck())
        .expect_err(sql)
        .to_string();
    assert!(e.contains(want), "want `{want}` for {sql}: {e}");
}

fn duck_db() -> duckdb::Connection {
    duckdb()
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

/// §7.3: a subquery in `WHERE` is a Layer 2 query of its own, compiled as one and
/// read by the predicate as SQL reads it: `IN`, `NOT IN`, `EXISTS` and a scalar.
/// It can evaluate a measure (`HAVING AGG(...)`) and hold a subquery of its own.
/// Each answer is the same query in plain SQL over the source tables.
#[test]
fn where_reads_subqueries() {
    let g = ossiec::from_yaml(common::MODEL).unwrap();
    let db = duckdb();
    let customers = "(SELECT * FROM raw_customers WHERE NOT deleted)";
    for (predicate, reference) in [
        (
            "customer_id IN (SELECT c.id FROM customers c WHERE c.segment = 'smb')",
            format!("customer_id IN (SELECT customer_id FROM {customers} WHERE segment = 'smb')"),
        ),
        (
            "customer_id NOT IN (SELECT c.id FROM customers c WHERE c.region = 'EU')",
            format!("customer_id NOT IN (SELECT customer_id FROM {customers} WHERE region = 'EU')"),
        ),
        (
            "EXISTS (SELECT c.id FROM customers c WHERE c.region = 'APAC')",
            format!("EXISTS (SELECT 1 FROM {customers} WHERE region = 'APAC')"),
        ),
        (
            "status = 'completed' OR NOT EXISTS (SELECT c.id FROM customers c WHERE c.region = 'nowhere')",
            format!("status = 'completed' OR NOT EXISTS (SELECT 1 FROM {customers} WHERE region = 'nowhere')"),
        ),
        // A scalar subquery, here a plain aggregate.
        (
            "id > (SELECT MIN(o2.id) FROM orders o2 WHERE o2.status = 'returned')",
            "order_id > (SELECT MIN(order_id) FROM orders WHERE status = 'returned')".to_string(),
        ),
        // A subquery that evaluates a measure: customers with more than one order.
        (
            "customer_id IN (SELECT o2.customer_id FROM orders o2 GROUP BY o2.customer_id \
             HAVING AGG(o2.order_count) > 1)",
            "customer_id IN (SELECT customer_id FROM orders GROUP BY customer_id \
             HAVING COUNT(*) > 1)"
                .to_string(),
        ),
        // A subquery inside a subquery.
        (
            "customer_id IN (SELECT c.id FROM customers c WHERE c.id IN \
             (SELECT o2.customer_id FROM orders o2 WHERE o2.status = 'returned'))",
            format!(
                "customer_id IN (SELECT customer_id FROM {customers} WHERE customer_id IN \
                 (SELECT customer_id FROM orders WHERE status = 'returned'))"
            ),
        ),
    ] {
        let sql = format!(
            "SELECT status, AGG(order_count) FROM orders WHERE {predicate} GROUP BY status"
        );
        let compiled =
            ossiec::compile_sql(&g, &sql, &duck()).unwrap_or_else(|e| panic!("{predicate}: {e}"));
        let got = sorted(duckdb_rows(&db, &compiled.sql));
        let want = sorted(duckdb_rows(
            &db,
            &format!("SELECT status, COUNT(*) FROM orders WHERE {reference} GROUP BY status"),
        ));
        assert_eq!(got, want, "{predicate}\n{}", compiled.sql);
        assert!(!want.is_empty(), "{predicate}: proves nothing");
    }
    for (predicate, want) in [
        // It reads the outer query's row: correlated, not supported yet.
        (
            "EXISTS (SELECT c.id FROM customers c WHERE c.id = orders.customer_id)",
            "correlated",
        ),
        (
            "customer_id IN (SELECT c.id, c.region FROM customers c)",
            "one column",
        ),
        (
            "customer_id IN (SELECT o2.customer_id, o2.order_count FROM orders o2)",
            "unevaluated measure",
        ),
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
    // Two levels of pass-through, one CTE reading the other (§7.7).
    answers(
        "WITH a AS (SELECT status, customer_id, order_count FROM orders), \
              b AS (SELECT status, order_count FROM a WHERE customer_id IS NOT NULL) \
         SELECT status, AGG(order_count) FROM b GROUP BY status",
        "SELECT status, COUNT(*) FROM orders WHERE customer_id IS NOT NULL GROUP BY status",
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

/// §7.2, §7.5: a plain aggregate in `HAVING` or `ORDER BY` reads the group's
/// joined rows as plain SQL does, beside `AGG()`, whether or not it is selected.
#[test]
fn having_and_order_by_read_plain_aggregates() {
    let g = ossiec::from_yaml(common::MODEL).unwrap();
    let db = duck_db();
    let run = |sql: &str| {
        let c = ossiec::compile_sql(&g, sql, &duck()).unwrap_or_else(|e| panic!("{sql}: {e}"));
        duckdb_rows(&db, &c.sql)
    };
    assert_eq!(
        sorted(run(
            "SELECT status, AGG(order_count) AS n FROM orders GROUP BY status HAVING COUNT(*) > 1"
        )),
        sorted(duckdb_rows(
            &db,
            "SELECT status, COUNT(*) FROM orders GROUP BY status HAVING COUNT(*) > 1"
        ))
    );
    // Regions with a customer who has no orders: more joined rows than orders.
    assert_eq!(
        sorted(run(
            "SELECT c.region, AGG(o.order_count) AS n FROM customers c \
             LEFT JOIN orders o ON o.customer_id = c.id \
             GROUP BY c.region HAVING COUNT(*) > AGG(o.order_count)"
        )),
        sorted(duckdb_rows(
            &db,
            "SELECT c.region, COUNT(o.order_id) FROM {customers} c \
             LEFT JOIN orders o ON o.customer_id = c.customer_id \
             GROUP BY c.region HAVING COUNT(*) > COUNT(o.order_id)"
        ))
    );
    // Sorted, not just the same rows.
    assert_eq!(
        run("SELECT status FROM orders GROUP BY status ORDER BY MAX(id) DESC, status LIMIT 3"),
        duckdb_rows(
            &db,
            "SELECT status FROM orders GROUP BY status ORDER BY MAX(order_id) DESC, status LIMIT 3"
        )
    );
    // A HAVING conjunct with a plain aggregate filters groups, not rows.
    assert_eq!(
        sorted(run(
            "SELECT status, AGG(order_count) AS n FROM orders GROUP BY status HAVING SUM(id) > 3"
        )),
        sorted(duckdb_rows(
            &db,
            "SELECT status, COUNT(*) FROM orders GROUP BY status HAVING SUM(order_id) > 3"
        ))
    );
}
