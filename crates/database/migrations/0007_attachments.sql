-- Attachments (M2). `media` holds one row per uploaded object; the bytes live
-- in the media store (local-fs in dev, an S3/rustls backend later — the store
-- is a trait seam in media-service). `message_attachments` links a message to
-- its media in display order. A media row is claimed by AT MOST ONE message
-- (media_id is the PK there), so uploads are one-shot: an id referenced by a
-- send can never be reused on another message.
CREATE TABLE media (
  id           BIGINT PRIMARY KEY,                       -- snowflake; also the store object key
  uploader_id  BIGINT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  filename     TEXT NOT NULL CHECK (char_length(filename) BETWEEN 1 AND 255),
  content_type TEXT NOT NULL CHECK (char_length(content_type) BETWEEN 1 AND 255),
  size_bytes   BIGINT NOT NULL CHECK (size_bytes >= 0),
  width        INTEGER NOT NULL DEFAULT 0,               -- 0 = not an image
  height       INTEGER NOT NULL DEFAULT 0,
  created_at   TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX media_uploader_idx ON media (uploader_id);

CREATE TABLE message_attachments (
  media_id   BIGINT PRIMARY KEY REFERENCES media(id) ON DELETE CASCADE,
  message_id BIGINT NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
  position   SMALLINT NOT NULL
);

-- The page join reads attachments for a set of messages in display order.
CREATE INDEX message_attachments_msg_idx ON message_attachments (message_id, position);

-- A message may now be attachment-only (empty content), so relax the content
-- CHECK from 1..4000 to 0..4000. The "non-empty content OR ≥1 attachment" rule
-- is enforced in chat-service (the DB cannot see the junction rows at INSERT).
ALTER TABLE messages DROP CONSTRAINT messages_content_check;
ALTER TABLE messages ADD CONSTRAINT messages_content_check
  CHECK (char_length(content) <= 4000);
