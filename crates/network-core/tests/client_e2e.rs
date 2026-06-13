//! Client-half E2E: the real `ApiClient` + gateway driver against an
//! IN-PROCESS api-gateway (same wiring as services/api-gateway/tests/
//! gateway_e2e.rs: live Postgres + Local bus + Memory cache + ephemeral JWT
//! keys + temp-dir dev certs, port 0).
//!
//! Needs live Postgres (DATABASE_URL, or the workspace .env). Identities are
//! unique per process+counter and rows are removed afterwards.

#![allow(clippy::unwrap_used)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::Duration;

use api_gateway::{GatewayConfig, GatewayDeps, Started};
use auth_service::AuthService;
use chat_service::ChatService;
use dice_auth_core::token::JwtKeys;
use dice_cache::{CacheConfig, RateLimiter};
use dice_common::SnowflakeGenerator;
use dice_common::shutdown::CancellationToken;
use dice_database::{DbConfig, PgPool};
use dice_event_bus::BusConfig;
use dice_network_core::client::url::Url;
use dice_network_core::client::{
    ApiClient, ApiError, ClientEvent, Command, ConnState, ConnStateLite, GatewayClientConfig,
    GatewayHandle, PreferredTransport, QuicAddr, QuicEndpoint, QuicTransport, TlsOptions,
    TokenError, TokenProvider, TransportKind, TransportPolicy, connect,
};
use dice_network_core::tls::{generate_dev_certs, install_ring_provider};
use dice_protocol::v1::frame::Payload;
use dice_protocol::v1::{self, ErrorCode, Frame};
use futures_util::future::BoxFuture;
use presence_service::PresenceService;

const RECV_TIMEOUT: Duration = Duration::from_secs(10);

// ------------------------------------------------------------ environment

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn unique(tag: &str) -> (String, String) {
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis();
    let username = format!("{tag}{}x{n}x{stamp}", std::process::id());
    let email = format!("{username}@cliente2e.dice.test");
    (username, email)
}

fn node_id() -> u16 {
    let n = COUNTER.fetch_add(1, Ordering::Relaxed) as u16;
    ((std::process::id() as u16).wrapping_mul(41).wrapping_add(n)) & 0x3FF
}

fn database_url() -> String {
    if let Ok(url) = std::env::var("DATABASE_URL") {
        return url;
    }
    let env_path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.env");
    let content = std::fs::read_to_string(&env_path)
        .unwrap_or_else(|e| panic!("read {} for DATABASE_URL: {e}", env_path.display()));
    for line in content.lines() {
        if let Some(value) = line.trim().strip_prefix("DATABASE_URL=") {
            return value.trim().to_owned();
        }
    }
    panic!("DATABASE_URL not set and not found in workspace .env");
}

struct Env {
    started: Started,
    ct: CancellationToken,
    pool: PgPool,
    base: Url,
    wss: Url,
    /// Bound QUIC endpoint (UDP, ALPN `dice/1`).
    quic: std::net::SocketAddr,
    tls: TlsOptions,
    cert_dir: PathBuf,
}

/// dev-lite in-process gateway: Local bus + Memory cache + live Postgres +
/// ephemeral JWT keys + temp-dir dev certs; REST/WSS on port 0.
async fn spawn_env(tag: &str, heartbeat_interval_ms: u32, resume_window_ms: u32) -> Env {
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
                std::env::temp_dir().join(format!("dice-client-e2e-media-{}", std::process::id())),
            )),
            ids.clone(),
        )),
        presence: Arc::new(PresenceService::new(
            cache.clone(),
            bus.clone(),
            pool.clone(),
            ids.clone(),
        )),
        bus,
        jwt,
        ids,
        rate: RateLimiter::new(cache),
    };

    let cert_dir = std::env::temp_dir().join(format!(
        "dice-client-e2e-{tag}-{}-{}",
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
            heartbeat_interval_ms,
            resume_window_ms,
        },
        deps,
        ct.clone(),
    )
    .await
    .unwrap();

    let port = started.bound_rest.port();
    let quic = started.bound_quic;
    Env {
        started,
        ct,
        pool,
        base: Url::parse(&format!("https://localhost:{port}")).unwrap(),
        wss: Url::parse(&format!("wss://localhost:{port}/gateway/v1")).unwrap(),
        quic,
        tls: TlsOptions {
            extra_ca_pem: Some(certs.ca_pem),
        },
        cert_dir,
    }
}

/// Dial the in-process gateway's QUIC port with SNI `localhost` (the dev
/// leaf's DNS SAN) even though the addr is 127.0.0.1 — addr and server name
/// are deliberately decoupled.
fn quic_endpoint(addr: std::net::SocketAddr) -> QuicEndpoint {
    QuicEndpoint {
        server_name: "localhost".to_owned(),
        addr: QuicAddr::Socket(addr),
    }
}

