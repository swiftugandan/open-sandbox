#!/usr/bin/env bash
# 09-ws-read-file — verify the streaming WS file-read endpoint.
#
# Writes a known payload to /tmp/09-payload.bin via the unary
# POST /files/write_file endpoint, then reads it back via the
# new WS endpoint:
#
#   GET ws://gateway/v1/sandboxes/{id}/files/read?path=...
#
# Server emits raw file bytes as WS Binary frames; closes with
# code 1000 on EOF or a 44xx-range code on failure.
#
# Asserts: payload comes back byte-exact and large enough to
# require multiple chunks (so we actually exercise the chunking
# path on the agent side).
source "$(dirname "$0")/common.sh"

require_stack_up
SB=$(create_sandbox)
trap "delete_sandbox $SB" EXIT
log "created sandbox $SB"
wait_for_running "$SB"
wait_for_routing

# Build a 256 KiB payload — bigger than the 64 KiB chunk size,
# so the agent must emit ≥ 4 stdout frames and the gateway must
# stream them back without coalescing or truncating.
PAYLOAD=/tmp/09-payload.bin
GOT=/tmp/09-got.bin
dd if=/dev/urandom of="$PAYLOAD" bs=1024 count=256 status=none

log "writing 256 KiB payload to /tmp/09-payload.bin via POST /files/write_file"
curl -fsS -X POST "$API_BASE/v1/sandboxes/$SB/files/write_file" \
  -H "Authorization: Bearer $API_KEY" \
  -H 'content-type: application/json' \
  --data-binary @- <<EOF >/dev/null
{
  "path": "/tmp/09-payload.bin",
  "content_b64": "$(base64 -w0 < "$PAYLOAD" 2>/dev/null || base64 < "$PAYLOAD" | tr -d '\n')"
}
EOF

log "streaming back via WS /files/read"
# The example binary lives in the ws-client crate. Build with
# --release for parity with the rest of the e2e harness.
EXAMPLE_BIN="$(cd "$(dirname "$0")/../../.." && pwd)/target/release/examples/stream-read-file"
if [ ! -x "$EXAMPLE_BIN" ]; then
  cargo build --release -p open-sandbox-ws-client --example stream-read-file >/dev/null 2>&1
fi
set +e
"$EXAMPLE_BIN" \
  --base "$WS_BASE" --sandbox "$SB" --api-key "$API_KEY" \
  --path /tmp/09-payload.bin >"$GOT" 2>/tmp/09-stderr
EXIT=$?
set -e
if [ "$EXIT" -ne 0 ]; then
  fail "stream-read-file exited $EXIT; stderr: $(cat /tmp/09-stderr)"
fi

WANT_SHA=$(shasum -a 256 "$PAYLOAD" | awk '{print $1}')
GOT_SHA=$(shasum -a 256 "$GOT"     | awk '{print $1}')
if [ "$WANT_SHA" != "$GOT_SHA" ]; then
  fail "payload mismatch: want=$WANT_SHA got=$GOT_SHA"
fi

pass "WS /files/read streamed 256 KiB intact (sha256=$GOT_SHA)"
