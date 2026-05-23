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

## [comp-3 · decision] Duplicate stream_id on IoStart silently overwrites existing session (C5)

- **Blocker class:** `decision`
- **Source:** comp-3 Angle C
- **File:** `crates/agent/src/proxy_client.rs:201`
- **Summary:** `io_sessions.insert(stream_id, in_tx)` overwrites silently if a second Start arrives for an already-active stream_id. The original `drive_io_session` is orphaned (no Close frame can reach it through the now-overwritten in_tx). Defensive against a malformed or compromised proxy; current proxy uses sequential `io-N` ids and wouldn't repeat absent a process restart.
- **Recommended next step:** on duplicate Start, emit `IoError(STREAM_ID_REUSED)` on the new stream and drop the new request. ~10 LOC. Defensive only; not blocking.

