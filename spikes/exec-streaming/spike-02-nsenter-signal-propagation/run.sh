#!/usr/bin/env bash
# Spike 02 — When the host-side `nsenter` process is killed, does its
# in-namespace child process die or survive as an orphan?
#
# Background:
#   nsenter setns()'s into target namespaces, then fork()s a child and
#   exec()s the target command in the child. The parent waits. Both
#   processes are host-PID-visible during the exec.
#
# In the youki backend (crates/agent-youki/src/exec.rs) we spawn nsenter
# as a child of the agent via std::process::Command. Killing that child
# from Rust will SIGKILL the nsenter parent — the question is whether
# the actual in-container command dies with it.
#
# Method:
#   - Inside a privileged Linux container (so we can use nsenter against
#     another running container's namespaces), launch
#     `nsenter -t <target_pid> --mount --uts --ipc --net --pid --
#     sh -c 'sleep 30; touch /tmp/marker'`
#   - SIGKILL the nsenter process at 2s.
#   - Wait 35s. Look for the marker inside the target container.
#
#   marker EXISTS  -> in-namespace child survived nsenter death (orphaned
#                     to host PID 1, kept running). Agent must explicitly
#                     kill it.
#   marker MISSING -> in-namespace child died with nsenter. Disconnect-
#                     kills-process is free for youki.

set -euo pipefail

TARGET=spike02-target
RUNNER=spike02-runner
MARKER=/tmp/spike02-marker
TASK_DURATION=15
KILL_AFTER=2
WAIT_PAST_TASK=18

cleanup() {
  docker rm -f "$TARGET" "$RUNNER" >/dev/null 2>&1 || true
}
trap cleanup EXIT

cleanup
echo "[spike02] starting target container (sandbox under test)"
docker run -d --rm --name "$TARGET" alpine sleep infinity >/dev/null
TARGET_PID=$(docker inspect -f '{{.State.Pid}}' "$TARGET")
echo "[spike02] target container PID on host = $TARGET_PID"

# Use a Linux container with PID host so it can see all PIDs and call
# nsenter against the target. --privileged is needed for the namespace
# manipulation.
echo "[spike02] launching nsenter-runner (privileged, pid=host)"
docker run -d --rm --name "$RUNNER" --privileged --pid=host alpine \
  sh -c "apk add --no-cache util-linux >/dev/null 2>&1; \
         nsenter -t $TARGET_PID --mount --uts --ipc --net --pid -- \
         sh -c 'sleep $TASK_DURATION; echo done > $MARKER' & \
         NS_PID=\$!; \
         echo nsenter_pid=\$NS_PID > /tmp/spike02.info; \
         sleep $((KILL_AFTER + WAIT_PAST_TASK + 5))" >/dev/null

# Give the runner a moment to install util-linux and start nsenter.
echo "[spike02] waiting 6s for runner to spawn nsenter (apk install + launch)"
sleep 6

NS_INFO=$(docker exec "$RUNNER" cat /tmp/spike02.info 2>/dev/null || echo "")
NS_PID=$(echo "$NS_INFO" | sed -n 's/nsenter_pid=//p')
echo "[spike02] nsenter pid (inside runner's pid=host view) = $NS_PID"

if [ -z "$NS_PID" ]; then
  echo "[spike02] ERROR: failed to capture nsenter pid"
  exit 1
fi

echo "[spike02] SIGKILLing nsenter pid=$NS_PID from inside the runner"
docker exec "$RUNNER" kill -9 "$NS_PID" 2>&1 || true

echo "[spike02] waiting ${WAIT_PAST_TASK}s for in-namespace task to complete (or not)..."
sleep "$WAIT_PAST_TASK"

echo "[spike02] checking marker inside target container:"
if docker exec "$TARGET" test -f "$MARKER"; then
  CONTENT=$(docker exec "$TARGET" cat "$MARKER")
  echo "[spike02] RESULT: marker EXISTS — content=\"$CONTENT\""
  echo "[spike02] CONCLUSION: in-namespace child SURVIVED nsenter SIGKILL."
  echo "[spike02] Agent must explicitly kill the in-namespace PID when its stream closes."
else
  echo "[spike02] RESULT: marker MISSING"
  echo "[spike02] CONCLUSION: in-namespace child died with nsenter."
  echo "[spike02] Disconnect-kills-process is free for the youki backend."
fi
