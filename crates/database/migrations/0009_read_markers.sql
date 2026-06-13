-- Read markers (M2 item 10). Server-side per-(user, channel) last-read pointer,
-- so "read" state survives a cache flush and SYNCS across a user's devices (the
-- mark-read path broadcasts ReadMarkerUpdate to the user's own subject). The
-- M1 cut kept this client-local; this re-introduces the backend half.
CREATE TABLE read_markers (
  user_id              BIGINT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  channel_id           BIGINT NOT NULL REFERENCES channels(id) ON DELETE CASCADE,
  last_read_message_id BIGINT NOT NULL,
  updated_at           TIMESTAMPTZ NOT NULL DEFAULT now(),
  PRIMARY KEY (user_id, channel_id)
);
