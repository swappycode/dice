//! api-gateway: the realtime gateway (QUIC + WSS) and REST layer.
//!
//! The wire contract is docs/protocol.md (NORMATIVE). One process serves:
//!
//! - HTTPS REST + the `/gateway/v1` WebSocket upgrade on `rest_addr`
//!   (one TLS port, hand-rolled accept loop from `dice-network-core`),
//! - QUIC (ALPN `dice/1`) on `quic_addr`.
//!
//! Both transports feed the same session entry point
//! ([`session::drive_connection`]): Hello → Identify/Resume → Ready →
//! sequenced fan-out from the event bus with per-session replay buffers.
//!
//! Module map (backend-services.md §1):
//! - [`session`]  — per-connection state machine, outbound queue, seq assignment
//! - [`handshake`] — Identify verification + Ready assembly
//! - [`dispatch`] — client→server request handling (send/typing/presence)
//! - [`router`]  — refcounted interest map, ONE bus subscription per subject
//! - [`resume`]  — replay ring buffer + detached-session registry
//! - [`rest`]    — axum REST surface (protobuf bodies) + WS upgrade
//! - [`ws`]      — `FramedTransport` adapter over `axum::extract::ws`

mod dispatch;
mod durable;
mod handshake;
mod rest;
mod resume;
mod router;
mod session;
mod voice_dg;
mod ws;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context as _;
use dice_common::shutdown::CancellationToken;
use dice_network_core::server::QuicAcceptor;
use dice_network_core::tls;
use tokio::task::JoinHandle;
use tokio_util::task::TaskTracker;

/// Gateway socket + protocol-timing configuration.
pub struct GatewayConfig {
    /// REST + WSS (one TLS port).
    pub rest_addr: std::net::SocketAddr,
    /// QUIC endpoint (UDP, ALPN `dice/1`).
    pub quic_addr: std::net::SocketAddr,
    /// TLS certificate chain (PEM, leaf first).
    pub tls_cert: std::path::PathBuf,
    /// TLS private key (PEM, PKCS#8).
    pub tls_key: std::path::PathBuf,
    /// Advertised in `Hello`; 30000 in production.
    pub heartbeat_interval_ms: u32,
    /// Advertised in `Hello`; 60000 in production.
    pub resume_window_ms: u32,
    /// QUIC server transport tuning (the 100k-benchmark knobs). `Default`
    /// reproduces the protocol §1 production values, so existing callers can use
    /// `QuicServerTuning::default()` for behaviour-neutral operation.
    pub quic: QuicServerTuning,
    /// This node's externally-reachable `host:port`, recorded in the cross-node
    /// session directory so a reconnect that lands on another node can be
    /// redirected here to resume (ADR-0007 phase 0b). `None` = no redirect
    /// emitted (the sticky-LB phase-0 path).
    pub advertised_addr: Option<String>,
}

/// Re-exported so the monolith + harnesses can set [`GatewayConfig::quic`]
/// without depending on `dice-network-core` directly.
pub use tls::QuicServerTuning;

/// Everything the gateway calls out to. The monolith builds this once.
pub struct GatewayDeps {
    pub auth: std::sync::Arc<dyn auth_service::Auth>,
    pub chat: std::sync::Arc<dyn chat_service::Chat>,
    pub media: std::sync::Arc<dyn media_service::Media>,
    pub presence: std::sync::Arc<dyn presence_service::Presence>,
    pub voice: std::sync::Arc<dyn voice_service::Voice>,
    pub bus: std::sync::Arc<dyn dice_event_bus::EventBus>,
    /// Verify-capable JWT keys (public half is enough; gateway never signs).
    pub jwt: std::sync::Arc<dice_auth_core::token::JwtKeys>,
    pub ids: std::sync::Arc<dice_common::SnowflakeGenerator>,
    pub rate: dice_cache::RateLimiter,
    /// Per-user unread counts (maintained by notification-service; read by
    /// `GET /v1/unread`, cleared by `POST /v1/channels/{id}/read`).
    pub unread: dice_cache::UnreadStore,
    /// Shared cache backend, for the cross-node session directory (resume phase 0).
    pub cache: std::sync::Arc<dyn dice_cache::Cache>,
}

