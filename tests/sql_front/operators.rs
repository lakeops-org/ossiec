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

/// §7.4: `ROLLUP`, `CUBE` and `GROUPING SETS` report each measure at every level
/// they name. Each answer is DuckDB's own grouping over the same rows, so a
/// subtotal row's NULLs, `HAVING` on a grouping column, and a one-side measure
/// counted once per level all follow SQL.
#[test]
fn grouping_sets_report_every_level() {
    let g = ossiec::from_yaml(common::MODEL).unwrap();
    let db = duckdb();
    let from = "FROM orders o JOIN customers c ON o.customer_id = c.id";
    let raw = "FROM orders o JOIN (SELECT * FROM raw_customers WHERE NOT deleted) c \
               ON o.customer_id = c.customer_id";
    for (select, plain, rest) in [
        (
            "c.region, o.status, AGG(o.order_count) AS n",
            "c.region, o.status, COUNT(*) AS n",
            "GROUP BY ROLLUP(c.region, o.status)",
        ),
        (
            "c.region, o.status, AGG(o.order_count) AS n",
            "c.region, o.status, COUNT(*) AS n",
            "GROUP BY CUBE(c.region, o.status)",
        ),
        // The largest CUBE: 4 columns, 16 sets.
        (
            "c.region, o.status, c.segment, c.name, AGG(o.order_count) AS n",
            "c.region, o.status, c.segment, c.name, COUNT(*) AS n",
            "GROUP BY CUBE(c.region, o.status, c.segment, c.name)",
        ),
        // No set has both columns: a branch with no rows types them first.
        (
            "c.region, o.status, AGG(o.order_count) AS n",
            "c.region, o.status, COUNT(*) AS n",
            "GROUP BY GROUPING SETS ((c.region), (o.status))",
        ),
        (
            "o.status, c.region, AGG(o.order_count) AS n",
            "o.status, c.region, COUNT(*) AS n",
            "GROUP BY o.status, ROLLUP(c.region)",
        ),
        (
            "c.region, AGG(o.order_count) AS n",
            "c.region, COUNT(*) AS n",
            "GROUP BY GROUPING SETS ((c.region), ())",
        ),
        // HAVING reads a subtotal row's grouping column as NULL: these drop it.
        (
            "c.region, o.status, AGG(o.order_count) AS n",
            "c.region, o.status, COUNT(*) AS n",
            "GROUP BY ROLLUP(c.region, o.status) HAVING c.region = 'EU'",
        ),
        (
            "c.region, o.status, AGG(o.order_count) AS n",
            "c.region, o.status, COUNT(*) AS n",
            "GROUP BY ROLLUP(c.region, o.status) HAVING AGG(o.order_count) > 1",
        ),
        // An aggregate in HAVING reads the rows' values of a column the set
        // leaves out: only the bare column is NULL at a subtotal.
        (
            "c.region, o.status, AGG(o.order_count) AS n",
            "c.region, o.status, COUNT(*) AS n",
            "GROUP BY ROLLUP(c.region, o.status) HAVING COUNT(o.status) > 0",
        ),
        (
            "c.region, o.status, AGG(o.order_count) AS n",
            "c.region, o.status, COUNT(*) AS n",
            "GROUP BY ROLLUP(c.region, o.status) \
             HAVING SUM(CASE WHEN o.status = 'returned' THEN 1 ELSE 0 END) > 0",
        ),
        // Unnamed expressions and plain aggregates keep their names in every
        // set, though a set leaves a column before them out.
        (
            "c.region, AGG(o.order_count) + 1, AGG(o.order_count) + 2",
            "c.region, COUNT(*) + 1, COUNT(*) + 2",
            "GROUP BY ROLLUP(c.region)",
        ),
        (
            "c.region, COUNT(*), SUM(o.amount_cents)",
            "c.region, COUNT(*), SUM(o.amount_cents)",
            "GROUP BY ROLLUP(c.region)",
        ),
    ] {
        let sql = format!("SELECT {select} {from} {rest}");
        let c = ossiec::compile_sql(&g, &sql, &duck()).unwrap_or_else(|e| panic!("{rest}: {e}"));
        let reference = format!(
            "SELECT {plain} {raw} {}",
            rest.replace("AGG(o.order_count)", "COUNT(*)")
        );
        let want = sorted(duckdb_rows(&db, &reference));
        assert_eq!(sorted(duckdb_rows(&db, &c.sql)), want, "{rest}\n{}", c.sql);
        assert!(want.len() > 1, "{rest}: proves nothing");
    }
    // A one-side measure counts each customer once at every level, the grand
    // total included.
    let c = ossiec::compile_sql(
        &g,
        "SELECT o.status, AGG(c.known_customers) AS k FROM customers c \
         JOIN orders o ON o.customer_id = c.id GROUP BY ROLLUP(o.status)",
        &duck(),
    )
    .unwrap();
    assert_eq!(
        sorted(duckdb_rows(&db, &c.sql)),
        sorted(duckdb_rows(
            &db,
            &format!("SELECT o.status, COUNT(DISTINCT c.customer_id) AS k {raw} GROUP BY ROLLUP(o.status)")
        )),
        "{}",
        c.sql
    );
    // ORDER BY and LIMIT apply to the stacked whole, by output name or position.
    let c = ossiec::compile_sql(
        &g,
        &format!(
            "SELECT c.region, AGG(o.order_count) AS n {from} GROUP BY ROLLUP(c.region) \
             ORDER BY n DESC, 1 NULLS FIRST LIMIT 2"
        ),
        &duck(),
    )
    .unwrap();
    assert_eq!(
        duckdb_rows(&db, &c.sql),
        duckdb_rows(
            &db,
            &format!(
                "SELECT c.region, COUNT(*) AS n {raw} GROUP BY ROLLUP(c.region) \
                 ORDER BY n DESC, 1 NULLS FIRST LIMIT 2"
            )
        ),
        "{}",
        c.sql
    );
    // A measure listed before its dimension: position 1 is the measure, so the
    // grand total (the largest count) wins, not the last status.
    let c = ossiec::compile_sql(
        &g,
        &format!(
            "SELECT AGG(o.order_count) AS n, o.status {from} GROUP BY ROLLUP(o.status) \
             ORDER BY 1 DESC LIMIT 1"
        ),
        &duck(),
    )
    .unwrap();
    assert_eq!(
        duckdb_rows(&db, &c.sql),
        duckdb_rows(
            &db,
            &format!(
                "SELECT COUNT(*) AS n, o.status {raw} GROUP BY ROLLUP(o.status) \
                 ORDER BY 1 DESC LIMIT 1"
            )
        ),
        "{}",
        c.sql
    );
    // The subtotal selects no measure once `status` is left out. It is still one
    // NULL row.
    let c = ossiec::compile_sql(
        &g,
        "SELECT status FROM orders GROUP BY ROLLUP(status)",
        &duck(),
    )
    .unwrap();
    let want = sorted(duckdb_rows(
        &db,
        "SELECT status FROM orders GROUP BY ROLLUP(status)",
    ));
    assert_eq!(sorted(duckdb_rows(&db, &c.sql)), want, "{}", c.sql);
    assert!(want.iter().any(|r| r[0] == "NULL"), "{want:?}");
    // The same under includeUnmatched: the grand total is a constant row, not a
    // plain aggregate, which includeUnmatched refuses.
    let unmatched = Options {
        include_unmatched: true,
        ..duck()
    };
    let c = ossiec::compile_sql(
        &g,
        "SELECT status FROM orders GROUP BY ROLLUP(status)",
        &unmatched,
    )
    .unwrap();
    assert_eq!(sorted(duckdb_rows(&db, &c.sql)), want, "{}", c.sql);
    // A grand total that selects nothing still answers to its HAVING.
    let sql =
        format!("SELECT c.region {from} GROUP BY ROLLUP(c.region) HAVING AGG(o.order_count) > 3");
    let c = ossiec::compile_sql(&g, &sql, &duck()).unwrap();
    let want = sorted(duckdb_rows(
        &db,
        &format!("SELECT c.region {raw} GROUP BY ROLLUP(c.region) HAVING COUNT(*) > 3"),
    ));
    assert_eq!(sorted(duckdb_rows(&db, &c.sql)), want, "{sql}\n{}", c.sql);
    assert!(want.iter().any(|r| r[0] == "NULL"), "{want:?}");
    // An unnamed expression is named as in the same query without ROLLUP.
    let names = |sql: &str| {
        let c = ossiec::compile_sql(&g, sql, &duck()).unwrap();
        c.sql
            .match_indices("AS \"__l2_")
            .map(|(i, _)| c.sql[i..].split('"').nth(1).unwrap().to_string())
            .collect::<std::collections::BTreeSet<_>>()
    };
    assert_eq!(
        names(&format!(
            "SELECT c.region, AGG(o.order_count) + 1 {from} GROUP BY ROLLUP(c.region)"
        )),
        names(&format!(
            "SELECT c.region, AGG(o.order_count) + 1 {from} GROUP BY c.region"
        )),
    );
    for (rest, want) in [
        ("GROUP BY ROLLUP(UPPER(c.region))", "take columns"),
        (
            "GROUP BY ROLLUP(c.region, ROLLUP(o.status))",
            "take columns",
        ),
        (
            "GROUP BY CUBE(c.region, o.status, c.segment, c.name, o.id)",
            "at most 4",
        ),
        (
            "GROUP BY ROLLUP(c.region) ORDER BY AGG(o.order_count)",
            "output column",
        ),
        (
            "GROUP BY ROLLUP(c.region) HAVING GROUPING(c.region) = 0",
            "GROUPING()",
        ),
    ] {
        let sql = format!("SELECT c.region, AGG(o.order_count) AS n {from} {rest}");
        let e = ossiec::compile_sql(&g, &sql, &duck())
            .expect_err(rest)
            .to_string();
        assert!(e.contains(want), "{rest}: {e}");
    }
}

