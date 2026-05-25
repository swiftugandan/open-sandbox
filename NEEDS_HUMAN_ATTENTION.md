# Needs Human Attention

Log of issues surfaced during the autonomous component-by-component review
(`CODE_REVIEW_PLAN.md`) that I could not resolve without a decision, a
deferred contract change, or live-environment validation. Each entry names
the component, the blocker class, and the recommended next step.

This file is append-only during the review pass. Once you've triaged an
entry, prepend `[done YYYY-MM-DD]` to the heading or remove it.

---

## Conventions

- **Component:** which review slot in `CODE_REVIEW_PLAN.md` surfaced it.
- **Blocker class:** `decision`, `contract-change`, `live-validation`, `external-dep`.
- **Recommended next step:** the single thing that unblocks me.

---

## Status as of the 2026-05-25 blitz

User accepted recommended options for the four big decisions. Everything below
either landed on `main` already (with a `**closed**` note) or is now waiting
on the items in this file. The contracts `v1.0.2` additive amendment landed
(tag exists); the downstream cascade is still pending per
`PLAN_CONTRACTS_v1.0.2.md`.

---

## [comp-2 · live-validation] PG-side end-to-end LISTEN/NOTIFY needs a real Postgres

- **Blocker class:** `live-validation`
- **What I shipped:** controller emits `pg_notify('routing_changed', json)` inside each routing-table mutation transaction; proxy spawns a `PgListener` and parses notifications into `cache.insert` / `cache.remove_by_sandbox_id` calls. Schema parser has unit tests; the listener task itself has no unit test.
- **What you need to do:** run `crates/controller/tests/live_e2e.rs` (or a new proxy-side live test) against a real Postgres. Verify (1) deletion → notify → cache evict within a single round-trip; (2) inserts visible to the proxy without waiting for the 30s periodic refresh; (3) listener reconnects cleanly when the PG connection drops.

## [comp-2 · decision] try_send silently drops disconnect notifications (C2)

- **Blocker class:** `decision`
- **Source:** comp-2 Angle C
- **File:** `crates/proxy/src/io_sessions.rs:97` (`fail_stream`)
- **Summary:** when the gateway-side `server_tx` channel is full, the agent-disconnect error sent via `try_send` is silently dropped. The session record is then removed, `server_tx` drops, and the gateway observes a clean stream EOF rather than a terminal `Unavailable`. The HoL fix bumped the buffer to 256 frames, making this rare but still possible.
- **Recommended next step:** spawn a fallback `tokio::spawn(async move { let _ = tx.send(Err(...)).await; })` so the error eventually lands. ~10 LOC.

## [comp-3 · decision] Spawned io-session tasks leak on tunnel disconnect (A4)

- **Blocker class:** `decision`
- **Source:** comp-3 Angle A
- **File:** `crates/agent/src/proxy_client.rs:214, 228`
- **Summary:** the per-session `drive_io_session` and outbound pump tasks are spawned detached. When `ProxyConnection::run` returns (now common, since A1 introduced reconnect loops), the local `io_sessions` HashMap drops, which eventually closes every per-session in_tx — but each `drive_io_session` then sits in `cleanup` for `EXEC_KILL_GRACE` before exiting.
- **Recommended next step:** track `JoinHandle<()>` for each spawned per-session task; on `ProxyConnection::run` return, abort them all. ~25 LOC.

## [comp-3 · decision] stop_sandbox doesn't notify in-flight ExecRegistry sessions (B3)

- **Blocker class:** `decision`
- **Source:** comp-3 Angle B
- **File:** `crates/agent/src/sandbox.rs:85` (`stop_sandbox`)
- **Summary:** when a sandbox is stopped, in-flight exec sessions discover the container is gone via the runtime backend's exit detection. Gateway clients may see "stream ended without terminal frame" instead of a clean `IoError(SANDBOX_GONE)`.
- **Recommended next step:** add a `cancel_tx` to `ExecRecord` so `SandboxManager::stop_sandbox` broadcasts a terminal `IoError(SANDBOX_GONE)` before tearing down the container. ~50 LOC.

## [comp-3 · decision] Application-level keepalive on the agent's proxy tunnel (B6)

