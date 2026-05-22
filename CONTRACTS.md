# Contracts

> This document is the prose companion to the contracts crate at `crates/contracts/`. The crate is the source of truth — types compile, prose does not. This document explains the *why* and the cross-cutting policies that the types themselves cannot express.

## Status

Current frozen version: **contracts/v0.7.0-frozen**

*Frozen at `contracts/v0.7.0-frozen` on 2026-05-22 (paired with `spec/v0.7.0`). Changes require a `contracts/amendment-<desc>` branch and a version bump.*

v0.7.0 is a **breaking** amendment introducing SDK-agent ergonomics fixes from the 10-item friction report: `ListSandboxes` RPC; `cwd` field on `ExecCommand`/`ExecSandboxRequest`; `command_not_found` on `ExecResult`/`ExecSandboxResponse`; new `ApiError` variants `InvalidRequest`, `InvalidUpload`, `CommandNotFound`; renamed `ApiError::FileNotFound.path` → `resolved_path` (callers were relying on `.path` — recompile required).

## Cross-cutting policies

### Serialization

- Format: Protocol Buffers for gRPC wire messages (defined in `proto/controller.proto` and `proto/proxy.proto`). JSON via `serde` for API responses, Postgres JSONB columns, and debugging.
- Field naming: Proto messages use `snake_case` per proto3 convention. Serde-serialized Rust types use `#[serde(rename_all = "camelCase")]` for API-facing JSON only; internal types use Rust's default `snake_case`.
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
  - `SandboxStatus` — agent reports sandbox state changes
  - `ResourceReport` — agent reports available capacity
  - `ExecCommand` / `FetchLogsCommand` — operator-initiated sandbox operations
- **Invariants:** `agent_id` in `Heartbeat` and `ResourceReport` must match the `agent_id` from the initial `RegisterRequest` on the same stream. `SandboxStatus` may only be sent for sandboxes that the controller has assigned to this agent via `StartSandbox`.
- **Compatibility:** Adding new `oneof` variants to `AgentMessage` or `ControllerCommand` is additive (minor bump). Changing existing message fields is breaking (major bump).

### Tunnel service (`proxy.proto`)

- **Producer:** Proxy (sends `TunnelRequest` to agents)
- **Consumers:** Agent (sends `TunnelResponse` back)
- **Purpose:** Reverse tunnel for forwarding public HTTP requests through the agent's outbound connection to local sandbox containers.
- **Shape:** see `proto/proxy.proto`
- **Key messages:**
  - `TunnelReady` — agent sends on stream open to identify itself
  - `HttpRequest` / `HttpResponse` — encapsulated HTTP request/response
  - `DataChunk` — streaming body data for large payloads
  - `StreamClose` — signals end of a virtual stream
- **Invariants:** `stream_id` correlates request and response messages within a single tunnel. Each `stream_id` is unique within the lifetime of a tunnel connection. The proxy assigns `stream_id` values; the agent echoes them.
- **Compatibility:** Same rules as controller service.

### Sandbox management service (`api.proto`)

- **Producer:** Controller (implements the gRPC server)
- **Consumers:** API gateway (calls via tonic gRPC client)
- **Purpose:** Unary RPCs for external sandbox lifecycle management. Deliberately separate from the bidirectional `AgentStream` — the agent stream is for persistent agent connections, this service is for request/response client interactions.
- **Shape:** see `proto/api.proto`
- **Key messages:**
  - `CreateSandboxRequest` / `CreateSandboxResponse` — create a sandbox, returns sandbox_id + subdomain + status (initially `creating`)
  - `GetSandboxRequest` / `GetSandboxResponse` — query sandbox status
  - `ListSandboxesRequest` / `ListSandboxesResponse` — enumerate all sandboxes the caller owns (added v0.7.0)
  - `DeleteSandboxRequest` / `DeleteSandboxResponse` — stop and remove a sandbox
  - `ExecSandboxRequest` / `ExecSandboxResponse` — run a command, returns stdout/stderr/exit_code. `cwd` (field 4, added v0.7.0) sets the working directory; empty string means default (`/home`). `stdin` (field 3) is the bytes written to the process's stdin before close. `command_not_found` (response field 4, added v0.7.0) is true when the runtime reported the executable was missing — distinguishes "command not found" from a process that ran and exited 127 of its own accord.
