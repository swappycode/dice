# Dice Wire Protocol v1 — NORMATIVE

This document is the single source of truth for the client↔server protocol. Implementations
(server `services/api-gateway`, client `crates/network-core`) follow this document, not each
other's code. Changes here require updating `proto/dice/v1/*.proto` and both endpoints in the
same PR. Internal bus envelope: see §9.

Protocol identity: protobuf package `dice.v1`, QUIC ALPN `dice/1`, WSS path `/gateway/v1`.

## 1. Transports

| | QUIC (primary) | WSS (fallback) |
|---|---|---|
| Endpoint | UDP 8444, ALPN `dice/1` | `wss://host:8443/gateway/v1` |
| TLS | 1.3 (rustls, ring provider) | 1.3 via tokio-rustls |
| Carrier | ONE bidirectional control stream per connection | one binary WS message = one Frame |
| Framing | `u32 big-endian length ‖ Frame bytes` | none (WS self-delimits); text frames ⇒ protocol-error close |
| Compression | none | permessage-deflate OFF |
| Keep-alive | QUIC keep-alive OFF (app heartbeat is the keep-alive); `max_idle_timeout` 90 s both ends | tungstenite auto-pong; liveness = app heartbeat |
| 0-RTT | disabled (Identify token would be replayable) | n/a |
| quinn tuning | `max_concurrent_bidi_streams=4`, stream rx window 1 MiB, conn window 4 MiB | max message 256 KiB |

`MAX_FRAME_BYTES = 262144` (256 KiB) in **both directions on both transports**, advertised in
`Hello.max_frame_bytes`, checked **before allocation**; violation ⇒ `Close{PAYLOAD_TOO_LARGE}`.
QUIC datagrams are NOT used in M1 (`Identify.capabilities` bit 0 reserves the typing-over-datagram
upgrade; servers reject the bit in M1 by ignoring it).

The one codec lives in `dice-protocol::framing`: `encode_frame` (prefix + cap), `FrameDecoder`
(bounded accumulator for the QUIC stream), `decode_frame_bare` (WS messages). No other codec
implementations are permitted anywhere.

## 2. Envelope

One protobuf envelope for everything (`proto/dice/v1/envelope.proto`):

```proto
message Frame {
  uint64 seq   = 1;  // per-session monotonic, gateway-assigned; >0 ONLY on sequenced dispatches (100+, except typing). 0 otherwise.
  uint64 nonce = 2;  // client-chosen random u64 on requests (30–49); echoed on replies (50–69) and request-scoped Errors. 0 = n/a.
  oneof payload {
    // control 10–29: never sequenced, never replayed
    Hello hello = 10;  Identify identify = 11;  Resume resume = 12;  Ready ready = 13;
    Resumed resumed = 14;  Heartbeat heartbeat = 15;  HeartbeatAck heartbeat_ack = 16;
    Close close = 17;  Error error = 18;
    // client→server requests 30–49
    SendMessageRequest send_message = 30;
    StartTypingRequest start_typing = 31;
    UpdatePresenceRequest update_presence = 32;
    // server→client replies 50–69
    SendMessageAck send_message_ack = 50;
    // sequenced dispatch 100+: replayed on Resume (exception: typing_start, seq=0, never replayed)
    MessageCreate message_create = 100;
    MessageUpdate message_update = 101;   // RESERVED in M1 (no edit)
    MessageDelete message_delete = 102;   // RESERVED in M1 (no delete)
    TypingStart typing_start = 103;
    PresenceUpdate presence_update = 104;
    GuildCreate guild_create = 105;  GuildUpdate guild_update = 106;  GuildDelete guild_delete = 107;
    ChannelCreate channel_create = 108;  ChannelUpdate channel_update = 109;  ChannelDelete channel_delete = 110;
    GuildMemberAdd member_add = 111;  GuildMemberRemove member_remove = 112;
    DmChannelCreate dm_channel_create = 113;
  }
}
```

**Unknown-payload policy** (newer peer): decoded `payload: None` — if `seq > 0`, ignore but STILL
advance the ack cursor; if it carried a `nonce`, reply `Error{UNKNOWN_OPCODE}`; else ignore.

