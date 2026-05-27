# PLAN: DX magic — next batch after Phase-0 dev mode

**Status:** scoped, not started. Drafted 2026-05-27 immediately after
`PLAN_DEV_MODE.md` Phase 0 shipped (scripts/dev-up.sh + dev-down.sh + 5
cascade fixes, commits `cd9bbe2..f3d8728`). The 502 trap a real user
hit on the freshly-exposed sandbox URL motivated this — the platform
needs to optimize for three load-bearing moments:

1. **First 30 seconds** — `dev-up.sh` should print a URL that *works*.
2. **First time doing something real** — running a one-off command,
   editing files, sharing a result.
3. **First time sharing** — handing a link to a colleague.

The four features below are ranked by **magic** (wow-per-effort), not
by pure engineering leverage. All four reuse existing infrastructure:
the exec stream, ApiState, ProxyClientPool, and the sandbox-list UI.

---

## #1 — `open-sandbox ssh <id>` — the killer feature (~3 days)

**Magic moment:** `open-sandbox ssh abc123` → you're in.
`code --remote ssh-remote+sandbox-abc123 /workspace` opens VS Code
Remote against an isolated container that the agent dialed out from a
Raspberry Pi on your desk. Same for `git push`, `scp`, `mosh`,
`rsync`. The agent-dials-out architecture only sells itself once
people feel this: no inbound ports, no port forwarding, no NAT —
just `ssh sandbox-abc123` and you're inside.

**Scope:**

- `crates/cli/src/ssh.rs` — new subcommand. Spawns a local Unix socket
  that forwards to a new `WS /v1/sandboxes/{id}/ssh` endpoint, which
  proxies through the existing exec data plane.
- Agent: requires `dropbear` or `openssh-server` in the sandbox image.
  Either make it a sandbox-image requirement (like `kill` already is
  in `SPEC.md`) or gate behind `--with-ssh` so it's opt-in. Agent
  execs `sshd -D -p <random>` and pipes the TCP stream.
- Auth: API key authenticates the WS upgrade. sshd runs with
  `PermitEmptyPasswords yes` because the channel is already
  authenticated one layer up. Alternative: sign an ephemeral SSH cert
  per session.
- CLI plumbing: `open-sandbox ssh <id>` shells out to
  `ssh -o ProxyCommand="open-sandbox ssh-tunnel <id>" sandbox-<id>`
  so users get a real ssh client (with config-file integration,
  keepalive, mosh fallback, …) without us reimplementing it.

**Risks / unknowns:**

- sshd as a sandbox-image requirement is a real ask. The `--with-ssh`
  flag is friendlier but means each sandbox does a one-time
  `apk add openssh-server` (~2-3s overhead).
- The bidirectional byte channel is already proven by exec streaming
  (see `docs/design/EXEC_STREAMING_DESIGN.md`), so the proxy work is
  framing + auth, not transport.
- This is the highest-magic feature on the list but also the highest
  scope. Ship after #3 and #4 so the foundation is solid.

---

## #2 — Live edit + URL preview — "Replit in a tab" (~3-4 days)

**Magic moment:** Left pane: CodeMirror editing `app.py`. Middle pane:
iframe showing the sandbox URL. Right pane: live process logs. Save
the file → process auto-restarts → iframe auto-reloads → see the
change. All in one browser tab. Isolated container. No install.

**Scope:**

- Replace the textarea-based files panel with a CodeMirror 6 editor.
  Add a left-rail file tree fed by a new
  `GET /v1/sandboxes/{id}/files/tree` endpoint (or lazy-load each
  directory via the existing `read-stream`). ~1.5 days.
- Add a "Preview" tab in `right-pane.tsx` alongside Exec/Files/Info,
  rendering `<iframe src={publicUrl}>`. ~1 hour.
