# CHANGELOG

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
