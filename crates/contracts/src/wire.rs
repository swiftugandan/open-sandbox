//! v1.0.2 amendment types — additive, not breaking.
//!
//! These types tighten the existing wire surface in three places the
//! comp-0 audit flagged:
//!
//!   * [`IoErrorCode`] turns the stringly-typed `IoError.code` field into
//!     a known set of variants. Closes the comp-3 C3 / comp-6 alias hack
//!     where the agent emitted `SANDBOX_NOT_FOUND` and the api collapsed
//!     unknown codes into a generic 500.
//!
//!   * [`Signum`] wraps an `u32` from `IoSignal.signum` so callers can
//!     only construct one in the documented POSIX 1..=31 + RT 34..=64
//!     range. Closes the comp-0 unchecked-signum + the comp-3 A6 inline
//!     `is_valid_signum` check now lives in contracts.
//!
//!   * [`Port`] wraps `u16` for TCP ports the proto wires as `uint32`.
//!     `try_from(u32)` rejects > 65535. Closes the comp-0 exposed_port
//!     u32 finding.
//!
//! Senders continue to write the underlying primitive types on the wire
//! (so v1.0.2 stays wire-compatible with v1.0.1); receivers parse via
//! `TryFrom` to surface invalid values as `Err` early.

use std::fmt;
use std::str::FromStr;

// ─── IoErrorCode ─────────────────────────────────────────────────────

/// Known values of the `IoError.code` wire field. The canonical string
/// form is what goes on the wire (compatible with v1.0.1's free-form
/// string field); the enum exists for type-safe matching in Rust code.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum IoErrorCode {
    RuntimeError,
    SandboxGone,
    ExecFailed,
    ReadFailed,
    WriteFailed,
    FileNotFound,
    InvalidRequest,
    ExtractFailed,
    PayloadTooLarge,
    Cancelled,
    StreamIdReused,
    /// Catch-all for codes the contract doesn't know about. Senders
    /// should still avoid emitting unknown codes; receivers tolerate
    /// them so a downstream crate that runs v1.0.1 forward-compat keeps
    /// working with a future v1.0.3 sender.
    Other(String),
}

impl IoErrorCode {
    pub fn as_str(&self) -> &str {
        match self {
            Self::RuntimeError => "RUNTIME_ERROR",
            Self::SandboxGone => "SANDBOX_GONE",
            Self::ExecFailed => "EXEC_FAILED",
            Self::ReadFailed => "READ_FAILED",
            Self::WriteFailed => "WRITE_FAILED",
            Self::FileNotFound => "FILE_NOT_FOUND",
            Self::InvalidRequest => "INVALID_REQUEST",
            Self::ExtractFailed => "EXTRACT_FAILED",
            Self::PayloadTooLarge => "PAYLOAD_TOO_LARGE",
            Self::Cancelled => "CANCELLED",
            Self::StreamIdReused => "STREAM_ID_REUSED",
            Self::Other(s) => s.as_str(),
        }
    }
}

impl fmt::Display for IoErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl From<&str> for IoErrorCode {
    fn from(s: &str) -> Self {
        match s {
            "RUNTIME_ERROR" => Self::RuntimeError,
            // v1.0.2 closes the comp-3 C3 alias: the agent emits
            // SANDBOX_NOT_FOUND but it means the same as SANDBOX_GONE.
            // Receivers normalize both to the same variant.
            "SANDBOX_GONE" | "SANDBOX_NOT_FOUND" => Self::SandboxGone,
            "EXEC_FAILED" => Self::ExecFailed,
            "READ_FAILED" => Self::ReadFailed,
            "WRITE_FAILED" => Self::WriteFailed,
            "FILE_NOT_FOUND" => Self::FileNotFound,
            "INVALID_REQUEST" => Self::InvalidRequest,
            "EXTRACT_FAILED" => Self::ExtractFailed,
            "PAYLOAD_TOO_LARGE" => Self::PayloadTooLarge,
            "CANCELLED" => Self::Cancelled,
            "STREAM_ID_REUSED" => Self::StreamIdReused,
            other => Self::Other(other.to_string()),
        }
    }
}

impl FromStr for IoErrorCode {
    type Err = std::convert::Infallible;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self::from(s))
    }
}

// ─── Signum ─────────────────────────────────────────────────────────

/// Validated POSIX signal number. Only constructible from values in
/// 1..=31 (POSIX) or 34..=64 (RT). Closes the comp-0 / comp-3 A6
/// finding where a client-supplied `u32` could become `kill -0` (a
/// liveness probe, not a kill) or a runtime-rejected garbage value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Signum(u8);

impl Signum {
    pub const SIGINT: Signum = Signum(2);
    pub const SIGKILL: Signum = Signum(9);
    pub const SIGTERM: Signum = Signum(15);

    pub fn as_u8(self) -> u8 {
        self.0
    }

    pub fn as_i32(self) -> i32 {
        self.0 as i32
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidSignum(pub u32);

impl fmt::Display for InvalidSignum {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "signum {} out of range (expected 1..=31 or 34..=64)",
            self.0
        )
    }
}

impl std::error::Error for InvalidSignum {}

impl TryFrom<u32> for Signum {
    type Error = InvalidSignum;
    fn try_from(value: u32) -> Result<Self, Self::Error> {
        if (1..=31).contains(&value) || (34..=64).contains(&value) {
            Ok(Self(value as u8))
        } else {
            Err(InvalidSignum(value))
        }
    }
}

