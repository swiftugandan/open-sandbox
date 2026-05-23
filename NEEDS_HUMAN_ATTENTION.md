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

## [comp-2 · live-validation] PG-side end-to-end LISTEN/NOTIFY needs a real Postgres

- **Blocker class:** `live-validation`
- **What I shipped:** controller (F4) emits `pg_notify('routing_changed', json)` inside each routing-table mutation transaction; proxy spawns a `PgListener` and parses notifications into `cache.insert` / `cache.remove_by_sandbox_id` calls. Schema parser has unit tests; the listener task itself has no unit test.
- **What you need to do:** run `crates/controller/tests/live_e2e.rs` (or a new proxy-side live test) against a real Postgres. Verify (1) deletion → notify → cache evict within a single round-trip; (2) inserts visible to the proxy without waiting for the 30s periodic refresh; (3) listener reconnects cleanly when the PG connection drops.

## [comp-2 · decision] TLS on the OpenTunnel public listener (C5)

- **Blocker class:** `decision`
- **Source:** comp-2 Angle C
- **File:** `crates/cli/src/run.rs:~290` (public gRPC server bind)
- **Summary:** the proxy's public listener — where agents on hostile networks dial in — is plaintext (`tonic::transport::Server::builder()` without `.tls_config()`). Per CLAUDE.md the foundational decision is "agents dial out over TLS", but the binary serves agents over h2c. On-path attackers can read/modify every tunneled HTTP body, exec stdin (pasted secrets), and stdout.
- **Recommended next step:** decide on the cert source. Options: (a) operator-provided cert/key paths via env (`PROXY_TLS_CERT_PATH`, `PROXY_TLS_KEY_PATH`); (b) Cloudflare-origin-pull / cloud-LB termination + a config flag asserting "TLS terminates upstream of me"; (c) Let's Encrypt with `rustls-acme` (Pulumi infra needs to expose port 443 + DNS-01). Tell me which path, and I'll wire it up.

## [comp-2 · decision] Intra-tunnel head-of-line blocking (B2)

- **Blocker class:** `decision`
- **Source:** comp-2 Angle B
- **File:** `crates/proxy/src/grpc.rs:~158` (OpenTunnel inbound dispatch), `crates/proxy/src/io_sessions.rs:~80` (deliver_server_frame awaits per-session send)
- **Summary:** the agent's OpenTunnel inbound loop dispatches `IoServerFrame`s by `await`-ing `sessions.deliver_server_frame(...)`. One slow gateway-side session backpressures the whole tunnel: every other exec / file op on that agent stalls until the slow session drains. This is the documented backpressure-chain design today but produces unfair multi-tenancy when sessions share a tunnel.
- **Recommended next step:** decide the desired multiplexing model. Cleanest fix is a per-session pump that owns the gateway-side `server_tx` and consumes from a per-session bounded `mpsc`; the tunnel-side dispatcher then uses `try_send` (drop oldest with a warn) on the per-session queue rather than `send().await`. This is ~80-150 LOC and preserves end-to-end backpressure within a session while isolating slow consumers. I can implement either approach once you decide.

## [comp-2 · decision] try_send silently drops disconnect notifications (C2)

- **Blocker class:** `decision`
- **Source:** comp-2 Angle C
- **File:** `crates/proxy/src/io_sessions.rs:97` (`fail_stream`) and `:112` (`cancel_agent_streams_at_generation`)
- **Summary:** when the gateway-side `server_tx` channel is full (32 frames), the agent-disconnect error sent via `try_send` is silently dropped. The session record is then removed, `server_tx` drops, and the gateway observes a clean stream EOF rather than a terminal `Unavailable` — violating the spike-03 conclusion that agent disconnect MUST surface a clean error to the WS client.
- **Recommended next step:** either (a) make these methods async and use `send().await` (small refactor, propagates upward to the OpenTunnel cleanup task); (b) keep try_send but spawn a fallback `tokio::spawn(async move { let _ = tx.send(Err(...)).await; })` so the error eventually lands; (c) bump the per-session channel size enough that the failure mode is implausible. Tell me which.

---

## [comp-3 · decision] Intra-tunnel head-of-line blocking on the agent (A3/B1)

