-- Voice channels (M3). Allow channel_type = 3 (dice.v1 ChannelKind VOICE), which
-- has the same shape as GUILD_TEXT: lives in a guild, is named, no dm_key.
--
-- The 0003 table-level CHECK was created unnamed (Postgres auto-names it). Drop
-- it by introspection (the table-level check is the only CHECK on `channels`
-- spanning more than one column — the name/topic checks are single-column) and
-- re-add a named one that also admits VOICE.
DO $$
DECLARE
  cname text;
BEGIN
  SELECT conname INTO cname
  FROM pg_constraint
  WHERE conrelid = 'channels'::regclass
    AND contype = 'c'
    AND cardinality(conkey) > 1;
  IF cname IS NOT NULL THEN
    EXECUTE format('ALTER TABLE channels DROP CONSTRAINT %I', cname);
  END IF;
END $$;

ALTER TABLE channels ADD CONSTRAINT channels_kind_shape_check CHECK (
     (channel_type = 1 AND guild_id IS NOT NULL AND name IS NOT NULL AND dm_key IS NULL)
  OR (channel_type = 2 AND guild_id IS NULL     AND dm_key IS NOT NULL)
  OR (channel_type = 3 AND guild_id IS NOT NULL AND name IS NOT NULL AND dm_key IS NULL)
);
