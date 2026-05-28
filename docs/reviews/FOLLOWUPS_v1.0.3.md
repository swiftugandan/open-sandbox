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

## Closed in the ninth code-review pass (Group D polish)

| # | Finding | Closing commit |
|---|---|---|
| 30 | `LiveEditPanel`'s D12 visibility re-stat fired a spurious REVISION_MISMATCH conflict when the agent runtime transitioned from non-revision-supporting (returning `null`) to revision-supporting (returning a real token) mid-session. Capability change is not a file mutation. Now silently adopts the new revision as the new baseline. | `fix(ui): close v9 code-review findings on Group D polish` |
| 31 | Duplicate "Conflict" label in StatusBar AND ConflictBanner. Removed the StatusMessage inline string — the banner above the editor is the single source. | Same commit. |
| 32 | `listUnsavedBuffersForSandbox` was imported but unused (the restore-prompt UI didn't ship in D9). Removed the dead import. | Same commit. |

## Deferred follow-ups (v1.0.4 or polish)

| # | Finding | Plan |
|---|---|---|
| 33 | `openDb` in unsaved-buffer.ts caches the resolved Promise even on failure — one bad open (private-window toggle, quota) disables IndexedDB persistence for the rest of the tab's lifetime. | Null the cache on error so the next call re-attempts; alternatively invalidate on `visibilitychange`. |
| 34 | `putUnsavedBuffer` fires unthrottled on every keystroke — full file content per keypress. For a 1 MB file at 10 keystrokes/sec, ~10 MB/s of IDB transaction overhead. | Add a 200–300 ms per-path debounce. 300 ms of lost work on a crash is still within the acceptable v1.0.3 crash-safety bound. |
| 35 | D12 periodic re-stat fetches the full file body via `api.readFile` just to read the X-File-Revision header. For a 10 MB asset, ~10 MB/30s of wasted bandwidth. | Introduce a dedicated `GET /v1/sandboxes/{id}/files/stat?path=…` endpoint that returns the FileMeta header with no body (and matching `api.statRevision` client helper). v1.0.4 wire addition. |
| 36 | Stash-restore UX gives misleading conflict-banner copy when BOTH the stashed buffer AND disk content have moved since the last save. The banner says "the file was changed on the agent since you opened it" but the stash is also older than disk. | Compare stash mtime to disk revision freshness on restore; if stash is also stale, surface a third banner state ("local + remote both changed; reload to discard local, overwrite to last-write-wins"). |

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
| 16 | ~~agent-docker `list_dir` uses `ls --time-style=+%s`, a GNU coreutils flag busybox rejects. Alpine sandboxes get LIST_DIR_FAILED with a cryptic stderr message.~~ **Closed 2026-05-28 in `fix(agent-docker): busybox-portable list_dir via shell loop + stat`** — replaced `ls -lAn --time-style=+%s -q` with a `for f in *; stat -c '%F|%s|%Y|%a' "$f"` shell loop that works on both GNU + busybox. Verified live on python:3.12-alpine. |
| 17 | `drive_read_file` now ALWAYS issues a `runtime.stat_revision()` round trip even when the caller would discard the FileMeta sidecar. Adds latency for v1.0.2-shaped callers. | Add an opt-in `with_revision: bool` to `ReadFileParams` (additive proto change, low risk) in a v1.0.4 amendment. |
| 18 | `WaitPortListeningResult.elapsed_ms` is clamped to the server-clamped `timeout_ms`, not the caller's original value. A caller asking for 600s gets `elapsed_ms <= 300_000` with no signal that the platform cap was hit. | Either document the clamp behavior in CONTRACTS.md as part of the v1.0.3 surface, or add a sentinel `clamped: bool` field on the result. |
| 19 | `mtime`-second granularity opens a sub-1s TOCTOU window for optimistic writes (writer A reads at second T, writer B writes between A's read and A's write at second T, A's expected_revision still matches). | Either bump revision encoding to `<mtime_nanos>:<size>` (a wire-format change but additive to the opaque-string contract) or add a `<size>:<inode>` component. v1.1+. |
| 20 | Cross-runtime divergence in `total_entries` semantics: agent-docker counts only parseable ls lines; agent-youki counts every readdir entry. | Tighten the trait contract in CONTRACTS.md to "every entry the runtime saw, including ones it couldn't fully describe" and align both impls. |
| 21 | `LiveEditPanel.previewPort` defaults to 8080 — sandboxes created with a custom `exposed_port` (e.g. `3000` for Next.js, `5173` for Vite) get a broken save chain: wait_port_listening probes 8080 unconditionally, times out after 3s on every save, and the iframe reload races the real watchexec restart on the actual port. | Contracts change: add `exposed_port: u32` to `SandboxInfo` (`crates/api/src/service.rs`) + the TS `Sandbox` interface (`ui/lib/api.ts`). Plumb through `right-pane.tsx → LiveEditPanel`. Defer to a v1.0.4 amendment alongside the other deferred contract changes. |
| 22 | Preview iframe re-creates on every save (via `key={reloadKey}` for cross-origin-reload correctness), destroying scroll position / form inputs / client-side router state in the previewed app. Plan-documented behavior, but worth measuring against a Next.js / Vite app to see if a same-origin path (where `iframe.contentWindow.location.reload()` IS callable) is worth special-casing. | Polish / measurement, not correctness. |
| 38 | NOT_IMPLEMENTED routing uses `detail.starts_with("…not yet implemented")` — brittle stringly-typed contract that only the test stub emits. Future "backend doesn't support this op" surfaces lose typed routing unless they remember the magic string. | Introduce an `AgentError::NotImplemented { op: &'static str }` variant (or a discriminant on `Runtime`) so backends signal capability gaps structurally; update map_io_error / drive_* match arms to consume it. |
| 39 | agent-docker `wait_port_listening` cadence drift — `nc -z; sleep 0.05` in a shell loop means effective interval is `nc_cost + 50ms`, slightly above the documented 50ms. agent-youki's setns probe preserves the v6 deadline-anchored invariant; the two backends drift by ~20-40% on observed `elapsed_ms`. | Move to a deadline-anchored shell loop: `end=$(($(date +%s%N)/1000000 + timeout))` + per-iteration check. Or accept the drift as documented; the user-visible impact is minor. |
| 40 | `_host_ports` parameter on `drive_io_session` and the `HostPortLookup` trait are unused after the wait_port_listening netns redirect. Surfaces as dead-code in the public agent surface. | Either delete the wire-through (and re-introduce when a real host-side probe lands) or add an explicit module-level comment citing the planned future use. |
| 41 | `MockContainerRuntime::wait_port_listening` ignores `_timeout` and returns immediately. The in-tree test `wait_port_listening_returns_not_ready_when_runtime_reports_silent` can only assert `elapsed_ms <= 250` (trivially true) — the v6 lower-bound invariant `elapsed_ms >= 200` from the previous test is gone. | Have the mock honor `timeout` via `tokio::time::sleep(timeout)` when the port isn't in the listening set (or expose a configurable mock-delay knob), and restore the lower-bound assertion. |
| 42 | agent-youki's netprobe thread is uncancellable — a plain `std::thread::spawn` polls inside the container's netns until `timeout` (clamped at 5 min) elapses. If `drive_io_session` is cancelled (stream closes, agent shutdown), the thread keeps running. Mount-ns helper has the same shape; both pre-date v1.0.3. | Wire `tokio::sync::watch` or a flag the spawned thread polls, so cancellation propagates. Document the cancellation semantics either way. |
| 43 | agent-docker rewrites `timeout = 0` to 50ms via `.max(50)`; agent-youki honors zero (loop's first guard breaks immediately, returns ready=false). Cross-runtime divergence on the zero-timeout edge case. | Pick one — most natural is to honor zero on both (poll once, return whatever the synchronous check sees). Document on the trait. |
| 44 | `DeleteFileQuery` (and ListDirQuery / ReadFileQuery / WaitPortListeningRequest) use `#[serde(deny_unknown_fields)]`. A UI cache-buster query (`&t=12345`) or a future tracing parameter would silently 400. | Audit the v1.0.3 query types; drop deny_unknown_fields on the ones the UI is expected to evolve, or add a documented allowlist of pass-through fields. |
| 45 | `delete_file` missing-path resolves to Ok(()) on both runtimes (idempotent under concurrent external rm). A typo'd path or wrong rootPath prefix silently succeeds with no UI signal. | Add a `not_found_was_idempotent: bool` field to a new typed result (or expose via response header) so the UI can log info-level when the delete was a no-op. |
| 46 | agent-docker uses `rm -rf` / `rm -f` semantics (removes symlinks-to-dirs as the link only); agent-youki uses `std::fs::symlink_metadata` + dispatch on `is_dir()` (also removes the link only on the recursive path). Coreutils diverges with trailing-slash combos. Low impact today (tree paths don't carry trailing slashes). | Document the contract on the trait method and add a regression test covering the symlink-to-dir cases on both runtimes. |

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

## v12 polish-iteration deferred items (2026-05-28)

These three surfaced in the v12 code-review pass on the live-edit
polish batch (commits 7c4eb2f..HEAD; URL bar removal, Files tab
removal, /workspace template realignment, hot-reload, entrypoint
mkdir). Five of the eight findings were closed in the same iteration;
these three need structural changes that didn't fit the polish scope.

### V1 — Mobile (<768px) loses ad-hoc file IO

The previous Files tab let any-viewport user read /etc/os-release,
write /tmp/hello.txt, and inspect any absolute path. The Edit tab
that replaced it gates the file tree + editor column behind
`hidden md:block` (live-edit-panel.tsx:508, 546) so mobile sees the
preview iframe only. Combined with FilesPanel's deletion, mobile
users have zero in-UI file IO.

**Options:**

- **Mobile FileTree drawer.** Slide-over from the side; same tree
  data, same delete/new-file affordances, just stacked instead of
  side-by-side. Touches live-edit-panel.tsx layout + adds a small
  drawer component.
- **Mobile-only slimmed FilesPanel.** Bring back FilesPanel as a
  `md:hidden` affordance — two text inputs over read/writeFile, no
  tree. Cheapest, ugliest.
- **Accept the limitation.** Document "use the desktop console or
  the `open-sandbox` CLI for ad-hoc file IO on mobile" in a tooltip
  or empty-state on the mobile preview pane.

**Recommendation:** Mobile FileTree drawer is the structurally
right fix. Defer until there's user demand — mobile via the dev
console is the minority use case.

### V2 — Preview iframe shows 502 for blank sandboxes

PreviewPane (preview-pane.tsx:131) gates the iframe only on
`isRunningStatus(status)`. A blank sandbox or one created with
autorun=false transitions to running with `sleep infinity` as PID 1
and nothing bound to :8080. The iframe then loads the public URL and
the user sees a raw proxy 502. The old urlExpected lifecycle gate
(removed via user request in 7208938) covered this case.

**Options:**

- **"Load preview" button.** Render an empty-state with a manual
  load button until the user clicks it (or until exec fires
  successfully). One-click cost, no lifecycle tracking.
- **Cheap HEAD probe.** Fetch HEAD against the public URL on mount;
  if 502, show empty-state; if 200, load iframe. One request per
  preview pane open. Doesn't reintroduce the urlExpected map.
- **Reintroduce urlExpected (rejected).** User explicitly removed
  this in the same iteration as the URL bar; don't re-add it.

**Recommendation:** HEAD-probe is cleanest — covers all cases
(autorun, blank, exec-not-yet-fired) without re-coupling to exec
lifecycle. Cost is one HEAD per sandbox-open. Defer until a user
reports the 502.

### V3 — nginx template sed without `g` flag

`sed -i 's|/usr/share/nginx/html|/workspace|'` replaces only the
first match per line in `/etc/nginx/conf.d/default.conf`. Today
upstream `nginx:alpine`'s default.conf happens to have each `root`
directive on its own line, so it works. If upstream ever ships two
`root`s on one line, or if a user-customized default.conf does, the
second silently keeps pointing at the original docroot.

**Fix:** add the `g` flag (and likewise to the `listen` substitution
for symmetry). One-character change; trivial. Hold until the next
template touch so it doesn't fire its own review pass.

## Pointer

The original code-review pass output is in the session log for
`contracts/amendment-live-edit-v1.0.3`. Findings were filtered to the
top-10 most severe (recall-biased, ≤10 cap); items numbered 6–10 above
correspond to the lower-severity P3/P4 findings from that pass that
were not closed in this batch.