- **Blocker class:** `decision`
- **Source:** comp-3 Angles A + B
- **File:** `crates/agent/src/proxy_client.rs:97` (the OpenTunnel inbound loop)
- **Summary:** symmetric to comp-2 B2 on the proxy side. The agent's inbound `inbound.message().await` loop awaits per-session `in_tx.send(...).await` and the outbound HTTP forward inline. One slow consumer (slow proxy WS drain, slow in-container HTTP server) head-of-line blocks every other session and HTTP request multiplexed onto the same agent.
- **Recommended next step:** decide alongside comp-2 B2 (same multiplexing model on both sides). Per-session pumps + `try_send`-on-overflow with a documented drop policy is the standard pattern; ~80-120 LOC on this side.

## [comp-3 · decision] Spawned io-session tasks leak on tunnel disconnect (A4)

- **Blocker class:** `decision`
- **Source:** comp-3 Angle A
- **File:** `crates/agent/src/proxy_client.rs:214` (drive_io_session spawn) and `:228` (outbound pump spawn)
- **Summary:** the per-session `drive_io_session` and outbound pump tasks are spawned detached. When `ProxyConnection::run` returns (now common, since A1 introduced reconnect loops), the local `io_sessions` HashMap drops, which eventually closes every per-session in_tx — but each `drive_io_session` then sits in `cleanup` for `EXEC_KILL_GRACE` (10s) before exiting. Under a reconnect storm this stacks up: 100 sessions × 10s × N reconnects = unbounded transient task accumulation.
- **Recommended next step:** track `JoinHandle<()>` for each spawned per-session task; on `ProxyConnection::run` return, abort them all. ~25 LOC. Defensive; once decided I can ship.

## [comp-3 · decision] stop_sandbox doesn't notify in-flight ExecRegistry sessions (B3)

- **Blocker class:** `decision`
- **Source:** comp-3 Angle B
- **File:** `crates/agent/src/sandbox.rs:85` (`stop_sandbox`) vs `crates/agent/src/exec_registry.rs:74` (`list_for_sandbox`)
- **Summary:** when a sandbox is stopped, every in-flight exec session for that sandbox is left to discover the container is gone via the runtime backend's exit detection — gateway-side clients may see "stream ended without terminal frame" instead of a clean `IoError(SANDBOX_GONE)`. The runtime backend (docker / youki) implementation determines whether the exec_session's exit channel fires cleanly; verifying that is part of comp-4 / comp-5 review.
- **Recommended next step:** add a `cancel_tx` to `ExecRecord` (or wire server_tx through) so `SandboxManager::stop_sandbox` can broadcast a terminal `IoError(SANDBOX_GONE)` to every session for the stopping sandbox before tearing down the container. ~50 LOC. Once comp-4 / comp-5 confirm the runtime-side cleanup is reliable, this may turn out to be belt-and-braces rather than necessary.

## [comp-3 · decision] Application-level keepalive on the agent's proxy tunnel (B6)

- **Blocker class:** `decision`
- **Source:** comp-3 Angle B
- **File:** `crates/agent/src/proxy_client.rs:51` (OpenTunnel dial)
- **Summary:** comp-2 B4 added HTTP/2 keepalive on the proxy's server side, which catches a frozen agent. The reverse direction (proxy frozen, agent still believes the tunnel is live) is uncaught — the agent has no application-level ping and the TCP keepalive interval is OS-dependent (often minutes). Idle execs continue buffering stdout into a dead channel until OS TCP timeout.
- **Recommended next step:** decide whether to (a) configure tonic client-side HTTP/2 keepalive on `Channel::from_shared(addr)` (set `.keep_alive_while_idle(true).keep_alive_timeout(20s).http2_keep_alive_interval(15s)`), which is the cheapest fix; or (b) add an application-level IoPing/IoPong to the proxy protocol (contract change). Tell me which.

## [comp-3 · cross-component] SandboxStatus(Stopped) never persisted because release_sandbox runs first (C2)

- **Blocker class:** `cross-component` (controller-side fix)
- **Source:** comp-3 Angle C
- **File:** controller-side at `crates/controller/src/management.rs:152-155` (release_sandbox call); agent-side at `crates/agent/src/controller_client.rs:144` (SandboxStatus emission timing)
- **Summary:** controller's `delete_sandbox` calls `release_sandbox` immediately after dispatching `StopSandbox`, deleting the routing_entries row before the agent's terminal `SandboxStatus(Stopped)` arrives. The F2 owner check then drops the late message. `sandbox_states` never advances past 'running' for clean deletions.
- **Recommended next step:** either (a) controller keeps the routing entry alive until SandboxStatus(Stopped) confirms (with a bounded timeout falling back to release-anyway); or (b) controller's F2 owner check makes a terminal-state exception that records the state even when no routing entry exists. This is comp-1 territory; logged here so it isn't lost when comp-1 closes.

