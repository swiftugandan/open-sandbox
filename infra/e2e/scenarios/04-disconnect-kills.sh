#!/usr/bin/env bash
# 04-disconnect-kills — start a long-running command that would
# touch /tmp/marker after a delay; SIGKILL the local opensandbox-
# exec process; wait 12s; verify /tmp/marker does NOT exist.
#
# Tests the agent-side ExecRegistry cleanup hook fires SIGTERM
# (then SIGKILL after grace) when the gateway-side stream closes.
# Spikes 01 + 02 confirmed neither runtime kills the in-container
# process for free; this scenario validates the explicit kill path
# the registry adds on top.
source "$(dirname "$0")/common.sh"

require_stack_up
SB=$(create_sandbox)
trap "delete_sandbox $SB" EXIT
log "created sandbox $SB"
wait_for_running "$SB"
wait_for_routing

# Start the marker-touching sleep in background. The sleep MUST be
# short enough that it would have finished (creating the marker)
# during our post-kill wait window — otherwise the absence of the
# marker proves nothing. With grace=5s + kill at t=3s, the
# expected kill point is t=8s. Sleep of 6s finishes at t=6s if
# never interrupted. Wait window ends at t=18s, which is well
# past the natural finish of the sleep (would-have marker would
# definitely be present by then).
log "launching 'sleep 6 && touch marker' (will be SIGKILLed at 3s; would natural-finish at 6s)"
"$OPENSB_EXEC" --base "$WS_BASE" --sandbox "$SB" --api-key "$API_KEY" \
  -- sh -c 'sleep 6; touch /tmp/04-marker; echo NO_KILL' \
  >/tmp/04-kill.log 2>&1 &
WSPID=$!

# Let the exec start.
sleep 3
log "SIGKILLing opensandbox-exec pid=$WSPID"
kill -9 $WSPID 2>/dev/null
wait $WSPID 2>/dev/null || true

# Wait until well past both the registry grace window AND the
# natural finish time of the unkilled sleep. If the kill failed,
# the sleep finishes at t=6s and the marker exists by t=15s.
log "waiting 12s past the abrupt-drop point (past natural sleep finish)"
sleep 12

# Check the marker via a fresh exec session.
log "checking for /tmp/04-marker"
MARKER_OUT=$("$OPENSB_EXEC" --base "$WS_BASE" --sandbox "$SB" --api-key "$API_KEY" \
             -- sh -c 'test -f /tmp/04-marker && echo PRESENT || echo ABSENT' \
             2>/dev/null | grep -E '^(PRESENT|ABSENT)$' | tail -1)
log "marker state = $MARKER_OUT"

if [ "$MARKER_OUT" = "PRESENT" ]; then
  fail "the in-container process was NOT killed — marker exists"
fi
pass "in-container process killed by ExecRegistry cleanup (marker absent)"