/// An `ORDER BY` without `NULLS` means what the consumer's SQL means, NULL as the
/// smallest value, even when the model's fields pick a vendor dialect that sorts
/// NULL last (Postgres): both the grouping-set and the ordinary path write it out.
#[test]
fn order_by_keeps_nulls_first_under_a_vendor_source() {
    let g = ossiec::from_yaml(
        r#"
version: "0.2.0.dev0"
name: sales
datasets:
  - name: sales
    source: sales
    primary_key: [id]
    fields:
      - { name: id, expression: { sql: id } }
      - name: region
        expression:
          sql: region
          dialects:
            - { dialect: POSTGRES, expression: region }
    metrics:
      - { name: n, expression: { sql: COUNT(*) } }
"#,
    )
    .unwrap();
    let db = duckdb::Connection::open_in_memory().unwrap();
    db.execute_batch(
        "CREATE TABLE sales (id INTEGER, region VARCHAR);
         INSERT INTO sales VALUES (1, 'a'), (2, 'b'), (3, 'b'), (4, NULL);",
    )
    .unwrap();
    let opts = Options {
        target: Some(DialectType::DuckDB),
        prefer_dialect: Some("POSTGRES".into()),
        ..Default::default()
    };
    for (sql, want) in [
        // NULL regions sort first: the grand total and the NULL region's own group
        // (n breaks the tie), before 'a'.
        (
            "SELECT region, AGG(n) AS n FROM sales GROUP BY ROLLUP(region) \
             ORDER BY region, n DESC LIMIT 1",
            vec![vec!["NULL", "4"]],
        ),
        // The NULL region's own group sorts first.
        (
            "SELECT region, AGG(n) AS n FROM sales GROUP BY region ORDER BY region LIMIT 1",
            vec![vec!["NULL", "1"]],
        ),
        (
            "SELECT region, AGG(n) AS n FROM sales GROUP BY region ORDER BY region DESC LIMIT 1",
            vec![vec!["b", "2"]],
        ),
    ] {
        let c = ossiec::compile_sql(&g, sql, &opts).unwrap();
        assert_eq!(c.source_dialect, "postgres", "{sql}");
        let got: Vec<Vec<String>> = duckdb_rows(&db, &c.sql)
            .into_iter()
            .map(|r| {
                r.into_iter()
                    .map(|v| v.trim_end_matches(".000000").to_string())
                    .collect()
            })
            .collect();
        assert_eq!(got, want, "{sql}\n{}", c.sql);
    }
}

