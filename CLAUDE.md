# Project: open-sandbox

@import ./ENGINEERING_DISCIPLINE.md

## Overview

A Rust-based sandbox platform where agents dial out to a controller/proxy over TLS, enabling BYO workers from any network topology. The platform provides isolated OCI container sandbox environments with public HTTPS access via wildcard subdomains, backed by a cloud-portable Pulumi infrastructure layer.

## Current phase

Phase 6 complete: all modules implemented including `agent-youki`. Controller, agent, proxy, CLI shell, infra, agent-docker, proxy-http, api, api-files, and agent-youki modules done. The `agent-youki` module (`module/agent-youki/done`) replaces Docker Engine with youki/libcontainer as a daemonless OCI container runtime (ADR-009). Image pull via oci-client, container lifecycle via libcontainer, networking via CNI bridge+portmap, exec via nsenter. `DockerRuntime` extracted to its own crate (`crates/agent-docker/`); `YoukiRuntime` in `crates/agent-youki/`. Runtime selection via compile-time Cargo features (`docker` default, `youki` for Linux production). Build constraint: full build/test on Linux only (`Dockerfile.test` + `docker-compose.test.yml`).

**Contracts currently at `contracts/v0.7.0-frozen`** (SDK agent ergonomics — list sandboxes, exec stdin/cwd, single-file write_file, GET read_file, base64 stdout, COMMAND_NOT_FOUND, structured exec logs). Implementation merged on `contracts/amendment-sdk-agent-friction`. `AgentError::Docker` is now `AgentError::Runtime`; `ApiError::FileNotFound.path` is now `.resolved_path`.

**Pending major amendment: exec streaming → v1.0.0.** A full pre-amendment design lives at `EXEC_STREAMING_DESIGN.md` (settled decisions, spike-confirmed assumptions, forward trajectory). Reshapes exec from a message exchange to a stream-shaped session over WebSocket, riding the data plane (proxy) instead of the control plane (controller). Closes friction items H1–H4, M1, M2, M4, M5 from the post-v0.7 friction report. Implementation will live on `contracts/amendment-exec-streaming`. Before working on any "exec timeout / streaming / process control / file ops" item — read that design first.

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
- `EXEC_STREAMING_DESIGN.md` — **pending v1.0 refactor** design doc. Source of truth for the exec streaming amendment. Read before any work on exec timeouts, streaming, process control, sessions, file ops, computer-use, or VNC-from-browser.
- `spikes/exec-streaming/spike-0{1,2,3}-*/RESULT.md` — confirmed spike outcomes the design relies on (both runtimes need explicit kill-on-disconnect; axum WebSocket backpressures and detects disconnects cleanly).

## Notes for future sessions

- Primary design document was provided at project inception with full architecture, cost analysis, and scaling path
- Agent-dials-out architecture is the foundational decision — all other choices flow from it
- Hetzner is the default cloud for cost optimization; AWS validates the cloud abstraction
- Cloudflare for DNS regardless of compute cloud
- Multi-tenancy decision and raw TCP exposure are open questions from the design doc
- **The exec-as-message vs. exec-as-stream architectural decision has been made and documented in `EXEC_STREAMING_DESIGN.md`.** v1.0 will move exec, file ops, and any future log streaming onto the proxy's data plane as bidi WebSocket streams. Do not propose band-aids on the current message-shaped exec; they perpetuate the architectural mistake the design explicitly rejects. The data-plane choice also positions v1.1 transparent-WebSocket-forwarding (VNC-from-browser, inbound WS apps) and computer-use agent APIs to fall out naturally.
