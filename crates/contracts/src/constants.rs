use std::time::Duration;

pub const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);

pub const DEAD_AGENT_THRESHOLD: u32 = 3;

pub const DEAD_AGENT_TIMEOUT: Duration =
    Duration::from_secs(HEARTBEAT_INTERVAL.as_secs() * DEAD_AGENT_THRESHOLD as u64);

pub const UPSTREAM_TIMEOUT: Duration = Duration::from_secs(30);

pub const ROUTING_CACHE_REFRESH_INTERVAL: Duration = Duration::from_secs(60);

pub const RECONNECT_BASE_DELAY: Duration = Duration::from_secs(1);

pub const RECONNECT_MAX_DELAY: Duration = Duration::from_secs(30);

pub const SANDBOX_STOP_TIMEOUT: Duration = Duration::from_secs(10);

/// v1.0.2 (iter11): operator-facing upper bound on a single
/// `ContainerRuntime::create_and_start` call, enforced uniformly
/// across all runtimes by `SandboxManager::start_sandbox`'s
/// `tokio::time::timeout` wrap. Intentionally set BELOW the docker
/// runtime's theoretical worst-case retry budget (3 outer
/// port-retry × 4 pull-attempt × ~7.5s of backoff ≈ 90s): the
/// agent fails loud rather than letting an outlier camp on a
/// thread for the full retry ceiling. The trade-off: a legitimate
/// slow operation past 60s (e.g. a multi-GB cold pull on a slow
/// link) gets killed; raise this constant if your fleet
/// regularly pulls images larger than ~150 MB on first start.
///
/// On expiry the agent returns
/// `AgentError::Runtime { detail: "create_and_start deadline of
/// 60s exceeded ..." }`. If `create_container` succeeded but
/// `start_container` was mid-flight when the timeout fired, the
/// partially-created docker container leaks until the agent
/// process restarts — `SandboxManager::reconcile` exists but is
/// not yet periodically invoked. Caller-supplied deadlines (e.g.
/// a tonic-propagated gRPC deadline) and a periodic reconcile
/// task are both follow-ups.
pub const SANDBOX_CREATE_DEADLINE: Duration = Duration::from_secs(60);

pub const DEFAULT_SANDBOX_CPU_MILLICORES: u32 = 1000;

pub const DEFAULT_SANDBOX_MEMORY_BYTES: u64 = 512 * 1024 * 1024;

pub const DEFAULT_SANDBOX_EXPOSED_PORT: u32 = 8080;

pub const METRICS_DEFAULT_PORT: u16 = 9090;

pub const API_DEFAULT_PORT: u16 = 8081;

// EXEC_TIMEOUT removed in v1.0 — streaming exec sessions live as
// long as the WebSocket. Idle WebSocket keepalive uses
// WS_IDLE_PING_INTERVAL / WS_IDLE_PING_TIMEOUT instead.
pub const WS_IDLE_PING_INTERVAL: Duration = Duration::from_secs(30);

pub const WS_IDLE_PING_TIMEOUT: Duration = Duration::from_secs(60);

// Default grace period between SIGTERM and SIGKILL when the
// ExecRegistry cleanup hook fires on stream close. Generous because
// the in-container process may be a shell with cleanup logic.
pub const EXEC_KILL_GRACE: Duration = Duration::from_secs(5);

// Env var holding the shared secret the API gateway uses to
// authenticate to the proxy's internal OpenIoStream listener.
// Defense in depth alongside network isolation (per D2 / SAD).
pub const INTERNAL_TOKEN_ENV: &str = "OPEN_SANDBOX_INTERNAL_TOKEN";

pub const PROXY_STARTUP_RETRY_ATTEMPTS: u32 = 15;

pub const PROXY_STARTUP_RETRY_INTERVAL: Duration = Duration::from_secs(2);

pub const DEFAULT_WRITE_CWD: &str = "/home";

/// Default cmd handed to the runtime when starting a sandbox.
///
/// `mkdir -p /workspace` seeds the directory the Edit tab opens to
/// (`DEFAULT_TREE_ROOT` in the UI, also the convention every
/// quickstart template uses) so the file tree shows an existing
/// (possibly empty) directory before any user-supplied process runs.
/// `exec sleep infinity` replaces the shell with sleep so signals
/// (SIGTERM on stop) propagate to PID 1 as expected.
///
/// **Image requirement:** this assumes `/bin/sh` is on PATH —
/// busybox / dash / bash all work. Distroless / scratch images
/// without a shell will fail to start; users of those images must
/// override via a future per-sandbox `entrypoint` config (deferred).
pub const DEFAULT_SANDBOX_ENTRYPOINT: &[&str] =
    &["sh", "-c", "mkdir -p /workspace && exec sleep infinity"];

// v1.0.2 (closes comp-0 subdomain hardcoded-12 finding): single source of
// truth for the sandbox-subdomain length. SandboxId::subdomain() and the
// proxy's router validator both use this.
pub const SUBDOMAIN_LEN: usize = 12;

