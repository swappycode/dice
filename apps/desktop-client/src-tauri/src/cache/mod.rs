//! Client SQLite cache (design §3): ONE dedicated worker thread owns the
//! `rusqlite::Connection`; callers send `FnOnce(&mut Connection)` jobs over
//! an mpsc and await a oneshot reply. WAL, `synchronous=NORMAL`,
//! `foreign_keys=ON`. Presence is NEVER persisted.

mod schema;

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::atomic::{AtomicI64, Ordering};

use dice_common::time::now_ms;
use dice_protocol::v1;
use dice_protocol::v1::frame::Payload;
use rusqlite::{Connection, OptionalExtension, params};

use crate::dto::{
    BootstrapDto, ChannelDto, GuildDto, MemberDto, MessageDto, UserDto, snowflake_ms,
};

/// Pending rows older than this at open are marked failed (design §3.3).
const PENDING_TTL_MS: u64 = 60_000;

type Job = Box<dyn FnOnce(&mut Connection) + Send>;

#[derive(Debug, thiserror::Error)]
pub enum CacheError {
    #[error("cache worker is gone")]
    Closed,
    #[error("sqlite: {0}")]
    Sql(#[from] rusqlite::Error),
}

/// Contiguous-window cursor for one channel (design §3.4).
#[derive(Debug, Clone, Copy, Default)]
pub struct SyncRow {
    pub oldest_fetched_id: Option<i64>,
    pub newest_synced_id: Option<i64>,
    pub stale: bool,
}

/// Everything `get_bootstrap` needs except presence (RAM-only).
#[derive(Debug, Clone)]
pub struct BootstrapData {
    pub user: UserDto,
    pub guilds: Vec<GuildDto>,
    pub channels: Vec<ChannelDto>,
    pub dms: Vec<ChannelDto>,
    pub users: Vec<UserDto>,
    pub last_channel_id: Option<String>,
}

impl BootstrapData {
    pub fn into_dto(self, presence: BTreeMap<String, String>) -> BootstrapDto {
        BootstrapDto {
            user: self.user,
            guilds: self.guilds,
            channels: self.channels,
            dms: self.dms,
            users: self.users,
            presence,
            last_channel_id: self.last_channel_id,
        }
    }
}

#[derive(Clone)]
pub struct Cache {
    tx: std::sync::mpsc::Sender<Job>,
}

impl Cache {
    /// Open (or create) the cache db and spawn the worker thread. The path is
    /// injectable so tests never touch `%APPDATA%`.
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let (tx, rx) = std::sync::mpsc::channel::<Job>();
        let (boot_tx, boot_rx) = std::sync::mpsc::channel::<anyhow::Result<()>>();
        let path = path.to_owned();
        std::thread::Builder::new()
            .name("dice-cache".to_owned())
            .spawn(move || {
                let mut conn = match open_connection(&path) {
                    Ok(conn) => {
                        let _ = boot_tx.send(Ok(()));
                        conn
                    }
                    Err(e) => {
                        let _ = boot_tx.send(Err(e));
                        return;
                    }
                };
                while let Ok(job) = rx.recv() {
                    job(&mut conn);
                }
            })?;
        boot_rx
            .recv()
            .map_err(|_| anyhow::anyhow!("cache worker died during open"))??;
        Ok(Self { tx })
    }

