# PLAN: `open-sandbox dev` — single-command first-run

**Status:** draft, post-probe (2026-05-27). Three-agent brainstorm converged on the shape; probe pass below resolves the unknowns into concrete decisions.

**Goal:** a developer on macOS or Linux with Docker installed runs **one command** and sees a working sandbox with a console URL printed in the terminal within ~30 seconds.

```sh
cargo run --release --bin open-sandbox -- dev          # from source
open-sandbox dev                                       # post-install
```

Production topology (4 binaries + Postgres + Pulumi) is unchanged. `dev` is a strictly additive subcommand.

---

## Convergent design (from 3-agent brainstorm)

| Concern | Decision |
|---|---|
| Process model | Single supervisor that spawns controller + proxy + api + agent as **in-process `tokio::spawn` tasks**, not subprocesses |
| Storage | Auto-`docker run` `postgres:16-alpine` keyed by a managed container name (see Probe #1 for why not SQLite) |
| Tokens | Auto-generate the 5 env vars on first run, persist to `~/.open-sandbox/dev.env`, re-use on restart |
| UI | Pre-build the Next.js console as a static export, embed via `rust-embed` into the `api` binary, serve at `/console` |
| Subdomain routing | Use `*.localtest.me` (resolves to 127.0.0.1, no `/etc/hosts` edits) |
| Demo sandbox | After all four services are healthy, auto-POST one `alpine:3.21` sandbox so the user sees something concrete |
| Ctrl-C | Cancel all spawned tasks, stop the managed Postgres container (don't delete the volume — restart is fast) |

---

## Probe findings

### #1 — SQL portability (SQLite vs Postgres)

**Result: SQLite is not viable. Use auto-managed `postgres:16-alpine` container.**

The proxy depends on Postgres-specific features that have no clean SQLite analogue:

- `crates/proxy/src/pg_store.rs:9` — `LISTEN`/`NOTIFY` on a `routing_changed` channel, used by `RoutingCache` for real-time invalidation. The 30s periodic refresh is only a fallback.
- `crates/proxy/src/pg_store.rs:101` — functional index `replace(sandbox_id::text, '-', '') text_pattern_ops` on `routing_entries` for subdomain prefix-matching.
- `crates/controller/src/pg_store.rs:116,202,297` — `ON CONFLICT (...) DO UPDATE SET ... EXCLUDED.x` upserts (could be ported, but adds work).
- `crates/controller/src/pg_store.rs:378,492` — `SELECT ... FOR UPDATE` row locking inside scheduler transactions.

Porting to SQLite would mean either re-implementing the LISTEN/NOTIFY pathway with a sidechannel (tokio broadcast across in-process tasks, but doesn't survive a process restart and changes the cache invariants) or a polling-only mode (regresses proxy correctness). The right call is to keep the production storage layer intact and bring up a real Postgres container managed by `dev`.

**Implementation:**

- Use `bollard` (already a workspace dependency via `agent-docker`) to:
  - Pull `postgres:16-alpine` (cached after first run).
  - Run a container named `open-sandbox-dev-pg` bound to `127.0.0.1:15432`, with a named volume `open-sandbox-dev-pg-data` for persistence across restarts.
  - On startup, `docker inspect` first; if already running, reuse. If exists-but-stopped, start. Else create.
  - On Ctrl-C, stop (not remove). Volume survives. `open-sandbox dev --reset` removes the volume.

### #2 — In-process supervision feasibility

**Result: trivially feasible. The work is mostly orchestration code, not refactoring.**

`crates/cli/src/run.rs` already exposes:

- `pub async fn run_controller(args: ControllerArgs) -> Result<...>`
- `pub async fn run_proxy(args: ProxyArgs) -> Result<...>`
- `pub async fn run_api(args: ApiArgs) -> Result<...>`
- `pub async fn run_agent(args: AgentArgs) -> Result<...>`

All four are `async` functions that take args structs and run until shutdown. They use `serve_with_incoming_shutdown` and respond to `shutdown_signal()` — see `crates/cli/src/run.rs:96`.

**Implementation:**

- Add `Command::Dev(DevArgs)` to `crates/cli/src/cli.rs`.
- Add `pub async fn run_dev(args: DevArgs)` in `crates/cli/src/run.rs` that:
  1. Generates tokens, persists to `~/.open-sandbox/dev.env`.
  2. `std::env::set_var` for each token (the run_* functions still read some from env directly: `OPEN_SANDBOX_JOIN_TOKEN`, `TUNNEL_JOIN_TOKEN`, `CONTROLLER_ADMIN_TOKEN`, `OPEN_SANDBOX_INTERNAL_TOKEN`, `OPEN_SANDBOX_API_CORS_ORIGINS=*`).
  3. Brings up the managed Postgres (Probe #1).
  4. `tokio::spawn` each of the four run_* functions; collect `JoinHandle`s.
  5. Polls the api `/healthz` until it returns 200, with a 30s timeout.
  6. POSTs the demo sandbox via the api.
  7. Prints the banner.
  8. Waits on Ctrl-C; on signal, drops the spawned tasks' shutdown channels and joins with a 5s grace, then stops the Postgres container.

One caveat surfaced by the probe: the run_proxy function calls `return Err(...)` if `TUNNEL_JOIN_TOKEN` is unset (`run.rs:200`) and similarly for `CONTROLLER_ADMIN_TOKEN`. The `dev` subcommand must set these *before* spawning, which it does — no code change needed in the services.

### #3 — Next.js console static export

**Result: viable with one config change. No app code changes needed.**

`ui/app/` contains exactly two files: `layout.tsx` and `page.tsx`. `grep` for `'use server'`, `getServerSideProps`, `generateStaticParams`, `export const dynamic` across `ui/{app,components,lib}` returned **zero hits**. There are no route handlers, no server actions, no dynamic segments.

**Implementation:**

- Add `output: 'export'` to `ui/next.config.ts`. (Next.js 14+ replaced the legacy `next export` command with this config flag; the build emits a static `out/` dir.)
- Add a `crates/api/build.rs` (or workspace-level build script) that runs `pnpm install --frozen-lockfile && pnpm build` in `ui/` when the `embedded-console` Cargo feature is on. Gate the feature so day-to-day Rust builds don't require pnpm.
- Use `rust-embed = "8"` to embed `ui/out/` into the api binary.
- Add an axum route `/console/*path` that serves the embedded files; root `/` redirects to `/console/`.
- The UI today derives the API base from `window.location.hostname` — already correct for the embed (same origin, same port).

Tradeoff: cold build slows down by however long `pnpm build` takes (~20–40s typical). Mitigated by the feature gate — `dev` mode and CI release builds turn it on; iterating on Rust does not.

### #4 — Wildcard DNS

**Result: confirmed working.** `dscacheutil -q host -a name foo.localtest.me` returns `127.0.0.1` and `::1`. No setup needed on any user's machine.

The api binary's CORS hardening accepts `OPEN_SANDBOX_API_CORS_ORIGINS=*` (`crates/api/src/router.rs:29`), so the static-served console at `/console` and any other-origin tool can both work. `dev` sets the wildcard automatically; it stays off by default in production.

### #5 — Token wiring

Mixed CLI-flag-and-env: some tokens are clap args with `env =` fallback (`OPEN_SANDBOX_JOIN_TOKEN`, `OPEN_SANDBOX_API_KEY`, `OPEN_SANDBOX_DATABASE_URL`); others are pure `std::env::var` (`TUNNEL_JOIN_TOKEN`, `CONTROLLER_ADMIN_TOKEN`, `OPEN_SANDBOX_INTERNAL_TOKEN`, `OPEN_SANDBOX_API_CORS_ORIGINS`). For `dev`, setting them all via `std::env::set_var` before spawning the in-process tasks is the cleanest path and requires no upstream changes.

### #6 — Runtime feature gate

`docker` vs `youki` is a **compile-time Cargo feature** (`crates/cli/Cargo.toml:7-10`), not runtime-selectable. The `dev` subcommand will be gated to the `docker` feature only on macOS; on Linux it can compile with either. This matches the user mental model: "first-run uses Docker; production Linux uses youki."

---

## Implementation phases

### Phase 0 — quick win (ship this week, no code)

- `scripts/dev-up.sh`: wraps today's 8-line preamble, traps SIGINT, generates a stable dev token, tails `target/release/open-sandbox` logs in one stream.
- `scripts/dev-down.sh`: tears down.
- README.md "Quick start" section gets a 3-line `./scripts/dev-up.sh` block above the existing manual instructions. Manual instructions stay for reference.

### Phase 1 — `dev` subcommand (real fix)

1. Add `Command::Dev(DevArgs)` to `crates/cli/src/cli.rs`. Args:
   - `--postgres-url <URL>` (env `OPEN_SANDBOX_DEV_POSTGRES_URL`) — **BYO Postgres**. If set, `dev` skips the managed container entirely and connects directly. Useful for users running PG via Homebrew (`brew services start postgresql@16`), a Neon dev branch, an existing project's container, or a NixOS service. The schema is created on first connect (`CREATE DATABASE IF NOT EXISTS open_sandbox` then `pg_store.migrate()`).
   - `--reset` — wipe Postgres volume (managed mode only; ignored with `--postgres-url`).
   - `--no-demo` — skip demo sandbox.
   - `--api-port <PORT>` (default 8081, env `OPEN_SANDBOX_DEV_API_PORT`).
2. Add `crates/cli/src/dev.rs`:
   - `resolve_postgres_source(args) -> PostgresSource` — three branches, in priority order:
     1. `--postgres-url` set → `BYO(url)`.
     2. `DATABASE_URL` env set → `BYO(url)` (matches the convention from the no-Docker brainstorm; lets `dev` cooperate with `direnv`/`mise` setups).
     3. Otherwise → `Managed { container: "open-sandbox-dev-pg", port: 15432, volume: "open-sandbox-dev-pg-data" }`.
   - `manage_dev_postgres()` — bollard-driven container lifecycle. Only invoked for the `Managed` branch. For `BYO`, replaced by a `pg_isready`-style `SELECT 1` probe with a 10s deadline + a clear error pointing at the offending URL (with password redacted).
   - `generate_or_load_dev_env()` — `~/.open-sandbox/dev.env` read/write, generates a random 32-byte hex token per slot if absent. The DB URL slot is only written for the `Managed` branch; for `BYO` we don't persist the user's connection string (it's their secret, not ours).
   - `spawn_supervisor()` — `tokio::spawn` controller/proxy/api/agent, return a struct holding their `JoinHandle`s + a `CancellationToken`.
   - `wait_for_api_healthy()` — poll `GET http://127.0.0.1:{port}/healthz` with backoff, 30s deadline.
   - `seed_demo_sandbox()` — POST one alpine sandbox via the api.
   - `print_banner()` — the "magical" output (see template below). The Postgres line reflects the chosen source: `postgres   managed (docker container open-sandbox-dev-pg)` vs `postgres   byo (postgresql://…@host:5432/open_sandbox)`.
3. Wire `Command::Dev` in `main.rs`.

**Why `--postgres-url` matters:** the three-agent no-Docker brainstorm independently converged on this as the smallest, highest-leverage addition. It costs ~1 day to implement, removes the Docker prereq for anyone who already runs Postgres, makes `dev` cooperate with existing dev environments instead of fighting them, and is the same flag the Linux no-Docker path (Phase 4, deferred) will need anyway. Implementing it now means Phase 4 has one fewer thing to invent.

### Phase 2 — embedded console

1. Add `output: 'export'` to `ui/next.config.ts`.
2. Add `crates/api/build.rs` that runs `pnpm build` in `ui/` when `embedded-console` feature is on. Skip cleanly when pnpm is missing — print a warning, don't break the build.
3. Add `rust-embed` dep + an axum static handler in `crates/api/src/router.rs`.
4. Default the `embedded-console` feature ON for `dev`, OFF for the bare `open-sandbox api` binary in CI so non-UI work doesn't pay the pnpm tax.

### Phase 3 — `curl | sh` installer (later)

Publish prebuilt `open-sandbox` binaries (GitHub Releases). Host `https://get.open-sandbox.dev/install.sh` that downloads the binary for the user's OS/arch into `~/.local/bin`, then suggests `open-sandbox dev` as the next step. Matches the README's stated DoD aesthetically.

---

## Magical terminal output (target)

```
open-sandbox dev — first-run setup (~30s)
  [1/4] starting postgres (docker container open-sandbox-dev-pg) ........ ready
  [2/4] generating dev tokens (~/.open-sandbox/dev.env) ................. ready
  [3/4] spawning controller, proxy, api, agent ......................... ready
  [4/4] creating demo sandbox (alpine:3.21) ............................ ready

  Console     http://127.0.0.1:8081/console
  API         http://127.0.0.1:8081           (key in ~/.open-sandbox/dev.env)
  Demo        sb_abc123                       http://abc123.localtest.me:8080
  Postgres    managed                         (docker container open-sandbox-dev-pg)
  Runtime     docker

  Try:        curl -H "Authorization: Bearer $(cat ~/.open-sandbox/dev.env | grep API_KEY | cut -d= -f2)" \
                   http://127.0.0.1:8081/v1/sandboxes
  Stop:       Ctrl-C  (postgres volume preserved; --reset to wipe)
```

With `--postgres-url postgresql://me:pw@localhost:5432/open_sandbox`, step 1 changes to:

```
  [1/4] connecting to postgres (byo: postgresql://me@localhost:5432/open_sandbox) ... ready
  ...
  Postgres    byo                              (postgresql://me@localhost:5432/open_sandbox)
```

(Password redacted in the banner; full URL only ever lives in the user's shell history / env.)

---

## Sacrifices (all acceptable for `dev`, none affect production)

- **Single failure domain** — one process crashes them all. Fine: dev wants one log stream, not crash isolation.
- **Loopback only** — no LAN visibility unless the user passes `--bind 0.0.0.0`. Already true today.
- **No TLS** — plaintext between services. Matches today's quick-start.
- **Dev tokens on disk at `~/.open-sandbox/dev.env`** — readable only by the user (`chmod 600`). Production never sees this path.
- **`embedded-console` feature adds pnpm to the release build path.** Gated so non-UI workflows (CI, agent-only work) don't pay for it.

## What's not in scope for `dev`

- BYO-worker demo (that's the `curl | sh` installer story, Phase 3).
- youki on macOS (impossible; youki is Linux-only).
- Multi-agent fleet (single in-process agent; `dev --agents=3` could come later if requested).
- Production Pulumi-style provisioning.

---

## Open questions for human review

1. **Container name collision.** If a user already has a `postgres` container on port 15432 from another project, `dev` will conflict. Auto-pick a free port and write it to `dev.env`? Or fail loud and let the user pass `--db-port`? **Recommend: fail loud first with a suggestion to use `--postgres-url` (which sidesteps the managed container entirely); add auto-pick if it still becomes a friction point.**
2. **`~/.open-sandbox/` vs XDG.** Should we use `$XDG_DATA_HOME/open-sandbox/` on Linux? **Recommend: yes, behind `dirs` crate; falls back to `~/.open-sandbox/`.**
3. **Should `dev` block the `youki` feature at compile time?** It's a hard requirement on macOS but optional on Linux. **Recommend: emit a clear error if invoked on a build with only `youki`, suggesting the user rebuild with `--features docker`.**

---

## Files that will change

| Path | Change |
|---|---|
| `crates/cli/Cargo.toml` | Add `bollard`, `rust-embed` (feature-gated), `dirs`, `rand` |
| `crates/cli/src/cli.rs` | Add `Command::Dev(DevArgs)` |
| `crates/cli/src/main.rs` | Wire `Command::Dev => run::run_dev(args).await` |
| `crates/cli/src/run.rs` | Add `pub use crate::dev::run_dev;` |
| `crates/cli/src/dev.rs` | **New.** Supervisor, postgres manager, token loader, banner. |
| `crates/api/Cargo.toml` | Add `embedded-console` feature, `rust-embed` dep |
| `crates/api/build.rs` | **New.** Runs `pnpm build` in `ui/` when feature is on |
| `crates/api/src/router.rs` | Add `/console/*` static handler under `cfg(feature = "embedded-console")` |
| `ui/next.config.ts` | Add `output: 'export'` |
| `scripts/dev-up.sh`, `scripts/dev-down.sh` | **New.** Phase 0 quick-win bridge |
| `README.md` | New "Try it in 30 seconds" section above existing Quick start |
| `docs/plans/PLAN_DEV_MODE.md` | This file |
