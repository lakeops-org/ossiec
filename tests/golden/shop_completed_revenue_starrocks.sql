-- request: {"metrics": ["completed_revenue"], "dimensions": ["customers.region"]}
-- dialect: starrocks (source: generic)
SELECT
  base.d0 AS `customers.region`,
  base.t0 AS `completed_revenue`
FROM (
  SELECT
    `customers`.region AS d0,
    SUM(CASE WHEN `orders`.status = 'completed' THEN `orders`.amount_cents / 100.0 END) AS t0
  FROM orders AS `orders`
  LEFT JOIN (
    SELECT
      *
    FROM raw_customers
    WHERE
      NOT deleted
  ) AS `customers`
    ON `orders`.customer_id = `customers`.customer_id
  GROUP BY
    `customers`.region
) AS base
