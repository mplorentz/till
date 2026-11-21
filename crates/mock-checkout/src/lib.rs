//! Local mock checkout site (spec §9; bead till-6q0).
//!
//! Four programmable modes:
//! - `success`     → order reference on submit
//! - `declined`     → "card declined" error
//! - `ambiguous`    → indefinite "processing…" spinner
//! - `short_hold`   → 60-second checkout TTL
//!
//! Must expose a submission counter (`/stats`) so tests can assert the
//! submit button was clicked exactly once per payment — including across
//! broker restarts.
//!
//! Routes:
//! - `GET  /checkout?mode=<mode>`                     → the checkout form
//! - `POST /checkout?mode=<mode>[&token=…]`           → mode-specific result
//! - `GET  /stats`                                     → `{"total":N,"by_mode":{…}}`
//! - `GET  /`                                          → redirect to `/checkout`
//!
//! The mode comes from the `?mode=` query param, falling back to the
//! `MOCK_CHECKOUT_MODE` default; `short_hold` additionally honours
//! `?hold_seconds=` so tests need not actually wait the full 60 seconds.
//!
//! The site is the Docker compose target for the later end-to-end tests
//! (bead till-8). It never logs or persists submitted form values: card
//! fields are radioactive here too (AGENTS.md rule 1).

pub mod mode;
pub mod pages;
pub mod routes;
pub mod stats;

use std::fmt;
use std::path::PathBuf;

pub use mode::{DEFAULT_HOLD_SECONDS, Mode};
pub use routes::{AppState, router};
pub use stats::{StatsSnapshot, SubmissionCounter};

/// Default listen address (`MOCK_CHECKOUT_BIND`).
pub const DEFAULT_BIND: &str = "127.0.0.1:9390";

/// Default append-only counter file (`MOCK_CHECKOUT_STATS_FILE`).
pub const DEFAULT_STATS_FILE: &str = "mock-checkout-stats.jsonl";

/// Runtime configuration, resolved from the environment in `from_env`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// Address the site binds to.
    pub bind_addr: String,
    /// Mode used when `GET /checkout` carries no usable `?mode=`.
    pub default_mode: Mode,
    /// Append-only submission-counter file (survives restarts).
    pub stats_file: PathBuf,
    /// `short_hold` checkout TTL in seconds.
    pub hold_seconds: u64,
}

impl Config {
    /// Config with defaults, pointing at `stats_file`.
    pub fn new(stats_file: impl Into<PathBuf>) -> Self {
        Self {
            bind_addr: DEFAULT_BIND.to_string(),
            default_mode: Mode::Success,
            stats_file: stats_file.into(),
            hold_seconds: DEFAULT_HOLD_SECONDS,
        }
    }

    /// Read `MOCK_CHECKOUT_BIND`, `MOCK_CHECKOUT_MODE`,
    /// `MOCK_CHECKOUT_STATS_FILE` and `MOCK_CHECKOUT_HOLD_SECONDS`.
    pub fn from_env() -> Result<Self, ConfigError> {
        let mut config = Self::new(DEFAULT_STATS_FILE);

        if let Some(bind) = std::env::var("MOCK_CHECKOUT_BIND")
            .ok()
            .filter(|b| !b.is_empty())
        {
            config.bind_addr = bind;
        }

        if let Some(raw) = std::env::var("MOCK_CHECKOUT_MODE")
            .ok()
            .filter(|m| !m.is_empty())
        {
            config.default_mode = raw
                .parse::<Mode>()
                .map_err(|error| ConfigError::InvalidMode(error.to_string()))?;
        }

        if let Some(raw) = std::env::var("MOCK_CHECKOUT_STATS_FILE")
            .ok()
            .filter(|f| !f.is_empty())
        {
            config.stats_file = PathBuf::from(raw);
        }

        if let Some(raw) = std::env::var("MOCK_CHECKOUT_HOLD_SECONDS")
            .ok()
            .filter(|h| !h.is_empty())
        {
            config.hold_seconds = raw
                .parse::<u64>()
                .map_err(|_| ConfigError::InvalidNumber("MOCK_CHECKOUT_HOLD_SECONDS", raw))?;
        }

        Ok(config)
    }

    /// Resolve the mode for one request: the query param wins, then the
    /// configured default.
    pub fn mode_for(&self, query_mode: Option<&str>) -> Mode {
        query_mode
            .and_then(Mode::from_query)
            .unwrap_or(self.default_mode)
    }
}

/// Configuration error raised by `Config::from_env`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigError {
    /// `MOCK_CHECKOUT_MODE` was not one of the four spec modes.
    InvalidMode(String),
    /// A numeric environment variable did not parse.
    InvalidNumber(&'static str, String),
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigError::InvalidMode(message) => write!(f, "invalid MOCK_CHECKOUT_MODE: {message}"),
            ConfigError::InvalidNumber(name, value) => {
                write!(f, "{name} must be a non-negative integer, got {value:?}")
            }
        }
    }
}

impl std::error::Error for ConfigError {}
