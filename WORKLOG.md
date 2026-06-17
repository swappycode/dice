# Dice Worklog

Running log of milestone progress. Newest entry first. Each entry records what was done,
the current project status (and branch), and **what the next milestone is** so work can be
picked up with full context at any time. Update this file at every milestone boundary and
whenever direction changes; keep git commits small and per-logical-unit so `git log` mirrors it.

---

## 2026-06-17 — M4 (5/n): Scaling — lazy member lists (RequestGuildMembers)

**M4 theme = Scaling.** Branch `main`. Per-unit commits (`bd6f4b8` proto / `e01188c`
chat-service / `c80af6e` gateway / `54336bf` network-core + host-bridge / `f16a88f`
client-host command / `fa4bb02` client-ui / `51b43a9` >100 paging test), pushed.
Gates green: `just check` (server) + host clippy/fmt + frontend `npm run check`/`build`.

**What + why.** `Ready` inlines only ≤100 members/guild (the M1 cap); for larger guilds
the rest were unreachable. This adds the designed-but-unbuilt on-demand paging flow,
end to end:
- **Wire** (`bd6f4b8`): `RequestGuildMembers` (frame **37**, request) + `GuildMembersChunk`
  (frame **51**, a nonce-echoed REPLY — Control class, deliberately NOT a #100+ dispatch,
  so it is never replayed on resume) + `CAP_LAZY_MEMBERS` (Identify capabilities bit 1).
- **chat-service** (`e01188c`): `Chat::request_members(actor, guild, after, limit)` keyset-
  pages `guild_members` by `user_id` (membership-gated; a +1-row probe sets `has_more`;
  loads the matching user records). Full split-mode RPC parity (`ChatRequestMembersReq/Resp`
  + serve arm + `ChatNatsClient`).
- **gateway** (`c80af6e`): a `dispatch.rs` arm — rate-limited via the per-session bucket, one
  chunk per request, errors mapped through `chat_error_frame`.
- **client** (`54336bf` / `f16a88f` / `fa4bb02`): network-core `Command::RequestGuildMembers`
  (nonce-tracked) + the chunk surfaces as `ClientEvent::GuildMembers`; the Tauri host forwards
  it as `DiceEvent::GuildMembers` and exposes a `request_guild_members` command; the SolidJS
  `MemberSidebar` requests the roster when a guild at the cap is opened, and the `guildMembers`
  dispatch merges each page into the directory (dedup by userId) and pages on with `after` =
  the last user_id until `has_more` is false.

**Verified (live, infra up).** `chat_rpc` round-trip (wire mapping); `client_e2e`
`request_guild_members_returns_the_roster` drives the full client→gateway→chat path and gets
the nonce-matched chunk; `live.rs` `request_members_pages_large_rosters` seeds a 150-member
guild and asserts page 1 = 100 + has_more, page 2 = 50 + done, strict ascending keyset (no
overlap), non-member rejected.

**Deferred (documented).** Trimming `Ready.users[]` for CAP_LAZY_MEMBERS clients — today
`sync_user_state` still puts ALL members in the dictionary (service.rs:131), so the Ready
bandwidth win is partial until that dict is capped to the inlined set. The one remaining
lazy-members optimization.

