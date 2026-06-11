-- guilds, guild_members, channels, channel_recipients. Owned by chat-service.
CREATE TABLE guilds (
  id          BIGINT PRIMARY KEY,                    -- snowflake
  name        TEXT NOT NULL CHECK (char_length(name) BETWEEN 1 AND 100),
  owner_id    BIGINT NOT NULL REFERENCES users(id),
  invite_code TEXT NOT NULL,
  created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE UNIQUE INDEX guilds_invite_code_key ON guilds (invite_code);

CREATE TABLE guild_members (
  guild_id    BIGINT NOT NULL REFERENCES guilds(id) ON DELETE CASCADE,
  user_id     BIGINT NOT NULL REFERENCES users(id)  ON DELETE CASCADE,
  nickname    TEXT CHECK (char_length(nickname) BETWEEN 1 AND 32),
  -- Deliberately NO SQL DEFAULT: the value is always supplied by Rust as
  -- Permissions::to_db() from dice-permissions, which owns the canonical bit
  -- layout. A magic literal here would duplicate (and silently poison) that
  -- layout. u64 bit set stored via the i64 bit-cast.
  permissions BIGINT NOT NULL,
  joined_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
  PRIMARY KEY (guild_id, user_id)
);

CREATE INDEX guild_members_user_idx ON guild_members (user_id);

CREATE TABLE channels (
  id              BIGINT PRIMARY KEY,                -- snowflake
  -- channel_type stores the dice.v1 ChannelKind proto enum value VERBATIM:
  -- 1 = GUILD_TEXT, 2 = DM. No layer (Postgres, client SQLite, wire) ever
  -- remaps enum numbers.
  channel_type    SMALLINT NOT NULL,
  guild_id        BIGINT REFERENCES guilds(id) ON DELETE CASCADE,
  name            TEXT CHECK (char_length(name) BETWEEN 1 AND 100),
  topic           TEXT CHECK (char_length(topic) <= 1024),
  position        INT NOT NULL DEFAULT 0,
  dm_key          TEXT,                              -- "minUid:maxUid" for channel_type = 2
  last_message_id BIGINT,
  created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
  CHECK ( (channel_type = 1 AND guild_id IS NOT NULL AND name IS NOT NULL AND dm_key IS NULL)
       OR (channel_type = 2 AND guild_id IS NULL AND dm_key IS NOT NULL) )
);

CREATE INDEX channels_guild_idx         ON channels (guild_id) WHERE guild_id IS NOT NULL;
CREATE UNIQUE INDEX channels_dm_key_key ON channels (dm_key)   WHERE dm_key   IS NOT NULL;

-- DM membership (exactly the two participants in M1).
CREATE TABLE channel_recipients (
  channel_id BIGINT NOT NULL REFERENCES channels(id) ON DELETE CASCADE,
  user_id    BIGINT NOT NULL REFERENCES users(id)    ON DELETE CASCADE,
  PRIMARY KEY (channel_id, user_id)
);

CREATE INDEX channel_recipients_user_idx ON channel_recipients (user_id);
