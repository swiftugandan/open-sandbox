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

## Closed in the second code-review pass

| # | Finding | Closing commit |
|---|---|---|
| 6 | `drive_wait_port_listening` probe-loop math: `elapsed_attempt = attempt_timeout.saturating_sub(remaining)` was always 0, so the post-attempt sleep ignored the actual connect duration and effective cadence drifted to ~2× the documented 50ms under slow handshakes. The not-ready branch could also return `elapsed_ms > timeout_ms` by up to one probe interval. | `fix(agent): close code-review findings on B1-B7` |
| 7 | `drive_list_dir` error mapper routed the docker / youki stub error to `LIST_DIR_FAILED` instead of `NOT_IMPLEMENTED`, breaking capability feature-detection symmetry with `drive_write_file`'s precondition guard. Also tightened the `FILE_NOT_FOUND` substring match from loose `"No such"` to `"No such file"` to match `drive_read_file` and avoid mis-classifying `"No such container"`. | Same commit. |
| 8 | `crates/agent/Cargo.toml` `[dependencies] tokio` was missing the `net` feature even though `drive_wait_port_listening` calls `tokio::net::TcpStream::connect`. Workspace builds compiled by feature-unification leak only. | Same commit. |
| 9 | `impl<R: ContainerRuntime> HostPortLookup for SandboxManager<R>` had no explicit `where R: Send + Sync + 'static` bound; relied transitively on `ContainerRuntime`'s supertrait. | Same commit. |

## Closed in the third code-review pass

| # | Finding | Closing commit |
|---|---|---|
| 10 | `parse_ls_entry_line` used `splitn(7, char::is_whitespace).filter(...)` which silently dropped lines whenever ls right-justified a numeric column (real ls output collapses to runs of spaces between columns). | `fix(agent-docker): close v3 code-review findings` |
| 11 | `mode_string_to_octal` emitted a fixed-leading-zero 4-char string and never carried the setuid / setgid / sticky bits — a 0o4755 setuid binary reported as `"0755"`, cross-runtime asymmetric with youki which preserved all four nibbles. | Same commit. |
| 12 | `exec_collect_stdout` buffered child stdout into an unbounded `Vec<u8>` — an adversarial directory with millions of entries OOM'd the agent before parse_ls_lan_output applied its LIST_DIR_MAX_ENTRIES cap. | Same commit. |
| 13 | `exec_collect_stdout`'s NotFound classifier matched on `"not found"`, folding the OCI runtime's `"executable file not found in $PATH"` into FILE_NOT_FOUND. Tightened to `"No such file"` / `"cannot access"` only. | Same commit. |
| 14 | `drive_write_file` returned early on REVISION_MISMATCH / NOT_IMPLEMENTED / write-error without draining pipelined `client_frames`, leaving Stdin frames queued on the demux and head-of-line-blocking other multiplexed sessions on the same tunnel. | Same commit. |
| 15 | `stat_revision_in_ns` (youki) used `symlink_metadata` while agent-docker's `stat -c "%Y %s"` follows symlinks by default. A symlink path returned different revisions across runtimes, breaking the cross-runtime continuity claim. Aligned youki to use `std::fs::metadata` (follows symlinks). | Same commit. |

## Closed in the seventh + eighth code-review pass (UI)

| # | Finding | Closing commit |
|---|---|---|
| 24 | `LiveEditPanel.onSave` deps included `tabs`, so its identity changed on every keystroke. That identity flowed into the Editor's `saveKeymap` / `blurExtension` memos, which changed `extensions` reference, which forced @uiw/react-codemirror to tear down and recreate the EditorView on every character typed — destroying cursor / scroll / undo state per keystroke. | `fix(ui): close v7 code-review findings on live-edit integration` |
| 25 | CM6 `Mod-s` keymap + document-level Cmd-S handler both fired for one keystroke when focus was inside the editor → concurrent double-save against the same revision token. | Same commit. |
| 26 | `LiveEditPanel` was not keyed by `sandbox_id` in `right-pane.tsx`, so switching sandboxes left tabs / dirty buffers / cached revisions / reloadKey from the previous sandbox in place. Subsequent saves wrote a sandbox-A path into sandbox B with a sandbox-A revision token. | `fix(ui): close v8 code-review findings on preview pane` |
| 27 | `LiveEditPanel` (and therefore the preview `<iframe>`) was always mounted under `className="hidden"` even when the user was on Exec/Files/Info — every save-chain reloadKey bump fired a real network request against the sandbox's public URL invisibly. Added a one-shot `editTabEverVisited` gate so the panel only mounts after first visit. | Same commit. |
| 28 | `scheduleReload` setTimeout and the fire-and-forget `waitPortListening` IIFE in `onSave` could `setState` on an unmounted component (sandbox switch, route nav). Added `mountedRef` guard + cleanup useEffect. | Same commit. |
| 29 | `onSave` useCallback was missing `previewPort` and `scheduleReload` in its dep array — stale capture if the previewPort prop ever changed. | Same commit. |