**Compatibility rules:** field numbers never reused/renumbered; removals get `reserved`; additions
only; every enum has `*_UNSPECIFIED = 0`; prost enums are open (i32) — route unknown values to a
safe default. Breaking change ⇒ new package `dice.v2` + ALPN `dice/2`, old compiled in parallel.

## 3. Handshake

```
connect ──► server: Hello{heartbeat_interval_ms=30000, resume_window_ms=60000, max_frame_bytes=262144}
client (within 5 s, else Close{UNAUTHENTICATED}):
  Identify{access_token, properties{client,version,os}, capabilities, protocol_version=1}
    ──► Ready{...}                      (fresh session)
  Resume{gateway_session_id, resume_token, last_seq}
    ──► Resumed{} + replay of buffered frames with seq > last_seq     (success)
    ──► Error{INVALID_SESSION}          (failure; CONNECTION STAYS OPEN — client must send a
                                         fresh Identify within the same standing 5 s deadline)
```

`Ready{gateway_session_id (fixed64), resume_token (32 random bytes), user, guilds[], dm_channels[],
presences[], users[]}` — `guilds[]` include channels, `invite_code` (members only), and `members[]`
(cap 100 in M1); `users[]` is a deduplicated user dictionary covering all guild members and DM
recipients (so clients can render any author name post-Ready). `GuildMemberAdd{member, user}` keeps
client dictionaries warm thereafter.

**Two distinct session IDs — never conflate:**
- `auth_session_id` — JWT `sid` claim, minted by auth-service, lives as long as the refresh-token family.
- `gateway_session_id` — minted by api-gateway, returned in `Ready`, used ONLY for `Resume`.

## 4. Heartbeat

Interval from `Hello` (30 s); client jitters the first beat by ±10%. `Heartbeat{last_seq,
client_time_ms}` → `HeartbeatAck{client_time_ms}` (echo gives client RTT). `last_seq` is a
**cumulative ack**: the gateway trims that session's replay buffer to `seq > last_seq` and
refreshes presence TTL. Server closes after 2 missed beats; client reconnects-and-resumes after
2 missing acks.

## 5. Resume

Per-session replay ring buffer: **256 frames OR 256 KiB**, whichever first; retained
`resume_window_ms` (60 s) after disconnect; node-local (cross-node resume is out of scope —
fails as INVALID_SESSION). Sequenced dispatches (class A) enter the buffer; typing never does.
Resume failure degrades to full Identify + REST history backfill
(`GET /v1/channels/{id}/messages?after=<last known id>`).

## 6. Message classes

| Class | Frames | seq | Replayed | Notes |
|---|---|---|---|---|
| Control | 10–69 | 0 | no | reliable, ordered |
| A: Dispatch | 100+ except 103 | monotonic from 1 | yes | includes PresenceUpdate (dots must be correct after resume) |
| B: Ephemeral | TypingStart (103) | 0 | no | lossy-tolerant; on control stream in M1 |

Typing rates: client emits ≤ 1 `StartTypingRequest` / 8 s / channel while typing; gateway enforces
1 / 3 s; UI expires indicator 10 s after last receipt.

## 7. Send path (messages go over the gateway, not REST)

```
client: Frame{nonce: random u64, send_message: {channel_id, content}}
server: Frame{nonce: <echo>, send_message_ack: {message}}         (reply)
server: Frame{seq: N, message_create: {message, nonce}}           (dispatch to ALL channel members,
                                                                   nonce set for the author's reconcile)
```

Client reconciles its optimistic pending row on whichever of ack/dispatch arrives first and
dedupes the other by message id. Errors (permission, rate limit) come as `Error{code, nonce}`.

## 8. Close & error codes

`ErrorCode`: `UNSPECIFIED=0, UNAUTHENTICATED=1, INVALID_SESSION=2, RATE_LIMITED=3,
PERMISSION_DENIED=4, NOT_FOUND=5, INVALID_ARGUMENT=6, UNKNOWN_OPCODE=7, PAYLOAD_TOO_LARGE=8,
INTERNAL=9, SLOW_CONSUMER=10, GOING_AWAY=11`.

WS close code = QUIC application close code = `4000 + ErrorCode`. **4010 (slow consumer) and
4011 (going away/shutdown) are resumable** — client reconnects with backoff and attempts Resume.
There is no `Reconnect` frame; graceful shutdown sends `Close{GOING_AWAY}` then closes.
Slow consumer: per-session outbound queue is bounded (128); on overflow the gateway closes with
`SLOW_CONSUMER` rather than buffering — resume heals the gap.

