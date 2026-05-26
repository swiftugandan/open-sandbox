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

/// v1.0.2 (#13): how the agent runtime should treat the image cache
/// when starting a sandbox. Default `IfNotPresent` matches `docker run`
/// semantics (and the agent's current optimized warm path); `Always`
/// restores the previous v1.0.1 behavior of pulling on every start
/// (necessary for floating tags like `:latest`); `Never` is for
/// air-gapped / strict-pin deployments where the image MUST already
/// be on the agent and a missing image should fail fast.
///
/// JSON deserialization accepts `"if-not-present"` (or omitted, which
/// defaults to `IfNotPresent`), `"always"`, and `"never"`. The wire
/// format is the prost-generated `api::PullPolicy` enum; the From/Into
/// conversions below collapse the proto `UNSPECIFIED` zero-value to the
/// safe default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum PullPolicy {
    #[default]
    IfNotPresent,
    Always,
    Never,
}

impl PullPolicy {
    /// Map from the prost-generated wire enum; `Unspecified` collapses
    /// to the default so old clients (proto3 field default = 0) get
    /// the same behavior as new ones that explicitly request
    /// `IfNotPresent`.
    pub fn from_wire(v: crate::api::PullPolicy) -> Self {
        match v {
            crate::api::PullPolicy::Unspecified | crate::api::PullPolicy::IfNotPresent => {
                Self::IfNotPresent
            }
            crate::api::PullPolicy::Always => Self::Always,
            crate::api::PullPolicy::Never => Self::Never,
        }
    }

    /// Map to the wire enum for outbound gRPC requests.
    pub fn to_wire(self) -> crate::api::PullPolicy {
        match self {
            Self::IfNotPresent => crate::api::PullPolicy::IfNotPresent,
            Self::Always => crate::api::PullPolicy::Always,
            Self::Never => crate::api::PullPolicy::Never,
        }
    }
}

/// v1.0.2 (iter10): error returned by `PullPolicy::from_wire_i32_strict`
/// when the wire value doesn't map to a known variant. Carries the
/// raw i32 so the wire boundary (controller's management endpoint)
/// can surface it as `Status::InvalidArgument` with operator-actionable
/// detail.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("unknown PullPolicy wire value {value} (expected 0..=3); refusing to silently downgrade because a future variant could carry stricter semantics than IfNotPresent")]
pub struct UnknownPullPolicy {
    pub value: i32,
}

impl PullPolicy {
    /// v1.0.2 (iter10): fail-closed wire decoding. A newer client may
    /// add `PULL_POLICY_NEVER_OFFLINE = 4` (stricter than `Never`);
    /// silently downgrading to `IfNotPresent` would defeat the
    /// air-gap guarantee for callers who set `Never`. The
    /// controller's management endpoint uses this method at the wire
    /// boundary and rejects unknowns with `Status::InvalidArgument`.
    /// Downstream code paths (e.g. the agent, which trusts the
    /// controller's validation) may keep using `From<i32>` which
    /// preserves the v1.0.2-iter9 defensive fallback.
    ///
    /// (We don't expose this as `TryFrom<i32>` because Rust's blanket
    /// `impl<T,U: Into<T>> TryFrom<U> for T` already produces a free
    /// infallible `TryFrom<i32>` from our `From<i32>` impl, and the
    /// two impls would conflict.)
    pub fn from_wire_i32_strict(v: i32) -> Result<Self, UnknownPullPolicy> {
        crate::api::PullPolicy::try_from(v)
            .map(Self::from_wire)
            .map_err(|_| UnknownPullPolicy { value: v })
    }
}

impl From<i32> for PullPolicy {
    /// Defensive (lossy) wire decoding: unknown values collapse to
    /// the safe default. Use this only at trust boundaries downstream
    /// of a wire validator (the controller validates at ingress via
    /// `from_wire_i32_strict` — the agent then receives a known-good
    /// payload). For the wire-facing path use the strict method
    /// instead.
    fn from(v: i32) -> Self {
        Self::from_wire_i32_strict(v).unwrap_or_default()
    }
}

#[cfg(test)]
mod pull_policy_tests {
    use super::*;

    #[test]
    fn json_default_when_field_omitted() {
        #[derive(Deserialize, Default)]
        struct T {
            #[serde(default)]
            policy: PullPolicy,
        }
        let v: T = serde_json::from_str("{}").unwrap();
        assert_eq!(v.policy, PullPolicy::IfNotPresent);
    }

