# Open Sandbox

Isolated, publicly-accessible OCI sandbox environments where **agents dial out** to a central controller/proxy over TLS — so any machine with outbound internet (cloud VM, laptop, Raspberry Pi) can join the worker fleet with a single command.

Done means: a developer runs `curl ... | sh` on any internet-connected machine, it registers as a worker, and serves sandboxes at `<id>.sandbox.example.com` within seconds — with the entire infrastructure deployable to any cloud from a single Pulumi stack costing under $20/month at the default scale.

## Architecture

```
                ┌─────────────────────────────────────────────┐
                │           Open Sandbox Platform              │
                │                                             │
End users ──HTTPS──► [Proxy] ────routing────► [Agent] ──► [Sandbox Container]
                │     ▲                          │
                │     │                          │
                │  TLS term                OCI runtime
                │  Host routing            (in-process, daemonless)
                │                                             │
AI agents ──REST──► [API Gateway] ──gRPC──► [Controller] ◄──gRPC──── [Agent]
Operators ──REST──►      │                     │
                │     ▼                       ▲
                │  [Postgres]            outbound TLS
                │  [Object Storage]      (no inbound)
                │                                             │
BYO devs ──install──► [Agent on their machine]               │
                └─────────────────────────────────────────────┘
```

**Trust model:** agents authenticate with join tokens at registration; the controller is the authoritative source of sandbox-to-agent mapping. Agents never have inbound listening ports — every connection is outbound, collapsing the networking problem to "can you reach port 443 outbound?"

