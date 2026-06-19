//! Typed builders for the cache key conventions (workspace design §5.4).
//!
//! Always build keys through these helpers so the conventions live in exactly
//! one place. Values are protobuf bytes, never JSON. Refresh tokens are NEVER
//! cached.

use std::time::Duration;

use dice_common::id::{ChannelId, UserId};

/// TTL for `presence:{user_id}` entries: 3 × the 30 s heartbeat interval, so
/// presence dots die naturally when heartbeats stop.
pub const PRESENCE_TTL: Duration = Duration::from_secs(90);

/// TTL for `unread:{user}:{channel}` counters — long, so a badge survives a
/// disconnect, but stale counters for abandoned channels eventually expire.
pub const UNREAD_TTL: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// `unread:{user_id}:{channel_id}` → little-endian u64 unread message count,
/// maintained by notification-service and cleared by the read-marker path.
pub fn unread(user_id: UserId, channel_id: ChannelId) -> String {
    format!("unread:{user_id}:{channel_id}")
}

/// `presence:{user_id}` → `dice.v1.PresenceUpdate` protobuf bytes,
/// written with [`PRESENCE_TTL`] and refreshed on every heartbeat.
pub fn presence(user_id: UserId) -> String {
    format!("presence:{user_id}")
}

/// `rl:{scope}:{principal}` → fixed-window rate-limit counter
/// (see [`crate::RateLimiter`]).
pub fn rate_limit(scope: &str, principal: &str) -> String {
    format!("rl:{scope}:{principal}")
}

/// `resume:owner:{session_id}` → little-endian `u16` node id of the gateway that
/// owns a detached session's replay buffer, optionally followed by that node's
/// advertised `host:port` (UTF-8), so a reconnect on another node can be routed
/// back to the owner — via a sticky LB (phase 0) or an actionable redirect
/// (phase 0b) — within the resume window (see [`crate::SessionDirectory`]). TTL
/// is the resume window, supplied at write time.
pub fn session_owner(session_id: u64) -> String {
    format!("resume:owner:{session_id}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_formats_are_stable() {
        assert_eq!(presence(UserId::from_raw(42)), "presence:42");
        assert_eq!(rate_limit("send", "12345"), "rl:send:12345");
        assert_eq!(
            unread(UserId::from_raw(7), ChannelId::from_raw(9)),
            "unread:7:9"
        );
    }
}
