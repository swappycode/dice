# 🎲 Dice

**An ultra-lightweight, retro-themed Discord alternative — built in Rust.**

Discord-class features with Telegram-class efficiency: a QUIC + secure-WebSocket binary-protobuf
gateway, async Rust backend services over PostgreSQL / Redis / NATS, and a native **Tauri 2 +
SolidJS** desktop client wrapped in a funky **Windows XP / Windows 7 retro UI**.

> **Status:** Milestone 1 complete — register, log in, guilds & channels, DMs, real-time
> messaging, typing indicators, presence, and resume-after-disconnect all work end-to-end over
> both QUIC and WSS. ~200 backend tests + a headless client gate, all green. See
> [WORKLOG.md](WORKLOG.md) for the build log and [docs/ROADMAP.md](docs/ROADMAP.md) for what's next.

## Why

Discord idles at 400–800 MB of RAM. Dice targets a fraction of that by going native: no Electron,
a binary protocol instead of JSON on the wire, zero-copy where it counts, and a single
self-hostable binary for the whole backend.

| Target | Status (M1, measured) |
|---|---|
| Cold start < 2 s | ✅ ~1.5 s |
| Idle CPU < 1% | ✅ ~0.05% |
| Idle RAM < 100 MB | ⚠️ ~170 MB (Rust host is ~5.5 MB; the rest is the WebView2 floor — the headline M2 item) |
| 100k+ connections / gateway node | architecture in place (interest-map fan-out, bounded per-session memory) |

## Highlights

- **Two transports, one protocol** — QUIC (quinn) first with automatic secure-WebSocket fallback,
  behind one shared codec. TLS 1.3 everywhere (rustls, **ring** provider — no NASM/CMake toolchain pain).
- **Binary protobuf wire format** with a single `Frame` envelope, sequenced dispatch + resumable
  sessions (replay buffer), and snowflake IDs. The wire spec is normative: [docs/protocol.md](docs/protocol.md).
- **Self-hostable monolith** — every service is a library crate with a thin bin, plus one
  `dice-monolith` that runs the whole backend in one process (auto-generates dev TLS + JWT keys on
  first boot). Scale out to separate services later without a rewrite.
- **Offline-first native client** — Tauri 2 host does all networking/crypto/storage in Rust;
  a rusqlite cache paints the UI instantly before the gateway syncs; OS-keyring sessions.
- **Retro UI that stays light** — hand-rolled CSS token system with switchable **Luna** (XP) and
  **Aero** (Win7) themes; system fonts, no raster images, no webfonts, total CSS ~31 KB. (More
  themes are coming in M2 — see the roadmap.)

## Tech stack

Rust (edition 2024) · tokio · quinn (QUIC) · axum + tokio-tungstenite (WSS/REST) · prost (protobuf)
· sqlx + PostgreSQL · Redis / moka · NATS JetStream · Argon2id + Ed25519 JWT · Tauri 2 · SolidJS +
Vite · rusqlite.

## Quick start (Windows / macOS / Linux; Docker + Rust + Node 22 required)

```powershell
./scripts/bootstrap.ps1   # one-time: installs `just` + sqlx-cli
Copy-Item .env.example .env
just infra-up             # Postgres (+ Redis + NATS) via docker compose
just db-setup             # create DB + run migrations
just dev                  # the backend monolith (dev-lite profile)
just client               # the desktop client (separate terminal)
```

Then register an account on the retro login screen and start chatting. Full guide:
[docs/getting-started.md](docs/getting-started.md).

### Test it with two users locally

```powershell
just client-build              # build the release client once
just client-as alice           # window 1  (own cache + keyring)
just client-as bob             # window 2
```

Create a guild in alice's window, copy the **Invite:** code from the channel header, and join it
from bob's window. (Tip: a browser tab at `localhost:1420` runs in *mock mode* with fake data — use
two real `client-as` windows for real users.)

## Repository layout

| Path | What |
|---|---|
| `crates/` | Shared library crates (`dice-*`): protocol, network-core, event-bus, database, cache, auth-core, permissions, common, logging, metrics |
| `services/` | Backend services (auth, chat, presence, api-gateway) as libs + thin bins, plus the `dice-monolith` all-in-one bin |
| `apps/desktop-client/` | Tauri 2 host (`src-tauri/`) + SolidJS frontend (`src/`) — its own cargo workspace |
| `proto/` | Protobuf schemas (`dice.v1` client-facing, `dice.internal.v1` bus) |
| `infrastructure/docker/` | docker-compose for Postgres, Redis, NATS JetStream |
| `docs/` | Getting started, normative protocol spec, ADRs, design docs, roadmap |

## Documentation

- [docs/getting-started.md](docs/getting-started.md) — run it, test two users, gotchas
- [docs/ROADMAP.md](docs/ROADMAP.md) — M2 (polish + more themes) → M3 (voice) → M4 (scaling) → M5 (optimization)
- [docs/protocol.md](docs/protocol.md) — the normative wire protocol
- [docs/architecture.md](docs/architecture.md) · [docs/database.md](docs/database.md) · [docs/adr/](docs/adr/)
- [WORKLOG.md](WORKLOG.md) — the running build log

## Development

```powershell
just check        # the full gate: fmt + clippy -D warnings + tests + the aws-lc-sys guard
```

CI-grade rules: ring is the only TLS crypto provider (the gate fails if `aws-lc-sys` enters the
tree), and the committed `.sqlx/` cache keeps `cargo check` fully offline.

## License

Dual-licensed under **MIT** or **Apache-2.0**, at your option. See [LICENSE-MIT](LICENSE-MIT) and
[LICENSE-APACHE](LICENSE-APACHE).

The retro look is *inspired by* the Windows XP/7 eras — it ships **no** Microsoft artwork, fonts, or
sounds (all gradients/SVGs are original). "Luna" and "Aero" are internal theme codenames. Dice is
not affiliated with or endorsed by Microsoft. See [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md).
