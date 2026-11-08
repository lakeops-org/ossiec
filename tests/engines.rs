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

use common::{
    comparable, compile, duckdb, duckdb_rows, run, seed_shop as seed, Engine, Layer2NestedCase,
    CASES, LAYER2_NESTED_CASES, LAYER2_TWO_FACT_CASES, TWO_FACT_SHOP,
};

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

/// Layer 2 nested shop forms on `engine`, against the DuckDB reference.
/// Cases live in [`common::LAYER2_NESTED_CASES`] (A.2 / A.3 / §7.7 / base
/// `TO MEASURE` / `includeUnmatched`+derived / A.7 script).
fn sweep_layer2_nested(engine: Engine) {
    seed(engine);
    let mut failures = vec![];
    for (model, cases) in [
        (common::MODEL, LAYER2_NESTED_CASES),
        (TWO_FACT_SHOP, LAYER2_TWO_FACT_CASES),
    ] {
        sweep_cases(engine, model, cases, &mut failures);
    }
    assert!(
        failures.is_empty(),
        "{engine:?}:\n\n{}",
        failures.join("\n\n")
    );
}

fn sweep_cases(
    engine: Engine,
    model: &str,
    cases: &[Layer2NestedCase],
    failures: &mut Vec<String>,
) {
    let db = duckdb();
    let g = ossiec::from_yaml(model).unwrap();
    let opts = ossiec::Options {
        target: Some(engine.dialect()),
        ..Default::default()
    };
    for case in cases {
        let compiled = if case.script {
            ossiec::compile_sql_script(&g, case.sql, &opts)
        } else {
            ossiec::compile_sql(&g, case.sql, &opts)
        }
        .unwrap_or_else(|e| panic!("{}: {e}", case.name));
        let mut want = duckdb_rows(&db, case.reference);
        want.sort();
        match run(engine, &compiled.sql) {
            Ok(mut got) => {
                got.sort();
                if got != want {
                    failures.push(format!(
                        "{}: wrong rows\n  got:  {got:?}\n  want: {want:?}\n{}",
                        case.name, compiled.sql
                    ));
                }
            }
            Err(e) => failures.push(format!("{}: {e}\n{}", case.name, compiled.sql)),
        }
    }
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
fn layer2_nested_athena() {
    sweep_layer2_nested(Engine::Athena);
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

/// Athena's engine (v3) is Trino, so its SQL runs on the Trino container.
#[test]
#[ignore = "needs Docker: scripts/engines.sh up"]
fn athena() {
    sweep(Engine::Athena);
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