/// Shared per-process gateway state (internal).
pub(crate) struct Gateway {
    pub(crate) deps: GatewayDeps,
    pub(crate) heartbeat_interval: Duration,
    pub(crate) resume_window: Duration,
    pub(crate) router: router::Router,
    /// Voice datagram fan-out registry (the SFU's I/O half).
    pub(crate) voice_dg: std::sync::Arc<voice_dg::VoiceDatagrams>,
    pub(crate) resume: Box<dyn resume::ResumeRegistry>,
    /// Cross-node session→owner directory + this gateway's node id (resume
    /// phase 0, ADR-0007): a reconnect that lands on another node can be told
    /// which node still owns the detached session.
    pub(crate) directory: dice_cache::SessionDirectory,
    /// Durable cross-node resume store (ADR-0007 phase 2b): the snapshot another
    /// node re-hosts from after this one is gone, + the single-takeover claim.
    pub(crate) durable: durable::DurableResume,
    pub(crate) node_id: u16,
    /// This node's externally-reachable `host:port` (`DICE_ADVERTISED_ADDR`),
    /// recorded in the directory so another node can redirect a reconnect here
    /// (resume phase 0b). `None` ⇒ phase-0 sticky-LB behaviour.
    pub(crate) advertised_addr: Option<String>,
    /// Process-wide shutdown token; sessions broadcast `Close{GOING_AWAY}`
    /// on cancellation.
    pub(crate) ct: CancellationToken,
    /// Tracks accept loops + session tasks + bus consume tasks for the drain.
    pub(crate) tracker: TaskTracker,
}

/// Handle returned by [`start`]: the OS-assigned addresses (port 0 resolves
/// here) and the supervisor that resolves after graceful drain.
pub struct Started {
    pub bound_rest: SocketAddr,
    pub bound_quic: SocketAddr,
    handle: JoinHandle<()>,
}

impl Started {
    /// Wait until the gateway has fully drained (after `ct` cancellation).
    pub async fn wait(self) -> anyhow::Result<()> {
        self.handle
            .await
            .context("gateway supervisor task panicked")
    }
}

/// Test-only seam: seed a durable cross-node resume snapshot into `cache` so an
/// integration test can exercise re-host (ADR-0007 phase 2b) without standing up
/// a second node whose live lease-refresh would race the test. Hidden from docs;
/// not part of the supported API.
#[doc(hidden)]
#[allow(clippy::too_many_arguments)] // mirrors the (pub(crate)) snapshot fields
pub async fn seed_resume_snapshot(
    cache: Arc<dyn dice_cache::Cache>,
    session_id: u64,
    user: u64,
    auth_session: u64,
    resume_token: [u8; 32],
    next_seq: u64,
    trimmed_to: u64,
    frames: Vec<dice_protocol::v1::Frame>,
) {
    let snapshot = durable::ResumeSnapshot {
        user,
        auth_session,
        resume_token,
        next_seq,
        trimmed_to,
        frames,
    };
    let _ = durable::DurableResume::new(cache)
        .save(session_id, &snapshot, Duration::from_secs(60))
        .await;
}

/// Budget for the post-shutdown drain (docs/design §9: 10 s).
const DRAIN_DEADLINE: Duration = Duration::from_secs(10);

/// Bound on concurrent in-flight QUIC handshakes (TLS + the 5 s control-stream
/// wait). Establishment runs off the accept loop so a slow peer can't block new
/// accepts; this caps the number of simultaneously-establishing connections so a
/// connect flood can't spawn unbounded tasks. Established sessions don't count.
const MAX_INFLIGHT_HANDSHAKES: usize = 1024;

