#!/usr/bin/env bash
# 07-command-not-found — exec a non-existent binary. Verifies:
#   - exit code is 127
#   - IoExited.command_not_found = true (printed as
#     '# command not found' by the client to stderr)
#   - OCI runtime diagnostic is forwarded to the caller (the v1.0
#     streaming runtime preserves Docker's emission; Docker happens
#     to put this text on stdout, which is honest because we can't
#     re-route streamed bytes after exit code is known — the cnf
#     flag is the structural signal)
source "$(dirname "$0")/common.sh"

require_stack_up
SB=$(create_sandbox)
trap "delete_sandbox $SB" EXIT
log "created sandbox $SB"
wait_for_running "$SB"
wait_for_routing

LOG=/tmp/07-cnf.log
set +e
"$OPENSB_EXEC" --base "$WS_BASE" --sandbox "$SB" --api-key "$API_KEY" \
  -- definitely_not_a_real_binary >"$LOG.stdout" 2>"$LOG.stderr"
EXIT=$?
set -e
log "exit=$EXIT"

if [ "$EXIT" -ne 127 ]; then
  fail "expected exit 127, got $EXIT"
fi
grep -q '^# command not found' "$LOG.stderr" \
  || fail "expected '# command not found' marker on stderr (cnf flag), got:\n$(cat $LOG.stderr)"
# Diagnostic for the missing binary must appear on either stream.
# Two equivalent shapes accepted:
#   - OCI runtime form:    "executable file not found"
#   - Shell-wrapper form:  "opensb-wrapper: exec: ... not found"
# The wrapper form is emitted by v1.0's in-container-PID capture
# wrapper (`sh -c '...; exec "$@"'`) when `exec` cannot resolve
# the requested binary — same honest signal, different wording.
matched=0
for stream in stdout stderr; do
  if grep -q 'executable file not found' "$LOG.$stream" \
     || grep -q 'opensb-wrapper.*exec.*not found' "$LOG.$stream"; then
    matched=1
  fi
done
[ "$matched" -eq 1 ] || fail \
  "expected exec-failure diagnostic on stdout or stderr, got:\n  stdout: $(cat $LOG.stdout)\n  stderr: $(cat $LOG.stderr)"

pass "command-not-found: exit=127 + cnf flag set + exec-failure diagnostic preserved"
