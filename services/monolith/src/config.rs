//! Monolith configuration: env vars only (ADR-0002, critique #21).
//!
//! Every variable below is documented in the workspace `.env.example`.
//! `just dev` / `just run-full` load `.env` via `dotenv-load`, so a plain
//! `cargo run -p dice-monolith` needs the variables in the environment.
//!
//! | Variable               | Default                       | Meaning |
//! |------------------------|-------------------------------|---------|
//! | `DICE_PROFILE`         | `full`                        | `dev-lite` (in-proc bus + memory cache) or `full` (NATS + Redis) |
//! | `DICE_BUS`             | per profile                   | override: `local` or `nats` |
//! | `DICE_CACHE`           | per profile                   | override: `memory` or `redis` |
//! | `DICE_NODE_ID`         | `0`                           | snowflake node id (0..=1023) |
//! | `DATABASE_URL`         | — (required)                  | Postgres URL; migrations run on boot |
//! | `DICE_DB_MAX_CONNS`    | `16`                          | pool cap (read by `DbConfig::from_env`) |
//! | `DICE_REDIS_URL`       | `redis://localhost:6379`      | used when the cache backend is `redis` |
//! | `DICE_NATS_URL`        | `nats://localhost:4222`       | used when the bus backend is `nats` |
//! | `DICE_REST_ADDR`       | `0.0.0.0:8443`                | HTTPS REST + WSS `/gateway/v1` (one TLS port) |
//! | `DICE_QUIC_ADDR`       | `0.0.0.0:8444`                | QUIC endpoint (UDP, ALPN `dice/1`) |
//! | `DICE_ADMIN_ADDR`      | `127.0.0.1:9600`              | Prometheus `/metrics` listener |
//! | `DICE_TLS_CERT`        | `dev/certs/server.crt`        | TLS chain PEM (auto-generated when missing) |
//! | `DICE_TLS_KEY`         | `dev/certs/server.key`        | TLS key PEM (auto-generated when missing) |
//! | `DICE_JWT_PRIVATE_PEM` | `dev/keys/jwt_ed25519.pem`    | Ed25519 PKCS#8 (auto-generated when missing) |
//! | `DICE_JWT_PUBLIC_PEM`  | `dev/keys/jwt_ed25519.pub.pem`| Ed25519 SPKI (auto-generated when missing) |
//! | `RUST_LOG`             | `info,dice=debug`             | tracing filter (read by `dice-logging`) |
//! | `DICE_LOG_JSON`        | unset                         | `1` ⇒ NDJSON logs (read by `dice-logging`) |

use std::net::SocketAddr;
use std::path::PathBuf;

use dice_cache::CacheConfig;
use dice_common::config::{ConfigError, DiceProfile, env_opt, env_or};
use dice_database::DbConfig;
use dice_event_bus::{BusConfig, DEFAULT_LOCAL_CAPACITY};

pub const DEFAULT_REDIS_URL: &str = "redis://localhost:6379";
pub const DEFAULT_NATS_URL: &str = "nats://localhost:4222";
pub const DEFAULT_REST_ADDR: &str = "0.0.0.0:8443";
pub const DEFAULT_QUIC_ADDR: &str = "0.0.0.0:8444";
pub const DEFAULT_ADMIN_ADDR: &str = "127.0.0.1:9600";

/// Everything the monolith needs, resolved once at boot.
#[derive(Debug, Clone)]
pub struct MonolithConfig {
    pub profile: DiceProfile,
    pub bus: BusConfig,
    pub cache: CacheConfig,
    pub node_id: u16,
    pub db: DbConfig,
    pub rest_addr: SocketAddr,
    pub quic_addr: SocketAddr,
    pub admin_addr: SocketAddr,
    pub tls_cert: PathBuf,
    pub tls_key: PathBuf,
    pub jwt_private_pem: PathBuf,
    pub jwt_public_pem: PathBuf,
}

impl MonolithConfig {
    pub fn from_env() -> Result<Self, ConfigError> {
        let profile = DiceProfile::from_env()?;
        Ok(Self {
            profile,
            bus: parse_bus(
                env_opt("DICE_BUS").as_deref(),
                profile,
                env_opt("DICE_NATS_URL").unwrap_or_else(|| DEFAULT_NATS_URL.to_owned()),
            )?,
            cache: parse_cache(
                env_opt("DICE_CACHE").as_deref(),
                profile,
                env_opt("DICE_REDIS_URL").unwrap_or_else(|| DEFAULT_REDIS_URL.to_owned()),
            )?,
            node_id: env_or("DICE_NODE_ID", 0u16)?,
            db: DbConfig::from_env()?,
            rest_addr: parse_addr("DICE_REST_ADDR", DEFAULT_REST_ADDR)?,
            quic_addr: parse_addr("DICE_QUIC_ADDR", DEFAULT_QUIC_ADDR)?,
            admin_addr: parse_addr("DICE_ADMIN_ADDR", DEFAULT_ADMIN_ADDR)?,
            tls_cert: path_or("DICE_TLS_CERT", "dev/certs/server.crt"),
            tls_key: path_or("DICE_TLS_KEY", "dev/certs/server.key"),
            jwt_private_pem: path_or("DICE_JWT_PRIVATE_PEM", "dev/keys/jwt_ed25519.pem"),
            jwt_public_pem: path_or("DICE_JWT_PUBLIC_PEM", "dev/keys/jwt_ed25519.pub.pem"),
        })
    }

