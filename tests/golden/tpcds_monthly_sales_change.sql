-- request: {"metrics": ["monthly_sales_change"], "dimensions": ["date_dim.d_year", "date_dim.d_moy"], "deferredMetrics": true}
-- dialect: generic (source: generic)
SELECT
  base.d0 AS "d_year",
  base.d1 AS "d_moy",
  base.t0 - LAG(base.t0, 1) OVER (ORDER BY base.d0, base.d1) AS "monthly_sales_change"
FROM (
  SELECT
    "date_dim".d_year AS d0,
    "date_dim".d_moy AS d1,
    SUM("store_sales".ss_ext_sales_price) AS t0
  FROM tpcds.public.store_sales AS "store_sales"
  LEFT JOIN tpcds.public.date_dim AS "date_dim"
    ON "store_sales".ss_sold_date_sk = "date_dim".d_date_sk
  GROUP BY
    "date_dim".d_year,
    "date_dim".d_moy
) AS base
