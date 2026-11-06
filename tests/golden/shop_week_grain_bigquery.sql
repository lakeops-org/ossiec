-- request: {"metrics": ["total_revenue"], "dimensions": ["orders.order_date"], "timeGrain": "week"}
-- dialect: bigquery (source: generic)
SELECT
  base.d0 AS `order_date`,
  base.t0 AS `total_revenue`
FROM (
  SELECT
    CAST(DATE_TRUNC(`orders`.order_date, ISOWEEK) AS DATE) AS d0,
    SUM(`orders`.amount_cents / 100.0) AS t0
  FROM orders AS `orders`
  GROUP BY
    CAST(DATE_TRUNC(`orders`.order_date, ISOWEEK) AS DATE)
) AS base
