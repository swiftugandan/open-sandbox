use thiserror::Error;

#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ControllerError {
    #[error("invalid join token")]
    InvalidToken,

    #[error("agent {agent_id} not found")]
    AgentNotFound { agent_id: String },

    #[error("sandbox {sandbox_id} not found")]
    SandboxNotFound { sandbox_id: String },

    #[error("no agents available with sufficient resources")]
    NoAvailableAgents,

    #[error("database error: {detail}")]
    Database { detail: String },

    #[error("internal error: {detail}")]
    Internal { detail: String },
}

#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ProxyError {
    #[error("sandbox {sandbox_id} not found in routing table")]
    RoutingMiss { sandbox_id: String },

    #[error("tunnel to agent {agent_id} unavailable")]
    TunnelUnavailable { agent_id: String },

    #[error("upstream timeout after {timeout_ms}ms for sandbox {sandbox_id}")]
    UpstreamTimeout { sandbox_id: String, timeout_ms: u64 },

    #[error("upstream rejected request for stream {stream_id}: {reason}")]
    UpstreamRejected { stream_id: String, reason: String },

    #[error("internal error: {detail}")]
    Internal { detail: String },
}

#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ApiError {
    #[error("unauthorized: {detail}")]
    Unauthorized { detail: String },

    #[error("sandbox {sandbox_id} not found")]
    SandboxNotFound { sandbox_id: String },

    #[error("controller unavailable: {detail}")]
    ControllerUnavailable { detail: String },

    #[error("exec failed: {detail}")]
    ExecFailed { detail: String },

    #[error("internal error: {detail}")]
    Internal { detail: String },
}

#[derive(Debug, Error)]
#[non_exhaustive]
pub enum AgentError {
    #[error("controller connection lost")]
    ControllerDisconnected,

    #[error("proxy tunnel lost")]
    TunnelDisconnected,

    #[error("docker error: {detail}")]
    Docker { detail: String },

    #[error("sandbox {sandbox_id} not found locally")]
    SandboxNotFound { sandbox_id: String },

    #[error("internal error: {detail}")]
    Internal { detail: String },
}
