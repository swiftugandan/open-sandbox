# Plan — Exec Streaming Amendment (v1.0.0)

> **Source of truth for the design:** `EXEC_STREAMING_DESIGN.md`.
> **Spike results consumed:** `spikes/exec-streaming/spike-0{1,2,3}-*/RESULT.md`.
> **This document:** the executable plan. Six sub-modules, each with a
> branch, exact file list, type signatures, TDD cycle expectations,
> acceptance criterion, smoke test, risks, and effort estimate.
>
> If you are picking this up cold: read the design doc first, then this
> file top to bottom, then start at sub-module 12.1.

## Status & dependencies

- **Prerequisite contracts version:** `contracts/v0.7.0-frozen` ✓ (already on `main`)
- **Target contracts version after 12.1:** `contracts/v1.0.0-frozen`
- **Spike pre-conditions** (must all be satisfied — they are):
  - [x] Spike 01 — docker exec does not propagate disconnect; agent must explicitly kill
  - [x] Spike 02 — nsenter does not propagate SIGTERM; agent must explicitly kill
  - [x] Spike 03 — axum WebSocket backpressures cleanly; disconnect detected in ms while sending; idle sessions need 30s ping
- **Amendment integration branch:** `contracts/amendment-exec-streaming` (off `main` after merging `contracts/amendment-sdk-agent-friction`)
- **Sub-module branches:** `module/exec-streaming-<n>-<slug>` branched off the amendment integration branch; merged back as fast-forward when each TDD cycle completes its `live-verified` tag

## Decomposition DAG

```
              contracts/v0.7.0-frozen  (main)
                       │
                       ▼
       ┌───────  12.1 contracts/proto  ───────┐    contracts/v1.0.0-frozen
       │     (HARD sequence point: nothing    │
       │      below can start until frozen)   │
       ▼                                       ▼
   12.2 agent                              12.3 proxy
   (runtime trait, ExecRegistry,           (originate-session
    both backends, file ops)                 endpoint + internal authn)
       │                                       │
       └──────────────┬────────────────────────┘
                      ▼
                12.4 api gateway
                (WS endpoints, gRPC client pool to proxy,
                 WS auth, ws-client harness)
                      │
                      ▼
                12.5 controller cleanup
                (remove exec broker / message-shaped exec
                 + delete obsolete v0.7 test fixtures)
                      │
                      ▼
                12.6 live e2e scenarios
                (scripted: cancel, slow-client, signals,
                 disconnect-kills-process, idle keepalive,
                 both runtimes)
                      │
                      ▼
                12.7 migration & docs
                (v0.7 → v1.0 guide, SDK examples,
                 SPEC update, changelog)
```

