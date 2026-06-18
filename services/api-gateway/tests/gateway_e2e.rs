//! End-to-end gate rehearsal: real services over live Postgres, the Local
//! bus, the in-memory cache, ephemeral JWT keys and freshly generated dev
//! certs — then the full client journey over WSS (and a QUIC smoke pass):
//! register → Identify → Ready → guild create/join → send/ack/dispatch →
//! typing → DM → presence snapshot → heartbeat → abrupt drop → Resume.
//!
//! Needs live Postgres (DATABASE_URL, or the workspace .env). Robust to
//! concurrent runs: all identities are unique per process+counter and the
//! test deletes its own rows afterwards.

#![allow(clippy::unwrap_used)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use api_gateway::{GatewayConfig, GatewayDeps, Started};
use auth_service::AuthService;
use bytes::BytesMut;
use chat_service::ChatService;
use dice_auth_core::token::JwtKeys;
use dice_cache::{CacheConfig, RateLimiter};
use dice_common::SnowflakeGenerator;
use dice_common::shutdown::CancellationToken;
use dice_database::{DbConfig, PgPool};
use dice_event_bus::BusConfig;
use dice_network_core::tls::{generate_dev_certs, install_ring_provider, load_certs};
use dice_network_core::{quinn, rustls, tokio_rustls};
use dice_protocol::framing::{FrameDecoder, decode_frame_bare, encode_frame, encode_frame_bare};
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

const RECV_TIMEOUT: Duration = Duration::from_secs(10);
const PROTOBUF: &str = "application/x-protobuf";

// ------------------------------------------------------------ environment

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

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// Unique, concurrent-run-safe identity material.
fn unique(tag: &str) -> (String, String) {
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis();
    let pid = std::process::id();
    let username = format!("{tag}{pid}x{n}x{stamp}");
    let email = format!("{username}@e2e.dice.test");
    (username, email)
}

fn node_id() -> u16 {
    let n = COUNTER.fetch_add(1, Ordering::Relaxed) as u16;
    ((std::process::id() as u16).wrapping_mul(31).wrapping_add(n)) & 0x3FF
}

