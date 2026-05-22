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

pub const EXEC_TIMEOUT: Duration = Duration::from_secs(60);

pub const PROXY_STARTUP_RETRY_ATTEMPTS: u32 = 15;

pub const PROXY_STARTUP_RETRY_INTERVAL: Duration = Duration::from_secs(2);
