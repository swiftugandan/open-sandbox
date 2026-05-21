#!/bin/bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
COMPOSE_FILE="$SCRIPT_DIR/docker-compose.yml"

cleanup() {
  echo "Tearing down..."
  docker compose -f "$COMPOSE_FILE" down -v --remove-orphans 2>/dev/null || true
}
trap cleanup EXIT

echo "=== open-sandbox infra e2e-mock ==="
echo "Topology: postgres(10.0.0.2) + controller(10.0.0.3) + proxy(10.0.0.4) + agent(10.0.0.10)"
echo ""

echo "[1/4] Building and starting services..."
docker compose -f "$COMPOSE_FILE" up -d --build --wait --wait-timeout 120

echo "[2/4] Verifying all services are running..."
for svc in postgres controller proxy agent; do
  status=$(docker compose -f "$COMPOSE_FILE" ps --format json "$svc" 2>/dev/null | python3 -c "import sys,json; print(json.load(sys.stdin)['State'])" 2>/dev/null || echo "unknown")
  if [ "$status" != "running" ]; then
    echo "FAIL: $svc is not running (state: $status)"
    docker compose -f "$COMPOSE_FILE" logs "$svc" 2>&1 | tail -20
    exit 1
  fi
  echo "  $svc: running"
done

echo "[3/4] Waiting for agent registration in Postgres..."
for i in $(seq 1 30); do
  count=$(docker compose -f "$COMPOSE_FILE" exec -T postgres \
    psql -U postgres -d open_sandbox -tAc "SELECT count(*) FROM agents" 2>/dev/null | tr -d '[:space:]' || echo "0")
  if [ "$count" -ge 1 ]; then
    echo "  Agent registered ($count agent(s) in database)"
    break
  fi
  if [ "$i" -eq 30 ]; then
    echo "FAIL: Agent did not register within 30 seconds"
    echo "--- controller logs ---"
    docker compose -f "$COMPOSE_FILE" logs controller 2>&1 | tail -30
    echo "--- agent logs ---"
    docker compose -f "$COMPOSE_FILE" logs agent 2>&1 | tail -30
    exit 1
  fi
  sleep 1
done

echo "[4/4] Verifying agent state is active..."
state=$(docker compose -f "$COMPOSE_FILE" exec -T postgres \
  psql -U postgres -d open_sandbox -tAc "SELECT state FROM agents LIMIT 1" 2>/dev/null | tr -d '[:space:]')
if [ "$state" != "active" ]; then
  echo "FAIL: Agent state is '$state', expected 'active'"
  exit 1
fi
echo "  Agent state: active"

echo ""
echo "=== E2E MOCK PASSED ==="
echo "The Pulumi topology (private network, ports, env vars, token flow) produces a working system."
