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

**Streaming exec:** sandbox stdin/stdout/stderr ride a stream-shaped session on the proxy's data plane (`SandboxIoService.OpenIoStream`), exposed to clients as WebSocket (`WS /v1/sandboxes/{id}/exec`). The connection IS the lifetime — closing the WebSocket sends `SIGTERM` + 5s grace + `SIGKILL`. See [`EXEC_STREAMING_DESIGN.md`](EXEC_STREAMING_DESIGN.md) for the ADR.

## Status

- **Frozen wire shape:** `contracts/v1.0.0-frozen` (2026-05-23)
- **Current released contracts:** `contracts/v1.0.1`
- **In-progress on `main`:** `contracts/v1.0.2` amendment, item #13 landed (`PullPolicy` + warm-startup optimization arc — see [`CHANGELOG.md`](CHANGELOG.md))
- 86 macOS unit tests + 31 Linux youki tests green on `main`

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
| `crates/agent-youki/` | youki/libcontainer runtime impl (production) |
| `crates/ws-client/` | Rust SDK for the WebSocket exec API |
| `crates/cli/` | The `open-sandbox` binary — bundles all subcommands |
| `infra/` | Pulumi stack (TypeScript) and end-to-end shell scenarios |
| `spikes/` | Time-boxed investigations with `RESULT.md` write-ups |
| `SPEC.md`, `SAD.md`, `PLAN.md` | Functional spec, architecture doc, decomposition plan |
| `CONTRACTS.md` | Prose companion to the contracts crate |

## Quick start

### Run the dev fleet locally (macOS/Linux, requires Docker)

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

./target/release/open-sandbox controller --database-url "$DBURL"  &
./target/release/open-sandbox proxy      --database-url "$DBURL"  &
./target/release/open-sandbox api        --controller-url http://127.0.0.1:50051 --proxy-url http://127.0.0.1:50053 &
./target/release/open-sandbox agent      --controller-url http://127.0.0.1:50051 --proxy-url http://127.0.0.1:50052 &

# Create a sandbox
curl -X POST -H "Authorization: Bearer dev-api-key" -H 'content-type: application/json' \
     -d '{"image":"alpine:3.21"}' \
     http://127.0.0.1:8081/v1/sandboxes
```

### Build/test the youki runtime (Linux only — runs inside a dev container on macOS)

```sh
docker compose -f crates/agent-youki/docker-compose.dev.yml up -d
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

## Engineering discipline

This repo follows a strict spec → architecture → contracts → decomposition → per-module TDD flow. See [`ENGINEERING_DISCIPLINE.md`](ENGINEERING_DISCIPLINE.md) for the rules, and [`PLAN.md`](PLAN.md) for the binary-decomposition DAG. Every module has a `module/<name>/{red,green,refactored,e2e-mock,live-verified}` tag trail:

```sh
git tag --list 'module/*' | grep 'live-verified'   # what's live-verified
```

## Reading order for new contributors

1. [`VISION.md`](VISION.md) — the one-paragraph "why"
2. [`SPEC.md`](SPEC.md) — functional + non-functional requirements
3. [`SAD.md`](SAD.md) — 30k-ft → 10k-ft → per-component zoom
4. [`CONTRACTS.md`](CONTRACTS.md) — wire types + cross-cutting policies
5. [`CHANGELOG.md`](CHANGELOG.md) — what's changed in each contracts version
6. [`EXEC_STREAMING_DESIGN.md`](EXEC_STREAMING_DESIGN.md) — the v1.0 ADR for the exec/file-ops data plane
7. [`infra/e2e/scenarios/`](infra/e2e/scenarios/) — runnable end-to-end scenarios

## License

Not specified yet — treat as proprietary until a `LICENSE` file is added.
