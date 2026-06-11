> Preserved milestone-1 design document (agent-produced, 2026-06-11).
> Where this conflicts with docs/protocol.md or the critiques' resolutions, those win.

The repo is empty (greenfield confirmed), so this is pure design from the spec. Below is the design document for the desktop client + `network-core`.

---

# Dice â€” Design Doc: `network-core` + Desktop Client (Tauri 2 + SolidJS)

Scope: `crates/network-core`, `apps/desktop-client` (SolidJS frontend + `src-tauri` host), client SQLite cache, and the build/dev pipeline on Windows 11. Coordination points with the protocol/gateway teams are flagged inline.

---

## 0. Architectural overview

```
â”Œâ”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€ Dice.exe (Tauri host, Rust) â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”
â”‚  tauri commands â”€â”€â–º ClientCore â”€â”€â–º ApiClient (HTTPS + protobuf bodies)          â”‚
â”‚       â–²                  â”‚  â–²                                                   â”‚
â”‚       â”‚                  â”‚  â””â”€â”€â–º GatewayClient (network-core: QUIC â–¸ WSS)       â”‚
â”‚  tauri events â—„â”€â”€ bridge task â—„â”€â”€ typed event stream (prost types)              â”‚
â”‚       â”‚                  â”‚                                                      â”‚
â”‚       â”‚                  â””â”€â”€â–º Cache worker thread (rusqlite, WAL)               â”‚
â”‚       â”‚                       (events write cache FIRST, then emit to webview)  â”‚
â””â”€â”€â”€â”€â”€â”€â”€â”¼â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”˜
        â–¼
  WebView2 (SolidJS) â€” stores only; no network, no SQLite, no tokens
```

Hard rule: **all networking, crypto, tokens, and persistence live in the Rust host.** The webview is a pure renderer fed by Tauri commands (pull) and Tauri events (push). The "no JSON on the realtime path" mandate applies to the wire; the in-process Tauri IPC bridge is serde-JSON and that is an explicit, documented exception (local IPC, not network).

**Split of responsibilities (coordination point with api-gateway team):**
- **Gateway connection (QUIC/WSS, protobuf frames):** serverâ†’client event push (message_create, typing, presence, guild/channel changes) + clientâ†’server lightweight commands (identify/resume/heartbeat, typing_start, presence_update).
- **HTTPS + `application/x-protobuf` bodies (reqwest):** auth (register/login/refresh), `send_message`, `fetch_history`, and other request/response actions. Rationale: HTTP gives idempotency, retry semantics, and per-request error reporting for free; this mirrors Discord's proven REST+gateway split. Still binary protobuf â€” no JSON anywhere on the network. The protobuf envelope reserves a `SendMessage` client frame number so sends can move onto the gateway later without a breaking change.

---

## 1. `crates/network-core`

### 1.1 File tree

```
crates/network-core/
â”œâ”€â”€ Cargo.toml
â””â”€â”€ src/
    â”œâ”€â”€ lib.rs              # re-exports: GatewayClient, GatewayHandle, ApiClient, Command, GatewayEvent, ConnState
    â”œâ”€â”€ config.rs           # GatewayConfig, TransportPolicy, TlsOptions, endpoints
    â”œâ”€â”€ tls.rs              # shared rustls ClientConfig builder; dev-trust anchor injection
    â”œâ”€â”€ error.rs            # TransportError, GatewayError, ApiError (thiserror)
    â”œâ”€â”€ transport/
    â”‚   â”œâ”€â”€ mod.rs          # Transport trait + AnyTransport enum (static dispatch)
    â”‚   â”œâ”€â”€ quic.rs         # quinn impl: 1 bidi stream, length-prefixed frames
    â”‚   â”œâ”€â”€ wss.rs          # tokio-tungstenite impl: 1 binary WS message = 1 frame
    â”‚   â””â”€â”€ framing.rs      # u32-LE length prefix codec, max-size guards
    â”œâ”€â”€ gateway/
    â”‚   â”œâ”€â”€ mod.rs          # GatewayClient::connect â†’ spawns driver task; GatewayHandle
    â”‚   â”œâ”€â”€ driver.rs       # the single owning task: select! over transport/commands/timers
    â”‚   â”œâ”€â”€ state.rs        # ConnState machine + transitions
    â”‚   â”œâ”€â”€ heartbeat.rs    # interval, ack tracking, jittered first beat
    â”‚   â””â”€â”€ backoff.rs      # full-jitter exponential backoff
    â””â”€â”€ api.rs              # ApiClient: HTTPS + protobuf bodies, auto token refresh-and-retry
```

