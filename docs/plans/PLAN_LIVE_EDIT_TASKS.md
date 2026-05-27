# PLAN_LIVE_EDIT — per-commit task graph

Companion to `docs/plans/PLAN_LIVE_EDIT.md`. This decomposes that
3–4-day batch into a TDD-ordered commit graph against the current
tree, grounded in the file shapes that already exist on `main`. Each
commit is independently reviewable; every group ends with a green
build under the project's existing CI gates.

The discipline gates in `ENGINEERING_DISCIPLINE.md` apply:

- Red tests commit **before** the implementation that makes them pass.
  Each [red] commit must include the new failing test(s) and nothing
  else.
- Conventional-commit subjects; trailer `Contract: contracts/v1.0.3`
  on contract-bearing commits.
- Contracts crate changes ship in their own PR, **ahead** of any
  consumer.
- One logical change per commit. No `WIP`, no `fix typo` chains.

Versioning: contract surface is `contracts/v1.0.3` (separate track
from the pending v1.0.2 amendments in
`PLAN_CONTRACTS_v1.0.2.md`). Tag movement (`contracts/v1.0.3`,
`contracts/v1.0.3-frozen`) deferred to the closing commit of group A.

## Grounding (current-tree anchors)

- `proto/proxy.proto:125-168` — `IoStart`, `ReadFileParams`,
  `WriteFileParams`. New `ListDirParams` + `WaitPortListeningParams`
  variants land here.
- `crates/agent/src/io_stream.rs:94-142` — dispatch on
  `start.params`. New match arms route to two new
  `drive_list_dir` / `drive_wait_port_listening` handlers below
  `drive_write_files_targz` (≈line 637+).
- `crates/agent/src/io_stream.rs:501-554` — `drive_read_file` shape.
  `drive_list_dir` is structurally parallel: one emit, then Exited.
- `crates/agent/src/container.rs:171-196` — `ContainerRuntime` trait.
  Adds `list_dir`, and a `stat_file` revision helper (or extends
  `read_file` to return `(Bytes, Revision)`).
- `crates/agent/src/sandbox.rs:254-263` — `host_port_for` is already
  the only state `drive_wait_port_listening` needs.
- `crates/api/src/router.rs:99-150` — current routes. Two new
  routes (`files/list` GET, `wait_port_listening` POST) land in the
  same block; both honor the existing `dev_cors_layer`.
- `crates/api/src/service.rs:90-94` — `ReadFileQuery`. New
  `ListDirQuery` and `WaitPortListeningRequest` join this module.
  `WriteFileRequest` (line 59) gains a required `expected_revision`
  field plus `?force=true` query.
- `crates/api/src/handlers.rs` — `read_file` (line ~563),
  `write_file`, `write_files`. New `list_dir` and
  `wait_port_listening` handlers; existing `write_file` handler
  grows the revision check; existing `read_file` emits the
  `X-File-Revision` response header.
- `ui/lib/api.ts:200-232` — current `readFile` / `writeFile`. Both
  grow revision-aware return shapes; `listDir` and
  `waitPortListening` join here.
- `ui/components/files-panel.tsx` — current `<Textarea>`-based
  panel. Group D rewrites this component shell.
- `ui/components/right-pane.tsx` — preview iframe lands as a tab
  alongside Exec/Files/Info.

---

## Group A — Contracts (PR #1, lands first, no consumers yet)

Goal: `contracts/v1.0.3` surface is wire-stable before any agent or
gateway code references it. Branch:
`contracts/amendment-live-edit-v1.0.3`.

