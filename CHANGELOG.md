# CHANGELOG

## v1.0.2 (in progress) — pull_policy, startup-time optimization

Adds a wire-compatible `pull_policy` field on `CreateSandboxRequest`
(api.proto) and `SandboxConfig` (controller.proto). Old clients send
the proto3 default zero (`UNSPECIFIED`), which the agent resolves to
`IfNotPresent` — the same behavior new clients get by omitting the
field. The `pull_policy` field is the structural fix for the
warm-startup-time optimization shipped on this branch (see
`/code-review` findings 2026-05-26 for context).

### Additions
- **`CreateSandboxRequest.pull_policy`** (api.proto) +
  **`SandboxConfig.pull_policy`** (controller.proto): new enum
  `PullPolicy { UNSPECIFIED, IF_NOT_PRESENT, ALWAYS, NEVER }`. JSON
  API accepts kebab-case: `"if-not-present"` (default), `"always"`,
  `"never"`. Unknown wire-i32 values collapse to `IfNotPresent` for
  forward-compat.
- **`open_sandbox_contracts::types::PullPolicy`**: rust-side newtype
  with serde derive, defaulting to `IfNotPresent` when the JSON field
  is omitted. From/To conversions to the prost-generated wire enum.
- **`ContainerConfig.pull_policy`** on the agent: the runtime trait
  parameter that DockerRuntime and YoukiRuntime honor.

### Rolling-upgrade caveat
- An old v1.0.1 agent binary receiving `StartSandbox` from a v1.0.2
  controller will silently drop the unknown `pull_policy` field
  (proto3 unknown-field semantics) and continue its always-pull
  behavior. Practical impact: a caller setting
  `pull_policy = "never"` against a mixed-version fleet will see
  some sandboxes attempt a registry pull on agents that haven't
  rolled to v1.0.2 yet — violating the air-gapped guarantee. Roll
  agents to v1.0.2 before relying on `Never` semantics. The
  inverse (v1.0.2 agent receiving v1.0.1-shaped messages) is safe:
  the missing field defaults to UNSPECIFIED → IfNotPresent, which
  matches v1.0.1's effective behavior on a warm cache.

### Behavior change (DockerRuntime)
- Warm-path `create_and_start` no longer issues a docker registry
  round-trip when the image is locally cached (`pull_policy =
  IfNotPresent`). Measured 2026-05-26 on the dev fleet:
  serial-warm `t_running_ms` p50 **1623 → 562 ms (−65%)**, p99 **1874
  → 1226 ms (−35%)**. Concurrent batch-of-4 `batch_total_ms`
  **2824 → 1603 ms (−43%)**.
- `Always` opts back into the v1.0.1 always-pull behavior for
  floating tags. `Never` returns `Runtime { detail }` if the image is
  not present locally — required for air-gapped deployments.
- New `image present locally; skipping pull` info event replaces the
  `pulling image` / `image pull complete` pair on the warm path.
  **Downstream consumers**: log-grep dashboards that paired these
  events to count sandbox creates need to also count the new event.
  Tracked separately in FOLLOWUPS\_v1.0.1.md P4 alongside the missing
  Prometheus metrics.
- TOCTOU recovery: if `create_container` returns 404 (image was
  pruned between inspect and create, or a layer was GC'd under disk
  pressure), the runtime pulls and retries once unless policy is
  `Never`.

### Behavior change (YoukiRuntime)
- Accepts the `pull_policy` field. `IfNotPresent` is the existing
  behavior (oci-client's `.complete` marker fast path). `Always` is
  threaded through but currently degrades to `IfNotPresent` (no
  force-refetch flag in `ImageManager`; tracked as a youki
  follow-up). `Never` fails fast with a `Runtime` error citing the
  feature gap.

### Internal
- `pull_image_with_retry` extracted from `create_and_start` so the
  TOCTOU fallback and the cold-cache path share one retry/backoff
  implementation.
- Tri-state `Presence { Present, Absent, Unknown }` in `agent-docker`
  replaces the boolean `already_present`. Closes the iter2 guard bug
  where the 404 fallback was incorrectly gated by `already_present =
  false` on the inspect-error path.

### Startup-path round-trip elimination (iter4 + iter5)
- DockerRuntime now pre-allocates the host port (kernel-assigned via
  a momentary `TcpListener::bind("0.0.0.0:0")`) before
  `create_container` and passes it explicitly via `port_bindings`.
  The post-`start_container` `inspect_container` round-trip is gone —
  the agent already knows the host port. Measured: best-case
  agent-internal phase delta on macOS Docker Desktop **365ms → 315ms
  (−50ms)**. The same code path saves ~5ms on native Linux (one
  bollard call worth) and is below measurement noise there.
- `publish_all_ports: true` is also gone — replaced by an explicit
  single-port `port_bindings`. Sandbox-image EXPOSE directives are no
  longer auto-published to the host. The contract is already
  single-port (`CreateSandboxRequest.exposed_port` is a `uint32`, not
  a list), so this is dead-binding cleanup, not behavior loss.
- The pre-allocation widens the TOCTOU window between our probe-and-
  release and docker's bind. Iter5 adds a bounded retry loop (3
  attempts) around the create+start pair: when `start_container`
  returns the docker-specific "bind: address already in use" /
  "port is already allocated" 500, the agent force-removes the
  orphan container, allocates a fresh ephemeral port, and retries.
  Detection is substring-based on the daemon-supplied message — see
  the `is_port_collision` helper and its 4 unit tests for the
  matched patterns. Other 500s fall straight through.
