//! TPC-DS tier, on real generated data.
//!
//! - **Ossie's own TPC-DS example model, unchanged** (`fixtures/ossie/`): its
//!   metrics (including the de-duplicated and window ones) against hand-written
//!   reference SQL.
//! - **Official TPC-DS queries** (`fixtures/tpcds/sweep_model.yaml`): requests that
//!   express q3, q7, q12, q26, q42, q43, q48, q52, q55, q96 and q98, compared
//!   with the benchmark's own SQL from DuckDB's `tpcds_queries()`.
//!
//! Ground truth always runs on the *same* engine as ossiec's query (reference SQL
//! is written for DuckDB and transpiled), so differences between engines' data
//! generators cancel out. Reference SQL sticks to constructs every engine runs
//! (derived-table aliases, no `IS NOT DISTINCT FROM`, no `LAG`), so it never
//! depends on the dialect rules it is checking. DuckDB generates sf=0.1 with `dsdgen` (needs network
//! once to install the `tpcds` extension). Trino copies its built-in `tpcds.tiny`
//! into its memory catalog once per run (its connector regenerates data on every
//! query otherwise). Engine runs are ignored by default:
//!
//! ```console
//! cargo test --test tpcds                     # DuckDB
//! scripts/engines.sh up && cargo test --test tpcds -- --ignored   # Trino
//! ```

mod common;

use common::{duckdb_rows, rows_close, run_in, Engine};
use duckdb::Connection;
use polyglot_sql::{transpile, DialectType};

const OSSIE: &str = include_str!("fixtures/ossie/tpcds_semantic_model.yaml");
const SWEEP: &str = include_str!("fixtures/tpcds/sweep_model.yaml");

