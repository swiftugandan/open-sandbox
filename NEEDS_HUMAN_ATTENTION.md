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

## Status as of the 2026-05-25 refactor blitz

User accepted recommended options for the four big decisions (OCI baseline, HoL
multiplex model, upload chunking, schedule v1.0.2). The contracts `v1.0.2`
amendment and downstream cascade landed; the OCI hardening + tar traversal +
atomic image install + cwd allowlist + CNI rollback all validated on the Linux
dev env. Everything below is genuinely open.

---

## [comp-2 · live-validation] PG-side end-to-end LISTEN/NOTIFY needs a real Postgres

- **What's shipped:** controller emits `pg_notify('routing_changed', json)` inside each routing-table mutation transaction; proxy spawns a `PgListener` and parses notifications. Schema parser has unit tests; the listener task itself has no unit test.
- **What you need to do:** point the existing `crates/controller/tests/live_e2e.rs` at a real Postgres + add assertions on the LISTEN side. Verify (1) deletion → notify → cache evict within a single round-trip; (2) inserts visible to the proxy without waiting for the 30s periodic refresh; (3) listener reconnects cleanly when the PG connection drops.

## [comp-3 · decision] stop_sandbox doesn't notify in-flight ExecRegistry sessions (B3)

- **File:** `crates/agent/src/sandbox.rs:85` (`stop_sandbox`)
- **Summary:** when a sandbox is stopped, in-flight exec sessions discover the container is gone via the runtime backend's exit detection. Gateway clients may see "stream ended without terminal frame" instead of a clean `IoError(SANDBOX_GONE)`.
- **Recommended next step:** moderate refactor — add `cancel_tx: watch::Sender<bool>` to `ExecRecord`; thread `Arc<ExecRegistry>` into `SandboxManager` so `stop_sandbox` can walk `list_for_sandbox()` and broadcast cancel. drive_io_session adds a third select arm that emits IoError(SANDBOX_GONE) and breaks. ~80 LOC across 3 files. Tell me to ship.

## [comp-4 · decision] inspect_exec failure conflated with process exit -1

- **File:** `crates/agent-docker/src/lib.rs:350`
- **Summary:** output-pump emits `ExecExitInfo { exit_code: -1, command_not_found: false }` for both "process exited -1" and "inspect_exec failed". Agent-core's natural-exit fast path then skips cleanup → orphaned in-container PID on daemon restart.
- **Recommended next step:** introduce a runtime-error path on the `exited` channel so io_stream's runtime-error branch fires the cleanup hook. ~15 LOC + a small ContainerRuntime trait extension. Confirm and I ship.

## [comp-5 · decision] OCI hardening: seccomp + user namespace + readonly_rootfs still deferred

- **What's shipped (Linux validated):** docker-default capability set + no_new_privileges + masked_paths + readonly_paths + pids limit 256 + tar traversal guard + atomic image install + cwd allowlist + CNI rollback.
- **Still deferred:**
  1. **seccomp profile**: vendor docker's default ~330-line JSON, or roll a narrower one? Operator decision.
  2. **user namespace + subuid/subgid mapping**: requires kernel ≥5.11 + host-side `/etc/subuid` setup. Operator decision (host config).
  3. **readonly_rootfs + tmpfs overlays**: many images break (apt cache, npm install); need to pick + ship the writable-tmpfs overlay set. Operator decision (image compat tradeoff).

## [comp-5 · decision] setns(2) thread doesn't enter PID namespace → /proc-based host access

- **File:** `crates/agent-youki/src/setns_ops.rs:57`
- **Summary:** the thread enters the container's mount namespace but not its PID, user, or network namespaces, giving a primitive for reading arbitrary host files and memory via `/proc/<host_pid>/root/...`. **High-impact security finding** that's still open.
- **Recommended next step:** add `setns(CLONE_NEWPID | CLONE_NEWUSER | CLONE_NEWNET)` before any file op. Requires an `unshare(CLONE_NEWPID)` fork dance (CLONE_NEWPID can only affect children). ~50 LOC + Linux test. Confirm and I ship.

## [comp-5 · live-validation] Image layer unbounded buffering OOMs on large pulls

- **File:** `crates/agent-youki/src/image.rs:61`
- **Recommended next step:** stream through the gunzip+tar decoder rather than buffering the layer into a `Vec<u8>`. ~40 LOC + Linux validation. Confirm and I ship.

## [comp-7 · decision] ws-client read-timeout default policy

