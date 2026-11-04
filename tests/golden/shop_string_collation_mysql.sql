-- request: {"metrics": ["total_revenue"], "dimensions": ["customers.segment"],
            "filters": [{"field": "customers.region", "op": "IN", "values": ["EU", "US"]}]}
-- dialect: mysql (source: generic)
SELECT
  base.d0 AS `customers.segment`,
  base.t0 AS `total_revenue`
FROM (
  SELECT
    `customers`.segment COLLATE utf8mb4_bin AS d0,
    SUM(`orders`.amount_cents / 100.0) AS t0
  FROM orders AS `orders`
  LEFT JOIN (
    SELECT
      *
    FROM raw_customers
    WHERE
      NOT deleted
  ) AS `customers`
    ON `orders`.customer_id = `customers`.customer_id
  WHERE
    `customers`.region COLLATE utf8mb4_bin IN ('EU', 'US')
  GROUP BY
    `customers`.segment COLLATE utf8mb4_bin
) AS base
