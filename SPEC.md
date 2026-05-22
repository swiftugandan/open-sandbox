# Specification

> Every non-obvious claim, threshold, or constraint in this document must trace to a citation (RFC, standard, vendor doc, empirical source) or an explicit rationale. Numbers without provenance are not specifications — they are guesses.

## Problem statement

Developers need isolated, publicly-accessible sandbox environments that can run on any machine — cloud VMs, personal laptops, or on-premise hardware — without requiring inbound network access, public IPs, or VPN infrastructure. Open Sandbox solves this by having agents dial *out* to a central controller/proxy over TLS, collapsing the networking problem to "can you reach port 443 outbound?" and enabling a BYO-worker model where joining the fleet is a single command.

## Functional requirements

- **FR-1:** An agent binary, given a controller URL and join token, establishes a persistent bidirectional gRPC stream over HTTP/2 with TLS to the controller and registers itself with declared resources (vCPU, RAM, arch, OS).
  - *Source:* gRPC core concepts define bidirectional streaming as a native call type over HTTP/2 [1]. The `tonic` Rust library implements this with async/await on top of `hyper`'s HTTP/2 stack [2].

- **FR-2:** The controller authenticates agents via join tokens (opaque strings), stores agent records in Postgres, and maintains a live connection table. Agents send heartbeats; the controller marks agents dead after N consecutive missed heartbeats and reschedules their sandboxes.
  - *Rationale:* Heartbeat-based failure detection is standard for distributed systems with long-lived connections. N = 3 missed heartbeats at 5-second intervals (15s detection) balances detection speed with tolerance for transient network jitter.

- **FR-3:** The controller can issue `StartSandbox`, `StopSandbox`, `Exec`, and `FetchLogs` commands to agents over the existing gRPC stream. Agents execute sandbox lifecycle operations via a pluggable container runtime. The default production runtime uses youki/libcontainer (daemonless OCI) for direct in-process container management. A Docker Engine runtime (via Unix socket) is available as a development fallback.
  - *Source:* OCI Runtime Specification defines container lifecycle operations [9]. youki/libcontainer implements the OCI spec as an in-process Rust library [15]. Docker Engine API provides an alternative via `/var/run/docker.sock` [3].

- **FR-4:** Each agent opens a second gRPC connection to the proxy for reverse tunneling. The proxy multiplexes virtual streams over the single HTTP/2 connection — one per inbound sandbox request — using HTTP/2's native stream multiplexing.
  - *Source:* HTTP/2 (RFC 9113) supports multiplexing multiple streams over a single TCP connection [4]. gRPC maps each RPC to an HTTP/2 stream [1].

- **FR-5:** The proxy terminates TLS for `*.sandbox.example.com`, extracts the sandbox ID from the `Host` header, looks up the owning agent in the routing table, and forwards the request through the agent's reverse tunnel to the sandbox container's local port.
  - *Rationale:* Subdomain-based routing is the simplest addressing scheme and avoids path-prefix conflicts with sandbox applications.

- **FR-6:** The routing table is stored in Postgres and cached in-memory by the proxy with invalidation via Postgres `LISTEN/NOTIFY`.
  - *Source:* PostgreSQL `LISTEN/NOTIFY` provides asynchronous notification delivery between sessions, operates entirely in memory, and requires no additional infrastructure [5]. It is suitable for low-to-medium notification rates (hundreds/second) [6], which matches routing-table update frequency.

- **FR-7:** Join tokens come in two flavors: managed (baked into cloud-init by Pulumi at provision time) and BYO (issued via API/CLI, scoped to account/team, with TTL and revocability).
  - *Rationale:* Separating token types allows managed infrastructure to be fully automated while giving BYO workers a human-friendly onboarding flow.

- **FR-8:** A BYO worker joins the fleet via a single shell command: `curl -sSL https://get.example.com | sh -s -- --token <TOKEN>`. This downloads a statically-linked agent binary, installs a systemd unit (Linux) or launchd plist (macOS), and starts the service.
  - *Rationale:* Static linking via `musl` eliminates runtime library dependencies on the target host. Target triples: `x86_64-unknown-linux-musl`, `aarch64-unknown-linux-musl`, `aarch64-apple-darwin`.

