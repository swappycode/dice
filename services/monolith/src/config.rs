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
//! | `DICE_MEDIA_DIR`       | `data/media`                  | local-fs object store for attachments (created on first upload) |
//! | `DICE_SPLIT`           | unset                         | `1` ⇒ route auth/chat/presence over NATS RPC to standalone bins (requires `full`); media+voice stay in-process |
//! | `RUST_LOG`             | `info,dice=debug`             | tracing filter (read by `dice-logging`) |
//! | `DICE_LOG_JSON`        | unset                         | `1` ⇒ NDJSON logs (read by `dice-logging`) |
//!
//! QUIC server tuning for the 100k-connection benchmark (M4 scaling). Each
//! defaults to the protocol §1 production value, so an unset boot is unchanged:
//!
//! | Variable                       | Default  | Meaning |
//! |--------------------------------|----------|---------|
//! | `DICE_QUIC_RECV_WINDOW`        | `4194304`| per-connection flow-control window (bytes) — the dominant per-conn memory term at 100k; shrink to fit RAM |
//! | `DICE_QUIC_STREAM_RECV_WINDOW` | `1048576`| per-stream flow-control window (bytes) |
//! | `DICE_QUIC_MAX_IDLE_MS`        | `90000`  | transport idle timeout (ms) |
//! | `DICE_QUIC_MAX_BIDI_STREAMS`   | `4`      | max concurrent bidi streams the peer may open |
//! | `DICE_QUIC_MAX_UNI_STREAMS`    | unset    | max concurrent uni streams; unset = quinn default (100), `0` = none (Dice opens none) |
//! | `DICE_QUIC_DATAGRAMS`          | `true`   | QUIC datagrams (voice); `0`/`false` disables them, saving ~128 KiB/conn for a control-only bench |
//! | `DICE_QUIC_SO_SNDBUF`          | OS       | UDP socket send buffer SO_SNDBUF (bytes); raise so GSO batches aren't dropped |
//! | `DICE_QUIC_SO_RCVBUF`          | OS       | UDP socket receive buffer SO_RCVBUF (bytes) |
//! | `DICE_HEARTBEAT_MS`           | `30000`  | heartbeat interval advertised in `Hello`; raise to cut per-conn keep-alive load at 100k |
//! | `DICE_RESUME_WINDOW_MS`       | `60000`  | resume window advertised in `Hello` |

use std::net::SocketAddr;
use std::path::PathBuf;

use api_gateway::QuicServerTuning;
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
    /// Local-fs object store root for media-service (attachments).
    pub media_dir: PathBuf,
    /// Split mode (`DICE_SPLIT=1`): route auth/chat/presence to standalone
    /// service processes over NATS RPC instead of mounting them in-process.
    /// Requires the NATS bus (full profile); media + voice stay in-process.
    pub split: bool,
    /// QUIC server transport tuning (100k-benchmark knobs; `DICE_QUIC_*`).
    pub quic: QuicServerTuning,
    /// Heartbeat interval advertised in `Hello` (ms). Default = protocol const
    /// (30000). Raising it cuts per-connection heartbeat wakeups + presence
    /// cache writes at 100k; the server closes a session after 2× this of silence.
    pub heartbeat_ms: u32,
    /// Resume window advertised in `Hello` (ms). Default = protocol const (60000).
    /// Governs how long a detached session's task + replay ring live (memory
    /// under reconnect churn).
    pub resume_window_ms: u32,
    /// This node's externally-reachable `host:port` (`DICE_ADVERTISED_ADDR`).
    /// Recorded in the cross-node session directory so a reconnect on another
    /// node can be redirected here to resume (ADR-0007 phase 0b). Unset = no
    /// redirect emitted (single-node or sticky-LB-only deployments).
    pub advertised_addr: Option<String>,
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
            media_dir: path_or("DICE_MEDIA_DIR", "data/media"),
            split: parse_flag("DICE_SPLIT"),
            quic: parse_quic_tuning()?,
            heartbeat_ms: env_or("DICE_HEARTBEAT_MS", dice_protocol::HEARTBEAT_INTERVAL_MS)?,
            resume_window_ms: env_or("DICE_RESUME_WINDOW_MS", dice_protocol::RESUME_WINDOW_MS)?,
            advertised_addr: env_opt("DICE_ADVERTISED_ADDR"),
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

