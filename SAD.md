# Software Architecture Document

> Structured top-down. Resist the urge to jump into per-component detail before the higher zoom levels are settled — boundaries drawn at the wrong altitude are expensive to redraw later.

## 30,000-ft view

### Context

```
                    ┌─────────────────────────────────────────────┐
                    │              Open Sandbox Platform           │
                    │                                             │
  End users ──HTTPS──► [Proxy] ─────routing────► [Agent] ──► [Sandbox Container]
                    │     ▲                          │            │
                    │     │                          │            │
                    │  TLS term                  Docker API       │
                    │  Host routing               (local)         │
                    │                                             │
  Operators ──API───► [Controller] ◄──gRPC──── [Agent]           │
                    │     │                          ▲            │
                    │     ▼                          │            │
                    │  [Postgres]              outbound TLS       │
                    │  [Object Storage]        (no inbound)       │
                    │                                             │
  BYO devs ──install──► [Agent on their machine]                 │
                    └─────────────────────────────────────────────┘

  External actors:
  - End users: access sandbox applications via *.sandbox.example.com
  - Operators: manage the platform via CLI/API (deploy, configure, monitor)
  - BYO developers: run agent binary on their own machines
  - Cloudflare: DNS management, DNS-01 challenge for TLS certs
  - Let's Encrypt: TLS certificate authority
  - Docker Engine: container runtime on each worker
```

### Trust boundaries

1. **Internet → Proxy:** Untrusted. TLS terminates here. Proxy validates only that the requested sandbox ID exists in the routing table. No authentication — sandbox applications handle their own auth (NFR-SEC-2).

2. **Agent → Controller:** Semi-trusted. Agents authenticate with join tokens during registration. After registration, the controller trusts commands from authenticated agents (heartbeats, status updates). The controller never trusts an agent's claim about *which* sandboxes it owns — the controller is the authoritative source of sandbox-to-agent mapping.

3. **Controller → Agent:** Trusted. The agent validates the controller's TLS certificate. Commands from the controller (StartSandbox, StopSandbox, Exec, FetchLogs) are executed without further authentication — the TLS channel is the trust boundary.

4. **Agent → Docker Engine:** Trusted. The agent has full access to the Docker socket on its host. Sandboxes are isolated from each other and from the agent via Linux namespaces and cgroups (NFR-SEC-3), but the agent is not isolated from Docker — it is the supervisor.

5. **Proxy → Agent (reverse tunnel):** Trusted after registration. The proxy uses the routing table (written by the controller) to determine which agent serves which sandbox. A compromised proxy could route traffic to the wrong agent, but it cannot forge agent authentication.

### Out-of-band concerns

**Deployment topology (default):**
- 1 controller+proxy VM (2 vCPU, 4 GB, Hetzner CAX11/CX22) with Postgres on local block volume
- 2–5 worker VMs (smallest available, no public IP, egress-only networking)
- 1 floating/elastic IP for the controller VM
- Object storage bucket for Pulumi state and Postgres backups
- Cloudflare for DNS (wildcard A record → controller IP)

**State at rest:**
- Postgres on the controller VM: agent records, sandbox metadata, routing table, hashed join tokens, API keys
- Object storage: Pulumi state, pg_dump backups (6-hour RPO)
- Agent local state: only Docker containers. No durable state on workers — they are cattle, not pets.

## 10,000-ft view

### Components

- **Controller:** owns agent lifecycle (registration, heartbeat monitoring, dead-agent detection), sandbox scheduling (assign sandbox to agent), routing-table writes, join-token management, and the operator API. Speaks gRPC to agents, SQL to Postgres.
- **Proxy:** owns TLS termination, Host-header-based routing, and reverse-tunnel management. Speaks HTTPS to end users, gRPC (reverse tunnel) to agents, SQL (read-only + LISTEN) to Postgres.
- **Agent:** owns sandbox lifecycle on its host (Docker container create/start/stop/remove), heartbeat emission, resource reporting, and reverse-tunnel data forwarding. Speaks gRPC to controller and proxy, Docker API to local engine.
- **Postgres:** owns all durable state. Routing-table changes propagated to proxies via LISTEN/NOTIFY.
- **Contracts crate:** owns all shared types — message schemas, error types, newtypes. Every binary depends on it; no binary depends on another binary's internals.