/// §7.7: a scalar subquery in the select list is a Layer 2 query of its own,
/// alone or in arithmetic with `AGG()`. A correlated one reads the row's
/// dimensions through `=` in its own `WHERE`. Each answer is the same query in
/// plain SQL over the source tables, so a measure over no matching rows is the
/// measure over none (`COUNT` is 0), and a plain aggregate over none is NULL.
#[test]
fn select_list_reads_subqueries() {
    let g = ossiec::from_yaml(common::MODEL).unwrap();
    let db = duckdb();
    let customers = "(SELECT * FROM raw_customers WHERE NOT deleted)";
    let from = "FROM orders o JOIN customers c ON o.customer_id = c.id";
    let raw = format!("FROM orders o JOIN {customers} c ON o.customer_id = c.customer_id");
    for (sql, reference) in [
        // Each region's share of all orders.
        (
            format!(
                "SELECT c.region, AGG(o.order_count) * 1.0 / (SELECT AGG(o2.order_count) FROM orders o2) AS share \
                 {from} GROUP BY c.region"
            ),
            format!(
                "SELECT c.region, COUNT(*) * 1.0 / (SELECT COUNT(*) FROM orders) AS share \
                 {raw} GROUP BY c.region"
            ),
        ),
        // Without AGG(): a dimension listing beside a constant.
        (
            format!(
                "SELECT o.status, (SELECT MIN(o2.order_date) FROM orders o2) AS first {from} GROUP BY o.status"
            ),
            format!(
                "SELECT o.status, (SELECT MIN(order_date) FROM orders) AS first {raw} GROUP BY o.status"
            ),
        ),
        // Correlated with the row's region: its small customers, 0 where none.
        (
            format!(
                "SELECT c.region, AGG(o.order_count) AS n, (SELECT AGG(c2.known_customers) FROM customers c2 \
                 WHERE c2.region = c.region AND c2.segment = 'smb') AS smb {from} GROUP BY c.region"
            ),
            format!(
                "SELECT c.region, COUNT(*) AS n, (SELECT COUNT(c2.customer_id) FROM {customers} c2 \
                 WHERE c2.region = c.region AND c2.segment = 'smb') AS smb {raw} GROUP BY c.region"
            ),
        ),
        // A plain aggregate, correlated: NULL where no row matches.
        (
            format!(
                "SELECT o.status, (SELECT MAX(o2.amount_cents) FROM orders o2 \
                 WHERE o2.status = o.status AND o2.customer_id = 2) AS m {from} GROUP BY o.status"
            ),
            format!(
                "SELECT o.status, (SELECT MAX(o2.amount_cents) FROM orders o2 \
                 WHERE o2.status = o.status AND o2.customer_id = 2) AS m {raw} GROUP BY o.status"
            ),
        ),
        // Grouped and correlated: one row per key.
        (
            format!(
                "SELECT c.region, (SELECT AGG(o2.order_count) FROM orders o2 JOIN customers c2 ON o2.customer_id = c2.id \
                 WHERE c2.region = c.region GROUP BY c2.region) AS n {from} GROUP BY c.region"
            ),
            format!(
                "SELECT c.region, (SELECT COUNT(*) FROM orders o2 JOIN {customers} c2 ON o2.customer_id = c2.customer_id \
                 WHERE c2.region = c.region GROUP BY c2.region) AS n {raw} GROUP BY c.region"
            ),
        ),
        // With ROLLUP: each level reads the same total.
        (
            format!(
                "SELECT c.region, AGG(o.order_count) AS n, (SELECT AGG(o2.order_count) FROM orders o2) AS total \
                 {from} GROUP BY ROLLUP(c.region)"
            ),
            format!(
                "SELECT c.region, COUNT(*) AS n, (SELECT COUNT(*) FROM orders) AS total \
                 {raw} GROUP BY ROLLUP(c.region)"
            ),
        ),
        // AGG() over a sole derived table is folded into one stage, which
        // compiles the subquery.
        (
            "SELECT t.status, AGG(t.order_count) AS n, (SELECT MAX(o2.order_date) FROM orders o2) AS d \
             FROM (SELECT status, order_count FROM orders WHERE customer_id IS NOT NULL) t GROUP BY t.status"
                .to_string(),
            "SELECT status, COUNT(*) AS n, (SELECT MAX(order_date) FROM orders) AS d \
             FROM orders WHERE customer_id IS NOT NULL GROUP BY status"
                .to_string(),
        ),
        // Grouped by the correlated column, written unqualified there: one field.
        (
            "SELECT o.customer_id, (SELECT AGG(o2.order_count) FROM orders o2 \
             WHERE o2.customer_id = o.customer_id GROUP BY customer_id) AS n \
             FROM orders o GROUP BY o.customer_id"
                .to_string(),
            "SELECT o.customer_id, (SELECT COUNT(*) FROM orders o2 \
             WHERE o2.customer_id = o.customer_id GROUP BY customer_id) AS n \
             FROM orders o GROUP BY o.customer_id"
                .to_string(),
        ),
        // In a derived table, read by the query around it.
        (
            format!(
                "SELECT t.region, t.share FROM (SELECT c.region, AGG(o.order_count) * 1.0 / \
                 (SELECT AGG(o2.order_count) FROM orders o2) AS share {from} GROUP BY c.region) t"
            ),
            format!(
                "SELECT c.region, COUNT(*) * 1.0 / (SELECT COUNT(*) FROM orders) AS share \
                 {raw} GROUP BY c.region"
            ),
        ),
    ] {
        let c = ossiec::compile_sql(&g, &sql, &duck()).unwrap_or_else(|e| panic!("{sql}: {e}"));
        let want = sorted(duckdb_rows(&db, &reference));
        assert!(want.len() > 1, "{sql}: proves nothing");
        assert_eq!(sorted(duckdb_rows(&db, &c.sql)), want, "{sql}\n{}", c.sql);
    }
    for (sql, want) in [
        (
            format!("SELECT c.region, (SELECT o2.id, o2.status FROM orders o2 LIMIT 1) AS x {from} GROUP BY c.region"),
            "one column",
        ),
        // `c.segment` has many values in a region's row.
        (
            format!(
                "SELECT c.region, (SELECT AGG(c2.known_customers) FROM customers c2 \
                 WHERE c2.segment = c.segment) AS x {from} GROUP BY c.region"
            ),
            "not a selected dimension",
        ),
        (
            format!(
                "SELECT t.region, (SELECT AGG(o2.order_count) FROM orders o2) AS x \
                 FROM (SELECT c.region, AGG(o.order_count) AS n {from} GROUP BY c.region) t"
            ),
            "plain SQL over derived tables",
        ),
        (
            format!(
                "SELECT t.region FROM (SELECT c.region, (SELECT AGG(o2.order_count) FROM orders o2) \
                 {from} GROUP BY c.region) t"
            ),
            "explicit alias",
        ),
        // The same subquery nested in another one.
        (
            format!(
                "SELECT c.region, (SELECT AGG(o2.order_count) FROM orders o2) + \
                 (SELECT AGG(o3.order_count) FROM orders o3 \
                  WHERE o3.id > (SELECT AGG(o2.order_count) FROM orders o2)) AS x {from} GROUP BY c.region"
            ),
            "write it once",
        ),
    ] {
        let e = ossiec::compile_sql(&g, &sql, &duck())
            .expect_err(&sql)
            .to_string();
        assert!(e.contains(want), "{sql}: {e}");
    }
}

