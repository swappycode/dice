//! Chat service: guilds, channels, DMs, messages, typing.
//!
//! The [`Chat`] trait below is the BINDING contract consumed by api-gateway
//! (REST + socket dispatch) and the monolith. Implementations must not change
//! signatures. All mutating calls publish their dispatch events to the bus
//! AFTER the DB transaction commits (M1 accepts the commit→publish gap;
//! clients heal via resume + REST backfill).

use dice_common::{ChannelId, GuildId, MediaId, MessageId, UserId};
use dice_permissions::MissingPermissions;
use dice_protocol::v1;

mod service;

pub use service::ChatService;

#[derive(Debug, thiserror::Error)]
pub enum ChatError {
    #[error("not found")]
    NotFound,
    #[error("not a member of this guild/channel")]
    NotAMember,
    #[error("permission denied: {0}")]
    PermissionDenied(#[from] MissingPermissions),
    /// Action forbidden for a reason not expressible as a permission bit
    /// (e.g. editing someone else's message — author-only, even for mods).
    #[error("{0}")]
    Forbidden(String),
    #[error("invalid argument: {0}")]
    InvalidArgument(String),
    #[error("invalid invite code")]
    InvalidInvite,
    #[error("internal chat error")]
    Internal(#[source] Box<dyn std::error::Error + Send + Sync>),
}

/// History pagination cursor (keyset; never OFFSET).
#[derive(Debug, Clone, Copy)]
pub enum HistoryCursor {
    /// Newest messages.
    Latest,
    /// Messages strictly older than this id (scrollback).
    Before(MessageId),
    /// Messages strictly newer than this id (gap backfill after resume failure).
    After(MessageId),
}

/// Everything the gateway needs to build `Ready` for one user.
#[derive(Debug, Default)]
pub struct UserSyncState {
    /// Full guilds incl. channels, members (≤100) and invite_code.
    pub guilds: Vec<v1::Guild>,
    pub dm_channels: Vec<v1::Channel>,
    /// Deduplicated user dictionary: all guild members + DM recipients + self.
    pub users: Vec<v1::User>,
}

#[async_trait::async_trait]
pub trait Chat: Send + Sync {
    /// Snapshot for `Ready`. Also used by the gateway to (re)build interest.
    async fn sync_user_state(&self, user: UserId) -> Result<UserSyncState, ChatError>;

    /// Persists and publishes `MessageCreate{message, nonce}` to the channel's
    /// subject. `reply_to` (when set) is stored as `reply_to_id`. `attachments`
    /// are pre-uploaded media ids (media-service), claimed one-shot in the send
    /// transaction (each must be owned by `actor` and unused). Content may be
    /// empty iff there is at least one attachment. Returns the message (with its
    /// resolved attachments) for the gateway's `SendMessageAck`.
    async fn send_message(
        &self,
        actor: UserId,
        channel: ChannelId,
        content: String,
        reply_to: Option<MessageId>,
        attachments: Vec<MediaId>,
        nonce: u64,
    ) -> Result<v1::Message, ChatError>;

    /// Keyset history, newest-first, limit clamped to 1..=100.
    async fn get_messages(
        &self,
        actor: UserId,
        channel: ChannelId,
        cursor: HistoryCursor,
        limit: u8,
    ) -> Result<Vec<v1::Message>, ChatError>;

    /// Edits a message's content. AUTHOR-ONLY (mods cannot edit others, even
    /// with MANAGE_MESSAGES — matches Discord). Sets `edited_at` and publishes
    /// `MessageUpdate` to the channel subject.
    async fn edit_message(
        &self,
        actor: UserId,
        channel: ChannelId,
        message: MessageId,
        content: String,
    ) -> Result<v1::Message, ChatError>;

    /// Deletes a message. The author may delete their own; in a guild channel a
    /// member with MANAGE_MESSAGES may delete anyone's (DMs are author-only).
    /// Publishes `MessageDelete` to the channel subject.
    async fn delete_message(
        &self,
        actor: UserId,
        channel: ChannelId,
        message: MessageId,
    ) -> Result<(), ChatError>;

    /// Add the actor's `emoji` reaction (idempotent). On a real change,
    /// broadcasts a `ReactionUpdate{added:true}` delta to the channel.
    async fn add_reaction(
        &self,
        actor: UserId,
        channel: ChannelId,
        message: MessageId,
        emoji: String,
    ) -> Result<(), ChatError>;

    /// Remove the actor's `emoji` reaction (idempotent). On a real change,
    /// broadcasts a `ReactionUpdate{added:false}` delta to the channel.
    async fn remove_reaction(
        &self,
        actor: UserId,
        channel: ChannelId,
        message: MessageId,
        emoji: String,
    ) -> Result<(), ChatError>;

    /// Creates the guild AND its `#general` channel in one transaction
    /// (integration critique #24e: there is no create-channel UI in M1).
    /// Publishes `GuildCreate` to the creator's user subject.
    async fn create_guild(&self, actor: UserId, name: String) -> Result<v1::Guild, ChatError>;

    /// Joins via invite code. Publishes `GuildMemberAdd` to the guild subject
    /// and `GuildCreate` (full guild) to the joiner's user subject (#14).
    async fn join_guild(&self, actor: UserId, code: &str) -> Result<v1::Guild, ChatError>;

    /// Requires MANAGE_CHANNELS. Publishes `ChannelCreate` to the guild subject.
    /// `kind` selects GUILD_TEXT or VOICE (UNSPECIFIED defaults to text); DM is
    /// rejected (use `open_dm`).
    async fn create_channel(
        &self,
        actor: UserId,
        guild: GuildId,
        name: String,
        kind: v1::ChannelKind,
    ) -> Result<v1::Channel, ChatError>;

    /// Idempotent via dm_key (`min:max` user ids). Publishes `DmChannelCreate`
    /// to BOTH recipients' user subjects when newly created.
    async fn open_dm(&self, actor: UserId, other: UserId) -> Result<v1::Channel, ChatError>;

    /// Membership check + ephemeral `TypingStart` publish. No DB row, ever.
    async fn typing(&self, actor: UserId, channel: ChannelId) -> Result<(), ChatError>;

    /// Advance the caller's read marker for `channel` to its current last
    /// message and persist it, then broadcast `ReadMarkerUpdate` to the
    /// caller's OWN subject so their other devices clear the badge in sync.
    /// Requires channel visibility (membership).
    async fn mark_read(&self, actor: UserId, channel: ChannelId) -> Result<(), ChatError>;

    /// Set (or clear, `media = None`) the caller's avatar. A non-None media id
    /// must reference an image the caller uploaded. Publishes `UserUpdate` to
    /// the caller's own subject + every guild and DM they share, so peers update
    /// live. Returns the updated user.
    async fn set_avatar(
        &self,
        actor: UserId,
        media: Option<MediaId>,
    ) -> Result<v1::User, ChatError>;

    // ---- Friends / social (M3) ----

    /// The caller's friends + pending requests (both directions), each carrying
    /// the other user's full record. Backs the Friends page.
    async fn list_friends(&self, actor: UserId) -> Result<v1::FriendList, ChatError>;

    /// Send a friend request to `username` (case-insensitive). If that user has
    /// already sent the caller a pending request, this accepts it instead.
    /// Publishes `FriendUpdate` to both users; returns the relationship from the
    /// caller's perspective.
    async fn add_friend(&self, actor: UserId, username: &str) -> Result<v1::Friend, ChatError>;

    /// Accept a pending INCOMING request from `other`. Ensures a DM (so they see
    /// each other's presence and can message) and publishes `FriendUpdate`
    /// (accepted) to both; returns the relationship from the caller's side.
    async fn accept_friend(&self, actor: UserId, other: UserId) -> Result<v1::Friend, ChatError>;

    /// Decline an incoming, or cancel an outgoing, PENDING request with `other`.
    /// Publishes `FriendUpdate{removed}` to both.
    async fn decline_friend(&self, actor: UserId, other: UserId) -> Result<(), ChatError>;

    /// Remove an ACCEPTED friend `other` (any DM is kept). Publishes
    /// `FriendUpdate{removed}` to both.
    async fn remove_friend(&self, actor: UserId, other: UserId) -> Result<(), ChatError>;
}
