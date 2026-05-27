# CHANGELOG

## v1.0.3 (in progress) — Live edit additions

Additive `proxy.proto` surface in support of the in-browser live-edit
UI scoped in `docs/plans/PLAN_LIVE_EDIT.md` (the "Replit in a tab"
feature in the DX-magic roadmap). Wire-compatible with v1.0.1 /
v1.0.2 — no field renames, no removed variants, defaults preserve
prior behavior on the legacy fields.

### Additions

- **One-level directory listing.** New `ListDirParams` variant on
  `IoStart.params` (tag 6) requests a `path` (relative to optional
  `cwd`); the agent emits a single `ListDirResult` payload (tag 7
  on `IoServerFrame.payload`) carrying typed `ListDirEntry`
  records — `name`, `type` (new `ListDirEntryType` enum:
  `File` / `Dir` / `Symlink` / `Other`), `size`, `revision`,
  `mode`, optional symlink `target`. Hard-capped at 5000 entries
  with a `truncated` flag and a `total_entries` count so the UI
  can render a "drill in" affordance instead of OOMing on
  `node_modules`.
- **TCP-probe of the sandbox's host port.** New
  `WaitPortListeningParams` variant on `IoStart.params` (tag 7)
  takes `{port, timeout_ms}`; the agent polls
  `127.0.0.1:<host_port>` (resolved via
  `SandboxManager::host_port_for`) every 50 ms until the
  in-container dev-server accepts a TCP connect or `timeout_ms`
  elapses, then emits a `WaitPortListeningResult` payload (tag 8
  on `IoServerFrame.payload`) with `{ready, elapsed_ms}`. The UI
  uses this to gate the preview-iframe refresh on `watchexec`
  restart completion instead of timer-based guessing.
- **Opaque file revision token.** New `FileMeta` payload (tag 9 on
  `IoServerFrame.payload`) carrying `{revision, size}`. Emitted
  before the first `Stdout` chunk of a `ReadFile` session and as
  the post-write ACK of a `WriteFile` session. The wire encoding
  is opaque; the reference agent implementation uses
  `mtime_nanos:size`.
- **Precondition on writes.** `WriteFileParams` gains
  `expected_revision` (string, tag 3) and `force` (bool, tag 4).
  Empty `expected_revision` preserves the v1.0.1 / v1.0.2 "no
  precondition" behavior so callers that never observed a
  revision keep working. The agent enforces the check when the
  field is non-empty and `force` is false; mismatches surface as
  `IoError { code: "REVISION_MISMATCH", detail: <actual> }` and
  the gateway maps this to `409 Conflict { actual_revision,
  conflicting_content_b64 }`.

### Behavior changes (operator-visible)

None. Existing callers are wire-compatible — the new IoStart
variants are opt-in, and the new WriteFileParams fields default to
the v1.0.2 behavior.

### Pending in this version

Group A of `PLAN_LIVE_EDIT_TASKS.md` ships the wire surface and
the matching contracts-crate regression tests; the runtime
handlers (group B), the gateway routes (group C), and the UI
(group D) follow on separate PRs. Until group B ships, the agent
returns `IoError { code: "NOT_IMPLEMENTED" }` for the two new
`IoStart` variants.

## v1.0.2 (in progress) — 12-factor decomposition: migrate subcommand, env-templated cloud-init

Operator-facing surface changes from `PLAN_12FACTOR.md` phases 1–3 +
related fixes. No wire protocol changes.

### Additions

- **`open-sandbox migrate` subcommand.** Runs controller + proxy
  schema migrations and exits. Idempotent (every statement is `CREATE
  TABLE/INDEX IF NOT EXISTS`). Production deploys should invoke this
  once before starting the long-running services so a migration
  failure doesn't cascade into a service-startup crash-loop.
- **`--auto-migrate` flag on `controller` and `proxy` subcommands**
  (also env `OPEN_SANDBOX_AUTO_MIGRATE`). Default **off**. When set,
  preserves the pre-v1.0.2 behavior of running migrations on service
  startup. Dev environments (`docker-compose.*.yml`, the future
  `open-sandbox dev`) pass this flag automatically.
- **`.env.example`** at the repo root documenting every required and
  optional env var organized by service. README gains an "Environment
  variables" section pointing at it. `.gitignore` extended so `.env`
  variants can't be committed by accident.
