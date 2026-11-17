#!/usr/bin/env bash
set -Eeuo pipefail

DATA_DIR="${VOXYTOK_DATA_DIR:-/home/container/data}"
PGDATA="${POSTGRES_DATA_DIR:-$DATA_DIR/postgres}"
mkdir -p "$DATA_DIR" "$PGDATA" /var/run/postgresql /var/log/postgresql
chown -R postgres:postgres "$PGDATA" /var/run/postgresql /var/log/postgresql

if [[ ! -s "$PGDATA/PG_VERSION" ]]; then
  gosu postgres initdb -D "$PGDATA" --auth-local=trust --auth-host=scram-sha-256
  gosu postgres pg_ctl -D "$PGDATA" -o "-c listen_addresses=127.0.0.1 -p ${POSTGRES_PORT:-5432}" -w start
  gosu postgres psql -v ON_ERROR_STOP=1 --username postgres <<SQL
CREATE USER "$POSTGRES_USER" WITH PASSWORD '$POSTGRES_PASSWORD';
CREATE DATABASE "$POSTGRES_DB" OWNER "$POSTGRES_USER";
SQL
  gosu postgres pg_ctl -D "$PGDATA" -m fast -w stop
fi

gosu postgres pg_ctl -D "$PGDATA" -o "-c listen_addresses=127.0.0.1 -p ${POSTGRES_PORT:-5432}" -w start
cleanup() {
  gosu postgres pg_ctl -D "$PGDATA" -m fast -w stop || true
}
trap cleanup EXIT TERM INT

export DATABASE_URL="postgresql://${POSTGRES_USER}:${POSTGRES_PASSWORD}@127.0.0.1:${POSTGRES_PORT:-5432}/${POSTGRES_DB}"
exec gosu voxytok /usr/local/bin/voxytok
