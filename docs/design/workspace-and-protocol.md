> Preserved milestone-1 design document (agent-produced, 2026-06-11).
> Where this conflicts with docs/protocol.md or the critiques' resolutions, those win.

All research is done â€” repo confirmed empty, current crate versions verified against docs.rs (crates.io itself is 503ing from this network, noted in risks). Here is the design document.

---

# Dice â€” Workspace Foundation, Wire Protocol, and Shared Crates (Milestone 1 Design)

## 0. Verified facts this design rests on

- Repo `D:\Dice` is empty; no git repo yet. Machine has Rust 1.81 today (will move to 1.88+/edition 2024 before work starts), Node 22, Docker 29, no protoc.
- Versions verified on docs.rs (2026-06-11): prost **0.14.4**, quinn **0.11.9**, protoc-bin-vendored **3.2.0** (bundles a Windows binary â€” confirmed), sqlx **0.9.0** (brand-new; we pin **0.8.x**, see Risks), async-nats **0.49.x**, redis **1.2.x**, tokio-tungstenite **0.29.x**, axum **0.8.x**, moka **0.12.x**.
- `prost_build::Config::protoc_executable(path)` exists (verified) â€” this is how we point prost at the vendored protoc **without** the edition-2024-unsafe `env::set_var`.

---

## 1. Repository file tree (this work package's area)

```
D:\Dice
â”œâ”€ Cargo.toml                     # workspace root (virtual manifest)
â”œâ”€ Cargo.lock                     # committed
â”œâ”€ rust-toolchain.toml
â”œâ”€ rustfmt.toml
â”œâ”€ justfile                       # task runner (single source of truth)
â”œâ”€ .gitignore
â”œâ”€ .gitattributes
â”œâ”€ .env.example                   # documented env vars; copied to .env (gitignored)
â”œâ”€ README.md
â”œâ”€ .sqlx/                         # COMMITTED sqlx offline query cache
â”œâ”€ proto/
â”‚  â””â”€ dice/
â”‚     â”œâ”€ v1/
â”‚     â”‚  â”œâ”€ common.proto          # User, Guild, Channel, Message, shared enums
â”‚     â”‚  â”œâ”€ envelope.proto        # Frame (the one wire envelope)
â”‚     â”‚  â”œâ”€ gateway.proto         # Hello/Identify/Resume/Ready/Heartbeat/Close/Error
â”‚     â”‚  â”œâ”€ auth.proto            # HTTPS request/response bodies (proto-over-HTTP)
â”‚     â”‚  â”œâ”€ guild.proto           # guild/channel/member events + REST bodies
â”‚     â”‚  â”œâ”€ message.proto         # message events, SendMessageRequest/Ack
â”‚     â”‚  â””â”€ presence.proto        # PresenceUpdate, TypingStart, status enum
â”‚     â””â”€ internal/
â”‚        â””â”€ v1/
â”‚           â””â”€ events.proto       # BusEvent envelope for NATS / in-proc bus
â”œâ”€ crates/
â”‚  â”œâ”€ common/                     # dice-common: ids, time, env config, shutdown
â”‚  â”‚  â””â”€ src/{lib,id,time,config,shutdown}.rs
â”‚  â”œâ”€ protocol/                   # dice-protocol: prost codegen + framing helpers
â”‚  â”‚  â”œâ”€ build.rs
â”‚  â”‚  â””â”€ src/{lib,framing}.rs
â”‚  â”œâ”€ network-core/               # dice-network-core: QUIC/WSS transport abstraction
â”‚  â”‚  â””â”€ src/lib.rs               #   (designed in a sibling work package; member here)
â”‚  â”œâ”€ event-bus/                  # dice-event-bus: trait + NATS + in-proc impls
â”‚  â”‚  â””â”€ src/{lib,subject,nats,local}.rs
â”‚  â”œâ”€ database/                   # dice-database: pool bootstrap + embedded migrations
â”‚  â”‚  â”œâ”€ src/lib.rs
â”‚  â”‚  â””â”€ migrations/              # single linear history (see Â§5.3)
â”‚  â”‚     â”œâ”€ 0001_users.sql
â”‚  â”‚     â”œâ”€ 0002_refresh_tokens.sql
â”‚  â”‚     â”œâ”€ 0003_guilds_channels_members.sql
â”‚  â”‚     â””â”€ 0004_messages.sql
â”‚  â”œâ”€ cache/                      # dice-cache: trait + Redis + moka impls
â”‚  â”‚  â””â”€ src/{lib,redis_impl,memory}.rs
â”‚  â”œâ”€ permissions/                # dice-permissions: bitflags u64 + compute
â”‚  â”‚  â””â”€ src/lib.rs
â”‚  â”œâ”€ auth-core/                  # dice-auth-core: Argon2id, JWT, refresh tokens
â”‚  â”‚  â””â”€ src/{lib,password,token}.rs
â”‚  â”œâ”€ logging/                    # dice-logging: tracing init
â”‚  â”‚  â””â”€ src/lib.rs
â”‚  â””â”€ metrics/                    # dice-metrics: metrics facade + Prometheus exporter
â”‚     â””â”€ src/lib.rs
â”œâ”€ services/
â”‚  â”œâ”€ api-gateway/                # lib + thin bin (src/lib.rs, src/main.rs)
â”‚  â”œâ”€ auth-service/               # lib + thin bin
â”‚  â”œâ”€ chat-service/               # lib + thin bin
â”‚  â”œâ”€ presence-service/           # lib + thin bin
â”‚  â”œâ”€ monolith/                   # bin only: mounts all four service libs
â”‚  â”œâ”€ voice-service/README.md     # placeholder â€” NO Cargo.toml
â”‚  â”œâ”€ media-service/README.md
â”‚  â”œâ”€ notification-service/README.md
â”‚  â””â”€ search-service/README.md
â”œâ”€ apps/
â”‚  â””â”€ desktop-client/             # Tauri 2 + SolidJS; OWN cargo workspace (see Â§2.1)
â”œâ”€ infrastructure/
â”‚  â”œâ”€ docker/
â”‚  â”‚  â””â”€ docker-compose.yml       # postgres:17, redis:7, nats:2.11 (-js)
â”‚  â”œâ”€ kubernetes/README.md        # placeholder
â”‚  â””â”€ terraform/README.md         # placeholder
â”œâ”€ scripts/
â”‚  â”œâ”€ bootstrap.ps1               # cold-start only: install just + sqlx-cli
â”‚  â””â”€ bootstrap.sh
â”œâ”€ docs/
â”‚  â”œâ”€ getting-started.md
â”‚  â”œâ”€ architecture.md
â”‚  â”œâ”€ protocol.md                 # NORMATIVE wire spec (mirrors Â§3 of this doc)
â”‚  â”œâ”€ database.md                 # migrations + sqlx-offline workflow
â”‚  â””â”€ adr/
â”‚     â”œâ”€ 0001-quic-primary-wss-fallback.md
â”‚     â”œâ”€ 0002-runtime-config-not-cargo-features.md
â”‚     â”œâ”€ 0003-postgres-always-required.md
â”‚     â”œâ”€ 0004-snowflake-ids.md
â”‚     â””â”€ 0005-proto-over-http-for-rest.md
â””â”€ benchmarks/README.md           # placeholder
```