/// Best-effort row cleanup (guilds cascade; DM channels go explicitly).
async fn cleanup(pool: &PgPool, user_ids: &[u64], dm_channel_ids: &[u64]) {
    for &channel in dm_channel_ids {
        let _ = sqlx::query("DELETE FROM channels WHERE id = $1")
            .bind(channel as i64)
            .execute(pool)
            .await;
    }
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

// --------------------------------------------------------- token providers

struct StaticToken(String);

impl TokenProvider for StaticToken {
    fn access_token(&self) -> BoxFuture<'_, Result<String, TokenError>> {
        let token = self.0.clone();
        Box::pin(async move { Ok(token) })
    }
}

/// Pops tokens front-to-back; the last one repeats forever. Counts calls.
struct SeqToken {
    tokens: std::sync::Mutex<Vec<String>>,
    calls: AtomicU32,
}

impl SeqToken {
    fn new(tokens: &[&str]) -> Arc<Self> {
        Arc::new(Self {
            tokens: std::sync::Mutex::new(tokens.iter().map(|t| (*t).to_owned()).collect()),
            calls: AtomicU32::new(0),
        })
    }

    fn calls(&self) -> u32 {
        self.calls.load(Ordering::Relaxed)
    }
}

impl TokenProvider for SeqToken {
    fn access_token(&self) -> BoxFuture<'_, Result<String, TokenError>> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        let mut tokens = self.tokens.lock().unwrap();
        let token = if tokens.len() > 1 {
            tokens.remove(0)
        } else {
            tokens[0].clone()
        };
        Box::pin(async move { Ok(token) })
    }
}

// ------------------------------------------------------------- gw helpers

fn gw_connect(env: &Env, provider: Arc<dyn TokenProvider>) -> GatewayHandle {
    gw_connect_with(env, provider, TransportPolicy::WssOnly, None, None)
}

