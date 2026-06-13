//! [`ChatService`]: the Postgres + event-bus implementation of the binding
//! [`Chat`] trait.
//!
//! Shape of every mutating call: validate → authorize from live rows → ONE
//! transaction → commit → publish ready-to-dispatch [`BusEvent`]s. M1 accepts
//! the commit→publish gap (no transactional outbox): a failed post-commit
//! publish is logged and swallowed — live clients heal via gateway resume and
//! REST history backfill (docs/design/backend-services.md §12).

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

use dice_common::time::now_ms;
use dice_common::{ChannelId, GuildId, MessageId, SnowflakeGenerator, UserId};
use dice_event_bus::{BusEvent, EventBus, Subject};
use dice_permissions::{DEFAULT_EVERYONE, Permissions, compute};
use dice_protocol::internal::v1::bus_event::Payload as BusPayload;
use dice_protocol::v1;
use dice_protocol::v1::frame::Payload as FramePayload;
use rand::Rng;
use sqlx::PgPool;
use sqlx::types::time::OffsetDateTime;

use crate::{Chat, ChatError, HistoryCursor, UserSyncState};

/// `BusEvent.origin` for every event this service publishes.
const ORIGIN: &str = "chat-service";

/// Auto-created text channel in every new guild (critique #24e: M1 has no
/// create-channel UI in the create-guild dialog, so `#general` always exists).
/// Stored without the `#` sigil — the sigil is presentation, not identity.
const GENERAL_CHANNEL: &str = "general";

/// Invite codes: 8 chars from `[a-z0-9]` (~41 bits of entropy).
const INVITE_CODE_LEN: usize = 8;
const INVITE_CHARSET: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";

/// `dice.v1.ChannelKind` enum values, stored VERBATIM in
/// `channels.channel_type` (critique #18: no layer ever remaps enum numbers).
const KIND_GUILD_TEXT: i16 = v1::ChannelKind::GuildText as i16;
const KIND_DM: i16 = v1::ChannelKind::Dm as i16;

/// `Guild.members` cap in M1 (protocol §3). The `users` dictionary in
/// [`UserSyncState`] still covers ALL members, not just the capped list.
const MEMBER_CAP: i64 = 100;

const MAX_MESSAGE_CHARS: usize = 4000;
const MAX_NAME_CHARS: usize = 100;
const MAX_EMOJI_CHARS: usize = 64; // matches the message_reactions CHECK

/// The Postgres-backed [`Chat`] implementation used by the monolith and the
/// split-mode chat bin.
pub struct ChatService {
    pool: PgPool,
    bus: Arc<dyn EventBus>,
    ids: Arc<SnowflakeGenerator>,
}

impl ChatService {
    pub fn new(pool: PgPool, bus: Arc<dyn EventBus>, ids: Arc<SnowflakeGenerator>) -> Self {
        Self { pool, bus, ids }
    }
}

/// What `actor` may do in a channel, resolved from live rows.
enum ChannelAccess {
    Guild {
        guild_id: GuildId,
        /// Effective permissions: `compute(is_owner, [member grant])`.
        perms: Permissions,
    },
    Dm {
        /// All recipient user ids (raw, ascending), actor included.
        recipient_ids: Vec<u64>,
    },
}

