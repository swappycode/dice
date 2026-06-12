# Dice Worklog

Running log of milestone progress. Newest entry first. Each entry records what was done,
the current project status (and branch), and **what the next milestone is** so work can be
picked up with full context at any time. Update this file at every milestone boundary and
whenever direction changes; keep git commits small and per-logical-unit so `git log` mirrors it.

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
The full original agent prompt (richer detail) is preserved in the workflow script:
`C:\Users\HP\.claude\projects\D--Dice\ad09a65c-eb63-4f90-afb2-72d383a4fd90\workflows\scripts\dice-phase3-client-wf_2a494195-a40.js`
(TauriHost phase). Review/fix the partial files against it before continuing.

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
whole workspace compiles with `SQLX_OFFLINE=true`. Master reference: the approved plan
(`C:\Users\HP\.claude\plans\master-prompt-build-cheeky-stream.md`) and `docs/design/*`.
