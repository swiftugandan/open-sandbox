#!/usr/bin/env bash
# 08-write-then-exec — POST a script via /files/write_file, then
# exec it via the WebSocket and verify the output round-trips.
# Tests:
#   - REST POST /files/write_file goes through proxy OpenIoStream
#     with WriteFile params (the runtime's first-class write_file)
#   - The just-written file is observable from a subsequent exec
#     session
#   - Atomic write (temp + rename) — file is fully readable
source "$(dirname "$0")/common.sh"

require_stack_up
SB=$(create_sandbox)
trap "delete_sandbox $SB" EXIT
log "created sandbox $SB"
wait_for_running "$SB"
wait_for_routing

log "writing /home/hello.sh via POST /files/write_file"
WRITE_RESP=$(curl -fsS -X POST "$API_BASE/v1/sandboxes/$SB/files/write_file" \
  -H "Authorization: Bearer $API_KEY" \
  -H 'content-type: application/json' \
  -d '{"path":"hello.sh","cwd":"/home","content":"#!/bin/sh\necho written-then-exec-ok\n"}')
echo "$WRITE_RESP" | grep -q '"success":true' || fail "write_file did not return success: $WRITE_RESP"

log "exec'ing /home/hello.sh via WebSocket"
OUT=$("$OPENSB_EXEC" --base "$WS_BASE" --sandbox "$SB" --api-key "$API_KEY" \
      -- sh /home/hello.sh 2>&1)
echo "$OUT" | grep -q '^written-then-exec-ok$' \
  || fail "expected 'written-then-exec-ok' from exec'd script, got:\n$OUT"
pass "write_file + exec round-trip works"
