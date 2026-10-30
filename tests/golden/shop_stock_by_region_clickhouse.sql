-- request: {"metrics": ["total_stock", "total_revenue"], "dimensions": ["customers.region"]}
-- dialect: clickhouse (source: generic)
-- warning: FAN_TRAP_RISK: `SUM("products".stock)` is computed over `products` rows de-duplicated by primary key; a `products` row that belongs to several groups counts in each, so groups do not add up to a grand total
SELECT
  stacked.d0 AS "customers.region",
  MAX(stacked.t0) AS "total_stock",
  MAX(stacked.t1) AS "total_revenue"
FROM (
  SELECT
    "customers".region AS d0,
    NULL AS t0,
    SUM("orders".amount_cents / 100.0) AS t1
  FROM orders AS "orders"
  LEFT JOIN (
    SELECT
      *
    FROM raw_customers
    WHERE
      NOT deleted
  ) AS "customers"
    ON "orders".customer_id = "customers".customer_id
  LEFT JOIN products AS "products"
    ON "orders".product_id = "products".product_id
  GROUP BY
    "customers".region
  UNION ALL
  SELECT
    dedup.d0 AS d0,
    SUM(dedup.c0) AS t0,
    NULL AS t1
  FROM (
    SELECT DISTINCT
      "customers".region AS d0,
      "products".product_id AS k0,
      "products".stock AS c0
    FROM orders AS "orders"
    LEFT JOIN (
      SELECT
        *
      FROM raw_customers
      WHERE
        NOT deleted
    ) AS "customers"
      ON "orders".customer_id = "customers".customer_id
    LEFT JOIN products AS "products"
      ON "orders".product_id = "products".product_id
    WHERE
      NOT (
        "products".product_id IS NULL
      )
  ) AS dedup
  GROUP BY
    dedup.d0
) AS stacked
GROUP BY
  stacked.d0
SETTINGS join_use_nulls = 1
