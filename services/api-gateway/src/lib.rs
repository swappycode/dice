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
}

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

/// Budget for the post-shutdown drain (docs/design §9: 10 s).
const DRAIN_DEADLINE: Duration = Duration::from_secs(10);

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
    let quic_cfg = tls::quic_server_config(quic_tls).context("build QUIC server config")?;

    let acceptor = QuicAcceptor::bind(cfg.quic_addr, quic_cfg)
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

    // QUIC accept loop: every established control stream becomes a session.
    {
        let gw = Arc::clone(&gw);
        tracker.spawn(async move {
            while let Some(transport) = acceptor.accept(&gw.ct).await {
                let session_gw = Arc::clone(&gw);
                gw.tracker
                    .spawn(session::drive_connection(session_gw, Box::new(transport)));
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