- Iter6/iter7 refinements on the same retry surface:
  * Final-attempt port-collision returns
    `AgentError::Runtime { detail: "port-bind collision after N attempts: <bollard message>" }`
    so log scanners can distinguish "first-time collision" from
    "all retries burned" without correlating across earlier
    `warn!` lines. (Iter5's post-loop "exhausted" Err was
    unreachable in the loop CFG.)
  * Small per-sandbox deterministic jitter (10–40ms range derived
    from the sandbox-id first byte) between port-retry iterations
    decorrelates concurrent agents racing the same ephemeral pool.
  * New 409 name-conflict recovery: a stale `sandbox-<uuid>`
    container from a crashed prior agent (or a transient
    force_remove failure) is force-removed by name and the create
    is retried within the same outer iteration. Gated on the
    message containing "is already in use" so future non-name 409
    reasons don't trigger spurious removal of unrelated
    containers.
  * Both the 404 image-missing recovery and the 409 name-conflict
    recovery now `continue` to the next outer port-retry iteration
    on second-attempt failure (rather than collapsing to a
    permanent FAILED via `?`). This closes the iter6-review
    finding where a transient force_remove failure during 409
    recovery — the exact scenario iter6 set out to fix from
    iter5 — would still produce a permanent FAILED because the
    inner-retry's error propagated unconditionally.
  * Iter8: extended the same `continue`-on-non-final treatment to
    the inner pull_image_with_retry inside the 404 arm. Iter7
    left that call's `?` unguarded, so a transient registry
    rate-limit during 404 recovery on attempt 1 of 3 would still
    produce a permanent FAILED — the exact failure mode the
    iter6/iter7 CHANGELOG entries had over-promised was closed.
    Iter8 closes it for real. Worst-case `create_and_start`
    latency under sustained registry pressure is now bounded by
    `MAX_PORT_RETRY_ATTEMPTS × MAX_PULL_ATTEMPTS × max_pull_backoff`
    ≈ 3 × 4 × ~7.5s ≈ 90s before final FAILED; in practice this
    is dominated by gRPC deadlines from the controller.
  * Iter10: PullPolicy fail-closed at the wire boundary. Iter3's
    `From<i32> for PullPolicy` silently collapsed any unknown wire
    value (e.g. a hypothetical future `PULL_POLICY_NEVER_OFFLINE`
    = 4 from a newer client) to the default `IfNotPresent` —
    which would defeat the air-gap guarantee for callers who set a
    stricter-than-`Never` policy. Iter10 adds
    `PullPolicy::from_wire_i32_strict(v) -> Result<Self,
    UnknownPullPolicy>` and uses it at the controller's management
    endpoint (the public gRPC wire boundary); unknown values now
    reject with `Status::InvalidArgument` carrying the raw value
    and rationale in the message. The lossy `From<i32>` is
    preserved for defense-in-depth at downstream call sites that
    trust the controller has already validated (e.g. the agent's
    sandbox.rs). Three new unit tests anchor: known-value
    round-trip, unknown-value fail-closed, negative-value
    fail-closed. A tripwire test documents that Rust's blanket
    `impl<T,U:Into<T>> TryFrom<U> for T` synthesizes a free
    infallible `TryFrom<i32>` from our `From<i32>`, so the
    idiomatic `PullPolicy::try_from(42)` returns `Ok(IfNotPresent)`
    — wire boundaries must use `from_wire_i32_strict` explicitly.

    Iter10's same-iteration `/code-review` surfaced a critical bug
    that defeated the fail-closed design end-to-end: the api
    gateway's `grpc_to_api` had no arm for
    `tonic::Code::InvalidArgument`, so the controller's structured
    reject collapsed to `ApiError::Internal` → HTTP 500. Fixed
    inside the same iteration: `Code::InvalidArgument` now maps to
    `ApiError::InvalidRequest` → HTTP 400 with the controller's
    actionable detail preserved. Two new unit tests pin the
    mapping (`invalid_argument_maps_to_invalid_request` and a
    sibling that asserts the existing x-os-error-code trailer
    cascade still overrides the Code-based fallback).

  * Iter9: four small followup polish items.
    `is_name_conflict_409` extracted from the inline match guard
    into a named helper alongside `is_port_collision`, with five
    unit tests covering the current docker message, an
    uppercase-cased variant, a 500-status-same-message false
    positive, an unrelated 409 (volume conflict) false positive,
    and a non-DockerResponseServerError variant. All three
    create_and_start final-attempt arms (port-collision, 404
    pull-recovery, 409 force-remove) now route through a single
    `final_attempt_err(kind, max, e)` helper that produces the
    uniform format `<kind> after <N> attempts: <bollard message>`
    — closing the iter6/iter9 inconsistency where the
    port-collision wrapper used `"after N attempts"` and the
    iter9 404/409 wrappers used `"on final attempt (N/N)"`. Two
    new unit tests anchor the format string so future refactors
    can't silently drift log-scanner regexes. `force_remove`'s
    parameter renamed from `container_id` to `target` and its
    warn log field follows, removing the misleading semantics on
    the 409-recovery call site which passes a container *name*
    rather than an id.
