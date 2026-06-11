> Preserved milestone-1 design document (agent-produced, 2026-06-11).
> Where this conflicts with docs/protocol.md or the critiques' resolutions, those win.

The repo is confirmed empty. Since this is a greenfield design task, no further exploration is needed. Below is the complete backend design document.

---

# Dice â€” Backend Services Design (Milestone 1 Vertical Slice)

## 0. Architecture summary

Four service **library** crates (`api-gateway`, `auth-service`, `chat-service`, `presence-service`), each with a thin bin wrapper, plus one `dice-server` monolith bin that mounts all four in one process. Shared infrastructure lives in `crates/*`. All realtime traffic is length-delimited protobuf over QUIC (quinn) or binary WebSocket frames over WSS (axum), behind one `Transport` trait. Cross-service events flow over an `EventBus` trait with two impls: NATS JetStream/core and an in-process broadcast bus. Postgres is **always required** (see Decisions); Redis and NATS are swappable for in-memory dev fallbacks. Service-to-service calls: direct trait calls in the monolith, NATS request-reply in split mode â€” never HTTP between services.

```
                    â”Œâ”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€ dice-server (monolith bin) â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”
 Tauri client       â”‚  â”Œâ”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”   Arc<dyn Chat/Auth/Presence> (direct calls)                    â”‚
 (Rust core) â”€â”€QUICâ”€â”¼â”€â–ºâ”‚ api-gatewayâ”‚â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â–º auth-service / chat-service / presence-service   â”‚
            â””â”€WSSâ”€â”€â”€â”¼â”€â–ºâ”‚  sessions  â”‚â—„â”€â”€â”€eventsâ”€â”€â”€â”€  crates/event-bus (in-proc OR NATS)               â”‚
                    â”‚  â””â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”˜                          â”‚            â”‚                          â”‚
                    â””â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”¼â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”¼â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”˜
                                                          PostgreSQL   Redis (or in-mem)
```

---

## 1. File tree (backend area)

```
D:\Dice\
â”œâ”€â”€ Cargo.toml                          # [workspace] resolver="3", workspace.dependencies (single version source)
â”œâ”€â”€ rust-toolchain.toml                 # channel = "stable" (1.88+), edition 2024 crates
â”œâ”€â”€ .gitignore                          # /target, /dev/, .env, .sqlx kept CHECKED IN (offline sqlx)
â”œâ”€â”€ .env.example                        # DATABASE_URL, DICE__* overrides
â”œâ”€â”€ proto/
â”‚   â””â”€â”€ dice/v1/
â”‚       â”œâ”€â”€ common.proto                # Snowflake=uint64, timestamps, error codes
â”‚       â”œâ”€â”€ entities.proto              # User, Guild, Channel, Member, Message, Presence
â”‚       â”œâ”€â”€ gateway.proto               # ClientFrame / ServerFrame (oneof), Hello/Identify/Resume/...
â”‚       â”œâ”€â”€ events.proto                # EventEnvelope + Event oneof (bus + gateway dispatch)
â”‚       â””â”€â”€ rpc.proto                   # request/response pairs for NATS request-reply (split mode)
â”œâ”€â”€ crates/
â”‚   â”œâ”€â”€ common/                         # SnowflakeGen, Id newtypes, DiceError, time helpers, constants
â”‚   â”œâ”€â”€ protocol/                       # prost-generated code; build.rs uses protoc-bin-vendored
â”‚   â”œâ”€â”€ network-core/                   # Transport trait, framing codec, QUIC+WSS acceptors, TLS loading
â”‚   â”œâ”€â”€ auth-core/                      # Argon2id hashing, JwtSigner/JwtVerifier (Ed25519), token structs
â”‚   â”œâ”€â”€ permissions/                    # struct Permissions(u64) + bit consts
â”‚   â”œâ”€â”€ event-bus/                      # EventBus trait; NatsBus, MemoryBus; subject builders; NatsRpc
â”‚   â”œâ”€â”€ database/                       # PgPool builder, migrations/ (sqlx), tx helpers
â”‚   â”‚   â””â”€â”€ migrations/
â”‚   â”‚       â”œâ”€â”€ 0001_users.sql
â”‚   â”‚       â”œâ”€â”€ 0002_auth_sessions.sql
â”‚   â”‚       â”œâ”€â”€ 0003_guilds_channels.sql
â”‚   â”‚       â””â”€â”€ 0004_messages.sql
â”‚   â”œâ”€â”€ cache/                          # Cache + RateLimiter traits; RedisCache, MemoryCache
â”‚   â”œâ”€â”€ config/                         # figment loader: default.toml < env profile < DICE__ env vars
â”‚   â”œâ”€â”€ metrics/                        # metrics facade init + prometheus exporter on admin port
â”‚   â””â”€â”€ logging/                        # tracing-subscriber setup (fmt + EnvFilter, JSON in prod)
â”œâ”€â”€ services/
â”‚   â”œâ”€â”€ api-gateway/
â”‚   â”‚   â”œâ”€â”€ src/
â”‚   â”‚   â”‚   â”œâ”€â”€ lib.rs                  # GatewayService::spawn(deps) -> GatewayHandle
â”‚   â”‚   â”‚   â”œâ”€â”€ session.rs              # Session state machine, outbound writer, ring buffer
â”‚   â”‚   â”‚   â”œâ”€â”€ router.rs               # interest map: guild/channel -> local sessions; bus bridge
â”‚   â”‚   â”‚   â”œâ”€â”€ handshake.rs            # Hello/Identify/Resume logic
â”‚   â”‚   â”‚   â”œâ”€â”€ dispatch.rs             # ClientFrame -> service trait calls; acks
â”‚   â”‚   â”‚   â”œâ”€â”€ rest.rs                 # axum Router: /v1/auth/*, /v1/channels/{id}/messages, ...
â”‚   â”‚   â”‚   â””â”€â”€ resume.rs               # detached-session registry (resume window)
â”‚   â”‚   â””â”€â”€ src/bin/api-gateway.rs      # split-mode bin: NATS rpc clients for Chat/Auth/Presence
â”‚   â”œâ”€â”€ auth-service/
â”‚   â”‚   â”œâ”€â”€ src/lib.rs                  # AuthService + `trait Auth`; register/login/refresh/revoke
â”‚   â”‚   â”œâ”€â”€ src/tokens.rs               # refresh rotation + reuse detection
â”‚   â”‚   â”œâ”€â”€ src/rate_limit.rs           # login/register limits via cache::RateLimiter
â”‚   â”‚   â””â”€â”€ src/bin/auth-service.rs     # split-mode bin: NATS rpc server loop
â”‚   â”œâ”€â”€ chat-service/
â”‚   â”‚   â”œâ”€â”€ src/lib.rs                  # ChatService + `trait Chat`
â”‚   â”‚   â”œâ”€â”€ src/guilds.rs  src/channels.rs  src/messages.rs  src/typing.rs
â”‚   â”‚   â””â”€â”€ src/bin/chat-service.rs
â”‚   â”œâ”€â”€ presence-service/
â”‚   â”‚   â”œâ”€â”€ src/lib.rs                  # PresenceService + `trait Presence`; PresenceStore trait
â”‚   â”‚   â”œâ”€â”€ src/store.rs                # RedisPresenceStore, MemoryPresenceStore
â”‚   â”‚   â””â”€â”€ src/bin/presence-service.rs
â”‚   â”œâ”€â”€ dice-server/                    # THE MONOLITH BIN
â”‚   â”‚   â””â”€â”€ src/main.rs                 # wire pools + bus + services + gateway; graceful shutdown
â”‚   â””â”€â”€ {voice,media,notification,search}-service/README.md   # placeholders only
â”œâ”€â”€ apps/desktop-client/                # (other workstream)
â”œâ”€â”€ infrastructure/
â”‚   â”œâ”€â”€ docker/
â”‚   â”‚   â”œâ”€â”€ docker-compose.yml
â”‚   â”‚   â””â”€â”€ README.md
â”‚   â”œâ”€â”€ kubernetes/README.md            # placeholder
â”‚   â””â”€â”€ terraform/README.md             # placeholder
â”œâ”€â”€ xtask/                              # cargo xtask dev-setup | gen-certs | gen-jwt-keys | prepare-sqlx
â”‚   â””â”€â”€ src/main.rs
â”œâ”€â”€ config/
â”‚   â”œâ”€â”€ default.toml
â”‚   â””â”€â”€ dev.toml
â”œâ”€â”€ scripts/  docs/  benchmarks/        # benchmarks = placeholder
â””â”€â”€ dev/                                # GITIGNORED runtime artifacts: certs/, keys/ (created by xtask)
```

