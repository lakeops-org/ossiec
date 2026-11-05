//! Apache Ossie's query compliance suite (`compliance/query`), vendored by
//! `scripts/sync-ossie-fixtures.sh`. Every case directory runs, so a case added
//! upstream runs after the next sync. The comparison follows the suite's README.

use std::fs;
use std::path::{Path, PathBuf};

use duckdb::types::Value;
use duckdb::Connection;
use ossiec::{DialectType, Options};
use polyglot_sql::{TokenType, Tokenizer};

#[derive(Debug, Clone, PartialEq)]
enum Cell {
    Null,
    Exact(String),
    Approx(f64),
    Other(String),
}

impl Cell {
    fn from_value(v: Value) -> Cell {
        match v {
            Value::Null => Cell::Null,
            Value::TinyInt(n) => Cell::Exact(n.to_string()),
            Value::SmallInt(n) => Cell::Exact(n.to_string()),
            Value::Int(n) => Cell::Exact(n.to_string()),
            Value::BigInt(n) => Cell::Exact(n.to_string()),
            Value::HugeInt(n) => Cell::Exact(n.to_string()),
            Value::UTinyInt(n) => Cell::Exact(n.to_string()),
            Value::USmallInt(n) => Cell::Exact(n.to_string()),
            Value::UInt(n) => Cell::Exact(n.to_string()),
            Value::UBigInt(n) => Cell::Exact(n.to_string()),
            // Types are not compared: 29025.00 matches 29025.
            Value::Decimal(d) => Cell::Exact(d.normalize().to_string()),
            Value::Float(f) => Cell::Approx(f64::from(f)),
            Value::Double(f) => Cell::Approx(f),
            Value::Text(s) => Cell::Other(s),
            other => Cell::Other(format!("{other:?}")),
        }
    }
}

/// A case must not return NaN or an infinity, so neither ever matches.
fn close(a: f64, e: f64) -> bool {
    a.is_finite() && e.is_finite() && (a - e).abs() <= 1e-9 * a.abs().max(e.abs()).max(1.0)
}

/// An approximate value against an exact expected one takes no tolerance. The
/// shortest decimal that round-trips the float must be the expected value, so a
/// float that only rounds to it doesn't match.
fn equals_exact(a: f64, exact: &str) -> bool {
    a.is_finite() && (if a == 0.0 { 0.0 } else { a }).to_string() == exact
}

/// Both NULL, or equal: exact numerics exactly, approximate ones within the
/// suite's tolerance when the expected value is approximate.
fn cells_match(got: &Cell, want: &Cell) -> bool {
    match (got, want) {
        (Cell::Exact(g), Cell::Approx(w)) => g.parse().is_ok_and(|g| close(g, *w)),
        (Cell::Approx(g), Cell::Approx(w)) => close(*g, *w),
        (Cell::Approx(g), Cell::Exact(w)) => equals_exact(*g, w),
        _ => got == want,
    }
}

fn rows_match(got: &[Cell], want: &[Cell]) -> bool {
    got.len() == want.len() && got.iter().zip(want).all(|(g, w)| cells_match(g, w))
}

/// Rows as multisets, or in order when the query has `ORDER BY`.
fn results_match(got: &[Vec<Cell>], want: &[Vec<Cell>], ordered: bool) -> bool {
    if got.len() != want.len() {
        return false;
    }
    if ordered {
        return got.iter().zip(want).all(|(g, w)| rows_match(g, w));
    }
    // A tolerance isn't transitive, so a row can match several: look for a full
    // pairing rather than taking the first match.
    let mut paired: Vec<Option<usize>> = vec![None; got.len()];
    (0..want.len()).all(|w| {
        let mut seen = vec![false; got.len()];
        pair_row(w, got, want, &mut paired, &mut seen)
    })
}

/// Kuhn's augmenting path: pair expected row `w`, moving earlier pairs if needed.
fn pair_row(
    w: usize,
    got: &[Vec<Cell>],
    want: &[Vec<Cell>],
    paired: &mut Vec<Option<usize>>,
    seen: &mut Vec<bool>,
) -> bool {
    for g in 0..got.len() {
        if seen[g] || !rows_match(&got[g], &want[w]) {
            continue;
        }
        seen[g] = true;
        let free = match paired[g] {
            None => true,
            Some(other) => pair_row(other, got, want, paired, seen),
        };
        if free {
            paired[g] = Some(w);
            return true;
        }
    }
    false
}

