//! Differential fuzzing: random requests against the shop model.
//!
//! Each is either refused or answered, decided here from the shop's relationships
//! alone, as wide table §5.3 says: a metric reaches a dataset only if every home
//! dataset does. A refused request must raise that row's code. An answered one is
//! compared with an independent reference, written straight from the spec's reference
//! evaluation strategy (relational semantics §9), which shares nothing with ossiec's
//! planner: it evaluates every measure with a correlated subquery over rows
//! de-duplicated to that measure's row identity, each reading its home dataset's
//! own rows, and the groups are the union of the branches' (wide table §5.4, §5.5).
//!
//! ```console
//! cargo test --test fuzz                                  # DuckDB, 100 requests
//! OSSIEC_FUZZ=1000 cargo test --test fuzz                  # more
//! scripts/engines.sh up && cargo test --test fuzz -- --ignored   # 40 requests on each engine
//! ```

mod common;

use common::{duckdb, duckdb_rows, rows_close, run, seed_shop, Engine, MODEL};

/// Tiny deterministic PRNG (xorshift64*), so failures reproduce from the seed.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
    fn pick<'a, T>(&mut self, xs: &'a [T]) -> &'a T {
        &xs[self.below(xs.len())]
    }
}

/// Columns of each home dataset's rows. A branch only reads what its home
/// reaches, so the columns it can't reach are NULL.
const JOINED_CUSTOMERS: &str = "SELECT CAST(NULL AS INTEGER) AS order_id, c.customer_id AS cid, CAST(NULL AS INTEGER) AS pid,
        CAST(NULL AS DOUBLE) AS revenue, CAST(NULL AS INTEGER) AS amount_cents, CAST(NULL AS VARCHAR) AS status,
        CAST(NULL AS DATE) AS order_date, CAST(NULL AS INTEGER) AS stock, CAST(NULL AS VARCHAR) AS category,
        c.region, c.segment, c.name
    FROM (SELECT * FROM raw_customers WHERE NOT deleted) c";
const JOINED_PRODUCTS: &str = "SELECT CAST(NULL AS INTEGER) AS order_id, CAST(NULL AS INTEGER) AS cid, p.product_id AS pid,
        CAST(NULL AS DOUBLE) AS revenue, CAST(NULL AS INTEGER) AS amount_cents, CAST(NULL AS VARCHAR) AS status,
        CAST(NULL AS DATE) AS order_date, p.stock, p.category,
        CAST(NULL AS VARCHAR) AS region, CAST(NULL AS VARCHAR) AS segment, CAST(NULL AS VARCHAR) AS name
    FROM products p";
const JOINED: &str = "SELECT o.order_id, c.customer_id AS cid, p.product_id AS pid,
        o.amount_cents / 100.0 AS revenue, o.amount_cents, o.status, o.order_date,
        p.stock, p.category, c.region, c.segment, c.name
    FROM orders o
    LEFT JOIN (SELECT * FROM raw_customers WHERE NOT deleted) c ON o.customer_id = c.customer_id
    LEFT JOIN products p ON o.product_id = p.product_id";

/// A measure over rows de-duplicated to `identity` (spec §9): one row per
/// source row, carrying the columns the aggregate reads.
fn term(rel: &str, identity: &str, cols: &[&str], agg: &str, dims: usize) -> String {
    let mut keep = vec![identity.to_string()];
    keep.extend(cols.iter().map(|c| c.to_string()));
    let matches: Vec<String> = (0..dims)
        .map(|i| format!("j.d{i} IS NOT DISTINCT FROM g.d{i}"))
        .chain([format!("j.{identity} IS NOT NULL")])
        .collect();
    format!(
        "(SELECT {agg} FROM (SELECT DISTINCT {} FROM {rel} j WHERE {}) AS x)",
        keep.iter()
            .map(|c| format!("j.{c}"))
            .collect::<Vec<_>>()
            .join(", "),
        matches.join(" AND ")
    )
}

/// The home dataset of each aggregate in a metric (wide table §5.2).
fn homes(metric: &str) -> &'static [&'static str] {
    match metric {
        "buying_customers" | "customer_count" => &["customers"],
        "total_stock" | "products_sold" | "avg_stock" => &["products"],
        "revenue_per_customer" | "orders_per_customer" => &["orders", "customers"],
        "revenue_per_stock_unit" => &["orders", "products"],
        _ => &["orders"],
    }
}

