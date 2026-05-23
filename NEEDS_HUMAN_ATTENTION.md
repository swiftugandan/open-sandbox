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
