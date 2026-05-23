# Exec Streaming — Pre-Amendment Design

> Status: **design / spikes** — not yet a contracts amendment. Lives on
> `contracts/amendment-sdk-agent-friction` next to the v0.7.0 work because
> both concern exec evolution. The implementation amendment branch will
> be `contracts/amendment-exec-streaming` once the questions below are
> settled and spikes have completed.

This document is the source of truth for the **exec streaming redesign**
discussion. If you are picking this up in a new session: read this doc
first, then the spike results under `spikes/exec-streaming/`, then the
friction-points consolidation at the bottom of this file. Do not rely on
memory or chat summaries — this file is the artifact.

---

## Why this exists

The `contracts/v0.7.0-frozen` amendment closed all 10 items from the original
SDK Agent friction report. Two follow-up experiments (Python data processing,
bash workflows) then surfaced a new cluster of friction items
(documented at the bottom of this file) that share a common architectural
root cause:

**Exec is modelled as a single request → single response message
exchange. The underlying primitive is a Unix process, which has lifetime,
streaming I/O, cancellation, environment, and connection-tied liveness
that cannot fit in a message exchange.**

Every open friction item in the H1–M5 cluster is the same shape: a process
attribute that cannot be expressed in a oneshot message exchange.

There is also a deeper symptom: **exec output is data-plane traffic
riding through the control plane.** The architecture has a clean
control/data split — proxy carries sandbox HTTP traffic, controller carries
lifecycle. But stdin/stdout/stderr ARE sandbox data flow, just over a
different file descriptor than HTTP. By stuffing them into controller
messages, the architecture quietly created a second hidden data path that
bypasses the proxy entirely. That is why every property the proxy has by
design (streaming, multiplexed lifetime, backpressure, connection-tied
teardown) is *missing* from exec.

This design is the structural fix.

## What I want the system to look like

Two kinds of operation, each on the correct plane:

| Kind        | Examples                                                                   | Belongs on                                |
| ----------- | -------------------------------------------------------------------------- | ----------------------------------------- |
| Lifecycle   | create/list/get/delete sandbox; agent register/heartbeat; routing-table writes | Control plane (controller, gRPC RPCs)     |
| Sandbox I/O | inbound HTTP; exec stdin/stdout/stderr/signals; file read/write; future log tails | Data plane (proxy/tunnel)                 |

The current architecture has this split correct for HTTP and wrong for
everything else. The fix is to finish the split: promote the existing
proxy tunnel from "HTTP reverse tunnel" to "sandbox I/O multiplex," and
route all I/O verbs through it.

Concretely:

- The agent's outbound multiplex connection (currently to the proxy) gains
  new stream types alongside `HttpRequest`/`HttpResponse`: `ExecStart`,
  `Stdin`, `Stdout`, `Stderr`, `Signal`, `Exited`, plus `ReadFile`,
  `WriteFile`. Same HTTP/2 multiplex, more verbs. Each I/O session is
  one virtual stream on the existing substrate.
- The **API gateway** gets a new client connection to the **proxy** (it
  has never had this before — only to the controller). The gateway
  originates exec and file ops through the proxy. Two clean backend
  connections: gateway→controller for lifecycle, gateway→proxy for I/O.
- The **controller** loses its exec broker entirely. It only does
  scheduling, routing-table writes, lifecycle CRUD. NFR-PERF-2 improves,
  not regresses, because the controller does strictly less work.
- The **proxy** evolves from "HTTP router" to "sandbox I/O director." It
  already knows which agent owns which sandbox (routing table), it
  already terminates streams from agents, it just gains the ability to
  *originate* sessions on the API gateway's behalf rather than only
  forwarding inbound HTTP.

## Why this is structurally pure, not pragmatic

I considered two alternatives and rejected both:

**Rejected A — "stream exec through the controller."** Adds streaming
gRPC between controller and agent, controller pipes chunks to API gateway.
This works but preserves the data-on-control-plane sin. NFR-PERF-2
(controller sized for 1000 agents on 2 vCPU) silently breaks because the
controller becomes a data-plane forwarder. Same wrong shape, streamingly.

