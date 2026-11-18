#!/usr/bin/env bash
set -Eeuo pipefail

DATA_DIR="${VOXYTOK_DATA_DIR:-/home/container/data}"
PGDATA="${POSTGRES_DATA_DIR:-$DATA_DIR/postgres}"
PG_RUN_DIR="$PGDATA/run"
PG_BIN="${PG_BIN:-$(find /usr/lib/postgresql -type f -name initdb -printf '%h\n' 2>/dev/null | sort -V | tail -n1)}"
DB_NAME="${POSTGRES_DB:-voxytok}"
DB_USER="${POSTGRES_USER:-voxytok}"
DB_PORT="${POSTGRES_PORT:-5432}"

# Pelican/Wings may run the container as an unprivileged UID. Only use the
# postgres system account when this entrypoint is actually running as root.
run_pg() {
  if [[ "$(id -u)" -eq 0 ]]; then
    gosu postgres "$@"
  else
    "$@"
  fi
}

if [[ -z "$PG_BIN" || ! -x "$PG_BIN/initdb" ]]; then
  echo "[VoxyTok] PostgreSQL server binaries not found." >&2
  exit 1
fi
for identifier in "$DB_NAME" "$DB_USER"; do
  if [[ ! "$identifier" =~ ^[A-Za-z_][A-Za-z0-9_]{0,62}$ ]]; then
    echo "[VoxyTok] POSTGRES_DB and POSTGRES_USER must be valid PostgreSQL identifiers." >&2
    exit 1
  fi
done
if [[ ! "$DB_PORT" =~ ^[0-9]{1,5}$ ]] || (( DB_PORT < 1 || DB_PORT > 65535 )); then
  echo "[VoxyTok] POSTGRES_PORT must be between 1 and 65535." >&2
  exit 1
fi
if [[ -z "${POSTGRES_PASSWORD:-}" || ! "$POSTGRES_PASSWORD" =~ ^[A-Za-z0-9_-]{24,128}$ ]]; then
  echo "[VoxyTok] POSTGRES_PASSWORD must be 24-128 URL-safe characters." >&2
  exit 1
fi

mkdir -p "$DATA_DIR" "$PGDATA"
chmod 0700 "$PGDATA"
if [[ "$(id -u)" -eq 0 ]]; then
  chown -R postgres:postgres "$PGDATA"
elif [[ ! -w "$PGDATA" ]]; then
  echo "[VoxyTok] Data directory is not writable by container UID $(id -u). Check Pelican volume permissions." >&2
  exit 1
fi

if [[ ! -s "$PGDATA/PG_VERSION" ]]; then
  echo "[VoxyTok] Initializing embedded PostgreSQL..."
  run_pg "$PG_BIN/initdb" -D "$PGDATA" --username=postgres --encoding=UTF8 --auth-local=trust --auth-host=trust
fi

# Keep the Unix socket inside the persistent PostgreSQL directory so both
# root-run and unprivileged Pelican containers can manage it without /run chown.
run_pg mkdir -p "$PG_RUN_DIR"
run_pg chmod 0700 "$PG_RUN_DIR"

export PGUSER=postgres
export PGHOST=127.0.0.1
echo "[VoxyTok] Starting embedded PostgreSQL..."
run_pg "$PG_BIN/pg_ctl" -D "$PGDATA" \
  -o "-c listen_addresses=127.0.0.1 -c unix_socket_directories=$PG_RUN_DIR -c port=$DB_PORT" \
  -w start

cleanup() {
  local status=${1:-0}
  trap - TERM INT
  if [[ -n "${APP_PID:-}" ]] && kill -0 "$APP_PID" 2>/dev/null; then
    kill -TERM "$APP_PID" 2>/dev/null || true
    wait "$APP_PID" || true
  fi
  echo "[VoxyTok] Stopping embedded PostgreSQL..."
  run_pg "$PG_BIN/pg_ctl" -D "$PGDATA" -m fast -w stop || true
  exit "$status"
}
trap 'cleanup 0' TERM INT

if run_pg "$PG_BIN/psql" -v ON_ERROR_STOP=1 --dbname=postgres -tAc "SELECT 1 FROM pg_roles WHERE rolname = '$DB_USER'" | grep -q '^1$'; then
  run_pg "$PG_BIN/psql" -v ON_ERROR_STOP=1 --dbname=postgres -c "ALTER ROLE \"$DB_USER\" WITH LOGIN PASSWORD '$POSTGRES_PASSWORD'"
else
  run_pg "$PG_BIN/psql" -v ON_ERROR_STOP=1 --dbname=postgres -c "CREATE ROLE \"$DB_USER\" WITH LOGIN PASSWORD '$POSTGRES_PASSWORD'"
fi

if ! run_pg "$PG_BIN/psql" -v ON_ERROR_STOP=1 --dbname=postgres -tAc "SELECT 1 FROM pg_database WHERE datname = '$DB_NAME'" | grep -q '^1$'; then
  run_pg "$PG_BIN/createdb" --owner="$DB_USER" "$DB_NAME"
fi

export DATABASE_URL="postgresql://$DB_USER:$POSTGRES_PASSWORD@127.0.0.1:$DB_PORT/$DB_NAME"
export PORT="${PORT:-8080}"
export RUST_LOG="${RUST_LOG:-voxytok=info}"

echo "[VoxyTok] Starting Rust service on port $PORT..."
if [[ "$(id -u)" -eq 0 ]]; then
  gosu voxytok /usr/local/bin/voxytok &
else
  /usr/local/bin/voxytok &
fi
APP_PID=$!
set +e
wait "$APP_PID"
APP_STATUS=$?
set -e
cleanup "$APP_STATUS"
