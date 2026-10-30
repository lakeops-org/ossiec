-- request: {"metrics": ["total_revenue", "orders.order_count"], "dimensions": ["orders.order_date"], "timeGrain": "month",
            "filters": [{"field": "orders.order_date", "op": ">=", "value": "2026-01-01"}]}
-- dialect: trino (source: generic)
SELECT
  base.d0 AS "orders.order_date",
  base.t0 AS "total_revenue",
  base.t1 AS "orders.order_count"
FROM (
  SELECT
    CAST(DATE_TRUNC('month', "orders".order_date) AS DATE) AS d0,
    SUM(CAST("orders".amount_cents AS DOUBLE) / 100.0) AS t0,
    COUNT(*) AS t1
  FROM orders AS "orders"
  WHERE
    "orders".order_date >= CAST('2026-01-01' AS DATE)
  GROUP BY
    CAST(DATE_TRUNC('month', "orders".order_date) AS DATE)
) AS base
