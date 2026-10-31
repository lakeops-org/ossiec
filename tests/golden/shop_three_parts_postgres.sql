-- request: {"metrics": ["total_revenue", "total_stock", "customer_count"], "dimensions": ["products.category"],
            "orderBy": [{"field": "products.category"}]}
-- dialect: postgresql (source: generic)
-- warning: FAN_TRAP_RISK: `SUM("products".stock)` is computed over `products` rows de-duplicated by primary key; a `products` row that belongs to several groups counts in each, so groups do not add up to a grand total
-- warning: FAN_TRAP_RISK: `COUNT("customers".customer_id)` is computed over `customers` rows de-duplicated by primary key; a `customers` row that belongs to several groups counts in each, so groups do not add up to a grand total
SELECT
  stacked.d0 COLLATE "C" AS "products.category",
  MAX(stacked.t0) AS "total_revenue",
  MAX(stacked.t1) AS "total_stock",
  COALESCE(MAX(stacked.t2), 0) AS "customer_count"
FROM (
  SELECT
    "products".category AS d0,
    SUM(CAST("orders".amount_cents AS DOUBLE PRECISION) / 100.0) AS t0,
    CAST(NULL AS DECIMAL) AS t1,
    CAST(NULL AS DECIMAL) AS t2
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
    "products".category
  UNION ALL
  SELECT
    dedup.d0 AS d0,
    CAST(NULL AS DECIMAL) AS t0,
    SUM(dedup.c0) AS t1,
    CAST(NULL AS DECIMAL) AS t2
  FROM (
    SELECT DISTINCT
      "products".category AS d0,
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
      NOT "products".product_id IS NULL
  ) AS dedup
  GROUP BY
    dedup.d0
  UNION ALL
  SELECT
    dedup.d0 AS d0,
    CAST(NULL AS DECIMAL) AS t0,
    CAST(NULL AS DECIMAL) AS t1,
    COUNT(dedup.c0) AS t2
  FROM (
    SELECT DISTINCT
      "products".category AS d0,
      "customers".customer_id AS k0,
      "customers".customer_id AS c0
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
      NOT "customers".customer_id IS NULL
  ) AS dedup
  GROUP BY
    dedup.d0
) AS stacked
GROUP BY
  stacked.d0
ORDER BY
  "products.category" ASC NULLS FIRST