---

## 2. Per-crate dependency lists (major versions; Windows risks flagged)

**Workspace-wide policy: all `rustls`-touching crates pinned to the `ring` crypto provider.** The default `aws-lc-rs` provider requires NASM + CMake on Windows MSVC â€” a known build-pain point on this dev machine. Flag: set `default-features = false` and enable ring features on quinn, sqlx, tokio-tungstenite/axum-server, reqwest (client side).

| Crate | Dependencies |
|---|---|
| `common` | thiserror 2.x, bytes 1.x, time 0.3 (or chrono 0.4 â€” pick **time**), rand 0.9 |
| `protocol` | prost 0.13.x, prost-types 0.13.x, bytes 1.x; **build**: prost-build 0.13.x, **protoc-bin-vendored 3.x** (ships win64 protoc â€” satisfies "no protoc on PATH") |
| `network-core` | tokio 1.x, quinn 0.11.x (`rustls-ring` feature, default-features off), rustls 0.23.x (ring), rustls-pemfile 2.x, tokio-tungstenite 0.26.x (`rustls-tls` w/ ring) for the *client* side, tokio-util 0.7 (codec, CancellationToken), bytes, futures 0.3 |
| `auth-core` | argon2 0.5.x, password-hash 0.5, jsonwebtoken 9.x (EdDSA), ed25519-dalek 2.x + pkcs8 0.10 (keygen in xtask only), sha2 0.10, rand 0.9, base64 0.22 |
| `permissions` | (no deps) |
| `event-bus` | async-nats 0.38.x (compat risk: API still moving between 0.3x releases â€” pin exact), tokio, dashmap 6.x, bytes, prost (envelope decode), async-trait 0.1 |
| `database` | sqlx 0.8.x â€” features: `runtime-tokio`, `tls-rustls-ring`, `postgres`, `macros`, `migrate`, `time` |
| `cache` | redis 0.29.x (`tokio-comp`, `connection-manager`) â€” flag: redis-rs minor versions break APIs, pin exact; dashmap 6.x, governor 0.8 (in-mem rate limiting) |
| `config` | figment 0.10.x (`toml`, `env`), serde 1.x |
| `metrics` | metrics 0.24.x, metrics-exporter-prometheus 0.16.x |
| `logging` | tracing 0.1, tracing-subscriber 0.3 (`env-filter`, `json`) |
| `api-gateway` | axum 0.8.x (`ws`), axum-server 0.7.x (`tls-rustls` â€” verify it can take a ring `ServerConfig`; fallback is hand-rolled hyper 1.x + tokio-rustls 0.26), tower 0.5, tower-http 0.6 (trace, cors), plus network-core/protocol/auth-core/event-bus/cache/metrics, async-trait |
| `auth-service` | sqlx (via database), auth-core, cache, protocol, event-bus, async-trait |
| `chat-service` | database, protocol, permissions, event-bus, common (snowflake), async-trait |
| `presence-service` | cache, protocol, event-bus, database (last_seen write), async-trait |
| `dice-server` | all four service libs + config, logging, metrics, tokio-util |
| `xtask` | rcgen 0.13.x, ed25519-dalek + pkcs8 (PEM key export), clap 4.x |

