//! Shared by the correctness tier (DuckDB) and the engine tier (Trino,
//! StarRocks, ClickHouse, Postgres): the shop seed data, the request cases with
//! their reference SQL, and result normalization.
#![allow(dead_code)]

use std::io::Write;
use std::process::{Command, Stdio};

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
    MySql,
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
            &["7", "'O''Brien'", "'APAC'", "'smb'", "false"], // a quote; her only order has no product
            &["8", "'ada'", "'EU'", "'enterprise'", "false"], // lower case: collations disagree on 'ada' vs 'Bob'
            &["9", "'C:\\temp'", "'APAC'", "'smb'", "false"], // a backslash: an escape character in some engines
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
            &["108", "7", "NULL", "2026-03-25", "300", "'completed'"], // no product: APAC has no product rows
            &["109", "2", "11", "2025-12-30", "100", "'completed'"], // its Monday-start week begins in 2025
            &["110", "2", "11", "2027-01-01", "100", "'completed'"], // a Friday: its week begins 2026-12-28
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
                    (Engine::StarRocks | Engine::MySql, Ty::Text) => "VARCHAR(64)",
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
                        // MySQL-family engines and ClickHouse treat `\\` in literals as an escape.
                        (Ty::Text, v)
                            if matches!(
                                engine,
                                Engine::StarRocks | Engine::ClickHouse | Engine::MySql
                            ) =>
                        {
                            v.replace('\\', "\\\\")
                        }
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
    /// Prefer the engine's own vendor expressions (`--prefer DUCKDB`, `--prefer TRINO`, ...).
    pub prefer: bool,
}

