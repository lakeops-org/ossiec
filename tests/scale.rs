//! Scale: compile time on a large generated model.
//! (Query run time on TPC-DS sf=1 is in `tests/tpcds.rs`, `trino_sf1_performance`.)

use std::time::{Duration, Instant};

const DIMS: usize = 120;
const ONE_SIDE_METRICS: usize = 40;

/// A star: one fact joined to 120 tables, each with a key and two fields;
/// 40 metrics on joined tables, each its own branch.
fn big_model() -> String {
    let mut y = String::from("version: \"0.2.0.dev0\"\nname: big\ndatasets:\n  - name: fact\n    source: f\n    primary_key: [id]\n    fields:\n      - { name: id, expression: { sql: id } }\n      - { name: amount, expression: { sql: amount } }\n");
    for k in 0..DIMS {
        y.push_str(&format!(
            "      - {{ name: fk{k}, expression: {{ sql: fk{k} }} }}\n"
        ));
    }
    for k in 0..DIMS {
        y.push_str(&format!(
            "  - name: dim{k}\n    source: d{k}\n    primary_key: [key]\n    fields:\n      - {{ name: key, expression: {{ sql: k }} }}\n      - {{ name: label, expression: {{ sql: label }}, datatype: String }}\n      - {{ name: value, expression: {{ sql: v }} }}\n"
        ));
    }
    y.push_str("relationships:\n");
    for k in 0..DIMS {
        y.push_str(&format!("  - {{ name: r{k}, from: fact, to: dim{k}, from_columns: [fk{k}], to_columns: [key] }}\n"));
    }
    y.push_str("metrics:\n  - { name: total, expression: { sql: SUM(fact.amount) } }\n");
    for k in 0..ONE_SIDE_METRICS {
        y.push_str(&format!(
            "  - {{ name: v{k}, expression: {{ sql: SUM(dim{k}.value) }} }}\n"
        ));
    }
    y
}

#[test]
fn large_model_binds_and_compiles_quickly() {
    let yaml = big_model();
    let t = Instant::now();
    let graph = ossiec::from_yaml(&yaml).unwrap();
    let bind = t.elapsed();

    let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut next = |n: usize| {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        (x % n as u64) as usize
    };
    let requests = 200;
    let t = Instant::now();
    for _ in 0..requests {
        // A joined table's metric reaches only that table, so a request with one
        // and a dimension of the same table gets a branch for each.
        let k = next(ONE_SIDE_METRICS);
        let dims = vec![format!("dim{k}.label").into()];
        let metrics = vec!["total".into(), format!("v{k}").into()];
        let req = ossiec::Request {
            metrics,
            dimensions: dims,
            ..Default::default()
        };
        ossiec::compile(
            &graph,
            &req,
            &ossiec::Options {
                target: Some(ossiec::DialectType::Trino),
                ..Default::default()
            },
        )
        .unwrap();
    }
    let per_request = t.elapsed() / requests;
    println!("bind: {bind:?}, compile: {per_request:?} per request (121 datasets, 1 dim, 2 metrics, 2 branches)");
    // Generous ceilings for unoptimized test builds: they catch a quadratic or
    // exponential regression, not small drifts.
    assert!(bind < Duration::from_secs(5), "bind took {bind:?}");
    assert!(
        per_request < Duration::from_millis(250),
        "compile took {per_request:?} per request"
    );
}