Other Windows notes: quinn UDP via socket2 works fine on Win11; Docker Desktop 29 + WSL2 runs all compose images; sqlx offline mode (`.sqlx/` checked in, `SQLX_OFFLINE=true`) avoids needing a live DB to compile on CI.

---

## 3. api-gateway

### 3.1 WSS stack choice: **axum** (not raw tokio-tungstenite server)

Justification: the gateway must also serve REST (auth, history) over the same TLS endpoint; axum gives us REST routing + WebSocket upgrade (`axum::extract::ws`, tungstenite underneath) in one server, one TLS config, one port (8443). Raw tokio-tungstenite would force a second HTTP stack for REST. The webview never opens the socket itself â€” the Tauri Rust core does (required anyway for QUIC and for trusting the dev CA) â€” so we keep full control of TLS roots on the client.

**Ports:** `8443/tcp` REST + WSS (`wss://host:8443/gateway`), `8444/udp` QUIC (ALPN `dice/1`), `9600/tcp` localhost-only admin (Prometheus `/metrics`, `/healthz`).

### 3.2 Transport abstraction (crates/network-core)

```rust
/// One logical client connection, QUIC or WSS. Frames are whole protobuf messages.
#[async_trait]
pub trait Transport: Send {
    async fn send(&mut self, frame: Bytes) -> Result<(), TransportError>;
    async fn recv(&mut self) -> Result<Option<Bytes>, TransportError>; // None = peer closed
    async fn close(&mut self, code: CloseCode, reason: &str);
    fn peer_addr(&self) -> SocketAddr;
    fn kind(&self) -> TransportKind; // Quic | Wss
}

pub struct QuicTransport { /* quinn::Connection + one bidi control stream, u32-LE length-prefixed frames, max frame 64 KiB */ }
pub struct WsTransport   { /* axum WebSocket; one binary message == one frame */ }

pub async fn run_quic_acceptor(cfg: TlsConfig, addr: SocketAddr, out: mpsc::Sender<Box<dyn Transport>>, ct: CancellationToken);
```

QUIC mapping: client opens **one bidirectional control stream** after connecting; all frames flow on it (ordered, like WSS â€” keeps M1 simple and resume logic identical across transports). Datagrams/extra streams reserved for voice later. Keep-alive: rely on app-level heartbeat, set quinn `max_idle_timeout` = 90 s.

### 3.3 Gateway wire protocol (proto/dice/v1/gateway.proto sketch)

```proto
message ClientFrame {
  oneof payload {
    Identify identify = 1;          // access_token, proto_version, initial Status, capabilities
    Resume resume = 2;              // session_id, resume_token, last_seq
    Heartbeat heartbeat = 3;        // last_seq received
    SendMessage send_message = 10;  // channel_id, content, client nonce (uint64)
    EditMessage edit_message = 11;
    DeleteMessage delete_message = 12;
    TypingStart typing = 13;        // channel_id
    SetStatus set_status = 14;      // online|idle|dnd|invisible
  }
}
message ServerFrame {
  uint64 seq = 1;                   // 0 for non-resumable control frames (Hello/HeartbeatAck)
  oneof payload {
    Hello hello = 1;                // heartbeat_interval_ms=30000, proto_version
    Ready ready = 2;                // user, session_id, resume_token, guilds+channels, dm_channels, presence snapshot
    Resumed resumed = 3;
    ResumeFailed resume_failed = 4; // client must re-Identify + backfill via REST
    HeartbeatAck hb_ack = 5;
    Event event = 6;                // shared with bus (events.proto)
    Ack ack = 7;                    // nonce -> created message id (echo of SendMessage)
    Error error = 8;                // code + closes connection where fatal
  }
}
```

Versioning rules: `dice.v1` package; tags never reused; new fields additive; `Identify.proto_version` lets the server reject too-old clients with a typed error.

### 3.4 Session lifecycle, backpressure, heartbeats

State machine: `AwaitingIdentify(10 s deadline)` â†’ `Ready` â†’ `Detached(resume window)` â†’ `Closed`.

- **Auth handshake:** server sends `Hello` immediately. Client sends `Identify{access_token}`. Gateway verifies the JWT **locally** with the Ed25519 public key (no auth-service round trip â€” critical for the 100k-connections goal). It then calls `Chat::sync_user_state(user_id)` (guild ids + channel ids + DM channels + member lists) and `Presence::connect(user_id, session_id, guild_ids, status)`, and replies `Ready`.
- **Per-session memory budget (~ a few KiB):** outbound `mpsc::channel::<Bytes>(128)`; a single writer task per session; inbound read loop with 64 KiB frame cap. **If `try_send` on the outbound queue fails, the session is closed with code `SLOW_CONSUMER` (4008)** â€” never buffer unboundedly; client reconnects and resumes. Replay ring buffer per session: last 256 event frames OR 256 KiB, whichever first.
- **Heartbeat:** interval 30 s (server-announced in `Hello`); server closes after **2 missed beats (75 s grace)**. Heartbeats also drive presence TTL refresh (gateway forwards to `Presence::heartbeat`).
- **Inbound command rate limit:** token bucket per session in gateway memory (e.g. 20 commands / 10 s, typing exempt at 1/3 s per channel) â€” protects services regardless of Redis availability.

### 3.5 Resume / sequence numbers

