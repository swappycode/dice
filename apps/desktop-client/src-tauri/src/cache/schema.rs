//! Embedded migrations keyed off `PRAGMA user_version` (design §3.1–3.2).
//! `invite_code` is an addition over the design sketch: the frontend Guild
//! DTO requires it.

use rusqlite::Connection;

const MIGRATIONS: &[&str] = &[
    // v1
    "
    CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
    CREATE TABLE users (
        id INTEGER PRIMARY KEY,
        username TEXT NOT NULL,
        display_name TEXT,
        avatar_hash TEXT,
        updated_at INTEGER NOT NULL
    );
    CREATE TABLE guilds (
        id INTEGER PRIMARY KEY,
        name TEXT NOT NULL,
        icon_hash TEXT,
        owner_id INTEGER NOT NULL,
        invite_code TEXT NOT NULL DEFAULT '',
        my_permissions INTEGER NOT NULL DEFAULT 0,
        updated_at INTEGER NOT NULL
    );
    CREATE TABLE channels (
        id INTEGER PRIMARY KEY,
        guild_id INTEGER,            -- NULL => DM channel
        kind INTEGER NOT NULL,       -- dice.v1.ChannelKind value (1=guild_text, 2=dm)
        name TEXT,
        position INTEGER,
        last_message_id INTEGER,
        updated_at INTEGER NOT NULL
    );
    CREATE INDEX idx_channels_guild ON channels(guild_id);
    CREATE TABLE dm_participants (
        channel_id INTEGER NOT NULL,
        user_id INTEGER NOT NULL,
        PRIMARY KEY (channel_id, user_id)
    );
    CREATE TABLE members (
        guild_id INTEGER NOT NULL,
        user_id INTEGER NOT NULL,
        nickname TEXT,
        PRIMARY KEY (guild_id, user_id)
    );
    CREATE TABLE messages (
        id INTEGER PRIMARY KEY,      -- snowflake; NEGATIVE for pending rows
        channel_id INTEGER NOT NULL,
        author_id INTEGER NOT NULL,
        content TEXT NOT NULL,
        created_at INTEGER NOT NULL,
        edited_at INTEGER,
        nonce TEXT,
        pending INTEGER NOT NULL DEFAULT 0,
        failed INTEGER NOT NULL DEFAULT 0
    );
    CREATE INDEX idx_messages_channel ON messages(channel_id, id DESC);
    CREATE TABLE channel_sync (
        channel_id INTEGER PRIMARY KEY,
        oldest_fetched_id INTEGER,
        newest_synced_id INTEGER,
        stale INTEGER NOT NULL DEFAULT 0
    );
    CREATE TABLE read_markers (
        channel_id INTEGER PRIMARY KEY,
        last_read_message_id INTEGER
    );
    ",
    // v2: replies + reactions (M2). reply_to_id is a plain column (the parent
    // may be uncached/deleted). Reactions are stored as the per-emoji AGGREGATE
    // from this user's perspective (count + me); `ord` preserves first-seen order.
    "
    ALTER TABLE messages ADD COLUMN reply_to_id INTEGER;
    CREATE TABLE message_reactions (
        message_id INTEGER NOT NULL,
        emoji TEXT NOT NULL,
        count INTEGER NOT NULL,
        me INTEGER NOT NULL DEFAULT 0,
        ord INTEGER NOT NULL,
        PRIMARY KEY (message_id, emoji)
    );
    CREATE INDEX idx_reactions_msg ON message_reactions(message_id, ord);
    ",
    // v3: attachments (M2). One row per attached media, in display `position`
    // order. Metadata only (filename/type/size/dims); the bytes are fetched
    // on demand from GET /v1/media/{id}. Stored for REAL messages only (pending
    // rows show attachments via the frontend's optimistic copy until the echo).
    "
    CREATE TABLE message_attachments (
        message_id INTEGER NOT NULL,
        media_id INTEGER NOT NULL,
        position INTEGER NOT NULL,
        filename TEXT NOT NULL,
        content_type TEXT NOT NULL,
        size_bytes INTEGER NOT NULL,
        width INTEGER NOT NULL DEFAULT 0,
        height INTEGER NOT NULL DEFAULT 0,
        PRIMARY KEY (message_id, media_id)
    );
    CREATE INDEX idx_attachments_msg ON message_attachments(message_id, position);
    ",
];

pub fn migrate(conn: &mut Connection) -> rusqlite::Result<()> {
    let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    for (idx, sql) in MIGRATIONS.iter().enumerate() {
        let target = idx as i64 + 1;
        if version < target {
            let tx = conn.transaction()?;
            tx.execute_batch(sql)?;
            tx.pragma_update(None, "user_version", target)?;
            tx.commit()?;
        }
    }
    Ok(())
}