| # | Commit | Files | Gate |
|---|---|---|---|
| A1 | `test(contracts): ListDirParams + ListDirResult proto roundtrip` [red] | `crates/contracts/tests/*` (new test); `proto/proxy.proto` (only enough scaffolding for `prost-build` to emit the types — full field set in A2). | `cargo test -p open-sandbox-contracts` reports the new test failing. |
| A2 | `feat(contracts): ListDirParams + ListDirResult on IoStart` [green] | `proto/proxy.proto` (add `ListDirParams`, `ListDirEntry`, `ListDirResult` per PLAN_LIVE_EDIT.md §Contract surface); `crates/contracts/src/wire.rs` if any hand-rolled re-exports. | A1 test green. |
| A3 | `test(contracts): WaitPortListeningParams variant + result shape` [red] | `crates/contracts/tests/*`. | Failing. |
| A4 | `feat(contracts): WaitPortListeningParams + WaitPortListeningResult` [green] | `proto/proxy.proto`. Variant fields: `{port: u32, timeout_ms: u32}`. Result: `{ready: bool, elapsed_ms: u32}`. | A3 green. |
| A5 | `test(contracts): file revision token field` [red] | `crates/contracts/tests/*`. | Failing. |
| A6 | `feat(contracts): revision token on file ops` [green] | `proto/proxy.proto` — `ReadFileParams` gains nothing (revision is response-only). `WriteFileParams` gains `expected_revision: string` + `force: bool`. New `IoServerFrame` variant **OR** extend `IoExited` with optional `revision: string`. Prefer a new `FileMeta` server frame so exec stays unchanged. | A5 green. |
| A7 | `docs(contracts): v1.0.3 surface in CONTRACTS.md + CHANGELOG.md` | `CONTRACTS.md`, `CHANGELOG.md`. | Cross-link to `PLAN_LIVE_EDIT.md`. |
| A8 | (tag) `contracts/v1.0.3` then `contracts/v1.0.3-frozen` after group D merges. | — | Tag-only; no commit. |

Group A confidence gate at the end:

```
Confidence: high if A1..A7 are independently testable with no agent/gateway code.
Residual risks: ListDir entry `type` enum coverage (file/dir/symlink/other) — confirm by hand against POSIX `d_type`.
Known gaps: none — wire shape is the contract.
```

---

## Group B — Agent runtime (PR #2, depends on A)

Branch: `module/agent-live-edit`. Each runtime-trait change lands
twice (docker + youki) in the same commit because both must compile
together; tests sit per-runtime where the trait impl lives.

| # | Commit | Files | Gate |
|---|---|---|---|
| B1 | `test(agent): drive_list_dir emits ListDirResult and Exited on happy path` [red] | `crates/agent/src/io_stream.rs` (new `#[tokio::test]` in the existing mod — see existing `read_file_returns_contents` at line 1048 for the shape). Mock runtime via the existing `testutil` fakes (`crates/agent/src/testutil.rs:195+`). | Failing. |
| B2 | `feat(agent): ContainerRuntime::list_dir` [green] | `crates/agent/src/container.rs` (extend trait); both runtime impls in `crates/agent-docker/` and `crates/agent-youki/` (read_dir via `setns(2)` in youki — parallels the existing file read path); `crates/agent/src/testutil.rs` (fake impls). | B1 green; cargo test on both runtime crates clean. |
| B3 | `feat(agent): drive_list_dir handler` [green] | `crates/agent/src/io_stream.rs` — new arm in the dispatch match (`Some(io_start::Params::ListDir(p)) => drive_list_dir(...)`); new fn modeled on `drive_read_file` (501-554). 5000-entry cap with `truncated` flag enforced server-side. | All `cargo test -p open-sandbox-agent` green. |
| B4 | `test(agent): drive_list_dir caps at 5000 and sets truncated` [red→green together if cheap] | Same test file. Either drive a fake `list_dir` that returns 6000 entries, or seed a tempdir. | Test green. |
| B5 | `test(agent): drive_wait_port_listening returns Ready when host_port accepts a TCP connect` [red] | `crates/agent/src/io_stream.rs` tests. Spin a tokio `TcpListener` on an ephemeral port and seed the `SandboxManager` mock with it. | Failing. |
| B6 | `feat(agent): drive_wait_port_listening handler` [green] | `crates/agent/src/io_stream.rs` — handler is exactly the snippet in PLAN_LIVE_EDIT.md §`wait_port_listening` (≈30 LOC). Needs the `SandboxManager` passed into `drive_io_session` (or surface `host_port_for` via the existing route the dispatch already has — check `drive_io_session` call site). | B5 green. |
| B7 | `test(agent): drive_wait_port_listening returns NotReady after timeout` [red→green] | Same test file. Use a `host_port` that nothing is listening on; assert `ready=false` and `elapsed_ms >= timeout`. | Green. |
| B8 | `test(agent): read_file emits revision in IoServerFrame::FileMeta before Stdout chunks` [red] | `crates/agent/src/io_stream.rs` tests. | Failing. |
| B9 | `feat(agent): emit revision token on read_file` [green] | `crates/agent/src/container.rs` — change `read_file` signature to return `(Bytes, Revision)` (or add `stat_file`); both runtime impls; `drive_read_file` (501-554) emits the new `FileMeta { revision }` frame between IoStarted (implicit) and the first Stdout chunk. Revision format: `mtime_nanos:size` per plan §External-mutation reconciliation. | B8 green. |
| B10 | `test(agent): write_file rejects mismatched expected_revision with 409-equivalent error code` [red] | Tests. | Failing. |
| B11 | `feat(agent): write_file enforces expected_revision; force=true bypasses` [green] | `crates/agent/src/io_stream.rs:556-635` (`drive_write_file`); add a stat-before-write step using the same Revision helper from B9. On mismatch, send `IoError { code: "REVISION_MISMATCH", detail: <actual_revision> }`. The gateway in C8 unpacks `detail` into the `409` JSON body. | B10 green. |
| B12 | `refactor(agent): factor Revision into its own type` if duplication appears across read/write/list. | `crates/agent/src/revision.rs` (new). | All agent tests green. |

