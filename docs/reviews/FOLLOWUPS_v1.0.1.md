# v1.0.1 follow-ups

Gaps surfaced by the post-amendment audit. P1–P3 are **closed and
shipped on `main`**; P4 items are deferred and tracked here for
visibility.

## Closed (shipped on `contracts/v1.0.1`)

| #  | Item                                       | Branch / tags                                                    |
|----|--------------------------------------------|------------------------------------------------------------------|
| P1 | WS `/files/read-stream` streaming endpoint | `module/v1.0.1-ws-read-file/{green,refactored,live-verified,done}` |
| P2 | Two-listener proxy split (`:50052` Public / `:50053` Internal) | `module/v1.0.1-two-listener-proxy/{live-verified,done}` |
| P3 | youki file ops via `setns(2)` (no in-container `cat`/`tee`/`tar`) | `module/v1.0.1-youki-setns-file-ops/{green,refactored,live-verified,done}` |

Operator-facing summaries of each item are in `CHANGELOG.md` under
`## v1.0.1`. The architectural "why" lives in
`EXEC_STREAMING_DESIGN.md` for shape decisions and in the merge
commit messages on `main` for the surprises uncovered during
implementation (notably the kernel's `fs->users == 1` requirement
for `setns(MNT)`, and the axum 0.7↔0.8 transitive-trait collision
that forced the WS route's distinct path).

## P4 — deferred, visibility-only

These were acknowledged during the audit but accepted as v1.0
limitations. None block production use; they improve operability
and should be scheduled before tagging `contracts/v1.1.0-frozen`.

### P4.1 — Prometheus metrics

Eleven metrics were specified across `crates/agent` and
`crates/api` in `PLAN_EXEC_STREAMING.md`. None are implemented;
the `prometheus` crate is not wired and no `/metrics` HTTP
endpoint exists. Current ops story is "read tracing logs".

### P4.2 — Missing tracing event names

The plan specified these structured event names; the code emits
the substantive events but under different (or no) names:

- `io_session.client_disconnected` (agent)
- `ws.upgrade_rejected` (gateway)
- `proxy_pool.channel_opened` (gateway)
- `proxy_pool.channel_lost` (gateway)

Pure-naming gap; no functional impact.

### P4.3 — youki e2e harness + scenario 02 rewrite

- `infra/e2e/scenarios/run-all-youki.sh` does not exist; only
  the docker-backed `run-all.sh` ships.
- Scenario 02 (bulk transfer) is a bash script measuring a
  10 MiB stdout round-trip. The plan specified a Rust client
  that measures gateway RSS to assert backpressure behavior
  rather than just throughput. Softer assertion of the
  intended property.

## Out of scope

Anything that requires a contract change (new RPCs, modified
message shapes) is by definition not v1.0.1 — it would need its
own `contracts/amendment-<name>` branch and a minor or major
version bump.