### Interaction diagram

```
Registration:
  Agent ──RegisterRequest(token, resources, agent_id)──► Controller
  Controller ──[validate token, store in PG, write routing]──► Postgres
  Controller ──RegisterResponse(agent_cert)──► Agent

Heartbeat loop:
  Agent ──Heartbeat(agent_id, resource_report)──► Controller  (every 5s)
  Controller ──HeartbeatAck──► Agent

Sandbox lifecycle:
  Operator ──CreateSandbox(image, config)──► Controller API
  Controller ──[pick agent, write PG, NOTIFY proxy]──► Postgres
  Controller ──StartSandbox(sandbox_id, image, config)──► Agent
  Agent ──[docker create + start]──► Docker
  Agent ──SandboxStatus(sandbox_id, running)──► Controller

Request routing:
  End user ──HTTPS──► Proxy (Host: abc123.sandbox.example.com)
  Proxy ──[lookup abc123 in routing cache]──► (in-memory, fed by PG LISTEN/NOTIFY)
  Proxy ──[open virtual stream on agent's reverse tunnel]──► Agent
  Agent ──[TCP connect to sandbox container port]──► Sandbox
  Sandbox ──response──► Agent ──tunnel──► Proxy ──HTTPS──► End user

Reverse tunnel setup:
  Agent ──OpenTunnel(agent_id)──► Proxy  (second gRPC connection)
  Proxy ──[register tunnel in connection pool]──►
  (tunnel stays open; proxy pushes virtual streams when requests arrive)
```

### Boundary rationale

- **Controller vs Proxy:** Separated because they have different scaling characteristics. The controller is CPU-bound (heartbeat processing, scheduling logic) and scales with agent count. The proxy is IO-bound (request forwarding, tunnel management) and scales with request throughput. In the default deployment they share a VM; splitting them is the first scale-up move.

- **Agent as a single binary:** The agent's responsibilities (Docker management, controller gRPC, proxy tunnel) are tightly coupled — they all need to know about the same set of sandboxes on the same host. Splitting them would create coordination complexity with no benefit.

- **Postgres as the single state store:** At this scale, adding Redis, NATS, or any other stateful service for routing/caching is not justified. Postgres LISTEN/NOTIFY gives us pub/sub for free. The routing table is small (thousands of rows at most) and fits entirely in memory.

- **Contracts crate as the boundary enforcer:** Without it, the controller, proxy, and agent would depend on each other's internal types. The contracts crate is the compilation firewall that makes "one binary at a time" safe.

## Per-component zoom

### Controller

**Responsibility.** The controller is the brain of the platform: it manages the agent fleet, schedules sandboxes onto agents, maintains the authoritative routing table, and exposes an operator API.

**Internal structure.**
- `grpc_server` — tonic gRPC server handling agent streams (Register, Heartbeat, SandboxStatus)
- `scheduler` — picks which agent gets a new sandbox based on available resources
- `agent_registry` — in-memory view of connected agents, backed by Postgres
- `routing_writer` — writes routing-table changes to Postgres and triggers NOTIFY
- `api_server` — HTTP API for operators (create/stop sandbox, list agents, issue join tokens)
- `token_manager` — generates, hashes, validates, and revokes join tokens
- `metrics` — Prometheus endpoint

**State.**
- Persistent (Postgres): agent records, sandbox metadata, routing table, hashed join tokens
- Ephemeral (in-memory): live gRPC stream handles per connected agent, agent health status, scheduler state

**Failure modes.**
- Controller crash: agents detect via broken gRPC stream, enter reconnection backoff. Sandboxes continue running (agent manages Docker independently). New sandbox creation and routing updates are unavailable until controller restarts. State is recovered from Postgres.
- Postgres failure: controller cannot write state. New registrations and sandbox operations fail. Existing agent connections stay alive (heartbeats are processed in-memory) but cannot be persisted. Recovery: restart Postgres, controller reconnects automatically.
- Agent heartbeat timeout: controller marks agent dead, reschedules its sandboxes to other agents, updates routing table.

**Observability surface.**
- Prometheus metrics: `controller_agents_connected`, `controller_sandboxes_active`, `controller_heartbeat_latency_seconds` (histogram), `controller_sandbox_starts_total`, `controller_sandbox_stops_total`, `controller_routing_table_size`
- Structured JSON logs: registration events, sandbox lifecycle events, agent death events, token operations

