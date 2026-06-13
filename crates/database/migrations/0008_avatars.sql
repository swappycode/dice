-- Avatars (M2). Re-introduces the M1-cut avatar field, now backed by the
-- media-service: an avatar is a `media` row, referenced by id. NULL = no avatar
-- (the client renders initials). ON DELETE SET NULL so deleting the media (a
-- future GC sweep) just clears the avatar rather than orphaning a dangling id.
ALTER TABLE users ADD COLUMN avatar_media_id BIGINT REFERENCES media(id) ON DELETE SET NULL;