`seq` is **per-session, gateway-assigned** at dispatch time (not global). On disconnect the session moves to a `Detached` registry (`DashMap<SessionId, DetachedSession>`) holding the ring buffer for **90 s**. `Resume{session_id, resume_token, last_seq}`: if the buffer still contains `last_seq+1..`, replay and send `Resumed`; otherwise `ResumeFailed` â†’ client re-Identifies and backfills history via REST (`after=<last known message id>` per visible channel). `resume_token` is a per-session random 32-byte secret issued in `Ready` (prevents session hijack by session_id guessing). Detached registry is monolith/node-local â€” resumes only work against the same gateway node, which is fine (a load balancer later can use connection affinity; cross-node resume is explicitly out of scope).

### 3.6 Subscription model and event fan-out

The gateway maintains an **interest map**, not per-session bus subscriptions:

```rust
struct Router {
    by_guild:   DashMap<GuildId,   SmallVec<SessionId>>, // who locally cares about this guild
    by_channel: DashMap<ChannelId, SmallVec<SessionId>>, // DM channels only
    bus_subs:   DashMap<String, BusSubGuard>,            // refcounted: one bus subscription per subject per NODE
}
```

On `Ready`, sessions register interest in all their guilds and DM channels; the router creates at most **one** bus subscription per `dice.evt.guild.{id}` / `dice.evt.dm.{channel_id}` subject per node (refcounted, dropped when the last interested session leaves). Incoming bus events are decoded once, then the encoded `ServerFrame` bytes are cloned (`Bytes` is refcounted) into each interested session's outbound queue. This keeps NATS subscription counts proportional to *distinct guilds with local members*, not to connections â€” the property that scales to 100k sessions/node.

### 3.7 REST vs gateway split

Over **HTTPS REST** (axum, same bin):
- `POST /v1/auth/register`, `POST /v1/auth/login`, `POST /v1/auth/refresh`, `POST /v1/auth/logout` â€” must work before any socket exists; retry-safe; JSON bodies (explicitly *not* the realtime path; easier curl-ability and form handling).
- `GET /v1/channels/{id}/messages?before|after=<snowflake>&limit=1..100` â€” history pagination. Pull-shaped, cacheable, used for cold-load and resume-failure backfill. Response is **protobuf** (`content-type: application/x-protobuf`, `MessageHistory` message) so the client decodes Message objects with the exact same generated code that handles gateway events â€” one codec path into the SQLite cache.
- `POST /v1/guilds`, `POST /v1/guilds/join {invite_code}`, `POST /v1/users/{id}/dm` (open DM), `GET /v1/users/@me` â€” low-frequency CRUD; protobuf responses.

Over the **gateway socket**: identify/resume/heartbeat, send/edit/delete message, typing, status changes, and *all* serverâ†’client events. Message send rides the socket (not REST) because it is the latency-critical path, it gets ordering for free on the control stream, and the `nonce â†’ Ack` correlation gives optimistic-UI echo without an HTTP round trip. (A REST send endpoint can be added later for bots without protocol changes.)

REST auth: `Authorization: Bearer <access JWT>` verified locally in axum middleware.

### 3.8 Routing to services: one coherent story

Each service crate defines a **trait** (its public API), e.g.:

```rust
#[async_trait]
pub trait Chat: Send + Sync {
    async fn sync_user_state(&self, user: UserId) -> Result<UserSyncState, ChatError>;
    async fn send_message(&self, ctx: ActorCtx, ch: ChannelId, content: String) -> Result<Message, ChatError>;
    async fn edit_message(&self, ctx: ActorCtx, ch: ChannelId, id: MessageId, content: String) -> Result<Message, ChatError>;
    async fn delete_message(&self, ctx: ActorCtx, ch: ChannelId, id: MessageId) -> Result<(), ChatError>;
    async fn get_messages(&self, ctx: ActorCtx, ch: ChannelId, cursor: Cursor, limit: u8) -> Result<Vec<Message>, ChatError>;
    async fn create_guild(&self, ctx: ActorCtx, name: String) -> Result<Guild, ChatError>;
    async fn join_guild(&self, ctx: ActorCtx, invite: &str) -> Result<Guild, ChatError>;
    async fn create_channel(&self, ctx: ActorCtx, guild: GuildId, name: String) -> Result<Channel, ChatError>;
    async fn open_dm(&self, ctx: ActorCtx, other: UserId) -> Result<Channel, ChatError>;
    async fn typing(&self, ctx: ActorCtx, ch: ChannelId) -> Result<(), ChatError>; // bus-only, no DB
}
```

- **Monolith:** gateway holds `Arc<dyn Chat>` etc. pointing at the concrete `ChatService` â€” plain async function calls, zero serialization.
- **Split mode:** `ChatNatsClient` implements the same trait by issuing **NATS request-reply** on `dice.rpc.chat.{method}` (protobuf request/response from `rpc.proto`); each service bin runs a `*NatsServer` loop subscribed with queue group `chat` (free load balancing, no service discovery, no HTTP server per service). HTTP-between-services is rejected: it would add a second serialization story, ports, and discovery for zero benefit when NATS is already mandated.
- M1 ships the Local impls fully and the NATS rpc impls minimally (enough that `auth-service`/`chat-service`/`presence-service`/`api-gateway` bins demonstrably work split â€” same demo, two processes optional).

---

## 4. auth-service

```rust
pub struct AuthService { db: PgPool, limiter: Arc<dyn RateLimiter>, signer: JwtSigner, cfg: AuthConfig, ids: SnowflakeGen }

pub struct AuthTokens { pub access_token: String, pub access_expires_in: u32, pub refresh_token: String, pub session_id: SessionId }
```

