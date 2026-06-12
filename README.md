# Dice

An ultra-lightweight, production-grade communication platform (Discord-class features,
Telegram-class efficiency) built in Rust: QUIC/WSS binary-protobuf gateway, tokio backend
services over PostgreSQL/Redis/NATS, and a native Tauri 2 + SolidJS desktop client with a
retro Windows XP/7 ("Luna"/"Aero") theme.

Targets: idle RAM < 100 MB, cold start < 2 s, idle CPU < 1%, 100k+ connections per gateway node.

- New here? Read [docs/getting-started.md](docs/getting-started.md).
- Roadmap (M2 polish → M3 voice → M4 scaling → M5 optimization): [docs/ROADMAP.md](docs/ROADMAP.md)
- Wire protocol (normative): [docs/protocol.md](docs/protocol.md)
- Architecture: [docs/architecture.md](docs/architecture.md)
- Database workflow: [docs/database.md](docs/database.md)
- Decision records: [docs/adr/](docs/adr/)

## Layout

| Path | What |
|---|---|
| `crates/` | Shared library crates (`dice-*`): protocol, network-core, event-bus, database, cache, auth-core, permissions, common, logging, metrics |
| `services/` | Backend services as library crates with thin bins + the `dice-monolith` all-in-one bin |
| `apps/desktop-client/` | Tauri 2 + SolidJS desktop client (own cargo workspace) |
| `proto/` | Protobuf schemas (`dice.v1` public, `dice.internal.v1` bus) |
| `infrastructure/docker/` | docker-compose for Postgres, Redis, NATS JetStream |
| `docs/design/` | Preserved milestone-1 design documents |

## Quick start

```powershell
./scripts/bootstrap.ps1   # installs just + sqlx-cli (one-time)
just infra-up             # Postgres (+Redis +NATS)
just db-setup
just dev                  # monolith, dev-lite profile
just client               # desktop client dev loop
```
