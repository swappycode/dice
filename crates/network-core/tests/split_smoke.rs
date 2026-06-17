//! Live split-fleet smoke test: drives the REAL `ApiClient` + gateway driver
//! over the network against an EXTERNAL running gateway (NOT in-process like
//! `client_e2e.rs`). It is the automated form of the "drive the client through
//! the split fleet" demo that M4 (3/n) left to a human.
//!
//! It exercises every gateway-facing service through the `DICE_SPLIT=1` NATS-RPC
//! seam in one journey:
//!   - **auth-service** — `register` + `login` (gateway forwards over `dice.rpc.auth.*`)
//!   - **chat-service** — `create_guild`, the socket `SendMessage` persist+ack,
//!     `sync_user_state` at Ready, and REST history (`dice.rpc.chat.*`)
//!   - **presence-service** — set-online during the Ready handshake (`dice.rpc.presence.*`)
//!
//! `#[ignore]` so `just check` skips it (it needs a live fleet, not an
//! in-process gateway). To run it:
//!   just infra-up
//!   just split-up            # gateway (DICE_SPLIT=1) + auth/chat/presence bins
//!   cargo test -p dice-network-core --test split_smoke -- --ignored --nocapture
//!
//! Targets `https://localhost:8443` with the dev CA at `dev/certs/dev-ca.pem`
//! by default; override with `DICE_SMOKE_URL` / `DICE_SMOKE_CA`. Needs
//! `DATABASE_URL` (or the workspace `.env`) for row cleanup. Identities are
//! unique per process+counter and the rows are removed afterwards.

#![allow(clippy::unwrap_used)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use dice_database::{DbConfig, PgPool};
use dice_network_core::client::url::Url;
use dice_network_core::client::{
    ApiClient, ClientEvent, Command, GatewayClientConfig, GatewayHandle, LoginOutcome, TlsOptions,
    TokenError, TokenProvider, TransportPolicy, connect,
};
use dice_network_core::tls::install_ring_provider;
use dice_protocol::v1;
use futures_util::future::BoxFuture;

const PASSWORD: &str = "correct-horse-battery";
const RECV_TIMEOUT: Duration = Duration::from_secs(10);
/// The fleet may still be booting (the bins start a few seconds after the
/// gateway); retry the first auth call until auth-service is answering RPC.
const FLEET_READY_ATTEMPTS: u32 = 30;

static COUNTER: AtomicU64 = AtomicU64::new(0);

// ----------------------------------------------------------- configuration

fn smoke_base() -> String {
    std::env::var("DICE_SMOKE_URL").unwrap_or_else(|_| "https://localhost:8443".to_owned())
}

/// Derive the WSS gateway URL from the REST base (same host/port, `/gateway/v1`).
fn smoke_wss() -> String {
    let base = smoke_base();
    let host = base.strip_prefix("https://").unwrap_or(&base);
    format!("wss://{host}/gateway/v1")
}

fn workspace_path(rel: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(rel)
}

fn dev_ca_path() -> PathBuf {
    let path = std::env::var("DICE_SMOKE_CA")
        .map(PathBuf::from)
        .unwrap_or_else(|_| workspace_path("dev/certs/dev-ca.pem"));
    assert!(
        path.exists(),
        "dev CA not found at {} (run the gateway once to generate it)",
        path.display()
    );
    path
}

fn database_url() -> String {
    if let Ok(url) = std::env::var("DATABASE_URL") {
        return url;
    }
    let env_path = workspace_path(".env");
    let content = std::fs::read_to_string(&env_path)
        .unwrap_or_else(|e| panic!("read {} for DATABASE_URL: {e}", env_path.display()));
    for line in content.lines() {
        if let Some(value) = line.trim().strip_prefix("DATABASE_URL=") {
            return value.trim().to_owned();
        }
    }
    panic!("DATABASE_URL not set and not found in workspace .env");
}

fn unique(tag: &str) -> (String, String) {
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis();
    let username = format!("{tag}{}x{n}x{stamp}", std::process::id());
    let email = format!("{username}@splitsmoke.dice.test");
    (username, email)
}

// --------------------------------------------------------- token provider

struct StaticToken(String);

impl TokenProvider for StaticToken {
    fn access_token(&self) -> BoxFuture<'_, Result<String, TokenError>> {
        let token = self.0.clone();
        Box::pin(async move { Ok(token) })
    }
}

// ------------------------------------------------------------- gw helpers

fn gw_connect(wss: &Url, tls: &TlsOptions, provider: Arc<dyn TokenProvider>) -> GatewayHandle {
    connect(
        GatewayClientConfig {
            wss_url: wss.clone(),
            quic: None,
            policy: TransportPolicy::WssOnly,
            initial_preference: None,
            tls: tls.clone(),
            token: provider,
            properties: v1::ClientProperties {
                client: "dice-desktop".into(),
                version: "0.0-split-smoke".into(),
                os: "windows".into(),
            },
        },
        tokio::runtime::Handle::current(),
    )
}

async fn next_event(handle: &mut GatewayHandle) -> ClientEvent {
    tokio::time::timeout(RECV_TIMEOUT, handle.events().recv())
        .await
        .expect("timed out waiting for a client event")
        .expect("gateway driver ended unexpectedly")
}

async fn expect_ready(handle: &mut GatewayHandle) -> v1::Ready {
    for _ in 0..64 {
        if let ClientEvent::Ready(ready) = next_event(handle).await {
            return *ready;
        }
    }
    panic!("never received Ready");
}