- **Passwords:** Argon2id, OWASP params `m=19456 KiB, t=2, p=1`, PHC string stored in `users.password_hash`. Login does a **constant-work dummy verify** when the user doesn't exist (no user-enumeration timing oracle).
- **Access JWT:** EdDSA (Ed25519) via jsonwebtoken 9.x. Claims: `sub` (user id, decimal string), `sid` (auth_session id), `iat`, `exp` (**10 min** lifetime), `iss="dice"`, `aud="dice"`. Asymmetric signing chosen so the gateway (and any future service) verifies with the public key only â€” no shared secret distribution, no auth round trips on Identify. Keypair PEMs generated by `cargo xtask gen-jwt-keys` into `dev/keys/` (prod: injected via config).
- **Refresh tokens:** opaque 32 random bytes, base64url, **only the SHA-256 hash stored**. Lifetime 30 days. **Rotation:** every `/refresh` marks the old row `rotated_at` + `replaced_by` and inserts a new one. **Reuse detection:** presenting an already-rotated token revokes the whole `auth_session` (token-family kill) and returns 401 â€” the canonical stolen-token defense.
- **Revocation:** `logout` revokes the session (`auth_sessions.revoked_at`); access tokens stay valid up to 10 min (accepted M1 trade-off; the gateway *does* drop live sockets on a `dice.evt.auth.session_revoked` bus event, so realtime access dies immediately).
- **Rate limiting** via `cache::RateLimiter` (fixed window `INCR`+`EXPIRE` in Redis; governor keyed limiter in memory-dev): login 10/5 min per IP and 5/5 min per account; register 3/h per IP; refresh 30/h per session.

```rust
#[async_trait]
pub trait RateLimiter: Send + Sync {
    async fn check(&self, key: &str, limit: u32, window: Duration) -> Result<RateDecision, CacheError>;
}
```

---

## 5. chat-service

Owns: users' social graph data â€” guilds, members, channels, DMs, messages. All writes publish events to the bus *after* commit (transactional outbox deferred â€” see Risks).

- **Snowflakes** (crates/common): 41 bits ms since `2026-01-01T00:00:00Z` | 10 bits node id | 12 bits sequence; `i64`-safe, generated in-process (`SnowflakeGen` per node, node id from config). PKs for users, sessions, guilds, channels, messages â€” gives free time-ordering for message pagination.
- **Pagination:** `WHERE channel_id=$1 AND id < $before ORDER BY id DESC LIMIT $n` (and the `after` mirror) on index `(channel_id, id DESC)` â€” pure keyset, no OFFSET.
- **DM dedup:** `channels.dm_key = "{min(user_a,user_b)}:{max(...)}"` with a partial unique index; `open_dm` is `INSERT ... ON CONFLICT (dm_key) DO NOTHING` then select.
- **Invites (M1-minimal):** `guilds.invite_code` â€” 8-char random alphanumeric created with the guild; `join_guild(code)` inserts membership + publishes `MemberAdd`. (Full invite objects later.)
- **Permission checks** on every mutating call: membership â†’ `Permissions(u64)` from `guild_members.permissions` plus owner override; DM channels check `channel_recipients`. Bits (crates/permissions): `VIEW_CHANNEL=1<<0, SEND_MESSAGES=1<<1, MANAGE_MESSAGES=1<<2, MANAGE_CHANNELS=1<<3, MANAGE_GUILD=1<<4, CREATE_INVITE=1<<5, ADMINISTRATOR=1<<6`. Default member bits: `VIEW_CHANNEL|SEND_MESSAGES|CREATE_INVITE`. Edit = author-only; delete = author or `MANAGE_MESSAGES`.
- **Typing:** no DB row ever â€” `typing()` validates membership (cheap, cached later) and publishes `TypingStart{channel_id, user_id, guild_id?}` to the bus; clients expire it after 8 s; gateway never persists it and it is excluded from resume replay buffers (marked non-resumable, `seq=0`... no â€” typing *is* an Event; instead mark `Event.ephemeral=true` and skip ring-buffer insertion while still assigning seq; simpler: ephemeral events are sent with `seq` but not retained, and resume gaps in ephemeral events are acceptable â€” they are stale by definition).
- Message constraints: content 1..4000 chars after trim; hard delete in M1 (`MessageDelete` event carries ids only).
- `channels.last_message_id` denormalized (updated in the send tx) for client channel-list ordering / unread baseline.

---

## 6. presence-service

```rust
pub enum Status { Online, Idle, Dnd, Invisible }  // Invisible broadcasts as Offline

#[async_trait]
pub trait Presence: Send + Sync {
    async fn connect(&self, user: UserId, gw_session: SessionId, guild_ids: Vec<GuildId>, status: Status) -> Result<(), PresenceError>;
    async fn heartbeat(&self, user: UserId, gw_session: SessionId) -> Result<(), PresenceError>;
    async fn set_status(&self, user: UserId, gw_session: SessionId, status: Status) -> Result<(), PresenceError>;
    async fn disconnect(&self, user: UserId, gw_session: SessionId) -> Result<(), PresenceError>;
    async fn snapshot(&self, users: &[UserId]) -> Result<Vec<UserPresence>, PresenceError>; // for Ready
}
```

- **Store (Redis):** per gateway-session key `prs:s:{user_id}:{gw_session_id}` â†’ status byte, **TTL 90 s**, refreshed on each 30 s heartbeat (3Ã— headroom over one missed beat). Set `prs:u:{user_id}` tracks that user's session keys. Aggregate status = highest-priority live session status (`Dnd > Online > Idle`); user is Offline when no session keys survive. A small reaper task (or lazy check on read) detects expiry â†’ publishes Offline. Memory impl mirrors this with `DashMap` + a tick task.
- **Fan-out:** presence-service does NOT know guild membership; the gateway passes `guild_ids` at `connect` (cached in `prs:g:{user}` alongside), and presence publishes `PresenceUpdate` to **each** `dice.evt.guild.{gid}.presence` subject (core NATS, ephemeral). Guild counts per user are small in M1; revisit with interest-based presence later.
- **Last seen:** on transition to Offline, presence writes `UPDATE users SET last_seen_at = now()` directly (shared DB, documented column-ownership exception) and includes `last_seen_at` in the Offline event.
- Invisible: stored truthfully, broadcast as Offline; `snapshot` masks it for everyone except the user themself.

