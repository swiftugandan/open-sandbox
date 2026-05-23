# Project: open-sandbox

@import ./ENGINEERING_DISCIPLINE.md

## Overview

A Rust-based sandbox platform where agents dial out to a controller/proxy over TLS, enabling BYO workers from any network topology. The platform provides isolated OCI container sandbox environments with public HTTPS access via wildcard subdomains, backed by a cloud-portable Pulumi infrastructure layer.

## Current phase

Phase 6 complete: all modules implemented including `agent-youki`. Controller, agent, proxy, CLI shell, infra, agent-docker, proxy-http, api, api-files, and agent-youki modules done. The `agent-youki` module (`module/agent-youki/done`) replaces Docker Engine with youki/libcontainer as a daemonless OCI container runtime (ADR-009). Image pull via oci-client, container lifecycle via libcontainer, networking via CNI bridge+portmap, exec via nsenter. `DockerRuntime` extracted to its own crate (`crates/agent-docker/`); `YoukiRuntime` in `crates/agent-youki/`. Runtime selection via compile-time Cargo features (`docker` default, `youki` for Linux production). Build constraint: full build/test on Linux only (`Dockerfile.test` + `docker-compose.test.yml`).

**Contracts currently at `contracts/v0.7.0-frozen`** (SDK agent ergonomics — list sandboxes, exec stdin/cwd, single-file write_file, GET read_file, base64 stdout, COMMAND_NOT_FOUND, structured exec logs). Implementation merged on `contracts/amendment-sdk-agent-friction`. `AgentError::Docker` is now `AgentError::Runtime`; `ApiError::FileNotFound.path` is now `.resolved_path`.

**Pending major amendment: exec streaming → v1.0.0.** A full pre-amendment design lives at `EXEC_STREAMING_DESIGN.md` (settled decisions, spike-confirmed assumptions, forward trajectory). The executable plan with seven sub-modules, exact file lists, type signatures, observability requirements, and acceptance criteria lives at `PLAN_EXEC_STREAMING.md` (current tag: `plan/v0.6.3`). All five pre-implementation spikes have run clean (results under `spikes/exec-streaming/`). Reshapes exec from a message exchange to a stream-shaped session over WebSocket, riding the data plane (proxy) instead of the control plane (controller). Closes friction items H1–H4, M1, M2, M4, M5 from the post-v0.7 friction report. Implementation will live on `contracts/amendment-exec-streaming`. Before working on any "exec timeout / streaming / process control / file ops" item — read the design first, then the plan.

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
- `EXEC_STREAMING_DESIGN.md` — **pending v1.0 refactor** design doc. Source of truth for *what* the exec streaming amendment is and *why* the data-plane choice was made. Read before any work on exec timeouts, streaming, process control, sessions, file ops, computer-use, or VNC-from-browser.
- `PLAN_EXEC_STREAMING.md` — **executable plan** for the v1.0 amendment. Seven sub-modules (12.1 – 12.7) with branches, exact file lists, type signatures, TDD cycle expectations, acceptance criteria, observability requirements, smoke tests, risks, and effort estimates. Current tag: `plan/v0.6.3`.
- `spikes/exec-streaming/spike-0{1..5}-*/RESULT.md` — five confirmed spike outcomes the design + plan rely on:
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
- **The exec-as-message vs. exec-as-stream architectural decision has been made and documented in `EXEC_STREAMING_DESIGN.md`.** v1.0 will move exec, file ops, and any future log streaming onto the proxy's data plane as bidi WebSocket streams. Do not propose band-aids on the current message-shaped exec; they perpetuate the architectural mistake the design explicitly rejects. The data-plane choice also positions v1.1 transparent-WebSocket-forwarding (VNC-from-browser, inbound WS apps) and computer-use agent APIs to fall out naturally.