**Sequence point at 12.1:** all sub-modules below depend on the
frozen proto. 12.2's `ExecRegistry` and unit tests for the registry
itself can be drafted before 12.1 freezes (they don't touch protos),
but no runtime integration work starts until then.

After 12.1, 12.2 and 12.3 are independently testable against mock
peers and can be implemented in parallel by two people. 12.4 depends
on both. 12.5 is a removal pass after 12.4 is e2e-verified. 12.6 is
the integration gate. 12.7 finalizes the user-visible surface.

## Branch policy

```
main
 │
 └── contracts/amendment-exec-streaming   (integration branch, long-lived)
      │
      ├── module/exec-streaming-1-contracts-proto    → merged ff after live-verified
      ├── module/exec-streaming-2-agent-runtime       → merged ff after live-verified
      ├── module/exec-streaming-3-proxy-originate     → merged ff after live-verified
      ├── module/exec-streaming-4-api-gateway-ws      → merged ff after live-verified
      ├── module/exec-streaming-5-controller-cleanup  → merged ff after live-verified
      ├── module/exec-streaming-6-live-e2e            → merged ff after live-verified
      └── module/exec-streaming-7-migration           → merged ff after live-verified
```

The integration branch is what merges to `main` at the end as a single
amendment-complete event. Each sub-module branch follows the standard
TDD cycle (red → green → refactor → e2e-mock → live-verified) with
those tags applied on its own branch before merging back to the
integration branch.

## Tags

Per the protocol:

- `contracts/v1.0.0-frozen` after 12.1 freeze gate
- `module/exec-streaming-<n>-<slug>/{red,green,refactored,e2e-mock,live-verified,done}` per sub-module
- `plan/v0.6.0` on this plan document itself

---

# 12.1 — Contracts & proto extension

**Branch:** `module/exec-streaming-1-contracts-proto`
**Depends on:** nothing past `contracts/v0.7.0-frozen`
**Effort:** S–M (1–2 days of proto + Rust scaffolding)

## Purpose

Define the new wire surface for streaming sandbox I/O on the data plane.
Freeze it as `contracts/v1.0.0-frozen` so 12.2 and 12.3 can start in
parallel against an immovable surface.

## Files that change

```
proto/proxy.proto                                       [edit]
proto/controller.proto                                  [edit — remove exec messages]
proto/api.proto                                         [edit — remove ExecSandbox RPC]
crates/contracts/Cargo.toml                             [bump to 1.0.0]
crates/contracts/src/lib.rs                             [re-export new types]
crates/contracts/src/error.rs                           [new error variants]
CONTRACTS.md                                            [prose update for v1.0]
SPEC.md                                                 [FR-12 / FR-13 amendment]
SAD.md                                                  [proxy + api per-component zoom]
```

## Proto changes — `proxy.proto`

Service is renamed and broadened. Two key additions: `OpenIoStream`
opens a multiplexed bidi stream identified by `sandbox_id`, carrying
typed I/O frames; the existing `OpenTunnel` RPC stays for now (gradually
deprecated as v1.1 transparent forwarding is rolled out — out of scope
of this amendment).

```protobuf
service SandboxIoService {
  // Existing — public HTTP routing. Stays.
  rpc OpenTunnel(stream TunnelResponse) returns (stream TunnelRequest);

  // NEW — gateway-originated I/O sessions multiplexed into the agent
  // tunnel. The client (api gateway) opens this and is treated as the
  // initiator. The proxy routes by sandbox_id on the first frame.
  rpc OpenIoStream(stream IoClientFrame) returns (stream IoServerFrame);
}

message IoClientFrame {
  string stream_id = 1;
  oneof payload {
    IoStart   start  = 2;   // first frame; carries sandbox_id + op
    bytes     stdin  = 3;   // bytes flowing toward the process
    IoSignal  signal = 4;   // SIGTERM / SIGKILL / SIGINT etc
    IoClose   close  = 5;   // half-close stdin or end session
  }
}

message IoServerFrame {
  string stream_id = 1;
  oneof payload {
    IoStarted started = 2;  // ack; carries exec_id / pid for debug
    bytes     stdout  = 3;
    bytes     stderr  = 4;
    IoExited  exited  = 5;  // exit_code; or command_not_found:true
    IoError   error   = 6;  // runtime-level error (container gone, etc)
  }
}

message IoStart {
  string sandbox_id = 1;
  IoOp op = 2;
}

enum IoOp {
  IO_OP_UNSPECIFIED = 0;
  IO_OP_EXEC = 1;        // payload: ExecParams
  IO_OP_READ_FILE = 2;   // payload: ReadFileParams
  // WriteFile + WriteFiles stay on the unary REST surface; they don't
  // need streaming. The agent runtime trait still gains first-class
  // methods for them (see 12.2) so they don't go through the shell
  // helper anymore — that's an internal cleanup, not a wire change.
}

message ExecParams {
  repeated string command = 1;
  string cwd = 2;
  map<string, string> env = 3;
  // PTY allocation deliberately NOT defined here. proto3 supports
  // additive fields without a major bump, so PTY arrives in v1.1
  // when it actually has an implementation. See "Out of scope" below.
}

message ReadFileParams {
  string path = 1;
  string cwd = 2;
}

message IoSignal {
  // POSIX signal number; agent maps and dispatches via the runtime
  uint32 signum = 1;
}

message IoClose {
  bool stdin_eof = 1;   // true → close stdin only; false → end session
}

message IoStarted {
  string exec_id = 1;
  int32 in_container_pid = 2;
}

message IoExited {
  int32 exit_code = 1;
  bool command_not_found = 2;
}

message IoError {
  string code = 1;     // "RUNTIME_ERROR", "SANDBOX_GONE", "EXEC_FAILED"
  string detail = 2;
}
```

## Proto changes — `controller.proto`

```diff
 message ControllerCommand {
   oneof payload {
     RegisterResponse register_response = 1;
     HeartbeatAck heartbeat_ack = 2;
     StartSandbox start_sandbox = 3;
     StopSandbox stop_sandbox = 4;
-    ExecCommand exec = 5;
+    reserved 5;  // was ExecCommand; now lives on proxy.proto IoStream
     FetchLogsCommand fetch_logs = 6;
   }
 }

 message AgentMessage {
   oneof payload {
     RegisterRequest register = 1;
     Heartbeat heartbeat = 2;
     SandboxStatus sandbox_status = 3;
     ResourceReport resource_report = 4;
-    ExecResult exec_result = 5;
+    reserved 5;  // was ExecResult; now flows on the I/O stream
   }
 }

-message ExecCommand { ... }
-message ExecResult  { ... }
```

`reserved` is the correct proto3 idiom — it prevents anyone from
reusing field 5 with a different meaning later.

## Proto changes — `api.proto`

```diff
 service SandboxManagementService {
   rpc CreateSandbox(...) returns (...);
   rpc GetSandbox(...) returns (...);
   rpc ListSandboxes(...) returns (...);
   rpc DeleteSandbox(...) returns (...);
-  rpc ExecSandbox(ExecSandboxRequest) returns (ExecSandboxResponse);
 }

-message ExecSandboxRequest  { ... }
-message ExecSandboxResponse { ... }
```

Public exec moves entirely to the proxy's `OpenIoStream`. The api
gateway becomes a WebSocket-to-gRPC adapter for streaming endpoints;
controller stays lifecycle-only.

## Contracts crate changes

```diff
 // crates/contracts/src/error.rs
 #[derive(Debug, Error)]
 #[non_exhaustive]
 pub enum ApiError {
     ...
-    ExecFailed { detail: String },
-    CommandNotFound { command: String },
+    // Streaming I/O errors surface on the WebSocket close frame /
+    // IoError envelope; ApiError loses the synchronous exec variants.
+    IoStreamFailed { detail: String },
+    SandboxGone { sandbox_id: String },
     ...
 }
```

```diff
 // crates/contracts/src/constants.rs
-pub const EXEC_TIMEOUT: Duration = Duration::from_secs(60);
+// EXEC_TIMEOUT removed — streaming exec has no synchronous deadline.
+// Idle WebSocket keepalive uses WS_IDLE_PING_INTERVAL instead.
+pub const WS_IDLE_PING_INTERVAL: Duration = Duration::from_secs(30);
+pub const WS_IDLE_PING_TIMEOUT: Duration  = Duration::from_secs(60);
```

## TDD cycle expectations

- **Red:** add a test in `crates/contracts/src/lib.rs` that asserts
  `proxy.IoOp::Exec` round-trips through prost serialize/deserialize.
  Compile fail expected (type doesn't exist yet).
- **Green:** add the proto + regen. Test passes.
- **Refactor:** check that no `ExecCommand`/`ExecResult`/`ExecSandbox*`
  references remain in `crates/contracts/`. Run `grep -r ExecCommand
  crates/contracts/` — must be empty.
- **E2E (mocked peers):** N/A for the contracts crate itself.
- **Live-verified:** `cargo check -p open-sandbox-contracts` passes;
  `cargo build --workspace` fails at consumer crates as expected (they
  still reference the deleted types). That failure IS the verification
  — it proves the surface change reaches every consumer.

## Acceptance criterion

```bash
# All three commands must produce the indicated results.
cargo check -p open-sandbox-contracts                                   # passes
grep -r 'ExecCommand\|ExecResult\|ExecSandboxRequest' crates/contracts  # empty
grep -r 'ExecCommand\|ExecResult\|ExecSandboxRequest' proto/            # empty
git tag contracts/v1.0.0-frozen                                          # tag created
```

## Smoke test (post-merge to integration branch)

```bash
# Verify the workspace breakage is exactly the expected set: only the
# downstream crates that import the deleted types fail, and they fail
# at known call sites.
cargo build --workspace 2>&1 | grep -c 'error\[E0432\]: unresolved'
# Expected: matches the count documented in this plan (~12 sites
# across controller, api, agent — listed in 12.5's removal pass).
```

## Risks

- **`reserved 5` interpretation.** Mitigation: tonic emits the
  reserved annotation; old binaries from v0.7 won't talk to v1.0
  controllers anyway (semver major), so wire-compat with reserved
  fields is not a concern.

## Effort

S–M. ~6 hours for proto + regen + crate update + doc amendments.

---

# 12.2 — Agent: streaming runtime trait + ExecRegistry + first-class file ops

**Branch:** `module/exec-streaming-2-agent-runtime`
**Depends on:** 12.1 (frozen `contracts/v1.0.0-frozen`)
**Effort:** L (the largest sub-module — 4–6 days)

## Purpose

Reshape the agent so each runtime backend speaks streaming I/O natively,
manages process lifetime via an `ExecRegistry`, and provides first-class
`WriteFile`/`ReadFile` operations without the shell helpers introduced
in v0.7.

This is where spike 01 and spike 02's "must explicitly kill on
disconnect" conclusion lives in code.

## Files that change

```
crates/agent/src/container.rs                            [edit — trait reshape]
crates/agent/src/sandbox.rs                              [edit — call sites]
crates/agent/src/exec_registry.rs                        [NEW]
crates/agent/src/io_stream.rs                            [NEW — stream wiring]
crates/agent/src/controller_client.rs                    [edit — remove exec handler]
crates/agent/src/proxy_client.rs                         [edit — handle IoStream frames]
crates/agent/src/testutil.rs                             [edit — mock for new trait]
crates/agent-docker/src/lib.rs                           [edit — streaming impl]
crates/agent-docker/src/exec_stream.rs                   [NEW — attach/pump logic]
crates/agent-youki/src/lib.rs                            [edit]
crates/agent-youki/src/exec_stream.rs                    [NEW]
crates/agent-youki/src/exec.rs                           [remove — replaced]
```

## Identifier semantics (must be clear before code starts)

- **`stream_id`** — proxy-assigned, identifies a virtual stream
  inside the agent's reverse tunnel. The `ExecRegistry` is **keyed
  on `stream_id`**. It is the lifecycle anchor: when the stream
  closes (for any reason — client disconnect, agent error, server
  shutdown), the registry's cleanup hook fires against this key.
- **`exec_id`** — agent runtime-assigned (UUID), identifies a
  started exec at the runtime layer. Carried in `IoStarted` for
  diagnostics; logged on every exec frame; **never used as the
  registry key**. Exists to correlate runtime-layer logs across
  the agent process.

The two identifiers are 1:1 for a successful exec but they belong
to different layers: `stream_id` is the wire, `exec_id` is the
process. Conflating them in code is a smell.

## Type signatures (concrete)

```rust
// crates/agent/src/container.rs

pub struct ExecStart {
    pub command: Vec<String>,
    pub cwd: String,
    pub env: HashMap<String, String>,
    // PTY field deliberately absent. Pipe-only for v1.0; PTY arrives
    // in v1.1 as a proto-additive change.
}

pub struct ExecHandle {
    pub in_container_pid: i32,
    pub exec_id: String,
    pub stdin: mpsc::Sender<bytes::Bytes>,
    pub stdout: mpsc::Receiver<bytes::Bytes>,
    pub stderr: mpsc::Receiver<bytes::Bytes>,
    pub exited: oneshot::Receiver<ExecExitInfo>,
}

pub struct ExecExitInfo {
    pub exit_code: i32,
    pub command_not_found: bool,
}

pub trait ContainerRuntime: Send + Sync {
    fn create_and_start(...) -> impl Future<Output = ...> + Send;
    fn stop_and_remove(...) -> impl Future<Output = ...> + Send;
    fn list_sandbox_containers(...) -> impl Future<Output = ...> + Send;

    // REPLACES the v0.7 `exec(options) -> ExecOutput`.
    fn start_exec(
        &self,
        container: &ContainerId,
        start: ExecStart,
    ) -> impl Future<Output = Result<ExecHandle, AgentError>> + Send;

    // NEW. Used by ExecRegistry cleanup hook on stream close.
    fn signal_exec(
        &self,
        container: &ContainerId,
        in_container_pid: i32,
        signum: i32,
    ) -> impl Future<Output = Result<(), AgentError>> + Send;

    // NEW. First-class file ops (no shell helpers).
    fn read_file(
        &self,
        container: &ContainerId,
        path: &str,
        cwd: Option<&str>,
    ) -> impl Future<Output = Result<bytes::Bytes, AgentError>> + Send;

    fn write_file(
        &self,
        container: &ContainerId,
        path: &str,
        cwd: Option<&str>,
        content: bytes::Bytes,
    ) -> impl Future<Output = Result<(), AgentError>> + Send;

    fn write_files_targz(
        &self,
        container: &ContainerId,
        cwd: Option<&str>,
        tarball: bytes::Bytes,
    ) -> impl Future<Output = Result<(), AgentError>> + Send;
}
```

```rust
// crates/agent/src/exec_registry.rs (new file)

pub struct ExecRecord {
    pub stream_id: String,
    pub sandbox_id: SandboxId,
    pub container_id: ContainerId,
    pub in_container_pid: i32,
    pub started_at: Instant,
}

pub struct ExecRegistry {
    inner: Mutex<HashMap<String /*stream_id*/, ExecRecord>>,
}

impl ExecRegistry {
    pub fn insert(&self, record: ExecRecord);
    pub fn remove(&self, stream_id: &str) -> Option<ExecRecord>;
    pub fn list_for_sandbox(&self, id: &SandboxId) -> Vec<ExecRecord>;
    pub fn reconcile(&self, alive_pids: &HashSet<i32>);  // agent restart
}

// Stream-close cleanup hook (called by io_stream.rs):
pub async fn on_stream_closed<R: ContainerRuntime>(
    runtime: &R,
    registry: &ExecRegistry,
    stream_id: &str,
    grace: Duration,
) -> Result<(), AgentError> {
    let Some(rec) = registry.remove(stream_id) else { return Ok(()); };
    // SIGTERM, wait up to `grace`, SIGKILL if still alive.
    runtime.signal_exec(&rec.container_id, rec.in_container_pid, libc::SIGTERM).await?;
    if !wait_for_exit(&rec, grace).await { 
        runtime.signal_exec(&rec.container_id, rec.in_container_pid, libc::SIGKILL).await?;
    }
    Ok(())
}
```

```rust
// crates/agent/src/io_stream.rs (new file)
// 
// The agent receives IoClientFrame streams from the proxy on its
// reverse tunnel (the existing OpenTunnel stream is reused — see 12.3
// for how the proxy multiplexes IoStream sessions into the same agent
// connection). io_stream.rs translates between IoClientFrame and the
// runtime's ExecHandle.

pub async fn drive_io_session<R: ContainerRuntime>(
    runtime: Arc<R>,
    registry: Arc<ExecRegistry>,
    stream_id: String,
    mut client_frames: impl Stream<Item = IoClientFrame> + Unpin,
    mut server_tx: mpsc::Sender<IoServerFrame>,
) -> Result<(), AgentError> {
    // 1. Await IoStart, dispatch to runtime.start_exec or read_file
    // 2. Register the exec in the registry
    // 3. Pump:
    //    - client stdin frames -> handle.stdin
    //    - handle.stdout/stderr -> server_tx
    //    - signal frames -> runtime.signal_exec
    //    - handle.exited -> emit IoExited, close stream
    // 4. On unexpected client disconnect (Stream returns None):
    //    - call exec_registry::on_stream_closed
    Ok(())
}
```

## Backend-specific notes

**Signal mechanism — backends are asymmetric on purpose.** The agent
typically runs inside a container itself (`docker-compose` mounts
`/var/run/docker.sock` into the agent container) and therefore does
NOT share a PID namespace with sandbox containers. So
host-PID-signal-from-host is unavailable for docker, and we go
through the Docker Engine API. For youki, the agent uses
`setns(pid_ns)` + `kill(2)` syscall directly — pure syscall, no
binary dependency on the sandbox image. This is the structurally
purer path where available.

### agent-docker (`crates/agent-docker/src/exec_stream.rs`)

- `start_exec`: uses `bollard::exec::create_exec` + `start_exec` with
  `attach_*: true`. Wraps the returned `StartExecResults::Attached`
  streams into our `ExecHandle` channels.
- `in_container_pid`: from `bollard::exec::inspect_exec` after start
  (the `Pid` field on the exec record).
- `signal_exec`: `docker exec <ctr> kill -<signum> <pid>` via
  bollard's exec API. **Requires `kill` to be present in the sandbox
  image** (most base images bundle busybox/coreutils; minimal
  `scratch` images may not). The runtime detects absence by
  inspecting the inner exec's exit code; absence is logged at WARN
  and the registry cleanup falls back to letting the process run to
  natural completion. Document this requirement in SPEC.md alongside
  the existing `tar` requirement.
- `read_file`: `bollard::container::download_from_container` returns
  a tar archive containing the target file. Unpack in-memory, return
  the file bytes. Preserves the v0.7 `FileNotFound { resolved_path }`
  contract — the runtime resolves the path against `cwd`/default
  before invoking the Docker API and constructs the error with
  the resolved absolute path when the file is missing.
- `write_file`: `bollard::container::upload_to_container` with a
  tar archive containing the file. Atomicity: pack a tar that
  writes to a temp path next to the target, then issue a second
  Docker exec (or a small post-extract rename via a runtime-internal
  shell wrapper — internal, not exposed in API logs as the v0.7
  leak point).
- `write_files_targz`: same as v0.7 — `upload_to_container` of the
  caller's tarball.

### agent-youki (`crates/agent-youki/src/exec_stream.rs`)

- `start_exec`: spawns `nsenter` with pipes (replaces v0.7 wait-with-output).
  Pump stdin pipe from `ExecHandle.stdin`; pump stdout/stderr to
  `ExecHandle.stdout/stderr`. Capture in-container PID by reading
  `/proc/<nsenter_pid>/task/<tid>/children` immediately after fork
  (poll with 5×10ms backoff; if empty after 50ms, the child already
  exited — registry treats it as a no-op exec).
- `signal_exec`: the agent process itself does
  `setns(target_pidns_fd, CLONE_NEWPID)` + `kill(in_container_pid,
  signum)`. Pure syscalls — no binary dependency on the sandbox
  image. (Falls back to `nsenter ... -- kill` only if setns
  capability is unavailable; documented but not the primary path.)
- `read_file`: `setns()` into the sandbox's mount namespace, then
  use a temporary thread to `read(2)` the resolved path. The
  resolved absolute path (post `cwd` resolution) is constructed in
  the runtime and returned in the `AgentError::Runtime { detail }`
  if missing — preserves the v0.7 `FileNotFound { resolved_path }`
  contract.
- `write_file`: `setns()` + write-to-temp-in-target-directory +
  `rename(2)`. Pure syscalls. Atomic by construction.
- `write_files_targz`: keep the existing tar extraction
  implementation; called from inside the runtime via setns+exec,
  not via the gateway-emitted shell helper.

## TDD cycle expectations

- **Red:**
  - Unit test for `ExecRegistry` insert/remove/reconcile behavior.
  - Unit test for `drive_io_session` against a `MockContainerRuntime`
    that records signal_exec calls — verifies that closing the
    `client_frames` stream causes `signal_exec(SIGTERM)` within `grace`.
  - Test for read_file / write_file paths.
- **Green:** implement the runtime trait reshape, the registry, and
  the io_stream driver. Both backends.
- **Refactor:** verify no shell strings remain in agent-docker or
  agent-youki for file ops. `grep -r 'sh.*-c' crates/agent-docker
  crates/agent-youki` should match only legitimate uses (none expected).
- **E2E (mocked peers):**
  - With a mock proxy stream, the agent processes IoStart→IoStdin→
    IoExited cycle.
  - Mock proxy drops the stream mid-exec; agent's registry triggers
    signal_exec; mocked runtime records the signal_exec call.
  - Both runtimes pass.

## Acceptance criterion (live e2e for 12.2)

Against a running agent connected to a real (or mocked-proxy)
controller stack:

```bash
# A real ExecHandle round-trip without involving the gateway yet.
# Hits the agent's stream handler directly via a test harness.
cargo test -p open-sandbox-agent --test streaming_runtime -- \
  --include-ignored

# Specific scenarios that must pass:
# 1. exec_runs_echo:        "echo hello" → stdout = "hello\n", exit=0
# 2. exec_streams_stdin:    cat with 10MB stdin → stdout = same 10MB
# 3. exec_signal:           sleep 60 + SIGTERM frame → exited within 1s
# 4. exec_disconnect:       sleep 60, drop client stream, verify
#                           in-container PID dies within grace (5s default)
# 5. read_file_missing:     read_file("nope.py", cwd="/home") →
#                           AgentError::Runtime with detail containing
#                           "/home/nope.py" (resolved path)
# 6. write_file_atomic:     write A then read A → A round-trips byte-for-byte
# 7. command_not_found:     "definitely_not_a_binary" → exit=127,
#                           command_not_found=true, stderr contains OCI msg
# 8. v07_compat_filenotfound: write_file then read_file with relative
#                             path → resolved-path appears in the error
#                             when missing (preserves the NFR-API-1
#                             contract from v0.7 — must not regress)
```

Run against both backends:

```bash
cargo test -p open-sandbox-agent-docker --test streaming_e2e
cargo test -p open-sandbox-agent-youki --test streaming_e2e  # Linux only
```

### Observability requirements (12.2)

The runtime + ExecRegistry must emit the following — these are part
of the acceptance, not a follow-up:

**Prometheus metrics** (in `crates/agent/src/metrics.rs` or equivalent):

- `agent_exec_registry_size` (gauge) — count of live ExecRecords
- `agent_exec_started_total{runtime}` (counter)
- `agent_exec_exited_total{runtime,outcome}` (counter; outcome ∈
  `{exited, command_not_found, signaled, disconnect_killed, error}`)
- `agent_exec_kill_signals_total{runtime,signal,outcome}` (counter;
  outcome ∈ `{delivered, target_gone, error}`)

**Structured tracing events** (via `tracing` crate):

- `io_session.start { stream_id, sandbox_id, op }`
- `io_session.exec_pid_captured { stream_id, exec_id, in_container_pid }`
- `io_session.client_disconnected { stream_id, reason }`
- `exec_registry.signal_sent { stream_id, signal, grace_ms }`
- `exec_registry.exec_exited { stream_id, exit_code, command_not_found }`

Acceptance asserts both metric presence (via `/metrics` scrape in
the e2e harness) and a representative trace event for each
lifecycle phase.

## Smoke test (post-merge)

```bash
# Confirm v0.7 shell-helper write_file path is fully gone from the agent.
grep -rE '"sh","-c"' crates/agent crates/agent-docker crates/agent-youki
# Expected: empty (or only legitimate user-payload exec sites — review
# the diff for any matches).

# Confirm ExecRegistry is referenced from controller_client/proxy_client.
grep -r ExecRegistry crates/agent/src
# Expected: registry.rs, io_stream.rs, lib.rs (the wiring).
```

## Risks

- **In-container PID capture for youki is the trickiest mechanic.**
  `nsenter` does `setns + fork + exec`. The fork → exec window is
  small but real. Mitigation: spike 02 already confirmed the
  signal-propagation gap; capturing the PID is well-known territory
  (read `/proc/<nsenter_pid>/task/*/children` right after spawn,
  retry with backoff for ~50ms if empty). If PID capture races and
  the in-container process exits before we record it, the cleanup
  hook is a no-op (the process is already gone) — benign.
- **`docker exec ... kill -SIGNAL pid` requires `kill` in the
  container.** Most base images have it (`coreutils`, `busybox`); a
  minimal scratch image might not. Mitigation: document the
  requirement in SPEC.md alongside the existing "container must have
  `tar`" caveat; if absent, the registry cleanup logs a warning and
  the exec runs to natural completion (graceful degradation).
- **bollard streaming attach semantics may differ between Docker
  Engine versions.** Mitigation: the v0.7 integration tests already
  exercise the attach path; extending the matrix to include stdin
  pumping is straightforward in the existing docker-compose.test.yml
  setup.

## Effort

L. ~4–6 days. Single biggest module of the amendment.

---

# 12.3 — Proxy: originate-session endpoint

**Branch:** `module/exec-streaming-3-proxy-originate`
**Depends on:** 12.1 (frozen contracts), 12.2 (agent speaks IoStream)
  — *or* can start in parallel with 12.2 against a mock agent.
**Effort:** M (2–3 days)

## Purpose

The proxy gains the ability to accept gateway-originated bidi streams
and route them into the agent's reverse tunnel by sandbox_id. Today
the proxy only accepts agent-originated tunnels (for HTTP forwarding);
this adds the second leg.

## Files that change

```
crates/proxy/src/grpc.rs                                 [edit — new RPC]
crates/proxy/src/io_session.rs                           [NEW]
crates/proxy/src/stream_mux.rs                           [edit — pump bidi]
crates/proxy/src/lib.rs                                  [edit — wire]
crates/proxy/src/testutil.rs                             [edit — mocks]
```

## Type signatures

```rust
// crates/proxy/src/io_session.rs

pub struct IoSessionRouter {
    routing_cache: Arc<RoutingCache>,
    tunnel_pool: Arc<TunnelPool>,  // existing — holds agent connections
}

impl IoSessionRouter {
    // Called from the new gRPC server impl when a gateway opens
    // OpenIoStream. Looks up the agent by sandbox_id from the
    // FIRST frame, then bridges the two streams.
    pub async fn route(
        &self,
        client_frames: impl Stream<Item = IoClientFrame> + Unpin,
        server_tx: mpsc::Sender<IoServerFrame>,
    ) -> Result<(), ProxyError>;
}

// In grpc.rs the new RPC handler:
impl SandboxIoService for ProxyServer {
    type OpenIoStreamStream = ReceiverStream<Result<IoServerFrame, Status>>;

    async fn open_io_stream(
        &self,
        request: Request<Streaming<IoClientFrame>>,
    ) -> Result<Response<Self::OpenIoStreamStream>, Status> { ... }
}
```

## Wire mechanic

The proxy already maintains a long-lived bidi gRPC stream to each
agent (`OpenTunnel`). For the IoStream:

1. Gateway opens `OpenIoStream` to the proxy. First frame is `IoStart`
   with `sandbox_id`.
2. Proxy looks up `sandbox_id → agent_id` in routing cache.
3. Proxy multiplexes a NEW virtual stream into the agent's existing
   tunnel connection, tagging it with a fresh `stream_id`.
4. Pumps client frames into the tunnel and server frames out.
5. Stream close on either side closes the virtual stream; the agent
   side triggers the ExecRegistry cleanup hook from 12.2.

The "multiplex a new virtual stream into the agent's tunnel" is the
key new capability. Today `stream_mux.rs` only originates HTTP-typed
virtual streams. The amendment adds an `IoStream` typed virtual stream.

This requires extending `TunnelRequest`/`TunnelResponse` in
proxy.proto with `IoClientFrame`/`IoServerFrame` payload variants (or
making them carry opaque bytes that the agent decodes — design
decision, captured below).

## Design decision inside 12.3

**Option A — Add `IoClientFrame`/`IoServerFrame` payload variants to
the existing `TunnelRequest`/`TunnelResponse` oneofs.** Reuses the
existing tunnel pump verbatim; one wire format.

**Option B — Make `IoClientFrame`/`IoServerFrame` carry the same shape
the gateway uses, just transparently forwarded.** Cleaner separation
but duplicates message types.

**Choice: Option A.** The agent's tunnel client (`crates/agent/src/proxy_client.rs`)
already dispatches by oneof variant; adding two more variants is a
small extension. Reuses existing flow control and stream-id machinery.

## Internal authn — gateway ↔ proxy

The proxy must distinguish a legitimate gateway-originated
`OpenIoStream` call from a malicious party that has reached the
proxy's network endpoint. Two-layer defense:

**Layer 1 — network isolation (primary).** In the default deployment
(NFR-OPS-1: controller + proxy on same VM, agents dial out, no
public inbound to the proxy gRPC port), the `OpenIoStream` RPC binds
to a **separate internal-only listener** on `127.0.0.1:50053` (or
the VM's private interface). The public `OpenTunnel` RPC binds to
the existing port (50052) reachable by agents. The two listeners
share the same `SandboxIoService` impl but have different bind
addresses.

**Layer 2 — shared-secret token in gRPC metadata (defense in depth
+ cross-host support).** The gateway sends `authorization: bearer
<INTERNAL_TOKEN>` in `OpenIoStream` metadata. The proxy verifies
against `OPEN_SANDBOX_INTERNAL_TOKEN` env var. Missing or invalid →
`Status::unauthenticated`. The same token is shared via Pulumi-
distributed env at deploy time.

For multi-host deployments where the gateway runs on a separate VM
from the proxy, Layer 1 is replaced with mTLS or VPN-based network
isolation; Layer 2 is unchanged.

**Files affected by authn:**

```
crates/proxy/src/grpc.rs           [edit — add interceptor for OpenIoStream metadata]
crates/proxy/src/lib.rs            [edit — bind two listeners]
crates/contracts/src/constants.rs  [edit — INTERNAL_TOKEN_ENV name]
infra/e2e/docker-compose.full.yml  [edit — pass env to both services]
```

This is documented in SAD.md under Cross-cutting concerns →
Authentication.

```diff
 message TunnelRequest {
   string stream_id = 1;
   oneof payload {
     HttpRequest http_request = 2;
     DataChunk data = 3;
     StreamClose close = 4;
+    IoClientFrame io_client = 5;
   }
 }

 message TunnelResponse {
   string stream_id = 1;
   oneof payload {
     HttpResponse http_response = 2;
     DataChunk data = 3;
     StreamClose close = 4;
     TunnelReady ready = 5;
+    IoServerFrame io_server = 6;
   }
 }
```

## TDD cycle expectations

- **Red:** test asserting that an `OpenIoStream` call with a sandbox
  that has no connected agent returns `Status::NotFound`.
- **Green:** implement the router. Test passes.
- **Refactor:** stream_mux.rs should not have to know IoStream
  specifics — it just forwards typed frames between two endpoints.
  Verify by reading the diff.
- **E2E (mocked peers):**
  - Mock agent: replies to every client stdin frame with a stdout
    echo, sends IoExited after 5 frames.
  - Test: gateway opens OpenIoStream, sends 5 stdin frames, receives
    5 stdout frames + IoExited. End-to-end round-trip.
- **Live:** see 12.6.

## Acceptance criterion

```bash
# Integration test with real proxy + mock agent.
cargo test -p open-sandbox-proxy --test io_session_e2e

# Scenarios:
# 1. unknown_sandbox_returns_not_found
# 2. open_close_round_trip: open, send stdin, recv stdout, recv exited
# 3. concurrent_streams: 100 concurrent OpenIoStream calls for 100
#    different sandboxes route correctly
# 4. agent_disconnect_propagates: agent's tunnel drops mid-session;
#    gateway sees stream error within 1s
```

## Smoke test (post-merge)

```bash
# Confirm the proxy can still serve its existing HTTP-forwarding role.
cargo test -p open-sandbox-proxy --test http_routing_e2e
# Must still pass — the IoStream addition is purely additive at this point.
```

## Risks

- **HTTP/2 max-concurrent-streams ceiling** (per spike 04 not yet
  run). If the gateway opens one connection and runs 100+ concurrent
  IoStreams, hits tonic's default cap. Mitigation: 12.4 (gateway)
  implements a small connection pool. The proxy itself doesn't care
  about the cap — it's a client-side concern.
- **Routing race**: a sandbox is deleted between the cache lookup
  and the stream open. Mitigation: standard pattern — catch the
  agent-side "unknown sandbox" error, close with `IoError { code:
  SANDBOX_GONE }`. Gateway translates to WebSocket close code 4404.

## Effort

M. ~2–3 days.

---

# 12.4 — API gateway: WebSocket endpoints + held-open gRPC to proxy

**Branch:** `module/exec-streaming-4-api-gateway-ws`
**Depends on:** 12.1, 12.2, 12.3.
**Effort:** L (3–4 days)

## Purpose

Public-facing WebSocket endpoints for streaming I/O, backed by the
held-open gateway↔proxy gRPC multiplex. WebSocket frame envelope
defined and implemented. Idle ping/pong keepalive wired per spike 03.

## Files that change

```
crates/api/Cargo.toml                                    [add axum ws, optional tokio-tungstenite]
crates/api/src/proxy_client.rs                           [NEW — held-open grpc to proxy]
crates/api/src/ws_exec.rs                                [NEW — /exec WebSocket handler]
crates/api/src/ws_read_file.rs                           [NEW — /files/read WebSocket]
crates/api/src/frame.rs                                  [NEW — WS binary frame envelope]
crates/api/src/handlers.rs                               [edit — drop unary exec, keep lifecycle]
crates/api/src/router.rs                                 [edit — add ws routes, drop /exec POST]
crates/api/src/service.rs                                [edit — service trait reshapes]
crates/api/src/grpc_service.rs                           [edit — drop unary exec impl, wire write_file via runtime]
crates/api/src/tests.rs                                  [edit — replace exec tests with ws ones]
crates/ws-client/Cargo.toml                              [NEW — thin Rust ws client]
crates/ws-client/src/lib.rs                              [NEW — Frame codec + Session API]
crates/ws-client/src/bin/opensandbox-exec.rs             [NEW — CLI: `opensandbox-exec <id> -- cmd args`]
```

The `ws-client` crate is the SDK-shape thin client that 12.6's e2e
scenarios depend on. It exposes:

```rust
pub struct ExecSession { /* ... */ }
impl ExecSession {
    pub async fn connect(base_url: &str, sandbox_id: &SandboxId,
                          api_key: &str, params: ExecParams)
                          -> Result<Self, WsClientError>;
    pub async fn send_stdin(&mut self, bytes: Bytes) -> Result<()>;
    pub async fn send_signal(&mut self, signum: i32) -> Result<()>;
    pub async fn close_stdin(&mut self) -> Result<()>;
    pub fn frames(&mut self) -> impl Stream<Item = Frame> + '_;
    pub async fn await_exit(self) -> Result<IoExited, WsClientError>;
}
```

Built as part of 12.4 (not 12.6) so it lands with the gateway and
can be unit-tested against the gateway in 12.4's own test suite.

## WebSocket frame envelope (concrete)

WebSocket binary frames carry exactly one application frame each.
The WebSocket frame itself already encodes payload length, so the
inner envelope is just **one prefix byte** identifying the frame
kind:

```
| 1 byte: kind | payload bytes |
```

`kind` values:

| Kind | Direction | Payload |
|------|-----------|---------|
| 0x00 | C→S | **start** — proto-encoded `IoStart` (MUST be first frame) |
| 0x01 | C→S | stdin bytes |
| 0x02 | C→S | signal — proto-encoded `IoSignal` |
| 0x03 | C→S | stdin_eof (no payload; signals stdin half-close) |
| 0x11 | S→C | stdout bytes |
| 0x12 | S→C | stderr bytes |
| 0x13 | S→C | exited — proto-encoded `IoExited` |
| 0x14 | S→C | error — proto-encoded `IoError` |
| 0x15 | S→C | started — proto-encoded `IoStarted` (first server frame) |

The first WS message after upgrade MUST be a `kind=0x00 start`
frame; anything else closes the WS with close code 4400
(`INVALID_REQUEST`). The server responds with `kind=0x15 started`
once the runtime has registered the exec; from that point both
sides may interleave stdin/stdout/stderr/signal/exited frames.

All non-byte frames (start, signal, exited, error, started) use
the same protobuf messages defined in `proxy.proto`. The gateway
serializes/deserializes via the same prost types the gRPC client
uses — no JSON, no parallel encoding. Frame codec lives in
`crates/api/src/frame.rs` with `Frame::encode(&self) -> Bytes` and
`Frame::decode(&[u8]) -> Result<Frame, FrameError>` helpers.

## URL surface (concrete)

```
WS  /v1/sandboxes/{id}/exec
WS  /v1/sandboxes/{id}/files/read?path=...&cwd=...   (read streamed as frames)
POST /v1/sandboxes/{id}/files/write_file              (unchanged - unary)
POST /v1/sandboxes/{id}/files/write_files             (unchanged - unary)
GET /v1/sandboxes/{id}/files/read?path=...&cwd=...    (existing unary; kept for
                                                       small-file convenience)

# REMOVED:
POST /v1/sandboxes/{id}/exec   <— message-shaped exec is gone
```

Exec params (command, cwd, env, tty) are sent in the FIRST WebSocket
frame as a JSON-encoded `IoStart` body in kind=0x01 (or a dedicated
0x00 "init" kind — final choice during implementation, will be
documented in `frame.rs`).

## Authentication on the WebSocket upgrade

Same model as v0.7 REST endpoints (NFR-API-1): caller supplies
`Authorization: Bearer <API_KEY>` on the upgrade request. Auth is
validated **before** the WebSocket upgrade completes — a missing
or invalid key returns HTTP `401 Unauthorized` with the standard
error envelope; the upgrade never happens.

After upgrade, no further auth checks: the established WebSocket is
the authenticated session. Closing the WebSocket ends the session;
re-opening requires re-authenticating.

TLS terminates at the gateway in production (`wss://`); the
docker-compose dev stack uses plain `ws://` over the gateway's
internal port. Frame format is identical across both schemes.

If the gateway runs behind a reverse proxy that strips or alters
the `Authorization` header, callers must use a query-string fallback
(`?api_key=...`) — flagged but not in v1.0 scope; revisit when a
deployment hits the case.

## Connection model (per D2)

```rust
// crates/api/src/proxy_client.rs (new file)

pub struct ProxyClientPool {
    channels: Vec<Channel>,  // small pool (default: 4)
    rr: AtomicUsize,
}

impl ProxyClientPool {
    pub async fn connect(proxy_url: &str, size: usize) -> Result<Self, ApiError>;

    pub fn next_client(&self) -> SandboxIoServiceClient<Channel> {
        let i = self.rr.fetch_add(1, Ordering::Relaxed) % self.channels.len();
        SandboxIoServiceClient::new(self.channels[i].clone())
    }
}
```

Pool size 4 default — enough to provide HTTP/2 stream headroom for
~400 concurrent sessions (each channel allows 100 streams by default)
without making the gateway "many connections." Configurable via env
var if 12.6 finds this insufficient.

## Idle keepalive (per spike 03)

The WebSocket handler wraps the upgraded socket in a keepalive task:

```rust
async fn keepalive(ws: &mut WebSocket) {
    let mut interval = tokio::time::interval(WS_IDLE_PING_INTERVAL);  // 30s
    let mut last_pong = Instant::now();
    loop {
        tokio::select! {
            _ = interval.tick() => {
                ws.send(Message::Ping(Vec::new())).await?;
                if last_pong.elapsed() > WS_IDLE_PING_TIMEOUT {
                    return Err(ApiError::IoStreamFailed { detail: "ping timeout".into() });
                }
            }
            msg = ws.recv() => match msg? {
                Message::Pong(_) => last_pong = Instant::now(),
                Message::Close(_) => return Ok(()),
                ...
            }
        }
    }
}
```

Runs concurrently with the frame-pump loop via `tokio::select!`.

## TDD cycle expectations

- **Red:**
  - WebSocket integration test: `cargo test ws_exec_echo` connects
    via `tungstenite::connect`, sends a 0x01 stdin frame, receives
    0x11 stdout frames, expects 0x13 exited. Mock proxy.
  - Idle keepalive test: client connects, sleeps 90s, server should
    have pinged 3x and not closed.
  - Disconnect test: client drops; gateway-side WebSocket future
    completes within 1s (spike 03 said ~7ms while sending, ≤90s
    while idle bounded by ping timeout).
- **Green:** implement the WS handler, the frame codec, the gRPC
  forwarder, the pool, the keepalive task.
- **Refactor:** the WS handler should NOT know about gRPC details;
  it talks to a `BoxStream<Frame>` abstraction. Verify the seam
  is clean.
- **E2E (mocked peers):** see acceptance criterion.

## Acceptance criterion

```bash
# WS integration tests in the api crate.
cargo test -p open-sandbox-api --test ws_streaming_e2e

# Required scenarios:
# 1. echo_roundtrip:     bash -c "cat" + stdin=hello → stdout=hello
# 2. backpressure:       slow client (1KB/s read) + busy producer
#                        (100MB/s emit) → gateway RSS stays bounded
# 3. signal_term:        sleep 60 + SIGTERM frame → exited{exit=143}
# 4. disconnect_kills:   sleep 60, drop WS, observe sandbox PID gone
# 5. idle_keepalive:     bash -i held open 90s with no traffic;
#                        verify 3 pings sent and session alive
# 6. unknown_sandbox:    open WS for /v1/sandboxes/bogus/exec →
#                        close code 4404, IoError SANDBOX_GONE
# 7. unauth:             open WS without API key → close 4401
```

Also: a manual smoke against the docker-compose.full stack using
the new ws-client binary:

```bash
opensandbox-exec --base http://localhost:18081 \
                 --api-key $KEY \
                 --sandbox $SB \
                 -- bash -c 'echo hello'
```

### Observability requirements (12.4)

**Prometheus metrics** (in `crates/api/src/metrics.rs`):

- `gateway_active_ws_sessions{kind}` (gauge; kind ∈ `{exec, read_file}`)
- `gateway_ws_session_duration_seconds` (histogram)
- `gateway_ws_session_closes_total{close_code}` (counter)
- `gateway_proxy_pool_size` (gauge)
- `gateway_proxy_pool_active_streams` (gauge)
- `gateway_proxy_open_io_stream_failures_total{grpc_code}` (counter)
- `gateway_ws_pings_sent_total`, `gateway_ws_pong_timeouts_total`

**Structured tracing events:**

- `ws.upgrade_authorized { sandbox_id }`
- `ws.upgrade_rejected { reason }`
- `ws.session_started { stream_id, sandbox_id }`
- `ws.client_disconnected { stream_id, close_code }`
- `ws.idle_ping_timeout { stream_id }`
- `proxy_pool.channel_opened { channel_idx }`
- `proxy_pool.channel_lost { channel_idx, reason }`

The acceptance test for 12.4 scrapes `/metrics` mid-session and
asserts the active-sessions gauge reflects reality.

## Smoke test (post-merge)

```bash
# Confirm the unary lifecycle endpoints still work.
curl -s -X POST http://localhost:18081/v1/sandboxes \
  -H 'content-type: application/json' \
  -d '{"image":"alpine"}'
# Expected: 201, sandbox_id present.

curl -s http://localhost:18081/v1/sandboxes  # list works
```

## Risks

- **WebSocket-via-HTTP/1.1 vs HTTP/2.** axum WebSocket uses the
  HTTP/1.1 Upgrade mechanism by default. If we later need HTTP/2
  WebSocket (RFC 8441) we'd need a different code path. Mitigation:
  not needed for v1.0; the v1.1 transparent-WS amendment will revisit.
- **JSON exec params in the first frame.** Means a small protobuf↔JSON
  boundary. Mitigation: only the first frame carries JSON; everything
  after is raw bytes. Codec lives in `frame.rs`, isolated.
- **Pool sizing wrong.** Mitigation: 12.6's slow-client scenario
  exercises pool behavior; pool size is configurable.

## Effort

L. ~3–4 days.

---

# 12.5 — Controller cleanup

**Branch:** `module/exec-streaming-5-controller-cleanup`
**Depends on:** 12.1, 12.4 (the new path must be live before deleting
  the old one).
**Effort:** S (1 day, mostly deletion)

## Purpose

Remove the message-shaped exec from the controller. After 12.4, no
public caller reaches `ExecSandbox` RPC; the controller's
`ExecBroker`, `EXEC_TIMEOUT`, exec_id pending map, and the agent
stream's `ExecCommand`/`ExecResult` variants can all go.

## Files that change

```
crates/controller/src/exec_broker.rs                     [DELETED]
crates/controller/src/management.rs                      [edit — drop exec_sandbox impl]
crates/controller/src/grpc.rs                            [edit — drop exec_broker field]
crates/controller/src/lib.rs                             [edit — drop mod]
crates/controller/src/controller_handler.rs             [edit — drop ExecResult handling]
crates/contracts/src/constants.rs                        [edit — drop EXEC_TIMEOUT]
crates/agent/src/controller_client.rs                    [edit — drop ExecCommand handler]
```

This is a pure removal pass. The work is mechanical: delete the file,
compile, follow the breakage, delete the call sites.

Expected casualties of the breakage pass (callsite rewrites or
deletions, not exhaustive — discovered by following compiler errors):

- `crates/api/src/tests.rs` — all v0.7 unary exec tests
  (`exec_returns_stdout_and_exit_code`, `exec_rejects_unknown_fields`,
  `exec_rejects_both_stdin_and_stdin_b64`,
  `exec_surfaces_command_not_found_in_response_envelope`,
  `exec_passes_stdin_through`) become invalid because the surface is
  gone. Delete them; their behavioral coverage moves to 12.4's WS
  tests and 12.6's e2e scenarios.
- Controller integration tests that construct `ExecResult` directly
  for assertions — delete or rewrite.

## TDD cycle expectations

- **Red:** N/A (removal). The "red" signal is that `cargo build`
  fails BEFORE the removal at the reserved-field protos from 12.1
  (which it will — that's the smoke test from 12.1). The "green"
  is that after removal, the workspace builds again.
- **Green:** delete the file, compile, follow errors, delete call
  sites. Workspace builds.
- **Refactor:** check that no `exec_broker`, `EXEC_TIMEOUT`, or
  `ExecResult` references remain.

  ```bash
  grep -r 'exec_broker\|EXEC_TIMEOUT\|ExecResult' crates/
  # Expected: empty.
  ```

- **E2E:** existing controller unit + integration tests must still
  pass.

## Acceptance criterion

```bash
cargo build --workspace                            # passes
cargo test -p open-sandbox-controller              # all pass
grep -r 'exec_broker\|EXEC_TIMEOUT' crates/        # empty
ls crates/controller/src/exec_broker.rs            # No such file
```

Controller binary shrinks (verify with `ls -la target/release/open-sandbox`
before and after — expected to drop ~50–100KB of code size).

## Smoke test (post-merge)

```bash
# End-to-end against docker-compose.full: lifecycle still works
# without the exec path on the controller.
docker compose -f infra/e2e/docker-compose.full.yml up -d
sleep 8
curl -s -X POST http://localhost:18081/v1/sandboxes \
  -H 'content-type: application/json' -d '{"image":"alpine"}' \
  | python3 -m json.tool
curl -s http://localhost:18081/v1/sandboxes | python3 -m json.tool
```

Lifecycle endpoints respond correctly; exec is only reachable via
WebSocket (by design).

## Risks

- **Hidden dependency.** Mitigation: `cargo build --workspace` is
  the verifier; if anything depends on the removed types, it fails
  loudly. Follow the compiler.
- **Test fixture references.** Some tests may construct
  `ExecResult` directly for assertions. Mitigation: those tests
  belong to 12.5's "follow the breakage" pass.

## Effort

S. ~1 day.

---

# 12.6 — Live e2e scenarios

**Branch:** `module/exec-streaming-6-live-e2e`
**Depends on:** 12.1–12.5 all merged to the integration branch.
**Effort:** M (2–3 days, mostly test authoring + Docker stack work)

## Purpose

The protocol's `live-verified` gate for the whole amendment. Scripted
scenarios that exercise the streaming exec path against the real
docker-compose stack, both runtimes. This is the final confidence
check before the amendment merges to `main`.

## Files that change

```
infra/e2e/scenarios/                                     [NEW dir]
infra/e2e/scenarios/01-echo.sh                           [NEW]
infra/e2e/scenarios/02-backpressure.rs                   [NEW]
infra/e2e/scenarios/03-signal-term.sh                    [NEW]
infra/e2e/scenarios/04-disconnect-kills.sh               [NEW]
infra/e2e/scenarios/05-idle-keepalive.sh                 [NEW]
infra/e2e/scenarios/06-long-running.sh                   [NEW]
infra/e2e/scenarios/07-command-not-found.sh              [NEW]
infra/e2e/scenarios/08-write-then-exec.sh                [NEW]
infra/e2e/scenarios/run-all.sh                           [NEW]
infra/e2e/scenarios/run-all-youki.sh                     [NEW — runs on Linux only]
```

## Scenario details

Each is a self-contained shell or Rust script that:
- Brings up `docker-compose.full.yml`
- Creates a sandbox
- Exercises one specific behavior end-to-end via WebSocket (using
  `wscat` or a small Rust client in `infra/e2e/scenarios/wsclient/`)
- Asserts the expected outcome
- Tears down

**01-echo:** Open WS exec `["bash", "-c", "cat"]`. Send stdin "hello
world\n". Expect stdout "hello world\n" + IoExited{exit=0}.

**02-backpressure:** Open WS exec `["bash", "-c", "yes 'XXXXXXXXXXX' |
head -c 200M"]`. Client reads at 1 MB/s. Measure gateway RSS via
`docker stats e2e-api-1` over the run. Assert RSS does NOT grow
beyond a sane bound (e.g., 100 MB).

**03-signal-term:** Open WS exec `["sleep", "60"]`. After 2s, send a
SIGTERM frame (kind=0x02, payload=15). Expect IoExited{exit=143} within
~1s of the signal.

**04-disconnect-kills:** Open WS exec `["bash", "-c", "sleep 20; touch
/tmp/marker"]`. After 2s, abrupt WS close (TCP RST equivalent). Wait
10s (the agent's default grace is 5s; 10s gives margin without
needlessly slowing the suite). `cat /tmp/marker` inside the sandbox
via a fresh WS exec → file does NOT exist (process was killed by the
registry).

**05-idle-keepalive:** Open WS exec `["bash", "-i"]`. Idle 90s with no
traffic. Verify the WS connection is still alive and the bash process
is still running. (Tests both gateway ping/pong and that no premature
timeout kicks in.)

**06-long-running:** Open WS exec `["bash", "-c", "for i in $(seq 1
200); do echo $i; sleep 1; done"]`. Run for 200s (3.3 min — past the
old 60s EXEC_TIMEOUT). Receive all 200 lines. Confirm the
`EXEC_TIMEOUT` ceiling is gone.

**07-command-not-found:** Open WS exec
`["definitely_not_a_binary"]`. Expect IoExited{exit=127,
command_not_found=true} and stderr (NOT stdout) carrying the OCI
"executable file not found" message. Validates that v0.7's
command-not-found behavior is preserved through the streaming path.

**08-write-then-exec:** `POST /files/write_file` with a Python script.
Open WS exec `["python3", "script.py"]`. Expect script output. Tests
that the new first-class WriteFile (no shell helper) works with
exec immediately after.

Each scenario emits a single `PASS` or `FAIL <reason>` line, plus
optional diagnostic JSON. `run-all.sh` runs every scenario sequentially
and prints a summary.

## TDD cycle expectations

- **Red:** scenarios fail (no streaming path implemented end-to-end
  yet).
- **Green:** scenarios pass against the integration branch.
- **Refactor:** factor common WS-client logic into a Rust helper at
  `infra/e2e/scenarios/wsclient/` — every scenario should be a thin
  setup/assert pair on top of it.
- **E2E:** by construction.

## Acceptance criterion

```bash
# All scenarios pass on docker runtime.
infra/e2e/scenarios/run-all.sh
# Expected output: 8/8 PASS.

# On a Linux host, also run the youki variant.
infra/e2e/scenarios/run-all-youki.sh
# Expected output: 8/8 PASS.
```

Plus: load tracing logs from the agent and verify that exec lifecycle
events (`io_session.start`, `io_session.exec_pid_captured`,
`io_session.close`, `exec_registry.signal_sent`) are present.

## Smoke test (post-merge)

`run-all.sh` runs as part of CI on every PR thereafter.

## Risks

- **WebSocket clients in bash are limited.** Mitigation: small Rust
  WS client lives at `infra/e2e/scenarios/wsclient/` and is invoked
  from the scenario shell scripts.
- **Backpressure measurement is noisy.** Mitigation: use a generous
  ceiling (100 MB), not a tight assertion; the spike already showed
  the property works structurally.
- **Youki scenarios require Linux.** Mitigation: documented in
  scenario file headers; CI uses Linux runners for the youki suite.

## Effort

M. ~2–3 days.

---

# 12.7 — Migration & documentation

**Branch:** `module/exec-streaming-7-migration`
**Depends on:** 12.4 merged (so the new public surface exists),
  12.5 merged (so removed surface is final), 12.6 green (so the
  examples actually run).
**Effort:** S (1 day)

## Purpose

v1.0 is a breaking public API change. v0.7 callers (SDKs, the
example agent loop in this repo's chat history, any external
integrators) need a migration path. This sub-module produces the
caller-facing artifacts: a migration guide, updated example code,
SPEC.md amendment, and the changelog entry for the v1.0.0 release.

Without this, the amendment leaves users to discover the breakage
by failing requests.

## Files that change

```
MIGRATION_v0.7_to_v1.0.md              [NEW]
CHANGELOG.md                            [NEW or edit if exists]
SPEC.md                                 [edit — supersede FR-12 wire shape, mark base64 exec response as removed]
README.md                               [edit if exists — quick-start example uses ws-client]
crates/ws-client/README.md              [NEW — SDK usage]
crates/ws-client/examples/echo.rs       [NEW — minimal example]
crates/ws-client/examples/long-running-build.rs    [NEW — demonstrates v0.7's H1 closure]
crates/ws-client/examples/interactive-bash.rs      [NEW — demonstrates D5 (exec-as-session)]
infra/e2e/scenarios/README.md           [NEW — points at examples]
```

## MIGRATION doc structure

```
# Migrating from v0.7 to v1.0

## TL;DR
- POST /v1/sandboxes/{id}/exec  →  WS /v1/sandboxes/{id}/exec
- POST /v1/sandboxes/{id}/files/read  →  unchanged (REST kept for small reads)
- POST /v1/sandboxes/{id}/files/write_file  →  unchanged
- Exec response shape: ExecResponseBody{exit_code,stdout_b64,stderr_b64,error_code?}
                       →  stream of Frame{kind, payload} terminating in IoExited
- 60s exec timeout: gone. Sessions live as long as the WebSocket.
- `error_code: COMMAND_NOT_FOUND` envelope field
   →  IoExited{exit_code:127, command_not_found:true}

## Cookbook
  ### "Run a command, get the output" — was 1 HTTP call, is now 1 WS session
  ### "Pipe stdin into a command"   — was 1 HTTP call with stdin_b64
                                       in JSON, is now stdin frames on the WS
  ### "Long-running build"          — was impossible (60s ceiling),
                                       now works
  ### "Cancel a stuck command"      — was impossible, now WS close
                                       (or signal frame)
  ### "Interactive shell session"   — was impossible, now `bash -i`
                                       held over WS

## Concrete diffs
  ### shell with curl (before / after)
  ### Python `requests` (before / after)
  ### Rust with ws-client (the new SDK shape)
```

## TDD cycle expectations

- **Red:** none — documentation work.
- **Green:** all examples in MIGRATION compile / run.
- **Refactor:** verify no stale references to `/exec` POST or
  `ExecResponseBody` remain in user-facing docs.
- **E2E:** the three example binaries (`echo`, `long-running-build`,
  `interactive-bash`) run end-to-end against
  `docker-compose.full.yml`.

## Acceptance criterion

```bash
# Examples compile.
cargo build -p ws-client --examples

# Examples run against a live stack.
docker compose -f infra/e2e/docker-compose.full.yml up -d
SB=$(curl -s -X POST http://localhost:18081/v1/sandboxes \
        -H 'content-type: application/json' \
        -d '{"image":"alpine"}' | jq -r .sandbox_id)
cargo run -p ws-client --example echo -- --sandbox $SB
cargo run -p ws-client --example long-running-build -- --sandbox $SB
cargo run -p ws-client --example interactive-bash -- --sandbox $SB --once
# All three exit 0.

# Migration doc references no removed types.
grep -E 'ExecResponseBody|stdout_b64|/exec POST|EXEC_TIMEOUT' \
     MIGRATION_v0.7_to_v1.0.md
# Expected: empty (or only in "removed" sections explicitly).
```

## Smoke test

A docs-only check: render the migration doc and ensure all internal
links resolve.

```bash
# If we adopt mdbook later, `mdbook test` would cover this. For v1.0,
# manual review of the rendered doc is sufficient.
```

## Risks

- **Drift risk:** if 12.4 changes the WS surface late in the cycle,
  the migration doc and examples have to follow. Mitigation: 12.7
  runs LAST, after 12.6 green. The surface is frozen by then.

## Effort

S. ~1 day.

---

# Cross-cutting concerns

## Testing strategy

- **Unit tests** per sub-module against mock peers. Run on every
  PR via `cargo test --workspace` in CI.
- **Mocked integration tests** (12.2, 12.3, 12.4) — agent against
  mock proxy, proxy against mock agent + mock gateway, gateway
  against mock proxy. Each catches integration issues without
  needing the full stack.
- **Live e2e scenarios** (12.6) — full stack, both runtimes,
  scripted. The contractual proof of the amendment.

## Rollback strategy

If the amendment proves problematic mid-implementation:

1. The integration branch (`contracts/amendment-exec-streaming`) is
   never merged back to `main` until 12.6 passes. So `main` stays
   on `contracts/v0.7.0-frozen` and the v0.7 message-shaped exec
   keeps working.
2. Sub-module branches are revertible individually. Reverting
   12.5 (controller cleanup) is the only one that affects the wire
   protocol — but since the integration branch hasn't merged, no
   external party has seen v1.0 contracts yet.
3. If 12.6 reveals a design flaw, return to `EXEC_STREAMING_DESIGN.md`
   and amend the design BEFORE touching code; rerun spikes if
   load-bearing assumptions are affected.

## What this amendment does NOT address

- **M3 — sandbox inspect endpoint** (additive; separate amendment).
- **L2 — list-after-delete state coherence** (docs note; separate).
- **L3 — public URL in create response** (additive; separate).
- **v1.1 transparent WebSocket forwarding** (public-side inbound WS;
  enabled architecturally by v1.0 but not implemented here).
- **Streaming logs** (additive; reuses the v1.0 frame envelope).
- **PTY allocation** (deliberately not in the v1.0 proto; arrives
  in v1.1 as an additive proto change once it has an implementation).
- **Query-string API key fallback for WS** (only needed when a
  reverse proxy strips the Authorization header — revisit when a
  real deployment hits this).
- **Multi-host gateway↔proxy with mTLS** (the v1.0 internal authn
  uses network isolation + shared secret; mTLS pattern is documented
  but a separate operational concern, not contract change).

Explicitly out of scope. Document in `SPEC.md` as "v1.1 / additive."

---

# Final confidence gate

```
Confidence: high

Residual risks:
  - 12.2 is the largest sub-module and touches both runtimes. Spike 01
    and spike 02 confirmed the load-bearing assumption (both backends
    need explicit kill plumbing); the implementation path is
    well-understood but the matrix is wider than any prior amendment.
    Mitigation: 12.2 is testable against mock proxy frames before any
    of 12.3/12.4 are done, so it can be developed in isolation with
    fast iteration.

  - 12.3's "extend TunnelRequest/TunnelResponse oneofs" approach
    (Option A) reuses the existing tunnel pump but couples HTTP and
    I/O variants. If this coupling becomes painful in v1.1
    (transparent inbound WS forwarding) we may regret the choice
    and split the pumps. Worth flagging during v1.1 design.

  - WebSocket-on-HTTP/1.1 vs HTTP/2: axum's WS is HTTP/1.1 Upgrade.
    Browsers don't care; SDKs don't care; CLI tools don't care.
    HTTP/2 WebSocket (RFC 8441) is not widely supported anyway.
    Acceptable for v1.0.

  - 12.4 introduces a connection pool to the proxy with default
    size 4. If actual load exceeds 400 concurrent streams (~100
    per HTTP/2 channel), gateway must increase pool size or run
    multiple gateway replicas. Configurable; not a contract change.

  - 12.6 scenarios on youki can only run on Linux. macOS dev iteration
    on the youki path is limited to cargo check + the existing
    docker-compose.full.yml stack (which uses docker runtime). The
    youki live-verified tag will be gated by CI Linux runners.

  - Docker backend's `signal_exec` depends on `kill` being present
    in the sandbox image. Common base images bundle it; minimal
    `scratch` images do not. Mitigation: documented in SPEC.md as a
    sandbox-image requirement alongside the existing `tar`
    requirement; runtime logs a WARN on detection failure and falls
    back to letting the process run to natural completion. Youki
    backend bypasses this via direct syscalls.

  - Internal authn between gateway and proxy uses a shared secret
    via env var (Layer 2). If the deployment topology evolves to
    cross-host, mTLS replaces network isolation; the shared-secret
    layer stays. Operational concern, not contract.

Known gaps:
  None blocking. Design doc is the source of truth; spike results
  confirm the load-bearing assumptions; the DAG is acyclic; every
  sub-module has a concrete acceptance criterion including
  observability requirements; migration sub-module (12.7) covers
  the user-visible bridge from v0.7 to v1.0.
```

This plan revision is `plan/v0.6.1` once committed and tagged
(supersedes the v0.6.0 pre-review draft).
