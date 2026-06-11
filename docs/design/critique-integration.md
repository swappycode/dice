> Preserved milestone-1 design document (agent-produced, 2026-06-11).
> Where this conflicts with docs/protocol.md or the critiques' resolutions, those win.

# Dice M1 â€” Integration-Consistency Review (Docs A: workspace+protocol, B: backend, C: client)

Doc A's `docs/protocol.md` is declared normative; where A is silent or demonstrably worse, B/C win. Each item below: the disagreement, then the single resolution implementers follow.

## Wire framing and envelope

1. **Length-prefix endianness and frame caps disagree.** A: u32 **big-endian**, cap 256 KiB. B: u32 **little-endian**, cap 64 KiB. C: u32 **LE**, 1 MiB inbound / 256 KiB outbound.
**Resolution:** u32 big-endian, `MAX_FRAME_BYTES = 256 * 1024` in both directions on both transports, advertised in `Hello.max_frame_bytes` and honored by the client (C deletes its 1 MiB/64 KiB constants). B and C must NOT implement their own codecs â€” both consume `dice-protocol::framing::{encode_frame, FrameDecoder, decode_frame_bare}` (A Â§4). C's `transport/framing.rs` shrinks to a re-export.

2. **Envelope shape: one `Frame` (A) vs `ClientFrame`/`ServerFrame` + nested `Event` wrapper (B), with C coding against B's shape.**
**Resolution:** A's single `Frame{seq, nonce, oneof payload}` with its numbered ranges (10â€“29 control, 30â€“49 requests, 50â€“69 replies, 100+ dispatch) is the only client-facing envelope. B's `ClientFrame`/`ServerFrame`/`Event`/`EventEnvelope` are deleted; B's extra control frames map as: `ResumeFailed` â†’ `Error{INVALID_SESSION}` (see #5), `Ack` â†’ `SendMessageAck`, `Reconnect`/`GOING_AWAY` â†’ `Close` (see #16). C's `GatewayEvent::Dispatch(proto::ServerEvent)` becomes `Dispatch(frame::Payload)`.

3. **Gateway fan-out contradiction inside B: per-session `seq` yet "pre-encoded ServerFrame bytes cloned into each session queue".** Per-session seq makes shared encoded frames impossible as written.
**Resolution:** encode the `Frame` once **without** seq (payload-only bytes, shared `Bytes`); per session, prepend the protobuf encoding of field 1 (`0x08` + varint seq) â€” field concatenation is valid protobuf and scalar last-write-wins is irrelevant since seq appears once. If the implementer wants to defer the trick, per-session re-encode is the acceptable M1 fallback; the shared-Bytes claim in B's Â§3.6 is otherwise wrong.

## Handshake / auth

4. **JWT claims and refresh schema disagree.** A: claims `{sub, iat, exp}`, `refresh_tokens` with `family_id`. B: claims `{sub, sid, iat, exp, iss, aud}`, `auth_sessions` table + `refresh_tokens(session_id, rotated_at, replaced_by)`.
**Resolution:** adopt B's model wholesale â€” the `sid` claim is required for B's session-revocation socket drop, and `auth_sessions` replaces A's `family_id` (reuse-detection = revoke the session). A's `dice-auth-core::AccessClaims` gains `sid`. Migrations become A's layout with B's content: `0001_users.sql`, `0002_auth_sessions.sql` (both tables), `0003_guilds_channels.sql`, `0004_messages.sql`. Keep A's `drt_` refresh-token prefix; token is 32 random bytes, SHA-256 stored.

5. **Resume failure: close-and-reconnect (A: `Close{INVALID_SESSION}`) vs in-band re-identify (B: `ResumeFailed`; C expects `InvalidSession` frame). Also resume token 16 bytes (A) vs 32 (B).**
**Resolution:** keep the connection open: server replies `Error{code=INVALID_SESSION}` (no close), client must send a fresh `Identify` within the standing 5 s deadline â€” saves a full TLS/QUIC re-handshake and reuses A's envelope unchanged. Resume token = 32 bytes. Resume window = 60 s (`Hello.resume_window_ms`); replay ring buffer = 256 frames OR 256 KiB (B's tighter bound), not A's 1024/512 KiB. Identify deadline 5 s (A), not 10 s (B).