// v1.0.2: gRPC metadata key carrying a structured ControllerError /
// ProxyError variant name on Status responses. Senders set it on Status
// trailers; receivers prefer it over the legacy status.code()-based
// per-method mapping. Closes the comp-0 NotFound-collapse finding.
pub const ERROR_CODE_HEADER: &str = "x-os-error-code";

// v1.0.2: maximum entries returned from ListSandboxes. Comp-1 F1 capped
// this server-side; v1.0.2 elevates the cap into the contracts so SDKs
// know to paginate.
pub const LIST_SANDBOXES_MAX: usize = 1000;

// v1.0.3: maximum entries returned from a single ListDirParams session.
// The agent caps server-side; the response's `truncated` flag tells the
// UI to render a "drill in" affordance instead of attempting to display
// an unbounded directory. Picked to comfortably exceed real source-tree
// directories while staying well under a number that would OOM the
// gateway's per-frame JSON marshalling.
pub const LIST_DIR_MAX_ENTRIES: usize = 5000;

// v1.0.3: server-side clamp on WaitPortListeningParams.timeout_ms. The
// wire field is `uint32` (FOLLOWUPS_v1.0.3 D2 captured the design
// tradeoff); the agent clamps callers to this max so a malicious or
// buggy client can't pin an IoSessions / StreamMux slot on a no-op
// probe loop for hours. Five minutes is comfortably above any real
// in-container dev-server restart time (watchexec restart of a typical
// Node / Vite / Python app is <30s wall-clock).
pub const WAIT_PORT_LISTENING_MAX_TIMEOUT_MS: u32 = 300_000;

// v1.0.3: agent probe interval inside drive_wait_port_listening. Each
// iteration is a non-blocking TCP connect attempt; 50ms is fast enough
// that the UI's save-chain p50 doesn't lag behind watchexec and slow
// enough that the agent isn't constantly thrashing the kernel.
pub const WAIT_PORT_LISTENING_PROBE_INTERVAL_MS: u64 = 50;

// =====  WebSocket subprotocol auth (browser clients) =====
//
// Browser `WebSocket` constructors cannot attach an `Authorization`
// header, so the API gateway accepts an equivalent credential on the
// `Sec-WebSocket-Protocol` offered-protocols list:
//
//   Sec-WebSocket-Protocol: open-sandbox.v1, bearer.<base64url-no-pad(api_key)>
//
// The server validates the bearer entry, then echoes the sentinel
// (NOT the bearer value) back in the 101 response. The base64url-no-pad
// encoding keeps the value inside the RFC 7230 token grammar that
// `new WebSocket()` enforces on subprotocol entries.

/// Subprotocol the server echoes back when WS subprotocol auth succeeds.
/// Picking a fixed sentinel (instead of echoing the offered bearer entry)
/// keeps the shared API key out of response headers, access logs, and
/// browser DevTools network panels.
pub const WS_AUTH_PROTOCOL_SENTINEL: &str = "open-sandbox.v1";

/// Subprotocol prefix that carries a base64url-no-padding-encoded API
/// key. Matched case-insensitively on the server side (HTTP scheme
/// tradition; `Authorization: Bearer` is case-insensitive per RFC 7235).
pub const WS_AUTH_BEARER_PREFIX: &str = "bearer.";

/// Per-request cap on the number of offered subprotocol entries the
/// auth helper inspects. Prevents pre-auth algorithmic amplification:
/// an attacker stuffing `bearer.X1, bearer.X2, …, bearer.XN` cannot
/// force more than this many `constant_time_eq` calls per upgrade.
pub const WS_AUTH_MAX_OFFERED_PROTOCOLS: usize = 16;

// =====  WebSocket frame envelope kinds  =====
//
// One WebSocket binary message = one application frame, prefixed with
// a single u8 `kind` byte. Both the API gateway's encoder
// (`crates/api/src/frame.rs`) AND the ws-client crate import these
// constants from here so the two ends of the wire cannot drift —
// adding a kind in one place and forgetting the other was a real
// regression risk before v1.0.3.
//
// Reserved ranges:
//   0x00..=0x0f — client → server
//   0x10..=0x1f — server → client

// client → server
pub const FRAME_KIND_START: u8 = 0x00;
pub const FRAME_KIND_STDIN: u8 = 0x01;
pub const FRAME_KIND_SIGNAL: u8 = 0x02;
pub const FRAME_KIND_STDIN_EOF: u8 = 0x03;

// server → client
pub const FRAME_KIND_STDOUT: u8 = 0x11;
pub const FRAME_KIND_STDERR: u8 = 0x12;
pub const FRAME_KIND_EXITED: u8 = 0x13;
pub const FRAME_KIND_ERROR: u8 = 0x14;
pub const FRAME_KIND_STARTED: u8 = 0x15;

// v1.0.3 additions
pub const FRAME_KIND_LIST_DIR_RESULT: u8 = 0x16;
pub const FRAME_KIND_WAIT_PORT_LISTENING_RESULT: u8 = 0x17;
pub const FRAME_KIND_FILE_META: u8 = 0x18;