/// Whether the query itself has `ORDER BY`. Tokens, so comments and strings don't
/// count and line breaks do not matter. A subquery's doesn't fix the result order.
fn has_order_by(query: &str) -> bool {
    let tokens = Tokenizer::default_config().tokenize(query).unwrap();
    let mut depth = 0;
    tokens.iter().enumerate().any(|(i, t)| {
        match t.token_type {
            TokenType::LParen => depth += 1,
            TokenType::RParen => depth -= 1,
            _ => {}
        }
        depth == 0
            && (t.token_type == TokenType::OrderBy
                || (t.token_type == TokenType::Order
                    && tokens.get(i + 1).map(|n| n.token_type) == Some(TokenType::By)))
    })
}

fn run(db: &Connection, sql: &str) -> Result<(Vec<String>, Vec<Vec<Cell>>), String> {
    let mut st = db.prepare(sql).map_err(|e| format!("{e}\n{sql}"))?;
    let mut rows = vec![];
    {
        let mut rs = st.query([]).map_err(|e| format!("{e}\n{sql}"))?;
        while let Some(r) = rs.next().map_err(|e| e.to_string())? {
            let n = r.as_ref().column_count();
            rows.push(
                (0..n)
                    .map(|i| r.get::<_, Value>(i).map(Cell::from_value))
                    .collect::<Result<_, _>>()
                    .map_err(|e| e.to_string())?,
            );
        }
    }
    let names = st.column_names().iter().map(|c| c.to_lowercase()).collect();
    Ok((names, rows))
}

fn check_case(g: &ossiec::SemanticGraph, db: &Connection, dir: &Path) -> Result<(), String> {
    let read = |f: &str| fs::read_to_string(dir.join(f)).map_err(|e| format!("{f}: {e}"));
    let query = read("query.ossie.sql")?;
    let opts = Options {
        target: Some(DialectType::DuckDB),
        ..Default::default()
    };
    let compiled = ossiec::compile_sql(g, &query, &opts).map_err(|e| format!("compile: {e}"))?;
    let (got_names, got) = run(db, &compiled.sql)?;
    let (want_names, want) = run(db, &read("expected.sql")?)?;
    if got_names != want_names {
        return Err(format!("columns {got_names:?}, expected {want_names:?}"));
    }
    if !results_match(&got, &want, has_order_by(&query)) {
        return Err(format!(
            "rows\n  got:  {got:?}\n  want: {want:?}\n{}",
            compiled.sql
        ));
    }
    Ok(())
}