- **FR-9:** The entire infrastructure is deployable via a single Pulumi stack with a cloud-portable abstraction layer. Changing the target cloud is a config change (`platform:cloud: hetzner` → `platform:cloud: aws`), not a code change.
  - *Rationale:* The Platform interface abstracts VM, bucket, and database primitives. Cloud-specific implementations are 100–300 lines each. The abstraction is at the level of cloud primitives (a VM, a bucket), not application resources (the controller).

- **FR-10:** Agents reconnect to the controller on disconnection using exponential backoff with jitter, reusing their agent ID. The controller picks up where it left off.
  - *Rationale:* Exponential backoff with jitter is the standard approach for reconnection in distributed systems, preventing thundering-herd reconnection storms.

- **FR-11:** An API gateway exposes a REST interface for external clients (primarily AI agents) to manage the sandbox lifecycle: create a sandbox, stop/delete a sandbox, get sandbox status. The API gateway is a separate component (`open-sandbox api`) that speaks REST to clients and gRPC (unary RPCs) to the controller.
  - *Rationale:* The controller's bidirectional gRPC stream is designed for persistent agent connections, not request/response client interactions. Unary RPCs for the external control plane keep the agent stream protocol unchanged. Separating the API into its own component preserves the data-plane/control-plane boundary — the proxy routes bytes, the controller orchestrates agents, the API translates external intent.

- **FR-12:** The API gateway supports command execution on running sandboxes: an external client sends an exec request with a command and arguments, and receives stdout, stderr, and exit code in the response. Commands are forwarded from the API to the controller, which dispatches them to the hosting agent over the existing agent stream, using the `ExecCommand` message already defined in the controller proto.
  - *Rationale:* Command execution is the primary interaction pattern for AI agents using sandboxes. The proto already defines `ExecCommand`; the API gateway provides the external entry point.

- **FR-13:** The API gateway supports file operations on running sandboxes: writing files into a sandbox (as a tar.gz upload) and reading files out of a sandbox (as an octet-stream download). File operations are implemented as `ExecCommand` invocations under the hood: write extracts an uploaded archive via `tar xzf`, read uses `cat` piped back through the tunnel.
  - *Rationale:* AI agents need to put code into sandboxes and retrieve results. Using exec-backed file operations avoids adding a new data channel to the agent protocol. The tar.gz format handles multiple files, directories, and permissions in a single upload — matching Vercel's sandbox SDK convention [13].
  - *Source:* Vercel Sandbox REST API uses tar.gz upload for file writes, octet-stream for reads [13].

- **FR-14:** The API gateway authenticates clients via API key in the `Authorization: Bearer <key>` header. API keys are stored hashed in Postgres and validated by the API gateway.
  - *Rationale:* Bearer token authentication is the standard for REST APIs (RFC 6750 [14]). API keys are simpler than OAuth for programmatic agent access.

## Non-functional requirements

### Performance

- **NFR-PERF-1:** Proxy request forwarding adds ≤ 5ms p99 latency to sandbox responses (measured at the proxy, excluding sandbox processing time and network RTT to the agent).
  - *Rationale:* The proxy performs only a routing-table lookup (in-memory hash map) and stream multiplexing over an existing HTTP/2 connection. Both operations are sub-millisecond. The 5ms budget accounts for connection scheduling and kernel buffer copies. Chosen as a ceiling, not a target — actual latency should be lower.

- **NFR-PERF-2:** The controller handles ≤ 1,000 concurrent agent connections on a 2 vCPU / 4 GB VM.
  - *Rationale:* Each agent connection is one HTTP/2 stream consuming ~50 KB of state (connection buffers + routing metadata). 1,000 agents × 50 KB = ~50 MB, well within 4 GB. The bottleneck is CPU for heartbeat processing, not memory.