    #[test]
    fn json_accepts_kebab_case_names() {
        for (input, expected) in [
            (r#"{"policy":"if-not-present"}"#, PullPolicy::IfNotPresent),
            (r#"{"policy":"always"}"#, PullPolicy::Always),
            (r#"{"policy":"never"}"#, PullPolicy::Never),
        ] {
            #[derive(Deserialize)]
            struct T {
                policy: PullPolicy,
            }
            let v: T = serde_json::from_str(input).unwrap();
            assert_eq!(v.policy, expected, "input: {input}");
        }
    }

    #[test]
    fn wire_unspecified_collapses_to_default() {
        assert_eq!(
            PullPolicy::from_wire(crate::api::PullPolicy::Unspecified),
            PullPolicy::IfNotPresent
        );
    }

    #[test]
    fn unknown_i32_collapses_to_default_via_from() {
        // Defensive From<i32> path: unknown values collapse to the
        // safe default. This is used downstream of the wire boundary
        // (e.g. in the agent, which trusts the controller's
        // validation). Behavior preserved from iter9.
        assert_eq!(PullPolicy::from(42), PullPolicy::IfNotPresent);
    }

    #[test]
    fn known_i32_round_trip_via_strict() {
        // Every value the wire enum knows about must round-trip
        // through TryFrom without error.
        for (wire, expected) in [
            (0, PullPolicy::IfNotPresent), // Unspecified → default
            (1, PullPolicy::IfNotPresent),
            (2, PullPolicy::Always),
            (3, PullPolicy::Never),
        ] {
            assert_eq!(
                PullPolicy::from_wire_i32_strict(wire).expect("known wire value"),
                expected,
                "wire={wire}"
            );
        }
    }

    #[test]
    fn unknown_i32_fails_via_strict() {
        // Iter10 fix: a newer client sending PULL_POLICY_NEVER_OFFLINE
        // = 4 (hypothetical, stricter than Never) must NOT be
        // silently downgraded to IfNotPresent. The controller's
        // management endpoint propagates this as
        // Status::InvalidArgument.
        let err = PullPolicy::from_wire_i32_strict(4).expect_err("4 is not a known variant");
        assert_eq!(err.value, 4);
        // Display includes the raw value so operators can pivot off
        // the error message.
        let s = err.to_string();
        assert!(s.contains("4"), "Display missing raw value: {s}");
        assert!(
            s.contains("silently downgrade"),
            "Display missing the load-bearing rationale: {s}"
        );
    }

    #[test]
    fn negative_i32_fails_via_strict() {
        // Defense-in-depth: prost generates `i32` for proto3 enums
        // and its `try_from` rejects values outside the known range.
        // A misencoded negative value must fail-closed too.
        let err = PullPolicy::from_wire_i32_strict(-1).expect_err("negative is not a known variant");
        assert_eq!(err.value, -1);
    }

    /// v1.0.2 (iter10): TRIPWIRE — documents that Rust's std blanket
    /// `impl<T, U: Into<T>> TryFrom<U> for T` synthesizes a free
    /// infallible `TryFrom<i32, Error = Infallible>` for PullPolicy
    /// from our `From<i32>` impl. This means a contributor reaching
    /// for the idiomatic `PullPolicy::try_from(v)` at a wire boundary
    /// gets `Ok(IfNotPresent)` for ANY unknown value — the SILENT
    /// DOWNGRADE iter10 was specifically designed to prevent. The
    /// correct API at wire boundaries is `from_wire_i32_strict`.
    ///
    /// This test exists so that if anyone ever
    ///   (a) removes the lossy `From<i32>` impl, or
    ///   (b) implements an explicit fallible `TryFrom<i32>` (e.g.
    ///       to "fix" the silent-downgrade hazard the test
    ///       documents),
    /// the test will flip and force a deliberate code review
    /// decision instead of silently changing every existing
    /// `.try_from(...)` call site's semantics.
    #[test]
    fn blanket_tryfrom_is_silently_lossy_today() {
        let v: Result<PullPolicy, std::convert::Infallible> = PullPolicy::try_from(42);
        assert_eq!(
            v.expect("blanket TryFrom is infallible while From<i32> exists"),
            PullPolicy::IfNotPresent,
            "if this changes, `try_from` semantics flipped — audit every call site"
        );
    }
}
