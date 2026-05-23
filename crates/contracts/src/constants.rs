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

pub const PROXY_STARTUP_RETRY_ATTEMPTS: u32 = 15;

pub const PROXY_STARTUP_RETRY_INTERVAL: Duration = Duration::from_secs(2);

pub const DEFAULT_WRITE_CWD: &str = "/home";

pub const DEFAULT_SANDBOX_ENTRYPOINT: &[&str] = &["sleep", "infinity"];