enum Reference {
    /// DuckDB SQL computing the answer independently of ossiec.
    Sql(&'static str),
    /// An official TPC-DS query number, text from DuckDB's `tpcds_queries()`.
    Official(i32),
}

struct Case {
    name: &'static str,
    model: &'static str,
    request: &'static str,
    reference: Reference,
}

const CASES: &[Case] = &[
    // ---- Apache Ossie's TPC-DS example model, unchanged ----
    Case {
        name: "ossie_sales_and_profit_by_category",
        model: OSSIE,
        request: r#"{"metrics": ["total_sales", "total_profit"], "dimensions": ["item.i_category"]}"#,
        reference: Reference::Sql(
            "SELECT i.i_category, SUM(ss.ss_ext_sales_price), SUM(ss.ss_net_profit)
             FROM store_sales ss LEFT JOIN item i ON ss.ss_item_sk = i.i_item_sk
             GROUP BY i.i_category",
        ),
    },
    Case {
        name: "ossie_customer_lifetime_value_by_year",
        model: OSSIE,
        request: r#"{"metrics": ["customer_lifetime_value"], "dimensions": ["date_dim.d_year"],
            "filters": [{"field": "date_dim.d_year", "op": "IN", "values": [1999, 2000, 2001]}]}"#,
        reference: Reference::Sql(
            "SELECT d.d_year, SUM(ss.ss_ext_sales_price) / COUNT(DISTINCT c.c_customer_sk)
             FROM store_sales ss
             LEFT JOIN date_dim d ON ss.ss_sold_date_sk = d.d_date_sk
             LEFT JOIN customer c ON ss.ss_customer_sk = c.c_customer_sk
             WHERE d.d_year IN (1999, 2000, 2001)
             GROUP BY d.d_year",
        ),
    },
    Case {
        // Employees live on store: counted once per (category, store), not once per sale.
        name: "ossie_store_productivity_by_category",
        model: OSSIE,
        request: r#"{"metrics": ["store_productivity", "total_sales"], "dimensions": ["item.i_category"]}"#,
        reference: Reference::Sql(
            "WITH sales AS (
               SELECT i.i_category AS cat, SUM(ss.ss_ext_sales_price) AS s
               FROM store_sales ss LEFT JOIN item i ON ss.ss_item_sk = i.i_item_sk
               GROUP BY i.i_category
             ), staff AS (
               SELECT cat, SUM(emp) AS e FROM (
                 SELECT DISTINCT i.i_category AS cat, st.s_store_sk, st.s_number_employees AS emp
                 FROM store_sales ss
                 LEFT JOIN item i ON ss.ss_item_sk = i.i_item_sk
                 LEFT JOIN store st ON ss.ss_store_sk = st.s_store_sk
                 WHERE st.s_store_sk IS NOT NULL
               ) AS per_store GROUP BY cat
             )
             SELECT sales.cat, sales.s / NULLIF(staff.e, 0), sales.s
             FROM sales LEFT JOIN staff ON COALESCE(sales.cat, '~') = COALESCE(staff.cat, '~')",
        ),
    },
    Case {
        name: "ossie_cumulative_sales_november_2000",
        model: OSSIE,
        request: r#"{"metrics": ["total_sales", "cumulative_sales"], "dimensions": ["date_dim.d_date"],
            "filters": [{"field": "date_dim.d_year", "op": "=", "value": 2000}, {"field": "date_dim.d_moy", "op": "=", "value": 11}],
            "orderBy": [{"field": "date_dim.d_date"}]}"#,
        reference: Reference::Sql(
            "SELECT d_date, s, SUM(s) OVER (ORDER BY d_date ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW)
             FROM (SELECT d.d_date, SUM(ss.ss_ext_sales_price) AS s
                   FROM store_sales ss LEFT JOIN date_dim d ON ss.ss_sold_date_sk = d.d_date_sk
                   WHERE d.d_year = 2000 AND d.d_moy = 11
                   GROUP BY d.d_date) AS daily
             ORDER BY d_date",
        ),
    },
    Case {
        name: "ossie_top3_brands_per_store",
        model: OSSIE,
        request: r#"{"metrics": ["brand_rank_in_store", "total_sales"], "dimensions": ["store.s_store_sk", "item.i_brand"],
            "metricFilters": [{"metric": "brand_rank_in_store", "op": "<=", "value": 3}]}"#,
        reference: Reference::Sql(
            "SELECT * FROM (
               SELECT st.s_store_sk, i.i_brand,
                      RANK() OVER (PARTITION BY st.s_store_sk ORDER BY SUM(ss.ss_ext_sales_price) DESC) AS rnk,
                      SUM(ss.ss_ext_sales_price)
               FROM store_sales ss
               LEFT JOIN item i ON ss.ss_item_sk = i.i_item_sk
               LEFT JOIN store st ON ss.ss_store_sk = st.s_store_sk
               GROUP BY st.s_store_sk, i.i_brand
             ) AS ranked WHERE rnk <= 3",
        ),
    },
    Case {
        name: "ossie_monthly_sales_change",
        model: OSSIE,
        request: r#"{"metrics": ["monthly_sales_change"], "dimensions": ["date_dim.d_year", "date_dim.d_moy"],
            "filters": [{"field": "date_dim.d_year", "op": "IN", "values": [1999, 2000]}],
            "orderBy": [{"field": "date_dim.d_year"}, {"field": "date_dim.d_moy"}]}"#,
        reference: Reference::Sql(
            // No window functions: the previous month is a self-join, so the
            // reference runs unchanged on every engine (ClickHouse has no LAG).
            "WITH monthly AS (
               SELECT d.d_year AS y, d.d_moy AS m, SUM(ss.ss_ext_sales_price) AS s
               FROM store_sales ss LEFT JOIN date_dim d ON ss.ss_sold_date_sk = d.d_date_sk
               WHERE d.d_year IN (1999, 2000)
               GROUP BY d.d_year, d.d_moy
             )
             SELECT cur.y, cur.m, cur.s - prev.s
             FROM monthly cur LEFT JOIN monthly prev ON prev.y * 12 + prev.m = cur.y * 12 + cur.m - 1
             ORDER BY cur.y, cur.m",
        ),
    },
    Case {
        name: "ossie_dataset_metric_by_state",
        model: OSSIE,
        request: r#"{"metrics": ["store_sales.total_quantity"], "dimensions": ["store.s_state"]}"#,
        reference: Reference::Sql(
            "SELECT st.s_state, SUM(ss.ss_quantity)
             FROM store_sales ss LEFT JOIN store st ON ss.ss_store_sk = st.s_store_sk
             GROUP BY st.s_state",
        ),
    },
    Case {
        name: "ossie_dimension_values",
        model: OSSIE,
        request: r#"{"dimensions": ["item.i_category"]}"#,
        reference: Reference::Sql("SELECT DISTINCT i_category FROM item"),
    },
    Case {
        name: "ossie_dimension_combinations",
        model: OSSIE,
        request: r#"{"dimensions": ["store.s_state", "item.i_category"]}"#,
        reference: Reference::Sql(
            "SELECT DISTINCT st.s_state, i.i_category
             FROM store_sales ss
             LEFT JOIN item i ON ss.ss_item_sk = i.i_item_sk
             LEFT JOIN store st ON ss.ss_store_sk = st.s_store_sk",
        ),
    },
    // ---- Multi-hop joins and composite keys ----
    Case {
        name: "two_hop_dimension",
        model: SWEEP,
        request: r#"{"metrics": ["total_sales"], "dimensions": ["customer_address.ca_state"],
            "filters": [{"field": "item.i_category", "op": "=", "value": "Books"}]}"#,
        reference: Reference::Sql(
            "SELECT ca.ca_state, SUM(ss.ss_ext_sales_price)
             FROM store_sales ss
             LEFT JOIN item i ON ss.ss_item_sk = i.i_item_sk
             LEFT JOIN customer c ON ss.ss_customer_sk = c.c_customer_sk
             LEFT JOIN customer_address ca ON c.c_current_addr_sk = ca.ca_address_sk
             WHERE i.i_category = 'Books'
             GROUP BY ca.ca_state",
        ),
    },
    Case {
        // A measure two hops away: each address counts once per category.
        name: "two_hop_deduplicated_measure",
        model: SWEEP,
        request: r#"{"metrics": ["address_gmt_total"], "dimensions": ["item.i_category"]}"#,
        reference: Reference::Sql(
            "SELECT g.cat, s.v
             FROM (SELECT DISTINCT i.i_category AS cat FROM store_sales ss LEFT JOIN item i ON ss.ss_item_sk = i.i_item_sk) AS g
             LEFT JOIN (
               SELECT cat, SUM(gmt) AS v FROM (
                 SELECT DISTINCT i.i_category AS cat, ca.ca_address_sk, ca.ca_gmt_offset AS gmt
                 FROM store_sales ss
                 LEFT JOIN item i ON ss.ss_item_sk = i.i_item_sk
                 LEFT JOIN customer c ON ss.ss_customer_sk = c.c_customer_sk
                 LEFT JOIN customer_address ca ON c.c_current_addr_sk = ca.ca_address_sk
                 WHERE ca.ca_address_sk IS NOT NULL
               ) AS per_address GROUP BY cat
             ) AS s ON COALESCE(g.cat, '~') = COALESCE(s.cat, '~')",
        ),
    },
    Case {
        // Returns join sales on (item, ticket), then sales join item: two hops,
        // the first on a composite key.
        name: "composite_key_relationship",
        model: SWEEP,
        request: r#"{"metrics": ["returned_amount"], "dimensions": ["item.i_category"]}"#,
        reference: Reference::Sql(
            "SELECT i.i_category, SUM(sr.sr_return_amt)
             FROM store_returns sr
             LEFT JOIN store_sales ss ON sr.sr_item_sk = ss.ss_item_sk AND sr.sr_ticket_number = ss.ss_ticket_number
             LEFT JOIN item i ON ss.ss_item_sk = i.i_item_sk
             GROUP BY i.i_category",
        ),
    },
    Case {
        // Rooted at store_returns, sales are a one-side measure: de-duplicated on
        // store_sales' composite primary key (item, ticket).
        name: "composite_primary_key_dedup",
        model: SWEEP,
        request: r#"{"metrics": ["returned_amount", "total_sales"], "dimensions": ["item.i_category"]}"#,
        reference: Reference::Sql(
            "WITH j AS (
               SELECT i.i_category AS cat, sr.sr_return_amt, ss.ss_item_sk, ss.ss_ticket_number, ss.ss_ext_sales_price
               FROM store_returns sr
               LEFT JOIN store_sales ss ON sr.sr_item_sk = ss.ss_item_sk AND sr.sr_ticket_number = ss.ss_ticket_number
               LEFT JOIN item i ON ss.ss_item_sk = i.i_item_sk
             ), r AS (SELECT cat, SUM(sr_return_amt) AS ret FROM j GROUP BY cat),
             s AS (
               SELECT cat, SUM(ss_ext_sales_price) AS sales FROM (
                 SELECT DISTINCT cat, ss_item_sk, ss_ticket_number, ss_ext_sales_price FROM j WHERE ss_item_sk IS NOT NULL
               ) AS per_sale GROUP BY cat
             )
             SELECT r.cat, r.ret, s.sales FROM r LEFT JOIN s ON COALESCE(r.cat, '~') = COALESCE(s.cat, '~')",
        ),
    },
    // ---- Role-playing dimensions ----
    Case {
        // `customer_first_sales.d_year`: date_dim reached through the customer's
        // first-sale relationship, not the sale's own date.
        name: "role_dimension",
        model: SWEEP,
        request: r#"{"metrics": ["total_sales"], "dimensions": ["customer_first_sales.d_year"],
            "filters": [{"field": "item.i_category", "op": "=", "value": "Books"}]}"#,
        reference: Reference::Sql(
            "SELECT fs.d_year, SUM(ss.ss_ext_sales_price)
             FROM store_sales ss
             LEFT JOIN item i ON ss.ss_item_sk = i.i_item_sk
             LEFT JOIN customer c ON ss.ss_customer_sk = c.c_customer_sk
             LEFT JOIN date_dim fs ON c.c_first_sales_date_sk = fs.d_date_sk
             WHERE i.i_category = 'Books'
             GROUP BY fs.d_year",
        ),
    },
    Case {
        // Three date_dim roles in one request: the sale date (shortest path, by
        // default), the first-sale date and the first-ship-to date.
        name: "three_date_roles_at_once",
        model: SWEEP,
        request: r#"{"metrics": ["total_sales"], "dimensions": ["customer_first_sales.d_year", "customer_first_shipto.d_year"],
            "filters": [{"field": "date_dim.d_year", "op": "=", "value": 2000}, {"field": "customer_first_sales.d_year", "op": ">=", "value": 2000}]}"#,
        reference: Reference::Sql(
            "SELECT fs.d_year AS first_sales, fsh.d_year AS first_shipto, SUM(ss.ss_ext_sales_price)
             FROM store_sales ss
             LEFT JOIN date_dim d ON ss.ss_sold_date_sk = d.d_date_sk
             LEFT JOIN customer c ON ss.ss_customer_sk = c.c_customer_sk
             LEFT JOIN date_dim fs ON c.c_first_sales_date_sk = fs.d_date_sk
             LEFT JOIN date_dim fsh ON c.c_first_shipto_date_sk = fsh.d_date_sk
             WHERE d.d_year = 2000 AND fs.d_year >= 2000
             GROUP BY fs.d_year, fsh.d_year",
        ),
    },
    // ---- Independent facts ----
    Case {
        // Store and web sales share item and date_dim but not each other: one
        // leg per fact. Categories sold in either channel appear.
        name: "store_and_web_by_category",
        model: SWEEP,
        request: r#"{"metrics": ["total_sales", "web_sales_total"], "dimensions": ["item.i_category"],
            "filters": [{"field": "date_dim.d_year", "op": "=", "value": 2000}]}"#,
        reference: Reference::Sql(
            "WITH s AS (
               SELECT i.i_category AS cat, SUM(ss.ss_ext_sales_price) AS v
               FROM store_sales ss
               LEFT JOIN date_dim d ON ss.ss_sold_date_sk = d.d_date_sk
               LEFT JOIN item i ON ss.ss_item_sk = i.i_item_sk
               WHERE d.d_year = 2000 GROUP BY i.i_category
             ), w AS (
               SELECT i.i_category AS cat, SUM(ws.ws_ext_sales_price) AS v
               FROM web_sales ws
               LEFT JOIN date_dim d ON ws.ws_sold_date_sk = d.d_date_sk
               LEFT JOIN item i ON ws.ws_item_sk = i.i_item_sk
               WHERE d.d_year = 2000 GROUP BY i.i_category
             ), g AS (SELECT cat FROM s UNION SELECT cat FROM w)
             SELECT g.cat, s.v AS store_v, w.v AS web_v FROM g
             LEFT JOIN s ON COALESCE(g.cat, '~') = COALESCE(s.cat, '~')
             LEFT JOIN w ON COALESCE(g.cat, '~') = COALESCE(w.cat, '~')",
        ),
    },
    Case {
        // One metric over two facts: each term in its own leg, combined per group.
        name: "ratio_across_independent_facts",
        model: SWEEP,
        request: r#"{"metrics": ["web_to_store_ratio"], "dimensions": ["item.i_category"]}"#,
        reference: Reference::Sql(
            "WITH s AS (
               SELECT i.i_category AS cat, SUM(ss.ss_ext_sales_price) AS v
               FROM store_sales ss LEFT JOIN item i ON ss.ss_item_sk = i.i_item_sk GROUP BY i.i_category
             ), w AS (
               SELECT i.i_category AS cat, SUM(ws.ws_ext_sales_price) AS v
               FROM web_sales ws LEFT JOIN item i ON ws.ws_item_sk = i.i_item_sk GROUP BY i.i_category
             ), g AS (SELECT cat FROM s UNION SELECT cat FROM w)
             SELECT g.cat, CAST(w.v AS DOUBLE) / NULLIF(s.v, 0) FROM g
             LEFT JOIN s ON COALESCE(g.cat, '~') = COALESCE(s.cat, '~')
             LEFT JOIN w ON COALESCE(g.cat, '~') = COALESCE(w.cat, '~')",
        ),
    },
    // ---- Parents without children (includeUnmatched) ----
    Case {
        // Items no store sale reaches join the NULL-year group with their price.
        name: "unmatched_items_included",
        model: SWEEP,
        // With only an item measure, which fact links items to years would be
        // ambiguous (store, web, returns); the store count names store_sales.
        request: r#"{"metrics": ["item_price_total", "store_sales.sales_count"], "dimensions": ["date_dim.d_year"], "includeUnmatched": true}"#,
        reference: Reference::Sql(
            "WITH matched AS (
               SELECT DISTINCT d.d_year AS y, i.i_item_sk, i.i_current_price AS p
               FROM store_sales ss
               LEFT JOIN date_dim d ON ss.ss_sold_date_sk = d.d_date_sk
               LEFT JOIN item i ON ss.ss_item_sk = i.i_item_sk
               WHERE i.i_item_sk IS NOT NULL
             ), unmatched AS (
               SELECT DISTINCT CAST(NULL AS INTEGER) AS y, i.i_item_sk, i.i_current_price AS p
               FROM item i WHERE NOT EXISTS (SELECT 1 FROM store_sales ss WHERE ss.ss_item_sk = i.i_item_sk)
             ), r AS (SELECT * FROM matched UNION ALL SELECT * FROM unmatched),
             g AS (
               SELECT DISTINCT d.d_year AS y FROM store_sales ss LEFT JOIN date_dim d ON ss.ss_sold_date_sk = d.d_date_sk
               UNION SELECT y FROM unmatched
             )
             SELECT g.y, s.v, COALESCE(n.c, 0) FROM g
             LEFT JOIN (SELECT y, SUM(p) AS v FROM r GROUP BY y) AS s ON COALESCE(g.y, -1) = COALESCE(s.y, -1)
             LEFT JOIN (
               SELECT d.d_year AS y, COUNT(*) AS c FROM store_sales ss LEFT JOIN date_dim d ON ss.ss_sold_date_sk = d.d_date_sk GROUP BY d.d_year
             ) AS n ON COALESCE(g.y, -1) = COALESCE(n.y, -1)",
        ),
    },
    // ---- Official TPC-DS queries ----
    Case {
        name: "q3",
        model: SWEEP,
        request: r#"{"metrics": ["total_sales"], "dimensions": ["date_dim.d_year", "item.i_brand_id", "item.i_brand"],
            "filters": [{"field": "item.i_manufact_id", "op": "=", "value": 128}, {"field": "date_dim.d_moy", "op": "=", "value": 11}],
            "orderBy": [{"field": "date_dim.d_year"}, {"field": "total_sales", "direction": "desc"}, {"field": "item.i_brand_id"}],
            "limit": 100}"#,
        reference: Reference::Official(3),
    },
    Case {
        name: "q7",
        model: SWEEP,
        request: r#"{"metrics": ["avg_quantity", "avg_list_price", "avg_coupon_amt", "avg_sales_price"],
            "dimensions": ["item.i_item_id"],
            "filters": [
                {"field": "customer_demographics.cd_gender", "op": "=", "value": "M"},
                {"field": "customer_demographics.cd_marital_status", "op": "=", "value": "S"},
                {"field": "customer_demographics.cd_education_status", "op": "=", "value": "College"},
                {"field": "promotion.channel_email_or_event_n", "op": "=", "value": 1},
                {"field": "date_dim.d_year", "op": "=", "value": 2000}
            ],
            "orderBy": [{"field": "item.i_item_id"}],
            "limit": 100}"#,
        reference: Reference::Official(7),
    },
    Case {
        // Catalog twin of q7.
        name: "q26",
        model: SWEEP,
        request: r#"{"metrics": ["catalog_avg_quantity", "catalog_avg_list_price", "catalog_avg_coupon_amt", "catalog_avg_sales_price"],
            "dimensions": ["item.i_item_id"],
            "filters": [
                {"field": "customer_demographics.cd_gender", "op": "=", "value": "M"},
                {"field": "customer_demographics.cd_marital_status", "op": "=", "value": "S"},
                {"field": "customer_demographics.cd_education_status", "op": "=", "value": "College"},
                {"field": "promotion.channel_email_or_event_n", "op": "=", "value": 1},
                {"field": "date_dim.d_year", "op": "=", "value": 2000}
            ],
            "orderBy": [{"field": "item.i_item_id"}],
            "limit": 100}"#,
        reference: Reference::Official(26),
    },
    Case {
        // OR-of-AND filter groups live in the metric CASE; year is a request filter.
        name: "q48",
        model: SWEEP,
        request: r#"{"metrics": ["q48_quantity"],
            "filters": [{"field": "date_dim.d_year", "op": "=", "value": 2000}]}"#,
        reference: Reference::Official(48),
    },
    Case {
        name: "q42",
        model: SWEEP,
        request: r#"{"metrics": ["total_sales"], "dimensions": ["date_dim.d_year", "item.i_category_id", "item.i_category"],
            "filters": [{"field": "item.i_manager_id", "op": "=", "value": 1}, {"field": "date_dim.d_moy", "op": "=", "value": 11},
                        {"field": "date_dim.d_year", "op": "=", "value": 2000}],
            "orderBy": [{"field": "total_sales", "direction": "desc"}, {"field": "date_dim.d_year"}, {"field": "item.i_category_id"}, {"field": "item.i_category"}],
            "limit": 100}"#,
        reference: Reference::Official(42),
    },
    Case {
        name: "q52",
        model: SWEEP,
        request: r#"{"metrics": ["total_sales"], "dimensions": ["date_dim.d_year", "item.i_brand_id", "item.i_brand"],
            "filters": [{"field": "item.i_manager_id", "op": "=", "value": 1}, {"field": "date_dim.d_moy", "op": "=", "value": 11},
                        {"field": "date_dim.d_year", "op": "=", "value": 2000}],
            "orderBy": [{"field": "date_dim.d_year"}, {"field": "total_sales", "direction": "desc"}, {"field": "item.i_brand_id"}],
            "limit": 100}"#,
        reference: Reference::Official(52),
    },
    Case {
        name: "q55",
        model: SWEEP,
        request: r#"{"metrics": ["total_sales"], "dimensions": ["item.i_brand_id", "item.i_brand"],
            "filters": [{"field": "item.i_manager_id", "op": "=", "value": 28}, {"field": "date_dim.d_moy", "op": "=", "value": 11},
                        {"field": "date_dim.d_year", "op": "=", "value": 1999}],
            "orderBy": [{"field": "total_sales", "direction": "desc"}, {"field": "item.i_brand_id"}],
            "limit": 100}"#,
        reference: Reference::Official(55),
    },
    Case {
        name: "q98",
        model: SWEEP,
        request: r#"{"metrics": ["total_sales", "revenue_ratio"],
            "dimensions": ["item.i_item_id", "item.i_item_desc", "item.i_category", "item.i_class", "item.i_current_price"],
            "filters": [{"field": "item.i_category", "op": "IN", "values": ["Sports", "Books", "Home"]},
                        {"field": "date_dim.d_date", "op": "BETWEEN", "values": ["1999-02-22", "1999-03-24"]}],
            "orderBy": [{"field": "item.i_category", "nulls": "first"}, {"field": "item.i_class", "nulls": "first"},
                        {"field": "item.i_item_id", "nulls": "first"}, {"field": "item.i_item_desc", "nulls": "first"},
                        {"field": "revenue_ratio", "nulls": "first"}]}"#,
        reference: Reference::Official(98),
    },
    Case {
        name: "q12",
        model: SWEEP,
        request: r#"{"metrics": ["web_sales_total", "web_revenue_ratio"],
            "dimensions": ["item.i_item_id", "item.i_item_desc", "item.i_category", "item.i_class", "item.i_current_price"],
            "filters": [{"field": "item.i_category", "op": "IN", "values": ["Sports", "Books", "Home"]},
                        {"field": "date_dim.d_date", "op": "BETWEEN", "values": ["1999-02-22", "1999-03-24"]}],
            "orderBy": [{"field": "item.i_category", "nulls": "last"}, {"field": "item.i_class", "nulls": "last"},
                        {"field": "item.i_item_id", "nulls": "last"}, {"field": "item.i_item_desc", "nulls": "last"},
                        {"field": "web_revenue_ratio", "nulls": "last"}],
            "limit": 100}"#,
        // The official query relies on DuckDB's default NULL placement (last).
        reference: Reference::Official(12),
    },
    Case {
        name: "q43",
        model: SWEEP,
        request: r#"{"metrics": ["sun_sales", "mon_sales", "tue_sales", "wed_sales", "thu_sales", "fri_sales", "sat_sales"],
            "dimensions": ["store.s_store_name", "store.s_store_id"],
            "filters": [{"field": "store.s_gmt_offset", "op": "=", "value": -5}, {"field": "date_dim.d_year", "op": "=", "value": 2000}],
            "orderBy": [{"field": "store.s_store_name"}, {"field": "store.s_store_id"}, {"field": "sun_sales"}, {"field": "mon_sales"},
                        {"field": "tue_sales"}, {"field": "wed_sales"}, {"field": "thu_sales"}, {"field": "fri_sales"}, {"field": "sat_sales"}],
            "limit": 100}"#,
        reference: Reference::Official(43),
    },
    Case {
        name: "q96",
        model: SWEEP,
        request: r#"{"metrics": ["store_sales.sales_count"],
            "filters": [{"field": "time_dim.t_hour", "op": "=", "value": 20}, {"field": "time_dim.t_minute", "op": ">=", "value": 30},
                        {"field": "household_demographics.hd_dep_count", "op": "=", "value": 7}, {"field": "store.s_store_name", "op": "=", "value": "ese"}],
            "orderBy": [{"field": "store_sales.sales_count"}], "limit": 100}"#,
        reference: Reference::Official(96),
    },
];

