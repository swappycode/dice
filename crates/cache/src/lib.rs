//! Cache abstraction for Dice services: one [`Cache`] trait, two backends
//! (Redis for `full` deployments, in-memory moka for `dev-lite` / the
//! monolith), selected at **runtime** via [`CacheConfig`] (ADR-0002: runtime
//! config, not cargo features).
//!
//! # Key conventions (workspace design §5.4)
//!
//! Values are **protobuf bytes, never JSON**.
//!
//! | Key                       | Value                          | TTL    |
//! |---------------------------|--------------------------------|--------|
//! | `presence:{user_id}`      | `dice.v1.PresenceUpdate` bytes | 90 s (3 × 30 s heartbeat; dots die naturally when heartbeats stop) |
//! | `rl:{scope}:{principal}`  | fixed-window counter           | the rate-limit window |
//!
//! Use the [`keys`] module to build keys instead of hand-formatting them.
//!
//! **Refresh tokens are NEVER cached** — revocation correctness lives in
//! Postgres only (see `dice-auth-core`).

pub mod keys;
mod memory;
mod redis_impl;

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use dice_common::id::{ChannelId, UserId};

pub use memory::MemoryCache;
pub use redis_impl::RedisCache;

/// Errors surfaced by cache backends.
///
/// The in-memory backend is infallible; only the Redis backend produces
/// errors in practice.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum CacheError {
    /// Underlying Redis transport / protocol error (includes connection
    /// failures surfaced through [`redis::aio::ConnectionManager`]).
    #[error("redis cache error: {0}")]
    Redis(#[from] redis::RedisError),
}

/// Default Redis URL (`DICE_REDIS_URL`) — matches `.env.example` and the dev
/// docker compose. Used by the split-mode service bins when the env var is unset.
pub const DEFAULT_REDIS_URL: &str = "redis://localhost:6379";

/// Runtime backend selection (`DICE_CACHE=memory|redis` at the service layer).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CacheConfig {
    /// In-process moka cache. Used by `dev-lite` and as the monolith default.
    Memory,
    /// Shared Redis, e.g. `redis://127.0.0.1:6379`.
    Redis {
        /// Redis connection URL.
        url: String,
    },
}

/// Connect the configured backend and return it as a shared trait object.
///
/// For [`CacheConfig::Redis`] this establishes the initial connection (so
/// misconfiguration fails fast at boot); reconnects afterwards are handled
/// transparently by [`redis::aio::ConnectionManager`].
pub async fn connect(cfg: CacheConfig) -> Result<Arc<dyn Cache>, CacheError> {
    match cfg {
        CacheConfig::Memory => Ok(Arc::new(MemoryCache::new())),
        CacheConfig::Redis { url } => Ok(Arc::new(RedisCache::connect(&url).await?)),
    }
}

/// Byte-oriented cache with per-entry TTLs and a fixed-window counter
/// primitive. Object-safe; services hold `Arc<dyn Cache>`.
#[async_trait::async_trait]
pub trait Cache: Send + Sync {
    /// Fetch the value at `key`. Expired entries are never returned.
    async fn get(&self, key: &str) -> Result<Option<Bytes>, CacheError>;

    /// Store `value` at `key`. `ttl = None` means no expiry. Overwriting an
    /// entry replaces its TTL.
    async fn set(&self, key: &str, value: Bytes, ttl: Option<Duration>) -> Result<(), CacheError>;

    /// Remove `key` (both value and counter namespaces). Idempotent.
    async fn delete(&self, key: &str) -> Result<(), CacheError>;

    /// Fixed-window counter: atomically increment the counter at `key`; the
    /// expiry is set to `ttl` ONLY when the key is created (the start of a
    /// window). Returns the post-increment count, starting at 1 for a fresh
    /// window.
    async fn incr_expire(&self, key: &str, ttl: Duration) -> Result<i64, CacheError>;
}

/// Outcome of a [`RateLimiter::check`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RateDecision {
    /// Whether the request is within the limit and may proceed.
    pub allowed: bool,
    /// How many further requests fit in the current window (0 when denied).
    pub remaining: u32,
    /// How long the caller should wait before retrying. [`Duration::ZERO`]
    /// when allowed; the full `window` when denied (see fixed-window note on
    /// [`RateLimiter::check`]).
    pub retry_after: Duration,
}