**Rejected B — "add a dedicated WriteFile/ReadFile gRPC on the
controller's agent stream."** Repeats the same data-on-control-plane
mistake at lower volume, plus introduces a parallel verb set that
duplicates what the I/O multiplex would do natively.

Only "exec rides the data plane, alongside HTTP" survives. Once that
choice is made, almost every open friction item resolves without further
design:

| Item                                | Why it disappears                                                                 |
| ----------------------------------- | --------------------------------------------------------------------------------- |
| H1 (60s timeout)                    | Stream has no RPC-style deadline; lives as long as the process                    |
| H2 (no shell session persistence)   | A streaming `bash -i` exec **is** a session — env/cwd live in the bash process    |
| H3 (disconnect doesn't kill)        | Stream close → agent's `ExecRegistry` cleanup SIGTERMs the in-container PID (spikes 01 + 02 showed neither runtime does this for free) |
| H4 (write_file shell helper in logs)| WriteFile becomes a first-class verb; no embedded shell script                    |
| M1 (no signals/cancel)              | `Signal` is just another frame on the open stream                                 |
| M2 (no streaming output)            | Stdout/stderr ARE frames on the stream by construction                            |
| M5 (stdin utf8 vs b64 footgun)      | Stdin is bytes on a stream; no JSON-string encoding question exists               |
| M4 (cwd default inconsistency)      | Single sandbox `working_dir` set at create time; all I/O verbs share it           |
| L1 (read_file cwd lost in logs)     | One resolution path for everyone, one place to log                                |
| L4 (no exec_id in container)        | `stream_id` is available; agent can inject `OPEN_SANDBOX_EXEC_ID` env at start    |

M3 (sandbox inspect endpoint) and L2/L3 (post-delete state, public URL)
are unrelated and additive — not part of this amendment.

## Load-bearing assumptions that need spike confirmation

These are the things the structural model **depends on being true**. If
a spike shows otherwise, the design has to grow to compensate.

### Assumption 1: Docker exec dies when its attached stream is dropped

The claim "disconnect kills the process" only holds if the underlying
runtime kills the in-container process when the agent drops its attached
exec stream. For Docker (bollard), this is plausible but unverified —
dockerd may or may not propagate the disconnect into a SIGKILL of the
exec target.

**Spike:** `spikes/exec-streaming/spike-01-docker-exec-disconnect/`

**Result (2026-05-23): FALSE.** dockerd does NOT propagate client
disconnect to the exec. The in-container process ran to completion
after the local docker client was SIGKILLed. See
`spikes/exec-streaming/spike-01-docker-exec-disconnect/RESULT.md`.

**Consequence:** the docker backend requires an agent-side
`ExecRegistry` and explicit kill-on-stream-close plumbing.

### Assumption 2: nsenter signals do NOT propagate to the in-namespace child

For the youki backend, we exec via `nsenter --target <pid> -- <cmd>`.
When the agent kills the local `nsenter` process, does the child running
inside the container namespaces die too?

Reading the `nsenter(1)` man page suggests **no** — nsenter exec's the
target program after setting up namespaces, so the in-namespace process
becomes a direct descendant of the agent (not of nsenter), and killing
nsenter does nothing.

**Spike:** `spikes/exec-streaming/spike-02-nsenter-signal-propagation/`

**Result (2026-05-23): CONFIRMED.** Killing the host-side `nsenter` does
**not** kill the in-namespace child; the child is reparented to PID 1
in its PID namespace and survives. See
`spikes/exec-streaming/spike-02-nsenter-signal-propagation/RESULT.md`.

**Consequence:** the youki backend also requires an agent-side
`ExecRegistry` and explicit kill-on-stream-close plumbing. The kill is
issued by exec'ing a fresh `nsenter ... kill -TERM <pid>` into the same
namespaces (with SIGKILL after a grace period). The in-container PID
must be captured at exec start by inspecting
`/proc/<nsenter_pid>/task/*/children` immediately after fork or by an
equivalent mechanism.

### Joint conclusion from spikes 01 + 02

**Both backends require the same plumbing.** The structurally pure design
now formally includes:

- An `ExecRegistry<StreamId, ExecRecord>` on the agent, where
  `ExecRecord` carries `{ in_container_pid, sandbox_id, runtime_handle }`.
- A stream-close hook that performs SIGTERM (grace) → SIGKILL on the
  in-container PID via the runtime trait, then removes the registry
  entry.
- Symmetric reconciliation on agent restart: walk known containers,
  reap any leftover marker / state, drop stale registry entries.

This is a small additional piece of work (one HashMap + one async
cancellation handler) but a non-trivial set of invariants (must not
leak entries, must not double-kill, must survive agent restart). The
runtime trait gains one method:

```rust
fn kill_exec(&self, container: &ContainerId, in_container_pid: i32,
             grace: Duration) -> impl Future<Output = Result<(), AgentError>> + Send;
```

Both `agent-docker` and `agent-youki` implement it; the shape is the
same, only the kill mechanism differs (`docker exec <ctr> kill` vs
`nsenter ... -- kill`).

### Assumption 3: axum chunked-transfer streams propagate backpressure

The API gateway needs to surface stdout/stderr to the caller as the
process emits them. If axum's response stream applies backpressure to
the upstream `Stream<Item = Bytes>` when the HTTP client is slow, we
can keep memory usage bounded. If not, we need to bound the buffer
explicitly.

**Verification:** read the axum/hyper/h2 docs (well-known territory, not
worth a code spike unless docs are ambiguous).

### Assumption 4: tonic server-streaming surfaces client cancel

The proxy→agent leg uses gRPC. If a tonic server-streaming RPC's
`Stream::poll_next` returns `Ready(None)` (or an error) when the client
cancels, the agent learns of the disconnect promptly and can act.

**Verification:** standard tonic behavior, documented. No spike needed
unless we see weird latency.

## Open questions for the user

These shape the work and need a decision before code is written:

1. **Multiplex protocol: extend `proxy.proto` or new `sandbox_io.proto`?**

   Extending `proxy.proto` reuses the existing message types
   (`StreamClose`, `DataChunk`) and the agent's existing tunnel client.
   But it conflates "HTTP forwarder" semantics into a thing that is no
   longer HTTP-specific.

   New `sandbox_io.proto` is honest about the new role. Requires a new
   service definition, new agent client. Cost is one new file plus the
   agent learning two clients.

   **Lean:** new file. Honest naming matters and the new file is small.

2. **API gateway → proxy: held-open multiplex or one stream per session?**

   Held-open avoids per-request handshake cost; adds liveness/reconnect
   logic in the gateway.

   One-per-session is simpler; pays a connection setup per exec.

   **Lean:** one-per-session for v0.8.0. Optimize later if a benchmark
   shows the per-call cost matters.

3. **Stream-to-HTTP surface: SSE, chunked transfer, or HTTP/2 trailers?**

   SSE: one-way, line-based, easy in browsers, awkward for binary.
   Chunked: universal, works with curl, but you have to invent a frame
   format to distinguish stdout / stderr / exit / signal.
   HTTP/2 trailers: nice for exit_code at the end, but client support
   is patchy and many HTTP libraries don't expose them.

   **Lean:** chunked transfer with a length-delimited frame format.
   First byte is frame type (`stdout`, `stderr`, `exit`, `signal`),
   next four bytes are length, then payload. Same machinery later
   serves `fetch_logs`.

4. **Signals: in-band frame or side channel?**

   In-band: one stream per exec, `Signal` frames go in the same stream
   as `Stdin`. Structurally cleanest.

   Side channel: separate control stream alongside the I/O stream.
   Avoids any "did the stdin queue block the signal" question.

   **Lean:** in-band, but signal frames go on the *control* sub-queue
   not the stdin queue. One stream, two logical queues, no blocking
   concern.

5. **Is "Session" a primitive, or is exec the session?**

   I do not want to add a `Session` abstraction (env + cwd carried
   between calls). The right shape is: one long-lived streaming exec
   of the caller's chosen shell. Anything you would put in a session,
   you put in the shell. Confirm this before baking in.

   **Lean:** no Session primitive. Exec is the session.

6. **Backward compatibility policy.**

   This is the largest contract amendment the project has seen. Every
   prior amendment has been a breaking minor or major. The current
   contracts version is `v0.7.0`. The change here removes the
   message-shaped exec entirely — that is a major-version reshape.

   **Lean:** `v1.0.0`. Has the project deliberately held v1.0.0 back,
   or is it just that no amendment has been big enough to claim it?

## Implementation scope (rough)

Not for sign-off here — for cost honesty. This is approximately 3–5×
the v0.7.0 amendment.

- **Proto:** new `sandbox_io.proto` (or extended `proxy.proto`) with
  stream-typed exec/file verbs. Wire format for the data-plane frames.
- **Agent:** `ContainerRuntime` trait reshape from
  `exec(options) -> output` to `exec(streams) -> streams`. Both
  backends (docker, youki) implement streaming exec. Both backends
  implement first-class WriteFile / ReadFile (no shell helpers).
- **Proxy:** gains an "originate session" path. The API gateway opens
  a stream to the proxy; the proxy multiplexes it into the agent's
  reverse tunnel; the agent dispatches to its runtime.
- **API gateway:** new client connection to the proxy. New
  streaming HTTP endpoints (`/exec`, `/files/write_file`,
  `/files/read`) that surface the multiplex as chunked-transfer
  framed responses.
- **Controller:** delete the exec broker entirely. Delete the
  `EXEC_TIMEOUT` constant. Delete `ExecCommand`/`ExecResult` from the
  agent stream proto. Controller shrinks.
- **Tests:** unit tests do not cover stream lifetime semantics well.
  Live e2e is the only meaningful verification — needs scripted
  cancellation, slow-client backpressure, signal-injection, and
  disconnect-kills-process scenarios.

Per the protocol: this will live on `contracts/amendment-exec-streaming`
with a `v1.0.0-frozen` tag on contract freeze, then per-module loops
for proxy, agent, controller, api, and CLI consumers.

## Consolidated friction snapshot (rounds 2–3)

Open items at time of writing this design. Detailed root causes in the
chat log; one-line summary below for cross-session continuity.

### HIGH (resolved by this amendment)

- **H1** Hard 60s exec timeout, no per-call override, no async/stream.
- **H2** No shell session persistence (env, $PWD reset per exec).
- **H3** Caller disconnect does not kill the in-container process.
- **H4** Internal `write_file` shell helper leaks into observability.

### MEDIUM (resolved or significantly improved by this amendment)

- **M1** No process inspection / control / cancel API.
- **M2** No streaming exec output (buffered until exit).
- **M3** No sandbox inspect endpoint. *(NOT addressed here — separate
  additive amendment.)*
- **M4** Default-cwd inconsistency between writes (`/home`) and exec (`/`).
- **M5** `stdin` UTF-8 is a footgun; `stdin_b64` is the reliable form
  but discoverability is low.

### LOW

- **L1** `read_file` cwd context lost from agent logs.
- **L2** `list` after `delete` shows empty before container teardown
  completes. *(NOT addressed here — spec/docs note.)*
- **L3** `exposed_port` accepted on create, but the response gives only
  `subdomain` — no full public URL. *(NOT addressed here — separate
  additive amendment.)*
- **L4** No `exec_id` exposure inside the container.

## Spike index

| Spike | Question | Result location |
| ----- | -------- | --------------- |
| 01 | Does docker exec kill the in-container process when bollard's attached stream is dropped? | `spikes/exec-streaming/spike-01-docker-exec-disconnect/RESULT.md` |
| 02 | Does killing host-side `nsenter` propagate SIGTERM to the in-namespace child? | `spikes/exec-streaming/spike-02-nsenter-signal-propagation/RESULT.md` |

Spike results are committed alongside this doc so the conclusions are
auditable. If a spike contradicts an assumption above, this doc is
updated in the same commit as the spike result.
