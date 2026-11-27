//! Where the Camofox instance is and who the broker is on it.
//!
//! # Per-`userId` identity (spike Q3)
//!
//! Camofox scopes tabs strictly by `userId`, has no tab-transfer API, and
//! `userId` itself carries no authentication. A tab created under one id is
//! invisible to every other id, so **the broker must run under the agent's
//! `userId`** to touch the tab the agent already drove to the checkout screen.
//!
//! That is a real (accepted) exposure, recorded in ADR 0005: the agent retains
//! DOM access to the tab for the length of the fill→submit window, and it is
//! fixed only by per-principal auth plus tab leasing upstream (ADR 0005
//! Tier 2). The baseline mitigations live in this crate: `/type` for the fill
//! (length-only bus payload on camofox ≥ 1.16), `evaluate` for digit-free
//! operations only, no tracing, and `about:blank` containment at the end.
//!
//! # Configuration
//!
//! Nothing here is version-gated: camofox ≥ 1.16 is a **documented deployment
//! prerequisite** (SPEC §7, ADR 0005), asserted by ops, not by the broker
//! (overseer correction, 2026-11-21). The URL is config-declared because the
//! guess `http://localhost:9377` is wrong on the test host, where only the
//! docker-bridge address answers (spike Environment).

use secrecy::SecretString;

/// Environment variable naming the Camofox base URL.
pub const URL_ENV_VAR: &str = "TILL_CAMOFOX_URL";

/// Environment variable naming the `userId` the broker acts as.
pub const USER_ID_ENV_VAR: &str = "TILL_CAMOFOX_USER_ID";

/// Environment variable naming the optional access key (camofox ≥ 1.16
/// access-key middleware; unset on a stock local instance).
pub const ACCESS_KEY_ENV_VAR: &str = "TILL_CAMOFOX_ACCESS_KEY";

/// Used only when `TILL_CAMOFOX_URL` is unset, i.e. in local development.
///
/// The test deployment must set the variable: only the docker-bridge address
/// answers there (spike Environment).
pub const DEV_DEFAULT_URL: &str = "http://localhost:9377";

/// Anything wrong with the configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ConfigError {
    /// The base URL is not an absolute `http(s)` URL.
    #[error("camofox base url must be an absolute http(s) url")]
    Url,
    /// No `userId` was configured. The broker must share the agent's id
    /// (spike Q3) and will not invent one.
    #[error("a camofox user id is required")]
    UserId,
}

/// How to reach Camofox and which `userId` to act as.
#[derive(Debug, Clone)]
pub struct CamofoxConfig {
    base_url: String,
    user_id: String,
    access_key: Option<SecretString>,
}

impl CamofoxConfig {
    /// Build a configuration, validating the URL shape.
    ///
    /// # Errors
    ///
    /// [`ConfigError::Url`] if `base_url` is not absolute `http(s)`, or
    /// [`ConfigError::UserId`] if `user_id` is blank.
    pub fn new(
        base_url: impl Into<String>,
        user_id: impl Into<String>,
    ) -> Result<Self, ConfigError> {
        let base_url = base_url.into();
        let trimmed = base_url.trim();
        if !(trimmed.starts_with("http://") || trimmed.starts_with("https://")) {
            return Err(ConfigError::Url);
        }
        let user_id = user_id.into();
        if user_id.trim().is_empty() {
            return Err(ConfigError::UserId);
        }
        Ok(Self {
            base_url: trimmed.trim_end_matches('/').to_string(),
            user_id,
            access_key: None,
        })
    }

    /// Read the deployment's configuration from the environment.
    ///
    /// `TILL_CAMOFOX_URL` falls back to [`DEV_DEFAULT_URL`] for local work;
    /// `TILL_CAMOFOX_USER_ID` has no default, because guessing the identity
    /// would silently address the wrong `userId`'s tabs (spike Q3).
    ///
    /// # Errors
    ///
    /// [`ConfigError`] as for [`CamofoxConfig::new`].
    pub fn from_env() -> Result<Self, ConfigError> {
        let url = std::env::var(URL_ENV_VAR)
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| DEV_DEFAULT_URL.to_string());
        let user_id = std::env::var(USER_ID_ENV_VAR).unwrap_or_default();
        let mut config = Self::new(url, user_id)?;
        if let Some(key) = std::env::var(ACCESS_KEY_ENV_VAR)
            .ok()
            .filter(|value| !value.trim().is_empty())
        {
            config = config.with_access_key(key);
        }
        Ok(config)
    }

    /// Attach the camofox access key (ADR 0005 Tier 1 ops hardening).
    #[must_use]
    pub fn with_access_key(mut self, key: impl Into<String>) -> Self {
        self.access_key = Some(SecretString::from(key.into()));
        self
    }

    /// The base URL, without a trailing slash.
    #[must_use]
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// The `userId` every request carries (spike Q3: it must be the agent's).
    #[must_use]
    pub fn user_id(&self) -> &str {
        &self.user_id
    }

    /// The optional access key. Never logged, never serialized.
    #[must_use]
    pub fn access_key(&self) -> Option<&SecretString> {
        self.access_key.as_ref()
    }
}

#[cfg(test)]
mod tests {
    use super::{CamofoxConfig, ConfigError};

    #[test]
    fn url_must_be_absolute_http_and_trailing_slash_is_trimmed() -> Result<(), ConfigError> {
        assert_eq!(
            CamofoxConfig::new("ftp://camofox:9377", "jamie").map(|_| ()),
            Err(ConfigError::Url),
            "only http(s) is accepted"
        );
        assert_eq!(
            CamofoxConfig::new("172.17.0.1:9377", "jamie").map(|_| ()),
            Err(ConfigError::Url),
            "a bare host is not an absolute url"
        );
        let config = CamofoxConfig::new("http://172.17.0.1:9377/", "jamie")?;
        assert_eq!(config.base_url(), "http://172.17.0.1:9377");
        assert_eq!(config.user_id(), "jamie");
        Ok(())
    }

    #[test]
    fn user_id_is_required_because_tabs_are_scoped_to_it() {
        assert_eq!(
            CamofoxConfig::new("http://localhost:9377", "   ").map(|_| ()),
            Err(ConfigError::UserId),
            "the broker must not invent a userId (spike Q3)"
        );
    }

    #[test]
    fn config_never_prints_the_access_key() -> Result<(), ConfigError> {
        let config = CamofoxConfig::new("http://localhost:9377", "jamie")?
            .with_access_key("super-secret-access-key");
        let rendered = format!("{config:?}");
        assert!(
            !rendered.contains("super-secret-access-key"),
            "the access key must not reach a log line: {rendered}"
        );
        Ok(())
    }
}
