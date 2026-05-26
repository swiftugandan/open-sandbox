# Plan: contracts/v1.0.2 amendment

## Why

The component-0 audit (REVIEW_LOG comp-0 section) surfaced 10 findings that the
contracts crate frozen at `v1.0.1` cannot fix without a bump. This document is
the amendment plan — what changes, what cascades downstream, and how to ship
the cycle without breaking the existing-on-`main` system.

User decided "Schedule v1.0.2 now" in the comp-blitz round (2026-05-25). This
plan is the next session's starting point; implementation is **not yet on the
tree** — `main` still anchors on `contracts/v1.0.1`.

## Scope (the 10 findings)

| # | Source | What v1.0.2 changes |
|---|---|---|
| 1 | comp-0 + comp-3 C3 | New `IoErrorCode` enum + `IoError.code` wire field constrained to it. Vendor the agent-emitted set: `RuntimeError`, `SandboxGone`, `ExecFailed`, `ReadFailed`, `WriteFailed`, `FileNotFound`, `InvalidRequest`, `ExtractFailed`, `PayloadTooLarge`, `Cancelled`. Closes the alias hack (`SANDBOX_NOT_FOUND → SandboxGone`) shipped in comp-6. |
| 2 | comp-0 + comp-6 + comp-1 | Structured error trailer: gRPC responses carry `x-os-error-code: <variant>` metadata. New trait `WithErrorCode` on tonic Status. `grpc_to_api` reads the trailer first; falls back to per-method NotFound mapping. Closes the comp-1 controller-side flattening to `Status::internal` and the comp-6 api-side `NotFound → SandboxNotFound` collapse. |
| 3 | comp-0 (port range) | `exposed_port` typed as `Port(u16)` newtype in contracts + new `MAX_PORT_NUMBER = 65535` const. Proto field stays `uint32` (wire-compat) but parsing rejects > 65535. |
| 4 | comp-0 (cpu units) | Rename `AgentResources.cpu_cores` → `cpu_millicores` (wire-breaking) OR add doc-comment + a `CpuMillicores(u32)` newtype so the type system catches the cores/millicores mismatch. Recommend the latter — keeps wire compat. |
| 5 | comp-0 (SandboxId validator) | `SandboxId::try_from(&str)` validator; `MAX_SANDBOX_ID_LEN = 36` (uuid dashed). Used at every wire-decode boundary in proxy/agent/controller. |
| 6 | comp-0 (subdomain hardcoded 12) | `SUBDOMAIN_LEN = 12` + `subdomain_is_valid(&str)` helper. `SandboxId::subdomain()` uses the const; proxy's router uses the helper. |
| 7 | comp-0 (ApiError wildcard) | Drop the `_ => "UNKNOWN"` wildcard in `error_code()`. The `#[non_exhaustive]` attribute forces the compiler to flag any new variant inside the defining crate. External crates still see `#[non_exhaustive]` semantics. |
| 8 | comp-0 (IoClientFrame first-frame) | `validate_first_frame(&IoClientFrame) -> Result<&IoStart, IoFrameError>` helper. Proxy + agent both use it; no more "each side must remember to reject". |
| 9 | comp-0 (IoSignal.signum) | `Signum(u8)` newtype in 1..=64 (POSIX + RT). Proto field stays `uint32` (wire-compat) but parsing rejects out-of-range. Closes the silent `signum=0` `kill -0` no-op vector. |
| 10 | comp-0 (as_secs truncation) | `try_as_seconds_u32(d: Duration) -> Result<u32, OverflowError>` helper. Detects truncation when callers convert a `Duration` constant to the `uint32` wire field. Used by `controller::management.rs` and `cli/run.rs`. |
| 11 (bonus) | comp-1 F2 cross-component | The Stopped/state-propagation issue (agent → controller → sandbox_states): if v1.0.2 ships the structured error trailer (#2), the controller's F2 owner check can include a terminal-state exception so SandboxStatus(Stopped) lands even after `release_sandbox` removes the routing row. |
| 12 (bonus) | comp-3 C3 | Closed in comp-6's `SANDBOX_NOT_FOUND` alias — once #1 ships the IoErrorCode enum, the alias is no longer needed. |
| 13 (bonus) | startup-time /code-review 2026-05-26 | `PullPolicy { UNSPECIFIED, IF_NOT_PRESENT, ALWAYS, NEVER }` enum added to `CreateSandboxRequest` (api.proto) and `SandboxConfig` (controller.proto). New `open_sandbox_contracts::types::PullPolicy` rust-side serde wrapper (kebab-case JSON, default `IfNotPresent`). Closes the "floating-tag drift" and "no out-of-band pull mechanism" findings from iter1/iter2 of the startup-time loop; structurally lands the docker-runtime warm-path optimization (p50 1623 → 562 ms) without the silent-staleness production risk. **Wire-compat:** new proto3 field; old clients send zero (`UNSPECIFIED`), agent collapses to `IfNotPresent`. **Shipped on the tree (uncommitted) 2026-05-26**; first item of v1.0.2 to actually land on `main`. |

## Wire compatibility

Two wire-breaking candidates: #1 (IoError.code becomes typed enum) and #4
(cpu_cores rename). Everything else is a contracts-internal type tightening that
doesn't change the proto wire schema.

**Recommendation**: ship #1 as an enum on the Rust side that *parses* a string
field. Sender (agent) still writes strings; receiver (proxy/api/SDK) parses to
the enum. No wire break; new code gets type safety. The string→enum mapping
goes in contracts so all crates agree.

**For #4**: keep the proto field name `cpu_cores`; rely on the `CpuMillicores`
newtype + doc to prevent confusion in Rust. Pure-Rust type safety; wire stays
identical.

This keeps `contracts/v1.0.2` wire-backwards-compatible with `v1.0.1`. A
deployed cluster running mixed v1.0.1 + v1.0.2 binaries keeps working.

## Implementation order

Reverse-topology: contracts crate first, then downstream consumers in dependency
order so each commit compiles standalone.

1. **`crates/contracts` v1.0.2 amendment branch** (`contracts/amendment-v1.0.2`)
   - New types: `IoErrorCode` enum, `Port(u16)`, `CpuMillicores(u32)`,
     `Signum(u8)`, `SUBDOMAIN_LEN` const, validators.
   - `ApiError::error_code()` loses the wildcard arm.
   - Helpers: `validate_first_frame`, `try_as_seconds_u32`,
     `subdomain_is_valid`, `SandboxId::try_from(&str)`.
   - New module: `wire_errors` with `WithErrorCode` trait + `x-os-error-code`
     header constant.
   - Tag: `contracts/v1.0.2-frozen`.

2. **`crates/controller`** (~150 LOC)
   - Emit `x-os-error-code` trailer on every Status mapped via
     `controller_error_to_status`.
   - SandboxStatus(Stopped) terminal-state exception in F2 owner check
     (cross-component finding from comp-3 C2 — controller-side fix).
   - Rename internal `cpu_cores * 1000` → `CpuMillicores::from_cores()`.

3. **`crates/api`** (~80 LOC)
   - `grpc_to_api` reads `x-os-error-code` first; falls back to per-method
     NotFound mapping.
   - Drop the `SANDBOX_NOT_FOUND` alias hack — `map_io_error` consumes the
     new `IoErrorCode` enum instead.

4. **`crates/proxy`** (~50 LOC)
   - `Router::extract_sandbox_id` uses `subdomain_is_valid` and
     `SUBDOMAIN_LEN`.
   - Hardcoded `12` removed.

5. **`crates/agent`** (~30 LOC)
   - signal_exec uses `Signum::try_from(u32)` instead of the inline
     `is_valid_signum` helper.
   - Agent emits `IoError { code: <IoErrorCode-string-form> }` everywhere
     (no functional change; just uses the enum's serde).

6. **`crates/ws-client`** (~20 LOC)
   - Receive side parses `IoErrorCode` enum (better SDK ergonomics).

7. **`tests/live_e2e.rs`** in controller + youki: new tests for the
   trailer round-trip, port-range rejection, signum-range rejection,
   subdomain validator.

## Acceptance gate

- `cargo test --workspace --lib` green on macOS + Linux (Linux for youki).
- Tag `contracts/v1.0.2-frozen` exists.
- `CODE_REVIEW_PLAN.md` row 0 ("contracts (audit)") moves from "done; all
  findings deferred" to "v1.0.2 frozen; deferred items closed".
- `REVIEW_LOG.md` comp-0 entries each get a `**closed in v1.0.2**` note.
- Cross-component follow-ups in `NEEDS_HUMAN_ATTENTION.md` referencing
  comp-0 items are removed.

## Estimated effort

~400 LOC total across all crates. One focused session if no scope creep.
The biggest risk is the trailer mechanism in tonic — it's straightforward
but I'll want to verify the `x-os-error-code` propagation across the
proxy hop (controller → api → SDK is one hop; the proxy doesn't relay
controller errors but does emit its own ProxyError that should also use
the trailer convention).

## What this DOESN'T address

- Multi-tenancy (still an open SPEC question in CLAUDE.md).
- The OCI hardening decisions deferred for the operator (comp-5).
- The wildcard HTTPS / TLS for user-facing `*.sandbox.<domain>` (comp-2
  C5 partial — the OpenTunnel listener now has ACME, but the public HTTP
  wildcard for sandbox traffic still expects Cloudflare termination).

These remain in `NEEDS_HUMAN_ATTENTION.md` until you make those calls.
