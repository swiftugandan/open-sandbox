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

## Decisions

Six design questions, decided 2026-05-23. Structural purity is the
guide. Two of these revise earlier pragmatic leans — flagged where
relevant.

### D1. Multiplex protocol home: extend `proxy.proto`

Proto files in this repo follow *one file per hosting component*:
`controller.proto` is what the controller speaks, `proxy.proto` is what
the proxy speaks, `api.proto` is what the gateway speaks. The proxy is
still the proxy — it is just growing more verbs. A new
`sandbox_io.proto` would imply a second channel exists when
structurally there is one (the agent's reverse tunnel, gaining frame
types).

The *service name* inside `proxy.proto` is broadened to reflect the
generalised role (e.g. `TunnelService` → `SandboxIoService` or
equivalent if the existing service name is HTTP-specific). The file
stays put.

### D2. Gateway ↔ proxy connection: held-open multiplex

*Revising the earlier "one-per-session" lean — that was pragmatic, not
pure.*

The agent ↔ proxy connection is already held-open and multiplexed.
The gateway ↔ controller connection is already held-open. The
gateway ↔ proxy connection mirrors that shape — one (or a small pool
of) long-lived bidi gRPC connections, each incoming I/O request opens
a new stream on the existing multiplex. The gateway is a long-lived
service; its backend connections should be too.

The "reconnect logic cost" raised earlier is a non-cost — the gateway
already implements that for its controller connection. It is a tonic
primitive.

This preserves the streaming-over-stream invariant end-to-end: caller
WebSocket frame → gateway-internal stream → proxy multiplex stream →
agent reverse-tunnel stream → runtime. No impedance jumps in the
middle.

### D3. Public streaming surface: WebSocket

*Revising the earlier "chunked transfer with custom framing" lean — it
was inventing a sub-protocol when a standard one exists.*

The honest shape of streaming exec is bidirectional framed messaging:
stdin and signal frames flow caller→server while stdout/stderr/exit
frames flow server→caller, concurrently. Candidates evaluated:

| Option | Outcome |
| ------ | ------- |
| SSE | One-way, eliminated |
| Chunked + custom frame format | Technically works but requires concurrent request-write + response-read on one HTTP exchange (poor client support); forces an invented frame format |
| gRPC + grpc-web | Most honest about underlying shape but splits the public API into REST + gRPC surfaces — two mental models for the same product |
| **WebSocket** | **Chosen** |

WebSocket is HTTP-native (upgrade from `GET`), bidi by construction,
frames have opcodes built in (we use binary frames with a 1-byte
type prefix: stdin/stdout/stderr/signal/exit), universally supported
(curl, browsers, every language), and connection close = stream close
gives clean disconnect semantics matching the spike-01/02 cleanup
path.

Lifecycle endpoints (`POST /v1/sandboxes`, `GET`, `DELETE`,
`GET /v1/sandboxes/{id}`) stay REST because they are unary
RPC-shaped. Two surfaces because two different shapes — which is
honest. The I/O endpoints become:

- `GET /v1/sandboxes/{id}/exec` with `Upgrade: websocket`
- `GET /v1/sandboxes/{id}/files/read?path=…` with `Upgrade: websocket`
  (or remain REST since reads are unary-ish — TBD during impl, but
  WebSocket is the default choice for any I/O endpoint)
- `POST /v1/sandboxes/{id}/files/write_file` — stays REST (unary)
- `POST /v1/sandboxes/{id}/files/write_files` — stays REST (unary)

axum has first-class WebSocket support so no new dependency is needed.

### D4. Signals: in-band on the same stream

A session IS a stream. Splitting signals onto a side channel would
break the invariant "one stream = one session, one close terminates
it" and create ambiguous teardown semantics.

