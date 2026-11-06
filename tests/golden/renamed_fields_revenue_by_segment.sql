-- request: {"metrics": ["total_revenue"], "dimensions": ["customers.segment"]}
-- dialect: generic (source: generic)
SELECT
  base.d0 AS "segment",
  base.t0 AS "total_revenue"
FROM (
  SELECT
    "customers".c_mktsegment AS d0,
    SUM("orders".total_cents / 100.0) AS t0
  FROM sales.public.orders AS "orders"
  LEFT JOIN sales.public.customers AS "customers"
    ON "orders".o_custkey = "customers".c_custkey
  GROUP BY
    "customers".c_mktsegment
) AS base
