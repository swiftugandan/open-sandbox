# Implementation Plan

> Decomposition of the system into binaries. Each binary depends only on the frozen contracts crate and on lower-level binaries through their published contracts. This is what makes "one binary at a time, protected by contracts" actually work.

## Prerequisites

- [x] `contracts/v0.1.0-frozen` tag exists
- [x] `SPEC.md`, `SAD.md`, `CONTRACTS.md` are committed and tagged
- [ ] Final confidence gate (below) is "high"

## Dependency DAG

```
  open-sandbox-contracts (frozen)
       │
   ┌───┼───────────┐
   │   │            │
   │   │            │
   ▼   ▼            ▼
 agent controller  proxy
   │       │        │
   │       │        │
   └───┬───┘        │
       │            │
       └─────┬──────┘
             │
             ▼
      open-sandbox (CLI binary — subcommand dispatch)
```

No cycles. Each component depends only on `contracts`. The final `open-sandbox` binary is the shell that dispatches to subcommands; it depends on all three component crates.

## Implementation order

> Sorted by dependency and by ability to test in isolation. Components with no peer dependencies are implemented first.

### 1. `contracts` (already frozen)

- **Depends on:** nothing
- **Status:** frozen at `contracts/v0.1.0-frozen`

### 2. `controller`

- **Depends on:** `contracts` only
- **Consumes contracts:** `AgentMessage`, `RegisterRequest` (from agents, received via gRPC)
- **Produces contracts:** `ControllerCommand`, `RegisterResponse`, `RoutingEntry`
- **Acceptance criterion (live e2e):** Given a mock agent that sends a valid `RegisterRequest` with a correct join token, the controller accepts the registration, stores the agent in Postgres, and responds with `RegisterResponse { accepted: true }`. Given subsequent `Heartbeat` messages, the controller responds with `HeartbeatAck`. Given a `CreateSandbox` API call, the controller selects an agent, writes a `RoutingEntry` to Postgres (triggering NOTIFY), and sends `StartSandbox` to the agent. Given 3 missed heartbeats, the controller marks the agent dead and removes its routing entries.
- **Estimated complexity:** L
- **Risks:**
  - gRPC bidirectional stream management with tonic is the most complex networking pattern in the system
  - Postgres LISTEN/NOTIFY integration needs careful connection management (separate connection for LISTEN)
  - Scheduler logic (agent selection) needs to handle edge cases (all agents full, agents dying mid-assignment)

### 3. `agent`

- **Depends on:** `contracts` only
- **Consumes contracts:** `ControllerCommand`, `TunnelRequest`
- **Produces contracts:** `AgentMessage`, `RegisterRequest`, `TunnelResponse`
- **Acceptance criterion (live e2e):** Given a running controller and proxy, the agent binary starts with a valid join token, registers successfully, begins heartbeating, receives a `StartSandbox` command, creates a Docker container, reports `SandboxStatus(running)`, establishes a reverse tunnel to the proxy, and forwards a tunneled HTTP request to the container's exposed port and returns the response.
- **Estimated complexity:** L
- **Risks:**
  - Docker Engine API integration (container lifecycle, log streaming)
  - Dual gRPC connection management (controller + proxy) with independent reconnection logic
  - Reconciliation on restart (what containers are already running vs what the controller thinks)

### 4. `proxy`

- **Depends on:** `contracts` only
- **Consumes contracts:** `TunnelResponse`, `RoutingEntry` (via Postgres read + LISTEN/NOTIFY)
- **Produces contracts:** `TunnelRequest`
- **Acceptance criterion (live e2e):** Given a Postgres routing table with an entry mapping sandbox `abc123` to agent `worker-7`, and agent `worker-7` connected via reverse tunnel, an HTTPS request to `abc123.sandbox.example.com` is routed through the tunnel to the agent, which forwards it to the local container, and the response is returned to the client with ≤ 5ms proxy-added latency at p99.
- **Estimated complexity:** L
- **Risks:**
  - TLS termination with wildcard cert and hot-reload on renewal
  - HTTP/2 stream multiplexing over agent tunnels under concurrent load
  - Routing cache consistency (stale cache → 502 errors; LISTEN/NOTIFY + 60s fallback mitigates)

### 5. `open-sandbox` (CLI shell)

- **Depends on:** `contracts`, `controller`, `agent`, `proxy`
- **Consumes contracts:** all (transitively)
- **Produces contracts:** none (this is the entry point)
- **Acceptance criterion (live e2e):** `open-sandbox controller` starts the controller, `open-sandbox proxy` starts the proxy, `open-sandbox agent --token <TOKEN>` starts the agent. All three subcommands respect CLI flags, env vars, and config file. `--help` is accurate. `--version` reports the contracts crate version.
- **Estimated complexity:** S
- **Risks:** Minimal — this is plumbing (clap subcommand dispatch).

### 6. `infra` (Pulumi stack)

- **Depends on:** compiled `open-sandbox` binary (uploaded to object storage or built on cloud-init)
- **Consumes contracts:** none (infrastructure, not Rust)
- **Produces:** running platform on target cloud
- **Acceptance criterion (live e2e):** `pulumi up` on a clean Hetzner account provisions the controller VM, 2 worker VMs, Postgres, DNS records, and TLS cert. A BYO agent from a developer's laptop can join via the install script. A sandbox is created and accessible at `<id>.sandbox.example.com`.
- **Estimated complexity:** L
- **Risks:**
  - Cloud provider API quirks (Hetzner's API for floating IPs, firewall rules)
  - Cloud-init reliability (agent binary download, systemd unit installation)
  - DNS propagation delay for wildcard records
  - Let's Encrypt DNS-01 challenge timing with Cloudflare

---

## Per-binary TDD cycle (applies to every entry above)

For each binary, in order:

1. Branch `module/<name>` from `main`
2. **Red:** failing tests against the contract → tag `module/<name>/red`
3. **Green:** minimal implementation → tag `module/<name>/green`
4. **Refactor:** smells checklist applied → tag `module/<name>/refactored`
5. **E2E (mocked peers):** → tag `module/<name>/e2e-mock`
6. **E2E (live peers):** → tag `module/<name>/live-verified`
7. Merge to `main` → tag `module/<name>/done`

See `ENGINEERING_DISCIPLINE.md` for the full cycle definition.

## Status snapshot

> This section is maintained by querying git, not by hand. Run:
>
> ```sh
> git tag --list 'module/*'
> ```

---

## Final confidence gate

```
Confidence: high
Residual risks:
  - All three core binaries (controller, agent, proxy) are estimated L complexity. The total implementation effort is substantial. The contracts freeze and TDD discipline mitigate integration risk, but calendar risk is real.
  - The Pulumi stack (module 6) depends on a working binary, so it cannot be started until at least the CLI shell (module 5) produces a runnable artifact. However, the Platform abstraction layer and cloud-init scripts can be developed in parallel with the Rust work.
  - Live e2e testing for the proxy requires a real TLS cert and DNS setup, which means the infra module (or a local dev equivalent) must exist before proxy live-e2e can complete.
Known gaps:
  - None blocking. The DAG is acyclic, all contracts surfaces are covered, and every acceptance criterion is stated as a testable contract-boundary assertion.
```

Once confidence is high, commit with `docs: implementation plan` and tag `plan/v0.1.0`. Phase 6 (implementation) may begin.
