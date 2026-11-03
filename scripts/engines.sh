#!/usr/bin/env bash
# Start / stop the engines the cross-engine test tier runs against.
#
#   scripts/engines.sh up      # start (or reuse) and wait until every engine answers
#   scripts/engines.sh down    # remove the containers
#   scripts/engines.sh only trino  # pause the other engines (less noise for timings)
#   cargo test --test engines -- --ignored
#
# Containers are named ossiec-*; nothing else on the machine is touched.
set -euo pipefail

TRINO_IMAGE="${TRINO_IMAGE:-trinodb/trino:484}"
STARROCKS_IMAGE="${STARROCKS_IMAGE:-starrocks/allin1-ubuntu:4.1.6}"
CLICKHOUSE_IMAGE="${CLICKHOUSE_IMAGE:-clickhouse/clickhouse-server:26.8}"
# Debian-based on purpose: its en_US.UTF-8 collation sorts like real deployments
# (Alpine/musl sorts by byte), which the string_ordering case depends on.
POSTGRES_IMAGE="${POSTGRES_IMAGE:-postgres:16}"
MYSQL_IMAGE="${MYSQL_IMAGE:-mysql:8.4}"
ENGINES=(postgres clickhouse trino starrocks mysql)

unpause_all() {
  for e in "${ENGINES[@]}"; do docker unpause "ossiec-$e" >/dev/null 2>&1 || true; done
}

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
    unpause_all
    # StarRocks is the slowest to boot (FE + BE). Start it first so it warms
    # while the other images pull; CI has timed out waiting for port 9030.
    start ossiec-starrocks "$STARROCKS_IMAGE"
    start ossiec-postgres "$POSTGRES_IMAGE" -e POSTGRES_PASSWORD=ossiec
    start ossiec-clickhouse "$CLICKHOUSE_IMAGE"
    start ossiec-trino "$TRINO_IMAGE"
    # local-infile: the TPC-DS tier bulk-loads CSV.
    start ossiec-mysql "$MYSQL_IMAGE" -e MYSQL_ROOT_PASSWORD=ossiec -e MYSQL_DATABASE=ossiec
    wait_for ossiec-postgres 60 psql -U postgres -c 'SELECT 1'
    wait_for ossiec-clickhouse 60 clickhouse-client --query 'SELECT 1'
    wait_for ossiec-trino 180 trino --execute 'SELECT 1 FROM memory.information_schema.schemata LIMIT 1'
    wait_for ossiec-mysql 120 mysql -uroot -possiec -e "SELECT 1"
    wait_for ossiec-starrocks 600 bash -c \
      "mysql -h127.0.0.1 -P9030 -uroot -N -e 'SHOW BACKENDS' | grep -q true"
    ;;
  only)
    [[ " ${ENGINES[*]} " == *" ${2:-} "* ]] || { echo "usage: $0 only <${ENGINES[*]}>" >&2; exit 2; }
    unpause_all
    for e in "${ENGINES[@]}"; do
      [[ $e == "$2" ]] || docker pause "ossiec-$e" >/dev/null
    done
    echo "only ossiec-$2 running; '$0 up' resumes the rest"
    ;;
  down)
    unpause_all
    docker rm -f ossiec-postgres ossiec-clickhouse ossiec-trino ossiec-starrocks ossiec-mysql >/dev/null 2>&1 || true
    echo "removed"
    ;;
  *)
    echo "usage: $0 up|down|only <engine>" >&2
    exit 2
    ;;
esac
