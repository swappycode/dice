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
use dice_common::{ChannelId, GuildId, MediaId, MessageId, SnowflakeGenerator, UserId};
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
const KIND_VOICE: i16 = v1::ChannelKind::Voice as i16;

/// `Guild.members` cap in M1 (protocol §3). The `users` dictionary in
/// [`UserSyncState`] still covers ALL members, not just the capped list.
const MEMBER_CAP: i64 = 100;

/// `friendships.status` discriminants (stored verbatim).
const FRIEND_PENDING: i16 = 1;
const FRIEND_ACCEPTED: i16 = 2;

const MAX_MESSAGE_CHARS: usize = 4000;
const MAX_NAME_CHARS: usize = 100;
const MAX_EMOJI_CHARS: usize = 64; // matches the message_reactions CHECK
const MAX_ATTACHMENTS: usize = 10; // per message

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
                r#"SELECT id, guild_id AS "guild_id!", channel_type, name AS "name!", position, last_message_id
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
                        r.channel_type,
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
            r#"SELECT id, username, display_name, flags, avatar_media_id FROM users
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
            avatar_id: r.avatar_media_id.map_or(0, |v| v as u64),
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
        attachments: Vec<MediaId>,
        nonce: u64,
    ) -> Result<v1::Message, ChatError> {
        // Content may be empty IFF there is ≥1 attachment; trim + cap either way
        // (chars, not bytes — matches the relaxed `messages_content_check`).
        let content = content.trim().to_owned();
        let content_chars = content.chars().count();
        if content_chars > MAX_MESSAGE_CHARS {
            return Err(ChatError::InvalidArgument(format!(
                "message content exceeds {MAX_MESSAGE_CHARS} characters (got {content_chars})"
            )));
        }
        if content.is_empty() && attachments.is_empty() {
            return Err(ChatError::InvalidArgument(
                "a message needs content or at least one attachment".to_owned(),
            ));
        }
        if attachments.len() > MAX_ATTACHMENTS {
            return Err(ChatError::InvalidArgument(format!(
                "a message may have at most {MAX_ATTACHMENTS} attachments (got {})",
                attachments.len()
            )));
        }
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
        // Claim attachments in the same tx (each owned by the sender + unused).
        let wire_attachments = self
            .attach_to_message(&mut tx, actor, MessageId::from(id), &attachments)
            .await?;
        sqlx::query!(
            "UPDATE channels SET last_message_id = $1 WHERE id = $2",
            id.as_i64(),
            channel.as_i64()
        )
        .execute(&mut *tx)
        .await
        .map_err(internal)?;
        tx.commit().await.map_err(internal)?;
        dice_metrics::counter!("dice_chat_messages_total").increment(1);

        let message = v1::Message {
            id: id.0,
            channel_id: channel.raw(),
            author_id: actor.raw(),
            content,
            edited_at_ms: 0,
            reply_to_id: reply_to.map_or(0, |m| m.raw()),
            reactions: Vec::new(),
            attachments: wire_attachments,
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
        self.attach_attachments(&mut messages).await?;
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
            // The client MERGES updates (keeps its cached reply/reactions/
            // attachments), so an edit need not re-fetch them — they stay
            // zero/empty on the wire.
            reply_to_id: 0,
            reactions: Vec::new(),
            attachments: Vec::new(),
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
        kind: v1::ChannelKind,
    ) -> Result<v1::Channel, ChatError> {
        // Only guild channels are creatable here; UNSPECIFIED defaults to text
        // (back-compat), DMs go through open_dm.
        let stored_kind: i16 = match kind {
            v1::ChannelKind::Unspecified | v1::ChannelKind::GuildText => KIND_GUILD_TEXT,
            v1::ChannelKind::Voice => KIND_VOICE,
            v1::ChannelKind::Dm => {
                return Err(ChatError::InvalidArgument(
                    "cannot create a DM channel here".to_owned(),
                ));
            }
        };
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
            stored_kind,
            guild.as_i64(),
            name.as_str()
        )
        .fetch_one(&self.pool)
        .await
        .map_err(internal)?;

        let channel = guild_channel(
            id.as_i64(),
            guild.as_i64(),
            stored_kind,
            name,
            position,
            None,
        );
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

    async fn mark_read(&self, actor: UserId, channel: ChannelId) -> Result<(), ChatError> {
        // Visibility check (errors NotFound / NotAMember).
        self.channel_access(actor, channel).await?;
        // The read marker tracks the channel's current newest message (0 if none).
        let last = sqlx::query_scalar!(
            "SELECT last_message_id FROM channels WHERE id = $1",
            channel.as_i64()
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(internal)?
        .flatten()
        .unwrap_or(0);
        sqlx::query!(
            "INSERT INTO read_markers (user_id, channel_id, last_read_message_id) \
             VALUES ($1, $2, $3) \
             ON CONFLICT (user_id, channel_id) DO UPDATE SET \
                 last_read_message_id = GREATEST(read_markers.last_read_message_id, excluded.last_read_message_id), \
                 updated_at = now()",
            actor.as_i64(),
            channel.as_i64(),
            last
        )
        .execute(&self.pool)
        .await
        .map_err(internal)?;
        // Broadcast to the user's own subject: other devices clear the badge.
        let payload = FramePayload::ReadMarkerUpdate(v1::ReadMarkerUpdate {
            channel_id: channel.raw(),
            last_read_message_id: last as u64,
        });
        let event = self.make_event(0, vec![actor.raw()], false, payload);
        self.publish(Subject::User(actor), event).await;
        Ok(())
    }

    async fn set_avatar(
        &self,
        actor: UserId,
        media: Option<MediaId>,
    ) -> Result<v1::User, ChatError> {
        // A non-None avatar must be an image the caller uploaded.
        if let Some(m) = media {
            let row = sqlx::query!(
                "SELECT content_type FROM media WHERE id = $1 AND uploader_id = $2",
                m.as_i64(),
                actor.as_i64()
            )
            .fetch_optional(&self.pool)
            .await
            .map_err(internal)?
            .ok_or_else(|| {
                ChatError::InvalidArgument("avatar media not found or not yours".to_owned())
            })?;
            if !row.content_type.starts_with("image/") {
                return Err(ChatError::InvalidArgument(
                    "avatar must be an image".to_owned(),
                ));
            }
        }
        sqlx::query!(
            "UPDATE users SET avatar_media_id = $1, updated_at = now() WHERE id = $2",
            media.map(|m| m.as_i64()),
            actor.as_i64()
        )
        .execute(&self.pool)
        .await
        .map_err(internal)?;

        let user = self.load_user(actor).await?;
        self.broadcast_user_update(actor, &user).await;
        Ok(user)
    }

    // ---- Friends / social (M3) ----

    async fn list_friends(&self, actor: UserId) -> Result<v1::FriendList, ChatError> {
        let rows = sqlx::query!(
            "SELECT user_lo, user_hi, status, requester_id FROM friendships \
             WHERE user_lo = $1 OR user_hi = $1 \
             ORDER BY created_at DESC",
            actor.as_i64()
        )
        .fetch_all(&self.pool)
        .await
        .map_err(internal)?;
        let mut friends = Vec::with_capacity(rows.len());
        for r in rows {
            let other = if r.user_lo == actor.as_i64() {
                r.user_hi
            } else {
                r.user_lo
            };
            let user = self.load_user(UserId::from_raw(other as u64)).await?;
            friends.push(v1::Friend {
                user: Some(user),
                status: friend_status(r.status, r.requester_id, actor) as i32,
            });
        }
        Ok(v1::FriendList { friends })
    }

    async fn add_friend(&self, actor: UserId, username: &str) -> Result<v1::Friend, ChatError> {
        let username = username.trim();
        if username.is_empty() {
            return Err(ChatError::InvalidArgument(
                "username must not be empty".to_owned(),
            ));
        }
        let target_id = sqlx::query_scalar!(
            "SELECT id FROM users WHERE LOWER(username) = LOWER($1)",
            username
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(internal)?
        .ok_or(ChatError::NotFound)?;
        let target = UserId::from_raw(target_id as u64);
        if target == actor {
            return Err(ChatError::InvalidArgument(
                "you cannot add yourself".to_owned(),
            ));
        }

        let (lo, hi) = friend_pair(actor, target);
        let mut tx = self.pool.begin().await.map_err(internal)?;
        // Claim a fresh pending row. ON CONFLICT serializes on the pair PK, so a
        // concurrent mutual add (A adds B while B adds A) can't double-insert and
        // 500 — the loser falls through to re-derive the outcome from the now-
        // committed row (locked FOR UPDATE). Mirrors open_dm's insert-then-select.
        let inserted = sqlx::query!(
            "INSERT INTO friendships (user_lo, user_hi, status, requester_id) \
             VALUES ($1, $2, $3, $4) ON CONFLICT (user_lo, user_hi) DO NOTHING",
            lo,
            hi,
            FRIEND_PENDING,
            actor.as_i64()
        )
        .execute(&mut *tx)
        .await
        .map_err(internal)?
        .rows_affected()
            == 1;

        // Fresh insert = create pending; otherwise the pair already existed:
        // reverse-pending = accept; same-direction pending or accepted = no change.
        enum Step {
            Created,
            Accepted,
            AlreadyPending,
            AlreadyFriends,
        }
        let step = if inserted {
            Step::Created
        } else {
            let row = sqlx::query!(
                "SELECT status, requester_id FROM friendships \
                 WHERE user_lo = $1 AND user_hi = $2 FOR UPDATE",
                lo,
                hi
            )
            .fetch_one(&mut *tx)
            .await
            .map_err(internal)?;
            if row.status == FRIEND_ACCEPTED {
                Step::AlreadyFriends
            } else if row.requester_id == actor.as_i64() {
                Step::AlreadyPending
            } else {
                // The other user already requested me — adding them accepts it.
                sqlx::query!(
                    "UPDATE friendships SET status = $3, updated_at = now() \
                     WHERE user_lo = $1 AND user_hi = $2",
                    lo,
                    hi,
                    FRIEND_ACCEPTED
                )
                .execute(&mut *tx)
                .await
                .map_err(internal)?;
                Step::Accepted
            }
        };
        tx.commit().await.map_err(internal)?;

        match step {
            Step::AlreadyFriends => Err(ChatError::InvalidArgument("already friends".to_owned())),
            Step::Accepted => self.on_friend_accepted(actor, target).await,
            Step::Created => {
                let actor_user = self.load_user(actor).await?;
                let target_user = self.load_user(target).await?;
                self.publish_friend_update(
                    actor,
                    target_user.clone(),
                    v1::FriendStatus::PendingOutgoing,
                    false,
                )
                .await;
                self.publish_friend_update(
                    target,
                    actor_user,
                    v1::FriendStatus::PendingIncoming,
                    false,
                )
                .await;
                Ok(v1::Friend {
                    user: Some(target_user),
                    status: v1::FriendStatus::PendingOutgoing as i32,
                })
            }
            Step::AlreadyPending => {
                let target_user = self.load_user(target).await?;
                Ok(v1::Friend {
                    user: Some(target_user),
                    status: v1::FriendStatus::PendingOutgoing as i32,
                })
            }
        }
    }

    async fn accept_friend(&self, actor: UserId, other: UserId) -> Result<v1::Friend, ChatError> {
        if actor == other {
            return Err(ChatError::InvalidArgument("invalid friend".to_owned()));
        }
        let (lo, hi) = friend_pair(actor, other);
        let mut tx = self.pool.begin().await.map_err(internal)?;
        let row = sqlx::query!(
            "SELECT status, requester_id FROM friendships \
             WHERE user_lo = $1 AND user_hi = $2 FOR UPDATE",
            lo,
            hi
        )
        .fetch_optional(&mut *tx)
        .await
        .map_err(internal)?
        .ok_or(ChatError::NotFound)?;
        if row.status == FRIEND_ACCEPTED {
            tx.rollback().await.ok();
            let other_user = self.load_user(other).await?;
            return Ok(v1::Friend {
                user: Some(other_user),
                status: v1::FriendStatus::Accepted as i32,
            });
        }
        // Only the recipient of a pending request may accept it.
        if row.requester_id == actor.as_i64() {
            return Err(ChatError::Forbidden(
                "cannot accept your own request".to_owned(),
            ));
        }
        sqlx::query!(
            "UPDATE friendships SET status = $3, updated_at = now() \
             WHERE user_lo = $1 AND user_hi = $2",
            lo,
            hi,
            FRIEND_ACCEPTED
        )
        .execute(&mut *tx)
        .await
        .map_err(internal)?;
        tx.commit().await.map_err(internal)?;
        self.on_friend_accepted(actor, other).await
    }

    async fn decline_friend(&self, actor: UserId, other: UserId) -> Result<(), ChatError> {
        let (lo, hi) = friend_pair(actor, other);
        let deleted = sqlx::query!(
            "DELETE FROM friendships WHERE user_lo = $1 AND user_hi = $2 AND status = $3",
            lo,
            hi,
            FRIEND_PENDING
        )
        .execute(&self.pool)
        .await
        .map_err(internal)?
        .rows_affected();
        if deleted == 0 {
            return Err(ChatError::NotFound);
        }
        self.publish_friend_removed(actor, other).await;
        Ok(())
    }

    async fn remove_friend(&self, actor: UserId, other: UserId) -> Result<(), ChatError> {
        let (lo, hi) = friend_pair(actor, other);
        let deleted = sqlx::query!(
            "DELETE FROM friendships WHERE user_lo = $1 AND user_hi = $2 AND status = $3",
            lo,
            hi,
            FRIEND_ACCEPTED
        )
        .execute(&self.pool)
        .await
        .map_err(internal)?
        .rows_affected();
        if deleted == 0 {
            return Err(ChatError::NotFound);
        }
        self.publish_friend_removed(actor, other).await;
        Ok(())
    }
}

impl ChatService {
    /// Shared accept path (from `accept_friend` and the reverse-pending case of
    /// `add_friend`): ensure a DM, then publish `FriendUpdate{accepted}` to both.
    async fn on_friend_accepted(
        &self,
        actor: UserId,
        other: UserId,
    ) -> Result<v1::Friend, ChatError> {
        // Opening the DM (idempotent) seeds both user dictionaries, fans
        // `DmChannelCreate` to both, and — via the gateway's handling of that —
        // registers mutual DM presence interest, so friends see each other's
        // presence and can message with one click. A failure here is non-fatal:
        // the friendship is already committed.
        if let Err(error) = self.open_dm(actor, other).await {
            tracing::warn!(%error, "open_dm on friend-accept failed");
        }
        let actor_user = self.load_user(actor).await?;
        let other_user = self.load_user(other).await?;
        self.publish_friend_update(actor, other_user.clone(), v1::FriendStatus::Accepted, false)
            .await;
        self.publish_friend_update(other, actor_user, v1::FriendStatus::Accepted, false)
            .await;
        Ok(v1::Friend {
            user: Some(other_user),
            status: v1::FriendStatus::Accepted as i32,
        })
    }

    /// Publish a `FriendUpdate` carrying `other_user`'s record to `recipient`'s
    /// own subject.
    async fn publish_friend_update(
        &self,
        recipient: UserId,
        other_user: v1::User,
        status: v1::FriendStatus,
        removed: bool,
    ) {
        let payload = FramePayload::FriendUpdate(v1::FriendUpdate {
            friend: Some(v1::Friend {
                user: Some(other_user),
                status: status as i32,
            }),
            removed,
        });
        let event = self.make_event(0, vec![recipient.raw()], false, payload);
        self.publish(Subject::User(recipient), event).await;
    }

    /// Tell both sides a friendship is gone (each gets the other's record so the
    /// client can drop it by id regardless of what's cached).
    async fn publish_friend_removed(&self, actor: UserId, other: UserId) {
        if let Ok(other_user) = self.load_user(other).await {
            self.publish_friend_update(actor, other_user, v1::FriendStatus::Unspecified, true)
                .await;
        }
        if let Ok(actor_user) = self.load_user(actor).await {
            self.publish_friend_update(other, actor_user, v1::FriendStatus::Unspecified, true)
                .await;
        }
    }

    /// Fan a `UserUpdate` to the user's own subject + every guild and DM they
    /// share, so peers see profile changes (avatar) without a reconnect.
    async fn broadcast_user_update(&self, actor: UserId, user: &v1::User) {
        let payload = FramePayload::UserUpdate(v1::UserUpdate {
            user: Some(user.clone()),
        });
        // Self subject (covers a user with no guilds/DMs).
        let event = self.make_event(0, vec![actor.raw()], false, payload.clone());
        self.publish(Subject::User(actor), event).await;

        let guild_ids = sqlx::query_scalar!(
            "SELECT guild_id FROM guild_members WHERE user_id = $1",
            actor.as_i64()
        )
        .fetch_all(&self.pool)
        .await
        .unwrap_or_default();
        for gid in guild_ids {
            let event = self.make_event(gid as u64, Vec::new(), false, payload.clone());
            self.publish(Subject::GuildMsg(GuildId::from_i64(gid)), event)
                .await;
        }

        let dm_ids = sqlx::query_scalar!(
            r#"SELECT c.id FROM channels c
               JOIN channel_recipients cr ON cr.channel_id = c.id
               WHERE cr.user_id = $1 AND c.channel_type = $2"#,
            actor.as_i64(),
            KIND_DM
        )
        .fetch_all(&self.pool)
        .await
        .unwrap_or_default();
        for cid in dm_ids {
            let recipients = sqlx::query_scalar!(
                "SELECT user_id FROM channel_recipients WHERE channel_id = $1",
                cid
            )
            .fetch_all(&self.pool)
            .await
            .unwrap_or_default()
            .into_iter()
            .map(|v| v as u64)
            .collect();
            let event = self.make_event(0, recipients, false, payload.clone());
            self.publish(Subject::DmMsg(ChannelId::from_i64(cid)), event)
                .await;
        }
    }

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

        // VOICE channels are guild-scoped like text — same membership/permission
        // gate (so chat-side access checks on a voice channel don't 500).
        if row.channel_type == KIND_GUILD_TEXT || row.channel_type == KIND_VOICE {
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
            r#"SELECT id, channel_type, name AS "name!", position, last_message_id
               FROM channels WHERE guild_id = $1 ORDER BY position, id"#,
            guild.as_i64()
        )
        .fetch_all(&self.pool)
        .await
        .map_err(internal)?
        .into_iter()
        .map(|r| {
            guild_channel(
                r.id,
                g.id,
                r.channel_type,
                r.name,
                r.position,
                r.last_message_id,
            )
        })
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
            "SELECT id, username, display_name, flags, avatar_media_id FROM users WHERE id = $1",
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
            avatar_id: r.avatar_media_id.map_or(0, |v| v as u64),
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
                KIND_GUILD_TEXT,
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

    /// Claim `media` for a new message inside the send transaction. Every id
    /// must reference a `media` row owned by `actor` that is not already
    /// attached (the junction PK is `media_id`, so use is one-shot). Returns the
    /// resolved [`v1::Attachment`]s in the caller's order for the broadcast.
    async fn attach_to_message(
        &self,
        conn: &mut sqlx::PgConnection,
        actor: UserId,
        message: MessageId,
        media: &[MediaId],
    ) -> Result<Vec<v1::Attachment>, ChatError> {
        if media.is_empty() {
            return Ok(Vec::new());
        }
        let ids: Vec<i64> = media.iter().map(|m| m.as_i64()).collect();
        let rows = sqlx::query!(
            r#"SELECT m.id AS "id!", m.filename AS "filename!", m.content_type AS "content_type!",
                      m.size_bytes AS "size_bytes!", m.width AS "width!", m.height AS "height!"
               FROM media m
               WHERE m.id = ANY($1) AND m.uploader_id = $2
                 AND NOT EXISTS (SELECT 1 FROM message_attachments ma WHERE ma.media_id = m.id)"#,
            &ids[..],
            actor.as_i64()
        )
        .fetch_all(&mut *conn)
        .await
        .map_err(internal)?;
        // Every requested id must resolve (distinct, owned, unused). A count
        // mismatch also catches a client that sent a duplicate id.
        let by_id: HashMap<i64, _> = rows.into_iter().map(|r| (r.id, r)).collect();
        if by_id.len() != media.len() {
            return Err(ChatError::InvalidArgument(
                "one or more attachments are unknown, not yours, or already in use".to_owned(),
            ));
        }
        let mut out = Vec::with_capacity(media.len());
        for (position, mid) in media.iter().enumerate() {
            let r = &by_id[&mid.as_i64()];
            sqlx::query!(
                "INSERT INTO message_attachments (media_id, message_id, position) \
                 VALUES ($1, $2, $3)",
                mid.as_i64(),
                message.as_i64(),
                position as i16
            )
            .execute(&mut *conn)
            .await
            .map_err(internal)?;
            out.push(v1::Attachment {
                id: mid.raw(),
                filename: r.filename.clone(),
                content_type: r.content_type.clone(),
                size_bytes: r.size_bytes as u64,
                width: r.width as u32,
                height: r.height as u32,
            });
        }
        Ok(out)
    }

    /// Populate each message's `attachments` (in display order) from the
    /// `message_attachments` junction joined to `media` — one query per page.
    async fn attach_attachments(&self, messages: &mut [v1::Message]) -> Result<(), ChatError> {
        if messages.is_empty() {
            return Ok(());
        }
        let ids: Vec<i64> = messages.iter().map(|m| m.id as i64).collect();
        let rows = sqlx::query!(
            r#"SELECT ma.message_id AS "message_id!", m.id AS "media_id!",
                      m.filename AS "filename!", m.content_type AS "content_type!",
                      m.size_bytes AS "size_bytes!", m.width AS "width!", m.height AS "height!"
               FROM message_attachments ma
               JOIN media m ON m.id = ma.media_id
               WHERE ma.message_id = ANY($1)
               ORDER BY ma.message_id, ma.position"#,
            &ids[..]
        )
        .fetch_all(&self.pool)
        .await
        .map_err(internal)?;
        let mut by_msg: HashMap<i64, Vec<v1::Attachment>> = HashMap::new();
        for r in rows {
            by_msg
                .entry(r.message_id)
                .or_default()
                .push(v1::Attachment {
                    id: r.media_id as u64,
                    filename: r.filename,
                    content_type: r.content_type,
                    size_bytes: r.size_bytes as u64,
                    width: r.width as u32,
                    height: r.height as u32,
                });
        }
        for m in messages.iter_mut() {
            if let Some(attachments) = by_msg.remove(&(m.id as i64)) {
                m.attachments = attachments;
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

/// Canonical ordered `(lo, hi)` user-id pair for the `friendships` PK.
fn friend_pair(a: UserId, b: UserId) -> (i64, i64) {
    if a.raw() <= b.raw() {
        (a.as_i64(), b.as_i64())
    } else {
        (b.as_i64(), a.as_i64())
    }
}

/// The friendship's status from `actor`'s point of view (pending requests split
/// into incoming vs outgoing by who the requester was).
fn friend_status(status: i16, requester_id: i64, actor: UserId) -> v1::FriendStatus {
    if status == FRIEND_ACCEPTED {
        v1::FriendStatus::Accepted
    } else if requester_id == actor.as_i64() {
        v1::FriendStatus::PendingOutgoing
    } else {
        v1::FriendStatus::PendingIncoming
    }
}

fn ms(t: OffsetDateTime) -> u64 {
    u64::try_from(t.unix_timestamp_nanos() / 1_000_000).unwrap_or(0)
}

/// Build a `v1::Channel` for a guild channel. `kind` is the stored
/// `channels.channel_type` (the dice.v1 ChannelKind proto value, verbatim) —
/// GUILD_TEXT or VOICE — so reading a channel back reflects its real kind.
fn guild_channel(
    id: i64,
    guild_id: i64,
    kind: i16,
    name: String,
    position: i32,
    last_message_id: Option<i64>,
) -> v1::Channel {
    v1::Channel {
        id: id as u64,
        guild_id: guild_id as u64,
        kind: i32::from(kind),
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
        // Filled by `attach_reactions` / `attach_attachments` after the page is built.
        reactions: Vec::new(),
        attachments: Vec::new(),
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
