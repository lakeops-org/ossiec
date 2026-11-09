//! The Layer 2 SQL cases (shop and a two-fact model), each with its reference
//! SQL, run on DuckDB and in the engine tier.

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

/// Two independent facts over the shop tables, sharing `customers`: every order,
/// and the completed ones. A join on one narrows the other through `EXISTS`.
pub const TWO_FACT_SHOP: &str = r#"
version: "0.2.0.dev0"
name: shop_two_facts
datasets:
  - name: orders
    source: orders
    primary_key: [id]
    fields:
      - { name: id, expression: { sql: order_id }, datatype: Integer }
      - { name: customer_id, expression: { sql: customer_id }, datatype: Integer }
      - { name: status, expression: { sql: status }, datatype: String }
    metrics:
      - { name: order_count, expression: { sql: COUNT(*) } }
  - name: completed
    source: SELECT * FROM orders WHERE status = 'completed'
    primary_key: [id]
    fields:
      - { name: id, expression: { sql: order_id }, datatype: Integer }
      - { name: customer_id, expression: { sql: customer_id }, datatype: Integer }
    metrics:
      - { name: completed_count, expression: { sql: COUNT(*) } }
  - name: customers
    source: SELECT * FROM raw_customers WHERE NOT deleted
    primary_key: [id]
    fields:
      - { name: id, expression: { sql: customer_id }, datatype: Integer }
      - { name: region, expression: { sql: region }, datatype: String, dimension: {} }
relationships:
  - { name: o_c, from: orders, to: customers, from_columns: [customer_id], to_columns: [id] }
  - { name: k_c, from: completed, to: customers, from_columns: [customer_id], to_columns: [id] }
"#;

