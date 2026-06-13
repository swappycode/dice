# Dice — handoff

Orientation for whoever picks this up next. Living doc; update it at each
milestone close.

**As of 2026-06-14** — Milestone 2 is **complete** (all 15 items shipped,
tested, pushed). Branch `main`, HEAD `c7ff5f4`, working tree clean. Next up is
**M3 (Voice)** — see `docs/ROADMAP.md`.

---

## First 15 minutes

Read in this order:
1. **`WORKLOG.md`** — top entry is the source of truth for "what just happened +
   what's next". Entries `M2 (1/n)…(9/n)` cover all of M2.
2. **`docs/ROADMAP.md`** — the whole arc (M1✅, M2✅, M3 voice, M4 scaling,
   M5 hardening) + the M2 *Carried follow-ups* list.
3. **`docs/testing-m2.md`** — drive every feature by hand.
4. **`docs/design/`** — the normative designs + reserved seams
   (`backend-services.md`, `workspace-and-protocol.md`, `desktop-client.md`,
   `retro-ui.md`, `m2-theme-pack.md`). `docs/protocol.md` is the wire contract.
5. **`docs/adr/`** — architecture decisions.

---

## What this is

A retro-styled (Windows XP/Aero) Discord-like chat app.

- **Backend**: a Rust workspace of service *library* crates behind `Arc<dyn Trait>`
  seams (`auth-service`, `chat-service`, `media-service`, `notification-service`,
  `presence-service`) + an `api-gateway` (REST + QUIC/WSS realtime), all mounted
  in one `dice-monolith` bin. Postgres is the source of truth; an `EventBus`
  fabric (in-process **or** NATS) carries fan-out events; a `Cache` (in-memory or
  Redis) holds ephemeral state.
- **Client**: a Tauri 2 desktop app. The Rust **host** (`apps/desktop-client/
  src-tauri`) owns networking/cache/keyring and exposes `#[tauri::command]`s; the
  **UI** is SolidJS + CSS-module themes (`apps/desktop-client/src`). They talk
  over a single `DiceIpc` seam (`src/lib/ipc.ts`) with a real impl
  (`ipc.real.ts` → invoke) and an in-memory **mock** (`ipc.mock.ts`) for browser
  demos — keep the two in parity.

### Workspace shape (important)

- Root `Cargo.toml` is the server workspace. `services/*` is **not** a glob
  (placeholder dirs would break it).
- `apps/desktop-client/src-tauri` is a **separate workspace** (excluded from
  root) so the heavyweight Tauri/webview deps never taint server builds. It has
  its **own** `Cargo.lock` and its **own** gate run.
- Wire types live in `crates/protocol` (prost-generated from `proto/`), shared by
  server, host, and (via the host) the client.

### Runtime profiles

`DICE_PROFILE=dev-lite` (in-proc bus + memory cache, Postgres only) vs `full`
(NATS + Redis). The durable unread-count consumer (`notification-service`) only
runs under `full`.

---

## Dev loop & gates

```powershell
just infra-up                 # Postgres :5433, Redis :6379, NATS :4222
just dev                      # monolith, dev-lite (or just run-full)
just client                   # desktop client (HMR)
```

**Every commit must pass all three gate sets** (this has been the discipline
throughout):

```powershell
just check                                         # fmt + clippy -D warnings + cargo test + aws-lc gate
cd apps/desktop-client/src-tauri; cargo clippy --all-targets -- -D warnings; cargo test
cd apps/desktop-client; npm run check; npm run build
```

`just check` needs infra up — much of the suite is live-infra integration tests.
See `docs/testing-m2.md §6` for the full list incl. `just sqlx-prepare`.

---

## Gotchas that will bite you

- **Ring-only TLS.** The whole tree pins the `ring` rustls provider; `just check`
  runs `gate-aws-lc` to fail the build if `aws-lc-sys` ever enters the tree
  (NASM/CMake trap on Windows). Any new dep that pulls a TLS/S3 SDK is suspect —
  check `cargo tree -i aws-lc-sys`.
- **sqlx is compile-time checked.** After changing any `sqlx::query!` or adding a
  migration, run `just sqlx-prepare` (applies migrations to the dev DB **and**
  regenerates the committed `.sqlx/` cache) or offline builds fail. Migrations are
  `crates/database/migrations/000N_*.sql` (latest: `0011_email_verify_reset`); the
  manifest test in `crates/database/src/lib.rs` asserts the exact list — update it
  when you add one.