- Live-reload: each quickstart template (#4) ships with a tiny
  supervisor — `watchexec --restart -- python app.py` — so file
  saves auto-restart the process. ~half day.
- Auto-refresh the iframe on file save via key-bump (cheapest) or
  postMessage. ~1 hour.

**Risks / unknowns:**

- CodeMirror bundle is ~150KB gzipped — fine on desktop, worth
  feature-gating for mobile.
- HTTPS iframes need a wildcard cert. `*.localtest.me` is plain HTTP
  which works in dev; production needs `*.sandbox.example.com` with a
  matching cert.
- Cross-origin iframe of a `localtest.me:8080` URL on the same origin
  works without CORS contortions.

---

## #3 — `open-sandbox run --image X -- <cmd>` (~1 day)

**Magic moment:** `open-sandbox run --image ubuntu:22.04 -- bash -c
"uname -a && lsb_release -a"`. Three seconds later, output streams in
real time. Sandbox auto-destroys when the command exits. The natural
answer to "is my script Linux-portable?" / "does my build work in a
clean container?" — like `docker run --rm`, but the workload runs on
whatever fleet of agents you've connected.

**Scope:**

- New top-level `Command::Run(RunArgs)` in `crates/cli/src/cli.rs`.
  Args: `--image`, `--env KEY=VAL` (repeatable), `--cpu-millicores`,
  `--memory-bytes`, `--ttl`, `--api-base` (env
  `OPEN_SANDBOX_API_BASE`), `--api-key` (env
  `OPEN_SANDBOX_API_KEY`), `--`, command-and-args.
- `crates/cli/src/run_subcommand.rs` — calls
  `POST /v1/sandboxes` → polls `GET` until `status=running` →
  opens `WS /v1/sandboxes/{id}/exec` → pipes local stdin/stdout/stderr
  to the IoStart/Stdin/Stdout/Stderr frames → on `IoExited`,
  propagates the exit code → `DELETE /v1/sandboxes/{id}` in defer.
- Local Ctrl-C → WS close → already triggers SIGTERM+SIGKILL+cleanup
  via the existing exec-streaming design (spike 03 + ADR-006).

**Risks / unknowns:**

- Cold start is dominated by image pull (5–30s for unfamiliar images,
  sub-second for cached ones). `--warm` flag (keep a pool of common
  images pre-pulled per agent) is a deferred follow-up.
- Output ordering: stderr/stdout interleave correctly via the existing
  exec frames — confirmed by the ws-client crate.
- Exit code propagation: the agent already sends
  `IoExited { exit_code }`; CLI just exits with the same value.

---

## #4 — Quickstart templates + working demo seed (~2-3 hours)

**Magic moment:** First-run banner shows
`Demo  http://abc123.localtest.me:8080  ✓ live` and the URL actually
serves a page. In the Create form, instead of a freeform image input,
a dropdown:

- **Static site** — `python:3.12-alpine`, exec
  `sh -c "cd /tmp && echo hello > index.html && python3 -m http.server 8080"`
- **Node** — `node:20-alpine`, exec
  `node -e "require('http').createServer((q,r)=>r.end('hi')).listen(8080)"`
- **Nginx** — `nginx:alpine`, with a port-80→8080 reconfigure
- **Python web** — `python:3.12-alpine` + tiny Flask boilerplate
- **Plain shell** — `alpine:3.21`, no auto-exec (the current default)

Pick one, hit Create, hit Run, click URL — it works. Removes the
entire "I made a sandbox but it doesn't do anything" failure mode that
just bit us.

**Scope:**

- `ui/lib/templates.ts` — array of
  `{id, label, image, execCommand, exposedPort, description}`.
- `ui/components/sandbox-list.tsx` create form: replace the freeform
  image `<Input>` with a select+input pair (select picks a template,
  input still lets advanced users override). Selecting a template
  stashes the suggested exec command on the sandbox object so
  `ExecTerminal` can prefill it on mount.
- Demo seed: in `scripts/dev-up.sh` (until Phase 1 lands a `dev`
  subcommand), POST a `python:3.12-alpine` sandbox + exec after the
  banner so the printed Demo URL works on first try. When
  `PLAN_DEV_MODE.md` Phase 1 lands, move into `crates/cli/src/dev.rs`
  alongside the existing token-generation / postgres-management code.

**Risks / unknowns:**

- Almost none — pure UX polish. The 502 we just hit *will* keep
  biting first-time users until this ships.
- Image-pull cold start hits first-run; `dev-up.sh` could pre-pull
  the demo image during `cargo build` time as an optimization, but
  that's premature.

---

## Recommended ship order

1. **#4 templates + demo seed** (3h). Foundation. Without it every
   other magic moment starts from "what do I even type?"
2. **#3 `open-sandbox run`** (1d). Markets itself ("docker run, but
   in your fleet"). Pure additive — no existing surface changes.
3. **#1 `open-sandbox ssh`** (3d). The moat. Worth the time once #4
   exists so demos land cleanly.
4. **#2 live edit + preview** (3-4d). Demo-day feature. Only useful
   once templates exist (#4) because the iframe + supervisor pattern
   depends on each template's structure.

## Deferred

- **Public share tunnels** (`<id>.share.open-sandbox.dev`) — needs a
  hosted instance + abuse story + wildcard TLS. Operational lift, not
  technical. Defer until there's an open-sandbox.dev to host on.
- **Live-preview thumbnails in the sandbox list** — cute but not
  magic. Cross-origin iframe sizing is fiddly. Headless-chromium
  screenshot service is heavyweight. Skip.

## Pointer

A condensed version of this plan lives in agent memory at
`memory/dx-magic-roadmap.md` for future sessions; this doc is the
canonical, git-versioned copy.
