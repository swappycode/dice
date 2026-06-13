//! The Phase-2 acceptance gate: the full two-user demo journey over WSS
//! against LIVE Postgres with dev-lite monolith semantics (Local bus +
//! Memory cache), built in-process exactly the way `src/main.rs` wires them
//! (same constructors, same `GatewayDeps`) — the binary itself is not
//! spawned so the test can use port 0 + a temp cert dir.
//!
//! Journey: register alice+bob → Identify both → alice creates "dice hq" →
//! bob joins by invite code → guild message with nonce-ack reconcile →
//! typing → DM open + DM message → REST history → cross-user presence →
//! abrupt drop + Resume with replay.
//!
//! Robustness: 30 s overall deadline, unique per-process identities, frame
//! helpers skip interleaved presence noise, rows cleaned up afterwards.

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
use dice_event_bus::BusConfig;
use dice_network_core::tls::{generate_dev_certs, install_ring_provider, load_certs};
use dice_network_core::{rustls, tokio_rustls};
use dice_protocol::framing::{decode_frame_bare, encode_frame_bare};
use dice_protocol::v1::frame::Payload;
use dice_protocol::v1::{self, ErrorCode, Frame};
use futures_util::{SinkExt, StreamExt};
use presence_service::PresenceService;
use prost::Message;
use rustls::pki_types::ServerName;
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::protocol::Message as WsMessage;

/// Per-frame receive budget (the whole journey is separately capped at 30 s).
const RECV_TIMEOUT: Duration = Duration::from_secs(10);
const PROTOBUF: &str = "application/x-protobuf";

// ---------------------------------------------------------------- identity

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// Unique, concurrent-run-safe (username, email). Usernames stay within the
/// 32-char auth-service cap: tag(<=2) + pid + 'x' + n + 'x' + millis ≈ 25.
fn unique(tag: &str) -> (String, String) {
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis();
    let username = format!("{tag}{}x{n}x{stamp}", std::process::id());
    let email = format!("{username}@wssdemo.dice.test");
    (username, email)
}

fn node_id() -> u16 {
    let n = COUNTER.fetch_add(1, Ordering::Relaxed) as u16;
    ((std::process::id() as u16).wrapping_mul(37).wrapping_add(n)) & 0x3FF
}

fn database_url() -> String {
    if let Ok(url) = std::env::var("DATABASE_URL") {
        return url;
    }
    // sqlx macros compile against the workspace .env; reuse it at runtime.
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

// -------------------------------------------------------------- environment

struct Env {
    started: Started,
    ct: CancellationToken,
    pool: PgPool,
    http: reqwest::Client,
    base: String,
    ca_pem: PathBuf,
    cert_dir: PathBuf,
}

/// dev-lite semantics, in-process: Local{4096} bus + Memory cache + live
/// Postgres + ephemeral JWT keys + temp-dir dev certs; gateway on port 0.
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
        auth: Arc::new(AuthService::new(
            pool.clone(),
            cache.clone(),
            jwt.clone(),
            ids.clone(),
            bus.clone(),
        )),
        chat: Arc::new(ChatService::new(pool.clone(), bus.clone(), ids.clone())),
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
        "dice-monolith-demo-{tag}-{}-{}",
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
            heartbeat_interval_ms: dice_protocol::HEARTBEAT_INTERVAL_MS,
            resume_window_ms: dice_protocol::RESUME_WINDOW_MS,
        },
        deps,
        ct.clone(),
    )
    .await
    .unwrap();

    let ca_bytes = std::fs::read(&certs.ca_pem).unwrap();
    let http = reqwest::Client::builder()
        .add_root_certificate(reqwest::Certificate::from_pem(&ca_bytes).unwrap())
        .resolve("localhost", started.bound_rest)
        .build()
        .unwrap();
    let base = format!("https://localhost:{}", started.bound_rest.port());

    Env {
        started,
        ct,
        pool,
        http,
        base,
        ca_pem: certs.ca_pem,
        cert_dir,
    }
}

fn client_tls(ca_pem: &Path) -> Arc<rustls::ClientConfig> {
    let mut roots = rustls::RootCertStore::empty();
    for cert in load_certs(ca_pem).unwrap() {
        roots.add(cert).unwrap();
    }
    Arc::new(
        rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_protocol_versions(&[&rustls::version::TLS13])
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth(),
    )
}

// ------------------------------------------------------------ REST helpers

