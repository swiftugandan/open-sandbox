use open_sandbox_contracts::constants::ERROR_CODE_HEADER;
use open_sandbox_contracts::error::ControllerError;
use tonic::Status;
use tonic::metadata::MetadataValue;
#[cfg(test)]
use tonic::Code;

/// v1.0.2 cascade: every Status the controller surfaces now carries the
/// `x-os-error-code: <VARIANT>` trailer in addition to the tonic Code.
/// The api gateway prefers the trailer over `Code::NotFound`-based
/// per-method mapping, closing the comp-0 NotFound-collapse finding
/// without a wire-protocol break.
pub fn controller_error_to_status(err: &ControllerError) -> Status {
    let (mut status, code) = match err {
        ControllerError::InvalidToken => (
            Status::unauthenticated(err.to_string()),
            "INVALID_TOKEN",
        ),
        ControllerError::SandboxNotFound { sandbox_id } => (
            Status::not_found(sandbox_id.clone()),
            "SANDBOX_NOT_FOUND",
        ),
        ControllerError::AgentNotFound { agent_id } => (
            Status::failed_precondition(format!("agent {agent_id} not available")),
            "AGENT_NOT_FOUND",
        ),
        ControllerError::NoAvailableAgents => (
            Status::resource_exhausted(err.to_string()),
            "NO_AVAILABLE_AGENTS",
        ),
        ControllerError::Database { detail } => (
            Status::internal(format!("database error: {detail}")),
            "DATABASE_ERROR",
        ),
        ControllerError::Internal { detail } => (Status::internal(detail.clone()), "INTERNAL"),
        // ControllerError is #[non_exhaustive]; new variants land here.
        // If you add one, add a dedicated arm above so callers can match
        // on a known wire-string instead of bucketing into UNKNOWN.
        other => {
            tracing::warn!(error = %other, "unmapped ControllerError variant; defaulting to Internal");
            (Status::internal(other.to_string()), "UNKNOWN")
        }
    };
    if let Ok(value) = MetadataValue::try_from(code) {
        status.metadata_mut().insert(ERROR_CODE_HEADER, value);
    }
    status
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_token_maps_to_unauthenticated() {
        let s = controller_error_to_status(&ControllerError::InvalidToken);
        assert_eq!(s.code(), Code::Unauthenticated);
    }

    #[test]
    fn sandbox_not_found_maps_to_not_found_with_id_in_message() {
        let s = controller_error_to_status(&ControllerError::SandboxNotFound {
            sandbox_id: "abc123".into(),
        });
        assert_eq!(s.code(), Code::NotFound);
        assert_eq!(s.message(), "abc123");
    }

    #[test]
    fn agent_not_found_maps_to_failed_precondition() {
        let s = controller_error_to_status(&ControllerError::AgentNotFound {
            agent_id: "node-7".into(),
        });
        assert_eq!(s.code(), Code::FailedPrecondition);
        assert!(s.message().contains("node-7"));
    }

    #[test]
    fn no_available_agents_maps_to_resource_exhausted() {
        let s = controller_error_to_status(&ControllerError::NoAvailableAgents);
        assert_eq!(s.code(), Code::ResourceExhausted);
    }

    #[test]
    fn database_error_maps_to_internal() {
        let s = controller_error_to_status(&ControllerError::Database {
            detail: "pool exhausted".into(),
        });
        assert_eq!(s.code(), Code::Internal);
        assert!(s.message().contains("pool exhausted"));
    }

    #[test]
    fn internal_error_maps_to_internal() {
        let s = controller_error_to_status(&ControllerError::Internal {
            detail: "boom".into(),
        });
        assert_eq!(s.code(), Code::Internal);
        assert_eq!(s.message(), "boom");
    }

    #[test]
    fn status_carries_x_os_error_code_trailer() {
        // v1.0.2 cascade: every Status should expose the structured
        // variant via metadata so the api gateway can map without
        // guessing from tonic Code alone.
        let s = controller_error_to_status(&ControllerError::NoAvailableAgents);
        let header = s
            .metadata()
            .get(ERROR_CODE_HEADER)
            .expect("x-os-error-code header missing");
        assert_eq!(header.to_str().unwrap(), "NO_AVAILABLE_AGENTS");

        let s = controller_error_to_status(&ControllerError::AgentNotFound {
            agent_id: "x".into(),
        });
        assert_eq!(
            s.metadata().get(ERROR_CODE_HEADER).unwrap().to_str().unwrap(),
            "AGENT_NOT_FOUND"
        );
    }
}