/// [`TWO_FACT_SHOP`] cases: §7.6 / §10.2, the query is one relation.
pub const LAYER2_TWO_FACT_CASES: &[Layer2NestedCase] = &[
    Layer2NestedCase {
        // Only customers with a completed order: their orders of any status.
        name: "inner_join_on_one_fact_narrows_the_other",
        sql: "SELECT c.region, AGG(o.order_count) AS n, AGG(k.completed_count) AS k \
              FROM customers c LEFT JOIN orders o ON o.customer_id = c.id \
              JOIN completed k ON k.customer_id = c.id GROUP BY c.region",
        reference:
            "SELECT c.region, COUNT(DISTINCT o.order_id) AS n, COUNT(DISTINCT k.order_id) AS k \
                    FROM {customers} c LEFT JOIN orders o ON o.customer_id = c.customer_id \
                    JOIN (SELECT * FROM orders WHERE status = 'completed') k \
                      ON k.customer_id = c.customer_id \
                    GROUP BY c.region",
        script: false,
    },
    Layer2NestedCase {
        // From the fact: customers with an order, and orders with no customer.
        name: "left_join_from_one_fact_narrows_the_other",
        sql: "SELECT c.region, AGG(o.order_count) AS n, AGG(k.completed_count) AS k \
              FROM orders o LEFT JOIN customers c ON o.customer_id = c.id \
              LEFT JOIN completed k ON k.customer_id = c.id GROUP BY c.region",
        reference:
            "SELECT c.region, COUNT(DISTINCT o.order_id) AS n, COUNT(DISTINCT k.order_id) AS k \
                    FROM orders o LEFT JOIN {customers} c ON o.customer_id = c.customer_id \
                    LEFT JOIN (SELECT * FROM orders WHERE status = 'completed') k \
                      ON k.customer_id = c.customer_id \
                    GROUP BY c.region",
        script: false,
    },
    Layer2NestedCase {
        // §7.3: a WHERE on one fact's columns drops the joined rows it fails, so
        // the other fact keeps only the customers that still have such an order.
        name: "where_on_one_fact_narrows_the_other",
        sql: "SELECT c.region, AGG(o.order_count) AS n, AGG(k.completed_count) AS k \
              FROM customers c LEFT JOIN orders o ON o.customer_id = c.id \
              LEFT JOIN completed k ON k.customer_id = c.id \
              WHERE o.id > 3 GROUP BY c.region",
        reference:
            "SELECT c.region, COUNT(DISTINCT o.order_id) AS n, COUNT(DISTINCT k.order_id) AS k \
                    FROM {customers} c LEFT JOIN orders o ON o.customer_id = c.customer_id \
                    LEFT JOIN (SELECT * FROM orders WHERE status = 'completed') k \
                      ON k.customer_id = c.customer_id \
                    WHERE o.order_id > 3 \
                    GROUP BY c.region",
        script: false,
    },
    Layer2NestedCase {
        // §7.3: BETWEEN, NOT IN and LIKE on one fact's columns narrow the other
        // fact too, as a comparison does.
        name: "where_in_between_like_on_one_fact",
        sql: "SELECT c.region, AGG(o.order_count) AS n, AGG(k.completed_count) AS k \
              FROM customers c LEFT JOIN orders o ON o.customer_id = c.id \
              LEFT JOIN completed k ON k.customer_id = c.id \
              WHERE o.id BETWEEN 100 AND 105 AND o.id NOT IN (101, 104) \
                AND o.status LIKE 'c%' GROUP BY c.region",
        reference:
            "SELECT c.region, COUNT(DISTINCT o.order_id) AS n, COUNT(DISTINCT k.order_id) AS k \
                    FROM {customers} c LEFT JOIN orders o ON o.customer_id = c.customer_id \
                    LEFT JOIN (SELECT * FROM orders WHERE status = 'completed') k \
                      ON k.customer_id = c.customer_id \
                    WHERE o.order_id BETWEEN 100 AND 105 AND o.order_id NOT IN (101, 104) \
                      AND o.status LIKE 'c%' \
                    GROUP BY c.region",
        script: false,
    },
];

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
        // §7.2: arithmetic on plain aggregates, alone and with AGG(): plain SQL over
        // the joined rows beside a measure that counts its own rows once.
        name: "plain_aggregate_arithmetic_beside_a_measure",
        sql: "SELECT c.region, COUNT(*) - AGG(o.order_count) AS no_orders, MAX(c.id) + 1 AS next_id \
              FROM customers c LEFT JOIN orders o ON o.customer_id = c.id GROUP BY c.region",
        reference: "SELECT c.region, COUNT(*) - COUNT(o.order_id), MAX(c.customer_id) + 1 \
              FROM {customers} c LEFT JOIN orders o ON o.customer_id = c.customer_id \
              GROUP BY c.region",
        script: false,
    },
    Layer2NestedCase {
        // §7.5: plain aggregates in HAVING and ORDER BY, beside AGG().
        name: "having_and_order_by_on_plain_aggregates",
        sql: "SELECT c.region, AGG(o.order_count) AS n FROM customers c \
              LEFT JOIN orders o ON o.customer_id = c.id GROUP BY c.region \
              HAVING COUNT(*) > AGG(o.order_count) ORDER BY COUNT(*) DESC, c.region",
        reference: "SELECT c.region, COUNT(o.order_id) FROM {customers} c \
              LEFT JOIN orders o ON o.customer_id = c.customer_id GROUP BY c.region \
              HAVING COUNT(*) > COUNT(o.order_id) ORDER BY COUNT(*) DESC, c.region",
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
        // §7.4: every level of a ROLLUP, stacked: subtotal rows carry NULLs.
        name: "rollup_reports_every_level",
        sql: "SELECT c.region, o.status, AGG(o.order_count) AS n FROM orders o \
              JOIN customers c ON o.customer_id = c.id GROUP BY ROLLUP(c.region, o.status)",
        reference: "SELECT c.region, o.status, COUNT(*) AS n FROM orders o \
                    JOIN {customers} c ON o.customer_id = c.customer_id \
                    GROUP BY ROLLUP(c.region, o.status)",
        script: false,
    },
    Layer2NestedCase {
        // §7.4: no set has both columns, so a branch with no rows types them
        // first (Postgres types UNION pairwise). A one-side measure, per level.
        name: "grouping_sets_without_the_full_set",
        sql: "SELECT o.status, c.segment, AGG(c.known_customers) AS k FROM customers c \
              JOIN orders o ON o.customer_id = c.id \
              GROUP BY GROUPING SETS ((o.status), (c.segment), ())",
        reference: "SELECT o.status, c.segment, COUNT(DISTINCT c.customer_id) AS k \
                    FROM {customers} c JOIN orders o ON o.customer_id = c.customer_id \
                    GROUP BY GROUPING SETS ((o.status), (c.segment), ())",
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
        // §7.2: plain aggregates count the joined rows, NULL-extended ones included;
        // the measure counts orders.
        name: "plain_aggregates_beside_a_measure",
        sql: "SELECT c.region, COUNT(*) AS rows_, MAX(o.order_date) AS last, \
              AGG(o.order_count) AS n \
              FROM customers c LEFT JOIN orders o ON o.customer_id = c.id GROUP BY c.region",
        reference: "SELECT c.region, COUNT(*) AS rows_, MAX(o.order_date) AS last, \
                    COUNT(o.order_id) AS n \
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
