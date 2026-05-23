# v1.0.1 follow-ups

Gaps surfaced by the post-amendment audit (PLAN_EXEC_STREAMING.md
vs. actual implementation) that did NOT block merging the v1.0
amendment to `main`, but should be addressed before tagging
`contracts/v1.1.0-frozen` (or sooner where security-relevant).

Prioritized order matches the user's stated focus during the audit.

## P1 — CHANGELOG ↔ implementation mismatch (`WS /files/read`)

`CHANGELOG.md` advertises:

```
WebSocket I/O: WS /v1/sandboxes/{id}/exec, WS .../files/read
```

…but the streaming `WS .../files/read` route does not exist in
`crates/api/src/router.rs`. The unary `GET /v1/sandboxes/{id}/files/read`
endpoint survives and is wired through `OpenIoStream` with an
`IoStart::ReadFile` variant.

**Resolution options:**

- **Implement.** Add `crates/api/src/ws_read_file.rs`, route as
  `WS /v1/sandboxes/{id}/files/read?path=...`, drive an
  `IoStart::ReadFile` over the same `ProxyClientPool`, stream
  bytes back as WS binary frames. The agent's runtime
  `read_file` already returns a `Bytes`; for a true streaming
  read we'd want to change the agent trait to return a stream,
  which is invasive.
- **Drop the line from CHANGELOG.** The unary `GET` endpoint
  covers small-file reads (the primary AI-agent use case);
  streaming reads are a v1.1 ergonomic improvement, not a v1.0
  requirement.

Recommended: drop the line, document the streaming variant as a
v1.1 ergonomic; revisit when there's a concrete consumer that
needs > 64 MiB single-file reads.

## P2 — single proxy gRPC listener (restore the two-listener split)

The proxy currently binds ONE TCP port (`crates/cli/src/run.rs`)
hosting both:

- `OpenTunnel` — agent ingress, must reach the public internet.
- `OpenIoStream` — gateway egress to proxy, must be reachable
  ONLY by the api gateway process.

Plan called for a separate internal-only listener on its own
port (e.g. 50053) as the **primary** defense. The bearer-token
check in `OpenIoStream` is currently the only guard; if an
attacker reaches the public proxy port and exfiltrates the
shared secret, they can dispatch `IoStart` frames against any
sandbox.

**Resolution sketch:**

- Split `run_proxy` to bind two `tonic::Server`s:
  - public listener (existing): only the `OpenTunnel` service
  - internal listener (new): only the `OpenIoStream` service,
    bound to a separate port and (in production) a separate
    interface or loopback-only
- `docker-compose.full.yml` and the operator-facing
  configuration grow a `PROXY_INTERNAL_PORT` knob.
- The api gateway's `ProxyClientPool` reads the new port from
  config; existing `INTERNAL_TOKEN` bearer check stays as
  defense-in-depth.
- A test scenario that attempts `OpenIoStream` against the
  public port should be rejected at the listener level
  (connection refused / no such service), not by the bearer
  check.

This is the highest-impact security fix in the v1.0.1 batch —
without it, deployments that don't enforce network isolation
between proxy and untrusted callers rely on a single shared
secret.

## P3 — youki file ops via `cat`/`tee`/`tar` instead of setns syscalls

`crates/agent-youki/src/lib.rs` implements `read_file`,
`write_file`, and `write_files_targz` by invoking
`start_exec_streaming` against `cat`, `tee`, and `tar` inside the
container. The plan's intent (and the structural-purity ideal)
was to use `setns(2)` to enter the container's mount namespace
and then perform direct file syscalls in the agent's process.

**Why it matters:**

- Reintroduces the binary-dependency-in-image footprint the v1.0
  refactor explicitly removed for the docker backend's case.
  Pure-distroless sandbox images don't work today.
- The wrapper-script in-container-PID capture (`sh -c '... exec
  "$@"'`) ALREADY requires `sh` in the image; adding `cat`,
  `tee`, and `tar` widens the footprint further.

**Resolution sketch:**

- New `youki::syscalls` module: `setns_into_container(pid, ns)`,
  `read_file_via_ns(path)`, `atomic_write_via_ns(path, bytes)`.
- The agent process must be `CAP_SYS_ADMIN` and run on the same
  host as the container — which it already is.
- Add a `Drop` guard that re-enters the agent's original
  namespace, to keep the setns scoped per-call.
- Tests: live-verify against the same scenario 08 plus a new
  scenario that runs in a distroless sandbox image (no `sh`,
  `cat`, `tar`).

## P4 (deferred, tracked here for visibility) — gaps acknowledged but not closed for v1.0.1

These came up in the audit but were explicitly accepted as
v1.0 limitations (operator-resolvable for now). Listed here so
future sessions know they exist.

- **Prometheus metrics.** Eleven metrics were specified across
  agent and gateway; none are implemented. The metrics surface
  itself (HTTP `/metrics` endpoint, `prometheus` crate) is also
  not wired. Plan called metrics "part of acceptance"; current
  ops story is "read tracing logs".
- **Tracing events not in code.** `io_session.client_disconnected`
  (agent), `ws.upgrade_rejected`, `proxy_pool.channel_opened`,
  `proxy_pool.channel_lost` (gateway). Logging-only gaps; no
  functional impact.
- **e2e missing `run-all-youki.sh`** and scenario 02 is a bash
  bulk-transfer rather than the Rust RSS-measurement
  backpressure test the plan specified.

## Out of scope

Anything that requires a contract change (new RPCs, modified
message shapes) is by definition not v1.0.1 — it would need its
own `contracts/amendment-<name>` branch and a minor or major
version bump.