**Contracts consumed.**
- `AgentMessage` (from agents): Heartbeat, SandboxStatus, ResourceReport
- `RegisterRequest` (from agents): token, resources, agent_id

**Contracts produced.**
- `ControllerCommand` (to agents): StartSandbox, StopSandbox, Exec, FetchLogs
- `RegisterResponse` (to agents): agent_cert or rejection reason
- `RoutingEntry` (to Postgres/proxy): sandbox_id → agent_id mapping
- `ApiResponse` (to operators): sandbox/agent/token CRUD responses

---

### Proxy

**Responsibility.** The proxy is the data plane: it terminates public TLS, routes HTTP requests to the correct agent via reverse tunnels, and manages the tunnel connection pool.

**Internal structure.**
- `tls_acceptor` — TLS termination with wildcard cert, `rustls` based
- `router` — extracts sandbox ID from Host header, looks up in routing cache
- `routing_cache` — in-memory HashMap fed by Postgres LISTEN/NOTIFY
- `tunnel_pool` — manages reverse-tunnel gRPC connections from agents, indexed by agent ID
- `stream_mux` — multiplexes virtual streams over agent tunnels for concurrent requests
- `metrics` — Prometheus endpoint

**State.**
- Persistent: none. The proxy is stateless — all routing data is derived from Postgres.
- Ephemeral (in-memory): routing cache (sandbox_id → agent_id), tunnel connection pool (agent_id → gRPC stream), active virtual streams

**Failure modes.**
- Proxy crash: all sandbox HTTP traffic fails. Agents detect broken tunnel, enter reconnection backoff. Recovery: restart proxy, agents reconnect, routing cache rebuilds from Postgres.
- Routing cache stale: a sandbox is created but the NOTIFY hasn't arrived yet. Request returns 502. Self-healing: cache rebuilds on next NOTIFY or periodic full refresh (every 60s as fallback).
- Tunnel to agent breaks: requests for that agent's sandboxes fail with 502. Agent reconnects and re-establishes tunnel. Proxy detects broken tunnel via gRPC stream error.
- Upstream sandbox timeout: proxy returns 504 after configurable timeout (default: 30s).

**Observability surface.**
- Prometheus metrics: `proxy_requests_total` (by sandbox_id), `proxy_request_duration_seconds` (histogram, by sandbox_id), `proxy_active_tunnels`, `proxy_errors_total` (by type: `routing_miss`, `tunnel_failure`, `upstream_timeout`), `proxy_routing_cache_size`
- Structured JSON logs: request log (method, host, status, duration), tunnel lifecycle events, routing cache refresh events

**Contracts consumed.**
- `RoutingEntry` (from Postgres, written by controller): sandbox_id → agent_id
- `TunnelStream` (from agents): the reverse-tunnel gRPC bidi stream

**Contracts produced.**
- `TunnelRequest` (to agents via tunnel): encapsulated HTTP request for a sandbox
- HTTP responses (to end users): proxied sandbox responses or error pages (502, 504)

---

### Agent

**Responsibility.** The agent is the worker: it manages Docker sandbox containers on its host, maintains connections to both the controller and proxy, and forwards tunneled traffic to local containers.

**Internal structure.**
- `controller_client` — gRPC client maintaining the bidi stream to the controller (registration, heartbeats, receiving commands)
- `proxy_client` — gRPC client maintaining the reverse tunnel to the proxy (receiving forwarded requests)
- `sandbox_manager` — manages Docker container lifecycle via the Docker Engine API
- `tunnel_forwarder` — receives virtual streams from the proxy, connects them to local sandbox ports
- `reconnect_loop` — exponential backoff with jitter for both controller and proxy connections
- `resource_monitor` — reports available CPU/memory to the controller

**State.**
- Persistent: none on the agent itself. Sandbox containers are Docker's state. Agent ID is generated on first run and stored in a local file for reconnection identity.
- Ephemeral (in-memory): list of running sandboxes, controller stream handle, proxy tunnel handle, active forwarded connections