---

## 7. PostgreSQL schema (sqlx migrations, crates/database/migrations/)

```sql
-- 0001_users.sql
CREATE TABLE users (
  id            BIGINT PRIMARY KEY,                  -- snowflake
  username      TEXT NOT NULL CHECK (username ~ '^[a-z0-9_.]{2,32}$'),
  display_name  TEXT CHECK (char_length(display_name) BETWEEN 1 AND 32),
  email         TEXT NOT NULL,
  password_hash TEXT NOT NULL,                       -- PHC argon2id string
  flags         BIGINT NOT NULL DEFAULT 0,
  last_seen_at  TIMESTAMPTZ,
  created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
  updated_at    TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE UNIQUE INDEX users_username_lower_key ON users (LOWER(username));
CREATE UNIQUE INDEX users_email_lower_key    ON users (LOWER(email));

-- 0002_auth_sessions.sql
CREATE TABLE auth_sessions (
  id          BIGINT PRIMARY KEY,                    -- snowflake; JWT `sid`
  user_id     BIGINT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  created_ip  INET,
  user_agent  TEXT,
  created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
  revoked_at  TIMESTAMPTZ
);
CREATE INDEX auth_sessions_user_idx ON auth_sessions (user_id) WHERE revoked_at IS NULL;

CREATE TABLE refresh_tokens (
  id          BIGINT PRIMARY KEY,
  session_id  BIGINT NOT NULL REFERENCES auth_sessions(id) ON DELETE CASCADE,
  token_hash  BYTEA  NOT NULL,                       -- sha256(raw token)
  issued_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
  expires_at  TIMESTAMPTZ NOT NULL,
  rotated_at  TIMESTAMPTZ,
  replaced_by BIGINT REFERENCES refresh_tokens(id)
);
CREATE UNIQUE INDEX refresh_tokens_hash_key   ON refresh_tokens (token_hash);
CREATE INDEX refresh_tokens_session_idx       ON refresh_tokens (session_id);

-- 0003_guilds_channels.sql
CREATE TABLE guilds (
  id          BIGINT PRIMARY KEY,
  name        TEXT NOT NULL CHECK (char_length(name) BETWEEN 1 AND 100),
  owner_id    BIGINT NOT NULL REFERENCES users(id),
  invite_code TEXT NOT NULL,
  created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE UNIQUE INDEX guilds_invite_code_key ON guilds (invite_code);

CREATE TABLE guild_members (
  guild_id    BIGINT NOT NULL REFERENCES guilds(id) ON DELETE CASCADE,
  user_id     BIGINT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  nickname    TEXT CHECK (char_length(nickname) BETWEEN 1 AND 32),
  permissions BIGINT NOT NULL DEFAULT 35,            -- u64 bits stored via i64::from_ne_bytes cast in Rust
  joined_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
  PRIMARY KEY (guild_id, user_id)
);
CREATE INDEX guild_members_user_idx ON guild_members (user_id);

CREATE TABLE channels (
  id              BIGINT PRIMARY KEY,
  channel_type    SMALLINT NOT NULL,                 -- 0 guild_text, 1 dm
  guild_id        BIGINT REFERENCES guilds(id) ON DELETE CASCADE,
  name            TEXT CHECK (char_length(name) BETWEEN 1 AND 100),
  topic           TEXT CHECK (char_length(topic) <= 1024),
  position        INT NOT NULL DEFAULT 0,
  dm_key          TEXT,                              -- "minUid:maxUid" for type=1
  last_message_id BIGINT,
  created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
  CHECK ( (channel_type = 0 AND guild_id IS NOT NULL AND name IS NOT NULL AND dm_key IS NULL)
       OR (channel_type = 1 AND guild_id IS NULL AND dm_key IS NOT NULL) )
);
CREATE INDEX channels_guild_idx          ON channels (guild_id) WHERE guild_id IS NOT NULL;
CREATE UNIQUE INDEX channels_dm_key_key  ON channels (dm_key)  WHERE dm_key  IS NOT NULL;

CREATE TABLE channel_recipients (
  channel_id BIGINT NOT NULL REFERENCES channels(id) ON DELETE CASCADE,
  user_id    BIGINT NOT NULL REFERENCES users(id)    ON DELETE CASCADE,
  PRIMARY KEY (channel_id, user_id)
);
CREATE INDEX channel_recipients_user_idx ON channel_recipients (user_id);

-- 0004_messages.sql
CREATE TABLE messages (
  id         BIGINT PRIMARY KEY,                     -- snowflake => created_at derivable
  channel_id BIGINT NOT NULL REFERENCES channels(id) ON DELETE CASCADE,
  author_id  BIGINT NOT NULL REFERENCES users(id),
  content    TEXT NOT NULL CHECK (char_length(content) BETWEEN 1 AND 4000),
  edited_at  TIMESTAMPTZ
  -- reply_to_id BIGINT: reserved for a later migration
);
CREATE INDEX messages_channel_pagination_idx ON messages (channel_id, id DESC);
```

---

## 8. NATS subjects + event payloads

**Subject scheme** (mirrored verbatim by the in-proc bus, which implements `*`/`>` wildcard matching):

| Subject | Kind | Payload |
|---|---|---|
| `dice.evt.guild.{guild_id}.msg` | **JetStream** (stream `DICE_EVT`, subjects `dice.evt.>` minus ephemeral; limits: maxage 5 min, maxbytes 256 MB â€” fan-out buffer, NOT the source of truth) | MessageCreate/Update/Delete, ChannelCreate, MemberAdd/Remove, GuildUpdate |
| `dice.evt.dm.{channel_id}.msg` | JetStream | DM message events |
| `dice.evt.guild.{guild_id}.typing` | core NATS (ephemeral) | TypingStart |
| `dice.evt.dm.{channel_id}.typing` | core NATS | TypingStart |
| `dice.evt.guild.{guild_id}.presence` | core NATS | PresenceUpdate |
| `dice.evt.auth.session_revoked` | core NATS | SessionRevoked{user_id, sid} |
| `dice.rpc.{service}.{method}` | request-reply, queue group = service name | rpc.proto pairs |