#[test]
fn every_query_compliance_case_returns_its_expected_result() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/ossie/query");
    let g =
        ossiec::from_yaml(&fs::read_to_string(root.join("scott/scott_model.ossie.yaml")).unwrap())
            .unwrap();
    let db = Connection::open_in_memory().unwrap();
    db.execute_batch(&fs::read_to_string(root.join("scott/scott.sql")).unwrap())
        .unwrap();

    let mut cases: Vec<PathBuf> = fs::read_dir(root.join("cases"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    cases.sort();
    assert!(!cases.is_empty(), "no compliance cases vendored");

    let failures: Vec<String> = cases
        .iter()
        .filter_map(|dir| {
            check_case(&g, &db, dir)
                .err()
                .map(|e| format!("{}: {e}", dir.file_name().unwrap().to_string_lossy()))
        })
        .collect();
    assert!(
        failures.is_empty(),
        "{} of {} cases failed\n\n{}",
        failures.len(),
        cases.len(),
        failures.join("\n\n")
    );
}

fn exact(s: &str) -> Cell {
    Cell::Exact(s.into())
}

/// The comparison is what makes a pass mean something, so it is tested too.
#[test]
fn comparison_follows_the_suite_rules() {
    // Values from DuckDB: 29025.00 and 29025 are one exact value.
    let db = Connection::open_in_memory().unwrap();
    let (_, a) = run(&db, "SELECT 29025.00").unwrap();
    let (_, b) = run(&db, "SELECT 29025").unwrap();
    assert!(results_match(&a, &b, false));
    let (_, c) = run(&db, "SELECT 29025.01").unwrap();
    assert!(!results_match(&a, &c, false));

    assert!(cells_match(&Cell::Null, &Cell::Null));
    assert!(!cells_match(&Cell::Null, &exact("0")));
    assert!(!cells_match(&exact("1"), &exact("2")));
    // Exact expected values take no tolerance.
    assert!(!cells_match(&Cell::Approx(1.000_000_000_1), &exact("1")));
    // Approximate expected values take 1e-9, relative above 1 and absolute below.
    assert!(cells_match(&Cell::Approx(1e6 + 1e-4), &Cell::Approx(1e6)));
    assert!(!cells_match(&Cell::Approx(1e6 + 1e-2), &Cell::Approx(1e6)));
    assert!(cells_match(&Cell::Approx(5e-10), &Cell::Approx(0.0)));
    assert!(!cells_match(&Cell::Approx(5e-9), &Cell::Approx(0.0)));

    // Multisets count duplicates; ORDER BY also fixes the order.
    let (one, two) = (vec![exact("1")], vec![exact("2")]);
    let want = [one.clone(), one.clone(), two.clone()];
    assert!(results_match(
        &[two.clone(), one.clone(), one.clone()],
        &want,
        false
    ));
    assert!(!results_match(
        &[two.clone(), two.clone(), one.clone()],
        &want,
        false
    ));
    assert!(!results_match(&[one.clone(), two.clone()], &want, false));
    assert!(!results_match(
        &[two.clone(), one.clone(), one.clone()],
        &want,
        true
    ));

    // A full pairing exists even though the first row matches both expected rows.
    let approx = |x: f64| vec![Cell::Approx(x)];
    assert!(results_match(
        &[approx(0.0), approx(1.5e-9)],
        &[approx(0.75e-9), approx(0.0)],
        false
    ));
    assert!(!results_match(
        &[approx(0.0), approx(0.0)],
        &[approx(0.0), approx(1.5e-9)],
        false
    ));

    // NaN and infinities never match, whichever side has them.
    assert!(!cells_match(
        &Cell::Approx(f64::INFINITY),
        &Cell::Approx(1.0)
    ));
    assert!(!cells_match(
        &Cell::Approx(1.0),
        &Cell::Approx(f64::INFINITY)
    ));
    assert!(!cells_match(
        &Cell::Approx(f64::INFINITY),
        &Cell::Approx(f64::INFINITY)
    ));
    assert!(!cells_match(
        &Cell::Approx(f64::NAN),
        &Cell::Approx(f64::NAN)
    ));
    assert!(!cells_match(&Cell::Approx(f64::INFINITY), &exact("1")));

    // An exact expected value is compared exactly, even beyond a float's integers.
    assert!(cells_match(
        &Cell::Approx(9_007_199_254_740_992.0),
        &exact("9007199254740992")
    ));
    assert!(!cells_match(
        &Cell::Approx(9_007_199_254_740_992.0),
        &exact("9007199254740993")
    ));
    assert!(cells_match(&Cell::Approx(0.1), &exact("0.1")));
    assert!(cells_match(&Cell::Approx(-0.0), &exact("0")));

    // Only the query's own clause counts.
    assert!(has_order_by("SELECT 1\nORDER BY 1"));
    assert!(has_order_by("SELECT 1\nORDER\n  BY 1"));
    assert!(has_order_by("SELECT 1 ORDER -- why\n BY 1"));
    assert!(!has_order_by("-- tests ORDER BY\nSELECT 1"));
    assert!(!has_order_by("SELECT 1 /* ORDER BY */"));
    assert!(!has_order_by("SELECT 1 -- ORDER BY 1"));
    assert!(!has_order_by("SELECT 'ORDER BY' AS x"));
    assert!(!has_order_by("SELECT x FROM (SELECT 1 AS x ORDER BY 1) t"));
}