- **`apiControllerUrl` / `apiProxyUrl` optional args on
  `controllerUserData`** (Pulumi infra). Default to the existing
  `http://127.0.0.1:${port}` templates so single-host deploys are
  byte-identical; future multi-host deploys can override these without
  editing infra code.
- **Proxy SIGTERM drain.** On shutdown signal the proxy now sets a
  shared drain flag (new `OpenTunnel` / `OpenIoStream` calls return
  `Status::unavailable`), polls `IoSessions::is_empty()` until empty
  or `--shutdown-drain-timeout` seconds elapse (env
  `OPEN_SANDBOX_SHUTDOWN_DRAIN_TIMEOUT`, default 30), then sends each
  remaining session a terminal `Unavailable` frame so the gateway
  never sees a silent disconnect. Bounds the blast radius of a
  planned proxy restart to "sessions older than the drain timeout."
  Partner to ADR-010's "1-of-N proxy is acceptable" stance.
- **Release artifact integrity verification** (`infra/src/cloud-init.ts`).
  Cloud-init now downloads `SHA256SUMS` alongside the binary,
  matches the per-arch entry, and fails-closed on mismatch. When
  `SHA256SUMS` is absent (releases pre-dating the policy) it logs a
  visible warning and continues, so existing deploys keep working.
  Applies to both controller and worker host bootstraps. **Release
  publishers should now ship `SHA256SUMS` alongside each binary**
  (one line per arch: `<sha256>  open-sandbox-linux-<arch>`).

### Behavior changes (operator-visible)

- **Default migration behavior on `controller` / `proxy` startup
  flipped from "auto-migrate" to "off".** Existing dev environments
  must either run `open-sandbox migrate` once or pass
  `--auto-migrate`. Existing production deploys regenerated from the
  updated `cloud-init.ts` template do the migrate step inline at
  cloud-init time AND via `ExecStartPre=` on the controller systemd
  unit (the latter covers in-place binary upgrades by re-running
  migrations on every service restart). **Deploys that bypass the
  Pulumi template must add a `migrate` step before starting
  controller/proxy.**

### Security fixes (cloud-init.ts)

- **`INTERNAL_TOKEN` env var name in the proxy + api systemd units
  corrected to `OPEN_SANDBOX_INTERNAL_TOKEN`.** The previous name
  was unread by the binary; the production proxy in split-listener
  mode was therefore starting with no internal-token auth, and the
  api gateway was never sending the bearer to the proxy. Both sides
  silently agreed on `None` so requests succeeded, but the
  defense-in-depth layer was absent.
- **`CONTROLLER_ADMIN_TOKEN` added to the api gateway's systemd env**
  (`infra/src/cloud-init.ts`). Without it the api could not
  authenticate to the controller's management gRPC — every sandbox
  create/list/pause/delete call would have returned
  `Status::unauthenticated`. **Production deployments before this fix
  were broken at the api layer.** Re-run `pulumi up` (or manually
  patch `/etc/systemd/system/open-sandbox-api.service` +
  `systemctl daemon-reload && systemctl restart open-sandbox-api`).

### Architectural documents

- **New `docs/design/SCALING_TIERS.md`** (ADR-010 in `SAD.md`). Locks
  in the decision that the proxy is a connection-affinity tier and
  the controller is a single coordinator, both 1-of-N by design;
  agent fleet is the horizontal scaling unit. Read before proposing
  any "externalize proxy session state" refactor.
- **`docs/plans/PLAN_12FACTOR.md`** — the gap-closure plan driving
  these changes.

## v1.0.2 (in progress) — Pause/Unpause sandbox lifecycle

Additive lifecycle operation for the v1.0.x sandbox surface. A running
sandbox can be frozen (in-container processes stop receiving CPU but
the container, tunnel, exec-registry, and DB row stay alive) and later
resumed without re-creating anything.

### Additions
- **Public REST**: `POST /v1/sandboxes/{id}/pause` and
  `POST /v1/sandboxes/{id}/unpause`. Both return 202 Accepted with a
  JSON body `{"status": "pausing"}` or `{"status": "unpausing"}` — the
  optimistic transition state. Clients poll `GET /v1/sandboxes/{id}`
  for the steady-state `"paused"` / `"running"` once the agent
  acknowledges (typically tens of milliseconds).

