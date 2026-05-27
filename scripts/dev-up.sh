#!/usr/bin/env bash
# scripts/dev-up.sh — bring up the open-sandbox dev fleet in one command.
#
# Phase 0 of docs/plans/PLAN_DEV_MODE.md: a pure-shell wrapper around today's
# Quick Start preamble. Generates a stable dev env file on first run, brings
# up a managed postgres container, spawns all four services, and tails one
# combined log stream. Ctrl-C stops the services (postgres container is left
# running; ./scripts/dev-down.sh fully tears down).
#
# Knobs (all optional):
#   OPEN_SANDBOX_DEV_HOME   override ~/.open-sandbox
#   OPEN_SANDBOX_DEV_PG_PORT  override 15432
#   OPEN_SANDBOX_DEV_API_PORT override 8081
#
# Reset state:  ./scripts/dev-down.sh --reset  (deletes the postgres volume)

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

BIN="$ROOT/target/release/open-sandbox"
ENV_DIR="${OPEN_SANDBOX_DEV_HOME:-$HOME/.open-sandbox}"
ENV_FILE="$ENV_DIR/dev.env"
LOG_DIR="$ENV_DIR/logs"
PG_CONTAINER="open-sandbox-dev-pg"
PG_VOLUME="open-sandbox-dev-pg-data"
PG_PORT="${OPEN_SANDBOX_DEV_PG_PORT:-15432}"
API_PORT="${OPEN_SANDBOX_DEV_API_PORT:-8081}"

mkdir -p "$ENV_DIR" "$LOG_DIR"
chmod 700 "$ENV_DIR"

# ----------------------------------------------------------------- 1. tokens
gen_token() { openssl rand -hex 32; }

if [[ ! -f "$ENV_FILE" ]]; then
  echo "==> generating dev tokens at $ENV_FILE"
  umask 077
  cat > "$ENV_FILE" <<EOF
# open-sandbox dev tokens — auto-generated $(date -u +%Y-%m-%dT%H:%M:%SZ)
# Re-using these on every run keeps the API key stable across restarts.
OPEN_SANDBOX_JOIN_TOKEN=$(gen_token)
TUNNEL_JOIN_TOKEN=$(gen_token)
CONTROLLER_ADMIN_TOKEN=$(gen_token)
OPEN_SANDBOX_INTERNAL_TOKEN=$(gen_token)
OPEN_SANDBOX_API_KEY=$(gen_token)
OPEN_SANDBOX_DATABASE_URL=postgres://postgres:dev@127.0.0.1:${PG_PORT}/open_sandbox
OPEN_SANDBOX_API_CORS_ORIGINS=*
EOF
fi

# shellcheck disable=SC1090
set -a; source "$ENV_FILE"; set +a

DBURL="$OPEN_SANDBOX_DATABASE_URL"

# ----------------------------------------------------------------- 2. binary
# Always invoke cargo — it's a no-op when nothing changed (~250ms), and
# the previous `[[ ! -x BIN ]]` guard silently re-used stale binaries
# from before recent CLI changes (e.g. the `migrate` subcommand or
# new flags), which surfaced as confusing runtime errors much later.
echo "==> building release binary (no-op if up to date)"
cargo build --release --bin open-sandbox

# --------------------------------------------------------------- 3. postgres
if docker inspect "$PG_CONTAINER" >/dev/null 2>&1; then
  state=$(docker inspect -f '{{.State.Status}}' "$PG_CONTAINER")
else
  state=missing
fi
case "$state" in
  running)
    echo "==> postgres ($PG_CONTAINER) already running"
    ;;
  exited|created)
    echo "==> starting existing postgres ($PG_CONTAINER)"
    docker start "$PG_CONTAINER" >/dev/null
    ;;
  missing)
    echo "==> creating postgres ($PG_CONTAINER) on 127.0.0.1:${PG_PORT}"
    docker run -d --name "$PG_CONTAINER" \
      -e POSTGRES_DB=open_sandbox \
      -e POSTGRES_USER=postgres \
      -e POSTGRES_PASSWORD=dev \
      -v "${PG_VOLUME}:/var/lib/postgresql/data" \
      -p "127.0.0.1:${PG_PORT}:5432" \
      postgres:16-alpine >/dev/null
    ;;
  *)
    echo "!! unexpected postgres container state: $state" >&2
    exit 1
    ;;
esac

echo "==> waiting for postgres to accept connections"
for _ in $(seq 1 30); do
  if docker exec "$PG_CONTAINER" pg_isready -U postgres -d open_sandbox -q 2>/dev/null; then
    break
  fi
  sleep 1
done

