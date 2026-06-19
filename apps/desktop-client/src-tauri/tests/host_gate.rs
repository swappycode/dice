//! Headless host gate (no webview, no Tauri runtime): the FULL desktop-host
//! surface — `ClientCore` command bodies + bridge + cache + session — against
//! an IN-PROCESS api-gateway (same wiring as
//! crates/network-core/tests/client_e2e.rs).
//!
//! Flow (the Phase-3 continuation contract):
//! register → login session installed in a fake keystore → gateway `Ready` →
//! `send_message` (sqlite pending row → ack reconciles to the real id) →
//! an incoming message from a second RAW WSS client reaches the cache AND
//! the test emitter → `fetch_messages` pages from the cache → the core is
//! dropped and rebuilt on the same cache path + keystore with NO backend
//! reachable: `get_bootstrap` serves the cached snapshot offline.
//!
//! Needs live Postgres (DATABASE_URL, or the workspace .env).

#![allow(clippy::unwrap_used)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use api_gateway::{GatewayConfig, GatewayDeps, Started};
use auth_service::AuthService;
use chat_service::ChatService;
use dice_auth_core::token::JwtKeys;
use dice_cache::{CacheConfig, RateLimiter};
use dice_common::SnowflakeGenerator;
use dice_common::shutdown::CancellationToken;
use dice_database::{DbConfig, PgPool};
use dice_desktop_lib::dto::{EVENT_CHANNEL, MessageDto};
use dice_desktop_lib::emit::Emitter;
use dice_desktop_lib::keystore::{KeyStore, MemoryKeyStore};
use dice_desktop_lib::state::{ClientCore, CoreConfig};
use dice_event_bus::BusConfig;
use dice_network_core::client::url::Url;
use dice_network_core::client::{
    ApiClient, ClientEvent, Command, GatewayClientConfig, GatewayHandle, TlsOptions, TokenError,
    TokenProvider, connect,
};
use dice_network_core::tls::{generate_dev_certs, install_ring_provider};
use dice_protocol::v1;
use futures_util::future::BoxFuture;
use tokio::sync::mpsc;

const WAIT: Duration = Duration::from_secs(10);

// ------------------------------------------------------------- environment

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn unique(tag: &str) -> (String, String) {
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis();
    let username = format!("{tag}{}h{n}h{stamp}", std::process::id());
    let email = format!("{username}@hostgate.dice.test");
    (username, email)
}

fn node_id() -> u16 {
    let n = COUNTER.fetch_add(1, Ordering::Relaxed) as u16;
    ((std::process::id() as u16).wrapping_mul(43).wrapping_add(n)) & 0x3FF
}

fn database_url() -> String {
    if let Ok(url) = std::env::var("DATABASE_URL") {
        return url;
    }
    let env_path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../.env");
    let content = std::fs::read_to_string(&env_path)
        .unwrap_or_else(|e| panic!("read {} for DATABASE_URL: {e}", env_path.display()));
    for line in content.lines() {
        if let Some(value) = line.trim().strip_prefix("DATABASE_URL=") {
            return value.trim().to_owned();
        }
    }
    panic!("DATABASE_URL not set and not found in the workspace .env");
}

struct Env {
    started: Started,
    ct: CancellationToken,
    pool: PgPool,
    base: Url,
    wss: Url,
    tls: TlsOptions,
    cert_dir: PathBuf,
}