- **Wire protocol** (`proto/api.proto`, `proto/controller.proto`):
  new RPCs `PauseSandbox` / `UnpauseSandbox` on
  `SandboxManagementService`; new `ControllerCommand` oneof variants
  (fields 6 and 7); new `SandboxState` enum variants `PAUSING` (6),
  `PAUSED` (7), `UNPAUSING` (8). All additive — old clients that
  pattern-match on the legacy variants round-trip the new ones as
  their integer values.

- **Agent runtime trait** (`crates/agent/src/container.rs`): new
  `pause(&ContainerRuntime, &ContainerId)` and `unpause(...)` methods.
  Implemented in both backends:
    * **agent-docker**: bollard `pause_container` / `unpause_container`.
      Docker's 409-on-already-paused is treated as idempotent success.
    * **agent-youki**: libcontainer's `Container::pause()` /
      `Container::resume()` — cgroup-v2 freezer write under the hood.

- **SandboxManager** (`crates/agent/src/sandbox.rs`):
  `pause_sandbox` / `unpause_sandbox` that update the local
  `SandboxEntry.state` and emit `SandboxStatus(PAUSED|RUNNING)` to
  the controller. Idempotent — pausing an already-paused sandbox or
  unpausing a running one returns the steady state without
  dispatching to the runtime.

- **UI** (`ui/`): pause/play button per sandbox row (lucide
  `Pause` / `Play` icons), wired to the new REST endpoints; status
  badge gains `pausing` / `paused` / `unpausing` variants (accent-blue
  styling, distinct from green-running and red-stopped).

### Tests
- 4 new agent unit tests (idempotency, state transitions, unknown sandbox)
- 4 new api gateway HTTP tests (202 responses, transition body, 404, 401)
- All existing tests still pass.

### Out of scope (future work)
- **Stop/Start** (graceful shutdown without removal + restart). The
  current `DELETE /v1/sandboxes/{id}` is "stop and remove" — the
  container row is destroyed. A retain-on-stop + start-from-stopped
  flow would need a DB-backed config-retention model and is not part
  of this change.

---

## v1.0.2 (in progress) — WebSocket subprotocol auth, opt-in CORS, fail-closed empty key

Additive auth path for browser-based WebSocket clients (the existing
`Authorization: Bearer` path is unchanged for programmatic clients).
Opt-in CORS layer for cross-origin dev consoles. Startup-time guard
that refuses to boot the API binary with an empty API key.

### Additions
- **WebSocket subprotocol auth** (`crates/api/src/handlers.rs`,
  constants in `crates/contracts/src/constants.rs`). Browsers can
  authenticate WS upgrades by offering both subprotocols:
  ```
  Sec-WebSocket-Protocol: open-sandbox.v1, bearer.<base64url-no-pad(api_key)>
  ```
  The server validates the `bearer.<…>` entry and echoes ONLY the
  sentinel (`open-sandbox.v1`) back — the API key never lands in the
  101 response header. The `bearer.` prefix is matched
  case-insensitively. Per-request iteration is capped at
  `WS_AUTH_MAX_OFFERED_PROTOCOLS = 16` to prevent pre-auth
  algorithmic amplification. New contract constants:
  `WS_AUTH_PROTOCOL_SENTINEL`, `WS_AUTH_BEARER_PREFIX`,
  `WS_AUTH_MAX_OFFERED_PROTOCOLS`. See `CONTRACTS.md § WebSocket auth`.

- **Opt-in CORS layer** for the REST routes (`OPEN_SANDBOX_API_CORS_ORIGINS`
  env var). Unset → no CORS headers (production default). Set to
  a comma-separated list to allowlist explicit origins; sole `*`
  activates wildcard. Mixed `*` + explicit origins logs a WARN and
  keeps the explicit allowlist (silent wildcard escalation is
  prevented). Surrounding ASCII quotes are stripped per entry.
  See `CONTRACTS.md § CORS`.

- **Single-file dev console** (`ui/index.html`). xterm.js terminal
  for streaming exec + REST file read/write. Talks to the API
  through the new subprotocol path so a browser can drive sandboxes
  without a server-side companion.

### Behavior changes
- **API binary now refuses to start with an empty `OPEN_SANDBOX_API_KEY`**
  (`crates/cli/src/run.rs`). The previous behavior — silent
  open-auth because `constant_time_eq("", "") == true` — has been
  closed. Operators with empty-key configs (typo'd env vars,
  unset-in-CI templating bugs) will now see an explicit startup
  error: `OPEN_SANDBOX_API_KEY must be set to a non-empty value;
  refusing to start with empty key`.