// ─── Port ───────────────────────────────────────────────────────────

/// Validated TCP port. The protos wire ports as `uint32` (for proto
/// compatibility) but the only valid range is 0..=65535.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Port(u16);

impl Port {
    pub fn new(value: u16) -> Self {
        Self(value)
    }
    pub fn as_u16(self) -> u16 {
        self.0
    }
    pub fn as_u32(self) -> u32 {
        self.0 as u32
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PortOutOfRange(pub u32);

impl fmt::Display for PortOutOfRange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "port {} out of range (must be 0..=65535)",
            self.0
        )
    }
}

impl std::error::Error for PortOutOfRange {}

impl TryFrom<u32> for Port {
    type Error = PortOutOfRange;
    fn try_from(value: u32) -> Result<Self, Self::Error> {
        if value <= u16::MAX as u32 {
            Ok(Self(value as u16))
        } else {
            Err(PortOutOfRange(value))
        }
    }
}

// ─── helpers ────────────────────────────────────────────────────────

/// v1.0.2: validate a routing subdomain. Used by both the proxy's HTTP
/// router and the controller's `SandboxId::subdomain()` so the two
/// can't drift on length / charset rules.
pub fn subdomain_is_valid(s: &str) -> bool {
    s.len() == crate::constants::SUBDOMAIN_LEN && s.chars().all(|c| c.is_ascii_hexdigit())
}

/// v1.0.2: lossy `Duration::as_secs() as u32` was a comp-0 finding.
/// This helper returns Err when the conversion would truncate (Duration
/// < 1s or > u32::MAX seconds).
pub fn try_duration_as_secs_u32(d: std::time::Duration) -> Result<u32, &'static str> {
    let s = d.as_secs();
    if d.subsec_nanos() != 0 {
        return Err("Duration has sub-second component; would truncate");
    }
    u32::try_from(s).map_err(|_| "Duration overflows u32 seconds")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn io_error_code_normalizes_sandbox_not_found_alias() {
        assert_eq!(IoErrorCode::from("SANDBOX_GONE"), IoErrorCode::SandboxGone);
        assert_eq!(
            IoErrorCode::from("SANDBOX_NOT_FOUND"),
            IoErrorCode::SandboxGone
        );
    }

    #[test]
    fn io_error_code_round_trips_known_strings() {
        for s in [
            "RUNTIME_ERROR",
            "EXEC_FAILED",
            "READ_FAILED",
            "WRITE_FAILED",
            "FILE_NOT_FOUND",
            "INVALID_REQUEST",
            "EXTRACT_FAILED",
            "PAYLOAD_TOO_LARGE",
            "CANCELLED",
            "STREAM_ID_REUSED",
        ] {
            assert_eq!(IoErrorCode::from(s).as_str(), s);
        }
    }

    #[test]
    fn io_error_code_preserves_unknown_via_other() {
        match IoErrorCode::from("FUTURE_VARIANT") {
            IoErrorCode::Other(s) => assert_eq!(s, "FUTURE_VARIANT"),
            _ => panic!("expected Other"),
        }
    }

    #[test]
    fn signum_accepts_posix_and_rt_ranges() {
        assert!(Signum::try_from(1).is_ok());
        assert!(Signum::try_from(15).is_ok());
        assert!(Signum::try_from(31).is_ok());
        assert!(Signum::try_from(34).is_ok());
        assert!(Signum::try_from(64).is_ok());
    }

    #[test]
    fn signum_rejects_out_of_range() {
        assert!(Signum::try_from(0).is_err());
        assert!(Signum::try_from(32).is_err());
        assert!(Signum::try_from(33).is_err());
        assert!(Signum::try_from(65).is_err());
        assert!(Signum::try_from(u32::MAX).is_err());
    }

    #[test]
    fn port_accepts_in_range() {
        assert_eq!(Port::try_from(0).unwrap().as_u16(), 0);
        assert_eq!(Port::try_from(8080).unwrap().as_u16(), 8080);
        assert_eq!(Port::try_from(65535).unwrap().as_u16(), 65535);
    }

    #[test]
    fn port_rejects_over_u16() {
        assert!(Port::try_from(65536).is_err());
        assert!(Port::try_from(70000).is_err());
        assert!(Port::try_from(u32::MAX).is_err());
    }

    #[test]
    fn subdomain_is_valid_accepts_12_hex_lowercase() {
        assert!(subdomain_is_valid("abc123def456"));
        assert!(subdomain_is_valid("0123456789ab"));
    }

    #[test]
    fn subdomain_is_valid_rejects_wrong_length() {
        assert!(!subdomain_is_valid("abc"));
        assert!(!subdomain_is_valid("abc123def4567"));
    }

    #[test]
    fn subdomain_is_valid_rejects_non_hex() {
        assert!(!subdomain_is_valid("ghi123def456"));
        assert!(!subdomain_is_valid("abc!23def456"));
    }

    #[test]
    fn try_duration_as_secs_u32_round_trips() {
        use std::time::Duration;
        assert_eq!(try_duration_as_secs_u32(Duration::from_secs(10)).unwrap(), 10);
        assert_eq!(try_duration_as_secs_u32(Duration::from_secs(0)).unwrap(), 0);
    }

    #[test]
    fn try_duration_as_secs_u32_rejects_subsecond() {
        use std::time::Duration;
        assert!(try_duration_as_secs_u32(Duration::from_millis(500)).is_err());
        assert!(try_duration_as_secs_u32(Duration::from_micros(1)).is_err());
    }
}
