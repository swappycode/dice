//! Voice service: voice-channel membership + per-member speaking/mute/deafen
//! state, cache-backed, with bus fan-out.
//!
//! Signaling ONLY — the actual Opus audio never passes through here; it rides
//! QUIC datagrams handled by the gateway SFU (see `dice-voice-core` for the
//! framing/jitter math and the gateway's datagram pump for forwarding).
//!
//! The [`Voice`] trait below is the binding contract consumed by api-gateway
//! and the monolith. State lives in the [`Cache`](dice_cache::Cache) (Redis in
//! `full`, in-memory in `dev-lite`):
//!
//! - `voice:ch:{channel_id}` — the channel's roster (a list of [`VoiceMember`]).
//! - `voice:u:{user_id}` — the single channel a user is currently in (Discord
//!   semantics: one voice channel at a time), for fast leave-on-disconnect.
//!
//! Every mutation publishes a VoiceJoin/VoiceLeave/VoiceState dispatch to the
//! guild's voice subject ([`Subject::GuildVoice`](dice_event_bus::Subject)),
//! delivered ephemerally (never replayed on resume — clients re-sync the roster
//! via GET on reconnect).
//!
//! [`VoiceMember`]: dice_protocol::v1::VoiceMember

mod service;

use dice_common::{ChannelId, UserId};
use dice_protocol::v1::VoiceRoster;

pub use service::{ORIGIN, VoiceService};

/// Errors surfaced by [`Voice`].
#[derive(Debug, thiserror::Error)]
pub enum VoiceError {
    /// The channel id does not exist.
    #[error("channel not found")]
    NotFound,
    /// The channel exists but is not a VOICE channel.
    #[error("not a voice channel")]
    NotAVoiceChannel,
    /// The caller is not a member of the channel's guild.
    #[error("not a member of this guild")]
    NotAMember,
    /// A state update referenced a channel the caller has not joined.
    #[error("not in this voice channel")]
    NotInChannel,
    /// Cache / database / bus failure.
    #[error("internal voice error")]
    Internal(#[source] Box<dyn std::error::Error + Send + Sync>),
}

/// Voice-channel membership + speaking state. Object-safe; held as
/// `Arc<dyn Voice>`.
#[async_trait::async_trait]
pub trait Voice: Send + Sync {
    /// Join a VOICE `channel` (the caller must be a member of its guild). A user
    /// is in at most one voice channel, so this leaves any current one first.
    /// Returns the full roster (incl. the joiner) and publishes `VoiceJoin`.
    async fn join(
        &self,
        user: UserId,
        channel: ChannelId,
        muted: bool,
        deafened: bool,
    ) -> Result<VoiceRoster, VoiceError>;

    /// Leave `channel`. Idempotent (a no-op if the caller is not in it).
    /// Publishes `VoiceLeave` when a member was actually removed.
    async fn leave(&self, user: UserId, channel: ChannelId) -> Result<(), VoiceError>;

    /// Update the caller's own mute / deafen / speaking flags in `channel`.
    /// Publishes `VoiceState`. Errors if the caller has not joined.
    async fn update_state(
        &self,
        user: UserId,
        channel: ChannelId,
        muted: bool,
        deafened: bool,
        speaking: bool,
    ) -> Result<(), VoiceError>;

    /// The current roster of `channel` (for `GET` + the SFU routing table).
    async fn roster(&self, channel: ChannelId) -> Result<VoiceRoster, VoiceError>;

    /// Drop `user` from whatever voice channel they are in (called on gateway
    /// session teardown). Idempotent; publishes `VoiceLeave` if they were in one.
    async fn disconnect(&self, user: UserId) -> Result<(), VoiceError>;
}
