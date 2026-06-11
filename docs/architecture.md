# Architecture

```
 Tauri client (Rust core) ──QUIC 8444 / WSS+REST 8443──► api-gateway ──Arc<dyn Trait>──► auth / chat / presence services
        │  webview = pure renderer                            │  ▲                              │
        │  (SolidJS, retro Luna/Aero UI)                      │  └── events ── dice-event-bus ──┘
        └── SQLite cache (rusqlite worker thread)             │        (in-proc broadcast OR NATS)
                                                              ▼
                                                    PostgreSQL (truth)   Redis/moka (cache)
```

## Deploy shapes
- **Monolith** (`services/monolith`, pkg `dice-monolith`): all four service libs in one process,
  shared PgPool/cache/bus, direct trait calls. The self-hosting story and the M1 demo target.
- **Split bins**: each service has a thin bin. They compile and start in M1; the cross-service
  NATS request-reply interconnect is M2 (the `Arc<dyn Auth/Chat/Presence>` trait seam makes it additive).

## Crate dependency sketch
`common` ← `protocol` (no tokio!) ← {`event-bus`, `network-core`} ;
`permissions`, `auth-core`, `cache`, `database`, `logging`, `metrics` ← services ← `monolith`.
The desktop client (`apps/desktop-client/src-tauri`) is its OWN workspace (different profiles,
avoids compiling Tauri in backend dev loops) and path-depends on `protocol`, `common`, `network-core`.

## Profiles (runtime config, not cargo features)
`DICE_PROFILE=dev-lite|full` selects defaults; `DICE_BUS=local|nats`, `DICE_CACHE=memory|redis`
override per-backend. Postgres is always required (sqlx compile-time queries). See ADR-0002/0003.

## Scaling properties built in from M1
- Gateway interest-map fan-out: ONE bus subscription per subject per node (not per session).
- Per-session bounded outbound queue (128) — slow consumers are disconnected (resumable), never buffered.
- Pre-encoded event payloads shared via refcounted `Bytes`.
- JWT verified locally at the gateway (Ed25519 public key) — zero auth round-trips on connect.
- Binary protobuf everywhere on the wire; replay-buffer-based resume; REST backfill as the safety net.

The five ADRs in [adr/](adr/) record the decisions; [design/](design/) preserves the full
milestone-1 design documents these were distilled from.