- **Blocker class:** `decision`
- **Source:** comp-3 Angle B
- **File:** `crates/agent/src/proxy_client.rs:51` (OpenTunnel dial)
- **Summary:** comp-2 B4 added HTTP/2 keepalive on the proxy's server side. The reverse direction (proxy frozen, agent still believes the tunnel is live) is uncaught — the agent has no application-level ping.
- **Recommended next step:** configure tonic client-side HTTP/2 keepalive via `Channel::from_shared(addr).keep_alive_while_idle(true).keep_alive_timeout(Duration::from_secs(20)).http2_keep_alive_interval(Duration::from_secs(15))`. Smallest fix; no contract change.

## [comp-3 · cross-component] SandboxStatus(Stopped) never persisted because release_sandbox runs first (C2)

- **Blocker class:** `cross-component` (controller-side fix; folded into v1.0.2 plan)
- **File:** controller-side at `crates/controller/src/management.rs:152-155` and agent-side at `crates/agent/src/controller_client.rs:144`
- **Status:** documented in PLAN_CONTRACTS_v1.0.2.md as bonus item #11. Defer until the v1.0.2 cascade lands.

## [comp-4 · decision] Image pull has no retry, no progress visibility, no in-flight dedup

- **Blocker class:** `decision`
- **File:** `crates/agent-docker/src/lib.rs:62`
- **Recommended next step:** (a) wrap pull in bounded retry with exponential backoff (~30 LOC); (b) process-wide dedup map keyed by image (~80 LOC); (c) defer pull retries to the controller's StartSandbox retry policy. Tell me which.

## [comp-4 · decision] inspect_exec failure conflated with process exit -1

- **Blocker class:** `decision`
- **File:** `crates/agent-docker/src/lib.rs:350`
- **Summary:** output-pump emits `ExecExitInfo { exit_code: -1, command_not_found: false }` for both "process exited -1" and "inspect_exec failed". Agent-core's natural-exit fast path then skips cleanup → orphaned in-container PID on daemon restart.
- **Recommended next step:** introduce a runtime-error path on the `exited` channel so io_stream's runtime-error branch fires the cleanup hook. ~15 LOC + a small ContainerRuntime trait extension. Confirm and I ship.

## [comp-5 · live-validation] OCI security defaults — caps/masked/pids landed; seccomp + userns + readonly_rootfs still deferred

- **Blocker class:** `decision` + `live-validation`
- **File:** `crates/agent-youki/src/spec.rs`
- **What I shipped:** docker-default capability set + no_new_privileges + masked_paths + readonly_paths + pids limit 256 (`fix/oci-hardening`).
- **Still deferred:**
  1. **seccomp profile**: vendor docker's default ~330-line JSON, or roll a narrower one?
  2. **user namespace + subuid/subgid mapping**: requires kernel ≥5.11 + host-side `/etc/subuid` setup. Operator decision.
  3. **readonly_rootfs + tmpfs overlays**: many images break (apt cache, npm install); need to pick + ship the writable-tmpfs overlay set.
- **What you need to do:** validate the landed hardening on the Linux dev env (`docker compose -f crates/agent-youki/docker-compose.dev.yml exec dev cargo test -p open-sandbox-agent-youki`). Then decide each of the three remaining items.

## [comp-5 · live-validation] Tar layer extraction path-traversal — fix landed; needs Linux validation

- **Blocker class:** `live-validation`
- **File:** `crates/agent-youki/src/image.rs`
- **What I shipped:** three-layer guard (path-component check, whiteout target canonicalize, dest parent canonicalize) on `fix/youki-tar-traversal`.
- **What you need to do:** Linux dev env validation. Add a malicious-tar fixture test if desired.

## [comp-5 · decision] Image install is non-atomic — partial failures corrupt subsequent pulls

- **Blocker class:** `decision`
- **File:** `crates/agent-youki/src/image.rs:42`
- **Recommended next step:** extract to a sibling `<rootfs>.tmp.<uuid>` dir, atomic rename on success, `rm -rf` on failure. ~30 LOC.

## [comp-5 · decision] setns(2) thread doesn't enter PID namespace → /proc-based host access

