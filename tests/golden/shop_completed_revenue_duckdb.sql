-- request: {"metrics": ["completed_revenue"], "dimensions": ["customers.region"]}
-- dialect: duckdb (source: generic)
SELECT
  base.d0 AS "region",
  base.t0 AS "completed_revenue"
FROM (
  SELECT
    "customers".region AS d0,
    SUM("orders".amount_cents / 100.0) FILTER(WHERE "orders".status = 'completed') AS t0
  FROM orders AS "orders"
  LEFT JOIN (
    SELECT
      *
    FROM raw_customers
    WHERE
      NOT deleted
  ) AS "customers"
    ON "orders".customer_id = "customers".customer_id
  GROUP BY
    "customers".region
) AS base
