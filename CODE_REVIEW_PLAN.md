# Component Code-Review Plan

A cross-session plan for running `/code-review` over the system one component at a time while holding `proto/*.proto` + `crates/contracts` frozen as the anchor.

## Guiding constraint

`proto/*.proto` and `crates/contracts` are **frozen** for the duration of this pass at `contracts/v1.0.1`. Any finding that would require a contract change is **logged, not applied** — contract drift invalidates every other component's review. If a contract change becomes unavoidable, halt the pass, re-tag, and re-plan.

## Mechanics

`/code-review` is a Claude Code skill (formerly `/simplify`) that Claude can invoke directly via the Skill tool. It operates on the current diff. To scope a review to one component, stage that component's diff against the anchor before invoking the skill:

```sh
# Review entire crate as if it were a new contribution against the anchor
git diff contracts/v1.0.1 HEAD -- crates/<name>/

# Or, against an empty tree (full-content review — use for component 0)
git diff $(git hash-object -t tree /dev/null) HEAD -- crates/<name>/
```

Per component:

1. Branch off `main` (skip for audit-only slots like component 0 if no edits will land).
2. Claude invokes the `code-review` skill — start at `medium`, escalate to `high` for trust-boundary code.
3. Triage findings: contract-touching ones go to the deferred list in `REVIEW_LOG.md`; in-crate fixes land in a focused PR.
4. Re-run `code-review` on the fix diff.
5. Merge; move to next component.

One component in flight at a time so reviewer context stays scoped.

## Order (bottom-up by dependency)

| # | Component | Scope | Effort | Why this slot |
|---|---|---|---|---|
| 0 | `proto/` + `crates/contracts` | Audit-only, no edits | `high` | Anchor for everything else. Findings deferred to a separate contract-bump cycle. |
| 1 | `crates/controller` | gRPC handlers, scheduling, token mgmt, PG writes, LISTEN/NOTIFY emit | `high` | Central trust anchor; authority over routing + agent lifecycle. |
| 2 | `crates/proxy` | TLS term, host routing, reverse-tunnel pool, `SandboxIoService.OpenIoStream`, two-listener split, DB cache + miss fallback | `high` | Public attack surface + data-plane correctness (exec/file streaming). |
| 3 | `crates/agent` (core) | Outbound dial, heartbeat, sandbox lifecycle FSM, tunnel forwarding, exec session lifetime | `high` | Reverse trust boundary; carries spike-validated invariants (disconnect → kill, SIGTERM propagation). |
| 4 | `crates/agent-docker` | Docker runtime backend, bollard usage | `medium` | Default dev backend; check against spike 04 backpressure assumptions. |
| 5 | `crates/agent-youki` | libcontainer in-process, CNI exec, oci-client pull, `setns(2)` file ops, cgroup v2 | `high` | Production runtime; kernel-adjacent; PID-capture race (spike 05). |
| 6 | `crates/api` | REST → gRPC translation, API-key auth, error mapping, `/v1/sandboxes/{id}` surface | `medium` | Boundary translator — focus on auth, validation, error shape. |
| 7 | `crates/ws-client` | WS framing for exec + file streams | `medium` | Client of the same data plane the proxy exposes — review as a pair with proxy IO. |
| 8 | `crates/cli` | Operator UX | `low` | Thin client; correctness over surface area. |
| 9 | `infra/` (Pulumi TS) | Topology, DNS, secrets handling | `medium` | Different language → separate pass; not bundled with Rust crates. |

## Per-component checklist (applied every round)

1. **Contract conformance** — respects `contracts/v1.0.1` exactly; no private extensions, no untyped escapes.
2. **Trust boundary** — every untrusted input is validated on entry; every output to a less-trusted layer is sanitized.
3. **Lifetime / cancellation** — for streaming code, disconnect propagates the way the spikes proved it must (see `EXEC_STREAMING_DESIGN.md` spike conclusions).
4. **State authority** — no crate other than `controller` writes routing/agent/sandbox state.
5. **Error mapping** — errors cross boundaries as `contracts::Error`, not panics or string-typed leakage.
6. **Concurrency** — no shared mutability without a documented invariant; no `unwrap()` on cross-task channels.

## Deliverables

- One PR per component, with `/code-review` findings addressed; PR description links the review output.
- A running `REVIEW_LOG.md` (created on the first PR) capturing: deferred contract-change candidates, cross-component findings, spike-invariant violations.
- End of pass: re-tag `contracts/v1.0.2` only if the deferred list justifies it; otherwise close the pass clean.

## Progress tracking

Each row gets ticked as it merges. Update this table as components land.

| # | Component | Status | PR | Notes |
|---|---|---|---|---|
| 0 | contracts (audit) | done | audit-only (no PR) | 10 findings logged in `REVIEW_LOG.md`; all deferred to potential `contracts/v1.0.2`. |
| 1 | controller | fixes-landed; awaiting merge | `review/01-controller` | 10 findings F1–F10 closed; PG-side verification deferred to `tests/live_e2e.rs`. Cross-component follow-ups for cli/proxy logged in `REVIEW_LOG.md`. |
| 2 | proxy | fixes-landed; awaiting merge | `review/02-proxy` | 11 findings closed (A1-A6, B1-B6 minus 3 deferred-with-decision: B2, C5, C2). LISTEN side of comp-1 F4 closed here. |
| 3 | agent (core) | fixes-landed; awaiting merge | `review/03-agent-core` | 7 closed (A1/B2/C1, A2, A5, A6, B5, C6); 7 deferred-with-decision in `NEEDS_HUMAN_ATTENTION.md`. CLI reconnect-loop follow-up from comp-1 closed. |
| 4 | agent-docker | not started | — | |
| 5 | agent-youki | not started | — | |
| 6 | api | not started | — | |
| 7 | ws-client | not started | — | |
| 8 | cli | not started | — | |
| 9 | infra | not started | — | |

## Out of scope for this pass

- Performance tuning, refactors, new features.
- P4 visibility-only items in `FOLLOWUPS_v1.0.1.md` (metrics, tracing event names, Rust scenario-02 rewrite) — tracked separately.
