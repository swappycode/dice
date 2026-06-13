-- Replies (M2). Deliberately NOT a foreign key: a reply whose parent is later
-- deleted keeps its reply_to_id and renders as "original message" client-side,
-- rather than failing the send or cascading. 0/NULL = not a reply.
ALTER TABLE messages ADD COLUMN reply_to_id BIGINT;