/// Fixed-window rate limiter over any [`Cache`] backend.
///
/// Works identically on Redis (cross-node) and the in-memory backend
/// (per-process) because it is built purely on [`Cache::incr_expire`].
#[derive(Clone)]
pub struct RateLimiter {
    cache: Arc<dyn Cache>,
}

impl RateLimiter {
    /// Wrap a cache handle.
    pub fn new(cache: Arc<dyn Cache>) -> Self {
        Self { cache }
    }

    /// Count one request for `principal` under `scope` and decide whether it
    /// is within `limit` requests per `window`.
    ///
    /// Keys follow the `rl:{scope}:{principal}` convention.
    ///
    /// # Fixed-window semantics (accepted for M1)
    ///
    /// The counter resets `window` after the *first* request of a window, so
    /// up to `2 × limit` requests can land around a window boundary. When
    /// denied, `retry_after` is reported as the full `window` — a
    /// conservative upper bound (the actual remainder of the window may be
    /// shorter). Good enough for M1; sliding windows are a backend-local
    /// upgrade later.
    pub async fn check(
        &self,
        scope: &str,
        principal: &str,
        limit: u32,
        window: Duration,
    ) -> Result<RateDecision, CacheError> {
        let key = keys::rate_limit(scope, principal);
        let count = self.cache.incr_expire(&key, window).await?;
        // Counters start at 1; clamp defensively for the u32 comparison.
        let count = u32::try_from(count).unwrap_or(u32::MAX);
        if count <= limit {
            Ok(RateDecision {
                allowed: true,
                remaining: limit - count,
                retry_after: Duration::ZERO,
            })
        } else {
            Ok(RateDecision {
                allowed: false,
                remaining: 0,
                retry_after: window,
            })
        }
    }
}

/// Per-(user, channel) unread message counter over any [`Cache`] backend.
///
/// Stored in the value namespace as a little-endian `u64` (so it can be read
/// back, unlike the increment-only counter primitive), with [`keys::UNREAD_TTL`].
/// notification-service is the only writer that [`bump`](Self::bump)s; the
/// read-marker path [`clear`](Self::clear)s. The bump's read-modify-write can
/// race a concurrent clear, which is benign (a message arriving exactly as the
/// channel is marked read may leave a count of 1) — acceptable eventual
/// consistency for M2.
#[derive(Clone)]
pub struct UnreadStore {
    cache: Arc<dyn Cache>,
}

impl UnreadStore {
    pub fn new(cache: Arc<dyn Cache>) -> Self {
        Self { cache }
    }

    /// Increment and return the new unread count for `(user, channel)`.
    pub async fn bump(&self, user: UserId, channel: ChannelId) -> Result<u64, CacheError> {
        let key = keys::unread(user, channel);
        let next = self.read(&key).await?.saturating_add(1);
        self.cache
            .set(
                &key,
                Bytes::copy_from_slice(&next.to_le_bytes()),
                Some(keys::UNREAD_TTL),
            )
            .await?;
        Ok(next)
    }

    /// Current unread count for `(user, channel)` (0 if none).
    pub async fn count(&self, user: UserId, channel: ChannelId) -> Result<u64, CacheError> {
        self.read(&keys::unread(user, channel)).await
    }

    /// Reset the unread count for `(user, channel)` to zero.
    pub async fn clear(&self, user: UserId, channel: ChannelId) -> Result<(), CacheError> {
        self.cache.delete(&keys::unread(user, channel)).await
    }

    async fn read(&self, key: &str) -> Result<u64, CacheError> {
        Ok(self
            .cache
            .get(key)
            .await?
            .and_then(|b| <[u8; 8]>::try_from(b.as_ref()).ok())
            .map_or(0, u64::from_le_bytes))
    }
}

/// The gateway node that owns a detached session's replay buffer: its `u16`
/// node id plus, when the node advertises one (`DICE_ADVERTISED_ADDR`), its
/// reachable `host:port`. A reconnect that lands on another node uses `addr` to
/// emit an actionable redirect (ADR-0007 phase 0b); `addr` is `None` for nodes
/// that don't advertise one (the sticky-LB phase-0 path).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionOwner {
    pub node_id: u16,
    pub addr: Option<String>,
}

