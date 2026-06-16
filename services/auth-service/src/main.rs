//! auth-service: the split-mode deployment shape. The monolith mounts
//! [`AuthService`] in-process behind `Arc<dyn Auth>`; this bin runs the SAME
//! service as a standalone process that answers `dice.rpc.auth.*` over NATS
//! (see `auth_service::rpc`). A gateway started with `DICE_SPLIT=1` reaches it
//! through the unchanged trait seam. Shares Postgres + Redis + NATS with the
//! rest of the fleet (`just split-up`).
//!
//! Config is env-only — a subset of the monolith's table: `DATABASE_URL`,
//! `DICE_REDIS_URL`, `DICE_NATS_URL`, `DICE_NODE_ID` (MUST be distinct per
//! service so minted snowflake ids never collide), and the `DICE_JWT_*` PEM
//! paths. The JWT keys are loaded READ-ONLY: the gateway/monolith owns key
//! generation, and auth-service must read the SAME files so the access tokens
//! it mints verify back at the gateway.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context as _;
use auth_service::AuthService;
use dice_auth_core::token::JwtKeys;
use dice_cache::{CacheConfig, DEFAULT_REDIS_URL};
use dice_common::SnowflakeGenerator;
use dice_common::config::{env_opt, env_or};
use dice_database::DbConfig;
use dice_event_bus::rpc::RpcClient;
use dice_event_bus::{BusConfig, DEFAULT_NATS_URL};

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
    let jwt = load_jwt()?;

    let auth = Arc::new(AuthService::new(pool, cache, jwt, ids, bus));
    let rpc = RpcClient::connect(&nats_url)
        .await
        .context("connect split-mode RPC client")?;

    tracing::info!(%nats_url, subject = "dice.rpc.auth.*", "auth-service: serving split-mode RPC");
    println!("auth-service up — answering dice.rpc.auth.* on {nats_url} (Ctrl-C to stop)");

    tokio::select! {
        result = auth_service::rpc::serve(rpc, auth) => {
            result.context("auth RPC responder exited")?;
        }
        _ = tokio::signal::ctrl_c() => {
            tracing::info!("auth-service: Ctrl-C received, shutting down");
        }
    }
    Ok(())
}

/// Load the Ed25519 signing pair from the configured PEM files. Unlike the
/// monolith's `keygen`, the service NEVER generates keys — the gateway owns
/// generation and persistence, and auth-service reads the SAME files (shared
/// dev dir) so its tokens verify at the gateway.
fn load_jwt() -> anyhow::Result<Arc<JwtKeys>> {
    let private = path_or("DICE_JWT_PRIVATE_PEM", "dev/keys/jwt_ed25519.pem");
    let public = path_or("DICE_JWT_PUBLIC_PEM", "dev/keys/jwt_ed25519.pub.pem");
    let private_pem = std::fs::read(&private).with_context(|| {
        format!(
            "read {} (run the gateway once to generate dev keys)",
            private.display()
        )
    })?;
    let public_pem =
        std::fs::read(&public).with_context(|| format!("read {}", public.display()))?;
    let keys = JwtKeys::from_pem(&private_pem, &public_pem).context("parse JWT PEM pair")?;
    anyhow::ensure!(keys.can_sign(), "JWT private key is not signing-capable");
    Ok(Arc::new(keys))
}

fn path_or(key: &'static str, default: &str) -> PathBuf {
    PathBuf::from(env_opt(key).unwrap_or_else(|| default.to_owned()))
}
