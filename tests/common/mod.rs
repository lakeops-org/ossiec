//! Shared by the correctness tier (DuckDB) and the engine tier (Trino,
//! StarRocks, ClickHouse, Postgres): the shop seed data, the request cases with
//! their reference SQL, and result normalization.
#![allow(dead_code)]

use duckdb::types::Value;
use duckdb::Connection;

pub const MODEL: &str = include_str!("../fixtures/shop/model.yaml");

/// Reference SQL is written for DuckDB; `{customers}` is the customers dataset's query source.
const CUSTOMERS: &str = "(SELECT * FROM raw_customers WHERE NOT deleted)";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Engine {
    DuckDb,
    Postgres,
    Trino,
    StarRocks,
    ClickHouse,
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
        }
    }
}

// ---------------------------------------------------------------- seed data

#[derive(Clone, Copy)]
enum Ty {
    Int,
    Text,
    Date,
    Bool,
}

struct Table {
    name: &'static str,
    cols: &'static [(&'static str, Ty)],
    /// SQL literals; dates are bare `YYYY-MM-DD` and get each engine's date literal syntax.
    rows: &'static [&'static [&'static str]],
}

/// A tiny shop, shaped to break naive SQL: fan-out joins, NULL keys, a product
/// that never sold, a soft-deleted customer and a guest checkout.
const TABLES: &[Table] = &[
    Table {
        name: "raw_customers",
        cols: &[
            ("customer_id", Ty::Int),
            ("name", Ty::Text),
            ("region", Ty::Text),
            ("segment", Ty::Text),
            ("deleted", Ty::Bool),
        ],
        rows: &[
            &["1", "'Ada'", "'EU'", "'enterprise'", "false"],
            &["2", "'Bob'", "'EU'", "'smb'", "false"],
            &["3", "'Cyd'", "'US'", "'smb'", "false"],
            &["4", "'Dee'", "NULL", "'smb'", "false"], // region unknown
            &["5", "'Eve'", "'US'", "'enterprise'", "false"], // never ordered
            &["6", "'Old'", "'US'", "'smb'", "true"], // soft-deleted: the dataset's query source drops it
        ],
    },
    Table {
        name: "products",
        cols: &[
            ("product_id", Ty::Int),
            ("category", Ty::Text),
            ("stock", Ty::Int),
        ],
        rows: &[
            &["10", "'books'", "100"],
            &["11", "'books'", "50"],
            &["12", "'games'", "30"],
            &["13", "'games'", "7"], // never sold
        ],
    },
    Table {
        name: "orders",
        cols: &[
            ("order_id", Ty::Int),
            ("customer_id", Ty::Int),
            ("product_id", Ty::Int),
            ("order_date", Ty::Date),
            ("amount_cents", Ty::Int),
            ("status", Ty::Text),
        ],
        rows: &[
            &["100", "1", "10", "2026-01-05", "1000", "'completed'"],
            &["101", "1", "11", "2026-01-20", "2500", "'completed'"],
            &["102", "2", "10", "2026-02-03", "1200", "'returned'"],
            &["103", "3", "12", "2026-02-14", "4000", "'completed'"],
            &["104", "3", "12", "2026-03-01", "4000", "'completed'"],
            &["105", "4", "11", "2026-03-09", "800", "'completed'"],
            &["106", "NULL", "10", "2026-03-15", "500", "'completed'"], // guest checkout
            &["107", "6", "12", "2026-03-20", "999", "'completed'"],    // customer was deleted
        ],
    },
];

