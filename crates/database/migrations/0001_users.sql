-- users: account identity. Owned by auth-service; queried by every service.
-- id is a Dice snowflake (bit 63 always 0, fits BIGINT) — created_at of the
-- account is derivable from it, but we keep explicit timestamps here because
-- accounts are mutated (updated_at) unlike messages.
CREATE TABLE users (
  id            BIGINT PRIMARY KEY,                  -- snowflake
  username      TEXT NOT NULL CHECK (username ~ '^[a-z0-9_.]{2,32}$'),
  display_name  TEXT CHECK (char_length(display_name) BETWEEN 1 AND 32),
  email         TEXT NOT NULL,
  password_hash TEXT NOT NULL,                       -- PHC argon2id string
  flags         BIGINT NOT NULL DEFAULT 0,
  last_seen_at  TIMESTAMPTZ,
  created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
  updated_at    TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Case-insensitive uniqueness for login + registration collision checks.
CREATE UNIQUE INDEX users_username_lower_key ON users (LOWER(username));
CREATE UNIQUE INDEX users_email_lower_key    ON users (LOWER(email));
