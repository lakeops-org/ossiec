-- request: {"metrics": ["users"], "dimensions": ["events.day"]}
-- dialect: snowflake (source: snowflake)
SELECT
  base.d0 AS "events.day",
  base.t0 AS "users"
FROM (
  SELECT
    TO_DATE("events".event_ts) AS d0,
    COUNT(DISTINCT "events".user_id) AS t0
  FROM analytics.events AS "events"
  GROUP BY
    TO_DATE("events".event_ts)
) AS base
