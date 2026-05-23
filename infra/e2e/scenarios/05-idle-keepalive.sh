#!/usr/bin/env bash
# 05-idle-keepalive — hold a WebSocket open for 90 seconds with no
# data flowing. Verifies the gateway's 30s ping / 60s timeout
# keepalive (per spike 03's idle-detection finding) keeps the
# session alive past what TCP keepalive defaults would allow.
source "$(dirname "$0")/common.sh"

require_stack_up
SB=$(create_sandbox)
trap "delete_sandbox $SB" EXIT
log "created sandbox $SB"
wait_for_running "$SB"
wait_for_routing

# `sleep 90` produces no output and the WS is idle for the full 90s.
# If the keepalive ping/pong is working, we read for 90s cleanly,
# then exit on the read deadline.
START=$(date +%s)
log "opening idle WS for 90s (no traffic expected)"
"$OPENSB_EXEC" --base "$WS_BASE" --sandbox "$SB" --api-key "$API_KEY" \
  --read-for-secs 90 \
  -- sleep 120 >/tmp/05-idle.log 2>&1
EXIT=$?
END=$(date +%s)
ELAPSED=$((END - START))
log "session ended exit=$EXIT after ${ELAPSED}s"

if [ "$ELAPSED" -lt 85 ]; then
  fail "session ended early at ${ELAPSED}s (expected ~90s); WS likely got torn down"
fi
if [ "$EXIT" -ne 0 ]; then
  fail "client exited non-zero ($EXIT); log: $(cat /tmp/05-idle.log)"
fi
pass "WS survived 90s idle; gateway keepalive ping/pong working"