    /// Ship one closure to the worker and await its result.
    async fn run<T, F>(&self, op: F) -> Result<T, CacheError>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection) -> rusqlite::Result<T> + Send + 'static,
    {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.tx
            .send(Box::new(move |conn| {
                let _ = tx.send(op(conn));
            }))
            .map_err(|_| CacheError::Closed)?;
        rx.await
            .map_err(|_| CacheError::Closed)?
            .map_err(Into::into)
    }

    // ------------------------------------------------------------ snapshots

    /// Diff-upsert a fresh `Ready` snapshot. Every existing sync window is
    /// marked stale first: a fresh session means dispatches may have been
    /// missed, so per-channel windows must reconcile on next open (§3.4).
    pub async fn apply_ready(&self, ready: v1::Ready) -> Result<(), CacheError> {
        self.run(move |conn| {
            let tx = conn.transaction()?;
            // Cross-account hygiene: a Ready for a DIFFERENT user than the one
            // the cache currently holds means an account switch in this data
            // dir (logout + login as someone else). The normal diff only
            // reconciles guilds the NEW user shares, so without this purge the
            // previous account's messages/users/read-markers would linger.
            if let Some(user) = &ready.user {
                let switching = get_meta(&tx, "current_user_id")?
                    .is_some_and(|prev| prev != user.id.to_string());
                if switching {
                    clear_all(&tx)?;
                }
            }
            tx.execute("UPDATE channel_sync SET stale = 1", [])?;
            if let Some(user) = &ready.user {
                upsert_user(&tx, user)?;
                set_meta(&tx, "current_user_id", &user.id.to_string())?;
            }
            for user in &ready.users {
                upsert_user(&tx, user)?;
            }
            diff_guilds(&tx, &ready.guilds)?;
            for guild in &ready.guilds {
                upsert_guild_bundle(&tx, guild)?;
            }
            for dm in &ready.dm_channels {
                upsert_channel(&tx, dm)?;
            }
            tx.commit()
        })
        .await
    }

    /// Apply one dispatch payload: message upsert + cursor advance,
    /// guild/channel/member/dm upserts. Presence/typing are never persisted.
    pub async fn apply_event(&self, payload: Payload) -> Result<(), CacheError> {
        self.run(move |conn| {
            let tx = conn.transaction()?;
            match &payload {
                Payload::MessageCreate(mc) => {
                    if let Some(message) = &mc.message {
                        upsert_message(&tx, message, None)?;
                        advance_cursor(&tx, message.channel_id, message.id)?;
                    }
                }
                Payload::MessageUpdate(mu) => {
                    // ON CONFLICT updates content + edited_at; no-op if we
                    // never cached the original (a no-row UPDATE is harmless).
                    if let Some(message) = &mu.message {
                        upsert_message(&tx, message, None)?;
                    }
                }
                Payload::MessageDelete(md) => {
                    tx.execute(
                        "DELETE FROM messages WHERE id = ?1 AND channel_id = ?2",
                        params![md.message_id as i64, md.channel_id as i64],
                    )?;
                }
                Payload::GuildCreate(v1::GuildCreate { guild })
                | Payload::GuildUpdate(v1::GuildUpdate { guild }) => {
                    if let Some(guild) = guild {
                        upsert_guild_bundle(&tx, guild)?;
                    }
                }
                Payload::GuildDelete(gd) => delete_guild(&tx, gd.guild_id)?,
                Payload::ChannelCreate(v1::ChannelCreate { channel })
                | Payload::ChannelUpdate(v1::ChannelUpdate { channel }) => {
                    if let Some(channel) = channel {
                        upsert_channel(&tx, channel)?;
                    }
                }
                Payload::ChannelDelete(cd) => delete_channel(&tx, cd.channel_id)?,
                Payload::MemberAdd(ma) => {
                    if let Some(user) = &ma.user {
                        upsert_user(&tx, user)?;
                    }
                    if let Some(member) = &ma.member {
                        tx.execute(
                            "INSERT OR IGNORE INTO members(guild_id, user_id) VALUES (?1, ?2)",
                            params![member.guild_id as i64, member.user_id as i64],
                        )?;
                    }
                }
                Payload::MemberRemove(mr) => {
                    tx.execute(
                        "DELETE FROM members WHERE guild_id = ?1 AND user_id = ?2",
                        params![mr.guild_id as i64, mr.user_id as i64],
                    )?;
                }
                Payload::DmChannelCreate(dc) => {
                    if let Some(channel) = &dc.channel {
                        upsert_channel(&tx, channel)?;
                    }
                }
                _ => {}
            }
            tx.commit()
        })
        .await
    }

    // ------------------------------------------------------------- messages

    /// Insert an optimistic pending row (negative synthetic id) and return
    /// its DTO.
    pub async fn insert_pending(
        &self,
        channel_id: u64,
        author_id: u64,
        content: String,
        client_nonce: String,
    ) -> Result<MessageDto, CacheError> {
        static SEQ: AtomicI64 = AtomicI64::new(0);
        let created = now_ms();
        let id = -((created as i64) * 4096 + (SEQ.fetch_add(1, Ordering::Relaxed) & 0xFFF));
        let dto = MessageDto {
            id: id.to_string(),
            channel_id: channel_id.to_string(),
            author_id: author_id.to_string(),
            content: content.clone(),
            created_at_ms: created,
            edited_at_ms: None,
            nonce: Some(client_nonce.clone()),
            pending: Some(true),
            failed: None,
        };
        self.run(move |conn| {
            conn.execute(
                "INSERT INTO messages(id, channel_id, author_id, content, created_at, nonce, pending)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, 1)",
                params![id, channel_id as i64, author_id as i64, content, created as i64, client_nonce],
            )?;
            Ok(())
        })
        .await?;
        Ok(dto)
    }

    /// The gateway acked (or echoed) our send: drop the pending row carrying
    /// this nonce and upsert the real message. Idempotent.
    pub async fn reconcile_by_nonce(
        &self,
        client_nonce: String,
        message: v1::Message,
    ) -> Result<(), CacheError> {
        self.run(move |conn| {
            let tx = conn.transaction()?;
            tx.execute(
                "DELETE FROM messages WHERE nonce = ?1 AND pending = 1",
                params![client_nonce],
            )?;
            upsert_message(&tx, &message, Some(&client_nonce))?;
            advance_cursor(&tx, message.channel_id, message.id)?;
            tx.commit()
        })
        .await
    }

    /// The send failed for good: keep the row for the retry UI.
    pub async fn mark_failed(&self, client_nonce: String) -> Result<(), CacheError> {
        self.run(move |conn| {
            conn.execute(
                "UPDATE messages SET pending = 0, failed = 1 WHERE nonce = ?1 AND pending = 1",
                params![client_nonce],
            )?;
            Ok(())
        })
        .await
    }

    /// One ascending page. `before = None` ⇒ the newest `limit` real rows
    /// plus any local pending/failed rows; otherwise strictly older rows.
    pub async fn page_messages(
        &self,
        channel_id: u64,
        before: Option<u64>,
        limit: u32,
    ) -> Result<Vec<MessageDto>, CacheError> {
        self.run(move |conn| {
            let mut out: Vec<MessageDto> = Vec::new();
            match before {
                Some(before) => {
                    let mut stmt = conn.prepare_cached(
                        "SELECT id, channel_id, author_id, content, created_at, edited_at,
                                nonce, pending, failed
                         FROM messages
                         WHERE channel_id = ?1 AND id > 0 AND id < ?2
                         ORDER BY id DESC LIMIT ?3",
                    )?;
                    let rows = stmt.query_map(
                        params![channel_id as i64, before as i64, limit],
                        row_to_message,
                    )?;
                    for row in rows {
                        out.push(row?);
                    }
                    out.reverse();
                }
                None => {
                    let mut stmt = conn.prepare_cached(
                        "SELECT id, channel_id, author_id, content, created_at, edited_at,
                                nonce, pending, failed
                         FROM messages
                         WHERE channel_id = ?1 AND id > 0
                         ORDER BY id DESC LIMIT ?2",
                    )?;
                    let rows = stmt.query_map(params![channel_id as i64, limit], row_to_message)?;
                    for row in rows {
                        out.push(row?);
                    }
                    out.reverse();
                    let mut local = conn.prepare_cached(
                        "SELECT id, channel_id, author_id, content, created_at, edited_at,
                                nonce, pending, failed
                         FROM messages
                         WHERE channel_id = ?1 AND id < 0
                         ORDER BY created_at ASC",
                    )?;
                    let rows = local.query_map(params![channel_id as i64], row_to_message)?;
                    for row in rows {
                        out.push(row?);
                    }
                }
            }
            Ok(out)
        })
        .await
    }

    pub async fn channel_sync(&self, channel_id: u64) -> Result<Option<SyncRow>, CacheError> {
        self.run(move |conn| {
            conn.query_row(
                "SELECT oldest_fetched_id, newest_synced_id, stale
                 FROM channel_sync WHERE channel_id = ?1",
                params![channel_id as i64],
                |row| {
                    Ok(SyncRow {
                        oldest_fetched_id: row.get(0)?,
                        newest_synced_id: row.get(1)?,
                        stale: row.get::<_, i64>(2)? != 0,
                    })
                },
            )
            .optional()
        })
        .await
    }

    /// Record a freshly fetched NEWEST page (descending from the API):
    /// if it overlaps the cached window the ranges connect (stale clears);
    /// otherwise the window resets to just this page (design §3.4).
    pub async fn note_newest_page(
        &self,
        channel_id: u64,
        messages: Vec<v1::Message>,
    ) -> Result<(), CacheError> {
        self.run(move |conn| {
            let tx = conn.transaction()?;
            for message in &messages {
                upsert_message(&tx, message, None)?;
            }
            let newest = messages.iter().map(|m| m.id as i64).max();
            let oldest = messages.iter().map(|m| m.id as i64).min();
            let existing: Option<SyncRow> = tx
                .query_row(
                    "SELECT oldest_fetched_id, newest_synced_id, stale
                     FROM channel_sync WHERE channel_id = ?1",
                    params![channel_id as i64],
                    |row| {
                        Ok(SyncRow {
                            oldest_fetched_id: row.get(0)?,
                            newest_synced_id: row.get(1)?,
                            stale: row.get::<_, i64>(2)? != 0,
                        })
                    },
                )
                .optional()?;
            let (new_oldest, new_newest) = match (oldest, newest) {
                (Some(oldest), Some(newest)) => match existing {
                    Some(SyncRow {
                        oldest_fetched_id: Some(prev_oldest),
                        newest_synced_id: Some(prev_newest),
                        ..
                    }) if oldest <= prev_newest => {
                        // Ranges connect: one contiguous window.
                        (Some(prev_oldest.min(oldest)), Some(prev_newest.max(newest)))
                    }
                    _ => (Some(oldest), Some(newest)), // reset to this page
                },
                // Empty channel: the window is trivially complete.
                _ => existing
                    .map(|s| (s.oldest_fetched_id, s.newest_synced_id))
                    .unwrap_or((None, None)),
            };
            tx.execute(
                "INSERT INTO channel_sync(channel_id, oldest_fetched_id, newest_synced_id, stale)
                 VALUES (?1, ?2, ?3, 0)
                 ON CONFLICT(channel_id) DO UPDATE SET
                     oldest_fetched_id = excluded.oldest_fetched_id,
                     newest_synced_id  = excluded.newest_synced_id,
                     stale             = 0",
                params![channel_id as i64, new_oldest, new_newest],
            )?;
            if let Some(newest) = newest {
                bump_last_message(&tx, channel_id as i64, newest)?;
            }
            tx.commit()
        })
        .await
    }

    /// Record an OLDER history page: the window extends downward.
    pub async fn note_older_page(
        &self,
        channel_id: u64,
        messages: Vec<v1::Message>,
    ) -> Result<(), CacheError> {
        self.run(move |conn| {
            let tx = conn.transaction()?;
            for message in &messages {
                upsert_message(&tx, message, None)?;
            }
            if let Some(oldest) = messages.iter().map(|m| m.id as i64).min() {
                tx.execute(
                    "UPDATE channel_sync
                     SET oldest_fetched_id = MIN(COALESCE(oldest_fetched_id, ?2), ?2)
                     WHERE channel_id = ?1",
                    params![channel_id as i64, oldest],
                )?;
            }
            tx.commit()
        })
        .await
    }

    /// Resume failed / session invalidated: every cached window may be gapped.
    pub async fn mark_all_stale(&self) -> Result<(), CacheError> {
        self.run(|conn| {
            conn.execute("UPDATE channel_sync SET stale = 1", [])?;
            Ok(())
        })
        .await
    }

    // ----------------------------------------------------------- bootstrap

    pub async fn set_current_user(&self, user: v1::User) -> Result<(), CacheError> {
        self.run(move |conn| {
            let tx = conn.transaction()?;
            upsert_user(&tx, &user)?;
            set_meta(&tx, "current_user_id", &user.id.to_string())?;
            tx.commit()
        })
        .await
    }

    pub async fn current_user(&self) -> Result<Option<UserDto>, CacheError> {
        self.run(|conn| {
            let Some(id) = get_meta(conn, "current_user_id")? else {
                return Ok(None);
            };
            conn.query_row(
                "SELECT id, username, display_name FROM users WHERE id = ?1",
                params![id.parse::<i64>().unwrap_or_default()],
                row_to_user,
            )
            .optional()
        })
        .await
    }

    /// The instant-first-paint snapshot (presence is filled by the caller).
    pub async fn bootstrap_snapshot(&self) -> Result<Option<BootstrapData>, CacheError> {
        self.run(|conn| {
            let Some(user_id) = get_meta(conn, "current_user_id")? else {
                return Ok(None);
            };
            let Some(user) = conn
                .query_row(
                    "SELECT id, username, display_name FROM users WHERE id = ?1",
                    params![user_id.parse::<i64>().unwrap_or_default()],
                    row_to_user,
                )
                .optional()?
            else {
                return Ok(None);
            };

            let mut users = Vec::new();
            let mut stmt =
                conn.prepare_cached("SELECT id, username, display_name FROM users ORDER BY id")?;
            for row in stmt.query_map([], row_to_user)? {
                users.push(row?);
            }

            let mut members_by_guild: BTreeMap<i64, Vec<MemberDto>> = BTreeMap::new();
            let mut stmt =
                conn.prepare_cached("SELECT guild_id, user_id FROM members ORDER BY user_id")?;
            for row in
                stmt.query_map([], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)))?
            {
                let (guild_id, member_id) = row?;
                members_by_guild
                    .entry(guild_id)
                    .or_default()
                    .push(MemberDto {
                        user_id: member_id.to_string(),
                        guild_id: guild_id.to_string(),
                    });
            }

            let mut guilds = Vec::new();
            let mut stmt = conn
                .prepare_cached("SELECT id, name, owner_id, invite_code FROM guilds ORDER BY id")?;
            for row in stmt.query_map([], |row| {
                let id: i64 = row.get(0)?;
                Ok(GuildDto {
                    id: id.to_string(),
                    name: row.get(1)?,
                    owner_id: row.get::<_, i64>(2)?.to_string(),
                    invite_code: row.get::<_, Option<String>>(3)?.unwrap_or_default(),
                    members: Vec::new(),
                })
            })? {
                let mut guild = row?;
                guild.members = members_by_guild
                    .remove(&guild.id.parse::<i64>().unwrap_or_default())
                    .unwrap_or_default();
                guilds.push(guild);
            }

            let mut recipients: BTreeMap<i64, Vec<String>> = BTreeMap::new();
            let mut stmt = conn.prepare_cached(
                "SELECT channel_id, user_id FROM dm_participants ORDER BY user_id",
            )?;
            for row in
                stmt.query_map([], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)))?
            {
                let (channel_id, recipient) = row?;
                recipients
                    .entry(channel_id)
                    .or_default()
                    .push(recipient.to_string());
            }

            let mut channels = Vec::new();
            let mut dms = Vec::new();
            let mut stmt = conn.prepare_cached(
                "SELECT id, guild_id, kind, name, position, last_message_id
                 FROM channels ORDER BY position, id",
            )?;
            for row in stmt.query_map([], |row| {
                let id: i64 = row.get(0)?;
                let guild_id: Option<i64> = row.get(1)?;
                let kind: i64 = row.get(2)?;
                Ok(ChannelDto {
                    id: id.to_string(),
                    guild_id: guild_id.map(|g| g.to_string()),
                    kind: if kind == v1::ChannelKind::Dm as i64 {
                        "dm".to_owned()
                    } else {
                        "guild_text".to_owned()
                    },
                    name: row.get::<_, Option<String>>(3)?.unwrap_or_default(),
                    position: row.get::<_, Option<i64>>(4)?.unwrap_or_default() as u32,
                    last_message_id: row.get::<_, Option<i64>>(5)?.map(|m| m.to_string()),
                    recipient_ids: Vec::new(),
                })
            })? {
                let mut channel = row?;
                if channel.guild_id.is_some() {
                    channels.push(channel);
                } else {
                    channel.recipient_ids = recipients
                        .remove(&channel.id.parse::<i64>().unwrap_or_default())
                        .unwrap_or_default();
                    dms.push(channel);
                }
            }

            Ok(Some(BootstrapData {
                user,
                guilds,
                channels,
                dms,
                users,
                last_channel_id: get_meta(conn, "last_channel_id")?,
            }))
        })
        .await
    }

    pub async fn get_users(&self, ids: Vec<u64>) -> Result<Vec<UserDto>, CacheError> {
        self.run(move |conn| {
            let mut out = Vec::new();
            let mut stmt =
                conn.prepare_cached("SELECT id, username, display_name FROM users WHERE id = ?1")?;
            for id in ids {
                if let Some(user) = stmt.query_row(params![id as i64], row_to_user).optional()? {
                    out.push(user);
                }
            }
            Ok(out)
        })
        .await
    }

    // ----------------------------------------------------------------- meta

    pub async fn set_meta(&self, key: String, value: String) -> Result<(), CacheError> {
        self.run(move |conn| set_meta(conn, &key, &value)).await
    }

    pub async fn get_meta(&self, key: String) -> Result<Option<String>, CacheError> {
        self.run(move |conn| get_meta(conn, &key)).await
    }

    /// Logout: drop every row in every table.
    pub async fn wipe(&self) -> Result<(), CacheError> {
        self.run(|conn| clear_all(conn)).await
    }
}

