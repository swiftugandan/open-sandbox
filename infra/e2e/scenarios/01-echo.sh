#!/usr/bin/env bash
# 01-echo — basic round-trip: `echo hello-streaming-exec` over the
# v1.0 WebSocket exec path. Verifies the whole chain (WS client →
# axum gateway → tonic OpenIoStream → proxy router → agent tunnel →
# runtime → bash → IoServerFrame → WS).
source "$(dirname "$0")/common.sh"

require_stack_up
SB=$(create_sandbox)
trap "delete_sandbox $SB" EXIT
log "created sandbox $SB"
wait_for_running "$SB"
wait_for_routing

OUT=$("$OPENSB_EXEC" --base "$WS_BASE" --sandbox "$SB" --api-key "$API_KEY" \
      -- echo hello-streaming-exec 2>&1)
echo "$OUT" | grep -q '^hello-streaming-exec$' || fail "expected 'hello-streaming-exec' in stdout, got: $OUT"
pass