/// DuckDB with TPC-DS sf=0.1 generated into `main` (q3's `i_manufact_id = 128` has no items at 0.01).
fn duckdb_tpcds() -> Connection {
    // dsdgen keeps global state: two at once in one process crash (SIGSEGV
    // when the engine tests run in parallel).
    static DSDGEN: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _one_at_a_time = DSDGEN.lock().unwrap_or_else(|e| e.into_inner());
    let c = Connection::open_in_memory().unwrap();
    c.execute_batch("INSTALL tpcds; LOAD tpcds; CALL dsdgen(sf = 0.1);")
        .expect("DuckDB's tpcds extension (installing it needs network access once)");
    c
}

fn official(c: &Connection, n: i32) -> String {
    c.query_row(
        "SELECT query FROM tpcds_queries() WHERE query_nr = ?",
        [n],
        |r| r.get(0),
    )
    .unwrap_or_else(|e| panic!("TPC-DS query {n}: {e}"))
}

/// Point the model's `tpcds.public.` sources at the engine's TPC-DS tables.
fn graph_for(model: &str, engine: Engine) -> ossiec::SemanticGraph {
    let prefix = match engine {
        Engine::Trino => "memory.tpcds.",
        _ => "",
    };
    ossiec::from_yaml(&model.replace("tpcds.public.", prefix)).unwrap()
}

