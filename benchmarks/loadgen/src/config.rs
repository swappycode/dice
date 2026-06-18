//! Env-driven configuration (ADR-0002: backend config is env vars, never cargo
//! features or an arg parser). Every knob has a `DICE_LOADGEN_*` variable with a
//! sensible default so `cargo run -p dice-loadgen` works from the repo root
//! against a freshly-booted gateway with zero flags. The `just bench` recipe and
//! the Linux runbook set these; on the load box you can also export them inline
//! (`DICE_LOADGEN_CONNS=100000 dice-loadgen`).

use std::path::PathBuf;
use std::time::Duration;

use anyhow::Context as _;
use dice_common::config::{env_opt, env_or};

/// Which transport the harness drives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    /// QUIC (UDP, the primary transport, ALPN `dice/1`) — the headline 100k gate.
    Quic,
    /// WSS fallback (`/gateway/v1` over the REST TLS port).
    Wss,
}

impl Transport {
    pub fn label(self) -> &'static str {
        match self {
            Transport::Quic => "quic",
            Transport::Wss => "wss",
        }
    }
}

/// Fully-resolved loadgen run configuration.
#[derive(Debug, Clone)]
pub struct LoadgenConfig {
    pub transport: Transport,
    /// QUIC dial target `host:port` (default `127.0.0.1:8444`).
    pub quic_target: String,
    /// WSS URL (default `wss://127.0.0.1:8443/gateway/v1`).
    pub wss_url: String,
    /// TLS server name override; defaults to the host of the dial target. Must
    /// match a leaf SAN — the dev cert only carries `localhost`/`127.0.0.1`/`::1`,
    /// so a remote run dials one of those (loadgen co-located with the gateway).
    pub server_name: Option<String>,
    /// Extra trust anchor PEM (the dev CA). Full verification always runs.
    pub ca_pem: PathBuf,
    /// Ed25519 signing key the gateway verifies with its public half.
    pub jwt_private: PathBuf,
    pub jwt_public: PathBuf,
    /// Snowflake node id stamped into the synthetic user/session ids. Kept off
    /// the gateway's own node (0) so minted ids never collide with real ones.
    pub node_id: u16,
    /// Total connections to open.
    pub conns: usize,
    /// Connection open rate (conns/sec) during the ramp. 0 = open all at once.
    pub rate: u32,
    /// Number of shared QUIC client endpoints (UDP sockets). Connections are
    /// spread across them so one socket isn't the bottleneck and GSO can batch.
    /// Ignored for WSS (each conn is its own TCP socket).
    pub endpoints: usize,
    /// Hold time after the ramp completes. 0 = hold until Ctrl-C.
    pub hold: Duration,
    /// Override the heartbeat cadence; 0 = honour the gateway's advertised
    /// `Hello.heartbeat_interval_ms`.
    pub heartbeat_ms: u32,
    /// `Identify.capabilities` bitset (default 0 — plain control session).
    pub capabilities: u64,
    /// Stats print interval.
    pub report: Duration,
    pub connect_timeout: Duration,
    pub handshake_timeout: Duration,
    /// Cleanly close each connection on shutdown so the gateway connection gauge
    /// drains promptly (vs. letting the OS reap sockets).
    pub close_on_exit: bool,
}

impl LoadgenConfig {
    pub fn from_env() -> anyhow::Result<Self> {
        let transport = match env_opt("DICE_LOADGEN_TRANSPORT")
            .unwrap_or_else(|| "quic".to_owned())
            .to_ascii_lowercase()
            .as_str()
        {
            "quic" => Transport::Quic,
            "wss" | "ws" => Transport::Wss,
            other => anyhow::bail!("DICE_LOADGEN_TRANSPORT must be 'quic' or 'wss', got {other:?}"),
        };

        let endpoints_default = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4);

        Ok(Self {
            transport,
            quic_target: env_opt("DICE_LOADGEN_TARGET")
                .unwrap_or_else(|| "127.0.0.1:8444".to_owned()),
            wss_url: env_opt("DICE_LOADGEN_WSS_URL")
                .unwrap_or_else(|| "wss://127.0.0.1:8443/gateway/v1".to_owned()),
            server_name: env_opt("DICE_LOADGEN_SERVER_NAME"),
            // Reuse the gateway/client's own default dev-asset locations.
            ca_pem: path_or("DICE_LOADGEN_CA", "dev/certs/dev-ca.pem"),
            jwt_private: path_or("DICE_LOADGEN_JWT_PRIVATE", "dev/keys/jwt_ed25519.pem"),
            jwt_public: path_or("DICE_LOADGEN_JWT_PUBLIC", "dev/keys/jwt_ed25519.pub.pem"),
            node_id: env_or("DICE_LOADGEN_NODE_ID", 512u16).context("DICE_LOADGEN_NODE_ID")?,
            conns: env_or("DICE_LOADGEN_CONNS", 1000usize).context("DICE_LOADGEN_CONNS")?,
            rate: env_or("DICE_LOADGEN_RATE", 500u32).context("DICE_LOADGEN_RATE")?,
            endpoints: env_or("DICE_LOADGEN_ENDPOINTS", endpoints_default)
                .context("DICE_LOADGEN_ENDPOINTS")?
                .max(1),
            hold: Duration::from_secs(
                env_or("DICE_LOADGEN_HOLD_SECS", 60u64).context("DICE_LOADGEN_HOLD_SECS")?,
            ),
            heartbeat_ms: env_or("DICE_LOADGEN_HEARTBEAT_MS", 0u32)
                .context("DICE_LOADGEN_HEARTBEAT_MS")?,
            capabilities: env_or("DICE_LOADGEN_CAPABILITIES", 0u64)
                .context("DICE_LOADGEN_CAPABILITIES")?,
            report: Duration::from_secs(
                env_or("DICE_LOADGEN_REPORT_SECS", 5u64).context("DICE_LOADGEN_REPORT_SECS")?,
            ),
            connect_timeout: Duration::from_millis(
                env_or("DICE_LOADGEN_CONNECT_TIMEOUT_MS", 10_000u64)
                    .context("DICE_LOADGEN_CONNECT_TIMEOUT_MS")?,
            ),
            handshake_timeout: Duration::from_millis(
                env_or("DICE_LOADGEN_HANDSHAKE_TIMEOUT_MS", 5_000u64)
                    .context("DICE_LOADGEN_HANDSHAKE_TIMEOUT_MS")?,
            ),
            close_on_exit: parse_flag("DICE_LOADGEN_CLOSE_ON_EXIT", true),
        })
    }
}

fn path_or(key: &'static str, default: &str) -> PathBuf {
    PathBuf::from(env_opt(key).unwrap_or_else(|| default.to_owned()))
}

/// Truthy env flag with a default when unset (mirrors the monolith's `parse_flag`
/// but with a configurable default — `close_on_exit` defaults ON).
fn parse_flag(key: &'static str, default: bool) -> bool {
    match env_opt(key) {
        Some(v) => !matches!(
            v.to_ascii_lowercase().as_str(),
            "0" | "false" | "no" | "off"
        ),
        None => default,
    }
}
