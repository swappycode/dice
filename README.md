# 🎲 Dice

**An ultra-lightweight, retro-themed Discord alternative — built in Rust.**

Discord-class features with Telegram-class efficiency: a QUIC + secure-WebSocket binary-protobuf
gateway, async Rust backend services over PostgreSQL / Redis / NATS, and a native **Tauri 2 +
SolidJS** desktop client wrapped in a funky **Windows XP / Windows 7 retro UI**.

> **Status:** Milestones 1–4 complete — auth, guilds/channels/DMs, real-time messaging, typing,
> presence, resume-after-disconnect, **voice end-to-end** (SFU over QUIC datagrams), themes and
> friends, all over both QUIC and WSS. **M4 (scaling)**: the backend runs as a monolith **or** split
> microservices from one codebase; **one gateway node sustains 30k concurrent connections at ~44 KB
> each** (≈4.7 GB extrapolated to 100k); **cross-node resume** survives a gateway node's death
> (redirect to the live owner, or re-host the session from a durable snapshot elsewhere); + a
> transactional outbox, lazy member lists, full observability (metrics/traces), and real
> Kubernetes/Terraform deploy manifests. 200+ backend tests + a headless client gate, all green.
> See [WORKLOG.md](WORKLOG.md) for the build log. **M5 (optimization & hardening) is in progress** —
> TOTP-secret encryption-at-rest (AES-256-GCM, key derived from the signing seed) landed first.

## Why

Discord idles at 400–800 MB of RAM. Dice targets a fraction of that by going native: no Electron,
a binary protocol instead of JSON on the wire, zero-copy where it counts, and a single
self-hostable binary for the whole backend.

| Target | Status (measured) |
|---|---|
| Cold start < 2 s | ✅ ~1.5 s |
| Idle CPU < 1% | ✅ ~0.05% |
| Idle RAM < 100 MB | ⚠️ ~170 MB (Rust host is ~5.5 MB; the rest is the WebView2 floor — the headline M2 item) |
| 100k+ connections / gateway node | ✅ 30k held @ ~44 KB/conn (0 fails, 0 shedding, 1 ms hb-RTT) → ~4.7 GB extrapolated to 100k/node |

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
| `infrastructure/` | `docker/` (compose for Postgres/Redis/NATS + the opt-in Prometheus/Grafana/Tempo stack + the bin Dockerfile), `kubernetes/` (split-fleet manifests: gateway StatefulSet, services, LB), `terraform/` (namespace, secrets, backing stores) |
| `docs/` | Getting started, normative protocol spec, ADRs, design docs, roadmap |

## Documentation

- [docs/getting-started.md](docs/getting-started.md) — run it, test two users, gotchas
- [docs/protocol.md](docs/protocol.md) — the normative wire protocol
- [docs/architecture.md](docs/architecture.md) · [docs/database.md](docs/database.md) · [docs/adr/](docs/adr/)
- [WORKLOG.md](WORKLOG.md) — the running build log

## Development

```powershell
just check        # the full gate: fmt + clippy -D warnings + tests + the aws-lc-sys guard
```

CI-grade rules: ring is the only TLS crypto provider (the gate fails if `aws-lc-sys` enters the
tree), and the committed `.sqlx/` cache keeps `cargo check` fully offline.

## Observability

Every backend process exposes Prometheus metrics on its admin port (`DICE_ADMIN_ADDR`, default
`:9600`): gateway connections by transport, frames in/out by class, closes by code, chat messages,
event-bus drops, and DB-pool gauges — plus, in split mode, **per-method RPC latency**
(`dice_rpc_request_seconds{service,method}`) on each service bin (`:9601`/`:9602`/`:9603`).

```powershell
just infra-up && just run-full   # or `just split-up` for the per-service RPC panels
just metrics-up                  # Prometheus (:9090) + Grafana (:3000)
```

Open Grafana at `http://localhost:3000` (anonymous) — the **Dice — Gateway & Services** dashboard
is provisioned (connections, frame & message rates, RPC p50/p99 per service, close codes, pool
saturation). Stop with `just metrics-down`. Host ports are overridable via `DICE_PROMETHEUS_PORT` /
`DICE_GRAFANA_PORT` if 9090/3000 are taken.

**Distributed tracing.** With `DICE_OTLP_ENDPOINT` set (which `just split-up` does), services also
export OpenTelemetry spans to **Tempo** (bundled in the same stack). A request's trace context
rides the split-mode NATS-RPC boundary as a W3C `traceparent`, so Grafana's **Explore → Tempo** view
shows one end-to-end trace per request — a gateway `rpc.client` span linked to the target service's
`rpc.server` span (auth / chat / presence). Off by default; no exporter, no overhead, when unset.

## License

Dual-licensed under **MIT** or **Apache-2.0**, at your option. See [LICENSE-MIT](LICENSE-MIT) and
[LICENSE-APACHE](LICENSE-APACHE).

The retro look is *inspired by* the Windows XP/7 eras — it ships **no** Microsoft artwork, fonts, or
sounds (all gradients/SVGs are original). "Luna" and "Aero" are internal theme codenames. Dice is
not affiliated with or endorsed by Microsoft. See [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md).