Group B confidence gate:

```
Confidence: medium until B6 picks a path for SandboxManager handoff into drive_io_session.
Residual risks:
  - host_port_for is on the manager, but drive_io_session today only sees the container_id. Plumbing the SandboxManager Arc through is a small but real refactor — verify whether it's already in scope or has to be added.
  - youki list_dir via setns(2) on PID namespace — confirm reuse of the existing setns helper used for read_file is safe for opendir/readdir.
Known gaps: revision format (mtime:size vs content-hash) decision deferred to B9 implementation comment.
```

---

## Group C — API gateway (PR #3, depends on B)

Branch: `module/api-live-edit`.

| # | Commit | Files | Gate |
|---|---|---|---|
| C1 | `test(api): GET /files/list returns 200 with entries` [red] | `crates/api/src/tests.rs` — existing test harness already opens an OpenIoStream against a fake proxy; pattern off the existing read-file test. | Failing. |
| C2 | `feat(api): list_dir handler + route` [green] | `crates/api/src/handlers.rs` (new `list_dir` handler), `crates/api/src/router.rs:133` (new `.route("/v1/sandboxes/{id}/files/list", get(handlers::list_dir::<S>))`), `crates/api/src/service.rs` (new `ListDirQuery {path, cwd}`), `crates/api/src/proxy_client.rs` if a typed helper is wanted. JSON response shape: as in PLAN_LIVE_EDIT.md §`/files/list`. | C1 green. |
| C3 | `test(api): list_dir entries cap at 5000 and propagate truncated` [red→green together] | Same test file. | Green. |
| C4 | `test(api): POST /wait_port_listening returns {ready,elapsed_ms}` [red] | tests.rs. | Failing. |
| C5 | `feat(api): wait_port_listening handler + route` [green] | `crates/api/src/handlers.rs`, `crates/api/src/router.rs` (`.route("/v1/sandboxes/{id}/wait_port_listening", post(handlers::wait_port_listening::<S>))`), `crates/api/src/service.rs` (`WaitPortListeningRequest {port, timeout_ms}`). | C4 green. |
| C6 | `test(api): read_file response carries X-File-Revision header` [red] | tests.rs — extend existing read-file test. | Failing. |
| C7 | `feat(api): emit X-File-Revision on read_file response` [green] | `crates/api/src/handlers.rs` `read_file` handler (~line 563) consumes the new `FileMeta` server frame from B9 and sets the response header before the body. | C6 green. |
| C8 | `test(api): write_file with stale expected_revision returns 409 with body shape` [red] | tests.rs. Body shape per plan: `{actual_revision, conflicting_content_b64}`. | Failing. |
| C9 | `feat(api): write_file requires expected_revision; 409 on mismatch; ?force=true bypass` [green] | `crates/api/src/service.rs` (`WriteFileRequest` gains required `expected_revision: String`; `force: bool` query param via a new `WriteFileQuery`), `crates/api/src/handlers.rs` `write_file` handler maps `IoError {code: "REVISION_MISMATCH", detail}` from B11 into `409 Conflict` with the JSON body and a fresh `X-File-Revision` of the actual file content. | C8 green. |
| C10 | `refactor(api): share OpenIoStream client-frame helpers across list/read/write` if duplication appears. | `crates/api/src/handlers.rs` or new module. | All api tests green. |

