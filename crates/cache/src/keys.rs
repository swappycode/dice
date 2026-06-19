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
/// advertised `host:port` (UTF-8). Written as a **lease** (short TTL, refreshed
/// while the session lives) so a reconnect on another node can both be routed
/// back to a *live* owner — via a sticky LB (phase 0) or an actionable redirect
/// (phase 0b) — and detect a *dead* owner (the lease expires) to re-host the
/// session from its durable snapshot (phase 2b). See [`crate::SessionDirectory`].
pub fn session_owner(session_id: u64) -> String {
    format!("resume:owner:{session_id}")
}

/// `resume:snapshot:{session_id}` → the detached session's durable resume state
/// (identity + next seq + the serialized replay ring), so a *different* gateway
/// node can re-host the session after the origin is gone (cross-node resume
/// phase 2b, ADR-0007). TTL is the resume window, supplied at write time.
pub fn resume_snapshot(session_id: u64) -> String {
    format!("resume:snapshot:{session_id}")
}

/// `resume:claim:{session_id}` → single-takeover fence for cross-node re-host
/// (phase 2b): a counter incremented via [`crate::Cache::incr_expire`] so EXACTLY
/// ONE node (the one that reads back `1`) re-hosts a given session snapshot, even
/// if several reconnects race. TTL is the resume window.
pub fn resume_claim(session_id: u64) -> String {
    format!("resume:claim:{session_id}")
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