#[async_trait::async_trait]
impl Chat for ChatService {
    async fn sync_user_state(&self, user: UserId) -> Result<UserSyncState, ChatError> {
        let guild_rows = sqlx::query!(
            r#"SELECT g.id AS "id!", g.name AS "name!", g.owner_id AS "owner_id!",
                      g.invite_code AS "invite_code!"
               FROM guilds g
               JOIN guild_members gm ON gm.guild_id = g.id
               WHERE gm.user_id = $1
               ORDER BY g.id"#,
            user.as_i64()
        )
        .fetch_all(&self.pool)
        .await
        .map_err(internal)?;
        let gids: Vec<i64> = guild_rows.iter().map(|r| r.id).collect();

        // The deduplicated user dictionary starts with self.
        let mut user_ids: BTreeSet<i64> = BTreeSet::new();
        user_ids.insert(user.as_i64());

        // Guild channels, batched then grouped; ORDER BY (position, id).
        let mut channels_by_guild: HashMap<i64, Vec<v1::Channel>> = HashMap::new();
        if !gids.is_empty() {
            let rows = sqlx::query!(
                r#"SELECT id, guild_id AS "guild_id!", name AS "name!", position, last_message_id
                   FROM channels WHERE guild_id = ANY($1::bigint[])
                   ORDER BY position, id"#,
                &gids[..]
            )
            .fetch_all(&self.pool)
            .await
            .map_err(internal)?;
            for r in rows {
                channels_by_guild
                    .entry(r.guild_id)
                    .or_default()
                    .push(guild_channel(
                        r.id,
                        r.guild_id,
                        r.name,
                        r.position,
                        r.last_message_id,
                    ));
            }

            // ALL members go into the user dictionary (protocol §3), even
            // beyond the per-guild member cap.
            let all_member_ids = sqlx::query_scalar!(
                r#"SELECT DISTINCT user_id AS "user_id!" FROM guild_members
                   WHERE guild_id = ANY($1::bigint[])"#,
                &gids[..]
            )
            .fetch_all(&self.pool)
            .await
            .map_err(internal)?;
            user_ids.extend(all_member_ids);
        }

        let mut guilds = Vec::with_capacity(guild_rows.len());
        for g in guild_rows {
            let members = self.load_members(GuildId::from_i64(g.id)).await?;
            guilds.push(v1::Guild {
                id: g.id as u64,
                name: g.name,
                owner_id: g.owner_id as u64,
                channels: channels_by_guild.remove(&g.id).unwrap_or_default(),
                invite_code: g.invite_code,
                members,
            });
        }

        // DM channels (kind=2) where the user is a recipient.
        let dm_rows = sqlx::query!(
            r#"SELECT c.id AS "id!", c.last_message_id
               FROM channels c
               JOIN channel_recipients cr ON cr.channel_id = c.id
               WHERE cr.user_id = $1 AND c.channel_type = $2
               ORDER BY c.id"#,
            user.as_i64(),
            KIND_DM
        )
        .fetch_all(&self.pool)
        .await
        .map_err(internal)?;
        let dm_ids: Vec<i64> = dm_rows.iter().map(|r| r.id).collect();

        let mut recipients_by_channel: HashMap<i64, Vec<u64>> = HashMap::new();
        if !dm_ids.is_empty() {
            let rows = sqlx::query!(
                r#"SELECT channel_id, user_id FROM channel_recipients
                   WHERE channel_id = ANY($1::bigint[])
                   ORDER BY user_id"#,
                &dm_ids[..]
            )
            .fetch_all(&self.pool)
            .await
            .map_err(internal)?;
            for r in rows {
                user_ids.insert(r.user_id);
                recipients_by_channel
                    .entry(r.channel_id)
                    .or_default()
                    .push(r.user_id as u64);
            }
        }
        let dm_channels = dm_rows
            .into_iter()
            .map(|r| {
                dm_channel(
                    r.id,
                    r.last_message_id,
                    recipients_by_channel.remove(&r.id).unwrap_or_default(),
                )
            })
            .collect();

        let id_list: Vec<i64> = user_ids.into_iter().collect();
        let users = sqlx::query!(
            r#"SELECT id, username, display_name, flags FROM users
               WHERE id = ANY($1::bigint[]) ORDER BY id"#,
            &id_list[..]
        )
        .fetch_all(&self.pool)
        .await
        .map_err(internal)?
        .into_iter()
        .map(|r| v1::User {
            id: r.id as u64,
            username: r.username,
            display_name: r.display_name.unwrap_or_default(),
            flags: r.flags as u32,
        })
        .collect();

        Ok(UserSyncState {
            guilds,
            dm_channels,
            users,
        })
    }

    async fn send_message(
        &self,
        actor: UserId,
        channel: ChannelId,
        content: String,
        reply_to: Option<MessageId>,
        nonce: u64,
    ) -> Result<v1::Message, ChatError> {
        let content = validate_trimmed(&content, MAX_MESSAGE_CHARS, "message content")?;
        let access = self.channel_access(actor, channel).await?;
        if let ChannelAccess::Guild { perms, .. } = &access {
            perms.check(Permissions::SEND_MESSAGES)?;
        }

        let id = self.ids.generate();
        // reply_to_id is a plain column (no FK): a since-deleted parent is fine,
        // it just renders as "original message" client-side.
        let reply_to_id = reply_to.map(|m| m.as_i64());
        let mut tx = self.pool.begin().await.map_err(internal)?;
        sqlx::query!(
            "INSERT INTO messages (id, channel_id, author_id, content, reply_to_id) \
             VALUES ($1, $2, $3, $4, $5)",
            id.as_i64(),
            channel.as_i64(),
            actor.as_i64(),
            content.as_str(),
            reply_to_id
        )
        .execute(&mut *tx)
        .await
        .map_err(internal)?;
        sqlx::query!(
            "UPDATE channels SET last_message_id = $1 WHERE id = $2",
            id.as_i64(),
            channel.as_i64()
        )
        .execute(&mut *tx)
        .await
        .map_err(internal)?;
        tx.commit().await.map_err(internal)?;

        let message = v1::Message {
            id: id.0,
            channel_id: channel.raw(),
            author_id: actor.raw(),
            content,
            edited_at_ms: 0,
            reply_to_id: reply_to.map_or(0, |m| m.raw()),
            reactions: Vec::new(),
        };
        let payload = FramePayload::MessageCreate(v1::MessageCreate {
            message: Some(message.clone()),
            nonce,
        });
        self.publish_to_channel(&access, channel, payload).await;
        Ok(message)
    }