/// DDL and inserts for the seed data, in `engine`'s SQL.
pub fn seed_sql(engine: Engine) -> String {
    let mut out = String::new();
    for t in TABLES {
        let cols: Vec<String> = t
            .cols
            .iter()
            .enumerate()
            .map(|(i, (n, ty))| {
                // ClickHouse columns are non-Nullable unless they hold NULLs, as in real tables.
                let nullable = t.rows.iter().any(|r| r[i] == "NULL");
                let ty = match (engine, ty) {
                    (Engine::ClickHouse, ty) => {
                        let base = match ty {
                            Ty::Int => "Int32",
                            Ty::Text => "String",
                            Ty::Date => "Date",
                            Ty::Bool => "Bool",
                        };
                        return format!(
                            "{n} {}",
                            if nullable {
                                format!("Nullable({base})")
                            } else {
                                base.to_string()
                            }
                        );
                    }
                    (Engine::StarRocks, Ty::Text) => "VARCHAR(64)",
                    (_, Ty::Int) => "INTEGER",
                    (_, Ty::Text) => "VARCHAR",
                    (_, Ty::Date) => "DATE",
                    (_, Ty::Bool) => "BOOLEAN",
                };
                format!("{n} {ty}")
            })
            .collect();
        let suffix = match engine {
            Engine::ClickHouse => " ENGINE = Memory",
            Engine::StarRocks => " PROPERTIES (\"replication_num\" = \"1\")",
            _ => "",
        };
        let rows: Vec<String> = t
            .rows
            .iter()
            .map(|r| {
                let vals: Vec<String> = r
                    .iter()
                    .zip(t.cols)
                    .map(|(v, (_, ty))| match (ty, *v) {
                        (_, "NULL") => "NULL".into(),
                        (Ty::Date, d)
                            if matches!(engine, Engine::StarRocks | Engine::ClickHouse) =>
                        {
                            format!("'{d}'")
                        }
                        (Ty::Date, d) => format!("DATE '{d}'"),
                        (_, v) => v.to_string(),
                    })
                    .collect();
                format!("({})", vals.join(", "))
            })
            .collect();
        out.push_str(&format!(
            "DROP TABLE IF EXISTS {n};\nCREATE TABLE {n} ({}){suffix};\nINSERT INTO {n} VALUES {};\n",
            cols.join(", "),
            rows.join(", "),
            n = t.name
        ));
    }
    out
}

// ---------------------------------------------------------------- cases

pub struct Case {
    pub name: &'static str,
    pub request: &'static str,
    /// DuckDB SQL computing the right answer independently of ossiec.
    pub reference: &'static str,
}