---

## 2. Workspace foundation

### 2.1 Root `Cargo.toml`

```toml
[workspace]
resolver = "3"                       # edition-2024 resolver
members = [
  "crates/*",
  "services/api-gateway",
  "services/auth-service",
  "services/chat-service",
  "services/presence-service",
  "services/monolith",
]
# services/* is NOT a glob: placeholder dirs (voice/media/notification/search)
# hold only a README and must not be parsed as packages.
exclude = ["apps/desktop-client/src-tauri"]

[workspace.package]
version = "0.1.0"
edition = "2024"
rust-version = "1.88"
license = "TBD"                      # open question, see Â§8
repository = "https://github.com/TBD/dice"

[workspace.dependencies]
# ---- internal (every crate refs these via { workspace = true }) ----
dice-common      = { path = "crates/common" }
dice-protocol    = { path = "crates/protocol" }
dice-network-core= { path = "crates/network-core" }
dice-event-bus   = { path = "crates/event-bus" }
dice-database    = { path = "crates/database" }
dice-cache       = { path = "crates/cache" }
dice-permissions = { path = "crates/permissions" }
dice-auth-core   = { path = "crates/auth-core" }
dice-logging     = { path = "crates/logging" }
dice-metrics     = { path = "crates/metrics" }

# ---- async runtime ----
tokio        = { version = "1", features = ["rt-multi-thread", "macros", "net", "sync", "time", "signal", "io-util"] }
tokio-util   = { version = "0.7", features = ["rt"] }     # CancellationToken
futures-util = "0.3"
bytes        = "1"
async-trait  = "0.1"

# ---- transport (ALL pinned to the *ring* crypto provider â€” see Risks: aws-lc-rs needs NASM/CMake on Windows) ----
quinn             = { version = "0.11", default-features = false, features = ["rustls-ring", "runtime-tokio", "log"] }
rustls            = { version = "0.23", default-features = false, features = ["ring", "std", "logging"] }
tokio-rustls      = { version = "0.26", default-features = false, features = ["ring"] }
rcgen             = "0.13"            # dev-mode self-signed certs, generated in-process
tokio-tungstenite = "0.29"            # server accepts on an already-TLS stream; no TLS features
axum              = { version = "0.8", features = ["http1"] }
tower-http        = { version = "0.6", features = ["trace", "limit"] }

# ---- protocol ----
prost       = "0.14"
prost-build = "0.14"                  # [build-dependencies] in dice-protocol
protoc-bin-vendored = "3"             # [build-dependencies]; bundles protoc.exe for Windows

# ---- data ----
sqlx  = { version = "0.8", default-features = false, features = ["runtime-tokio", "tls-rustls-ring", "postgres", "macros", "migrate", "time"] }
redis = { version = "1", features = ["tokio-comp", "connection-manager"] }
moka  = { version = "0.12", features = ["future"] }
async-nats = "0.49"

# ---- auth ----
argon2       = { version = "0.5", features = ["std"] }
password-hash= { version = "0.5", features = ["getrandom"] }  # OsRng for salts; avoids rand-version coupling
jsonwebtoken = "9"                    # EdDSA (Ed25519) keys
rand         = "0.9"
sha2         = "0.10"
subtle       = "2"
base64       = "0.22"

# ---- observability ----
tracing            = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter", "fmt", "json"] }
metrics            = "0.24"
metrics-exporter-prometheus = { version = "0.17", default-features = false, features = ["http-listener"] }

# ---- misc ----
thiserror = "2"
anyhow    = "1"
bitflags  = "2"
time      = { version = "0.3", features = ["formatting", "macros"] }
serde     = { version = "1", features = ["derive"] }   # config + JWT claims ONLY; never on realtime path
dashmap   = "6"
parking_lot = "0.12"

[workspace.lints.rust]
unsafe_code      = "deny"
unused_must_use  = "deny"
rust_2018_idioms = { level = "warn", priority = -1 }

[workspace.lints.clippy]
all          = { level = "warn", priority = -1 }
unwrap_used  = "warn"        # forces explicit error handling on the server
dbg_macro    = "warn"
todo         = "warn"
large_futures = "warn"       # guards per-connection memory (100k-conn target)

# ---- profiles (perf targets: see Â§7 decisions) ----
[profile.dev]
debug = "line-tables-only"   # faster link, smaller target/

[profile.dev.package."*"]    # deps at -O2: ring/quinn/argon2 are unusable at -O0
opt-level = 2

[profile.release]
opt-level     = 3
lto           = "thin"
codegen-units = 1
strip         = "symbols"
panic         = "unwind"     # deliberate: panic=abort would let one buggy task kill
                             # 100k connections; tokio isolates unwinding task panics
```

Every member crate uses `[lints] workspace = true` and `edition.workspace = true`, `version.workspace = true`.

**Desktop client is OUT of this workspace** (`apps/desktop-client/src-tauri` has its own `[workspace]` table + own lockfile, and is `exclude`d above). Reasons: `cargo check --workspace` on the backend must not compile Tauri/WebView2/windows-rs (huge dev-loop cost); the client needs different profiles (size-optimized); it shares code via plain path deps on `crates/protocol` and `crates/common`, which work fine across workspaces.

### 2.2 `rust-toolchain.toml`, `rustfmt.toml`, `.gitignore`, `.gitattributes`

```toml
# rust-toolchain.toml â€” pin EXACT version; bump via PR so .sqlx/lockfile/CI move together
[toolchain]
channel    = "1.88.0"
components = ["rustfmt", "clippy"]
```

```toml
# rustfmt.toml â€” stable options only
edition = "2024"
newline_style = "Unix"
```

