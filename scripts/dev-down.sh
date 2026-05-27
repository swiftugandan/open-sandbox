#!/usr/bin/env bash
# scripts/dev-down.sh — tear down the open-sandbox dev fleet.
#
# Phase 0 of docs/plans/PLAN_DEV_MODE.md. Kills any running service
# processes and stops the managed postgres container. The postgres
# volume is preserved unless --reset is passed.

set -uo pipefail

PG_CONTAINER="open-sandbox-dev-pg"
PG_VOLUME="open-sandbox-dev-pg-data"

echo "==> stopping open-sandbox service processes"
pkill -TERM -f 'target/release/open-sandbox' 2>/dev/null || true
sleep 1
pkill -KILL -f 'target/release/open-sandbox' 2>/dev/null || true

if docker inspect "$PG_CONTAINER" >/dev/null 2>&1; then
  echo "==> stopping postgres ($PG_CONTAINER)"
  docker stop "$PG_CONTAINER" >/dev/null || true
fi

if [[ "${1:-}" == "--reset" ]]; then
  echo "==> --reset: removing postgres container + volume"
  docker rm "$PG_CONTAINER" >/dev/null 2>&1 || true
  docker volume rm "$PG_VOLUME" >/dev/null 2>&1 || true
fi

echo "==> done"