pub const CASES: &[Case] = &[
    Case {
        // String comparison is case-sensitive everywhere but MySQL's default
        // collation: 'ada' must not also match 'Ada'.
        name: "case_sensitive_filter",
        request: r#"{"dimensions": ["customers.name"], "filters": [{"field": "customers.name", "op": "IN", "values": ["ada", "bob"]}]}"#,
        reference: "SELECT DISTINCT name FROM {customers} WHERE name IN ('ada', 'bob')",
        prefer: false,
    },
    Case {
        // A backslash in a LIKE pattern is an ordinary character (SQL declares
        // escapes with ESCAPE), but some engines escape with it by default.
        // `C:\%` must match names starting with `C:\`.
        name: "like_with_a_backslash",
        request: r#"{"dimensions": ["customers.name"], "filters": [{"field": "customers.name", "op": "LIKE", "value": "C:\\%"}]}"#,
        reference: r"SELECT DISTINCT name FROM {customers} WHERE name LIKE 'C:\%'",
        prefer: false,
    },
    Case {
        // COUNT over no rows is 0, not NULL (wide table 5.5). The order with no
        // product is a NULL group from its own branch, which `products` has no row for.
        name: "count_is_zero_for_a_group_without_rows",
        request: r#"{"metrics": ["total_revenue", "products_sold"], "dimensions": ["products.category"]}"#,
        reference: "WITH rev AS (
              SELECT p.category, SUM(o.amount_cents / 100.0) AS r
              FROM orders o LEFT JOIN products p ON o.product_id = p.product_id GROUP BY p.category
            ), sold AS (SELECT category, COUNT(*) AS n FROM products GROUP BY category)
            SELECT COALESCE(rev.category, sold.category), rev.r, COALESCE(sold.n, 0)
            FROM rev FULL JOIN sold ON rev.category IS NOT DISTINCT FROM sold.category",
        prefer: false,
    },
    Case {
        // The spec's `filter`: one expression over one dataset, in every branch.
        // `<>` removes an order with no customer, whose region is NULL (wide table 3.4).
        name: "filter_expression",
        request: r#"{"metrics": ["total_revenue", "customers.known_customers"], "dimensions": ["customers.segment"],
            "filter": "customers.region <> 'US' AND customers.segment LIKE '%m%'"}"#,
        reference: "WITH rev AS (
              SELECT c.segment, SUM(o.amount_cents / 100.0) AS r
              FROM orders o LEFT JOIN {customers} c ON o.customer_id = c.customer_id
              WHERE c.region <> 'US' AND c.segment LIKE '%m%' GROUP BY c.segment
            ), n AS (
              SELECT segment, COUNT(*) AS n FROM {customers}
              WHERE region <> 'US' AND segment LIKE '%m%' GROUP BY segment
            )
            SELECT COALESCE(rev.segment, n.segment), rev.r, COALESCE(n.n, 0)
            FROM rev FULL JOIN n ON rev.segment IS NOT DISTINCT FROM n.segment",
        prefer: false,
    },
    Case {
        // Aliases name the columns, with a field selected twice and a role-free
        // sort by an alias. Output names that equal a source column must not capture
        // the field's own column in any clause (`amount_cents`, `status`).
        name: "aliases_and_output_names",
        request: r#"{"dimensions": ["orders.status", {"field": "orders.status", "alias": "state"}],
            "metrics": [{"metric": "total_revenue", "alias": "amount_cents"}, "orders.order_count"],
            "filter": "orders.amount_cents > 100",
            "metricFilters": [{"metric": "total_revenue", "op": ">", "value": 1}],
            "orderBy": [{"field": "amount_cents", "direction": "desc"}]}"#,
        reference: "SELECT status, status, SUM(amount_cents / 100.0) AS r, COUNT(*)
            FROM orders WHERE amount_cents > 100
            GROUP BY status HAVING SUM(amount_cents / 100.0) > 1 ORDER BY r DESC",
        prefer: false,
    },
    Case {
        // A filter with no dataset still gives one row, of empty-input values.
        name: "filter_without_a_dataset",
        request: r#"{"metrics": ["total_revenue", "orders.order_count", "total_stock"], "filter": "1 = 0"}"#,
        reference: "SELECT CAST(NULL AS DOUBLE), 0, CAST(NULL AS INTEGER)",
        prefer: false,
    },
    Case {
        // Weeks start on Monday (Ossie expression language, DATE_TRUNC).
        name: "week_grain",
        request: r#"{"metrics": ["total_revenue"], "dimensions": ["orders.order_date"], "timeGrain": "week",
            "orderBy": [{"field": "orders.order_date"}]}"#,
        reference: "SELECT DATE_TRUNC('week', order_date), SUM(amount_cents / 100.0) FROM orders GROUP BY 1 ORDER BY 1",
        prefer: false,
    },
    Case {
        name: "quarter_grain",
        request: r#"{"metrics": ["total_revenue"], "dimensions": ["orders.order_date"], "timeGrain": "quarter"}"#,
        reference: "SELECT DATE_TRUNC('quarter', order_date), SUM(amount_cents / 100.0) FROM orders GROUP BY 1",
        prefer: false,
    },
    Case {
        // A quote and a backslash in filter values, which engines escape differently.
        name: "string_literals_with_quote_and_backslash",
        request: r#"{"dimensions": ["customers.name"],
            "filters": [{"field": "customers.name", "op": "IN", "values": ["O'Brien", "C:\\temp", "Nobody"]}]}"#,
        reference: r"SELECT DISTINCT name FROM {customers} WHERE name IN ('O''Brien', 'C:\temp')",
        prefer: false,
    },
    Case {
        name: "like_with_a_quote",
        request: r#"{"dimensions": ["customers.name"], "filters": [{"field": "customers.name", "op": "LIKE", "value": "%'%"}]}"#,
        reference: "SELECT DISTINCT name FROM {customers} WHERE name LIKE '%''%'",
        prefer: false,
    },
    Case {
        // Byte order on every engine: 'Bob' < 'ada' ('B' = 66 < 'a' = 97). Locale
        // collations (Postgres en_US) would put 'ada' first.
        name: "string_ordering",
        request: r#"{"dimensions": ["customers.name"], "orderBy": [{"field": "customers.name"}]}"#,
        reference: "SELECT DISTINCT name FROM {customers} ORDER BY name",
        prefer: false,
    },
    Case {
        name: "averages_of_integers_and_one_side_values",
        request: r#"{"metrics": ["avg_stock", "avg_amount_cents"], "dimensions": ["products.category"]}"#,
        reference: "WITH st AS (
              SELECT category, AVG(stock) AS s FROM products GROUP BY category
            ), g AS (
              SELECT p.category, AVG(o.amount_cents) AS a
              FROM orders o LEFT JOIN products p ON o.product_id = p.product_id GROUP BY p.category
            )
            SELECT COALESCE(st.category, g.category), st.s, g.a
            FROM st FULL JOIN g ON st.category IS NOT DISTINCT FROM g.category",
        prefer: false,
    },
    Case {
        // The engine's own vendor expression for `order_yyyymm`, executed on that engine.
        name: "vendor_expression_on_its_engine",
        request: r#"{"metrics": ["total_revenue"], "dimensions": ["orders.order_yyyymm"]}"#,
        reference: "SELECT EXTRACT(YEAR FROM order_date) * 100 + EXTRACT(MONTH FROM order_date), SUM(amount_cents / 100.0)
            FROM orders GROUP BY 1",
        prefer: true,
    },
    Case {
        // Values of one dataset's dimension: every customer counts, even one who never ordered.
        name: "dimension_values",
        request: r#"{"dimensions": ["customers.region"]}"#,
        reference: "SELECT DISTINCT region FROM {customers}",
        prefer: false,
    },
    Case {
        // Dimensions from three datasets: the anchor is orders, the one that reaches
        // the others, so these are the combinations some order links.
        name: "dimension_combinations",
        request: r#"{"dimensions": ["orders.status", "customers.segment", "products.category"],
            "filters": [{"field": "orders.status", "op": "=", "value": "completed"}],
            "orderBy": [{"field": "customers.segment"}, {"field": "products.category"}]}"#,
        reference: "SELECT DISTINCT o.status, c.segment, p.category
            FROM orders o
            LEFT JOIN {customers} c ON o.customer_id = c.customer_id
            LEFT JOIN products p ON o.product_id = p.product_id
            WHERE o.status = 'completed'
            ORDER BY c.segment NULLS FIRST, p.category NULLS FIRST",
        prefer: false,
    },
    Case {
        // `segment` is never NULL in the data, so on ClickHouse it's a non-Nullable column:
        // the guest checkout must still land in the NULL group, not under segment ''.
        name: "orphans_on_a_non_nullable_column",
        request: r#"{"metrics": ["total_revenue"], "dimensions": ["customers.segment"]}"#,
        reference: "SELECT c.segment, SUM(o.amount_cents / 100.0)
            FROM orders o LEFT JOIN {customers} c ON o.customer_id = c.customer_id
            GROUP BY c.segment",
        prefer: false,
    },
    Case {
        name: "integer_ratio",
        request: r#"{"metrics": ["orders_per_customer"], "dimensions": ["customers.segment"]}"#,
        reference: "WITH o AS (
              SELECT c.segment, COUNT(o.order_id) AS n
              FROM orders o LEFT JOIN {customers} c ON o.customer_id = c.customer_id GROUP BY c.segment
            ), c AS (SELECT segment, COUNT(DISTINCT customer_id) AS n FROM {customers} GROUP BY segment)
            SELECT COALESCE(o.segment, c.segment), o.n / NULLIF(c.n, 0)
            FROM o FULL JOIN c ON o.segment IS NOT DISTINCT FROM c.segment",
        prefer: false,
    },
    Case {
        name: "revenue_by_region",
        request: r#"{"metrics": ["total_revenue"], "dimensions": ["customers.region"]}"#,
        reference: "SELECT c.region, SUM(o.amount_cents / 100.0)
            FROM orders o LEFT JOIN {customers} c ON o.customer_id = c.customer_id
            GROUP BY c.region",
        prefer: false,
    },
    Case {
        name: "dataset_metric_and_filter_lowered",
        request: r#"{"metrics": ["orders.order_count", "completed_revenue"], "dimensions": ["products.category"]}"#,
        reference: "SELECT p.category, COUNT(*), SUM(CASE WHEN o.status = 'completed' THEN o.amount_cents / 100.0 END)
            FROM orders o LEFT JOIN products p ON o.product_id = p.product_id
            GROUP BY p.category",
        prefer: false,
    },
    Case {
        name: "count_distinct_buyers",
        request: r#"{"metrics": ["revenue_per_customer", "buying_customers"], "dimensions": ["customers.region"],
            "filters": [{"field": "customers.segment", "op": "=", "value": "smb"}]}"#,
        reference: "WITH r AS (
              SELECT c.region, SUM(o.amount_cents / 100.0) AS v
              FROM orders o LEFT JOIN {customers} c ON o.customer_id = c.customer_id
              WHERE c.segment = 'smb' GROUP BY c.region
            ), n AS (SELECT region, COUNT(DISTINCT customer_id) AS v FROM {customers} WHERE segment = 'smb' GROUP BY region)
            SELECT COALESCE(r.region, n.region), r.v / NULLIF(n.v, 0), COALESCE(n.v, 0)
            FROM r FULL JOIN n ON r.region IS NOT DISTINCT FROM n.region",
        prefer: false,
    },
    Case {
        name: "ratio_across_grains",
        request: r#"{"metrics": ["total_revenue", "total_stock", "revenue_per_stock_unit"], "dimensions": ["products.category"]}"#,
        reference: "WITH rev AS (
              SELECT p.category, SUM(o.amount_cents / 100.0) AS r
              FROM orders o LEFT JOIN products p ON o.product_id = p.product_id GROUP BY p.category
            ), stock AS (
              -- Each aggregate reads its home dataset (wide table 5.2): all products,
              -- including product 13, which never sold.
              SELECT category, SUM(stock) AS s FROM products GROUP BY category
            )
            SELECT COALESCE(rev.category, stock.category), rev.r, stock.s, rev.r / NULLIF(stock.s, 0)
            FROM rev FULL JOIN stock ON rev.category IS NOT DISTINCT FROM stock.category",
        prefer: false,
    },
    Case {
        name: "measure_alone_at_its_grain",
        request: r#"{"metrics": ["total_stock"]}"#,
        reference: "SELECT SUM(stock) FROM products",
        prefer: false,
    },
    Case {
        // Datasets that only meet through a shared dimension, with no dimension.
        name: "grand_totals_across_grains",
        request: r#"{"metrics": ["total_revenue", "total_stock"]}"#,
        reference: "SELECT (SELECT SUM(amount_cents / 100.0) FROM orders), (SELECT SUM(stock) FROM products)",
        prefer: false,
    },
    Case {
        name: "month_grain_with_date_range",
        request: r#"{"metrics": ["total_revenue"], "dimensions": ["orders.order_date"], "timeGrain": "month",
            "filters": [{"field": "orders.order_date", "op": "BETWEEN", "values": ["2026-01-01", "2026-02-28"]}]}"#,
        reference: "SELECT DATE_TRUNC('month', order_date), SUM(amount_cents / 100.0) FROM orders
            WHERE order_date BETWEEN DATE '2026-01-01' AND DATE '2026-02-28'
            GROUP BY 1",
        prefer: false,
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
        prefer: false,
    },
    Case {
        name: "filter_on_query_source",
        request: r#"{"metrics": ["avg_order_value"], "dimensions": ["customers.segment"],
            "filters": [{"field": "customers.region", "op": "IN", "values": ["EU", "US"]}]}"#,
        reference: "SELECT c.segment, AVG(o.amount_cents / 100.0)
            FROM orders o LEFT JOIN {customers} c ON o.customer_id = c.customer_id
            WHERE c.region IN ('EU', 'US')
            GROUP BY c.segment",
        prefer: false,
    },
    Case {
        name: "running_total_and_change",
        request: r#"{"metrics": ["total_revenue", "cumulative_revenue", "revenue_change"], "dimensions": ["orders.order_date"],
            "timeGrain": "month", "deferredMetrics": true, "orderBy": [{"field": "orders.order_date"}]}"#,
        reference: "SELECT m, r,
                SUM(r) OVER (ORDER BY m ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW),
                r - LAG(r, 1) OVER (ORDER BY m)
            FROM (SELECT DATE_TRUNC('month', order_date) AS m, SUM(amount_cents / 100.0) AS r FROM orders GROUP BY 1)
            ORDER BY m",
        prefer: false,
    },
    Case {
        // The last period has no next row: NULL, not the type's default.
        name: "change_to_next_period",
        request: r#"{"metrics": ["total_revenue", "revenue_change_to_next"], "dimensions": ["orders.order_date"],
            "timeGrain": "month", "deferredMetrics": true, "orderBy": [{"field": "orders.order_date"}]}"#,
        reference: "SELECT m, r, LEAD(r, 1) OVER (ORDER BY m) - r
            FROM (SELECT DATE_TRUNC('month', order_date) AS m, SUM(amount_cents / 100.0) AS r FROM orders GROUP BY 1)
            ORDER BY m",
        prefer: false,
    },
    Case {
        name: "rank_filter_after_window",
        request: r#"{"metrics": ["revenue_rank_in_region", "total_revenue"], "dimensions": ["customers.region", "products.category"], "deferredMetrics": true,
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
        prefer: false,
    },
    Case {
        // A window over stacked branches sees every group, here the NULL category
        // of the order with no product, whose stock is NULL.
        name: "window_over_two_branches",
        request: r#"{"metrics": ["stock_share", "total_revenue"], "dimensions": ["products.category"], "deferredMetrics": true}"#,
        reference: "WITH stock AS (SELECT category, SUM(stock) AS s FROM products GROUP BY category),
            rev AS (
              SELECT p.category, SUM(o.amount_cents / 100.0) AS r
              FROM orders o LEFT JOIN products p ON o.product_id = p.product_id GROUP BY p.category
            )
            SELECT COALESCE(stock.category, rev.category), stock.s / SUM(stock.s) OVER (), rev.r
            FROM stock FULL JOIN rev ON stock.category IS NOT DISTINCT FROM rev.category",
        prefer: false,
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
    let prefer = case.prefer.then(|| match target {
        Some(ossiec::DialectType::PostgreSQL) => "POSTGRES".to_string(),
        Some(d) => format!("{d:?}").to_ascii_uppercase(),
        None => "DUCKDB".to_string(),
    });
    let opts = ossiec::Options {
        target,
        prefer_dialect: prefer,
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

// ------------------------------------------------- Layer 2 nested (A.2 / A.3) shop forms

/// Layer 2 SQL, its reference (plain DuckDB SQL) per case: `(name, layer2, reference)`.
pub const A2_SHOP: &str = "SELECT o.status, AGG(o.order_count) \
    FROM orders o \
    JOIN (SELECT customer_id, AGG(order_count) AS n \
          FROM orders GROUP BY customer_id \
          ORDER BY n DESC, customer_id LIMIT 2) t \
      ON o.customer_id = t.customer_id \
    GROUP BY o.status";

pub const A2_SHOP_REF: &str = "SELECT o.status, COUNT(*) FROM orders o \
    JOIN (SELECT customer_id FROM orders GROUP BY customer_id \
          ORDER BY COUNT(*) DESC, customer_id LIMIT 2) t \
      ON o.customer_id = t.customer_id \
    GROUP BY o.status";

pub const A3_SHOP: &str = "SELECT d.status, AVG(m.n) \
    FROM (SELECT status, customer_id FROM orders GROUP BY status, customer_id) d \
    JOIN (SELECT customer_id, AGG(order_count) AS n FROM orders GROUP BY customer_id) m \
      ON d.customer_id IS NOT DISTINCT FROM m.customer_id \
    GROUP BY d.status";

pub const A3_SHOP_REF: &str = "SELECT d.status, AVG(m.n) \
    FROM (SELECT status, customer_id FROM orders GROUP BY status, customer_id) d \
    JOIN (SELECT customer_id, COUNT(*) AS n FROM orders GROUP BY customer_id) m \
      ON d.customer_id IS NOT DISTINCT FROM m.customer_id \
    GROUP BY d.status";

/// §7.7 fold form: a sole derived `FROM` filters and projects; the outer `AGG` evaluates
/// the passed-through measure (on shop, dataset-scoped `order_count` stands in).
pub const S77_FOLD_SHOP: &str = "SELECT status, AGG(order_count) AS n \
    FROM (SELECT status, order_count FROM orders WHERE customer_id IS NOT NULL) t \
    GROUP BY status";

pub const S77_FOLD_SHOP_REF: &str =
    "SELECT status, COUNT(*) AS n FROM orders WHERE customer_id IS NOT NULL GROUP BY status";

/// §7.7 fold form with an outer `WHERE` on top of the inner filter.
pub const S77_FOLD_WHERE_SHOP: &str = "SELECT status, AGG(order_count) AS n \
    FROM (SELECT status, order_count FROM orders WHERE customer_id IS NOT NULL) t \
    WHERE status = 'completed' \
    GROUP BY status";

pub const S77_FOLD_WHERE_SHOP_REF: &str = "SELECT status, COUNT(*) AS n FROM orders \
    WHERE customer_id IS NOT NULL AND status = 'completed' GROUP BY status";

/// A.2 join form with an unevaluated measure passed through the derived table:
/// the derived table only restricts (customers who ordered); the outer stage evaluates.
pub const A2_UNEVALUATED_SHOP: &str = "SELECT o.status, AGG(t.order_count) \
    FROM orders o \
    JOIN (SELECT customer_id, order_count FROM orders) t \
      ON o.customer_id = t.customer_id \
    GROUP BY o.status";

pub const A2_UNEVALUATED_SHOP_REF: &str =
    "SELECT status, COUNT(*) FROM orders WHERE customer_id IS NOT NULL GROUP BY status";

/// Base `COUNT(*) TO MEASURE` folded through a sole derived `FROM` (shop).
pub const BASE_TO_MEASURE_SHOP: &str = "SELECT status, AGG(n) AS n \
    FROM (SELECT status, COUNT(*) TO MEASURE AS n FROM orders \
          WHERE customer_id IS NOT NULL) t \
    WHERE status = 'completed' \
    GROUP BY status";

pub const BASE_TO_MEASURE_SHOP_REF: &str = "SELECT status, COUNT(*) AS n FROM orders \
    WHERE customer_id IS NOT NULL AND status = 'completed' GROUP BY status";

/// `includeUnmatched` with a derived join on the product grain. Product 13
/// (stock 7, never sold) is kept by the filter and appears under a NULL region.
pub const UNMATCHED_DERIVED_SHOP: &str = "-- ossiec: includeUnmatched\n\
    SELECT customers.region, AGG(products.stock_units) \
    FROM orders \
    LEFT JOIN customers ON orders.customer_id = customers.id \
    LEFT JOIN products ON orders.product_id = products.id \
    JOIN (SELECT id FROM products WHERE stock > 0 GROUP BY id) t \
      ON products.id = t.id \
    GROUP BY customers.region";

/// Portable reference for [`UNMATCHED_DERIVED_SHOP`] (DuckDB / engine seed).
pub const UNMATCHED_DERIVED_SHOP_REF: &str =
    "WITH keep AS (SELECT product_id AS id FROM products WHERE stock > 0 GROUP BY product_id),
    matched AS (
       SELECT DISTINCT c.region, p.product_id, p.stock
       FROM orders o
       LEFT JOIN raw_customers c ON o.customer_id = c.customer_id AND NOT c.deleted
       LEFT JOIN products p ON o.product_id = p.product_id
       JOIN keep t ON p.product_id = t.id
       WHERE p.product_id IS NOT NULL
    ), unmatched AS (
       SELECT CAST(NULL AS VARCHAR) AS region, p.product_id, p.stock
       FROM products p
       JOIN keep t ON p.product_id = t.id
       WHERE NOT EXISTS (SELECT 1 FROM orders o WHERE o.product_id = p.product_id)
    ), r AS (
       SELECT * FROM matched UNION ALL SELECT * FROM unmatched
    ), g AS (
       SELECT DISTINCT region FROM (
         SELECT c.region AS region
         FROM orders o
         LEFT JOIN raw_customers c ON o.customer_id = c.customer_id AND NOT c.deleted
         LEFT JOIN products p ON o.product_id = p.product_id
         JOIN keep t ON p.product_id = t.id
         UNION ALL SELECT region FROM unmatched
       ) u
    )
    SELECT g.region, s.stock
    FROM g
    LEFT JOIN (SELECT region, SUM(stock) AS stock FROM r GROUP BY region) s
      ON g.region IS NOT DISTINCT FROM s.region";

/// A.7 session view with a base `TO MEASURE`, then an outer `AGG`.
pub const A7_BASE_TO_MEASURE_SCRIPT: &str = "CREATE VIEW completed_counts AS \
    SELECT status, COUNT(*) TO MEASURE AS n FROM orders \
    WHERE customer_id IS NOT NULL; \
    SELECT status, AGG(n) AS n FROM completed_counts \
    WHERE status = 'completed' GROUP BY status";

pub const A7_BASE_TO_MEASURE_SCRIPT_REF: &str = BASE_TO_MEASURE_SHOP_REF;

/// One shop Layer 2 nested case for the DuckDB parity check and the engine tier.
#[derive(Clone, Copy)]
pub struct Layer2NestedCase {
    pub name: &'static str,
    pub sql: &'static str,
    pub reference: &'static str,
    /// `CREATE VIEW …; SELECT …` via [`ossiec::compile_sql_script`].
    pub script: bool,
}

/// Layer 3 reads each metric's home dataset; Layer 2 reads the consumer's joins, so
/// a measure on the one side needs de-duplicating there. This spells those joins.
macro_rules! shop_joins {
    ($select:literal, $rest:literal) => {
        concat!(
            $select,
            "FROM orders o LEFT JOIN customers c ON o.customer_id = c.id ",
            "LEFT JOIN products p ON o.product_id = p.id",
            $rest
        )
    };
}

pub const LAYER2_NESTED_CASES: &[Layer2NestedCase] = &[
    Layer2NestedCase {
        // §7.4: `GROUP BY ALL` groups by the selected fields.
        name: "group_by_all",
        sql: "SELECT status, AGG(order_count) AS n FROM orders GROUP BY ALL",
        reference: "SELECT status, COUNT(*) AS n FROM orders GROUP BY status",
        script: false,
    },
    Layer2NestedCase {
        // §7.8: an outer query's rows can repeat, and a stage's can't.
        name: "distinct_over_a_derived_table",
        sql: "SELECT DISTINCT t.status FROM \
              (SELECT status, customer_id FROM orders GROUP BY status, customer_id) t",
        reference: "SELECT DISTINCT status FROM orders",
        script: false,
    },
    Layer2NestedCase {
        name: "distinct_over_fields_and_measures",
        sql: "SELECT DISTINCT status, AGG(order_count) AS n FROM orders GROUP BY status",
        reference: "SELECT status, COUNT(*) AS n FROM orders GROUP BY status",
        script: false,
    },
    Layer2NestedCase {
        // §7.5: a Top-N by a measure nothing selects, stacked with a de-duplicated part.
        name: "top_n_by_an_unselected_measure",
        sql: shop_joins!(
            "SELECT c.region, AGG(o.order_count) AS n ",
            " GROUP BY c.region ORDER BY AGG(p.stock_units) * -1 NULLS LAST, c.region LIMIT 2"
        ),
        reference: "WITH n AS (
              SELECT c.region, COUNT(*) AS n
              FROM orders o LEFT JOIN {customers} c ON o.customer_id = c.customer_id GROUP BY c.region
            ), st AS (
              SELECT region, SUM(stock) AS v FROM (
                SELECT DISTINCT c.region, p.product_id, p.stock
                FROM orders o
                LEFT JOIN {customers} c ON o.customer_id = c.customer_id
                LEFT JOIN products p ON o.product_id = p.product_id
                WHERE p.product_id IS NOT NULL
              ) GROUP BY region
            )
            SELECT n.region, n.n FROM n LEFT JOIN st ON n.region IS NOT DISTINCT FROM st.region
            ORDER BY st.v * -1 NULLS LAST, n.region NULLS FIRST LIMIT 2",
        script: false,
    },
    Layer2NestedCase {
        // §7.5: over the finished groups, on a selected measure.
        name: "having_on_a_selected_measure",
        sql: "SELECT status, AGG(order_count) AS n FROM orders GROUP BY status HAVING AGG(order_count) > 1",
        reference: "SELECT status, COUNT(*) FROM orders GROUP BY status HAVING COUNT(*) > 1",
        script: false,
    },
    Layer2NestedCase {
        // A measure only HAVING reads, de-duplicated in its own stacked part, and a
        // ratio and an OR across the group: no fan-out in what it compares.
        name: "having_on_an_unselected_one_side_measure",
        sql: shop_joins!(
            "SELECT c.region, AGG(o.gross_revenue) ",
            " GROUP BY c.region HAVING AGG(p.stock_units) >= 100 OR AGG(o.gross_revenue) / AGG(o.order_count) > 30"
        ),
        reference: "WITH r AS (
              SELECT c.region, SUM(o.amount_cents / 100.0) AS v, COUNT(*) AS n
              FROM orders o LEFT JOIN {customers} c ON o.customer_id = c.customer_id GROUP BY c.region
            ), st AS (
              SELECT region, SUM(stock) AS v FROM (
                SELECT DISTINCT c.region, p.product_id, p.stock
                FROM orders o
                LEFT JOIN {customers} c ON o.customer_id = c.customer_id
                LEFT JOIN products p ON o.product_id = p.product_id
                WHERE p.product_id IS NOT NULL
              ) GROUP BY region
            )
            SELECT r.region, r.v FROM r LEFT JOIN st ON r.region IS NOT DISTINCT FROM st.region
            WHERE st.v >= 100 OR r.v / r.n > 30",
        script: false,
    },
    Layer2NestedCase {
        // A grouping column in HAVING compares as WHERE does: bytes, on every engine.
        name: "having_on_a_grouping_column",
        sql: "SELECT c.name, AGG(c.known_customers) AS n FROM customers c GROUP BY c.name HAVING c.name = 'ada'",
        reference: "SELECT name, COUNT(*) FROM {customers} WHERE name = 'ada' GROUP BY name",
        script: false,
    },
    Layer2NestedCase {
        // §7.7: a CTE is a derived table, so a measure passes through it unevaluated.
        name: "cte_passes_a_measure_through",
        sql: "WITH known AS (SELECT status, order_count FROM orders WHERE customer_id IS NOT NULL) \
            SELECT status, AGG(order_count) AS n FROM known GROUP BY status",
        reference: "SELECT status, COUNT(*) FROM orders WHERE customer_id IS NOT NULL GROUP BY status",
        script: false,
    },
    Layer2NestedCase {
        // One CTE reading another, joined, with HAVING inside it.
        name: "cte_chain_joined_with_having",
        sql: "WITH counts AS (SELECT customer_id, AGG(order_count) AS n FROM orders GROUP BY customer_id HAVING AGG(order_count) > 1), \
                  regulars AS (SELECT customer_id FROM counts GROUP BY customer_id) \
            SELECT o.status, AGG(o.order_count) AS n FROM orders o \
            JOIN regulars r ON o.customer_id = r.customer_id GROUP BY o.status",
        reference: "SELECT o.status, COUNT(*) FROM orders o
            JOIN (SELECT customer_id FROM orders GROUP BY customer_id HAVING COUNT(*) > 1) r
              ON o.customer_id = r.customer_id
            GROUP BY o.status",
        script: false,
    },
    Layer2NestedCase {
        // Groups come from every joined row (a group with no input rows is kept),
        // so APAC, whose only order has no product, appears with NULL stock.
        name: "one_side_measure_dedup",
        sql: shop_joins!("SELECT c.region, AGG(p.stock_units) ", " GROUP BY c.region"),
        reference: "SELECT g.region, s.stock
            FROM (SELECT DISTINCT c.region FROM orders o LEFT JOIN {customers} c ON o.customer_id = c.customer_id) g
            LEFT JOIN (
              SELECT region, SUM(stock) AS stock FROM (
                SELECT DISTINCT c.region, p.product_id, p.stock
                FROM orders o
                LEFT JOIN {customers} c ON o.customer_id = c.customer_id
                LEFT JOIN products p ON o.product_id = p.product_id
                WHERE p.product_id IS NOT NULL
              ) GROUP BY region
            ) s ON g.region IS NOT DISTINCT FROM s.region",
        script: false,
    },
    Layer2NestedCase {
        name: "one_side_average_dedup",
        sql: shop_joins!("SELECT c.region, AGG(p.average_stock) ", " GROUP BY c.region"),
        reference: "SELECT g.region, s.a
            FROM (SELECT DISTINCT c.region FROM orders o LEFT JOIN {customers} c ON o.customer_id = c.customer_id) g
            LEFT JOIN (
              SELECT region, AVG(stock) AS a FROM (
                SELECT DISTINCT c.region, p.product_id, p.stock
                FROM orders o
                LEFT JOIN {customers} c ON o.customer_id = c.customer_id
                LEFT JOIN products p ON o.product_id = p.product_id
                WHERE p.product_id IS NOT NULL
              ) GROUP BY region
            ) s ON g.region IS NOT DISTINCT FROM s.region",
        script: false,
    },
    Layer2NestedCase {
        // Three stacked parts (orders inline, products and customers de-duplicated)
        // and a filter on a one-side dataset, which every part must apply.
        name: "three_parts_with_a_one_side_filter",
        sql: shop_joins!(
            "SELECT p.category, AGG(o.gross_revenue), AGG(p.stock_units), AGG(c.known_customers) ",
            " WHERE c.segment = 'smb' GROUP BY p.category"
        ),
        reference: "WITH j AS (
              SELECT p.category, p.product_id, p.stock, c.customer_id AS cid, o.amount_cents
              FROM orders o
              LEFT JOIN {customers} c ON o.customer_id = c.customer_id
              LEFT JOIN products p ON o.product_id = p.product_id
              WHERE c.segment = 'smb'
            ), g AS (SELECT category, SUM(amount_cents / 100.0) AS r FROM j GROUP BY category),
            st AS (SELECT category, SUM(stock) AS s FROM (SELECT DISTINCT category, product_id, stock FROM j WHERE product_id IS NOT NULL) GROUP BY category),
            cc AS (SELECT category, COUNT(*) AS n FROM (SELECT DISTINCT category, cid FROM j WHERE cid IS NOT NULL) GROUP BY category)
            SELECT g.category, g.r, st.s, COALESCE(cc.n, 0)
            FROM g
            LEFT JOIN st ON g.category IS NOT DISTINCT FROM st.category
            LEFT JOIN cc ON g.category IS NOT DISTINCT FROM cc.category",
        script: false,
    },
    Layer2NestedCase {
        // includeUnmatched: product 13 never sold, so no order reaches it. It
        // joins the NULL-region group (its region is NULL), with its stock.
        name: "unmatched_products_included",
        sql: concat!(
            "-- ossiec: includeUnmatched\n",
            shop_joins!("SELECT c.region, AGG(p.stock_units), AGG(p.product_count) ", " GROUP BY c.region")
        ),
        reference: "WITH matched AS (
              SELECT DISTINCT c.region, p.product_id, p.stock
              FROM orders o
              LEFT JOIN {customers} c ON o.customer_id = c.customer_id
              LEFT JOIN products p ON o.product_id = p.product_id
              WHERE p.product_id IS NOT NULL
            ), unmatched AS (
              SELECT DISTINCT CAST(NULL AS VARCHAR) AS region, p.product_id, p.stock
              FROM products p WHERE NOT EXISTS (SELECT 1 FROM orders o WHERE o.product_id = p.product_id)
            ), r AS (SELECT * FROM matched UNION ALL SELECT * FROM unmatched),
            g AS (
              SELECT DISTINCT c.region FROM orders o LEFT JOIN {customers} c ON o.customer_id = c.customer_id
              UNION SELECT region FROM unmatched
            )
            SELECT g.region, s.stock, COALESCE(s.n, 0)
            FROM g LEFT JOIN (SELECT region, SUM(stock) AS stock, COUNT(*) AS n FROM r GROUP BY region) s
              ON g.region IS NOT DISTINCT FROM s.region",
        script: false,
    },
    Layer2NestedCase {
        // A filter on the fact side can't pass for unmatched rows (their region
        // is NULL), so they add nothing.
        name: "unmatched_rows_fail_fact_side_filters",
        sql: concat!(
            "-- ossiec: includeUnmatched\n",
            shop_joins!("SELECT c.region, AGG(p.stock_units) ", " WHERE c.region IN ('EU', 'US') GROUP BY c.region")
        ),
        reference: "SELECT region, SUM(stock) FROM (
              SELECT DISTINCT c.region, p.product_id, p.stock
              FROM orders o
              LEFT JOIN {customers} c ON o.customer_id = c.customer_id
              LEFT JOIN products p ON o.product_id = p.product_id
              WHERE c.region IN ('EU', 'US') AND p.product_id IS NOT NULL
            ) GROUP BY region",
        script: false,
    },
    Layer2NestedCase {
        // WHERE beyond comparisons (§7.3): OR, LIKE, BETWEEN, NOT and an expression,
        // across datasets, applied to every stacked part.
        name: "where_is_any_row_predicate",
        sql: shop_joins!(
            "SELECT c.region, AGG(o.gross_revenue), AGG(p.stock_units) ",
            " WHERE (c.segment = 'smb' OR p.category LIKE 'g%') AND o.amount_cents BETWEEN 500 AND 4000 AND NOT o.status = 'returned' AND LOWER(o.status) = o.status GROUP BY c.region"
        ),
        reference: "WITH j AS (
              SELECT c.region, o.amount_cents, p.product_id, p.stock
              FROM orders o
              LEFT JOIN {customers} c ON o.customer_id = c.customer_id
              LEFT JOIN products p ON o.product_id = p.product_id
              WHERE (c.segment = 'smb' OR p.category LIKE 'g%') AND o.amount_cents BETWEEN 500 AND 4000
                AND NOT o.status = 'returned' AND LOWER(o.status) = o.status
            ), r AS (SELECT region, SUM(amount_cents / 100.0) AS v FROM j GROUP BY region),
            st AS (
              SELECT region, SUM(stock) AS v FROM (SELECT DISTINCT region, product_id, stock FROM j WHERE product_id IS NOT NULL)
              GROUP BY region
            )
            SELECT r.region, r.v, st.v FROM r LEFT JOIN st ON r.region IS NOT DISTINCT FROM st.region",
        script: false,
    },
    Layer2NestedCase {
        // COUNT(DISTINCT) of a one-side key is normally inline; with
        // includeUnmatched it is de-duplicated so customers who never ordered
        // (Eve, ada, C:\temp) are counted, in the NULL-category group.
        name: "unmatched_customers_counted",
        sql: concat!(
            "-- ossiec: includeUnmatched\n",
            shop_joins!("SELECT p.category, AGG(c.distinct_customers) ", " GROUP BY p.category")
        ),
        reference: "WITH matched AS (
              SELECT DISTINCT p.category, c.customer_id
              FROM orders o
              LEFT JOIN {customers} c ON o.customer_id = c.customer_id
              LEFT JOIN products p ON o.product_id = p.product_id
              WHERE c.customer_id IS NOT NULL
            ), unmatched AS (
              SELECT DISTINCT CAST(NULL AS VARCHAR) AS category, c.customer_id
              FROM {customers} c WHERE NOT EXISTS (SELECT 1 FROM orders o WHERE o.customer_id = c.customer_id)
            ), r AS (SELECT * FROM matched UNION ALL SELECT * FROM unmatched),
            g AS (
              SELECT DISTINCT p.category FROM orders o LEFT JOIN products p ON o.product_id = p.product_id
              UNION SELECT category FROM unmatched
            )
            SELECT g.category, COALESCE(s.n, 0)
            FROM g LEFT JOIN (SELECT category, COUNT(*) AS n FROM r GROUP BY category) s
              ON g.category IS NOT DISTINCT FROM s.category",
        script: false,
    },
    Layer2NestedCase {
        name: "a2_top_two_customers",
        sql: A2_SHOP,
        reference: A2_SHOP_REF,
        script: false,
    },
    Layer2NestedCase {
        name: "a3_avg_orders_per_customer",
        sql: A3_SHOP,
        reference: A3_SHOP_REF,
        script: false,
    },
    Layer2NestedCase {
        name: "s77_fold_filter_and_projection",
        sql: S77_FOLD_SHOP,
        reference: S77_FOLD_SHOP_REF,
        script: false,
    },
    Layer2NestedCase {
        name: "s77_fold_with_outer_where",
        sql: S77_FOLD_WHERE_SHOP,
        reference: S77_FOLD_WHERE_SHOP_REF,
        script: false,
    },
    Layer2NestedCase {
        name: "a2_unevaluated_measure_pass_through",
        sql: A2_UNEVALUATED_SHOP,
        reference: A2_UNEVALUATED_SHOP_REF,
        script: false,
    },
    Layer2NestedCase {
        name: "base_to_measure_count_star",
        sql: BASE_TO_MEASURE_SHOP,
        reference: BASE_TO_MEASURE_SHOP_REF,
        script: false,
    },
    Layer2NestedCase {
        name: "include_unmatched_with_derived_join",
        sql: UNMATCHED_DERIVED_SHOP,
        reference: UNMATCHED_DERIVED_SHOP_REF,
        script: false,
    },
    Layer2NestedCase {
        name: "a7_create_view_base_to_measure",
        sql: A7_BASE_TO_MEASURE_SCRIPT,
        reference: A7_BASE_TO_MEASURE_SCRIPT_REF,
        script: true,
    },
    Layer2NestedCase {
        // §7.6, §8.1: planned as `orders RIGHT JOIN customers`. The guest order
        // matches no customer and drops; each aggregate skips the NULL-extended
        // customers with a FILTER on `orders.customer_id`.
        name: "left_join_from_the_one_side",
        sql: "SELECT c.region, AGG(o.order_count) AS n, AGG(o.gross_revenue) AS r \
              FROM customers c LEFT JOIN orders o ON o.customer_id = c.id GROUP BY c.region",
        reference: "SELECT c.region, COUNT(o.order_id) AS n, SUM(o.amount_cents / 100.0) AS r \
                    FROM {customers} c LEFT JOIN orders o ON o.customer_id = c.customer_id \
                    GROUP BY c.region",
        script: false,
    },
    Layer2NestedCase {
        name: "right_join_to_the_one_side",
        sql: "SELECT p.category, AGG(o.order_count) AS n \
              FROM orders o RIGHT JOIN products p ON o.product_id = p.id GROUP BY p.category",
        reference: "SELECT p.category, COUNT(o.order_id) AS n \
                    FROM products p LEFT JOIN orders o ON o.product_id = p.product_id \
                    GROUP BY p.category",
        script: false,
    },
];

// ---------------------------------------------------------------- engines

/// Run SQL through the engine's own CLI inside its container. Every client prints
/// NULL distinctly from '' (psql `-P null=NULL`, ClickHouse `\\N`, mysql `NULL`, Trino JSON `null`),
/// and prints strings unescaped (ClickHouse `TSVRaw`, mysql `-r`; the mysql client also
/// serves StarRocks).
pub fn run(engine: Engine, sql: &str) -> Result<Vec<Vec<String>>, String> {
    run_in(engine, sql, ("memory", "default"))
}

/// [`run`], with Trino's catalog and schema for unqualified names.
pub fn run_in(engine: Engine, sql: &str, trino: (&str, &str)) -> Result<Vec<Vec<String>>, String> {
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
