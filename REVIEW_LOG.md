# Code Review Log

Cross-session log for the component-by-component review pass described in `CODE_REVIEW_PLAN.md`. Anchored at `contracts/v1.0.1`.

Findings are grouped by category:
- **Deferred contract-change candidates** — fixes that would alter `proto/` or `crates/contracts`; held until end-of-pass, then triaged into a potential `contracts/v1.0.2` cycle.
- **Cross-component findings** — bugs in a downstream crate surfaced during another crate's review.
- **Spike-invariant violations** — code that contradicts a confirmed spike conclusion in `EXEC_STREAMING_DESIGN.md`.

Format per entry: `## [comp-N · severity] short title` with `Source`, `File`, `Summary`, `Failure scenario`, `Status`.

---

## Deferred contract-change candidates

### [comp-0 · high] proto/controller.proto: tag 5 reused without `reserved 5;`

- **Source:** component 0 audit (Angle A)
- **File:** `proto/controller.proto:34` (and the parallel site on AgentMessage)
- **Summary:** Tag 5 was removed (formerly `ExecCommand` / `ExecResult`) and immediately reused for `FetchLogsCommand` without a `reserved 5;` declaration on `AgentMessage` or `ControllerCommand`. Proto3 hygiene requires reserved tags to prevent wire-confusion with older peers.
- **Failure scenario:** A v0.x agent (or replayed v0.x capture) decoding a v1.0 `FetchLogsCommand{sandbox_id, tail_lines, follow}` on tag 5 parses the bytes as `ExecCommand` and invokes the runtime exec API on garbage taken from a logs request. Forward-compat is silently broken with no compile-time signal.
- **Status:** open — deferred to `contracts/v1.0.2`.

### [comp-0 · high] grpc_to_api maps every `tonic::Code::NotFound` to `SandboxNotFound`

- **Source:** component 0 audit (Angle C)
- **File:** `crates/api/src/grpc_service.rs:120` (consumer); root cause in `crates/contracts/src/error.rs` (missing `From<tonic::Status>` for `ControllerError`)
- **Summary:** `grpc_to_api` maps every `tonic::Code::NotFound` to `ApiError::SandboxNotFound{sandbox_id: status.message()}`, but controller emits `NotFound` from non-sandbox-lookup paths too (e.g. `AgentStream` in `crates/controller/src/grpc.rs:152`). `ControllerError` variants (`NoAvailableAgents`, `InvalidToken`, `Database`) have no `From`/`Into<tonic::Status>` impl in contracts — the entire variant set is un-roundtrippable across the wire.
- **Failure scenario:** Controller returns `Status::not_found("agent x not found")` for a stream-side condition. API surfaces `ApiError::SandboxNotFound{sandbox_id: "agent x not found"}` — SDK sees a 404 `SANDBOX_NOT_FOUND` with the literal error string in the `sandbox_id` field. `ControllerError::NoAvailableAgents` collapses to opaque `INTERNAL_ERROR`.
- **Status:** open — deferred to `contracts/v1.0.2`. Fix likely needs both a `ControllerError ↔ tonic::Status` codec in contracts AND the controller emitting trailers/details rather than bare `Status::not_found(string)`.

### [comp-0 · high] proto/proxy.proto: `IoError.code` stringly-typed; API handler drops most agent-emitted codes

- **Source:** component 0 audit (Angle B/C)
- **File:** `proto/proxy.proto:196` (contract); `crates/api/src/handlers.rs:~385` (consumer that misses variants)
- **Summary:** `IoError.code` is documented as a stable identifier (`RUNTIME_ERROR`, `SANDBOX_GONE`, `EXEC_FAILED`, `READ_FAILED`) but is a free-form string with no enum or constants module. `map_io_error` only matches `FILE_NOT_FOUND` and `SANDBOX_GONE`; agent actually emits `WRITE_FAILED`, `RUNTIME_ERROR`, `INVALID_REQUEST`, `EXTRACT_FAILED`, all of which collapse to `ApiError::IoStreamFailed`.
- **Failure scenario:** Agent returns `IoError{code: "WRITE_FAILED", detail: "disk full"}` on file upload. API folds it into a generic 500 `INTERNAL_ERROR`, hiding the disk-full signal. `INVALID_REQUEST` (4xx semantics) surfaces as 500. SDKs that key on the documented identifier set silently fall to a default branch.
- **Status:** open — deferred. Fix is a contracts-level enum or const set for `IoError.code` plus updated `map_io_error`.

### [comp-0 · high] cpu unit ambiguity: cores vs millicores in the same uint32 family

