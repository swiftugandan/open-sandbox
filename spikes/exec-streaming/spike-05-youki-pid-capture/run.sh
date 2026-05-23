#!/usr/bin/env bash
# Spike 05 — measure the youki PID-capture race window on Linux via
# a privileged docker container with PID=host.
#
# Spawns nsenter into a target container's namespaces 100 times,
# polls /proc/<nsenter_pid>/task/*/children, records elapsed
# microseconds. Reports stats for tight-loop and 10ms-poll strategies.

set -euo pipefail

TARGET=spike05-target
RUNNER=spike05-runner
SCRIPT_DIR=$(cd "$(dirname "$0")" && pwd)

cleanup() {
  docker rm -f "$TARGET" "$RUNNER" >/dev/null 2>&1 || true
}
trap cleanup EXIT

cleanup

echo "[spike05] starting target container"
docker run -d --rm --name "$TARGET" alpine sleep infinity >/dev/null
TARGET_PID=$(docker inspect -f '{{.State.Pid}}' "$TARGET")
echo "[spike05] target host PID = $TARGET_PID"

echo "[spike05] running measurement (privileged, pid=host, alpine + util-linux + python3)"
docker run --rm \
  --name "$RUNNER" \
  --privileged \
  --pid=host \
  -v "$SCRIPT_DIR/measure.py:/measure.py:ro" \
  alpine \
  sh -c "apk add --no-cache python3 util-linux >/dev/null 2>&1 && python3 /measure.py $TARGET_PID"
