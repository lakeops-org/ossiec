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

use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::Mutex;

use common::{comparable, compile, duckdb, duckdb_rows, norm_text, seed_sql, Engine, CASES};

/// Run SQL through the engine's own CLI inside its container. Every client prints
/// NULL distinctly from '' (psql `-P null=NULL`, ClickHouse `\\N`, mysql `NULL`, Trino JSON `null`).
fn run(engine: Engine, sql: &str) -> Result<Vec<Vec<String>>, String> {
    let (container, args, stdin): (&str, Vec<&str>, Option<String>) = match engine {
        Engine::Postgres => (
            "ossiec-postgres",
            vec![
                "psql",
                "-U",
                "postgres",
                "-X",
                "-q",
                "-At",
                "-F",
                "\t",
                "-P",
                "null=NULL",
                "-v",
                "ON_ERROR_STOP=1",
            ],
            Some(sql.to_string()),
        ),
        Engine::ClickHouse => (
            "ossiec-clickhouse",
            // No --join_use_nulls: ossiec's SQL must carry `SETTINGS join_use_nulls = 1` itself.
            vec!["clickhouse-client", "--multiquery", "--format", "TSV"],
            Some(sql.to_string()),
        ),
        Engine::Trino => (
            "ossiec-trino",
            vec![
                "trino",
                "--catalog",
                "memory",
                "--schema",
                "default",
                // JSON: Trino's TSV and CSV print NULL and '' identically.
                "--output-format",
                "JSON",
                "--execute",
                sql,
            ],
            None,
        ),
        Engine::StarRocks => (
            "ossiec-starrocks",
            vec!["mysql", "-h127.0.0.1", "-P9030", "-uroot", "-N", "-B"],
            Some(format!(
                "CREATE DATABASE IF NOT EXISTS ossiec;\nUSE ossiec;\n{sql};"
            )),
        ),
        Engine::DuckDb => unreachable!("DuckDB runs in-process"),
    };
    let mut cmd = Command::new("docker");
    cmd.arg("exec").arg("-i").arg(container).args(&args);
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().map_err(|e| format!("docker: {e}"))?;
    let mut pipe = child.stdin.take().expect("stdin");
    if let Some(s) = stdin {
        pipe.write_all(s.as_bytes()).map_err(|e| e.to_string())?;
    }
    drop(pipe);
    let out = child.wait_with_output().map_err(|e| e.to_string())?;
    let stderr = String::from_utf8_lossy(&out.stderr);
    if !out.status.success() || stderr.contains("ERROR") {
        return Err(stderr.trim().to_string());
    }
    let lines = String::from_utf8_lossy(&out.stdout);
    let lines = lines.lines().filter(|l| !l.is_empty());
    if engine != Engine::Trino {
        return Ok(lines
            .map(|l| l.split('\t').map(norm_text).collect())
            .collect());
    }
    lines
        .map(|l| {
            let row: serde_json::Map<String, serde_json::Value> =
                serde_json::from_str(l).map_err(|e| format!("trino JSON: {e}: {l}"))?;
            Ok(row
                .into_values()
                .map(|v| match v {
                    serde_json::Value::Null => "NULL".to_string(),
                    serde_json::Value::String(s) => norm_text(&s),
                    other => norm_text(&other.to_string()),
                })
                .collect())
        })
        .collect()
}

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