**Failure modes.**
- Controller connection lost: agent enters reconnection backoff. Existing sandboxes continue running. New commands cannot be received. Heartbeats stop — controller will eventually mark agent dead if reconnection takes too long.
- Proxy tunnel lost: agent enters reconnection backoff. Sandbox traffic is unavailable until tunnel is re-established. Sandboxes themselves are unaffected.
- Docker daemon failure: all sandbox operations fail. Agent reports `ResourceReport` with zero capacity. Controller stops scheduling new sandboxes to this agent.
- Agent crash: Docker containers continue running (Docker manages their lifecycle). On restart, agent reconnects and reconciles its sandbox list with Docker's actual state.

**Observability surface.**
- Prometheus metrics: `agent_sandboxes_running`, `agent_tunnel_active`, `agent_controller_connected` (gauge), `agent_forwarded_requests_total`, `agent_docker_errors_total`
- Structured JSON logs: sandbox lifecycle events, connection state changes, Docker operations

**Contracts consumed.**
- `ControllerCommand` (from controller): StartSandbox, StopSandbox, Exec, FetchLogs
- `TunnelRequest` (from proxy): encapsulated HTTP request to forward to a sandbox

**Contracts produced.**
- `AgentMessage` (to controller): Heartbeat, SandboxStatus, ResourceReport
- `RegisterRequest` (to controller): token, resources, agent_id
- `TunnelStream` (to proxy): the reverse-tunnel bidi stream
- `TunnelResponse` (to proxy): encapsulated HTTP response from the sandbox

---

### Postgres

**Responsibility.** Single durable state store for the platform.

**Tables (logical):**
- `agents` — registered agents (id, status, resources, last_heartbeat, token_hash)
- `sandboxes` — sandbox metadata (id, agent_id, image, config, status, created_at)
- `routing` — sandbox_id → agent_id mapping (denormalized from sandboxes for fast proxy lookups)
- `join_tokens` — hashed tokens with scope, TTL, revocation status

**Failure modes.**
- Crash: controller and proxy lose their connection. Controller cannot persist state; proxy falls back to stale routing cache. Recovery: restart, automatic reconnection.
- Data loss: restore from most recent pg_dump backup (6-hour RPO). Agents reconnect and re-register; running sandboxes are reconciled.

**Contracts consumed.** SQL from controller (read/write) and proxy (read-only + LISTEN).

**Contracts produced.** NOTIFY events on routing-table changes. Query results per table schemas.

---

### Contracts Crate

**Responsibility.** Single source of truth for all shared types across binaries.

**Contents:**
- gRPC message types (generated from `.proto` files via `tonic-build`)
- Shared domain newtypes: `AgentId`, `SandboxId`, `JoinToken`, `TenantId` (reserved)
- Error types: `ControllerError`, `ProxyError`, `AgentError` (all `#[non_exhaustive]`)
- Shared constants: heartbeat interval, dead-agent threshold, default timeouts

**Failure modes.** None at runtime — this is a compile-time dependency only.

**Contracts consumed.** None — this is the root of the dependency tree.

**Contracts produced.** Everything consumed by controller, proxy, and agent.

---

## Cross-cutting concerns

### Authentication and authorization

- **Agent → Controller:** Join-token-based registration. Post-registration, the gRPC stream is the implicit session.
- **Operator → Controller API:** API key in `Authorization` header. Keys stored hashed in Postgres. Scoped to the account.
- **End user → Sandbox:** Not the platform's concern. Platform delivers bytes; sandbox apps handle auth.

### Logging, metrics, tracing

- **Logging:** Structured JSON to stdout on all components. `tracing` crate with `tracing-subscriber` JSON formatter.
- **Metrics:** Prometheus exposition format on a dedicated port per component (/metrics endpoint).
- **Tracing:** Deferred. Request IDs are propagated through the proxy→agent→sandbox chain via an `X-Request-Id` header, but distributed tracing (OpenTelemetry) is not in v1 scope.

### Configuration

All components are configured via:
1. CLI flags (highest precedence)
2. Environment variables
3. Config file (TOML, optional)

Configuration shape per component:
- **Controller:** listen address, Postgres connection string, metrics port, heartbeat interval, dead-agent threshold
- **Proxy:** listen address, TLS cert/key paths, Postgres connection string (read-only), metrics port, upstream timeout
- **Agent:** controller URL, proxy URL, join token, agent ID file path, Docker socket path, metrics port

### Versioning and compatibility