/// Drop every data row — logout ([`Cache::wipe`]) and account-switch purge
/// ([`Cache::apply_ready`]) share this so the two can never drift apart.
fn clear_all(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "DELETE FROM meta; DELETE FROM users; DELETE FROM guilds;
         DELETE FROM channels; DELETE FROM dm_participants; DELETE FROM members;
         DELETE FROM messages; DELETE FROM channel_sync; DELETE FROM read_markers;",
    )
}

// ------------------------------------------------------------ SQL helpers

fn open_connection(path: &Path) -> anyhow::Result<Connection> {
    let mut conn = Connection::open(path)?;
    // journal_mode returns a row; query it instead of pragma_update.
    let _: String = conn.query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0))?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    schema::migrate(&mut conn)?;
    // Stale optimistic rows from a previous run: pending -> failed (§3.3).
    conn.execute(
        "UPDATE messages SET pending = 0, failed = 1
         WHERE pending = 1 AND created_at < ?1",
        params![now_ms().saturating_sub(PENDING_TTL_MS) as i64],
    )?;
    Ok(conn)
}

fn row_to_user(row: &rusqlite::Row<'_>) -> rusqlite::Result<UserDto> {
    let id: i64 = row.get(0)?;
    let username: String = row.get(1)?;
    let display: Option<String> = row.get(2)?;
    Ok(UserDto {
        id: id.to_string(),
        display_name: display
            .filter(|d| !d.is_empty())
            .unwrap_or_else(|| username.clone()),
        username,
    })
}