/// §7.5, §7.7: `HAVING` and `ORDER BY` read a scalar subquery as the select list
/// does, alone or in arithmetic with `AGG()`, correlated with the row's
/// dimensions or not. Each answer is the same query in plain SQL.
#[test]
fn having_and_order_by_read_subqueries() {
    let g = ossiec::from_yaml(common::MODEL).unwrap();
    let db = duckdb();
    let customers = "(SELECT * FROM raw_customers WHERE NOT deleted)";
    let from = "FROM orders o JOIN customers c ON o.customer_id = c.id";
    let raw = format!("FROM orders o JOIN {customers} c ON o.customer_id = c.customer_id");
    let smb = |customers: &str, count: &str| {
        format!(
            "(SELECT {count} FROM {customers} c2 WHERE c2.region = c.region AND c2.segment = 'smb')"
        )
    };
    let ours = smb("customers", "AGG(c2.known_customers)");
    let plain = smb(customers, "COUNT(c2.customer_id)");
    for (rest, reference, ordered) in [
        // Regions with more than a quarter of all orders.
        (
            "HAVING AGG(o.order_count) * 4 > (SELECT AGG(o2.order_count) FROM orders o2)".to_string(),
            "HAVING COUNT(*) * 4 > (SELECT COUNT(*) FROM orders)".to_string(),
            false,
        ),
        // More orders than small customers there (0 where none).
        (
            format!("HAVING AGG(o.order_count) > {ours}"),
            format!("HAVING COUNT(*) > {plain}"),
            false,
        ),
        // A conjunct that is only a subquery reads the group, not the rows.
        (
            format!("HAVING {ours} = 0"),
            format!("HAVING {plain} = 0"),
            false,
        ),
        (
            format!("ORDER BY {ours} DESC, c.region LIMIT 2"),
            format!("ORDER BY {plain} DESC, c.region NULLS FIRST LIMIT 2"),
            true,
        ),
        // The same subquery twice, side by side: one value, read twice.
        (
            "HAVING AGG(o.order_count) * 4 > (SELECT AGG(o2.order_count) FROM orders o2) \
             AND AGG(o.order_count) < (SELECT AGG(o2.order_count) FROM orders o2)"
                .to_string(),
            "HAVING COUNT(*) * 4 > (SELECT COUNT(*) FROM orders) \
             AND COUNT(*) < (SELECT COUNT(*) FROM orders)"
                .to_string(),
            false,
        ),
        // A key that is a subquery alone, read by no other column: the same
        // for every row, so the next key orders them.
        (
            "ORDER BY (SELECT AGG(o2.order_count) FROM orders o2), c.region".to_string(),
            "ORDER BY (SELECT COUNT(*) FROM orders), c.region NULLS FIRST".to_string(),
            true,
        ),
        (
            "ORDER BY AGG(o.order_count) * 1.0 / (SELECT AGG(o2.order_count) FROM orders o2) DESC, c.region"
                .to_string(),
            "ORDER BY COUNT(*) * 1.0 / (SELECT COUNT(*) FROM orders) DESC, c.region NULLS FIRST"
                .to_string(),
            true,
        ),
    ] {
        let sql = format!("SELECT c.region, AGG(o.order_count) AS n {from} GROUP BY c.region {rest}");
        let c = ossiec::compile_sql(&g, &sql, &duck()).unwrap_or_else(|e| panic!("{rest}: {e}"));
        let reference = format!("SELECT c.region, COUNT(*) AS n {raw} GROUP BY c.region {reference}");
        let (got, want) = if ordered {
            (duckdb_rows(&db, &c.sql), duckdb_rows(&db, &reference))
        } else {
            (sorted(duckdb_rows(&db, &c.sql)), sorted(duckdb_rows(&db, &reference)))
        };
        assert!(!want.is_empty(), "{rest}: proves nothing");
        let all = sorted(duckdb_rows(&db, &format!("SELECT c.region, COUNT(*) AS n {raw} GROUP BY c.region")));
        if !ordered {
            assert_ne!(want, all, "{rest}: keeps every group, proves nothing");
        }
        assert_eq!(got, want, "{rest}\n{}", c.sql);
    }
    for (rest, want) in [
        (
            "HAVING AGG(o.order_count) = ANY (SELECT AGG(o2.order_count) FROM orders o2)",
            "HAVING takes a subquery as a scalar value",
        ),
        (
            "HAVING AGG(o.order_count) > (SELECT AGG(c2.known_customers) FROM customers c2 \
             WHERE c2.segment = c.segment)",
            "not a selected dimension",
        ),
        (
            "ORDER BY (SELECT o2.id, o2.status FROM orders o2 LIMIT 1)",
            "ORDER BY must return one column",
        ),
    ] {
        let sql =
            format!("SELECT c.region, AGG(o.order_count) AS n {from} GROUP BY c.region {rest}");
        let e = ossiec::compile_sql(&g, &sql, &duck())
            .expect_err(rest)
            .to_string();
        assert!(e.contains(want), "{rest}: {e}");
    }
    // StarRocks orders by keys computed in a derived table: their names differ
    // from every output's, even one named like them.
    let sql = format!(
        "SELECT c.region AS __ossiec_ord0, AGG(o.order_count) AS n {from} GROUP BY c.region \
         ORDER BY (SELECT AGG(o2.order_count) FROM orders o2), c.region"
    );
    let opts = ossiec::Options {
        target: Some(polyglot_sql::DialectType::StarRocks),
        ..Default::default()
    };
    let c = ossiec::compile_sql(&g, &sql, &opts).unwrap_or_else(|e| panic!("{e}"));
    assert!(c.sql.contains("__ossiec_ord0_"), "{}", c.sql);
    assert_eq!(c.sql.matches("AS `__ossiec_ord0`").count(), 2, "{}", c.sql);
}