6. **Two different "session id"s are conflated.** A says gateway mints session IDs; B's auth-service mints `auth_sessions.id` (JWT `sid`); C stores one `session_id: String`.
**Resolution:** both exist, distinctly named everywhere: `auth_session_id` (`sid`, minted by auth-service, lives as long as the refresh-token family) vs `gateway_session_id` (`Ready.session_id`, minted by api-gateway, used only for Resume). C's `ConnState::Ready` and resume blob use `gateway_session_id`; C's keyring/session module never sees it.

7. **Auth response shape: `AuthSuccess{â€¦, user}` (A) vs `AuthTokens{â€¦, session_id}` (B/C).**
**Resolution:** one proto, A's `AuthSuccess{access_token, refresh_token, access_expires_in_s, user}`. No `session_id` field â€” it's inside the JWT. Login, register, and refresh all return it (refresh returning `user` removes any need for `GET /users/@me`, see cut list).

## Send path / REST split

8. **Message send path: gateway frame (A Â§3.5, B Â§3.7 "rides the socket") vs HTTPS POST (C Â§1.6 + bridge "fires the HTTP send").** Direct 2-vs-1 conflict.
**Resolution:** sends go over the gateway: `Frame{nonce, send_message: SendMessageRequest}` â†’ `Frame{nonce, send_message_ack}`. C deletes `ApiClient::send_message`; `Command` gains `SendMessage{channel_id, content, nonce}` and `GatewayEvent` gains `Ack(SendMessageAck, nonce)`. Nonce is a **client-generated random u64** (matches `Frame.nonce`), not a ulid string â€” drop the `ulid` dep.

9. **Optimistic-UI reconcile: C expects "nonce echo on MessageCreate", but A's `MessageCreate` has no nonce, and ack-vs-bus-dispatch ordering is not guaranteed.**
**Resolution:** add `uint64 nonce = 2` to `MessageCreate` (set by chat-service from the originating request; harmless for other recipients). Client reconciles the pending row on whichever of `SendMessageAck` / `MessageCreate{nonce}` arrives first and dedupes the other by message id.

10. **REST surface: B's endpoints with JSON auth bodies vs A/C's protobuf-everywhere; WSS path `/gateway/v1` (A) vs `/gateway` (B, C).**
**Resolution:** protobuf bodies (`application/x-protobuf`) on **every** REST endpoint including auth (A's ADR-0005; B's "curl-ability" loses 2-to-1 â€” one schema language). Canonical surface, all under `/v1`: `POST /v1/auth/{register,login,refresh,logout}` (A's request/response messages, logout takes `LogoutRequest{refresh_token}`), `GET /v1/channels/{id}/messages?before|after&limit` returning a new `MessageHistory{repeated Message}` (add to `message.proto` â€” currently in neither proto sketch), `POST /v1/guilds`, `POST /v1/guilds/join`, `POST /v1/dms` with body `OpenDmRequest{recipient_id}`, `POST /v1/channels` with `CreateChannelRequest`. REST errors: body is `dice.v1.Error` + appropriate HTTP status (C's undefined `ErrorResponse` is deleted). WSS endpoint: `wss://host:8443/gateway/v1`. Ports per B: 8443 TCP (REST+WSS), 8444 UDP (QUIC), 9600 admin. C's `ApiClient` adds logout/create-guild/join-guild/open-dm and the `after=` history param (needed for gap backfill, C Â§3.4).

## Events, bus, presence

11. **Typing: QUIC datagram Class B (A) vs control-stream frame (B/C); seq semantics waffle in B; rate limits 1/3 s (B) vs 1/8 s (C); expiry 8 s (B) vs 10 s (C).**
**Resolution:** cut datagrams from M1 entirely â€” all frames on the control stream / WS messages; A's `Identify.capabilities` bit 0 already reserves the datagram upgrade. `TypingStart` dispatch carries `seq = 0` and is never placed in the replay buffer (A's rule; B's "seq but not retained" would create permanent resume gaps). Client emits at most 1 typing frame / 8 s / channel while typing; gateway enforces 1 / 3 s; UI expires the indicator 10 s after last receipt.

