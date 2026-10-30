-- request: {"metrics": ["brand_rank_in_store", "total_sales"], "dimensions": ["store.s_store_sk", "item.i_brand"],
            "metricFilters": [{"metric": "brand_rank_in_store", "op": "<=", "value": 3}],
            "orderBy": [{"field": "store.s_store_sk"}, {"field": "brand_rank_in_store"}]}
-- dialect: trino (source: generic)
SELECT
  *
FROM (
  SELECT
    base.d0 AS "store.s_store_sk",
    base.d1 AS "item.i_brand",
    RANK() OVER (PARTITION BY base.d0 ORDER BY base.t0 DESC) AS "brand_rank_in_store",
    base.t0 AS "total_sales"
  FROM (
    SELECT
      "store".s_store_sk AS d0,
      "item".i_brand AS d1,
      SUM("store_sales".ss_ext_sales_price) AS t0
    FROM tpcds.public.store_sales AS "store_sales"
    LEFT JOIN tpcds.public.item AS "item"
      ON "store_sales".ss_item_sk = "item".i_item_sk
    LEFT JOIN tpcds.public.store AS "store"
      ON "store_sales".ss_store_sk = "store".s_store_sk
    GROUP BY
      "store".s_store_sk,
      "item".i_brand
  ) AS base
) AS windowed
WHERE
  "brand_rank_in_store" <= 3
ORDER BY
  "store.s_store_sk" ASC NULLS FIRST,
  "brand_rank_in_store" ASC NULLS FIRST
