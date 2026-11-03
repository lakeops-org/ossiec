//! Engine tier: every correctness case, compiled for and executed on real
//! engines, compared with the DuckDB reference answer.
//!
//! Needs Docker. Ignored by default:
//!
//! ```console
//! scripts/engines.sh up
//! cargo test --test engines -- --ignored
//! ```

mod common;

use std::sync::Mutex;

use common::{
    comparable, compile, duckdb, duckdb_rows, run, seed_sql, Engine, CASES, LAYER2_NESTED_CASES,
};

static SEEDED: Mutex<Vec<Engine>> = Mutex::new(Vec::new());

fn seed(engine: Engine) {
    let mut done = SEEDED.lock().unwrap();
    if !done.contains(&engine) {
        let sql = seed_sql(engine);
        let sql = if engine == Engine::Trino {
            sql.trim_end().trim_end_matches(';').to_string()
        } else {
            sql
        };
        run(engine, &sql).unwrap_or_else(|e| {
            panic!("seeding {engine:?} failed (is `scripts/engines.sh up` running?): {e}")
        });
        done.push(engine);
    }
}

/// Run every case on `engine`; report all mismatches at once.
fn sweep(engine: Engine) {
    seed(engine);
    let db = duckdb();
    let mut failures = vec![];
    for c in CASES {
        let (req, compiled) = compile(c, engine);
        let want = comparable(&req, duckdb_rows(&db, c.reference));
        match run(engine, &compiled.sql) {
            Ok(rows) => {
                let got = comparable(&req, rows);
                if got != want {
                    failures.push(format!(
                        "{}: wrong rows\n  got:  {got:?}\n  want: {want:?}\n{}",
                        c.name, compiled.sql
                    ));
                }
            }
            Err(e) => failures.push(format!("{}: {e}\n{}", c.name, compiled.sql)),
        }
    }
    assert!(
        failures.is_empty(),
        "{engine:?}: {} of {} cases failed\n\n{}",
        failures.len(),
        CASES.len(),
        failures.join("\n\n")
    );
}

/// Layer 2 nested shop forms (A.2 / A.3 / §7.7) on `engine`, against the DuckDB reference.
fn sweep_layer2_nested(engine: Engine) {
    seed(engine);
    let db = duckdb();
    let g = ossiec::from_yaml(common::MODEL).unwrap();
    let opts = ossiec::Options {
        target: Some(engine.dialect()),
        ..Default::default()
    };
    let mut failures = vec![];
    for (name, layer2, reference) in LAYER2_NESTED_CASES {
        let compiled = ossiec::compile_layer2(&g, layer2, &opts).unwrap();
        let mut want = duckdb_rows(&db, reference);
        want.sort();
        match run(engine, &compiled.sql) {
            Ok(mut got) => {
                got.sort();
                if got != want {
                    failures.push(format!(
                        "{name}: wrong rows\n  got:  {got:?}\n  want: {want:?}\n{}",
                        compiled.sql
                    ));
                }
            }
            Err(e) => failures.push(format!("{name}: {e}\n{}", compiled.sql)),
        }
    }
    assert!(
        failures.is_empty(),
        "{engine:?}:\n\n{}",
        failures.join("\n\n")
    );
}

#[test]
#[ignore = "needs Docker: scripts/engines.sh up"]
fn layer2_nested_postgres() {
    sweep_layer2_nested(Engine::Postgres);
}

#[test]
#[ignore = "needs Docker: scripts/engines.sh up"]
fn layer2_nested_trino() {
    sweep_layer2_nested(Engine::Trino);
}

#[test]
#[ignore = "needs Docker: scripts/engines.sh up"]
fn layer2_nested_starrocks() {
    sweep_layer2_nested(Engine::StarRocks);
}

#[test]
#[ignore = "needs Docker: scripts/engines.sh up"]
fn layer2_nested_clickhouse() {
    sweep_layer2_nested(Engine::ClickHouse);
}

#[test]
#[ignore = "needs Docker: scripts/engines.sh up"]
fn layer2_nested_mysql() {
    sweep_layer2_nested(Engine::MySql);
}

#[test]
#[ignore = "needs Docker: scripts/engines.sh up"]
fn postgres() {
    sweep(Engine::Postgres);
}

#[test]
#[ignore = "needs Docker: scripts/engines.sh up"]
fn trino() {
    sweep(Engine::Trino);
}

#[test]
#[ignore = "needs Docker: scripts/engines.sh up"]
fn starrocks() {
    sweep(Engine::StarRocks);
}

#[test]
#[ignore = "needs Docker: scripts/engines.sh up"]
fn clickhouse() {
    sweep(Engine::ClickHouse);
}

#[test]
#[ignore = "needs Docker: scripts/engines.sh up"]
fn mysql() {
    sweep(Engine::MySql);
}
