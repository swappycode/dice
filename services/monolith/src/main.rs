//! dice-monolith: the M1 deployment shape — auth + chat + presence mounted
//! in one process behind the api-gateway, wired by direct trait calls
//! (backend-services.md §1; critique #23: `services/monolith`, pkg
//! `dice-monolith`).
//!
//! Boot order: ring provider → logging → config → dev-keygen → Postgres
//! (connect + embedded migrations) → cache/bus backends → services →
//! Prometheus exporter (non-fatal) → gateway. Ctrl-C drains with a 15 s hard
//! deadline; live sessions receive `Close{GOING_AWAY}` from the gateway.
//!
//! All configuration comes from env vars (see [`config`] for the full table;
//! `.env.example` mirrors it). Run via `just dev` (dev-lite) or
//! `just run-full` (NATS + Redis).

mod config;
mod keygen;

use std::sync::Arc;
use std::time::Duration;

use anyhow::Context as _;
use api_gateway::{GatewayConfig, GatewayDeps};
use auth_service::AuthService;
use chat_service::ChatService;
use dice_cache::RateLimiter;
use dice_common::SnowflakeGenerator;
use dice_common::shutdown::Shutdown;
use dice_protocol::{HEARTBEAT_INTERVAL_MS, RESUME_WINDOW_MS};
use media_service::{LocalFsStore, MediaService};
use presence_service::PresenceService;
use voice_service::VoiceService;

use crate::config::MonolithConfig;

/// Hard deadline for the Ctrl-C drain. The gateway's own internal drain
/// budget (10 s) fits inside it.
const SHUTDOWN_DEADLINE: Duration = Duration::from_secs(15);

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // FIRST: pin the process-default rustls crypto provider to ring before
    // anything TLS-shaped is constructed (workspace policy: never aws-lc).
    dice_network_core::tls::install_ring_provider();
    dice_logging::init(&dice_logging::LogConfig::from_env());

    let cfg = MonolithConfig::from_env().context("read monolith configuration from env")?;
    run(cfg).await
}

async fn run(cfg: MonolithConfig) -> anyhow::Result<()> {
    // --- dev-keygen: persist dev TLS + JWT assets when missing (#22) ---
    let tls = keygen::ensure_tls(&cfg.tls_cert, &cfg.tls_key)?;
    let jwt = keygen::ensure_jwt(&cfg.jwt_private_pem, &cfg.jwt_public_pem)?;

    // --- shared dependencies, built exactly once ---
    let pool = dice_database::connect(&cfg.db)
        .await
        .with_context(|| format!("connect to Postgres ({})", redact(&cfg.db.url)))?;
    dice_database::migrate(&pool)
        .await
        .context("run embedded migrations")?;

    let cache = dice_cache::connect(cfg.cache.clone())
        .await
        .with_context(|| format!("connect cache backend ({})", cfg.cache_name()))?;
    let bus = dice_event_bus::connect(cfg.bus.clone())
        .await
        .with_context(|| format!("connect event bus ({})", cfg.bus_name()))?;
    let ids = Arc::new(SnowflakeGenerator::new(cfg.node_id).context("DICE_NODE_ID")?);
    let rate = RateLimiter::new(cache.clone());
    // Cloned before `bus` is moved into GatewayDeps below, for the outbox relay.
    let relay_bus = bus.clone();

    // --- services -> gateway deps (direct trait calls by default) ---
    let mut deps = GatewayDeps {
        auth: Arc::new(AuthService::new(
            pool.clone(),
            cache.clone(),
            jwt.clone(),
            ids.clone(),
            bus.clone(),
        )),
        chat: Arc::new(ChatService::new(pool.clone(), bus.clone(), ids.clone())),
        media: Arc::new(MediaService::new(
            pool.clone(),
            Arc::new(LocalFsStore::new(cfg.media_dir.clone())),
            ids.clone(),
        )),
        presence: Arc::new(PresenceService::new(
            cache.clone(),
            bus.clone(),
            pool.clone(),
            ids.clone(),
        )),
        voice: Arc::new(VoiceService::new(
            cache.clone(),
            bus.clone(),
            pool.clone(),
            ids.clone(),
        )),
        bus,
        jwt,
        ids,
        unread: dice_cache::UnreadStore::new(cache.clone()),
        rate,
    };

    // --- split mode (DICE_SPLIT=1): route auth/chat/presence to standalone
    // service bins over NATS RPC. The `Arc<dyn Trait>` seam is unchanged — only
    // the concrete impl behind it swaps from a direct call to a `*NatsClient`.
    // media + voice always stay in-process. The direct services built above are
    // cheap struct constructions (no I/O), so replacing three of them is fine. ---
    if cfg.split {
        let dice_event_bus::BusConfig::Nats { url } = &cfg.bus else {
            anyhow::bail!(
                "DICE_SPLIT=1 requires the NATS bus; run with DICE_PROFILE=full (or DICE_BUS=nats)"
            );
        };
        let rpc = dice_event_bus::rpc::RpcClient::connect(url)
            .await
            .context("connect split-mode RPC client")?;
        deps.auth = Arc::new(auth_service::rpc::AuthNatsClient::new(rpc.clone()));
        deps.chat = Arc::new(chat_service::rpc::ChatNatsClient::new(rpc.clone()));
        deps.presence = Arc::new(presence_service::rpc::PresenceNatsClient::new(rpc));
        tracing::info!(nats = %url, "split mode: auth/chat/presence routed over NATS RPC");
    }

    // --- observability: /metrics on the admin port; non-fatal on failure ---
    if let Err(error) = dice_metrics::init_prometheus(cfg.admin_addr) {
        tracing::warn!(%error, addr = %cfg.admin_addr, "Prometheus exporter unavailable; continuing without /metrics");
    }
    // Background DB pool-utilisation sampler (no-op until the exporter above).
    dice_database::spawn_pool_metrics(pool.clone());

    // --- gateway (REST + WSS on one TLS port, QUIC on UDP) ---
    let shutdown = Shutdown::new();

    // --- transactional-outbox relay (M4): reconcile any message event whose
    // inline publish was dropped (process crash / bus outage). Runs only when
    // chat is in-process; in split mode the chat-service bin owns the write path
    // and runs its own relay. ---
    if !cfg.split {
        let pool = pool.clone();
        let bus = relay_bus;
        let token = shutdown.child_token();
        shutdown.tracker.spawn(async move {
            tokio::select! {
                () = chat_service::relay::run(pool, bus) => {}
                () = token.cancelled() => {}
            }
        });
        tracing::info!("outbox relay started (in-process chat)");
    }

    // notification-service: durable JetStream consumer that maintains per-user
    // unread counts. Full profile only — dev-lite's in-process Local bus has no
    // stream, so unread badges there come purely from the live client side.
    if let dice_event_bus::BusConfig::Nats { url } = &cfg.bus {
        let url = url.clone();
        let pool = pool.clone();
        let unread = dice_cache::UnreadStore::new(cache.clone());
        let token = shutdown.child_token();
        shutdown.tracker.spawn(async move {
            if let Err(error) = notification_service::run(&url, pool, unread, token).await {
                tracing::error!(%error, "notification-service consumer exited with error");
            }
        });
        tracing::info!("notification-service: durable consumer on DICE_EVT started");
    } else {
        tracing::info!("notification-service skipped (dev-lite Local bus has no JetStream)");
    }

    let started = api_gateway::start(
        GatewayConfig {
            rest_addr: cfg.rest_addr,
            quic_addr: cfg.quic_addr,
            tls_cert: tls.cert.clone(),
            tls_key: tls.key.clone(),
            heartbeat_interval_ms: HEARTBEAT_INTERVAL_MS,
            resume_window_ms: RESUME_WINDOW_MS,
        },
        deps,
        shutdown.child_token(),
    )
    .await
    .context("start api-gateway")?;

    banner(&cfg, started.bound_rest, started.bound_quic, &tls);

    // The gateway supervisor joins the drain set: `Shutdown::drain` cancels
    // the token (gateway broadcasts Close{GOING_AWAY}) and waits for it.
    shutdown.tracker.spawn(async move {
        if let Err(error) = started.wait().await {
            tracing::error!(%error, "gateway supervisor exited with error");
        }
    });

    tokio::signal::ctrl_c().await.context("listen for Ctrl-C")?;
    tracing::info!(deadline_s = SHUTDOWN_DEADLINE.as_secs(), "Ctrl-C: draining");
    if shutdown.drain(SHUTDOWN_DEADLINE).await {
        tracing::info!("dice-monolith stopped cleanly");
    } else {
        tracing::warn!("drain deadline expired with tasks still running; exiting anyway");
    }
    dice_logging::shutdown();
    Ok(())
}

