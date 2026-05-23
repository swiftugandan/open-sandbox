# Spike 02 — Result

**Date:** 2026-05-23
**Question:** When the host-side `nsenter` is killed, does its
in-namespace child die or become orphaned and survive?
**Verdict:** **survives** — in-namespace child is reparented and runs
to completion.

## Method

`run.sh` in this directory. Spins up two containers:

- `spike02-target`: alpine `sleep infinity` (the "sandbox under test")
- `spike02-runner`: privileged, `--pid=host`, runs `nsenter` against the
  target's namespaces

The runner launches `nsenter -t <pid> --mount --uts --ipc --net --pid --
sh -c 'sleep 15; echo done > /tmp/spike02-marker'`, captures the nsenter
host PID, SIGKILLs nsenter at 2s, waits 18s, then checks for the marker
inside the target container.

Docker Engine 29.4.3, alpine + util-linux nsenter.

## Observation

```
[spike02] nsenter pid (inside runner's pid=host view) = 23840
[spike02] SIGKILLing nsenter pid=23840 from inside the runner
[spike02] waiting 18s for in-namespace task to complete (or not)...
[spike02] RESULT: marker EXISTS — content="done"
```

Marker file present with expected content. The in-namespace `sh -c
'sleep 15; echo done > marker'` survived the SIGKILL of nsenter and ran
to completion.

## Why

`nsenter` does `setns()` into the requested namespaces, `fork()`s a
child, and `exec()`s the target command in the child. The parent
nsenter waits on the child. When the parent is SIGKILLed, the child is
reparented to PID 1 in its PID namespace and continues running. This is
standard Unix orphan-reaping; nsenter does not install any
signal-forwarding or kill-on-parent-death machinery.

## Implication for the design

The youki backend behaves the same as the docker backend (spike 01):
disconnect does NOT propagate into a kill of the in-container process.

The structural model must include, on the agent side:

1. An `ExecRegistry` keyed by stream id holding:
   - host PID of the nsenter parent (for log/debug)
   - **in-container PID** (the actual process; must be captured at
     spawn, e.g. by parsing nsenter's stderr or by reading
     `/proc/<nsenter_pid>/task/*/children` immediately after fork)
   - the sandbox id (for context when we issue the kill)

2. A stream-close handler that:
   - Sends SIGTERM to the in-container PID (via a fresh `nsenter` into
     the same namespaces) with a short grace period
   - Sends SIGKILL if SIGTERM doesn't take effect within the grace
   - Removes the entry from the registry

3. Symmetric error path if the host nsenter dies on its own (e.g. OOM):
   discover the orphan via the registry and reap.

## Joint conclusion across spikes 01 + 02

The structurally pure design needs an `ExecRegistry` on the agent
**regardless of backend.** Neither runtime gives us
disconnect-kills-process for free. This is a non-trivial extra piece
of the agent that the design doc must acknowledge:

- It is small in code (one HashMap + one async cancellation hook).
- It is large in invariants (must not leak entries; must survive
  agent restart by reconciling against actually-running container
  processes; must not double-kill).
- It generalises cleanly to both runtimes because the kill mechanism
  is "exec a kill in the container's PID namespace" for youki and
  "docker exec <ctr> kill" for docker — same shape via the
  `ContainerRuntime` trait, different impl.

## Updated design implication

Section "Assumption 2: nsenter signals do NOT propagate to the
in-namespace child" in `EXEC_STREAMING_DESIGN.md` is **confirmed**.

Combined with spike 01, the design doc should now state plainly:

> **Both backends require explicit kill-on-stream-close plumbing.**
> An `ExecRegistry<StreamId, ExecRecord>` lives on the agent. Stream
> close triggers an async cleanup that sends SIGTERM (then SIGKILL
> after grace) to the in-container PID through the runtime's normal
> exec path.

This is captured as an amendment to the design doc in the same commit.