**Envelope (events.proto):**

```proto
message EventEnvelope {
  uint64 event_id = 1;      // snowflake (dedupe/ordering hints)
  uint64 occurred_at_ms = 2;
  bool   ephemeral = 3;     // typing/presence: excluded from resume buffers
  Event  event = 4;
}
message Event {
  oneof body {
    MessageCreate message_create = 1;   // full Message entity
    MessageUpdate message_update = 2;
    MessageDelete message_delete = 3;   // ids only
    TypingStart   typing_start   = 4;
    PresenceUpdate presence_update = 5;
    GuildMemberAdd member_add    = 6;
    ChannelCreate  channel_create = 7;
    SessionRevoked session_revoked = 8;
  }
}
```

The gateway wraps `Event` unchanged into `ServerFrame.event` â€” one schema from publisher to client. Postgres remains the source of truth; JetStream's short retention only smooths gateway-node consumer hiccups (consumers are ephemeral, `deliver_policy: new`).

```rust
#[async_trait]
pub trait EventBus: Send + Sync {
    async fn publish(&self, subject: &str, payload: Bytes) -> Result<(), BusError>;
    async fn subscribe(&self, pattern: &str) -> Result<BusSubscription, BusError>; // pattern: tokens, * and >
}
pub struct BusSubscription { pub rx: mpsc::Receiver<BusMessage> } // bounded 1024; lag => BusError::Overflow
```

`MemoryBus`: `RwLock<Vec<(SubjectPattern, mpsc::Sender<BusMessage>)>>` (subscription counts are small per node); publish matches patterns and `try_send`s â€” overflow drops + increments a metric, mirroring NATS slow-consumer semantics.

---

## 9. Monolith bin, config, shutdown

**`services/dice-server/src/main.rs` composition order:**

1. `logging::init()` â†’ `config::load()` (figment: `config/default.toml` â† `config/{profile}.toml` â† env vars `DICE__SECTION__KEY`, e.g. `DICE__DATABASE__URL`) â†’ `metrics::init(admin_addr)`.
2. Build shared deps **once**: `PgPool` (sqlx, max 20 conns, run migrations on start in dev), `Arc<dyn Cache>` (`redis`|`memory` per `[cache].backend`), `Arc<dyn EventBus>` (`nats`|`memory` per `[bus].backend`), `SnowflakeGen{node_id}`, `JwtSigner/Verifier` from PEM paths.
3. Construct services: `AuthService`, `ChatService`, `PresenceService` â†’ wrap as `Arc<dyn Auth/Chat/Presence>`.
4. `GatewayService::spawn(GatewayDeps{ auth_verifier, chat, presence, bus, cfg }, cancellation_token)` â†’ starts QUIC acceptor, axum (REST+WSS), router/bus bridge, detached-session reaper.
5. **Graceful shutdown** (tokio-util `CancellationToken` tree + `TaskTracker`): on Ctrl-C â†’ stop accepting new conns â†’ broadcast `ServerFrame::Error(code=GOING_AWAY)` hinting reconnect â†’ 10 s drain â†’ cancel children â†’ close bus subs â†’ `pool.close()`; hard deadline 15 s.

Degraded dev mode = `profile=dev-lite`: `cache.backend="memory"`, `bus.backend="memory"`, Postgres still required (decision below). Split mode: each `src/bin/*.rs` does the same wiring for its one service plus a NATS rpc server; `api-gateway` bin uses `*NatsClient` impls.

**config/default.toml sketch:**

```toml
node_id = 1
[gateway]  rest_addr = "0.0.0.0:8443"  quic_addr = "0.0.0.0:8444"  heartbeat_ms = 30000  session_queue = 128
[tls]      cert = "dev/certs/server.crt"  key = "dev/certs/server.key"
[auth]     access_ttl_secs = 600  refresh_ttl_days = 30  jwt_private_pem = "dev/keys/jwt_ed25519.pem"  jwt_public_pem = "dev/keys/jwt_ed25519.pub.pem"
[database] url = "postgres://dice:dice_dev@localhost:5432/dice"  max_conns = 20
[cache]    backend = "redis"   url = "redis://localhost:6379"
[bus]      backend = "nats"    url = "nats://localhost:4222"
[admin]    addr = "127.0.0.1:9600"
```

---

## 10. infrastructure/docker/docker-compose.yml + dev TLS

```yaml
name: dice
services:
  postgres:
    image: postgres:17-alpine
    environment: { POSTGRES_USER: dice, POSTGRES_PASSWORD: dice_dev, POSTGRES_DB: dice }
    ports: ["5432:5432"]
    volumes: [pgdata:/var/lib/postgresql/data]
    healthcheck: { test: ["CMD-SHELL", "pg_isready -U dice -d dice"], interval: 5s, timeout: 3s, retries: 10 }
  redis:
    image: redis:7.4-alpine
    command: ["redis-server", "--maxmemory", "256mb", "--maxmemory-policy", "allkeys-lru", "--appendonly", "no"]
    ports: ["6379:6379"]
    healthcheck: { test: ["CMD", "redis-cli", "ping"], interval: 5s, timeout: 3s, retries: 10 }
  nats:
    image: nats:2.11-alpine
    command: ["-js", "-sd", "/data", "-m", "8222"]
    ports: ["4222:4222", "8222:8222"]
    volumes: [natsdata:/data]
    healthcheck: { test: ["CMD", "wget", "-qO-", "http://localhost:8222/healthz"], interval: 5s, timeout: 3s, retries: 10 }
volumes: { pgdata: {}, natsdata: {} }
```

