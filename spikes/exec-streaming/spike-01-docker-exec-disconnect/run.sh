#!/usr/bin/env bash
# Spike 01 — Does docker exec kill the in-container process when the
# attached client is killed?
#
# Setup:
#   - Run alpine container with `sleep infinity` as PID 1.
#   - docker exec -i a long task that touches a marker file at the end.
#   - Kill the local docker client 2s in (before the task completes).
#   - Wait past the task's natural completion time.
#   - Inspect /tmp/marker inside the container.
#
# Interpretation:
#   - marker EXISTS  -> docker did NOT kill the exec; client kill does not
#                       propagate. We must explicitly kill the PID on
#                       disconnect in the agent.
#   - marker MISSING -> docker DID kill the exec; the disconnect-kills-
#                       process property is available for free.

set -euo pipefail

CONTAINER=spike01-exec-disconnect
MARKER=/tmp/spike01-marker
TASK_DURATION=15
KILL_AFTER=2
WAIT_PAST_TASK=18

cleanup() {
  docker rm -f "$CONTAINER" >/dev/null 2>&1 || true
}
trap cleanup EXIT

cleanup
echo "[spike01] starting alpine sandbox"
docker run -d --rm --name "$CONTAINER" alpine sleep infinity >/dev/null

echo "[spike01] launching long exec via docker exec -i (background)"
# The shell pipeline removes any chance the inner sleep is killed by signals
# arriving via stdin — we are testing what dockerd does when the *client*
# socket goes away, not what the kernel does.
( docker exec -i "$CONTAINER" sh -c "sleep $TASK_DURATION; echo done > $MARKER" </dev/null & echo $! > /tmp/spike01.client.pid ) &
WRAPPER_PID=$!

sleep "$KILL_AFTER"

CLIENT_PID=$(cat /tmp/spike01.client.pid 2>/dev/null || echo "")
echo "[spike01] killing local docker client pid=$CLIENT_PID (wrapper=$WRAPPER_PID) after ${KILL_AFTER}s"
if [ -n "$CLIENT_PID" ]; then
  kill -9 "$CLIENT_PID" 2>/dev/null || true
fi
kill -9 "$WRAPPER_PID" 2>/dev/null || true

echo "[spike01] waiting ${WAIT_PAST_TASK}s for in-container task to complete (or not)..."
sleep "$WAIT_PAST_TASK"

echo "[spike01] checking marker inside container:"
if docker exec "$CONTAINER" test -f "$MARKER"; then
  CONTENT=$(docker exec "$CONTAINER" cat "$MARKER")
  echo "[spike01] RESULT: marker EXISTS — content=\"$CONTENT\""
  echo "[spike01] CONCLUSION: docker exec process SURVIVED client disconnect."
  echo "[spike01] Agent must explicitly kill the exec PID when its stream closes."
  exit 0
else
  echo "[spike01] RESULT: marker MISSING"
  echo "[spike01] CONCLUSION: dockerd KILLED the exec process when the client went away."
  echo "[spike01] Disconnect-kills-process is free for the docker backend."
  exit 0
fi
