//! `dice-logging` — tracing initialization for Dice services.
//!
//! Thin by design (see `docs/design/workspace-and-protocol.md` §5.7): every
//! service binary calls [`LogConfig::from_env`] + [`init`] exactly once at
//! startup. The filter comes from `RUST_LOG` (falling back to
//! [`DEFAULT_FILTER`]) and the output format is human-readable by default,
//! switching to newline-delimited JSON when `DICE_LOG_JSON=1`.
//!
//! [`init`] uses `try_init` internally, so calling it twice (e.g. from
//! multiple `#[test]`s in one process) never panics — the second call is a
//! no-op that returns `false`.

use tracing_subscriber::EnvFilter;

/// Filter applied when `RUST_LOG` is unset or empty.
pub const DEFAULT_FILTER: &str = "info,dice=debug";

/// Output format for log events.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogFormat {
    /// Human-readable single-line output with ANSI colors (dev default).
    Pretty,
    /// Newline-delimited JSON, one event per line (production / log shippers).
    Json,
}

/// Logging configuration, normally built via [`LogConfig::from_env`].
#[derive(Debug, Clone)]
pub struct LogConfig {
    /// A `tracing_subscriber::EnvFilter` directive string,
    /// e.g. `"info,dice=debug"`.
    pub filter: String,
    /// Output format.
    pub format: LogFormat,
}

impl LogConfig {
    /// Builds the configuration from the environment:
    ///
    /// - `filter`: `RUST_LOG` if set and non-empty, else [`DEFAULT_FILTER`].
    /// - `format`: [`LogFormat::Json`] when `DICE_LOG_JSON=1`, else
    ///   [`LogFormat::Pretty`].
    #[must_use]
    pub fn from_env() -> Self {
        Self {
            filter: filter_from(std::env::var("RUST_LOG").ok()),
            format: format_from(std::env::var("DICE_LOG_JSON").ok()),
        }
    }
}

impl Default for LogConfig {
    fn default() -> Self {
        Self {
            filter: DEFAULT_FILTER.to_owned(),
            format: LogFormat::Pretty,
        }
    }
}

fn filter_from(rust_log: Option<String>) -> String {
    match rust_log {
        Some(s) if !s.trim().is_empty() => s,
        _ => DEFAULT_FILTER.to_owned(),
    }
}

fn format_from(dice_log_json: Option<String>) -> LogFormat {
    if dice_log_json.as_deref() == Some("1") {
        LogFormat::Json
    } else {
        LogFormat::Pretty
    }
}

/// Installs the global tracing subscriber described by `cfg`.
///
/// Returns `true` if this call installed the subscriber, `false` if a global
/// subscriber was already set (the call is then a no-op — it never panics, so
/// repeated initialization in tests is safe).
///
/// An invalid `cfg.filter` directive falls back to [`DEFAULT_FILTER`] with a
/// warning on stderr rather than failing startup.
pub fn init(cfg: &LogConfig) -> bool {
    let filter = EnvFilter::try_new(&cfg.filter).unwrap_or_else(|err| {
        eprintln!(
            "dice-logging: invalid filter directive {:?} ({err}); falling back to {:?}",
            cfg.filter, DEFAULT_FILTER
        );
        EnvFilter::new(DEFAULT_FILTER)
    });

    match cfg.format {
        LogFormat::Pretty => tracing_subscriber::fmt()
            .with_env_filter(filter)
            .try_init()
            .is_ok(),
        LogFormat::Json => tracing_subscriber::fmt()
            .json()
            .with_env_filter(filter)
            .try_init()
            .is_ok(),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    // The only test that calls `init` (a process-global side effect) so the
    // assertions stay deterministic under parallel test execution.
    #[test]
    fn init_twice_does_not_panic() {
        let cfg = LogConfig::default();
        let first = init(&cfg);
        let second = init(&cfg);
        assert!(first, "first init should install the subscriber");
        assert!(!second, "second init must be a no-op, not a panic");
        // Emitting through the installed subscriber must not panic either.
        tracing::info!("dice-logging init smoke test");

        // An invalid filter directive must fall back, not panic, even when a
        // subscriber is already installed.
        let invalid = LogConfig {
            filter: "not a [valid] directive!!!".to_owned(),
            format: LogFormat::Json,
        };
        assert!(!init(&invalid));
    }

    #[test]
    fn filter_falls_back_to_default() {
        assert_eq!(filter_from(None), DEFAULT_FILTER);
        assert_eq!(filter_from(Some(String::new())), DEFAULT_FILTER);
        assert_eq!(filter_from(Some("   ".to_owned())), DEFAULT_FILTER);
        assert_eq!(filter_from(Some("warn".to_owned())), "warn");
    }

    #[test]
    fn format_selected_by_dice_log_json() {
        assert_eq!(format_from(None), LogFormat::Pretty);
        assert_eq!(format_from(Some("0".to_owned())), LogFormat::Pretty);
        assert_eq!(format_from(Some("true".to_owned())), LogFormat::Pretty);
        assert_eq!(format_from(Some("1".to_owned())), LogFormat::Json);
    }
}
