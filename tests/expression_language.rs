//! `expression_language.md`, construct by construct: each one compiled for an
//! engine and run on fixed data, against a value worked out by hand. An engine's
//! known gaps are listed in [`gaps`], and the test fails when one closes, so the
//! list only shrinks.
//!
//! Beyond DuckDB this needs Docker (`scripts/engines.sh up`) and is ignored by
//! default, like the engine tier.

mod common;

use common::{duckdb_rows, norm_text, run, site_page, Engine};
use ossiec::{Options, Request};

/// What each engine can't do. Each is a clear refusal or an engine's error, apart from
/// the one marked: the test fails on any other wrong answer, and when an entry starts
/// to work.
fn gaps(e: Engine) -> &'static [&'static str] {
    match e {
        Engine::DuckDb => &[],
        Engine::Postgres => &[
            "TRY_CAST('12' AS INT)",
            "TRY_CAST(s AS INT)",
        ],
        Engine::MySql => &[
            "2 * MEDIAN(v)",
            "MEDIAN(v) FILTER (WHERE g = 'b')",
            "PERCENTILE_CONT(0.25) WITHIN GROUP (ORDER BY v) FILTER (WHERE g = 'b')",
            "PERCENTILE_DISC(0.5) WITHIN GROUP (ORDER BY v) FILTER (WHERE g = 'b')",
            "PERCENTILE_DISC(0.25) WITHIN GROUP (ORDER BY v)",
            "PERCENTILE_DISC(0.6) WITHIN GROUP (ORDER BY v)",
            "PERCENTILE_DISC(0) WITHIN GROUP (ORDER BY v)",
            "PERCENTILE_DISC(1) WITHIN GROUP (ORDER BY v)",
            "MEDIAN(v) FILTER (WHERE g = 'zzz')",
            "PERCENTILE_CONT(0.5) WITHIN GROUP (ORDER BY v) FILTER (WHERE g = 'zzz')",
            "PERCENTILE_DISC(0.5) WITHIN GROUP (ORDER BY v) FILTER (WHERE g = 'zzz')",
            "APPROX_PERCENTILE(v, 0.5) FILTER (WHERE g = 'zzz')",
            "REGEXP_COUNT(s, 'l')",
            "TRY_CAST('12' AS INT)",
            "TRY_CAST(s AS INT)",
            "MEDIAN(v)",
            "PERCENTILE_CONT(0.75) WITHIN GROUP (ORDER BY v)",
            "PERCENTILE_CONT(0.9) WITHIN GROUP (ORDER BY v)",
            "PERCENTILE_DISC(0.75) WITHIN GROUP (ORDER BY v)",
            "PERCENTILE_DISC(0.9) WITHIN GROUP (ORDER BY v)",
            "APPROX_PERCENTILE(v, 0.5) BETWEEN 1 AND 4",
            "MEDIAN(v) FILTER (WHERE g = 'a')",
            "PERCENTILE_CONT(0.5) WITHIN GROUP (ORDER BY v) FILTER (WHERE g = 'b')",
            "PERCENTILE_DISC(0.5) WITHIN GROUP (ORDER BY v) FILTER (WHERE g = 'a')",
        ],
        Engine::ClickHouse => &[],
        Engine::Trino => &[
            "2 * MEDIAN(v)",
            "MEDIAN(v) FILTER (WHERE g = 'b')",
            "PERCENTILE_CONT(0.25) WITHIN GROUP (ORDER BY v) FILTER (WHERE g = 'b')",
            "PERCENTILE_DISC(0.5) WITHIN GROUP (ORDER BY v) FILTER (WHERE g = 'b')",
            "PERCENTILE_DISC(0.25) WITHIN GROUP (ORDER BY v)",
            "PERCENTILE_DISC(0.6) WITHIN GROUP (ORDER BY v)",
            "PERCENTILE_DISC(0) WITHIN GROUP (ORDER BY v)",
            "PERCENTILE_DISC(1) WITHIN GROUP (ORDER BY v)",
            "MEDIAN(v) FILTER (WHERE g = 'zzz')",
            "PERCENTILE_CONT(0.5) WITHIN GROUP (ORDER BY v) FILTER (WHERE g = 'zzz')",
            "PERCENTILE_DISC(0.5) WITHIN GROUP (ORDER BY v) FILTER (WHERE g = 'zzz')",
            "MEDIAN(v)",
            "PERCENTILE_CONT(0.75) WITHIN GROUP (ORDER BY v)",
            "PERCENTILE_CONT(0.9) WITHIN GROUP (ORDER BY v)",
            "PERCENTILE_DISC(0.75) WITHIN GROUP (ORDER BY v)",
            "PERCENTILE_DISC(0.9) WITHIN GROUP (ORDER BY v)",
            "MEDIAN(v) FILTER (WHERE g = 'a')",
            "PERCENTILE_CONT(0.5) WITHIN GROUP (ORDER BY v) FILTER (WHERE g = 'b')",
            "PERCENTILE_DISC(0.5) WITHIN GROUP (ORDER BY v) FILTER (WHERE g = 'a')",
        ],
        // Athena's SQL runs on Trino (see `Engine::host`).
        Engine::Athena => gaps(Engine::Trino),
        Engine::StarRocks => &[
            // A known limit, and the one wrong answer on this list: `UPPER` on a column
            // only changes ASCII letters on StarRocks, which has no Unicode form.
            "UPPER(uni) = 'HÉLLO'",
            "NTH_VALUE(SUM(v), 2) OVER (ORDER BY k ROWS BETWEEN UNBOUNDED PRECEDING AND UNBOUNDED FOLLOWING)",
        ],
    }
}

const ONE_DDL: (&str, &str) = (
    "expr_one",
    "id INT, i INT, j INT, neg INT, z INT, nul INT, f DOUBLE, pi DOUBLE, \
     s TEXT, csv TEXT, sp TEXT, uni TEXT, datestr TEXT, tsstr TEXT, d DATE, ts TS, bt BOOL",
);
const ONE_ROWS: &str = "(1, 7, 2, -7, 0, NULL, 2.5, 3.14159, 'Hello', 'a,b,c', '  pad  ', 'héllo', \
     '2024-01-15', '2024-01-15 10:30:00', DATE '2024-03-15', TIMESTAMP '2024-03-15 13:45:30', TRUE)";
const MANY_DDL: (&str, &str) = ("expr_many", "k INT, g TEXT, v INT, s TEXT");
const MANY_ROWS: &str =
    "(1,'a',1,'x'),(2,'a',2,'x'),(3,'a',2,'y'),(4,'b',4,'y'),(5,'b',NULL,'z'),(6,'b',10,'z')";

fn seed(e: Engine) -> String {
    let e = e.host();
    let ty = |c: &str| -> String {
        let mut out = vec![];
        for col in c.split(',') {
            let col = col.trim();
            let (n, t) = col.split_once(' ').unwrap();
            let t = match (e, t) {
                (Engine::ClickHouse, "INT") if n == "nul" || n == "v" => "Nullable(Int32)",
                (Engine::ClickHouse, "INT") => "Int32",
                (Engine::ClickHouse, "DOUBLE") => "Float64",
                (Engine::ClickHouse, "TEXT") => "String",
                (Engine::ClickHouse, "DATE") => "Date",
                (Engine::ClickHouse, "TS") => "DateTime",
                (Engine::ClickHouse, "BOOL") => "Bool",
                (Engine::Postgres, "DOUBLE") => "DOUBLE PRECISION",
                (Engine::MySql | Engine::StarRocks, "TEXT") => "VARCHAR(64)",
                (Engine::MySql | Engine::StarRocks, "TS") => "DATETIME",
                (_, "TEXT") => "VARCHAR",
                (_, "TS") => "TIMESTAMP",
                (_, "BOOL") => "BOOLEAN",
                (_, "INT") => "INTEGER",
                (_, t) => t,
            };
            out.push(format!("{n} {t}"));
        }
        out.join(", ")
    };
    let suffix = match e {
        Engine::ClickHouse => " ENGINE = Memory",
        Engine::StarRocks => " PROPERTIES (\"replication_num\" = \"1\")",
        _ => "",
    };
    let lit = |r: &str| match e {
        Engine::ClickHouse | Engine::StarRocks => {
            r.replace("DATE '", "'").replace("TIMESTAMP '", "'")
        }
        _ => r.to_string(),
    };
    format!(
        "DROP TABLE IF EXISTS expr_one;\nCREATE TABLE expr_one ({}){suffix};\nINSERT INTO expr_one VALUES {};\n\
         DROP TABLE IF EXISTS expr_many;\nCREATE TABLE expr_many ({}){suffix};\nINSERT INTO expr_many VALUES {};\n",
        ty(ONE_DDL.1), lit(ONE_ROWS), ty(MANY_DDL.1), MANY_ROWS
    )
}

fn model(field: Option<&str>, metric: Option<&str>) -> String {
    let q = |s: &str| serde_json::to_string(s).unwrap();
    let f = field.unwrap_or("i");
    let m = metric.unwrap_or("COUNT(*)");
    format!(
        r#"version: "0.2.0.dev0"
name: probe
datasets:
  - name: one
    source: expr_one
    primary_key: [id]
    fields:
      - {{ name: id, expression: {{ sql: id }} }}
      - {{ name: val, expression: {{ sql: {} }} }}
  - name: many
    source: expr_many
    primary_key: [k]
    fields:
      - {{ name: k, expression: {{ sql: k }} }}
      - {{ name: g, expression: {{ sql: g }} }}
      - {{ name: v, expression: {{ sql: v }} }}
      - {{ name: s, expression: {{ sql: s }} }}
    metrics:
      - {{ name: m, expression: {{ sql: {} }} }}
"#,
        q(f),
        q(m)
    )
}