**Container runtime:** [youki](https://github.com/youki-dev/youki)/libcontainer in-process for production (zero daemon overhead, ~250 MB savings per VM vs the Docker daemon). Docker Engine runtime available as a development fallback on macOS.

**Streaming exec:** sandbox stdin/stdout/stderr ride a stream-shaped session on the proxy's data plane (`SandboxIoService.OpenIoStream`), exposed to clients as WebSocket (`WS /v1/sandboxes/{id}/exec`). The connection IS the lifetime — closing the WebSocket sends `SIGTERM` + 5s grace + `SIGKILL`. See [`docs/design/EXEC_STREAMING_DESIGN.md`](docs/design/EXEC_STREAMING_DESIGN.md) for the ADR.

## Status

- **Frozen wire shape:** `contracts/v1.0.0-frozen` (2026-05-23)
- **Current contracts version on `main`:** `contracts/v1.0.2` — item #13 (`PullPolicy` + warm-startup optimization arc) shipped 2026-05-26; subsequent v1.0.2 additions: WebSocket subprotocol auth (browsers), opt-in CORS, fail-closed-on-empty API key, Pause/Unpause sandbox lifecycle. Items #1–#12 pending a separate session (see [`docs/plans/PLAN_CONTRACTS_v1.0.2.md`](docs/plans/PLAN_CONTRACTS_v1.0.2.md))
- **Tag note:** the `contracts/v1.0.2` git tag points at the initial v1.0.2 commit (`0e68177`, pre-#13); tag movement deferred until #1–#12 ship
- 154 unit tests green on `main` across contracts/agent/agent-docker/api/controller; 31 Linux youki tests green

## Repository layout

| Path | What's there |
|---|---|
| `proto/` | Protocol buffers — the wire schemas |
| `crates/contracts/` | Shared types crate. Source of truth for messages, errors, constants |
| `crates/controller/` | Agent registration, heartbeat monitoring, scheduler, routing-table writes |
| `crates/proxy/` | TLS termination, host-header routing, reverse-tunnel management |
| `crates/api/` | REST/WebSocket gateway. Translates HTTP ↔ gRPC, owns the public surface |
| `crates/agent/` | Agent core: tunnel client, sandbox manager, exec session driver |
| `crates/agent-docker/` | Docker Engine runtime impl (dev fallback) |
| `crates/agent-youki/` | youki/libcontainer runtime impl (production) — includes a Linux dev-container compose so the agent builds + runs from a macOS host |
| `crates/ws-client/` | Rust SDK for the WebSocket exec API |
| `crates/cli/` | The `open-sandbox` binary — bundles all subcommands. Cargo features: `docker` (default), `youki` (Linux only) |
| `ui/` | Next.js 16 dev console (React 19 + Tailwind v4 + xterm.js + lucide icons). Lists / creates / deletes sandboxes, streams exec, reads + writes files |
| `ui/legacy/index.html` | Original single-file vanilla-HTML console — kept as the simplest possible reference client for the wire API |
| `infra/` | Pulumi stack (TypeScript) and end-to-end shell scenarios |
| `spikes/` | Time-boxed investigations with `RESULT.md` write-ups |
| `SPEC.md`, `SAD.md`, `CONTRACTS.md` | Functional spec, architecture doc, contracts prose |
| `docs/plans/` | Decomposition + amendment plans (PLAN, PLAN_EXEC_STREAMING, PLAN_CONTRACTS_v1.0.2, CODE_REVIEW_PLAN) |
| `docs/design/` | Architectural decision records (EXEC_STREAMING_DESIGN) |
| `docs/reviews/` | Review log, follow-ups, open questions |

## Quick start

### Try it in 30 seconds (macOS/Linux, requires Docker)

```sh
./scripts/dev-up.sh        # build (first run), generate dev tokens, spawn all 4 services, tail one log stream
./scripts/dev-down.sh      # stop services + managed postgres (volume preserved)
./scripts/dev-down.sh --reset   # also wipe the postgres volume
```

First run generates `~/.open-sandbox/dev.env` (chmod 600) with stable tokens and brings up a managed `postgres:16-alpine` container at `127.0.0.1:15432`. Subsequent runs re-use both. Ctrl-C stops the services; the postgres container is left running so restart stays fast. This is the Phase 0 shell-wrapper bridge to the future `open-sandbox dev` subcommand — see [`docs/plans/PLAN_DEV_MODE.md`](docs/plans/PLAN_DEV_MODE.md).

### Run the dev fleet manually (full control)

```sh
cargo build --release --bin open-sandbox

# Postgres
docker run -d --name os-pg \
  -e POSTGRES_DB=open_sandbox -e POSTGRES_PASSWORD=test \
  -p 15432:5432 postgres:16-alpine

# Four components in separate terminals (or backgrounded)
DBURL="postgres://postgres:test@127.0.0.1:15432/open_sandbox"
export CONTROLLER_ADMIN_TOKEN=dev-admin \
       TUNNEL_JOIN_TOKEN=dev-tunnel \
       OPEN_SANDBOX_JOIN_TOKEN=dev-join \
       OPEN_SANDBOX_INTERNAL_TOKEN=dev-internal \
       OPEN_SANDBOX_API_KEY=dev-api-key

# Apply schema migrations once (idempotent). Production deploys do this
# step separately so a migration failure doesn't crash-loop the services;
# dev environments can alternatively pass --auto-migrate on controller/proxy.
./target/release/open-sandbox migrate --database-url "$DBURL"

./target/release/open-sandbox controller --database-url "$DBURL"  &
./target/release/open-sandbox proxy      --database-url "$DBURL"  &
./target/release/open-sandbox api        --controller-url http://127.0.0.1:50051 --proxy-url http://127.0.0.1:50053 &
./target/release/open-sandbox agent      --controller-url http://127.0.0.1:50051 --proxy-url http://127.0.0.1:50052 &

# Create a sandbox (returns sandbox_id, subdomain, agent_id, status="creating")
curl -X POST -H "Authorization: Bearer dev-api-key" -H 'content-type: application/json' \
     -d '{"image":"alpine:3.21"}' \
     http://127.0.0.1:8081/v1/sandboxes

# Pause / resume a running sandbox (v1.0.2)
curl -X POST -H "Authorization: Bearer dev-api-key" \
     http://127.0.0.1:8081/v1/sandboxes/<id>/pause      # → 202 {"status":"pausing"}
curl -X POST -H "Authorization: Bearer dev-api-key" \
     http://127.0.0.1:8081/v1/sandboxes/<id>/unpause    # → 202 {"status":"unpausing"}
```

### SSH into a sandbox

`open-sandbox ssh` shells out to the local `ssh` client with a
`ProxyCommand` that pipes through the streaming exec WebSocket — no
inbound ports, no port forwarding. First connect auto-installs
`openssh-server` inside the sandbox (~3s); subsequent connects are
~200ms.

```sh
export OPEN_SANDBOX_API_KEY=…
open-sandbox ssh <sandbox-id>                       # interactive shell
open-sandbox ssh <sandbox-id> -- uname -a           # one-shot command
open-sandbox ssh <sandbox-id> --no-install          # skip auto-install (pre-baked images)
```

`scp`, `rsync`, `git push`, and VS Code Remote-SSH work via the
same ProxyCommand pattern — export `OPEN_SANDBOX_API_KEY` in your
shell, then:

```sh
scp -o ProxyCommand='open-sandbox ssh-pipe <id>' \
    file root@<id>:/tmp/

code --remote ssh-remote+<id> /workspace   # with the snippet below
```

If you want `ssh <id>` (and `scp <id>:…`, `code --remote …`) to
work without re-typing `-o ProxyCommand` every time, add a matching
block to `~/.ssh/config`. Pick whatever Host pattern fits — e.g.:

```
Host *.sb
    ProxyCommand sh -c 'exec open-sandbox ssh-pipe "${1%.sb}"' _ %h
    User root
    StrictHostKeyChecking no
    UserKnownHostsFile /dev/null
```

Then `ssh <id>.sb`, `scp file <id>.sb:/tmp/`, etc. all work.

### Web console (`ui/`)

The repo ships a Next.js 16 dev console:

```sh
# Allow the console (8090) to call the API (8081) cross-origin
OPEN_SANDBOX_API_CORS_ORIGINS='*' \
    ./target/release/open-sandbox api ...

# Run the console (binds 0.0.0.0:8090 — reachable from your LAN)
cd ui && pnpm install && pnpm dev
```

Then open `http://127.0.0.1:8090` (or `http://<your-LAN-IP>:8090` from another device — the API base auto-derives from `window.location.hostname`).

Features:
- Sandbox list with status badges, **Pause / Resume / Delete** actions (lucide icons)
- Four-tab right pane: **Exec** (xterm.js + streaming WS), **Edit** (v1.0.3 live-edit: lazy file tree + CodeMirror 6 tabbed editor + preview iframe with save-chain reload), **Files** (legacy unary read/write), **Info** (raw JSON)
- React confirm dialogs (replaces `window.confirm`), settings drawer for the API key
- Mobile-friendly: hamburger drawer ≤lg, every interactive component verified at 390×844

#### Live edit (v1.0.3)

Open the **Edit** tab on a running sandbox: three columns — file tree on the
left (lazy one-level expansion, `node_modules` and `.git` hidden by default;
toggle with `⇧⌘H`), CodeMirror 6 editor in the middle (per-file tabs;
`⌘S` saves, 5s blur-autosave fires if you click away with unsaved changes),
preview iframe on the right (cache-busted reload after every save, gated on
the agent's `wait_port_listening` so the preview waits for `watchexec` to
restart your dev-server before re-fetching).

Optimistic-concurrency conflict UX is built in: if the file changed on the
agent since you opened it (another `open-sandbox ssh` session, a `git pull`,
etc), the save returns 409 and the editor shows a Reload / Overwrite banner.
Unsaved edits survive a browser refresh via IndexedDB (per-`{sandboxId,
path}`).

Default preview port is `8080` — matching the platform-wide
`DEFAULT_SANDBOX_EXPOSED_PORT`. Sandboxes that bind a different port (e.g.
Next.js on `3000`) currently still preview correctly via the public URL,
but the save-chain's `wait_port_listening` probes `8080` — to be plumbed
through `Sandbox.exposed_port` in a v1.0.4 amendment
(see `docs/reviews/FOLLOWUPS_v1.0.3.md` finding #21).

**Dev-mode caveat — SameSite cookies.** When the UI runs on
`localhost:8090` and the sandbox preview iframe loads
`<id>.localtest.me:8080`, those are different origins on HTTP. Modern
browsers (Chrome since 80 / Feb 2020) reject `SameSite=None` cookies
without `Secure`, and the `Secure` localhost exception covers only
`localhost`, not arbitrary `.localtest.me` hosts. Sandbox templates
that use cookies for sessions will reset on every iframe reload in dev.
Production HTTPS deployments are unaffected. A `--dev-https` flag on
`dev-up.sh` that issues `*.localtest.me` certs via `mkcert` would close
the gap — tracked as a deferred follow-up.

`OPEN_SANDBOX_API_CORS_ORIGINS` accepts a comma-separated list (sole `*` = wildcard). Unset → no CORS layer (production default). Browser WS upgrades authenticate via `Sec-WebSocket-Protocol: open-sandbox.v1, bearer.<base64url(key)>` — see [`CONTRACTS.md § WebSocket auth`](CONTRACTS.md).

A single-file vanilla-HTML version of the same console lives at [`ui/legacy/index.html`](ui/legacy/index.html) — useful as the simplest possible reference client for the wire API (no Node toolchain required).

### Run the dev fleet with the youki agent (Linux runtime, daemonless)

The default `open-sandbox` binary on macOS builds with `--features docker`. To run the **youki** runtime — daemonless libcontainer, cgroup-v2 freezer for pause/unpause — the agent must run on Linux. The repo ships a Linux dev container; bring it up, build the youki-feature binary inside it, and have it dial out to the host's controller / proxy:

```sh
# 1. Start the dev container (also brings up a postgres sidecar; we use the host's instead)
docker compose -f crates/agent-youki/docker-compose.dev.yml up -d

# 2. Build the youki-feature binary inside the container (~9 min cold; seconds on rebuilds)
docker compose -f crates/agent-youki/docker-compose.dev.yml exec dev \
    cargo build --release --bin open-sandbox \
        --no-default-features --features youki

# 3. Stop the host's docker-runtime agent (keep controller/proxy/api up)
pkill -TERM -f 'open-sandbox agent'

# 4. Run the youki agent inside the container, dialing the host
docker compose -f crates/agent-youki/docker-compose.dev.yml exec -d dev \
    sh -c 'OPEN_SANDBOX_JOIN_TOKEN=dev-join \
           TUNNEL_JOIN_TOKEN=dev-tunnel \
           /build/target/release/open-sandbox agent \
             --controller-url http://host.docker.internal:50051 \
             --proxy-url      http://host.docker.internal:50052 \
           > /tmp/agent.log 2>&1'

# 5. Verify registration
docker compose -f crates/agent-youki/docker-compose.dev.yml exec dev \
    grep -E 'runtime|registered' /tmp/agent.log
```

The agent's log will show `runtime: "youki"` and `registered with controller`. New sandboxes created via the REST API or the web console now land on the youki agent. Pause/unpause uses libcontainer's cgroup-v2 freezer.

### Test the youki runtime crate in isolation

```sh
docker compose -f crates/agent-youki/docker-compose.dev.yml exec dev \
    cargo test -p open-sandbox-agent-youki -- --nocapture
```

### Benchmark warm-path sandbox creation (youki)

```sh
docker compose -f crates/agent-youki/docker-compose.dev.yml exec dev \
    cargo run --release --example bench_create_and_start -p open-sandbox-agent-youki
```

See the example's module doc for methodology + how to compare against the docker-runtime numbers from `infra/e2e/scenarios/`.

### Deploy to a cloud (Hetzner default, AWS supported)

```sh
cd infra
pulumi config set platform:cloud hetzner   # or aws
pulumi up
```

Single Pulumi stack provisions controller VM + worker VMs + DNS + TLS cert. Cost: <$20/month at default scale.

## Environment variables

The runtime services (`controller`, `proxy`, `api`, `agent`) are configured entirely from the environment. Copy [`.env.example`](.env.example) to `.env`, fill in the required values, and source it before launching a service.

Required variables fail-closed — services refuse to start if a required token or database URL is missing. The full surface, organized by service, lives in [`.env.example`](.env.example); the most-asked questions:

- **Where do I generate tokens?** `openssl rand -hex 32` for every `CHANGE_ME` line.
- **Which `OPEN_SANDBOX_PROXY_URL` do I set?** Different per service. The **api gateway** points at the proxy's **internal** listener (default `50053`); the **agent** points at the proxy's **public** listener (default `50052`). Crossing them gets you `Unimplemented` at the proxy's role gate.
- **Why does the proxy need `OPEN_SANDBOX_INTERNAL_TOKEN` even in split-listener mode?** Defense in depth. The internal listener should already be network-isolated to the api gateway's segment; the bearer token covers you if that isolation is misconfigured.
- **Which variables are read by which binary?** Each section of `.env.example` is headed by the service that consumes it; cross-service variables (the join tokens, the database URL, the internal token) are noted explicitly.

The source of truth for the variable surface is `crates/cli/src/cli.rs` (clap `env =` attributes) and direct `std::env::var(...)` reads in `crates/cli/src/run.rs`.

## Engineering discipline

This repo follows a strict spec → architecture → contracts → decomposition → per-module TDD flow. See [`ENGINEERING_DISCIPLINE.md`](ENGINEERING_DISCIPLINE.md) for the rules, and [`docs/plans/PLAN.md`](docs/plans/PLAN.md) for the binary-decomposition DAG. Every module has a `module/<name>/{red,green,refactored,e2e-mock,live-verified}` tag trail:

```sh
git tag --list 'module/*' | grep 'live-verified'   # what's live-verified
```

## Reading order for new contributors

1. [`VISION.md`](VISION.md) — the one-paragraph "why"
2. [`SPEC.md`](SPEC.md) — functional + non-functional requirements
3. [`SAD.md`](SAD.md) — 30k-ft → 10k-ft → per-component zoom
4. [`CONTRACTS.md`](CONTRACTS.md) — wire types + cross-cutting policies
5. [`CHANGELOG.md`](CHANGELOG.md) — what's changed in each contracts version
6. [`docs/design/EXEC_STREAMING_DESIGN.md`](docs/design/EXEC_STREAMING_DESIGN.md) — the v1.0 ADR for the exec/file-ops data plane
7. [`infra/e2e/scenarios/`](infra/e2e/scenarios/) — runnable end-to-end scenarios

## License

Licensed under the [Apache License, Version 2.0](LICENSE). Copyright © 2026 Swift Ugandan.