- **WS auth helper unified**. `ws_exec.rs` and `ws_read_file.rs`
  previously had local `check_auth` functions that accepted only
  `Authorization: Bearer`. Both now call the shared
  `handlers::check_ws_auth`, which accepts either path and is
  forgiving of a wrong Authorization header alongside a valid
  subprotocol (proxies that inject stale Authorization headers
  no longer lock out browser-based clients).

### Dependencies
- Added `tower-http = { version = "0.6", features = ["cors"] }` to
  `crates/api/Cargo.toml` for the opt-in CORS layer.

---

## v1.0.2 earlier work — pull_policy, startup-time optimization

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
  `"never"`. Unknown wire-i32 values are **rejected at the
  controller's management endpoint** with `Status::InvalidArgument`
  → HTTP 400 (see the iter10 entry below for the fail-closed
  rationale: silently collapsing a future stricter-than-`Never`
  variant to `IfNotPresent` would defeat the air-gap guarantee).
  Downstream of that wire boundary the agent uses a lossy `From<i32>`
  as defense-in-depth, since the controller has already validated.
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
- Accepts the `pull_policy` field. `IfNotPresent` (default) is the
  existing behavior (oci-client's `.complete` marker fast path).
  `Always` now actually force-refreshes the image (iter12 closes
  the original "silently degrades to IfNotPresent" gap). `Never`
  fails fast with a `Runtime` error pending a local tag→digest
  index — see iter12 below.

  Iter12 splits `ImageManager::pull_and_unpack(&str)` into a
  public wrapper that delegates to a new
  `pull_and_unpack_with(&str, force: bool)`:
  - `force = false`: existing IfNotPresent semantics. Pulls
    manifest + config (needed to compute the digest), then
    short-circuits on the `.complete` marker. No layer fetch
    when cached.
  - `force = true` (PullPolicy::Always): extracts into a fresh
    tmp_dir first, THEN (on extract success) evicts the cached
    `.complete` marker + `rootfs/` directory and atomically
    swaps the freshly-extracted tmp into place. The order is
    load-bearing: a force-pull that fails mid-extract leaves
    the previously-healthy cache intact, preserving the comp-5
    invariant `marker exists ⇒ rootfs exists`. (An earlier
    iter12 draft did the eviction upfront — surfaced by
    iter12's own /code-review as "failed force-pull destroys a
    healthy cache" — and was reordered before landing.)

  Three unit tests anchor the behavior, all run in the
  agent-youki dev container against a live alpine:latest pull:
  - `force_refetch_re_extracts_even_when_cached`: verifies the
    `.complete` marker mtime advances on a force=true second call.
  - `default_wrapper_uses_cache`: verifies the public wrapper
    short-circuits (marker mtime unchanged) so the IfNotPresent
    semantics are preserved by the convenience entry point.
  - `failed_force_pull_preserves_cache`: populates the cache,
    then issues a force-pull against a non-existent registry
    image; asserts the original alpine cache (marker + rootfs)
    is intact after the force-pull's Err return.

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

  * Iter11: deadline propagation, enforced uniformly at the
    `SandboxManager::start_sandbox` layer (NOT inside each
    `ContainerRuntime` impl) so both DockerRuntime AND YoukiRuntime
    are bounded by the same `SANDBOX_CREATE_DEADLINE` constant.
    Production runs youki per ADR-009; pinning the deadline at the
    runtime-impl level would have left production uncapped.
    `SANDBOX_CREATE_DEADLINE = 60s` is intentionally BELOW the
    docker runtime's worst-case retry budget (~90s under sustained
    registry pressure: 3 outer port-retry × 4 pull-attempt × ~7.5s
    of backoff): fail loud rather than letting an outlier camp on a
    thread for the full ceiling. On expiry the agent returns
    `AgentError::Runtime { detail: "create_and_start deadline of
    60s exceeded for sandbox <uuid>" }`; if `create_container`
    succeeded but `start_container` was mid-flight when the future
    was dropped, the partial docker container leaks until the agent
    process restarts — `SandboxManager::reconcile` exists but is
    not yet periodically invoked. Two anchor tests pin the
    operator-facing error format using `tokio::test(start_paused =
    true)` + a `SlowContainerRuntime` mock, so future refactors
    can't silently drift log-scanner regexes. Caller-supplied
    deadlines (e.g. propagating a tonic gRPC request deadline) and
    a periodic reconcile-cleanup task are both follow-ups.
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
