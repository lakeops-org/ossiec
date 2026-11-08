//! DuckDB, the reference engine, and normalizing result rows to text.

use duckdb::types::Value;
use duckdb::Connection;

use super::*;

/// One textual form per value, whichever engine or client produced it.
pub fn norm_text(s: &str) -> String {
    let s = s.trim();
    if s == "NULL" || s == "\\N" {
        return "NULL".into();
    }
    if let Ok(f) = s.parse::<f64>() {
        return format!("{f:.6}");
    }
    s.into()
}

/// Days since 1970-01-01 to `YYYY-MM-DD` (Howard Hinnant's civil_from_days).
pub(crate) fn civil(days: i64) -> String {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}

pub(crate) fn duck_text(v: Value) -> String {
    use duckdb::types::TimeUnit;
    match v {
        Value::Null => "NULL".into(),
        Value::Boolean(b) => b.to_string(),
        Value::TinyInt(i) => i.to_string(),
        Value::SmallInt(i) => i.to_string(),
        Value::Int(i) => i.to_string(),
        Value::BigInt(i) => i.to_string(),
        Value::HugeInt(i) => i.to_string(),
        Value::UTinyInt(i) => i.to_string(),
        Value::USmallInt(i) => i.to_string(),
        Value::UInt(i) => i.to_string(),
        Value::UBigInt(i) => i.to_string(),
        Value::Float(f) => f.to_string(),
        Value::Double(f) => f.to_string(),
        Value::Decimal(d) => d.to_string(),
        Value::Text(s) => s,
        Value::Date32(d) => civil(d.into()),
        Value::Timestamp(unit, t) => {
            let per_day: i64 = match unit {
                TimeUnit::Second => 86_400,
                TimeUnit::Millisecond => 86_400_000,
                TimeUnit::Microsecond => 86_400_000_000,
                TimeUnit::Nanosecond => 86_400_000_000_000,
            };
            if t.rem_euclid(per_day) == 0 {
                civil(t.div_euclid(per_day))
            } else {
                format!("{unit:?}:{t}")
            }
        }
        other => format!("{other:?}"),
    }
}

pub fn duckdb() -> Connection {
    let c = Connection::open_in_memory().unwrap();
    c.execute_batch(&seed_sql(Engine::DuckDb)).unwrap();
    c
}

pub fn duckdb_rows(c: &Connection, sql: &str) -> Vec<Vec<String>> {
    let sql = sql.replace("{customers}", CUSTOMERS);
    let mut st = c
        .prepare(&sql)
        .unwrap_or_else(|e| panic!("{e}\n---\n{sql}"));
    let mut rs = st.query([]).unwrap();
    let mut out = vec![];
    while let Some(r) = rs.next().unwrap() {
        let n = r.as_ref().column_count();
        out.push(
            (0..n)
                .map(|i| norm_text(&duck_text(r.get::<_, Value>(i).unwrap())))
                .collect(),
        );
    }
    out
}

/// Sort rows unless the request fixes their order.
pub fn comparable(req: &ossiec::Request, mut rows: Vec<Vec<String>>) -> Vec<Vec<String>> {
    if req.order_by.is_empty() {
        rows.sort();
    }
    rows
}