- **Blocker class:** `decision`
- **File:** `crates/agent-youki/src/setns_ops.rs:57`
- **Summary:** the thread enters the container's mount namespace but not its PID, user, or network namespaces, giving a primitive for reading arbitrary host files and memory via `/proc/<host_pid>/root/...`.
- **Recommended next step:** add `setns(CLONE_NEWPID | CLONE_NEWUSER | CLONE_NEWNET)` before any file op. May require an `unshare(CLONE_NEWPID)` fork dance. ~50 LOC + Linux test.

## [comp-5 · decision] create_and_start has no rollback on partial CNI/start failures

- **Blocker class:** `decision`
- **File:** `crates/agent-youki/src/lib.rs:154`
- **Recommended next step:** mirror the comp-4 docker rollback approach. ~60 LOC.

## [comp-5 · live-validation] Image layer unbounded buffering OOMs on large pulls

- **Blocker class:** `live-validation`
- **File:** `crates/agent-youki/src/image.rs:61`
- **Recommended next step:** stream through the gunzip+tar decoder rather than buffering the layer into a `Vec<u8>`. ~40 LOC.

## [comp-5 · decision] write_files_targz target_dir is client-controlled with no allowlist

- **Blocker class:** `decision`
- **File:** `crates/agent-youki/src/setns_ops.rs:197`
- **Recommended next step:** restrict cwd to a sandbox-prefix like `/workspace/...` (vs leaving it permissive). Comp-5's OCI hardening dominates the threat model here — if cap drop + readonly_rootfs ship, this becomes moot.

## [comp-6 · decision] Wire-level structured error codes vs per-method NotFound mapping

- **Blocker class:** `decision` (folded into v1.0.2 plan)
- **Status:** `contracts/v1.0.2` ships `ERROR_CODE_HEADER` constant. Controller still needs to emit the trailer; api needs to read it. Defer until v1.0.2 cascade.

## [comp-7 · decision] ws-client read-timeout / stale-connection detection

- **Blocker class:** `decision`
- **File:** `crates/ws-client/src/lib.rs:208`
- **Recommended next step:** (a) client sends its own keepalive every 15s + tracks last-server-frame timestamp, declares dead after 60s; (b) configurable per-session `read_timeout` and let the caller drive a watchdog.

## [comp-7 · decision] ws-client frame size limits

- **Blocker class:** `decision`
- **File:** `crates/ws-client/src/lib.rs:141`
- **Recommended next step:** decide the contracted per-frame upper bound (recommend 1 MiB), document in contracts, and set on both sides.

## [comp-9 · decision] Production deployment story — TLS + env passthrough closed; remaining items

- **Closed in `fix/infra-env-passthrough`:** Cloudflare proxied wildcard HTTPS, env passthrough for all auth tokens (deployment blocker), api gateway systemd unit, ports 50052/50053 separation.
- **Still deferred:**
  1. **Postgres password**: cloud-init still trust-on-localhost. Add `ALTER USER postgres PASSWORD ...` + bake the password into `DATABASE_URL`.
  2. **pg_dump backups on same volume**: a volume-loss event wipes both. Add off-host upload (S3, Hetzner Object Storage, etc) + retention policy.
  3. **Binary download is amd64-only on ARM controllers, no checksum**: pin version + verify sha256. The cax11 default is aarch64.
- **Recommended next step:** tell me to ship Postgres password (random-generated via Pulumi, stored as secret, baked into systemd Environment); the backup-off-host wiring is a separate operational choice (which cloud, which bucket).

## [comp-1 · SPEC] Multi-tenancy

- **Blocker class:** `decision` (SPEC-level)
- **Status:** comp-1 F1 closed the immediate exposure with single-tenant admin auth. Per-tenant ownership / API-key-per-tenant / billing-attribution all wait on the SPEC call.

## [bigger work item] contracts/v1.0.2 downstream cascade

- **Blocker class:** `decision` (when to schedule)
- **Status:** `contracts/v1.0.2` tag exists with the additive types. The downstream cascade per `PLAN_CONTRACTS_v1.0.2.md` (~300 LOC across controller / api / proxy / agent / ws-client) is the next focused session's work.