const TABLES: [&str; 13] = [
    "web_sales",
    "catalog_sales",
    "time_dim",
    "household_demographics",
    "customer_demographics",
    "promotion",
    "store_sales",
    "date_dim",
    "item",
    "customer",
    "store",
    "store_returns",
    "customer_address",
];

/// Copy DuckDB's generated tables into a container engine: CSV export, `docker cp`,
/// then the engine's bulk loader. Column types come from DuckDB's schema.
fn load(engine: Engine, duck: &Connection) {
    use std::process::Command;
    let container = match engine {
        Engine::Postgres => "ossiec-postgres",
        Engine::StarRocks => "ossiec-starrocks",
        Engine::ClickHouse => "ossiec-clickhouse",
        Engine::MySql => "ossiec-mysql",
        Engine::Trino => {
            // Trino's tpcds connector regenerates data on every query (about 30 s
            // each); copy tpcds.tiny into the memory catalog once instead.
            let mut sql = vec!["CREATE SCHEMA IF NOT EXISTS memory.tpcds".to_string()];
            for t in TABLES {
                sql.push(format!("DROP TABLE IF EXISTS memory.tpcds.{t}"));
                sql.push(format!(
                    "CREATE TABLE memory.tpcds.{t} AS SELECT * FROM tpcds.tiny.{t}"
                ));
            }
            common::run_in(engine, &sql.join(";\n"), ("memory", "tpcds"))
                .unwrap_or_else(|e| panic!("copying tpcds.tiny on Trino: {e}"));
            return;
        }
        _ => return, // DuckDB generates in place
    };
    // One directory per engine: the engine tests run in parallel.
    let dir = std::env::temp_dir().join(format!("ossiec-tpcds-{engine:?}"));
    std::fs::create_dir_all(&dir).unwrap();
    let sh = |args: &[&str]| {
        let out = Command::new("docker").args(args).output().expect("docker");
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(
            out.status.success() && !err.contains("ERROR"),
            "docker {args:?}: {err}"
        );
    };
    for t in TABLES {
        let mut st = duck
            .prepare(&format!("SELECT column_name, data_type FROM information_schema.columns WHERE table_name = '{t}' ORDER BY ordinal_position"))
            .unwrap();
        let cols: Vec<(String, String)> = st
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        let ddl_cols: Vec<String> = cols
            .iter()
            .map(|(n, ty)| {
                let ty = match (engine, ty.as_str()) {
                    (Engine::ClickHouse, "INTEGER") => "Nullable(Int32)".into(),
                    (Engine::ClickHouse, "BIGINT") => "Nullable(Int64)".into(),
                    (Engine::ClickHouse, "VARCHAR") => "Nullable(String)".into(),
                    (Engine::ClickHouse, "DATE") => "Nullable(Date)".into(),
                    (Engine::ClickHouse, d) => {
                        format!("Nullable({})", d.replace("DECIMAL", "Decimal"))
                    }
                    (Engine::StarRocks, "VARCHAR") => "VARCHAR(1024)".into(),
                    // MySQL caps a row at 64 KB; TPC-DS strings are at most 200 characters.
                    (Engine::MySql, "VARCHAR") => "VARCHAR(255)".into(),
                    (_, d) => d.to_string(),
                };
                format!("{n} {ty}")
            })
            .collect();
        let file = dir.join(format!("{t}.csv"));
        duck.execute_batch(&format!(
            "COPY {t} TO '{}' (HEADER false, DELIMITER '|', NULLSTR '\\N')",
            file.display()
        ))
        .unwrap();
        // MySQL only reads server-side files from its secure_file_priv directory.
        let dest = if engine == Engine::MySql {
            "/var/lib/mysql-files"
        } else {
            "/tmp"
        };
        sh(&[
            "cp",
            file.to_str().unwrap(),
            &format!("{container}:{dest}/{t}.csv"),
        ]);
        let (ddl, load): (String, Vec<String>) = match engine {
            Engine::Postgres => (
                format!("DROP TABLE IF EXISTS {t}; CREATE TABLE {t} ({});", ddl_cols.join(", ")),
                vec!["psql".into(), "-U".into(), "postgres".into(), "-v".into(), "ON_ERROR_STOP=1".into(), "-c".into(),
                     format!("\\copy {t} FROM '/tmp/{t}.csv' WITH (FORMAT csv, DELIMITER '|', NULL '\\N')")],
            ),
            Engine::ClickHouse => (
                format!("DROP TABLE IF EXISTS {t}; CREATE TABLE {t} ({}) ENGINE = MergeTree ORDER BY tuple();", ddl_cols.join(", ")),
                vec!["bash".into(), "-c".into(),
                     format!("clickhouse-client --format_csv_delimiter '|' --query 'INSERT INTO {t} FORMAT CSV' < /tmp/{t}.csv")],
            ),
            Engine::StarRocks => (
                format!("DROP TABLE IF EXISTS {t}; CREATE TABLE {t} ({}) PROPERTIES (\"replication_num\" = \"1\");", ddl_cols.join(", ")),
                vec!["bash".into(), "-c".into(), format!(
                    "curl -sf --location-trusted -u root: -H 'label:{t}-'$(date +%s%N) -H 'column_separator:|' -H 'enclose:\"' \
                     -H 'format:csv' -H 'Expect:100-continue' -T /tmp/{t}.csv http://127.0.0.1:8030/api/ossiec/{t}/_stream_load | grep -q '\"Status\": \"Success\"'"
                )],
            ),
            Engine::MySql => (
                format!("DROP TABLE IF EXISTS {t}; CREATE TABLE {t} ({});", ddl_cols.join(", ")),
                vec!["mysql".into(), "-uroot".into(), "-possiec".into(), "-D".into(), "ossiec".into(), "-e".into(), format!(
                    "LOAD DATA INFILE '/var/lib/mysql-files/{t}.csv' INTO TABLE {t} FIELDS TERMINATED BY '|' OPTIONALLY ENCLOSED BY '\"'"
                )],
            ),
            _ => unreachable!(),
        };
        common::run(engine, &ddl).unwrap_or_else(|e| panic!("creating {t} on {engine:?}: {e}"));
        let mut args = vec!["exec".to_string(), container.to_string()];
        args.extend(load);
        sh(&args.iter().map(String::as_str).collect::<Vec<_>>());
    }
}

