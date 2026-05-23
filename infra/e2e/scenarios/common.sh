# Shared helpers for the e2e scenarios.
# Source from each scenario script.

set -euo pipefail

# Configurable knobs (env-overridable).
API_HOST="${API_HOST:-127.0.0.1}"
API_PORT="${API_PORT:-18081}"
API_KEY="${API_KEY:-e2e-api-key}"
SANDBOX_IMAGE="${SANDBOX_IMAGE:-alpine:3.21}"
# The proxy's routing cache now falls back to a per-lookup DB hit
# on miss (RoutingCache::lookup_or_fetch), so freshly-created
# sandboxes are routable immediately. Default to no wait; the knob
# is kept for reproducing the legacy polling-only behavior.
ROUTING_REFRESH_SECS="${ROUTING_REFRESH_SECS:-0}"

API_BASE="http://${API_HOST}:${API_PORT}"
WS_BASE="ws://${API_HOST}:${API_PORT}"

# Path to the opensandbox-exec binary built by the workspace.
OPENSB_EXEC="${OPENSB_EXEC:-$(cd "$(dirname "$0")/../../.." && pwd)/target/release/opensandbox-exec}"

scenario_name() {
  basename "$0" .sh
}

log() {
  echo "[$(scenario_name)] $*"
}

require_stack_up() {
  if ! curl -fsS -H "Authorization: Bearer $API_KEY" "$API_BASE/v1/sandboxes" >/dev/null 2>&1; then
    echo "[$(scenario_name)] FAIL: API not reachable at $API_BASE (is docker compose up?)"
    exit 1
  fi
}

create_sandbox() {
  local resp
  resp=$(curl -fsS -X POST "$API_BASE/v1/sandboxes" \
    -H "Authorization: Bearer $API_KEY" \
    -H 'content-type: application/json' \
    -d "{\"image\":\"$SANDBOX_IMAGE\"}")
  echo "$resp" | python3 -c 'import sys,json;print(json.load(sys.stdin)["sandbox_id"])'
}

wait_for_running() {
  local sb=$1
  local attempts=${2:-15}
  for _ in $(seq 1 "$attempts"); do
    local state
    state=$(curl -fsS -H "Authorization: Bearer $API_KEY" "$API_BASE/v1/sandboxes/$sb" \
            | python3 -c 'import sys,json;print(json.load(sys.stdin)["status"])')
    if [ "$state" = "running" ]; then
      return 0
    fi
    if [ "$state" = "failed" ]; then
      echo "[$(scenario_name)] FAIL: sandbox $sb entered 'failed' state"
      return 1
    fi
    sleep 2
  done
  echo "[$(scenario_name)] FAIL: sandbox $sb did not reach 'running' within $((attempts*2))s"
  return 1
}

# Wait for the proxy's routing cache to pick up newly-created
# sandboxes. With RoutingCache::lookup_or_fetch the cache resolves
# misses against the DB on the hot path, so the default is no
# wait. A positive ROUTING_REFRESH_SECS reproduces legacy polling
# behavior for back-pressure testing.
wait_for_routing() {
  if [ "$ROUTING_REFRESH_SECS" -gt 0 ]; then
    log "waiting ${ROUTING_REFRESH_SECS}s for routing cache to refresh"
    sleep "$ROUTING_REFRESH_SECS"
  fi
}

delete_sandbox() {
  local sb=$1
  curl -fsS -X DELETE "$API_BASE/v1/sandboxes/$sb" \
    -H "Authorization: Bearer $API_KEY" >/dev/null 2>&1 || true
}

pass() {
  echo "[$(scenario_name)] PASS${1:+: $1}"
  exit 0
}

fail() {
  echo "[$(scenario_name)] FAIL: $1"
  exit 1
}
