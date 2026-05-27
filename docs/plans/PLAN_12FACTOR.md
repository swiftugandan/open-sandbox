# Plan: 12-factor decomposition

**Status:** draft (2026-05-27). Pre-implementation; phases sized but not started.

**Goal:** make open-sandbox's runtime services (controller, proxy, api, agent) operable as 12-factor apps — with the explicit caveat that the proxy tier is, by design, a **connection-affinity tier** rather than a stateless one. This plan closes the gaps that *are* fixable and documents the gaps that *aren't gaps* but design decisions.

## Why

A 12-factor audit of the runtime services surfaced 12 items. Three of them (#6 stateless processes, #8 horizontal concurrency, #10 dev/prod parity for the proxy) cannot be "fixed" without misunderstanding the architecture: the proxy holds **live socket endpoints** (`TunnelPool.request_tx`, `IoSessions.server_tx`), not serializable state. Externalizing those to Redis is incoherent — TCP sockets don't serialize. The right move is to document the proxy as a connection-affinity tier and scale the agent fleet (which is the actual unit of parallelism in this system) horizontally. See Phase 0.

The other nine items range from "missing `.env.example`" to "no separate migrate subcommand." Those are real, small, and fixable.

## Architectural decision (D)

From the session that produced this plan:

> The proxy tier is correctly modeled as connection-affinity, not stateless. 12-factor #8 demands "the process model can scale where it needs to" — for this system, what needs to scale is the agent fleet (embarrassingly parallel; one agent per worker). The proxy and controller can be 1-of-N because that's where the work isn't.

**HA story (Phase 7 in earlier drafts) is deferred.** Single-instance proxy + single-instance controller with controlled-downtime failover is acceptable until a concrete uptime requirement appears. Reopen as `PLAN_HA.md` if/when that requirement lands.

## Scope (the 12 items)

| # | Factor | Status | Phase |
|---|---|---|---|
| 1 | Codebase | compliant | — |
| 2 | Dependencies | compliant | — |
| 3 | Config | needs docs only | Phase 1 |
| 4 | Backing services | needs reframe | Phase 6 |
| 5 | Build/release/run | one TODO to close | Phase 5 |
| 6 | Processes (stateless) | **by-design exception** | Phase 0 |
| 7 | Port binding | compliant | — |
| 8 | Concurrency | **by-design exception** | Phase 0 |
| 9 | Disposability | proxy drain missing | Phase 4 |
| 10 | Dev/prod parity | hardcoded localhost in cloud-init | Phase 2 |
| 11 | Logs | compliant | — |
| 12 | Admin processes | migrate bundled into startup | Phase 3 |

---

## Phases

### Phase 0 — Lock in the architectural decision (½ day)

**Highest-leverage deliverable.** Without it, every future review surfaces "proxy isn't horizontally scalable" as a bug and someone tries to refactor it.

- **New `docs/design/SCALING_TIERS.md`** — ADR-style. Covers:
  - Proxy = connection-affinity tier. The `TunnelPool` and `IoSessions` maps hold **live socket endpoints** (`tunnel_pool.rs:21`, `io_sessions.rs:38`), not serializable state. Cannot be externalized; should not be tried.
  - Controller = single coordinator. Postgres is the source of truth; the controller process itself is a thin gRPC + LISTEN/NOTIFY broker. Can be hot-standby'd later (out of scope; see `PLAN_HA.md` when prioritized).
  - Agent fleet = the horizontal scaling unit. Each agent is one worker; the controller's scheduler distributes load across them.
  - Why 12-factor #6 and #8 are interpreted as "scale where the work is" rather than "every tier is stateless."
- **One-paragraph addendum to `SAD.md`** linking to `SCALING_TIERS.md`.
- **Update `CLAUDE.md` "Notes for future sessions"** so the constraint is loaded every session: "The proxy tier is connection-affinity by design — do not propose externalizing tunnel/session state to Redis."

### Phase 1 — Config surface (1 day) → closes #3

- **New `.env.example`** at repo root, organized by service. Every required and optional env var listed with one-line purpose. Includes:
  - Controller: `OPEN_SANDBOX_DATABASE_URL`, `CONTROLLER_ADMIN_TOKEN`, `OPEN_SANDBOX_INTERNAL_TOKEN`, `TUNNEL_JOIN_TOKEN`, `OPEN_SANDBOX_JOIN_TOKEN`, `OPEN_SANDBOX_CONTROLLER_GRPC_PORT`.
  - Proxy: same `DATABASE_URL`, `OPEN_SANDBOX_INTERNAL_TOKEN`, `TUNNEL_JOIN_TOKEN`, plus `OPEN_SANDBOX_PROXY_GRPC_PORT`, `OPEN_SANDBOX_PROXY_HTTP_PORT`, `ACME_CACHE_DIR`.
  - API: `OPEN_SANDBOX_API_KEY`, `OPEN_SANDBOX_API_CORS_ORIGINS`, `OPEN_SANDBOX_API_PORT`.
  - Agent: `OPEN_SANDBOX_JOIN_TOKEN`, `OPEN_SANDBOX_AGENT_ID`, proxy/controller URLs.
  - Observability: `RUST_LOG`.
- **README "Environment" section** pointing at `.env.example` and noting fail-closed behavior for the auth tokens.
- **No code changes.** Pure documentation of the env surface that already exists.

### Phase 2 — Inter-service URL env wiring (1 day) → closes #10

- **`infra/src/cloud-init.ts`** — replace hardcoded `http://127.0.0.1:5005x` defaults (around line 146) with env-templated values. Defaults stay `127.0.0.1` for single-host deployments; the variable surface is exposed for future multi-host deploys without requiring an infra-code change.
- **`infra/README.md`** — document the deploy-time variable surface.
- **No runtime code changes** — the binaries already read all inter-service URLs from env (`crates/cli/src/cli.rs:87,95,121`). Only the deploy template is updated.

### Phase 3 — Admin processes (2 days) → closes #12

- **New `open-sandbox migrate` subcommand** in `crates/cli/src/cli.rs`. Runs schema migrations only and exits. Takes the same `--database-url` / `OPEN_SANDBOX_DATABASE_URL` config as the services.
- **Implementation:** factor the existing migration logic out of `pg_store::migrate()` callers in `run_controller` and `run_proxy` into a shared call site. The `migrate` subcommand calls both controller and proxy migrations idempotently.
- **`--auto-migrate` flag (default off) on `controller` and `proxy` subcommands.** When set, the service runs migrations on startup as today; otherwise it assumes the schema is current. **Defaulting off in production** prevents migration failures from cascading into service-startup failures. **Defaulting on in dev** (set in `docker-compose.full.yml` and the `open-sandbox dev` subcommand) preserves the current low-friction dev loop.
- **Update `infra/src/cloud-init.ts`** to run `open-sandbox migrate` once during cloud-init before starting controller/proxy systemd units.
- **Deploy docs** updated to reflect the ordering: migrate → start services.

### Phase 4 — Disposability hardening (2 days) → closes #9

- **Proxy SIGTERM handler** drains in-flight IoSessions before shutting down `tonic::Server`:
  1. On SIGTERM, set a "draining" flag that causes new `OpenIoStream` and `OpenTunnel` calls to be rejected with `Unavailable`.
  2. Wait for `IoSessions::is_empty()` to be true OR for `SHUTDOWN_DRAIN_TIMEOUT` (default 30s, env-configurable) to elapse.
  3. If timeout elapsed: call `IoSessions::fail_stream` on each remaining session with a `Status::unavailable("proxy shutting down")` so gateways see a clean terminal frame instead of an abrupt disconnect.
  4. Then proceed with `tonic::Server::shutdown`.
- **Test:** spawn a long-running IO session, send SIGTERM, assert (a) the session completes if it finishes within the deadline, or (b) it gets a clean terminal frame past the deadline. No silent drops.
- **No changes to controller/api/agent** — controller already shuts down cleanly (`run.rs:543–567`), agent already stops running sandboxes on SIGTERM (`run.rs:471–487`).

### Phase 5 — Release artifact integrity (½ day) → closes the #5 TODO

- **`infra/src/cloud-init.ts:88`** — close the existing checksum TODO. SHA256 the downloaded `open-sandbox` binary; fail cloud-init if mismatch. Pulumi fetches the checksum file from the same GitHub release.
- **CI:** ensure release workflow publishes a `SHA256SUMS` file alongside the binaries.

### Phase 6 — Backing-services boundary (discussion only) → reframes #4

- **`agent-docker`'s Docker socket bind-mount is intrinsic** to the docker runtime. It cannot be removed without removing the runtime. **Document `agent-docker` as the dev/local-runtime path; `agent-youki` is the production runtime** (already true per `ENGINEERING_DISCIPLINE.md` and existing CLAUDE.md). The "12-factor violation" is real but scoped to a dev-only binary; that's an acceptable trade.
- **`ACME_CACHE_DIR`** is already env-configurable (cloud-init.ts:27). Just needs README mention. No code changes.
- **Update `docs/design/SCALING_TIERS.md`** to note the runtime-path distinction.

---

## Sequencing

```
Phase 0 (architectural ADR)  ─┐
                              ├─→ everything else can land in parallel
Phase 1 (.env.example)        │
Phase 2 (env wiring)          │
Phase 5 (release checksum)    │
Phase 6 (docs)                │
                              │
Phase 3 (migrate subcommand) ─┤
                              │
Phase 4 (drain)              ─┘  (independent; some affinity with Phase 3 since both touch CLI structure)
```

Phase 0 is a prerequisite for everything else — it's the doc that prevents the plan from being re-litigated next session. Phases 1, 2, 5, 6 are sub-day surgical changes plus docs. Phases 3 and 4 are real but scoped.

**Total effort:** ~7 days end-to-end if done serially; ~3 days wall-clock with parallel landing.

---

## Files that will change

| Path | Change | Phase |
|---|---|---|
| `docs/design/SCALING_TIERS.md` | **New.** ADR documenting D. | 0 |
| `SAD.md` | Addendum linking to SCALING_TIERS.md. | 0 |
| `CLAUDE.md` | Add proxy-affinity note to "Notes for future sessions." | 0 |
| `.env.example` | **New.** All env vars, organized by service. | 1 |
| `README.md` | New "Environment" section. | 1 |
| `infra/src/cloud-init.ts` | Replace hardcoded `127.0.0.1` defaults with env templates; close checksum TODO. | 2, 5 |
| `infra/README.md` | Document deploy-time variable surface. | 2 |
| `crates/cli/src/cli.rs` | Add `Command::Migrate(MigrateArgs)`; add `--auto-migrate` to controller/proxy. | 3 |
| `crates/cli/src/run.rs` | Factor migration logic; add `run_migrate`; gate auto-migrate. | 3 |
| `crates/cli/src/main.rs` | Wire `Command::Migrate`. | 3 |
| `crates/proxy/src/lib.rs` (or new `shutdown.rs`) | Drain logic for `IoSessions`. | 4 |
| `crates/proxy/src/grpc.rs` | "Draining" flag to reject new OpenIoStream/OpenTunnel. | 4 |
| `crates/proxy/tests/` | New integration test for drain behavior. | 4 |
| `infra/src/cloud-init.ts` | Run `open-sandbox migrate` before service startup. | 3 |
| `docker-compose.full.yml` (and `docker-compose.dev.yml`) | Set `--auto-migrate` on controller/proxy for dev. | 3 |
| `.github/workflows/release.yml` (or equivalent) | Publish `SHA256SUMS` alongside binaries. | 5 |

---

## What's not in scope

- **Phase 7 (HA / hot-standby).** Deferred. Reopen as `PLAN_HA.md` when there's a concrete uptime requirement.
- **Externalizing proxy session state to Redis/etcd.** Architecturally rejected; see Phase 0.
- **Multi-proxy horizontal scaling.** Same — rejected by D. If it becomes necessary, it's a separate plan that starts with adding `proxy_id` to the controller's `agents` table.
- **Replacing `agent-docker`'s socket bind-mount.** Out of scope; documented as a dev-runtime trade-off.
- **OpenTelemetry / distributed tracing.** Tracked separately under v1.0.1 P4 followups; not part of 12-factor.

---

## Open questions

All resolved during the 2026-05-27 implementation pass with the
recommended (least-friction) defaults:

1. **Phase 4 drain semantics — should `OpenTunnel` also be rejected during drain?**
   **Resolved: yes, reject.** Implemented in `crates/proxy/src/grpc.rs:open_tunnel`. Agents
   reconnect via existing exponential backoff; no bounded-window
   wait needed.
2. **Phase 3 — `migrate` idempotency.** **Resolved: idempotent.**
   `run_migrate` calls the same `pg_store::migrate()` paths the
   long-running services have always used; every statement is
   `CREATE TABLE/INDEX IF NOT EXISTS`. Re-running is a no-op.
3. **Phase 5 — GPG vs SHA256.** **Resolved: SHA256 only.**
   `cloud-init.ts` fetches `SHA256SUMS` from the release; absent
   warns-and-continues (preserves existing deploys), present and
   mismatched fails-closed. Cosign/sigstore deferred.

## Implementation status (2026-05-27)

Phases 0–6 landed in one pass. Phase 7 (HA) intentionally deferred —
see `What's not in scope` above. Operator-visible changes summarized
in `CHANGELOG.md` under the new v1.0.2 "12-factor decomposition"
section, including the two security fixes (`INTERNAL_TOKEN`
env-name regression and missing `CONTROLLER_ADMIN_TOKEN` on the
api gateway) discovered during the Phase 1 audit.