/// Best-effort row cleanup (guilds cascade to channels/members).
async fn cleanup(pool: &PgPool, user_id: u64, guild_id: u64) {
    let _ = sqlx::query("DELETE FROM guilds WHERE id = $1")
        .bind(guild_id as i64)
        .execute(pool)
        .await;
    let _ = sqlx::query("DELETE FROM users WHERE id = $1")
        .bind(user_id as i64)
        .execute(pool)
        .await;
}

// ------------------------------------------------------------------- test

#[tokio::test]
#[ignore = "needs a running split fleet: just infra-up + just split-up; run with --ignored"]
async fn split_fleet_end_to_end() {
    install_ring_provider();
    let tls = TlsOptions {
        extra_ca_pem: Some(dev_ca_path()),
    };
    let base = Url::parse(&smoke_base()).unwrap();
    let api = ApiClient::new(base, &tls).unwrap();

    let (username, email) = unique("smoke");

    // -- auth-service over RPC: register (retried until the fleet is serving) --
    let mut last_err = String::new();
    let auth = 'register: {
        for attempt in 1..=FLEET_READY_ATTEMPTS {
            match api.register(&email, &username, PASSWORD).await {
                Ok(auth) => break 'register auth,
                Err(error) => {
                    last_err = format!("{error:?}");
                    eprintln!(
                        "register attempt {attempt}/{FLEET_READY_ATTEMPTS} failed: {last_err}"
                    );
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            }
        }
        panic!("register never succeeded — is the split fleet up? last error: {last_err}");
    };
    let user = auth.user.clone().unwrap();
    assert!(
        !auth.access_token.is_empty(),
        "register returns an access token"
    );
    eprintln!(
        "auth-service OK: registered {} (id {})",
        user.username, user.id
    );

    // -- auth-service over RPC: login round-trip resolves to the same user --
    let logged_in = match api.login(&email, PASSWORD).await.unwrap() {
        LoginOutcome::Success(auth) => auth,
        other => panic!("expected LoginOutcome::Success, got {other:?}"),
    };
    assert_eq!(
        logged_in.user.unwrap().id,
        user.id,
        "login resolves the same user"
    );
    eprintln!("auth-service OK: login round-trip");

    let a_api = api
        .clone()
        .with_token_provider(Arc::new(StaticToken(auth.access_token.clone())));

    // -- chat-service over RPC: create a guild (auto #general) --
    let guild = a_api.create_guild("split smoke hq").await.unwrap();
    assert!(!guild.invite_code.is_empty(), "guild has an invite code");
    let general = guild
        .channels
        .iter()
        .find(|c| c.name == "general")
        .expect("auto-created #general")
        .clone();
    eprintln!(
        "chat-service OK: created guild {} (#{} general)",
        guild.id, general.id
    );

    // -- gateway realtime over WSS: Ready proves chat.sync_user_state +
    //    presence set-online both round-trip over RPC during the handshake --
    let mut handle = gw_connect(
        &smoke_wss_url(),
        &tls,
        Arc::new(StaticToken(auth.access_token.clone())),
    );
    let ready = expect_ready(&mut handle).await;
    assert_eq!(
        ready.user.as_ref().unwrap().id,
        user.id,
        "Ready is for our user"
    );
    assert!(
        ready.guilds.iter().any(|g| g.id == guild.id),
        "Ready (built via chat.sync_user_state RPC) includes the new guild",
    );
    eprintln!("gateway OK: Ready over WSS (chat+presence RPC engaged)");

    // -- send over the socket: chat-service persists it and the gateway acks
    //    back, nonce-correlated --
    let nonce = 0x5A0E_5A0E_u64;
    handle
        .send(Command::SendMessage {
            channel_id: general.id,
            content: "hello split fleet".into(),
            reply_to_id: 0,
            attachment_ids: Vec::new(),
            nonce,
        })
        .await
        .unwrap();
    let acked = 'ack: {
        for _ in 0..64 {
            if let ClientEvent::Ack { nonce: n, message } = next_event(&mut handle).await {
                assert_eq!(n, nonce, "ack echoes the request nonce");
                break 'ack message;
            }
        }
        panic!("never received the SendMessage ack");
    };
    assert_eq!(acked.content, "hello split fleet");
    assert_eq!(acked.author_id, user.id);
    eprintln!("chat-service OK: message persisted + acked over the socket");

    // -- REST history through chat-service RPC: the message is readable back --
    let history = a_api
        .fetch_messages(general.id, None, None, 50)
        .await
        .unwrap();
    assert!(
        history.iter().any(|m| m.content == "hello split fleet"),
        "the sent message is in REST history",
    );
    eprintln!(
        "chat-service OK: message readable via REST history ({} msg)",
        history.len()
    );

    // -- teardown + row cleanup --
    handle.shutdown().await;
    let pool = dice_database::connect(&DbConfig {
        url: database_url(),
        max_connections: 2,
        acquire_timeout: Duration::from_secs(5),
    })
    .await
    .expect("connect Postgres for cleanup");
    cleanup(&pool, user.id, guild.id).await;

    eprintln!(
        "\n  ✅ split-fleet end-to-end PASS — auth + chat + presence all round-tripped over NATS RPC\n"
    );
}

fn smoke_wss_url() -> Url {
    Url::parse(&smoke_wss()).unwrap()
}
