-- request: {"dimensions": ["customers.name"], "filters": [{"field": "customers.name", "op": "IN", "values": ["O'Brien", "C:\\temp"]}]}
-- dialect: starrocks (source: generic)
SELECT
  base.d0 AS `customers.name`
FROM (
  SELECT
    `customers`.name AS d0
  FROM (
    SELECT
      *
    FROM raw_customers
    WHERE
      NOT deleted
  ) AS `customers`
  WHERE
    `customers`.name IN ('O''Brien', 'C:\\temp')
  GROUP BY
    `customers`.name
) AS base
