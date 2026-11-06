-- request: {"metrics": ["total_revenue", "total_stock"], "dimensions": ["products.category"],
            "orderBy": [{"field": "products.category"}]}
-- dialect: postgresql (source: generic)
SELECT
  stacked.d0 COLLATE "C" AS "category",
  MAX(stacked.t0) AS "total_revenue",
  MAX(stacked.t1) AS "total_stock"
FROM (
  SELECT
    "products".category AS d0,
    SUM(CAST("orders".amount_cents AS DOUBLE PRECISION) / 100.0) AS t0,
    CAST(NULL AS DECIMAL) AS t1
  FROM orders AS "orders"
  LEFT JOIN products AS "products"
    ON "orders".product_id = "products".product_id
  GROUP BY
    "products".category
  UNION ALL
  SELECT
    "products".category AS d0,
    CAST(NULL AS DECIMAL) AS t0,
    SUM("products".stock) AS t1
  FROM products AS "products"
  GROUP BY
    "products".category
) AS stacked
GROUP BY
  stacked.d0
ORDER BY
  "category" ASC NULLS FIRST