fn gw_connect_with(
    env: &Env,
    provider: Arc<dyn TokenProvider>,
    policy: TransportPolicy,
    quic: Option<QuicEndpoint>,
    initial_preference: Option<PreferredTransport>,
) -> GatewayHandle {
    connect(
        GatewayClientConfig {
            wss_url: env.wss.clone(),
            quic,
            policy,
            initial_preference,
            tls: env.tls.clone(),
            token: provider,
            properties: v1::ClientProperties {
                client: "dice-desktop".into(),
                version: "0.0-test".into(),
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

/// Next event matching `pick`, skipping anything else (presence noise,
/// ConnState mirrors, …). Bounded so a wedged driver fails loudly.
async fn expect_event<T>(
    handle: &mut GatewayHandle,
    what: &str,
    pick: impl Fn(ClientEvent) -> Option<T>,
) -> T {
    for _ in 0..64 {
        if let Some(found) = pick(next_event(handle).await) {
            return found;
        }
    }
    panic!("never received: {what}");
}

fn dispatch_payload(event: ClientEvent) -> Option<Frame> {
    match event {
        ClientEvent::Dispatch(frame) => Some(*frame),
        _ => None,
    }
}

async fn expect_ready(handle: &mut GatewayHandle) -> v1::Ready {
    expect_event(handle, "Ready", |e| match e {
        ClientEvent::Ready(ready) => Some(*ready),
        _ => None,
    })
    .await
}

// ------------------------------------------------------------------ tests

/// REST surface: register/login/refresh/logout happy paths, the 401 mapping,
/// missing-credentials short circuit.
#[tokio::test]
async fn rest_auth_round_trip() {
    let env = spawn_env("auth", 30_000, 60_000).await;
    let api = ApiClient::new(env.base.clone(), &env.tls).unwrap();

    let (username, email) = unique("ca");
    let auth = api
        .register(&email, &username, "correct-horse-battery")
        .await
        .unwrap();
    let user = auth.user.clone().unwrap();
    assert!(!auth.access_token.is_empty());
    assert!(auth.refresh_token.starts_with("drt_"));
    assert!(auth.access_expires_in_s > 0);
    assert_eq!(user.username, username);

    // Login works; a wrong password maps to ApiError::Api{401, UNAUTHENTICATED}.
    let again = api.login(&email, "correct-horse-battery").await.unwrap();
    assert_eq!(again.user.unwrap().id, user.id);
    let denied = api.login(&email, "wrong-password").await.unwrap_err();
    match denied {
        ApiError::Api { status, error } => {
            assert_eq!(status, 401);
            assert_eq!(error.code, ErrorCode::Unauthenticated as i32);
        }
        other => panic!("expected ApiError::Api{{401}}, got {other:?}"),
    }

    // Refresh rotates the opaque token; logout revokes; the dead family
    // cannot refresh again.
    let rotated = api.refresh(&again.refresh_token).await.unwrap();
    assert_ne!(rotated.refresh_token, again.refresh_token);
    api.logout(&rotated.refresh_token).await.unwrap();
    let dead = api.refresh(&rotated.refresh_token).await.unwrap_err();
    assert_eq!(dead.status(), Some(401));

    // Bearer endpoints without a provider short-circuit.
    let no_creds = api.fetch_messages(1, None, None, 50).await.unwrap_err();
    assert!(matches!(
        no_creds,
        ApiError::Token(TokenError::NoCredentials)
    ));

    env.ct.cancel();
    tokio::time::timeout(Duration::from_secs(15), env.started.wait())
        .await
        .unwrap()
        .unwrap();
    cleanup(&env.pool, &[user.id], &[]).await;
    let _ = std::fs::remove_dir_all(&env.cert_dir);
}

/// The full client journey over the driver: Ready (with ConnState
/// transitions), guild create/join dispatches, nonce-correlated send,
/// typing, presence, heartbeat survival, DM, request errors, abrupt
/// reconnect + Resume with the missed dispatch, REST history, 401-retry.
#[tokio::test]
async fn gateway_journey() {
    // Short heartbeat so liveness is exercised inside the test budget: the
    // server drops silent sessions after 2 s, the client after ~3 s of
    // missing acks — staying Ready across 3.5 s proves both directions.
    let env = spawn_env("journey", 1_000, 60_000).await;
    let api = ApiClient::new(env.base.clone(), &env.tls).unwrap();

    let (a_name, a_email) = unique("ja");
    let (b_name, b_email) = unique("jb");
    let a_auth = api
        .register(&a_email, &a_name, "correct-horse-battery")
        .await
        .unwrap();
    let b_auth = api
        .register(&b_email, &b_name, "correct-horse-battery")
        .await
        .unwrap();
    let a_user = a_auth.user.clone().unwrap();
    let b_user = b_auth.user.clone().unwrap();
    let a_api = api
        .clone()
        .with_token_provider(Arc::new(StaticToken(a_auth.access_token.clone())));
    let b_api = api
        .clone()
        .with_token_provider(Arc::new(StaticToken(b_auth.access_token.clone())));

    // -- alice connects: Connecting → Authenticating → Ready(payload) --
    let mut alice = gw_connect(&env, Arc::new(StaticToken(a_auth.access_token.clone())));
    let mut transitions = Vec::new();
    let a_ready = loop {
        match next_event(&mut alice).await {
            ClientEvent::ConnState(state) => transitions.push(state),
            ClientEvent::Ready(ready) => break *ready,
            other => panic!("unexpected pre-Ready event: {other:?}"),
        }
    };
    assert_eq!(
        transitions,
        vec![ConnStateLite::Connecting, ConnStateLite::Authenticating],
        "state transitions precede Ready in order"
    );
    assert_eq!(a_ready.user.as_ref().unwrap().id, a_user.id);
    assert_eq!(a_ready.resume_token.len(), 32);
    assert!(a_ready.guilds.is_empty());
    let a_session = a_ready.gateway_session_id;
    assert!(a_session > 0);
    assert!(matches!(
        &*alice.state().borrow(),
        ConnState::Ready { gateway_session_id, .. } if *gateway_session_id == a_session
    ));

    let mut bob = gw_connect(&env, Arc::new(StaticToken(b_auth.access_token.clone())));
    let b_ready = expect_ready(&mut bob).await;
    assert_eq!(b_ready.user.as_ref().unwrap().id, b_user.id);

    // -- guild via REST; sequenced GuildCreate reaches alice's driver --
    let guild = a_api.create_guild("client e2e hq").await.unwrap();
    assert!(!guild.invite_code.is_empty());
    let general = guild
        .channels
        .iter()
        .find(|c| c.name == "general")
        .expect("auto-created #general")
        .clone();
    let gc = expect_event(&mut alice, "GuildCreate", |e| {
        dispatch_payload(e).and_then(|f| match f.payload {
            Some(Payload::GuildCreate(gc)) => Some((f.seq, gc)),
            _ => None,
        })
    })
    .await;
    assert!(gc.0 > 0, "GuildCreate is sequenced");
    assert_eq!(gc.1.guild.unwrap().id, guild.id);

    // -- bob joins; bob sees GuildCreate, alice sees MemberAdd --
    let joined = b_api.join_guild(&guild.invite_code).await.unwrap();
    assert_eq!(joined.id, guild.id);
    let bob_gc = expect_event(&mut bob, "GuildCreate on joiner", |e| {
        dispatch_payload(e).and_then(|f| match f.payload {
            Some(Payload::GuildCreate(gc)) => Some(gc),
            _ => None,
        })
    })
    .await;
    assert_eq!(bob_gc.guild.unwrap().id, guild.id);
    let member_add = expect_event(&mut alice, "GuildMemberAdd", |e| {
        dispatch_payload(e).and_then(|f| match f.payload {
            Some(Payload::MemberAdd(ma)) => Some(ma),
            _ => None,
        })
    })
    .await;
    assert_eq!(member_add.user.unwrap().id, b_user.id);

    // -- a bad invite is a proto 404 through ApiClient --
    let bad = b_api.join_guild("nosuchcd").await.unwrap_err();
    match bad {
        ApiError::Api { status, error } => {
            assert_eq!(status, 404);
            assert_eq!(error.code, ErrorCode::NotFound as i32);
        }
        other => panic!("expected 404, got {other:?}"),
    }

    // -- send over the socket: Ack and MessageCreate correlate by nonce.
    //    (Ack rides the session transport, the dispatch rides the bus — the
    //    arrival order is not fixed, so scan for both.) --
    let nonce = 0xDEAD_BEEF_u64;
    alice
        .send(Command::SendMessage {
            channel_id: general.id,
            content: "hello bob".into(),
            reply_to_id: 0,
            attachment_ids: Vec::new(),
            nonce,
        })
        .await
        .unwrap();
    let mut ack_message: Option<v1::Message> = None;
    let mut own_dispatch: Option<v1::MessageCreate> = None;
    for _ in 0..64 {
        if ack_message.is_some() && own_dispatch.is_some() {
            break;
        }
        match next_event(&mut alice).await {
            ClientEvent::Ack { nonce: n, message } => {
                assert_eq!(n, nonce, "ack echoes the request nonce");
                ack_message = Some(message);
            }
            ClientEvent::Dispatch(frame) => {
                if let Some(Payload::MessageCreate(mc)) = frame.payload {
                    assert!(frame.seq > 0);
                    own_dispatch = Some(mc);
                }
            }
            _ => {}
        }
    }
    let acked = ack_message.expect("SendMessageAck");
    assert_eq!(acked.content, "hello bob");
    assert_eq!(acked.author_id, a_user.id);
    let own = own_dispatch.expect("author's MessageCreate");
    assert_eq!(own.nonce, nonce);
    assert_eq!(own.message.unwrap().id, acked.id);

    let bob_mc = expect_event(&mut bob, "MessageCreate on recipient", |e| {
        dispatch_payload(e).and_then(|f| match f.payload {
            Some(Payload::MessageCreate(mc)) => Some(mc),
            _ => None,
        })
    })
    .await;
    assert_eq!(bob_mc.nonce, nonce);
    assert_eq!(bob_mc.message.unwrap().content, "hello bob");

    // -- typing: bob types, alice's driver surfaces the ephemeral dispatch --
    bob.send(Command::StartTyping {
        channel_id: general.id,
    })
    .await
    .unwrap();
    let typing = expect_event(&mut alice, "TypingStart", |e| {
        dispatch_payload(e).and_then(|f| match f.payload {
            Some(Payload::TypingStart(t)) => Some((f.seq, t)),
            _ => None,
        })
    })
    .await;
    assert_eq!(typing.0, 0, "typing is class B: never sequenced");
    assert_eq!(typing.1.user_id, b_user.id);
    assert_eq!(typing.1.channel_id, general.id);

    // -- presence: bob goes IDLE; alice sees the sequenced PresenceUpdate --
    bob.send(Command::UpdatePresence {
        status: v1::PresenceStatus::Idle as i32,
    })
    .await
    .unwrap();
    let presence = expect_event(&mut alice, "PresenceUpdate{bob, IDLE}", |e| {
        dispatch_payload(e).and_then(|f| match f.payload {
            Some(Payload::PresenceUpdate(p))
                if p.user_id == b_user.id && p.status == v1::PresenceStatus::Idle as i32 =>
            {
                Some(p)
            }
            _ => None,
        })
    })
    .await;
    assert_eq!(presence.user_id, b_user.id);

    // -- heartbeat: with a 1 s interval, surviving 3.5 s Ready proves the
    //    beat/ack loop in both directions (server kills at 2 s silent,
    //    client reconnects after 2 missing acks ≈ 3 s) --
    tokio::time::sleep(Duration::from_millis(3_500)).await;
    assert!(
        matches!(
            &*alice.state().borrow(),
            ConnState::Ready { gateway_session_id, .. } if *gateway_session_id == a_session
        ),
        "alice must still be Ready on the SAME session after 3.5 s"
    );

    // -- DM: alice opens a DM; bob's driver surfaces DmChannelCreate --
    let dm = a_api.open_dm(b_user.id).await.unwrap();
    assert_eq!(dm.kind, v1::ChannelKind::Dm as i32);
    let bob_dm = expect_event(&mut bob, "DmChannelCreate", |e| {
        dispatch_payload(e).and_then(|f| match f.payload {
            Some(Payload::DmChannelCreate(dc)) => Some(dc),
            _ => None,
        })
    })
    .await;
    assert_eq!(bob_dm.channel.unwrap().id, dm.id);

    // -- request errors correlate by nonce: sending into a bogus channel --
    let bad_nonce = 0xBAD_u64;
    alice
        .send(Command::SendMessage {
            channel_id: 0xFFFF_FFFF_FFFF,
            content: "into the void".into(),
            reply_to_id: 0,
            attachment_ids: Vec::new(),
            nonce: bad_nonce,
        })
        .await
        .unwrap();
    let req_err = expect_event(&mut alice, "RequestError", |e| match e {
        ClientEvent::RequestError { nonce, error } if nonce == bad_nonce => Some(error),
        _ => None,
    })
    .await;
    assert_ne!(req_err.code, 0);

    // -- resume: ForceReconnect drops the socket abruptly; bob talks while
    //    alice is down; the driver resumes and replays the missed dispatch --
    let mut a_state = alice.state();
    alice.send(Command::ForceReconnect).await.unwrap();
    tokio::time::timeout(
        Duration::from_secs(5),
        a_state.wait_for(|s| !matches!(s, ConnState::Ready { .. })),
    )
    .await
    .expect("alice must leave Ready after ForceReconnect")
    .unwrap();

    let nonce2 = 0xFEED_F00D_u64;
    bob.send(Command::SendMessage {
        channel_id: general.id,
        content: "while you were away".into(),
        reply_to_id: 0,
        attachment_ids: Vec::new(),
        nonce: nonce2,
    })
    .await
    .unwrap();
    let _ = expect_event(&mut bob, "bob's own ack", |e| match e {
        ClientEvent::Ack { nonce, message } if nonce == nonce2 => Some(message),
        _ => None,
    })
    .await;

    // Resumed (NOT SessionInvalidated, NOT a fresh Ready), then the replay.
    let mut resumed = false;
    for _ in 0..64 {
        match next_event(&mut alice).await {
            ClientEvent::Resumed { .. } => {
                resumed = true;
                break;
            }
            ClientEvent::SessionInvalidated => panic!("resume degraded to a fresh Identify"),
            ClientEvent::Ready(_) => panic!("resume produced a fresh session"),
            _ => {}
        }
    }
    assert!(resumed, "driver must Resume inside the window");
    let missed = expect_event(&mut alice, "replayed MessageCreate", |e| {
        dispatch_payload(e).and_then(|f| match f.payload {
            Some(Payload::MessageCreate(mc)) if mc.nonce == nonce2 => Some((f.seq, mc)),
            _ => None,
        })
    })
    .await;
    assert!(missed.0 > 0);
    assert_eq!(
        missed.1.message.unwrap().content,
        "while you were away",
        "the dispatch missed while down must arrive after Resumed"
    );
    tokio::time::timeout(
        Duration::from_secs(5),
        a_state.wait_for(
            |s| matches!(s, ConnState::Ready { gateway_session_id, .. } if *gateway_session_id == a_session),
        ),
    )
    .await
    .expect("alice must be Ready again on the SAME gateway session")
    .unwrap();

    // -- REST history through ApiClient, newest first --
    let history = b_api
        .fetch_messages(general.id, None, None, 50)
        .await
        .unwrap();
    let contents: Vec<&str> = history.iter().map(|m| m.content.as_str()).collect();
    assert_eq!(contents, vec!["while you were away", "hello bob"]);

    // -- bearer 401-retry: a stale first token is refreshed through the
    //    provider exactly once and the request succeeds --
    let retry_provider = SeqToken::new(&["stale-garbage", &b_auth.access_token]);
    let retry_api = api
        .clone()
        .with_token_provider(retry_provider.clone() as Arc<dyn TokenProvider>);
    let refetched = retry_api
        .fetch_messages(general.id, None, None, 10)
        .await
        .unwrap();
    assert_eq!(refetched.len(), 2);
    assert_eq!(retry_provider.calls(), 2, "exactly one refresh-and-retry");

    // -- teardown --
    alice.shutdown().await;
    bob.shutdown().await;
    env.ct.cancel();
    tokio::time::timeout(Duration::from_secs(15), env.started.wait())
        .await
        .expect("drain must finish inside the deadline")
        .unwrap();
    cleanup(&env.pool, &[a_user.id, b_user.id], &[dm.id]).await;
    let _ = std::fs::remove_dir_all(&env.cert_dir);
}

/// A resume attempt after the window expired is rejected with
/// `Error{INVALID_SESSION}`; the driver emits `SessionInvalidated` and
/// re-identifies ON THE SAME CONNECTION (protocol §3), landing on a fresh
/// session.
#[tokio::test]
async fn expired_resume_invalidates_and_reidentifies() {
    // Resume window 100 ms << the driver's 500 ms forced-reconnect delay.
    let env = spawn_env("invsess", 30_000, 100).await;
    let api = ApiClient::new(env.base.clone(), &env.tls).unwrap();

    let (name, email) = unique("ci");
    let auth = api
        .register(&email, &name, "correct-horse-battery")
        .await
        .unwrap();
    let user = auth.user.clone().unwrap();

    let mut handle = gw_connect(&env, Arc::new(StaticToken(auth.access_token.clone())));
    let first = expect_ready(&mut handle).await;
    assert!(first.gateway_session_id > 0);

    handle.send(Command::ForceReconnect).await.unwrap();

    let mut invalidated = false;
    let second = loop {
        match next_event(&mut handle).await {
            ClientEvent::SessionInvalidated => invalidated = true,
            ClientEvent::Ready(ready) => break *ready,
            ClientEvent::Resumed { .. } => panic!("expired session must not resume"),
            _ => {}
        }
    };
    assert!(
        invalidated,
        "SessionInvalidated must precede the fresh Ready"
    );
    assert_ne!(
        second.gateway_session_id, first.gateway_session_id,
        "re-identify mints a new gateway session"
    );

    handle.shutdown().await;
    env.ct.cancel();
    tokio::time::timeout(Duration::from_secs(15), env.started.wait())
        .await
        .unwrap()
        .unwrap();
    cleanup(&env.pool, &[user.id], &[]).await;
    let _ = std::fs::remove_dir_all(&env.cert_dir);
}

/// Credential rejection at Identify (close 4001): the driver asks the
/// TokenProvider once more and recovers if the second token is good — or
/// lands in `Failed` if the provider keeps producing garbage.
#[tokio::test]
async fn rejected_token_retries_once_then_fails() {
    let env = spawn_env("badtok", 30_000, 60_000).await;
    let api = ApiClient::new(env.base.clone(), &env.tls).unwrap();

    let (name, email) = unique("ct");
    let auth = api
        .register(&email, &name, "correct-horse-battery")
        .await
        .unwrap();
    let user = auth.user.clone().unwrap();

    // Garbage first, valid second: must end Ready, with the provider asked
    // exactly twice.
    let recovering = SeqToken::new(&["garbage-token", &auth.access_token]);
    let mut handle = gw_connect(&env, recovering.clone() as Arc<dyn TokenProvider>);
    let ready = expect_ready(&mut handle).await;
    assert_eq!(ready.user.unwrap().id, user.id);
    assert_eq!(
        recovering.calls(),
        2,
        "one fresh-token retry after the 4001 close"
    );
    handle.shutdown().await;

    // Garbage forever: Failed (no auto-retry), not an endless loop.
    let hopeless = SeqToken::new(&["still-garbage"]);
    let handle = gw_connect(&env, hopeless.clone() as Arc<dyn TokenProvider>);
    let mut state = handle.state();
    tokio::time::timeout(
        Duration::from_secs(10),
        state.wait_for(|s| matches!(s, ConnState::Failed { .. })),
    )
    .await
    .expect("driver must reach Failed")
    .unwrap();
    assert_eq!(
        hopeless.calls(),
        2,
        "exactly one extra provider round before failing"
    );
    handle.shutdown().await;

    env.ct.cancel();
    tokio::time::timeout(Duration::from_secs(15), env.started.wait())
        .await
        .unwrap()
        .unwrap();
    cleanup(&env.pool, &[user.id], &[]).await;
    let _ = std::fs::remove_dir_all(&env.cert_dir);
}

// ------------------------------------------------------------- QUIC (Ph. 4)

/// QUIC-only happy path: Ready over QUIC with the kind exposed on BOTH the
/// rich state watch and the mirrored `ConnState` event; a sequenced dispatch
/// arrives over the control stream; a `SendMessage` round-trips to its
/// nonce-correlated ack. Dev-CA verification necessarily passed (the
/// transport has no bypass — see `quic_rejects_untrusted_certificates`).
#[tokio::test]
async fn quic_only_happy_path_send_and_ack() {
    let env = spawn_env("quichappy", 30_000, 60_000).await;
    let api = ApiClient::new(env.base.clone(), &env.tls).unwrap();

    let (name, email) = unique("qh");
    let auth = api
        .register(&email, &name, "correct-horse-battery")
        .await
        .unwrap();
    let user = auth.user.clone().unwrap();
    let a_api = api
        .clone()
        .with_token_provider(Arc::new(StaticToken(auth.access_token.clone())));

    let mut handle = gw_connect_with(
        &env,
        Arc::new(StaticToken(auth.access_token.clone())),
        TransportPolicy::QuicOnly,
        Some(quic_endpoint(env.quic)),
        None,
    );
    let ready = expect_ready(&mut handle).await;
    assert_eq!(ready.user.as_ref().unwrap().id, user.id);

    // Kind on the watch…
    assert!(
        matches!(
            &*handle.state().borrow(),
            ConnState::Ready {
                transport: TransportKind::Quic,
                ..
            }
        ),
        "watch must expose the QUIC transport kind"
    );
    // …and on the mirrored ConnState event (what hosts persist/display).
    let kind = expect_event(&mut handle, "Ready ConnState event", |e| match e {
        ClientEvent::ConnState(ConnStateLite::Ready { transport }) => Some(transport),
        _ => None,
    })
    .await;
    assert_eq!(kind, TransportKind::Quic);

    // Server→client sequenced dispatch over QUIC.
    let guild = a_api.create_guild("quic hq").await.unwrap();
    let general = guild
        .channels
        .iter()
        .find(|c| c.name == "general")
        .expect("auto-created #general")
        .clone();
    let gc = expect_event(&mut handle, "GuildCreate over QUIC", |e| {
        dispatch_payload(e).and_then(|f| match f.payload {
            Some(Payload::GuildCreate(gc)) => Some((f.seq, gc)),
            _ => None,
        })
    })
    .await;
    assert!(gc.0 > 0, "GuildCreate is sequenced");
    assert_eq!(gc.1.guild.unwrap().id, guild.id);

    // Client→server request + nonce-correlated ack over QUIC.
    let nonce = 0xC0FFEE_u64;
    handle
        .send(Command::SendMessage {
            channel_id: general.id,
            content: "over quic".into(),
            reply_to_id: 0,
            attachment_ids: Vec::new(),
            nonce,
        })
        .await
        .unwrap();
    let acked = expect_event(&mut handle, "SendMessageAck over QUIC", |e| match e {
        ClientEvent::Ack { nonce: n, message } if n == nonce => Some(message),
        _ => None,
    })
    .await;
    assert_eq!(acked.content, "over quic");
    assert_eq!(acked.author_id, user.id);

    handle.shutdown().await;
    env.ct.cancel();
    tokio::time::timeout(Duration::from_secs(15), env.started.wait())
        .await
        .unwrap()
        .unwrap();
    cleanup(&env.pool, &[user.id], &[]).await;
    let _ = std::fs::remove_dir_all(&env.cert_dir);
}

/// QuicFirst with NO QUIC endpoint configured: straight to WSS (no timeout
/// paid, no failure counted) and Ready on the first attempt.
#[tokio::test]
async fn quic_first_without_endpoint_uses_wss() {
    let env = spawn_env("quicnone", 30_000, 60_000).await;
    let api = ApiClient::new(env.base.clone(), &env.tls).unwrap();

    let (name, email) = unique("qn");
    let auth = api
        .register(&email, &name, "correct-horse-battery")
        .await
        .unwrap();
    let user = auth.user.clone().unwrap();

    let mut handle = gw_connect_with(
        &env,
        Arc::new(StaticToken(auth.access_token.clone())),
        TransportPolicy::default(), // QuicFirst { 3 s }
        None,
        None,
    );
    let _ = expect_ready(&mut handle).await;
    assert!(matches!(
        &*handle.state().borrow(),
        ConnState::Ready {
            transport: TransportKind::Wss,
            ..
        }
    ));

    handle.shutdown().await;
    env.ct.cancel();
    tokio::time::timeout(Duration::from_secs(15), env.started.wait())
        .await
        .unwrap()
        .unwrap();
    cleanup(&env.pool, &[user.id], &[]).await;
    let _ = std::fs::remove_dir_all(&env.cert_dir);
}

/// QuicFirst against an unreachable QUIC port (bound-then-dropped UDP port):
/// the driver falls back to WSS WITHIN the same backoff attempt — no
/// `Backoff` state may precede the first `Ready` — and lands Ready within
/// the ~3 s QUIC budget plus the WSS handshake.
#[tokio::test]
async fn quic_first_unreachable_quic_falls_back_to_wss() {
    let env = spawn_env("quicdead", 30_000, 60_000).await;
    let api = ApiClient::new(env.base.clone(), &env.tls).unwrap();

    // A localhost UDP port that is certainly closed: bind, note, drop.
    let dead_port = {
        let socket = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        socket.local_addr().unwrap().port()
    };
    let dead = quic_endpoint(format!("127.0.0.1:{dead_port}").parse().unwrap());

    let (name, email) = unique("qd");
    let auth = api
        .register(&email, &name, "correct-horse-battery")
        .await
        .unwrap();
    let user = auth.user.clone().unwrap();

    let connect_started = std::time::Instant::now();
    let mut handle = gw_connect_with(
        &env,
        Arc::new(StaticToken(auth.access_token.clone())),
        TransportPolicy::default(), // QuicFirst { 3 s }
        Some(dead),
        None,
    );

    // Same-attempt fallback: Connecting → Authenticating → Ready with NO
    // Backoff in between.
    loop {
        match next_event(&mut handle).await {
            ClientEvent::ConnState(ConnStateLite::Backoff) => {
                panic!("fallback must happen within the SAME attempt, not after a backoff")
            }
            ClientEvent::Ready(_) => break,
            _ => {}
        }
    }
    let elapsed = connect_started.elapsed();
    assert!(
        elapsed < Duration::from_secs(8),
        "fallback should cost ≈ the 3 s QUIC budget, took {elapsed:?}"
    );
    assert!(matches!(
        &*handle.state().borrow(),
        ConnState::Ready {
            transport: TransportKind::Wss,
            ..
        }
    ));

    handle.shutdown().await;
    env.ct.cancel();
    tokio::time::timeout(Duration::from_secs(15), env.started.wait())
        .await
        .unwrap()
        .unwrap();
    cleanup(&env.pool, &[user.id], &[]).await;
    let _ = std::fs::remove_dir_all(&env.cert_dir);
}

/// Resume over QUIC (same shape as the WSS resume in `gateway_journey`):
/// ForceReconnect drops the transport abruptly (the drop closes 4011 so the
/// gateway detaches instead of reading a clean goodbye), a sequenced
/// dispatch lands while down, and the driver Resumes — NOT re-identifies —
/// on the SAME gateway session, over QUIC again, replaying the miss.
#[tokio::test]
async fn quic_resume_after_force_reconnect() {
    let env = spawn_env("quicresume", 30_000, 60_000).await;
    let api = ApiClient::new(env.base.clone(), &env.tls).unwrap();

    let (name, email) = unique("qr");
    let auth = api
        .register(&email, &name, "correct-horse-battery")
        .await
        .unwrap();
    let user = auth.user.clone().unwrap();
    let a_api = api
        .clone()
        .with_token_provider(Arc::new(StaticToken(auth.access_token.clone())));

    let mut handle = gw_connect_with(
        &env,
        Arc::new(StaticToken(auth.access_token.clone())),
        TransportPolicy::QuicOnly,
        Some(quic_endpoint(env.quic)),
        None,
    );
    let ready = expect_ready(&mut handle).await;
    let session = ready.gateway_session_id;
    assert!(session > 0);

    let mut state = handle.state();
    handle.send(Command::ForceReconnect).await.unwrap();
    tokio::time::timeout(
        Duration::from_secs(5),
        state.wait_for(|s| !matches!(s, ConnState::Ready { .. })),
    )
    .await
    .expect("must leave Ready after ForceReconnect")
    .unwrap();

    // While down: a sequenced dispatch (GuildCreate via REST) enters the
    // detached session's replay buffer.
    let guild = a_api.create_guild("made while away").await.unwrap();

    // Resumed — NOT SessionInvalidated, NOT a fresh Ready.
    let mut resumed = false;
    for _ in 0..64 {
        match next_event(&mut handle).await {
            ClientEvent::Resumed { .. } => {
                resumed = true;
                break;
            }
            ClientEvent::SessionInvalidated => {
                panic!("QUIC resume degraded to a fresh Identify (clean-close mapping broken?)")
            }
            ClientEvent::Ready(_) => panic!("resume produced a fresh session"),
            _ => {}
        }
    }
    assert!(resumed, "driver must Resume inside the window over QUIC");

    // The dispatch missed while down arrives after Resumed.
    let missed = expect_event(&mut handle, "replayed GuildCreate", |e| {
        dispatch_payload(e).and_then(|f| match f.payload {
            Some(Payload::GuildCreate(gc)) => Some((f.seq, gc)),
            _ => None,
        })
    })
    .await;
    assert!(missed.0 > 0);
    assert_eq!(missed.1.guild.unwrap().id, guild.id);

    // Same session, still QUIC.
    tokio::time::timeout(
        Duration::from_secs(5),
        state.wait_for(|s| {
            matches!(
                s,
                ConnState::Ready { gateway_session_id, transport: TransportKind::Quic }
                    if *gateway_session_id == session
            )
        }),
    )
    .await
    .expect("must be Ready again on the SAME gateway session over QUIC")
    .unwrap();

    handle.shutdown().await;
    env.ct.cancel();
    tokio::time::timeout(Duration::from_secs(15), env.started.wait())
        .await
        .unwrap()
        .unwrap();
    cleanup(&env.pool, &[user.id], &[]).await;
    let _ = std::fs::remove_dir_all(&env.cert_dir);
}

/// Certificate verification is ENFORCED on QUIC: without the dev-CA anchor
/// the handshake must fail (webpki roots cannot vouch for the dev leaf);
/// with the anchor the same dial succeeds. There is no bypass to misuse.
#[tokio::test]
async fn quic_rejects_untrusted_certificates() {
    let env = spawn_env("quictrust", 30_000, 60_000).await;
    let target = quic_endpoint(env.quic);

    // webpki roots only — the dev CA is NOT trusted.
    let untrusted = TlsOptions::default().quic_client_config().unwrap();
    let denied = QuicTransport::connect(&target, untrusted).await;
    let error = match denied {
        Err(error) => error.to_string().to_lowercase(),
        Ok(_) => panic!("QUIC connect without the dev CA anchor MUST fail verification"),
    };
    assert!(
        error.contains("certificate") || error.contains("unknown issuer"),
        "failure must be a certificate rejection, got: {error}"
    );

    // Control: the identical dial WITH the dev CA anchor succeeds, proving
    // the failure above was trust, not infrastructure.
    let trusted = env.tls.quic_client_config().unwrap();
    let mut ok = QuicTransport::connect(&target, trusted)
        .await
        .expect("dev-CA-anchored QUIC connect must succeed");
    ok.close(1000, "test done").await;

    env.ct.cancel();
    tokio::time::timeout(Duration::from_secs(15), env.started.wait())
        .await
        .unwrap()
        .unwrap();
    let _ = std::fs::remove_dir_all(&env.cert_dir);
}
