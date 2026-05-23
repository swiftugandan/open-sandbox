# Project: open-sandbox

@import ./ENGINEERING_DISCIPLINE.md

## Overview

A Rust-based sandbox platform where agents dial out to a controller/proxy over TLS, enabling BYO workers from any network topology. The platform provides isolated OCI container sandbox environments with public HTTPS access via wildcard subdomains, backed by a cloud-portable Pulumi infrastructure layer.

## Current phase

**Shipped: `contracts/v1.0.1` on `main`** — the v1.0 streaming-exec amendment (sub-modules 12.1–12.7) plus the three v1.0.1 follow-ups (WS `/files/read-stream`, two-listener proxy split, youki `setns(2)` file ops). Exec is a bidirectional stream-shaped session on the proxy's data plane (`SandboxIoService.OpenIoStream`), exposed publicly as WebSocket. File ops share that same data plane. The agent-dials-out architecture, BYO-workers, and youki daemonless runtime (ADR-009) are unchanged.

Runtime selection is via compile-time Cargo features (`docker` default; `youki` for Linux production via `crates/agent-youki`). Build constraint: full agent-youki build/test on Linux only — two flows live in `crates/agent-youki/`:

- **Daily iteration** (any host, including macOS): `Dockerfile.dev` + `docker-compose.dev.yml`. Long-lived dev container with bind-mounted source and named volumes for `target/` + cargo registry. `docker compose -f crates/agent-youki/docker-compose.dev.yml up -d` once, then `docker compose -f ... exec dev cargo test -p open-sandbox-agent-youki -- --nocapture`. Cargo's incremental compile makes typical edits ~seconds.
- **CI / self-contained reproducer**: `Dockerfile.test` + `docker-compose.test.yml`. Image bakes the test build via `cargo test --no-run`; reproducible but recompiles from scratch on every source change.

Both entrypoints handle cgroup v2 setup automatically for nested environments (Docker Desktop, Linux VMs); no manual prep needed. A repo-root `.dockerignore` keeps the BuildKit context from shipping host-side `target/` into either image.

**Outstanding v1.0.1 follow-ups** (deferred, visibility-only): see `FOLLOWUPS_v1.0.1.md` P4. Includes Prometheus metrics, four missing tracing event names, `run-all-youki.sh`, and a Rust rewrite of scenario 02. None of these block shipping.

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

- `ENGINEERING_DISCIPLINE.md` — how engineering work is done here (imported above; loaded in every session)
- `VISION.md` — one-paragraph problem statement and definition of done
- `SPEC.md` — functional and non-functional requirements with citations
- `SAD.md` — software architecture document (30k-ft → 10k-ft → per-component zoom)
- `CONTRACTS.md` — prose documentation of the contracts crate
- `crates/contracts/` — the contracts crate itself (source of truth)
- `PLAN.md` — decomposition into binaries with dependency DAG and acceptance criteria
- `EXEC_STREAMING_DESIGN.md` — **architectural-decision record** for the shipped v1.0 streaming exec refactor. The "why" behind the data-plane choice, the connection-as-lifetime model, and the five spike conclusions. Still the canonical reference for anyone touching exec timeouts, sessions, file ops, process control, computer-use APIs, or VNC-from-browser.
- `PLAN_EXEC_STREAMING.md` — **historical implementation plan** for v1.0 (shipped). Tagged `plan/v0.6.3`. Preserved for archeology; do not act on its instructions as if they were pending work.
- `FOLLOWUPS_v1.0.1.md` — closure log for P1/P2/P3 plus the deferred P4 visibility-only items.
- `CODE_REVIEW_PLAN.md` — cross-session plan for running `/code-review` over the system one component at a time, with `proto/*.proto` + `crates/contracts` held frozen as the anchor. Consult and update the progress table when running component reviews.
- `CHANGELOG.md` — public-facing API + behavior changes, organized by contracts version.
- `spikes/exec-streaming/spike-0{1..5}-*/RESULT.md` — five confirmed spike outcomes the v1.0 design + implementation rely on:
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
