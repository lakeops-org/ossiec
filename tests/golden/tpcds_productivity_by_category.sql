-- request: {"metrics": ["store_productivity", "total_sales"], "dimensions": ["item.i_category"]}
-- dialect: generic (source: generic)
-- warning: FAN_TRAP_RISK: `SUM("store".s_number_employees)` is computed over `store` rows de-duplicated by primary key; a `store` row that belongs to several groups counts in each, so groups do not add up to a grand total
SELECT
  stacked.d0 AS "item.i_category",
  CAST(MAX(stacked.t0) AS DOUBLE) / NULLIF(MAX(stacked.t1), 0) AS "store_productivity",
  MAX(stacked.t0) AS "total_sales"
FROM (
  SELECT
    "item".i_category AS d0,
    SUM("store_sales".ss_ext_sales_price) AS t0,
    NULL AS t1
  FROM tpcds.public.store_sales AS "store_sales"
  LEFT JOIN tpcds.public.item AS "item"
    ON "store_sales".ss_item_sk = "item".i_item_sk
  LEFT JOIN tpcds.public.store AS "store"
    ON "store_sales".ss_store_sk = "store".s_store_sk
  GROUP BY
    "item".i_category
  UNION ALL
  SELECT
    dedup.d0 AS d0,
    NULL AS t0,
    SUM(dedup.c0) AS t1
  FROM (
    SELECT DISTINCT
      "item".i_category AS d0,
      "store".s_store_sk AS k0,
      "store".s_number_employees AS c0
    FROM tpcds.public.store_sales AS "store_sales"
    LEFT JOIN tpcds.public.item AS "item"
      ON "store_sales".ss_item_sk = "item".i_item_sk
    LEFT JOIN tpcds.public.store AS "store"
      ON "store_sales".ss_store_sk = "store".s_store_sk
    WHERE
      NOT "store".s_store_sk IS NULL
  ) AS dedup
  GROUP BY
    dedup.d0
) AS stacked
GROUP BY
  stacked.d0