fn row_to_message(row: &rusqlite::Row<'_>) -> rusqlite::Result<MessageDto> {
    let id: i64 = row.get(0)?;
    let pending = row.get::<_, i64>(7)? != 0;
    let failed = row.get::<_, i64>(8)? != 0;
    Ok(MessageDto {
        id: id.to_string(),
        channel_id: row.get::<_, i64>(1)?.to_string(),
        author_id: row.get::<_, i64>(2)?.to_string(),
        content: row.get(3)?,
        created_at_ms: row.get::<_, i64>(4)? as u64,
        edited_at_ms: row.get::<_, Option<i64>>(5)?.map(|v| v as u64),
        nonce: row.get(6)?,
        pending: pending.then_some(true),
        failed: failed.then_some(true),
    })
}

fn set_meta(conn: &Connection, key: &str, value: &str) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO meta(key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![key, value],
    )?;
    Ok(())
}

fn get_meta(conn: &Connection, key: &str) -> rusqlite::Result<Option<String>> {
    conn.query_row("SELECT value FROM meta WHERE key = ?1", params![key], |r| {
        r.get(0)
    })
    .optional()
}

fn upsert_user(conn: &Connection, user: &v1::User) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO users(id, username, display_name, updated_at) VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(id) DO UPDATE SET
             username = excluded.username,
             display_name = excluded.display_name,
             updated_at = excluded.updated_at",
        params![
            user.id as i64,
            user.username,
            user.display_name,
            now_ms() as i64
        ],
    )?;
    Ok(())
}

