-- Reactions (M2). One row per (message, user, emoji). Cascades when the
-- message is deleted. The emoji is stored as its UTF-8 text (e.g. an emoji
-- codepoint or a short :name:); the gateway caps length.
CREATE TABLE message_reactions (
  message_id BIGINT NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
  user_id    BIGINT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  emoji      TEXT NOT NULL CHECK (char_length(emoji) BETWEEN 1 AND 64),
  created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
  PRIMARY KEY (message_id, user_id, emoji)
);

-- Aggregation reads group by (message_id, emoji); this index serves both the
-- per-message rollup and the count(*) per emoji.
CREATE INDEX message_reactions_msg_idx ON message_reactions (message_id, emoji);