/// Parse an optional typed env var: `None` when unset, else parsed (or a config
/// error). Used for the QUIC knobs whose "unset" means "leave quinn's default".
fn env_opt_parse<T>(key: &'static str) -> Result<Option<T>, ConfigError>
where
    T: std::str::FromStr,
    <T as std::str::FromStr>::Err: std::fmt::Display,
{
    match env_opt(key) {
        None => Ok(None),
        Some(raw) => match raw.parse::<T>() {
            Ok(v) => Ok(Some(v)),
            Err(e) => Err(ConfigError::Invalid {
                key,
                value: raw,
                reason: e.to_string(),
            }),
        },
    }
}

/// Truthy-env flag with an explicit default when unset (QUIC datagrams default ON).
fn parse_flag_default(key: &'static str, default: bool) -> bool {
    match env_opt(key) {
        Some(v) => !matches!(
            v.to_ascii_lowercase().as_str(),
            "0" | "false" | "no" | "off"
        ),
        None => default,
    }
}

/// QUIC server transport tuning from `DICE_QUIC_*`. Each knob defaults to the
/// protocol §1 production value, so an unconfigured boot is behaviour-neutral;
/// the 100k benchmark overrides them (see `.env.example` + the bench runbook).
fn parse_quic_tuning() -> Result<QuicServerTuning, ConfigError> {
    let d = QuicServerTuning::default();
    let quic = QuicServerTuning {
        stream_receive_window: env_or("DICE_QUIC_STREAM_RECV_WINDOW", d.stream_receive_window)?,
        receive_window: env_or("DICE_QUIC_RECV_WINDOW", d.receive_window)?,
        max_idle_timeout_ms: env_or("DICE_QUIC_MAX_IDLE_MS", d.max_idle_timeout_ms)?,
        max_concurrent_bidi_streams: env_or(
            "DICE_QUIC_MAX_BIDI_STREAMS",
            d.max_concurrent_bidi_streams,
        )?,
        max_concurrent_uni_streams: env_opt_parse("DICE_QUIC_MAX_UNI_STREAMS")?,
        datagrams: parse_flag_default("DICE_QUIC_DATAGRAMS", d.datagrams),
        socket_send_buffer: env_opt_parse("DICE_QUIC_SO_SNDBUF")?,
        socket_recv_buffer: env_opt_parse("DICE_QUIC_SO_RCVBUF")?,
    };
    // Reject footgun zeros: a 0 flow-control window stalls all stream data, and
    // DICE_QUIC_MAX_IDLE_MS=0 means "idle timeout DISABLED" (RFC 9000) — never
    // what the benchmark wants, and impossible to spot from the gateway side.
    for (key, value) in [
        ("DICE_QUIC_RECV_WINDOW", u64::from(quic.receive_window)),
        (
            "DICE_QUIC_STREAM_RECV_WINDOW",
            u64::from(quic.stream_receive_window),
        ),
        ("DICE_QUIC_MAX_IDLE_MS", u64::from(quic.max_idle_timeout_ms)),
    ] {
        if value == 0 {
            return Err(ConfigError::Invalid {
                key,
                value: "0".to_owned(),
                reason: "must be greater than 0".to_owned(),
            });
        }
    }
    Ok(quic)
}

/// Truthy-env flag: set + non-empty + not an explicit off value enables it.
/// Tolerant on purpose so `DICE_SPLIT=1` / `true` / `yes` / `on` all work.
fn parse_flag(key: &'static str) -> bool {
    match env_opt(key) {
        Some(v) => !matches!(
            v.to_ascii_lowercase().as_str(),
            "0" | "false" | "no" | "off"
        ),
        None => false,
    }
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
    fn split_flag_parsing() {
        // Pure helper: assert against representative truthy/falsy strings via
        // the same match `parse_flag` uses (env-free so tests don't mutate the
        // process environment, which is unsafe in edition 2024).
        let truthy = |v: &str| {
            !matches!(
                v.to_ascii_lowercase().as_str(),
                "0" | "false" | "no" | "off"
            )
        };
        assert!(truthy("1"));
        assert!(truthy("true"));
        assert!(truthy("YES"));
        assert!(truthy("on"));
        assert!(!truthy("0"));
        assert!(!truthy("false"));
        assert!(!truthy("off"));
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
