#!/usr/bin/env bash
set -Eeuo pipefail

DATA_DIR="${VOXYTOK_DATA_DIR:-/home/container/data}"
PGDATA="${POSTGRES_DATA_DIR:-$DATA_DIR/postgres}"
: "${POSTGRES_DB:=voxytok}"
: "${POSTGRES_USER:=voxytok}"
: "${POSTGRES_PORT:=5432}"

if [[ -z "${POSTGRES_PASSWORD:-}" ]]; then
  echo "POSTGRES_PASSWORD must be set to a strong secret." >&2
  exit 1
fi

mkdir -p "$DATA_DIR" "$PGDATA" /var/run/postgresql /var/log/postgresql
chown -R postgres:postgres "$PGDATA" /var/run/postgresql /var/log/postgresql

if [[ ! -s "$PGDATA/PG_VERSION" ]]; then
  gosu postgres initdb -D "$PGDATA" --auth-local=trust --auth-host=scram-sha-256
  gosu postgres pg_ctl -D "$PGDATA" -o "-c listen_addresses=127.0.0.1 -p $POSTGRES_PORT" -w start
  # Escape SQL string literals before using environment-provided credentials.
  escaped_password=${POSTGRES_PASSWORD//\'/\'\'}
  gosu postgres psql -v ON_ERROR_STOP=1 --username postgres <<SQL
CREATE USER "$POSTGRES_USER" WITH PASSWORD '$escaped_password';
CREATE DATABASE "$POSTGRES_DB" OWNER "$POSTGRES_USER";
SQL
  gosu postgres pg_ctl -D "$PGDATA" -m fast -w stop
fi

gosu postgres pg_ctl -D "$PGDATA" -o "-c listen_addresses=127.0.0.1 -p $POSTGRES_PORT" -w start
cleanup() {
  gosu postgres pg_ctl -D "$PGDATA" -m fast -w stop || true
}
trap cleanup EXIT TERM INT

export DATABASE_URL="postgresql://$POSTGRES_USER:$POSTGRES_PASSWORD@127.0.0.1:$POSTGRES_PORT/$POSTGRES_DB"
gosu voxytok /usr/local/bin/voxytok &
app_pid=$!
wait "$app_pid"
