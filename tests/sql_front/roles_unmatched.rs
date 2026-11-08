//! Roles and `includeUnmatched`: role-playing dimensions (`relationship.field`),
//! and the one-side rows no fact reaches.

use super::*;

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
