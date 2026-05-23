#!/usr/bin/env bash
# Runs every numbered scenario script in this directory against
# the running docker-compose.full stack and reports PASS/FAIL.
#
# Usage:
#   docker compose -f infra/e2e/docker-compose.full.yml up -d
#   cargo build --release -p open-sandbox-ws-client
#   infra/e2e/scenarios/run-all.sh
#
# Exit code = number of failing scenarios.

set -u
HERE="$(cd "$(dirname "$0")" && pwd)"

scenarios=$(find "$HERE" -maxdepth 1 -name '[0-9][0-9]-*.sh' -type f | sort)
total=0
passed=0
failed_names=()

for s in $scenarios; do
  total=$((total + 1))
  name=$(basename "$s" .sh)
  echo
  echo "=== $name ==="
  if bash "$s"; then
    passed=$((passed + 1))
  else
    failed_names+=("$name")
  fi
done

echo
echo "============================================"
echo "Scenarios: $passed/$total passed"
if [ "${#failed_names[@]}" -gt 0 ]; then
  echo "Failed: ${failed_names[*]}"
fi
echo "============================================"

[ "$passed" -eq "$total" ]