```gitignore
# .gitignore
/target/
**/node_modules/
apps/desktop-client/dist/
apps/desktop-client/src-tauri/target/
.env
*.local
.DS_Store
# NOTE: .sqlx/ and Cargo.lock are COMMITTED â€” never add them here
```

```gitattributes
# .gitattributes â€” must be the FIRST commit (Windows dev machine!)
*            text=auto eol=lf
*.ps1        text eol=crlf
*.cmd        text eol=crlf
*.png        binary
```

### 2.3 Task runner: **justfile** (chosen over PowerShell+bash pairs)

Why `just`: one source of truth instead of maintaining `.ps1`/`.sh` mirrors that drift; trivially installed (`cargo install just` or `winget install Casey.Just`); `set windows-shell` makes recipes run natively under PowerShell on this machine and under sh on CI/Linux; `just --list` is self-documenting. The only raw scripts kept are `scripts/bootstrap.ps1|.sh`, which exist solely to install `just` + `sqlx-cli` on a cold machine.

```just
set windows-shell := ["powershell.exe", "-NoLogo", "-Command"]

default:        # just --list
    @just --list

infra-up:       # Postgres + Redis + NATS
    docker compose -f infrastructure/docker/docker-compose.yml up -d
infra-down:
    docker compose -f infrastructure/docker/docker-compose.yml down

db-setup:       # create DB + run migrations (needs infra-up)
    sqlx database create --database-url $env:DATABASE_URL
    sqlx migrate run --source crates/database/migrations --database-url $env:DATABASE_URL
db-reset:
    sqlx database reset -y --source crates/database/migrations --database-url $env:DATABASE_URL

sqlx-prepare:   # regenerate committed .sqlx cache after query/migration changes
    cargo sqlx prepare --workspace -- --all-targets
sqlx-check:     # CI: fail if cache is stale
    cargo sqlx prepare --check --workspace -- --all-targets

check:          # the PR gate
    cargo fmt --all --check
    cargo clippy --workspace --all-targets -- -D warnings
    cargo test --workspace

dev:            # fast iteration: monolith, in-proc bus, memory cache, docker PG only
    $env:DICE_PROFILE = "dev-lite"; cargo run -p dice-monolith
run-full:       # monolith against full docker infra
    $env:DICE_PROFILE = "full"; cargo run -p dice-monolith

client:         # desktop client dev loop (own workspace)
    cd apps/desktop-client; npm run tauri dev
```

### 2.4 Repo bootstrap (ordered, day one)

1. `git init -b main` in `D:\Dice`; commit `.gitattributes` + `.gitignore` **first** (line-ending hygiene before any code lands).
2. Lay down root `Cargo.toml`, `rust-toolchain.toml`, `rustfmt.toml`, `justfile`, `.env.example`, `README.md`, `docs/`, `proto/`, empty placeholder READMEs. Commit.
3. `rustup show` (toolchain file auto-installs 1.88.0). Run `scripts/bootstrap.ps1` â†’ `cargo install just sqlx-cli --locked` (sqlx-cli pinned to 0.8.x to match the workspace sqlx minor).
4. Scaffold crates (`cargo new --lib crates/common` etc.), commit per logical unit; `Cargo.lock` committed from the start.
5. `just infra-up && just db-setup && just sqlx-prepare` â†’ commit `.sqlx/`.
6. `just check` green â†’ tag `scaffold-complete`.

### 2.5 docs/ at scaffold time

- `getting-started.md` â€” the Â§2.4 steps for new contributors, incl. "no DB? builds still work, see database.md".
- `protocol.md` â€” **normative** wire spec (envelope, framing, handshake, heartbeats, resume, close-code table). Clients implement against this doc, not against the Rust code.
- `architecture.md` â€” crate/service dependency diagram, monolith vs split-bin deploy shapes.
- `database.md` â€” migration workflow, `.sqlx` regeneration rules ("you touched a query â†’ run `just sqlx-prepare` â†’ commit the diff").
- `adr/0001..0005` â€” one page each for the five decisions in Â§7, so future contributors don't relitigate.

---

## 3. Wire protocol

### 3.1 proto/ organization & versioning

- Package `dice.v1` = public clientâ†”server protocol. `dice.internal.v1` = serviceâ†”service bus envelope (never sent to clients).
- **Compatibility rules (enforced by review, optionally `buf breaking` in CI later):** field numbers are never reused or renumbered; removed fields get `reserved`; additions only; every enum has `*_UNSPECIFIED = 0`; prost enums are open (i32 on the wire) â€” code must route unknown values to a safe default. Breaking change â‡’ new package `dice.v2`, new ALPN `dice/2`, old package stays compiled in parallel.
- Fine-grained negotiation inside v1: `Identify.capabilities` u64 bitfield (e.g. bit 0 = "accepts typing via QUIC datagram").

### 3.2 The envelope â€” `proto/dice/v1/envelope.proto`

One envelope for both transports. `oneof` (not opcode+bytes): type-safe in prost, single decode pass, and unknown payloads decode as `payload: None` which is exactly the forward-compat hook we need.

```proto
syntax = "proto3";
package dice.v1;
import "dice/v1/gateway.proto";
import "dice/v1/message.proto";
import "dice/v1/guild.proto";
import "dice/v1/presence.proto";

message Frame {
  // Monotonic per-session sequence, assigned by the gateway, >0 ONLY on
  // sequenced server->client dispatches (field numbers 100+). 0 otherwise.
  uint64 seq = 1;
  // Client-chosen correlation id on requests (30-49); echoed on replies
  // (50-69) and on request-scoped Error frames. 0 = not a request/reply.
  uint64 nonce = 2;

  oneof payload {
    // -- lifecycle / control (10-29): never sequenced, never replayed --
    Hello         hello          = 10;
    Identify      identify       = 11;
    Resume        resume         = 12;
    Ready         ready          = 13;
    Resumed       resumed        = 14;
    Heartbeat     heartbeat      = 15;
    HeartbeatAck  heartbeat_ack  = 16;
    Close         close          = 17;
    Error         error          = 18;
    // -- client -> server requests (30-49) --
    SendMessageRequest    send_message    = 30;
    StartTypingRequest    start_typing    = 31;  // Class B (lossy ok)
    UpdatePresenceRequest update_presence = 32;
    // -- server -> client replies (50-69) --
    SendMessageAck send_message_ack = 50;
    // -- sequenced dispatch events (100+): replayed on Resume --
    MessageCreate     message_create   = 100;
    MessageUpdate     message_update   = 101;
    MessageDelete     message_delete   = 102;
    TypingStart       typing_start     = 103;  // exception: Class B, seq=0
    PresenceUpdate    presence_update  = 104;
    GuildCreate       guild_create     = 105;
    GuildUpdate       guild_update     = 106;
    GuildDelete       guild_delete     = 107;
    ChannelCreate     channel_create   = 108;
    ChannelUpdate     channel_update   = 109;
    ChannelDelete     channel_delete   = 110;
    GuildMemberAdd    member_add       = 111;
    GuildMemberRemove member_remove    = 112;
    DmChannelCreate   dm_channel_create= 113;
  }
}
```