    async fn get_messages(
        &self,
        actor: UserId,
        channel: ChannelId,
        cursor: HistoryCursor,
        limit: u8,
    ) -> Result<Vec<v1::Message>, ChatError> {
        // Membership gate only: any guild member / DM recipient may read.
        self.channel_access(actor, channel).await?;
        let n = i64::from(limit.clamp(1, 100));

        let mut messages = match cursor {
            HistoryCursor::Latest => sqlx::query!(
                "SELECT id, author_id, content, edited_at, reply_to_id FROM messages \
                 WHERE channel_id = $1 ORDER BY id DESC LIMIT $2",
                channel.as_i64(),
                n
            )
            .fetch_all(&self.pool)
            .await
            .map_err(internal)?
            .into_iter()
            .map(|r| {
                message_row(
                    channel,
                    r.id,
                    r.author_id,
                    r.content,
                    r.edited_at,
                    r.reply_to_id,
                )
            })
            .collect(),
            HistoryCursor::Before(before) => sqlx::query!(
                "SELECT id, author_id, content, edited_at, reply_to_id FROM messages \
                 WHERE channel_id = $1 AND id < $2 ORDER BY id DESC LIMIT $3",
                channel.as_i64(),
                before.as_i64(),
                n
            )
            .fetch_all(&self.pool)
            .await
            .map_err(internal)?
            .into_iter()
            .map(|r| {
                message_row(
                    channel,
                    r.id,
                    r.author_id,
                    r.content,
                    r.edited_at,
                    r.reply_to_id,
                )
            })
            .collect(),
            HistoryCursor::After(after) => {
                // Fetched ASCENDING so the keyset picks the n oldest rows
                // strictly newer than the cursor (gap backfill), then reversed
                // so the return contract stays NEWEST-FIRST for every cursor
                // variant.
                let mut rows: Vec<v1::Message> = sqlx::query!(
                    "SELECT id, author_id, content, edited_at, reply_to_id FROM messages \
                     WHERE channel_id = $1 AND id > $2 ORDER BY id ASC LIMIT $3",
                    channel.as_i64(),
                    after.as_i64(),
                    n
                )
                .fetch_all(&self.pool)
                .await
                .map_err(internal)?
                .into_iter()
                .map(|r| {
                    message_row(
                        channel,
                        r.id,
                        r.author_id,
                        r.content,
                        r.edited_at,
                        r.reply_to_id,
                    )
                })
                .collect();
                rows.reverse();
                rows
            }
        };
        self.attach_reactions(actor, &mut messages).await?;
        Ok(messages)
    }

