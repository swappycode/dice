# Milestone 1 — QA Test Procedure

Repeatable acceptance test for M1 (auth, gateway, guilds/channels, DMs, realtime messaging,
presence; QUIC+WSS; retro desktop client). Run on Windows 11 from a clean `main` checkout at or
after `378c1af`. Companion docs: [getting-started.md](getting-started.md),
[protocol.md](protocol.md), `WORKLOG.md` (M1 wrap-up entry).

## Preconditions

- Docker Desktop running; Dice Postgres publishes on host **5433** (a native PostgreSQL owns
  5432 — never touch that service).
- `.env` exists (`Copy-Item .env.example .env` if missing).
- `just` + `sqlx-cli` installed (`./scripts/bootstrap.ps1`).
- Dev TLS certs + JWT keys auto-generate under `dev/` on first monolith boot.
- Verify `git log --oneline` tip ≥ `378c1af` and `git status --porcelain` is empty.

## Phase A — automated gates (hard pass/fail)

| # | Command (repo root unless noted) | Pass criteria |
|---|---|---|
| A1 | `docker compose -f infrastructure/docker/docker-compose.yml up -d --wait` | exit 0; postgres, redis, nats all healthy |
| A2 | `just check` | exit 0; record total test count; ends with `aws-lc-sys gate: clean`. Includes E2E suites `wss_demo_phase2_gate` + `gateway_e2e` (two-user chat, typing, presence, DM, history, kill-socket resume-with-replay over WSS and QUIC) |
| A3 | `cd apps/desktop-client/src-tauri; cargo test` | 14 tests, 0 failures (incl. `host_gate`: optimistic send → sqlite pending → ack reconcile → offline restart serves cache) |
| A4 | `cd apps/desktop-client; npx tsc --noEmit; npm run build` | both exit 0; built CSS total < 100 KB |
| A5 | `$env:SQLX_OFFLINE='true'; cargo check --workspace` | exit 0 (committed `.sqlx/` cache is complete) |

## Phase B — live boot + perf (measure, don't eyeball)

1. Free ports: `Get-NetTCPConnection -LocalPort 8443,9600 -State Listen`; kill stray
   `dice-monolith` only.
2. `just dev` (background) → banner shows profile **dev-lite**, rest+wss **:8443**, quic
   **:8444**; `Invoke-WebRequest http://127.0.0.1:9600/metrics` → 200.
3. Release client (`apps/desktop-client/src-tauri/target/release/dice-desktop.exe`; build with
   `cargo build --release` if absent — needs `dist/` from A4 first). Launch with
   `DICE_DEV_CA=<repo>\dev\certs\dev-ca.pem`.

| Metric | Method | Target |
|---|---|---|
| Cold start | `Start-Process` → first `msedgewebview2` child of the host pid | **< 2 s** |
| Idle host RAM (60 s) | host `PrivateMemorySize64` | ≈ 5–10 MB |
| Idle tree RAM (60 s) | Σ `PrivateMemorySize64` over host + its WebView2 descendants | record; known M1 result ≈ 170 MB vs < 100 MB goal — accepted miss; regression only if > +20% (> ~204 MB) |
| Idle CPU | Δ `TotalProcessorTime` over 10 s ÷ (10 s × logical cores) | **< 1%** |

## Phase C — functional walkthrough

One app window plus a scripted second user (simultaneous second instance is a known M1 gap).

1. Register a fresh account (email-shaped email, lowercase username, 8+ char password) → lands
   in shell; status bar reads **Connected (QUIC)**.
2. Create a guild via the `[+]` rail tile → `#general` auto-exists; channel header shows the
   beveled **Invite: CODE** chip; clicking it shows "Copied!" and puts the code on the clipboard.
3. Send messages → instant echo. Close the app, stop the monolith, relaunch the app: history
   paints instantly from the sqlite cache while offline. Restart the monolith: the status bar
   recovers to Connected on its own.
4. Theme dropdown Luna ↔ Aero: full live restyle, no reload, no broken colors.
5. Relaunch with `DICE_TRANSPORT=wss` → status bar reads **Connected (WSS)**.
6. Second-user realtime: run `cargo test --test host_gate -- --nocapture` in
   `apps/desktop-client/src-tauri` (registers a second raw WSS user that joins and messages
   live), and/or in-app: log off via the footer, register a second account, join the first
   guild via the invite code, send a message, log back in as user 1 and verify it is in history.
   Note: both accounts share one local cache (known M1 limitation) — minor cross-account cache
   oddities here are not failures.
7. Dev-loop connectivity (regression watch): with `just dev` running, `just client` must attach
   to **that** monolith — verify a register/login from the dev client appears in the monolith
   log. A client that "works" without traffic in the monolith log is running the mock IPC
   (`VITE_MOCK_IPC`/browser path) — that is the historical "connected to a different server"
   trap; see `src/lib/ipc.ts` selection rules.

## Known-and-accepted M1 gaps (do NOT count as failures)

Idle RAM > 100 MB (WebView2 floor); no simultaneous second instance; per-IP rate limits inert
(ip=None); no friends system (invite codes are the connect mechanism); no message edit/delete;
no voice (M3).

## Report format

One table per phase (`check | expected | observed | PASS/FAIL`), perf numbers vs targets, any
deviations from the docs, final verdict. Leave the working tree clean (`git status`) and stop
the monolith afterwards.