fn upsert_channel(conn: &Connection, channel: &v1::Channel) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO channels(id, guild_id, kind, name, position, last_message_id, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT(id) DO UPDATE SET
             guild_id = excluded.guild_id,
             kind = excluded.kind,
             name = excluded.name,
             position = excluded.position,
             last_message_id = MAX(COALESCE(channels.last_message_id, 0), COALESCE(excluded.last_message_id, 0)),
             updated_at = excluded.updated_at",
        params![
            channel.id as i64,
            (channel.guild_id != 0).then_some(channel.guild_id as i64),
            channel.kind as i64,
            channel.name,
            channel.position as i64,
            (channel.last_message_id != 0).then_some(channel.last_message_id as i64),
            now_ms() as i64
        ],
    )?;
    for recipient in &channel.recipient_ids {
        conn.execute(
            "INSERT OR IGNORE INTO dm_participants(channel_id, user_id) VALUES (?1, ?2)",
            params![channel.id as i64, *recipient as i64],
        )?;
    }
    Ok(())
}

/// Guild row + its channels + full member list replacement.
fn upsert_guild_bundle(conn: &Connection, guild: &v1::Guild) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO guilds(id, name, owner_id, invite_code, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(id) DO UPDATE SET
             name = excluded.name,
             owner_id = excluded.owner_id,
             invite_code = CASE WHEN excluded.invite_code = '' THEN guilds.invite_code
                                ELSE excluded.invite_code END,
             updated_at = excluded.updated_at",
        params![
            guild.id as i64,
            guild.name,
            guild.owner_id as i64,
            guild.invite_code,
            now_ms() as i64
        ],
    )?;
    for channel in &guild.channels {
        upsert_channel(conn, channel)?;
    }
    if !guild.members.is_empty() {
        conn.execute(
            "DELETE FROM members WHERE guild_id = ?1",
            params![guild.id as i64],
        )?;
        for member in &guild.members {
            conn.execute(
                "INSERT OR IGNORE INTO members(guild_id, user_id) VALUES (?1, ?2)",
                params![guild.id as i64, member.user_id as i64],
            )?;
        }
    }
    Ok(())
}