async fn post_proto(
    env: &Env,
    path: &str,
    bearer: Option<&str>,
    body: Vec<u8>,
) -> reqwest::Response {
    let mut req = env
        .http
        .post(format!("{}{}", env.base, path))
        .header(reqwest::header::CONTENT_TYPE, PROTOBUF)
        .body(body);
    if let Some(token) = bearer {
        req = req.bearer_auth(token);
    }
    req.send().await.unwrap()
}

async fn register(env: &Env, tag: &str) -> v1::AuthSuccess {
    let (username, email) = unique(tag);
    let body = v1::RegisterRequest {
        email,
        username,
        password: "correct-horse-battery".into(),
    }
    .encode_to_vec();
    let resp = post_proto(env, "/v1/auth/register", None, body).await;
    assert_eq!(resp.status(), 200, "register: {:?}", resp.text().await);
    v1::AuthSuccess::decode(resp.bytes().await.unwrap()).unwrap()
}

// ---------------------------------------------------------------- WS client

struct GwClient {
    stream: WebSocketStream<tokio_rustls::client::TlsStream<TcpStream>>,
    /// Highest dispatch seq actually received (cumulative-ack semantics).
    last_seq: u64,
}

impl GwClient {
    async fn connect(env: &Env) -> Self {
        let tcp = TcpStream::connect(env.started.bound_rest).await.unwrap();
        let connector = TlsConnector::from(client_tls(&env.ca_pem));
        let tls = connector
            .connect(ServerName::try_from("localhost").unwrap(), tcp)
            .await
            .unwrap();
        let url = format!(
            "wss://localhost:{}/gateway/v1",
            env.started.bound_rest.port()
        );
        let (stream, _resp) = tokio_tungstenite::client_async(url, tls).await.unwrap();
        Self {
            stream,
            last_seq: 0,
        }
    }

    async fn send(&mut self, frame: &Frame) {
        let bytes = encode_frame_bare(frame).unwrap();
        self.stream.send(WsMessage::Binary(bytes)).await.unwrap();
    }

    async fn recv(&mut self) -> Frame {
        loop {
            let msg = tokio::time::timeout(RECV_TIMEOUT, self.stream.next())
                .await
                .expect("timed out waiting for a frame")
                .expect("websocket stream ended")
                .unwrap();
            match msg {
                WsMessage::Binary(bytes) => {
                    let frame = decode_frame_bare(&bytes).unwrap();
                    self.last_seq = self.last_seq.max(frame.seq);
                    return frame;
                }
                WsMessage::Ping(_) | WsMessage::Pong(_) => continue,
                other => panic!("unexpected WS message: {other:?}"),
            }
        }
    }

    /// Next frame matching `pick`, skipping anything else (presence noise,
    /// stray typing, ...). Bounded so a wedged stream fails loudly.
    async fn expect<T>(&mut self, what: &str, pick: impl Fn(Frame) -> Option<T>) -> T {
        for _ in 0..64 {
            let frame = self.recv().await;
            if let Some(found) = pick(frame) {
                return found;
            }
        }
        panic!("never received: {what}");
    }

    async fn expect_hello(&mut self) -> v1::Hello {
        self.expect("Hello", |f| match f.payload {
            Some(Payload::Hello(h)) => Some(h),
            _ => None,
        })
        .await
    }

    async fn identify(&mut self, access_token: &str) -> v1::Ready {
        self.send(&Frame::control(Payload::Identify(v1::Identify {
            access_token: access_token.to_owned(),
            properties: None,
            capabilities: 0,
            protocol_version: 1,
        })))
        .await;
        self.expect("Ready", |f| match f.payload {
            Some(Payload::Ready(r)) => Some(r),
            _ => None,
        })
        .await
    }

    /// Receive until nothing arrives for `idle` — settles `last_seq` before a
    /// deliberate disconnect.
    async fn drain(&mut self, idle: Duration) {
        loop {
            match tokio::time::timeout(idle, self.stream.next()).await {
                Ok(Some(Ok(WsMessage::Binary(bytes)))) => {
                    let frame = decode_frame_bare(&bytes).unwrap();
                    self.last_seq = self.last_seq.max(frame.seq);
                }
                Ok(Some(Ok(_))) => continue,
                Ok(_) => panic!("stream ended during drain"),
                Err(_) => return,
            }
        }
    }
}

fn pick_message_create(f: Frame) -> Option<(u64, v1::MessageCreate)> {
    match f.payload {
        Some(Payload::MessageCreate(mc)) => Some((f.seq, mc)),
        _ => None,
    }
}

// --------------------------------------------------------------------- test

