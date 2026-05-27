# PLAN: live edit + URL preview ("Replit in a tab")

**Status:** scoped, not started. Drafted 2026-05-27 after shipping
PLAN_DX_MAGIC.md #3 (`open-sandbox run`) and #1 (`open-sandbox ssh`),
commits `d45e62b..5ccbaf8`. This is the last feature in the DX magic
batch — #4 (templates) and #3 (run) and #1 (ssh) are already on `main`.

## Magic moment

Three-pane horizontal layout. Left (200px): file tree of the sandbox's
working dir, lazily expanded. Center (flex): CodeMirror 6 editor with
tabs; Cmd-S saves; tab dirty-dot clears on save-ACK. Right (flex):
`<iframe src={publicUrl}>` showing the sandbox's HTTP. Bottom (collapsible
drawer spanning editor+preview): the existing exec terminal. User edits
`app.py`, hits Cmd-S, `watchexec` inside the sandbox restarts the
process, the UI waits for the in-container port to be listening again,
the iframe refreshes — round trip ~600 ms p50, ~1500 ms p95. On
narrow viewports (<768 px): editor + tree drop; iframe-only preview
with a "view on desktop to edit" hint.

This shape was validated by spikes against the alternatives — see
"Decisions backed by spikes" below.

## Architecture

```
┌────────── UI (Next.js 16 + React 19) ──────────┐
│  File tree │  Editor (CM6, tabbed)  │ <iframe> │
│            │                         │          │
│            ├─────────────────────────┤          │
│            │     Terminal drawer (collapsible)  │
└──────────┬─────────────────────────────────────┘
           │ HTTP / WS
           ▼
    api gateway
       ├─ /files/list                          NEW
       ├─ /files/read (+X-File-Revision)       updated
       ├─ /files/write_file (expected_revision required) updated
       ├─ /sandboxes/{id}/wait_port_listening  NEW
       └─ /sandboxes/{id}/exec                 existing
```

No backend code change to the proxy or controller. All new contract
surface is on the api gateway + agent runtime trait.

## Decisions backed by spikes

### Editor: CodeMirror 6 via `@uiw/react-codemirror`, pinned to 6.34.x

- **Why not Monaco:** ~4 MB minimum + per-language web workers; Next.js
  Turbopack worker plumbing is a sinkhole.
- **Why not Ace / Lexical / Slate:** Ace is stale, the others aren't
  code editors.