Receiver policy for `payload: None` (newer peer sent something we don't know): if `seq > 0` â†’ ignore but **still advance ack** (it was a dispatch we can't render); if it carried a `nonce` â†’ reply `Error{UNKNOWN_OPCODE}`; otherwise ignore.

### 3.3 Message classes & framing

| Class | Delivery | seq | Resume replay | QUIC carrier | WSS carrier |
|---|---|---|---|---|---|
| Control (10-69) | reliable, ordered | 0 | no | the control stream | binary WS message |
| A: Dispatch (100+ except typing) | reliable, ordered | per-session, monotonic from 1 | yes | the control stream | binary WS message |
| B: Typing (31, 103) | lossy, unordered OK | 0 | no | QUIC DATAGRAM (fallback: control stream) | binary WS message (reliable is harmless) |

**QUIC** (ALPN `dice/1`):
- Client connects, then opens **one bidirectional "control" stream** for the whole session. Frames on it are length-prefixed: `u32 big-endian length || Frame bytes`, hard cap `MAX_FRAME_BYTES = 256 KiB` (checked *before* allocation; violation â‡’ `Close{PAYLOAD_TOO_LARGE}`).
- One ordered stream (not stream-per-message) because resume requires a total order per session, and per-stream overhead per chat message wastes the per-connection memory budget. Head-of-line blocking within one user's event feed is acceptable; v2 can add per-guild streams behind a capability bit.
- Datagrams carry a bare `Frame` (no prefix â€” datagrams self-delimit). If the peer doesn't negotiate datagram support, Class B falls back to the control stream.
- quinn transport tuning toward 100k conns/node: `max_concurrent_bidi_streams = 4`, stream receive window 1 MiB, connection window 4 MiB, datagram receive buffer 64 KiB, QUIC keep-alive **off** (the app heartbeat is the keep-alive), idle timeout = 2.5 Ã— heartbeat interval. **0-RTT disabled** (an Identify token in 0-RTT data is replayable).

**WSS** (`wss://host/gateway/v1`, TLS 1.3 via tokio-rustls, upgrade via tokio-tungstenite):
- Each `Frame` = exactly one **binary** WS message, no length prefix (WS frames self-delimit). Text frames â‡’ protocol-error close. Max message size 256 KiB via tungstenite config. `permessage-deflate` **off** (per-connection CPU/window memory vs an already-compact binary protocol).
- Identical state machine above the transport; this is the contract `network-core` implements behind its transport trait.

### 3.4 Handshake, heartbeat, resume â€” `gateway.proto`

```proto
message Hello {
  uint32 heartbeat_interval_ms = 1;  // e.g. 30000; client jitters Â±10%
  uint32 resume_window_ms      = 2;  // e.g. 60000
  uint32 max_frame_bytes       = 3;  // 262144
}
message ClientProperties { string client = 1; string version = 2; string os = 3; }
message Identify {
  string access_token       = 1;     // JWT from auth REST
  ClientProperties properties = 2;
  uint64 capabilities       = 3;
  uint32 protocol_version   = 4;     // minor; major is in ALPN/URL
}
message Resume  { fixed64 session_id = 1; bytes resume_token = 2; uint64 last_seq = 3; }
message Ready {
  fixed64 session_id   = 1;
  bytes   resume_token = 2;          // 16 random bytes, bound to session
  User    user         = 3;
  repeated Guild   guilds      = 4;  // full snapshot incl. channels (m1 scale)
  repeated Channel dm_channels = 5;
  repeated PresenceUpdate presences = 6;  // snapshot for visible users
}
message Resumed { }
message Heartbeat    { uint64 last_seq = 1; uint64 client_time_ms = 2; }
message HeartbeatAck { uint64 client_time_ms = 1; }   // echo -> client RTT
message Close { uint32 code = 1; string reason = 2; } // code table below
message Error { ErrorCode code = 1; string message = 2; uint32 retry_after_ms = 3; }
enum ErrorCode {
  ERROR_CODE_UNSPECIFIED = 0;  UNAUTHENTICATED = 1; INVALID_SESSION = 2;
  RATE_LIMITED = 3; PERMISSION_DENIED = 4; NOT_FOUND = 5;
  INVALID_ARGUMENT = 6; UNKNOWN_OPCODE = 7; PAYLOAD_TOO_LARGE = 8; INTERNAL = 9;
}
```

Sequence: connect â†’ server sends `Hello` â†’ client must send `Identify` **or** `Resume` within 5 s (else close) â†’ server replies `Ready` (fresh session) or `Resumed` + replays buffered frames with `seq > last_seq` (resume) or `Close{INVALID_SESSION}` (client falls back to full `Identify`).

- `Heartbeat.last_seq` is a **cumulative ack**: the gateway trims the session's replay buffer to `seq > last_acked` and refreshes the presence TTL. Server drops the connection after 2 missed heartbeats; client reconnects-and-resumes after 2 missing acks.
- Replay buffer per session: ring buffer bounded by **1024 frames AND 512 KiB** (whichever first), retained `resume_window_ms` after disconnect. Overflow/expiry â‡’ resume fails â‡’ full re-Identify + fresh `Ready` snapshot. Buffer lives on the gateway node; cross-node resume is out of scope for m1 (documented as INVALID_SESSION).
- Close-code table (same numeric space for WS close codes and the QUIC application close code): `4000+ErrorCode` â€” e.g. 4001 UNAUTHENTICATED, 4002 INVALID_SESSION, 4003 RATE_LIMITED, 4008 PAYLOAD_TOO_LARGE.

### 3.5 Entity + event sketches (`common.proto`, `message.proto`, `guild.proto`, `presence.proto`, `auth.proto`)

All snowflakes are `fixed64`, not `uint64`: snowflake high bits are the timestamp, so varint would cost 9â€“10 bytes; fixed64 is always 8.