/// Reference SQL for one metric, given the number of dimensions. `rel` names the
/// relation a home dataset's aggregates read.
fn metric_ref(name: &str, d: usize, rel: &dyn Fn(&str) -> String) -> String {
    let (o, c, p) = (rel("orders"), rel("customers"), rel("products"));
    let rev = || term(&o, "order_id", &["revenue"], "SUM(revenue)", d);
    match name {
        "total_revenue" => rev(),
        "orders.order_count" => term(&o, "order_id", &[], "COUNT(*)", d),
        "completed_revenue" => term(
            &o,
            "order_id",
            &["revenue", "status"],
            "SUM(CASE WHEN status = 'completed' THEN revenue END)",
            d,
        ),
        "avg_order_value" => term(&o, "order_id", &["revenue"], "AVG(revenue)", d),
        "avg_amount_cents" => term(&o, "order_id", &["amount_cents"], "AVG(amount_cents)", d),
        "buying_customers" | "customer_count" => term(&c, "cid", &[], "COUNT(*)", d),
        "revenue_per_customer" => {
            format!(
                "{} / NULLIF({}, 0)",
                rev(),
                term(&c, "cid", &[], "COUNT(*)", d)
            )
        }
        "orders_per_customer" => format!(
            "{} / NULLIF({}, 0)",
            term(&o, "order_id", &[], "COUNT(*)", d),
            term(&c, "cid", &[], "COUNT(*)", d)
        ),
        "total_stock" => term(&p, "pid", &["stock"], "SUM(stock)", d),
        "products_sold" => term(&p, "pid", &[], "COUNT(*)", d),
        "avg_stock" => term(&p, "pid", &["stock"], "AVG(stock)", d),
        "revenue_per_stock_unit" => format!(
            "{} / NULLIF({}, 0)",
            rev(),
            term(&p, "pid", &["stock"], "SUM(stock)", d)
        ),
        other => panic!("no reference for {other}"),
    }
}

const ORDERS_METRICS: &[&str] = &[
    "total_revenue",
    "orders.order_count",
    "completed_revenue",
    "avg_order_value",
    "avg_amount_cents",
];
const OTHER_METRICS: &[&str] = &[
    "buying_customers",
    "customer_count",
    "revenue_per_customer",
    "orders_per_customer",
    "total_stock",
    "products_sold",
    "avg_stock",
    "revenue_per_stock_unit",
];
/// (request field, reference column expression over the joined relation, is a date)
const DIMS: &[(&str, &str, bool)] = &[
    ("customers.region", "region", false),
    ("customers.segment", "segment", false),
    ("customers.name", "name", false),
    ("products.category", "category", false),
    ("orders.status", "status", false),
    ("orders.order_date", "order_date", true),
];
/// (structured filter, the same as a `filter` expression, reference predicate)
const FILTERS: &[(&str, &str, &str)] = &[
    (
        r#"{"field": "customers.segment", "op": "=", "value": "smb"}"#,
        "customers.segment = 'smb'",
        "segment = 'smb'",
    ),
    (
        r#"{"field": "products.category", "op": "IN", "values": ["books"]}"#,
        "products.category IN ('books')",
        "category IN ('books')",
    ),
    (
        r#"{"field": "orders.status", "op": "!=", "value": "returned"}"#,
        "orders.status <> 'returned'",
        "status <> 'returned'",
    ),
    (
        r#"{"field": "orders.order_date", "op": ">=", "value": "2026-02-01"}"#,
        "orders.order_date >= DATE '2026-02-01'",
        "order_date >= DATE '2026-02-01'",
    ),
    (
        r#"{"field": "customers.region", "op": "IS NULL"}"#,
        "customers.region IS NULL",
        "region IS NULL",
    ),
    (
        r#"{"field": "customers.name", "op": "LIKE", "value": "%a%"}"#,
        "customers.name LIKE '%a%'",
        "name LIKE '%a%'",
    ),
];

struct Generated {
    request: String,
    /// The code §8 requires, or the reference for the rows.
    expected: Result<String, ossiec::ErrorCode>,
}

/// Orders reach every dataset here; customers and products only themselves.
fn metric_reaches(metric: &str, dataset: &str) -> bool {
    homes(metric)
        .iter()
        .all(|h| *h == "orders" || *h == dataset)
}

