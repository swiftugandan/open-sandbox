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

pub const DEFAULT_SANDBOX_ENTRYPOINT: &[&str] = &["sleep", "infinity"];

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