### 1.2 Dependencies (major versions)

| Dep | Version | Notes / risk flags |
|---|---|---|
| tokio | 1.x | features: `rt-multi-thread, macros, time, sync, net, io-util` |
| quinn | 0.11.x | **Use the `rustls-ring` feature, NOT `rustls-aws-lc-rs`.** `aws-lc-sys` requires CMake + NASM on Windows MSVC â€” a known dev-machine landmine. Ring builds clean with just MSVC tools. |
| rustls | 0.23.x | `ring` provider, `tls12` off (TLS 1.3 only) |
| tokio-tungstenite | 0.26.x | `rustls-tls-webpki-roots` feature; pass `Connector::Rustls(Arc<ClientConfig>)` so QUIC and WSS share one TLS config. Verify rustls 0.23 alignment at lock time (0.26.x uses rustls 0.23 â€” compatible). |
| webpki-roots | 0.26.x | real-world roots; dev cert appended as extra anchor (see 1.7) |
| prost | 0.13.x | decode/encode only; codegen lives in `crates/protocol` (which vendors protoc via `protoc-bin-vendored` 3.x â€” it ships a win32 `protoc.exe`, satisfying the "no protoc on PATH" constraint) |
| dice-protocol | path dep | generated types + `ClientFrame`/`ServerFrame` envelopes |
| reqwest | 0.12.x | `default-features = false`, features: `rustls-tls-manual-roots, http2` |
| bytes | 1.x | |
| rand | 0.9.x | backoff jitter |
| thiserror | 2.x | |
| tracing | 0.1.x | |
| futures-util | 0.3.x | stream splitting for tungstenite |
| url | 2.x | |

### 1.3 Transport abstraction

Async-fn-in-trait is not dyn-compatible; use **enum dispatch** instead of `Box<dyn>` â€” zero allocation, no `async-trait` macro dep:

```rust
pub trait Transport: Send {
    async fn send(&mut self, frame: Bytes) -> Result<(), TransportError>;
    async fn recv(&mut self) -> Result<Bytes, TransportError>;   // one whole frame
    async fn close(&mut self, reason: CloseReason) -> Result<(), TransportError>;
    fn kind(&self) -> TransportKind;  // Quic | Wss
}

pub enum AnyTransport { Quic(QuicTransport), Wss(WssTransport) }
// impl Transport for AnyTransport by match-delegation (5 lines each)
```

**QUIC framing:** after handshake (ALPN `dice/1`), client opens **one bidirectional stream** for the gateway protocol. Frames are `u32 LE length || prost bytes`. Inbound frame cap 1 MiB (drop connection if exceeded â€” bounded buffers), outbound cap 256 KiB. `TransportConfig`: `keep_alive_interval(15s)`, `max_idle_timeout(60s)`, 0-RTT disabled for M1. QUIC datagrams unused (reserved for voice milestone).

**WSS framing:** `wss://host/gateway`, one binary WS message = one protobuf frame (the WS layer already does length delimiting). permessage-deflate **off** (CPU/RAM cost; protobuf is already compact). tungstenite auto-answers pings; liveness is the app heartbeat, identical on both transports.

**Selection policy:** try QUIC with a 3 s connect timeout (UDP is commonly blocked on corp networks); on failure, fall back to WSS within the same backoff attempt. After 2 consecutive QUIC failures, prefer WSS for the rest of the session and re-probe QUIC opportunistically on later reconnects. Last-good transport persisted by the caller (client stores it in SQLite `meta`).

### 1.4 Connection state machine

```rust
pub enum ConnState {
    Idle,
    Connecting { attempt: u32, transport: TransportKind },
    Authenticating,                       // sent Identify or Resume, awaiting Ready/Resumed
    Ready { session_id: String, transport: TransportKind },
    Backoff { until: Instant, attempt: u32 },
    Failed { reason: FatalError },        // e.g. AuthRejected â€” no auto-retry, surface to UI
}
```

Flow: `Connecting â†’ (transport up) â†’ expect Hello{heartbeat_interval_ms} â†’ send Identify{token, protocol_version, capabilities} or Resume{session_id, resume_token, last_seq} â†’ Ready{...} | Resumed{} | InvalidSession{} â†’ Ready`. On `InvalidSession`, fall back to full Identify and emit `GatewayEvent::SessionInvalidated` so the client layer re-syncs state over HTTP (cache cursors, Â§3.4).

Every server frame carries a monotonically increasing `seq`; the driver records `last_seq` and includes it in heartbeats and Resume. *(Coordination point: protocol team owns the envelope; the field requirements from this side are: `seq` on ServerFrame, `Hello/Ready/Resumed/InvalidSession/HeartbeatAck/Reconnect` control frames, nonce echo on MessageCreate.)*

