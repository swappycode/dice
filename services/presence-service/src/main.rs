//! presence-service: the split-mode deployment shape. The monolith mounts
//! [`PresenceService`] in-process behind `Arc<dyn Presence>`; this bin runs the
//! SAME service as a standalone process that answers `dice.rpc.presence.*` over
//! NATS (see `presence_service::rpc`). A gateway started with `DICE_SPLIT=1`
//! reaches it through the unchanged trait seam. Shares Postgres + Redis + NATS
//! with the rest of the fleet (`just split-up`).
//!
//! Config is env-only: `DATABASE_URL`, `DICE_REDIS_URL`, `DICE_NATS_URL`, and
//! `DICE_NODE_ID` (MUST be distinct per service so minted snowflake ids never
//! collide).

use std::sync::Arc;

use anyhow::Context as _;
use dice_cache::{CacheConfig, DEFAULT_REDIS_URL};
use dice_common::SnowflakeGenerator;
use dice_common::config::{env_opt, env_or};
use dice_database::DbConfig;
use dice_event_bus::rpc::RpcClient;
use dice_event_bus::{BusConfig, DEFAULT_NATS_URL};
use presence_service::PresenceService;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dice_logging::init(&dice_logging::LogConfig::from_env());

    let nats_url = env_opt("DICE_NATS_URL").unwrap_or_else(|| DEFAULT_NATS_URL.to_owned());
    let redis_url = env_opt("DICE_REDIS_URL").unwrap_or_else(|| DEFAULT_REDIS_URL.to_owned());

    let pool = dice_database::connect(&DbConfig::from_env()?)
        .await
        .context("connect Postgres")?;
    let cache = dice_cache::connect(CacheConfig::Redis { url: redis_url })
        .await
        .context("connect Redis cache")?;
    let bus = dice_event_bus::connect(BusConfig::Nats {
        url: nats_url.clone(),
    })
    .await
    .context("connect NATS bus")?;
    let ids =
        Arc::new(SnowflakeGenerator::new(env_or("DICE_NODE_ID", 0u16)?).context("DICE_NODE_ID")?);

    dice_database::init_metrics_from_env(&pool);
    let presence = Arc::new(PresenceService::new(cache, bus, pool, ids));
    let rpc = RpcClient::connect(&nats_url)
        .await
        .context("connect split-mode RPC client")?;

    tracing::info!(%nats_url, subject = "dice.rpc.presence.*", "presence-service: serving split-mode RPC");
    println!("presence-service up — answering dice.rpc.presence.* on {nats_url} (Ctrl-C to stop)");

    tokio::select! {
        result = presence_service::rpc::serve(rpc, presence) => {
            result.context("presence RPC responder exited")?;
        }
        _ = tokio::signal::ctrl_c() => {
            tracing::info!("presence-service: Ctrl-C received, shutting down");
        }
    }
    Ok(())
}