/// dev-lite in-process gateway: Local bus + Memory cache + live Postgres +
/// ephemeral JWT keys + temp-dir dev certs; REST/WSS on port 0.
async fn spawn_env(tag: &str) -> Env {
    install_ring_provider();

    let pool = dice_database::connect(&DbConfig {
        url: database_url(),
        max_connections: 5,
        acquire_timeout: Duration::from_secs(5),
    })
    .await
    .expect("connect to live Postgres (just infra-up)");
    dice_database::migrate(&pool).await.expect("migrations");

    let bus = dice_event_bus::connect(BusConfig::Local { capacity: 4096 })
        .await
        .unwrap();
    let cache = dice_cache::connect(CacheConfig::Memory).await.unwrap();
    let jwt = Arc::new(JwtKeys::generate_ephemeral());
    let ids = Arc::new(SnowflakeGenerator::new(node_id()).unwrap());

    let deps = GatewayDeps {
        cache: cache.clone(),
        auth: Arc::new(AuthService::new(
            pool.clone(),
            cache.clone(),
            jwt.clone(),
            ids.clone(),
            bus.clone(),
        )),
        chat: Arc::new(ChatService::new(pool.clone(), bus.clone(), ids.clone())),
        media: Arc::new(media_service::MediaService::new(
            pool.clone(),
            Arc::new(media_service::LocalFsStore::new(
                std::env::temp_dir().join(format!("dice-host-gate-media-{}", std::process::id())),
            )),
            ids.clone(),
        )),
        presence: Arc::new(presence_service::PresenceService::new(
            cache.clone(),
            bus.clone(),
            pool.clone(),
            ids.clone(),
        )),
        voice: Arc::new(voice_service::VoiceService::new(
            cache.clone(),
            bus.clone(),
            pool.clone(),
            ids.clone(),
        )),
        bus,
        jwt,
        ids,
        unread: dice_cache::UnreadStore::new(cache.clone()),
        rate: RateLimiter::new(cache),
    };

    let cert_dir = std::env::temp_dir().join(format!(
        "dice-host-gate-{tag}-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let certs = generate_dev_certs(&cert_dir).unwrap();

    let ct = CancellationToken::new();
    let started = api_gateway::start(
        GatewayConfig {
            rest_addr: "127.0.0.1:0".parse().unwrap(),
            quic_addr: "127.0.0.1:0".parse().unwrap(),
            tls_cert: certs.server_cert.clone(),
            tls_key: certs.server_key.clone(),
            heartbeat_interval_ms: 30_000,
            resume_window_ms: 60_000,
            quic: Default::default(),
            advertised_addr: None,
        },
        deps,
        ct.clone(),
    )
    .await
    .unwrap();

    let port = started.bound_rest.port();
    Env {
        started,
        ct,
        pool,
        base: Url::parse(&format!("https://localhost:{port}")).unwrap(),
        wss: Url::parse(&format!("wss://localhost:{port}/gateway/v1")).unwrap(),
        tls: TlsOptions {
            extra_ca_pem: Some(certs.ca_pem),
        },
        cert_dir,
    }
}

async fn cleanup(pool: &PgPool, user_ids: &[u64]) {
    for &user in user_ids {
        let _ = sqlx::query("DELETE FROM guilds WHERE owner_id = $1")
            .bind(user as i64)
            .execute(pool)
            .await;
    }
    for &user in user_ids {
        let _ = sqlx::query("DELETE FROM users WHERE id = $1")
            .bind(user as i64)
            .execute(pool)
            .await;
    }
}

// ------------------------------------------------- test seams (the traits)

/// Channel-backed [`Emitter`]: what production sends to the webview, the test
/// receives on an unbounded mpsc.
struct TestEmitter(mpsc::UnboundedSender<(String, serde_json::Value)>);

impl Emitter for TestEmitter {
    fn emit(&self, event: &str, payload: serde_json::Value) {
        let _ = self.0.send((event.to_owned(), payload));
    }
}

fn test_emitter() -> (
    Arc<dyn Emitter>,
    mpsc::UnboundedReceiver<(String, serde_json::Value)>,
) {
    let (tx, rx) = mpsc::unbounded_channel();
    (Arc::new(TestEmitter(tx)), rx)
}

/// Next `dice://event` payload matching `pick`, skipping everything else
/// (conn-state chatter, presence noise). Bounded + timed so a wedged host
/// fails loudly.
async fn expect_emitted<T>(
    rx: &mut mpsc::UnboundedReceiver<(String, serde_json::Value)>,
    what: &str,
    pick: impl Fn(&serde_json::Value) -> Option<T>,
) -> T {
    for _ in 0..256 {
        let (channel, payload) = tokio::time::timeout(WAIT, rx.recv())
            .await
            .unwrap_or_else(|_| panic!("timed out waiting for: {what}"))
            .expect("emitter channel closed");
        if channel != EVENT_CHANNEL {
            continue;
        }
        if let Some(found) = pick(&payload) {
            return found;
        }
    }
    panic!("never received: {what}");
}

struct StaticToken(String);

impl TokenProvider for StaticToken {
    fn access_token(&self) -> BoxFuture<'_, Result<String, TokenError>> {
        let token = self.0.clone();
        Box::pin(async move { Ok(token) })
    }
}

/// Raw second client: the bare network-core driver, no ClientCore on top.
fn raw_client(env: &Env, token: &str) -> GatewayHandle {
    connect(
        GatewayClientConfig {
            wss_url: env.wss.clone(),
            quic: None,
            policy: dice_network_core::client::TransportPolicy::WssOnly,
            initial_preference: None,
            tls: env.tls.clone(),
            token: Arc::new(StaticToken(token.to_owned())),
            properties: v1::ClientProperties {
                client: "dice-host-gate-raw".into(),
                version: "0.0-test".into(),
                os: "windows".into(),
            },
        },
        tokio::runtime::Handle::current(),
    )
}

async fn expect_ready(handle: &mut GatewayHandle) -> v1::Ready {
    for _ in 0..64 {
        let event = tokio::time::timeout(WAIT, handle.events().recv())
            .await
            .expect("timed out waiting for raw-client Ready")
            .expect("raw-client driver ended unexpectedly");
        if let ClientEvent::Ready(ready) = event {
            return *ready;
        }
    }
    panic!("raw client never became Ready");
}

fn temp_cache(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!(
        "dice-host-gate-{tag}-{}-{nanos}.db",
        std::process::id()
    ))
}