- **Why not "wait for something better":** spike confirmed nothing
  credible exists in late 2026. Every new entrant since 2024 either
  layers on CM6 (Shiki-editor, cmshiki-editor) or doesn't ship
  (EditContext API is Chromium-only; Zed-on-web is explicitly not
  happening; Firebase Studio uses Monaco and isn't extractable).
- **Pin 6.34.x** to dodge the 6.35+ iOS selection-handle regressions
  ([codemirror/dev #1538](https://github.com/codemirror/dev/issues/1538))
  until upstream fixes them. Marijn's dev meta-repo moved off GitHub
  to a self-hosted Forgejo; `@codemirror/view` still ships normal
  cadence on npm — bug-triage friction, not abandonment.
- **Lazy lang-pack loading verified.** Spike build (Next.js 16 Turbopack)
  showed `@codemirror/lang-python` lands in its own 27 KB gz chunk,
  only fetched when a `.py` file is opened. Initial paint with core
  + basicSetup: ~165 KB gz. The ~135 KB estimate in PLAN_DX_MAGIC was
  optimistic; reality is in the same ballpark and acceptable.
- **Memoization gotcha (bake into commit 1):** `useMemo(extensions,
  [filename, vimEnabled])` — passing `extensions={[lang]}` inline
  trashes cursor/scroll/undo on every parent re-render.

### Save model: explicit Cmd/Ctrl-S + 5 s autosave-on-blur fallback

- **Why not aggressive autosave-on-keystroke** (the CodeSandbox /
  StackBlitz default): they bundle in-browser, so the cost of saving
  is zero. We trigger a real in-container process restart via
  `watchexec` on every write. Aggressive autosave = restart-thrashing
  + broken intermediate states visible in the preview iframe.
- **Why the blur fallback:** CodeSandbox issue #4110 keeps relitigating
  the "I edited, clicked off, lost work" failure mode. 5 s blur
  fallback catches it without polluting the save-on-keystroke path.
- **Optimistic UX:** filled-dot dirty indicator on tab title (matches
  VS Code; clearer than asterisk in a dense tab strip); becomes a
  spinner the moment Cmd-S fires; on 2xx, dot clears + `Saved` in
  status bar for ~1.5 s. IndexedDB buffer of unsaved content keyed
  by `{sandboxId, path}` survives reload.

### Preview reload: UI-driven cache-bust after wait-for-port

- **Why not in-process HMR** (StackBlitz / Bolt model): they own the
  runtime (WebContainers). We don't. We can observe file writes (we
  route them) but we cannot observe what the in-container dev-server
  is doing. Bridging HMR through the proxy is a tar pit.
- **Why not `iframe.contentWindow.location.reload()`:** cross-origin
  SecurityError. UI is on `localhost:8090`, sandbox is on
  `*.localtest.me:8080` — always different origins.
- **The actual flow:**
  1. UI: `await writeFile(...)` resolves with file-write ACK.
  2. UI: `await waitPortListening(sandbox_id, port, 3000ms)` — new
     RPC, see contract surface. Returns as soon as the in-container
     port accepts a TCP connect, with `elapsed_ms`.
  3. UI: `iframe.src = ${publicUrl}?__t=${Date.now()}` — cache-bust
     param forces a full reload.
- Trailing-edge debounce on the UI side (200 ms) coalesces saves
  during Cmd-S mashing.

### `wait_port_listening` is trivial — spike validated

Both runtimes already register a `host_port` per sandbox.
`SandboxManager::host_port_for(sandbox_id)` exists at
`crates/agent/src/sandbox.rs:254`. The RPC is ~30 LOC on the agent:

```rust
let host_port = mgr.host_port_for(&sandbox_id)?;
let deadline = Instant::now() + timeout;
loop {
    if tokio::net::TcpStream::connect(format!("127.0.0.1:{host_port}")).await.is_ok() {
        return Ready { elapsed_ms: ... };
    }
    if Instant::now() >= deadline { return NotReady { elapsed_ms: ... }; }
    tokio::time::sleep(Duration::from_millis(50)).await;
}
```

Pure async tokio. No `nsenter`, no `docker exec`, no namespace
traversal. Sub-50 ms per probe.

### File tree: lazy one-level `/files/list`, not `tree?depth=N`

- **Why not depth=N:** the moment someone expands `node_modules` (40k+
  entries) we DoS the agent's `read_dir` budget AND the browser's
  JSON parser. There's no right `N`.
- **Why not exec-shelled `find`:** parsing `find` output is fragile
  (filenames with newlines exist); the agent's `OpenIoStream` already
  has typed `ReadFileParams` / `WriteFileParams`. New `ListDirParams`
  is the structurally-parallel addition.
- **5000-entry response cap** with `truncated: true` flag. Default UI
  exclude patterns (purely client-side, in `ui/lib/tree-defaults.ts`):
  `node_modules`, `.git`, `target`, `dist`, `__pycache__`, `.next`,
  `.venv`. `Cmd-Shift-H` toggles.
- **Refresh story:** on `write_file` ACK for path `P`, UI invalidates
  the cached listing of `dirname(P)` so newly-created files appear
  without manual refresh. Explicit per-folder Refresh button for
  external mutations (e.g. `git pull` over ssh).

### External-mutation reconciliation: opaque `revision` token

- `read` returns `X-File-Revision` (opaque string — server picks the
  implementation; likely `mtime:size` composite, possibly a content
  hash for small files).
- `write_file` REQUIRES `expected_revision` (no backward-compat
  hedge). Escape hatch: `?force=true` query param for scripted bulk
  writes that intentionally last-write-wins.
- Conflict (`409 Conflict { actual_revision, conflicting_content_b64 }`)
  → UI shows a non-modal banner: `[Reload] [Overwrite] [Diff]`.
- Periodic re-stat of the active file every 30 s on visibility surface
  drift before the user hits save.
- No inotify-over-WS in v1 — it's a real engineering project
  (recursive watches, overflow handling, mount-namespace gotchas);
  mtime-on-focus + 409-on-write covers the data-loss case.

### In-container restart trigger: `watchexec`

- Static ~5 MB Rust binary baked into the base image.
- **Non-server templates** (Python script, Go binary, Rust binary):
  start command wrapped — `watchexec --restart --debounce 150ms --
  <user-cmd>`.
- **Dev-server templates** (Vite, Next): the dev server runs as PID 1,
  `watchexec` is not used at runtime — Vite's own HMR handles JS.
  The wildcard subdomain already proxies WS upgrades, so HMR sockets
  ride for free. Our UI-driven reload becomes a redundant no-op
  visual blip in the rare cases HMR doesn't suffice — acceptable.
- **Static-site templates:** no restart. UI cache-bust triggers
  iframe reload directly.

### Mobile policy: preview-only below 768 px

- One hard breakpoint (Tailwind `md`).
- Below 768 px: drop the tree + editor; render iframe full-bleed with
  a small header showing the sandbox URL + a "view on desktop to edit"
  hint.
- Above 768 px: the three-pane layout as designed.
- **Why not full mobile editor:** spike confirmed every serious
  browser editor either (a) builds a native app (Replit, CodeSandbox)
  or (b) tells mobile users "this is a desktop product"
  (vscode.dev, Gitpod, StackBlitz, github.dev). Two months of CM6
  mobile-bug whack-a-mole for a deprioritized use case is the
  wrong trade.

## Contract surface

All net-new in `contracts/v1.0.3` (separate from the pending v1.0.2
amendments backlog in `PLAN_CONTRACTS_v1.0.2.md`).

| # | Endpoint | Direction | Notes |
|---|---|---|---|
| 1 | `GET /v1/sandboxes/{id}/files/list?path=&cwd=` | new | One-level dir entries. Caps at 5000 with `truncated` flag. New `ListDirParams` variant in `IoStart::Params`. JSON shape below. |
| 2 | `read` response: `X-File-Revision` header | updated | Opaque string. Server picks implementation. |
| 3 | `write_file` body: REQUIRED `expected_revision` (+ optional `?force=true` query) | updated | `409 Conflict { actual_revision, conflicting_content_b64 }` on mismatch. |
| 4 | `POST /v1/sandboxes/{id}/wait_port_listening` | new | Body: `{port, timeout_ms}`. Returns `{ready: bool, elapsed_ms: u32}`. Agent TCP-polls `127.0.0.1:<host_port>` from the host. |
| 5 | `POST /v1/sandboxes/{id}/files/search` | **deferred** to v1.1 | Wraps `rg --json`. Specced, not built. |
| 6 | `WS /v1/sandboxes/{id}/files/watch` | **deferred** post-v1.1 | Recursive inotify-over-tunnel. Only if mtime-on-focus polling proves insufficient. |

### `/files/list` JSON shape

```json
{
  "path": "/workspace",
  "entries": [
    { "name": "src",          "type": "dir",     "size": null, "revision": "1716800123:0",    "mode": "0755" },
    { "name": "README.md",    "type": "file",    "size": 4231, "revision": "1716800200:4231", "mode": "0644" },
    { "name": "node_modules", "type": "dir",     "size": null, "revision": "1716800100:0",    "mode": "0755" },
    { "name": "logs",         "type": "symlink", "size": 16,   "revision": "1716800100:16",   "mode": "0777", "target": "/var/log" }
  ],
  "truncated": false,
  "total_entries": 4
}
```

`type` collapses kernel-level zoo (FIFO, socket, device) into `other`
so the UI doesn't enumerate them.

## Sharp edges

1. **SameSite cookies in HTTP dev** — sandbox templates that use
   cookies for sessions will reset on every iframe reload in dev
   (Chrome since 80 / Feb 2020 rejects `SameSite=None` without
   `Secure`; the `Secure` exception covers `localhost` only, not
   `<id>.localtest.me:8080`). The "dev-mode proxy rewrites Set-Cookie"
   idea is **NOT viable** in HTTP dev — the rewritten cookie is dropped
   by the browser before the application sees it. **Mitigation:**
   document the limitation; production HTTPS path is unaffected.
   **Optional follow-up:** `--dev-https` flag on `dev-up.sh` that
   uses `mkcert` to issue `*.localtest.me` certs. Deferred unless
   real users hit this.
2. **VirtioFS inotify on macOS Docker Desktop** — local-dev only.
   Production agents run on Linux + ext4 where inotify is native.
   Document `watchexec --poll 200` workaround in the local-dev
   README.
3. **Turbopack chunk naming** is unfriendly to manual inspection
   (random-string IDs, not source-based). Enable
   `next-bundle-analyzer` if we need to track CM growth over time.
   Not blocking.
4. **`@uiw/react-codemirror` re-creates `EditorView` on `extensions`
   identity change** (the memoization gotcha). Bake `useMemo` into
   commit 1.

## Milestones (~3-4 days)

1. **`/files/list` endpoint + UI lazy file tree** (~1 day)
   - `ListDirParams` variant in `crates/contracts/proto/proxy.proto`
   - Agent handler in `crates/agent/src/io_stream.rs`
   - api gateway route + JSON marshalling
   - UI tree component with expand/collapse, 5000-cap rendering,
     default exclude patterns toggle
2. **Editor swap to CodeMirror 6** (~1 day)
   - Replace `<Textarea>` in `files-panel.tsx` with `<CodeMirror>`
   - Tabs across top of editor pane
   - Lazy lang-pack loader (`ui/lib/lang.ts`)
   - Cmd-S save handler with optimistic dirty-dot
   - 5 s autosave-on-blur fallback
   - IndexedDB buffer keyed by `{sandboxId, path}`
3. **Conflict detection** (~0.5 day)
   - `X-File-Revision` header on `read` response
   - `expected_revision` required on `write_file`; `?force=true`
     escape hatch
   - 409-handling banner in UI with `[Reload] [Overwrite] [Diff]`
4. **`wait_port_listening` RPC + preview pane** (~1 day)
   - Agent RPC: ~30 LOC against `SandboxManager::host_port_for`
   - api gateway route
   - Preview pane (`<iframe>`) in `right-pane.tsx` alongside
     Exec/Files/Info
   - UI save-handler chains: write → wait_port → cache-bust src
5. **Mobile breakpoint + responsive layout** (~0.25 day)
   - Tailwind `md:` breakpoint
   - Below 768 px: hide tree + editor, render iframe full-bleed +
     "view on desktop" notice
6. **Docs** (~0.25 day)
   - README "Live editor" section
   - SameSite cookie limitation noted in dev guide

## Deferred (out of scope for this batch)

- **Search across files** (`POST /files/search` wrapping `rg --json`).
  Specced as item #5 in the contract table. Build when first user
  asks.
- **`WS /files/watch`** for live external-mutation reflection in the
  tree. Only if mtime-on-focus polling proves insufficient in real
  usage.
- **`--dev-https` flag** to eliminate the SameSite limitation in dev.
- **Native HMR forwarding optimization** — currently relies on Vite
  HMR riding the wildcard-subdomain WS upgrade path. If our UI-driven
  reload + HMR end up both firing for the same save, optimize then.
- **Full mobile editor** — see mobile-policy rationale above.

## Pointer

Spike findings (CM6 + Next.js 16 build verification, agent
`host_port` discovery, SameSite cookie investigation, alternative-editor
landscape) live in the session log for `5ccbaf8`. The contract surface
items folded back into `PLAN_CONTRACTS_v1.0.2.md` as the v1.0.3
preview when this plan starts shipping.