pub const CASES: &[Case] = &[
    Case {
        // Values of one dataset's dimension: every customer counts, even one who never ordered.
        name: "dimension_values",
        request: r#"{"dimensions": ["customers.region"]}"#,
        reference: "SELECT DISTINCT region FROM {customers}",
    },
    Case {
        // Dimensions from two datasets: only the combinations some order links.
        name: "dimension_combinations",
        request: r#"{"dimensions": ["customers.segment", "products.category"],
            "filters": [{"field": "orders.status", "op": "=", "value": "completed"}],
            "orderBy": [{"field": "customers.segment"}, {"field": "products.category"}]}"#,
        reference: "SELECT DISTINCT c.segment, p.category
            FROM orders o
            LEFT JOIN {customers} c ON o.customer_id = c.customer_id
            LEFT JOIN products p ON o.product_id = p.product_id
            WHERE o.status = 'completed'
            ORDER BY c.segment NULLS FIRST, p.category NULLS FIRST",
    },
    Case {
        // `segment` is never NULL in the data, so on ClickHouse it's a non-Nullable column:
        // the guest checkout must still land in the NULL group, not under segment ''.
        name: "orphans_on_a_non_nullable_column",
        request: r#"{"metrics": ["total_revenue"], "dimensions": ["customers.segment"]}"#,
        reference: "SELECT c.segment, SUM(o.amount_cents / 100.0)
            FROM orders o LEFT JOIN {customers} c ON o.customer_id = c.customer_id
            GROUP BY c.segment",
    },
    Case {
        name: "integer_ratio",
        request: r#"{"metrics": ["orders_per_customer"], "dimensions": ["products.category"]}"#,
        reference: "SELECT p.category, COUNT(o.order_id) / NULLIF(COUNT(DISTINCT c.customer_id), 0)
            FROM orders o
            LEFT JOIN {customers} c ON o.customer_id = c.customer_id
            LEFT JOIN products p ON o.product_id = p.product_id
            GROUP BY p.category",
    },
    Case {
        name: "revenue_by_region",
        request: r#"{"metrics": ["total_revenue"], "dimensions": ["customers.region"]}"#,
        reference: "SELECT c.region, SUM(o.amount_cents / 100.0)
            FROM orders o LEFT JOIN {customers} c ON o.customer_id = c.customer_id
            GROUP BY c.region",
    },
    Case {
        name: "dataset_metric_and_filter_lowered",
        request: r#"{"metrics": ["orders.order_count", "completed_revenue"], "dimensions": ["products.category"]}"#,
        reference: "SELECT p.category, COUNT(*), SUM(CASE WHEN o.status = 'completed' THEN o.amount_cents / 100.0 END)
            FROM orders o LEFT JOIN products p ON o.product_id = p.product_id
            GROUP BY p.category",
    },
    Case {
        name: "count_distinct_buyers",
        request: r#"{"metrics": ["revenue_per_customer", "buying_customers"], "dimensions": ["products.category"],
            "filters": [{"field": "customers.segment", "op": "=", "value": "smb"}]}"#,
        reference: "SELECT p.category,
                SUM(o.amount_cents / 100.0) / NULLIF(COUNT(DISTINCT c.customer_id), 0),
                COUNT(DISTINCT c.customer_id)
            FROM orders o
            LEFT JOIN {customers} c ON o.customer_id = c.customer_id
            LEFT JOIN products p ON o.product_id = p.product_id
            WHERE c.segment = 'smb'
            GROUP BY p.category",
    },
    Case {
        name: "one_side_measure_dedup",
        request: r#"{"metrics": ["total_stock"], "dimensions": ["customers.region"]}"#,
        reference: "SELECT region, SUM(stock) FROM (
              SELECT DISTINCT c.region, p.product_id, p.stock
              FROM orders o
              LEFT JOIN {customers} c ON o.customer_id = c.customer_id
              LEFT JOIN products p ON o.product_id = p.product_id
              WHERE p.product_id IS NOT NULL
            ) GROUP BY region",
    },
    Case {
        name: "ratio_across_grains",
        request: r#"{"metrics": ["total_revenue", "total_stock", "revenue_per_stock_unit"], "dimensions": ["products.category"]}"#,
        reference: "WITH rev AS (
              SELECT p.category, SUM(o.amount_cents / 100.0) AS r
              FROM orders o LEFT JOIN products p ON o.product_id = p.product_id GROUP BY p.category
            ), stock AS (
              SELECT category, SUM(stock) AS s FROM (
                SELECT DISTINCT p.category, p.product_id, p.stock
                FROM orders o LEFT JOIN products p ON o.product_id = p.product_id
                WHERE p.product_id IS NOT NULL
              ) GROUP BY category
            )
            SELECT rev.category, rev.r, stock.s, rev.r / NULLIF(stock.s, 0)
            FROM rev LEFT JOIN stock ON rev.category IS NOT DISTINCT FROM stock.category",
    },
    Case {
        name: "measure_alone_at_its_grain",
        request: r#"{"metrics": ["total_stock"]}"#,
        reference: "SELECT SUM(stock) FROM products",
    },
    Case {
        name: "grand_totals_across_grains",
        request: r#"{"metrics": ["total_revenue", "total_stock"], "filters": [{"field": "orders.status", "op": "IS NOT NULL"}]}"#,
        reference: "SELECT (SELECT SUM(amount_cents / 100.0) FROM orders),
                (SELECT SUM(stock) FROM products WHERE product_id IN (SELECT product_id FROM orders))",
    },
    Case {
        name: "month_grain_with_date_range",
        request: r#"{"metrics": ["total_revenue"], "dimensions": ["orders.order_date"], "timeGrain": "month",
            "filters": [{"field": "orders.order_date", "op": "BETWEEN", "values": ["2026-01-01", "2026-02-28"]}]}"#,
        reference: "SELECT DATE_TRUNC('month', order_date), SUM(amount_cents / 100.0) FROM orders
            WHERE order_date BETWEEN DATE '2026-01-01' AND DATE '2026-02-28'
            GROUP BY 1",
    },
    Case {
        name: "metric_filter_order_limit",
        request: r#"{"metrics": ["total_revenue"], "dimensions": ["customers.name"],
            "metricFilters": [{"metric": "total_revenue", "op": ">", "value": 20}],
            "orderBy": [{"field": "total_revenue", "direction": "desc"}], "limit": 2}"#,
        reference: "SELECT c.name, SUM(o.amount_cents / 100.0) AS r
            FROM orders o LEFT JOIN {customers} c ON o.customer_id = c.customer_id
            GROUP BY c.name HAVING SUM(o.amount_cents / 100.0) > 20
            ORDER BY r DESC LIMIT 2",
    },
    Case {
        name: "filter_on_query_source",
        request: r#"{"metrics": ["avg_order_value"], "dimensions": ["customers.segment"],
            "filters": [{"field": "customers.region", "op": "IN", "values": ["EU", "US"]}]}"#,
        reference: "SELECT c.segment, AVG(o.amount_cents / 100.0)
            FROM orders o LEFT JOIN {customers} c ON o.customer_id = c.customer_id
            WHERE c.region IN ('EU', 'US')
            GROUP BY c.segment",
    },
    Case {
        name: "running_total_and_change",
        request: r#"{"metrics": ["total_revenue", "cumulative_revenue", "revenue_change"], "dimensions": ["orders.order_date"],
            "timeGrain": "month", "orderBy": [{"field": "orders.order_date"}]}"#,
        reference: "SELECT m, r,
                SUM(r) OVER (ORDER BY m ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW),
                r - LAG(r, 1) OVER (ORDER BY m)
            FROM (SELECT DATE_TRUNC('month', order_date) AS m, SUM(amount_cents / 100.0) AS r FROM orders GROUP BY 1)
            ORDER BY m",
    },
    Case {
        name: "rank_filter_after_window",
        request: r#"{"metrics": ["revenue_rank_in_region", "total_revenue"], "dimensions": ["customers.region", "products.category"],
            "metricFilters": [{"metric": "revenue_rank_in_region", "op": "=", "value": 1}]}"#,
        reference: "SELECT * FROM (
              SELECT c.region, p.category,
                     RANK() OVER (PARTITION BY c.region ORDER BY SUM(o.amount_cents / 100.0) DESC),
                     SUM(o.amount_cents / 100.0)
              FROM orders o
              LEFT JOIN {customers} c ON o.customer_id = c.customer_id
              LEFT JOIN products p ON o.product_id = p.product_id
              GROUP BY c.region, p.category
            ) AS ranked(region, category, rnk, revenue) WHERE rnk = 1",
    },
    Case {
        name: "window_over_dedup_measure",
        request: r#"{"metrics": ["stock_share", "total_revenue"], "dimensions": ["customers.region"]}"#,
        reference: "WITH stock AS (
              SELECT region, SUM(stock) AS s FROM (
                SELECT DISTINCT c.region, p.product_id, p.stock
                FROM orders o
                LEFT JOIN {customers} c ON o.customer_id = c.customer_id
                LEFT JOIN products p ON o.product_id = p.product_id
                WHERE p.product_id IS NOT NULL
              ) GROUP BY region
            ), rev AS (
              SELECT c.region, SUM(o.amount_cents / 100.0) AS r
              FROM orders o LEFT JOIN {customers} c ON o.customer_id = c.customer_id GROUP BY c.region
            )
            SELECT rev.region, stock.s / SUM(stock.s) OVER (), rev.r
            FROM rev LEFT JOIN stock ON rev.region IS NOT DISTINCT FROM stock.region",
    },
];

pub fn case(name: &str) -> &'static Case {
    CASES
        .iter()
        .find(|c| c.name == name)
        .unwrap_or_else(|| panic!("no case {name}"))
}

pub fn compile(case: &Case, engine: Engine) -> (ossiec::Request, ossiec::Compiled) {
    compile_for(case, Some(engine.dialect()))
}

/// `None` emits the model's source dialect (`generic`), as a downstream translator would receive it.
pub fn compile_for(
    case: &Case,
    target: Option<ossiec::DialectType>,
) -> (ossiec::Request, ossiec::Compiled) {
    let graph = ossiec::from_yaml(MODEL).unwrap();
    let req: ossiec::Request = serde_json::from_str(case.request).unwrap();
    let opts = ossiec::Options {
        target,
        ..Default::default()
    };
    let compiled =
        ossiec::compile(&graph, &req, &opts).unwrap_or_else(|e| panic!("{}: {e}", case.name));
    (req, compiled)
}

// ---------------------------------------------------------------- results

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
fn civil(days: i64) -> String {
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

fn duck_text(v: Value) -> String {
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
