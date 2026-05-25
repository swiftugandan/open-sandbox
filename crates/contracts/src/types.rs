use serde::{Deserialize, Serialize};
use std::fmt;
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AgentId(pub Uuid);

impl AgentId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for AgentId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for AgentId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl From<Uuid> for AgentId {
    fn from(id: Uuid) -> Self {
        Self(id)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SandboxId(pub Uuid);

impl SandboxId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }

    /// v1.0.2: use the shared `SUBDOMAIN_LEN` constant rather than the
    /// hard-coded `12`. Closes the comp-0 finding that the generator
    /// here and the proxy's router could drift on length.
    pub fn subdomain(&self) -> String {
        self.0.simple().to_string()[..crate::constants::SUBDOMAIN_LEN].to_string()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidSandboxId;

impl fmt::Display for InvalidSandboxId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("invalid SandboxId (must be a UUID)")
    }
}

impl std::error::Error for InvalidSandboxId {}

impl std::str::FromStr for SandboxId {
    type Err = InvalidSandboxId;
    /// v1.0.2 (comp-0): wire-side validator. Closes the finding that
    /// `string sandbox_id` was free-form on the proto side and every
    /// downstream had to remember to revalidate.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Uuid::parse_str(s).map(Self).map_err(|_| InvalidSandboxId)
    }
}

impl TryFrom<&str> for SandboxId {
    type Error = InvalidSandboxId;
    fn try_from(s: &str) -> Result<Self, Self::Error> {
        s.parse()
    }
}

impl std::str::FromStr for AgentId {
    type Err = InvalidSandboxId; // shared error shape
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Uuid::parse_str(s).map(Self).map_err(|_| InvalidSandboxId)
    }
}

impl TryFrom<&str> for AgentId {
    type Error = InvalidSandboxId;
    fn try_from(s: &str) -> Result<Self, Self::Error> {
        s.parse()
    }
}

impl Default for SandboxId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for SandboxId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl From<Uuid> for SandboxId {
    fn from(id: Uuid) -> Self {
        Self(id)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct JoinToken(pub String);

impl JoinToken {
    pub fn new(token: String) -> Self {
        Self(token)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for JoinToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "JoinToken(***)")
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ApiKey(pub String);

impl ApiKey {
    pub fn new(key: String) -> Self {
        Self(key)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ApiKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ApiKey(***)")
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoutingEntry {
    pub sandbox_id: SandboxId,
    pub agent_id: AgentId,
}