**Heartbeats:** interval from Hello; first beat jittered by `rand(0..interval)`. Each beat carries `last_seq`. Two missed acks â‡’ close transport, transition to Backoff, attempt Resume. Heartbeat is app-level so behavior is identical over QUIC and WSS.

**Backoff:** full jitter â€” `delay = rand(0 ..= min(30s, 500ms * 2^attempt))`; attempt counter resets only after a connection reaches Ready and survives â‰¥ 60 s (prevents thundering reconnect loops counting as success).

### 1.5 Public API

```rust
pub struct GatewayConfig {
    pub quic: Option<QuicEndpoint>,        // server_name + SocketAddr or host:port
    pub wss: Option<Url>,
    pub policy: TransportPolicy,           // QuicFirst { quic_timeout: Duration } | WssOnly | QuicOnly
    pub tls: TlsOptions,                   // extra_trust_anchors: Vec<CertificateDer<'static>>
    pub token: Arc<dyn TokenProvider>,     // fresh access token per (re)identify
}

pub trait TokenProvider: Send + Sync {
    fn access_token(&self) -> BoxFuture<'_, Result<String, AuthError>>;
}

impl GatewayClient {
    /// Spawns the driver task; returns immediately. Never blocks the caller on network.
    pub fn connect(cfg: GatewayConfig) -> GatewayHandle;
}

pub struct GatewayHandle {
    pub fn send(&self, cmd: Command) -> impl Future<Output = Result<(), SendError>>; // bounded mpsc(64)
    pub fn events(&mut self) -> &mut mpsc::Receiver<GatewayEvent>;                   // bounded(256), single consumer
    pub fn state(&self) -> watch::Receiver<ConnState>;
    pub async fn shutdown(self);
}

pub enum Command {
    TypingStart { channel_id: u64 },
    UpdatePresence(proto::PresenceUpdate),
    ForceReconnect,        // user-triggered
}

pub enum GatewayEvent {
    Ready(proto::Ready),                 // includes guild/channel/DM snapshot
    Resumed { replayed: u32 },
    SessionInvalidated,                  // caller must HTTP-resync
    Dispatch(proto::ServerEvent),        // MessageCreate, TypingStart, PresenceUpdate, Guild*, Channel*, ...
}
```

Backpressure design: the driver `await`s `event_tx.send(...)` â€” if the consumer (bridge task) stalls, backpressure propagates naturally into the QUIC stream / WS socket. No unbounded queues anywhere; per-connection memory is `O(framing buffer + 2 bounded channels)`.

### 1.6 `ApiClient`

```rust
impl ApiClient {
    pub fn new(base: Url, tls: TlsOptions, token: Arc<dyn TokenProvider>) -> Self;
    pub async fn register(&self, r: proto::RegisterRequest) -> Result<proto::AuthTokens, ApiError>;
    pub async fn login(&self, r: proto::LoginRequest) -> Result<proto::AuthTokens, ApiError>;
    pub async fn refresh(&self, refresh_token: &str) -> Result<proto::AuthTokens, ApiError>;
    pub async fn send_message(&self, channel_id: u64, r: proto::SendMessageRequest) -> Result<proto::Message, ApiError>; // r includes client nonce
    pub async fn fetch_messages(&self, channel_id: u64, before: Option<u64>, limit: u8) -> Result<Vec<proto::Message>, ApiError>;
}
```

`Content-Type: application/x-protobuf`; `Authorization: Bearer <access>`; one automatic refresh-and-retry on 401. Errors are a protobuf `ErrorResponse{code, message}` body, mapped to `ApiError` variants.

### 1.7 Dev-mode trust of self-signed localhost cert