fn upsert_message(
    conn: &Connection,
    message: &v1::Message,
    client_nonce: Option<&str>,
) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO messages(id, channel_id, author_id, content, created_at, edited_at, nonce)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT(id) DO UPDATE SET
             content = excluded.content,
             edited_at = excluded.edited_at,
             pending = 0,
             failed = 0",
        params![
            message.id as i64,
            message.channel_id as i64,
            message.author_id as i64,
            message.content,
            snowflake_ms(message.id) as i64,
            (message.edited_at_ms != 0).then_some(message.edited_at_ms as i64),
            client_nonce,
        ],
    )?;
    Ok(())
}

/// Live event: extend the contiguous window upward + bump last_message_id.
fn advance_cursor(conn: &Connection, channel_id: u64, message_id: u64) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO channel_sync(channel_id, oldest_fetched_id, newest_synced_id, stale)
         VALUES (?1, ?2, ?2, 0)
         ON CONFLICT(channel_id) DO UPDATE SET
             newest_synced_id = MAX(COALESCE(newest_synced_id, 0), excluded.newest_synced_id),
             oldest_fetched_id = COALESCE(oldest_fetched_id, excluded.oldest_fetched_id)",
        params![channel_id as i64, message_id as i64],
    )?;
    bump_last_message(conn, channel_id as i64, message_id as i64)
}

fn bump_last_message(conn: &Connection, channel_id: i64, message_id: i64) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE channels SET last_message_id = MAX(COALESCE(last_message_id, 0), ?2)
         WHERE id = ?1",
        params![channel_id, message_id],
    )?;
    Ok(())
}

fn delete_channel(conn: &Connection, channel_id: u64) -> rusqlite::Result<()> {
    let id = channel_id as i64;
    conn.execute("DELETE FROM messages WHERE channel_id = ?1", params![id])?;
    conn.execute(
        "DELETE FROM channel_sync WHERE channel_id = ?1",
        params![id],
    )?;
    conn.execute(
        "DELETE FROM read_markers WHERE channel_id = ?1",
        params![id],
    )?;
    conn.execute(
        "DELETE FROM dm_participants WHERE channel_id = ?1",
        params![id],
    )?;
    conn.execute("DELETE FROM channels WHERE id = ?1", params![id])?;
    Ok(())
}