/// Startup banner: everything an operator (or the desktop client dev loop)
/// needs to point at this process.
fn banner(
    cfg: &MonolithConfig,
    rest: std::net::SocketAddr,
    quic: std::net::SocketAddr,
    tls: &keygen::TlsPaths,
) {
    let dev_ca = tls
        .dev_ca
        .as_ref()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "(pre-provisioned cert; no dev CA)".to_owned());
    let mode = if cfg.split {
        "split (auth/chat/presence over NATS RPC; media+voice in-process)"
    } else {
        "monolith (all services in-process)"
    };
    println!(
        "\n  dice-monolith up\n\
         ----------------------------------------------------------\n\
         profile   : {}\n\
         mode      : {mode}\n\
         rest+wss  : https://{rest}  (wss path /gateway/v1)\n\
         quic      : {quic}  (ALPN dice/1)\n\
         admin     : http://{}/metrics\n\
         bus       : {}\n\
         cache     : {}\n\
         tls cert  : {}\n\
         dev ca    : {dev_ca}\n\
         ----------------------------------------------------------\n",
        cfg.profile_name(),
        cfg.admin_addr,
        cfg.bus_name(),
        cfg.cache_name(),
        tls.cert.display(),
    );
    tracing::info!(
        profile = cfg.profile_name(),
        %rest,
        %quic,
        bus = cfg.bus_name(),
        cache = cfg.cache_name(),
        "dice-monolith listening"
    );
}

/// Strip credentials from a connection URL for log output.
fn redact(url: &str) -> String {
    match (url.find("//"), url.rfind('@')) {
        (Some(scheme), Some(at)) if at > scheme + 1 => {
            format!("{}//***{}", &url[..scheme], &url[at..])
        }
        _ => url.to_owned(),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::redact;

    #[test]
    fn redact_strips_credentials() {
        assert_eq!(
            redact("postgres://dice:secret@localhost:5433/dice"),
            "postgres://***@localhost:5433/dice"
        );
        assert_eq!(redact("localhost:5433"), "localhost:5433");
    }
}
