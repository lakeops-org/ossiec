//! The Docker engines (Trino, StarRocks, ClickHouse, Postgres, MySQL): which
//! one, running SQL on it, and comparing its rows. Athena has no container:
//! its engine (v3) is Trino, so ossiec's Athena SQL runs on the Trino one.

use std::io::Write;
use std::process::{Command, Stdio};

use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Engine {
    DuckDb,
    Postgres,
    Trino,
    StarRocks,
    ClickHouse,
    MySql,
    /// ossiec's Athena dialect, run on the Trino container.
    Athena,
}

impl Engine {
    pub fn dialect(self) -> ossiec::DialectType {
        use ossiec::DialectType as D;
        match self {
            Engine::DuckDb => D::DuckDB,
            Engine::Postgres => D::PostgreSQL,
            Engine::Trino => D::Trino,
            Engine::StarRocks => D::StarRocks,
            Engine::ClickHouse => D::ClickHouse,
            Engine::MySql => D::MySQL,
            Engine::Athena => D::Athena,
        }
    }

    /// The engine whose container runs this one's SQL: Trino for Athena,
    /// otherwise itself. Seed data and result parsing follow the host.
    pub fn host(self) -> Engine {
        match self {
            Engine::Athena => Engine::Trino,
            e => e,
        }
    }
}

/// Seed the shop tables on `engine`'s host once per test binary. Athena and
/// Trino share a container, so whichever runs first seeds it for both; seeding
/// again would drop the tables under the other's running queries.
pub fn seed_shop(engine: Engine) {
    static SEEDED: std::sync::Mutex<Vec<Engine>> = std::sync::Mutex::new(Vec::new());
    let host = engine.host();
    let mut done = SEEDED.lock().unwrap();
    if done.contains(&host) {
        return;
    }
    let sql = seed_sql(host);
    // The Trino CLI's --execute takes statements separated by `;`, without a trailing one.
    let sql = if host == Engine::Trino {
        sql.trim_end().trim_end_matches(';').to_string()
    } else {
        sql
    };
    run(host, &sql).unwrap_or_else(|e| {
        panic!("seeding {host:?} failed (is `scripts/engines.sh up` running?): {e}")
    });
    done.push(host);
}

/// Run SQL through the engine's own CLI inside its container. Every client prints
/// NULL distinctly from '' (psql `-P null=NULL`, ClickHouse `\\N`, mysql `NULL`, Trino JSON `null`),
/// and prints strings unescaped (ClickHouse `TSVRaw`, mysql `-r`; the mysql client also
/// serves StarRocks).
pub fn run(engine: Engine, sql: &str) -> Result<Vec<Vec<String>>, String> {
    run_in(engine, sql, ("memory", "default"))
}

/// [`run`], with Trino's catalog and schema for unqualified names.
pub fn run_in(engine: Engine, sql: &str, trino: (&str, &str)) -> Result<Vec<Vec<String>>, String> {
    let (container, args, stdin): (&str, Vec<&str>, Option<String>) = match engine.host() {
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
            vec!["clickhouse-client", "--multiquery", "--format", "TSVRaw"],
            Some(sql.to_string()),
        ),
        Engine::Trino => (
            "ossiec-trino",
            vec![
                "trino",
                "--catalog",
                trino.0,
                "--schema",
                trino.1,
                // JSON: Trino's TSV and CSV print NULL and '' identically.
                // JSON rows are keyed by column name: alias duplicate names.
                "--output-format",
                "JSON",
                "--execute",
                sql,
            ],
            None,
        ),
        Engine::StarRocks => (
            "ossiec-starrocks",
            vec!["mysql", "-h127.0.0.1", "-P9030", "-uroot", "-N", "-B", "-r"],
            Some(format!(
                "CREATE DATABASE IF NOT EXISTS ossiec;\nUSE ossiec;\n{sql};"
            )),
        ),
        Engine::MySql => (
            "ossiec-mysql",
            vec![
                "mysql",
                "-uroot",
                "-possiec",
                "-D",
                "ossiec",
                "-N",
                "-B",
                "-r",
                "--default-character-set=utf8mb4",
            ],
            Some(sql.to_string()),
        ),
        Engine::DuckDb => unreachable!("DuckDB runs in-process"),
        Engine::Athena => unreachable!("Athena runs on its host, Trino"),
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
    if engine.host() != Engine::Trino {
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

/// Rows equal, with numbers compared to a relative tolerance: a DOUBLE ratio and
/// a DECIMAL reference agree to ~1e-9, not to every digit.
pub fn rows_close(got: &[Vec<String>], want: &[Vec<String>]) -> bool {
    got.len() == want.len()
        && got.iter().zip(want).all(|(g, w)| {
            g.len() == w.len()
                && g.iter()
                    .zip(w)
                    .all(|(a, b)| match (a.parse::<f64>(), b.parse::<f64>()) {
                        (Ok(x), Ok(y)) => (x - y).abs() <= 1e-6 * x.abs().max(y.abs()).max(1.0),
                        _ => a == b,
                    })
        })
}