- **NFR-PERF-3:** Agent heartbeat interval: 5 seconds. Dead-agent detection: 3 missed heartbeats (15 seconds).
  - *Rationale:* 5s balances detection speed with network overhead. At 1,000 agents, this is 200 heartbeats/second — trivial for the controller.

### Durability & consistency

- **NFR-DUR-1:** Agent registrations, sandbox metadata, and routing table entries are persisted to Postgres before acknowledgment. Loss of the controller process does not lose state.
  - *Source:* PostgreSQL provides ACID transactions with write-ahead logging [7].

- **NFR-DUR-2:** Postgres is backed up to object storage via `pg_dump` on a cron schedule (default: every 6 hours). Recovery point objective (RPO): 6 hours. Recovery time objective (RTO): 30 minutes (restore from dump + restart services).
  - *Rationale:* For a small team at the default scale, `pg_dump` to object storage is the simplest backup strategy that provides disaster recovery without the cost of managed Postgres.

### Security

- **NFR-SEC-1:** All agent-to-controller and agent-to-proxy communication is over TLS 1.3 (or TLS 1.2 minimum). Agents validate the server certificate. The controller validates join tokens before accepting registrations.
  - *Source:* TLS 1.3 is defined in RFC 8446 [8]. `tonic` supports TLS via `rustls` [2].

- **NFR-SEC-2:** The proxy routes sandbox traffic solely based on the routing table written by the controller. A sandbox ID routes only to the agent that owns it. The platform does not authenticate end-user sandbox traffic — that is the sandbox's responsibility.
  - *Rationale:* The platform is a transport layer. Imposing auth requirements on sandbox traffic would limit the types of applications sandboxes can serve.

- **NFR-SEC-3:** Sandboxes run with OCI-standard isolation (Linux namespaces: pid, network, mount, ipc, uts; cgroups v2 for CPU/memory limits). No privileged capabilities. Seccomp profile applied via libseccomp (adds 8K to binary size; no reason to disable).
  - *Source:* OCI Runtime Specification defines container isolation via namespaces and cgroups [9]. libseccomp provides the seccomp BPF filter interface [15].

- **NFR-SEC-4:** Join tokens are generated with ≥ 128 bits of entropy, transmitted only over TLS, stored hashed (bcrypt or argon2) in Postgres, and are revocable.
  - *Rationale:* 128 bits of entropy makes brute-force infeasible. Hashing prevents token leakage from database compromise.

- **NFR-SEC-5:** The controller VM accepts inbound traffic only on port 443 (HTTPS/gRPC) and port 22 (SSH, restricted to operator IPs). Worker VMs accept no inbound traffic.
  - *Rationale:* Minimal attack surface. Workers only need outbound 443.

### Observability

- **NFR-OBS-1:** The controller exposes Prometheus metrics: connected agents, active sandboxes, heartbeat latency histogram, sandbox start/stop rates, routing-table size.
  - *Rationale:* These are the minimum metrics needed to operate the platform: capacity (agents, sandboxes), health (heartbeats), and activity (start/stop rates).

- **NFR-OBS-2:** The proxy exposes Prometheus metrics: request rate, latency histogram (by sandbox), active tunnels, error rates (by type: routing miss, tunnel failure, upstream timeout).
  - *Rationale:* Request-path observability is essential for debugging latency and availability issues.

- **NFR-OBS-3:** Structured logging (JSON) to stdout on all components. Log levels: error, warn, info, debug, trace. Default level: info.
  - *Rationale:* JSON to stdout is the lowest-common-denominator logging approach that works with every log aggregation system.

### Operability

- **NFR-OPS-1:** The platform runs as a single binary with subcommands: `open-sandbox controller`, `open-sandbox proxy`, `open-sandbox agent`. In the default deployment, controller and proxy run on the same VM.
  - *Rationale:* Single binary simplifies deployment, distribution, and versioning. Subcommands allow splitting components onto separate VMs without changing the binary.