```proto
// common.proto
message User    { fixed64 id = 1; string username = 2; string display_name = 3; uint32 flags = 4; }
enum ChannelKind { CHANNEL_KIND_UNSPECIFIED = 0; GUILD_TEXT = 1; DM = 2; }
message Channel { fixed64 id = 1; fixed64 guild_id = 2; /* 0 for DM */ ChannelKind kind = 3;
                  string name = 4; uint32 position = 5; fixed64 last_message_id = 6;
                  repeated fixed64 recipient_ids = 7; /* DM only */ }
message Guild   { fixed64 id = 1; string name = 2; fixed64 owner_id = 3; repeated Channel channels = 4; }
message Member  { fixed64 user_id = 1; fixed64 guild_id = 2; uint64 joined_at_ms = 3; uint64 permissions = 4; }
message Message { fixed64 id = 1; fixed64 channel_id = 2; fixed64 author_id = 3;
                  string content = 4; uint64 edited_at_ms = 5; }
                  // created_at derives from the snowflake â€” not duplicated on the wire

// message.proto â€” send path goes over the gateway (binary, low latency)
message SendMessageRequest { fixed64 channel_id = 1; string content = 2; }  // nonce in Frame
message SendMessageAck     { Message message = 1; }    // echoed nonce correlates; dispatch follows
message MessageCreate      { Message message = 1; }
message MessageUpdate      { Message message = 1; }
message MessageDelete      { fixed64 channel_id = 1; fixed64 message_id = 2; }

// presence.proto
enum PresenceStatus { PRESENCE_STATUS_UNSPECIFIED = 0; ONLINE = 1; IDLE = 2; DND = 3; OFFLINE = 4; }
message PresenceUpdate        { fixed64 user_id = 1; PresenceStatus status = 2; uint64 since_ms = 3; }
message UpdatePresenceRequest { PresenceStatus status = 1; }
message StartTypingRequest    { fixed64 channel_id = 1; }
message TypingStart           { fixed64 channel_id = 1; fixed64 user_id = 2; }

// guild.proto â€” events (GuildCreate{Guild}, ChannelCreate{Channel}, GuildMemberAdd{Member, User}, ...)
// plus REST bodies: CreateGuildRequest{name}, CreateChannelRequest{guild_id,name},
// CreateInviteRequest/JoinGuildRequest{code}, OpenDmRequest{recipient_id}

// auth.proto â€” HTTPS bodies, content-type: application/x-protobuf (see ADR-0005)
message RegisterRequest { string email = 1; string username = 2; string password = 3; }
message LoginRequest    { string email = 1; string password = 2; }
message AuthSuccess     { string access_token = 1; string refresh_token = 2;
                          uint64 access_expires_in_s = 3; User user = 4; }
message RefreshRequest  { string refresh_token = 1; }
message LogoutRequest   { string refresh_token = 1; }
```

Split of responsibilities: **gateway** carries high-rate realtime ops (send message, typing, presence) â€” binary end to end; **HTTPS REST** (axum on api-gateway) carries request/response management ops (register/login/refresh, create guild/channel/invite, open DM, history backfill `GET /channels/{id}/messages?before=`) with protobuf bodies, so there is exactly one schema language in the system.

```proto
// internal/v1/events.proto â€” what travels on NATS / the in-proc bus
message BusEvent {
  fixed64 event_id      = 1;   // snowflake; consumer idempotency key
  uint64  emitted_at_ms = 2;
  string  origin        = 3;   // "chat-service@node0"
  // routing hints so gateways fan out without decoding the payload deeply:
  fixed64 guild_id      = 4;   // 0 if not guild-scoped
  repeated fixed64 recipient_user_ids = 5;  // DM / user-targeted
  dice.v1.Frame frame   = 6;   // dispatch payload, seq=0; gateway assigns
                               // per-session seq at delivery time
}
```

### 3.6 Snowflake IDs

- 64-bit, **bit 63 always 0** so every ID fits Postgres `BIGINT` and JS `BigInt` without sign games: `[1 bit 0][41 bits ms since DICE_EPOCH][10 bits node id][12 bits sequence]`.
- `DICE_EPOCH_MS = 1_767_225_600_000` (2026-01-01T00:00:00Z); 41 bits of ms â‰ˆ 69 years â†’ good to ~2095.
- Generated **server-side only**, at the entity's owning service: auth-service â†’ user IDs; chat-service â†’ guild/channel/message IDs; gateway â†’ session IDs; event-bus â†’ event IDs. Clients never mint IDs (they use the `nonce` field for echo correlation).
- Node ID from `DICE_NODE_ID` env (0â€“1023, default 0; the monolith uses one generator). Generator lives in `dice-common`: lock-free CAS on a single `AtomicU64` packed as `(timestamp << 12) | seq`; 12-bit seq overflow spins to the next millisecond; clock regression waits (never goes backward).
- Wire: `fixed64`. DB: `BIGINT`. Human/REST string form: decimal string.

---

## 4. `dice-protocol` crate (prost codegen)

**Vendored protoc**: current practice confirmed â€” `prost-build` has not bundled/compiled protoc since 0.11; the standard solution is **`protoc-bin-vendored` 3.x**, which ships prebuilt `protoc` binaries including Windows (verified: win32 binary present, runs fine on x64 via WOW64). `protobuf-src` (compile-from-source) is rejected: needs cmake on Windows. Crucially we use `Config::protoc_executable(...)` (verified to exist) instead of `env::set_var("PROTOC", ...)`, because `set_var` is `unsafe` in edition 2024 and we deny `unsafe_code`.

```rust
// crates/protocol/build.rs
fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("cargo:rerun-if-changed=../../proto");
    let protos = [
        "../../proto/dice/v1/common.proto",   "../../proto/dice/v1/envelope.proto",
        "../../proto/dice/v1/gateway.proto",  "../../proto/dice/v1/auth.proto",
        "../../proto/dice/v1/guild.proto",    "../../proto/dice/v1/message.proto",
        "../../proto/dice/v1/presence.proto", "../../proto/dice/internal/v1/events.proto",
    ];
    prost_build::Config::new()
        .protoc_executable(protoc_bin_vendored::protoc_bin_path()?)
        .bytes(["."])                       // bytes fields decode as bytes::Bytes (zero-copy)
        .compile_protos(&protos, &["../../proto"])?;
    Ok(())
}
```