fn message_create(payload: &serde_json::Value) -> Option<(MessageDto, Option<String>)> {
    if payload["type"] != "messageCreate" {
        return None;
    }
    let message: MessageDto = serde_json::from_value(payload["message"].clone()).ok()?;
    let nonce = payload["nonce"].as_str().map(str::to_owned);
    Some((message, nonce))
}

// ------------------------------------------------------------------- gate

#[tokio::test]
async fn host_gate_full_journey_and_offline_restart() {
    let env = spawn_env("gate").await;
    let cache_path = temp_cache("core");
    let keys: Arc<MemoryKeyStore> = Arc::default();
    let cfg = CoreConfig {
        api_url: env.base.clone(),
        wss_url: env.wss.clone(),
        quic: None,
        policy: dice_network_core::client::TransportPolicy::WssOnly,
        tls: env.tls.clone(),
        cache_path: cache_path.clone(),
    };

    // ---- ClientCore A: register → session in the fake keystore → Ready ----
    let (emitter, mut events) = test_emitter();
    let core = ClientCore::new(
        cfg.clone(),
        keys.clone() as Arc<dyn KeyStore>,
        emitter,
        tokio::runtime::Handle::current(),
    )
    .unwrap();

    let (username, email) = unique("ga");
    let session = core
        .register(&email, &username, "correct-horse-battery")
        .await
        .unwrap();
    assert_eq!(session.user.username, username);
    let my_id = session.user.id.clone();
    assert!(
        keys.get().unwrap().is_some_and(|t| t.starts_with("drt_")),
        "register must land the rotating refresh token in the keystore"
    );
    assert!(core.has_stored_session());

    // get_bootstrap waits for the first Ready to hit the cache.
    let boot = tokio::time::timeout(WAIT, core.get_bootstrap())
        .await
        .expect("bootstrap timed out")
        .unwrap();
    assert_eq!(boot.user.id, my_id);
    assert!(boot.guilds.is_empty());

    // ---- guild + channel over REST (cache applies immediately) ----
    let guild = core.create_guild("host gate hq").await.unwrap();
    assert!(!guild.invite_code.is_empty());
    let boot = core.get_bootstrap().await.unwrap();
    let general = boot
        .channels
        .iter()
        .find(|c| c.guild_id.as_deref() == Some(guild.id.as_str()) && c.name == "general")
        .expect("auto-created #general in the cache")
        .clone();

    // ---- optimistic send: pending sqlite row → ack reconciles ----
    let channel_id: u64 = general.id.parse().unwrap();
    let pending = core
        .send_message(&general.id, "hello from the host", None, &[], "nonce-a1")
        .await
        .unwrap();
    assert_eq!(pending.pending, Some(true), "send returns the PENDING row");
    assert!(
        pending.id.starts_with('-'),
        "pending rows use negative synthetic ids"
    );
    assert_eq!(pending.nonce.as_deref(), Some("nonce-a1"));
    // The row is in sqlite right now (the ack may already race us, so accept
    // either the pending row or its reconciled replacement — same nonce).
    let page = core
        .cache()
        .page_messages(channel_id, None, 50)
        .await
        .unwrap();
    assert_eq!(page.len(), 1, "exactly one row for the optimistic send");
    assert_eq!(page[0].nonce.as_deref(), Some("nonce-a1"));
    if page[0].pending == Some(true) {
        assert_eq!(page[0].id, pending.id, "still the pending row");
    }

    // The bridge re-emits the reconciled message with the SAME client nonce.
    let (echoed, nonce) =
        expect_emitted(&mut events, "own messageCreate echo", message_create).await;
    assert_eq!(nonce.as_deref(), Some("nonce-a1"));
    assert!(!echoed.id.starts_with('-'), "echo carries the REAL id");
    assert_eq!(echoed.content, "hello from the host");
    // …and sqlite now holds the real row instead of the pending one.
    let page = core
        .cache()
        .page_messages(channel_id, None, 50)
        .await
        .unwrap();
    assert_eq!(page.len(), 1, "reconcile replaced the pending row");
    assert_eq!(page[0].id, echoed.id);
    assert_eq!(page[0].pending, None);
    assert_eq!(page[0].failed, None);
    assert_eq!(core.connection_state(), "connected");
    // Phase 4: the bridge persisted the active transport for the next start.
    // (The in-process gateway here is reached via WssOnly policy.)
    assert_eq!(
        core.cache()
            .get_meta("last_transport".to_owned())
            .await
            .unwrap()
            .as_deref(),
        Some("wss")
    );

    // ---- a second RAW WSS client joins and talks ----
    let api = ApiClient::new(env.base.clone(), &env.tls).unwrap();
    let (b_name, b_email) = unique("gb");
    let b_auth = api
        .register(&b_email, &b_name, "correct-horse-battery")
        .await
        .unwrap();
    let b_user = b_auth.user.clone().unwrap();
    let b_api = api
        .clone()
        .with_token_provider(Arc::new(StaticToken(b_auth.access_token.clone())));
    let joined = b_api.join_guild(&guild.invite_code).await.unwrap();
    assert_eq!(joined.id.to_string(), guild.id);

    let mut raw = raw_client(&env, &b_auth.access_token);
    let _ = expect_ready(&mut raw).await;
    raw.send(Command::SendMessage {
        channel_id,
        content: "hello from the raw client".into(),
        reply_to_id: 0,
        attachment_ids: Vec::new(),
        nonce: 0xB0B,
    })
    .await
    .unwrap();

    // The incoming dispatch reaches the test emitter…
    let (incoming, _) = expect_emitted(&mut events, "incoming messageCreate", |p| {
        message_create(p).filter(|(m, _)| m.content == "hello from the raw client")
    })
    .await;
    assert_eq!(incoming.author_id, b_user.id.to_string());
    // …AND the cache.
    let page = core
        .cache()
        .page_messages(channel_id, None, 50)
        .await
        .unwrap();
    assert_eq!(page.len(), 2);
    assert_eq!(page[1].id, incoming.id, "ascending: newest row is last");

    // ---- fetch_messages pages from the cache ----
    let newest = core
        .fetch_messages(&general.id, None, Some(50))
        .await
        .unwrap();
    let contents: Vec<&str> = newest.iter().map(|m| m.content.as_str()).collect();
    assert_eq!(
        contents,
        vec!["hello from the host", "hello from the raw client"],
        "newest page is ascending and complete"
    );
    let older = core
        .fetch_messages(&general.id, Some(&incoming.id), Some(50))
        .await
        .unwrap();
    let contents: Vec<&str> = older.iter().map(|m| m.content.as_str()).collect();
    assert_eq!(
        contents,
        vec!["hello from the host"],
        "before-cursor pages strictly older rows"
    );

    // ---- offline restart: same cache path + keystore, backend GONE ----
    raw.shutdown().await;
    core.shutdown_gateway().await;
    drop(core);

    env.ct.cancel();
    tokio::time::timeout(Duration::from_secs(15), env.started.wait())
        .await
        .expect("backend drain timed out")
        .unwrap();

    let (emitter2, _events2) = test_emitter();
    let core2 = ClientCore::new(
        cfg,
        keys.clone() as Arc<dyn KeyStore>,
        emitter2,
        tokio::runtime::Handle::current(),
    )
    .unwrap();
    let resumed = core2
        .session_status()
        .await
        .unwrap()
        .expect("stored session + cached user must resume offline");
    assert_eq!(resumed.user.id, my_id);

    let boot = core2.get_bootstrap().await.unwrap();
    assert_eq!(boot.user.id, my_id, "bootstrap user served from the cache");
    assert_eq!(boot.guilds.len(), 1);
    assert_eq!(boot.guilds[0].id, guild.id);
    assert_eq!(
        boot.guilds[0].members.len(),
        2,
        "the joiner's MemberAdd dispatch was cached"
    );
    assert!(boot.channels.iter().any(|c| c.id == general.id));
    assert!(
        boot.users.iter().any(|u| u.id == b_user.id.to_string()),
        "user dictionary includes the raw client's user"
    );

    // History also degrades to the cache when the API is unreachable.
    let offline = core2
        .fetch_messages(&general.id, None, Some(50))
        .await
        .unwrap();
    assert_eq!(offline.len(), 2, "messages served offline from sqlite");

    core2.shutdown_gateway().await;
    drop(core2);

    cleanup(&env.pool, &[my_id.parse().unwrap(), b_user.id]).await;
    let _ = std::fs::remove_file(&cache_path);
    let _ = std::fs::remove_dir_all(&env.cert_dir);
}

