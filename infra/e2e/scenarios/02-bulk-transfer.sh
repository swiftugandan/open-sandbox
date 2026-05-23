#!/usr/bin/env bash
# 02-bulk-transfer — pipe ~10 MiB of stdout from the sandbox to the
# client and verify it arrives intact. Spike 03 already verified
# axum WS backpressure properties under slow consumers; this
# scenario validates the end-to-end chain handles real volume
# without dropping bytes.
source "$(dirname "$0")/common.sh"

BYTES=$((10 * 1024 * 1024))

require_stack_up
SB=$(create_sandbox)
trap "delete_sandbox $SB" EXIT
log "created sandbox $SB"
wait_for_running "$SB"
wait_for_routing

log "streaming ${BYTES} bytes from sandbox"
OUT=$("$OPENSB_EXEC" --base "$WS_BASE" --sandbox "$SB" --api-key "$API_KEY" \
      -- sh -c "head -c $BYTES /dev/zero" 2>/dev/null | wc -c)
log "client received: $OUT bytes (expected: $BYTES)"

# Allow a small tolerance for the diagnostic '# started ...' line
# at the start of the client output (it goes to stderr, not stdout,
# but be defensive — actually no, eprintln to stderr won't land in
# stdout, so the count should be exact).
if [ "$OUT" -ne "$BYTES" ]; then
  fail "byte count mismatch: got $OUT, expected $BYTES"
fi
pass "received $OUT bytes intact"
