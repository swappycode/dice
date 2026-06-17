-- Transactional outbox for the chat message path (M4).
--
-- Every durable message event (create / edit / delete) is recorded here inside
-- the SAME transaction as the write it accompanies, so a committed write can
-- never lose its event. A best-effort inline publish delivers it immediately
-- and stamps `published_at`; a relay reconciles any row the inline publish
-- missed (process crash, bus unavailable) — at-least-once delivery, idempotent
-- by `event_id`. See docs/adr/0006-transactional-outbox.md.
CREATE TABLE event_outbox (
    -- BusEvent.event_id (snowflake; bit 63 always 0, so it fits BIGINT). The PK
    -- gives INSERT-time dedup if an event is ever re-recorded.
    event_id     BIGINT      PRIMARY KEY,
    -- The bus Subject in its canonical `dice.evt.*` string form (Display/FromStr
    -- round-trips), so the relay can republish to the exact same subject.
    subject      TEXT        NOT NULL,
    -- prost-encoded BusEvent — the exact bytes the bus would have carried.
    payload      BYTEA       NOT NULL,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    -- NULL until the event has been delivered to the bus (inline or by the relay).
    published_at TIMESTAMPTZ
);

-- The relay scans only unpublished rows, oldest first; a partial index keeps
-- that scan independent of the (growing) set of already-published rows.
CREATE INDEX event_outbox_unpublished ON event_outbox (created_at) WHERE published_at IS NULL;