fn delete_guild(conn: &Connection, guild_id: u64) -> rusqlite::Result<()> {
    let gid = guild_id as i64;
    let mut stmt = conn.prepare("SELECT id FROM channels WHERE guild_id = ?1")?;
    let channel_ids: Vec<i64> = stmt
        .query_map(params![gid], |row| row.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    drop(stmt);
    for channel in channel_ids {
        delete_channel(conn, channel as u64)?;
    }
    conn.execute("DELETE FROM members WHERE guild_id = ?1", params![gid])?;
    conn.execute("DELETE FROM guilds WHERE id = ?1", params![gid])?;
    Ok(())
}

/// Remove guilds (and their channels/members/messages) absent from a fresh
/// `Ready` snapshot.
fn diff_guilds(conn: &Connection, snapshot: &[v1::Guild]) -> rusqlite::Result<()> {
    let keep: std::collections::HashSet<i64> = snapshot.iter().map(|g| g.id as i64).collect();
    let mut stmt = conn.prepare("SELECT id FROM guilds")?;
    let cached: Vec<i64> = stmt
        .query_map([], |row| row.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    drop(stmt);
    for guild in cached {
        if !keep.contains(&guild) {
            delete_guild(conn, guild as u64)?;
        }
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn temp_db(tag: &str) -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "dice-cache-test-{tag}-{}-{nanos}.db",
            std::process::id()
        ))
    }

    fn msg(id: u64, channel: u64, author: u64, content: &str) -> v1::Message {
        v1::Message {
            id,
            channel_id: channel,
            author_id: author,
            content: content.to_owned(),
            edited_at_ms: 0,
        }
    }

    #[tokio::test]
    async fn pending_reconcile_and_failed_flow() {
        let path = temp_db("pending");
        let cache = Cache::open(&path).unwrap();

        let pending = cache
            .insert_pending(7, 1, "hello".into(), "n-1".into())
            .await
            .unwrap();
        assert!(pending.id.starts_with('-'), "pending ids are negative");
        assert_eq!(pending.pending, Some(true));

        let page = cache.page_messages(7, None, 50).await.unwrap();
        assert_eq!(page.len(), 1);
        assert_eq!(page[0].nonce.as_deref(), Some("n-1"));

        // Ack: pending row replaced by the real message.
        let real_id = 99u64 << 22;
        cache
            .reconcile_by_nonce("n-1".into(), msg(real_id, 7, 1, "hello"))
            .await
            .unwrap();
        let page = cache.page_messages(7, None, 50).await.unwrap();
        assert_eq!(page.len(), 1);
        assert_eq!(page[0].id, real_id.to_string());
        assert_eq!(page[0].pending, None);
        assert_eq!(
            page[0].nonce.as_deref(),
            Some("n-1"),
            "echo keeps the nonce"
        );

        // A second pending row fails: kept, flagged.
        cache
            .insert_pending(7, 1, "doomed".into(), "n-2".into())
            .await
            .unwrap();
        cache.mark_failed("n-2".into()).await.unwrap();
        let page = cache.page_messages(7, None, 50).await.unwrap();
        assert_eq!(page.len(), 2);
        assert_eq!(page[1].failed, Some(true));
        assert_eq!(page[1].pending, None);

        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn ready_snapshot_diffs_and_bootstrap_round_trips() {
        let path = temp_db("ready");
        let cache = Cache::open(&path).unwrap();

        let me = v1::User {
            id: 1,
            username: "sooru".into(),
            display_name: "Sooru".into(),
            flags: 0,
        };
        let stale_guild = v1::Guild {
            id: 50,
            name: "gone soon".into(),
            owner_id: 1,
            channels: vec![],
            invite_code: "x".into(),
            members: vec![],
        };
        let ready1 = v1::Ready {
            gateway_session_id: 1,
            resume_token: Default::default(),
            user: Some(me.clone()),
            guilds: vec![stale_guild],
            dm_channels: vec![],
            presences: vec![],
            users: vec![],
        };
        cache.apply_ready(ready1).await.unwrap();

        let guild = v1::Guild {
            id: 10,
            name: "hq".into(),
            owner_id: 1,
            channels: vec![v1::Channel {
                id: 11,
                guild_id: 10,
                kind: v1::ChannelKind::GuildText as i32,
                name: "general".into(),
                position: 0,
                last_message_id: 0,
                recipient_ids: vec![],
            }],
            invite_code: "abc".into(),
            members: vec![v1::Member {
                user_id: 1,
                guild_id: 10,
                joined_at_ms: 0,
                permissions: 0,
            }],
        };
        let dm = v1::Channel {
            id: 20,
            guild_id: 0,
            kind: v1::ChannelKind::Dm as i32,
            name: String::new(),
            position: 0,
            last_message_id: 0,
            recipient_ids: vec![1, 2],
        };
        let ready2 = v1::Ready {
            gateway_session_id: 2,
            resume_token: Default::default(),
            user: Some(me),
            guilds: vec![guild],
            dm_channels: vec![dm],
            presences: vec![],
            users: vec![v1::User {
                id: 2,
                username: "priya7".into(),
                display_name: String::new(),
                flags: 0,
            }],
        };
        cache.apply_ready(ready2).await.unwrap();

        let boot = cache.bootstrap_snapshot().await.unwrap().unwrap();
        assert_eq!(boot.user.id, "1");
        assert_eq!(boot.guilds.len(), 1, "guild 50 diffed away");
        assert_eq!(boot.guilds[0].id, "10");
        assert_eq!(boot.guilds[0].members.len(), 1);
        assert_eq!(boot.channels.len(), 1);
        assert_eq!(boot.dms.len(), 1);
        assert_eq!(boot.dms[0].recipient_ids, vec!["1", "2"]);
        assert_eq!(boot.users.len(), 2);
        assert_eq!(
            boot.users[1].display_name, "priya7",
            "empty display name falls back to username"
        );

        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn ready_for_a_different_user_purges_the_previous_account() {
        let path = temp_db("acct-switch");
        let cache = Cache::open(&path).unwrap();

        // Account A: one guild (channel + member) and itself in the user dict.
        let guild = v1::Guild {
            id: 10,
            name: "a-hq".into(),
            owner_id: 1,
            channels: vec![v1::Channel {
                id: 11,
                guild_id: 10,
                kind: v1::ChannelKind::GuildText as i32,
                name: "general".into(),
                position: 0,
                last_message_id: 0,
                recipient_ids: vec![],
            }],
            invite_code: "a".into(),
            members: vec![v1::Member {
                user_id: 1,
                guild_id: 10,
                joined_at_ms: 0,
                permissions: 0,
            }],
        };
        cache
            .apply_ready(v1::Ready {
                gateway_session_id: 1,
                resume_token: Default::default(),
                user: Some(v1::User {
                    id: 1,
                    username: "alice".into(),
                    display_name: String::new(),
                    flags: 0,
                }),
                guilds: vec![guild],
                dm_channels: vec![],
                presences: vec![],
                users: vec![],
            })
            .await
            .unwrap();

        // Account B logs in on the SAME data dir, sharing nothing.
        cache
            .apply_ready(v1::Ready {
                gateway_session_id: 2,
                resume_token: Default::default(),
                user: Some(v1::User {
                    id: 2,
                    username: "bob".into(),
                    display_name: String::new(),
                    flags: 0,
                }),
                guilds: vec![],
                dm_channels: vec![],
                presences: vec![],
                users: vec![],
            })
            .await
            .unwrap();

        let boot = cache.bootstrap_snapshot().await.unwrap().unwrap();
        assert_eq!(boot.user.id, "2", "cache now belongs to account B");
        assert!(
            boot.guilds.is_empty(),
            "account A's guild must be purged on switch, not diffed-and-left"
        );
        assert_eq!(boot.users.len(), 1, "only account B remains in the dict");
        assert_eq!(boot.users[0].id, "2");

        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn window_cursor_connects_or_resets() {
        let path = temp_db("window");
        let cache = Cache::open(&path).unwrap();

        // Live events build the window.
        for id in [100u64, 200, 300] {
            cache
                .apply_event(Payload::MessageCreate(v1::MessageCreate {
                    message: Some(msg(id, 7, 1, "live")),
                    nonce: 0,
                }))
                .await
                .unwrap();
        }
        let sync = cache.channel_sync(7).await.unwrap().unwrap();
        assert_eq!(sync.newest_synced_id, Some(300));
        assert_eq!(sync.oldest_fetched_id, Some(100));
        assert!(!sync.stale);

        // Fresh Ready: window goes stale.
        cache
            .apply_ready(v1::Ready {
                gateway_session_id: 3,
                resume_token: Default::default(),
                user: Some(v1::User {
                    id: 1,
                    username: "u".into(),
                    display_name: String::new(),
                    flags: 0,
                }),
                guilds: vec![],
                dm_channels: vec![],
                presences: vec![],
                users: vec![],
            })
            .await
            .unwrap();
        assert!(cache.channel_sync(7).await.unwrap().unwrap().stale);

        // Overlapping newest page reconnects the ranges.
        cache
            .note_newest_page(7, vec![msg(400, 7, 1, "d"), msg(300, 7, 1, "c")])
            .await
            .unwrap();
        let sync = cache.channel_sync(7).await.unwrap().unwrap();
        assert!(!sync.stale);
        assert_eq!(sync.newest_synced_id, Some(400));
        assert_eq!(
            sync.oldest_fetched_id,
            Some(100),
            "window stayed contiguous"
        );

        // A gapped newest page resets the window to itself.
        cache.mark_all_stale().await.unwrap();
        cache
            .note_newest_page(7, vec![msg(900, 7, 1, "f"), msg(800, 7, 1, "e")])
            .await
            .unwrap();
        let sync = cache.channel_sync(7).await.unwrap().unwrap();
        assert_eq!(
            sync.oldest_fetched_id,
            Some(800),
            "window reset to the page"
        );

        // Older page extends downward.
        cache
            .note_older_page(7, vec![msg(700, 7, 1, "old")])
            .await
            .unwrap();
        let sync = cache.channel_sync(7).await.unwrap().unwrap();
        assert_eq!(sync.oldest_fetched_id, Some(700));

        // Paging: newest page (ascending) then strictly-older rows.
        let newest = cache.page_messages(7, None, 3).await.unwrap();
        let ids: Vec<&str> = newest.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, vec!["700", "800", "900"]);
        let older = cache.page_messages(7, Some(300), 50).await.unwrap();
        let ids: Vec<&str> = older.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, vec!["100", "200"]);

        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn wipe_clears_everything() {
        let path = temp_db("wipe");
        let cache = Cache::open(&path).unwrap();
        cache
            .set_current_user(v1::User {
                id: 1,
                username: "u".into(),
                display_name: String::new(),
                flags: 0,
            })
            .await
            .unwrap();
        cache
            .set_meta("last_channel_id".into(), "7".into())
            .await
            .unwrap();
        assert!(cache.bootstrap_snapshot().await.unwrap().is_some());
        cache.wipe().await.unwrap();
        assert!(cache.bootstrap_snapshot().await.unwrap().is_none());
        assert!(
            cache
                .get_meta("last_channel_id".into())
                .await
                .unwrap()
                .is_none()
        );
        let _ = std::fs::remove_file(&path);
    }
}
