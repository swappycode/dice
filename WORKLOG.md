# Dice Worklog

Running log of milestone progress. Newest entry first. Each entry records what was done,
the current project status (and branch), and **what the next milestone is** so work can be
picked up with full context at any time. Update this file at every milestone boundary and
whenever direction changes; keep git commits small and per-logical-unit so `git log` mirrors it.

---

## 2026-06-11 — Phase 0: Repo bootstrap (IN PROGRESS)

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