/// §7.7: `[NOT] EXISTS` and `x [NOT] IN` read a subquery in the select list,
/// `HAVING` and `ORDER BY` as in `WHERE`, correlated with the row's dimensions
/// or not. Each answer is the same query in plain SQL, so SQL's own semantics
/// are the reference, `NOT IN` over a NULL among them.
#[test]
fn exists_and_in_outside_where() {
    let g = ossiec::from_yaml(common::MODEL).unwrap();
    let db = duckdb();
    let customers = "(SELECT * FROM raw_customers WHERE NOT deleted)";
    let from = "FROM orders o JOIN customers c ON o.customer_id = c.id";
    let raw = format!("FROM orders o JOIN {customers} c ON o.customer_id = c.customer_id");
    let smb = |customers: &str, id: &str| {
        format!(
            "(SELECT c2.{id} FROM {customers} c2 WHERE c2.region = c.region AND c2.segment = 'smb')"
        )
    };
    let (ours, plain) = (smb("customers", "id"), smb(customers, "customer_id"));
    let smb_regions = |customers: &str| {
        format!("(SELECT c2.region FROM {customers} c2 WHERE c2.segment = 'smb')")
    };
    let returned = "(SELECT o2.product_id FROM orders o2 \
                    WHERE o2.customer_id = o.customer_id AND o2.status = 'returned')";
    for (sql, reference, ordered) in [
        // The select list: each region, whether it has a small customer.
        (
            format!("SELECT c.region, EXISTS {ours} AS has_smb {from} GROUP BY c.region"),
            format!("SELECT c.region, EXISTS {plain} AS has_smb {raw} GROUP BY c.region"),
            false,
        ),
        (
            format!(
                "SELECT c.region, AGG(o.order_count) AS n, NOT EXISTS {ours} AS no_smb {from} GROUP BY c.region"
            ),
            format!(
                "SELECT c.region, COUNT(*) AS n, NOT EXISTS {plain} AS no_smb {raw} GROUP BY c.region"
            ),
            false,
        ),
        (
            format!(
                "SELECT c.region, c.region IN {} AS smb_region {from} GROUP BY c.region",
                smb_regions("customers")
            ),
            format!(
                "SELECT c.region, c.region IN {} AS smb_region {raw} GROUP BY c.region",
                smb_regions(customers)
            ),
            false,
        ),
        // Correlated: each customer's products, whether they returned one.
        (
            format!(
                "SELECT o.customer_id, o.product_id, o.product_id IN {returned} AS r \
                 FROM orders o GROUP BY o.customer_id, o.product_id"
            ),
            format!(
                "SELECT o.customer_id, o.product_id, o.product_id IN {returned} AS r \
                 FROM orders o GROUP BY o.customer_id, o.product_id"
            ),
            false,
        ),
        // NOT IN with a NULL region among the values: NULL or FALSE, never TRUE.
        (
            format!(
                "SELECT c.region, c.region NOT IN {} AS a {from} GROUP BY c.region",
                smb_regions("customers")
            ),
            format!(
                "SELECT c.region, c.region NOT IN {} AS a {raw} GROUP BY c.region",
                smb_regions(customers)
            ),
            false,
        ),
        // Correlated NOT IN: customer 7's product is NULL.
        (
            format!(
                "SELECT o.customer_id, o.product_id, o.product_id NOT IN {returned} AS kept \
                 FROM orders o GROUP BY o.customer_id, o.product_id"
            ),
            format!(
                "SELECT o.customer_id, o.product_id, o.product_id NOT IN {returned} AS kept \
                 FROM orders o GROUP BY o.customer_id, o.product_id"
            ),
            false,
        ),
        // HAVING: the regions with a small customer, and those without.
        (
            format!("SELECT c.region, AGG(o.order_count) AS n {from} GROUP BY c.region HAVING EXISTS {ours}"),
            format!("SELECT c.region, COUNT(*) AS n {raw} GROUP BY c.region HAVING EXISTS {plain}"),
            false,
        ),
        (
            format!(
                "SELECT c.region, AGG(o.order_count) AS n {from} GROUP BY c.region \
                 HAVING NOT EXISTS {ours} AND AGG(o.order_count) > 0"
            ),
            format!(
                "SELECT c.region, COUNT(*) AS n {raw} GROUP BY c.region \
                 HAVING NOT EXISTS {plain} AND COUNT(*) > 0"
            ),
            false,
        ),
        (
            format!(
                "SELECT c.region, AGG(o.order_count) AS n {from} GROUP BY c.region HAVING c.region NOT IN {}",
                smb_regions("customers").replace("smb", "enterprise")
            ),
            format!(
                "SELECT c.region, COUNT(*) AS n {raw} GROUP BY c.region HAVING c.region NOT IN {}",
                smb_regions(customers).replace("smb", "enterprise")
            ),
            false,
        ),
        // ORDER BY: regions with a small customer first.
        (
            format!(
                "SELECT c.region, AGG(o.order_count) AS n {from} GROUP BY c.region \
                 ORDER BY EXISTS {ours} DESC, c.region"
            ),
            format!(
                "SELECT c.region, COUNT(*) AS n {raw} GROUP BY c.region \
                 ORDER BY EXISTS {plain} DESC, c.region NULLS FIRST"
            ),
            true,
        ),
    ] {
        let c = ossiec::compile_sql(&g, &sql, &duck()).unwrap_or_else(|e| panic!("{sql}: {e}"));
        let (got, want) = if ordered {
            (duckdb_rows(&db, &c.sql), duckdb_rows(&db, &reference))
        } else {
            (sorted(duckdb_rows(&db, &c.sql)), sorted(duckdb_rows(&db, &reference)))
        };
        assert!(!want.is_empty(), "{sql}: proves nothing");
        assert_eq!(got, want, "{sql}\n{}", c.sql);
    }
    for (rest, want) in [
        (
            "HAVING AGG(o.order_count) IN (SELECT AGG(o2.order_count) FROM orders o2 GROUP BY o2.status)",
            "left side of `IN (SELECT …)` reads the grouping columns only",
        ),
        // No GROUP BY: one row for every region, matched or not.
        (
            "HAVING EXISTS (SELECT AGG(c2.known_customers) FROM customers c2 WHERE c2.region = c.region)",
            "as a scalar value",
        ),
        (
            "ORDER BY c.region IN (SELECT c2.region, c2.segment FROM customers c2)",
            "must return one column",
        ),
        (
            "HAVING EXISTS (SELECT c2.id FROM customers c2 WHERE c2.segment = c.segment)",
            "not a selected dimension",
        ),
    ] {
        let sql =
            format!("SELECT c.region, AGG(o.order_count) AS n {from} GROUP BY c.region {rest}");
        let e = ossiec::compile_sql(&g, &sql, &duck())
            .expect_err(rest)
            .to_string();
        assert!(e.contains(want), "{rest}: {e}");
    }
}