/// Issue 1 regression: a stored session the server no longer accepts must
/// drop the client to login (clear credentials + emit `sessionExpired`),
/// never hang on an "Offline" shell. Reproduces the dev-loop case where a
/// monolith restart wiped the server-side session the client still holds.
#[tokio::test]
async fn expired_session_routes_to_login_instead_of_hanging_offline() {
    let env = spawn_env("expire").await;
    let cache_path = temp_cache("expire");

    // A real account whose refresh token we then REVOKE server-side, so any
    // refresh the client attempts comes back 401 (the "server lost my
    // session" condition).
    let api = ApiClient::new(env.base.clone(), &env.tls).unwrap();
    let (username, email) = unique("ex");
    let auth = api
        .register(&email, &username, "correct-horse-battery")
        .await
        .unwrap();
    let user = auth.user.clone().unwrap();
    api.logout(&auth.refresh_token).await.unwrap(); // revoke the family

    // Stored (now-dead) refresh token + a cached user row → `session_status`
    // returns Some and the UI renders the shell (cache-first).
    let keys: Arc<MemoryKeyStore> = Arc::default();
    keys.set(&auth.refresh_token).unwrap();
    let cfg = CoreConfig {
        api_url: env.base.clone(),
        wss_url: env.wss.clone(),
        quic: None,
        policy: dice_network_core::client::TransportPolicy::WssOnly,
        tls: env.tls.clone(),
        cache_path: cache_path.clone(),
    };
    let (emitter, mut events) = test_emitter();
    let core = ClientCore::new(
        cfg,
        keys.clone() as Arc<dyn KeyStore>,
        emitter,
        tokio::runtime::Handle::current(),
    )
    .unwrap();
    core.cache().set_current_user(user.clone()).await.unwrap();

    let resumed = core.session_status().await.unwrap();
    assert!(
        resumed.is_some(),
        "cache-first: the shell renders before the gateway proves liveness"
    );
    assert!(core.has_stored_session());

    // The gateway driver refreshes → 401 → AuthExpired → the bridge clears
    // credentials and tells the webview to show login.
    expect_emitted(&mut events, "sessionExpired", |p| {
        (p["type"] == "sessionExpired").then_some(())
    })
    .await;
    assert!(
        !core.has_stored_session(),
        "credentials cleared so the next launch shows login, not an Offline shell"
    );

    core.shutdown_gateway().await;
    drop(core);
    cleanup(&env.pool, &[user.id]).await;
    let _ = std::fs::remove_file(&cache_path);
    let _ = std::fs::remove_dir_all(&env.cert_dir);
}
