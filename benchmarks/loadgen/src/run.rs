//! Orchestration: build the shared transport state, ramp connections open at a
//! controlled rate, hold them, and report. QUIC connections share a small pool
//! of endpoints (UDP sockets); WSS connections are one TCP socket each.

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context as _;
use dice_network_core::client::TlsOptions;
use dice_network_core::tls::install_ring_provider;
use dice_protocol::v1;
use quinn::Endpoint;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use crate::config::{LoadgenConfig, Transport};
use crate::conn::{HandshakeParams, run_connection};
use crate::identity::Identities;
use crate::stats::Stats;
use crate::transport::{quic_connect, wss_connect};

/// Connections are opened in fixed-cadence batches so the ramp rate is honoured
/// without one timer per connection.
const RAMP_BATCH_MS: u64 = 20;

pub async fn run(cfg: LoadgenConfig) -> anyhow::Result<()> {
    install_ring_provider();
    banner(&cfg);

    let stats = Arc::new(Stats::new());
    let shutdown = CancellationToken::new();
    let tracker = TaskTracker::new();
    let start = Instant::now();

    // Ctrl-C drains: cancel the token so every held connection closes cleanly.
    {
        let sd = shutdown.clone();
        tokio::spawn(async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                tracing::info!("Ctrl-C — draining connections");
                sd.cancel();
            }
        });
    }

    let reporter = tokio::spawn(reporter(stats.clone(), shutdown.clone(), cfg.report, start));

    let identities = Identities::load(&cfg.jwt_private, &cfg.jwt_public, cfg.node_id)
        .context("load JWT signing key")?;
    let tls = TlsOptions {
        extra_ca_pem: Some(cfg.ca_pem.clone()),
    };
    let properties = client_properties();

    // One spawn closure per transport so the ramp loop below is transport-blind.
    let mut spawn_one: Box<dyn FnMut(String)> = match cfg.transport {
        Transport::Quic => {
            let quic_cfg = tls
                .quic_client_config()
                .context("build QUIC client config")?;
            let addr = resolve(&cfg.quic_target).await?;
            let server_name = cfg
                .server_name
                .clone()
                .unwrap_or_else(|| host_of(&cfg.quic_target).to_owned());
            let endpoints = Arc::new(build_endpoints(cfg.endpoints)?);
            tracing::info!(
                "QUIC -> {addr} (sni={server_name}) over {} shared endpoint(s)",
                endpoints.len()
            );

            let stats = stats.clone();
            let shutdown = shutdown.clone();
            let tracker = tracker.clone();
            let connect_timeout = cfg.connect_timeout;
            let handshake_timeout = cfg.handshake_timeout;
            let caps = cfg.capabilities;
            let hb = cfg.heartbeat_ms;
            let close_on_exit = cfg.close_on_exit;
            let mut idx = 0usize;

            Box::new(move |token: String| {
                let endpoint = endpoints[idx % endpoints.len()].clone();
                idx += 1;
                let quic_cfg = quic_cfg.clone();
                let server_name = server_name.clone();
                let params = HandshakeParams {
                    token,
                    properties: properties.clone(),
                    capabilities: caps,
                    handshake_timeout,
                };
                let stats = stats.clone();
                let shutdown = shutdown.clone();
                tracker.spawn(async move {
                    match quic_connect(&endpoint, quic_cfg, addr, &server_name, connect_timeout)
                        .await
                    {
                        Ok((conn, tx, rx)) => {
                            let close_conn = conn.clone();
                            run_connection(
                                Ok((tx, rx)),
                                &params,
                                hb,
                                stats,
                                shutdown,
                                close_on_exit,
                                move || {
                                    close_conn
                                        .close(quinn::VarInt::from_u32(1000), b"loadgen shutdown");
                                },
                            )
                            .await;
                            drop(conn);
                        }
                        Err(err) => {
                            run_connection(
                                Err(err),
                                &params,
                                hb,
                                stats,
                                shutdown,
                                close_on_exit,
                                || {},
                            )
                            .await;
                        }
                    }
                });
            })
        }
        Transport::Wss => {
            let ws_tls = tls.client_config().context("build WSS client config")?;
            let url = cfg.wss_url.clone();
            tracing::info!("WSS -> {url}");

            let stats = stats.clone();
            let shutdown = shutdown.clone();
            let tracker = tracker.clone();
            let connect_timeout = cfg.connect_timeout;
            let handshake_timeout = cfg.handshake_timeout;
            let caps = cfg.capabilities;
            let hb = cfg.heartbeat_ms;
            let close_on_exit = cfg.close_on_exit;

            Box::new(move |token: String| {
                let ws_tls = ws_tls.clone();
                let url = url.clone();
                let params = HandshakeParams {
                    token,
                    properties: properties.clone(),
                    capabilities: caps,
                    handshake_timeout,
                };
                let stats = stats.clone();
                let shutdown = shutdown.clone();
                tracker.spawn(async move {
                    let established = wss_connect(&url, ws_tls, connect_timeout).await;
                    // Dropping the WS halves on shutdown ends the TCP connection,
                    // which the gateway reads as a clean close.
                    run_connection(
                        established,
                        &params,
                        hb,
                        stats,
                        shutdown,
                        close_on_exit,
                        || {},
                    )
                    .await;
                });
            })
        }
    };

    // ---- ramp ----
    let per_batch = if cfg.rate == 0 {
        cfg.conns.max(1)
    } else {
        usize::try_from((u64::from(cfg.rate) * RAMP_BATCH_MS / 1000).max(1)).unwrap_or(usize::MAX)
    };
    let mut ticker = tokio::time::interval(Duration::from_millis(RAMP_BATCH_MS));
    let mut spawned = 0usize;
    while spawned < cfg.conns {
        tokio::select! {
            _ = ticker.tick() => {}
            () = shutdown.cancelled() => break,
        }
        let n = per_batch.min(cfg.conns - spawned);
        for _ in 0..n {
            match identities.mint() {
                Ok(token) => {
                    stats.attempt();
                    spawn_one(token);
                }
                Err(err) => {
                    tracing::error!(error = %err, "mint token failed");
                    stats.connect_failed();
                }
            }
            spawned += 1;
        }
    }
    drop(spawn_one);
    tracing::info!("ramp complete: {spawned} connections attempted");

    // ---- hold ----
    if !shutdown.is_cancelled() {
        if cfg.hold.is_zero() {
            tracing::info!("holding until Ctrl-C");
            shutdown.cancelled().await;
        } else {
            tracing::info!("holding for {:?}", cfg.hold);
            tokio::select! {
                () = tokio::time::sleep(cfg.hold) => {}
                () = shutdown.cancelled() => {}
            }
        }
    }

    // ---- drain ----
    shutdown.cancel();
    // Let the clean closes flush before tearing the runtime down.
    tokio::time::sleep(Duration::from_millis(500)).await;
    tracker.close();
    if tokio::time::timeout(Duration::from_secs(5), tracker.wait())
        .await
        .is_err()
    {
        tracing::warn!("drain deadline expired with connections still closing");
    }
    reporter.abort();

    let snap = stats.snapshot();
    tracing::info!("FINAL {}", snap.line(start.elapsed().as_secs_f64()));
    let breakdown = stats.close_breakdown();
    if !breakdown.is_empty() {
        tracing::info!("FINAL closes: {breakdown}");
    }
    Ok(())
}

