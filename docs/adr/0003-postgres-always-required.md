# ADR-0003: PostgreSQL is always required (no in-memory DB mode)

**Status:** accepted

The hybrid dev mode swaps the bus (NATS → in-proc broadcast) and cache (Redis → moka), but never
the database. sqlx compile-time-checked queries are pinned to real Postgres semantics; an
in-memory or SQLite stand-in would fork every query path into a second implementation that rots
and hides bugs. Migrations, DM dedup (partial unique index on `dm_key`), and keyset pagination
rely on real SQL. One `docker compose up postgres` container (~50 MB RAM) is the accepted dev cost;
the expensive dev friction (Redis semantics, NATS streams) is what the memory fallbacks remove.
Offline builds still work via the committed `.sqlx/` cache (see docs/database.md).
