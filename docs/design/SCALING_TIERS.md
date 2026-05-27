# Scaling Tiers — Architectural Decision Record

> Status: **decided 2026-05-27** as part of the 12-factor decomposition
> pass (see `docs/plans/PLAN_12FACTOR.md`). This document is the
> canonical reference for which tiers scale how and why. Future review
> passes that surface "the proxy isn't horizontally scalable" or
> "session state should be externalized to Redis" should start here.
>
> Companion documents:
> - `EXEC_STREAMING_DESIGN.md` — why the proxy holds live socket
>   endpoints (the `TunnelPool` / `IoSessions` model).
> - `PLAN_12FACTOR.md` — the gap-closure plan that produced this ADR.

## TL;DR

The system has three tiers with three different scaling shapes:

| Tier | Scaling shape | Why |
|---|---|---|
| **Agent fleet** | Horizontal | Each agent is one independent worker; this is where the actual sandbox load lives. |
| **Controller** | Single coordinator (Postgres-backed) | Owns the global scheduling state; Postgres is the source of truth, the process is a thin gRPC + LISTEN/NOTIFY broker. |
| **Proxy** | **Connection-affinity tier** (1-of-N) | Holds **live socket endpoints** for agent tunnels and gateway IO sessions; the maps in `tunnel_pool.rs` / `io_sessions.rs` are not "state" that can be externalized — they are the live sockets themselves. |
| **API gateway** | Stateless, horizontally scalable | Trivially replicable; no in-memory state. |

12-factor #6 ("stateless processes") and #8 ("scale out via the
process model") are interpreted as **"scale where the work is,"** not
as a literal demand that every tier be stateless and N-of-N.

## Why the proxy is connection-affinity, not stateless

The proxy holds three in-memory maps:

1. **`TunnelPool`** (`crates/proxy/src/tunnel_pool.rs:21`) —
   `HashMap<AgentId, AgentTunnel>` where `AgentTunnel.request_tx` is
   the proxy's end of an mpsc channel whose receiver is being drained
   into the gRPC server-streaming response to the agent's
   `OpenTunnel` call. **The sender cannot exist without the live
   socket on the other side of the gRPC response.**
2. **`IoSessions`** (`crates/proxy/src/io_sessions.rs:38`) — same
   shape for the gateway-facing bidirectional `OpenIoStream`. The
   `server_tx` is the proxy's end of the gateway's live HTTP/2
   response stream.
3. **`StreamMux`** — short-lived oneshot map for unary HTTP
   request/response pairs.

Plus `RoutingCache`, which **is** an externalized cache (Postgres
source of truth, LISTEN/NOTIFY invalidation). That one is already
stateless-equivalent and not part of this discussion.

The first three are **not state** in the sense that 12-factor #6
addresses ("durable data should live in a backing service"). They
are **live operating-system resources** — TCP sockets, mpsc channel
halves bound to those sockets, tokio tasks pumping bytes between
them. None of this serializes to Redis. None of it survives
process restart. None of it can be picked up by a second proxy
process. The framing "externalize the session state" is incoherent
for a tier whose primary job is owning socket pairs.

This is identical in shape to how an L4 load balancer with sticky
sessions is stateful: the state IS the connection ownership, and the
correct decomposition treats the tier as a connection-affinity tier
rather than trying to make it stateless.

See `EXEC_STREAMING_DESIGN.md` for the deeper rationale on why exec
and file operations were deliberately moved onto the proxy's data
plane (and therefore became part of this connection-affinity
constraint).

## Why the controller is a single coordinator

The controller does three things:
1. Validate agent registration and issue routing entries.
2. Run the scheduler (place new sandboxes on agents).
3. Broadcast routing-cache invalidations via Postgres `NOTIFY`.

All three are coordination tasks where the global view matters. The
durable state lives in Postgres. The controller process itself is
thin enough that hot-standby (advisory-lock leader election) is the
right HA story when uptime starts to matter — **not** "run N
controllers and coordinate them externally," which would just
recreate the leader-election problem one tier up.

HA is deferred (see `PLAN_12FACTOR.md` § "What's not in scope") until
a concrete uptime requirement appears. The single-instance default is
documented in `SAD.md` § "Confidence gate" as the first residual
risk; this ADR upgrades that from "known risk" to "deliberate
scaling decision, HA available when needed."

## Why the agent fleet IS horizontally scalable

Each agent process owns one worker host's container runtime. Agents
register with the controller, get assigned sandboxes by the
scheduler, and run them independently. Adding capacity = adding more
agent processes / more worker hosts.

This is where actual platform load lives. Every other tier exists to
route work TO the agent fleet. The agent tier scales without
coordination because the controller's scheduler already handles
work distribution.

## Consequences

- **Proxy can run 1-of-N safely.** Document this in operator docs.
  When HA matters, add a hot-standby proxy (passive replica that
  takes over the address on primary failure; in-flight sessions drop
  but agents reconnect within seconds).
- **Controller can run 1-of-N safely** for the same reason.
- **Do not propose externalizing `TunnelPool` / `IoSessions` /
  `StreamMux`.** It is incoherent for the reasons above. If the
  proxy tier ever needs to horizontally scale (e.g. data-plane
  bandwidth exhaustion on one box), the path is **sticky L7 routing
  with `agent_id → proxy_id` ownership in the controller** — see
  `PLAN_12FACTOR.md` § "What's not in scope" → "Multi-proxy
  horizontal scaling" for the sketch. That work starts with adding
  a `proxy_id` column to the `agents` table; it does not start with
  Redis.
- **The Phase 4 drain story** (proxy SIGTERM gracefully completing
  in-flight IoSessions before shutdown — see `PLAN_12FACTOR.md`
  Phase 4) is the partner to "1-of-N is safe." Drain bounds the
  blast radius of a planned proxy restart to "sessions older than
  `SHUTDOWN_DRAIN_TIMEOUT`," not "every session."
- **Drain covers IoSessions (gateway streams) and in-flight HTTP
  responses; it does NOT send a terminal frame to TunnelPool entries
  (agent OpenTunnel streams).** This is intentional: agents already
  implement exponential-backoff reconnect (`run_agent` retry loop),
  and adding a tunnel-side terminal frame would require a new
  TunnelRequest payload variant (a contract bump) with no observable
  benefit over reconnect-on-RST. Agents seeing their tunnel close
  during proxy shutdown reconnect within seconds; until they
  re-register, their sandboxes are unreachable — same window as a
  passive-replica failover.

## Runtime-path note

The `agent-docker` crate bind-mounts the host's `/var/run/docker.sock`
into the agent container — a 12-factor #4 violation in spirit (the
agent is coupled to a specific local filesystem path, not an attached
resource). This is acceptable because:

- `agent-docker` is the **dev / local-runtime** path. The Docker
  socket dependency is intrinsic to the runtime; removing it removes
  the runtime.
- `agent-youki` is the **production** runtime (see ADR-009). It does
  not need a daemon socket; the agent process is fully
  self-contained.

The 12-factor decomposition treats these as two different
deployment profiles, not two implementations of the same one.
