//! The Layer 3 request cases, each with its reference SQL.

use super::*;

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