- The contracts crate is versioned independently (semver).
- Controller, proxy, and agent binaries embed the contracts crate version they were built against.
- Rolling upgrades: deploy new controller first (it can handle old and new agent messages via `#[non_exhaustive]` enums), then agents, then proxy. Order matters because the controller writes the routing table that the proxy reads.
- Breaking contract changes require all binaries to upgrade in lockstep. This is acceptable at the current scale; at larger scale, version negotiation in the RegisterRequest would be added.

## Architecture decision records

### ADR-001: Agent dials out, never accepts inbound

- **Context:** Workers need to operate behind NAT, firewalls, and residential networks. BYO workers from developer laptops must join the same fleet as managed cloud VMs.
- **Decision:** Agents only make outbound TLS connections (port 443) to the controller and proxy. All traffic — control plane and data plane — flows over these outbound connections.
- **Consequences:** No per-worker public IP, no NAT gateway, no VPN. BYO workers and managed workers use the same code path. The proxy must implement reverse tunneling to forward inbound HTTP requests back through the agent's outbound connection.

### ADR-002: gRPC bidirectional streaming over HTTP/2

- **Context:** Need a protocol for persistent bidirectional communication between agent and controller, and for multiplexed request forwarding between proxy and agent.
- **Decision:** gRPC with bidirectional streaming. HTTP/2 provides native stream multiplexing. `tonic` provides the Rust implementation.
- **Consequences:** Mature library support, native bidi, efficient framing. Avoids custom wire protocols. Trades off against WebSockets (simpler but no built-in multiplexing) and QUIC (blocked by corporate networks).

### ADR-003: Postgres LISTEN/NOTIFY for routing-cache invalidation

- **Context:** The proxy needs a near-real-time view of the routing table (sandbox_id → agent_id). Adding Redis or NATS for pub/sub is extra infrastructure.
- **Decision:** The controller writes routing changes to Postgres. The proxy subscribes via LISTEN and invalidates its in-memory cache on NOTIFY. Fallback: periodic full refresh every 60 seconds.
- **Consequences:** No additional infrastructure. Throughput ceiling of ~hundreds of notifications/second is adequate for routing updates at target scale. If sandbox churn ever exceeds this, the notification layer becomes the bottleneck and must be replaced.

### ADR-004: Single binary with subcommands

- **Context:** Three logical components (controller, proxy, agent) need to be built, distributed, and versioned.
- **Decision:** One binary, three subcommands. The binary is statically linked and self-contained.
- **Consequences:** Simplifies distribution (one download), versioning (one version number), and deployment (same binary everywhere). The tradeoff is binary size — all three components' code ships to every machine even though only one subcommand runs. Acceptable at this scale.

### ADR-005: Cloudflare for DNS regardless of compute cloud

- **Context:** Need DNS management and DNS-01 challenge support for wildcard TLS certs. Each cloud has its own DNS service with different APIs.
- **Decision:** Always use Cloudflare (free tier). Pulumi manages records via the Cloudflare provider.
- **Consequences:** Decouples domain from compute provider. Changing clouds doesn't touch DNS. Clean API for automation. Trades off against native cloud DNS (one fewer account to manage).

### ADR-006: Self-hosted Postgres on controller VM as default

- **Context:** Managed Postgres costs $15–25/month minimum on every cloud, which would double the default deployment cost.
- **Decision:** Run Postgres on the controller VM with a block volume. Backup via pg_dump to object storage. Pulumi flag `managedPostgres: true` for the upgrade path.
- **Consequences:** Total cost stays under $20/month. Operator handles backups (automated via cron). Upgrade to managed is a config change, not a migration. Tradeoff: controller VM failure takes down both the control plane and the database.

---

## Confidence gate

```
Confidence: high
Residual risks:
  - Single-VM default (controller + proxy + Postgres) is a single point of failure for the entire platform. Acceptable for the target scale but the first thing to split when reliability matters.
  - Reverse tunnel multiplexing performance under high concurrent request load is unvalidated. The design assumes HTTP/2 stream multiplexing is sufficient, but pathological workloads (many large concurrent responses) could saturate the single TCP connection per agent.
Known gaps:
  - None blocking. The per-component zoom covers all contracts surfaces. The contracts phase can proceed.
```

Once confidence is high and gaps are resolved, commit with `docs: architecture` and tag `sad/v0.1.0`.
