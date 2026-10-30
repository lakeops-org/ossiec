-- request: {"metrics": ["cumulative_sales", "total_sales"], "dimensions": ["date_dim.d_date"], "orderBy": [{"field": "date_dim.d_date"}]}
-- dialect: snowflake (source: generic)
SELECT
  base.d0 AS "date_dim.d_date",
  SUM(base.t0) OVER (ORDER BY base.d0 NULLS FIRST ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW) AS "cumulative_sales",
  base.t0 AS "total_sales"
FROM (
  SELECT
    "date_dim".d_date AS d0,
    SUM("store_sales".ss_ext_sales_price) AS t0
  FROM tpcds.public.store_sales AS "store_sales"
  LEFT JOIN tpcds.public.date_dim AS "date_dim"
    ON "store_sales".ss_sold_date_sk = "date_dim".d_date_sk
  GROUP BY
    "date_dim".d_date
) AS base
ORDER BY
  "date_dim.d_date" ASC NULLS FIRST
