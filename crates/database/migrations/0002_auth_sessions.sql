-- auth_sessions: one row per login; id IS the JWT `sid` claim and lives as
-- long as the refresh-token family. Distinct from gateway_session_id (which
-- is gateway-minted, Resume-only, and never persisted here).
CREATE TABLE auth_sessions (
  id          BIGINT PRIMARY KEY,                    -- snowflake; JWT `sid`
  user_id     BIGINT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  created_ip  INET,
  user_agent  TEXT,
  created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
  revoked_at  TIMESTAMPTZ
);

-- Hot path is "active sessions of user X" (login listing, mass revocation).
CREATE INDEX auth_sessions_user_idx ON auth_sessions (user_id) WHERE revoked_at IS NULL;

-- refresh_tokens: opaque drt_* tokens, stored as sha256(raw) only.
-- Rotation chain: rotated_at + replaced_by record the family history so that
-- reuse of a rotated token is detectable (theft) and revokes the whole session.
CREATE TABLE refresh_tokens (
  id          BIGINT PRIMARY KEY,                    -- snowflake
  session_id  BIGINT NOT NULL REFERENCES auth_sessions(id) ON DELETE CASCADE,
  token_hash  BYTEA  NOT NULL,                       -- sha256(raw token)
  issued_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
  expires_at  TIMESTAMPTZ NOT NULL,
  rotated_at  TIMESTAMPTZ,
  replaced_by BIGINT REFERENCES refresh_tokens(id)
);

CREATE UNIQUE INDEX refresh_tokens_hash_key ON refresh_tokens (token_hash);
CREATE INDEX refresh_tokens_session_idx     ON refresh_tokens (session_id);