```rust
// crates/protocol/src/lib.rs
pub mod v1 { include!(concat!(env!("OUT_DIR"), "/dice.v1.rs")); }
pub mod internal { pub mod v1 { include!(concat!(env!("OUT_DIR"), "/dice.internal.v1.rs")); } }
pub use prost;                                  // consumers never pin prost separately
pub const PROTOCOL_VERSION: u32 = 1;
pub const ALPN_GATEWAY: &[u8] = b"dice/1";
pub const MAX_FRAME_BYTES: usize = 256 * 1024;

pub enum FrameClass { Control, Sequenced, Unsequenced }
impl v1::Frame {
    pub fn class(&self) -> FrameClass;          // typing => Unsequenced; 100+ => Sequenced
    pub fn dispatch(payload: v1::frame::Payload) -> Self;   // seq filled by gateway
}

// crates/protocol/src/framing.rs â€” shared by network-core (server) and the Tauri client
pub fn encode_frame(frame: &v1::Frame, dst: &mut bytes::BytesMut) -> Result<(), FrameError>; // u32 BE prefix + cap
pub fn decode_frame_bare(buf: &[u8]) -> Result<v1::Frame, FrameError>;  // WS messages & QUIC datagrams
pub struct FrameDecoder { /* bounded accumulator for the QUIC control stream */ }
impl FrameDecoder {
    pub fn extend(&mut self, chunk: &[u8]) -> Result<(), FrameError>;   // PAYLOAD_TOO_LARGE before alloc
    pub fn next(&mut self) -> Result<Option<v1::Frame>, FrameError>;
}
```

Generated code is **not** committed (built into `OUT_DIR` every build; deterministic given the pinned protoc). Versioning: new `dice.v2` â‡’ new `pub mod v2` alongside `v1`; both compiled during any migration window.

**Crate deps**: prost 0.14, bytes 1, thiserror 2; build-deps: prost-build 0.14, protoc-bin-vendored 3. Deliberately **no tokio dependency** â€” the desktop client links this crate too.

---

## 5. Shared crates: responsibilities & public APIs

Package names are `dice-*`; directories are unprefixed. Trait-object crates use `async-trait` (dyn-compatible, needed for runtime backend selection â€” see Â§6).

### 5.1 `dice-common` â€” deps: thiserror, time, tokio-util (CancellationToken), serde (ids only)

```rust
pub mod id {
    #[derive(Copy, Clone, Eq, PartialEq, Ord, Hash)]
    pub struct Snowflake(pub u64);            // Display/FromStr = decimal string
    impl Snowflake { pub fn timestamp_ms(self) -> u64; pub fn as_i64(self) -> i64; /* DB */ }
    // typed-id macro: UserId, GuildId, ChannelId, MessageId, SessionId, EventId
    pub struct SnowflakeGenerator { /* node: u16, AtomicU64 (ts<<12|seq) */ }
    impl SnowflakeGenerator { pub fn new(node_id: u16) -> Self; pub fn generate(&self) -> Snowflake; }
}
pub mod time   { pub const DICE_EPOCH_MS: u64 = 1_767_225_600_000; pub fn now_ms() -> u64; }
pub mod config { // tiny typed env loader; no figment/config-crate dependency
    pub fn env_or<T: FromStr>(key: &str, default: T) -> T;
    pub fn require(key: &str) -> Result<String, ConfigError>;
    pub enum DiceProfile { DevLite, Full }     // parsed from DICE_PROFILE
}
pub mod shutdown { pub use tokio_util::sync::CancellationToken; /* + graceful drain helper */ }
```

### 5.2 `dice-event-bus` â€” deps: async-nats 0.49, tokio, prost, dice-protocol, dice-common, async-trait

```rust
pub enum BusConfig { Local { capacity: usize /* 4096 */ }, Nats { url: String } }
pub async fn connect(cfg: BusConfig) -> Result<Arc<dyn EventBus>, BusError>;

#[async_trait]
pub trait EventBus: Send + Sync {
    async fn publish(&self, subject: Subject, event: BusEvent) -> Result<(), BusError>;
    async fn subscribe(&self, filter: SubjectFilter) -> Result<BusSubscription, BusError>;
}
pub struct BusSubscription { /* impl futures_util::Stream<Item = BusEvent>; bounded */ }

// subject.rs â€” typed builders so subjects can't be fat-fingered:
pub enum Subject {
    GuildEvents(GuildId),     // "dice.evt.guild.{id}"      messages/channels/members
    UserEvents(UserId),       // "dice.evt.user.{id}"       DM delivery, user-targeted
    Presence(UserId),         // "dice.evt.presence.{id}"   high churn, isolated
}
pub enum SubjectFilter { Guild(GuildId), AnyGuild, User(UserId), Presence(UserId), All /* dice.evt.> */ }
```

- **NATS impl**: core (non-JetStream) pub/sub for gateway fan-out â€” at-most-once is correct here because client gap-recovery is the resume buffer + REST history backfill, and per-user durable consumers would sink the 100k-conn memory budget. A JetStream stream `DICE_EVT` capturing `dice.evt.>` (max_age ~10 min, limits-based) is **created** in m1 so future durable consumers (notification/search) can attach without a protocol change, but nothing consumes it yet.
- **Local impl**: one `tokio::sync::broadcast::<BusEvent>` channel; subscriptions wrap a receiver + `SubjectFilter` predicate. On `Lagged(n)`: increment `dice_bus_dropped_events_total` and continue â€” same loss semantics the resume machinery already tolerates. Payloads are `BusEvent` either way, so services can't tell which bus they're on.

### 5.3 `dice-database` â€” deps: sqlx 0.8 (runtime-tokio, tls-rustls-ring, postgres, macros, migrate, time)

```rust
pub use sqlx::PgPool;
pub struct DbConfig { pub url: String, pub max_connections: u32 /* 16 dev / sized prod */,
                      pub acquire_timeout: Duration }
pub async fn connect(cfg: &DbConfig) -> Result<PgPool, sqlx::Error>;   // PgPoolOptions
pub async fn migrate(pool: &PgPool) -> Result<(), MigrateError>;       // sqlx::migrate!("./migrations") â€” embedded
pub static MIGRATOR: sqlx::migrate::Migrator;                          // for tests/monolith startup
```

