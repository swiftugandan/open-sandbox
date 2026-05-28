# Follow-ups: contracts/v1.0.3 code-review pass

Closure log for the high-effort `/code-review` pass run against
the contracts/v1.0.3 surface (commits `b80a0ca..` on
`contracts/amendment-live-edit-v1.0.3`). Format mirrors
`FOLLOWUPS_v1.0.1.md`.

## Closed in this batch

| # | Finding | Closing commit |
|---|---|---|
| 1 | ws-client's KIND_* table missing 0x16 / 0x17 / 0x18 — hard-fail decode for the new server frames. | `refactor(contracts): centralize WS frame envelope kinds` |
| 2 | `drive_write_file` silently ignored `expected_revision` and `force` — gateway-side opt-in would have failed open. | `fix(agent): reject write_file expected_revision until group B lands` |
| 3 | `KIND_*` envelope tags duplicated between `crates/api/src/frame.rs` and `crates/ws-client/src/lib.rs` (root cause of #1). | Same commit as #1 — both tables now `use` from `crates/contracts/src/constants.rs`. |
| 4 | `proto/proxy.proto` comment on `ListDirEntry.type` lied about the prost rename. | `docs(contracts): clarify v1.0.3 FileMeta is a sidecar, not a terminator` |
| 5 | `CONTRACTS.md` said FileMeta is emitted "in place of (or alongside) IoExited" — ambiguous, weakened the one-terminator invariant. | Same commit as #4. |

## Deferred design decisions (P4 — to revisit before groups B / C land)

These are not blockers but want a deliberate call rather than letting the
default behavior become the spec by accident.

### D1 — `expected_revision = ""` is fail-open by default

`WriteFileParams.expected_revision` defaulting to empty-means-no-precondition
is a fail-open. A UI bug that clears the cached revision to `""` will
silently disable the precondition; the wire shape cannot distinguish "caller
forgot to set the field" from "caller explicitly opted out".

**Options:**

- **Status quo + agent guard.** The current `drive_write_file` guard
  already refuses non-empty revisions until group B is ready, so the
  fail-open default is only exposed once group B ships. Acceptable if
  the gateway is the only legitimate caller and we trust it to compute
  the field.
- **Wire-level distinction.** Replace the field with
  `oneof precondition { string expected_revision = 3; bool no_precondition = 5; }`
  and keep `force = 4` as the scripted escape hatch. Requires a
  `contracts/v1.0.3` bump while it's still in progress — cheap now,
  expensive after group D ships.

**Recommendation:** defer to a quick decision before group C wires the
gateway-side header mapping (so the gateway can plumb the wire-distinguished
shape from the start). If we go with status quo, document it on the
`WriteFileParams.expected_revision` proto comment.

### D2 — `WaitPortListeningParams.timeout_ms` is unbounded `uint32`

A buggy or hostile caller can pass `timeout_ms = u32::MAX` (~49 days). Each
such session pins a proxy `IoSessions` slot, a `StreamMux` entry, and an
agent probe loop — all per ADR-010 not externalizable. Multiplied across N
sessions, the 1-of-N proxy is a DoS amplifier.

**Options:**

- **Server-side clamp.** Document in CONTRACTS.md a max (e.g. `300_000` ms =
  5 min); agent clamps and warns when callers exceed it. Wire shape unchanged.
- **Wire-level cap.** Tighten the field to a saturating `uint32` with a
  documented max, or `uint16` (still 65 s — too tight). Wire-level cap is
  louder; clamp is more forgiving.

**Recommendation:** server-side clamp at 5 min, enforced in the group B
`drive_wait_port_listening` handler. Document in CONTRACTS.md.

### D3 — `ListDirResult` 5000-entry cap is invisible on the wire

The cap is only in proto comments + CHANGELOG. A heterogeneous fleet (during
rolling upgrade) returning different counts for the same readdir is hard to
debug.

**Options:**

- **Status quo.** Document explicitly in CONTRACTS.md that the cap is
  agent-version-dependent.
- **Wire-level cap field.** Add `uint32 max_entries = 3` to `ListDirParams`
  (caller-requested, agent-clamped). Caps default to a contracts-crate
  constant (e.g. `LIST_DIR_MAX = 5000`). UI can pass a smaller value for
  faster sub-listings; can't pass a larger value.

**Recommendation:** add the wire field while v1.0.3 is still in progress. ~5
LOC in proto, ~5 LOC in the group B handler.

### D4 — `decode_server` returns `FrameError::UnknownKind` on any unknown tag

The codebase has no skip-unknown forward-compat policy in either direction.
A v1.0.3 gateway against a future v1.0.4 agent that introduces a new server
kind fails closed.

**Options:**

- **Document `gateway >= agent` invariant** in CONTRACTS.md.
- **Tolerant decode.** Add `ServerFrame::Unknown { kind, payload }` /
  `IoServerFrame` payload variant that flows the raw bytes through to the
  next layer. Bigger lift, real forward-compat.

**Recommendation:** D4 is genuinely a v1.1+ concern; document `gateway >=
agent` for now and revisit if heterogeneous-version deploys become a real
operational pattern.

### D5 — `FrameError::DecodeFailed` reported on encode failures

`encode_server` arms (both the pre-existing Exited/Error/Started block and
the new ListDirResult/WaitPortListeningResult/FileMeta block) report prost
encoding failures via `FrameError::DecodeFailed { kind, detail }`. A failed
encode is not a decode. Real failures are operationally impossible (the
buffer is grown by `encoded_len()`), but if one ever fires, ops will hunt
the wrong cause.

**Options:**

- **Rename the variant** to `EncodeOrDecodeFailed`.
- **Split** into `EncodeFailed` / `DecodeFailed`.

**Recommendation:** cheap follow-up, do alongside the next `crates/api`
refactor. Not urgent.

## Pointer

The original code-review pass output is in the session log for
`contracts/amendment-live-edit-v1.0.3`. Findings were filtered to the
top-10 most severe (recall-biased, ≤10 cap); items numbered 6–10 above
correspond to the lower-severity P3/P4 findings from that pass that
were not closed in this batch.
