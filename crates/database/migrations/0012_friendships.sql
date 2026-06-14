-- Friendships (M3). One row per user-pair, normalized so user_lo < user_hi (the
-- pair is symmetric; storing it ordered makes the PK enforce a single row per
-- pair regardless of who asked). `status`: 1 = pending, 2 = accepted.
-- `requester_id` is whoever sent the request — kept after accept for provenance
-- and so each side can render incoming vs outgoing while pending.
CREATE TABLE friendships (
  user_lo      BIGINT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  user_hi      BIGINT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  status       SMALLINT NOT NULL,
  requester_id BIGINT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
  updated_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
  CHECK (user_lo < user_hi),
  PRIMARY KEY (user_lo, user_hi)
);

-- "list everything involving user X" scans both columns (a friendship for X may
-- store X as either the lo or the hi member), so index each.
CREATE INDEX friendships_user_lo_idx ON friendships (user_lo);
CREATE INDEX friendships_user_hi_idx ON friendships (user_hi);
