-- request: {"metrics": ["customer_lifetime_value"], "dimensions": ["date_dim.d_year"],
            "filters": [{"field": "store.s_state", "op": "IN", "values": ["CA", "TX"]}],
            "orderBy": [{"field": "customer_lifetime_value", "direction": "desc"}], "limit": 5}
-- dialect: bigquery (source: generic)
SELECT
  base.d0 AS `date_dim.d_year`,
  CAST(base.t0 AS FLOAT64) / base.t1 AS `customer_lifetime_value`
FROM (
  SELECT
    `date_dim`.d_year AS d0,
    SUM(`store_sales`.ss_ext_sales_price) AS t0,
    COUNT(DISTINCT `customer`.c_customer_sk) AS t1
  FROM tpcds.public.store_sales AS `store_sales`
  LEFT JOIN tpcds.public.date_dim AS `date_dim`
    ON `store_sales`.ss_sold_date_sk = `date_dim`.d_date_sk
  LEFT JOIN tpcds.public.customer AS `customer`
    ON `store_sales`.ss_customer_sk = `customer`.c_customer_sk
  LEFT JOIN tpcds.public.store AS `store`
    ON `store_sales`.ss_store_sk = `store`.s_store_sk
  WHERE
    `store`.s_state IN ('CA', 'TX')
  GROUP BY
    `date_dim`.d_year
) AS base
ORDER BY
  `customer_lifetime_value` DESC
LIMIT 5
