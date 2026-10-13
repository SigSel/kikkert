#!/usr/bin/env bash
# Throwaway databases for kikkert's engine tests, in local containers.
#
#   scripts/test-dbs.sh up       start and seed Postgres, MySQL, SQL Server and a SQLite file
#   scripts/test-dbs.sh status   say what is running; fails unless all of it is
#   scripts/test-dbs.sh test     run the engine tests against them
#   scripts/test-dbs.sh down     remove the containers again
#
# The justfile wraps these: `just dbs-up`, `just test-dbs`, `just dbs-down`.
# CONTAINER_ENGINE picks podman or docker; podman is preferred when both exist.
#
# Everything listens on 127.0.0.1 only. Starting the SQL Server container
# accepts Microsoft's developer EULA (ACCEPT_EULA=Y).
set -euo pipefail

cd "$(dirname "$0")/.."
RUNTIME="${CONTAINER_ENGINE:-$(command -v podman || command -v docker)}"
SEED=tests/seed
SQLITE_FILE="${KIKKERT_TEST_SQLITE:-target/kikkert-test.sqlite}"
MSSQL_PASSWORD='Kikkert!Passw0rd'

wait_for() {
  local what="$1"; shift
  for _ in $(seq 1 90); do
    if "$@" >/dev/null 2>&1; then return 0; fi
    sleep 1
  done
  echo "$what did not come up" >&2
  return 1
}

up() {
  "$RUNTIME" rm -f kikkert-test-pg kikkert-test-mysql kikkert-test-mssql >/dev/null 2>&1 || true
  "$RUNTIME" run -d --name kikkert-test-pg -e POSTGRES_PASSWORD=kikkert \
    -p 127.0.0.1:55432:5432 docker.io/library/postgres:17-alpine >/dev/null
  "$RUNTIME" run -d --name kikkert-test-mysql -e MYSQL_ROOT_PASSWORD=kikkert \
    -p 127.0.0.1:53306:3306 docker.io/library/mysql:8.4 >/dev/null
  "$RUNTIME" run -d --name kikkert-test-mssql -e ACCEPT_EULA=Y -e "MSSQL_SA_PASSWORD=$MSSQL_PASSWORD" \
    -p 127.0.0.1:51433:1433 mcr.microsoft.com/mssql/server:2022-latest >/dev/null

  wait_for postgres "$RUNTIME" exec kikkert-test-pg psql -U postgres -c 'select 1'
  "$RUNTIME" exec -i kikkert-test-pg psql -q -U postgres -v ON_ERROR_STOP=1 < "$SEED/postgres.sql"

  wait_for mysql "$RUNTIME" exec kikkert-test-mysql mysql -h127.0.0.1 -uroot -pkikkert -e 'select 1'
  "$RUNTIME" exec -i kikkert-test-mysql mysql -h127.0.0.1 -uroot -pkikkert --default-character-set=utf8mb4 < "$SEED/mysql.sql"

  local sqlcmd=(/opt/mssql-tools18/bin/sqlcmd -S localhost -U sa -P "$MSSQL_PASSWORD" -C -b)
  wait_for "sql server" "$RUNTIME" exec kikkert-test-mssql "${sqlcmd[@]}" -Q 'select 1'
  "$RUNTIME" exec -i kikkert-test-mssql "${sqlcmd[@]}" -f 65001 -i /dev/stdin < "$SEED/mssql.sql" >/dev/null

  mkdir -p "$(dirname "$SQLITE_FILE")"
  rm -f "$SQLITE_FILE"
  sqlite3 "$SQLITE_FILE" < "$SEED/sqlite.sql"
  echo "test databases are up"
}

status() {
  local missing=0
  for name in kikkert-test-pg kikkert-test-mysql kikkert-test-mssql; do
    if [ "$("$RUNTIME" inspect -f '{{.State.Running}}' "$name" 2>/dev/null)" = "true" ]; then
      echo "$name  running"
    else
      echo "$name  not running"
      missing=1
    fi
  done
  if [ -f "$SQLITE_FILE" ]; then
    echo "$SQLITE_FILE  present"
  else
    echo "$SQLITE_FILE  missing"
    missing=1
  fi
  return "$missing"
}

down() {
  "$RUNTIME" rm -f kikkert-test-pg kikkert-test-mysql kikkert-test-mssql >/dev/null 2>&1 || true
  rm -f "$SQLITE_FILE"
  echo "test databases removed"
}

case "${1:-}" in
  up) up ;;
  status) status ;;
  down) down ;;
  test) KIKKERT_TEST_DBS=1 KIKKERT_TEST_SQLITE="$PWD/$SQLITE_FILE" cargo test --test engines -- --test-threads=1 "${@:2}" ;;
  *) sed -n '2,13p' "$0"; exit 2 ;;
esac