12. **Presence: sequenced Class A (A) vs ephemeral-excluded-from-resume (B); enum lacks INVISIBLE (A) vs lacks OFFLINE-as-settable (B); and a real gap â€” B fans presence out per-guild only, so DM-only contacts never see each other's dots (C renders dots on the DM list).**
**Resolution:** presence dispatches are sequenced Class A and replayed on resume (A â€” small buffer cost, correct dots). Wire enum: `UNSPECIFIED, ONLINE, IDLE, DND, OFFLINE` with `INVISIBLE = 5` reserved-but-rejected in M1 (cut list). `Presence::connect` signature gains `dm_channel_ids: Vec<ChannelId>` and presence-service publishes to `dice.evt.guild.{gid}.presence` **and** `dice.evt.dm.{channel_id}.presence`; the gateway's existing `by_channel` DM interest covers delivery. No status field in `Identify` (B implied one) â€” sessions start ONLINE, change via `UpdatePresenceRequest`.

13. **Bus subjects and envelope: A's `dice.evt.{guild,user,presence}.{id}` + `BusEvent` wrapping a `dice.v1.Frame` vs B's `dice.evt.guild.{id}.msg/.typing/.presence`, `dice.evt.dm.{id}.*`, `dice.evt.auth.session_revoked` + `EventEnvelope{Event}`.**
**Resolution:** subjects = B's taxonomy (channel-scoped DM routing matches the gateway's `by_channel` map) **plus** A's `dice.evt.user.{user_id}` for self-targeted events (see #14); `session_revoked` is published on the user subject, not a global one. Envelope = A's `BusEvent` (in `proto/dice/internal/v1/events.proto`) carrying a ready-to-dispatch `dice.v1.Frame` with `seq=0` â€” B's `EventEnvelope`/`Event` double-wrapping is deleted; `BusEvent` gains `bool ephemeral` from B (controls replay-buffer insertion generically). JetStream: stream `DICE_EVT` captures `dice.evt.guild.*.msg` and `dice.evt.dm.*.msg` only, created at startup, **zero consumers in M1**; all publishes and gateway subscribes are core NATS (merges A Â§5.2 and B's intent without JetStream consumer machinery).

