#!/usr/bin/env bash
# Start / stop the engines the cross-engine test tier runs against.
#
#   scripts/engines.sh up      # start (or reuse) and wait until every engine answers
#   scripts/engines.sh down    # remove the containers
#   cargo test --test engines -- --ignored
#
# Containers are named ossiec-*; nothing else on the machine is touched.
set -euo pipefail

TRINO_IMAGE="${TRINO_IMAGE:-trinodb/trino:484}"
STARROCKS_IMAGE="${STARROCKS_IMAGE:-starrocks/allin1-ubuntu:4.1.6}"
CLICKHOUSE_IMAGE="${CLICKHOUSE_IMAGE:-clickhouse/clickhouse-server:25.8}"
POSTGRES_IMAGE="${POSTGRES_IMAGE:-postgres:16-alpine}"

start() { # name image [docker args...]
  local name=$1 image=$2
  shift 2
  if docker ps --format '{{.Names}}' | grep -qx "$name"; then
    echo "$name: running"
  elif docker ps -a --format '{{.Names}}' | grep -qx "$name"; then
    docker start "$name" >/dev/null && echo "$name: restarted"
  else
    docker run -d --name "$name" "$@" "$image" >/dev/null && echo "$name: started ($image)"
  fi
}

wait_for() { # name timeout-seconds command...
  local name=$1 timeout=$2
  shift 2
  local start=$SECONDS
  until docker exec "$name" "$@" >/dev/null 2>&1; do
    if ((SECONDS - start > timeout)); then
      echo "$name: not ready after ${timeout}s" >&2
      docker logs --tail 20 "$name" >&2
      exit 1
    fi
    sleep 2
  done
  echo "$name: ready ($((SECONDS - start))s)"
}

case "${1:-}" in
  up)
    start ossiec-postgres "$POSTGRES_IMAGE" -e POSTGRES_PASSWORD=ossiec
    start ossiec-clickhouse "$CLICKHOUSE_IMAGE"
    start ossiec-trino "$TRINO_IMAGE"
    start ossiec-starrocks "$STARROCKS_IMAGE"
    wait_for ossiec-postgres 60 psql -U postgres -c 'SELECT 1'
    wait_for ossiec-clickhouse 60 clickhouse-client --query 'SELECT 1'
    wait_for ossiec-trino 180 trino --execute 'SELECT 1 FROM memory.information_schema.schemata LIMIT 1'
    wait_for ossiec-starrocks 300 bash -c \
      "mysql -h127.0.0.1 -P9030 -uroot -N -e 'SHOW BACKENDS' | grep -q true"
    ;;
  down)
    docker rm -f ossiec-postgres ossiec-clickhouse ossiec-trino ossiec-starrocks >/dev/null 2>&1 || true
    echo "removed"
    ;;
  *)
    echo "usage: $0 up|down" >&2
    exit 2
    ;;
esac