**Dev TLS (QUIC + WSS share one cert):** `cargo xtask dev-setup` uses **rcgen 0.13** to generate (a) a dev CA, (b) a leaf cert with SANs `localhost`, `127.0.0.1`, `::1`, into gitignored `dev/certs/`. Server loads leaf+key into one rustls 0.23 `ServerConfig` (ring provider) used by both quinn (ALPN `dice/1`) and axum-server. **Client trust:** because the Tauri client does ALL networking in its Rust core (mandatory anyway â€” webviews can't speak raw QUIC), it builds its rustls `RootCertStore` from `DICE_DEV_CA` (path to `dev/certs/ca.crt`) in dev builds only; release builds use OS roots/webpki-roots. No OS cert-store installation needed. xtask also runs `gen-jwt-keys` and prints the compose up command.

---

## 11. Key decisions and why

1. **Postgres always required (no in-memory DB mode).** sqlx compile-time-checked queries would demand a second storage implementation to fake; migrations/dm-dedup/keyset pagination rely on real SQL. `docker compose up postgres` is one cheap container; the *expensive* dev friction (Redis semantics, NATS streams) is what the memory fallbacks remove.
2. **axum for WSS + REST in one server** â€” one TLS config, one port, REST needed anyway; tungstenite still does the WS framing underneath.
3. **Trait-per-service with Local + NATS request-reply impls** â€” direct calls in the monolith (zero cost), NATS RPC in split mode; rejects HTTP-between-services (second stack, discovery, no benefit).
4. **JWT verified locally at the gateway (Ed25519)** â€” Identify costs zero auth-service round trips; public-key distribution instead of shared secrets keeps the later multi-service story sane.
5. **Per-session gateway-assigned seq + node-local 90 s resume buffer** â€” bounded memory (256 frames/256 KiB per detached session), no global ordering service; resume failure degrades safely to REST backfill.
6. **Interest-map fan-out (one bus subscription per subject per node)** â€” subscription count scales with distinct guilds, not connections; the single biggest 100k-connection enabler in this design, together with `Bytes`-cloned pre-encoded frames and bounded per-session queues that disconnect slow consumers instead of buffering.
7. **Message send over the gateway socket, history over REST** â€” send is latency/ordering critical with nonce-acks for optimistic UI; history is pull-shaped and retry-safe. Auth REST is JSON (not realtime); history REST is protobuf to reuse the exact client decode path.
8. **JetStream as a short-retention fan-out buffer only; Postgres is truth** â€” avoids dual-source-of-truth consistency problems while honoring the JetStream mandate; typing/presence stay on core NATS because stale ephemeral events are worthless.
9. **ring crypto provider everywhere** â€” dodges the aws-lc-rs NASM/CMake toolchain requirement on Windows (the most common Rust-TLS Windows build failure).
10. **protoc-bin-vendored in protocol/build.rs** â€” satisfies "no protoc on PATH" hermetically on Windows/CI.
11. **Snowflake PKs with custom 2026 epoch** â€” time-ordered ids make message pagination index-only and give created_at for free; node-id field reserves multi-node correctness.

## 12. Risks and open questions

- **DB-commitâ†’bus-publish gap (no transactional outbox in M1):** a crash between commit and publish loses a fan-out event; clients heal via REST backfill on reconnect, but live clients could miss one message until then. Mitigation candidates for M2: outbox table or publishing inside a listener on WAL. Accepted for M1; document it.
- **async-nats and redis crate API churn:** both have history of breaking minor releases â€” pin exact versions in the workspace and gate upgrades.
- **axum-server 0.7 + rustls-ring interop:** verify it accepts a pre-built ring `ServerConfig`; fallback is a small hyper-1.x/tokio-rustls accept loop (contained in network-core).
- **quinn + self-signed certs:** rustls requires proper SANs and the client must use the dev CA root store â€” a frequent "works in WSS, fails in QUIC" footgun; xtask must generate CA-signed (not self-signed-leaf) certs and the client must thread `DICE_DEV_CA` into *both* transports.
- **Presence fan-out via per-guild publish** breaks down for users in hundreds of guilds â€” fine for M1, revisit with subscription/interest-based presence later.
- **Resume is node-local** â€” fine for one node; LB affinity or cross-node resume needed when scaling out (explicitly deferred).
- **sqlx offline data (`.sqlx/`)** must be regenerated (`cargo xtask prepare-sqlx`) whenever queries change, or CI breaks â€” add a CI check.
- **Open question:** should `Ready` include full member lists for large guilds or a lazy `RequestGuildMembers` flow? M1: include members inline (guilds are tiny), but cap at 100 and note the lazy flow as the v1.1 path so the protocol reserves the field now.
- **Open question:** WSS fallback negotiation policy in the client (try QUIC, race vs sequential with 2 s timeout) â€” client-team decision; gateway treats both identically so either works.

### Critical Files for Implementation
- D:\Dice\services\api-gateway\src\lib.rs â€” gateway composition: session state machine, interest-map router, handshake/resume, REST router
- D:\Dice\crates\network-core\src\lib.rs â€” Transport trait + QUIC/WSS acceptors + framing + TLS loading (everything else plugs into this seam)
- D:\Dice\proto\dice\v1\gateway.proto â€” the wire contract (with events.proto/entities.proto) that client and all services share
- D:\Dice\crates\event-bus\src\lib.rs â€” EventBus trait + NatsBus/MemoryBus + subject builders + NATS rpc helper
- D:\Dice\services\dice-server\src\main.rs â€” monolith wiring: config, pools, service construction, graceful shutdown
- D:\Dice\crates\database\migrations\0001_users.sql (and siblings 0002â€“0004) â€” the full M1 schema as specified in section 7