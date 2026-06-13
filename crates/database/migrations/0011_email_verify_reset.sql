-- Email verification + password reset (M2 item 11b).
--
-- `email_verified` tracks whether the address was confirmed (register sends a
-- verification mail; verification is currently informational, not a login gate).
ALTER TABLE users ADD COLUMN email_verified BOOLEAN NOT NULL DEFAULT FALSE;

-- Single-use, expiring opaque tokens for both flows. The token itself is an
-- opaque `dvt_`/`drst_` string sent by mail; only sha256(token) is stored (like
-- refresh tokens), so a DB leak can't be replayed. `purpose`: 1 = email-verify,
-- 2 = password-reset.
CREATE TABLE auth_tokens (
  id         BIGINT      PRIMARY KEY,                       -- snowflake
  user_id    BIGINT      NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  purpose    SMALLINT    NOT NULL,
  token_hash BYTEA       NOT NULL,
  expires_at TIMESTAMPTZ NOT NULL,
  used_at    TIMESTAMPTZ,
  created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE UNIQUE INDEX auth_tokens_hash_key ON auth_tokens (token_hash);
-- Look up / invalidate a user's live tokens of a given purpose.
CREATE INDEX auth_tokens_user_purpose_idx
  ON auth_tokens (user_id, purpose) WHERE used_at IS NULL;