## 9. Internal bus envelope (`dice.internal.v1`, NEVER sent to clients)

```proto
message BusEvent {
  fixed64 event_id = 1;            // snowflake, consumer idempotency key
  uint64 emitted_at_ms = 2;
  string origin = 3;               // "chat-service@node0"
  fixed64 guild_id = 4;            // routing hint; 0 if not guild-scoped
  repeated fixed64 recipient_user_ids = 5;  // user-targeted routing hint
  bool ephemeral = 6;              // true ⇒ gateway never inserts into replay buffers (typing)
  dice.v1.Frame frame = 7;         // ready-to-dispatch payload with seq=0; gateway assigns per-session seq
}
```

Subjects: `dice.evt.guild.{gid}.msg|typing|presence`, `dice.evt.dm.{channel_id}.msg|typing|presence`,
`dice.evt.user.{uid}` (GuildCreate, DmChannelCreate, SessionRevoked). Every session's gateway holds a
refcounted subscription to its `dice.evt.user.{uid}`; on receiving GuildCreate/DmChannelCreate the
router adds guild/channel interest for that session BEFORE forwarding the frame (mid-session joins
work). JetStream stream `DICE_EVT` captures `dice.evt.guild.*.msg` + `dice.evt.dm.*.msg` only;
created at startup; zero consumers in M1 (all real traffic is core NATS pub/sub or the in-proc bus).

## 10. REST surface (HTTPS :8443, protobuf bodies)

All bodies `content-type: application/x-protobuf`; auth via `Authorization: Bearer <access JWT>`
(except register/login/refresh); errors = `dice.v1.Error` body + appropriate HTTP status.

| Endpoint | Body → Response |
|---|---|
| `POST /v1/auth/register` | `RegisterRequest{email, username, password}` → `AuthSuccess` |
| `POST /v1/auth/login` | `LoginRequest{email, password}` → `AuthSuccess` |
| `POST /v1/auth/refresh` | `RefreshRequest{refresh_token}` → `AuthSuccess` (rotated) |
| `POST /v1/auth/logout` | `LogoutRequest{refresh_token}` → 204 |
| `GET /v1/channels/{id}/messages?before\|after=<id>&limit=1..100` | → `MessageHistory{repeated Message}` |
| `POST /v1/guilds` | `CreateGuildRequest{name}` → `Guild` (auto-creates `#general`) |
| `POST /v1/guilds/join` | `JoinGuildRequest{code}` → `Guild` |
| `POST /v1/channels` | `CreateChannelRequest{guild_id, name}` → `Channel` |
| `POST /v1/dms` | `OpenDmRequest{recipient_id}` → `Channel` (idempotent via dm_key) |

`AuthSuccess{access_token, refresh_token, access_expires_in_s, user}` — all three auth endpoints
return it; there is no `GET /users/@me`.

## 11. Identity & entities

- **Snowflakes:** `[1 bit 0][41 bits ms since 2026-01-01T00:00:00Z (1767225600000)][10 bits node][12 bits seq]`.
  Bit 63 always 0 (fits BIGINT/i64/JS BigInt). Wire: `fixed64`. Server-generated only; clients use `nonce`.
- `PresenceStatus`: `UNSPECIFIED=0, ONLINE=1, IDLE=2, DND=3, OFFLINE=4` (`INVISIBLE=5` reserved,
  rejected in M1). Sessions start ONLINE; change via `UpdatePresenceRequest`.
- `ChannelKind`: `UNSPECIFIED=0, GUILD_TEXT=1, DM=2` — **stored verbatim** in Postgres SMALLINT and
  client SQLite. No layer ever remaps enum numbers.
- Message content: 1–4000 chars after trim.

## 12. Auth tokens

- Access: JWT EdDSA (Ed25519), claims `{sub: user_id decimal string, sid: auth_session_id, iat, exp (600 s), iss: "dice", aud: "dice"}`.
  Gateway verifies with the PUBLIC key only.
- Refresh: opaque `drt_<base64url(32 bytes)>`; server stores SHA-256 only; 30-day; rotation on every
  refresh; reuse of a rotated token revokes the whole auth_session (theft detection). Gateway drops
  live sessions on the `session_revoked` bus event.