async fn reporter(stats: Arc<Stats>, shutdown: CancellationToken, every: Duration, start: Instant) {
    let mut ticker = tokio::time::interval(every);
    ticker.tick().await; // skip the immediate tick
    loop {
        tokio::select! {
            _ = ticker.tick() => {
                let snap = stats.snapshot();
                tracing::info!("{}", snap.line(start.elapsed().as_secs_f64()));
                let breakdown = stats.close_breakdown();
                if !breakdown.is_empty() {
                    tracing::info!("  closes: {breakdown}");
                }
            }
            () = shutdown.cancelled() => break,
        }
    }
}

fn build_endpoints(n: usize) -> anyhow::Result<Vec<Endpoint>> {
    (0..n)
        .map(|_| {
            Endpoint::client(SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0)))
                .context("bind QUIC client endpoint")
        })
        .collect()
}

/// Resolve `host:port`, preferring IPv4 (the dev gateway binds v4 and the dev
/// leaf carries 127.0.0.1 / ::1 SANs).
async fn resolve(target: &str) -> anyhow::Result<SocketAddr> {
    let mut fallback = None;
    for addr in tokio::net::lookup_host(target)
        .await
        .with_context(|| format!("resolve {target}"))?
    {
        if addr.is_ipv4() {
            return Ok(addr);
        }
        fallback.get_or_insert(addr);
    }
    fallback.with_context(|| format!("no addresses for {target}"))
}

/// Host portion of a `host:port` (handles `[::1]:p` bracket form).
fn host_of(target: &str) -> &str {
    if let Some(rest) = target.strip_prefix('[') {
        return rest.split(']').next().unwrap_or(target);
    }
    target.rsplit_once(':').map_or(target, |(host, _)| host)
}

fn client_properties() -> v1::ClientProperties {
    v1::ClientProperties {
        client: "dice-loadgen".to_owned(),
        version: env!("CARGO_PKG_VERSION").to_owned(),
        os: std::env::consts::OS.to_owned(),
    }
}

fn banner(cfg: &LoadgenConfig) {
    tracing::info!(
        "dice-loadgen: transport={} conns={} rate={}/s hold={:?} heartbeat={} endpoints={}",
        cfg.transport.label(),
        cfg.conns,
        if cfg.rate == 0 {
            "burst".to_owned()
        } else {
            cfg.rate.to_string()
        },
        cfg.hold,
        if cfg.heartbeat_ms == 0 {
            "server".to_owned()
        } else {
            format!("{}ms", cfg.heartbeat_ms)
        },
        cfg.endpoints,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_of_handles_forms() {
        assert_eq!(host_of("127.0.0.1:8444"), "127.0.0.1");
        assert_eq!(host_of("localhost:8444"), "localhost");
        assert_eq!(host_of("[::1]:8444"), "::1");
        assert_eq!(host_of("example.com:443"), "example.com");
    }
}
