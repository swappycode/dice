# ADR-0002: Runtime config (env vars), not cargo features, for backend selection

**Status:** accepted (M1)

`DICE_PROFILE=dev-lite|full` (+ `DICE_BUS`, `DICE_CACHE` overrides) selects bus/cache backends at
runtime; each shared crate exposes a config enum and `connect()` returning `Arc<dyn Trait>`.

Why not cargo features: (1) the monolith must ship with both backends compiled anyway —
self-hosters pick at runtime, so features buy zero binary savings where it matters; (2) feature
unification leaks choices across the workspace and creates a CI matrix; (3) switching modes must
not require a rebuild; (4) one dynamic dispatch per event is noise next to network IO.

Config is plain env vars via a tiny loader in `dice-common` (documented in `.env.example`).
No figment/config-rs/TOML layering in M1 — it can be added later without touching call sites.
