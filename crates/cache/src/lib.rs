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
    async fn rate_limiter_zero_limit_always_denies() {
        let rl = memory_limiter().await;
        let d = rl
            .check("none", "u", 0, Duration::from_secs(60))
            .await
            .unwrap();
        assert!(!d.allowed);
        assert_eq!(d.remaining, 0);
    }
}