- **NFR-OPS-2:** TLS certificates are obtained from Let's Encrypt via DNS-01 challenge against Cloudflare DNS. Wildcard certificate for `*.sandbox.example.com`. Automatic renewal.
  - *Source:* Let's Encrypt supports wildcard certificates via DNS-01 challenge [10]. Rate limit: up to 50 certificates per registered domain per week; wildcard renewals against the same set of domains are not counted against this limit [10]. Certificate lifetime is moving to 45 days as of 2026 [11].

- **NFR-OPS-3:** Infrastructure is managed via Pulumi with state stored in object storage. Secrets are encrypted by the Pulumi state backend. No external KMS or secrets manager required at the default scale.
  - *Rationale:* Pulumi's built-in encryption is sufficient for a small team and avoids the per-secret monthly cost of cloud KMS services.

## Non-goals

- **NG-1:** Multi-region deployment — *because* the architecture supports it in the future (per-region proxy fleets, regional agent pools) but designing for it now adds complexity without demand. Flag as future work.
- **NG-2:** Raw TCP port exposure for sandboxes (databases, SSH) — *because* it requires per-sandbox port allocation on the proxy or a separate TCP proxy mode, fundamentally changing the proxy design. HTTP-only via subdomain routing for v1.
- **NG-3:** Sandbox-level authentication or authorization by the platform — *because* the platform is a transport layer. Sandboxes implement their own auth if needed.
- **NG-4:** Automatic horizontal scaling of the controller or proxy — *because* manual scaling (adding VMs via Pulumi config) is sufficient at the target scale. Auto-scaling adds cloud-specific complexity.
- **NG-5:** QUIC/UDP transport — *because* corporate networks block UDP/443 surprisingly often, and HTTP/2 over TCP is the path of least resistance for BYO workers.
- **NG-6:** Multi-tenancy — *because* adding tenant IDs to every resource (sandbox, agent, routing row, join token) is recoverable later but premature now. Single-tenant for v1.

## Constraints

- **C-1:** Default deployment cost must stay under $20/month on Hetzner. This constrains the default to self-hosted Postgres on the controller VM, no managed load balancer, and small spot/cheap VMs for workers.
  - *Source:* Hetzner CAX11 (2 vCPU ARM, 4 GB RAM): ~€3.79/month. CX22 (2 vCPU x86, 4 GB RAM): ~€4.59/month. 20 GB block volume: ~€1/month. Floating IP: ~€0.60/month [12].
- **C-2:** Agent binary must be statically linked and run on Linux (x86_64, aarch64) and macOS (aarch64) without runtime dependencies beyond CNI plugin binaries (bridge, portmap, loopback) on Linux production hosts. Docker is an optional development dependency for the Docker runtime backend.
- **C-3:** The platform must work when agents are behind NAT, corporate firewalls, or residential ISPs — the only network requirement is outbound TCP/443.
- **C-4:** DNS is managed via Cloudflare regardless of compute cloud, to decouple domain management from infrastructure provider.

## Open questions

- [x] Primary cloud for v1 — **Hetzner** (cheapest, simplest Platform implementation; AWS validates the abstraction as second target)
- [x] Postgres: self-hosted vs managed — **Self-hosted on controller VM** for default; Pulumi flag `managedPostgres: true` for the flip
- [x] TLS termination — **L7 in the proxy** (gives Host-based routing for free; revisit only at CPU limits)
- [x] Sandbox addressing — **Subdomain only** (`<id>.sandbox.example.com`); raw TCP is a non-goal (NG-2)
- [ ] Agent binary auto-update channel — should agents self-update, or is operator-driven update sufficient for v1?
- [ ] Sandbox resource limits — what are the default CPU/memory cgroup limits per sandbox?

## Glossary

