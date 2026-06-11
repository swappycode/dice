//! Typed builders for the cache key conventions (workspace design §5.4).
//!
//! Always build keys through these helpers so the conventions live in exactly
//! one place. Values are protobuf bytes, never JSON. Refresh tokens are NEVER
//! cached.

use std::time::Duration;

use dice_common::id::UserId;

/// TTL for `presence:{user_id}` entries: 3 × the 30 s heartbeat interval, so
/// presence dots die naturally when heartbeats stop.
pub const PRESENCE_TTL: Duration = Duration::from_secs(90);

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_formats_are_stable() {
        assert_eq!(presence(UserId::from_raw(42)), "presence:42");
        assert_eq!(rate_limit("send", "12345"), "rl:send:12345");
    }
}
