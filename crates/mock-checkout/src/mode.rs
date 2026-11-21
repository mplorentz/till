//! The four programmable checkout modes (spec §9).

use std::fmt;
use std::str::FromStr;

/// Default `short_hold` checkout TTL, in seconds (spec §9).
pub const DEFAULT_HOLD_SECONDS: u64 = 60;

/// One of the four programmable modes described in `docs/SPEC.md` §9.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Mode {
    /// Order reference on submit → broker `succeeded`.
    Success,
    /// "Card declined" error on submit → broker `failed_no_charge`.
    Declined,
    /// Indefinite "processing…" spinner on submit → broker `ambiguous`.
    Ambiguous,
    /// 60-second checkout TTL → broker `approval_timeout` if nobody decides.
    ShortHold,
}

impl Mode {
    /// Every mode, in spec-table order.
    pub const ALL: [Mode; 4] = [
        Mode::Success,
        Mode::Declined,
        Mode::Ambiguous,
        Mode::ShortHold,
    ];

    /// The wire name used in query params and config.
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Success => "success",
            Mode::Declined => "declined",
            Mode::Ambiguous => "ambiguous",
            Mode::ShortHold => "short_hold",
        }
    }

    /// Parse a mode from a `?mode=` query value. Returns `None` for unknown
    /// values so the caller can fall back to the configured default.
    pub fn from_query(value: &str) -> Option<Mode> {
        match value.trim().to_ascii_lowercase().as_str() {
            "success" => Some(Mode::Success),
            "declined" => Some(Mode::Declined),
            "ambiguous" => Some(Mode::Ambiguous),
            "short_hold" => Some(Mode::ShortHold),
            _ => None,
        }
    }
}

impl fmt::Display for Mode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Mode {
    type Err = UnknownMode;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Mode::from_query(value).ok_or_else(|| UnknownMode(value.trim().to_string()))
    }
}

/// Error returned when a mode name is not one of the four spec modes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownMode(pub String);

impl fmt::Display for UnknownMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "unknown checkout mode {:?} (expected one of: success, declined, ambiguous, short_hold)",
            self.0
        )
    }
}

impl std::error::Error for UnknownMode {}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::Config;

    #[test]
    fn query_param_maps_all_four_modes() {
        assert_eq!(Mode::from_query("success"), Some(Mode::Success));
        assert_eq!(Mode::from_query("declined"), Some(Mode::Declined));
        assert_eq!(Mode::from_query("ambiguous"), Some(Mode::Ambiguous));
        assert_eq!(Mode::from_query("short_hold"), Some(Mode::ShortHold));

        // Tolerant of case and surrounding whitespace.
        assert_eq!(Mode::from_query(" Short_Hold "), Some(Mode::ShortHold));

        // Unknown / missing query values fall back to the config default.
        assert_eq!(Mode::from_query("bogus"), None);
        assert_eq!(Mode::from_query(""), None);

        let config = Config::new("unused-stats.jsonl");
        assert_eq!(config.mode_for(None), Mode::Success);
        assert_eq!(config.mode_for(Some("bogus")), Mode::Success);
        assert_eq!(config.mode_for(Some("declined")), Mode::Declined);

        let declined_by_default = Config {
            default_mode: Mode::Declined,
            ..Config::new("unused-stats.jsonl")
        };
        assert_eq!(declined_by_default.mode_for(None), Mode::Declined);
        assert_eq!(
            declined_by_default.mode_for(Some("ambiguous")),
            Mode::Ambiguous,
            "query param wins over the config default"
        );

        for mode in Mode::ALL {
            assert_eq!(
                mode.as_str().parse::<Mode>(),
                Ok(mode),
                "every mode round-trips through its wire name"
            );
        }
    }
}