The structural worry that motivated a side-channel option ("stdin can
backpressure-block; signals would queue behind it") assumed signals
share a queue with stdin. They do not have to. Stdin is one
application-level frame type; signals are another. The receiver
dispatches by frame type on arrival; the sender writes signals
directly without waiting on the stdin queue. Same stream, distinct
dispatch paths.

### D5. No `Session` primitive — exec IS the session

A long-lived streaming exec of the user's chosen shell IS the
stateful workspace. The shell process holds env, cwd, history,
aliases, functions. That is what shells exist for.

Adding a platform-level `Session` resource would mean:

- A new resource type (CRUD, lifecycle, expiry)
- Platform-side state to persist (where? controller? agent? for how
  long?)
- An arbitrary boundary on what the session contains (env? cwd?
  umask? exported functions? shell history?)
- A reinvention of the Unix shell concept at the API layer

None of that exists if a session is just a WebSocket exec of the
caller's chosen shell. The shell does all the work; the platform does
none. When the WebSocket closes, the shell exits, state evaporates.
Lifecycle is naturally connection-bound, which the spike results made
desirable anyway.

For one-shot commands: short streaming exec per command, no shell
needed. Same primitive serves both patterns.

### D6. Version: v1.0.0

After the streaming-exec amendment, no remaining structural reshape
is on the horizon. Every other known item (M3 inspect endpoint, L3
public URL, future log streaming, TCP exposure) is additive on top of
the right shape, not a reshape of it. Multi-tenancy and TCP exposure
(NG-6, NG-2) remain non-goals; if they later require ID-scoping or
data-plane reshape, v2.0 is the honest move at that time.

Staying at v0.8 would sandbag — it implies more major breakage is
coming when our actual posture after streaming exec is "additive
evolution from here." v1.0 forces the discipline of additive-only
afterward, which is a feature, not a constraint.

v1.0 also forces a few productive side-effects:

- SPEC.md open-questions section must be resolved or moved to
  non-goals
- The API gets a formal stability statement
- SDKs can publish with confidence

This will be the project's first v1.0.0.

## Implementation scope (rough)

Not for sign-off here — for cost honesty. This is approximately 3–5×
the v0.7.0 amendment.

- **Proto:** extend `proxy.proto` (D1) with stream-typed exec/file
  verbs. Broaden the service name if currently HTTP-specific. Define
  the application-level frame envelope shared by exec and (future) log
  streaming: `kind` (stdin/stdout/stderr/signal/exit), payload bytes.
- **Agent:** `ContainerRuntime` trait reshape from
  `exec(options) -> output` to `exec(streams) -> streams`, plus the
  new `kill_exec(container, pid, grace)` method that both backends
  implement. New `ExecRegistry<StreamId, ExecRecord>` (HashMap +
  async cancellation hook) wired to stream close — per spike 01/02,
  required for both runtimes. Both backends implement first-class
  `WriteFile` / `ReadFile` (no shell helpers — H4 fix).
- **Proxy:** gains an "originate session" path. Existing reverse
  tunnel to the agent remains; the proxy now accepts gateway-side
  streams and multiplexes them into the agent's tunnel by sandbox id.
- **API gateway:** new held-open gRPC connection(s) to the proxy
  (D2). New WebSocket endpoints (D3) for streaming I/O:
  `GET /v1/sandboxes/{id}/exec` (Upgrade: websocket) and similar.
  REST endpoints retained for lifecycle and unary writes.
- **Controller:** delete the exec broker entirely. Delete the
  `EXEC_TIMEOUT` constant. Delete `ExecCommand`/`ExecResult` from the
  agent stream proto. Controller shrinks; NFR-PERF-2 improves.
- **Tests:** unit tests do not cover stream lifetime semantics well.
  Live e2e is the only meaningful verification — needs scripted
  cancellation, slow-client backpressure, signal injection, and
  disconnect-kills-process scenarios across both runtimes.

Per the protocol: this lives on `contracts/amendment-exec-streaming`
with a `v1.0.0-frozen` tag on contract freeze (D6), then per-module
loops for proxy, agent, controller, api, and CLI consumers.

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