- **Realtime dispatch numbers are a registry.** New `Frame` dispatch events get a
  number in `crates/protocol/src/lib.rs` `payload_field_number()` — **next free =
  117** (highest used = `ReadMarkerUpdate` 116). Adding one ripples to the cache
  `apply_event`, the host `on_dispatch`, the `DiceEvent` enum (Rust **and** TS),
  and the frontend dispatcher.
- **Adding a field to a shared proto message ripples widely** — every struct
  literal of it across services/host/tests. (e.g. `User.avatar_id` touched ~all
  `v1::User{}` sites.) Search before you add.
- **`GatewayDeps` is constructed in 6 places** (monolith + 5 test harnesses:
  `gateway_e2e`, `wss_demo`, `client_e2e`, `host_gate`, and the auth/presence
  harnesses). A new dep field ripples to each.
- **`AuthService::new` keeps a stable signature**; the `Mailer` is injected via a
  defaulted `.with_mailer(...)` builder precisely so those call sites don't churn.
  Prefer that pattern for new optional deps.
- **No SMTP.** Auth emails (verify/reset) go through `LogMailer` — tokens are
  *logged*, not sent (see `docs/testing-m2.md §2`). Swap the `Mailer` impl for
  real SMTP later.
- **Client ↔ host contract is by-hand.** `commands.rs` names + DTO shapes
  (`dto.rs`, snake_case fields auto-mapped from camelCase JS) must match
  `ipc.real.ts`/`ipc.ts` exactly. Every id crosses IPC as a **string** (u64
  snowflakes overflow JS numbers).
- **Perf guardrails (UI):** tokens only (no raw hex in components), no raster
  images, no webfonts, no infinite animations except the typing dots; CSS budget
  ~under 100 KB (currently ~48 KB). `perf-mode` + `prefers-reduced-motion` +
  `html.app-idle` must keep working.

---

## M2 feature map (where things live)

| Area | Key files |
|---|---|
| TOTP 2FA | `crates/auth-core/src/{totp.rs,token.rs}`, `services/auth-service/src/service.rs`, gateway `rest.rs`, client `dialogs/SecurityDialog.tsx` + `auth/LoginCard.tsx` |
| Email verify / password reset | `services/auth-service/src/{mailer.rs,service.rs}`, migration `0011`, `LoginCard.tsx` (forgot-pw), `SecurityDialog.tsx` (verify) |
| Themes + funk | `apps/desktop-client/src/styles/{tokens,recipes}.css`, `src/themes/*.css`, `src/lib/theme.ts`, `chrome/StatusBar.tsx` |
| Chime + OS toast | `src/lib/chime.ts`, `gateway/dispatcher.ts`, host `commands.rs` (`notify`) + `lib.rs` (plugin), `ipc.*` |
| Split-mode RPC | `crates/event-bus/src/rpc.rs`, `proto/dice/internal/v1/rpc.proto`, `services/presence-service/src/rpc.rs` (+ `tests/presence_rpc.rs`) |

---

## Outstanding work

**M2 carried follow-ups** (small, optional — full list in `ROADMAP.md`):
- Auth + Chat over split-mode RPC + the `services/*/src/bin/*.rs` split bins +
  a NATS-client `api-gateway` bin (Presence is the worked example to copy).
- Orphaned-media GC sweep.
- "Unread divider line" UI (needs `last_read_message_id` exposed to the client).
- Email-verify as an enforced login gate (currently informational).
- TOTP-secret encryption-at-rest (`users.totp_secret` is plaintext today).

**Next milestone — M3 (Voice):** `voice-core` + `voice-service` SFU over QUIC
datagrams; the upgrade path is already reserved via `Identify.capabilities` bit 0
and a `VOICE (later)` channel-tree section. See `ROADMAP.md` M3.

---

## Memory (assistant continuity)

If you're an AI assistant resuming this: the persistent memory index is
`~/.claude/projects/d--Dice/memory/MEMORY.md`; `dice-m2-progress.md` has the
current state + carried follow-ups, and `worklog-and-git-discipline.md` records
the per-milestone-log + frequent-commit convention this repo follows.
