-- messages. Owned by chat-service. No created_at column: the snowflake id
-- encodes the creation timestamp (ms since DICE_EPOCH), so it is derivable.
CREATE TABLE messages (
  id         BIGINT PRIMARY KEY,                     -- snowflake => created_at derivable
  channel_id BIGINT NOT NULL REFERENCES channels(id) ON DELETE CASCADE,
  author_id  BIGINT NOT NULL REFERENCES users(id),
  content    TEXT NOT NULL CHECK (char_length(content) BETWEEN 1 AND 4000),
  edited_at  TIMESTAMPTZ
  -- reply_to_id BIGINT: reserved for a later migration (replies are post-M1)
);

-- History pagination: WHERE channel_id = $1 AND id < $2 ORDER BY id DESC LIMIT n.
CREATE INDEX messages_channel_pagination_idx ON messages (channel_id, id DESC);