/// Bind both listeners and start serving. Returns once the sockets are live;
/// the gateway then runs until `ct` is cancelled (see [`Started::wait`]).
pub async fn start(
    cfg: GatewayConfig,
    deps: GatewayDeps,
    ct: CancellationToken,
) -> anyhow::Result<Started> {
    tls::install_ring_provider();

    let rest_tls = tls::load_server_config(&cfg.tls_cert, &cfg.tls_key, &[b"http/1.1"])
        .context("load REST TLS config")?;
    let quic_tls =
        tls::load_server_config(&cfg.tls_cert, &cfg.tls_key, &[dice_protocol::ALPN_GATEWAY])
            .context("load QUIC TLS config")?;
    let quic_cfg =
        tls::quic_server_config_tuned(quic_tls, &cfg.quic).context("build QUIC server config")?;

    let acceptor = QuicAcceptor::bind_tuned(
        cfg.quic_addr,
        quic_cfg,
        cfg.quic.socket_send_buffer,
        cfg.quic.socket_recv_buffer,
    )
    .with_context(|| format!("bind QUIC endpoint on {}", cfg.quic_addr))?;
    let bound_quic = acceptor.local_addr().context("resolve bound QUIC addr")?;
    let listener = tokio::net::TcpListener::bind(cfg.rest_addr)
        .await
        .with_context(|| format!("bind REST listener on {}", cfg.rest_addr))?;
    let bound_rest = listener.local_addr().context("resolve bound REST addr")?;

    let tracker = TaskTracker::new();
    let gw = Arc::new(Gateway {
        router: router::Router::new(
            deps.bus.clone(),
            deps.presence.clone(),
            ct.clone(),
            tracker.clone(),
        ),
        voice_dg: voice_dg::VoiceDatagrams::new(deps.voice.clone()),
        resume: Box::new(resume::LocalResumeRegistry::new()),
        directory: dice_cache::SessionDirectory::new(deps.cache.clone()),
        durable: durable::DurableResume::new(deps.cache.clone()),
        node_id: deps.ids.node_id(),
        advertised_addr: cfg.advertised_addr,
        heartbeat_interval: Duration::from_millis(u64::from(cfg.heartbeat_interval_ms)),
        resume_window: Duration::from_millis(u64::from(cfg.resume_window_ms)),
        deps,
        ct: ct.clone(),
        tracker: tracker.clone(),
    });

    // HTTPS accept loop (REST + WS upgrade share the port).
    let axum_router = rest::build_router(Arc::clone(&gw));
    {
        let ct = ct.clone();
        tracker.spawn(async move {
            if let Err(error) =
                dice_network_core::server::serve_https_on(listener, rest_tls, axum_router, ct).await
            {
                tracing::error!(%error, "HTTPS accept loop failed");
            }
        });
    }

    // QUIC accept loop: accept is non-blocking — each connection's TLS handshake
    // + 5 s control-stream wait runs in its OWN task (bounded by a semaphore) so a
    // slow/stalled peer can't head-of-line-block new accepts, and establishment
    // throughput isn't serialized at the 100k ramp. The session itself does not
    // hold a handshake permit.
    {
        let gw = Arc::clone(&gw);
        let establishing = Arc::new(tokio::sync::Semaphore::new(MAX_INFLIGHT_HANDSHAKES));
        tracker.spawn(async move {
            while let Some(incoming) = acceptor.accept_incoming(&gw.ct).await {
                let Ok(permit) = Arc::clone(&establishing).acquire_owned().await else {
                    break; // semaphore closed (never, here) — stop accepting
                };
                let session_gw = Arc::clone(&gw);
                gw.tracker.spawn(async move {
                    if let Some(transport) = QuicAcceptor::establish(incoming).await {
                        drop(permit); // release before the long-lived session
                        session::drive_connection(session_gw, Box::new(transport)).await;
                    }
                });
            }
        });
    }

    tracing::info!(%bound_rest, %bound_quic, "api-gateway listening");

    let handle = tokio::spawn(async move {
        gw.ct.cancelled().await;
        // Accept loops stop on the same token; sessions send Close{GOING_AWAY}
        // themselves (session.rs) and exit. Wait for everything, bounded.
        gw.tracker.close();
        if tokio::time::timeout(DRAIN_DEADLINE, gw.tracker.wait())
            .await
            .is_err()
        {
            tracing::warn!("graceful drain deadline expired with tasks still running");
        }
        tracing::info!("api-gateway drained");
    });

    Ok(Started {
        bound_rest,
        bound_quic,
        handle,
    })
}

/// Runs QUIC acceptor + HTTPS (REST+WSS) until `ct` is cancelled. Resolves
/// after graceful drain. Thin wrapper over [`start`] + [`Started::wait`].
pub async fn run(
    cfg: GatewayConfig,
    deps: GatewayDeps,
    ct: dice_common::shutdown::CancellationToken,
) -> anyhow::Result<()> {
    start(cfg, deps, ct).await?.wait().await
}