/// Maps a detached gateway session to the node that owns its in-memory replay
/// buffer, so a reconnect that lands on another node can be routed back to the
/// owner within the resume window (cross-node resume phase 0, ADR-0007).
///
/// Backed by any [`Cache`]: with the shared Redis backend it is a genuine
/// cross-node directory; with the in-memory backend it is per-process (harmless
/// — single-node deployments never need it). The value is the owner's `u16`
/// node id optionally followed by its advertised `host:port`; an operator's LB
/// (phase 0) or a client-side redirect (phase 0b) maps it to a reachable node.
#[derive(Clone)]
pub struct SessionDirectory {
    cache: Arc<dyn Cache>,
}

impl SessionDirectory {
    pub fn new(cache: Arc<dyn Cache>) -> Self {
        Self { cache }
    }

    /// Record `node_id` (and its advertised `addr`, if any) as the owner of
    /// `session_id`, expiring after `ttl` (set to the resume window so the entry
    /// lives exactly as long as the session is resumable). The value is the node
    /// id little-endian followed by the UTF-8 address bytes (empty = no address).
    pub async fn record(
        &self,
        session_id: u64,
        node_id: u16,
        addr: Option<&str>,
        ttl: Duration,
    ) -> Result<(), CacheError> {
        let addr = addr.unwrap_or("");
        let mut value = Vec::with_capacity(2 + addr.len());
        value.extend_from_slice(&node_id.to_le_bytes());
        value.extend_from_slice(addr.as_bytes());
        self.cache
            .set(
                &keys::session_owner(session_id),
                Bytes::from(value),
                Some(ttl),
            )
            .await
    }

    /// The node currently owning `session_id`, if any. A malformed value (<2
    /// bytes) reads as no owner.
    pub async fn owner(&self, session_id: u64) -> Result<Option<SessionOwner>, CacheError> {
        Ok(self
            .cache
            .get(&keys::session_owner(session_id))
            .await?
            .and_then(|b| {
                let bytes = b.as_ref();
                let node_id = u16::from_le_bytes(<[u8; 2]>::try_from(bytes.get(..2)?).ok()?);
                let addr = match std::str::from_utf8(&bytes[2..]) {
                    Ok(s) if !s.is_empty() => Some(s.to_owned()),
                    _ => None,
                };
                Some(SessionOwner { node_id, addr })
            }))
    }

