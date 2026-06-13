-- TOTP 2FA (M2 item 11a). RFC 6238 authenticator-app second factor.
--
-- `totp_secret` is the base32 shared secret. It cannot be hashed (we must
-- regenerate codes to verify), so it is stored like a credential-at-rest, in the
-- same trust tier as `password_hash`; encryption-at-rest is a future hardening.
-- `totp_enabled` gates the login challenge: a secret may exist (enrollment begun)
-- without 2FA being active until the user confirms a code. `totp_last_step` is the
-- last TOTP time-step consumed at a successful login; a code at step <= this is
-- rejected (RFC 6238 §5.2 single-use, our own replay guard).
ALTER TABLE users
  ADD COLUMN totp_secret    TEXT,
  ADD COLUMN totp_enabled   BOOLEAN NOT NULL DEFAULT FALSE,
  ADD COLUMN totp_last_step BIGINT;

-- One-time recovery codes shown once at enrollment confirm; each row stores only
-- sha256(normalized code) (high-entropy, so a direct hashed lookup is fine, like
-- refresh tokens). A login may present a recovery code in place of a TOTP code;
-- it is then marked used and never accepted again.
CREATE TABLE totp_recovery_codes (
  id        BIGINT PRIMARY KEY,                                  -- snowflake
  user_id   BIGINT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  code_hash BYTEA  NOT NULL,
  used_at   TIMESTAMPTZ
);
CREATE INDEX totp_recovery_user_idx ON totp_recovery_codes (user_id) WHERE used_at IS NULL;
