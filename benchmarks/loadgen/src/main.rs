//! `dice-loadgen` — a connection load generator for the api-gateway's 100k
//! concurrent-connection benchmark (M4 scaling). It opens N concurrent QUIC (or
//! WSS) connections, drives each through the real Hello→Identify→Ready handshake
//! with a pre-minted access token, and holds it alive with app heartbeats while
//! reporting client-side established / failed / disconnected counts and connect /
//! heartbeat-RTT latency. Correlate its output with the gateway's exported
//! `dice_gateway_connections{transport}` / `dice_gateway_closes_total{code}` and
//! its RSS/CPU. Config is env-only (ADR-0002) — see `src/config.rs` and the
//! bench runbook. Do NOT measure throughput on Windows (no UDP GSO).

mod config;
mod conn;
mod identity;
mod run;
mod stats;
mod transport;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dice_logging::init(&dice_logging::LogConfig::from_env());
    let cfg = config::LoadgenConfig::from_env()?;
    run::run(cfg).await
}