    /// Drop the ownership record — the session resumed on its node, was torn
    /// down, or the window expired. Idempotent.
    pub async fn clear(&self, session_id: u64) -> Result<(), CacheError> {
        self.cache.delete(&keys::session_owner(session_id)).await
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    async fn memory_limiter() -> RateLimiter {
        let cache = connect(CacheConfig::Memory).await.unwrap();
        RateLimiter::new(cache)
    }

    #[tokio::test]
    async fn rate_limiter_allows_under_limit() {
        let rl = memory_limiter().await;
        let window = Duration::from_secs(60);

        let d1 = rl.check("send", "user-1", 3, window).await.unwrap();
        assert!(d1.allowed);
        assert_eq!(d1.remaining, 2);
        assert_eq!(d1.retry_after, Duration::ZERO);

        let d2 = rl.check("send", "user-1", 3, window).await.unwrap();
        assert!(d2.allowed);
        assert_eq!(d2.remaining, 1);

        let d3 = rl.check("send", "user-1", 3, window).await.unwrap();
        assert!(d3.allowed);
        assert_eq!(d3.remaining, 0);
    }

    #[tokio::test]
    async fn rate_limiter_denies_over_limit() {
        let rl = memory_limiter().await;
        let window = Duration::from_secs(60);

        for _ in 0..2 {
            assert!(
                rl.check("login", "1.2.3.4", 2, window)
                    .await
                    .unwrap()
                    .allowed
            );
        }
        let denied = rl.check("login", "1.2.3.4", 2, window).await.unwrap();
        assert!(!denied.allowed);
        assert_eq!(denied.remaining, 0);
        assert_eq!(denied.retry_after, window);
    }

    #[tokio::test]
    async fn rate_limiter_allows_again_after_window() {
        let rl = memory_limiter().await;
        let window = Duration::from_millis(50);

        assert!(rl.check("typing", "u", 1, window).await.unwrap().allowed);
        assert!(!rl.check("typing", "u", 1, window).await.unwrap().allowed);

        tokio::time::sleep(Duration::from_millis(80)).await;

        let after = rl.check("typing", "u", 1, window).await.unwrap();
        assert!(after.allowed, "window elapsed, counter must have reset");
        assert_eq!(after.remaining, 0);
    }

    #[tokio::test]
    async fn rate_limiter_scopes_and_principals_are_isolated() {
        let rl = memory_limiter().await;
        let window = Duration::from_secs(60);

        assert!(rl.check("a", "u1", 1, window).await.unwrap().allowed);
        // Different principal, same scope: own budget.
        assert!(rl.check("a", "u2", 1, window).await.unwrap().allowed);
        // Different scope, same principal: own budget.
        assert!(rl.check("b", "u1", 1, window).await.unwrap().allowed);
        // Original bucket is exhausted.
        assert!(!rl.check("a", "u1", 1, window).await.unwrap().allowed);
    }

    #[tokio::test]
    async fn unread_store_bumps_counts_and_clears() {
        let cache = connect(CacheConfig::Memory).await.unwrap();
        let store = UnreadStore::new(cache);
        let (u, c) = (UserId::from_raw(1), ChannelId::from_raw(7));
        assert_eq!(store.count(u, c).await.unwrap(), 0, "fresh = 0");
        assert_eq!(store.bump(u, c).await.unwrap(), 1);
        assert_eq!(store.bump(u, c).await.unwrap(), 2);
        assert_eq!(store.count(u, c).await.unwrap(), 2);
        // Independent per (user, channel).
        assert_eq!(store.count(u, ChannelId::from_raw(8)).await.unwrap(), 0);
        store.clear(u, c).await.unwrap();
        assert_eq!(store.count(u, c).await.unwrap(), 0, "cleared");
    }

    #[tokio::test]
    async fn rate_limiter_zero_limit_always_denies() {
        let rl = memory_limiter().await;
        let d = rl
            .check("none", "u", 0, Duration::from_secs(60))
            .await
            .unwrap();
        assert!(!d.allowed);
        assert_eq!(d.remaining, 0);
    }

    #[tokio::test]
    async fn session_directory_records_and_clears_owner() {
        let cache = connect(CacheConfig::Memory).await.unwrap();
        let dir = SessionDirectory::new(cache);
        let session = 9_000_000_001u64;
        assert_eq!(dir.owner(session).await.unwrap(), None, "fresh = no owner");
        dir.record(session, 7, None, Duration::from_secs(60))
            .await
            .unwrap();
        assert_eq!(
            dir.owner(session).await.unwrap(),
            Some(SessionOwner {
                node_id: 7,
                addr: None
            })
        );
        dir.clear(session).await.unwrap();
        assert_eq!(dir.owner(session).await.unwrap(), None, "cleared");
    }

    #[tokio::test]
    async fn session_directory_round_trips_advertised_addr() {
        let cache = connect(CacheConfig::Memory).await.unwrap();
        let dir = SessionDirectory::new(cache);
        let session = 9_000_000_002u64;
        dir.record(session, 42, Some("10.0.0.5:8443"), Duration::from_secs(60))
            .await
            .unwrap();
        assert_eq!(
            dir.owner(session).await.unwrap(),
            Some(SessionOwner {
                node_id: 42,
                addr: Some("10.0.0.5:8443".to_owned())
            }),
            "node id + advertised address both survive the round trip"
        );
    }

    /// Needs live Redis (`just infra-up`). Two directories over SEPARATE Redis
    /// connections to the same server stand in for two gateway nodes: node A
    /// records ownership, node B reads it back — the cross-node lookup path.
    #[tokio::test]
    #[ignore = "needs live Redis: run `just infra-up` first"]
    async fn session_directory_is_cross_node_over_shared_redis() {
        let url = std::env::var("DICE_REDIS_URL").unwrap_or_else(|_| DEFAULT_REDIS_URL.to_owned());
        let node_a = SessionDirectory::new(
            connect(CacheConfig::Redis { url: url.clone() })
                .await
                .unwrap(),
        );
        let node_b = SessionDirectory::new(connect(CacheConfig::Redis { url }).await.unwrap());
        let session = 9_123_456_789u64;
        node_b.clear(session).await.unwrap(); // clean slate
        node_a
            .record(
                session,
                3,
                Some("node-a.internal:8443"),
                Duration::from_secs(60),
            )
            .await
            .unwrap();
        assert_eq!(
            node_b.owner(session).await.unwrap(),
            Some(SessionOwner {
                node_id: 3,
                addr: Some("node-a.internal:8443".to_owned())
            }),
            "node B sees the owner + advertised address node A recorded"
        );
        node_a.clear(session).await.unwrap();
        assert_eq!(node_b.owner(session).await.unwrap(), None);
    }
}
