-- request: {"metrics": ["users"], "dimensions": ["events.day"]}
-- dialect: generic (source: generic)
SELECT
  base.d0 AS "day",
  base.t0 AS "users"
FROM (
  SELECT
    CAST("events".event_ts AS DATE) AS d0,
    COUNT(DISTINCT "events".user_id) AS t0
  FROM analytics.events AS "events"
  GROUP BY
    CAST("events".event_ts AS DATE)
) AS base