- **Source:** component 0 audit (Angle A/C)
- **File:** `proto/controller.proto:60` (cores) vs `:89,101` (millicores); `proto/api.proto:17` (millicores)
- **Summary:** `AgentResources.cpu_cores` (cores) and `ResourceReport.available_cpu_millicores` (millicores) share `uint32` and live next to each other. `SandboxConfig.cpu_limit_millicores` and `CreateSandboxRequest.cpu_millicores` are also `uint32`. The type system cannot catch a cores/millicores copy-paste.
- **Failure scenario:** Scheduler computes capacity by summing `AgentResources.cpu_cores` and comparing against `CreateSandboxRequest.cpu_millicores` without the `*1000` conversion. 1000-millicore request rejected against an 8-core box. The reverse mistake over-commits 1000x.
- **Status:** open — deferred. Possible fix: collapse to a single unit (millicores) on the wire and provide typed wrappers in contracts.

### [comp-0 · high] `exposed_port` is `uint32` but TCP ports are u16

- **Source:** component 0 audit (Angle A/B)
- **File:** `proto/api.proto:20`, `proto/controller.proto:104`, `crates/contracts/src/constants.rs:24`
- **Summary:** `CreateSandboxRequest.exposed_port` and `SandboxConfig.exposed_port` are `uint32`; TCP ports are bounded to u16 (0..=65535). Contracts/v1.0.1 has no validator. `constants.rs` worsens the inconsistency: `METRICS_DEFAULT_PORT`/`API_DEFAULT_PORT` are u16 but `DEFAULT_SANDBOX_EXPOSED_PORT` is u32.
- **Failure scenario:** Caller sends `CreateSandboxRequest{exposed_port: 70000}`; accepted at the contract layer; agent does `port as u16` producing port 4464; sandbox binds to the wrong port; public subdomain routing breaks silently.
- **Status:** open — deferred. Fix is contracts-level u16 newtype or a `validate_port` helper that downstream crates can call.

### [comp-0 · high] `SandboxId`/`AgentId` have no wire validator

- **Source:** component 0 audit (Angle B)
- **File:** `crates/contracts/src/types.rs:32`
- **Summary:** `SandboxId`/`AgentId` are typed UUID newtypes inside the crate, but every proto message uses raw `string sandbox_id`/`agent_id` on the wire. `types.rs` provides no `TryFrom<&str>`, `FromStr`, or `validate()` helper. No constant for max length / charset. Doc comments in proxy.proto assume well-formed IDs; contracts enforce nothing.
- **Failure scenario:** Proxy receives `IoStart{sandbox_id: "; DROP TABLE --"}` or a 4KB junk string from a public WS open. Nothing in contracts rejects it; downstream code must each remember to revalidate or pass garbage into DB lookups, log lines, and routing-cache keys. Two services can disagree about what constitutes a valid id.
- **Status:** open — deferred. Fix is `TryFrom<&str>` + `MAX_ID_LEN` const + validator usage at every wire-decode boundary.

### [comp-0 · med] `SandboxId::subdomain()` hardcodes 12; proxy router hardcodes 12 independently

- **Source:** component 0 audit (Angle B/C)
- **File:** `crates/contracts/src/types.rs:41`, mirrored by `crates/proxy/src/router.rs:24`
- **Summary:** `SandboxId::subdomain()` slices `self.0.simple().to_string()[..12]` with 12 hardcoded; the proxy router independently hardcodes `subdomain.len() != 12` plus a hex-only check. No shared `MAX_SUBDOMAIN_LEN`/`SUBDOMAIN_CHARSET` constant in `constants.rs`; generator and router can drift in isolation.
- **Failure scenario:** A maintainer raises the slice to `[..16]` in types.rs; the proxy router still rejects everything that isn't 12-char hex; every newly created sandbox returns `ProxyError::RoutingMiss` with no obvious cause. The reverse — proxy widens but generator stays — silently shortens routing keys and raises collision probability.
- **Status:** open — deferred. Fix is a single `SUBDOMAIN_LEN` const + a `subdomain_is_valid(&str)` helper.

### [comp-0 · med] `ApiError::error_code()` wildcard arm + non_exhaustive hides future variants as `"UNKNOWN"`

- **Source:** component 0 audit (Angle A/B)
- **File:** `crates/contracts/src/error.rs:86`
- **Summary:** `error_code()` uses `#[allow(unreachable_patterns)]` + a `_ => "UNKNOWN"` wildcard. `ApiError` is `#[non_exhaustive]` for external users, but inside the defining crate the match would be exhaustive — the wildcard silently absorbs every future variant. No round-trip test asserts that every variant maps to a non-`UNKNOWN` code.
- **Failure scenario:** Maintainer adds `ApiError::RateLimited{..}` and forgets the match arm. Build is green, but every rate-limit response ships with `error_code: "UNKNOWN"`. SDK retry logic keyed on the documented code never fires.
- **Status:** open — deferred. Fix is to drop the wildcard (so the compiler flags missing arms in this crate) or add a property test that iterates the variant set.

