#!/usr/bin/env bash
# 03-signal-term — start a long-running process with a SIGTERM trap;
# from a second exec, send `kill -TERM <pid>` to the in-container
# PID; verify the first session exits with the trapped exit code.
#
# Tests: (a) the IoStarted frame surfaces the in-container PID,
#        (b) `docker exec <ctr> kill ...` (the agent runtime's
#            signal_exec path used by the ExecRegistry cleanup
#            hook) actually delivers signals,
#        (c) the trap'd process completes its handler and
#            propagates the chosen exit code.
source "$(dirname "$0")/common.sh"

require_stack_up
SB=$(create_sandbox)
trap "delete_sandbox $SB" EXIT
log "created sandbox $SB"
wait_for_running "$SB"
wait_for_routing

# Run the trap'd sleep, capture its in-container PID from the
# "# started" line that opensandbox-exec prints to stderr.
LOG=/tmp/03-signal.log
log "starting trap'd sleep (background)"
"$OPENSB_EXEC" --base "$WS_BASE" --sandbox "$SB" --api-key "$API_KEY" \
  -- sh -c 'trap "exit 143" TERM; sleep 30; echo NO_SIGNAL' \
  >"$LOG" 2>&1 &
WSPID=$!

# Wait for the "# started" line to land, then extract pid.
PID=""
for _ in $(seq 1 50); do
  if grep -q "^# started" "$LOG" 2>/dev/null; then
    PID=$(grep "^# started" "$LOG" | sed -E 's/.*pid=([0-9]+).*/\1/')
    break
  fi
  sleep 0.2
done
[ -n "$PID" ] || { kill -9 $WSPID 2>/dev/null; fail "did not see '# started' from first exec"; }
log "in-container PID = $PID"

# Second exec: kill -TERM the PID inside the same sandbox.
log "sending SIGTERM to in-container pid=$PID"
"$OPENSB_EXEC" --base "$WS_BASE" --sandbox "$SB" --api-key "$API_KEY" \
  -- kill -TERM "$PID" >/dev/null 2>&1

# The first exec should now exit 143 within a second or two.
set +e
wait $WSPID
EXIT=$?
set -e
log "first exec exited $EXIT"
if [ "$EXIT" -ne 143 ]; then
  fail "expected exit 143 (trapped SIGTERM), got $EXIT; log: $(cat $LOG)"
fi
if grep -q NO_SIGNAL "$LOG"; then
  fail "the trap did not interrupt the sleep (NO_SIGNAL printed)"
fi
pass "trapped SIGTERM exited 143 as expected"