- **Invariants:** `sandbox_id` in responses matches the ID from the create or get request. `subdomain` is always the first 12 hex chars of the sandbox UUID. `ExecSandboxResponse` blocks until the command completes or the exec timeout (60s) is reached. When `command_not_found` is true, `exit_code` is 127 and `stderr` contains the runtime's "executable file not found" message; `stdout` is never used to carry runtime-level errors.
- **Compatibility:** Adding new RPC methods is additive (minor bump). Changing existing message fields is breaking (major bump).

### Exec result flow (`ExecResult` in `controller.proto`)

- **Producer:** Agent (sends exec output back to controller)
- **Consumer:** Controller (correlates by `exec_id` and delivers to waiting API request)
- **Purpose:** Closes the exec loop: API → Controller → Agent (ExecCommand) → Agent executes → Agent → Controller (ExecResult) → API → Client.
- **Key fields:** `exec_id` correlates the result with the originating `ExecCommand`. `exit_code`, `stdout`, `stderr` carry the command output. `error` (field 6, added in v0.6.0) carries a runtime-level error message when the exec infrastructure itself fails (container not found, exec API error). When `error` is non-empty, `exit_code` is -1, `stdout`/`stderr` are empty, and the controller returns a gRPC Internal error rather than forwarding the result as a successful exec. `command_not_found` (field 7, added in v0.7.0) is true when the runtime determined the executable was missing; in that case `exit_code` is 127, `stderr` carries the runtime's diagnostic line, and `stdout` is empty.
- **Invariant:** `exec_id` is unique per exec invocation (UUID). The controller holds a pending-exec map keyed by `exec_id`; when `ExecResult` arrives, the waiting API request is unblocked.

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
  - `ApiError`: `Unauthorized`, `SandboxNotFound`, `ControllerUnavailable`, `InvalidRequest`, `InvalidUpload`, `ExecFailed`, `CommandNotFound`, `FileNotFound`, `Internal`
- **Error codes:** `ApiError` exposes `fn error_code(&self) -> &'static str` that maps each variant to a stable uppercase string identifier (`UNAUTHORIZED`, `SANDBOX_NOT_FOUND`, `CONTROLLER_UNAVAILABLE`, `INVALID_REQUEST`, `INVALID_UPLOAD`, `EXEC_FAILED`, `COMMAND_NOT_FOUND`, `FILE_NOT_FOUND`, `INTERNAL_ERROR`). These codes are included in REST API error response JSON bodies as the `error_code` field for programmatic handling. `INVALID_UPLOAD` covers empty/malformed tar.gz bodies for `write_files` (returned as HTTP 400). `COMMAND_NOT_FOUND` is returned with HTTP 200 (since the exec request itself was valid) but the response envelope carries this code to disambiguate from a process that ran and exited 127. `FileNotFound.resolved_path` carries the absolute path the agent attempted to read, so callers can debug path issues without a second round-trip.
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

## Component-to-contract matrix

| Component     | Produces                                                   | Consumes                                                  |
|---------------|------------------------------------------------------------|-----------------------------------------------------------|
| API Gateway   | REST responses, `SandboxManagement` RPCs (as client)       | `SandboxManagement` RPC responses (from controller)       |
| Controller    | `ControllerCommand`, `RegisterResponse`, `RoutingEntry`, `SandboxManagement` RPC responses | `AgentMessage`, `RegisterRequest`, `SandboxManagement` RPCs, `ExecResult` |
| Proxy         | `TunnelRequest`, HTTP responses                            | `TunnelResponse`, `RoutingEntry` (via PG)                 |
| Agent         | `AgentMessage`, `RegisterRequest`, `TunnelResponse`, `ExecResult` | `ControllerCommand`, `TunnelRequest`                      |

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