#[tokio::test]
async fn wss_demo_phase2_gate() {
    // One hard deadline over the whole journey (assignment: 30 s). Box::pin —
    // the journey future is large (grew with the M2 Message fields).
    tokio::time::timeout(Duration::from_secs(30), Box::pin(journey()))
        .await
        .expect("the demo journey must finish within 30 s");
}

async fn journey() {
    let env = spawn_env("gate").await;

    // ---- 1. register alice + bob over REST -> AuthSuccess each ----
    let alice_auth = register(&env, "al").await;
    let bob_auth = register(&env, "bo").await;
    let alice_user = alice_auth.user.clone().unwrap();
    let bob_user = bob_auth.user.clone().unwrap();
    assert!(!alice_auth.access_token.is_empty());
    assert!(bob_auth.refresh_token.starts_with("drt_"));

    // ---- 2. Identify both on the socket -> Ready ----
    let mut alice = GwClient::connect(&env).await;
    let hello = alice.expect_hello().await;
    assert_eq!(hello.max_frame_bytes, dice_protocol::MAX_FRAME_BYTES as u32);
    let alice_ready = alice.identify(&alice_auth.access_token).await;
    assert_eq!(alice_ready.user.as_ref().unwrap().id, alice_user.id);
    assert!(alice_ready.guilds.is_empty(), "alice has no guilds yet");
    assert_eq!(alice_ready.resume_token.len(), 32);

    let mut bob = GwClient::connect(&env).await;
    bob.expect_hello().await;
    let bob_ready = bob.identify(&bob_auth.access_token).await;
    assert!(bob_ready.guilds.is_empty(), "bob has no guilds yet");

    // ---- 3. alice creates "dice hq" via REST ----
    let resp = post_proto(
        &env,
        "/v1/guilds",
        Some(&alice_auth.access_token),
        v1::CreateGuildRequest {
            name: "dice hq".into(),
        }
        .encode_to_vec(),
    )
    .await;
    assert_eq!(resp.status(), 200);
    let guild = v1::Guild::decode(resp.bytes().await.unwrap()).unwrap();
    assert!(!guild.invite_code.is_empty(), "owner sees the invite code");
    let general = guild
        .channels
        .iter()
        .find(|c| c.name == "general")
        .expect("auto-created #general")
        .clone();

    // ... and alice's socket receives the sequenced GuildCreate.
    let (gc_seq, alice_gc) = alice
        .expect("GuildCreate on alice", |f| match f.payload {
            Some(Payload::GuildCreate(gc)) => Some((f.seq, gc)),
            _ => None,
        })
        .await;
    assert!(gc_seq > 0, "GuildCreate is class A (sequenced)");
    assert_eq!(alice_gc.guild.unwrap().id, guild.id);

    // ---- 4. bob joins via invite code ----
    let resp = post_proto(
        &env,
        "/v1/guilds/join",
        Some(&bob_auth.access_token),
        v1::JoinGuildRequest {
            code: guild.invite_code.clone(),
        }
        .encode_to_vec(),
    )
    .await;
    assert_eq!(resp.status(), 200);
    let joined = v1::Guild::decode(resp.bytes().await.unwrap()).unwrap();
    assert_eq!(joined.id, guild.id);

    // bob's socket: GuildCreate on his user subject (mid-session join).
    let bob_gc = bob
        .expect("GuildCreate on bob", |f| match f.payload {
            Some(Payload::GuildCreate(gc)) => Some(gc),
            _ => None,
        })
        .await;
    assert_eq!(bob_gc.guild.unwrap().id, guild.id);

    // alice's socket: GuildMemberAdd AND — step 9 — bob's ONLINE
    // PresenceUpdate (the router's add_interest on bob's GuildCreate
    // broadcasts his current dot to the guild he just joined). The two ride
    // different subjects, so the arrival order is not fixed: scan for both.
    let mut saw_member_add = false;
    let mut saw_bob_online = false;
    for _ in 0..64 {
        if saw_member_add && saw_bob_online {
            break;
        }
        let frame = alice.recv().await;
        match frame.payload {
            Some(Payload::MemberAdd(ma)) => {
                assert_eq!(ma.user.as_ref().unwrap().id, bob_user.id);
                assert_eq!(ma.member.as_ref().unwrap().guild_id, guild.id);
                saw_member_add = true;
            }
            Some(Payload::PresenceUpdate(p))
                if p.user_id == bob_user.id && p.status == v1::PresenceStatus::Online as i32 =>
            {
                assert!(frame.seq > 0, "PresenceUpdate is class A (sequenced)");
                saw_bob_online = true;
            }
            _ => {} // tolerate any other interleaved frames
        }
    }
    assert!(saw_member_add, "alice must see GuildMemberAdd{{bob}}");
    assert!(
        saw_bob_online,
        "presence dots work cross-user: alice must see bob ONLINE after the join"
    );

    // ---- 5. alice sends "hello bob" (nonce 42) over the socket ----
    let nonce = 42_u64;
    alice
        .send(&Frame::with_nonce(
            nonce,
            Payload::SendMessage(v1::SendMessageRequest {
                channel_id: general.id,
                content: "hello bob".into(),
                reply_to_id: 0,
            }),
        ))
        .await;
    let (ack_nonce, ack) = alice
        .expect("send_message_ack", |f| match f.payload {
            Some(Payload::SendMessageAck(a)) => Some((f.nonce, a)),
            _ => None,
        })
        .await;
    assert_eq!(ack_nonce, nonce, "ack echoes the request nonce");
    let acked = ack.message.unwrap();
    assert_eq!(acked.content, "hello bob");
    assert_eq!(acked.author_id, alice_user.id);

    let (a_seq, a_mc) = alice
        .expect("MessageCreate on alice", pick_message_create)
        .await;
    assert!(a_seq > 0);
    assert_eq!(a_mc.nonce, nonce, "author's dispatch carries the nonce");
    assert_eq!(a_mc.message.as_ref().unwrap().id, acked.id);

    let (b_seq, b_mc) = bob
        .expect("MessageCreate on bob", pick_message_create)
        .await;
    assert!(b_seq >= 1);
    assert_eq!(b_mc.nonce, nonce, "nonce is visible to everyone — fine");
    assert_eq!(b_mc.message.unwrap().content, "hello bob");

    // ---- 6. bob types in #general -> alice sees TypingStart, seq=0 ----
    bob.send(&Frame::control(Payload::StartTyping(
        v1::StartTypingRequest {
            channel_id: general.id,
        },
    )))
    .await;
    let (t_seq, typing) = alice
        .expect("TypingStart", |f| match f.payload {
            Some(Payload::TypingStart(t)) => Some((f.seq, t)),
            _ => None,
        })
        .await;
    assert_eq!(t_seq, 0, "typing is class B: never sequenced");
    assert_eq!(typing.user_id, bob_user.id);
    assert_eq!(typing.channel_id, general.id);

    // ---- 7. bob opens a DM with alice; both sockets learn about it ----
    let resp = post_proto(
        &env,
        "/v1/dms",
        Some(&bob_auth.access_token),
        v1::OpenDmRequest {
            recipient_id: alice_user.id,
        }
        .encode_to_vec(),
    )
    .await;
    assert_eq!(resp.status(), 200);
    let dm = v1::Channel::decode(resp.bytes().await.unwrap()).unwrap();
    assert_eq!(dm.kind, v1::ChannelKind::Dm as i32);

    let bob_dm = bob
        .expect("DmChannelCreate on bob", |f| match f.payload {
            Some(Payload::DmChannelCreate(dc)) => Some(dc),
            _ => None,
        })
        .await;
    assert_eq!(bob_dm.channel.unwrap().id, dm.id);
    let alice_dm = alice
        .expect("DmChannelCreate on alice", |f| match f.payload {
            Some(Payload::DmChannelCreate(dc)) => Some(dc),
            _ => None,
        })
        .await;
    assert_eq!(alice_dm.channel.unwrap().id, dm.id);

    // bob sends a DM message; alice receives it.
    bob.send(&Frame::with_nonce(
        43,
        Payload::SendMessage(v1::SendMessageRequest {
            channel_id: dm.id,
            content: "psst, alice".into(),
            reply_to_id: 0,
        }),
    ))
    .await;
    let _ = bob
        .expect("bob's DM ack", |f| match f.payload {
            Some(Payload::SendMessageAck(a)) => Some(a),
            _ => None,
        })
        .await;
    let (_, dm_mc) = alice
        .expect("DM MessageCreate on alice", |f| match f.payload {
            Some(Payload::MessageCreate(mc))
                if mc.message.as_ref().is_some_and(|m| m.channel_id == dm.id) =>
            {
                Some((f.seq, mc))
            }
            _ => None,
        })
        .await;
    let dm_msg = dm_mc.message.unwrap();
    assert_eq!(dm_msg.content, "psst, alice");
    assert_eq!(dm_msg.author_id, bob_user.id);

    // ---- 8. REST history on #general (Bearer alice) has the message ----
    let resp = env
        .http
        .get(format!(
            "{}/v1/channels/{}/messages?limit=50",
            env.base, general.id
        ))
        .bearer_auth(&alice_auth.access_token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.headers()[reqwest::header::CONTENT_TYPE]
            .to_str()
            .unwrap(),
        PROTOBUF
    );
    let history = v1::MessageHistory::decode(resp.bytes().await.unwrap()).unwrap();
    assert!(
        history.messages.iter().any(|m| m.content == "hello bob"),
        "history must contain the guild message"
    );

    // (step 9 — cross-user presence — was asserted right after the join.)

    // ---- 10. resume: abrupt drop, missed message, replay ----
    alice.drain(Duration::from_millis(500)).await; // settle last_seq
    let alice_session = alice_ready.gateway_session_id;
    let alice_token = alice_ready.resume_token.clone();
    let alice_last_seq = alice.last_seq;
    drop(alice); // kill the TCP without a Close frame

    tokio::time::sleep(Duration::from_millis(300)).await; // server notices
    bob.send(&Frame::with_nonce(
        44,
        Payload::SendMessage(v1::SendMessageRequest {
            channel_id: general.id,
            content: "while you were away".into(),
            reply_to_id: 0,
        }),
    ))
    .await;
    let _ = bob
        .expect("bob's ack for the missed message", |f| match f.payload {
            Some(Payload::SendMessageAck(a)) => Some(a),
            _ => None,
        })
        .await;

    // Reconnect + Resume, retrying while the server detaches the dead socket.
    let mut alice2 = GwClient::connect(&env).await;
    alice2.expect_hello().await;
    alice2.last_seq = alice_last_seq;
    let mut resumed = false;
    for _ in 0..20 {
        alice2
            .send(&Frame::control(Payload::Resume(v1::Resume {
                gateway_session_id: alice_session,
                resume_token: alice_token.clone(),
                last_seq: alice_last_seq,
            })))
            .await;
        match alice2.recv().await.payload {
            Some(Payload::Resumed(_)) => {
                resumed = true;
                break;
            }
            Some(Payload::Error(e)) if e.code == ErrorCode::InvalidSession as i32 => {
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
            other => panic!("unexpected resume reply: {other:?}"),
        }
    }
    assert!(resumed, "Resume must succeed inside the 60 s window");

    // Replay: sequenced frames continue contiguously from last_seq+1 and
    // include the missed MessageCreate.
    let mut expected_seq = alice_last_seq;
    let mut missed: Option<v1::MessageCreate> = None;
    for _ in 0..64 {
        let frame = alice2.recv().await;
        if frame.seq > 0 {
            expected_seq += 1;
            assert_eq!(
                frame.seq, expected_seq,
                "replayed dispatches must be contiguous from last_seq+1"
            );
        }
        if let Some(Payload::MessageCreate(mc)) = frame.payload
            && mc.nonce == 44
        {
            missed = Some(mc);
            break;
        }
    }
    let missed = missed.expect("the missed MessageCreate must be replayed");
    assert_eq!(missed.message.unwrap().content, "while you were away");

    // ---- graceful shutdown: bob gets Close{GOING_AWAY}, drain ≤ 15 s ----
    env.ct.cancel();
    let close = bob
        .expect("Close{GOING_AWAY}", |f| match f.payload {
            Some(Payload::Close(c)) => Some(c),
            _ => None,
        })
        .await;
    assert_eq!(close.code, ErrorCode::GoingAway.close_code());
    tokio::time::timeout(Duration::from_secs(15), env.started.wait())
        .await
        .expect("gateway drain inside the deadline")
        .unwrap();

    // ---- cleanup: best-effort row removal (cascades do the bulk) ----
    let _ = sqlx::query("DELETE FROM channels WHERE id = $1")
        .bind(dm.id as i64)
        .execute(&env.pool)
        .await;
    for owner in [alice_user.id, bob_user.id] {
        let _ = sqlx::query("DELETE FROM guilds WHERE owner_id = $1")
            .bind(owner as i64)
            .execute(&env.pool)
            .await;
    }
    for user in [alice_user.id, bob_user.id] {
        let _ = sqlx::query("DELETE FROM users WHERE id = $1")
            .bind(user as i64)
            .execute(&env.pool)
            .await;
    }
    let _ = std::fs::remove_dir_all(&env.cert_dir);
}
