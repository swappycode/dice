//! Presence service: heartbeat-driven online/idle/dnd/offline tracking.
//!
//! The [`Presence`] trait below is the BINDING contract consumed by
//! api-gateway and the monolith. Implementations must not change signatures.
//!
//! Fan-out (integration critique #12): presence updates publish to EVERY
//! `dice.evt.guild.{gid}.presence` the user belongs to AND every
//! `dice.evt.dm.{channel_id}.presence` they participate in, so DM-only
//! contacts see dots too. The gateway passes both lists at `connect`.

pub mod rpc;
mod service;

use dice_common::{ChannelId, GuildId, SessionId, UserId};
use dice_protocol::v1::{PresenceStatus, PresenceUpdate};

pub use service::{ORIGIN, PRESENCE_KEY_TTL, PresenceService};

#[derive(Debug, thiserror::Error)]
pub enum PresenceError {
    #[error("unknown session")]
    UnknownSession,
    #[error("INVISIBLE is reserved and rejected in M1")]
    InvisibleNotSupported,
    #[error("internal presence error")]
    Internal(#[source] Box<dyn std::error::Error + Send + Sync>),
}

/// Store keys (cache crate): `presence:{user_id}` per-session keys TTL 90 s
/// (3× the 30 s heartbeat). Aggregate status = highest-priority live session
/// (DND > ONLINE > IDLE). User is OFFLINE when no session keys survive; the
/// implementation then writes `users.last_seen_at` and broadcasts OFFLINE.
#[async_trait::async_trait]
pub trait Presence: Send + Sync {
    /// New gateway session. `status` is ONLINE unless resuming a custom one.
    /// Rejects INVISIBLE in M1.
    async fn connect(
        &self,
        user: UserId,
        session: SessionId,
        guild_ids: &[GuildId],
        dm_channel_ids: &[ChannelId],
        status: PresenceStatus,
    ) -> Result<(), PresenceError>;

    /// Refreshes the session TTL (called on every gateway heartbeat).
    async fn heartbeat(&self, user: UserId, session: SessionId) -> Result<(), PresenceError>;

    /// Explicit status change; broadcasts if the aggregate changed.
    async fn set_status(
        &self,
        user: UserId,
        session: SessionId,
        status: PresenceStatus,
    ) -> Result<(), PresenceError>;

    /// Gateway session ended (socket closed AND resume window expired, or
    /// clean logout). Broadcasts OFFLINE if it was the last live session.
    async fn disconnect(&self, user: UserId, session: SessionId) -> Result<(), PresenceError>;

    /// Interest registration for mid-session guild joins / DM opens (#14):
    /// future updates for this session also fan out to these subjects.
    async fn add_interest(
        &self,
        user: UserId,
        session: SessionId,
        guild_ids: &[GuildId],
        dm_channel_ids: &[ChannelId],
    ) -> Result<(), PresenceError>;

    /// Current statuses for the `Ready` snapshot (OFFLINE entries included).
    async fn snapshot(&self, users: &[UserId]) -> Result<Vec<PresenceUpdate>, PresenceError>;
}