- **Status:** `set_read_timeout(Option<Duration>)` shipped with default None (legacy behavior). Calls in CLI / SDKs still need to opt in.
- **Recommended next step:** decide whether to flip the default to ~60s for `opensandbox-exec` to catch silently-broken connections, or leave at None (caller's responsibility).

## [comp-9 · decision] pg_dump backups on same volume

- **Summary:** `pg_dump` writes to `/mnt/data/backups/` — the same Hetzner block volume that holds the live data. A volume-loss event wipes both. RPO is effectively infinite even though the cron claims 6h.
- **Recommended next step:** add off-host upload to S3 / Hetzner Object Storage / similar. Choose the destination and credential model and I'll wire it.

## [comp-9 · decision] Binary download sha256 verification

- **What's shipped:** multi-arch detection (`uname -m` picks amd64 vs arm64).
- **Still deferred:** sha256 verification. Requires the release pipeline to publish a `*.sha256` file alongside each binary; cloud-init then runs `sha256sum -c`. Operational decision (where's the checksum source of truth).

## [comp-3 · decision] HoL across sessions on stdin-heavy uploads + delayed disconnect cleanup

- **Status:** consequence of the round 3 fix (`fix(agent): stdin >2 MiB hangs - revert to send().await for IoClient frames`, commit `61dc2be`). Round 4 quantified the trade-off.
- **Observed behavior:** with one session A doing a large stdin upload to a slow in-container consumer (e.g., `while read; sleep 0.1`), session B's frames are queued behind A's at the agent's tunnel inbound loop. Measured impact: 16 MiB stdin → slow consumer → session B `echo` exec took 811 seconds (~13.5 min) instead of <1 s. **Additionally, the WS-disconnect synthetic Close frame for session A is itself queued behind A's stdin frames**, so the EXEC_KILL_GRACE cleanup hook only fires after the entire upload drains — observed: io-20's cleanup signal_sent fired ~15 min after the WS closed. This leaves the in-container process running and grows zombie [sleep] children.
- **Root cause:** `crates/agent/src/proxy_client.rs:308` uses `t.send(io_frame).await` for IoClient routing. For Stdin frames this is correct (data integrity) but it backpressures the tunnel inbound pump, blocking all OTHER sessions on the same agent's tunnel. The blocker is `drive_io_session`'s stdin pump (`tx.send(Bytes::from(bytes)).await` in `crates/agent/src/io_stream.rs:472`) which blocks on the docker exec stdin pipe when the in-container consumer is slow.
- **Recommended next step (medium effort, ~150 LOC):** add a per-session "control" fast-path channel (try_send-able, never blocks) for Close/Signal frames separate from the bounded Stdin queue. Have `drive_io_session` tokio::select! across both so a queued Close can interrupt a wedged stdin pump. Cleanup hook then fires promptly on disconnect even when stdin is HoL'd. Inbound loop demux routes by frame variant: Stdin → bounded await, Close/Signal → fast-path try_send. Architecture is similar to what comp-2 B2 did on the proxy server-frame side, but with type-discriminated routing.
- **Production impact:** medium. Two concurrent stdin-heavy sessions on the same agent will see HoL. Single-session deployments are unaffected. Disconnect-cleanup delay is the more serious follow-on (orphan in-container processes accumulate until the inbound loop finally drains).

## [comp-1 · SPEC] Multi-tenancy

- **Status:** comp-1 F1 closed the immediate exposure with single-tenant admin auth. Per-tenant ownership / API-key-per-tenant / billing-attribution all wait on the SPEC call. The `controllerAdminToken` is the single key today.

---

## ✅ Closed since the original review pass

Recorded here so cross-references in commits / docs still resolve. See `REVIEW_LOG.md` for the per-component fix details.

- contracts/v1.0.2: amendment + downstream cascade
- comp-2 C5: in-binary ACME for OpenTunnel listener
- comp-2 B2 / comp-3 A3/B1: per-session try_send for HoL isolation
- comp-2 C2: send_or_spawn fallback for disconnect notifications
- comp-3 A4: JoinSet for per-session spawn-handle abort on tunnel disconnect
- comp-3 B6: tonic client HTTP/2 keepalive
- comp-3 C2: controller-side terminal-state exception for late SandboxStatus
- comp-3 C3: api alias retired via IoErrorCode enum
- comp-3 C5: dup stream_id defensive check
- comp-4 image pull retry policy (4-attempt exponential backoff)
- comp-4 signal_exec exec stream drain (detach: true)
- comp-5 docker-default caps + no_new_privileges + masked + readonly + pids 256
- comp-5 tar layer path-traversal guard
- comp-5 atomic image install
- comp-5 write_files_targz cwd allowlist
- comp-5 create_and_start CNI rollback
- comp-6 chunking, structured trailer, mutex poison, body limit, const-time auth, path validation
- comp-7 TLS feature, Ctrl-C → SIGINT, exit codes, read timeout knob, frame size cap
- comp-8 reqwest timeouts, /proc/meminfo memory, SIGTERM fallback, Redacted secret newtype, RUST_LOG warn + panic-hook
- comp-9 joinToken + operatorCidrs fail-closed, Cloudflare proxied wildcard, env passthrough for all auth tokens, api gateway systemd unit, Postgres password + md5 pg_hba, multi-arch binary download
