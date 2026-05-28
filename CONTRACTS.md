# Contracts

> This document is the prose companion to the contracts crate at `crates/contracts/`. The crate is the source of truth — types compile, prose does not. This document explains the *why* and the cross-cutting policies that the types themselves cannot express.

## Status

Current released version: **`contracts/v1.0.2`** on `main` (item #13 landed in commits `9341a62`..`bd414f6`; items #1-#12 from [`docs/plans/PLAN_CONTRACTS_v1.0.2.md`](docs/plans/PLAN_CONTRACTS_v1.0.2.md) are still pending and would ship in a separate v1.0.2-completion session).
Last freeze tag: **`contracts/v1.0.0-frozen`** (the wire-shape freeze; unchanged in v1.0.1 and wire-compatibly extended in v1.0.2).
The `contracts/v1.0.2` git tag itself currently points at `0e68177` (predating the #13 landing) and should be moved when #1-#12 ship.

*v1.0.0 frozen 2026-05-23 (paired with `spec/v1.0.0`). v1.0.1 is on-wire compatible: no proto changes, no new error variants, no constant changes. Changes after the freeze require a `contracts/amendment-<desc>` branch and a version bump for any wire-shape changes.*

v1.0.0 is the first stable contracts release. It was a **breaking** reshape from v0.7: exec moves from a message exchange routed through the control plane (controller's ExecBroker, agent stream ExecCommand/ExecResult, gateway's unary ExecSandbox RPC) to a stream-shaped session on the data plane (proxy's SandboxIoService.OpenIoStream, agent's tunnel multiplex, gateway WebSocket). Architectural-decision record: `EXEC_STREAMING_DESIGN.md`. Historical plan: `PLAN_EXEC_STREAMING.md` (tag `plan/v0.6.3`).

v1.0.2 begins on the tree as the first amendment to v1.0.1, starting with the `pull_policy` field ([`docs/plans/PLAN_CONTRACTS_v1.0.2.md`](docs/plans/PLAN_CONTRACTS_v1.0.2.md) item #13):

- **`api.PullPolicy` enum** with variants `UNSPECIFIED`, `IF_NOT_PRESENT`, `ALWAYS`, `NEVER`. Wire-compatible addition: new proto3 field on `CreateSandboxRequest` and `SandboxConfig`. Old clients send the zero `UNSPECIFIED`, which `from_wire` collapses to `IfNotPresent` — the same behavior new clients get by omitting the field. The JSON API accepts kebab-case strings (`"if-not-present"` / `"always"` / `"never"`) via `#[serde(rename_all = "kebab-case")]`. **Unknown wire-i32 values are rejected at the controller's management endpoint** with `Status::InvalidArgument` → HTTP 400 (via `PullPolicy::from_wire_i32_strict`), so a future stricter-than-`Never` variant from a newer client doesn't silently downgrade to `IfNotPresent` and defeat the air-gap guarantee. The lossy `From<i32>` is preserved downstream of the controller (e.g. in the agent's `sandbox.rs`) as defense-in-depth, since the controller has already validated.
- **Default policy = `IfNotPresent`** matches `docker run` semantics: skip the registry round-trip when the image is locally cached. Callers that need to refresh a floating tag like `:latest` on every start must set `pull_policy = "always"`. Air-gapped / strict-pin deployments set `pull_policy = "never"`.

v1.0.1 adds three internal-only improvements on top of the v1.0.0 wire shape (see `CHANGELOG.md` for the operator-facing summary):

- **WS `/v1/sandboxes/{id}/files/read-stream`** — streaming file-read endpoint exposed by the api gateway. Hosted on a distinct URL from the unary `GET /files/read` to sidestep a transitive axum 0.7/0.8 trait collision pulled in by tonic. Both endpoints back onto the same `IoStart::ReadFile` agent flow; no contract change.
- **Two-listener proxy split.** The proxy now binds `:50052` (Public role, OpenTunnel only) and `:50053` (Internal role, OpenIoStream only). Wrong-RPC calls return `Status::unimplemented` at the role gate. `OPEN_SANDBOX_INTERNAL_TOKEN` Bearer check stays as defense-in-depth.
- **youki file ops via `setns(2)`.** `YoukiRuntime::{read_file, write_file, write_files_targz}` enter the container's mount namespace from a dedicated thread (`unshare(CLONE_FS)` + `setns(CLONE_NEWNS)`, restored by a Drop guard). Eliminates the in-container `cat`/`tee`/`tar` invocations; distroless sandbox images are now first-class for the youki file plane.

Summary of v1.0 changes vs v0.7:

- `proxy.proto`: `TunnelService` renamed to `SandboxIoService`. New `OpenIoStream` RPC (gateway-originated bidi I/O sessions). New messages: `IoClientFrame`, `IoServerFrame`, `IoStart` (with `ExecParams` or `ReadFileParams` via oneof), `IoSignal`, `IoClose`, `IoStarted`, `IoExited`, `IoError`. `TunnelRequest`/`TunnelResponse` gain `IoClientFrame`/`IoServerFrame` oneof variants so I/O streams multiplex into the existing agent tunnel alongside HTTP forwarding.
- `controller.proto`: `ControllerCommand.ExecCommand` and `AgentMessage.ExecResult` removed. The oneof fields are renumbered to close the gaps (no shipped binaries to be wire-compatible with).
- `api.proto`: `ExecSandbox` RPC and its request/response messages removed. The gateway's external `POST /v1/sandboxes/{id}/exec` is replaced by `WS /v1/sandboxes/{id}/exec`.
- `crates/contracts/`: `ApiError` gains `IoStreamFailed`, `SandboxGone`, `ProxyUnavailable`; loses `ExecFailed`, `CommandNotFound` (those failure modes are now reported via `IoExited` / `IoError` frames on the WebSocket stream itself). `EXEC_TIMEOUT` constant removed; `WS_IDLE_PING_INTERVAL`, `WS_IDLE_PING_TIMEOUT`, `EXEC_KILL_GRACE` added.

## Cross-cutting policies

### Serialization

- Format: Protocol Buffers for gRPC wire messages (defined in `proto/controller.proto` and `proto/proxy.proto`). JSON via `serde` for API responses, Postgres JSONB columns, and debugging.
- Field naming: **`snake_case` everywhere on the wire** — proto3 convention for gRPC messages and, intentionally, the same convention for the public REST API. Serde-serialized Rust types at API boundaries (e.g. `CreateRequest`, `SandboxInfo` in `crates/api/src/service.rs`) use Rust's default `snake_case` field naming — no `#[serde(rename_all = "camelCase")]` — so external clients can read a single naming convention across REST and gRPC. (Earlier revisions of this doc claimed camelCase for API JSON, but the implementation has never matched that claim; ratified as snake_case here so the doc and the wire agree.)
- Unknown field handling: Proto3 preserves unknown fields by default. Serde types are `#[serde(deny_unknown_fields)]` at API boundaries, permissive internally.

### Versioning

- The contracts crate is versioned independently from any consumer.
- Semver applies: breaking changes require a major bump; additive changes require a minor bump.
- Public enums are marked `#[non_exhaustive]` so adding variants is a minor change, not a breaking one.
- Proto messages evolve via proto3's additive field rules: new fields get new field numbers, old fields are never reused.

### Error types

- Library errors use `thiserror`.
- Errors that cross the contract boundary carry a stable kind (an enum variant) plus an unstable detail message.
- Consumers match on the kind; they do not parse the detail message.
- All error enums are `#[non_exhaustive]` to allow adding variants without a major version bump.

### Newtypes over primitives

Every domain identifier is wrapped:

```rust
pub struct AgentId(pub Uuid);
pub struct SandboxId(pub Uuid);
pub struct JoinToken(pub String);  // Display redacts the value
pub struct ApiKey(pub String);     // Display redacts the value
```

This is enforced by the smells checklist in `ENGINEERING_DISCIPLINE.md`. Bare `Uuid` or `String` in function signatures where a domain type exists is a code smell.

## Contracts inventory

### Controller service (`controller.proto`)

- **Producer:** Controller
- **Consumers:** Agent
- **Purpose:** Bidirectional gRPC stream for agent registration, heartbeats, sandbox lifecycle commands, and status reporting.
- **Shape:** see `proto/controller.proto`
- **Key messages:**
  - `RegisterRequest` / `RegisterResponse` — agent joins the fleet
  - `Heartbeat` / `HeartbeatAck` — liveness signal (interval: 5s per `constants::HEARTBEAT_INTERVAL`)
  - `StartSandbox` / `StopSandbox` — sandbox lifecycle commands from controller
  - **v1.0.2** `PauseSandbox` / `UnpauseSandbox` — freeze / resume the
    in-container processes without destroying the container, tunnel, or
    exec-registry state. Dispatched by the controller's management
    `PauseSandbox` / `UnpauseSandbox` RPCs. The agent calls the runtime's
    pause primitive and reports back via `SandboxStatus` with state
    `SANDBOX_STATE_PAUSED` (on pause) or `SANDBOX_STATE_RUNNING` (on
    unpause).
  - `SandboxStatus` — agent reports sandbox state changes. v1.0.2 adds
    the `PAUSING` / `PAUSED` / `UNPAUSING` enum variants (field
    numbers 6/7/8 in the `SandboxState` enum). Old clients that
    pattern-match on the legacy variants round-trip the new ones as
    their integer values.
  - `ResourceReport` — agent reports available capacity
  - `FetchLogsCommand` — operator-initiated log fetch
- **Note:** `ExecCommand` / `ExecResult` were removed in v1.0. Exec is no longer routed through the controller; it flows on the proxy's data plane via `SandboxIoService.OpenIoStream`. The controller stream is now lifecycle-only.
- **Invariants:** `agent_id` in `Heartbeat` and `ResourceReport` must match the `agent_id` from the initial `RegisterRequest` on the same stream. `SandboxStatus` may only be sent for sandboxes that the controller has assigned to this agent via `StartSandbox`.
- **Compatibility:** Adding new `oneof` variants to `AgentMessage` or `ControllerCommand` is additive (minor bump). Changing existing message fields is breaking (major bump).

### Sandbox I/O service (`proxy.proto` — `SandboxIoService`)

The proxy's gRPC service, renamed in v1.0 from `TunnelService` to reflect its broadened role as the platform's sandbox I/O multiplex (not just HTTP forwarding).

**Two RPCs:**

1. **`OpenTunnel`** — agent → proxy long-lived reverse tunnel.
   - **Producer:** Proxy (sends `TunnelRequest` to agents)
   - **Consumers:** Agent (sends `TunnelResponse` back)
   - **Purpose:** Carries BOTH inbound public HTTP forwarding AND gateway-originated I/O sessions, multiplexed as typed oneof variants on the same envelope.
   - **Key messages:** `TunnelReady` (agent identifies on stream open), `HttpRequest` / `HttpResponse` (HTTP forwarding), `DataChunk` (streaming body), `StreamClose` (virtual stream end), and in v1.0 `IoClientFrame` / `IoServerFrame` (sandbox I/O).
   - **Invariants:** `stream_id` correlates messages within a single tunnel. The proxy assigns `stream_id` for both HTTP and I/O virtual streams.

2. **`OpenIoStream`** — gateway → proxy I/O session (new in v1.0).
   - **Producer:** API gateway (sends `IoClientFrame`)
   - **Consumer:** Proxy → bridges into the owning agent's `OpenTunnel` → agent emits `IoServerFrame` back.
   - **Purpose:** Originate a single bidirectional I/O session for exec or file read. First `IoClientFrame` MUST be `IoStart` carrying `sandbox_id` and op-specific parameters; the proxy routes by `sandbox_id` to the agent that owns the sandbox and bridges the streams.
   - **Key messages:** `IoStart` (with `ExecParams`, `ReadFileParams`, `WriteFileParams`, `WriteFilesTargzParams`, **v1.0.3** `ListDirParams`, **v1.0.3** `WaitPortListeningParams` via oneof), `IoStarted`, stdin/stdout/stderr bytes, `IoSignal`, `IoClose`, `IoExited`, `IoError`, **v1.0.3** `ListDirResult`, **v1.0.3** `WaitPortListeningResult`, **v1.0.3** `FileMeta`.
   - **Invariants:** Exactly one `IoStart` per session, always the first client frame. Exactly one terminator (`IoExited` or `IoError`) per session — **v1.0.3 does NOT change this**: `FileMeta` is a sidecar, never a terminator. On `ReadFile` sessions the agent emits `FileMeta` before the first `Stdout` chunk (so the UI captures the revision before consuming bytes). On `WriteFile` sessions the agent emits `FileMeta` *before* `IoExited`, never instead of it; v1.0.2 consumers that block on `IoExited` keep working unchanged. `stream_id` is the agent-side `ExecRegistry` key — the agent uses it to track the in-container PID for kill-on-disconnect cleanup.
- **Compatibility:** Adding new I/O ops via new oneof variants in `IoStart.params` is additive (minor bump). **v1.0.3** added `ListDir` (tag 6) and `WaitPortListening` (tag 7) under this rule, plus three new server-side payload variants (`ListDirResult` tag 7, `WaitPortListeningResult` tag 8, `FileMeta` tag 9), plus `expected_revision` (tag 3) and `force` (tag 4) on `WriteFileParams` — empty `expected_revision` preserves the pre-v1.0.3 "no precondition" behavior.

### Sandbox management service (`api.proto`)

- **Producer:** Controller (implements the gRPC server)
- **Consumers:** API gateway (calls via tonic gRPC client)
- **Purpose:** Unary RPCs for external sandbox lifecycle management. Deliberately separate from the bidirectional `AgentStream` — the agent stream is for persistent agent connections, this service is for request/response client interactions.
- **Shape:** see `proto/api.proto`
- **Key messages:**
  - `CreateSandboxRequest` / `CreateSandboxResponse` — create a sandbox, returns sandbox_id + subdomain + status (initially `creating`)
  - `GetSandboxRequest` / `GetSandboxResponse` — query sandbox status
  - `ListSandboxesRequest` / `ListSandboxesResponse` — enumerate all sandboxes the caller owns
  - `DeleteSandboxRequest` / `DeleteSandboxResponse` — stop and remove a sandbox
  - **v1.0.2** `PauseSandboxRequest` / `PauseSandboxResponse` — freeze the
    in-container processes via the runtime's pause primitive (Docker
    pause / cgroup-v2 freezer). Returns the optimistic transition state
    (`status="pausing"`); the steady-state `paused` arrives in
    subsequent `GetSandbox` responses once the agent has acknowledged.
    Public REST surface: `POST /v1/sandboxes/{id}/pause` → 202 Accepted.
    Idempotent on the steady-state side (pausing an already-paused
    sandbox is a no-op success).
  - **v1.0.2** `UnpauseSandboxRequest` / `UnpauseSandboxResponse` —
    inverse of `PauseSandbox`. `POST /v1/sandboxes/{id}/unpause` → 202.
    Idempotent on running sandboxes.
- **Note:** `ExecSandbox` RPC was removed in v1.0. Public exec is now a WebSocket session on the gateway (`WS /v1/sandboxes/{id}/exec`) backed by the proxy's `OpenIoStream`.
- **Invariants:** `sandbox_id` in responses matches the ID from the create or get request. `subdomain` is always the first 12 hex chars of the sandbox UUID.
- **Compatibility:** Adding new RPC methods is additive (minor bump). Changing existing message fields is breaking (major bump).

### Exec lifecycle (v1.0 — streaming on the data plane)

- **Path:** API gateway → proxy (`OpenIoStream`) → agent (via the existing `OpenTunnel` reverse tunnel, multiplexed as an `io_client` / `io_server` virtual stream) → runtime (docker or youki) → in-container process.
- **Identifiers:** the proxy assigns `stream_id` per session; the agent's runtime assigns `exec_id` per started process. The `ExecRegistry` is keyed on `stream_id`; `exec_id` is carried in `IoStarted` for diagnostic correlation only.
- **Connection-bound lifetime:** closing the upstream WebSocket → gateway closes its `OpenIoStream` → proxy closes the virtual stream into the agent → agent's `drive_io_session` sees end-of-stream on its `IoClientFrame` source → invokes `exec_registry::on_stream_closed`, which SIGTERMs (then SIGKILLs after `EXEC_KILL_GRACE`) the in-container PID via the runtime trait.
- **No global timeout:** sessions live as long as the WebSocket. Idle keepalive is application-level WebSocket ping/pong every `WS_IDLE_PING_INTERVAL` (30s); peer goes after `WS_IDLE_PING_TIMEOUT` (60s) of unanswered pings.
- **Error reporting:** runtime-level failures during a live session arrive as `IoError` frames on the stream itself; `command_not_found` is signalled via `IoExited { exit_code: 127, command_not_found: true }`. The HTTP-layer `ApiError` only models failures that happen BEFORE the I/O stream is established (auth, sandbox lookup) or that the gateway observes between WS upgrade and stream open.

### Domain types (`types.rs`)

- **Producer:** Contracts crate
- **Consumers:** Controller, Proxy, Agent
- **Purpose:** Typed domain identifiers and shared data structures.
- **Key types:**
  - `AgentId` — wraps `Uuid`, identifies an agent across reconnections
  - `SandboxId` — wraps `Uuid`, provides `subdomain()` for the first 12 hex chars
  - `JoinToken` — wraps `String`, `Display` implementation redacts value for safe logging
  - `ApiKey` — wraps `String`, `Display` implementation redacts value for safe logging
  - `RoutingEntry` — `(sandbox_id, agent_id)` tuple used by controller and proxy
- **Invariants:** `SandboxId::subdomain()` must produce a valid DNS label (lowercase alphanumeric, max 63 chars). The 12-char hex prefix from UUIDv4 satisfies this.

### Error types (`error.rs`)

- **Raised by:** Controller (`ControllerError`), Proxy (`ProxyError`), Agent (`AgentError`)
- **Observed by:** Callers of each component's public API
- **Kinds:**
  - `ControllerError`: `InvalidToken`, `AgentNotFound`, `SandboxNotFound`, `NoAvailableAgents`, `Database`, `Internal`
  - `ProxyError`: `RoutingMiss`, `TunnelUnavailable`, `UpstreamTimeout`, `UpstreamRejected`, `Internal`
  - `AgentError`: `ControllerDisconnected`, `TunnelDisconnected`, `Runtime`, `SandboxNotFound`, `Internal`
  - `ApiError`: `Unauthorized`, `SandboxNotFound`, `ControllerUnavailable`, `ProxyUnavailable`, `InvalidRequest`, `InvalidUpload`, `IoStreamFailed`, `SandboxGone`, `FileNotFound`, `Internal`
- **Error codes:** `ApiError` exposes `fn error_code(&self) -> &'static str` that maps each variant to a stable uppercase string identifier (`UNAUTHORIZED`, `SANDBOX_NOT_FOUND`, `CONTROLLER_UNAVAILABLE`, `PROXY_UNAVAILABLE`, `INVALID_REQUEST`, `INVALID_UPLOAD`, `IO_STREAM_FAILED`, `SANDBOX_GONE`, `FILE_NOT_FOUND`, `INTERNAL_ERROR`). These codes are included in REST API error response JSON bodies as the `error_code` field for programmatic handling. In v1.0, `ExecFailed` and `CommandNotFound` are no longer `ApiError` variants — exec failure modes are reported via `IoError` and `IoExited{command_not_found:true}` frames on the WebSocket I/O stream itself. `FileNotFound.resolved_path` continues to carry the absolute path the agent attempted to read.
- **Retry guidance:**
  - Retryable: `Database` (transient), `TunnelUnavailable` (agent may reconnect), `UpstreamTimeout` (sandbox may be slow)
  - Terminal: `InvalidToken`, `AgentNotFound`, `SandboxNotFound`, `RoutingMiss`, `NoAvailableAgents`
  - Ambiguous: `Internal`, `Runtime` (may be transient or persistent depending on cause)

### Constants (`constants.rs`)

- **Producer:** Contracts crate
- **Consumers:** All binaries
- **Purpose:** Shared timing and resource constants that must be consistent across components.
- **Key values:**
  - `HEARTBEAT_INTERVAL`: 5 seconds
  - `DEAD_AGENT_THRESHOLD`: 3 missed heartbeats
  - `UPSTREAM_TIMEOUT`: 30 seconds
  - `ROUTING_CACHE_REFRESH_INTERVAL`: 60 seconds (fallback for LISTEN/NOTIFY)
  - `RECONNECT_BASE_DELAY`: 1 second (exponential backoff start)
  - `RECONNECT_MAX_DELAY`: 30 seconds (backoff ceiling)
  - `DEFAULT_SANDBOX_CPU_MILLICORES`: 1000 (1 core)
  - `DEFAULT_SANDBOX_MEMORY_BYTES`: 512 MB
  - `PROXY_STARTUP_RETRY_ATTEMPTS`: 15 attempts
  - `PROXY_STARTUP_RETRY_INTERVAL`: 2 seconds
  - `DEFAULT_WRITE_CWD`: `/home` (default target directory for file writes when no explicit cwd is provided)
  - `DEFAULT_SANDBOX_ENTRYPOINT`: `["sleep", "infinity"]` (overrides image CMD/ENTRYPOINT to keep sandbox idle for exec-based interaction)
  - `WS_IDLE_PING_INTERVAL`: 30 seconds (gateway → client WebSocket ping cadence on idle exec sessions)
  - `WS_IDLE_PING_TIMEOUT`: 60 seconds (peer-gone threshold; triggers ExecRegistry cleanup)
  - `EXEC_KILL_GRACE`: 5 seconds (between SIGTERM and SIGKILL when the registry hook fires)
  - `WS_AUTH_PROTOCOL_SENTINEL`: `"open-sandbox.v1"` — the subprotocol the server echoes on successful WS subprotocol auth (see §WebSocket auth)
  - `WS_AUTH_BEARER_PREFIX`: `"bearer."` — case-insensitive prefix marking a subprotocol entry that carries a base64url-encoded API key
  - `WS_AUTH_MAX_OFFERED_PROTOCOLS`: 16 — per-request cap on offered-protocol entries the auth helper inspects

### WebSocket auth (REST gateway, v1.0.x)

Every WebSocket upgrade on the gateway (`/v1/sandboxes/{id}/exec`,
`/v1/sandboxes/{id}/files/read-stream`) is bearer-authenticated. There
are two equivalent paths:

1. **`Authorization: Bearer <api_key>`** — preferred for programmatic
   clients (CLIs, SDKs, server-to-server).
2. **`Sec-WebSocket-Protocol: open-sandbox.v1, bearer.<base64url-no-pad(api_key)>`**
   — required for browsers, which cannot set `Authorization` on a
   `WebSocket` constructor. The client offers both entries; the server
   validates the bearer entry and echoes ONLY the sentinel
   (`open-sandbox.v1`) back in the 101 response, so the API key never
   appears in a response header / access log / DevTools network panel.

Both paths are tried per request; a wrong `Authorization` header does
not lock out a request that also presents a valid subprotocol (this
tolerates proxies that inject stale Authorization). On failure both
paths surface the same 401 body:

```json
{ "error": "missing or invalid API key", "error_code": "UNAUTHORIZED" }
```

The base64url-no-padding encoding is mandatory for the subprotocol
path: WebSocket subprotocol values must satisfy the RFC 7230 token
grammar (no `=`, `+`, `/`, `,`, whitespace), so API keys containing
those characters can only be carried over the subprotocol path after
encoding. The encoding is applied client-side; the server decodes
nothing — it compares the offered string against
`base64url-no-pad(state.api_key)` in constant time.

The `bearer.` prefix is matched case-insensitively (HTTP scheme
tradition); `Bearer.<…>`, `BEARER.<…>`, etc. all work.

### CORS (REST gateway, opt-in)

Set `OPEN_SANDBOX_API_CORS_ORIGINS` to enable CORS for development
consoles served from a different origin than the API. Unset → no CORS
layer is installed (correct production default for single-origin
deployments).

- Comma-separated list of allowed origins, e.g.
  `http://localhost:8090,https://console.dev`.
- A sole value of `*` activates wildcard CORS. Mixed `*` + explicit
  origins logs a WARN and treats the value as the explicit allowlist
  (the `*` is ignored) — silent wildcard escalation is prevented.
- Surrounding ASCII quotes are stripped per entry, so YAML/Helm-style
  `'"https://x"'` normalizes to `https://x`.
- Empty / all-invalid input produces an ERROR log and no layer.

WebSocket upgrades are not subject to CORS by browser policy, so this
layer only affects the REST routes.

## Component-to-contract matrix

| Component     | Produces                                                   | Consumes                                                  |
|---------------|------------------------------------------------------------|-----------------------------------------------------------|
| API Gateway   | REST/WS responses, `SandboxManagement` RPCs (as client to controller), `IoClientFrame` (as client to proxy via `OpenIoStream`) | `SandboxManagement` RPC responses, `IoServerFrame`        |
| Controller    | `ControllerCommand`, `RegisterResponse`, `RoutingEntry`, `SandboxManagement` RPC responses | `AgentMessage`, `RegisterRequest`, `SandboxManagement` RPCs |
| Proxy         | `TunnelRequest` (HTTP and `io_client` variants), HTTP responses, `IoServerFrame` (to gateway) | `TunnelResponse`, `IoClientFrame` (from gateway), `RoutingEntry` (via PG) |
| Agent         | `AgentMessage`, `RegisterRequest`, `TunnelResponse` (HTTP and `io_server` variants) | `ControllerCommand`, `TunnelRequest` (HTTP and `io_client` variants) |

---

## Freeze gate

Before tagging `contracts/v0.1.0-frozen`:

- [x] Confidence self-assessment is "high" with gaps resolved
- [x] `crates/contracts/` compiles cleanly with `cargo check`
- [x] Every component in `SAD.md` has its produced and consumed contracts represented in the crate
- [x] The component-to-contract matrix above matches the crate
- [ ] `cargo test -p open-sandbox-contracts` passes (even if only doctests)

```
Confidence: high
Residual risks:
  - Proto message design is based on anticipated usage patterns; actual usage during implementation may reveal missing fields or awkward ergonomics. The #[non_exhaustive] and proto3 additive-field policies mitigate this — amendments are minor bumps, not major.
  - The TunnelService uses encapsulated HTTP (HttpRequest/HttpResponse messages) rather than raw byte forwarding. This adds parsing overhead but gives the proxy visibility into request metadata for routing and observability. If performance is insufficient, a raw-bytes mode can be added as a new oneof variant (minor bump).
Known gaps:
  - None blocking. All components' contract surfaces are represented in the crate.
```

Once all boxes are checked and confidence is high, commit with `docs: contracts` and tag `contracts/v0.1.0-frozen`.
