-- request: {"metrics": ["revenue_change"], "dimensions": ["orders.order_date"], "timeGrain": "month"}
-- dialect: clickhouse (source: generic)
SELECT
  base.d0 AS "orders.order_date",
  base.t0 - lagInFrame(toNullable(base.t0), 1) OVER (ORDER BY base.d0 NULLS FIRST ROWS BETWEEN UNBOUNDED PRECEDING AND UNBOUNDED FOLLOWING) AS "revenue_change"
FROM (
  SELECT
    CAST(dateTrunc('month', "orders".order_date) AS Nullable(DATE)) AS d0,
    SUM("orders".amount_cents / 100.0) AS t0
  FROM orders AS "orders"
  GROUP BY
    CAST(dateTrunc('month', "orders".order_date) AS Nullable(DATE))
) AS base
SETTINGS join_use_nulls = 1