struct Env {
    started: Started,
    ct: CancellationToken,
    pool: PgPool,
    http: reqwest::Client,
    base: String,
    ca_pem: PathBuf,
    cert_dir: PathBuf,
}

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
            Arc::new(media_service::LocalFsStore::new(std::env::temp_dir().join(
                format!(
                    "dice-gw-media-{}-{}",
                    std::process::id(),
                    COUNTER.fetch_add(1, Ordering::Relaxed)
                ),
            ))),
            ids.clone(),
        )),
        presence: Arc::new(PresenceService::new(
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
        "dice-gw-e2e-{tag}-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let certs = generate_dev_certs(&cert_dir).unwrap();

    let cfg = GatewayConfig {
        rest_addr: "127.0.0.1:0".parse().unwrap(),
        quic_addr: "127.0.0.1:0".parse().unwrap(),
        tls_cert: certs.server_cert.clone(),
        tls_key: certs.server_key.clone(),
        heartbeat_interval_ms: 30_000,
        resume_window_ms: 60_000,
        quic: Default::default(),
    };
    let ct = CancellationToken::new();
    let started = api_gateway::start(cfg, deps, ct.clone()).await.unwrap();

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

fn client_tls(ca_pem: &Path, alpn: Option<&[u8]>) -> Arc<rustls::ClientConfig> {
    let mut roots = rustls::RootCertStore::empty();
    for cert in load_certs(ca_pem).unwrap() {
        roots.add(cert).unwrap();
    }
    let mut cfg = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_protocol_versions(&[&rustls::version::TLS13])
    .unwrap()
    .with_root_certificates(roots)
    .with_no_client_auth();
    if let Some(proto) = alpn {
        cfg.alpn_protocols = vec![proto.to_vec()];
    }
    Arc::new(cfg)
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
    assert_eq!(
        resp.status(),
        200,
        "register failed: {:?}",
        resp.text().await
    );
    v1::AuthSuccess::decode(resp.bytes().await.unwrap()).unwrap()
}

// -------------------------------------------------------------- WS client

struct GwClient {
    stream: WebSocketStream<tokio_rustls::client::TlsStream<TcpStream>>,
    /// Highest dispatch seq this client has actually received.
    last_seq: u64,
}

impl GwClient {
    async fn connect(env: &Env) -> Self {
        let tcp = TcpStream::connect(env.started.bound_rest).await.unwrap();
        let connector = TlsConnector::from(client_tls(&env.ca_pem, None));
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

    /// Skip frames until `pick` matches (covers interleaved presence noise).
    async fn expect<T>(&mut self, what: &str, pick: impl Fn(Frame) -> Option<T>) -> T {
        for _ in 0..50 {
            let frame = self.recv().await;
            if let Some(found) = pick(frame) {
                return found;
            }
        }
        panic!("never received: {what}");
    }

    /// Identify and return Ready.
    async fn identify(&mut self, access_token: &str) -> v1::Ready {
        self.send(&Frame::control(Payload::Identify(v1::Identify {
            access_token: access_token.to_owned(),
            properties: None,
            capabilities: 0,
            protocol_version: 1,
        })))
        .await;
        self.expect("Ready", |f| match f.payload {
            Some(Payload::Ready(ready)) => Some(ready),
            _ => None,
        })
        .await
    }

    async fn expect_hello(&mut self) -> v1::Hello {
        self.expect("Hello", |f| match f.payload {
            Some(Payload::Hello(hello)) => Some(hello),
            _ => None,
        })
        .await
    }

    /// Receive frames until nothing arrives for `idle` (settles seq state
    /// before a deliberate disconnect).
    async fn drain(&mut self, idle: Duration) {
        loop {
            match tokio::time::timeout(idle, self.stream.next()).await {
                Ok(Some(Ok(WsMessage::Binary(bytes)))) => {
                    let frame = decode_frame_bare(&bytes).unwrap();
                    self.last_seq = self.last_seq.max(frame.seq);
                }
                Ok(Some(Ok(_))) => continue,
                Ok(_) => panic!("stream ended during drain"),
                Err(_) => return, // idle: drained
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

// ------------------------------------------------------------------ tests

#[tokio::test]
async fn wss_end_to_end_gate_rehearsal() {
    let env = spawn_env("wss").await;

    // -- register two users over REST --
    let a_auth = register(&env, "ga").await;
    let b_auth = register(&env, "gb").await;
    let a_user = a_auth.user.clone().unwrap();
    let b_user = b_auth.user.clone().unwrap();

    // -- REST without a bearer is rejected with a proto Error --
    let resp = env
        .http
        .get(format!("{}/v1/channels/1/messages", env.base))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);
    let err = v1::Error::decode(resp.bytes().await.unwrap()).unwrap();
    assert_eq!(err.code, ErrorCode::Unauthenticated as i32);

    // -- healthz is plain --
    let resp = env
        .http
        .get(format!("{}/healthz", env.base))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.text().await.unwrap(), "ok");

    // -- client A: Hello → Identify → Ready --
    let mut a = GwClient::connect(&env).await;
    let hello = a.expect_hello().await;
    assert_eq!(hello.heartbeat_interval_ms, 30_000);
    assert_eq!(hello.resume_window_ms, 60_000);
    assert_eq!(hello.max_frame_bytes, dice_protocol::MAX_FRAME_BYTES as u32);
    let a_ready = a.identify(&a_auth.access_token).await;
    assert_eq!(a_ready.user.as_ref().unwrap().id, a_user.id);
    assert_eq!(a_ready.resume_token.len(), 32);
    assert!(a_ready.gateway_session_id > 0);
    assert!(a_ready.guilds.is_empty());

    // -- A creates a guild via REST; GuildCreate arrives on A's user subject --
    let resp = post_proto(
        &env,
        "/v1/guilds",
        Some(&a_auth.access_token),
        v1::CreateGuildRequest {
            name: "e2e guild".into(),
        }
        .encode_to_vec(),
    )
    .await;
    assert_eq!(resp.status(), 200);
    let guild = v1::Guild::decode(resp.bytes().await.unwrap()).unwrap();
    assert!(!guild.invite_code.is_empty());
    let general = guild
        .channels
        .iter()
        .find(|c| c.name == "general")
        .expect("auto-created #general")
        .clone();

    let a_guild_create = a
        .expect("GuildCreate", |f| match f.payload {
            Some(Payload::GuildCreate(gc)) => Some((f.seq, gc)),
            _ => None,
        })
        .await;
    assert!(a_guild_create.0 > 0, "GuildCreate is sequenced");
    assert_eq!(a_guild_create.1.guild.unwrap().id, guild.id);

    // -- client B identifies, then joins via invite code --
    let mut b = GwClient::connect(&env).await;
    b.expect_hello().await;
    let b_ready = b.identify(&b_auth.access_token).await;
    assert!(b_ready.guilds.is_empty());
    // B's own snapshot shows B online.
    assert!(
        b_ready
            .presences
            .iter()
            .any(|p| p.user_id == b_user.id && p.status == v1::PresenceStatus::Online as i32)
    );

    let resp = post_proto(
        &env,
        "/v1/guilds/join",
        Some(&b_auth.access_token),
        v1::JoinGuildRequest {
            code: guild.invite_code.clone(),
        }
        .encode_to_vec(),
    )
    .await;
    assert_eq!(resp.status(), 200);
    let joined = v1::Guild::decode(resp.bytes().await.unwrap()).unwrap();
    assert_eq!(joined.id, guild.id);

    // B receives GuildCreate (its user subject), A receives GuildMemberAdd.
    let b_gc = b
        .expect("GuildCreate for joiner", |f| match f.payload {
            Some(Payload::GuildCreate(gc)) => Some(gc),
            _ => None,
        })
        .await;
    assert_eq!(b_gc.guild.unwrap().id, guild.id);
    let member_add = a
        .expect("GuildMemberAdd", |f| match f.payload {
            Some(Payload::MemberAdd(ma)) => Some(ma),
            _ => None,
        })
        .await;
    assert_eq!(member_add.user.unwrap().id, b_user.id);

    // -- a bad invite code is a proto 404 --
    let resp = post_proto(
        &env,
        "/v1/guilds/join",
        Some(&b_auth.access_token),
        v1::JoinGuildRequest {
            code: "nosuchcd".into(),
        }
        .encode_to_vec(),
    )
    .await;
    assert_eq!(resp.status(), 404);
    let err = v1::Error::decode(resp.bytes().await.unwrap()).unwrap();
    assert_eq!(err.code, ErrorCode::NotFound as i32);

    // -- send path: A sends over the socket; ack echoes the nonce; the
    //    MessageCreate dispatch (with nonce) reaches BOTH clients --
    let nonce = 0xDEAD_BEEF_u64;
    a.send(&Frame::with_nonce(
        nonce,
        Payload::SendMessage(v1::SendMessageRequest {
            channel_id: general.id,
            content: "first message".into(),
            reply_to_id: 0,
            attachment_ids: Vec::new(),
        }),
    ))
    .await;
    let ack = a
        .expect("SendMessageAck", |f| match f.payload {
            Some(Payload::SendMessageAck(ack)) => Some((f.nonce, ack)),
            _ => None,
        })
        .await;
    assert_eq!(ack.0, nonce, "ack echoes the request nonce");
    let acked_message = ack.1.message.unwrap();
    assert_eq!(acked_message.content, "first message");
    assert_eq!(acked_message.author_id, a_user.id);

    let (a_seq, a_mc) = a
        .expect("MessageCreate on sender", pick_message_create)
        .await;
    assert!(a_seq > 0);
    assert_eq!(a_mc.nonce, nonce);
    assert_eq!(a_mc.message.as_ref().unwrap().id, acked_message.id);
    let (b_seq, b_mc) = a_b_message(&mut b).await;
    assert!(b_seq > 0);
    assert_eq!(b_mc.nonce, nonce, "nonce rides the dispatch to everyone");
    assert_eq!(b_mc.message.unwrap().content, "first message");

    // -- typing: B types in #general; A sees ephemeral TypingStart, seq=0 --
    b.send(&Frame::control(Payload::StartTyping(
        v1::StartTypingRequest {
            channel_id: general.id,
        },
    )))
    .await;
    let typing = a
        .expect("TypingStart", |f| match f.payload {
            Some(Payload::TypingStart(t)) => Some((f.seq, t)),
            _ => None,
        })
        .await;
    assert_eq!(typing.0, 0, "typing is class B: never sequenced");
    assert_eq!(typing.1.user_id, b_user.id);
    assert_eq!(typing.1.channel_id, general.id);

    // -- DM: A opens a DM with B; B gets DmChannelCreate mid-session and the
    //    DM typing subject works right away (critique #14) --
    let resp = post_proto(
        &env,
        "/v1/dms",
        Some(&a_auth.access_token),
        v1::OpenDmRequest {
            recipient_id: b_user.id,
        }
        .encode_to_vec(),
    )
    .await;
    assert_eq!(resp.status(), 200);
    let dm = v1::Channel::decode(resp.bytes().await.unwrap()).unwrap();
    assert_eq!(dm.kind, v1::ChannelKind::Dm as i32);
    let b_dm = b
        .expect("DmChannelCreate", |f| match f.payload {
            Some(Payload::DmChannelCreate(dc)) => Some(dc),
            _ => None,
        })
        .await;
    assert_eq!(b_dm.channel.unwrap().id, dm.id);
    b.send(&Frame::control(Payload::StartTyping(
        v1::StartTypingRequest { channel_id: dm.id },
    )))
    .await;
    let dm_typing = a
        .expect("DM TypingStart", |f| match f.payload {
            Some(Payload::TypingStart(t)) if t.channel_id == dm.id => Some(t),
            _ => None,
        })
        .await;
    assert_eq!(dm_typing.user_id, b_user.id);

    // -- heartbeat round trip (also acks A's replay buffer) --
    a.send(&Frame::control(Payload::Heartbeat(v1::Heartbeat {
        last_seq: a.last_seq,
        client_time_ms: 777,
    })))
    .await;
    let hb_ack = a
        .expect("HeartbeatAck", |f| match f.payload {
            Some(Payload::HeartbeatAck(ack)) => Some(ack),
            _ => None,
        })
        .await;
    assert_eq!(hb_ack.client_time_ms, 777);

    // -- presence snapshot: a fresh session for B sees A ONLINE --
    let mut b2 = GwClient::connect(&env).await;
    b2.expect_hello().await;
    let b2_ready = b2.identify(&b_auth.access_token).await;
    assert_eq!(b2_ready.guilds.len(), 1, "B is a member now");
    assert!(
        b2_ready
            .presences
            .iter()
            .any(|p| p.user_id == a_user.id && p.status == v1::PresenceStatus::Online as i32),
        "A must be ONLINE in B's Ready snapshot"
    );
    assert!(
        b2_ready.users.iter().any(|u| u.id == a_user.id),
        "user dictionary covers fellow members"
    );
    drop(b2); // abrupt; its detached session dies with the gateway

    // -- invalid resume: unknown session ⇒ Error{INVALID_SESSION}, the
    //    connection stays open and a fresh Identify still works --
    let mut x = GwClient::connect(&env).await;
    x.expect_hello().await;
    x.send(&Frame::control(Payload::Resume(v1::Resume {
        gateway_session_id: 12345,
        resume_token: bytes::Bytes::from_static(&[9u8; 32]),
        last_seq: 0,
    })))
    .await;
    let invalid = x
        .expect("Error{INVALID_SESSION}", |f| match f.payload {
            Some(Payload::Error(e)) => Some(e),
            _ => None,
        })
        .await;
    assert_eq!(invalid.code, ErrorCode::InvalidSession as i32);
    let x_ready = x.identify(&b_auth.access_token).await;
    assert!(x_ready.gateway_session_id > 0);
    drop(x);

    // -- resume: drop A's socket abruptly, send a message from B while A is
    //    detached, reconnect and Resume --
    a.drain(Duration::from_millis(500)).await;
    let a_session_id = a_ready.gateway_session_id;
    let a_resume_token = a_ready.resume_token.clone();
    let a_last_seq = a.last_seq;
    drop(a); // no close frame: abrupt

    tokio::time::sleep(Duration::from_millis(300)).await; // let the server notice
    let nonce2 = 0xFEED_F00D_u64;
    b.send(&Frame::with_nonce(
        nonce2,
        Payload::SendMessage(v1::SendMessageRequest {
            channel_id: general.id,
            content: "while you were away".into(),
            reply_to_id: 0,
            attachment_ids: Vec::new(),
        }),
    ))
    .await;
    let _ = b
        .expect("B's own ack", |f| match f.payload {
            Some(Payload::SendMessageAck(ack)) => Some(ack),
            _ => None,
        })
        .await;

    // Reconnect + Resume (retrying while the server notices the dead socket).
    let mut a2 = GwClient::connect(&env).await;
    a2.expect_hello().await;
    a2.last_seq = a_last_seq;
    let mut resumed = false;
    for _ in 0..15 {
        a2.send(&Frame::control(Payload::Resume(v1::Resume {
            gateway_session_id: a_session_id,
            resume_token: a_resume_token.clone(),
            last_seq: a_last_seq,
        })))
        .await;
        let frame = a2.recv().await;
        match frame.payload {
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
    assert!(resumed, "Resume must succeed within the window");

    // The replay must deliver B's message with a fresh seq > last_seq.
    let (replay_seq, replayed) = a2
        .expect("replayed MessageCreate", pick_message_create)
        .await;
    assert!(replay_seq > a_last_seq);
    assert_eq!(replayed.nonce, nonce2);
    assert_eq!(replayed.message.unwrap().content, "while you were away");
    // And the resumed session is fully live: heartbeats still work.
    a2.send(&Frame::control(Payload::Heartbeat(v1::Heartbeat {
        last_seq: a2.last_seq,
        client_time_ms: 42,
    })))
    .await;
    let ack = a2
        .expect("post-resume HeartbeatAck", |f| match f.payload {
            Some(Payload::HeartbeatAck(ack)) => Some(ack),
            _ => None,
        })
        .await;
    assert_eq!(ack.client_time_ms, 42);

    // -- REST history: both messages, newest first --
    let resp = env
        .http
        .get(format!(
            "{}/v1/channels/{}/messages?limit=50",
            env.base, general.id
        ))
        .bearer_auth(&b_auth.access_token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.headers()
            .get(reqwest::header::CONTENT_TYPE)
            .unwrap()
            .to_str()
            .unwrap(),
        PROTOBUF
    );
    let history = v1::MessageHistory::decode(resp.bytes().await.unwrap()).unwrap();
    let contents: Vec<&str> = history
        .messages
        .iter()
        .map(|m| m.content.as_str())
        .collect();
    assert_eq!(contents, vec!["while you were away", "first message"]);

    // -- oversized REST body ⇒ proto 413. The server rejects on the
    //    announced content-length BEFORE the body arrives, so this uses a
    //    raw HTTP/1.1 exchange (reqwest would race its own body upload
    //    against the early response and surface a write error).
    let err = oversized_body_response(&env, &a_auth.access_token).await;
    assert_eq!(err.code, ErrorCode::PayloadTooLarge as i32);

    // -- graceful shutdown: live sessions get Close{GOING_AWAY} (4011) --
    env.ct.cancel();
    let close = b
        .expect("Close{GOING_AWAY}", |f| match f.payload {
            Some(Payload::Close(c)) => Some(c),
            _ => None,
        })
        .await;
    assert_eq!(close.code, ErrorCode::GoingAway.close_code());
    tokio::time::timeout(Duration::from_secs(15), env.started.wait())
        .await
        .expect("drain must finish inside the deadline")
        .unwrap();

    // -- cleanup --
    cleanup(&env.pool, &[a_user.id, b_user.id], &[dm.id]).await;
    let _ = std::fs::remove_dir_all(&env.cert_dir);
}

async fn a_b_message(b: &mut GwClient) -> (u64, v1::MessageCreate) {
    b.expect("MessageCreate on recipient", pick_message_create)
        .await
}

/// POST with `content-length: 2 MiB` and read the early 413 without sending
/// the body. Returns the decoded `dice.v1.Error`.
async fn oversized_body_response(env: &Env, bearer: &str) -> v1::Error {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let tcp = TcpStream::connect(env.started.bound_rest).await.unwrap();
    let connector = TlsConnector::from(client_tls(&env.ca_pem, None));
    let mut tls = connector
        .connect(ServerName::try_from("localhost").unwrap(), tcp)
        .await
        .unwrap();
    let head = format!(
        "POST /v1/guilds HTTP/1.1\r\nhost: localhost\r\nauthorization: Bearer {bearer}\r\n\
         content-type: {PROTOBUF}\r\ncontent-length: {}\r\n\r\n",
        2 * 1024 * 1024
    );
    tls.write_all(head.as_bytes()).await.unwrap();

    let mut raw = Vec::new();
    let mut chunk = [0u8; 4096];
    let response = loop {
        let n = tokio::time::timeout(RECV_TIMEOUT, tls.read(&mut chunk))
            .await
            .expect("timed out waiting for the 413")
            .unwrap();
        if n == 0 {
            break raw;
        }
        raw.extend_from_slice(&chunk[..n]);
        // Headers + body both present? (proto Error bodies are tiny.)
        if let Some(header_end) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
            let headers = String::from_utf8_lossy(&raw[..header_end]).to_ascii_lowercase();
            let body_len: usize = headers
                .lines()
                .find_map(|l| l.strip_prefix("content-length:"))
                .map(|v| v.trim().parse().unwrap())
                .unwrap_or(0);
            if raw.len() >= header_end + 4 + body_len {
                break raw;
            }
        }
    };
    let text = String::from_utf8_lossy(&response);
    assert!(
        text.starts_with("HTTP/1.1 413"),
        "expected 413, got: {text}"
    );
    let header_end = response
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("complete response head")
        + 4;
    v1::Error::decode(&response[header_end..]).unwrap()
}

#[tokio::test]
async fn quic_handshake_identify_ready() {
    let env = spawn_env("quic").await;
    let auth = register(&env, "gq").await;
    let user = auth.user.clone().unwrap();

    let tls = client_tls(&env.ca_pem, Some(dice_protocol::ALPN_GATEWAY));
    let crypto = quinn::crypto::rustls::QuicClientConfig::try_from(tls).unwrap();
    let client_cfg = quinn::ClientConfig::new(Arc::new(crypto));
    let endpoint = quinn::Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap();
    let conn = endpoint
        .connect_with(client_cfg, env.started.bound_quic, "localhost")
        .unwrap()
        .await
        .expect("QUIC connect (ALPN dice/1, dev CA)");

    // The client opens the single bidi control stream; quinn only surfaces it
    // to the server once data flows, so Identify goes out immediately and
    // Hello arrives back concurrently (protocol §1/§3).
    let (mut send, mut recv) = conn.open_bi().await.unwrap();
    let identify = Frame::control(Payload::Identify(v1::Identify {
        access_token: auth.access_token.clone(),
        properties: None,
        capabilities: 0,
        protocol_version: 1,
    }));
    let mut buf = BytesMut::new();
    encode_frame(&identify, &mut buf).unwrap();
    send.write_all(&buf).await.unwrap();

    let mut decoder = FrameDecoder::new();
    let mut frames: Vec<Frame> = Vec::new();
    let mut scratch = [0u8; 16 * 1024];
    while frames.len() < 2 {
        if let Some(frame) = decoder.try_next().unwrap() {
            frames.push(frame);
            continue;
        }
        let n = tokio::time::timeout(RECV_TIMEOUT, recv.read(&mut scratch))
            .await
            .expect("timed out reading QUIC control stream")
            .unwrap()
            .expect("control stream closed early");
        decoder.extend(&scratch[..n]).unwrap();
    }

    let Some(Payload::Hello(hello)) = &frames[0].payload else {
        panic!("first frame must be Hello, got {:?}", frames[0]);
    };
    assert_eq!(hello.max_frame_bytes, dice_protocol::MAX_FRAME_BYTES as u32);
    let Some(Payload::Ready(ready)) = &frames[1].payload else {
        panic!("second frame must be Ready, got {:?}", frames[1]);
    };
    assert_eq!(ready.user.as_ref().unwrap().id, user.id);
    assert_eq!(ready.resume_token.len(), 32);

    conn.close(0u32.into(), b"test done");
    endpoint.wait_idle().await;

    env.ct.cancel();
    tokio::time::timeout(Duration::from_secs(15), env.started.wait())
        .await
        .expect("drain must finish inside the deadline")
        .unwrap();

    cleanup(&env.pool, &[user.id], &[]).await;
    let _ = std::fs::remove_dir_all(&env.cert_dir);
}

/// Best-effort row cleanup (guilds cascade channels/messages/members; users
/// cascade auth sessions). DM channels have no guild, so they go explicitly.
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
