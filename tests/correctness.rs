//! Correctness tier: compile requests, run them on DuckDB, and compare the rows
//! with hand-written reference SQL. A golden file can only say "same as last
//! time"; these tests say "right". The cases live in `common`, where the engine
//! tier (`tests/engines.rs`) reuses them on Trino, StarRocks, ClickHouse and Postgres.

mod common;

use common::{case, comparable, compile, duckdb, duckdb_rows, Engine, CASES};

/// Run a case on DuckDB and compare with its reference. Returns the rows.
fn check(name: &str) -> Vec<Vec<String>> {
    let c = case(name);
    let (req, compiled) = compile(c, Engine::DuckDb);
    let db = duckdb();
    let got = comparable(&req, duckdb_rows(&db, &compiled.sql));
    let want = comparable(&req, duckdb_rows(&db, c.reference));
    assert_eq!(got, want, "\n{name}: compiled SQL:\n{}\n", compiled.sql);
    assert!(
        !want.is_empty(),
        "{name}: reference returned no rows; the test proves nothing"
    );
    got
}

fn naive(sql: &str) -> Vec<Vec<String>> {
    let mut rows = duckdb_rows(&duckdb(), sql);
    rows.sort();
    rows
}

#[test]
fn every_case_matches_its_reference() {
    for c in CASES {
        check(c.name);
    }
}

#[test]
fn null_and_orphan_rows_land_in_the_null_group() {
    // Dee has no region; the guest checkout and the deleted customer's order
    // find no customer row. All three stay, in the NULL group.
    let got = check("revenue_by_region");
    assert!(got.iter().any(|r| r[0] == "NULL"));
}

#[test]
fn one_side_measure_beats_the_naive_join() {
    // Stock lives on products. Grouped by customer region, each product sold
    // into a region counts once, however many orders it had there.
    let got = check("one_side_measure_dedup");
    let fanned_out = naive(
        "SELECT c.region, SUM(p.stock) FROM orders o
         LEFT JOIN {customers} c ON o.customer_id = c.customer_id
         LEFT JOIN products p ON o.product_id = p.product_id
         GROUP BY c.region",
    );
    assert_ne!(
        got, fanned_out,
        "the naive join should over-count; otherwise this test proves nothing"
    );
}

#[test]
fn measure_alone_counts_the_never_sold_product() {
    assert_eq!(check("measure_alone_at_its_grain"), [["187.000000"]]);
}

#[test]
fn order_and_limit_are_applied() {
    assert_eq!(check("metric_filter_order_limit")[0][0], "Cyd");
}

#[test]
fn rank_filter_keeps_one_top_category_per_region() {
    assert_eq!(
        check("rank_filter_after_window").len(),
        4,
        "EU, US, APAC and the unknown region"
    );
}
