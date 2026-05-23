use open_sandbox_contracts::error::ControllerError;
use tonic::Status;
#[cfg(test)]
use tonic::Code;

pub fn controller_error_to_status(err: &ControllerError) -> Status {
    match err {
        ControllerError::InvalidToken => Status::unauthenticated(err.to_string()),
        ControllerError::SandboxNotFound { sandbox_id } => Status::not_found(sandbox_id.clone()),
        ControllerError::AgentNotFound { agent_id } => {
            Status::failed_precondition(format!("agent {agent_id} not available"))
        }
        ControllerError::NoAvailableAgents => Status::resource_exhausted(err.to_string()),
        ControllerError::Database { detail } => {
            Status::internal(format!("database error: {detail}"))
        }
        ControllerError::Internal { detail } => Status::internal(detail.clone()),
        // ControllerError is #[non_exhaustive]; new variants land here as Internal.
        // If you add a variant, add a dedicated arm above — Internal is a fallback,
        // not a contract.
        other => {
            tracing::warn!(error = %other, "unmapped ControllerError variant; defaulting to Internal");
            Status::internal(other.to_string())
        }
    }
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
}
