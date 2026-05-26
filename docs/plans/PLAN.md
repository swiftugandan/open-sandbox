# Implementation Plan

> **⚠️ Mostly shipped — historical reference + structural map.**
>
> This document was the executable plan written at `contracts/v0.3.0-frozen`
> and drove the original binary decomposition. Every binary listed below
> has shipped to `main`; the v1.0 streaming-exec reshape (see
> `EXEC_STREAMING_DESIGN.md` + `PLAN_EXEC_STREAMING.md`) and the v1.0.1 /
> v1.0.2-#13 amendments have layered on top. The structural map (the DAG
> + the per-binary acceptance criteria) is still useful as a high-level
> reference. Do NOT treat the prerequisite checkboxes as live state —
> see `CONTRACTS.md` for the current contracts version.
>
> If you're planning a *new* binary or a *new* amendment, draft a fresh
> plan document. Operator-facing changelog: `CHANGELOG.md`.

> Decomposition of the system into binaries. Each binary depends only on the frozen contracts crate and on lower-level binaries through their published contracts. This is what makes "one binary at a time, protected by contracts" actually work.

## Prerequisites (historical — original plan; current contracts version is `v1.0.2`)

- [x] `contracts/v0.3.0-frozen` tag exists
- [x] `SPEC.md`, `SAD.md`, `CONTRACTS.md` are committed and tagged
- [x] Final confidence gate (below) was "high" — plan executed

## Dependency DAG

```
  open-sandbox-contracts (frozen at v0.3.0)
       │
   ┌───┼──────────┬──────┬──────────────┬───────────────┐
   │   │          │      │              │               │
   ▼   ▼          ▼      ▼              ▼               ▼
 agent controller proxy  api     agent-docker(MOVED) agent-youki(NEW)
   │       │      │      │              │               │
   └───┬───┘──────┘──────┘──────────────┘───────────────┘
       │
       ▼
    open-sandbox (CLI — feature-gated runtime selection)
```

No cycles. Each component depends only on `contracts` (and `agent` for the runtime crates, which defines the `ContainerRuntime` trait). The final `open-sandbox` binary is the shell that dispatches to subcommands; runtime selection is compile-time via Cargo features (`docker` default, `youki` for Linux production).

## Implementation order

> Sorted by dependency and by ability to test in isolation. Components with no peer dependencies are implemented first.

### 1. `contracts` (already frozen)

- **Depends on:** nothing
- **Status:** frozen at `contracts/v0.1.0-frozen`

### 2. `controller`

- **Depends on:** `contracts` only
- **Consumes contracts:** `AgentMessage`, `RegisterRequest` (from agents, received via gRPC)
- **Produces contracts:** `ControllerCommand`, `RegisterResponse`, `RoutingEntry`
- **Acceptance criterion (live e2e):** Given a mock agent that sends a valid `RegisterRequest` with a correct join token, the controller accepts the registration, stores the agent in Postgres, and responds with `RegisterResponse { accepted: true }`. Given subsequent `Heartbeat` messages, the controller responds with `HeartbeatAck`. Given a `CreateSandbox` API call, the controller selects an agent, writes a `RoutingEntry` to Postgres (triggering NOTIFY), and sends `StartSandbox` to the agent. Given 3 missed heartbeats, the controller marks the agent dead and removes its routing entries.
- **Estimated complexity:** L
- **Risks:**
  - gRPC bidirectional stream management with tonic is the most complex networking pattern in the system
  - Postgres LISTEN/NOTIFY integration needs careful connection management (separate connection for LISTEN)
  - Scheduler logic (agent selection) needs to handle edge cases (all agents full, agents dying mid-assignment)

### 3. `agent`

