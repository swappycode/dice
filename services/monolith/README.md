# dice-monolith

The M1 deployment shape: **one process** mounting auth-service, chat-service
and presence-service behind the api-gateway (REST + WSS on one TLS port, QUIC
on UDP), wired by direct trait calls — no inter-service RPC.

## Run

```powershell
# 1. infra (Postgres always; Redis/NATS only needed for `full`)
just infra-up

# 2a. fast dev loop: in-proc bus + memory cache, Postgres only
just dev            # DICE_PROFILE=dev-lite cargo run -p dice-monolith

# 2b. full backends: NATS bus + Redis cache
just run-full       # DICE_PROFILE=full cargo run -p dice-monolith
```

`just` loads `.env` (copy `.env.example`). A plain `cargo run -p dice-monolith`
needs at least `DATABASE_URL` in the environment.

On boot the monolith:

1. installs the rustls **ring** provider (workspace policy),
2. runs **dev-keygen**: if the configured TLS or JWT files are missing it
   generates and persists a dev CA + leaf into `dev/certs/` and an Ed25519
   JWT keypair into `dev/keys/` (idempotent — assets survive restarts so
   client trust and minted tokens stay valid),
3. connects Postgres and applies the embedded migrations,
4. starts the gateway and prints a banner with the bound addresses.

Point the desktop client at the printed addresses; it must trust
`dev/certs/dev-ca.pem` (`DICE_DEV_CA` — `just client` exports it).

Ctrl-C drains gracefully: live sessions receive `Close{GOING_AWAY}` (4011,
resumable) and the process exits within a 15 s hard deadline.

## Environment variables

All config is env-only (ADR-0002). The full table lives in `src/config.rs`
and `.env.example`; summary:

| Variable | Default | Meaning |
|---|---|---|
| `DICE_PROFILE` | `full` | `dev-lite` (local bus + memory cache) or `full` (NATS + Redis) |
| `DICE_BUS` / `DICE_CACHE` | per profile | per-backend overrides: `local\|nats`, `memory\|redis` |
| `DICE_NODE_ID` | `0` | snowflake node id (0..=1023) |
| `DATABASE_URL` | — required | Postgres; migrations run on boot |
| `DICE_DB_MAX_CONNS` | `16` | connection pool cap |
| `DICE_REDIS_URL` | `redis://localhost:6379` | cache backend `redis` |
| `DICE_NATS_URL` | `nats://localhost:4222` | bus backend `nats` |
| `DICE_REST_ADDR` | `0.0.0.0:8443` | HTTPS REST + `wss://…/gateway/v1` |
| `DICE_QUIC_ADDR` | `0.0.0.0:8444` | QUIC, ALPN `dice/1` |
| `DICE_ADMIN_ADDR` | `127.0.0.1:9600` | Prometheus `GET /metrics` |
| `DICE_TLS_CERT` / `DICE_TLS_KEY` | `dev/certs/server.{crt,key}` | auto-generated when missing |
| `DICE_JWT_PRIVATE_PEM` / `DICE_JWT_PUBLIC_PEM` | `dev/keys/jwt_ed25519{,.pub}.pem` | auto-generated when missing |
| `RUST_LOG` / `DICE_LOG_JSON` | `info,dice=debug` / unset | logging filter / NDJSON switch |

## Tests

`tests/wss_demo.rs` is the Phase-2 acceptance gate: the complete two-user
demo journey (register → Identify → guild create/join → message with
nonce-ack → typing → DM → REST history → cross-user presence → abrupt drop +
Resume replay) over WSS against **live Postgres** with dev-lite semantics,
all built in-process. It needs `DATABASE_URL` (or the workspace `.env`) and
a running Postgres (`just infra-up`).

```powershell
cargo test -p dice-monolith
```