## Closed in the fourth code-review pass

| # | Finding | Closing commit |
|---|---|---|
| 21 | CORS `expose_headers` only listed `content-type`. The v1.0.3 work introduced `X-File-Revision` as the primary channel for the UI to capture the revision token, but a browser running on a different origin had the header stripped before JS could read it — silently disabling the optimistic-concurrency loop. | `fix(api): close v4 code-review findings on Group C` |
| 22 | `drive_write_file` skipped `drain_remaining_client_frames` on the pre-existing PAYLOAD_TOO_LARGE / INVALID_REQUEST early-returns; only the v1.0.3-added precondition paths had the drain. A pipelining client tripping the cap would HoL-block other multiplexed sessions on the same agent tunnel. Same fix in `drive_write_files_targz`. | Same commit. |
| 23 | `ws_read_file::pump` maintained a raw-string IoError translation table (FILE_NOT_FOUND / SANDBOX_GONE / `_` → IoStreamFailed) parallel to the centralized `handlers::map_io_error`. Routed every dispatch through `map_io_error_pub` so the WS endpoint inherits REVISION_MISMATCH → 4409 / NOT_IMPLEMENTED → 4501 mapping. Added explicit arms to `close_for_api_error` for both v1.0.3 ApiError variants, removing the wildcard's silent downgrade. | Same commit. |

## Deferred to a v1.0.3 follow-up (or v1.1)

| # | Finding | Plan |
|---|---|---|
| 16 | agent-docker `list_dir` uses `ls --time-style=+%s`, a GNU coreutils flag busybox rejects. Alpine sandboxes get LIST_DIR_FAILED with a cryptic stderr message. | Add a busybox-detection probe + fallback parser (or a `find -exec stat` pipeline). Deferred — most production sandbox images use coreutils-based distros. |
| 17 | `drive_read_file` now ALWAYS issues a `runtime.stat_revision()` round trip even when the caller would discard the FileMeta sidecar. Adds latency for v1.0.2-shaped callers. | Add an opt-in `with_revision: bool` to `ReadFileParams` (additive proto change, low risk) in a v1.0.4 amendment. |
| 18 | `WaitPortListeningResult.elapsed_ms` is clamped to the server-clamped `timeout_ms`, not the caller's original value. A caller asking for 600s gets `elapsed_ms <= 300_000` with no signal that the platform cap was hit. | Either document the clamp behavior in CONTRACTS.md as part of the v1.0.3 surface, or add a sentinel `clamped: bool` field on the result. |
| 19 | `mtime`-second granularity opens a sub-1s TOCTOU window for optimistic writes (writer A reads at second T, writer B writes between A's read and A's write at second T, A's expected_revision still matches). | Either bump revision encoding to `<mtime_nanos>:<size>` (a wire-format change but additive to the opaque-string contract) or add a `<size>:<inode>` component. v1.1+. |
| 20 | Cross-runtime divergence in `total_entries` semantics: agent-docker counts only parseable ls lines; agent-youki counts every readdir entry. | Tighten the trait contract in CONTRACTS.md to "every entry the runtime saw, including ones it couldn't fully describe" and align both impls. |
| 21 | `LiveEditPanel.previewPort` defaults to 8080 — sandboxes created with a custom `exposed_port` (e.g. `3000` for Next.js, `5173` for Vite) get a broken save chain: wait_port_listening probes 8080 unconditionally, times out after 3s on every save, and the iframe reload races the real watchexec restart on the actual port. | Contracts change: add `exposed_port: u32` to `SandboxInfo` (`crates/api/src/service.rs`) + the TS `Sandbox` interface (`ui/lib/api.ts`). Plumb through `right-pane.tsx → LiveEditPanel`. Defer to a v1.0.4 amendment alongside the other deferred contract changes. |
| 22 | Preview iframe re-creates on every save (via `key={reloadKey}` for cross-origin-reload correctness), destroying scroll position / form inputs / client-side router state in the previewed app. Plan-documented behavior, but worth measuring against a Next.js / Vite app to see if a same-origin path (where `iframe.contentWindow.location.reload()` IS callable) is worth special-casing. | Polish / measurement, not correctness. |

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