    async fn edit_message(
        &self,
        actor: UserId,
        channel: ChannelId,
        message: MessageId,
        content: String,
    ) -> Result<v1::Message, ChatError> {
        let content = validate_trimmed(&content, MAX_MESSAGE_CHARS, "message content")?;
        let access = self.channel_access(actor, channel).await?;
        let row = sqlx::query!(
            "SELECT author_id FROM messages WHERE id = $1 AND channel_id = $2",
            message.as_i64(),
            channel.as_i64()
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(internal)?
        .ok_or(ChatError::NotFound)?;
        // Edit is strictly author-only — MANAGE_MESSAGES does NOT grant it.
        if row.author_id != actor.as_i64() {
            return Err(ChatError::Forbidden(
                "only the author can edit a message".to_owned(),
            ));
        }
        let edited_at = sqlx::query_scalar!(
            "UPDATE messages SET content = $1, edited_at = now() WHERE id = $2 RETURNING edited_at",
            content.as_str(),
            message.as_i64()
        )
        .fetch_one(&self.pool)
        .await
        .map_err(internal)?;

        let message = v1::Message {
            id: message.raw(),
            channel_id: channel.raw(),
            author_id: actor.raw(),
            content,
            edited_at_ms: edited_at.map_or(0, ms),
            // The client MERGES updates (keeps its cached reply/reactions), so an
            // edit need not re-fetch them — these stay zero/empty on the wire.
            reply_to_id: 0,
            reactions: Vec::new(),
        };
        let payload = FramePayload::MessageUpdate(v1::MessageUpdate {
            message: Some(message.clone()),
        });
        self.publish_to_channel(&access, channel, payload).await;
        Ok(message)
    }

    async fn delete_message(
        &self,
        actor: UserId,
        channel: ChannelId,
        message: MessageId,
    ) -> Result<(), ChatError> {
        let access = self.channel_access(actor, channel).await?;
        let row = sqlx::query!(
            "SELECT author_id FROM messages WHERE id = $1 AND channel_id = $2",
            message.as_i64(),
            channel.as_i64()
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(internal)?
        .ok_or(ChatError::NotFound)?;
        // Author always may; otherwise MANAGE_MESSAGES in a guild, never in a DM.
        if row.author_id != actor.as_i64() {
            match &access {
                ChannelAccess::Guild { perms, .. } => perms.check(Permissions::MANAGE_MESSAGES)?,
                ChannelAccess::Dm { .. } => {
                    return Err(ChatError::Forbidden(
                        "you can only delete your own messages".to_owned(),
                    ));
                }
            }
        }
        sqlx::query!("DELETE FROM messages WHERE id = $1", message.as_i64())
            .execute(&self.pool)
            .await
            .map_err(internal)?;

        let payload = FramePayload::MessageDelete(v1::MessageDelete {
            channel_id: channel.raw(),
            message_id: message.raw(),
        });
        self.publish_to_channel(&access, channel, payload).await;
        Ok(())
    }

    async fn add_reaction(
        &self,
        actor: UserId,
        channel: ChannelId,
        message: MessageId,
        emoji: String,
    ) -> Result<(), ChatError> {
        let emoji = validate_trimmed(&emoji, MAX_EMOJI_CHARS, "reaction")?;
        let access = self.channel_access(actor, channel).await?;
        self.require_message_in_channel(channel, message).await?;
        let changed = sqlx::query!(
            "INSERT INTO message_reactions (message_id, user_id, emoji) VALUES ($1, $2, $3) \
             ON CONFLICT DO NOTHING",
            message.as_i64(),
            actor.as_i64(),
            emoji.as_str()
        )
        .execute(&self.pool)
        .await
        .map_err(internal)?
        .rows_affected();
        // Only fan out a delta when the state actually changed (no double-add).
        if changed > 0 {
            self.broadcast_reaction(&access, channel, message, &emoji, actor, true)
                .await;
        }
        Ok(())
    }

    async fn remove_reaction(
        &self,
        actor: UserId,
        channel: ChannelId,
        message: MessageId,
        emoji: String,
    ) -> Result<(), ChatError> {
        let emoji = validate_trimmed(&emoji, MAX_EMOJI_CHARS, "reaction")?;
        let access = self.channel_access(actor, channel).await?;
        let changed = sqlx::query!(
            "DELETE FROM message_reactions WHERE message_id = $1 AND user_id = $2 AND emoji = $3",
            message.as_i64(),
            actor.as_i64(),
            emoji.as_str()
        )
        .execute(&self.pool)
        .await
        .map_err(internal)?
        .rows_affected();
        if changed > 0 {
            self.broadcast_reaction(&access, channel, message, &emoji, actor, false)
                .await;
        }
        Ok(())
    }

    async fn create_guild(&self, actor: UserId, name: String) -> Result<v1::Guild, ChatError> {
        let name = validate_trimmed(&name, MAX_NAME_CHARS, "guild name")?;
        let guild = match self
            .try_create_guild(actor, &name, &generate_invite_code())
            .await
        {
            Ok(g) => g,
            Err(e) if is_invite_collision(&e) => {
                // 1-in-36^8 per attempt; retry exactly once with a fresh code.
                tracing::warn!("invite code collision on create_guild; retrying once");
                self.try_create_guild(actor, &name, &generate_invite_code())
                    .await
                    .map_err(internal)?
            }
            Err(e) => return Err(internal(e)),
        };

        // Critique #14: the gateway adds guild interest for this session from
        // the GuildCreate it sees on the creator's user subject.
        let event = self.make_event(
            guild.id,
            vec![actor.raw()],
            false,
            FramePayload::GuildCreate(v1::GuildCreate {
                guild: Some(guild.clone()),
            }),
        );
        self.publish(Subject::User(actor), event).await;
        Ok(guild)
    }

    async fn join_guild(&self, actor: UserId, code: &str) -> Result<v1::Guild, ChatError> {
        let row = sqlx::query!("SELECT id FROM guilds WHERE invite_code = $1", code.trim())
            .fetch_optional(&self.pool)
            .await
            .map_err(internal)?
            .ok_or(ChatError::InvalidInvite)?;
        let guild_id = GuildId::from_i64(row.id);

        // Idempotent: RETURNING only yields a row when the INSERT happened,
        // so a re-join skips the publishes and just returns the guild.
        let inserted_at = sqlx::query_scalar!(
            "INSERT INTO guild_members (guild_id, user_id, permissions) VALUES ($1, $2, $3) \
             ON CONFLICT (guild_id, user_id) DO NOTHING RETURNING joined_at",
            row.id,
            actor.as_i64(),
            DEFAULT_EVERYONE.to_db()
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(internal)?;

        let guild = self.load_full_guild(guild_id).await?;
        if let Some(joined_at) = inserted_at {
            let user = self.load_user(actor).await?;
            let member = v1::Member {
                user_id: actor.raw(),
                guild_id: guild_id.raw(),
                joined_at_ms: ms(joined_at),
                permissions: DEFAULT_EVERYONE.bits(),
            };
            let member_add = self.make_event(
                guild_id.raw(),
                Vec::new(),
                false,
                FramePayload::MemberAdd(v1::GuildMemberAdd {
                    member: Some(member),
                    user: Some(user),
                }),
            );
            self.publish(Subject::GuildMsg(guild_id), member_add).await;

            // Critique #14: full guild to the joiner's user subject so their
            // gateway session gains interest mid-session.
            let guild_create = self.make_event(
                guild_id.raw(),
                vec![actor.raw()],
                false,
                FramePayload::GuildCreate(v1::GuildCreate {
                    guild: Some(guild.clone()),
                }),
            );
            self.publish(Subject::User(actor), guild_create).await;
        }
        Ok(guild)
    }

    async fn create_channel(
        &self,
        actor: UserId,
        guild: GuildId,
        name: String,
    ) -> Result<v1::Channel, ChatError> {
        let name = validate_trimmed(&name, MAX_NAME_CHARS, "channel name")?;
        let row = sqlx::query!(
            r#"SELECT g.owner_id AS "owner_id!", gm.permissions AS "permissions?"
               FROM guilds g
               LEFT JOIN guild_members gm ON gm.guild_id = g.id AND gm.user_id = $2
               WHERE g.id = $1"#,
            guild.as_i64(),
            actor.as_i64()
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(internal)?
        .ok_or(ChatError::NotFound)?;
        let grant = row.permissions.ok_or(ChatError::NotAMember)?;
        compute(
            row.owner_id == actor.as_i64(),
            [Permissions::from_db(grant)],
        )
        .check(Permissions::MANAGE_CHANNELS)?;

        let id = self.ids.generate();
        let position = sqlx::query_scalar!(
            "INSERT INTO channels (id, channel_type, guild_id, name, position) \
             VALUES ($1, $2, $3, $4, \
                     (SELECT COALESCE(MAX(position) + 1, 0) FROM channels WHERE guild_id = $3)) \
             RETURNING position",
            id.as_i64(),
            KIND_GUILD_TEXT,
            guild.as_i64(),
            name.as_str()
        )
        .fetch_one(&self.pool)
        .await
        .map_err(internal)?;

        let channel = guild_channel(id.as_i64(), guild.as_i64(), name, position, None);
        let event = self.make_event(
            guild.raw(),
            Vec::new(),
            false,
            FramePayload::ChannelCreate(v1::ChannelCreate {
                channel: Some(channel.clone()),
            }),
        );
        self.publish(Subject::GuildMsg(guild), event).await;
        Ok(channel)
    }

    async fn open_dm(&self, actor: UserId, other: UserId) -> Result<v1::Channel, ChatError> {
        if actor == other {
            return Err(ChatError::InvalidArgument(
                "cannot open a DM with yourself".to_owned(),
            ));
        }
        sqlx::query!("SELECT id FROM users WHERE id = $1", other.as_i64())
            .fetch_optional(&self.pool)
            .await
            .map_err(internal)?
            .ok_or(ChatError::NotFound)?;

        let key = dm_key(actor, other);
        let (lo, hi) = (actor.raw().min(other.raw()), actor.raw().max(other.raw()));
        let new_id = self.ids.generate();

        let mut tx = self.pool.begin().await.map_err(internal)?;
        // channels_dm_key_key is a PARTIAL unique index, so the conflict
        // target must repeat its predicate.
        let created = sqlx::query!(
            "INSERT INTO channels (id, channel_type, dm_key) VALUES ($1, $2, $3) \
             ON CONFLICT (dm_key) WHERE dm_key IS NOT NULL DO NOTHING",
            new_id.as_i64(),
            KIND_DM,
            key.as_str()
        )
        .execute(&mut *tx)
        .await
        .map_err(internal)?
        .rows_affected()
            == 1;
        let row = sqlx::query!(
            "SELECT id, last_message_id FROM channels WHERE dm_key = $1",
            key.as_str()
        )
        .fetch_one(&mut *tx)
        .await
        .map_err(internal)?;
        for uid in [lo, hi] {
            sqlx::query!(
                "INSERT INTO channel_recipients (channel_id, user_id) VALUES ($1, $2) \
                 ON CONFLICT (channel_id, user_id) DO NOTHING",
                row.id,
                uid as i64
            )
            .execute(&mut *tx)
            .await
            .map_err(internal)?;
        }
        tx.commit().await.map_err(internal)?;

        let channel = dm_channel(row.id, row.last_message_id, vec![lo, hi]);
        if created {
            // Both recipients learn about the channel on their user subjects
            // (critique #14: the gateway adds DM interest from this event).
            for uid in [actor, other] {
                let event = self.make_event(
                    0,
                    vec![lo, hi],
                    false,
                    FramePayload::DmChannelCreate(v1::DmChannelCreate {
                        channel: Some(channel.clone()),
                    }),
                );
                self.publish(Subject::User(uid), event).await;
            }
        }
        Ok(channel)
    }

    async fn typing(&self, actor: UserId, channel: ChannelId) -> Result<(), ChatError> {
        let payload = FramePayload::TypingStart(v1::TypingStart {
            channel_id: channel.raw(),
            user_id: actor.raw(),
        });
        match self.channel_access(actor, channel).await? {
            ChannelAccess::Guild { guild_id, .. } => {
                let event = self.make_event(guild_id.raw(), Vec::new(), true, payload);
                self.publish(Subject::GuildTyping(guild_id), event).await;
            }
            ChannelAccess::Dm { recipient_ids } => {
                let event = self.make_event(0, recipient_ids, true, payload);
                self.publish(Subject::DmTyping(channel), event).await;
            }
        }
        Ok(())
    }
}

impl ChatService {
    /// Resolve a channel and what `actor` may do in it.
    async fn channel_access(
        &self,
        actor: UserId,
        channel: ChannelId,
    ) -> Result<ChannelAccess, ChatError> {
        let row = sqlx::query!(
            "SELECT channel_type, guild_id FROM channels WHERE id = $1",
            channel.as_i64()
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(internal)?
        .ok_or(ChatError::NotFound)?;

        if row.channel_type == KIND_GUILD_TEXT {
            let Some(guild_id) = row.guild_id else {
                return Err(internal("guild channel row without guild_id"));
            };
            let member = sqlx::query!(
                r#"SELECT g.owner_id AS "owner_id!", gm.permissions AS "permissions!"
                   FROM guilds g
                   JOIN guild_members gm ON gm.guild_id = g.id AND gm.user_id = $2
                   WHERE g.id = $1"#,
                guild_id,
                actor.as_i64()
            )
            .fetch_optional(&self.pool)
            .await
            .map_err(internal)?
            .ok_or(ChatError::NotAMember)?;
            let perms = compute(
                member.owner_id == actor.as_i64(),
                [Permissions::from_db(member.permissions)],
            );
            Ok(ChannelAccess::Guild {
                guild_id: GuildId::from_i64(guild_id),
                perms,
            })
        } else if row.channel_type == KIND_DM {
            let recipients = sqlx::query_scalar!(
                "SELECT user_id FROM channel_recipients WHERE channel_id = $1 ORDER BY user_id",
                channel.as_i64()
            )
            .fetch_all(&self.pool)
            .await
            .map_err(internal)?;
            if !recipients.contains(&actor.as_i64()) {
                return Err(ChatError::NotAMember);
            }
            Ok(ChannelAccess::Dm {
                recipient_ids: recipients.into_iter().map(|v| v as u64).collect(),
            })
        } else {
            Err(internal(format!(
                "unknown channel_type {} for channel {channel}",
                row.channel_type
            )))
        }
    }

    /// One guild + channels (position, id) + members (cap [`MEMBER_CAP`]).
    async fn load_full_guild(&self, guild: GuildId) -> Result<v1::Guild, ChatError> {
        let g = sqlx::query!(
            "SELECT id, name, owner_id, invite_code FROM guilds WHERE id = $1",
            guild.as_i64()
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(internal)?
        .ok_or(ChatError::NotFound)?;
        let channels = sqlx::query!(
            r#"SELECT id, name AS "name!", position, last_message_id
               FROM channels WHERE guild_id = $1 ORDER BY position, id"#,
            guild.as_i64()
        )
        .fetch_all(&self.pool)
        .await
        .map_err(internal)?
        .into_iter()
        .map(|r| guild_channel(r.id, g.id, r.name, r.position, r.last_message_id))
        .collect();
        let members = self.load_members(guild).await?;
        Ok(v1::Guild {
            id: g.id as u64,
            name: g.name,
            owner_id: g.owner_id as u64,
            channels,
            invite_code: g.invite_code,
            members,
        })
    }

    async fn load_members(&self, guild: GuildId) -> Result<Vec<v1::Member>, ChatError> {
        let rows = sqlx::query!(
            "SELECT user_id, permissions, joined_at FROM guild_members \
             WHERE guild_id = $1 ORDER BY joined_at, user_id LIMIT $2",
            guild.as_i64(),
            MEMBER_CAP
        )
        .fetch_all(&self.pool)
        .await
        .map_err(internal)?;
        Ok(rows
            .into_iter()
            .map(|r| v1::Member {
                user_id: r.user_id as u64,
                guild_id: guild.raw(),
                joined_at_ms: ms(r.joined_at),
                permissions: Permissions::from_db(r.permissions).bits(),
            })
            .collect())
    }

    async fn load_user(&self, user: UserId) -> Result<v1::User, ChatError> {
        let r = sqlx::query!(
            "SELECT id, username, display_name, flags FROM users WHERE id = $1",
            user.as_i64()
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(internal)?
        .ok_or(ChatError::NotFound)?;
        Ok(v1::User {
            id: r.id as u64,
            username: r.username,
            display_name: r.display_name.unwrap_or_default(),
            flags: r.flags as u32,
        })
    }

    /// The whole guild creation transaction, returning the full guild without
    /// re-querying. Errors bubble raw so the caller can spot the invite-code
    /// unique collision and retry.
    async fn try_create_guild(
        &self,
        actor: UserId,
        name: &str,
        code: &str,
    ) -> Result<v1::Guild, sqlx::Error> {
        let guild_id = self.ids.generate();
        let channel_id = self.ids.generate();
        let mut tx = self.pool.begin().await?;
        sqlx::query!(
            "INSERT INTO guilds (id, name, owner_id, invite_code) VALUES ($1, $2, $3, $4)",
            guild_id.as_i64(),
            name,
            actor.as_i64(),
            code
        )
        .execute(&mut *tx)
        .await?;
        let joined_at = sqlx::query_scalar!(
            "INSERT INTO guild_members (guild_id, user_id, permissions) VALUES ($1, $2, $3) \
             RETURNING joined_at",
            guild_id.as_i64(),
            actor.as_i64(),
            DEFAULT_EVERYONE.to_db()
        )
        .fetch_one(&mut *tx)
        .await?;
        sqlx::query!(
            "INSERT INTO channels (id, channel_type, guild_id, name, position) \
             VALUES ($1, $2, $3, $4, 0)",
            channel_id.as_i64(),
            KIND_GUILD_TEXT,
            guild_id.as_i64(),
            GENERAL_CHANNEL
        )
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;

        Ok(v1::Guild {
            id: guild_id.0,
            name: name.to_owned(),
            owner_id: actor.raw(),
            channels: vec![guild_channel(
                channel_id.as_i64(),
                guild_id.as_i64(),
                GENERAL_CHANNEL.to_owned(),
                0,
                None,
            )],
            invite_code: code.to_owned(),
            members: vec![v1::Member {
                user_id: actor.raw(),
                guild_id: guild_id.0,
                joined_at_ms: ms(joined_at),
                permissions: DEFAULT_EVERYONE.bits(),
            }],
        })
    }

    /// A ready-to-dispatch [`BusEvent`] wrapping `Frame{seq:0, payload}`.
    fn make_event(
        &self,
        guild_id: u64,
        recipient_user_ids: Vec<u64>,
        ephemeral: bool,
        payload: FramePayload,
    ) -> BusEvent {
        BusEvent {
            event_id: self.ids.generate().0,
            emitted_at_ms: now_ms(),
            origin: ORIGIN.to_owned(),
            guild_id,
            recipient_user_ids,
            ephemeral,
            payload: Some(BusPayload::Frame(v1::Frame::dispatch(payload))),
        }
    }

    /// Post-commit publish: failures are logged, never propagated — the DB is
    /// already committed and clients heal via resume + REST backfill.
    async fn publish(&self, subject: Subject, event: BusEvent) {
        if let Err(error) = self.bus.publish(subject, event).await {
            tracing::error!(%error, %subject, "post-commit bus publish failed");
        }
    }

    /// Route a non-ephemeral message dispatch (create/update/delete) to the
    /// channel's subject — guild members or DM recipients, identical fan-out.
    async fn publish_to_channel(
        &self,
        access: &ChannelAccess,
        channel: ChannelId,
        payload: FramePayload,
    ) {
        match access {
            ChannelAccess::Guild { guild_id, .. } => {
                let event = self.make_event(guild_id.raw(), Vec::new(), false, payload);
                self.publish(Subject::GuildMsg(*guild_id), event).await;
            }
            ChannelAccess::Dm { recipient_ids } => {
                let event = self.make_event(0, recipient_ids.clone(), false, payload);
                self.publish(Subject::DmMsg(channel), event).await;
            }
        }
    }

    /// Confirm a message exists in this channel before reacting to it (a
    /// reaction to an unknown/foreign message is a clean `NotFound`).
    async fn require_message_in_channel(
        &self,
        channel: ChannelId,
        message: MessageId,
    ) -> Result<(), ChatError> {
        let exists = sqlx::query_scalar!(
            "SELECT EXISTS(SELECT 1 FROM messages WHERE id = $1 AND channel_id = $2)",
            message.as_i64(),
            channel.as_i64()
        )
        .fetch_one(&self.pool)
        .await
        .map_err(internal)?;
        if exists == Some(true) {
            Ok(())
        } else {
            Err(ChatError::NotFound)
        }
    }

    /// Broadcast a reaction DELTA to the channel (each client adjusts its own
    /// aggregate and flips `me` when `user_id` is itself).
    async fn broadcast_reaction(
        &self,
        access: &ChannelAccess,
        channel: ChannelId,
        message: MessageId,
        emoji: &str,
        actor: UserId,
        added: bool,
    ) {
        let payload = FramePayload::ReactionUpdate(v1::ReactionUpdate {
            channel_id: channel.raw(),
            message_id: message.raw(),
            emoji: emoji.to_owned(),
            user_id: actor.raw(),
            added,
        });
        self.publish_to_channel(access, channel, payload).await;
    }

    /// Populate each message's `reactions` with the per-emoji aggregate from
    /// the requesting user's perspective (one grouped query for the whole page).
    async fn attach_reactions(
        &self,
        actor: UserId,
        messages: &mut [v1::Message],
    ) -> Result<(), ChatError> {
        if messages.is_empty() {
            return Ok(());
        }
        let ids: Vec<i64> = messages.iter().map(|m| m.id as i64).collect();
        let rows = sqlx::query!(
            r#"SELECT message_id, emoji, COUNT(*) AS "count!", BOOL_OR(user_id = $2) AS "me!"
               FROM message_reactions WHERE message_id = ANY($1)
               GROUP BY message_id, emoji ORDER BY message_id, MIN(created_at)"#,
            &ids[..],
            actor.as_i64()
        )
        .fetch_all(&self.pool)
        .await
        .map_err(internal)?;
        let mut by_msg: std::collections::HashMap<i64, Vec<v1::Reaction>> =
            std::collections::HashMap::new();
        for r in rows {
            by_msg.entry(r.message_id).or_default().push(v1::Reaction {
                emoji: r.emoji,
                count: u32::try_from(r.count).unwrap_or(u32::MAX),
                me: r.me,
            });
        }
        for m in messages.iter_mut() {
            if let Some(reactions) = by_msg.remove(&(m.id as i64)) {
                m.reactions = reactions;
            }
        }
        Ok(())
    }
}

fn internal<E>(e: E) -> ChatError
where
    E: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    let e = e.into();
    tracing::error!(error = %e, "chat-service internal error");
    ChatError::Internal(e)
}

fn is_invite_collision(e: &sqlx::Error) -> bool {
    matches!(e, sqlx::Error::Database(db) if db.constraint() == Some("guilds_invite_code_key"))
}

/// Trim, then enforce `1..=max` chars (chars, not bytes — matches the DB
/// `char_length` CHECKs).
fn validate_trimmed(s: &str, max: usize, what: &str) -> Result<String, ChatError> {
    let trimmed = s.trim();
    let chars = trimmed.chars().count();
    if chars == 0 {
        return Err(ChatError::InvalidArgument(format!(
            "{what} must not be empty after trimming"
        )));
    }
    if chars > max {
        return Err(ChatError::InvalidArgument(format!(
            "{what} exceeds {max} characters (got {chars})"
        )));
    }
    Ok(trimmed.to_owned())
}

fn generate_invite_code() -> String {
    let mut rng = rand::rng();
    (0..INVITE_CODE_LEN)
        .map(|_| INVITE_CHARSET[rng.random_range(0..INVITE_CHARSET.len())] as char)
        .collect()
}

/// Canonical DM dedup key: `"{min}:{max}"` of the two raw user ids.
fn dm_key(a: UserId, b: UserId) -> String {
    let (lo, hi) = if a.raw() <= b.raw() {
        (a.raw(), b.raw())
    } else {
        (b.raw(), a.raw())
    };
    format!("{lo}:{hi}")
}

fn ms(t: OffsetDateTime) -> u64 {
    u64::try_from(t.unix_timestamp_nanos() / 1_000_000).unwrap_or(0)
}

fn guild_channel(
    id: i64,
    guild_id: i64,
    name: String,
    position: i32,
    last_message_id: Option<i64>,
) -> v1::Channel {
    v1::Channel {
        id: id as u64,
        guild_id: guild_id as u64,
        kind: v1::ChannelKind::GuildText as i32,
        name,
        position: u32::try_from(position).unwrap_or(0),
        last_message_id: last_message_id.map_or(0, |v| v as u64),
        recipient_ids: Vec::new(),
    }
}

fn dm_channel(id: i64, last_message_id: Option<i64>, recipient_ids: Vec<u64>) -> v1::Channel {
    v1::Channel {
        id: id as u64,
        guild_id: 0,
        kind: v1::ChannelKind::Dm as i32,
        name: String::new(),
        position: 0,
        last_message_id: last_message_id.map_or(0, |v| v as u64),
        recipient_ids,
    }
}

fn message_row(
    channel: ChannelId,
    id: i64,
    author_id: i64,
    content: String,
    edited_at: Option<OffsetDateTime>,
    reply_to_id: Option<i64>,
) -> v1::Message {
    v1::Message {
        id: id as u64,
        channel_id: channel.raw(),
        author_id: author_id as u64,
        content,
        edited_at_ms: edited_at.map_or(0, ms),
        reply_to_id: reply_to_id.map_or(0, |v| v as u64),
        // Filled by `attach_reactions` after the page is built.
        reactions: Vec::new(),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn invite_codes_are_8_chars_from_the_charset() {
        for _ in 0..200 {
            let code = generate_invite_code();
            assert_eq!(code.len(), INVITE_CODE_LEN);
            assert!(
                code.bytes().all(|b| INVITE_CHARSET.contains(&b)),
                "bad code {code:?}"
            );
        }
    }

    #[test]
    fn dm_key_is_order_independent_min_max() {
        let a = UserId::from_raw(7);
        let b = UserId::from_raw(3);
        assert_eq!(dm_key(a, b), "3:7");
        assert_eq!(dm_key(b, a), "3:7");
        assert_eq!(dm_key(a, a), "7:7");
    }

    #[test]
    fn validate_trimmed_enforces_char_bounds() {
        assert!(matches!(
            validate_trimmed("", 10, "x"),
            Err(ChatError::InvalidArgument(_))
        ));
        assert!(matches!(
            validate_trimmed("  \n\t ", 10, "x"),
            Err(ChatError::InvalidArgument(_))
        ));
        assert!(matches!(
            validate_trimmed("abcdefghijk", 10, "x"),
            Err(ChatError::InvalidArgument(_))
        ));
        assert_eq!(validate_trimmed("  hi  ", 10, "x").unwrap(), "hi");
        // chars, not bytes: 10 two-byte chars pass a max of 10.
        assert_eq!(
            validate_trimmed(&"é".repeat(10), 10, "x").unwrap().len(),
            20
        );
    }

    #[test]
    fn channel_kind_constants_match_the_wire_enum() {
        // Critique #18: stored VERBATIM, never remapped.
        assert_eq!(KIND_GUILD_TEXT, 1);
        assert_eq!(KIND_DM, 2);
    }
}
