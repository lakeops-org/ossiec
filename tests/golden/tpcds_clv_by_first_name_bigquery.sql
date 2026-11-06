-- request: {"metrics": ["customer_lifetime_value"], "dimensions": ["customer.c_first_name"],
            "filters": [{"field": "customer.c_last_name", "op": "IN", "values": ["Smith", "Jones"]}],
            "orderBy": [{"field": "customer_lifetime_value", "direction": "desc"}], "limit": 5}
-- dialect: bigquery (source: generic)
SELECT
  stacked.d0 AS `c_first_name`,
  CAST(MAX(stacked.t0) AS FLOAT64) / NULLIF(COALESCE(MAX(stacked.t1), 0), 0) AS `customer_lifetime_value`
FROM (
  SELECT
    `customer`.c_first_name AS d0,
    SUM(`store_sales`.ss_ext_sales_price) AS t0,
    NULL AS t1
  FROM tpcds.public.store_sales AS `store_sales`
  LEFT JOIN tpcds.public.customer AS `customer`
    ON `store_sales`.ss_customer_sk = `customer`.c_customer_sk
  WHERE
    `customer`.c_last_name IN ('Smith', 'Jones')
  GROUP BY
    `customer`.c_first_name
  UNION ALL
  SELECT
    `customer`.c_first_name AS d0,
    NULL AS t0,
    COUNT(DISTINCT `customer`.c_customer_sk) AS t1
  FROM tpcds.public.customer AS `customer`
  WHERE
    `customer`.c_last_name IN ('Smith', 'Jones')
  GROUP BY
    `customer`.c_first_name
) AS stacked
GROUP BY
  `c_first_name`
ORDER BY
  `customer_lifetime_value` DESC
LIMIT 5
