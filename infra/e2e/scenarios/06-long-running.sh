#!/usr/bin/env bash
# 06-long-running — run a process that takes >60 seconds. Validates
# the v0.x EXEC_TIMEOUT ceiling is gone in v1.0 (sessions live as
# long as the WebSocket).
source "$(dirname "$0")/common.sh"

DURATION_SECS=70  # past the old 60s ceiling

require_stack_up
SB=$(create_sandbox)
trap "delete_sandbox $SB" EXIT
log "created sandbox $SB"
wait_for_running "$SB"
wait_for_routing

START=$(date +%s)
log "running 'sleep ${DURATION_SECS}' (past old 60s EXEC_TIMEOUT)"
"$OPENSB_EXEC" --base "$WS_BASE" --sandbox "$SB" --api-key "$API_KEY" \
  -- sh -c "sleep ${DURATION_SECS}; echo DONE_AT_$(date +%s)" \
  >/tmp/06-long.log 2>&1
EXIT=$?
END=$(date +%s)
ELAPSED=$((END - START))
log "exited $EXIT after ${ELAPSED}s"

if [ "$EXIT" -ne 0 ]; then
  fail "exec failed: $(cat /tmp/06-long.log)"
fi
if [ "$ELAPSED" -lt $((DURATION_SECS - 5)) ]; then
  fail "exec terminated early at ${ELAPSED}s — old EXEC_TIMEOUT may still be present"
fi
grep -q "^DONE_AT_" /tmp/06-long.log || fail "no DONE_AT_ marker in output: $(cat /tmp/06-long.log)"
pass "long-running exec completed normally after ${ELAPSED}s"