- **Agent:** The binary running on a worker machine that manages OCI container sandboxes and maintains connections to the controller and proxy.
- **Controller:** The control-plane component that manages agent registrations, sandbox scheduling, and the routing table.
- **Proxy:** The data-plane component that terminates public TLS, routes requests by subdomain to agents via reverse tunnels.
- **Sandbox:** An isolated OCI container running a user's workload, accessible via a unique subdomain.
- **Join token:** An opaque credential used by an agent to authenticate with the controller during registration.
- **BYO worker:** A bring-your-own worker — any machine running the agent binary that has been registered with a join token.
- **Routing table:** The Postgres-backed mapping from sandbox ID to the agent that hosts it, consumed by the proxy for request routing.
- **Reverse tunnel:** The outbound gRPC connection from agent to proxy, over which inbound sandbox traffic is multiplexed back to the agent.
- **API gateway:** The external control-plane component that translates REST requests from clients into gRPC calls to the controller. Separate from the proxy (data plane) and controller (internal control plane).

## References

1. gRPC Core Concepts — bidirectional streaming, HTTP/2 mapping. https://grpc.io/docs/what-is-grpc/core-concepts/
2. Tonic — Rust gRPC implementation over HTTP/2 with async/await, TLS via rustls. https://github.com/hyperium/tonic
3. Docker Engine API — container lifecycle, default seccomp profile. https://docs.docker.com/engine/api/
4. RFC 9113 — HTTP/2 stream multiplexing. https://www.rfc-editor.org/rfc/rfc9113
5. PostgreSQL Documentation — LISTEN/NOTIFY asynchronous notification. https://www.postgresql.org/docs/current/sql-notify.html
6. Recall.ai — Postgres LISTEN/NOTIFY scalability characteristics (hundreds/second throughput). https://www.recall.ai/blog/postgres-listen-notify-does-not-scale
7. PostgreSQL Documentation — Reliability and Write-Ahead Logging. https://www.postgresql.org/docs/current/wal-intro.html
8. RFC 8446 — TLS 1.3. https://www.rfc-editor.org/rfc/rfc8446
9. OCI Runtime Specification — container isolation via namespaces and cgroups. https://github.com/opencontainers/runtime-spec
10. Let's Encrypt — Rate Limits, wildcard certificates, DNS-01 challenge. https://letsencrypt.org/docs/rate-limits/
11. Let's Encrypt — Decreasing certificate lifetimes to 45 days. https://letsencrypt.org/2025/12/02/from-90-to-45
12. Hetzner Cloud — ARM and x86 VM pricing, included bandwidth. https://www.hetzner.com/cloud/
13. Vercel Sandbox REST API — file write via tar.gz upload, file read via octet-stream. https://vercel.com/docs/rest-api/sandboxes/write-files
14. RFC 6750 — Bearer Token Usage for OAuth 2.0. https://www.rfc-editor.org/rfc/rfc6750
15. youki — OCI container runtime in Rust, libcontainer library. https://github.com/youki-dev/youki

---

## Confidence gate

```
Confidence: high
Residual risks:
  - Postgres LISTEN/NOTIFY scalability ceiling (~hundreds/sec) is adequate for routing updates but would need replacement (Redis, NATS) if sandbox churn exceeds this rate — unlikely at target scale but worth monitoring
  - Let's Encrypt 45-day certificate lifetime (effective 2026) requires reliable automated renewal; failure means sandbox downtime
  - File operations via exec (FR-13) add latency compared to a dedicated file channel, but avoid protocol complexity. Acceptable for the AI agent use case where file operations are infrequent relative to exec.
Known gaps:
  - Agent auto-update strategy (open question) — deferring to v1.1; operator-driven updates are sufficient for initial deployment
  - Default sandbox resource limits not yet specified — will be determined during contracts phase based on target workload profiling
  - ExecCommand response path (stdout/stderr/exit code back from agent to controller to API) needs implementation — the proto message exists but the response flow is not yet wired
```

Amended with FR-11 through FR-14 (API gateway). Tagged `spec/v0.2.0`.

Amended with runtime-agnostic language and youki/libcontainer as default production runtime (FR-3, NFR-SEC-3, C-2, glossary). Tagged `spec/v0.3.0`.
