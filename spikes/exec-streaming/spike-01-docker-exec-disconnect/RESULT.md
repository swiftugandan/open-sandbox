# Spike 01 — Result

**Date:** 2026-05-23
**Question:** Does docker exec kill the in-container process when the
attached client is killed?
**Verdict:** **NO** — process survives client disconnect.

## Method

`run.sh` in this directory. Starts an alpine container with
`sleep infinity` as PID 1, launches `docker exec -i ... sleep 15; touch marker`
as a child process, SIGKILLs the local docker client at 2s, then waits 18s
(well past the natural completion of the inner sleep) and checks for the
marker file inside the container.

Docker Engine version: 29.4.3 (Docker Desktop, macOS).

## Observation

```
[spike01] killing local docker client pid=58988 (wrapper=58986) after 2s
[spike01] waiting 18s for in-container task to complete (or not)...
[spike01] RESULT: marker EXISTS — content="done"
```

The marker file exists with the content the script intended to write at
the end. The inner `sleep 15 && echo done > /tmp/spike01-marker` ran to
completion despite the client process being terminated at 2s.

## Implication for the design

The docker backend does **not** get disconnect-kills-process for free.
Closing the client-side attached stream / killing the local docker client
does not propagate into a kill of the exec target. dockerd keeps the
exec running until its natural completion.

The agent must therefore:

1. After `start_exec`, capture the exec instance id returned by bollard.
2. Bind that exec id to the lifetime of the agent-side stream that the
   proxy opened (the stream the API gateway is talking to).
3. When the agent-side stream closes (client cancel, proxy drop, gateway
   crash, etc.), the agent must explicitly invoke a kill against that
   exec — either by sending a signal frame to the exec instance via
   Docker's exec API, or by `docker exec <container> kill -TERM <pid>`
   targeting the inner PID (which itself requires capturing the inner
   PID at start time, easiest via `getpgid 0` echoed on stderr or by
   inspecting the exec's `Pid` field via `docker exec inspect`).

The simpler path is the explicit-kill-on-stream-close approach via
`docker exec inspect` → `pid` → `kill`. Recommend storing the pid in an
agent-side `ExecRegistry` keyed by stream id.

## Updated design implication

Section "Assumption 1: Docker exec dies when its attached stream is
dropped" in `EXEC_STREAMING_DESIGN.md` should be amended to record
this **false** result. The streaming exec amendment must include an
agent-side `ExecRegistry` (or equivalent) and an explicit teardown
path on stream close.

Cost: small. One more agent state machine that mirrors the one we will
need for youki anyway (spike 02 will likely show youki has the same
property — nsenter does not signal-propagate either).
