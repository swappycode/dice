//! Tiny typed env-var loader (ADR-0002: env vars only, no config framework).
//! Every variable is documented in `.env.example`.

use std::fmt;
use std::str::FromStr;

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("required env var {0} is not set")]
    Missing(&'static str),
    #[error("env var {key} has invalid value {value:?}: {reason}")]
    Invalid {
        key: &'static str,
        value: String,
        reason: String,
    },
}

/// Read and parse, falling back to `default` when unset. Panics never; a
/// malformed value is an error (silent fallback would mask typos).
pub fn env_or<T: FromStr>(key: &'static str, default: T) -> Result<T, ConfigError>
where
    T::Err: fmt::Display,
{
    match std::env::var(key) {
        Ok(v) => v.parse().map_err(|e: T::Err| ConfigError::Invalid {
            key,
            value: v,
            reason: e.to_string(),
        }),
        Err(_) => Ok(default),
    }
}

pub fn env_opt(key: &'static str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.is_empty())
}

pub fn require(key: &'static str) -> Result<String, ConfigError> {
    std::env::var(key).map_err(|_| ConfigError::Missing(key))
}

/// `DICE_PROFILE`: selects bus/cache defaults; per-backend overrides win.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DiceProfile {
    /// In-proc bus + memory cache; docker Postgres only.
    DevLite,
    /// NATS bus + Redis cache.
    #[default]
    Full,
}

impl FromStr for DiceProfile {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "dev-lite" | "devlite" | "dev_lite" => Ok(Self::DevLite),
            "full" | "prod" => Ok(Self::Full),
            other => Err(format!("expected dev-lite|full, got {other:?}")),
        }
    }
}

impl DiceProfile {
    pub fn from_env() -> Result<Self, ConfigError> {
        env_or("DICE_PROFILE", Self::Full)
    }
}
