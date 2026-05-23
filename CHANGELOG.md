# CHANGELOG

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
