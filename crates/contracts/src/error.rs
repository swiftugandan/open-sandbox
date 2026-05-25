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

    #[error("proxy unavailable: {detail}")]
    ProxyUnavailable { detail: String },

    #[error("invalid request: {detail}")]
    InvalidRequest { detail: String },

    #[error("invalid upload: {detail}")]
    InvalidUpload { detail: String },

    // v1.0: streaming I/O. ExecFailed and CommandNotFound were
    // synchronous-exec error variants in v0.x; they have been
    // replaced by IoExited / IoError frames on the WebSocket
    // stream itself. ApiError now models only failures that
    // occur BEFORE the I/O stream is established (upgrade, auth,
    // sandbox lookup) or runtime-level errors observed by the
    // gateway between WS upgrade and stream open.
    #[error("I/O stream failed: {detail}")]
    IoStreamFailed { detail: String },

    #[error("sandbox {sandbox_id} no longer exists (was deleted or its agent disconnected)")]
    SandboxGone { sandbox_id: String },

    #[error("file not found: {resolved_path}")]
    FileNotFound { resolved_path: String },

    #[error("internal error: {detail}")]
    Internal { detail: String },
}

impl ApiError {
    /// v1.0.2 (closes comp-0 wildcard finding): exhaustive match without
    /// a `_ => "UNKNOWN"` fallback. `ApiError` is `#[non_exhaustive]` for
    /// external callers, but inside the defining crate the match is
    /// exhaustive — adding a new variant becomes a compile error here,
    /// which is the intended forcing function.
    pub fn error_code(&self) -> &'static str {
        match self {
            ApiError::Unauthorized { .. } => "UNAUTHORIZED",
            ApiError::SandboxNotFound { .. } => "SANDBOX_NOT_FOUND",
            ApiError::ControllerUnavailable { .. } => "CONTROLLER_UNAVAILABLE",
            ApiError::ProxyUnavailable { .. } => "PROXY_UNAVAILABLE",
            ApiError::InvalidRequest { .. } => "INVALID_REQUEST",
            ApiError::InvalidUpload { .. } => "INVALID_UPLOAD",
            ApiError::IoStreamFailed { .. } => "IO_STREAM_FAILED",
            ApiError::SandboxGone { .. } => "SANDBOX_GONE",
            ApiError::FileNotFound { .. } => "FILE_NOT_FOUND",
            ApiError::Internal { .. } => "INTERNAL_ERROR",
        }
    }
}

#[derive(Debug, Error)]
#[non_exhaustive]
pub enum AgentError {
    #[error("controller connection lost")]
    ControllerDisconnected,

    #[error("proxy tunnel lost")]
    TunnelDisconnected,

    #[error("container runtime error: {detail}")]
    Runtime { detail: String },

    #[error("sandbox {sandbox_id} not found locally")]
    SandboxNotFound { sandbox_id: String },

    #[error("internal error: {detail}")]
    Internal { detail: String },
}