    /// Short backend name for the startup banner.
    pub fn bus_name(&self) -> &'static str {
        match self.bus {
            BusConfig::Local { .. } => "local (in-proc broadcast)",
            BusConfig::Nats { .. } => "nats",
        }
    }

    /// Short backend name for the startup banner.
    pub fn cache_name(&self) -> &'static str {
        match self.cache {
            CacheConfig::Memory => "memory (moka)",
            CacheConfig::Redis { .. } => "redis",
        }
    }

    pub fn profile_name(&self) -> &'static str {
        match self.profile {
            DiceProfile::DevLite => "dev-lite",
            DiceProfile::Full => "full",
        }
    }
}

fn parse_addr(key: &'static str, default: &str) -> Result<SocketAddr, ConfigError> {
    let raw = env_opt(key).unwrap_or_else(|| default.to_owned());
    raw.parse()
        .map_err(|e: std::net::AddrParseError| ConfigError::Invalid {
            key,
            value: raw,
            reason: e.to_string(),
        })
}

fn path_or(key: &'static str, default: &str) -> PathBuf {
    PathBuf::from(env_opt(key).unwrap_or_else(|| default.to_owned()))
}

/// `DICE_BUS` override wins; otherwise the profile decides
/// (dev-lite ⇒ local, full ⇒ NATS). Pure so it is unit-testable without
/// mutating process env (`std::env::set_var` is unsafe in edition 2024).
fn parse_bus(
    over: Option<&str>,
    profile: DiceProfile,
    nats_url: String,
) -> Result<BusConfig, ConfigError> {
    let choice = match over {
        Some(s) => s.to_ascii_lowercase(),
        None => match profile {
            DiceProfile::DevLite => "local".to_owned(),
            DiceProfile::Full => "nats".to_owned(),
        },
    };
    match choice.as_str() {
        "local" => Ok(BusConfig::Local {
            capacity: DEFAULT_LOCAL_CAPACITY,
        }),
        "nats" => Ok(BusConfig::Nats { url: nats_url }),
        other => Err(ConfigError::Invalid {
            key: "DICE_BUS",
            value: other.to_owned(),
            reason: "expected local|nats".to_owned(),
        }),
    }
}

/// `DICE_CACHE` override wins; otherwise the profile decides
/// (dev-lite ⇒ memory, full ⇒ Redis).
fn parse_cache(
    over: Option<&str>,
    profile: DiceProfile,
    redis_url: String,
) -> Result<CacheConfig, ConfigError> {
    let choice = match over {
        Some(s) => s.to_ascii_lowercase(),
        None => match profile {
            DiceProfile::DevLite => "memory".to_owned(),
            DiceProfile::Full => "redis".to_owned(),
        },
    };
    match choice.as_str() {
        "memory" => Ok(CacheConfig::Memory),
        "redis" => Ok(CacheConfig::Redis { url: redis_url }),
        other => Err(ConfigError::Invalid {
            key: "DICE_CACHE",
            value: other.to_owned(),
            reason: "expected memory|redis".to_owned(),
        }),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn bus_defaults_follow_profile() {
        assert!(matches!(
            parse_bus(None, DiceProfile::DevLite, DEFAULT_NATS_URL.into()).unwrap(),
            BusConfig::Local {
                capacity: DEFAULT_LOCAL_CAPACITY
            }
        ));
        assert!(matches!(
            parse_bus(None, DiceProfile::Full, "nats://x:1".into()).unwrap(),
            BusConfig::Nats { url } if url == "nats://x:1"
        ));
    }

    #[test]
    fn bus_override_beats_profile() {
        assert!(matches!(
            parse_bus(Some("local"), DiceProfile::Full, DEFAULT_NATS_URL.into()).unwrap(),
            BusConfig::Local { .. }
        ));
        assert!(matches!(
            parse_bus(Some("NATS"), DiceProfile::DevLite, "nats://y:2".into()).unwrap(),
            BusConfig::Nats { url } if url == "nats://y:2"
        ));
        assert!(parse_bus(Some("kafka"), DiceProfile::Full, String::new()).is_err());
    }

    #[test]
    fn cache_defaults_follow_profile() {
        assert_eq!(
            parse_cache(None, DiceProfile::DevLite, DEFAULT_REDIS_URL.into()).unwrap(),
            CacheConfig::Memory
        );
        assert_eq!(
            parse_cache(None, DiceProfile::Full, "redis://z:3".into()).unwrap(),
            CacheConfig::Redis {
                url: "redis://z:3".into()
            }
        );
    }

    #[test]
    fn cache_override_beats_profile() {
        assert_eq!(
            parse_cache(Some("memory"), DiceProfile::Full, DEFAULT_REDIS_URL.into()).unwrap(),
            CacheConfig::Memory
        );
        assert_eq!(
            parse_cache(Some("Redis"), DiceProfile::DevLite, "redis://w:4".into()).unwrap(),
            CacheConfig::Redis {
                url: "redis://w:4".into()
            }
        );
        assert!(parse_cache(Some("memcached"), DiceProfile::Full, String::new()).is_err());
    }
}
