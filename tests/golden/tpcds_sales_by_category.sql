-- request: {"metrics": ["total_sales", "total_profit"], "dimensions": ["item.i_category"]}
-- dialect: generic (source: generic)
SELECT
  base.d0 AS "item.i_category",
  base.t0 AS "total_sales",
  base.t1 AS "total_profit"
FROM (
  SELECT
    "item".i_category AS d0,
    SUM("store_sales".ss_ext_sales_price) AS t0,
    SUM("store_sales".ss_net_profit) AS t1
  FROM tpcds.public.store_sales AS "store_sales"
  LEFT JOIN tpcds.public.item AS "item"
    ON "store_sales".ss_item_sk = "item".i_item_sk
  GROUP BY
    "item".i_category
) AS base