## [comp-3 · cross-component] IoError code "SANDBOX_NOT_FOUND" not recognized by api (C3)

- **Blocker class:** `cross-component` (api-side mapping)
- **Source:** comp-3 Angle C
- **File:** `crates/agent/src/proxy_client.rs:190` (agent emit) and `crates/api/src/handlers.rs:385-396` (api `map_io_error`)
- **Summary:** the agent emits `IoError { code: "SANDBOX_NOT_FOUND" }` when a routing race hits before the agent's in-memory sandbox_manager has the entry; api's `map_io_error` only recognizes `SANDBOX_GONE` and collapses the rest to `IoStreamFailed`. SDKs that should retry on transient-not-found instead see opaque 500s.
- **Recommended next step:** comp-6 (api review) — add `SANDBOX_NOT_FOUND` as an alias for `SANDBOX_GONE` in `map_io_error` and `ws_read_file.rs:221`. Or change the agent emission to `SANDBOX_GONE` (one-line agent change). Comp-0's stringly-typed `IoError.code` finding already flagged this drift class; this is a concrete instance to wire when comp-6 lands.

## [comp-4 · decision] Image pull has no retry, no progress visibility, no in-flight dedup

- **Blocker class:** `decision`
- **Source:** comp-4 line-by-line
- **File:** `crates/agent-docker/src/lib.rs:62` (`try_collect::<Vec<_>>()` on the pull stream)
- **Summary:** transient registry hiccups abort the entire create_and_start. Multiple concurrent creates of the same fresh image stampede the registry instead of sharing one in-flight pull. Errors map to opaque runtime-error strings with no signal that this is retryable.
- **Recommended next step:** decide between (a) wrap the pull in a per-image bounded retry with exponential backoff, no in-flight dedup (~30 LOC); (b) add a process-wide dedup map keyed by image (~80 LOC) so concurrent pulls share one stream; (c) defer pull retries to the controller's StartSandbox retry policy (no change here). I lean toward (a) + map errors to a typed `ImagePullFailed` variant on `AgentError` if you allow a contract change.

## [comp-4 · decision] signal_exec leaks an undrained bollard exec stream

- **Blocker class:** `decision`
- **Source:** comp-4 line-by-line
- **File:** `crates/agent-docker/src/lib.rs:421` (signal_exec)
- **Summary:** the kill exec opens a `start_exec(detach: false, attach_stdout: true, attach_stderr: true)` and binds the `StartExecResults::Attached` to `_`. The output stream is dropped immediately; bollard's internal buffer is never drained, and the docker exec instance may not finalize cleanly. Repeated SIGTERM/SIGKILL per disconnect accumulates undrained streams.
- **Recommended next step:** either (a) use `detach: true` (cleanest — no attach, no buffer); (b) drain the output stream via a small `spawn` task. (a) is ~3 LOC. Confirm bollard's detach semantics match what we want and I'll ship.

## [comp-4 · decision] inspect_exec failure conflated with process exit -1

- **Blocker class:** `decision`
- **Source:** comp-4 line-by-line
- **File:** `crates/agent-docker/src/lib.rs:350`
- **Summary:** when the bollard output stream ends and inspect_exec fails, the output-pump emits `ExecExitInfo { exit_code: -1, command_not_found: false }` — indistinguishable from a process that legitimately exits -1 (impossible on Linux but allowed by the wire format). Worse, agent-core's natural-exit fast path then SKIPS the SIGTERM/SIGKILL cleanup, so a daemon restart leaves the in-container PID orphaned.
- **Recommended next step:** introduce a new `RUNTIME_ERROR` ExecInfo path (or emit `Err` on the `exited` channel) so io_stream's runtime-error branch fires the cleanup hook. ~15 LOC. Touches the ContainerRuntime trait shape — decide whether to extend it.

## [comp-5 · live-validation] OCI security defaults missing (caps, readonly_rootfs, seccomp, userns, pids limit)