- `extract_host_port` and its supporting code paths are deleted — no
  remaining callers.

## v1.0.1 — Streaming read, two-listener proxy, youki setns file ops

On-wire compatible with v1.0.0; no proto changes.

### Additions
- **`WS /v1/sandboxes/{id}/files/read-stream?path=<...>`** — streaming
  variant of `GET /files/read`. Raw file bytes as WS Binary frames,
  terminated by WS Close (`1000` EOF, `44xx` error). Hosted on a
  distinct path from the unary endpoint to sidestep a transitive
  axum 0.7/0.8 trait collision pulled in by tonic.
- **`ReadFileSession`** in `open-sandbox-ws-client` — `connect()` +
  `next_chunk()`. Companion example: `examples/stream-read-file.rs`.

### Security
- **Two-listener proxy split.** The proxy now binds two ports:
  `:50052` (Public role, agents dial here for `OpenTunnel` only)
  and `:50053` (Internal role, api gateway dials here for
  `OpenIoStream` only). Wrong-RPC calls return
  `Status::unimplemented` at the role gate before bearer-token
  validation. The `OPEN_SANDBOX_INTERNAL_TOKEN` bearer check
  remains as defense-in-depth. Set both ports equal to fall back
  to a single combined listener (development only).
- New flags: `--internal-grpc-port` /
  `OPEN_SANDBOX_PROXY_INTERNAL_GRPC_PORT`.
- The api gateway's `--proxy-url` default moves from `:50052` to
  `:50053`.

### youki backend
- **File ops via `setns(2)`.** `YoukiRuntime::{read_file,
  write_file, write_files_targz}` now enter the container's
  mount namespace from a dedicated thread and call plain
  `std::fs::*`. Removes the in-container `cat` / `tee` / `tar` /
  `mkdir` / `mv` invocations. Pure-distroless sandbox images are
  first-class for the youki file plane.

### Build & test
- `crates/agent-youki/Dockerfile.test` gains an ENTRYPOINT shim
  that performs cgroup v2 root-controller delegation before
  exec'ing the test command. Fixes `libcontainer`'s "no internal
  process constraint" failure on Docker Desktop's nested Linux
  VM.

## v1.0.0 — Streaming exec (first stable release)

Open Sandbox v1.0 is the first contracts version with stability
guarantees. Earlier `contracts/v0.x` tags were internal development
milestones and are not consumed by external integrators.

### Public surface

REST lifecycle (`Authorization: Bearer <api-key>` on every request):

- `POST   /v1/sandboxes` — create
- `GET    /v1/sandboxes` — list
- `GET    /v1/sandboxes/{id}` — inspect
- `DELETE /v1/sandboxes/{id}` — destroy
- `POST   /v1/sandboxes/{id}/files/write_file` — single-file upload
- `POST   /v1/sandboxes/{id}/files/write_files` — tar.gz extraction
- `GET    /v1/sandboxes/{id}/files/read?path=...` — file read

Streaming I/O (`Authorization: Bearer <api-key>` on the WebSocket
upgrade):

- `WS /v1/sandboxes/{id}/exec` — bidirectional exec session
- `WS /v1/sandboxes/{id}/files/read-stream?path=<...>` — chunked
  file read; raw bytes as WS Binary frames, terminated by WS
  Close (1000 = EOF, 44xx = error)

### Architecture

- Exec is a bidirectional stream-shaped session, not a request /
  response. Sessions live as long as the WebSocket; there is no
  built-in per-call timeout.
- Long-running tasks (builds, training runs, integration suites)
  and interactive shells (`bash -i`, `python -i`) are first-class
  via the same primitive.
- Process lifecycle is connection-bound: closing the WebSocket
  triggers `SIGTERM` (with a 5s grace) then `SIGKILL` on the
  in-container PID.
- File operations and exec share one data-plane gRPC stream
  (`SandboxIoService.OpenIoStream`), so writes ride the same
  proxy → agent path as exec.

### Reference clients

- `crates/ws-client` — Rust SDK exposing `ExecSession`.
- Three runnable examples under `crates/ws-client/examples/`:
  - `echo` — minimal command + capture stdout
  - `long-running-build` — exec > 60s, demonstrates no client-side
    timeout
  - `interactive-bash` — exec-as-session: bidirectional shell with
    half-closing stdin

### Compatibility

There is no `v0.x → v1.0` migration: nothing previously shipped to
external consumers. The pre-1.0 internal milestone tags
(`contracts/v0.7.0-frozen` etc.) remain in git for historical
reference only.
