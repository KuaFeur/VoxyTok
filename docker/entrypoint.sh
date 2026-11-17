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

# Keep SQL identifiers and the libpq URL safe; generated secrets should use
# letters, digits, underscores and hyphens.
for identifier in "$POSTGRES_DB" "$POSTGRES_USER"; do
  if [[ ! "$identifier" =~ ^[A-Za-z_][A-Za-z0-9_]{0,62}$ ]]; then
    echo "POSTGRES_DB and POSTGRES_USER must be valid PostgreSQL identifiers." >&2
    exit 1
  fi
done
if [[ ! "$POSTGRES_PASSWORD" =~ ^[A-Za-z0-9_-]{24,128}$ ]]; then
  echo "POSTGRES_PASSWORD must be 24-128 URL-safe characters (letters, digits, _ or -)." >&2
  exit 1
fi
if [[ ! "$POSTGRES_PORT" =~ ^[0-9]{1,5}$ ]] || (( POSTGRES_PORT < 1 || POSTGRES_PORT > 65535 )); then
  echo "POSTGRES_PORT must be between 1 and 65535." >&2
  exit 1
fi

mkdir -p "$DATA_DIR" "$PGDATA" /var/run/postgresql /var/log/postgresql
chown -R postgres:postgres "$PGDATA" /var/run/postgresql /var/log/postgresql

# Debian stores server binaries in a versioned directory, not on PATH.
PG_BIN="$(find /usr/lib/postgresql -mindepth 3 -maxdepth 3 -type f -name initdb -printf '%h\n' | sort -V | tail -n 1)"
if [[ -z "$PG_BIN" ]]; then
  echo "Could not locate PostgreSQL server binaries." >&2
  exit 1
fi

if [[ ! -s "$PGDATA/PG_VERSION" ]]; then
  gosu postgres "$PG_BIN/initdb" -D "$PGDATA" --auth-local=trust --auth-host=scram-sha-256
  gosu postgres "$PG_BIN/pg_ctl" -D "$PGDATA" -o "-c listen_addresses=127.0.0.1 -p $POSTGRES_PORT" -w start
  gosu postgres "$PG_BIN/psql" -v ON_ERROR_STOP=1 --username postgres <<SQL
CREATE USER "$POSTGRES_USER" WITH PASSWORD '$POSTGRES_PASSWORD';
CREATE DATABASE "$POSTGRES_DB" OWNER "$POSTGRES_USER";
SQL
  gosu postgres "$PG_BIN/pg_ctl" -D "$PGDATA" -m fast -w stop
fi

gosu postgres "$PG_BIN/pg_ctl" -D "$PGDATA" -o "-c listen_addresses=127.0.0.1 -p $POSTGRES_PORT" -w start
app_pid=""
cleanup() {
  if [[ -n "$app_pid" ]] && kill -0 "$app_pid" 2>/dev/null; then
    kill -TERM "$app_pid" 2>/dev/null || true
    wait "$app_pid" 2>/dev/null || true
  fi
  gosu postgres "$PG_BIN/pg_ctl" -D "$PGDATA" -m fast -w stop || true
}
trap cleanup EXIT TERM INT

export DATABASE_URL="postgresql://$POSTGRES_USER:$POSTGRES_PASSWORD@127.0.0.1:$POSTGRES_PORT/$POSTGRES_DB"
gosu voxytok /usr/local/bin/voxytok &
app_pid=$!
wait "$app_pid"