14. **Demo-breaking gap no doc resolves: mid-session interest updates.** Gateway interest registers at `Ready`; when a user creates/joins a guild or someone opens a DM with them, no one is subscribed to the new subject yet, so `GuildCreate`/`GuildMemberAdd`/`DmChannelCreate` never arrive â€” the demo's "client 2 joins client 1's guild" fails. B's `Event` oneof is also missing `GuildCreate`, `DmChannelCreate`, `MemberRemove`, `ChannelUpdate/Delete` that A's envelope has.
**Resolution:** every session's gateway always holds a (refcounted) subscription to `dice.evt.user.{uid}`. chat-service publishes `GuildCreate` (full guild, to the creator/joiner's user subject) and `DmChannelCreate` (to both recipients' user subjects). On receiving these, the gateway router adds guild/channel interest for that session *before* forwarding the frame. A's full dispatch list (100â€“113) is the event set; B implements all of it.

15. **`Ready`/entity gaps required by the demo:** A's `Guild` proto has no `invite_code` (client 2 can't be invited) and no members (client can't render author names for users learned after Ready, nor a member list to click-to-DM).
**Resolution:** `Guild` gains `string invite_code = 5` (sent to members only) and `repeated Member members = 6` (cap 100 in M1); `Ready` gains `repeated User users = 7` as a deduped user dictionary covering all members and DM recipients. `GuildMemberAdd{Member, User}` keeps caches warm thereafter.

## Data-layer field disagreements

16. **Close-code collision and missing codes.** A defines `4000+ErrorCode` (so 4008 = PAYLOAD_TOO_LARGE); B independently assigns `SLOW_CONSUMER = 4008` and uses an undefined `GOING_AWAY`; C expects a `Reconnect` frame that exists nowhere.
**Resolution:** extend A's `ErrorCode`: `SLOW_CONSUMER = 10`, `GOING_AWAY = 11` â†’ close codes 4010/4011. Graceful shutdown and slow-consumer disconnect use `Close{code}`, not `Error`. Client treats 4011 (and 4010) as resumable reconnect; there is no `Reconnect` frame.

17. **Permissions bit layout conflict + poisoned DB default.** A: `CREATE_INVITE = 1<<7`, `ADMINISTRATOR = 1<<63`. B: `CREATE_INVITE = 1<<5`, `ADMINISTRATOR = 1<<6`, and migration hardcodes `permissions BIGINT DEFAULT 35` (only correct under B's layout).
**Resolution:** A's layout (richer, reserves kick/ban/roles, ADMIN at top bit) is canonical â€” it lives in `dice-permissions` which B must import. The migration loses the magic number: `permissions BIGINT NOT NULL` with the value supplied by Rust as `DEFAULT_EVERYONE.to_db()` (= 0b1000_0011 = 131 under A).

18. **ChannelKind numbering skew across layers:** wire enum `GUILD_TEXT=1, DM=2` (A); Postgres `channel_type` 0/1 (B); client SQLite `kind` 0/1 (C) â€” an off-by-one mapping bug waiting to happen.
**Resolution:** the proto enum value (1/2) is stored verbatim in both Postgres `SMALLINT` and client SQLite; B's `CHECK` constraints update accordingly. No layer ever remaps enum numbers.

19. **QUIC transport params: A disables QUIC keep-alive, idle = 2.5Ã— heartbeat (75 s); B idle 90 s; C enables `keep_alive_interval(15s)` and idle 60 s.**
**Resolution:** QUIC keep-alive OFF on both ends (the 30 s app heartbeat is the keep-alive â€” A's rationale stands, and C's 15 s pings defeat the idle-CPU budget); `max_idle_timeout = 90 s` on both ends (negotiated min anyway). 0-RTT disabled (A and C agree). Server `max_concurrent_bidi_streams = 4` (A).

## Crate ownership, tooling, infra

20. **`crates/network-core` is claimed by two teams with incompatible shapes:** B designs it as server acceptors (`#[async_trait]`, `Box<dyn Transport>`), C as the client (native async fn, enum dispatch).
**Resolution:** one crate, two feature-gated modules: `network-core/server` (B's quinn acceptor + a hand-rolled tokio-rustls accept loop feeding hyper/axum â€” adopt B's own flagged fallback and skip `axum-server` entirely; dyn dispatch is fine here) and `network-core/client` (C's `AnyTransport` enum, gateway driver, backoff). The axum-`ws`-to-`Transport` adapter lives in `api-gateway` (axum types stay out of network-core). Both sides use `dice-protocol::framing` per #1, and one shared `tls.rs` builds rustls configs (ring provider) for quinn/tungstenite/reqwest/server.

21. **Config story: A's env-only loader in `dice-common` (explicitly "no figment") vs B's `crates/config` + figment + `config/*.toml`.**
**Resolution:** A wins for M1: env vars only (`DICE_PROFILE`, `DICE_BUS`, `DICE_CACHE`, `DICE_NODE_ID`, `DATABASE_URL`, addresses, key/cert paths), documented in `.env.example`. Delete `crates/config`, figment, and `config/*.toml`. B's structured fields become `DICE_*` env vars with the defaults B's `default.toml` listed. This also settles node-id sourcing (env, default 0).

22. **Dev TLS + JWT key assets: three stories.** A: rcgen "in-process" (no file for the client to trust!); B: `cargo xtask gen-certs/gen-jwt-keys` â†’ `dev/certs`, `dev/keys`, client env `DICE_DEV_CA`; C: `scripts/gen-dev-certs` â†’ `infrastructure/docker/certs/dev-ca.pem`, env `DICE_EXTRA_CA`.
**Resolution:** the monolith, in dev profiles only, auto-generates and **persists** a dev CA + leaf (SANs `localhost`, `127.0.0.1`, `::1`) to `dev/certs/` and an Ed25519 JWT keypair to `dev/keys/` on first boot if missing (rcgen + ed25519-dalek/pkcs8 behind a `dev-keygen` helper in the monolith crate). No xtask crate, no gen scripts. Canonical paths: `dev/certs/dev-ca.pem`, `dev/certs/server.{crt,key}`, `dev/keys/jwt_ed25519{,.pub}.pem` (all gitignored). Env var name: `DICE_DEV_CA` (B's). The `just client` recipe exports `DICE_DEV_CA`, `DICE_API_URL`, `DICE_GATEWAY_QUIC`, `DICE_GATEWAY_WSS` before `npm run tauri dev`. Persisting (not A's ephemeral) keys matters so client tokens and trust survive server restarts.

23. **Version pins and naming conflicts.** prost 0.14 (A, verified) vs 0.13 (B, C); tokio-tungstenite 0.29 (A) vs 0.26 (B, C); async-nats 0.49 (A) vs 0.38 (B); redis 1.x (A) vs 0.29 (B); metrics-exporter-prometheus 0.17 (A) vs 0.16 (B); monolith at `services/monolith` pkg `dice-monolith` (A) vs `services/dice-server` (B).
**Resolution:** A's verified pins win across the board (B's were stale); single source = root `[workspace.dependencies]`, client workspace pins prost/tokio-tungstenite to the same minors (C must re-verify tokio-tungstenite 0.29 â†” rustls 0.23 at lock time â€” flagged risk). Monolith: `services/monolith`, package `dice-monolith` (the justfile already references it). Migration filenames per #4.

24. **Demo-required client UI that doc C omits entirely:** no create-guild, no invite-code display, no join-guild, no create-channel, no way to start a DM. The demo cannot run on C's screen list.
**Resolution:** add to M1 UI: (a) "+" in the guild rail â†’ create-guild dialog (name only); (b) guild header shows `invite_code` with copy button; (c) join-guild dialog (paste code); (d) member sidebar in ChatView (from `Ready.guilds[].members`) with click-to-DM (`POST /v1/dms` with `recipient_id` â€” avoids needing a user-search endpoint); (e) **no** create-channel UI: chat-service auto-creates a `#general` channel inside the create-guild transaction (backend addition, kills a whole dialog).

25. **Rate-limiter duplication:** B adds a `RateLimiter` trait + `governor` for the in-memory path; A's `dice-cache` already has `incr_expire` for fixed-window limiting.
**Resolution:** `RateLimiter` is a thin helper in `dice-cache` over `Cache::incr_expire` (works identically on Redis and moka); the gateway's per-session token bucket stays hand-rolled in-process. Drop `governor`.

## Cut list (out of M1, with the seam that keeps it cheap later)

- **QUIC datagrams** (typing stays on the control stream; `Identify.capabilities` bit 0 reserved).
- **Split-mode NATS RPC**: `rpc.proto`, `*NatsClient`/`*NatsServer`. Service bins still exist and start (locked decision #4) but cross-service interconnect is M2; the `Arc<dyn Chat/Auth/Presence>` trait seam makes it additive. M1 demo runs the monolith only.
- **Edit/delete message** end-to-end (no UI in C): drop `EditMessage`/`DeleteMessage` requests and trait methods; keep dispatch numbers 101/102 reserved and `edited_at` columns.
- **INVISIBLE presence** (reserve enum value 5; masking logic deferred).
- **`crates/config` + figment + `config/*.toml`** (â†’ env vars, #21) and the **xtask crate** (â†’ auto-gen dev assets, #22).
- **`governor`** (#25) and **`ulid`** (#8) dependencies.
- **`GET /v1/users/@me`** (AuthSuccess and Ready both carry `user`).
- **JetStream consumers** (stream is capture-only) â€” already implied by #13.
- **`avatar_hash`/`icon_hash`** in the client cache and any avatar rendering (media is a future milestone; initials avatars only). **`read_markers`** stays client-local with no backend endpoint.
- **CORS middleware** on the gateway (desktop client only; no browser origin in M1).
- **C's HTTP-send + reserved-frame migration story** (Â§0/Â§1.6) â€” superseded by #8; delete rather than carry dead code.

### Critical Files for Implementation
- D:\Dice\docs\protocol.md â€” must be updated to encode resolutions #1â€“3, #5, #8â€“9, #11â€“12, #16 before any transport code is written
- D:\Dice\proto\dice\v1\envelope.proto â€” single `Frame` envelope with the field additions from #9 (MessageCreate.nonce), #15 (Guild.invite_code/members, Ready.users), #16 (ErrorCode additions)
- D:\Dice\crates\protocol\src\framing.rs â€” the one codec (u32-BE, 256 KiB) both `network-core/server` and `network-core/client` must consume
- D:\Dice\crates\network-core\src\lib.rs â€” feature-gated server/client split and shared TLS builder (#20, #22)
- D:\Dice\services\api-gateway\src\router.rs â€” interest map + user-subject subscriptions + per-session seq append (#3, #13, #14)