Group C confidence gate:

```
Confidence: high — gateway changes are mechanical translations of the contracts surface.
Residual risks: 409 body's `conflicting_content_b64` field requires the gateway to re-read the file after the agent reports mismatch. Confirm whether a single OpenIoStream session can carry "list → read" or two are needed; latency budget is loose enough either way.
Known gaps: none.
```

---

## Group D — UI (PR #4, depends on C)

Branch: `module/ui-live-edit`. UI gets no Rust-style red-test
discipline gate in this repo's CI, so commits are organized by
user-visible deliverable; manual `pnpm dev` smoke testing is the
gate (per CLAUDE.md global instructions on UI changes).

### D.1 — File tree (~1 day)

| # | Commit | Files | Gate |
|---|---|---|---|
| D1 | `feat(ui): listDir / waitPortListening API clients` | `ui/lib/api.ts` (new `listDir`, `waitPortListening`; revision-aware return shape for `readFile`/`writeFile`). | Type-check passes. |
| D2 | `feat(ui): lazy file tree component (one-level expand)` | `ui/components/file-tree.tsx` (new); `ui/lib/tree-defaults.ts` (new, default exclude patterns: `node_modules`, `.git`, `target`, `dist`, `__pycache__`, `.next`, `.venv`); `Cmd-Shift-H` toggle. | Smoke: open a sandbox, expand `/workspace`, expand a subdir, hidden toggle works. |
| D3 | `feat(ui): tree refresh on write_file ACK invalidates dirname cache` | `file-tree.tsx`, wired from D5 below (deferred until D5 lands). | Smoke: write a new file, tree shows it without manual refresh. |
| D4 | `feat(ui): truncated banner on >5000 entry directories` | `file-tree.tsx`. | Smoke: synthetic test by listing `/usr/bin` or similar. |

### D.2 — Editor swap (~1 day)

| # | Commit | Files | Gate |
|---|---|---|---|
| D5 | `chore(ui): add @uiw/react-codemirror@^4.x pinned to codemirror 6.34.x` | `ui/package.json`, lockfile. **Pin** per plan §Editor decision (iOS selection-handle regression in 6.35+). | `pnpm install` clean. |
| D6 | `feat(ui): CodeMirror editor with tabs, replacing Textarea` | `ui/components/files-panel.tsx` (rewrite — see existing at this path); `ui/components/editor.tsx` (new); `ui/lib/lang.ts` (new lazy lang-pack loader: dynamic-import `@codemirror/lang-python`, `lang-javascript`, etc.); **`useMemo(extensions, [filename, vimEnabled])`** baked in per plan §Memoization gotcha. | Smoke: open file, edit, tab between two files, no cursor-trash on parent re-render. |
| D7 | `feat(ui): Cmd-S save with optimistic dirty-dot indicator` | `editor.tsx` keymap; `files-panel.tsx` tab strip dirty-dot. Filled-dot, not asterisk (matches VS Code; plan §Save model). | Smoke: edit, Cmd-S, dot clears, `Saved` status appears for ~1.5s. |
| D8 | `feat(ui): 5s autosave-on-blur fallback` | `editor.tsx`. | Smoke: edit, click off, wait 5s, file saved. |
| D9 | `feat(ui): IndexedDB unsaved-buffer keyed by {sandboxId, path}` | `ui/lib/unsaved-buffer.ts` (new, wraps `idb` or vanilla IndexedDB). | Smoke: edit, reload tab, prompt to restore. |

### D.3 — Conflict UX (~0.5 day)