/// DuckDB sorts NULLs last; StarRocks and MySQL sort them first on ASC. Spell
/// DuckDB's order out so a transpiled reference sorts the way it does on DuckDB:
/// `NULLS LAST`, or for MySQL, which has no such syntax, a leading `x IS NULL` key.
fn duckdb_null_order(sql: &str, engine: Engine) -> String {
    let ast = polyglot_sql::parse(sql, DialectType::DuckDB)
        .unwrap()
        .remove(0);
    let ast = polyglot_sql::transform(ast, &|e| {
        Ok(Some(match e {
            polyglot_sql::Expression::Select(mut s) => {
                for order in s.order_by.iter_mut() {
                    let mut keys = vec![];
                    for mut o in std::mem::take(&mut order.expressions) {
                        if engine == Engine::MySql {
                            let mut is_null = o.clone();
                            is_null.this = polyglot_sql::builder::Expr(o.this.clone())
                                .is_null()
                                .into_inner();
                            is_null.desc = o.nulls_first.take() == Some(true);
                            keys.push(is_null);
                        } else {
                            o.nulls_first.get_or_insert(false);
                        }
                        keys.push(o);
                    }
                    order.expressions = keys;
                }
                polyglot_sql::Expression::Select(s)
            }
            other => other,
        }))
    })
    .unwrap();
    polyglot_sql::generate(&ast, DialectType::DuckDB).unwrap()
}

