//! The shop model and its seed data for every engine.

use super::*;

pub const MODEL: &str = include_str!("../fixtures/shop/model.yaml");

/// Reference SQL is written for DuckDB; `{customers}` is the customers dataset's query source.
pub(crate) const CUSTOMERS: &str = "(SELECT * FROM raw_customers WHERE NOT deleted)";

#[derive(Clone, Copy)]
pub(crate) enum Ty {
    Int,
    Text,
    Date,
    Bool,
}

pub(crate) struct Table {
    name: &'static str,
    cols: &'static [(&'static str, Ty)],
    /// SQL literals; dates are bare `YYYY-MM-DD` and get each engine's date literal syntax.
    rows: &'static [&'static [&'static str]],
}

/// A tiny shop, shaped to break naive SQL: fan-out joins, NULL keys, a product
/// that never sold, a soft-deleted customer and a guest checkout.
pub(crate) const TABLES: &[Table] = &[
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
    let engine = engine.host();
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