/// §8 rows 13, 14 and 16, in order.
fn rejection(metrics: &[&str], dims: &[&str], filter: Option<&str>) -> Option<ossiec::ErrorCode> {
    let reached = |d: &&str| metrics.iter().filter(|m| metric_reaches(m, d)).count();
    if dims.iter().any(|d| reached(d) == 0) {
        Some(ossiec::ErrorCode::NoPath)
    } else if dims.iter().any(|d| reached(d) < metrics.len()) {
        Some(ossiec::ErrorCode::PartialDimensionReachability)
    } else if filter.is_some_and(|f| metrics.iter().any(|m| !metric_reaches(m, f))) {
        Some(ossiec::ErrorCode::UnsupportedFilterPath)
    } else {
        None
    }
}

fn generate(rng: &mut Rng) -> Generated {
    // A third of requests use only metrics with a home other than orders, the
    // shape where groups are most at risk of depending on the measures.
    let one_side_only = rng.below(3) == 0;
    let first = if one_side_only {
        rng.pick(OTHER_METRICS)
    } else {
        rng.pick(ORDERS_METRICS)
    };
    let mut metrics = vec![*first];
    for _ in 0..rng.below(3) {
        let m = if !one_side_only && rng.below(3) == 0 {
            *rng.pick(ORDERS_METRICS)
        } else {
            *rng.pick(OTHER_METRICS)
        };
        if !metrics.contains(&m) {
            metrics.push(m);
        }
    }
    let mut dims: Vec<&(&str, &str, bool)> = vec![];
    for _ in 0..rng.below(3) {
        let d = rng.pick(DIMS);
        if !dims.iter().any(|x| x.0 == d.0) {
            dims.push(d);
        }
    }
    let filter = (rng.below(2) == 0).then(|| rng.pick(FILTERS));
    let as_expression = rng.below(2) == 0;
    let grain = dims.iter().any(|d| d.2) && rng.below(2) == 0;

    let quote = |xs: Vec<&str>| {
        xs.iter()
            .map(|x| format!("\"{x}\""))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let mut request = format!(
        r#"{{"metrics": [{}], "dimensions": [{}]"#,
        quote(metrics.clone()),
        quote(dims.iter().map(|d| d.0).collect())
    );
    if let Some((json, expression, _)) = filter {
        if as_expression {
            request.push_str(&format!(r#", "filter": "{expression}""#));
        } else {
            request.push_str(&format!(r#", "filters": [{json}]"#));
        }
    }
    if grain {
        request.push_str(r#", "timeGrain": "month""#);
    }
    request.push('}');

    let dim_cols: Vec<String> = dims
        .iter()
        .enumerate()
        .map(|(i, d)| {
            let col = if d.2 && grain {
                format!("CAST(DATE_TRUNC('month', {}) AS DATE)", d.1)
            } else {
                d.1.to_string()
            };
            format!("{col} AS d{i}")
        })
        .collect();
    let where_ = filter
        .map(|(.., p)| format!(" WHERE {p}"))
        .unwrap_or_default();
    let joined = |source: &str| {
        format!(
            "SELECT *{} FROM ({source}) AS base{where_}",
            dim_cols
                .iter()
                .map(|c| format!(", {c}"))
                .collect::<String>()
        )
    };
    let dataset = |field: &'static str| field.split('.').next().expect("dataset");
    let filter_dataset =
        filter.map(|(json, ..)| dataset(json.split("\"field\": \"").nth(1).expect("filter field")));
    let dim_datasets: Vec<&str> = dims.iter().map(|d| dataset(d.0)).collect();
    if let Some(code) = rejection(&metrics, &dim_datasets, filter_dataset) {
        return Generated {
            request,
            expected: Err(code),
        };
    }
    let mut used: Vec<&str> = metrics.iter().flat_map(|m| homes(m)).copied().collect();
    used.sort();
    used.dedup();
    let rel = |home: &str| format!("joined_{home}");
    let n = dims.len();
    let groups = if n == 0 {
        "SELECT 1 AS one".to_string()
    } else {
        let cols = (0..n)
            .map(|i| format!("d{i}"))
            .collect::<Vec<_>>()
            .join(", ");
        used.iter()
            .map(|h| format!("SELECT DISTINCT {cols} FROM joined_{h}"))
            .collect::<Vec<_>>()
            .join(" UNION ")
    };
    let select: Vec<String> = (0..n)
        .map(|i| format!("g.d{i}"))
        .chain(metrics.iter().map(|m| metric_ref(m, n, &rel)))
        .collect();
    let reference = format!(
        "WITH joined_orders AS ({}), joined_customers AS ({}), joined_products AS ({}), groups AS ({groups}) SELECT {} FROM groups g",
        joined(JOINED),
        joined(JOINED_CUSTOMERS),
        joined(JOINED_PRODUCTS),
        select.join(", ")
    );
    Generated {
        request,
        expected: Ok(reference),
    }
}

fn fuzz(engine: Engine, count: usize) {
    let graph = ossiec::from_yaml(MODEL).unwrap();
    let db = duckdb();
    if engine != Engine::DuckDb {
        seed_shop(engine);
    }
    let mut rng = Rng(0x0551_EC00_F00D_5EED);
    let mut failures = vec![];
    let mut answered = 0;
    for i in 0..count {
        let g = generate(&mut rng);
        let req: ossiec::Request = serde_json::from_str(&g.request).unwrap();
        let opts = ossiec::Options {
            target: Some(engine.dialect()),
            ..Default::default()
        };
        let compiled = ossiec::compile(&graph, &req, &opts);
        let reference = match g.expected {
            Err(code) => {
                let got = compiled.map(|_| ()).map_err(|e| e.code());
                if got != Err(Some(code)) {
                    failures.push(format!("#{i} {}: {got:?}, expected {code}", g.request));
                }
                continue;
            }
            Ok(reference) => reference,
        };
        let compiled = match compiled {
            Ok(c) => c,
            Err(e) => {
                failures.push(format!("#{i} {}: compile: {e}", g.request));
                continue;
            }
        };
        answered += 1;
        let got = match engine {
            Engine::DuckDb => Ok(duckdb_rows(&db, &compiled.sql)),
            _ => run(engine, &compiled.sql),
        };
        let mut want = duckdb_rows(&db, &reference);
        match got {
            Ok(mut got) => {
                got.sort();
                want.sort();
                if !rows_close(&got, &want) {
                    failures.push(format!(
                        "#{i} {}: wrong rows\n  got:  {got:?}\n  want: {want:?}\n{}",
                        g.request, compiled.sql
                    ));
                }
            }
            Err(e) => failures.push(format!("#{i} {}: {e}", g.request)),
        }
    }
    assert!(
        answered * 3 >= count,
        "{engine:?}: only {answered} of {count} random requests were answered, too few to prove much"
    );
    assert!(
        failures.is_empty(),
        "{engine:?}: {} of {count} random requests failed\n\n{}",
        failures.len(),
        failures.join("\n\n")
    );
}

/// `OSSIEC_FUZZ=<n>` sets how many requests each test generates.
fn count(default: usize) -> usize {
    std::env::var("OSSIEC_FUZZ")
        .ok()
        .and_then(|n| n.parse().ok())
        .unwrap_or(default)
}

#[test]
fn duckdb_random_requests_match_the_spec_reference() {
    fuzz(Engine::DuckDb, count(100));
}

#[test]
#[ignore = "needs Docker: scripts/engines.sh up"]
fn postgres() {
    fuzz(Engine::Postgres, count(40));
}

#[test]
#[ignore = "needs Docker: scripts/engines.sh up"]
fn trino() {
    fuzz(Engine::Trino, count(40));
}

#[test]
#[ignore = "needs Docker: scripts/engines.sh up"]
fn athena() {
    fuzz(Engine::Athena, count(40));
}

#[test]
#[ignore = "needs Docker: scripts/engines.sh up"]
fn spark() {
    fuzz(Engine::Spark, count(40));
}

#[test]
#[ignore = "needs Docker: scripts/engines.sh up"]
fn starrocks() {
    fuzz(Engine::StarRocks, count(40));
}

#[test]
#[ignore = "needs Docker: scripts/engines.sh up"]
fn clickhouse() {
    fuzz(Engine::ClickHouse, count(40));
}

#[test]
#[ignore = "needs Docker: scripts/engines.sh up"]
fn mysql() {
    fuzz(Engine::MySql, count(40));
}