fn sweep(engine: Engine) {
    let duck = duckdb_tpcds();
    load(engine, &duck);
    let mut failures = vec![];
    for c in CASES {
        let req: ossiec::Request = serde_json::from_str(c.request).unwrap();
        let opts = ossiec::Options {
            target: Some(engine.dialect()),
            ..Default::default()
        };
        let compiled = match ossiec::compile(&graph_for(c.model, engine), &req, &opts) {
            Ok(c) => c,
            Err(e) => {
                failures.push(format!("{}: compile: {e}", c.name));
                continue;
            }
        };
        let reference = match &c.reference {
            Reference::Sql(s) => s.to_string(),
            Reference::Official(n) => official(&duck, *n),
        };
        let (got, want) = match engine {
            Engine::DuckDb => (
                duckdb_rows(&duck, &compiled.sql),
                duckdb_rows(&duck, &reference),
            ),
            _ => {
                let mut reference = transpile(
                    &duckdb_null_order(&reference, engine),
                    DialectType::DuckDB,
                    engine.dialect(),
                )
                .unwrap()
                .join(";");
                if engine == Engine::ClickHouse {
                    // References use LEFT JOINs too; give them standard NULL semantics.
                    reference = format!("SET join_use_nulls = 1;\n{reference}");
                }
                match (
                    run_in(engine, &compiled.sql, ("memory", "tpcds")),
                    run_in(engine, &reference, ("memory", "tpcds")),
                ) {
                    (Ok(g), Ok(w)) => (g, w),
                    (Err(e), _) => {
                        failures.push(format!(
                            "{}: ossiec SQL failed: {e}\n{}",
                            c.name, compiled.sql
                        ));
                        continue;
                    }
                    (_, Err(e)) => {
                        failures.push(format!("{}: reference failed: {e}\n{reference}", c.name));
                        continue;
                    }
                }
            }
        };
        let (got, want) = if req.order_by.is_empty() {
            let (mut g, mut w) = (got, want);
            g.sort();
            w.sort();
            (g, w)
        } else {
            (got, want)
        };
        if want.is_empty() {
            failures.push(format!(
                "{}: reference returned no rows; the case proves nothing",
                c.name
            ));
        } else if !rows_close(&got, &want) {
            failures.push(format!(
                "{}: wrong rows ({} vs {})\n  got:  {:?}\n  want: {:?}\n{}",
                c.name,
                got.len(),
                want.len(),
                &got[..got.len().min(5)],
                &want[..want.len().min(5)],
                compiled.sql
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "{engine:?}: {} of {} TPC-DS cases failed\n\n{}",
        failures.len(),
        CASES.len(),
        failures.join("\n\n")
    );
}

#[test]
fn duckdb() {
    sweep(Engine::DuckDb);
}

#[test]
#[ignore = "needs Docker: scripts/engines.sh up"]
fn trino() {
    sweep(Engine::Trino);
}

#[test]
#[ignore = "needs Docker: scripts/engines.sh up"]
fn postgres() {
    sweep(Engine::Postgres);
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

/// Performance on Trino's TPC-DS sf=1 (2.9M `store_sales` rows): ossiec's
/// query against its reference, per case. Catches plans that are right but slow,
/// such as a de-duplicated part scanning the fact table again. Ignored by default.
#[test]
#[ignore = "needs Docker: scripts/engines.sh up; takes several minutes"]
fn trino_sf1_performance() {
    use std::time::Instant;
    let duck = Connection::open_in_memory().unwrap();
    duck.execute_batch("INSTALL tpcds; LOAD tpcds;").unwrap();
    let mut report = vec![];
    let mut slow = vec![];
    for c in CASES {
        let graph = ossiec::from_yaml(&c.model.replace("tpcds.public.", "tpcds.sf1.")).unwrap();
        let req: ossiec::Request = serde_json::from_str(c.request).unwrap();
        let opts = ossiec::Options {
            target: Some(ossiec::DialectType::Trino),
            ..Default::default()
        };
        let sql = ossiec::compile(&graph, &req, &opts).unwrap().sql;
        let reference = match &c.reference {
            Reference::Sql(s) => s.to_string(),
            Reference::Official(n) => official(&duck, *n),
        };
        let reference = transpile(&reference, DialectType::DuckDB, DialectType::Trino)
            .unwrap()
            .join(";");
        let time = |q: &str| {
            let t = Instant::now();
            run_in(Engine::Trino, q, ("tpcds", "sf1"))
                .unwrap_or_else(|e| panic!("{}: {e}", c.name));
            t.elapsed().as_secs_f64()
        };
        let (ours, theirs) = (time(&sql), time(&reference));
        report.push(format!(
            "{:40} ossiec {ours:6.2}s  reference {theirs:6.2}s",
            c.name
        ));
        // Within 3x of the hand-written reference, plus slack for small queries.
        if ours > 3.0 * theirs + 5.0 {
            slow.push(c.name);
        }
    }
    println!("{}", report.join("\n"));
    assert!(
        slow.is_empty(),
        "slower than 3x the reference: {slow:?}\n{}",
        report.join("\n")
    );
}

#[test]
#[ignore = "needs Docker: scripts/engines.sh up"]
fn mysql() {
    sweep(Engine::MySql);
}