- **Depends on:** `contracts` only
- **Consumes contracts:** `ControllerCommand`, `TunnelRequest`
- **Produces contracts:** `AgentMessage`, `RegisterRequest`, `TunnelResponse`
- **Acceptance criterion (live e2e):** Given a running controller and proxy, the agent binary starts with a valid join token, registers successfully, begins heartbeating, receives a `StartSandbox` command, creates a Docker container, reports `SandboxStatus(running)`, establishes a reverse tunnel to the proxy, and forwards a tunneled HTTP request to the container's exposed port and returns the response.
- **Estimated complexity:** L
- **Risks:**
  - Docker Engine API integration (container lifecycle, log streaming)
  - Dual gRPC connection management (controller + proxy) with independent reconnection logic
  - Reconciliation on restart (what containers are already running vs what the controller thinks)

### 4. `proxy`

- **Depends on:** `contracts` only
- **Consumes contracts:** `TunnelResponse`, `RoutingEntry` (via Postgres read + LISTEN/NOTIFY)
- **Produces contracts:** `TunnelRequest`
- **Acceptance criterion (live e2e):** Given a Postgres routing table with an entry mapping sandbox `abc123` to agent `worker-7`, and agent `worker-7` connected via reverse tunnel, an HTTPS request to `abc123.sandbox.example.com` is routed through the tunnel to the agent, which forwards it to the local container, and the response is returned to the client with ≤ 5ms proxy-added latency at p99.
- **Estimated complexity:** L
- **Risks:**
  - TLS termination with wildcard cert and hot-reload on renewal
  - HTTP/2 stream multiplexing over agent tunnels under concurrent load
  - Routing cache consistency (stale cache → 502 errors; LISTEN/NOTIFY + 60s fallback mitigates)

### 5. `open-sandbox` (CLI shell)

- **Depends on:** `contracts`, `controller`, `agent`, `proxy`
- **Consumes contracts:** all (transitively)
- **Produces contracts:** none (this is the entry point)
- **Acceptance criterion (live e2e):** `open-sandbox controller` starts the controller, `open-sandbox proxy` starts the proxy, `open-sandbox agent --token <TOKEN>` starts the agent. All three subcommands respect CLI flags, env vars, and config file. `--help` is accurate. `--version` reports the contracts crate version.
- **Estimated complexity:** S
- **Risks:** Minimal — this is plumbing (clap subcommand dispatch).

### 6. `api` (API gateway)

- **Depends on:** `contracts` only (communicates with controller via gRPC, not via Rust imports)
- **Consumes contracts:** `SandboxManagementService` gRPC (from controller via `api.proto`)
- **Produces contracts:** REST responses (JSON for metadata/exec, octet-stream for file reads)
- **Implementation scope:**
  - New crate at `crates/api/` — axum HTTP server, tonic gRPC client
  - New `open-sandbox api` subcommand in the CLI crate
  - Controller amendment: implement `SandboxManagementService` server, exec result correlation (pending exec map keyed by `exec_id`)
  - Agent amendment: handle `ExecCommand` with `exec_id`, send `ExecResult` back through the agent stream
- **Acceptance criterion (live e2e):** Given a running controller with a connected agent, `POST /v1/sandboxes` creates a Docker container on the agent and returns a sandbox ID. `POST /v1/sandboxes/:id/exec` with `{"command": ["echo", "hello"]}` returns `{"exit_code": 0, "stdout": "hello\n"}`. `DELETE /v1/sandboxes/:id` stops the container. All verified against real controller + agent + Docker.
- **Estimated complexity:** L
- **Risks:**
  - Exec result correlation requires the controller to hold pending requests keyed by `exec_id` and deliver results when `ExecResult` arrives from the agent. Timeout handling is critical — a hung command must not leak a waiting request forever.
  - File operations via exec depend on `tar` being available in the sandbox container image. Most base images include it but it's not guaranteed.

### 7. `infra` (Pulumi stack)