- **One database, one linear migration history** in `crates/database/migrations/`, embedded via `sqlx::migrate!` so the monolith self-migrates on boot (the self-hosting story: one binary + one Postgres). Files are namespaced by owning service (`0001_users.sql`, `0003_guilds_channels_members.sql`); services keep their *queries* (the `sqlx::query!` macros) in their own crates.
- **sqlx compile-time checking without a running DB â€” DECIDED: committed offline cache.** `.sqlx/` lives at the workspace root and is committed. Developers who change SQL run `just sqlx-prepare` (`cargo sqlx prepare --workspace -- --all-targets`) against docker Postgres and commit the diff; everyone else (and CI) builds with `SQLX_OFFLINE=true` and never needs a database. CI runs `just sqlx-check` to fail stale caches. Rejected alternatives: `query_unchecked` (discards the headline feature), per-developer live DB requirement (breaks "clone â†’ cargo check" and CI hermeticity).

### 5.4 `dice-cache` â€” deps: redis 1.x (tokio-comp, connection-manager), moka 0.12 (future), bytes, async-trait

```rust
pub enum CacheConfig { Memory, Redis { url: String } }
pub async fn connect(cfg: CacheConfig) -> Result<Arc<dyn Cache>, CacheError>;

#[async_trait]
pub trait Cache: Send + Sync {
    async fn get(&self, key: &str) -> Result<Option<Bytes>, CacheError>;
    async fn set(&self, key: &str, value: Bytes, ttl: Option<Duration>) -> Result<(), CacheError>;
    async fn delete(&self, key: &str) -> Result<(), CacheError>;
    async fn incr_expire(&self, key: &str, ttl: Duration) -> Result<i64, CacheError>; // fixed-window rate limit
}
```

- Redis impl: `redis::aio::ConnectionManager` (auto-reconnect); `incr_expire` = `INCR`+`EXPIRE NX` pipeline. Memory impl: `moka::future::Cache<String, (Bytes, expiry)>` with per-entry TTL via moka's `Expiry` policy.
- Documented key conventions (values are protobuf bytes, never JSON): `presence:{user_id}` â†’ `dice.v1.PresenceUpdate`, TTL = 3 Ã— heartbeat interval (presence dots die naturally when heartbeats stop); `rl:{scope}:{principal}:{window}` for rate limits. **Refresh tokens are NOT cached** â€” revocation correctness lives in Postgres only.

### 5.5 `dice-permissions` â€” deps: bitflags 2 (no async, no IO)

```rust
bitflags::bitflags! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct Permissions: u64 {
        const VIEW_CHANNEL    = 1 << 0;  const SEND_MESSAGES  = 1 << 1;
        const MANAGE_MESSAGES = 1 << 2;  const MANAGE_CHANNELS= 1 << 3;
        const MANAGE_GUILD    = 1 << 4;  const KICK_MEMBERS   = 1 << 5;
        const BAN_MEMBERS     = 1 << 6;  const CREATE_INVITE  = 1 << 7;
        const MANAGE_ROLES    = 1 << 8;  const ADMINISTRATOR  = 1 << 63;
    }
}
pub const DEFAULT_EVERYONE: Permissions = /* VIEW_CHANNEL | SEND_MESSAGES | CREATE_INVITE */;
pub fn compute(is_owner: bool, grants: impl IntoIterator<Item = Permissions>) -> Permissions;
   // owner or ADMINISTRATOR => Permissions::all(); signature is forward-proof for roles/overwrites
impl Permissions {
    pub fn check(self, required: Permissions) -> Result<(), MissingPermissions>;
    pub fn from_db(v: i64) -> Self;  pub fn to_db(self) -> i64;   // BIGINT round-trip
}
```

### 5.6 `dice-auth-core` â€” deps: argon2 0.5, password-hash 0.5 (getrandom), jsonwebtoken 9, sha2, subtle, base64, rand, serde, dice-common

```rust
pub mod password {
    // Argon2id v=19, m=19456 KiB (19 MiB), t=2, p=1  (OWASP cheat-sheet params)
    // PHC string output ($argon2id$...). CPU-bound: callers MUST wrap in spawn_blocking.
    pub fn hash(password: &str) -> Result<String, PasswordError>;
    pub fn verify(password: &str, phc: &str) -> Result<bool, PasswordError>;
}
pub mod token {
    pub struct JwtKeys { /* jsonwebtoken Encoding/DecodingKey, alg = EdDSA (Ed25519) */ }
    impl JwtKeys {
        pub fn from_pem(private: &[u8], public: &[u8]) -> Result<Self, KeyError>;
        pub fn generate_ephemeral() -> Self;   // dev-lite: random keypair per boot
        pub fn verify_only(public: &[u8]) -> Result<Self, KeyError>;  // gateway never signs
    }
    #[derive(Serialize, Deserialize)]
    pub struct AccessClaims { pub sub: UserId, pub iat: u64, pub exp: u64 }  // ttl 600 s
    pub fn sign_access(keys: &JwtKeys, user: UserId) -> Result<String, TokenError>;
    pub fn verify_access(keys: &JwtKeys, jwt: &str) -> Result<AccessClaims, TokenError>;

    // Opaque rotating refresh tokens: client sees "drt_<base64url(32 bytes)>";
    // server stores only SHA-256(token). Constant-time lookup compare via `subtle`.
    pub fn mint_refresh() -> (String, [u8; 32]);
    pub fn hash_refresh(presented: &str) -> Option<[u8; 32]>;
}
```

Ed25519 (not HS256) from day one so split-service deploys hand the gateway a **public** key only. Rotation model auth-service implements (schema in `0002_refresh_tokens.sql`): `refresh_tokens(id BIGINT PK, user_id, token_hash BYTEA UNIQUE, family_id BIGINT, expires_at /* 30 d */, used_at, revoked_at)`; refresh marks the row used and mints a child in the same family; presenting an already-used token revokes the whole family (theft detection).

### 5.7 `dice-logging` + `dice-metrics` (thin by design)

```rust
// dice-logging â€” deps: tracing, tracing-subscriber (env-filter, fmt, json)
pub enum LogFormat { Pretty, Json }
pub struct LogConfig { pub filter: String /* "info,dice=debug" */, pub format: LogFormat }
pub fn init(cfg: &LogConfig);   // EnvFilter (RUST_LOG overrides) + fmt layer; Pretty in dev-lite, Json otherwise

// dice-metrics â€” deps: metrics 0.24, metrics-exporter-prometheus 0.17 (http-listener only)
pub use metrics::{counter, gauge, histogram};   // services import macros from HERE only
pub fn init_prometheus(bind: SocketAddr) -> Result<(), ExporterError>;  // GET /metrics
// naming convention enforced by review: dice_{service}_{name}_{unit}
// m1 minimum: dice_gateway_connections{transport}, dice_gateway_frames_total{dir,class},
//             dice_bus_dropped_events_total, dice_chat_messages_total, dice_db_pool_acquire_seconds
```