/// §7.3: a correlated subquery in `WHERE` reads the outer row through `=` in its
/// own `WHERE`. Each answer is the same correlated query in plain SQL over the
/// source tables, so SQL's own semantics are the reference: NULL keys, `NOT IN`,
/// and an aggregate over no matching rows (`COUNT` is 0, not NULL).
#[test]
fn where_reads_correlated_subqueries() {
    let g = ossiec::from_yaml(common::MODEL).unwrap();
    let db = duckdb();
    let customers = "(SELECT * FROM raw_customers WHERE NOT deleted)";
    for (predicate, reference) in [
        (
            "EXISTS (SELECT c.id FROM customers c WHERE c.id = orders.customer_id AND c.region = 'EU')",
            format!("EXISTS (SELECT 1 FROM {customers} c WHERE c.customer_id = orders.customer_id AND c.region = 'EU')"),
        ),
        // The outer column on the left, and a guest order (no customer) kept.
        (
            "NOT EXISTS (SELECT c.id FROM customers c WHERE orders.customer_id = c.id AND c.region = 'EU')",
            format!("NOT EXISTS (SELECT 1 FROM {customers} c WHERE orders.customer_id = c.customer_id AND c.region = 'EU')"),
        ),
        (
            "product_id IN (SELECT o2.product_id FROM orders o2 \
             WHERE o2.customer_id = orders.customer_id AND o2.status = 'returned')",
            "product_id IN (SELECT o2.product_id FROM orders o2 \
             WHERE o2.customer_id = orders.customer_id AND o2.status = 'returned')"
                .to_string(),
        ),
        (
            "product_id NOT IN (SELECT o2.product_id FROM orders o2 \
             WHERE o2.customer_id = orders.customer_id AND o2.status = 'returned')",
            "product_id NOT IN (SELECT o2.product_id FROM orders o2 \
             WHERE o2.customer_id = orders.customer_id AND o2.status = 'returned')"
                .to_string(),
        ),
        // Two correlations.
        (
            "EXISTS (SELECT o2.id FROM orders o2 WHERE o2.customer_id = orders.customer_id \
             AND o2.status = orders.status AND o2.id <> 101)",
            "EXISTS (SELECT 1 FROM orders o2 WHERE o2.customer_id = orders.customer_id \
             AND o2.status = orders.status AND o2.order_id <> 101)"
                .to_string(),
        ),
        // Grouped, with HAVING on a measure: customers with more than one order.
        (
            "EXISTS (SELECT o2.customer_id FROM orders o2 WHERE o2.customer_id = orders.customer_id \
             GROUP BY o2.customer_id HAVING AGG(o2.order_count) > 1)",
            "EXISTS (SELECT 1 FROM orders o2 WHERE o2.customer_id = orders.customer_id \
             GROUP BY o2.customer_id HAVING COUNT(*) > 1)"
                .to_string(),
        ),
        // Grouped by the correlated column without selecting it: the key is
        // already part of the grain.
        (
            "EXISTS (SELECT AGG(o2.order_count) AS n FROM orders o2 WHERE o2.customer_id = orders.customer_id \
             GROUP BY o2.customer_id HAVING AGG(o2.order_count) > 1)",
            "EXISTS (SELECT COUNT(*) FROM orders o2 WHERE o2.customer_id = orders.customer_id \
             GROUP BY o2.customer_id HAVING COUNT(*) > 1)"
                .to_string(),
        ),
        // A scalar: orders above their customer's average.
        (
            "revenue > (SELECT AVG(o2.revenue) FROM orders o2 WHERE o2.customer_id = orders.customer_id)",
            "amount_cents / 100.0 > (SELECT AVG(o2.amount_cents / 100.0) FROM orders o2 \
             WHERE o2.customer_id = orders.customer_id)"
                .to_string(),
        ),
        // A measure over no matching rows is the measure over none: 0, so orders
        // outside the EU (and the guest order) stay.
        (
            "(SELECT AGG(c.known_customers) FROM customers c \
             WHERE c.id = orders.customer_id AND c.region = 'EU') = 0",
            format!(
                "(SELECT COUNT(c.customer_id) FROM {customers} c \
                 WHERE c.customer_id = orders.customer_id AND c.region = 'EU') = 0"
            ),
        ),
        (
            "1 < (SELECT AGG(o2.order_count) FROM orders o2 WHERE o2.customer_id = orders.customer_id)",
            "1 < (SELECT COUNT(*) FROM orders o2 WHERE o2.customer_id = orders.customer_id)"
                .to_string(),
        ),
        // A query nested in the subquery names `orders` itself; that doesn't
        // hide the subquery's own correlation with the outer `orders`.
        (
            "EXISTS (SELECT c.id FROM customers c WHERE c.id = orders.customer_id \
             AND c.id IN (SELECT orders.customer_id FROM orders WHERE orders.status = 'returned'))",
            format!(
                "EXISTS (SELECT 1 FROM {customers} c WHERE c.customer_id = orders.customer_id \
                 AND c.customer_id IN (SELECT o3.customer_id FROM orders o3 WHERE o3.status = 'returned'))"
            ),
        ),
        // The subquery names `orders` itself, so `orders.id` is its own column, as
        // in SQL: not correlated.
        (
            "id IN (SELECT orders.id FROM orders WHERE orders.status = 'returned')",
            "order_id IN (SELECT order_id FROM orders WHERE status = 'returned')".to_string(),
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
        let all = sorted(duckdb_rows(
            &db,
            "SELECT status, COUNT(*) FROM orders GROUP BY status",
        ));
        assert_ne!(got, all, "{predicate}: keeps every row, proves nothing");
    }
    for (predicate, want) in [
        (
            "EXISTS (SELECT o2.id FROM orders o2 WHERE o2.id < orders.id)",
            "reads `orders.id`",
        ),
        (
            "EXISTS (SELECT o2.id FROM orders o2 WHERE o2.customer_id = orders.customer_id OR o2.id = 1)",
            "reads `orders.customer_id`",
        ),
        (
            "1 < (SELECT MAX(orders.id) FROM customers c)",
            "outside",
        ),
        (
            "EXISTS (SELECT o2.id FROM orders o2 WHERE o2.customer_id = orders.customer_id LIMIT 1)",
            "LIMIT",
        ),
        (
            "EXISTS (SELECT AGG(o2.order_count) FROM orders o2 WHERE o2.customer_id = orders.customer_id)",
            "as a scalar value",
        ),
        (
            "1 < (SELECT AGG(o2.order_count) FROM orders o2 WHERE o2.customer_id = orders.customer_id \
             HAVING AGG(o2.order_count) > 2)",
            "HAVING",
        ),
        // A subtotal row has no key to match: refused, not dropped.
        (
            "product_id NOT IN (SELECT o2.product_id FROM orders o2 \
             WHERE o2.customer_id = orders.customer_id GROUP BY ROLLUP(o2.product_id))",
            "ROLLUP, CUBE or GROUPING SETS",
        ),
        // Two levels out: the inner subquery reads the outermost query.
        (
            "customer_id IN (SELECT c.id FROM customers c WHERE c.id IN \
             (SELECT o2.customer_id FROM orders o2 WHERE o2.status = orders.status))",
            "reads `orders.status`",
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
    // Correlated with a joined dataset: orders of customers with a returned order.
    let compiled = ossiec::compile_sql(
        &g,
        "SELECT o.status, AGG(o.order_count) FROM orders o JOIN customers c ON o.customer_id = c.id \
         WHERE EXISTS (SELECT o2.id FROM orders o2 WHERE o2.customer_id = c.id AND o2.status = 'returned') \
         GROUP BY o.status",
        &duck(),
    )
    .unwrap();
    let want = sorted(duckdb_rows(
        &db,
        &format!(
            "SELECT o.status, COUNT(*) FROM orders o JOIN {customers} c ON o.customer_id = c.customer_id \
             WHERE EXISTS (SELECT 1 FROM orders o2 WHERE o2.customer_id = c.customer_id AND o2.status = 'returned') \
             GROUP BY o.status"
        ),
    ));
    assert!(!want.is_empty());
    assert_eq!(
        sorted(duckdb_rows(&db, &compiled.sql)),
        want,
        "{}",
        compiled.sql
    );
    // Each grouping set's stage compiles the correlated subquery with it.
    let compiled = ossiec::compile_sql(
        &g,
        "SELECT c.region, o.status, AGG(o.order_count) AS n FROM orders o JOIN customers c ON o.customer_id = c.id \
         WHERE EXISTS (SELECT o2.id FROM orders o2 WHERE o2.customer_id = o.customer_id AND o2.status = 'returned') \
         OR o.product_id = 12 GROUP BY ROLLUP(c.region, o.status)",
        &duck(),
    )
    .unwrap();
    let want = sorted(duckdb_rows(
        &db,
        &format!(
            "SELECT c.region, o.status, COUNT(*) AS n FROM orders o JOIN {customers} c ON o.customer_id = c.customer_id \
             WHERE EXISTS (SELECT 1 FROM orders o2 WHERE o2.customer_id = o.customer_id AND o2.status = 'returned') \
             OR o.product_id = 12 GROUP BY ROLLUP(c.region, o.status)"
        ),
    ));
    assert!(want.len() > 2);
    assert_eq!(
        sorted(duckdb_rows(&db, &compiled.sql)),
        want,
        "{}",
        compiled.sql
    );
    // A derived table that AGG() folds away can't be read by a subquery.
    let e = ossiec::compile_sql(
        &g,
        "SELECT s, AGG(m) FROM (SELECT status AS s, order_count AS m, customer_id AS cid FROM orders) t \
         WHERE EXISTS (SELECT c.id FROM customers c WHERE c.id = t.cid) GROUP BY s",
        &duck(),
    )
    .unwrap_err()
    .to_string();
    assert!(e.contains("folds into the query"), "{e}");
    // A stand-in's column written by the consumer names no relation.
    let e = ossiec::compile_sql(
        &g,
        "SELECT status, AGG(order_count) FROM orders WHERE __ossiec_sq0.id = 1 GROUP BY status",
        &duck(),
    )
    .unwrap_err()
    .to_string();
    assert!(e.contains("does not refer to a relation"), "{e}");
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
    // A subquery shaped like ossiec's stand-in for a compiled one is nothing of
    // the kind when the consumer writes it: in WHERE it is compiled as a query
    // (and has no such dataset), in HAVING too, and sql_to_request takes none.
    let stand_in = "status IN (SELECT * FROM __ossiec_deriv0 AS x)";
    for sql in [
        format!("SELECT status, AGG(order_count) FROM orders WHERE {stand_in} GROUP BY status"),
        format!("SELECT status, AGG(order_count) FROM orders GROUP BY status HAVING {stand_in}"),
    ] {
        let e = ossiec::compile_sql(&g, &sql, &duck())
            .expect_err(&sql)
            .to_string();
        assert!(!e.contains("internal"), "{sql}: {e}");
    }
    let e = ossiec::compile_sql(
        &g,
        &format!("SELECT status, AGG(order_count) FROM orders GROUP BY status HAVING {stand_in}"),
        &duck(),
    )
    .unwrap_err()
    .to_string();
    assert!(e.contains("`SELECT *` is not supported"), "{e}");
    for predicate in [
        stand_in,
        "customer_id IN (SELECT c.id FROM customers c WHERE c.segment = 'smb')",
    ] {
        let sql = format!(
            "SELECT status, AGG(order_count) FROM orders WHERE {predicate} GROUP BY status"
        );
        let e = ossiec::sql_to_request(&g, &sql)
            .expect_err(predicate)
            .to_string();
        assert!(e.contains("compiled by compile_sql"), "{predicate}: {e}");
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
        "needs a FROM clause",
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
        "needs a FROM clause",
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
