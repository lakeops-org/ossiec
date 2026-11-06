-- request: {"metrics": ["revenue_change_to_next"], "dimensions": ["orders.order_date"], "timeGrain": "month", "deferredMetrics": true}
-- dialect: clickhouse (source: generic)
SELECT
  base.d0 AS "order_date",
  leadInFrame(toNullable(base.t0), 1) OVER (ORDER BY base.d0 NULLS FIRST ROWS BETWEEN UNBOUNDED PRECEDING AND UNBOUNDED FOLLOWING) - base.t0 AS "revenue_change_to_next"
FROM (
  SELECT
    CAST(dateTrunc('month', "orders".order_date) AS Nullable(DATE)) AS d0,
    SUM("orders".amount_cents / 100.0) AS t0
  FROM orders AS "orders"
  GROUP BY
    CAST(dateTrunc('month', "orders".order_date) AS Nullable(DATE))
) AS base
SETTINGS join_use_nulls = 1, aggregate_functions_null_for_empty = 1