- **Blocker class:** `decision` + `live-validation`
- **Source:** comp-5 spec.rs review
- **File:** `crates/agent-youki/src/spec.rs:83` (and the broader spec builder)
- **Summary:** generated OCI spec has NO capabilities drop, NO readonly_rootfs, NO seccomp profile, NO masked_paths / readonly_paths, NO user namespace, NO pids limit. Container runs as host-root with full caps and unfiltered /proc and /sys. This is the **single largest production-safety gap** in the codebase — the entire backend's threat model collapses without baseline hardening.
- **Recommended next step:** I cannot make these decisions alone — each touches what user code can do inside the sandbox. Concretely, you need to decide:
  1. **Capability set**: which caps to keep? Recommend dropping all except a minimal subset (CAP_AUDIT_WRITE for some glibc operations, CAP_CHOWN/DAC_OVERRIDE/SETUID/SETGID for in-container useradd, NET_BIND_SERVICE for binding < 1024). Match docker's default `cap_drop_default` or stricter.
  2. **User namespace**: enable it? Requires kernel ≥5.11 + subuid/subgid mapping setup. Big improvement to defense-in-depth.
  3. **readonly_rootfs**: yes/no? Many images expect writable / (e.g. apt cache, /tmp); requires explicit writable mount points (`/tmp`, `/var`, `/run`). Common docker default is read-write rootfs; consider read-only with tmpfs overlays.
  4. **seccomp**: ship a default profile? docker's default profile is ~330 lines of JSON. Could vendor a copy.
  5. **masked_paths / readonly_paths**: standard set is `/proc/asound,/proc/acpi,/proc/kcore,/proc/keys,/proc/latency_stats,/proc/timer_list,/proc/timer_stats,/proc/sched_debug,/proc/scsi,/sys/firmware`.
  6. **pids limit**: numeric cap; recommend ~256 default.

This needs an SPEC amendment and live testing on the Linux dev env. Block on your decisions.

## [comp-5 · live-validation] Tar layer extraction has path-traversal vulnerability