**NEXT (M4 remaining).** Observability ✅ + lazy member lists ✅. Remaining BIG M4 items:
multi-node gateway cross-node resume, transactional outbox. Next free Frame **dispatch** #
= **121** (the chunk is reply #51, not a dispatch).

---

## 2026-06-17 — M4 (4/n): Scaling — observability (metrics, Grafana, tracing)

**M4 theme = Scaling.** Branch `main`. Pushed as a run of per-unit commits
(`5f57513` smoke / `666a5e5` bus / `f3523f4` gateway / `c414a82` chat /
`364a0b4` db / `f6cf9f5` rpc / `aeb1d16` bins / `5499b1b` Grafana stack +
lockfile/readme; `24c9838` tracing instrumentation / `e289a54` Tempo wiring),
`just check` green (fmt + clippy --workspace --all-targets
-D warnings + cargo test --workspace + aws-lc gate). **Item chosen with the
user** from a 5-agent assessment of the four BIG M4 candidates — observability
scored highest: a designed-but-hollow seam, the easiest to verify on one box,
the best demo, and fully server-side (the <100 MB client budget is untouched).

**What + why.** The `dice-metrics` facade, the `:9600` admin port, and the
design's `dice_*` metric set + naming existed since M1 but had **zero call
sites** — `/metrics` served an empty registry. This milestone fills the seam and
ships the dashboards:
- **Core metric set.** `dice_gateway_connections{transport}` (a `ConnGauge` RAII
  guard around each `ready_loop`, so detached/resuming time isn't counted),
  `dice_gateway_frames_total{dir,class}` (the in/out arms of `ready_loop`),
  `dice_gateway_closes_total{code}` (in `close_with`, the one coded-close
  chokepoint), `dice_chat_messages_total` (post-commit in `send_message`),
  `dice_bus_dropped_events_total` + `dice_bus_decode_failures_total` (bridged
  from the existing atomics), and DB-pool gauges via
  `dice_database::spawn_pool_metrics`. The `dice_db_pool_acquire_seconds`
  histogram is **deferred** (sqlx has no per-acquire hook; most queries borrow
  `&pool`, so a faithful histogram needs an Executor wrapper — the gauges give
  the saturation signal meanwhile). Every emit is a no-op until `init_prometheus`,
  so dev-lite + the test suite are unaffected.
- **Split-mode RPC latency.** `dice_rpc_request_seconds{service,method}` recorded
  around the handler in `event_bus::rpc::serve`; the auth/chat/presence bins call
  `dice_database::init_metrics_from_env` to expose `/metrics` when `DICE_ADMIN_ADDR`
  is set. `just split-up` now gives each bin a distinct admin port
  (9600/9601/9602/9603), bound `0.0.0.0` so the dockerised Prometheus can scrape
  it via `host.docker.internal`.
- **Prometheus + Grafana.** `just metrics-up` runs both from
  `infrastructure/docker/observability.yml`; Grafana (anonymous) auto-provisions
  a Prometheus datasource + a "Dice — Gateway & Services" dashboard (connections,
  frame/message rates, RPC p50/p99 per service, close codes, pool, bus drops).
  Host ports overridable via `DICE_{PROMETHEUS,GRAFANA}_PORT`.
- **Cross-service tracing.** `dice-logging` gains an opt-in OTLP/HTTP span
  exporter (HTTP over the workspace's ring `reqwest` — no aws-lc; gate stays
  clean) + the W3C `TraceContextPropagator`, active when `DICE_OTLP_ENDPOINT` is
  set (off otherwise — zero cost). `event_bus::rpc` injects the active trace
  context into NATS headers on `call` and extracts it to parent the handler span
  on `serve`, so a request's trace crosses the split-mode RPC boundary. Tempo
  joins the stack (`just metrics-up`) with a Grafana Tempo datasource; `just
  split-up` sets `DICE_OTLP_ENDPOINT` + `DICE_SERVICE_NAME` per process and the
  bins/monolith flush via `dice_logging::shutdown()` on exit.

**Verified live (infra up).** Monolith `:9600` after a smoke run — the
connections gauge balances back to 0 on disconnect, frames in/out by class,
`chat_messages_total`=1. Split fleet `:9601/9602/9603` — per-method RPC p99
(auth register ~168 ms, chat send_message ~24 ms, presence snapshot ~2 ms) +
pool gauges; all four Prometheus targets scrape **up**; Grafana provisioned the
datasource + dashboard and its PromQL returns real data. Bonus: the client-driven
split-fleet demo M4(3/n) left to a human is now an automated `#[ignore]` test,
`crates/network-core/tests/split_smoke.rs` — which also surfaced that the M2
per-IP registration limiter correctly returns 429 + retry-after **through** the
gateway→auth RPC seam.

**NEXT (M4 remaining).** Observability is DONE — `dice_*` metrics + the Grafana
dashboard + cross-service tracing (a split-fleet smoke produced 9/9 cross-service
gateway→service traces in Tempo). The remaining BIG M4 items stay user-steerable:
multi-node cross-node resume, lazy member lists, transactional outbox. (Deferred
within observability: the `dice_db_pool_acquire_seconds` histogram.) Next free
Frame dispatch # = **121**.

---

## 2026-06-17 — M4 (3/n): Scaling — split-mode is now DEPLOYABLE

**M4 theme = Scaling ("microservices split").** Branch `main`. Three commits (`bb90a54` service
bins / `21a1dd0` monolith DICE_SPLIT / `63bd49e` just+env), pushed. Gate green: `just check`
(fmt + `clippy --workspace --all-targets -D warnings` + `cargo test --workspace` + aws-lc gate),
infra up. The split RPC *code* + round-trip tests landed in (2/n); this step makes the fleet
actually **run** decomposed — so the app demonstrably runs as a monolith OR as microservices from
one codebase.

**What + why.** Three gaps closed:
- **Real service bins** (`services/{auth,chat,presence}-service/src/main.rs`, were stubs) — each
  connects the same infra the monolith wires per service (Postgres + NATS bus + Redis cache where
  needed + a snowflake generator), constructs the concrete service, and runs `rpc::serve` on
  `dice.rpc.{service}.*` until Ctrl-C. **auth-service** also loads the Ed25519 JWT pair **read-only**
  (the gateway owns key generation/persistence; the bin reads the SAME files so its minted tokens
  verify at the gateway) and keeps the default `LogMailer`. chat needs only PG+NATS; auth/presence
  add Redis. Added `DEFAULT_NATS_URL` (event-bus) / `DEFAULT_REDIS_URL` (cache) consts in the
  config-owning crates so the bins default like the monolith; new deps anyhow + dice-logging.
- **Gateway split switch** (`DICE_SPLIT=1`) — the SAME `dice-monolith` bin builds the direct services
  (cheap, no I/O) then swaps auth/chat/presence behind their unchanged `Arc<dyn Trait>` seams for the
  `*NatsClient` RPC stubs. media + voice always stay in-process (no RPC seam). Requires the NATS bus
  (full profile) — bails with a clear message otherwise. The startup banner reports the mode.
- **`just split-up`** — pre-builds the bins, starts the gateway first (it generates dev keys + runs
  migrations), pauses, then starts the three service bins each in its own window with a **distinct
  `DICE_NODE_ID`** (0/1/2/3) so snowflake ids never collide. `.env.example` documents both.

**Verified (live, infra up).** ① the three RPC round-trip tests pass against real NATS
(`{auth,chat,presence}_rpc`, the exact `*NatsClient`→`serve` paths the gateway swaps in); ②
**auth-service bin** boots against Postgres+Redis+NATS, loads JWT, logs "serving split-mode RPC";
③ **gateway** boots with `DICE_SPLIT=1`, connects its RPC client, logs the split-routing line, and
serves REST/WSS (banner: `mode: split (… media+voice in-process)`). The only step left to a human is
the client-driven end-to-end demo (a REST register body is protobuf, awkward to hand-craft); the
round-trip is already proven by the integration tests using the identical client stub.

**NEXT (M4 candidates, user to steer).** With the monolith↔microservices demo done, the remaining
M4-scaling items are the BIG ones: multi-node gateway cross-node resume (the replay buffer is
single-node by construction — `api-gateway/src/resume.rs`), lazy member lists (RequestGuildMembers),
transactional outbox (gap mitigated today by resume + REST backfill), and observability. Plus
carried M2/M3 follow-ups. (Per-IP rate limiting is already done — M2 auth hardening threads the
peer IP via `PeerAddr` and auth-service keys limits on `{scope}:{ip}`.) Next free Frame dispatch # = **121**.

---

## 2026-06-17 — M4 (2/n): Scaling — Auth + Chat over split-mode NATS RPC

**M4 theme = Scaling (user-chosen: "microservices split").** Branch `main`. Two commits (`a07407a`
auth / `afd0983` chat), pushed. Gate green: `clippy -p {auth,chat}-service --all-targets -D warnings`,
`fmt`, and **live NATS round-trip tests pass** (`cargo test -p auth-service --test auth_rpc`,
`-p chat-service --test chat_rpc`; both skip cleanly if NATS is down). Note: the headline 100k-conn
benchmark needs real hardware (Windows dev box lacks quinn UDP GSO), so M4 builds the scaling
*architecture* that's verifiable here.

**What + why.** The gateway calls services through `Arc<dyn Auth/Chat/Presence>`. Presence already had a
split-mode NATS RPC seam (M2: `PresenceNatsClient` + `serve` + a live test) so the same code runs as a
monolith (direct call) OR as independent processes (RPC). This step completes that seam for the two
remaining services, so the WHOLE app can decompose into separately-scalable services:
- **`services/auth-service/src/rpc.rs`** — `serve` + `AuthNatsClient` for all 12 `Auth` methods. Requests
  reuse the dice.v1 auth bodies where they suffice + 5 internal `AuthReq` messages carrying the extra
  `ip` / bearer-user fields; the `LoginOutcome` oneof and the typed-error mapping (incl. the
  `InvalidArgument` detail + the `RateLimited` retry-after, both in the fault `message`) round-trip.
- **`services/chat-service/src/rpc.rs`** — `serve` + `ChatNatsClient` for all 19 `Chat` methods. Single
  returns reuse dice.v1 messages; `sync_user_state`/`get_messages` get list wrappers; `HistoryCursor` +
  `ChannelKind` pass as scalars; `PermissionDenied` carries the missing-permission **bits** in the fault
  message (reconstructed with `Permissions::from_bits_truncate`).
- Added the per-method request/response messages to `proto/dice/internal/v1/rpc.proto` (imports
  `dice/v1/common.proto` for the chat wrappers).

**Pattern (for the next service / reviewer):** subjects `dice.rpc.{service}.{method}`, queue group =
service (N replicas load-balance for free); the 1-byte ok/err envelope is generic in
`dice_event_bus::rpc`; only payloads + error mapping live per-service. The error contract is a `u32`
code + a `message` string that carries any data that doesn't fit the code (detail strings, retry-after,
permission bits).

**NEXT (M4 3/n) — make split-mode actually deployable.** The RPC *code* + tests exist for all three
services, but the service `main.rs` files are still STUBS and the gateway only constructs the direct
(monolith) services. Remaining: (a) real service bins that connect NATS + build the concrete service
(PgPool/cache/bus) + run `serve`; (b) a gateway/deployment mode (env, e.g. `DICE_SPLIT=1` or per-service
`DICE_{AUTH,CHAT,PRESENCE}_RPC`) that swaps in the `*NatsClient`s behind `GatewayDeps`; (c) a `just`
recipe to run the split fleet locally. Then the demo: monolith OR microservices from the same code.
Next free Frame dispatch # = **121**.

---

## 2026-06-17 — M4 (1/n): live voice device switching

**M4 STARTED.** Theme otherwise TBD with the user; first task = the live-device-switching item the
user flagged at the M3 close. Branch `main`. Two commits (`3712c0f` host / `e14258c` UI hint), pushed.
Gate green: host `clippy --lib --bins -D warnings` + fmt; frontend `tsc` + vite (CSS 56.54 KB).

**Problem (from M3):** changing the input/output device in Voice settings only applied on the next
voice-engine start, so the user had to re-login for it to take effect (the cpal streams bind at engine
start).

**Fix — apply live, even mid-call.** `audio::VoiceControl` gains a **device-generation `watch`**
(`device_gen`, bumped inside `set_devices`); `subscribe_device_changes()` hands out a receiver. The
**bridge** subscribes in `run()` and, on a change, calls `on_device_change()`: if a voice session is
running it **restarts the engine in place** — drop (stops the thread + closes the old cpal streams)
then recreate `VoiceEngine::start(ssrc, sender, control)` with the **stored ssrc** (the bridge now
tracks `voice_ssrc` alongside `voice`, set on the self `VoiceJoin`, cleared on the self `VoiceLeave`).
Brief audio gap; **no membership change** (no VoiceLeave/Join, peers see nothing). Not in voice ⇒ the
engine reads the new device on the next join (unchanged). Updated the `set_audio_devices` docs + the
Voice-settings hint ("device changes apply immediately").

**Plumbing note:** the device change flows UI `setAudioDevices` → `ClientCore::set_audio_devices` →
`VoiceControl::set_devices` (bumps `device_gen`) → bridge `device_rx.changed()` → engine restart. Same
shared-`Arc<VoiceControl>` seam as mute/deafen/PTT; the only addition is the watch the bridge selects on.

**NEXT:** user verifies live device switching (rebuilt). Then define M4's broader theme with the user
(ROADMAP slots M4 = Scaling: multi-node gateway/resume, guild sharding, lazy member lists; or the
carried M2/M3 follow-ups, or voice hardening — AEC, polyphase resampling, gateway-crash roster-TTL).
Next free Frame dispatch # = **121**.

---

## 2026-06-16 — M3 (10/n): Voice — Step 5 + M3 CLOSE-OUT

**Branch:** `main`. One commit (`9399d6c`, audio.rs) + this worklog. Gate green: host `clippy --lib
--bins -D warnings` + fmt + audio unit tests (incl. new resampler test). **PTT + device picker (M3
8–9/n) verified by the user** before this.

**M3 is COMPLETE.** Everything shipped: login-card cohesion ✅, Vantablack theme ✅, in-app theme
builder ✅, Friends/social ✅, and **Voice end-to-end** ✅ — signaling + SFU + QUIC-datagram transport
+ cpal/Opus engine, then (this run) the live-bug fix (client dropped seq=0 ephemeral voice dispatches,
`3a64ec5`) and the full control surface: **mute/deafen, VAD speaking orbs, global push-to-talk,
device picker** (all user-verified). Voice audio + roster were **user-verified working**.

**Step 5 shipped (this entry):**
- **Non-48 kHz resampling** (`9399d6c`) — linear `PushResampler` (capture `in_rate`→48 kHz) +
  `PullResampler` (playback 48 kHz→`out_rate`); bypassed at 48 kHz (verified path untouched); rate
  helpers unit-tested. Removes the old warn-and-proceed "wrong pitch" limitation.
- **`DICE_VOICE_LOSS` test aid** (`9399d6c`) — drops N% of inbound voice frames (xorshift, no dep) so
  the headline "graceful at 5 % loss" gate is testable; jitter buffer + Opus PLC conceal the gaps.
- **Headline-gate procedure** → local `docs/testing-m3.md` (gitignored): how to measure < 5 % CPU with
  3+ users + verify graceful 5 % loss via `DICE_VOICE_LOSS`. **User-measured** (can't measure here).

**Documented carried limitations (deferred, with reasons):**
- **AEC / NS** (WebRTC APM, C++/abseil) — headphones for now; the seam exists.
- **Resampling quality** — linear, not polyphase (fine for voice; upgradeable).
- **Gateway-crash voice-roster TTL** — normal teardown calls `voice.disconnect`; a gateway *crash* can
  orphan members until restart. The `Cache` trait has no SCAN / hash-field TTL, so a per-member
  heartbeat TTL is a storage redesign — deferred (was a documented phase-1 limitation already).
- **Engine-start Fix 3** — engine starts on the self `VoiceJoin` dispatch (now reliable + verified);
  also starting from the local `voice_join` ack is belt-and-suspenders — deferred.
- **Live device switching → M4 (user-flagged 2026-06-16).** Changing the input/output device is NOT
  applied live — the engine binds cpal streams at start, so a change only takes effect on the next
  engine start (the user found a full **re-login** applies it). **M4 task:** apply it live — add a
  device-generation `watch` to `VoiceControl` (bump in `set_devices`); the bridge subscribes and, if
  the engine is running, drops + recreates it with the stored ssrc (clean drop+recreate, brief gap, no
  membership change / no VoiceLeave-Join to peers). Store the current ssrc on the bridge when it starts
  the engine.

**Carried follow-ups (optional, from M2, still open):** Auth+Chat over split-mode RPC + `services/*/
src/bin/*.rs` split bins; orphaned-media GC; "unread divider line" UI; email-verify as an enforced
login gate; TOTP-secret encryption-at-rest; per-`--profile` WebView2 data-dir.

**NEXT MILESTONE (M4) — starts 2026-06-17 (tomorrow), theme TBD with the user.** First concrete task
the user flagged: **live device switching** (see the limitation above). Other candidates: the carried
follow-ups above; voice hardening (AEC via a Linux/WebRTC path, polyphase resampling, roster-TTL
redesign); or a new feature area. Next free Frame dispatch # = **121**. Infra: `just infra-up` + `just run-full`; client:
`just client-build` + `just client-as <name>`; logs at `%APPDATA%\com.dice.app\profiles\<name>\dice.log`.

---

## 2026-06-16 — M3 (9/n): Voice — input/output device picker (Step 4 complete)

**Branch:** `main`. Two commits (`151a671` host / `45cd2d7` UI), pushed. Gate green: host `clippy
--lib --bins -D warnings` + fmt; frontend `tsc` + vite (CSS 56.54 KB). **PTT (M3 8/n) was VERIFIED
working by the user** before this. **Step 4 (PTT + VAD + device-picker + mute/deafen orbs) is now
complete** pending the device-picker verify.

- **Enumeration + selection (host).** `audio::list_devices()` lists cpal input/output device names +
  the system defaults; `VoiceControl` now holds the chosen input/output device NAMES (`None` = system
  default), and the engine picks by name at start (`pick_input`/`pick_output`, falling back to default)
  and logs which it opened (`in_device`/`out_device` on "voice engine running"). Commands
  `list_audio_devices` (cpal enum via `spawn_blocking`) + `set_audio_devices`. **Applies on the next
  voice join** (cpal streams are bound at engine start) — noted in the UI.
- **UI.** The Voice settings dialog (🎚️) now lists capture/playback devices (fetched on open) with a
  "System default" option; the choice persists in localStorage and pushes to the host on change + at
  startup. `ipc.listAudioDevices`/`setAudioDevices` parity; `AudioDevices` type.

**NEXT — Step 5 (M3 close-out):** the remaining items are hardening/edge-cases (the headline voice
functionality is done + verified):
- **Headline gate** — user-measured: 3+ users < 5% CPU, graceful at 5% loss. The loss-resilience
  (jitter buffer + PLC + `LossStats`) already exists in `dice-voice-core`; needs the measurement +
  doc. (I can't measure; provide the procedure.)
- **Heartbeat-refreshed voice-roster TTL** — server robustness vs. a gateway *crash* orphaning voice
  members (normal teardown already calls `voice.disconnect`). Assess feasibility against the `Cache`
  trait; implement or document as a carried limitation.
- **Resampling for non-48 kHz devices** — the engine warns + proceeds today. NEITHER the dev box nor
  the user (both 48 kHz) can verify resampled audio, so likely a documented known-limitation rather
  than ship-blind.
- **AEC seam** (WebRTC APM, C++/abseil) — still deferred; headphones for now.
- **Robustness Fix 3** — start the engine from the local `voice_join` command too (defense-in-depth;
  the dispatch path works + is verified, so lower priority now).
- **M3 close-out docs.** Next free Frame dispatch # = **121**.

---

## 2026-06-16 — M3 (8/n): Voice — global push-to-talk

**Branch:** `main`. Two commits (`7f96365` host / `418074f` UI), pushed. Gate green: host `cargo check`
+ `clippy --lib --bins -D warnings` + fmt; frontend `tsc` + vite (CSS 56.54 KB). **M3 7/n
(mute/deafen + VAD orbs) was VERIFIED working by the user** before this. New host dep
`tauri-plugin-global-shortcut 2` (pulls `global-hotkey`).

- **Engine gate generalised.** `VoiceControl` gains `ptt_enabled`/`ptt_held` atomics and
  `transmitting() = !muted && (open-mic || ptt_held)`; the engine now gates capture + VAD on
  `transmitting()` (was just `muted`), so releasing the PTT key cuts the mic and clears the speaking
  orb instantly. Mute still independent; deafen still gates playback.
- **Binding (host).** New `ptt` module binds ONE curated key (Backquote / CapsLock / Insert / F8–F10)
  via the global-shortcut plugin and mirrors press/release into `ClientCore::set_ptt_held`. The
  `set_ptt(app, enabled, key)` command (re)binds or clears it; registration is **Rust-side**, so no JS
  capability is needed (mirrors how `notify` uses the notification plugin). Plugin initialised in
  `lib.rs` with `ptt::on_shortcut` as the handler.
- **UI + persistence.** `stores/voiceSettings.ts` (PTT enabled + key, localStorage-persisted) pushes to
  the host on change AND once at startup (`App.tsx` `onMount` → `syncVoiceSettings()`), so PTT re-binds
  if it was on last session. A 🎚️ button in `SelfStrip` opens `VoiceSettingsDialog` (checkbox + key
  `<select>`). `ipc.setPtt` parity across interface/real/mock.

**Verification note:** global shortcuts are runtime/OS-specific — needs a live check that the key
press/release actually gates the mic (and that a bare key registers globally on Windows). Rebuilt the
client for the user to verify PTT.

**NEXT:** verify PTT, then **device-picker** (enumerate cpal in/out devices, select into the engine —
the last grouped Step-4 item), then robustness **Fix 3** (start engine from the local `voice_join`
command too), then **Step 5** (non-48 kHz resampling, heartbeat roster TTL, headline <5% CPU/5%-loss
gate, AEC seam still deferred, M3 close-out docs). Next free Frame dispatch # = **121**.

---

## 2026-06-16 — M3 (7/n): Voice — self-controls (mute/deafen) + VAD speaking orbs

**Branch:** `main`. Four commits (`1a412ce` host mute/deafen / `8c98384` UI mute/deafen / `043135d` host
VAD / `4af00d7` UI speaking orb), pushed. Gate green: host `clippy --lib --bins -D warnings` + fmt +
audio unit tests (incl. new `rms_distinguishes_silence_from_speech_level`); frontend `tsc` + vite (CSS
55.18 KB). **Voice audio + live roster were VERIFIED working by the user** on the 6.6/n build before this.

Built on the now-working voice path. Two coherent features (the next rebuild verifies both):
- **Mute / deafen (functional).** New `audio::VoiceControl` (mute/deafen atomics + a speaking `watch`)
  is created by `ClientCore` (`VoiceControl::new()`), shared via `Arc` through `Bridge::new` into
  `VoiceEngine::start`, and read by the audio thread each tick — so toggling needs **no engine restart**.
  `muted` → captured frames aren't transmitted (capture still drained so it can't back up); `deafened` →
  playout skipped + inbound datagrams dropped (jitter buffers can't grow). `ClientCore::voice_state` sets
  it (and `voice_join` resets it so a rejoin can't inherit stale state). UI: mic + headphone toggles in
  `SelfStrip` (shown only while in a voice channel; a red `--orb-dnd` slash marks engaged); the voice
  store tracks own mute/deafen, optimistically tags our roster member, and fans out via `ipc.voiceState`;
  deafen implies mute; resets on join/leave/logout.
- **VAD speaking orbs.** The engine computes per-frame RMS (`rms_i16`) with a ~300 ms hangover
  (`VAD_RMS_THRESHOLD=900`, `VAD_HANGOVER_FRAMES=15` — **tunable, refine on real mics**), pushes speaking
  transitions into the `VoiceControl` watch; a `ClientCore` task fans each out as `VoiceState` to the
  active channel (`active_voice` tracked on join/leave). Muting forces speaking off; the engine clears it
  on stop. **No frontend logic change** — the existing `m.speaking` orb path lights up; added a green
  `--orb-online` dot before a speaking member's name (static, no loop).

**Plumbing note (for the next session):** the audio thread (`!Send`, std thread) talks to the async world
only through `Arc<VoiceControl>` (atomics) + a `tokio::sync::watch<bool>` (speaking; `watch::Sender::send`
is sync-callable off-runtime). Engine→server voice updates go engine → `VoiceControl` watch → `ClientCore`
task → REST `voice_state`. The engine never touches the bridge/ApiClient directly.

**NEXT (M3 voice remainder):** verify this batch (rebuild done), then **PTT** (push-to-talk — a hotkey
that gates `VoiceControl.muted`; needs a Tauri global-shortcut or focused-key handler) + **device-picker**
(enumerate cpal in/out devices, select into the engine). Then the deferred **robustness Fix 3** (start the
engine from the local `voice_join` command too, not only the self broadcast dispatch). Then **Step 5**:
non-48 kHz resampling, heartbeat-refreshed voice-roster TTL, the headline <5% CPU / 5%-loss measurement
(user-measured), AEC seam (WebRTC APM — still deferred; headphones), and M3 close-out docs. Next free
Frame dispatch # = **121**.

---

## 2026-06-15 — M3 (6.6/n): Voice — ROOT CAUSE FOUND + fixed (client dropped voice dispatches)

**Branch:** `main`. One commit (`3a64ec5`), pushed. Gate green: `cargo fmt`, `cargo clippy
-p dice-network-core --features client -D warnings`, all 33 network-core lib tests (incl. the new
regression test), host `clippy --lib --bins -D warnings`.

**Live re-test (2026-06-15) reproduced BOTH bugs** with two real clients (swappy + asfawfaf), both
**Connected (QUIC)**, same guild/voice channel: (a) NO audio either way; (b) asfawfaf (joined 1st) saw
only itself in the roster, swappy (joined 2nd) saw both. (Prereq fixed first this session: backend
wasn't running → started monolith via `just run-full`; then a stale Redis session → re-login.)

**ROOT CAUSE (one client-side line; found via an 11-agent adversarial Workflow + own trace).** The
gateway **driver** only surfaced a frame as `ClientEvent::Dispatch` for `TypingStart` **or** `seq > 0`
(`crates/network-core/src/client/gateway.rs`, the `handle_frame` match). Voice control frames
(`VoiceJoin`/`Leave`/`State`) are published **ephemeral**, so they arrive with **seq = 0**, and — not
being `TypingStart` — fell through to the `_ => Continue` (ignore) arm and were **silently dropped
before the bridge ever saw them.** That ONE drop explains BOTH symptoms:
- **Stale roster:** asfawfaf never received `VoiceJoin(swappy)`. swappy saw both only from its REST
  join *snapshot* (`voice-service join()` returns the roster incl. prior members) — which is why the
  symptom was asymmetric and looked like a frontend bug but wasn't.
- **No audio:** the cpal `VoiceEngine` starts at exactly one site (`bridge.rs` `Payload::VoiceJoin` arm,
  `is_self`), i.e. off the client's OWN `VoiceJoin` dispatch — also dropped → no engine ever started →
  neither side captured or played, so the (correct, unit-tested) QUIC SFU/datagram path was inert.
- **Why presence worked:** presence is published **non-ephemeral** (`seq > 0`) → passed the `seq > 0`
  arm. Voice was the only ephemeral *non-typing* dispatch, so it was uniquely affected.

The Workflow **refuted** the leading "gateway not subscribing/delivering" hypothesis and the
"frontend reactivity" hypothesis — gateway `guild_subjects` includes `GuildVoice`, `deliver()` fans to
all senders incl. self with no filter, and the Solid store/`ChannelTree` are reactive. All correct;
the only fault was the client driver's `seq=0` filter.

**Fix (`3a64ec5`):** extract the rule into a pure `is_app_dispatch(payload, seq)` that admits
`TypingStart` **and** the three voice frames, used by `handle_frame`; updated the stale `Dispatch`
doc comment; added a **regression unit test** so a future voice dispatch can't be silently forgotten.
Additive, client-driver-only — cannot affect the server voice-service tests or presence/typing.

**NEXT — verify (rebuild in progress, `just client-build`):**
1. Relaunch `just client-as alice` + `just client-as bob` (or swappy/asfawfaf), join the same voice
   channel, speak on **headphones**. Expect: rosters update live on BOTH clients, and audio is audible.
2. Confirm in each `%APPDATA%\com.dice.app\profiles\<name>\dice.log`: `voice join dispatch ...
   is_self=true` → `starting voice audio` → `voice engine running quic_voice_path=true`, and inbound
   `VoiceData` lines. If rosters now update but audio still fails, the residual fault is in the QUIC
   datagram SFU path (separate investigation) — but that path is already E2E-verified, so audio is
   expected to work.
3. **Deferred robustness (Fix 3, not yet done):** start/stop the engine from the LOCAL `voice_join`/
   `voice_leave` command result too (not solely the self broadcast dispatch), so a single dropped
   ephemeral can't leave a joined client permanently silent. Then Step 4 (PTT/VAD/device-picker/orbs)
   + Step 5 (AEC/resampling/roster-TTL/headline-gate/docs).

---

## 2026-06-15 — M3 (6.5/n): Voice — make the no-audio bug diagnosable (logs + diagnostics)

**Branch:** `main`. Four commits (`4618632` network-core `is_connected` / `b578362` host file-log /
`034b479` voice diagnostics / `898f97c` frontend self-leave), all pushed. Gates green at the level
runnable without infra: frontend `tsc` + vite (CSS 54.6 KB), host `clippy --lib --bins -D warnings`,
network-core `clippy --lib -D warnings`, `cargo fmt` both workspaces. **NOT re-run (Postgres/infra was
down): full `just check` workspace tests + the host_gate integration test** — but these changes are
non-SQL (diagnostics + a frontend store fix) and don't touch any service/DB path. Run `just infra-up`
then `just check` to confirm before the next feature commit.

**Root cause of yesterday's bug (a) "client logs NOTHING — not even `starting voice audio`":** the
two-client test runs the **release exe** (`just client-as alice/bob`), and `main.rs` sets
`#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]` → the release build is a
**windowless GUI app with no console**, so every `tracing` line went to a stdout attached to nothing.
The tracing *filter* was fine (`dice_desktop` prefix-matches the `dice_desktop_lib` crate; `info` is
the default level). So the audio bug was never visible — debugging was blocked at step 0.

**What shipped (all to make the next live test decisive — no behaviour change to the audio path):**
- **Per-profile file log** (`lib.rs`): tracing now also tees to a truncated-per-launch `dice.log` next
  to each profile's `cache.db` in the app-data dir (`%APPDATA%\com.dice.app\[profiles\<name>\]dice.log`).
  No `unsafe` (the crate `deny`s it) — a `Mutex<File>` `MakeWriter`, not a console attach. Stdout layer
  stays, so `tauri dev` is unchanged.
- **`VoiceSender::is_connected()`** (network-core): true iff a live QUIC datagram conn exists. `send` is
  a silent no-op on WSS — this exposes that.
- **Engine diagnostics** (`audio.rs`): `voice engine running` now logs `quic_voice_path` + device
  rates/formats; a **one-time `warn!`** fires if we're capturing audio but `!is_connected()` (i.e. on
  WSS → nothing transmitted). So a WSS session announces itself loudly instead of being silent.
- **Join trace** (`bridge.rs`): every `VoiceJoin` dispatch logs `is_self` / `engine_running`, so a
  self-join that fails to start the engine (is_self false, or already running) is visible.
- **Frontend self-leave fix** (`stores/voice.ts` + `dispatcher.ts`): the empty "clear active if we left"
  block is completed with an explicit `isSelf` from the dispatcher — a server-driven removal (kick /
  joining voice elsewhere) now clears our `active` channel; a remote peer leaving our channel does not.
  (NOT confirmed to be bug (b) — see below.)

**bug (b) "channel switch not reactive" — status:** could NOT reproduce by inspection. The server is
correct (`joining_a_second_channel_leaves_the_first` proves a switch publishes VoiceLeave(old) then
VoiceJoin(new) on the GuildVoice subject), and the Solid store/`ChannelTree` wiring looks reactive
(`voice.rosters[ch]` / `voice.active` are tracked store reads). Needs the live log to see whether the
switch dispatches actually ARRIVE at the client. Deferred until the re-test gives data.

**NEXT — the re-test (user runs, I read the logs):**
1. `just infra-up` → `just check` (confirm infra-gated tests still green). Then `just client-build`.
2. `just client-as alice` and (2nd terminal) `just client-as bob`. **Logs now appear in each terminal's
   stdout AND in `%APPDATA%\com.dice.app\profiles\{alice,bob}\dice.log`** (truncated each launch).
3. Both join the same voice channel; speak (HEADPHONES — no AEC yet). Then read each `dice.log` for:
   - `voice join dispatch ... is_self=true` then `starting voice audio` then `voice engine running
     quic_voice_path=<?>`. If `quic_voice_path=false` or the `no QUIC datagram path` warn fires → the
     session is on **WSS**, not QUIC (status bar should show QUIC); that's bug (a). If `is_self=false` or
     no `voice engine running` → engine-start path; if `voice engine stopped` with an error → cpal/device.
4. Send me the two `dice.log` files (or the relevant lines) and we close bug (a), then chase (b) with the
   dispatch trace.

**Still remaining after the bugs (unchanged):** Step 4 PTT + VAD (currently OPEN MIC), device-picker,
real-VAD speaking orbs; Step 5 AEC seam, non-48 kHz resampling, heartbeat roster TTL, headline
<5% CPU / 5%-loss gate, M3 close-out docs. Next free Frame dispatch # = **121**.

---

## 2026-06-14 — M3 (6/n): Voice — phase-3 audio engine (cpal + Opus) + create-channel

**Branch:** `main`. Commits `26635d8` (cpal+Opus foundation) / `e7dd5c1` (VoiceEngine) / `fea3d60`
(create-channel). Host gate green (clippy `-D warnings`, 21 host tests incl. 3 new audio/codec unit
tests + host_gate), frontend `tsc` + vite (CSS 54.6 KB). New host deps: `cpal 0.15` (0.18 clashes
with Tauri's windows-core; 0.15 + windows 0.54 coexist), `audiopus 0.2` (the `opus` crate FAILS to
build on MSVC — CMake; audiopus uses cc + links), `dice-voice-core`.

**Built here + verified as far as a mic-less agent can** (compiles, clippy, codec/conversion unit
tests). **User confirmed Step 1** (cpal loopback example → heard themselves; devices = 48 kHz, in
mono/F32, out stereo/F32 → no resampling needed).

- **`audio.rs`:** `VoiceCodec` trait + `OpusCodec` (libopus via audiopus, 48 kHz mono 20 ms / 960-sample
  frames, VoIP; round-trip + PLC unit-tested). `VoiceEngine`: a dedicated thread owns the
  Windows-`!Send` cpal streams and runs capture → f32→i16 → Opus → `voice_core::VoiceFrame` →
  `VoiceSender` (QUIC datagram), and `VoiceData` → `VoiceFrame::decode` → per-ssrc
  `voice_core::JitterBuffer` → Opus decode (PLC on Conceal) → mix → playback (backlog gate paces it).
  `examples/voice_loopback.rs` = the Step-1 mic→speaker probe.
- **network-core:** cloneable `VoiceSender` (off `GatewayHandle::voice_sender()`) so the audio thread
  sends without touching the bridge.
- **bridge:** starts the engine on our own `VoiceJoin` dispatch (server ssrc), drops it on `VoiceLeave`;
  routes `VoiceData` to it. So clicking a voice channel starts real audio.
- **create-channel (fea3d60):** `create_channel` command + `ApiClient`/IPC + a "＋" in the VOICE
  CHANNELS header (owner, auto-named); the bridge now emits a live `ChannelCreate` DiceEvent (was
  cache-only) → `stores/guilds::addChannel`, so a new channel appears for all members without reload.

**Runtime test (2026-06-14) — partial.** Fixes `e29b8cb` (VOICE in chat-service `channel_access` +
client-cache kind read) landed after the first test. Then: ✅ both users **join** voice channels and
the **channels persist across refresh**. ❌ TWO open bugs:
- **(a) No audio + the client logs NOTHING** — not even `starting voice audio`. Debug order tomorrow:
  **1)** confirm both clients are on **QUIC, not WSS** — `send_voice` is a no-op on WSS (voice rides
  QUIC datagrams), so a WSS session is silent; status bar shows Connected (QUIC|WSS). **2)** confirm
  the client tracing filter shows **info** level (the engine's `tracing::info!` lines) — "prints
  nothing" may just be the log level; set `RUST_LOG=info` / check the subscriber in `src-tauri/src/
  lib.rs`. **3)** verify the self `VoiceJoin` reaches the bridge and `is_self()` is true
  (`current_user` set) so `VoiceEngine::start` actually runs. **4)** then the cpal path itself.
- **(b) Channel switch isn't reactive** — switching channels needs a manual page refresh to show the
  change (a frontend signal/reactivity bug — likely the voice roster / active-channel or the
  channel-list rendering not tracking reactively). Investigate `stores/voice.ts` + `ChannelTree`.

**REMAINING (next chat) — Voice phase 3–4 finish:**
1. Fix bugs (a) + (b) above; then confirm two clients actually hear each other on **headphones**.
2. **Step 4:** PTT + VAD (currently OPEN MIC — transmits constantly), device-picker dialog, speaking
   orbs from real VAD (the muted/deaf/speaking tags + VoiceState path already exist).
3. **Step 5:** AEC seam (WebRTC APM C++ — deferred, headphones for now), resampling for non-48 kHz
   devices, heartbeat-refreshed voice roster TTL, the headline gate (3+ users <5% CPU, graceful at 5%
   loss — user-measured), and the M3 close-out docs.

Audio CONTROLS/codec/engine all compile + are unit-tested; the irreducible unverified-here part is the
live sound, which the user verifies. Voice TRANSPORT (datagrams + SFU) is already E2E-verified (M3 5/n).

---

## 2026-06-14 — M3 (5/n): Voice — phase 3 transport (SFU datagrams over QUIC)

**Branch:** `main`. Two commits (`23b2ce4` server transport / `e2f55f0` host VoiceData). All
gate sets green: full `just check` (incl. the new real-QUIC voice E2E + the existing QUIC/resume
suites), host clippy `-D warnings` + compile. No new dispatch # (datagrams are out-of-band, not
Frames). New api-gateway dep `quinn` (already in-tree).

**What shipped — the *verifiable* half of phase 3.** Voice audio now flows **client → gateway SFU →
other clients over QUIC datagrams**, proven end-to-end with **synthetic packets over real QUIC** (no
audio hardware). This is the transport the cpal capture/playback layer plugs into.

- **network-core:** QUIC datagram I/O — server `QuicTransport` exposes `quic_connection()` (new
  `FramedTransport` method; `None` for WSS), client `QuicTransport::connection()` +
  `AnyTransport::quic_connection()`. The driver publishes the live QUIC connection in a shared slot on
  Ready; `GatewayHandle::send_voice()` sends on it; a per-connection reader task surfaces inbound
  datagrams as `ClientEvent::VoiceData` (try_send → dropped under queue pressure; voice is
  loss-tolerant). Identify advertises `CAP_VOICE_DATAGRAMS` on QUIC. `ApiClient::create_channel`.
- **api-gateway `voice_dg`:** a `VoiceDatagrams` registry of voice-capable QUIC connections keyed by
  user; a per-connection read pump forwards each datagram via `Voice::forward`; the registry is the
  `VoiceSink` (fan-out via `send_datagram`). Registered per transport-attach in `run_ready` behind an
  RAII guard (unregister + abort pump on detach/teardown), so voice survives resume. Capability bit is
  read/logged; QUIC presence is the functional enable.
- **host:** the bridge handles `ClientEvent::VoiceData` (drops the bytes for now — the audio pipeline
  decodes them on-hardware).
- **E2E:** two QUIC clients in one voice channel — A's datagram reaches B verbatim, never echoes to A.

**Remaining = Voice phase 3 audio-device layer + phase 4 (the cowork hand-off).** Genuinely
not-doable-here (an agent can't verify sound; Opus/WebRTC are C/C++ deps): `cpal` capture/playback
(cpal builds clean here; `opus` crate FAILS on MSVC — use `audiopus` or system libopus), the WebRTC
APM (AEC/NS/AGC), PTT + VAD, the device-picker dialog, real-VAD speaking orbs, and the headline
<5% CPU / 5%-loss measurement. Plan + the cpal pipeline seams in `docs/ROADMAP.md` → M3 Voice
phase 3–4. The transport above (`send_voice` / `VoiceData` / the SFU) is what that layer feeds.

---

## 2026-06-14 — M3 (4/n): Voice — phases 1–2 (signaling + SFU), buildable slice

**Branch:** `main`. Eight commits (`add02b0`→`d3068ed`). All gate sets green: `just check`
(fmt + clippy `-D warnings` + full workspace tests + aws-lc clean, incl. `sqlx-prepare` for
migration 0013), host clippy `-D warnings` + tests (15 lib + 2 host_gate), frontend `tsc` + vite
(CSS **54 KB**). New Frame dispatches **VoiceJoin #118 / VoiceLeave #119 / VoiceState #120**
(next free now **121**). Migration **0013**.

**Scope decision.** Voice was the sole remaining M3 item, deferred because the headline gate
(3+ users at <5 % CPU, graceful at 5 % loss, real PTT/VAD) is an on-hardware, multi-client audio
measurement. This session shipped everything that's **buildable + verifiable here** — the device-
independent backend, protocol, and client *signaling* — and explicitly left the audio device I/O
for the on-hardware phase.

**voice-core (new crate, pure logic, 24 tests).** `framing` (an RTP-like `VoiceFrame`: ssrc/seq/
timestamp/marker + opaque Opus payload; 11-byte BE header; encoded as a bare QUIC datagram, zero-
copy decode), `jitter` (a playout jitter buffer: reorders, drops dup/late, reports gaps as
`Conceal`, handles 16-bit seq wrap via extended-seq unwrapping), `plc` (`LossStats` so loss is
measurable + a `ConcealmentLimiter` that caps consecutive conceal before silence). Inc. an
end-to-end lossy+reordered 5 %-loss playout sim. No codec/async/IO.

**Protocol.** `ChannelKind::Voice = 3`; `CreateChannelRequest.kind`; `voice.proto`
(VoiceMember/VoiceRoster + the three dispatches + VoiceJoin/StateRequest REST bodies);
`CAP_VOICE_DATAGRAMS` (Identify bit 0, reserved for the phase-3 datagram negotiation).

**Channel VOICE kind (folded into chat-service).** Migration 0013 relaxes the channels CHECK to
admit `channel_type = 3` (same shape as GUILD_TEXT; old unnamed check dropped by introspection).
`create_channel` takes a kind (UNSPECIFIED→text, VOICE→voice, DM→InvalidArgument); `guild_channel`
now carries the stored kind so the two channel-read paths report the real kind. Live test covers
create + read-back + DM rejection; manifest test → 13.

**voice-service (new crate, mirrors presence).** `Voice` trait (join/leave/update_state/roster/
disconnect/forward) + Redis-backed state (`voice:ch:{channel}` roster, `voice:u:{user}` one-channel
pointer; explicit-leave liveness, gateway calls `voice.disconnect` on teardown). Every mutation
publishes its dispatch **ephemerally** to a new `Subject::GuildVoice(guild)` (added to the gateway's
`guild_subjects`, so guild members auto-subscribe). `GatewayDeps.voice` threaded through all 5
construction sites; REST `GET /v1/channels/{id}/voice` + `POST .../voice/{join,leave,state}`. 3 live
tests (full round-trip over the bus, non-voice/non-member rejection, join-second-leaves-first).

**Phase-2 SFU (the CI-able half).** `Voice::forward(sender, packet, sink)` — forward-only fan-out to
every OTHER channel member via a `VoiceSink` seam (the gateway will impl it over datagram
connections). The **synthetic-packet loopback test** (feed a real voice-core frame through a
recording mock sink, assert fan-out, never echo the sender) is the roadmap's phase-2 gate. QUIC
datagrams enabled in the shared transport config (64 KiB buffer) with a real-QUIC round-trip test
(a datagram flows both ways over the actual server+client configs).

**Client signaling (no audio).** network-core `ApiClient` voice methods; host DTOs + dispatch arms +
ClientCore methods + 4 commands; IPC parity (interface/real/mock — the mock seeds a "Voice Lounge");
a `voice` store (per-channel rosters + active channel, dispatch-driven, reset on logout); a **VOICE
CHANNELS** section in `ChannelTree` (click to join/leave, members listed with muted/deaf tags +
speaking accent).

**Deferred to the on-hardware phase 3** (no client emits voice datagrams until cpal capture exists):
the gateway per-connection datagram pump (wires `forward()` to real connections via the `VoiceSink`),
`Identify.capabilities` bit-0 negotiation consumer, `cpal` capture/playback + WebRTC AEC/NS/AGC +
PTT/VAD + device pickers, per-user speaking orbs driven by real VAD, and the headline <5 % CPU /
5 %-loss measurement. Plan: `docs/ROADMAP.md` → M3 Voice phases 3–4.

**M3 status:** login-cohesion ✅, Vantablack ✅, theme builder ✅, Friends ✅, **Voice phases 1–2 ✅
(signaling + SFU routing + datagram transport, all verified here)**; Voice phases 3–4 (audio device
I/O + headline gate) remain as a dedicated on-hardware pass. Next free Frame dispatch # = **121**.

---

## 2026-06-14 — M3 (3/n): Friends / social

**Branch:** `main`. Five commits (backend / client-host / client-ui + 2 review fixes). All gates green:
`just check` (incl. 2 new live friend tests + `sqlx-prepare` for migration 0012), host clippy `-D warnings` +
tests, frontend `tsc` + vite (CSS 54 KB). New Frame dispatch **FriendUpdate #117** (next free now **118**).

**Design.** Folded friends into **chat-service** (it owns DMs + the bus-publish / `make_event` /
`Subject::User` machinery + user-record seeding) rather than a new service — so no new `GatewayDeps` field,
construction-site churn, or RPC seam. `friendships` table = a normalized `(user_lo < user_hi)` pair + `status`
(pending/accepted) + `requester_id`. **accept reuses `open_dm`** (idempotent): that seeds both user
dictionaries, fans `DmChannelCreate`, and — via the gateway's existing handling — registers mutual DM presence
interest, so friends see each other's presence + can DM in one click, with **zero presence-service changes**.

**Backend.** `Chat` trait + ChatService: `add_friend` (by username; a reverse-pending request auto-accepts),
`accept_friend` (recipient-only), `decline_friend` (pending), `remove_friend` (accepted), `list_friends`
(status from the caller's side). Every mutation publishes `FriendUpdate` to BOTH users' own subjects carrying
the OTHER user's record (no "unknown"). proto `friend.proto` + envelope #117 + `payload_field_number`. REST:
`GET/POST /v1/friends` + `/v1/friends/{id}/{accept,decline,remove}`.

**Client.** network-core `ApiClient` + host commands / `FriendDto` / `DiceEvent::FriendUpdate` + a bridge
`on_dispatch` arm (friends live in the frontend store, not the SQLite cache). UI: a Messages/Friends tab
(`HomePane`); `DmList` is now body-only sharing one `SelfStrip` with the new `FriendsList` (add-by-username,
incoming/outgoing requests, accepted grouped by presence, one-click DM, remove). Friends store hydrated at
bootstrap + kept live by the dispatch; Friends-tab incoming-request badge. IPC parity across
interface/real/mock.

**Adversarial review (5 agents) + 2 fixes.** **(high)** manual "Log off" didn't reset the friends store →
cross-account friends/badge leak → factored a single `resetClientState()` both logout paths call. **(medium)**
`add_friend`'s bare INSERT could 500 on a concurrent mutual add → `ON CONFLICT DO NOTHING` + re-derive (mirrors
`open_dm`).

**M3 status:** login-cohesion ✅, Vantablack ✅, theme builder ✅, Friends ✅. **Voice is the sole remaining
M3 item — DEFERRED:** a real-time audio SFU (`voice-core` + `voice-service` over QUIC datagrams, Opus, cpal
capture/playback, WebRTC AEC/NS/AGC, PTT/VAD) can't be built + verified in this dev environment (no audio
hardware / loopback in the gates), so it needs a dedicated on-hardware build+test pass. Concrete phased plan
in `docs/ROADMAP.md` → M3 Voice. Next free Frame dispatch # = **118**.

---

## 2026-06-14 — M3 (2/n): in-app theme builder

**Branch:** `main`. Two client commits (`custom-theme state/plumbing` / `builder dialog`). Frontend gate
green (`tsc` + vite; CSS **50.7 KB** main + a separate **2.5 KB** lazy builder chunk, builder JS a **4.5 KB**
chunk kept out of the initial/login bundle). No Rust / proto / SQL.

**Custom theme** = a base built-in + five COLOR controls (accent / surface / text / backdrop / titlebar);
everything else (bevels, button/rail/selection gradients, dim text, frame, rings, ink) is *derived* via
`color-mix` toward white/black — direction-agnostic, so it reads on any light OR dark surface. Applied as
inline `--*` overrides on `:root` (inline beats the base `[data-theme]` rule), persisted as a JSON map
(`dice.customTheme` = `{base, controls, overrides}`), and re-applied by the `index.html` pre-paint before
first paint (no FOUC). `installThemeEffect` re-derives on every edit for live preview and clears stale inline
props when switching back to a built-in. Colors only, NO image upload (user decision).

**Builder UI** (`ThemeBuilderDialog`, lazy-loaded): five native `<input type=color>` rows + a base picker +
a live WCAG **contrast readout** (text/surface, accent/surface) + Save / Cancel (reverts the live preview) /
Reset. Reached via a "Custom…" entry in the StatusBar theme dropdown + a ✎ button to re-edit. New tokens:
`--c-brand-ink` (from 1/n) and `--c-page-ink`.

**Adversarial review (7 agents) + fixes.** Confirmed + fixed: **(F1, high)** `readableOn` ink-picker
threshold was mis-tuned (chose white at ~2:1 in the 0.18–0.42 luminance band) → now picks the higher-contrast
ink by direct comparison; **(F2)** the login footer keyed ink to the titlebar but sits on the backdrop → new
`--c-page-ink` derived from the backdrop; **(F4)** the dialog's Escape handler sat on a non-focused div →
focus the dialog on mount (`tabindex=-1`). **Known limitation (F3, deferred):** `::-webkit-scrollbar` colors
are hardcoded per `[data-theme]` (Luna/Aero only) and can't be reached by inline var overrides, so a
Luna/Aero-based custom flipped to a dark surface keeps a light scrollbar — the builder steers users to pick a
base with matching light/dark polarity; full fix = tokenize scrollbar colors + derive them (future).

**M3 remaining:** **Voice** (headline — `voice-core` + `voice-service` SFU over QUIC datagrams),
**Friends/social** (`friendships` + REST + `FriendUpdate` dispatch **#117** + presence interest on accept;
client Friends page). Next free Frame dispatch # = **117**.

---

## 2026-06-14 — M3 (1/n): login-card cohesion fix + Vantablack OLED theme

**Branch:** `main`. Three client commits (`login-ui` / `vantablack` / `bubble-contrast`).
Frontend gate green (`tsc` + vite, CSS **50.5 KB** vs the 100 KB budget). No Rust / proto / SQL
touched. **First M3 work** — both are M3 *Client / themes* items from the ROADMAP.

**Login card cohesion (user-flagged).** The 2-pane card painted `.brand` with the light titlebar
gradient and `.formPane` with `--c-window-face`; in dark themes the near-black form pane read as a
separate black box split by a hard vertical seam. Repainted the whole card as ONE `--c-window-face`
surface with a continuous left→right accent wash that fades out before the form (no element
boundary = no seam); brand text moved onto the face (wordmark → new `--c-brand-ink` token, tagline →
`--c-text-dim`); bevel + themed shadow lift it as one raised window. All token-driven.

**Vantablack theme.** The true-black OLED dark theme: `#000` fields, barely-lifted `#070707` panels
(bevels still read via a 13%-white inset hi), one restrained silver accent, dimmed text, flat-black
backdrop (no gradient — the power-saver win), `--glass-blur:0`. Additive `.gloss`/`.glass-panel`
cool-down (mirrors Midnight). The theme dropdown grows 6 → 7; registered in the `Theme` union/list,
`index.html` pre-paint array, and `main.tsx`.

**Cross-theme review + fixes.** A 7-agent adversarial review resolved the new tokens against every
theme and confirmed **4** real contrast issues, all fixed: Bubble wordmark ink (2.4:1 → deep-teal
`--c-brand-ink`), Bubble `--c-text-dim` (sub-AA → `#426170`), Vantablack card lift on flat black
(`--c-window-frame` `#000` → `#2a2a2a`, `--c-bevel-hi` 0.10 → 0.13), Vantablack text-selection band
(1.5:1 → `--c-select-bg #4a505a`).

**M3 remaining** (see `docs/ROADMAP.md` M3): **Voice** (the headline — `voice-core` + `voice-service`
SFU over QUIC datagrams), **in-app theme builder** (Custom = base + color overrides, colors-only),
**Friends / social** (`friendships` table + REST + `FriendUpdate` dispatch **#117** + mutual presence
interest on accept; client Friends page grouped by presence). Next free Frame dispatch # = **117**.

---

## 2026-06-14 — M2 (9/n): UI funk + theme pack + chime/toast + split-mode RPC — **M2 COMPLETE**

**Branch:** `main`. Four commits (items 12–15). Full `just check` green (two new live RPC tests over
real NATS), host clippy + tests (15 lib + 2 host_gate), frontend `tsc` + vite (CSS 48 KB). New host
dep `tauri-plugin-notification`; new proto `dice/internal/v1/rpc.proto`. **This closes Milestone 2.**

**12 — UI retro-funk pass.** The shared recipes now ease the bevel/ring/gloss changes in on hover/press
(one-shot `var(--t-fast)`, killed under `prefers-reduced-motion`) instead of snapping, and the default
action blooms a held accent glow on keyboard focus (the XP/Aqua "throb", no loop). All token-driven, so
every theme inherits it. (Deliberately light + safe — no visual-feedback loop available; the deferred
"unread divider line" stays deferred: `last_read` isn't exposed to the UI yet, that's its own vertical.)

**13 — theme pack.** Curated **4 of the 6** explored themes (`docs/design/m2-theme-pack.md`) into drop-in
`[data-theme]` token files — the app's first dark modes: **Midnight** (smoked-glass + ice-cyan; ships an
additive `.gloss/.glass-panel` cool-down so glass isn't milky), **Nocturne** (charcoal + magenta neon),
**Bubble** (bright Y2K aqua), **Phosphor** (CRT green + a static scanline veil `.crt-veil` at the app
root, CSS-gated to Phosphor, killed under perf-mode/idle). `lib/theme.ts` is now data-driven (`THEMES`
list → StatusBar dropdown + index.html pre-paint honors any stored theme); added a `--z-overlay` layer.

**14 — chime + OS toast.** New messages NOT in the channel you're actively viewing get a synthesized
two-note Web-Audio chime (`lib/chime.ts`, no asset, 1.5 s throttle) and — when the window is in the
background — an OS toast via `tauri-plugin-notification` (host `notify` command using the plugin's Rust
API; no extra npm dep). The ipc seam gains `notify()` (mock no-ops); the dispatcher gates both so a
message in the focused, active channel stays silent.

**15 — split-mode NATS RPC (LAST).** `dice-event-bus::rpc`: a generic request-reply layer —
`dice.rpc.{service}.{method}`, queue group = service name (free load-balancing), reply framed by a
hand-rolled 1-byte ok/fault tag (no envelope proto), method payloads protobuf. `RpcClient::{call,serve}`
+ `RpcFault` (service-defined code the client stub maps back to a typed error). **Presence is the first
service fully over the wire**: `PresenceNatsClient` implements `Presence` via RPC (drops into
`GatewayDeps.presence` unchanged in a split deploy) and `rpc::serve` runs the responder against any
`Arc<dyn Presence>` — full `PresenceError` ⇄ code mapping. `rpc.proto` holds the presence payloads;
Auth/Chat follow the identical pattern (the design's "minimal — demonstrably works split" bar). Two live
NATS tests: the generic ok/fault round-trip + a complete Presence vertical (unit returns, typed errors,
snapshot) through a mock `Presence` (no Postgres needed).

**M2 is DONE.** All 15 items shipped: RAM/perf, profile polish, per-IP limiting, hb close code, cache
hygiene, edit/delete, replies/reactions, attachments, avatars, notifications, read-markers, **auth
hardening (2FA + verify + reset)**, **UI funk**, **theme pack**, **chime + OS toast**, **split-mode RPC**.
Carried follow-ups (small, optional, post-M2): orphaned-media GC sweep; the "unread divider line" UI;
split-mode RPC for Auth/Chat (same pattern as Presence) + the `src/bin/*.rs` split bins; email-verify as
an enforced login gate; TOTP-secret encryption-at-rest. Next free Frame dispatch # = 117.

---

## 2026-06-14 — M2 (8/n): auth hardening (TOTP 2FA + email-verify + password-reset)

**Branch:** `main`. Five commits (11a wire+service / 11a login-ui / 11a settings-dialog / 11b
wire+service / 11b ui). Full `just check` green (two new auth live tests), host clippy + tests (15
lib + 2 host_gate), frontend `tsc` + vite (CSS 39 KB). Migrations `0010_totp` + `0011_email_verify_reset`,
`.sqlx` re-prepared. New dep `totp-rs` (default-features off, `otpauth` only — pure Rust, aws-lc gate
still clean). **The security-critical heavyweight.**

**11a — TOTP 2FA.** `auth-core::totp`: SHA1/6-digit/±1-step verify that returns the MATCHED time-step
so the caller enforces single-use (RFC 6238 §5.2 replay guard — reject any step ≤ the last consumed);
`otpauth://` URI; unbiased (rejection-sampled) recovery codes + normalized sha256 hashing. `token.rs`
signs/verifies a short-lived **audience-tagged** ("dice-totp") login ticket so an access token and a
2FA challenge ticket can never be swapped. Migration 0010 = `users.totp_secret/totp_enabled/totp_last_step`
+ `totp_recovery_codes`. auth-service: **`login` now returns `LoginOutcome` (Success | TotpRequired{ticket})**
— the breaking signature change rippled to rest.rs, network-core `ApiClient` (its own `LoginOutcome`),
the host, and every login test; `complete_totp_login` takes a TOTP **or** a recovery code, per-user
rate-limited (5/5min); `totp_enroll/confirm/disable` (secret inactive until a confirm code proves it).
REST: `/v1/auth/login` → `LoginResponse`, `/v1/auth/login/totp`, bearer `/v1/users/@me/totp/{enroll,confirm,disable}`.
Client: LoginCard 2-step challenge; **SecurityDialog** (🔒 in SelfStrip) for enroll (setup key + otpauth
link → confirm → recovery codes) / disable.

**11b — email-verify + password-reset.** A **`Mailer` seam** (auth-service `mailer.rs`): `LogMailer`
dev default logs the token; SMTP drops in behind the trait; `AuthService::with_mailer` swaps it (so the
6 `AuthService::new` call sites DON'T churn — builder, defaulted). Migration 0011 = `users.email_verified`
+ `auth_tokens` (sha256, purpose 1=verify/2=reset, expiry, single-use). `token.rs` `mint_prefixed`/
`hash_prefixed` generalize the refresh-token scheme for `dvt_`/`drst_` tokens. register mails a verify
token; `verify_email`; `resend_verification` (bearer, rate-limited). `request_password_reset` ALWAYS
returns Ok (no account-enumeration oracle), per-IP+per-email rate-limited; `reset_password` validates the
new password BEFORE spending the token and **revokes every session** (publishes SessionRevoked per kill).
REST public `/v1/auth/verify-email` + `/v1/auth/password-reset/{request,confirm}`, bearer
`/v1/auth/verify-email/resend`. Client: LoginCard "Forgot your password?" (request code → enter code +
new password); SecurityDialog email-verify section. Mock simulates both flows.

**Tests.** Two new auth-service live tests: full 2FA lifecycle (enroll→confirm→challenge→TOTP replay
rejected→recovery login→disable) and verify+reset (capturing mailer extracts the mailed token; reset
kills the pre-reset session). DB migration-manifest test updated (11 migrations).

**Deferred (small):** email-verify is informational, NOT a login gate (enforcement is a future toggle);
TOTP secret is plaintext-at-rest (same tier as `password_hash`; encryption is future). No `email_verified`
flag is exposed to the client (avoided a `User` proto ripple) — the verify UI is always available.
Carried over: orphaned-media GC, the "unread divider line" UI. Next free dispatch # = 117 (11 added no
Frame dispatch — all REST).

**M2 done so far:** edit/delete/replies/reactions, attachments, avatars, notifications, read-markers,
**auth hardening**. Remaining: UI funk pass + theme pack, chime + OS toast; split-mode NATS RPC last.

---

## 2026-06-13 — M2 (7/n): read-markers sync

**Branch:** `main`. One commit. Full `just check` green (new chat live test), host clippy + tests,
frontend `tsc` + vite. Migration `0009_read_markers`. Completes the read path started in (6/n).

Re-introduces the M1-cut **server** read-marker. `read_markers (user, channel, last_read_message_id)`.
`Chat::mark_read` advances the marker to the channel's current newest message (`GREATEST`, so a slow
device can't regress it) and broadcasts **`ReadMarkerUpdate(116)`** to the caller's OWN subject — the
multi-device "sync": reading on one device clears the badge on the others. `POST /v1/channels/{id}/read`
now persists+broadcasts via chat, THEN clears the unread counter (a counter-clear hiccup is non-fatal;
the dispatch clears every device's badge regardless). The server derives `last_read` from
`channels.last_message_id`, so the client sends no body.

**Client.** `ReadMarkerUpdate` → cache `read_markers` upsert (the client table was unused since M1) +
emit; the dispatcher clears the channel's unread badge. Live test: persist + self-broadcast +
non-member rejected.

**Deferred (small):** an "unread divider line" / jump-to-unread UI from the now-available
`last_read_message_id` (the data is there; the rendering is a later polish). Item 14 = OS toast +
chime. Orphaned-media GC still pending.

**M2 done so far:** edit/delete/replies/reactions, attachments, avatars, notifications (unread),
read-markers sync. Remaining: auth hardening, UI funk pass + theme pack, chime; split-mode NATS RPC last.

---

## 2026-06-13 — M2 (6/n): notifications (unread via a durable consumer)

**Branch:** `main`. Three commits (notify backend / read+clear endpoints / client badges). Full
`just check` green (new cache + notification-service live tests), host clippy + tests, frontend `tsc`
+ vite (CSS 36 KB). No migration (uses the cache).

**notification-service (new crate).** A **durable JetStream consumer** on the `DICE_EVT` stream
(durable name `notifications`): for each `MessageCreate` it resolves channel recipients (guild
members / DM recipients) from Postgres and bumps a per-(user, channel) unread counter for everyone
except the author. The decode→resolve→bump **core (`handle_event`) is bus-agnostic and live-tested**
against Postgres + an in-memory cache (no NATS); `run()` is thin JetStream glue. **Full profile only**
— `dev-lite`'s in-process Local bus has no stream, so the monolith skips the consumer there and unread
accrues purely client-side. Wired into the monolith drain set.

**Unread store.** `dice-cache::UnreadStore` keeps the count in the value namespace as a LE-`u64`
(`keys::unread`, `UNREAD_TTL` 30 d) so it reads back (the increment-only counter primitive can't).
notification-service is the only writer that bumps; the read-marker path clears. The bump's
read-modify-write can race a clear — benign eventual consistency for M2.

**Read path.** `GET /v1/unread` → the caller's non-zero per-channel counts (channels from
`sync_user_state`, counts from the store). `POST /v1/channels/{id}/read` clears one. `GatewayDeps`
gained `unread: UnreadStore` (built from the cache, like `rate`) — threaded through every
`GatewayDeps` construction site.

**Client.** Unread store seeded on bootstrap (`GET /v1/unread`), bumped live by the dispatcher for
non-active-channel messages (never the author's own), cleared (local + `POST /read`) on channel open,
reset on logout/expiry. Badges: counts in the channel tree + DM list, a guild-rail dot when any of a
guild's channels is unread. Mock accrues badges live from the ambient/echo stream.

**Item 10 status:** the user-visible read path (clear-on-read) ships here. The FULLER read-markers
work — a persistent `last_read_message_id` table + a `ReadMarkerUpdate` dispatch for multi-device
badge sync — remains as item 10. **Deferred:** OS toasts (a Tauri notification plugin) + the chime
sound are item 14; an orphaned-media GC sweep is still pending.

**M2 done so far:** edit/delete/replies/reactions, attachments, avatars, notifications (unread).
Remaining: read-markers multi-device sync, auth hardening, UI funk pass + theme pack, chime;
split-mode NATS RPC last.

---

## 2026-06-13 — M2 (5/n): avatars (user avatars on the media infra)

**Branch:** `main`. Three commits (wire+service / client-host / client-ui). Full `just check` green
(incl. a new chat avatar live test), host clippy + tests, frontend `tsc` + vite (CSS 35 KB), `.sqlx`
re-prepared, migration `0008_avatars`. Re-introduces the M1-cut avatar field, now backed by
media-service (an avatar is just a `media` row).

**Wire/DB.** `User.avatar_id` (media id; 0 ⇒ initials). New `user.proto`: `SetAvatarRequest` +
`UserUpdate(115)` — the FIRST new dispatch event since M1, so it rippled through
`payload_field_number`, the cache `apply_event`, the host bridge `on_dispatch`, the `DiceEvent` enum
(Rust + TS), and the frontend dispatcher. Migration 0008 adds `users.avatar_media_id` (FK→media, ON
DELETE SET NULL). Adding `avatar_id` to `User` rippled to every `v1::User{}` literal (chat sync/load,
auth `auth_success`, all host cache/test literals). **prost gotcha:** the extra field tipped the
generated `BusEvent`/`Frame` oneofs past clippy's `large_enum_variant` threshold (224 vs 16 B) — fixed
with `type_attribute(..., allow(large_enum_variant))` in build.rs (boxing would ripple to every
construction site).

**Service.** `chat-service::set_avatar(actor, media)` validates the media is an image the caller
uploaded, updates the column, and **broadcasts `UserUpdate` to the caller's own subject + every guild
and DM they share** so peers update live (no reconnect). auth-service register/login/refresh now carry
the avatar. REST: `PUT /v1/users/@me/avatar` (204; the broadcast does the propagation). Live test:
validate (non-image / not-yours rejected) + persist + guild-subject broadcast + sync reflection +
clear.

**Client.** Avatars reuse the attachment byte path (`fetch_attachment`/`ipc.attachmentSrc`). Host:
`ApiClient::set_avatar`, `ClientCore::set_avatar`, `UserUpdate` → cache (`upsert_user` writes the media
id into the existing `users.avatar_hash` column — no cache migration) + emit. UI: the `Avatar`
component renders the image or falls back to initials; **MemberSidebar** shows member avatars and
**SelfStrip** shows the self avatar with click-to-change (file picker → upload → `setAvatar`, UI
updates via the `userUpdate` echo). Mock keeps browser demos working via object URLs.

**Deferred (follow-ups):** guild icons (symmetric: `guilds.icon_hash` cache column already exists +
GuildRail render), and avatars in message-row headers (text-only today). An orphaned-media GC sweep is
still pending from attachments.

**Chat/profile completeness:** edit ✅ delete ✅ replies ✅ reactions ✅ attachments ✅ avatars ✅.
Remaining M2: notifications, read-markers sync, auth hardening, UI funk pass + theme pack, chime;
split-mode NATS RPC last.

---

## 2026-06-13 — M2 (4/n): attachments (media-service + message attachments)

**Branch:** `main`. Three commits (wire+service / client-host / client-ui). Full `just check` green
(incl. new chat + media live tests), host clippy + tests (15 lib + 2 host_gate), frontend `tsc` +
vite build (CSS 35 KB), `.sqlx` re-prepared, one new migration (`0007_attachments`).

**Storage seam.** New `media-service` crate (was a placeholder dir). A `MediaStore` trait with a
`LocalFsStore` dev impl (`DICE_MEDIA_DIR`, default `data/media`, gitignored); the S3/rustls-ring +
SigV4 backend is the documented seam, **MinIO still deferred** (aws-lc gate forbids `aws-sdk-s3`).
`MediaService` validates size (8 MiB cap)/filename/MIME, sniffs image `width`/`height` via `imagesize`
(pure-Rust, header-only — a declared image that won't parse is rejected), writes bytes THEN the
`media` row. Live tests: round-trip, dimension sniff, reject empty/oversize/corrupt, unknown→NotFound.

**Wire/DB.** `Message.attachments` (+ `Attachment`), `SendMessageRequest.attachment_ids`,
`UploadMediaRequest/Response`. Migration 0007: `media` + `message_attachments` (junction PK on
`media_id` ⇒ one-shot use) and **relaxes the messages content CHECK to 0..4000** so attachment-only
(empty content) messages are valid (the "content OR ≥1 attachment" rule is enforced in chat-service).
`MediaId` added to dice-common. Adding `attachments` to `Message` rippled to every `v1::Message{}`
literal across services + both client workspaces + all SendMessageRequest test literals.

**Service.** `send_message` claims attachments one-shot in the send tx (each must be owned by the
sender AND unused — a count-mismatch rejects foreign/used/duplicate ids); history joins attachments in
display order. Live test covers ownership, one-shot, empty-content-with-attachment, order round-trip.

**Transport.** Upload is a separate bearer `POST /v1/media` with its OWN larger body-limit layer
(8 MiB + slack, ≠ the 1 MiB realtime/REST cap) — protobuf body, no multipart dep. Download is bearer
`GET /v1/media/{id}` streaming the bytes + MIME. Channel-scoped download ACL is a future hardening
(any authed user may fetch by the unguessable snowflake id today).

**Client.** network-core `ApiClient::upload_media`/`download_media`; `Command::SendMessage` carries
attachment_ids. Host: `upload_attachment` (base64 in → store) + `fetch_attachment` (→ `data:` URL;
base64 keeps it compact over IPC, bounded by the cap — a streaming `dice-media:` URI scheme is a
future optimisation). Cache v3 `message_attachments` table, stored from authoritative messages
(create/echo/snapshot) but NOT on edit (preserve, like reply/reactions), joined on read. **Bonus fix:**
`clear_all` now also purges `message_reactions` + `message_attachments` on logout/account-switch
(reactions were previously left behind). UI: a 📎 button stages removable upload chips; images render
inline (dims reserve layout space, click → full size), other files as download chips; mock keeps
browser demos working via object URLs.

**Chat completeness so far:** edit ✅ delete ✅ replies ✅ reactions ✅ attachments ✅. Remaining M2:
avatars (re-introduce M1-cut fields on this media infra), notifications, read-markers sync, auth
hardening, UI funk pass + theme pack, chime; split-mode NATS RPC last.

---

## 2026-06-13 — M2 (3/n): replies + reactions

**Branch:** `main`. Full `just check` green (incl. the new chat live tests), host clippy + tests,
frontend `tsc` + vite build (CSS 34 KB, well under the 100 KB budget), `.sqlx` re-prepared, two new
migrations (`0005_replies`, `0006_reactions`).

**Replies.** `reply_to_id` on `Message` + `SendMessageRequest`; a plain column (migration 0005, NO
foreign key) so a reply whose parent is later deleted just renders as "original message" rather than
failing the send. `send_message` gained a `reply_to` param (threaded through gateway dispatch,
network-core `Command`, the host, and the composer); history preserves it. Client cache stores it on
INSERT only (ON CONFLICT can't wipe it, so an edit keeps the reply ref). UI: a "Reply" hover action
sets the composer reply-target (reply bar above the input); replying rows show a `↪ author: snippet`
reference resolved from the store.

**Reactions.** New `message_reactions` table (0006). `AddReaction(35)`/`RemoveReaction(36)` requests;
the broadcast is a `ReactionUpdate(114)` **delta** (`{message_id, emoji, user_id, added}`) — each
client adjusts its own aggregate and flips `me` when the user is itself, so one event personalises
correctly for everyone. add/remove are idempotent (only a real change fans out). `get_messages`
attaches the per-emoji aggregate (`count` + `me`) for the requesting user, so reactions survive
reload; the client cache mirrors this (aggregate table written from API snapshots, adjusted by live
deltas, joined on read). UI: reaction pills (highlighted when `me`) toggle on click; a "React" action
opens a fixed system-emoji palette (no image assets).

Live tests: `reply_to_id_round_trips_through_history`,
`reactions_aggregate_in_history_and_broadcast_deltas`.

**Chat completeness so far:** edit ✅ delete ✅ replies ✅ reactions ✅. Remaining M2: attachments
(media-service + MinIO), notifications (notification-service + JetStream), read-markers sync, auth
hardening, the UI funk pass + theme pack, chime; split-mode NATS RPC last.

---

## 2026-06-13 — M2 (2/n): carried gaps cleared + message edit/delete

**Branch:** `main`. All gates green: full `just check` (fmt, clippy -D warnings, ~200 tests, aws-lc
clean), host clippy + `cargo test` (15 lib + 2 host_gate), frontend `tsc` + vite build, `.sqlx`
re-prepared. Continues from the M2 (1/n) RAM entry below.

**Carried M1 gaps — done (4 of the 5; split-mode NATS RPC deferred to last per the user):**
- **`--profile` polish** — a named profile now titles its window `Dice — <name>` so two side-by-side
  instances are tellable apart in Alt-Tab. (The bigger `--profile` blocker — the release exe loading
  an error page — was the custom-protocol fix in the RAM entry.)
- **Per-IP rate limiting** — auth-service already had per-IP limits but the gateway always passed
  `ip=None`, so every unauthenticated client shared one `noip` bucket (one attacker exhausts
  everyone's login budget). The TLS accept loop now injects the socket peer as a `PeerAddr` request
  extension; `login`/`register` read it and pass the real IP. X-Forwarded-For stays untrusted.
  Regression test `serve_https_injects_peer_addr`.
- **Dedicated heartbeat-timeout close code** — `ERROR_CODE_HEARTBEAT_TIMEOUT=12` (→ 4012), distinct
  from GOING_AWAY (4011, shutdown) for observability; client maps `4010..=4012` to resume.
  Protocol §8 + `close_code_mapping` test updated.
- **Per-account cache hygiene** — `apply_ready` only diffed shared guilds, so logout→login as a
  different account in the same data dir left the prior account's messages/users/read-markers in the
  local cache. It now purges all tables on a `current_user_id` mismatch (shares `clear_all` with
  `wipe`). Test `ready_for_a_different_user_purges_the_previous_account`.

**Chat completeness — message edit + delete (full vertical):**
- Proto: `EditMessageRequest(33)`/`DeleteMessageRequest(34)` requests; `MessageUpdate(101)`/
  `MessageDelete(102)` dispatch events (reserved since M1) now live.
- chat-service: `edit_message` (AUTHOR-ONLY, even for mods — Discord semantics) + `delete_message`
  (author, or MANAGE_MESSAGES in a guild; DMs author-only). New `ChatError::Forbidden`. Both publish
  via a shared `publish_to_channel` (refactored `send_message` onto it). Live tests for author-only
  edit, MANAGE_MESSAGES delete, and the dispatched events (13 chat live tests green).
- Gateway dispatch arms reply only on error — success is confirmed by the broadcast dispatch the
  requester also receives (so edit/delete are non-optimistic, no rollback logic).
- Client: network-core `EditMessage`/`DeleteMessage` commands; host bridge/cache/state/commands/DTO
  plumbing (cache `MessageUpdate` upserts via ON CONFLICT, `MessageDelete` drops the row); UI hover
  Edit/Delete on own messages, inline editor (Enter saves / Esc cancels), `(edited)` label; mock IPC
  implements both for browser demos.

**Next:** replies (`reply_to_id`, reserved in `0004_messages.sql`) + reactions (new table) →
attachments (media-service + MinIO) → notifications (notification-service + JetStream) → read-markers
sync → auth hardening → UI funk pass + theme pack. Split-mode NATS RPC last.

---

## 2026-06-13 — M2 (1/n): WebView2 RAM −44% + release-load fix + perf-mode

**Branch:** `main`. First M2 item — the headline carried gap (idle RAM <100 MB). Host compiles +
clippy clean; frontend `tsc` clean; release client builds and **renders the real login card**
(screenshotted), idle RAM measured.

**Pre-existing release bug found & fixed (priority).** `just client-build` ran a plain
`cargo build --release`, which has `tauri::is_dev() == true` (that's `!cfg!(feature =
"custom-protocol")`). So the "release" exe loaded `devUrl` (`localhost:1420`) and showed
*"Hmmm… can't reach this page"* — i.e. `client-as` (the two-user demo) never actually worked
standalone, and the M1 perf snapshot's "~170 MB" was measured on that **error page**, not the app.
Fix: added the standard `[features] custom-protocol = ["tauri/custom-protocol"]` to the host
`Cargo.toml` (was missing) and `--features custom-protocol` to `just client-build`. Dev
(`tauri dev`) deliberately leaves it off.

**WebView2 RAM reduction (the work).** Window creation moved OUT of `tauri.conf.json` (`"windows":
[]`) and INTO the Rust `setup` hook via `WebviewWindowBuilder`, created **after** `app.manage(core)`
(also closes a latent race: state is now guaranteed present before the webview's first IPC). This
lets us set `additional_browser_args` from `DICE_WEBVIEW_ARGS` (one build, many experiments) with a
tuned default (`DEFAULT_WEBVIEW_ARGS`). Because we set the args ourselves, wry stops applying its own
default, so it's re-included and all feature-disables folded into the single `--disable-features=`
list Chromium honours.

Measured on the **real rendered app** (release exe, isolated `--profile bench` = clean login screen,
~25–30 s idle; metric = summed **private commit** across host + WebView2 tree, matching M1's "private"):

| Config | Private | WS (naive) | Procs |
|---|---|---|---|
| Before (wry-default args only) | **~210 MB** (208/214) | ~404–413 MB | 7 |
| Shipped default (`--in-process-gpu` + feature/bg trims) | **118 MB** | ~317 MB | 6 |

`--in-process-gpu` is the headline: folds the separate GPU process (~41 MB) into the browser WHILE
KEEPING hardware acceleration (strictly better than `--disable-gpu`'s software render, which would
punish the Aero glass for ~3 MB less). The feature disables (Translate/MediaRouter/OptimizationHints/
…) also cut the renderer ~108→60 MB. Net **−92 MB / −44%**. `--renderer-process-limit=1` and
audio-in-process were no-ops here (already one renderer; no idle audio process). Host stays 6 MB.

**Still ABOVE the <100 MB stretch goal**: the residual is the Chromium renderer (~60 MB) + browser
(~28 MB) floor. Closing it needs the heavier levers M1 flagged — `SetMemoryUsageTargetLevel(LOW)`
memory-trim on blur/minimize (helps *backgrounded* use, NOT this focused-idle benchmark; and it'd be
the host's FIRST `unsafe` block — `unsafe_code = "deny"` today — so it's a pending policy decision),
and longer-term a native-UI shell.

**Perf-mode toggle** (roadmap escape hatch): `src/lib/perfMode.ts` (persisted like the theme, with
an index.html pre-paint to avoid FOUC), a "Perf" checkbox in the StatusBar, and
`html.perf-mode { --glass-blur: 0 !important }` in base.css. Forces glass off regardless of theme and
is the hook the future CRT-veil will check.

**New tooling:** `apps/desktop-client/scripts/measure-ram.ps1` + `just client-measure [idle]` — launches
the release exe under `--profile bench`, sums private commit across the descendant tree only (never
matches `msedgewebview2.exe` by name — VSCode et al. share it), tears the tree down. A/B an arg
experiment with `$env:DICE_WEBVIEW_ARGS`.

Files: `src-tauri/{Cargo.toml, tauri.conf.json, src/lib.rs}`, `scripts/measure-ram.ps1`, `justfile`,
`src/{App.tsx, index.html, lib/perfMode.ts, styles/base.css, components/chrome/StatusBar.*}`.

**Next (M2 cont.):** carried gaps — `--profile` polish, per-IP rate-limit plumbing, split-mode NATS
RPC, dedicated heartbeat close code, per-account cache namespacing — then chat completeness
(edit/delete/replies/reactions, attachments, notifications) and the UI funk pass + theme pack.

---

## 2026-06-13 — Post-M1 QA fixes (4 issues)

**Branch:** `main`. Reproducible client connection bug + three smaller QA issues. All gates green
(`just check`, src-tauri clippy/test, tsc+build, aws-lc-sys clean).

**Issue 1 (priority) — client stuck "Offline" after a monolith restart.** Root cause: on cold
start the client restores the keystore session, renders the shell cache-first, then the gateway
driver tries to (re-)authenticate; when the server rejects it (dev-lite keeps sessions only as
long as the process — a restart loses them, and a refresh-token rotation desync makes refresh
return 401), the driver landed in a silent `Failed` → host emitted `"offline"` → stuck, no way
back but manual Log off. Fix (vertical):
- network-core: `TokenError::Rejected` (terminal) vs `Refresh` (transient); driver maps via
  `token_error_flow` and the 2nd handshake 4001/4002 to a new terminal `Flow::AuthExpired`, which
  emits `ClientEvent::AuthExpired` before parking. Transient refresh failures now back off+retry
  instead of failing hard.
- host: `SessionManager` maps a refresh **4xx → Rejected** (5xx/transport stay transient); the
  bridge, on AuthExpired, clears credentials + wipes the cache, emits `sessionExpired`, and tears
  the driver down so the next login reconnects (the parked driver no longer blocks `ensure_gateway`).
- frontend: `sessionExpired` resets stores + routes to `LoginCard` with a "session expired" notice.
- regression test `expired_session_routes_to_login_instead_of_hanging_offline` (host_gate.rs):
  revoked session → shell renders cache-first → `sessionExpired` emitted → credentials cleared.

**Issue 2 — two instances for local two-user testing.** `--profile <name>` / `DICE_CLIENT_PROFILE`:
own app-data dir (`profiles/<name>/cache.db`) + scoped keyring (`OsKeyring::for_profile`) + exempt
from the single-instance lock. `just client-build` + `just client-as <name>`; documented in
getting-started (incl. the "a browser tab is mock mode, not a real second user" warning that
explains the user's "invalid server code" confusion — they were joining a real code into a mock).

**Issue 3 — `just client` "Port 1420 in use".** `predev` (scripts/free-port.mjs) frees an orphaned
dev server before vite's strictPort claim; best-effort, cross-platform. **Follow-up (a5d6d42):**
the first version used `netstat -ano -p tcp`, which lists only IPv4, so a vite orphan bound to the
IPv6 loopback (`::1:1420`) stayed invisible and the port was never freed. Switched the Windows path
to `Get-NetTCPConnection` (both families); verified live against a real `::1:1420` listener.

**Issue 4 — hard-coded "(mock mode)" footer.** Driven by the real `MOCK_IPC` flag now (so only a
plain browser tab, which runs the mock IPC, shows it).

**Manual-test clarifications (NOT bugs — confirmed correct behavior):**
- Relaunching a closed client logs back in as the same user = intended session persistence
  (keyring). The Issue-1 fix only triggers when the SERVER rejects the session; manually force it
  with `just db-reset` while a client is logged in, then relaunch → drops to login, not Offline.
- A user's presence orb goes OFFLINE for others ~**60 s** after they close (the deliberate
  resume-window; the gateway calls `presence.disconnect` only when the detached window expires).
  Candidate M2 tuning: shorten the dev resume window for snappier offline detection.

Commits: ff3f5a3 (network-core), 1697089 (host+test), d0d48c8 (frontend+label), ed58ea8 (profile),
5335d5b (free-port), a5d6d42 (free-port IPv6 fix). All gates green; HEAD at a5d6d42; tree clean
(except pre-existing untracked `docs/testing-m1.md` + `qa/`, which are the user's, not this work).

**Next milestone — M2** (full slice in the local-only `docs/ROADMAP.md`, gitignored). Carried gaps first:
WebView2 RAM (~170 MB vs <100 MB target — host is only 5.5 MB; headline item), `--profile` polish,
per-IP rate-limit plumbing (`serve_https` peer addr → gateway), split-mode NATS RPC. Then chat
completeness (edit/delete/replies/reactions, attachments via media-service, notifications off the
JetStream stream) and the **UI retro-funk pass** (user wants the UI funkier while keeping the retro
aesthetic — gloss/gradients on the flat panes, guild-tile tinting, Bliss-style backdrop, XP balloon
notifications) + Midnight Aero dark theme. Infra: `just infra-up` (Postgres on host **5433**),
`just dev` monolith, `just client` (one dev instance) / `just client-as <name>` (built, isolated).

---

## 2026-06-12 — MILESTONE 1 WRAP-UP: Phase 5 polish gate

**Branch:** `main`. All five phases done. `just check` fully green (fmt, clippy -D warnings,
full test suite, aws-lc gate). Live boot verified: `just dev` monolith banner clean (dev-keygen,
migrations, REST+WSS 8443 / QUIC 8444 / metrics 9600 answering), release client (12.1 MB exe)
launched against it.

**Perf snapshot (release client, login screen, 60 s idle, Win11):**
| Target | Measured | Verdict |
|---|---|---|
| Cold start < 2 s | **1.54 s** (process → first webview child) | ✅ |
| Idle CPU < 1% | **0.05%** (10 s window, whole tree) | ✅ |
| Idle RAM < 100 MB | **~170 MB private** (host 5.5 MB + WebView2 164 MB; 373 MB naive WS sum overcounts shared pages) | ❌ see below |

RAM verdict detail: the Rust host is exceptionally lean (5.5 MB private). The entire overage is
the WebView2 process tree floor on current Win11 (6 processes; top consumers: 70 MB renderer,
41 MB GPU). Still ~2.4–4.7× lighter than Discord's 400–800 MB, but the <100 MB goal needs M2
work: webview memory-trim on blur/minimize, `--disable-gpu`-class browser-arg experiments via
WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS, single-renderer tuning, and (longer term) evaluating a
native-UI shell. Filed as the headline M2 optimization item.

**Demo status:** headless E2E suites prove the full multi-user flow (chat/typing/presence/DM/
resume) over BOTH transports. The interactive two-instance visual demo is ready for the user:
terminal 1 `just infra-up && just dev`; terminal 2 `just client`; a second instance needs its
own data dir (single-instance plugin focuses otherwise) — e.g. launch the release exe with a
distinct `APPDATA` or add a `--profile` arg in M2. Status bar shows Connected (QUIC) or (WSS);
`DICE_TRANSPORT=wss` forces the fallback path.

**Known gaps carried to M2:** RAM target (above); per-IP auth rate limits get ip=None (peer addr
not threaded through serve_https); second-instance profile switch; heartbeat-timeout close code
reuses 4011; split-mode NATS RPC interconnect; message edit/delete; voice (M3 per master plan).

**MILESTONE 1 COMPLETE.** Master-prompt Phase 1 scope (auth, gateway, guilds/channels, DMs,
realtime messaging, presence) shipped end-to-end: Rust backend (QUIC+WSS binary-protobuf
gateway, Postgres/Redis/NATS with in-proc dev fallbacks, monolith + service bins) and the
retro Luna/Aero Tauri desktop client, 200+ tests, all milestone gates green.

---

## 2026-06-12 — Phase 4 COMPLETE: QUIC client transport + QuicFirst policy

**Branch:** `main`. The network-core half was built by an agent (which then died silently before
the host wiring — second such death; both post-mortems: long silent tool call, no processes left);
its work compiled clean and all tests passed on my verification. I wired the host side by hand.

**Shipped:**
- `crates/network-core` client: `QuicTransport` (quinn client endpoint, single bidi control
  stream, u32-BE framing via the shared codec, keep-alive OFF/idle 90 s/0-RTT off per protocol §1),
  `AnyTransport::Quic`, `TransportPolicy{QuicFirst{3 s}|WssOnly|QuicOnly}` + `TransportSelector`
  (2 consecutive QUIC failures ⇒ WSS preference + opportunistic re-probe), `PreferredTransport`
  (as_str/from_name) + `initial_preference` on `GatewayClientConfig`, `QuicEndpoint::from_host_port`
  (IPv4-preferring resolution), `ConnState::Ready{transport}` / `ConnStateLite::Ready{transport}`.
  **42 tests green** incl. QUIC E2E: happy path over QUIC, untrusted-cert rejection, QuicFirst
  fallback (no endpoint AND unreachable endpoint), resume over QUIC.
- Host wiring (by hand): `CoreConfig` gains `quic` + `policy` from `DICE_TRANSPORT`
  (quic-first default | wss | quic) and `DICE_GATEWAY_QUIC` (default localhost:8444);
  `ensure_gateway` is async and feeds the persisted `last_transport` cache-meta back as
  `initial_preference` (double-checked locking around the await); the bridge persists the active
  transport on every Ready and emits it in the connState event; frontend shows
  "Connected (QUIC|WSS)" in the status bar (types/store/dispatcher/mock updated).
- host_gate test now asserts `last_transport` == "wss" after Ready (WssOnly in-process backend).

**Gates:** root workspace fmt/clippy -D warnings clean, network-core 42 tests; src-tauri clippy
clean + 14 tests (incl. the extended host gate); tsc + vite build green (CSS still 30.56 KB);
aws-lc-sys absent in BOTH workspaces.

**Next milestone — Phase 5: polish gate.** (1) `just check` full green; (2) live two-instance
demo: `just dev` + `just client` ×2 (second instance needs a distinct cache/keyring scope —
set a per-instance DICE_PROFILE-style env or app-data dir override if single-instance plugin
blocks; verify chat/typing/presence between real windows, then `DICE_TRANSPORT=wss` vs
quic-first and confirm the status bar shows QUIC vs WSS); (3) perf snapshot vs targets
(<100 MB idle full tree via `Get-Process dice-desktop, msedgewebview2 | Measure-Object
WorkingSet64 -Sum`, cold start <2 s, idle CPU <1%) recorded in docs/; (4) known-gaps review
(per-IP rate limits ip=None; QUIC server keylog?; heartbeat-timeout close code) ⇒ file follow-ups
in worklog; (5) final M1 wrap-up entry.

---

## 2026-06-12 — Phase 3 COMPLETE: Tauri 2 desktop host over WSS

**Branch:** `main`. Resumed from the Phase-3 checkpoint below; the partial src-tauri files were
audited, fixed, and finished. ALL gates green.

**Shipped (`apps/desktop-client`):**
- `src-tauri/src/lib.rs` + `main.rs`: ONE tokio runtime built first → `tauri::async_runtime::set`
  before the Builder; ring provider install; tracing init; single-instance plugin (focus+unminimize);
  ClientCore managed from the setup hook (cache at `app_data_dir()/cache.db`); background
  session-resume + gateway connect at startup when the keystore holds a session.
- `src-tauri/src/commands.rs`: 13 `#[tauri::command]` shims matching `src/lib/ipc.ts` exactly
  (session_status, login, register, logout, get_bootstrap, send_message → returns the pending row,
  fetch_messages, start_typing, set_presence, create_guild, join_guild, open_dm, connection_state);
  camelCase JS keys map onto snake_case args via the Tauri 2 default; errors cross as
  `CoreError::user_message()` strings.
- Frontend wiring: new `src/lib/ipc.real.ts` (invoke/listen, rejections wrapped in `Error`);
  `ipc.ts` now selects real-inside-Tauri (detected via `__TAURI_INTERNALS__`) with
  `VITE_MOCK_IPC=true` as the mock override; TitleBar was already wired to getCurrentWindow().
  `npm run tauri` script + @tauri-apps/cli added (makes `just client` work).
- Fixes to the checkpointed files: rusqlite 0.40→0.32 (`links="sqlite3"` clash — sqlx-sqlite 0.8
  needs libsqlite3-sys 0.30; 0.40 cannot coexist), `time` pinned 0.3.47 (0.3.48 breaks cookie
  0.18.1), lib target renamed `dice_desktop_lib` (MSVC .pdb collision with the bin), logout now
  shuts the gateway down BEFORE clearing credentials, `get_bootstrap` waits (bounded 10 s, 250 ms
  state polls) for the first applied Ready when the cache is empty/user-only AND the driver is
  actively connecting — fresh login of an existing account no longer paints an empty shell;
  offline starts still serve the cache instantly.
- New headless gate test `src-tauri/tests/host_gate.rs` (in-process backend per
  network-core/client_e2e.rs; fake keystore + channel-backed emitter): register → keystore holds
  drt_ → Ready → optimistic send (pending sqlite row → ack reconciles to the real id, nonce kept)
  → second RAW WSS client joins + sends → dispatch reaches cache AND emitter → fetch_messages
  pages from cache → core rebuilt on the same cache+keystore with the backend GONE →
  session_status + get_bootstrap + history all served offline from sqlite.

**Gates:** cargo check/clippy `-D warnings`/fmt clean; **14 tests passing** (13 unit + 1 E2E gate
vs live Postgres, 7.5 s); `cargo tree -i aws-lc-sys` empty; `npx tsc --noEmit` + `npm run build`
green; `npm run tauri dev` boots end-to-end (vite 2.3 s, cargo 22 s, window process spawned ~28 s).

**Next milestone — Phase 4: QUIC client transport** (fill `AnyTransport::Quic`; QuicFirst{3 s}
policy, 2-failure WSS preference, persist last-good transport in cache meta; verify dev-CA on both
transports). Then Phase 5 polish gate (perf snapshot vs <100 MB/<2 s/<1% targets, `just check`,
two-instance live demo). Live demo now: `just dev` (monolith) + `just client` (sets DICE_DEV_CA +
URLs, runs `npm run tauri dev`).

---

## 2026-06-11 (evening) — Phase 3 CHECKPOINT: client core DONE, Tauri host PARTIAL

**Branch:** `main`. Session checkpointed deliberately (token-budget stop requested by user);
the in-flight Tauri-host agent was stopped cleanly mid-write. THIS ENTRY IS THE CONTINUATION
CONTRACT for the next session.

**DONE + verified by hand (committed):**
- `crates/network-core` CLIENT half (feature `client`, in default features):
  `src/client/` — WSS `AnyTransport` enum (QUIC variant reserved for Phase 4), gateway driver
  (Hello→Identify/Resume→Ready state machine, ±10% heartbeat jitter, cumulative-ack last_seq,
  2-missed-acks reconnect, full-jitter backoff w/ 60 s-healthy attempt reset, INVALID_SESSION
  re-identify on same connection, nonce→Ack/RequestError correlation, bounded mpsc(256) events,
  `connect(cfg, tokio Handle)` for Tauri setup-hook safety), `TokenProvider` trait, `ApiClient`
  (reqwest, protobuf bodies, dev-CA via add_root_certificate, one 401 refresh-retry).
  **Gates green: clippy -D warnings; 22 tests (15 unit + 4 E2E vs in-process live backend + 3 tls).**
  Read `crates/network-core/tests/client_e2e.rs` for usage patterns.

**PARTIAL — `apps/desktop-client/src-tauri` (committed as WIP, DOES NOT COMPILE YET):**
Files that EXIST (written by the stopped agent, quality unreviewed): build.rs, Cargo.toml
(own standalone workspace; tauri 2 + rusqlite bundled + keyring 3 + path deps w/
network-core default-features=false features=["client"]), tauri.conf.json (decorations:false,
shadow:true, devUrl :1420, nsis), capabilities/default.json, placeholder icons,
src/{dto.rs, emit.rs (testable emitter trait), keystore.rs (keyring abstraction),
session.rs (token lifecycle), bridge.rs (cache-first event pump), cache/{mod.rs (40 KB rusqlite
worker), schema.rs}}.

**REMAINING WORK for the next session (Tauri host completion):**
1. `src/state.rs` — ClientCore struct: ApiClient (DICE_API_URL default https://localhost:8443,
   TlsOptions::from_env reading DICE_DEV_CA), Mutex<Option<GatewayHandle>>, cache handle, session.
2. `src/commands/` — Tauri commands EXACTLY matching `apps/desktop-client/src/lib/ipc.ts`
   (names + payload shapes; ids are STRINGS): login, register, logout, getBootstrap,
   sendMessage (pending row + nonce, returns pending message), fetchMessages, startTyping
   (host throttle 1/8 s/channel), setPresence, createGuild, joinGuild, openDm, connectionState.
3. `src/lib.rs` + `src/main.rs` — ONE tokio Runtime built first → `tauri::async_runtime::set`
   BEFORE Builder (NEVER bare tokio::spawn from setup hook); install_ring_provider; manage
   ClientCore; single-instance plugin; spawn bridge when session exists.
4. Frontend: `src/lib/ipc.real.ts` (invoke/listen via @tauri-apps/api) + make ipc.ts pick real
   when in Tauri; wire TitleBar buttons to getCurrentWindow(). Keep tsc + npm run build green.
5. Headless gate test (no webview): factor command bodies into plain async fns; fake keystore
   trait impl; in-process backend (copy pattern from network-core/tests/client_e2e.rs);
   register→login→Ready→send (pending→ack reconcile in sqlite)→incoming message→cache→
   fetchMessages pages→offline restart serves bootstrap from cache.
6. Gates: cargo check/test/clippy -D warnings for the src-tauri package; npm run build;
   try `npm run tauri dev` once (60 s, kill after; report if WebView2/bundling blocks).
The full original agent prompt (richer detail) is preserved in a local build
script (TauriHost phase). Review/fix the partial files against it before continuing.

**After Phase 3:** Phase 4 = QUIC client transport (fill AnyTransport::Quic; QuicFirst{3 s}
policy, 2-failure WSS preference, persist last-good transport in cache meta) + verify dev-CA
works on both transports. Phase 5 = polish gate (perf snapshot vs <100 MB/<2 s/<1% targets,
`just check` green, two-instance live demo, final worklog wrap-up). Infra: containers stay up
(`just infra-up`); monolith via `just dev`.

---

## 2026-06-11 — Phase 2: WSS backend vertical slice (DONE) + retro frontend scaffold

**Branch:** `main`

**Done (backend — 6 workflow agents in 3 waves, all verified independently after):**
- network-core server half: shared tls.rs (ring-only, install_ring_provider, load_server_config,
  generate_dev_certs CA+leaf with localhost/127.0.0.1/::1 SANs, quic_server_config with the
  protocol §1 tuning), FramedTransport trait, QuicAcceptor/QuicTransport (5 s accept_bi deadline,
  FrameDecoder, app-close→clean None), serve_https (tokio-rustls + hyper http1 with_upgrades —
  NO axum-server).
- auth-service: register/login/refresh/logout, race-safe unique-violation mapping, dummy_verify,
  single-tx rotation + theft detection → SessionRevoked on user subject, rate limits.
- chat-service: sync_user_state (full Ready payload incl. uncapped user dictionary),
  send_message (tx + post-commit MessageCreate{nonce}), keyset history (NEWEST-FIRST contract),
  create_guild (auto-#general + invite code), idempotent join/open_dm, ephemeral typing.
- presence-service: 90 s TTL key families, DND>ONLINE>IDLE aggregate, guild+DM fan-out,
  add_interest (broadcasts current dot to only-new subjects), last_seen on OFFLINE.
- api-gateway: session state machine (5 s identify deadline, mpsc(128), per-session seq+replay
  ring 256/256KiB, heartbeat 2× deadline), detached-registry resume (live session task keeps
  absorbing fan-out while detached), refcounted interest router + always-on user subject +
  mid-session interest from GuildCreate/DmChannelCreate, REST proto-over-HTTP (§10 complete,
  Bearer middleware, 1 MiB cap with proto-413), WS adapter, start()→Started{bound addrs}.
- dice-monolith: env config, dev-keygen (certs+JWT on first boot), profile wiring
  (dev-lite=Local+Memory, full=Nats+Redis), migrate-on-boot, banner, Ctrl-C drain ≤15 s.
- **E2E gate test `wss_demo_phase2_gate` PASSES** (~1.3 s): register×2 → Ready → guild create/join
  via invite (live GuildCreate to joiner) → realtime chat w/ nonce ack+dispatch → typing seq=0 →
  presence via add_interest → DM → REST history → abrupt-drop resume with contiguous replay →
  graceful Close{GOING_AWAY}. Binary smoke-ran in BOTH profiles.

**Done (frontend, in parallel):** apps/desktop-client — retro Luna/Aero SolidJS scaffold with
mock IPC seam (`npm run dev` standalone). 30.5 KB CSS, tsc strict clean, zero hardcoded hex.

**Verified by me after the workflow:** `cargo fmt` clean, `cargo clippy --workspace --all-targets
-- -D warnings` clean, **146 tests passing / 0 failed** (incl. both E2E suites against live
infra), aws-lc-sys absent, `.sqlx` generated (49 queries) and **offline build verified**
(SQLX_OFFLINE=true cargo check green).

**Known M1 gaps (accepted/flagged):** per-IP auth rate limits get ip=None (peer addr not threaded
through serve_https — wire in Phase 5 or network-core follow-up); heartbeat timeout closes 4011;
per-session re-encode of dispatch frames (critique #3 fallback).

**Next milestone — Phase 3: desktop client over WSS (est. 3–4 d):**
(1) network-core "client" feature: AnyTransport enum (Wss first), gateway driver state machine
(Idle→Connecting→Authenticating→Ready→Backoff full-jitter→Failed), heartbeats w/ jitter,
resume, TokenProvider trait, ApiClient (reqwest, proto bodies, one 401 refresh-retry, dev CA via
DICE_DEV_CA / add_root_certificate); read services/api-gateway tests/gateway_e2e.rs for the
exact client-side handshake sequences. (2) apps/desktop-client/src-tauri: Tauri 2 host —
tauri::async_runtime::set FIRST (never bare tokio::spawn in setup), ClientCore state, keyring
(windows-native) refresh token + RAM access token behind refresh Mutex, rusqlite(bundled) cache
worker thread (schema per docs/design/desktop-client.md §3.2, single contiguous window/channel),
bridge task (cache.apply BEFORE emit, presence coalesced 100 ms), commands matching the
frontend's src/lib/ipc.ts seam (ids as strings), decorations:false window config + capabilities.
(3) Replace ipc.mock.ts default with real Tauri IPC when window.__TAURI__ present. Gate: the
mock demo flows work against the REAL monolith (just dev) with two app instances.

---

## 2026-06-11 — Phase 1: Protocol + shared crates (DONE)

**Branch:** `main`

**Done:**
- All 8 proto files (`dice.v1` + `dice.internal.v1`) per docs/protocol.md.
- 10 crates implemented and green: dice-protocol (vendored-protoc codegen, the ONE framing
  codec u32-BE/256 KiB, FrameClass), dice-common (snowflakes, env config, shutdown),
  dice-permissions (binding bit layout, DEFAULT_EVERYONE=131), dice-auth-core (Argon2id
  OWASP params + dummy-verify, Ed25519 JWT {sub,sid,iss,aud,exp=600s}, drt_ refresh tokens),
  dice-logging, dice-metrics, dice-cache (trait + Redis ConnectionManager + moka,
  RateLimiter fixed-window), dice-event-bus (typed subjects, local broadcast + NATS core,
  JetStream DICE_EVT created-not-consumed), dice-database (pool + 4 migrations).
- **57 tests passing** (54 unit + 3 live-infra integration: migrations create all 8 tables
  on real Postgres, Redis round-trip, NATS pub/sub round-trip).
- `cargo clippy --workspace --all-targets -- -D warnings` and `cargo fmt` clean.
- Six crates were built by parallel agents; protocol/common by hand. FrameDecoder::next
  renamed try_next (clippy should_implement_trait).

**Environment fixes along the way (important context):**
- C: drive was 100% FULL → moved npm cache to D:\npm-cache, removed Rust 1.81 toolchain
  (1.96.0 now default + pinned), moved Docker's corrupted 5.3 GB data disk to
  D:\DevCache\docker-old\docker_data.vhdx.bak (KEPT as backup — ask user before deleting).
  Docker recreated a fresh data disk; its system disk is at D:\DevCache\docker.
- Docker images from the out-of-space era were CORRUPTED (postgres "unknown user postgres") —
  fixed by the fresh data disk.
- **Native PostgreSQL 18 service owns host port 5432** → Dice postgres is published on
  **host port 5433** (compose + .env updated; DATABASE_URL=postgres://dice:dice_dev@localhost:5433/dice).
- just 1.52 + sqlx-cli 0.8.6 installed (cargo install with CARGO_TARGET_DIR=D:\tmp).

**Project status:** workspace compiles clean; infra (Postgres:5433/Redis/NATS) healthy via
compose; no .sqlx cache yet (no query! macros until services land — generate in Phase 2).

**Next milestone — Phase 2: WSS vertical slice (est. 3–4 d):**
Order: (1) service trait definitions in each service crate lib.rs (BINDING contracts for
agents); (2) network-core "server" feature: shared tls.rs (ring provider install,
RootCertStore, per-protocol configs with ALPN dice/1), tokio-rustls accept loop feeding
axum (REST+WSS, NO axum-server), quinn acceptor; dev-keygen (rcgen CA+leaf SANs
localhost/127.0.0.1/::1 → dev/certs/, Ed25519 JWT pems → dev/keys/) lives in monolith;
(3) auth/chat/presence service impls (sqlx query! → then `just sqlx-prepare` + commit .sqlx);
(4) api-gateway (session state machine, refcounted interest map + always-on user-subject
subs, per-session seq, replay ring 256/256KiB/60s, REST routes proto-over-HTTP, resume);
(5) dice-monolith wiring + graceful shutdown; (6) GATE: two Rust test clients chat over WSS
in dev-lite. Key resolutions: sends over gateway with nonce→ack; auth_session_id (JWT sid)
≠ gateway_session_id; chat-service auto-creates #general; presence to guild AND dm subjects;
typing ephemeral seq=0. See docs/protocol.md + docs/design/critique-integration.md.

---

## 2026-06-11 — Phase 0: Repo bootstrap (DONE)

**Branch:** `main` (only branch; milestone phases land directly on main while pre-release)

**Done so far:**
- `git init -b main`; first commit = `.gitattributes` + `.gitignore` (line-ending hygiene before any code).
- Root workspace `Cargo.toml` (resolver 3, edition 2024, full reconciled `[workspace.dependencies]` table,
  ring-only TLS policy, lints, dev/release profiles), `rust-toolchain.toml` pinned **1.96.0**
  (latest stable verified via `rustup check` today — supersedes the placeholder 1.88 pin in the plan),
  `rustfmt.toml`, `justfile` (dotenv-load, infra/db/sqlx/check/dev recipes incl. the `aws-lc-sys` gate),
  `.env.example`, `README.md`, `infrastructure/docker/docker-compose.yml`
  (postgres:17-alpine, redis:7.4-alpine, nats:2.11-alpine `-js`), `scripts/bootstrap.{ps1,sh}`.

**Remaining in Phase 0:**
- Placeholder READMEs (future services, kubernetes/terraform, benchmarks).
- `docs/` skeleton: getting-started, architecture, database, ADRs 0001–0005.
- `docs/protocol.md` — the NORMATIVE wire spec (must exist before any transport code).
- `docs/design/` — preserve the six milestone-1 design documents from the planning agents.
- `rustup` toolchain install (1.96.0 via rust-toolchain.toml), bootstrap.ps1 run, `cargo fetch` sanity.

**Project status:** scaffold only; no Rust code yet; workspace manifest references crates that
do not exist yet (created in Phase 1), so `cargo` commands will fail until Phase 1 starts.

**Next milestone — Phase 1: Protocol + shared crates (est. 1–2 d):**
Write `proto/dice/v1/*.proto` + `proto/dice/internal/v1/events.proto` exactly per
`docs/protocol.md`; then crates in dependency order: `dice-protocol` (vendored-protoc build.rs,
framing codec u32-BE/256 KiB + forward-compat tests) → `dice-common` (snowflake gen, env config,
shutdown) → `dice-permissions`, `dice-auth-core`, `dice-logging`, `dice-metrics`, `dice-cache`,
`dice-event-bus` (Local impl first, NATS second), `dice-database` (migrations 0001–0004).
Gate: `just infra-up && just db-setup && just sqlx-prepare`, commit `.sqlx/`,
whole workspace compiles with `SQLX_OFFLINE=true`. Master reference: the approved
local build plan and `docs/design/*`.