# ---------------------------------------------------------------- 4. spawn
# The controller and proxy run migrations themselves via --auto-migrate
# (the README explicitly endorses this for dev environments — production
# uses a separate `open-sandbox migrate` step so a migration failure
# doesn't crash-loop the long-running services). Doing it this way
# avoids the redundant explicit migrate + the "skipping migrations
# (auto-migrate off)" log noise the services would otherwise emit.
LOG="$LOG_DIR/dev-$(date +%Y%m%d-%H%M%S).log"
ln -sf "$LOG" "$LOG_DIR/dev.log"
echo "==> log: $LOG  (symlinked from $LOG_DIR/dev.log)"

PIDS=()
spawn() {
  local label="$1"; shift
  # Process substitution (instead of `cmd | sed &` in a subshell) so
  # `$!` is the service PID directly, not a wrapper subshell PID
  # whose children we'd lose track of when we try to kill them.
  "$@" > >(sed -u "s/^/[$label] /" >> "$LOG") 2>&1 &
  PIDS+=($!)
}

cleanup() {
  trap - INT TERM EXIT
  echo
  echo "==> shutdown signal received — stopping services (SIGTERM, 5s grace, then SIGKILL)"
  # Kill the foreground tail first so its output doesn't race the trap.
  if [[ -n "${TAIL_PID:-}" ]]; then
    kill -TERM "$TAIL_PID" 2>/dev/null || true
  fi
  for pid in "${PIDS[@]:-}"; do
    kill -TERM "$pid" 2>/dev/null || true
  done
  # Poll up to 5s for graceful exit before escalating.
  for _ in $(seq 1 50); do
    local any_alive=0
    for pid in "${PIDS[@]:-}"; do
      if kill -0 "$pid" 2>/dev/null; then any_alive=1; break; fi
    done
    [[ "$any_alive" == "0" ]] && break
    sleep 0.1
  done
  for pid in "${PIDS[@]:-}"; do
    if kill -0 "$pid" 2>/dev/null; then
      echo "   (pid $pid did not exit on SIGTERM, sending SIGKILL)"
      kill -KILL "$pid" 2>/dev/null || true
    fi
  done
  wait 2>/dev/null || true
  echo "==> services stopped (postgres container left running; ./scripts/dev-down.sh to fully stop)"
}
trap cleanup INT TERM EXIT

# The api and agent both retry their upstream dials with exponential
# backoff (api: see connect_upstream_with_retry; agent: see
# crates/agent/src/reconnect.rs), so parallel spawn is safe.
spawn controller "$BIN" controller --database-url "$DBURL" --auto-migrate
spawn proxy      "$BIN" proxy      --database-url "$DBURL" --auto-migrate
spawn api        "$BIN" api \
                    --controller-url http://127.0.0.1:50051 \
                    --proxy-url      http://127.0.0.1:50053
spawn agent      "$BIN" agent \
                    --controller-url http://127.0.0.1:50051 \
                    --proxy-url      http://127.0.0.1:50052

# --------------------------------------------------------------- 5. healthz
echo "==> waiting for api on http://127.0.0.1:${API_PORT}/healthz"
ready=0
for _ in $(seq 1 30); do
  if curl -fsS "http://127.0.0.1:${API_PORT}/healthz" >/dev/null 2>&1; then
    ready=1
    break
  fi
  sleep 1
done
if [[ "$ready" != "1" ]]; then
  echo "!! api did not come up in 30s — see $LOG" >&2
fi

# ---------------------------------------------------------------- 6. banner
cat <<EOF

open-sandbox dev fleet ready
  API         http://127.0.0.1:${API_PORT}
  Postgres    127.0.0.1:${PG_PORT}        (docker container ${PG_CONTAINER})
  Env file    ${ENV_FILE}     (chmod 600 — sourced into this shell)
  Logs        ${LOG_DIR}/dev.log

  Create a sandbox:
    curl -X POST -H "Authorization: Bearer \$OPEN_SANDBOX_API_KEY" \\
         -H 'content-type: application/json' \\
         -d '{"image":"alpine:3.21"}' \\
         http://127.0.0.1:${API_PORT}/v1/sandboxes

  Stop:  Ctrl-C  (postgres volume preserved; ./scripts/dev-down.sh --reset to wipe)

EOF

# ----------------------------------------------------------------- 7. tail
# Background tail + wait, so bash returns from wait on SIGINT/SIGTERM
# and runs the cleanup trap. A foreground `tail -F` blocks the trap
# until tail itself exits (bash defers signal handlers until the
# current foreground command returns), which silently strands the
# services when the script is killed by anything other than Ctrl-C
# from the same controlling TTY.
tail -F "$LOG" &
TAIL_PID=$!
wait "$TAIL_PID" 2>/dev/null || true
