# Dice Worklog

Running log of milestone progress. Newest entry first. Each entry records what was done,
the current project status (and branch), and **what the next milestone is** so work can be
picked up with full context at any time. Update this file at every milestone boundary and
whenever direction changes; keep git commits small and per-logical-unit so `git log` mirrors it.

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