- **Blocker class:** `live-validation`
- **Source:** comp-5 image.rs review
- **File:** `crates/agent-youki/src/image.rs:117` (and `:104-112` whiteout)
- **Summary:** `entry.unpack(&dest)` with pre-joined `dest = rootfs.join(&path)` does NOT verify the resolved path stays within `rootfs`. (`Archive::unpack` does check; `entry.unpack` does not.) A malicious OCI image layer with `../../../etc/cron.d/evil` entries writes host files under the agent's privilege.
- **Recommended next step:** I can implement the fix (canonicalize each entry's path, reject any escape from `rootfs`, ~40 LOC). Need a Linux env to validate; no risk on macOS since the crate doesn't build there. Want me to ship the fix unvalidated?

## [comp-5 · decision] Image install is non-atomic — partial failures corrupt subsequent pulls

- **Blocker class:** `decision`
- **Source:** comp-5 image.rs review
- **File:** `crates/agent-youki/src/image.rs:42`
- **Summary:** layers extract into `rootfs_dir` directly; `.complete` marker only written after all succeed. Partial extracts leave dirty state that the next pull builds atop, silently corrupting the rootfs.
- **Recommended next step:** extract to a sibling `<rootfs>.tmp.<uuid>` directory, atomic rename on success, `rm -rf` the tmp on failure. ~30 LOC. Want me to ship?

## [comp-5 · decision] setns(2) thread doesn't enter PID namespace → /proc-based host access

- **Blocker class:** `decision`
- **Source:** comp-5 setns_ops.rs review
- **File:** `crates/agent-youki/src/setns_ops.rs:57`
- **Summary:** the thread enters the container's mount namespace (`CLONE_NEWNS`) but not its PID, user, or network namespaces. The container's `/proc` is the procfs mount the container saw, but viewed through the host PID namespace's eyes — paths like `/proc/1/root/etc/shadow` resolve to host paths. Combined with full host-root privilege inside the file-op thread, this is a primitive for reading arbitrary host files and memory.
- **Recommended next step:** add `setns(CLONE_NEWPID | CLONE_NEWUSER | CLONE_NEWNET)` before any file op. May require `unshare(CLONE_NEWPID)`-style fork dance since CLONE_NEWPID has fork semantics. ~50 LOC + a Linux test. Confirm and I'll ship.

## [comp-5 · decision] create_and_start has no rollback on partial failure

- **Blocker class:** `decision`
- **Source:** comp-5 lib.rs review
- **File:** `crates/agent-youki/src/lib.rs:154`
- **Summary:** if CNI ADD fails after libcontainer create, or container.start fails after CNI is up, no cleanup runs. Leaks accumulate: libcontainer state_dir, container_dir on disk, CNI ip allocations.
- **Recommended next step:** mirror the comp-4 docker rollback approach — track the partial state and run a best-effort cleanup chain on any error. ~60 LOC. Want me to ship?

## [comp-5 · live-validation] Image layer unbounded buffering OOMs on large pulls

- **Blocker class:** `live-validation`
- **Source:** comp-5 image.rs review
- **File:** `crates/agent-youki/src/image.rs:61`
- **Summary:** each layer is fully buffered into `Vec<u8>` before extraction. A 5 GiB layer = 5 GiB host RAM spike. Same shape as comp-4 image-pull issue but at higher impact (youki is the production runtime).
- **Recommended next step:** stream through the gunzip+tar decoder rather than buffering. ~40 LOC. Want me to ship?

## [comp-5 · decision] write_files_targz target_dir is client-controlled with no allowlist

- **Blocker class:** `decision`
- **Source:** comp-5 setns_ops.rs review
- **File:** `crates/agent-youki/src/setns_ops.rs:197`
- **Summary:** `target_dir` comes from client's `cwd` with no normalization. Client can write tarball contents to `/etc`, `/usr/bin`, etc. inside the container. Sandbox-internal but enables in-container privilege manipulation primitives (esp. without the OCI hardening from the first comp-5 entry).
- **Recommended next step:** decide whether (a) this is by-design ("inside the container is the sandbox; trust it"); (b) restrict cwd to a sandbox-prefix like `/workspace/...` and reject anything outside; (c) leave it but document. Comp-5's OCI hardening dominates the risk model here — if you ship the cap-drop + readonly_rootfs + tmpfs overlays, this finding is moot.

## [comp-6 · decision] Wire-level structured error codes vs per-method NotFound mapping

- **Blocker class:** `decision`
- **Source:** comp-6 cross-file (comp-0 follow-up)
- **File:** `crates/api/src/grpc_service.rs:120`
- **Summary:** `grpc_to_api` collapses every controller-emitted `tonic::Code::NotFound` to `SandboxNotFound`. Controller's ControllerError already has `AgentNotFound` and other potential NotFound variants. Need a structured wire signal so the api maps correctly.
- **Recommended next step:** controller emits `x-os-error-code` trailer with the variant name (e.g. "AGENT_NOT_FOUND"); api reads the trailer first and falls back to current behavior. Touches both controller (~30 LOC) and api (~30 LOC), but no contract change. Want me to ship?

## [comp-6 · decision] Stdin frame chunking for write_file uploads

- **Blocker class:** `decision`
- **Source:** comp-6 line-by-line
- **File:** `crates/api/src/handlers.rs:296`
- **Summary:** write_file sends the entire upload as one `IoClientFrame::Stdin(bytes)` proto message. tonic's default 4 MiB codec cap fails uploads larger than that with a cryptic ResourceExhausted. Same applies to write_files (gzip tarballs >4 MiB).
- **Recommended next step:** chunk the Stdin pushes at 64 KiB (matching the read-side chunking). ~25 LOC. Could also raise codec limits on both sides as a less-clean workaround. Tell me which.

## [comp-7 · decision] ws-client read-timeout / stale-connection detection

- **Blocker class:** `decision`
- **Source:** comp-7 review
- **File:** `crates/ws-client/src/lib.rs:208` (`ExecSession::next_frame`)
- **Summary:** the client never times out a `next_frame().await`. Server-initiated pings keep the connection alive when the server is healthy, but a middle-box silent drop (NAT/LB idle timeout with no FIN/RST) parks the client indefinitely.
- **Recommended next step:** decide between (a) client sends its own keepalive every 15s + tracks last-server-frame timestamp, declares dead after 60s; (b) configurable per-session `read_timeout` and let the caller drive a watchdog. Tell me which and I'll ship.

## [comp-7 · decision] ws-client frame size limits

- **Blocker class:** `decision`
- **Source:** comp-7 review
- **File:** `crates/ws-client/src/lib.rs:141` (`connect_async`)
- **Summary:** tokio-tungstenite defaults to max_message_size=64 MiB, max_frame_size=16 MiB; the api side has no coordinated cap. A single stdout chunk >16 MiB silently closes the client.
- **Recommended next step:** decide the contracted per-frame upper bound (recommend 1 MiB matching typical WS gateway defaults), document in the contracts crate, and set the same on both sides. I can ship once you decide.

## [comp-8 · decision] Secrets leak via Debug derive

- **Blocker class:** `decision`
- **Source:** comp-8 review
- **File:** `crates/cli/src/cli.rs` (AgentArgs/ApiArgs/ControllerArgs/ProxyArgs)
- **Summary:** API key, join token, and the postgres `DATABASE_URL` (which contains password in userinfo) are stored as plain `String` with `#[derive(Debug)]`. Any future `tracing::error!(?args, ...)` during incident triage prints them in plaintext.
- **Recommended next step:** wrap each secret in a newtype with a redacted Debug impl (or pull in the `secrecy` crate). 3 fields, ~30 LOC. Want me to ship?

## [comp-8 · decision] RUST_LOG malformed → silent info fallback + no panic-on-task-panic

- **Blocker class:** `decision`
- **Source:** comp-8 review
- **File:** `crates/cli/src/main.rs:11`
- **Summary:** EnvFilter parse errors fall back to plain "info" silently; operator sees no signal that their RUST_LOG had a typo. Plus no `std::panic::set_hook` to abort on spawned-task panics, so a panic in the routing-cache refresh loop or LISTEN subscriber gets caught by tokio and the proxy keeps serving with broken state.
- **Recommended next step:** eprintln on EnvFilter parse error before falling back, and install set_hook that calls process::abort. Tell me if you want me to ship this together with the secret-redaction.

## [comp-9 · decision] Production deployment story (TLS, secrets, backups, observability)

- **Blocker class:** `decision`
- **Source:** comp-9 infra/Pulumi review
- **Files:** `infra/src/cloud-init.ts`, `infra/src/constants.ts`, `infra/Pulumi.dev.yaml`, `infra/index.ts`, `infra/src/dns.ts`
- **Summary:** comp-9 surfaced 8 distinct production-readiness gaps. Each needs your call before infra is safe to deploy:
  1. **TLS termination** (comp-2 C5 cross-ref): proxy binds plaintext :443 with no cert source. Decide between (a) Cloudflare proxied + edge TLS; (b) operator-provided cert files via env; (c) Let's Encrypt in-binary via `rustls-acme`.
  2. **Postgres has no password**: trust-on-127.0.0.1 means any process on the controller host becomes superuser. Add `ALTER USER postgres PASSWORD ...` to cloud-init.
  3. **operatorCidrs default `0.0.0.0/0`**: SSH wide open by default. Change `Pulumi.dev.yaml` and the index.ts fallback to require explicit CIDR.
  4. **Cloud-init missing env passthrough**: `CONTROLLER_ADMIN_TOKEN`, `INTERNAL_TOKEN`, `TUNNEL_JOIN_TOKEN` aren't wired anywhere in cloud-init systemd units. The auth tokens added in comp-1/2 have no delivery path.
  5. **joinToken default `"changeme"`**: change to `config.requireSecret("joinToken")` so deploys fail closed when unset.
  6. **pg_dump backups on same volume**: a volume-loss event wipes both. Add off-host upload (S3, etc) and retention policy.
  7. **Binary download is amd64-only on ARM controllers, no checksum**: cloud-init's `curl ...linux-amd64` fails on cax11 default. Pin version + verify sha256.
  8. **Cloudflare `proxied: false` on wildcard**: bypasses Cloudflare DDoS protection AND blocks the free Universal SSL wildcard path. Flip to `true` and the TLS story (#1) becomes "Cloudflare handles it."
- **Recommended next step:** at minimum answer #1 (TLS) since it unblocks comp-2 C5 and is the foundational decision. The rest are operational; I can implement once you decide the topology.

- **Blocker class:** `decision`
- **Source:** comp-3 Angle C
- **File:** `crates/agent/src/proxy_client.rs:201`
- **Summary:** `io_sessions.insert(stream_id, in_tx)` overwrites silently if a second Start arrives for an already-active stream_id. The original `drive_io_session` is orphaned (no Close frame can reach it through the now-overwritten in_tx). Defensive against a malformed or compromised proxy; current proxy uses sequential `io-N` ids and wouldn't repeat absent a process restart.
- **Recommended next step:** on duplicate Start, emit `IoError(STREAM_ID_REUSED)` on the new stream and drop the new request. ~10 LOC. Defensive only; not blocking.

