//! Comp-8: tiny redacted-string newtype used for clap args that hold
//! secrets (API keys, join tokens, database URLs containing passwords).
//! The `Debug` impl prints `<redacted>` so a future
//! `tracing::error!(?args, ...)` during incident triage doesn't ship the
//! secret to the log aggregator.
//!
//! Avoids pulling in the `secrecy` crate for one field-shape.

use std::fmt;
use std::str::FromStr;

#[derive(Clone)]
pub struct Redacted(String);

impl Redacted {
    pub fn expose(&self) -> &str {
        &self.0
    }

    pub fn into_inner(self) -> String {
        self.0
    }
}

impl fmt::Debug for Redacted {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.0.is_empty() {
            f.write_str("Redacted(empty)")
        } else {
            f.write_str("Redacted(<redacted>)")
        }
    }
}

impl FromStr for Redacted {
    type Err = std::convert::Infallible;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self(s.to_string()))
    }
}

impl From<String> for Redacted {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl PartialEq<&str> for Redacted {
    fn eq(&self, other: &&str) -> bool {
        self.0 == *other
    }
}

impl PartialEq<str> for Redacted {
    fn eq(&self, other: &str) -> bool {
        self.0 == other
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_does_not_reveal_value() {
        let s = Redacted::from_str("super-secret-token").unwrap();
        let debug = format!("{s:?}");
        assert!(!debug.contains("super-secret-token"));
        assert!(debug.contains("redacted"));
    }

    #[test]
    fn expose_returns_underlying_string() {
        let s = Redacted::from_str("api-key-123").unwrap();
        assert_eq!(s.expose(), "api-key-123");
    }
}