### [comp-0 · med] proto/proxy.proto: IoClientFrame first-frame contract + IoSignal.signum bounds unenforced

- **Source:** component 0 audit (Angle B)
- **File:** `proto/proxy.proto:89` (IoClientFrame oneof), `:~155` (IoSignal.signum)
- **Summary:** `IoClientFrame.payload` is a oneof whose first frame MUST be `IoStart` (proxy.proto:96–97 doc), but no contracts-side helper validates 'first frame is IoStart'. `IoSignal.signum` is `uint32` with no clamp to valid POSIX signals (1..=31 + RT range); doc lists only 15/9/2 as examples. `signum=0` is documented POSIX semantics for a liveness probe, not a kill.
- **Failure scenario:** Client opens `OpenIoStream` and sends `IoClientFrame{payload: stdin(b"...")}` as the first frame, skipping `IoStart`; each proxy/agent independently has to remember to reject. Or client sends `IoSignal{signum: 0}` — agent calls `kill(pid, 0)` and the documented 'SIGTERMs the in-container PID' contract breaks silently.
- **Status:** open — deferred. Fix is a `validate_first_frame()` helper + `Signum` newtype or const set in contracts.

### [comp-0 · low-med] `as_secs() as u32` truncation hazard when Duration constants change

- **Source:** component 0 audit (Angle B/C)
- **File:** `crates/contracts/src/constants.rs:18` (SANDBOX_STOP_TIMEOUT), `:8` (DEAD_AGENT_TIMEOUT derivation)
- **Summary:** `SANDBOX_STOP_TIMEOUT` is `Duration::from_secs(10)` but the wire field `StopSandbox.timeout_seconds` is `uint32` seconds. Two call sites (`crates/controller/src/management.rs:135`, `crates/cli/src/run.rs:300`) do `SANDBOX_STOP_TIMEOUT.as_secs() as u32`. The same shape applies to `DEAD_AGENT_TIMEOUT` computed from `HEARTBEAT_INTERVAL.as_secs() * 3`. If either constant is ever changed to a sub-second `Duration`, the truncation silently produces 0.
- **Failure scenario:** Operator tunes `SANDBOX_STOP_TIMEOUT` to `Duration::from_millis(500)`; controller and CLI send `timeout_seconds=0`; agent SIGKILLs immediately with zero grace, losing unflushed state in every sandbox shutdown.
- **Status:** open — deferred. Fix is either a wire schema that carries millis (or a `Duration` codec) or a `try_into_seconds_u32()` helper that returns `Err` on lossy conversion.

---

## Cross-component findings

### [comp-1 · high] CLI agent runtime has no reconnect loop

- **Source:** component 1 review (Angle C)
- **File:** `crates/cli/src/run.rs:281`
- **Summary:** The CLI agent binary spawns `ControllerConnection::run` exactly once and the surrounding `tokio::select!` exits on its first error. `ExponentialBackoff` is already defined in `crates/agent/src/reconnect.rs` but the binary never uses it.
- **Failure scenario:** Any transient controller restart, network blip, or controller-side `Status::not_found` from `registry.heartbeat` propagates `AgentError::ControllerDisconnected` out of the select, tearing down the entire agent process — including the still-healthy proxy_handle. Sandbox containers are forcibly stopped on shutdown_signal, and the controller's stale routing entries point at this agent until `sweep_dead_agents` fires (`DEAD_AGENT_TIMEOUT` later).
- **Status:** open — defer to the cli / agent-core review (slots 3 and 8 of `CODE_REVIEW_PLAN.md`).

### [comp-1 · med] Proxy `routing_entries` lookup uses `LIKE` on expression with no functional index, and proxy runs no migrations

- **Source:** component 1 review (Angle C)
- **File:** `crates/proxy/src/pg_store.rs:42`
- **Summary:** Proxy cache-miss path queries `WHERE replace(sandbox_id::text, '-', '') LIKE $1` — the expression has no functional index, so every cache miss does a sequential scan over routing_entries. The proxy crate also runs no migrations of its own, so it implicitly depends on the controller's `pg_store::migrate()` having executed first.
- **Failure scenario:** (1) Boot ordering: if the proxy starts before the controller has migrated, lookups return `ProxyError::Internal { detail: 'relation "routing_entries" does not exist' }` instead of the contracts-level `RoutingNotFound`. (2) Scaling: under any non-trivial sandbox count, the cache-miss path added in `da9e791 feat(proxy): cache-miss DB fallback` becomes O(n) per miss — defeating the whole point of the cache.
- **Status:** open — defer to the proxy review (slot 2 of `CODE_REVIEW_PLAN.md`).

---

## Spike-invariant violations

_(empty — populated as component reviews land)_
