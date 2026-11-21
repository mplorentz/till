//! Per-agent API keys (ADR 0006).
//!
//! Every agent has its own key; the broker learns the agent id from the key it
//! was presented, and scope-sensitive decisions (idempotency keys, "is this
//! your payment") hang off that id rather than off anything the caller claims.
//!
//! Fail closed: an empty key set authenticates nobody. A missing
//! `TILL_AGENT_KEYS` therefore disables the API rather than opening it.

use axum::extract::FromRequestParts;
use axum::http::header::AUTHORIZATION;
use axum::http::request::Parts;

use crate::http::{ApiError, AppState};

/// The environment variable holding `agent_id=key` pairs, comma separated.
pub const ENV_VAR: &str = "TILL_AGENT_KEYS";

/// The configured key set.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct ApiKeys {
    entries: Vec<(String, String)>,
}

impl std::fmt::Debug for ApiKeys {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never print the keys themselves (AGENTS.md rule 1, applied to
        // credentials too: a logged API key is a leaked API key).
        formatter
            .debug_struct("ApiKeys")
            .field(
                "agents",
                &self
                    .entries
                    .iter()
                    .map(|(agent, _)| agent)
                    .collect::<Vec<_>>(),
            )
            .finish()
    }
}

impl ApiKeys {
    /// Parse `agent=key,agent=key`.
    ///
    /// # Errors
    ///
    /// Rejects malformed entries, empty ids/keys, and duplicate agent ids.
    pub fn from_spec(spec: &str) -> Result<Self, AuthConfigError> {
        let mut entries: Vec<(String, String)> = Vec::new();
        for entry in spec
            .split(',')
            .map(str::trim)
            .filter(|entry| !entry.is_empty())
        {
            let (agent, key) = entry.split_once('=').ok_or(AuthConfigError::Malformed)?;
            let (agent, key) = (agent.trim(), key.trim());
            if agent.is_empty() || key.is_empty() {
                return Err(AuthConfigError::Malformed);
            }
            if entries.iter().any(|(existing, _)| existing == agent) {
                return Err(AuthConfigError::DuplicateAgent);
            }
            entries.push((agent.to_string(), key.to_string()));
        }
        Ok(Self { entries })
    }

    /// Read the key set from [`ENV_VAR`].
    ///
    /// # Errors
    ///
    /// Returns [`AuthConfigError`] if the variable is set but malformed. An
    /// unset variable yields an empty (nobody authenticates) set.
    pub fn from_env() -> Result<Self, AuthConfigError> {
        match std::env::var(ENV_VAR) {
            Ok(spec) => Self::from_spec(&spec),
            Err(std::env::VarError::NotPresent) => Ok(Self::default()),
            Err(std::env::VarError::NotUnicode(_)) => Err(AuthConfigError::NotUnicode),
        }
    }

    /// Resolve the agent id that owns `token`, if any.
    ///
    /// Compares against every configured key without early exit, so the answer
    /// does not depend on an entry's position. (Key length is not hidden; keys
    /// are generated, not user-chosen.)
    #[must_use]
    pub fn authenticate(&self, token: &str) -> Option<&str> {
        let presented = token.as_bytes();
        let mut found = None;
        for (agent, key) in &self.entries {
            if constant_time_eq(key.as_bytes(), presented) {
                found = Some(agent.as_str());
            }
        }
        found
    }

    /// Whether no agent is configured (the API then refuses everything).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// The key set could not be parsed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum AuthConfigError {
    /// An entry was not `agent=key`.
    #[error("malformed TILL_AGENT_KEYS: expected `agent=key[,agent=key]`")]
    Malformed,
    /// Two entries named the same agent.
    #[error("duplicate agent id in TILL_AGENT_KEYS")]
    DuplicateAgent,
    /// The variable was not valid UTF-8.
    #[error("TILL_AGENT_KEYS is not valid UTF-8")]
    NotUnicode,
}

/// The authenticated agent id, extracted from `Authorization: Bearer <key>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthenticatedAgent(pub String);

impl FromRequestParts<AppState> for AuthenticatedAgent {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let token = parts
            .headers
            .get(AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(bearer_token)
            .ok_or_else(ApiError::unauthorized)?;

        state
            .api_keys()
            .authenticate(token)
            .map(|agent| Self(agent.to_string()))
            .ok_or_else(ApiError::unauthorized)
    }
}

fn bearer_token(value: &str) -> Option<&str> {
    let (scheme, token) = value.split_once(' ')?;
    (scheme.eq_ignore_ascii_case("bearer") && !token.is_empty()).then(|| token.trim())
}

/// XOR-accumulate a comparison so the loop runs for every byte.
fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let mut difference = 0_u8;
    for (lhs, rhs) in left.iter().zip(right) {
        difference |= lhs ^ rhs;
    }
    difference == 0
}

#[cfg(test)]
mod tests {
    use super::{ApiKeys, AuthConfigError, bearer_token, constant_time_eq};

    #[test]
    fn spec_parses_agents_and_authenticates() {
        let keys = ApiKeys::from_spec("jamie=k1, juniper=k2");
        assert!(keys.is_ok());
        let Ok(keys) = keys else { return };
        assert_eq!(keys.authenticate("k1"), Some("jamie"));
        assert_eq!(keys.authenticate("k2"), Some("juniper"));
        assert_eq!(keys.authenticate("k3"), None);
    }

    #[test]
    fn malformed_and_duplicate_specs_are_rejected() {
        assert_eq!(ApiKeys::from_spec("nokey"), Err(AuthConfigError::Malformed));
        assert_eq!(ApiKeys::from_spec("=k"), Err(AuthConfigError::Malformed));
        assert_eq!(
            ApiKeys::from_spec("jamie=k1,jamie=k2"),
            Err(AuthConfigError::DuplicateAgent)
        );
    }

    #[test]
    fn empty_key_set_authenticates_nobody() {
        let Ok(keys) = ApiKeys::from_spec("") else {
            return;
        };
        assert!(keys.is_empty());
        assert_eq!(keys.authenticate(""), None);
    }

    #[test]
    fn bearer_scheme_is_required_and_case_insensitive() {
        assert_eq!(bearer_token("Bearer abc"), Some("abc"));
        assert_eq!(bearer_token("bearer abc"), Some("abc"));
        assert_eq!(bearer_token("Basic abc"), None);
        assert_eq!(bearer_token("Bearer "), None);
        assert_eq!(bearer_token("abc"), None);
    }

    #[test]
    fn constant_time_comparison_matches_equality() {
        assert!(constant_time_eq(b"secret", b"secret"));
        assert!(!constant_time_eq(b"secret", b"secreT"));
        assert!(!constant_time_eq(b"secret", b"secre"));
    }
}
