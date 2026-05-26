# Project: open-sandbox

@import ./ENGINEERING_DISCIPLINE.md

## Overview

A Rust-based sandbox platform where agents dial out to a controller/proxy over TLS, enabling BYO workers from any network topology. The platform provides isolated OCI container sandbox environments with public HTTPS access via wildcard subdomains, backed by a cloud-portable Pulumi infrastructure layer.

## Current phase

**Shipped on `main`:** `contracts/v1.0.2` item #13 (`PullPolicy` + warm-startup optimization arc, commits `9341a62..bd414f6`, 2026-05-26). Built on top of `contracts/v1.0.1` (WS `/files/read-stream`, two-listener proxy split, youki `setns(2)` file ops) and the v1.0 streaming-exec amendment. Exec is a bidirectional stream-shaped session on the proxy's data plane (`SandboxIoService.OpenIoStream`), exposed publicly as WebSocket; file ops share that data plane. The agent-dials-out architecture, BYO-workers, and youki daemonless runtime (ADR-009) are unchanged.

**Tag status:** `contracts/v1.0.2` tag points at the initial v1.0.2 commit (`0e68177`, predates #13). Tag movement deferred until items #1–#12 ship — see `docs/plans/PLAN_CONTRACTS_v1.0.2.md`.

**Pending work, by category:**

- **v1.0.2 amendments #1–#12** (`docs/plans/PLAN_CONTRACTS_v1.0.2.md`): `IoErrorCode` enum, structured error trailer, `Port`/`Signum`/`SUBDOMAIN_LEN` newtypes, etc. Separate completion session.
- **v1.0.1 P4 visibility-only items** (`docs/reviews/FOLLOWUPS_v1.0.1.md`): Prometheus metrics, missing tracing event names, `run-all-youki.sh`, Rust rewrite of scenario 02. None block shipping.
- **Open questions from the review pass** (`docs/reviews/NEEDS_HUMAN_ATTENTION.md`): OCI hardening (seccomp / userns / readonly_rootfs), setns(2) PID/USER/NET namespaces, HoL multiplex policy, pg_dump off-volume backup, multi-tenancy SPEC.

## Building and testing

Runtime selection is via compile-time Cargo features (`docker` default; `youki` for Linux production via `crates/agent-youki`). The youki crate builds/tests on Linux only — `crates/agent-youki/` ships two compose flows:

- **`Dockerfile.dev` + `docker-compose.dev.yml`** — daily iteration on any host (including macOS). Long-lived dev container with bind-mounted source and named volumes for `target/` + cargo registry. Incremental compile is seconds-fast. Run `docker compose -f crates/agent-youki/docker-compose.dev.yml up -d` once, then `... exec dev cargo test -p open-sandbox-agent-youki -- --nocapture`. Warm-path benchmark: `... exec dev cargo run --release --example bench_create_and_start -p open-sandbox-agent-youki` (module doc has methodology + how to compare against docker-runtime numbers).
- **`Dockerfile.test` + `docker-compose.test.yml`** — CI / self-contained reproducer. Image bakes the test build (`cargo test --no-run`); recompiles from scratch on every source change.

Both handle cgroup v2 setup for nested Docker Desktop / Linux VMs; no manual prep. The repo-root `.dockerignore` keeps BuildKit from shipping host-side `target/` into either image.

## Quick status

Run these to get a snapshot:

```sh
# Phase artifacts that have been tagged
git tag --list 'spec/*' 'sad/*' 'contracts/*' 'plan/*'

# Modules and where they are in their TDD cycle
git tag --list 'module/*'

# What is live-verified
git tag --list 'module/*/live-verified'
```

## Key documents

**Canonical (root):**

- `ENGINEERING_DISCIPLINE.md` — how engineering work is done here (imported above; loaded in every session)
- `VISION.md` — one-paragraph problem statement and definition of done
- `SPEC.md` — functional and non-functional requirements with citations
- `SAD.md` — software architecture document (30k-ft → 10k-ft → per-component zoom)
- `CONTRACTS.md` — prose documentation of the contracts crate
- `crates/contracts/` — the contracts crate itself (source of truth)
- `CHANGELOG.md` — public-facing API + behavior changes, organized by contracts version

**Plans (`docs/plans/`):**

- `PLAN.md` — original binary decomposition + dependency DAG. **Historical reference** (every binary shipped); structural map still useful.
- `PLAN_EXEC_STREAMING.md` — v1.0 streaming-exec implementation plan. **Historical** (shipped, tagged `plan/v0.6.3`). Preserved for archeology; do not act on its instructions as pending work.
- `PLAN_CONTRACTS_v1.0.2.md` — amendment plan for contracts v1.0.2. Item #13 shipped; #1–#12 pending separate session.
- `CODE_REVIEW_PLAN.md` — process doc for the component-by-component `/code-review` pass. **Pass complete** (components 0–9, see progress table); kept for the per-component checklist and mechanics.

**Design (`docs/design/`):**

- `EXEC_STREAMING_DESIGN.md` — **architectural-decision record** for the shipped v1.0 streaming exec refactor. The "why" behind the data-plane choice, the connection-as-lifetime model, and the five spike conclusions. Still the canonical reference for anyone touching exec timeouts, sessions, file ops, process control, computer-use APIs, or VNC-from-browser.

**Reviews (`docs/reviews/`):**

- `REVIEW_LOG.md` — cross-session findings log from the component-by-component review pass (anchored at v1.0.1 when run). Deferred contract changes cascade into `PLAN_CONTRACTS_v1.0.2.md`.
- `FOLLOWUPS_v1.0.1.md` — closure log for v1.0.1 P1/P2/P3 plus deferred P4 visibility-only items.
- `NEEDS_HUMAN_ATTENTION.md` — open questions surfaced during the review pass that need a decision, a contract change, or live-environment validation. Has a "Closed since the original review pass" section at the bottom.

**Spike results:** `spikes/exec-streaming/spike-0{1..5}-*/RESULT.md` — five confirmed spike outcomes the v1.0 design + implementation rely on:

  - 01: docker exec does NOT propagate disconnect → agent must kill explicitly
  - 02: nsenter does NOT propagate SIGTERM → agent must kill explicitly
  - 03: axum WebSocket backpressures + detects abrupt disconnect in ~7ms (idle needs 30s ping)
  - 04: bollard exec pipeline backpressures end-to-end (~10 MiB chain buffer)
  - 05: youki PID-capture race is sub-millisecond (p99 = 484 μs, max = 12 ms; plan's 5×10ms polling has 4× margin)

## Notes for future sessions

- Primary design document was provided at project inception with full architecture, cost analysis, and scaling path
- Agent-dials-out architecture is the foundational decision — all other choices flow from it
- Hetzner is the default cloud for cost optimization; AWS validates the cloud abstraction
- Cloudflare for DNS regardless of compute cloud
- Multi-tenancy decision and raw TCP exposure are open questions from the design doc
- **Exec is a stream-shaped session on the proxy's data plane, not a message exchange.** Shipped in v1.0 — see `EXEC_STREAMING_DESIGN.md` for the rationale. Do not propose band-aids that re-introduce message-shaped exec or route exec/file ops via the controller; the architecture explicitly rejects both. The data-plane choice positions transparent-WebSocket-forwarding (VNC-from-browser, inbound WS apps) and computer-use agent APIs to fall out naturally — those remain v1.1+ work.