| # | Commit | Files | Gate |
|---|---|---|---|
| D10 | `feat(ui): track X-File-Revision per open file; send expected_revision on write` | `files-panel.tsx`, `api.ts`. | Type-check passes. |
| D11 | `feat(ui): 409 conflict banner with [Reload] [Overwrite] [Diff]` | `ui/components/conflict-banner.tsx` (new); diff view can defer to a simple `<pre>` side-by-side initially — full diff renderer not in scope. | Smoke: edit two browser tabs on same file, save in one, save in other → banner appears with all three actions. |
| D12 | `feat(ui): periodic 30s re-stat of active file on visibility` | `files-panel.tsx` — `document.visibilitychange` + `setInterval` (cleared on hide). | Smoke: edit file externally via `open-sandbox ssh`, wait 30s, focus tab, banner appears before user hits save. |

### D.4 — Preview pane (~1 day)

| # | Commit | Files | Gate |
|---|---|---|---|
| D13 | `feat(ui): Preview tab in right-pane with iframe` | `ui/components/right-pane.tsx` (alongside Exec/Files/Info); `ui/components/preview-pane.tsx` (new). `<iframe src={publicUrl}>`. | Smoke: sandbox with a server template; preview renders. |
| D14 | `feat(ui): save chain — write → waitPortListening → cache-bust src` | `files-panel.tsx` save handler. 200ms trailing-edge debounce. | Smoke: edit `app.py`, Cmd-S, preview iframe reloads after watchexec restart. |
| D15 | `docs(ui): note SameSite cookie limitation in dev` | `README.md` Live editor section (per plan §Sharp edges #1). | Read-through. |

### D.5 — Responsive (~0.25 day)

| # | Commit | Files | Gate |
|---|---|---|---|
| D16 | `feat(ui): mobile breakpoint hides tree+editor, full-bleed iframe` | `app/page.tsx` Tailwind `md:` classes; small mobile header with sandbox URL + "view on desktop to edit" hint. | Smoke at <768px viewport. |

### D.6 — Docs (~0.25 day)

| # | Commit | Files | Gate |
|---|---|---|---|
| D17 | `docs: README Live editor section + dev-server template `watchexec --poll 200` note for macOS Docker Desktop VirtioFS` | `README.md`, possibly `docs/dev-guide.md`. | Read-through. |

Group D confidence gate:

```
Confidence: medium.
Residual risks:
  - @uiw/react-codemirror's `EditorView` reinitialization gotcha (plan §Sharp edges #4) — first manifest is cursor-trash on save; mitigate in D6 commit itself.
  - Watchexec restart latency on Docker Desktop / VirtioFS — may need a longer initial waitPortListening timeout (>3000ms) than the plan's number.
Known gaps: full diff renderer for D11 deferred; initial Reload/Overwrite are the must-haves.
```

---

## Cross-cutting deferred work (not in this batch)

Tracked here so they don't get lost; punted per PLAN_LIVE_EDIT.md §Deferred:

- `POST /v1/sandboxes/{id}/files/search` (rg --json wrapper). Build when first user asks.
- `WS /v1/sandboxes/{id}/files/watch` (recursive inotify-over-tunnel). Build only if mtime-on-focus polling proves insufficient.
- `--dev-https` flag on `dev-up.sh` for SameSite mitigation.
- Full diff renderer in the conflict banner.
- Full mobile editor (Replit-style native).

## Tagging at the end

After group D ships and `make ci` is green on `main`:

```
git tag contracts/v1.0.3                          # the v1.0.3 surface
git tag contracts/v1.0.3-frozen                   # surface locked
git tag module/live-edit/done                     # batch complete
```

Update `CLAUDE.md` "Current phase" to point at `contracts/v1.0.3`
and the live-edit batch closure.

## Parallelism windows

| Window | Parallel work allowed |
|---|---|
| During A | Spike work only — no consumers exist yet. |
| During B | After B6 lands, C1..C5 can start (wait_port_listening + list_dir don't depend on revision). |
| During C | After C2 lands, D1..D4 (tree) can start against the gateway. |
| During D | D.1, D.2, D.3, D.4 are loosely orderable; D.3 and D.4 can develop in parallel after D.2 lands the editor host. D.5+D.6 are last. |

## Estimated calendar

Single-developer serial: ~3 days excluding contracts review.
Two-developer split (one on A→B→C, one on D from D1 onward after
C2 ships): ~2 days. The bottleneck is group B (revision plumbing
across both runtimes); fan-out is in D.
