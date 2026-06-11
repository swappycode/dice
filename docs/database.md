# Database Workflow

One PostgreSQL database, one linear migration history in `crates/database/migrations/`,
embedded via `sqlx::migrate!` (the monolith self-migrates on boot in dev). Files are namespaced
by owning service (`0001_users.sql`, `0003_guilds_channels.sql`); services keep their *queries*
(`sqlx::query!` macros) in their own crates.

## Offline compile-time checking (the rules)
- `.sqlx/` at the workspace root is **committed**. Everyone (and CI) builds with
  `SQLX_OFFLINE=true` and never needs a live database.
- You touched a query or a migration → run `just sqlx-prepare` (starts from `db-setup`,
  needs docker Postgres) → **commit the `.sqlx/` diff**.
- CI runs `just sqlx-check` to fail stale caches.
- `sqlx-cli` is pinned to the same 0.8.x minor as the workspace (`scripts/bootstrap.ps1`);
  a mismatched cli/workspace pair corrupts the cache format.

## Conventions
- Snowflake PKs everywhere (`BIGINT`, bit 63 always 0; `created_at` derivable from the id —
  message rows have no created_at column).
- `channels.channel_type` stores the **proto enum value verbatim** (1=GUILD_TEXT, 2=DM).
- `guild_members.permissions BIGINT NOT NULL` — value always supplied from Rust
  (`Permissions::to_db()`), never a SQL magic literal.
- Keyset pagination only (`WHERE channel_id=$1 AND id < $before ORDER BY id DESC LIMIT $n`),
  never OFFSET; backed by the `(channel_id, id DESC)` index.
