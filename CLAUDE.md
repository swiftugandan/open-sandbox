# Project: open-sandbox

@import ./ENGINEERING_DISCIPLINE.md

## Overview

A Rust-based sandbox platform where agents dial out to a controller/proxy over TLS, enabling BYO workers from any network topology. The platform provides isolated Docker sandbox environments with public HTTPS access via wildcard subdomains, backed by a cloud-portable Pulumi infrastructure layer.

## Current phase

Phase 6 in progress: per-binary implementation. Controller, agent, proxy, and CLI shell modules complete (`module/controller/done`, `module/agent/done`, `module/proxy/done`, `module/open-sandbox/done`). Next: `infra` (Pulumi stack).

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

## Notes for future sessions

- Primary design document was provided at project inception with full architecture, cost analysis, and scaling path
- Agent-dials-out architecture is the foundational decision — all other choices flow from it
- Hetzner is the default cloud for cost optimization; AWS validates the cloud abstraction
- Cloudflare for DNS regardless of compute cloud
- Multi-tenancy decision and raw TCP exposure are open questions from the design doc