#[derive(Clone, Copy)]
enum P {
    Row(&'static str, &'static str),
    Agg(&'static str, &'static str),
    Win(&'static str, &'static [&'static str]),
    /// A window that reads `v` as a column of the grouped result.
    WinV(&'static str, &'static [&'static str]),
}

fn row(e: &'static str, w: &'static str) -> P {
    P::Row(e, w)
}
fn agg(e: &'static str, w: &'static str) -> P {
    P::Agg(e, w)
}
fn win(e: &'static str, w: &'static [&'static str]) -> P {
    P::Win(e, w)
}
fn winv(e: &'static str, w: &'static [&'static str]) -> P {
    P::WinV(e, w)
}

fn probes() -> Vec<P> {
    vec![
        // ## Operators
        row("i + j", "9"), row("i - j", "5"), row("i * j", "14"), row("i / j", "3.5"),
        row("i % j", "1"), row("neg % j", "-1"), row("-i", "-7"), row("+i", "7"),
        row("i = 7", "T"), row("i <> 7", "F"), row("i != 7", "F"), row("i < 7", "F"),
        row("i > 7", "F"), row("i <= 7", "T"), row("i >= 7", "T"),
        row("i = 7 AND j = 2", "T"), row("i = 0 OR j = 2", "T"), row("NOT i = 0", "T"),
        row("i BETWEEN 1 AND 7", "T"), row("i NOT BETWEEN 1 AND 7", "F"),
        row("i IN (1, 7)", "T"), row("i NOT IN (1, 7)", "F"),
        row("s LIKE 'He%'", "T"), row("s LIKE 'he%'", "F"), row("s LIKE 'H_llo'", "T"),
        row("s NOT LIKE 'He%'", "F"), row("s ILIKE 'he%'", "T"), row("s NOT ILIKE 'he%'", "F"),
        row("nul IS NULL", "T"), row("nul IS NOT NULL", "F"),
        row("CASE WHEN i > 5 THEN 'big' ELSE 'small' END", "big"),
        row("CASE i WHEN 7 THEN 'seven' ELSE 'other' END", "seven"),
        row("CASE WHEN i > 50 THEN 1 END", "NULL"),
        row("j + 3 * 4", "14"), row("(j + 3) * 4", "20"), row("-j * 3", "-6"),
        row("NOT i = 1 AND i = 1", "F"), row("i = 7 OR i = 7 AND i = 1", "T"),
        row("j + 1 = 3 AND NOT j = 1", "T"),
        // ## Current date/time
        row("d < CURRENT_DATE", "T"), row("d < CURRENT_DATE()", "T"),
        row("ts < CURRENT_TIMESTAMP", "T"), row("ts < CURRENT_TIMESTAMP()", "T"),
        row("CURRENT_TIME IS NOT NULL", "T"), row("CURRENT_TIME() IS NOT NULL", "T"),
        // ## Extraction functions
        row("YEAR(d)", "2024"), row("QUARTER(d)", "1"), row("MONTH(d)", "3"), row("DAY(d)", "15"),
        row("DAYOFYEAR(d)", "75"), row("HOUR(ts)", "13"), row("MINUTE(ts)", "45"), row("SECOND(ts)", "30"),
        // ## EXTRACT
        row("EXTRACT(YEAR FROM d)", "2024"), row("EXTRACT(QUARTER FROM d)", "1"),
        row("EXTRACT(MONTH FROM d)", "3"), row("EXTRACT(WEEK FROM d)", "11"),
        row("EXTRACT(DAY FROM d)", "15"), row("EXTRACT(DAYOFWEEK FROM d)", "5"),
        row("EXTRACT(DAYOFYEAR FROM d)", "75"), row("EXTRACT(HOUR FROM ts)", "13"),
        row("EXTRACT(MINUTE FROM ts)", "45"), row("EXTRACT(SECOND FROM ts)", "30"),
        row("EXTRACT(MILLISECOND FROM ts)", "30000"),
        // ## DATE_PART
        row("DATE_PART('year', d)", "2024"), row("DATE_PART('quarter', d)", "1"),
        row("DATE_PART('month', d)", "3"), row("DATE_PART('week', d)", "11"),
        row("DATE_PART('day', d)", "15"), row("DATE_PART('dayofweek', d)", "5"),
        row("DATE_PART('dayofyear', d)", "75"), row("DATE_PART('hour', ts)", "13"),
        row("DATE_PART('minute', ts)", "45"), row("DATE_PART('second', ts)", "30"),
        row("DATE_PART('millisecond', ts)", "30000"),
        // ## DATE_TRUNC
        row("DATE_TRUNC('year', d)", "2024-01-01"), row("DATE_TRUNC('quarter', d)", "2024-01-01"),
        row("DATE_TRUNC('month', d)", "2024-03-01"), row("DATE_TRUNC('week', d)", "2024-03-11"),
        row("DATE_TRUNC('day', ts)", "2024-03-15"),
        row("HOUR(DATE_TRUNC('hour', ts))", "13"), row("MINUTE(DATE_TRUNC('hour', ts))", "0"),
        row("MINUTE(DATE_TRUNC('minute', ts))", "45"), row("SECOND(DATE_TRUNC('minute', ts))", "0"),
        row("SECOND(DATE_TRUNC('second', ts))", "30"),
        // ## Date arithmetic
        row("DATEADD(day, 7, d)", "2024-03-22"), row("DATEADD(month, -1, d)", "2024-02-15"),
        row("DATEADD(year, 1, d)", "2025-03-15"),
        row("DATEADD(month, 1, DATE '2024-01-31')", "2024-02-29"),
        row("DATEDIFF(quarter, d, DATE '2024-10-01')", "3"),
        row("DATEDIFF(week, DATE '2024-03-17', DATE '2024-03-18')", "1"),
        row("DATEDIFF(hour, TIMESTAMP '2024-03-15 13:59:00', TIMESTAMP '2024-03-15 14:00:00')", "1"),
        row("DATEDIFF(minute, TIMESTAMP '2024-03-15 13:59:59', TIMESTAMP '2024-03-15 14:00:00')", "1"),
        row("DATEDIFF(second, TIMESTAMP '2024-03-15 13:59:59', TIMESTAMP '2024-03-15 14:00:00')", "1"),
        row("HOUR(DATEADD(hour, 2, ts))", "15"),
        row("MINUTE(DATEADD(minute, 2, ts))", "47"),
        row("SECOND(DATEADD(second, 2, ts))", "32"),
        row("DATEADD(week, 1, d)", "2024-03-22"),
        row("DATEADD(quarter, 1, d)", "2024-06-15"),
        row("DATEDIFF(day, d, DATE '2024-04-15')", "31"), row("DATEDIFF(month, d, DATE '2024-05-15')", "2"),
        row("DATEDIFF(year, d, DATE '2026-03-15')", "2"),
        row("DATEDIFF(month, DATE '2024-01-31', DATE '2024-02-01')", "1"),
        row("DATEDIFF(year, DATE '2024-12-31', DATE '2025-01-01')", "1"),
        // ## Construction
        row("YEAR(DATE '2024-01-15')", "2024"),
        row("HOUR(TIMESTAMP_NTZ '2024-01-15 10:30:00')", "10"),
        row("TIME '10:30:00' IS NOT NULL", "T"),
        row("CAST('2024-01-15' AS DATE)", "2024-01-15"),
        row("HOUR(CAST('2024-01-15 10:30:00' AS TIMESTAMP_NTZ))", "10"),
        row("CAST(datestr AS DATE)", "2024-01-15"),
        row("HOUR(CAST(tsstr AS TIMESTAMP_NTZ))", "10"),
        row("CAST('10:30:00' AS TIME) IS NOT NULL", "T"),
        row("TO_DATE(datestr)", "2024-01-15"), row("HOUR(TO_TIMESTAMP(tsstr))", "10"),
        // ## Experimental
        row("TO_DATE(datestr, 'YYYY-MM-DD')", "2024-01-15"),
        row("HOUR(TO_TIMESTAMP(tsstr, 'YYYY-MM-DD HH24:MI:SS'))", "10"),
        row("TO_CHAR(d, 'YYYY-MM-DD')", "2024-03-15"),
        row("TO_CHAR(ts, 'HH24:MI:SS')", "13:45:30"), row("TO_CHAR(ts, 'YY')", "24"),
        // ## Strings
        row("CONCAT(s, '!')", "Hello!"), row("CONCAT(s, '-', s)", "Hello-Hello"), row("s || '!'", "Hello!"),
        row("LENGTH(s)", "5"), row("LENGTH(uni)", "5"), row("LOWER(s)", "hello"), row("UPPER(s)", "HELLO"),
        row("LENGTH(TRIM(sp))", "3"), row("LENGTH(LTRIM(sp))", "5"), row("LENGTH(RTRIM(sp))", "5"),
        row("TRIM(sp) = 'pad'", "T"), row("LEFT(s, 2)", "He"), row("RIGHT(s, 2)", "lo"),
        row("SUBSTRING(s, 2, 3)", "ell"), row("REPLACE(s, 'l', 'L')", "HeLLo"),
        row("SPLIT_PART(csv, ',', 2)", "b"),
        row("POSITION('l' IN s)", "3"), row("CHARINDEX('l', s)", "3"),
        row("CONTAINS(s, 'ell')", "T"), row("CONTAINS(s, 'zzz')", "F"),
        row("STARTSWITH(s, 'He')", "T"), row("STARTSWITH(s, 'lo')", "F"),
        row("ENDSWITH(s, 'lo')", "T"), row("ENDSWITH(s, 'He')", "F"),
        row("REGEXP_LIKE(s, '^H.*o$')", "T"), row("REGEXP_LIKE(s, '^x')", "F"),
        row("REGEXP_EXTRACT(s, 'l+')", "ll"), row("REGEXP_REPLACE(s, 'l+', 'L')", "HeLo"),
        row("REGEXP_REPLACE(s, 'l', 'L')", "HeLLo"), row("REGEXP_COUNT(s, 'l')", "2"),
        // ## Math
        row("ABS(neg)", "7"), row("ROUND(pi, 2)", "3.14"), row("FLOOR(f)", "2"), row("CEIL(f)", "3"),
        row("CEILING(f)", "3"), row("TRUNC(pi, 2)", "3.14"), row("TRUNCATE(pi, 2)", "3.14"),
        row("MOD(i, j)", "1"), row("SIGN(neg)", "-1"), row("POWER(j, 3)", "8"), row("SQRT(i + 9)", "4"),
        row("EXP(z)", "1"), row("LN(z + 1)", "0"), row("LOG(j, 8)", "3"), row("LOG10(i + 93)", "2"),
        row("SIN(z)", "0"), row("COS(z)", "1"), row("TAN(z)", "0"), row("ASIN(z)", "0"),
        row("ACOS(z)", "1.570796"), row("ATAN(z)", "0"), row("ATAN2(j, j)", "0.785398"),
        row("RADIANS(i + 173)", "3.141593"), row("DEGREES(z)", "0"), row("PI()", "3.141593"),
        row("GREATEST(i, j)", "7"), row("LEAST(i, j)", "2"), row("GREATEST(i, j, 9)", "9"),
        // ## Conditional
        row("IF(i > j, 'y', 'n')", "y"), row("IFF(i < j, 'y', 'n')", "n"), row("NULLIF(i, 7)", "NULL"),
        row("NULLIF(i, 1)", "7"), row("COALESCE(nul, j)", "2"), row("COALESCE(nul, nul, i)", "7"),
        row("IFNULL(nul, j)", "2"), row("NVL(nul, j)", "2"), row("NVL2(nul, 1, 2)", "2"),
        row("NVL2(i, 1, 2)", "1"), row("ZEROIFNULL(nul)", "0"), row("ZEROIFNULL(i)", "7"),
        row("NULLIFZERO(z)", "NULL"), row("NULLIFZERO(i)", "7"),
        row("i = 7 AND TRUE", "T"), row("i = 7 AND FALSE", "F"), row("bt", "T"),
        // ## Casts
        row("CAST(i AS VARCHAR)", "7"), row("CAST(i AS STRING)", "7"), row("CAST(FLOOR(f) AS INTEGER)", "2"),
        row("CAST(i AS INT)", "7"), row("CAST(i AS BIGINT)", "7"), row("CAST(f AS DECIMAL(10,2))", "2.5"),
        row("CAST(f AS NUMERIC(10,2))", "2.5"), row("CAST(i AS FLOAT)", "7"), row("CAST(i AS DOUBLE)", "7"),
        row("CAST(i AS DOUBLE) / 2", "3.5"), row("CAST(z AS BOOLEAN)", "F"),
        row("CAST(datestr AS DATE)", "2024-01-15"), row("HOUR(CAST(tsstr AS TIMESTAMP))", "10"),
        row("TRY_CAST('12' AS INT)", "12"), row("TRY_CAST(s AS INT)", "NULL"),
        // ## Null-safe
        row("nul IS NOT DISTINCT FROM i", "F"),
        row("nul IS NOT DISTINCT FROM nul", "T"), row("i IS DISTINCT FROM nul", "T"),
        row("i IS DISTINCT FROM 7", "F"), row("i IS NOT DISTINCT FROM 7", "T"),
        // ## Edge cases
        // Boundaries: what a single fixed date can't tell apart.
        row("EXTRACT(DAYOFWEEK FROM DATE '2024-03-17')", "0"),
        row("EXTRACT(DAYOFWEEK FROM DATE '2024-03-16')", "6"),
        row("EXTRACT(DAYOFWEEK FROM DATE '2024-03-18')", "1"),
        row("DATE_PART('dayofweek', DATE '2024-03-17')", "0"),
        row("EXTRACT(WEEK FROM DATE '2024-12-30')", "1"),
        row("EXTRACT(WEEK FROM DATE '2021-01-03')", "53"),
        row("EXTRACT(WEEK FROM DATE '2024-01-01')", "1"),
        row("DATE_PART('week', DATE '2024-12-30')", "1"),
        row("EXTRACT(DAYOFYEAR FROM DATE '2024-12-31')", "366"),
        row("EXTRACT(DAYOFYEAR FROM DATE '2023-12-31')", "365"),
        row("DAYOFYEAR(DATE '2024-01-01')", "1"),
        row("QUARTER(DATE '2024-10-01')", "4"),
        row("QUARTER(DATE '2024-03-31')", "1"),
        row("HOUR(TIMESTAMP '2024-03-15 00:00:00')", "0"),
        row("HOUR(TIMESTAMP '2024-03-15 23:59:59')", "23"),
        row("DATE_TRUNC('week', DATE '2024-03-17')", "2024-03-11"),
        row("DATE_TRUNC('week', DATE '2024-03-18')", "2024-03-18"),
        row("DATE_TRUNC('quarter', DATE '2024-11-15')", "2024-10-01"),
        row("DATE_TRUNC('month', DATE '2024-02-29')", "2024-02-01"),
        row("DATEDIFF(day, DATE '2024-01-17', DATE '2024-01-15')", "-2"),
        row("DATEDIFF(month, DATE '2024-03-01', DATE '2024-01-31')", "-2"),
        row("DATEDIFF(year, DATE '2025-01-01', DATE '2024-12-31')", "-1"),
        row("DATEDIFF(quarter, DATE '2025-01-01', DATE '2024-12-31')", "-1"),
        row("DATEDIFF(week, DATE '2024-03-18', DATE '2024-03-17')", "-1"),
        row("DATEDIFF(week, DATE '2024-03-11', DATE '2024-03-17')", "0"),
        row("DATEDIFF(week, DATE '2024-12-30', DATE '2025-01-06')", "1"),
        row("DATEDIFF(day, TIMESTAMP '2024-03-15 23:59:59', TIMESTAMP '2024-03-16 00:00:00')", "1"),
        row("DATEDIFF(hour, TIMESTAMP '2024-03-15 14:00:00', TIMESTAMP '2024-03-15 13:59:00')", "-1"),
        row("DATEDIFF(second, TIMESTAMP '2024-03-15 14:00:00', TIMESTAMP '2024-03-15 13:59:59')", "-1"),
        row("DATEADD(day, -1, DATE '2024-03-01')", "2024-02-29"),
        row("DATEADD(year, 1, DATE '2024-02-29')", "2025-02-28"),
        row("DATEADD(month, -1, DATE '2024-03-31')", "2024-02-29"),
        row("DATEADD(quarter, -1, DATE '2024-03-31')", "2023-12-31"),
        row("DATEADD(week, -1, DATE '2024-03-07')", "2024-02-29"),
        row("HOUR(DATEADD(hour, -14, ts))", "23"),
        row("TO_CHAR(ts, 'HH12 PM')", "01 PM"),
        row("TO_CHAR(TIMESTAMP '2024-03-15 00:05:09', 'HH24:MI:SS')", "00:05:09"),
        row("TO_CHAR(TIMESTAMP '2024-03-15 09:05:09', 'HH12 AM')", "09 AM"),
        // Not found, empty and NULL.
        row("POSITION('z' IN s)", "0"),
        row("CHARINDEX('z', s)", "0"),
        row("SPLIT_PART(csv, ',', 9) = ''", "T"),
        row("SPLIT_PART(csv, ',', 3)", "c"),
        row("SPLIT_PART(csv, 'x', 1)", "a,b,c"),
        row("STARTSWITH(s, '')", "T"),
        row("ENDSWITH(s, '')", "T"),
        row("CONTAINS(s, '')", "T"),
        row("STARTSWITH(uni, 'hé')", "T"),
        row("ENDSWITH(uni, 'llo')", "T"),
        row("CHARINDEX('l', uni)", "3"),
        row("POSITION('l' IN uni)", "3"),
        row("LEFT(uni, 2) = 'hé'", "T"),
        row("RIGHT(uni, 4) = 'éllo'", "T"),
        row("SUBSTRING(uni, 2, 2) = 'él'", "T"),
        row("UPPER(uni) = 'HÉLLO'", "T"),
        row("LOWER(UPPER(uni)) = 'héllo'", "T"),
        row("LENGTH(UPPER(uni))", "5"),
        row("REPLACE(uni, 'é', 'e') = 'hello'", "T"),
        row("SPLIT_PART(CAST(nul AS VARCHAR), ',', 1) IS NULL", "T"),
        row("CONCAT(s, '-', s, '-', CAST(nul AS VARCHAR)) IS NULL", "T"),
        row("s || CAST(nul AS VARCHAR) IS NULL", "T"),
        row("LENGTH(CAST(nul AS VARCHAR)) IS NULL", "T"),
        row("CONTAINS(CAST(nul AS VARCHAR), 'x') IS NULL", "T"),
        row("STARTSWITH(CAST(nul AS VARCHAR), 'x') IS NULL", "T"),
        row("CONCAT(s, CAST(nul AS VARCHAR)) IS NULL", "T"),
        row("SUBSTRING(s, 4, 99)", "lo"),
        row("ZEROIFNULL(z)", "0"),
        row("NVL2(z, 1, 2)", "1"),
        // A rewritten call stays one operand.
        row("2 * EXTRACT(DAYOFWEEK FROM d)", "10"),
        row("10 - EXTRACT(DAYOFWEEK FROM d)", "5"),
        row("2 * DATEDIFF(month, DATE '2024-01-31', DATE '2024-03-01')", "4"),
        row("100 - DATEDIFF(year, DATE '2024-12-31', DATE '2026-01-01')", "98"),
        row("100 - DATEDIFF(quarter, DATE '2024-01-01', DATE '2024-07-01')", "98"),
        row("100 - DATEDIFF(day, DATE '2024-01-01', DATE '2024-01-03')", "98"),
        row("100 - DATEDIFF(week, DATE '2024-01-01', DATE '2024-01-15')", "98"),
        row("2 * EXTRACT(MILLISECOND FROM ts)", "60000"),
        row("NOT CONTAINS(s, 'zzz')", "T"),
        row("NOT STARTSWITH(s, 'zzz')", "T"),
        row("NOT ENDSWITH(s, 'zzz')", "T"),
        row("CONTAINS(s, 'ell') = TRUE", "T"),
        row("STARTSWITH(s, 'He') AND ENDSWITH(s, 'lo')", "T"),
        row("CONCAT(s, '!') = 'Hello!'", "T"),
        row("YEAR(DATEADD(year, 1, d)) + 1", "2026"),
        row("LENGTH(CONCAT(s, '!')) * 2", "12"),
        row("2 * LENGTH(s)", "10"),
        row("ZEROIFNULL(nul) + 1", "1"),
        row("2 * NULLIFZERO(i)", "14"),
        row("2 * CHARINDEX('l', s)", "6"),
        row("IFF(i > j, 1, 2) * 2", "2"),
        row("CASE WHEN CONTAINS(s, 'ell') THEN 1 ELSE 0 END", "1"),
        // Expressions polyglot reads as statements unless they're in a select list.
        row("((i = 7)) AND j = 2", "T"),
        // Signs.
        row("MOD(neg, j)", "-1"),
        row("SIGN(z)", "0"),
        row("FLOOR(0 - f)", "-3"),
        row("CEIL(0 - f)", "-2"),
        row("TRUNC(0 - pi, 2)", "-3.14"),
        row("ROUND(0 - pi, 2)", "-3.14"),
        row("ROUND(pi, 0)", "3"),
        row("TRUNC(pi, 0)", "3"),
        // ## Case-insensitive function names
        row("upper(s)", "HELLO"), row("Upper(s)", "HELLO"),
        // ## Aggregates
        agg("SUM(v)", "19"), agg("COUNT(v)", "5"), agg("COUNT(*)", "6"), agg("COUNT(DISTINCT v)", "4"),
        agg("AVG(v)", "3.8"), agg("MIN(v)", "1"), agg("MAX(v)", "10"), agg("SUM(DISTINCT v)", "17"),
        agg("sum(v)", "19"), agg("Sum(v)", "19"),
        agg("STDDEV(v)", "3.633180"), agg("STDDEV_POP(v)", "3.249615"), agg("STDDEV_SAMP(v)", "3.633180"),
        agg("VARIANCE(v)", "13.2"), agg("VAR_POP(v)", "10.56"), agg("VAR_SAMP(v)", "13.2"),
        agg("2 * MEDIAN(v)", "4"),
        agg("10 - COUNT(*)", "4"),
        agg("MEDIAN(v)", "2"),
        // An even count interpolates, and a picked percentile takes the lower value.
        agg("MEDIAN(v) FILTER (WHERE g = 'b')", "7"),
        agg("PERCENTILE_CONT(0.25) WITHIN GROUP (ORDER BY v) FILTER (WHERE g = 'b')", "5.5"),
        agg("PERCENTILE_DISC(0.5) WITHIN GROUP (ORDER BY v) FILTER (WHERE g = 'b')", "4"),
        agg("PERCENTILE_DISC(0.25) WITHIN GROUP (ORDER BY v)", "2"),
        agg("PERCENTILE_DISC(0.6) WITHIN GROUP (ORDER BY v)", "2"),
        agg("PERCENTILE_DISC(0) WITHIN GROUP (ORDER BY v)", "1"),
        agg("PERCENTILE_DISC(1) WITHIN GROUP (ORDER BY v)", "10"),
        // Over no rows, NULL.
        agg("MEDIAN(v) FILTER (WHERE g = 'zzz')", "NULL"),
        agg("PERCENTILE_CONT(0.5) WITHIN GROUP (ORDER BY v) FILTER (WHERE g = 'zzz')", "NULL"),
        agg("PERCENTILE_DISC(0.5) WITHIN GROUP (ORDER BY v) FILTER (WHERE g = 'zzz')", "NULL"),
        agg("APPROX_PERCENTILE(v, 0.5) FILTER (WHERE g = 'zzz')", "NULL"),
        agg("APPROX_COUNT_DISTINCT(v) FILTER (WHERE g = 'zzz')", "0"),
        agg("STDDEV(v) FILTER (WHERE g = 'zzz')", "NULL"),
        agg("VAR_POP(v) FILTER (WHERE g = 'zzz')", "NULL"),
        agg("PERCENTILE_CONT(0.75) WITHIN GROUP (ORDER BY v)", "4"),
        agg("PERCENTILE_CONT(0.9) WITHIN GROUP (ORDER BY v)", "7.6"),
        agg("PERCENTILE_DISC(0.75) WITHIN GROUP (ORDER BY v)", "4"),
        agg("PERCENTILE_DISC(0.9) WITHIN GROUP (ORDER BY v)", "10"),
        agg("APPROX_COUNT_DISTINCT(v)", "4"), agg("APPROX_PERCENTILE(v, 0.5) BETWEEN 1 AND 4", "T"),
        // ## Conditional aggregates
        agg("SUM(CASE WHEN g = 'a' THEN v END)", "5"), agg("COUNT(CASE WHEN g = 'a' THEN 1 END)", "3"),
        agg("SUM(v) FILTER (WHERE g = 'a')", "5"), agg("COUNT(*) FILTER (WHERE g = 'a')", "3"),
        agg("COUNT(v) FILTER (WHERE g = 'b')", "2"), agg("COUNT(DISTINCT v) FILTER (WHERE g = 'a')", "2"),
        agg("AVG(v) FILTER (WHERE g = 'a')", "1.666667"), agg("MIN(v) FILTER (WHERE g = 'b')", "4"),
        agg("MAX(v) FILTER (WHERE g = 'a')", "2"), agg("SUM(DISTINCT v) FILTER (WHERE g = 'a')", "3"),
        agg("STDDEV(v) FILTER (WHERE g = 'b')", "4.242641"), agg("VAR_POP(v) FILTER (WHERE g = 'b')", "9"),
        agg("MEDIAN(v) FILTER (WHERE g = 'a')", "2"),
        agg("PERCENTILE_CONT(0.5) WITHIN GROUP (ORDER BY v) FILTER (WHERE g = 'b')", "7"),
        agg("PERCENTILE_DISC(0.5) WITHIN GROUP (ORDER BY v) FILTER (WHERE g = 'a')", "2"),
        agg("VAR_SAMP(v) FILTER (WHERE g = 'b')", "18"),
        agg("VARIANCE(v) FILTER (WHERE g = 'b')", "18"),
        agg("STDDEV_POP(v) FILTER (WHERE g = 'b')", "3"),
        agg("STDDEV_SAMP(v) FILTER (WHERE g = 'b')", "4.242641"),
        agg("SUM(YEAR(DATEADD(day, 1, DATE '2024-01-01'))) FILTER (WHERE g = 'a')", "6072"),
        agg("SUM(DATEDIFF(day, DATE '2024-01-01', DATE '2024-01-03'))", "12"),
        agg("SUM(LENGTH(s))", "6"),
        agg("MAX(UPPER(s))", "Z"),
        agg("SUM(v) FILTER (WHERE g = 'zzz')", "NULL"), agg("COUNT(*) FILTER (WHERE g = 'zzz')", "0"),
        agg("APPROX_COUNT_DISTINCT(v) FILTER (WHERE g = 'a')", "2"),
        // ## Windows
        win("ROW_NUMBER() OVER (ORDER BY k)", &["1","2","3","4","5","6"]),
        win("RANK() OVER (ORDER BY SUM(v) DESC NULLS LAST)", &["5","3","3","2","6","1"]),
        win("RANK() OVER (ORDER BY SUM(v) DESC NULLS FIRST)", &["6","4","4","3","1","2"]),
        win("RANK() OVER (ORDER BY SUM(v) ASC NULLS FIRST)", &["2","3","3","5","1","6"]),
        win("DENSE_RANK() OVER (ORDER BY SUM(v) DESC NULLS LAST)", &["4","3","3","2","5","1"]),
        win("NTILE(2) OVER (ORDER BY k)", &["1","1","1","2","2","2"]),
        win("PERCENT_RANK() OVER (ORDER BY k)", &["0","0.2","0.4","0.6","0.8","1"]),
        win("CUME_DIST() OVER (ORDER BY k)", &["0.166667","0.333333","0.5","0.666667","0.833333","1"]),
        win("LAG(SUM(v), 1, 0) OVER (ORDER BY k)", &["0","1","2","2","4","NULL"]),
        win("LEAD(SUM(v), 1, 0) OVER (ORDER BY k)", &["2","2","4","NULL","10","0"]),
        win("LAG(SUM(v)) OVER (ORDER BY k)", &["NULL","1","2","2","4","NULL"]),
        win("FIRST_VALUE(SUM(v)) OVER (ORDER BY k)", &["1","1","1","1","1","1"]),
        win("LAST_VALUE(SUM(v)) OVER (ORDER BY k ROWS BETWEEN UNBOUNDED PRECEDING AND UNBOUNDED FOLLOWING)", &["10","10","10","10","10","10"]),
        win("NTH_VALUE(SUM(v), 2) OVER (ORDER BY k ROWS BETWEEN UNBOUNDED PRECEDING AND UNBOUNDED FOLLOWING)", &["2","2","2","2","2","2"]),
        win("SUM(SUM(v)) OVER (ORDER BY k)", &["1","3","5","9","9","19"]),
        win("SUM(SUM(v)) OVER (ORDER BY k ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW)", &["1","3","5","9","9","19"]),
        win("SUM(SUM(v)) OVER (ORDER BY k ROWS BETWEEN 1 PRECEDING AND 1 FOLLOWING)", &["3","5","8","6","14","10"]),
        win("SUM(SUM(v)) OVER (ORDER BY k RANGE BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW)", &["1","3","5","9","9","19"]),
        win("MAX(SUM(v)) OVER (ORDER BY k)", &["1","2","2","4","4","10"]),
        win("COUNT(SUM(v)) OVER (ORDER BY k)", &["1","2","3","4","4","5"]),
        win("SUM(v) / SUM(SUM(v)) OVER () * 100", &["5.263158","10.526316","10.526316","21.052632","NULL","52.631579"]),
        // The spec's own forms, over a field rather than an aggregate.
        winv("SUM(v) OVER (ORDER BY k)", &["1","3","5","9","9","19"]),
        winv("AVG(v) OVER (ORDER BY k ROWS BETWEEN 1 PRECEDING AND CURRENT ROW)", &["1","1.5","2","3","4","10"]),
        winv("SUM(v) OVER (PARTITION BY g)", &["5","5","5","14","14","14"]),
        winv("v / SUM(v) OVER () * 100", &["5.263158","10.526316","10.526316","21.052632","NULL","52.631579"]),
    ]
}

fn same(got: &str, want: &str) -> bool {
    let got = got.trim();
    match want {
        "T" => ["true", "t", "1", "1.000000"].contains(&got),
        "F" => ["false", "f", "0", "0.000000"].contains(&got),
        _ => {
            // A date may come back as a midnight timestamp (Postgres `DATE_TRUNC`).
            let got = got.strip_suffix("+00").unwrap_or(got);
            let g = norm_text(got.strip_suffix(" 00:00:00").unwrap_or(got));
            let g = g.strip_suffix(" 00:00:00.000").unwrap_or(&g).to_string();
            let w = norm_text(want);
            match (g.parse::<f64>(), w.parse::<f64>()) {
                (Ok(a), Ok(b)) => (a - b).abs() <= 1e-5 * a.abs().max(b.abs()).max(1.0),
                _ => g == w,
            }
        }
    }
}

fn exec(e: Engine, db: &duckdb::Connection, sql: &str) -> Result<Vec<Vec<String>>, String> {
    if e == Engine::DuckDb {
        let mut st = db.prepare(sql).map_err(|e| e.to_string())?;
        drop(st.query([]).map_err(|e| e.to_string())?);
        return Ok(duckdb_rows(db, sql));
    }
    run(e, sql)
}

/// A construct compiled for `e`: its text, and the SQL or why it was refused.
fn compile_probe(e: Engine, p: &P) -> (&'static str, ossiec::Result<ossiec::Compiled>) {
    let opts = Options {
        target: Some(e.dialect()),
        pretty: false,
        ..Default::default()
    };
    let (label, yaml, req): (&'static str, String, &str) = match *p {
        P::Row(x, _) => (x, model(Some(x), None), r#"{"dimensions":["one.val"]}"#),
        P::Agg(x, _) => (x, model(None, Some(x)), r#"{"metrics":["many.m"]}"#),
        P::Win(x, _) => (
            x,
            model(None, Some(x)),
            r#"{"metrics":["many.m"],"dimensions":["many.k"]}"#,
        ),
        P::WinV(x, _) if x.contains("PARTITION") => (
            x,
            model(None, Some(x)),
            r#"{"metrics":["many.m"],"dimensions":["many.k","many.v","many.g"]}"#,
        ),
        P::WinV(x, _) => (
            x,
            model(None, Some(x)),
            r#"{"metrics":["many.m"],"dimensions":["many.k","many.v"]}"#,
        ),
    };
    let compiled = (|| {
        let g = ossiec::from_yaml(&yaml)?;
        let r: Request = Request::from_json(req)?;
        ossiec::compile(&g, &r, &opts)
    })();
    (label, compiled)
}

/// One failure per construct: its text, then what happened.
fn probe_engine(e: Engine, db: &duckdb::Connection) -> (usize, Vec<String>) {
    let mut fails = vec![];
    let ps = probes();
    for p in &ps {
        let (label, compiled) = compile_probe(e, p);
        let c = match compiled {
            Ok(c) => c,
            Err(err) => {
                fails.push(format!("COMPILE {label}\n    {err}"));
                continue;
            }
        };
        if c.sql.contains("__ossiec") || c.sql.contains("__OSSIEC") {
            fails.push(format!("WRONG   {label}\n    internal name in {}", c.sql));
            continue;
        }
        let rows = match exec(e, db, &c.sql) {
            Ok(r) => r,
            Err(err) => {
                let err: String = err.chars().take(170).collect();
                fails.push(format!("RUN     {label}\n    {err}\n    sql: {}", c.sql));
                continue;
            }
        };
        let (ok, got) = match *p {
            P::Row(_, w) | P::Agg(_, w) => (
                rows.len() == 1 && rows[0].len() == 1 && same(&rows[0][0], w),
                format!("{rows:?} want {w}"),
            ),
            P::Win(_, w) | P::WinV(_, w) => {
                let mut rows = rows.clone();
                rows.sort_by_key(|r| r[0].parse::<f64>().map(|f| f as i64).unwrap_or(0));
                let vals: Vec<&str> = rows.iter().map(|r| r[r.len() - 1].as_str()).collect();
                (
                    vals.len() == w.len() && vals.iter().zip(w).all(|(a, b)| same(a, b)),
                    format!("{vals:?} want {w:?}"),
                )
            }
        };
        if !ok {
            fails.push(format!("WRONG   {label}\n    {got}\n    sql: {}", c.sql));
        }
    }
    (ps.len(), fails)
}

fn seeded() -> duckdb::Connection {
    let db = duckdb::Connection::open_in_memory().unwrap();
    db.execute_batch(&seed(Engine::DuckDb)).unwrap();
    db
}

/// The one wrong answer `gaps` allows: `UPPER` on a StarRocks column only changes
/// ASCII letters, and StarRocks has no Unicode form.
const KNOWN_WRONG: (Engine, &str) = (Engine::StarRocks, "UPPER(uni) = 'HÉLLO'");

fn check_engine(e: Engine) {
    let db = seeded();
    if e != Engine::DuckDb {
        // Once per host: Athena and Trino share a container, and seeding again would
        // drop the tables under the other's running probes.
        static SEEDED: std::sync::Mutex<Vec<Engine>> = std::sync::Mutex::new(Vec::new());
        let mut done = SEEDED.lock().unwrap_or_else(|e| e.into_inner());
        if !done.contains(&e.host()) {
            let sql = seed(e);
            let sql = if e.host() == Engine::Trino {
                sql.trim_end().trim_end_matches(';').to_string()
            } else {
                sql
            };
            run(e, &sql).expect("seed (is `scripts/engines.sh up` running?)");
            done.push(e.host());
        }
    }
    let (_, fails) = probe_engine(e, &db);
    let label = |f: &String| f[8..].lines().next().unwrap().to_string();
    // A gap is a refusal or an engine error, never a wrong answer, apart from
    // `KNOWN_WRONG`: that probe, on that engine only.
    let wrong: Vec<&String> = fails
        .iter()
        .filter(|f| f.starts_with("WRONG") && KNOWN_WRONG != (e, label(f).as_str()))
        .collect();
    assert!(wrong.is_empty(), "{e:?}: wrong answers: {wrong:#?}");
    let known = gaps(e);
    let new: Vec<&String> = fails
        .iter()
        .filter(|f| !known.contains(&label(f).as_str()))
        .collect();
    let closed: Vec<&&str> = known
        .iter()
        .filter(|k| !fails.iter().any(|f| label(f) == **k))
        .collect();
    assert!(
        new.is_empty() && closed.is_empty(),
        "{e:?}: gaps that closed, remove them from `gaps`: {closed:#?}\n\nnew failures:\n{}",
        new.iter()
            .map(|f| f.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    );
}

#[test]
fn every_construct_on_duckdb() {
    check_engine(Engine::DuckDb);
}

#[test]
#[ignore]
fn every_construct_on_postgres() {
    check_engine(Engine::Postgres);
}

#[test]
#[ignore]
fn every_construct_on_mysql() {
    check_engine(Engine::MySql);
}

#[test]
#[ignore]
fn every_construct_on_clickhouse() {
    check_engine(Engine::ClickHouse);
}

#[test]
#[ignore]
fn every_construct_on_trino() {
    check_engine(Engine::Trino);
}

/// Athena's SQL, run on Trino (Athena's engine v3 is Trino).
#[test]
#[ignore]
fn every_construct_on_athena() {
    check_engine(Engine::Athena);
}

#[test]
#[ignore]
fn every_construct_on_starrocks() {
    check_engine(Engine::StarRocks);
}

const HEAD: &str = "version: \"0.2.0.dev0\"\nname: p\n";

/// A dataset `t` with fields `id` and `amount` (column `amt`), and `extra` as YAML.
fn t(extra_fields: &str, metric: &str) -> Result<ossiec::SemanticGraph, String> {
    let q = serde_json::to_string(metric).unwrap();
    ossiec::from_yaml(&format!(
        "{HEAD}datasets:\n  - name: t\n    source: tbl\n    primary_key: [id]\n    fields:\n      - {{ name: id, expression: {{ sql: id }} }}\n      - {{ name: amount, expression: {{ sql: amt }} }}\n{extra_fields}    metrics:\n      - {{ name: m, expression: {{ sql: {q} }} }}\n"
    ))
    .map_err(|e| e.to_string())
}

fn sql_for(g: &ossiec::SemanticGraph, target: ossiec::DialectType) -> String {
    let opts = Options {
        target: Some(target),
        pretty: false,
        ..Default::default()
    };
    let r = Request::from_json(r#"{"metrics":["t.m"]}"#).unwrap();
    ossiec::compile(g, &r, &opts).unwrap().sql
}

#[test]
fn refuses_what_the_language_leaves_out() {
    for (metric, why) in [
        ("AGG(amount)", "AGG is reserved"),
        ("agg(amount)", "AGG is reserved"),
        ("Agg(amount)", "AGG is reserved"),
        ("SUM(AGG(amount))", "AGG is reserved"),
        ("COUNT(*) FILTER (WHERE AGG(amount) > 1)", "AGG is reserved"),
        ("SUM((SELECT 1))", "subqueries"),
        ("SUM(amount) + (SELECT 1)", "subqueries"),
        ("COUNT(*) FILTER (WHERE id IN (SELECT 1))", "subqueries"),
        (
            "COUNT(CASE WHEN EXISTS (SELECT 1) THEN 1 END)",
            "subqueries",
        ),
    ] {
        let e = t("", metric).map(|_| ()).expect_err(metric);
        assert!(e.contains(why), "{metric}: {e}");
    }
    // Read as a select list to parse `REPLACE(...) = x`, text must still be one
    // expression: nothing after it may be dropped.
    for field in [
        "id WHERE id > 1",
        "id AS other",
        "id ORDER BY id",
        "id GROUP BY id",
        "id LIMIT 1",
        "id, amt",
        "REPLACE(id, 'a', 'b') = 'c' WHERE TRUE",
    ] {
        let f = format!(
            "      - {{ name: x, expression: {{ sql: {} }} }}\n",
            serde_json::to_string(field).unwrap()
        );
        assert!(t(&f, "SUM(amount)").is_err(), "{field}");
    }
    for field in [
        "AGG(id)",
        "(SELECT 1)",
        "id IN (SELECT 1)",
        "EXISTS (SELECT 1)",
        "SELECT id",
    ] {
        let f = format!(
            "      - {{ name: x, expression: {{ sql: {} }} }}\n",
            serde_json::to_string(field).unwrap()
        );
        assert!(t(&f, "SUM(amount)").is_err(), "{field}");
    }
}

#[test]
fn only_a_call_in_the_portable_language_reserves_agg() {
    // A field, metric or column may be named `agg`.
    let f = "      - { name: agg, expression: { sql: agg } }\n";
    let g = t(f, "SUM(agg)").unwrap();
    assert!(sql_for(&g, ossiec::DialectType::DuckDB).contains("\"t\".agg"));
    // Another dialect's AGG is its own function.
    let y = format!("{HEAD}datasets:\n  - name: t\n    source: tbl\n    fields:\n      - name: x\n        expression:\n          dialects:\n            - {{ dialect: SNOWFLAKE, expression: AGG(id) }}\n");
    ossiec::from_yaml(&y).unwrap();
    // The portable dialect, named rather than defaulted, is the same language.
    let y = y.replace("SNOWFLAKE", "ossie_sql_2026");
    assert!(ossiec::from_yaml(&y)
        .err()
        .unwrap()
        .to_string()
        .contains("AGG is reserved"));
}

#[test]
fn identifiers_are_at_most_128_characters() {
    let fits = "a".repeat(128);
    let long = "a".repeat(129);
    let model = |dataset: &str, field: &str, metric: &str, top: &str, rel: &str| {
        format!("{HEAD}datasets:\n  - name: {dataset}\n    source: tbl\n    primary_key: [{field}]\n    fields:\n      - {{ name: {field}, expression: {{ sql: id }} }}\n    metrics:\n      - {{ name: {metric}, expression: {{ sql: COUNT(*) }} }}\n  - name: o\n    source: tbl\n    primary_key: [id]\n    fields:\n      - {{ name: id, expression: {{ sql: id }} }}\nrelationships:\n  - {{ name: {rel}, from: o, to: {dataset}, from_columns: [id], to_columns: [{field}] }}\nmetrics:\n  - {{ name: {top}, expression: {{ sql: COUNT(o.id) }} }}\n")
    };
    let other = "b".repeat(128);
    ossiec::from_yaml(&model(&fits, &fits, &other, &other, &fits))
        .map(|_| ())
        .unwrap();
    for (y, what) in [
        (model(&long, "f", "m", "n", "r"), "dataset"),
        (model("d", &long, "m", "n", "r"), "field"),
        (model("d", "f", &long, "n", "r"), "metric"),
        (model("d", "f", "m", &long, "r"), "metric"),
        (model("d", "f", "m", "n", &long), "relationship"),
    ] {
        let e = ossiec::from_yaml(&y).err().unwrap().to_string();
        assert!(
            e.contains(what) && e.contains("longer than 128"),
            "{what}: {e}"
        );
    }
}

/// Name Resolution: `source.column` in a dataset-scoped metric reads the source.
#[test]
fn dataset_metrics_read_source_columns() {
    for m in [
        "SUM(source.amt)",
        "SUM(SOURCE.amt)",
        "SUM(Source.amt)",
        "SUM(\"SOURCE\".amt)",
    ] {
        let s = sql_for(&t("", m).unwrap(), ossiec::DialectType::DuckDB);
        assert!(s.contains("SUM(\"t\".amt)"), "{m}: {s}");
    }
    // A column the dataset doesn't declare, read twice, in a FILTER too.
    let m = "SUM(source.hidden) FILTER (WHERE source.hidden > 0) + MAX(source.hidden)";
    let s = sql_for(&t("", m).unwrap(), ossiec::DialectType::DuckDB);
    assert_eq!(s.matches("\"t\".hidden").count(), 3, "{s}");
    // It answers from the real column.
    let db = duckdb::Connection::open_in_memory().unwrap();
    db.execute_batch("CREATE TABLE tbl (id INT, amt INT); INSERT INTO tbl VALUES (1, 5), (2, 7);")
        .unwrap();
    assert_eq!(
        duckdb_rows(
            &db,
            &sql_for(
                &t("", "SUM(source.amt)").unwrap(),
                ossiec::DialectType::DuckDB
            )
        ),
        [["12.000000"]]
    );
    // A field named `source` takes the name, and then the source is out of reach.
    let f = "      - { name: source, expression: { sql: src } }\n";
    let s = sql_for(&t(f, "SUM(source)").unwrap(), ossiec::DialectType::DuckDB);
    assert!(s.contains("\"t\".src"), "{s}");
    assert!(t(f, "SUM(source.amt)")
        .err()
        .unwrap()
        .contains("unknown dataset"));
    // A hidden field is nothing a consumer sees.
    let g = t("", "SUM(source.hidden)").unwrap();
    assert!(g
        .describe()
        .dimensions
        .iter()
        .all(|d| !d.name.contains("__source")));
    assert!(!t("", "SUM(source.amt) + SUM(nope)")
        .err()
        .unwrap()
        .contains("__source"));
    // Model-scoped metrics can't reach the source.
    let y = format!("{HEAD}datasets:\n  - name: t\n    source: tbl\n    fields:\n      - {{ name: id, expression: {{ sql: id }} }}\nmetrics:\n  - {{ name: m, expression: {{ sql: SUM(source.id) }} }}\n");
    assert!(ossiec::from_yaml(&y)
        .err()
        .unwrap()
        .to_string()
        .contains("unknown dataset"));
}

/// A field's SQL replaces its name inside every function, not just some of them.
#[test]
fn fields_resolve_inside_every_function() {
    let f = "      - { name: placed, expression: { sql: placed_at } }\n      - { name: title, expression: { sql: UPPER(raw) } }\n";
    let s = sql_for(
        &t(f, "SUM(YEAR(placed))").unwrap(),
        ossiec::DialectType::DuckDB,
    );
    assert!(s.contains("\"t\".placed_at"), "{s}");
    let s = sql_for(
        &t(f, "COUNT(CASE WHEN CONTAINS(title, 'X') THEN 1 END)").unwrap(),
        ossiec::DialectType::DuckDB,
    );
    assert!(s.contains("UPPER(\"t\".raw)"), "{s}");
    let s = sql_for(
        &t(f, "SUM(DATEDIFF(day, placed, placed))").unwrap(),
        ossiec::DialectType::DuckDB,
    );
    assert!(
        s.contains("\"t\".placed_at") && !s.contains("\"t\".day"),
        "{s}"
    );
    // ... and a name that isn't a field is an error there too.
    assert!(t(f, "SUM(YEAR(nope))")
        .err()
        .unwrap()
        .contains("not a field"));
}

/// The language's STDDEV and VARIANCE are the sample forms, whatever the engine
/// calls them. On MySQL and StarRocks the same names mean the population ones.
#[test]
fn stddev_and_variance_are_the_sample_forms() {
    use ossiec::DialectType::{MySQL, StarRocks};
    for d in [MySQL, StarRocks] {
        let s = sql_for(&t("", "STDDEV(amount)").unwrap(), d);
        assert!(s.contains("STDDEV_SAMP("), "{d:?}: {s}");
        let s = sql_for(&t("", "VARIANCE(amount)").unwrap(), d);
        assert!(s.contains("VAR_SAMP("), "{d:?}: {s}");
    }
}

/// An engine without `FILTER` gets the `CASE` form, on what `FILTER` wraps too,
/// and a constant parameter is left alone.
#[test]
fn filter_becomes_case_on_the_input_only() {
    use ossiec::DialectType::Snowflake;
    let s = sql_for(
        &t(
            "",
            "PERCENTILE_CONT(0.5) WITHIN GROUP (ORDER BY amount) FILTER (WHERE id > 1)",
        )
        .unwrap(),
        Snowflake,
    );
    assert!(!s.contains("FILTER"), "{s}");
    assert!(
        s.contains("ORDER BY CASE WHEN") && s.contains("PERCENTILE_CONT(0.5)"),
        "{s}"
    );
    let s = sql_for(
        &t("", "APPROX_PERCENTILE(amount, 0.5) FILTER (WHERE id > 1)").unwrap(),
        Snowflake,
    );
    assert!(
        !s.contains("FILTER") && s.contains(", 0.5)") && !s.contains("THEN 0.5"),
        "{s}"
    );
    // The predicate must stay on the aggregate's dataset, wrapped or not.
    let y = format!("{HEAD}datasets:\n  - name: a\n    source: a\n    primary_key: [id]\n    fields:\n      - {{ name: id, expression: {{ sql: id }} }}\n  - name: b\n    source: b\n    primary_key: [id]\n    fields:\n      - {{ name: id, expression: {{ sql: id }} }}\nrelationships:\n  - {{ name: r, from: a, to: b, from_columns: [id], to_columns: [id] }}\nmetrics:\n  - {{ name: m, expression: {{ sql: 'PERCENTILE_CONT(0.5) WITHIN GROUP (ORDER BY a.id) FILTER (WHERE b.id > 1)' }} }}\n");
    let g = ossiec::from_yaml(&y).unwrap();
    let e = ossiec::compile(
        &g,
        &Request::from_json(r#"{"metrics":["m"]}"#).unwrap(),
        &Options::default(),
    );
    assert_eq!(
        e.err().unwrap().code(),
        Some(ossiec::ErrorCode::AmbiguousMeasureGrain)
    );
}

/// A window with no aggregate, or that reads a field, runs over the grouped result,
/// so the fields it reads must be requested.
#[test]
fn a_window_names_the_dimensions_it_needs() {
    let g = t("", "ROW_NUMBER() OVER (ORDER BY id)").unwrap();
    let opts = Options {
        target: Some(ossiec::DialectType::DuckDB),
        ..Default::default()
    };
    let e = ossiec::compile(
        &g,
        &Request::from_json(r#"{"metrics":["t.m"]}"#).unwrap(),
        &opts,
    )
    .err()
    .unwrap()
    .to_string();
    assert!(e.contains("t.id"), "{e}");
    let g = t("", "amount / SUM(amount) OVER ()").unwrap();
    let e = ossiec::compile(
        &g,
        &Request::from_json(r#"{"metrics":["t.m"]}"#).unwrap(),
        &opts,
    )
    .err()
    .unwrap()
    .to_string();
    assert!(e.contains("t.amount"), "{e}");
    // Outside a window, a bare field is still an error.
    assert!(t("", "amount + SUM(amount)")
        .err()
        .unwrap()
        .contains("outside an aggregate"));
}

#[test]
fn timestamp_ntz_is_a_timestamp() {
    let f = |sql: &str| {
        format!(
            "      - {{ name: x, expression: {{ sql: {} }} }}\n",
            serde_json::to_string(sql).unwrap()
        )
    };
    let dim = |sql: &str| {
        let g = t(&f(sql), "COUNT(*)").unwrap();
        let opts = Options {
            target: Some(ossiec::DialectType::DuckDB),
            pretty: false,
            ..Default::default()
        };
        ossiec::compile(
            &g,
            &Request::from_json(r#"{"dimensions":["t.x"]}"#).unwrap(),
            &opts,
        )
        .unwrap()
        .sql
    };
    assert!(dim("timestamp_ntz '2024-01-15 10:30:00'").contains("TIMESTAMP"));
    assert!(dim("CAST(a AS TIMESTAMP_NTZ)").contains("AS TIMESTAMP)"));
    // Only the type: not inside a string, a quoted name or a longer word.
    assert!(dim("'TIMESTAMP_NTZ x'").contains("'TIMESTAMP_NTZ x'"));
    assert!(dim("\"TIMESTAMP_NTZ\"").contains("\"TIMESTAMP_NTZ\""));
    assert!(dim("TIMESTAMP_NTZ_X").contains("TIMESTAMP_NTZ_X"));
    assert!(dim("X_TIMESTAMP_NTZ").contains("X_TIMESTAMP_NTZ"));
}

fn shop_db() -> duckdb::Connection {
    let db = duckdb::Connection::open_in_memory().unwrap();
    db.execute_batch(
        "CREATE TABLE a (id INT, k INT, v INT, \"Exact\" INT);
         INSERT INTO a VALUES (1, 10, 1, 7), (2, 10, 2, 8), (3, 20, 3, 9);
         CREATE TABLE b (id INT, name TEXT);
         INSERT INTO b VALUES (10, 'p'), (20, 'q'), (30, 'never sold');",
    )
    .unwrap();
    db
}

fn shop_model(metric: &str, top: &str) -> String {
    let q = serde_json::to_string(metric).unwrap();
    format!("{HEAD}datasets:\n  - name: a\n    source: a\n    primary_key: [id]\n    fields:\n      - {{ name: id, expression: {{ sql: id }} }}\n      - {{ name: k, expression: {{ sql: k }} }}\n      - {{ name: v, expression: {{ sql: v }} }}\n    metrics:\n      - {{ name: m, expression: {{ sql: {q} }} }}\n  - name: b\n    source: b\n    primary_key: [id]\n    fields:\n      - {{ name: id, expression: {{ sql: id }} }}\n      - {{ name: name, expression: {{ sql: name }} }}\nrelationships:\n  - {{ name: r, from: a, to: b, from_columns: [k], to_columns: [id] }}\n{top}")
}

fn duck_opts() -> Options {
    Options {
        target: Some(ossiec::DialectType::DuckDB),
        pretty: false,
        ..Default::default()
    }
}

fn answer(yaml: &str, request: &str) -> Vec<Vec<String>> {
    let g = ossiec::from_yaml(yaml).map_err(|e| e.to_string()).unwrap();
    let r = Request::from_json(request).unwrap();
    let c = ossiec::compile(&g, &r, &duck_opts()).unwrap();
    let mut rows = duckdb_rows(&shop_db(), &c.sql);
    rows.sort();
    rows
}

/// A window with no aggregate still runs over its own dataset's rows.
#[test]
fn a_window_without_an_aggregate_keeps_its_home_dataset() {
    let y = shop_model("ROW_NUMBER() OVER ()", "");
    // No dimensions: the one grouped row of its dataset, and valid SQL.
    assert_eq!(answer(&y, r#"{"metrics":["a.m"]}"#), [["1.000000"]]);
    // A dimension on another dataset: only what `a` reaches, not every `b` row.
    let rows = answer(&y, r#"{"metrics":["a.m"],"dimensions":["b.name"]}"#);
    assert_eq!(rows.len(), 2, "{rows:?}");
    assert!(rows.iter().all(|r| r[0] != "never sold"), "{rows:?}");
    // At model scope a window that reads a field runs over that field's dataset,
    let top =
        "metrics:\n  - { name: t, expression: { sql: 'ROW_NUMBER() OVER (ORDER BY a.k)' } }\n";
    let rows = answer(
        &shop_model("COUNT(*)", top),
        r#"{"metrics":["t"],"dimensions":["a.k"],"deferredMetrics":true}"#,
    );
    assert_eq!(rows.len(), 2, "{rows:?}");
    // and one that reads none has no dataset to run over.
    let top = "metrics:\n  - { name: t, expression: { sql: 'ROW_NUMBER() OVER ()' } }\n";
    let g = ossiec::from_yaml(&shop_model("COUNT(*)", top));
    let e = match g {
        Err(e) => e.to_string(),
        Ok(g) => {
            let r = Request::from_json(r#"{"metrics":["t"],"deferredMetrics":true}"#).unwrap();
            ossiec::compile(&g, &r, &duck_opts())
                .err()
                .unwrap()
                .to_string()
        }
    };
    assert!(e.contains("no dataset to run over"), "{e}");
}

/// The hidden field behind `source.column` belongs to the metric alone.
#[test]
fn nothing_else_can_name_a_source_column() {
    let y = shop_model("SUM(source.v)", "");
    let g = ossiec::from_yaml(&y).map_err(|e| e.to_string()).unwrap();
    // Its name is `__source_` and the field's position: `id`, `k`, `v`, then this.
    let r = Request::from_json(r#"{"metrics":["a.m"],"dimensions":["a.__source_3"]}"#).unwrap();
    let e = ossiec::compile(&g, &r, &duck_opts())
        .err()
        .unwrap()
        .to_string();
    assert!(e.contains("not a field"), "{e}");
    let q = "SELECT __source_3, AGG(m) FROM a GROUP BY __source_3";
    let e = ossiec::compile_sql(&g, q, &duck_opts())
        .err()
        .unwrap()
        .to_string();
    assert!(
        e.contains("not a field") || e.contains("unknown field"),
        "{e}"
    );
    // Nor through a filter or a sort, in either layer.
    for q in [
        "SELECT k, AGG(m) FROM a WHERE __source_3 > 0 GROUP BY k",
        "SELECT k, AGG(m) FROM a GROUP BY k ORDER BY __source_3",
        "SELECT k, AGG(m) FROM a GROUP BY k HAVING __source_3 > 0",
        "SELECT k, AGG(m) FROM a GROUP BY k ORDER BY __source_3 + 1",
    ] {
        assert!(ossiec::compile_sql(&g, q, &duck_opts()).is_err(), "{q}");
    }
    for req in [
        r#"{"metrics":["a.m"],"filter":"a.__source_3 > 0"}"#,
        r#"{"metrics":["a.m"],"filters":[{"field":"a.__source_3","op":">","value":0}]}"#,
        r#"{"metrics":["a.m"],"dimensions":["a.k"],"orderBy":[{"field":"a.__source_3"}]}"#,
    ] {
        let r = Request::from_json(req);
        assert!(
            r.and_then(|r| ossiec::compile(&g, &r, &duck_opts()))
                .is_err(),
            "{req}"
        );
    }
    // A window reads the grouped result, which doesn't have the column.
    let y = shop_model("SUM(SUM(v)) OVER (PARTITION BY source.grp)", "");
    let g = ossiec::from_yaml(&y).map_err(|e| e.to_string()).unwrap();
    let r = Request::from_json(r#"{"metrics":["a.m"],"dimensions":["a.k"]}"#).unwrap();
    let e = ossiec::compile(&g, &r, &duck_opts())
        .err()
        .unwrap()
        .to_string();
    assert!(
        e.contains("declare a field") && !e.contains("__source"),
        "{e}"
    );
}

/// A name declared in quotes works everywhere a name goes, and comes out plain.
#[test]
fn quoted_names_work_end_to_end() {
    let y = format!("{HEAD}datasets:\n  - name: a\n    source: a\n    primary_key: ['\"Exact\"']\n    fields:\n      - {{ name: '\"Exact\"', expression: {{ sql: '\"Exact\"' }} }}\n      - {{ name: k, expression: {{ sql: k }} }}\n    metrics:\n      - {{ name: m, expression: {{ sql: 'COUNT(\"Exact\")' }} }}\n  - name: '\"B\"'\n    source: b\n    primary_key: [id]\n    fields:\n      - {{ name: id, expression: {{ sql: id }} }}\n      - {{ name: name, expression: {{ sql: name }} }}\nrelationships:\n  - {{ name: r, from: a, to: '\"B\"', from_columns: [k], to_columns: [id] }}\n");
    let g = ossiec::from_yaml(&y).map_err(|e| e.to_string()).unwrap();
    let columns = |c: &ossiec::Compiled| {
        let db = shop_db();
        let mut st = db
            .prepare(&c.sql)
            .unwrap_or_else(|e| panic!("{e}\n{}", c.sql));
        drop(st.query([]).unwrap());
        st.column_names()
    };
    // In a request, and in its output name.
    let r = Request::from_json(r#"{"metrics":["a.m"],"dimensions":["a.\"Exact\""]}"#).unwrap();
    assert_eq!(
        columns(&ossiec::compile(&g, &r, &duck_opts()).unwrap()),
        ["Exact", "m"]
    );
    // Across a join to a quoted dataset.
    let r = Request::from_json(r#"{"metrics":["a.m"],"dimensions":["\"B\".name"]}"#).unwrap();
    assert_eq!(
        columns(&ossiec::compile(&g, &r, &duck_opts()).unwrap()),
        ["name", "m"]
    );
    // In Layer 2.
    let q = "SELECT \"Exact\", AGG(m) FROM a GROUP BY \"Exact\"";
    assert_eq!(
        columns(&ossiec::compile_sql(&g, q, &duck_opts()).unwrap()),
        ["Exact", "m"]
    );
    // Sorted by and filtered on, in both layers.
    for q in [
        "SELECT \"Exact\", AGG(m) FROM a GROUP BY \"Exact\" ORDER BY \"Exact\"",
        "SELECT a.\"Exact\", AGG(m) FROM a GROUP BY a.\"Exact\" ORDER BY a.\"Exact\" DESC",
        "SELECT \"Exact\", AGG(m) FROM a WHERE \"Exact\" > 0 GROUP BY \"Exact\" HAVING \"Exact\" > 0",
        "SELECT \"Exact\" AS x, AGG(m) FROM a GROUP BY \"Exact\" ORDER BY x",
    ] {
        let c = ossiec::compile_sql(&g, q, &duck_opts()).unwrap_or_else(|e| panic!("{q}: {e}"));
        assert_eq!(duckdb_rows(&shop_db(), &c.sql).len(), 3, "{q}\n{}", c.sql);
    }
    let r = Request::from_json(r#"{"metrics":["a.m"],"dimensions":["a.\"Exact\""],"filter":"a.\"Exact\" > 7","orderBy":[{"field":"a.\"Exact\""}]}"#).unwrap();
    let c = ossiec::compile(&g, &r, &duck_opts()).unwrap();
    assert_eq!(duckdb_rows(&shop_db(), &c.sql).len(), 2, "{}", c.sql);
    // Two columns can't share a name, however each is spelled.
    let y2 = y
        .replace(
            "      - {{ name: name, expression: {{ sql: name }} }}",
            "      - { name: name, expression: { sql: name } }",
        )
        .replace(
            "      - { name: name, expression: { sql: name } }",
            "      - { name: '\"Exact\"', expression: { sql: name } }",
        );
    let g2 = ossiec::from_yaml(&y2).map_err(|e| e.to_string()).unwrap();
    let q = "SELECT a.\"Exact\", \"B\".\"Exact\", AGG(a.m) FROM a JOIN \"B\" ON a.k = \"B\".id GROUP BY a.\"Exact\", \"B\".\"Exact\"";
    let e = ossiec::compile_sql(&g2, q, &duck_opts())
        .err()
        .map(|e| e.to_string())
        .unwrap_or_default();
    assert!(e.contains("two output columns named"), "Layer 2: {e}");
    let r =
        Request::from_json(r#"{"metrics":["a.m"],"dimensions":["a.\"Exact\"","\"B\".\"Exact\""]}"#);
    let e = r
        .and_then(|r| ossiec::compile(&g2, &r, &duck_opts()))
        .err()
        .map(|e| e.to_string())
        .unwrap_or_default();
    assert!(e.contains("E_NAME_COLLISION"), "Layer 3: {e}");
    // A regular name doesn't reach it.
    let r = Request::from_json(r#"{"metrics":["a.m"],"dimensions":["a.exact"]}"#).unwrap();
    assert!(ossiec::compile(&g, &r, &duck_opts()).is_err());
}

/// `"k"` and `k` are different names, so each finds its own field.
#[test]
fn quoted_and_regular_names_are_different_fields() {
    let y = shop_model("COUNT(*)", "").replace(
        "      - { name: name, expression: { sql: name } }",
        "      - { name: name, expression: { sql: name } }\n      - { name: '\"k\"', expression: { sql: name } }",
    );
    let g = ossiec::from_yaml(&y).map_err(|e| e.to_string()).unwrap();
    let q = |c: &str| format!("SELECT {c}, AGG(a.m) FROM a JOIN b ON a.k = b.id GROUP BY {c}");
    let s = ossiec::compile_sql(&g, &q("\"k\""), &duck_opts())
        .unwrap()
        .sql;
    assert!(s.contains("\"b\".name"), "{s}");
    let s = ossiec::compile_sql(&g, &q("k"), &duck_opts()).unwrap().sql;
    assert!(s.contains("\"a\".k AS d0"), "{s}");
}

/// A column named in quotes and an alias can't share an output name either.
#[test]
fn an_alias_collides_with_a_quoted_name() {
    let y = format!("{HEAD}datasets:\n  - name: a\n    source: a\n    fields:\n      - {{ name: '\"Exact\"', expression: {{ sql: '\"Exact\"' }} }}\n      - {{ name: k, expression: {{ sql: k }} }}\n    metrics:\n      - {{ name: m, expression: {{ sql: COUNT(*) }} }}\n      - {{ name: '\"Q\"', expression: {{ sql: COUNT(*) }} }}\n");
    let g = ossiec::from_yaml(&y).map_err(|e| e.to_string()).unwrap();
    let r =
        Request::from_json(r#"{"metrics":["a.\"Q\""],"dimensions":[{"field":"a.k","alias":"Q"}]}"#);
    let e = r
        .and_then(|r| ossiec::compile(&g, &r, &duck_opts()))
        .err()
        .map(|e| e.to_string())
        .unwrap_or_default();
    assert!(e.contains("E_NAME_COLLISION"), "a metric: {e}");
    let q = "SELECT \"Exact\", k AS \"Exact\", AGG(m) FROM a GROUP BY \"Exact\", k";
    let e = ossiec::compile_sql(&g, q, &duck_opts())
        .err()
        .map(|e| e.to_string())
        .unwrap_or_default();
    assert!(e.contains("two output columns named"), "Layer 2: {e}");
    let r = Request::from_json(
        r#"{"metrics":["a.m"],"dimensions":["a.\"Exact\"",{"field":"a.k","alias":"Exact"}]}"#,
    );
    let e = r
        .and_then(|r| ossiec::compile(&g, &r, &duck_opts()))
        .err()
        .map(|e| e.to_string())
        .unwrap_or_default();
    assert!(e.contains("E_NAME_COLLISION"), "Layer 3: {e}");
}

/// Layer 2 and Layer 3 read a field's name by the same rule.
#[test]
fn both_layers_match_field_names_alike() {
    let y = shop_model("COUNT(*)", "");
    let g = ossiec::from_yaml(&y).map_err(|e| e.to_string()).unwrap();
    for (name, ok) in [("k", true), ("K", true), ("\"K\"", true), ("\"k\"", false)] {
        let q = format!("SELECT {name}, AGG(m) FROM a GROUP BY {name}");
        assert_eq!(
            ossiec::compile_sql(&g, &q, &duck_opts()).is_ok(),
            ok,
            "Layer 2 {name}"
        );
        let dim = serde_json::to_string(&format!("a.{name}")).unwrap();
        let r =
            Request::from_json(&format!(r#"{{"metrics":["a.m"],"dimensions":[{dim}]}}"#)).unwrap();
        assert_eq!(
            ossiec::compile(&g, &r, &duck_opts()).is_ok(),
            ok,
            "Layer 3 {name}"
        );
    }
    // A relation's name reads the same way.
    for (from, ok) in [("a", true), ("A", true), ("\"A\"", true), ("\"a\"", false)] {
        let q = format!("SELECT k, AGG(m) FROM {from} GROUP BY k");
        assert_eq!(
            ossiec::compile_sql(&g, &q, &duck_opts()).is_ok(),
            ok,
            "FROM {from}"
        );
    }
    // Among several datasets, too.
    let q = "SELECT \"k\", AGG(a.m) FROM a JOIN b ON a.k = b.id GROUP BY \"k\"";
    assert!(ossiec::compile_sql(&g, q, &duck_opts()).is_err());
    let q = "SELECT \"K\", AGG(a.m) FROM a JOIN b ON a.k = b.id GROUP BY \"K\"";
    ossiec::compile_sql(&g, q, &duck_opts()).unwrap();
}

/// A dataset declared in quotes answers exactly like one that isn't, through the
/// paths that build aliases from its name.
#[test]
fn a_quoted_dataset_answers_like_a_regular_one() {
    let model = |name: &str, spare: bool| {
        let extra = if spare {
            format!("  - {{ name: spare, from: orders, to: {name}, from_columns: [spare_id], to_columns: [id] }}\n")
        } else {
            String::new()
        };
        format!("{HEAD}datasets:\n  - name: {name}\n    source: products\n    primary_key: [id]\n    fields:\n      - {{ name: id, expression: {{ sql: id }} }}\n      - {{ name: stock, expression: {{ sql: stock }} }}\n    metrics:\n      - {{ name: units, expression: {{ sql: SUM(stock) }} }}\n  - name: orders\n    source: orders\n    primary_key: [id]\n    fields:\n      - {{ name: id, expression: {{ sql: id }} }}\n      - {{ name: product_id, expression: {{ sql: product_id }} }}\n      - {{ name: spare_id, expression: {{ sql: spare_id }} }}\n      - {{ name: region, expression: {{ sql: region }} }}\n    metrics:\n      - {{ name: n, expression: {{ sql: COUNT(*) }} }}\nrelationships:\n  - {{ name: bought, from: orders, to: {name}, from_columns: [product_id], to_columns: [id] }}\n{extra}")
    };
    let db = duckdb::Connection::open_in_memory().unwrap();
    db.execute_batch(
        "CREATE TABLE products (id INT, stock INT);
         INSERT INTO products VALUES (1, 10), (2, 20), (3, 7);
         CREATE TABLE orders (id INT, product_id INT, spare_id INT, region TEXT);
         INSERT INTO orders VALUES (1, 1, 2, 'EU'), (2, 1, 2, 'EU'), (3, 2, 1, 'US');",
    )
    .unwrap();
    let load = |name: &str, spare: bool| {
        ossiec::from_yaml(&model(name, spare))
            .map_err(|e| e.to_string())
            .unwrap()
    };
    let (regular, quoted) = (load("p", false), load("'\"P\"'", false));
    let rows = |sql: &str| {
        let mut r = duckdb_rows(&db, sql);
        r.sort();
        r
    };
    // Unmatched rows: product 3 has no order, and still counts.
    let unmatched = Options {
        include_unmatched: true,
        ..duck_opts()
    };
    let q = |p: &str| {
        format!("SELECT o.region, AGG(x.units) FROM orders o LEFT JOIN {p} x ON o.product_id = x.id GROUP BY o.region")
    };
    let want = rows(
        &ossiec::compile_sql(&regular, &q("p"), &unmatched)
            .unwrap()
            .sql,
    );
    assert!(
        want.iter().any(|r| r[0] == "NULL" && r[1] == "7.000000"),
        "{want:?}"
    );
    let c = ossiec::compile_sql(&quoted, &q("\"P\""), &unmatched).unwrap();
    assert_eq!(rows(&c.sql), want, "{}", c.sql);
    // Roles: one dataset under two relationships at once.
    let (regular, quoted) = (load("p", true), load("'\"P\"'", true));
    let req = r#"{"metrics":["orders.n"],"dimensions":[{"field":"bought.stock","alias":"b"},{"field":"spare.stock","alias":"s"}]}"#;
    let r = Request::from_json(req).unwrap();
    let want = rows(&ossiec::compile(&regular, &r, &duck_opts()).unwrap().sql);
    assert_eq!(want.len(), 2, "{want:?}");
    let c = ossiec::compile(&quoted, &r, &duck_opts()).unwrap();
    assert_eq!(rows(&c.sql), want, "{}", c.sql);
}

/// Stacked parts leave a measure's column NULL, typed to combine with its real
/// values: `PERCENTILE_DISC` returns what it sorts, not a number.
#[test]
#[ignore]
fn stacked_percentiles_keep_their_type_on_postgres() {
    let y = format!("{HEAD}datasets:\n  - name: d\n    source: pct_d\n    primary_key: [id]\n    fields:\n      - {{ name: id, expression: {{ sql: id }} }}\n      - {{ name: nm, expression: {{ sql: nm }} }}\n  - name: x\n    source: pct_x\n    primary_key: [id]\n    fields:\n      - {{ name: id, expression: {{ sql: id }} }}\n      - {{ name: dk, expression: {{ sql: dk }} }}\n      - {{ name: amt, expression: {{ sql: amt }} }}\n    metrics:\n      - {{ name: tot, expression: {{ sql: SUM(amt) }} }}\n  - name: z\n    source: pct_x\n    primary_key: [id]\n    fields:\n      - {{ name: id, expression: {{ sql: id }} }}\n      - {{ name: dk, expression: {{ sql: dk }} }}\n      - {{ name: amt, expression: {{ sql: amt }} }}\n    metrics:\n      - {{ name: tot2, expression: {{ sql: SUM(amt) }} }}\n  - name: y\n    source: pct_y\n    primary_key: [id]\n    fields:\n      - {{ name: id, expression: {{ sql: id }} }}\n      - {{ name: dk, expression: {{ sql: dk }} }}\n      - {{ name: at, datatype: date, expression: {{ sql: at }} }}\n      - {{ name: lbl, datatype: string, expression: {{ sql: lbl }} }}\n      - {{ name: n, datatype: integer, expression: {{ sql: id }} }}\n    metrics:\n      - {{ name: on_date, expression: {{ sql: 'PERCENTILE_DISC(0.5) WITHIN GROUP (ORDER BY at)' }} }}\n      - {{ name: on_text, expression: {{ sql: 'PERCENTILE_DISC(0.5) WITHIN GROUP (ORDER BY lbl)' }} }}\n      - {{ name: filtered, expression: {{ sql: 'PERCENTILE_DISC(0.5) WITHIN GROUP (ORDER BY at) FILTER (WHERE id > 0)' }} }}\n      - {{ name: earliest, expression: {{ sql: 'MIN(at) FILTER (WHERE id > 0)' }} }}\n      - {{ name: cont, expression: {{ sql: 'PERCENTILE_CONT(0.5) WITHIN GROUP (ORDER BY n)' }} }}\nrelationships:\n  - {{ name: xd, from: x, to: d, from_columns: [dk], to_columns: [id] }}\n  - {{ name: zd, from: z, to: d, from_columns: [dk], to_columns: [id] }}\n  - {{ name: yd, from: y, to: d, from_columns: [dk], to_columns: [id] }}\n");
    run(
        Engine::Postgres,
        "DROP TABLE IF EXISTS pct_d, pct_x, pct_y;
         CREATE TABLE pct_d (id INT, nm TEXT); INSERT INTO pct_d VALUES (1, 'n');
         CREATE TABLE pct_x (id INT, dk INT, amt INT); INSERT INTO pct_x VALUES (1, 1, 9);
         CREATE TABLE pct_y (id INT, dk INT, at DATE, lbl TEXT);
         INSERT INTO pct_y VALUES (1, 1, DATE '2024-01-01', 'z');",
    )
    .expect("seed (is `scripts/engines.sh up` running?)");
    let g = ossiec::from_yaml(&y).map_err(|e| e.to_string()).unwrap();
    let opts = Options {
        target: Some(Engine::Postgres.dialect()),
        pretty: false,
        ..Default::default()
    };
    for (metric, want) in [
        ("y.on_date", "2024-01-01"),
        ("y.on_text", "z"),
        ("y.filtered", "2024-01-01"),
        ("y.earliest", "2024-01-01"),
        ("y.cont", "1"),
    ] {
        // Two parts before the percentile's, each with its column NULL.
        let req = format!(r#"{{"metrics":["x.tot","z.tot2","{metric}"],"dimensions":["d.nm"]}}"#);
        let r = Request::from_json(&req).unwrap();
        let c = ossiec::compile(&g, &r, &opts).unwrap();
        let rows =
            run(Engine::Postgres, &c.sql).unwrap_or_else(|e| panic!("{metric}: {e}\n{}", c.sql));
        assert!(same(&rows[0][3], want), "{metric}: {rows:?}");
    }
}

fn sql_on(
    g: &ossiec::SemanticGraph,
    target: Option<ossiec::DialectType>,
    prefer: Option<&str>,
) -> Result<String, String> {
    let opts = Options {
        target,
        prefer_dialect: prefer.map(str::to_string),
        pretty: false,
        ..Default::default()
    };
    let r = Request::from_json(r#"{"dimensions":["t.x"]}"#).unwrap();
    ossiec::compile(g, &r, &opts)
        .map(|c| c.sql)
        .map_err(|e| e.to_string())
}

fn field(sql: &str) -> ossiec::SemanticGraph {
    let f = format!(
        "      - {{ name: x, expression: {{ sql: {} }} }}\n",
        serde_json::to_string(sql).unwrap()
    );
    t(&f, "COUNT(*)").unwrap_or_else(|e| panic!("{e}"))
}

/// A vendor's expression means what that vendor says, whichever engine runs it.
#[test]
fn a_vendor_expression_keeps_its_meaning() {
    use ossiec::DialectType::MySQL;
    let y = format!("{HEAD}datasets:\n  - name: t\n    source: tbl\n    primary_key: [id]\n    fields:\n      - {{ name: id, expression: {{ sql: id }} }}\n      - name: x\n        expression:\n          sql: LENGTH(a)\n          dialects:\n            - {{ dialect: MYSQL, expression: LENGTH(a) }}\n");
    let g = ossiec::from_yaml(&y).map_err(|e| e.to_string()).unwrap();
    // The portable form counts characters, the vendor's what MySQL's LENGTH counts.
    assert!(sql_on(&g, Some(MySQL), None)
        .unwrap()
        .contains("CHAR_LENGTH("));
    let s = sql_on(&g, Some(MySQL), Some("MYSQL")).unwrap();
    assert!(s.contains("LENGTH(") && !s.contains("CHAR_LENGTH("), "{s}");
}

/// Without a target the statement stays in Ossie's own words, for a caller who
/// translates downstream.
#[test]
fn no_target_leaves_ossies_functions_alone() {
    let s = sql_on(&field("CONTAINS(a, 'x')"), None, None).unwrap();
    assert!(s.contains("CONTAINS("), "{s}");
}

/// Athena's engine (v3) is Trino, so every construct compiles for Athena to the
/// SQL Trino runs, refusals included. The engine tier runs Athena's SQL on Trino;
/// this holds it to Trino's without Docker.
#[test]
fn athena_compiles_every_construct_as_trino_does() {
    let mut differ = vec![];
    for p in probes() {
        let (label, athena) = compile_probe(Engine::Athena, &p);
        let (_, trino) = compile_probe(Engine::Trino, &p);
        match (athena, trino) {
            (Ok(a), Ok(t)) if a.sql == t.sql => {}
            (Err(a), Err(t)) if a.to_string() == t.to_string().replace("Trino", "Athena") => {}
            (a, t) => differ.push(format!(
                "{label}\n  athena: {:?}\n  trino:  {:?}",
                a.map(|c| c.sql),
                t.map(|c| c.sql)
            )),
        }
    }
    assert!(differ.is_empty(), "{}", differ.join("\n"));
}

/// Where an engine can't give the same answer, the compile says so.
#[test]
fn an_engine_without_a_faithful_form_refuses() {
    use ossiec::DialectType::{ClickHouse, DuckDB, MySQL, PostgreSQL, Trino};
    for (sql, d) in [
        ("TRY_CAST(a AS INT)", MySQL),
        ("TRY_CAST(a AS INT)", PostgreSQL),
        ("REGEXP_COUNT(a, 'x')", MySQL),
    ] {
        let e = sql_on(&field(sql), Some(d), None).unwrap_err();
        assert!(
            e.contains("no equivalent") && e.contains(&format!("{d:?}")),
            "{sql} on {d:?}: {e}"
        );
    }
    for (m, d) in [
        ("MEDIAN(amount)", MySQL),
        ("MEDIAN(amount)", Trino),
        ("PERCENTILE_CONT(0.5) WITHIN GROUP (ORDER BY amount)", Trino),
        ("PERCENTILE_DISC(0.5) WITHIN GROUP (ORDER BY amount)", MySQL),
    ] {
        let opts = Options {
            target: Some(d),
            ..Default::default()
        };
        let r = Request::from_json(r#"{"metrics":["t.m"]}"#).unwrap();
        let e = ossiec::compile(&t("", m).unwrap(), &r, &opts)
            .err()
            .unwrap()
            .to_string();
        assert!(e.contains("no equivalent"), "{m} on {d:?}: {e}");
    }
    // What can only be translated when it's a constant says that instead.
    let e = sql_on(&field("TO_CHAR(a, b)"), Some(DuckDB), None).unwrap_err();
    assert!(e.contains("constant format"), "{e}");
    let opts = Options {
        target: Some(ClickHouse),
        ..Default::default()
    };
    let r = Request::from_json(r#"{"metrics":["t.m"]}"#).unwrap();
    let m = t("", "PERCENTILE_CONT(id) WITHIN GROUP (ORDER BY amount)").unwrap();
    let e = ossiec::compile(&m, &r, &opts).err().unwrap().to_string();
    assert!(e.contains("constant percentile"), "{e}");
}

/// Each format token, on an engine that spells them differently. The longest token
/// wins, so `MONTH` isn't `MON` and an `H`.
#[test]
fn format_tokens_are_translated_whole() {
    use ossiec::DialectType::{DuckDB, MySQL};
    let s = sql_on(
        &field("TO_CHAR(a, 'YYYY YY MM MON MONTH DD DY DAY HH24 HH12 HH MI SS AM - x')"),
        Some(DuckDB),
        None,
    )
    .unwrap();
    assert!(
        s.contains("'%Y %y %m %b %B %d %a %A %H %I %I %M %S %p - x'"),
        "{s}"
    );
    let s = sql_on(
        &field("TO_CHAR(a, 'YYYY MONTH DAY HH24 MI SS PM')"),
        Some(MySQL),
        None,
    )
    .unwrap();
    assert!(s.contains("'%Y %M %W %H %i %s %p'"), "{s}");
}

/// The table applies wherever an expression ends up: inside a metric, on a joined
/// dataset, and in a Layer 2 query's filter.
#[test]
fn functions_are_implemented_wherever_they_appear() {
    use ossiec::DialectType::MySQL;
    let opts = Options {
        target: Some(MySQL),
        pretty: false,
        ..Default::default()
    };
    let g = t(
        "      - { name: title, expression: { sql: UPPER(raw) } }\n",
        "SUM(LENGTH(title))",
    )
    .unwrap();
    let r = Request::from_json(r#"{"metrics":["t.m"]}"#).unwrap();
    let s = ossiec::compile(&g, &r, &opts).unwrap().sql;
    assert!(s.contains("CHAR_LENGTH(UPPER("), "{s}");
    let q = "SELECT id, AGG(m) FROM t WHERE LENGTH(title) > 1 GROUP BY id";
    let s = ossiec::compile_sql(&g, q, &opts).unwrap().sql;
    assert!(s.contains("WHERE (CHAR_LENGTH(UPPER("), "{s}");
    let r = Request::from_json(r#"{"metrics":["t.m"],"filter":"LENGTH(t.title) > 1"}"#).unwrap();
    let s = ossiec::compile(&g, &r, &opts).unwrap().sql;
    assert!(s.contains("WHERE CHAR_LENGTH(UPPER("), "{s}");
}

/// `FILTER` stays with an aggregate that was rewritten, where the engine has it.
#[test]
fn a_rewritten_aggregate_keeps_its_filter() {
    use ossiec::DialectType::PostgreSQL;
    for m in [
        "MEDIAN(amount) FILTER (WHERE id > 1)",
        "APPROX_COUNT_DISTINCT(amount) FILTER (WHERE id > 1)",
        "APPROX_PERCENTILE(amount, 0.5) FILTER (WHERE id > 1)",
    ] {
        let s = sql_for(&t("", m).unwrap(), PostgreSQL);
        assert!(s.contains("FILTER(WHERE \"t\".id > 1)"), "{m}: {s}");
    }
}

/// A form the table doesn't handle keeps every argument, rather than silently losing
/// one or failing inside the compiler.
#[test]
fn other_argument_counts_pass_through() {
    use ossiec::DialectType::{ClickHouse, DuckDB, PostgreSQL};
    for (sql, d, kept) in [
        ("REGEXP_REPLACE(a, 'x', 'y', 'i')", DuckDB, "'i'"),
        ("REGEXP_REPLACE(a, 'x', 'y', 'i')", PostgreSQL, "'i'"),
        ("REGEXP_EXTRACT(a, 'x', 1)", DuckDB, ", 1)"),
        ("CHARINDEX('l', a, 3)", DuckDB, "3"),
        ("ZEROIFNULL(a, 1)", DuckDB, ", 1)"),
    ] {
        let s = sql_on(&field(sql), Some(d), None).unwrap();
        assert!(s.contains(kept), "{sql} on {d:?}: {s}");
    }
    // Too few arguments are the engine's to refuse, not an internal error.
    for sql in ["CHARINDEX('l')", "SPLIT_PART(a, ',')", "REGEXP_LIKE(a)"] {
        let r = sql_on(&field(sql), Some(DuckDB), None);
        assert!(!r.is_err_and(|e| e.contains("internal")), "{sql}");
    }
    let r = Request::from_json(r#"{"metrics":["t.m"]}"#).unwrap();
    for d in [ClickHouse, DuckDB] {
        let opts = Options {
            target: Some(d),
            ..Default::default()
        };
        let g = t("", "APPROX_PERCENTILE(amount)").unwrap();
        let out = ossiec::compile(&g, &r, &opts);
        assert!(
            !out.is_err_and(|e| e.to_string().contains("internal")),
            "{d:?}"
        );
    }
}

/// The engines the expression language is run on, in the site's column order.
const SITE_ENGINES: &[(Engine, &str)] = &[
    (Engine::DuckDb, "DuckDB"),
    (Engine::Postgres, "Postgres"),
    (Engine::MySql, "MySQL"),
    (Engine::ClickHouse, "ClickHouse"),
    (Engine::Trino, "Trino"),
    (Engine::Athena, "Athena"),
    (Engine::StarRocks, "StarRocks"),
];

/// How a construct fares on an engine, from [`gaps`] (which the engine tier holds
/// to what really happens) and from compiling it here.
fn site_status(e: Engine, p: &P) -> (&'static str, Option<String>) {
    let label = match *p {
        P::Row(x, _) | P::Agg(x, _) | P::Win(x, _) | P::WinV(x, _) => x,
    };
    if !gaps(e).contains(&label) {
        return ("verified", None);
    }
    match compile_probe(e, p).1 {
        Err(err) => {
            let why = err.to_string();
            let why = why.split(": ").last().unwrap_or(&why).trim().to_string();
            ("refused", Some(why))
        }
        Ok(_) if KNOWN_WRONG == (e, label) => ("wrong", None),
        Ok(_) => ("engine error", None),
    }
}

/// The section each construct sits under in [`probes`], read from this file: a
/// `// ## Name` comment starts a section; other comments are notes.
fn probe_sections() -> Vec<(String, Vec<&'static str>)> {
    let src = include_str!("expression_language.rs");
    let body = &src[src.find("fn probes() -> Vec<P> {").unwrap()..];
    let body = &body[..body.find("\n}\n").unwrap()];
    let labels: Vec<&'static str> = probes()
        .iter()
        .map(|p| match *p {
            P::Row(x, _) | P::Agg(x, _) | P::Win(x, _) | P::WinV(x, _) => x,
        })
        .collect();
    let mut sections: Vec<(String, Vec<&'static str>)> = vec![];
    let mut placed = std::collections::HashSet::new();
    for line in body.lines() {
        let t = line.trim();
        if let Some(h) = t.strip_prefix("// ## ") {
            sections.push((h.trim().to_string(), vec![]));
            continue;
        }
        if t.starts_with("//") {
            continue;
        }
        for &l in &labels {
            let quoted = format!("\"{}\"", l.replace('\\', "\\\\").replace('"', "\\\""));
            if t.contains(&quoted) && placed.insert(l) {
                if sections.is_empty() {
                    sections.push(("Other".into(), vec![]));
                }
                sections.last_mut().unwrap().1.push(l);
            }
        }
    }
    let rest: Vec<&'static str> = labels.into_iter().filter(|l| !placed.contains(l)).collect();
    if !rest.is_empty() {
        sections.push(("Other".into(), rest));
    }
    sections.retain(|(_, ls)| !ls.is_empty());
    sections
}

/// `site/src/generated/functions.md`: every construct of `expression_language.md`
/// on every engine, as the tests establish it.
#[test]
fn site_function_matrix_is_current() {
    let probes = probes();
    let by_label: std::collections::HashMap<&str, &P> = probes
        .iter()
        .map(|p| match *p {
            P::Row(x, _) | P::Agg(x, _) | P::Win(x, _) | P::WinV(x, _) => (x, p),
        })
        .collect();
    let cell = |s: &str| s.replace('|', "\\|");
    let mut out = String::new();
    out.push_str("<!-- Generated by `site_function_matrix_is_current` in tests/expression_language.rs. Do not edit: run `OSSIEC_UPDATE_SITE=1 cargo test --test expression_language`. -->\n\n");
    out.push_str("# Functions by engine\n\n");
    out.push_str(&format!(
        "Every construct of `expression_language.md` that ossiec tests: {} in all. Each is compiled for each engine and run on fixed data, against a value worked out by hand. **Verified** means it returned that value. Anything else is listed under [Gaps](#gaps): a refusal (ossiec won't compile what the engine can't express faithfully), an engine error, or the one known wrong answer. The page is written from each engine's list of gaps, and CI holds that list exact: on every commit it runs every construct on every engine, and fails if one off the list doesn't return its value or one on the list does.\n\n",
        probes.len()
    ));
    out.push_str("Athena's column is ossiec's Athena SQL run on Trino, which Athena's engine (v3) is built on, so a function that Trino has and Athena lacks wouldn't show up here.\n\n");
    out.push_str("## Summary\n\n| Section |");
    for (_, n) in SITE_ENGINES {
        out.push_str(&format!(" {n} |"));
    }
    out.push_str("\n|---|");
    for _ in SITE_ENGINES {
        out.push_str("---|");
    }
    out.push('\n');
    let sections = probe_sections();
    let mut gaps_rows = vec![];
    for (name, labels) in &sections {
        out.push_str(&format!("| {name} |"));
        for &(e, en) in SITE_ENGINES {
            let mut ok = 0;
            for l in labels {
                let (st, why) = site_status(e, by_label[l]);
                if st == "verified" {
                    ok += 1;
                } else {
                    gaps_rows.push((en, *l, st, why));
                }
            }
            let mark = if ok == labels.len() { "✅" } else { "⚠️" };
            out.push_str(&format!(" {mark} {ok}/{} |", labels.len()));
        }
        out.push('\n');
    }
    let engine_order = |en: &str| SITE_ENGINES.iter().position(|(_, n)| *n == en);
    gaps_rows.sort_by_key(|(en, ..)| engine_order(en));
    out.push_str("\n## Gaps\n\n");
    if gaps_rows.is_empty() {
        out.push_str("None.\n");
    } else {
        out.push_str("| Engine | Construct | What happens |\n|---|---|---|\n");
        for (en, l, st, why) in &gaps_rows {
            let what = match (*st, why) {
                ("refused", Some(w)) => format!("Refused: {}", cell(w)),
                ("refused", None) => "Refused".to_string(),
                ("wrong", _) => {
                    "**Wrong answer** (known: the engine has no Unicode form)".to_string()
                }
                _ => "Engine error".to_string(),
            };
            out.push_str(&format!("| {en} | `{}` | {what} |\n", cell(l)));
        }
    }
    out.push_str("\n## Every construct\n\n");
    for (name, labels) in &sections {
        out.push_str(&format!(
            "<details><summary>{name} ({})</summary>\n\n",
            labels.len()
        ));
        for l in labels {
            out.push_str(&format!("- `{}`\n", l.replace('`', "'")));
        }
        out.push_str("\n</details>\n\n");
    }
    site_page("functions.md", out.trim_end());
}
