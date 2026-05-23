use thiserror::Error;
use tonic::{Request, Status, service::Interceptor};

/// Maximum number of routing entries returned from any management ListSandboxes
/// call. Without a max_results field in ListSandboxesRequest (contracts/v1.0.1
/// is frozen), the controller caps server-side; entries beyond this are silently
/// truncated. See REVIEW_LOG.md F1 — proper pagination is deferred to a
/// contract bump.
pub const LIST_SANDBOXES_MAX: usize = 1000;

#[derive(Debug, Error)]
pub enum AuthInitError {
    #[error("CONTROLLER_ADMIN_TOKEN is not set in the environment")]
    TokenUnset,
    #[error("CONTROLLER_ADMIN_TOKEN is set but empty")]
    TokenEmpty,
}

/// gRPC interceptor that requires `authorization: Bearer <token>` on every
/// call. The token is supplied via the CONTROLLER_ADMIN_TOKEN env var at
/// startup; the comparison is constant-time so a request handler doesn't
/// leak the token via timing differences.
///
/// See REVIEW_LOG.md F1: this is the minimum-viable single-tenant auth.
/// Per-sandbox ownership (multi-tenancy) is a SPEC question that remains
/// open in CLAUDE.md and is out of scope for this PR.
#[derive(Clone, Debug)]
pub struct AdminAuthInterceptor {
    expected_token: String,
}

impl AdminAuthInterceptor {
    pub fn from_env() -> Result<Self, AuthInitError> {
        let token = std::env::var("CONTROLLER_ADMIN_TOKEN")
            .map_err(|_| AuthInitError::TokenUnset)?;
        if token.is_empty() {
            return Err(AuthInitError::TokenEmpty);
        }
        Ok(Self {
            expected_token: token,
        })
    }

    pub fn new(expected_token: impl Into<String>) -> Self {
        Self {
            expected_token: expected_token.into(),
        }
    }
}

impl Interceptor for AdminAuthInterceptor {
    fn call(&mut self, req: Request<()>) -> Result<Request<()>, Status> {
        let header = req
            .metadata()
            .get("authorization")
            .ok_or_else(|| Status::unauthenticated("missing authorization header"))?
            .to_str()
            .map_err(|_| Status::unauthenticated("authorization header is not ascii"))?;

        let token = header
            .strip_prefix("Bearer ")
            .ok_or_else(|| Status::unauthenticated("authorization must use Bearer scheme"))?;

        if constant_time_eq(token.as_bytes(), self.expected_token.as_bytes()) {
            Ok(req)
        } else {
            Err(Status::unauthenticated("invalid token"))
        }
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use tonic::metadata::MetadataValue;

    fn make_request(auth: Option<&str>) -> Request<()> {
        let mut req = Request::new(());
        if let Some(value) = auth {
            req.metadata_mut().insert(
                "authorization",
                MetadataValue::try_from(value).unwrap(),
            );
        }
        req
    }

    #[test]
    fn accepts_correct_bearer_token() {
        let mut interceptor = AdminAuthInterceptor::new("s3cret");
        let req = make_request(Some("Bearer s3cret"));
        assert!(interceptor.call(req).is_ok());
    }

    #[test]
    fn rejects_missing_authorization_header() {
        let mut interceptor = AdminAuthInterceptor::new("s3cret");
        let err = interceptor.call(make_request(None)).unwrap_err();
        assert_eq!(err.code(), tonic::Code::Unauthenticated);
    }

    #[test]
    fn rejects_non_bearer_scheme() {
        let mut interceptor = AdminAuthInterceptor::new("s3cret");
        let err = interceptor
            .call(make_request(Some("Basic s3cret")))
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::Unauthenticated);
    }

    #[test]
    fn rejects_wrong_token() {
        let mut interceptor = AdminAuthInterceptor::new("s3cret");
        let err = interceptor
            .call(make_request(Some("Bearer wrong")))
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::Unauthenticated);
    }

    #[test]
    fn constant_time_eq_matches_std_eq() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"abcd"));
        assert!(!constant_time_eq(b"", b"a"));
        assert!(constant_time_eq(b"", b""));
    }

    #[test]
    fn from_env_rejects_unset() {
        // Use a unique-ish var name to avoid collision with concurrent tests.
        unsafe { std::env::remove_var("CONTROLLER_ADMIN_TOKEN") };
        let err = AdminAuthInterceptor::from_env().unwrap_err();
        assert!(matches!(err, AuthInitError::TokenUnset));
    }
}