`dice-network-core` is a workspace member here but designed in the sibling work package; its contract with this package: it consumes `dice-protocol`'s `FrameDecoder`/`encode_frame`, `ALPN_GATEWAY`, `MAX_FRAME_BYTES`, and the Â§3.3 class table, and exposes transport-agnostic framed connections to api-gateway.

---

## 6. Dev-mode flag â€” DECIDED: runtime config, not cargo features

`DICE_PROFILE=dev-lite|full` (default `full`), plus per-backend overrides `DICE_BUS=local|nats`, `DICE_CACHE=memory|redis`. The profile only selects defaults; each shared crate exposes a config enum (`BusConfig`, `CacheConfig`) and a `connect()` returning `Arc<dyn Trait>`.

Why not cargo features: (1) the monolith must ship with **both** backends compiled anyway (self-hosters pick at runtime), so features buy zero binary savings where it matters; (2) feature unification leaks choices across the workspace and creates a CI combinatorial matrix; (3) switching modes must not require a rebuild â€” `just dev` vs `just run-full` is an env var; (4) one dynamic dispatch per event is noise next to network IO.

**Postgres stays required always** (the hybrid option the user left open): sqlx compile-time queries are pinned to real Postgres semantics, so any in-memory DB would fork every query path into a second implementation that rots and hides bugs; `docker compose up postgres` is one command and ~50 MB RAM. `dev-lite` = monolith + local bus + moka cache + dockerized Postgres only (no Redis, no NATS containers).

---

## 7. Key decisions and why (summary)

1. **`ring` crypto provider everywhere** (quinn `rustls-ring`, tokio-rustls `ring`, sqlx `tls-rustls-ring`): rustls 0.23's default `aws-lc-rs` requires NASM+CMake to build on Windows â€” an instant contributor-onboarding failure on this very dev machine. Single provider also avoids rustls "no process-level default provider" runtime panics.
2. **One `Frame` oneof envelope + u32-BE length prefix on QUIC / message-per-frame on WSS**: a single decode path shared by both transports and the client; oneof gives type safety and a natural unknown-payload hook; explicit prefix lets us reject oversized frames before allocating (per-connection memory frugality).
3. **One ordered QUIC control stream + datagrams only for typing**: resume requires a per-session total order; datagrams are reserved for the only m1 traffic that is genuinely loss-tolerant. Presence stays reliable (a lost presence update is wrong until the next change â€” not acceptable for "presence dots" in the demo).
4. **Sends over the gateway, management over proto-over-HTTPS REST**: keeps the realtime path JSON-free and low-latency with nonce/ack correlation, while auth and CRUD get ordinary request/response semantics, middleware, and per-route rate limits â€” without introducing a second schema language.
5. **Snowflakes with bit 63 = 0**: IDs fit `BIGINT`/`fixed64`/JS `BigInt` cleanly; embedded timestamp eliminates `created_at` columns and wire fields; node-id from env keeps the monolith and split deploys identical.
6. **Core NATS for fan-out, JetStream stream created-but-unconsumed**: at-most-once + resume-buffer + REST backfill is the gap-recovery story; per-user durable consumers would destroy the 100k-connection memory budget; the JetStream stream future-proofs notification/search without protocol change.
7. **Committed `.sqlx` offline cache; Postgres always required; runtime dev-mode config; justfile; client outside the workspace; panic=unwind; deps at `-O2` in dev** â€” rationales in Â§Â§2, 5.3, 6.

## 8. Risks and open questions

- **crates.io was returning 503 from this network today** (both WebFetch and `cargo search`). If it persists at implementation time, builds need a mirror/proxy (e.g. `[source.crates-io]` replacement or sparse-index mirror) â€” verify before bootstrap day.
- **sqlx 0.9.0 just shipped; we pin 0.8.x.** Open question: adopt 0.9 at kickoff if it has a few patch releases by then â€” check `.sqlx` cache format compatibility and `tls-rustls-ring` feature name before switching, and pin `sqlx-cli` to the same minor.
- **redis crate crossed to 1.x (1.2)** â€” most online examples target 0.2x; API module paths (`redis::aio`) changed. Implementer should code from current docs, not blog posts.
- **rand-version split in the RustCrypto stack**: `password-hash` 0.5 uses `rand_core` 0.6 while workspace `rand` is 0.9. Mitigated by using `password-hash`'s own `getrandom`-backed `OsRng` for salts (the design above does this); do not pass a workspace `rand` RNG into argon2 APIs.
- **prost open enums**: every `match` on a protobuf enum field goes through `try_from` on an `i32` and must handle unknown variants â€” easy to get wrong; add a clippy-visible helper pattern in `dice-protocol` and cover with a decode-forward-compat unit test.
- **Resume across gateway nodes fails (by design) in m1** â€” fine for the two-client demo; the post-m1 path (Redis-backed session buffer or hand-off) is noted in ADR-0001 so the session/replay types don't bake in node-locality assumptions (they don't: buffer is behind a trait in api-gateway's session struct).
- **Cargo glob members + placeholder dirs**: placeholders deliberately have no `Cargo.toml` and services are enumerated explicitly to avoid cargo manifest-discovery errors; do not "tidy" this into `services/*`.
- **quinn on Windows**: works, but no UDP GSO/sendmmsg batching â€” Windows dev boxes will show worse QUIC throughput than Linux prod; don't benchmark transport perf on the dev machine.
- Open: license choice (affects `workspace.package.license`); whether to adopt `buf` lint/breaking-change CI now or post-m1; exact Ready-snapshot size limits once guild counts grow (pagination is a v1-compatible additive change via capabilities bit).

### Critical Files for Implementation
- D:\Dice\Cargo.toml (workspace root: members, workspace.dependencies, lints, profiles â€” everything else hangs off it)
- D:\Dice\proto\dice\v1\envelope.proto (the Frame envelope; the contract every transport, service, and client shares)
- D:\Dice\crates\protocol\build.rs (vendored-protoc codegen; unblocks all proto-consuming crates)
- D:\Dice\crates\protocol\src\framing.rs (length-prefix codec + bare-frame decode used by network-core and the desktop client)
- D:\Dice\justfile (bootstrap, infra, db-setup, sqlx-prepare, check â€” the workflows contributors live in)