- **Depends on:** compiled `open-sandbox` binary (uploaded to object storage or built on cloud-init)
- **Consumes contracts:** none (infrastructure, not Rust)
- **Produces:** running platform on target cloud
- **Acceptance criterion (live e2e):** `pulumi up` on a clean Hetzner account provisions the controller VM, 2 worker VMs, Postgres, DNS records, and TLS cert. A BYO agent from a developer's laptop can join via the install script. A sandbox is created and accessible at `<id>.sandbox.example.com`.
- **Estimated complexity:** L
- **Risks:**
  - Cloud provider API quirks (Hetzner's API for floating IPs, firewall rules)
  - Cloud-init reliability (agent binary download, systemd unit installation)
  - DNS propagation delay for wildcard records
  - Let's Encrypt DNS-01 challenge timing with Cloudflare
- **Amendment for youki:** Worker cloud-init replaces Docker installation with CNI plugin installation. The systemd unit drops `After=docker.service` / `Requires=docker.service`. Binary is built with `--features youki`.

### 8a. `agent-docker` (extract DockerRuntime from CLI)

- **Depends on:** `contracts`, `agent` (for `ContainerRuntime` trait)
- **Lives in:** `crates/agent-docker/`
- **Consumes contracts:** `AgentError::Runtime` (error variant)
- **Produces:** `DockerRuntime` implementing `ContainerRuntime` trait
- **Implementation scope:** Mechanical extraction — move `crates/cli/src/docker_runtime.rs` to `crates/agent-docker/src/lib.rs`. No logic changes. Update CLI crate to depend on `agent-docker` behind the `docker` feature flag.
- **Acceptance criterion:** `cargo check -p open-sandbox-agent-docker` compiles. `cargo test -p open-sandbox` (with default `docker` feature) passes all existing tests. No behavioral change.
- **Estimated complexity:** S (mechanical refactor)

### 8b. `agent-youki` (YoukiRuntime — daemonless OCI container runtime)

- **Depends on:** `contracts` (v0.3.0), `agent` (for `ContainerRuntime` trait)
- **Lives in:** `crates/agent-youki/`
- **Consumes contracts:** `AgentError::Runtime` (error variant)
- **Produces:** `YoukiRuntime` implementing `ContainerRuntime` trait
- **Implementation scope:**
  - `lib.rs` — `YoukiRuntime` struct with `ContainerRuntime` impl (~200 lines)
  - `image.rs` — OCI image pull and unpack via `oci-client` (~150 lines)
  - `cni.rs` — CNI plugin invocation, bridge+portmap conflist, dynamic port allocation (~450 lines)
  - `spec.rs` — OCI spec generation from `ContainerConfig` (~80 lines)
  - `exec.rs` — `TenantContainerBuilder` with pipe-based stdio capture (~100 lines)
  - CLI amendment: `run_agent` conditionally constructs `YoukiRuntime` or `DockerRuntime` based on feature flag
- **Acceptance criterion (live e2e):** Given the agent binary compiled with `--features youki` running on a Linux VM with CNI plugins installed, the agent starts, registers with the controller, receives a `StartSandbox` command, pulls an alpine:latest image via oci-client, creates an OCI container via libcontainer with bridge+portmap networking, reports `SandboxStatus(running)`, executes a command via `exec` and returns stdout/stderr/exit_code, and the sandbox is accessible via the reverse tunnel. Stop removes the container and cleans up CNI state.
- **Estimated complexity:** L
- **Risks:**
  - CNI dynamic port allocation (bind 0, read port, close, pass to portmap) has a theoretical TOCTOU race. Benign in practice (portmap uses iptables DNAT).
  - libcontainer documentation is 14.7% — implementation relies on source reading.
  - Cannot run live tests on macOS — CI must use Linux runners or Docker-in-Docker.
  - OCI image whiteout file handling and zstd layer support are production gaps for complex images.

## Runtime feature flags (CLI crate)

The `open-sandbox` binary supports compile-time runtime selection via Cargo features:

- `docker` (default): Uses `DockerRuntime` backed by `bollard`. Suitable for macOS development and environments with Docker Engine installed.
- `youki`: Uses `YoukiRuntime` backed by `libcontainer`, `oci-client`, `oci-spec`. Requires Linux with CNI plugins. Production default for worker VMs.

Build commands:
- Dev (macOS): `cargo build` (docker feature, default)
- Production (Linux): `cargo build --features youki --no-default-features` or via Alpine Dockerfile
- Check only (macOS, youki code): `cargo check -p open-sandbox-agent-youki --target x86_64-unknown-linux-musl`

---

## Per-binary TDD cycle (applies to every entry above)

For each binary, in order:

1. Branch `module/<name>` from `main`
2. **Red:** failing tests against the contract → tag `module/<name>/red`
3. **Green:** minimal implementation → tag `module/<name>/green`
4. **Refactor:** smells checklist applied → tag `module/<name>/refactored`
5. **E2E (mocked peers):** → tag `module/<name>/e2e-mock`
6. **E2E (live peers):** → tag `module/<name>/live-verified`
7. Merge to `main` → tag `module/<name>/done`

See `ENGINEERING_DISCIPLINE.md` for the full cycle definition.

## Status snapshot

> This section is maintained by querying git, not by hand. Run:
>
> ```sh
> git tag --list 'module/*'
> ```

---

## Final confidence gate

```
Confidence: high
Residual risks:
  - All three core binaries (controller, agent, proxy) are estimated L complexity. The total implementation effort is substantial. The contracts freeze and TDD discipline mitigate integration risk, but calendar risk is real.
  - The Pulumi stack (module 6) depends on a working binary, so it cannot be started until at least the CLI shell (module 5) produces a runnable artifact. However, the Platform abstraction layer and cloud-init scripts can be developed in parallel with the Rust work.
  - Live e2e testing for the proxy requires a real TLS cert and DNS setup, which means the infra module (or a local dev equivalent) must exist before proxy live-e2e can complete.
  - agent-youki can only be fully built and tested on Linux. macOS dev iteration is limited to cargo check with musl target. CI must use Linux runners.
  - libcontainer documentation is sparse (14.7% coverage); implementation relies on reading youki source.
Known gaps:
  - None blocking. The DAG is acyclic, all contracts surfaces are covered, and every acceptance criterion is stated as a testable contract-boundary assertion.
```

Once confidence is high, commit with `docs: implementation plan` and tag `plan/v0.1.0`. Phase 6 (implementation) may begin.

Amended with agent-docker extraction, agent-youki module, and feature flag strategy. Tagged `plan/v0.2.0`.

### Module 9: `ops-resilience-observability` (cross-cutting amendment)

**Depends on:** `contracts` v0.4.0
**Scope:** Three targeted fixes across CLI, proxy, and API crates.

**Sub-tasks:**
1. Tracing subscriber init in CLI `main()` + lifecycle logging in all `run_*` functions + replace proxy `eprintln!` with `tracing::warn!`
2. Proxy startup retry with backoff using `PROXY_STARTUP_RETRY_ATTEMPTS` / `PROXY_STARTUP_RETRY_INTERVAL` constants
3. API error codes via `ApiError::error_code()` + `write_files` response enrichment (`WriteFilesResult`)

**Acceptance criterion:** Proxy survives starting before controller (self-heals within 30s), all components produce JSON log output with `RUST_LOG=info`, API error responses contain `error_code` field, `POST /files/write` returns `{"success": true}`.

Amended with ops-resilience-observability module (proxy startup retry, tracing, API feedback). Tagged `plan/v0.3.0`.

### Module 10: `friction-fixes` (cross-cutting amendment)

**Depends on:** `contracts` v0.5.0
**Scope:** Seven friction points found during live agent experiment, across agent-docker, controller, API, and CLI crates.

**Sub-tasks:**
1. Sandbox state tracking — `sandboxes` table in controller, process `SandboxStatus` from agents, return actual state from `GetSandbox`
2. Docker image pull — add `create_image` before `create_container` in `DockerRuntime`
3. Error message stuttering — controller puts raw sandbox ID in gRPC status messages, not formatted strings
4. Agent graceful shutdown — stop and remove all containers on SIGTERM/SIGINT
5. Axum validation errors — `ValidJson` extractor wrapping rejections into structured error envelope
6. Empty command validation — reject `{"command": []}` at API boundary with `INVALID_REQUEST`
7. Default write cwd — change from `/` to `DEFAULT_WRITE_CWD` (`/home`)

**Acceptance criterion:** Create sandbox with uncached image succeeds (agent pulls). `GetSandbox` returns actual state (`creating` → `running` or `failed`). Invalid JSON returns `{"error": "...", "error_code": "INVALID_REQUEST"}`. Error messages don't stutter. Agent shutdown cleans up containers. Write without cwd extracts to `/home`.

Amended with friction-fixes module (image pull, state tracking, validation, shutdown). Tagged `plan/v0.4.0`.

### Module 11: `ops-resilience-observability-api-feedback` (cross-cutting amendment)

**Depends on:** `contracts` v0.6.0
**Scope:** Six friction points from second round of live agent testing, across agent-docker, agent, controller, API, and CLI crates.

**Sub-tasks:**
1. Docker CMD override — set `cmd: ["sleep", "infinity"]` in `build_docker_config` to match YoukiRuntime behavior
2. ExecResult error field — add `string error = 6` to ExecResult proto, agent sets it on runtime errors, controller returns gRPC error, API returns HTTP 500
3. SIGTERM handling — add `tokio::signal::unix::signal(SignalKind::terminate())` to `shutdown_signal()`
4. Agent lifecycle logging — add tracing calls to `controller_client.rs`, `sandbox.rs`, `agent-docker/src/lib.rs`
5. FileNotFound — add `ApiError::FileNotFound` variant, detect `No such file` in `read_file` stderr, return HTTP 404
6. Container exit detection — DEFERRED to future cycle

**Acceptance criterion:** Container stays alive with `sleep infinity` entrypoint, exec works on any base image. Runtime exec errors return structured error, not stderr bytes. `docker stop` agent triggers container cleanup. Agent logs show lifecycle events. Read non-existent file returns HTTP 404 `FILE_NOT_FOUND`.

Amended with ops-resilience-observability-api-feedback module. Tagged `plan/v0.5.0`.

### Module 12: `exec-streaming` (major architectural amendment — v1.0) — **SHIPPED**

Released as `contracts/v1.0.0-frozen` (paired with `spec/v1.0.0`),
then `contracts/v1.0.1` follow-ups on `main`.

- **Architectural record:** [`EXEC_STREAMING_DESIGN.md`](../design/EXEC_STREAMING_DESIGN.md)
- **Historical plan:** [`PLAN_EXEC_STREAMING.md`](./PLAN_EXEC_STREAMING.md) (`plan/v0.6.3`)
- **Per-sub-module tags:** `module/exec-streaming-{1..7}-*/{red,green,refactored,e2e-mock,live-verified,done}`
- **v1.0.1 follow-ups:** see [`../reviews/FOLLOWUPS_v1.0.1.md`](../reviews/FOLLOWUPS_v1.0.1.md)
  - `module/v1.0.1-ws-read-file/done` — streaming WS `/files/read-stream`
  - `module/v1.0.1-two-listener-proxy/done` — split OpenTunnel / OpenIoStream listeners
  - `module/v1.0.1-youki-setns-file-ops/done` — `setns(2)` file ops, no in-container binaries

What shipped, in one paragraph: exec is now a bidirectional
stream-shaped session on the proxy's data plane
(`SandboxIoService.OpenIoStream`), exposed publicly as
`WS /v1/sandboxes/{id}/exec`. File ops share the same flow.
The connection IS the session lifetime — closing the WebSocket
triggers SIGTERM → SIGKILL on the in-container PID via the
agent's `ExecRegistry`. The synchronous `POST /exec` REST
endpoint, the 60s `EXEC_TIMEOUT`, the controller's exec broker,
and the `ExecCommand` / `ExecResult` proto messages were all
removed; the controller stream is lifecycle-only.

Closed friction items: H1–H4 (timeout, session persistence,
disconnect-kills, write_file helper logs), M1, M2, M4, M5.

Forward trajectory enabled by the data-plane choice: computer-use
agent API, v1.1 transparent WebSocket forwarding (VNC-from-browser
+ inbound WS apps), v1.2 desktop sandbox recipe. None are
implemented yet; the architecture is positioned to add them
additively.
