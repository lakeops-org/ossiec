-- request: {"metrics": ["store_productivity", "total_sales"], "dimensions": ["store.s_state"]}
-- dialect: generic (source: generic)
SELECT
  stacked.d0 AS "s_state",
  CAST(MAX(stacked.t0) AS DOUBLE) / NULLIF(MAX(stacked.t1), 0) AS "store_productivity",
  MAX(stacked.t0) AS "total_sales"
FROM (
  SELECT
    "store".s_state AS d0,
    SUM("store_sales".ss_ext_sales_price) AS t0,
    NULL AS t1
  FROM tpcds.public.store_sales AS "store_sales"
  LEFT JOIN tpcds.public.store AS "store"
    ON "store_sales".ss_store_sk = "store".s_store_sk
  GROUP BY
    "store".s_state
  UNION ALL
  SELECT
    "store".s_state AS d0,
    NULL AS t0,
    SUM("store".s_number_employees) AS t1
  FROM tpcds.public.store AS "store"
  GROUP BY
    "store".s_state
) AS stacked
GROUP BY
  stacked.d0