Do **not** ship a "disable verification" switch. Instead: `TlsOptions { extra_trust_anchors }` appends the dev CA cert (PEM, generated by the infra team's `scripts/gen-dev-certs` via rcgen, written to a well-known path like `infrastructure/docker/certs/dev-ca.pem`) into the rustls root store. Full verification still runs â€” hostname (`localhost`) must match the cert SAN. One `Arc<rustls::ClientConfig>` from `tls.rs` is shared by quinn, tungstenite, and reqwest, so dev trust works identically across QUIC, WSS, and HTTPS. Loading an extra anchor is gated by `cfg(debug_assertions)` **or** explicit env `DICE_EXTRA_CA=path` so release builds can still point at self-hosted servers (self-hosting story) without code changes.

---

## 2. Tauri 2 host (`apps/desktop-client/src-tauri`)

### 2.1 File tree

```
apps/desktop-client/src-tauri/
â”œâ”€â”€ Cargo.toml
â”œâ”€â”€ tauri.conf.json
â”œâ”€â”€ build.rs                 # tauri-build
â”œâ”€â”€ capabilities/default.json
â”œâ”€â”€ icons/                   # generated by `tauri icon` from one 1024px PNG
â””â”€â”€ src/
    â”œâ”€â”€ main.rs              # thin: dice_desktop::run()
    â”œâ”€â”€ lib.rs               # tauri::Builder, manage(ClientCore), register commands, setup hook
    â”œâ”€â”€ state.rs             # ClientCore { api, gateway: Mutex<Option<GatewayHandle>>, cache, session }
    â”œâ”€â”€ session.rs           # keyring refresh-token storage, in-memory access token, TokenProvider impl
    â”œâ”€â”€ bridge.rs            # event pump: gateway events â†’ cache.apply â†’ emit to webview (+presence coalescing)
    â”œâ”€â”€ commands/
    â”‚   â”œâ”€â”€ auth.rs          # login, register, logout, session_status
    â”‚   â”œâ”€â”€ bootstrap.rs     # get_bootstrap (cache snapshot for instant first paint)
    â”‚   â”œâ”€â”€ messages.rs      # send_message, fetch_messages (cache-first + network reconcile)
    â”‚   â”œâ”€â”€ guilds.rs        # list_guilds, list_channels, list_dms, member list
    â”‚   â””â”€â”€ presence.rs      # set_presence, start_typing
    â””â”€â”€ cache/
        â”œâ”€â”€ mod.rs           # Cache handle (mpsc to worker thread)
        â”œâ”€â”€ worker.rs        # dedicated thread owning rusqlite::Connection
        â”œâ”€â”€ schema.rs        # embedded migrations, PRAGMA user_version
        â””â”€â”€ queries.rs       # typed upserts/pages
```

**Host crate deps:** tauri 2.x, tauri-build 2.x, tauri-plugin-single-instance 2.x, dice-network-core (path), dice-protocol (path), rusqlite 0.32.x (`bundled` feature â€” compiles SQLite via cc, no system dep, clean on MSVC), keyring 3.x (`windows-native` feature), serde/serde_json 1.x (IPC only), ulid 1.x (nonces), tokio, tracing + tracing-subscriber. Optional: tauri-specta 2.x for generated TS bindings (recommended; if it lags Tauri point releases, fall back to hand-written `ipc.ts`).

### 2.2 The bridge

`bridge.rs` runs one task consuming `GatewayHandle::events()`:

1. **Cache first:** `cache.apply_event(&ev).await` (upsert message/user/channel rows, advance sync cursors).
2. **Then emit** the corresponding Tauri event to the webview.

Sequential single task â‡’ ordering is preserved and the UI can never observe state the cache doesn't have (offline restart is always consistent).

**Tauri commands (UI â†’ host), all async:**

| Command | Behavior |
|---|---|
| `login{email,password}` / `register{...}` | ApiClient call â†’ store refresh token (keyring) + access token (RAM) â†’ connect gateway |
| `logout` | revoke via API best-effort, clear keyring, wipe cache DB, drop gateway |
| `get_bootstrap` | cache snapshot: current user, guilds, channels, DM list, last-selected channel + its newest 50 messages |
| `send_message{channel_id, content}` | host generates `nonce` (ulid), writes `pending=1` row to cache, returns the pending message to UI immediately, fires the HTTP send in the background; on success the gateway echo (or HTTP response) reconciles by nonce; on failure emits `message_failed{nonce}` |
| `fetch_messages{channel_id, before?, limit}` | serve cached page instantly; if cursor says the range may be stale/gapped, also fetch from API, upsert, emit `messages_refreshed{channel_id}` |
| `start_typing{channel_id}` | gateway `Command::TypingStart` (host rate-limits to 1 per 8 s per channel) |
| `set_presence{status}` | gateway `Command::UpdatePresence` |
| `connection_state` | current `ConnState` (UI also subscribes to the event) |

**Tauri events (host â†’ UI):** `gateway://ready`, `gateway://conn_state`, `chat://message_create` (with nonce for optimistic-send reconcile), `chat://message_failed`, `chat://typing_start`, `presence://batch` (PresenceUpdates coalesced on a 100 ms tick â€” keeps webview wakeups low for the idle-CPU target), `guild://changed`, `cache://resynced`.

**IDs cross IPC as strings.** Snowflake u64 exceeds JS `Number.MAX_SAFE_INTEGER`; every id is stringified at the bridge and back-parsed in the host. This is a hard convention, enforced by the TS types.

### 2.3 Token/session storage â€” decision: OS keyring

- **Refresh token (long-lived, opaque, rotating):** `keyring` crate â†’ Windows Credential Manager (service `"Dice"`, account = user id). Justification: OS-grade at-rest protection tied to the OS login, zero extra UX (no master password), tiny dependency; Windows credential blob limit (2560 bytes) is far above an opaque token. The alternatives lose: encrypted file (e.g. stronghold plugin) needs a key that either comes from a password prompt (bad UX) or sits on disk next to the file (defeats the purpose), and stronghold is heavy.
- **Access token (JWT, ~10â€“15 min):** RAM only, inside `session.rs`; the `TokenProvider` impl refreshes through `ApiClient::refresh` on demand (gateway re-identify and HTTP 401 paths both flow through it, with a `tokio::sync::Mutex` so concurrent callers don't double-refresh â€” rotation invalidates the old refresh token).
- Linux Secret-Service-absent fallback (headless) is explicitly out of scope for M1 (Windows target); flagged in risks.

### 2.4 App lifecycle

- **Startup:** open SQLite + migrate â†’ load session from keyring â†’ window shows immediately; UI calls `get_bootstrap` and renders from cache (offline-first) â†’ in parallel, host refreshes token and connects gateway â†’ on `Ready`, diff snapshot into cache, emit granular updates. No network on the first-paint path â‡’ cold start < 2 s is achievable.
- **No session:** UI gets `session_status = logged_out`, shows login screen.
- **Reconnect:** entirely inside network-core (backoff + resume). UI just renders `conn_state` as a banner. On `SessionInvalidated`, host re-syncs guild/channel lists over HTTP and marks per-channel message windows stale (Â§3.4).
- **Sleep/resume (Windows):** missed heartbeat acks after wake trigger the resume path automatically; no OS power-event handling needed for M1.
- **Single instance:** tauri-plugin-single-instance focuses the existing window.
- **Close = exit** for M1 (tray is a later milestone).

---

## 3. Client SQLite cache

### 3.1 Library â€” decision: rusqlite (`bundled`)

Over sqlx-sqlite because: (a) no async executor machinery or macro infrastructure in the binary â€” smaller and simpler; (b) sqlx's compile-time checking adds a build-time DB dependency for near-zero value on a tiny local schema; (c) the canonical embedded-SQLite pattern is **one dedicated worker thread owning the `Connection`**, with an `mpsc` command channel and oneshot replies â€” serialized writes, no pool, no `Send` contortions. WAL mode, `synchronous=NORMAL`, `foreign_keys=ON`. DB at `app_data_dir()/cache.db` (`%APPDATA%\com.dice.app\cache.db`). Migrations: embedded SQL array keyed off `PRAGMA user_version`.

### 3.2 Schema (v1)

```sql
CREATE TABLE meta      (key TEXT PRIMARY KEY, value TEXT);  -- current_user_id, last_transport, last_channel_id, resume blob
CREATE TABLE users     (id INTEGER PRIMARY KEY, username TEXT NOT NULL, display_name TEXT,
                        avatar_hash TEXT, updated_at INTEGER NOT NULL);
CREATE TABLE guilds    (id INTEGER PRIMARY KEY, name TEXT NOT NULL, icon_hash TEXT,
                        owner_id INTEGER NOT NULL, my_permissions INTEGER NOT NULL, updated_at INTEGER NOT NULL);
CREATE TABLE channels  (id INTEGER PRIMARY KEY, guild_id INTEGER,          -- NULL â‡’ DM channel
                        kind INTEGER NOT NULL,                              -- 0=guild_text, 1=dm
                        name TEXT, position INTEGER, last_message_id INTEGER, updated_at INTEGER NOT NULL);
CREATE INDEX idx_channels_guild ON channels(guild_id);
CREATE TABLE dm_participants (channel_id INTEGER NOT NULL, user_id INTEGER NOT NULL,
                        PRIMARY KEY (channel_id, user_id));
CREATE TABLE members   (guild_id INTEGER NOT NULL, user_id INTEGER NOT NULL, nickname TEXT,
                        PRIMARY KEY (guild_id, user_id));
CREATE TABLE messages  (id INTEGER PRIMARY KEY, channel_id INTEGER NOT NULL, author_id INTEGER NOT NULL,
                        content TEXT NOT NULL, created_at INTEGER NOT NULL, edited_at INTEGER,
                        nonce TEXT, pending INTEGER NOT NULL DEFAULT 0, failed INTEGER NOT NULL DEFAULT 0);
CREATE INDEX idx_messages_channel ON messages(channel_id, id DESC);
CREATE TABLE channel_sync (channel_id INTEGER PRIMARY KEY,
                        oldest_fetched_id INTEGER, newest_synced_id INTEGER,
                        stale INTEGER NOT NULL DEFAULT 0);                  -- contiguous-window cursor
CREATE TABLE read_markers (channel_id INTEGER PRIMARY KEY, last_read_message_id INTEGER);
```

Snowflakes stored as `INTEGER` (i64) â€” fine while the protocol team keeps the top bit clear (flagged as a cross-team invariant). Presence is **never persisted** (ephemeral, webview RAM only).

### 3.3 Write path

Single direction: gateway event â†’ bridge â†’ cache upsert â†’ Tauri event â†’ Solid store. Optimistic sends insert `pending=1, nonce=X`; the echoed `MessageCreate` with the same nonce replaces the row (real id, `pending=0`). Pending rows older than 60 s on startup are marked `failed=1` (UI offers retry).

### 3.4 Sync cursors & gap handling (M1 rule: one contiguous window per channel)

`channel_sync` tracks a single contiguous cached range `[oldest_fetched_id, newest_synced_id]`. While connected/resumed, gateway events extend `newest_synced_id`. If a resume fails (`SessionInvalidated`) or the app was offline, the window *may* have a gap at the top: set `stale=1` for all channels. Stale cached rows are still **displayed instantly** for offline UX, but opening a channel triggers a fetch of the newest page; if its oldest message â‰¤ `newest_synced_id`, the ranges connect (clear `stale`); otherwise reset the window to just the fetched page (old rows below remain for scrollback display but `oldest_fetched_id` resets â€” accepted M1 simplification, avoids multi-range bookkeeping). History pagination (`fetch_messages before=oldest_fetched_id`) extends the window downward.

### 3.5 Startup hydration order

1. Open DB, migrate. 2. `get_bootstrap` serves cache (UI paints). 3. Gateway connects; `Ready` snapshot diff-upserts guilds/channels/users. 4. Per-channel reconcile lazily on open (3.4). Cache-first always; network only reconciles.

---

## 4. SolidJS frontend (`apps/desktop-client/src`)

### 4.1 File tree

```
apps/desktop-client/
â”œâ”€â”€ package.json  vite.config.ts  tsconfig.json  index.html
â””â”€â”€ src/
    â”œâ”€â”€ main.tsx            # render(<App/>); installs gateway dispatcher
    â”œâ”€â”€ App.tsx             # session-gated: <Login/> | <AppShell/>
    â”œâ”€â”€ lib/
    â”‚   â”œâ”€â”€ ipc.ts          # typed invoke/listen wrappers (or tauri-specta generated bindings)
    â”‚   â”œâ”€â”€ types.ts        # bridge DTOs â€” ALL ids are `string`
    â”‚   â””â”€â”€ time.ts         # tiny Intl-based timestamp formatting (no dayjs/moment)
    â”œâ”€â”€ gateway/dispatcher.ts  # single listen() registration â†’ dispatch into stores
    â”œâ”€â”€ stores/
    â”‚   â”œâ”€â”€ session.ts      # createSignal: auth state, current user
    â”‚   â”œâ”€â”€ connection.ts   # createSignal<ConnState>
    â”‚   â”œâ”€â”€ guilds.ts       # createStore: guilds, channels by guild, DM list, selection signals
    â”‚   â”œâ”€â”€ messages.ts     # createStore<Record<channelId, ChannelMessages>> + LRU eviction
    â”‚   â”œâ”€â”€ presence.ts     # Map<userId, Signal<Status>> â€” per-user fine-grained dots
    â”‚   â””â”€â”€ typing.ts       # per-channel Map<userId, expiresAt> + single sweep timer (active channel only)
    â”œâ”€â”€ components/
    â”‚   â”œâ”€â”€ auth/LoginRegister.tsx
    â”‚   â”œâ”€â”€ shell/AppShell.tsx  GuildSidebar.tsx  ChannelList.tsx  DmList.tsx  ConnectionBanner.tsx
    â”‚   â”œâ”€â”€ chat/ChatView.tsx  MessageList.tsx  MessageRow.tsx  Composer.tsx  TypingIndicator.tsx
    â”‚   â””â”€â”€ common/Avatar.tsx  PresenceDot.tsx
    â””â”€â”€ styles/  tokens.css (dark theme custom properties)  + CSS modules per component
```

### 4.2 State management â€” deviation from spec

The product spec says Zustand; Zustand is React-idiom. **Decision: Solid's built-in `createStore`/`createSignal`, no state library at all.** Solid's stores already provide the fine-grained subscription model Zustand approximates, with `reconcile()` for snapshot diffing and `produce()` for surgical event application. nanostores was considered (framework-agnostic) but adds a dependency for strictly less capability than the built-ins. Zero bytes added; this is the idiomatic Solid equivalent and is noted as an explicit spec deviation.

**Avoiding global rerenders:** Solid has no VDOM â€” updates are already per-binding. The design choices that matter:
- `presence.ts` keeps a `Map<userId, Signal>`; a `presence://batch` event sets only the affected signals â†’ only those `<PresenceDot>` text/class bindings update.
- `messages.ts` appends via `setStore(channelId, "messages", produce(arr => arr.push(m)))` â†’ only the list tail materializes; nonce reconcile replaces one element by index.
- `Ready`/resync snapshots applied with `reconcile(data, { key: "id" })` â†’ referential stability, no list re-creation.
- Per-channel message arrays capped at 200 in memory with LRU eviction of non-active channels (older history re-pages from SQLite) â€” bounds webview heap for the <100 MB target.

### 4.3 Message list virtualization

**Decision: `@tanstack/solid-virtual`** (headless, ~12 KB, dynamic measurement for variable-height rows, reverse-scroll friendly). Wiring: bottom-anchored scroll; on `message_create` while pinned-to-bottom, scroll to end; when scrolled up, show a "new messages" pill instead. Top-edge intersection triggers `fetch_messages(before=oldest)`. Documented fallback if reverse-scroll measurement fights us: render-last-100 + "Load older" button (no virtualizer) â€” acceptable M1 escape hatch, swap later.

### 4.4 M1 screens

Login/Register (single card, inline errors); AppShell = guild icon rail â†’ channel list + DM list (presence dots on DM entries) â†’ ChatView (virtualized MessageList, Composer, TypingIndicator bar showing "X is typingâ€¦", read from typing store with 10 s TTL); ConnectionBanner renders `connection` signal (reconnecting/offline). No router library â€” view switching off `session` + selection signals.

**npm deps (complete, deliberately short):** `solid-js` 1.9.x, `@tauri-apps/api` 2.x, `@tanstack/solid-virtual` 3.x. Dev: `vite` 6.x, `vite-plugin-solid` 2.x, `typescript` 5.x, `@tauri-apps/cli` 2.x. No component library, no Tailwind (CSS modules + custom properties = zero runtime), no date lib (`Intl`), no icon font (a handful of inline SVGs).

---

## 5. Build & dev pipeline (Windows 11)

- **Dev loop:** `npm run tauri dev` â†’ vite dev server on `127.0.0.1:1420` (HMR for UI) + `cargo build` + app launch. Rust-side changes rebuild on save via tauri CLI watcher. Backend: either `docker compose up` (full stack) or the monolith bin in dev mode; client endpoints configured via env consumed in Rust (`DICE_API_URL`, `DICE_GATEWAY_QUIC`, `DICE_GATEWAY_WSS`, `DICE_EXTRA_CA`) with localhost defaults baked into debug builds.
- **tauri.conf.json essentials:** `identifier: "com.dice.app"`, `build.devUrl: http://localhost:1420`, `frontendDist: ../dist`, `withGlobalTauri: false`, strict CSP (`default-src 'self'`), bundle targets `["nsis"]`, `webviewInstallMode: downloadBootstrapper` (Win11 ships WebView2; bootstrapper covers stripped images). Icons via `tauri icon app-icon.png`.
- **Cargo profiles (workspace root):** dev â€” `[profile.dev.package."*"] opt-level = 2` (quinn/rustls/SQLite are unusable at opt-level 0; keeps own-crate compile fast). Release â€” `lto = "thin"`, `codegen-units = 1`, `strip = "symbols"`, `opt-level = "s"` for the desktop binary (network throughput is not client-bound); `panic = "abort"` flagged as a try-it (verify Tauri plugin compatibility before committing).
- **Vite:** `target: "esnext"` (WebView2 is evergreen Chromium â€” no polyfills), `sourcemap: false` in release; Tauri devtools feature only in debug builds.
- **RAM budget & measurement:** WebView2 child processes cost ~50â€“70 MB floor; host process budget < 25 MB (tokio + quinn + SQLite page cache is comfortably under this). Measure the full tree: `Get-Process Dice, msedgewebview2 | Measure-Object WorkingSet64 -Sum` after 5-minute idle; gate in docs, not CI, for M1. Idle-CPU levers already designed in: presence coalescing (one 100 ms tick only while events flow), typing sweep only for the focused channel, no UI polling, heartbeat is the only steady-state timer (~1/30 s).

---

## 6. Key decisions and why

1. **Enum dispatch over `dyn Transport`** â€” async-fn-in-trait isn't dyn-compatible; an enum avoids `async-trait` boxing, costs nothing, and there will only ever be 2â€“3 transports.
2. **App-level heartbeat + seq-based resume, identical over both transports** â€” QUIC keep-alives can't carry `last_seq`; one liveness/resume mechanism means WSS isn't a second-class fallback.
3. **HTTPS+protobuf for actions, gateway for push** â€” proven split (Discord-shaped), free retry/idempotency semantics, decouples this team from the gateway team's command handling; envelope reserves frame numbers to move sends onto the gateway later.
4. **rustls `ring` provider everywhere** â€” dodges the aws-lc-sys CMake/NASM Windows build trap; one shared `ClientConfig` across QUIC/WSS/HTTPS makes dev-cert trust a single code path.
5. **Dev trust = extra root anchor, never verification-off** â€” full hostname/chain validation still runs against the pinned dev CA; no footgun flag can leak into release behavior.
6. **rusqlite on a dedicated worker thread** â€” smaller binary than sqlx-sqlite, serialized writes by construction, idiomatic embedded-SQLite shape.
7. **Keyring for refresh token, RAM for access token** â€” OS-grade protection with zero UX cost; encrypted-file alternatives either prompt for passwords or store the key beside the data.
8. **Cache-before-emit bridge ordering** â€” UI can never display state the cache lacks â‡’ offline restarts are always self-consistent.
9. **No state library, no router, three runtime npm deps** â€” directly services the <100 MB RAM / <2 s cold start / <1% idle CPU targets; every dep must justify itself.
10. **IDs as strings across IPC** â€” u64 snowflakes overflow JS numbers; making this a typed convention now prevents a class of silent corruption bugs.

## 7. Risks and open questions

1. **Envelope ownership (cross-team, highest priority):** `ClientFrame`/`ServerFrame` shape, `seq` semantics, resume replay window, and nonce echo on `MessageCreate` must be agreed with the protocol/gateway teams before driver coding starts.
2. **quinn/rustls/tokio-tungstenite/reqwest version alignment** on rustls 0.23 must be verified at `Cargo.lock` time; a mismatched rustls major splits the shared `ClientConfig` design (mitigation: pin all four together, CI check).
3. **Reverse-scroll virtualization** with dynamic heights is the fiddliest UI piece; escape hatch (last-100 + load-older) is pre-approved above.
4. **tauri-specta** maturity vs Tauri 2 point releases â€” if bindings generation breaks, fall back to hand-written `ipc.ts` (small, stable surface: ~12 commands, ~9 events).
5. **`panic = "abort"` + Tauri** interaction unverified â€” try late, not load-bearing.
6. **Snowflake top-bit invariant** for SQLite i64 storage â€” needs a one-line guarantee from the protocol team (or store ids as BLOBs, minor churn).
7. **Cache at rest is plaintext** (message content in `%APPDATA%`) â€” acceptable for M1? If not, SQLCipher via rusqlite's `bundled-sqlcipher` is a contained swap but adds key-management questions (likely keyring-stored key). Open question for the product owner.
8. **QUIC through real-world NATs/firewalls** â€” fallback policy handles it, but the 3 s QUIC probe adds worst-case connect latency on UDP-blocked networks; persisted last-good transport mitigates after first run.
9. **Tauri IPC throughput** for large bootstrap payloads is fine at M1 scale (a few hundred KB JSON), but if guild snapshots grow, move bootstrap to `tauri::ipc::Response` raw bytes (protobuf over IPC) â€” seam already isolated in `commands/bootstrap.rs`.

### Critical Files for Implementation
- D:\Dice\crates\network-core\src\gateway\driver.rs â€” connection state machine, heartbeat/resume, the heart of the crate
- D:\Dice\crates\network-core\src\transport\mod.rs â€” `Transport` trait + `AnyTransport`, QUIC/WSS contract everything else builds on
- D:\Dice\apps\desktop-client\src-tauri\src\bridge.rs â€” gateway-events â†’ cache â†’ webview pump, ordering and coalescing rules
- D:\Dice\apps\desktop-client\src-tauri\src\cache\worker.rs â€” rusqlite worker thread, schema/migrations, sync-cursor logic
- D:\Dice\apps\desktop-client\src\gateway\dispatcher.ts â€” Tauri events â†’ fine-grained Solid store updates