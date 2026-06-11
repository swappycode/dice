//! Redis cache backend over [`redis::aio::ConnectionManager`]
//! (auto-reconnecting multiplexed connection; `DICE_CACHE=redis`).
//!
//! Requires Redis >= 7.0 for `PEXPIRE ... NX` (the compose file pins
//! `redis:7`).

use std::time::Duration;

use bytes::Bytes;
use redis::aio::ConnectionManager;

use crate::{Cache, CacheError};

/// Redis-backed [`Cache`]. Cheap to clone the inner manager per call; safe
/// for concurrent use from many tasks.
pub struct RedisCache {
    manager: ConnectionManager,
}

impl RedisCache {
    /// Open `url` (e.g. `redis://127.0.0.1:6379`) and establish the initial
    /// connection — misconfiguration fails fast at boot. Subsequent drops
    /// reconnect automatically inside [`ConnectionManager`].
    pub async fn connect(url: &str) -> Result<Self, CacheError> {
        let client = redis::Client::open(url)?;
        let manager = client.get_connection_manager().await?;
        Ok(Self { manager })
    }
}

/// Clamp a TTL to the `[1, i64::MAX]` millisecond range Redis accepts
/// (`PX 0` is rejected by the server).
fn ttl_millis(ttl: Duration) -> i64 {
    i64::try_from(ttl.as_millis()).unwrap_or(i64::MAX).max(1)
}

#[async_trait::async_trait]
impl Cache for RedisCache {
    async fn get(&self, key: &str) -> Result<Option<Bytes>, CacheError> {
        let mut conn = self.manager.clone();
        let value: Option<Vec<u8>> = redis::cmd("GET").arg(key).query_async(&mut conn).await?;
        Ok(value.map(Bytes::from))
    }

    async fn set(&self, key: &str, value: Bytes, ttl: Option<Duration>) -> Result<(), CacheError> {
        let mut conn = self.manager.clone();
        let mut cmd = redis::cmd("SET");
        cmd.arg(key).arg(&value[..]);
        if let Some(ttl) = ttl {
            cmd.arg("PX").arg(ttl_millis(ttl));
        }
        let () = cmd.query_async(&mut conn).await?;
        Ok(())
    }

    async fn delete(&self, key: &str) -> Result<(), CacheError> {
        let mut conn = self.manager.clone();
        let _removed: i64 = redis::cmd("DEL").arg(key).query_async(&mut conn).await?;
        Ok(())
    }

    async fn incr_expire(&self, key: &str, ttl: Duration) -> Result<i64, CacheError> {
        let mut conn = self.manager.clone();
        // INCR + PEXPIRE NX in one pipeline: NX sets the expiry only when the
        // key has none yet, i.e. exactly when INCR just created it — the
        // window deadline is never extended by later increments. One
        // round-trip; atomic enough for fixed-window limiting (a competing
        // INCR between the two commands lands in the same window either way).
        let (count, _expire_set): (i64, i64) = redis::pipe()
            .cmd("INCR")
            .arg(key)
            .cmd("PEXPIRE")
            .arg(key)
            .arg(ttl_millis(ttl))
            .arg("NX")
            .query_async(&mut conn)
            .await?;
        Ok(count)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn ttl_millis_clamps() {
        assert_eq!(ttl_millis(Duration::ZERO), 1);
        assert_eq!(ttl_millis(Duration::from_micros(10)), 1);
        assert_eq!(ttl_millis(Duration::from_secs(90)), 90_000);
        assert_eq!(ttl_millis(Duration::MAX), i64::MAX);
    }

    /// Round-trips every trait method against a live Redis.
    ///
    /// Needs `just infra-up` (Redis 7 on 127.0.0.1:6379); override with
    /// `REDIS_URL`. Run via:
    /// `cargo test -p dice-cache -- --ignored`
    #[tokio::test]
    #[ignore = "needs live Redis: run `just infra-up` first"]
    async fn redis_round_trip_live() {
        let url =
            std::env::var("REDIS_URL").unwrap_or_else(|_| "redis://127.0.0.1:6379".to_owned());
        let cache = RedisCache::connect(&url).await.unwrap();

        // Unique key prefix so reruns / parallel CI don't collide.
        let prefix = format!("dice-cache-test:{}", std::process::id());
        let value_key = format!("{prefix}:v");
        let counter_key = format!("{prefix}:c");

        // set / get round-trip (no TTL).
        cache
            .set(&value_key, Bytes::from_static(b"proto-bytes"), None)
            .await
            .unwrap();
        assert_eq!(
            cache.get(&value_key).await.unwrap(),
            Some(Bytes::from_static(b"proto-bytes"))
        );

        // set with TTL expires.
        cache
            .set(
                &value_key,
                Bytes::from_static(b"short-lived"),
                Some(Duration::from_millis(50)),
            )
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(cache.get(&value_key).await.unwrap(), None);

        // delete is idempotent.
        cache
            .set(&value_key, Bytes::from_static(b"x"), None)
            .await
            .unwrap();
        cache.delete(&value_key).await.unwrap();
        assert_eq!(cache.get(&value_key).await.unwrap(), None);
        cache.delete(&value_key).await.unwrap();

        // incr_expire: 1, 2, 3 within the window; fresh window after expiry.
        let window = Duration::from_millis(100);
        assert_eq!(cache.incr_expire(&counter_key, window).await.unwrap(), 1);
        assert_eq!(cache.incr_expire(&counter_key, window).await.unwrap(), 2);
        assert_eq!(cache.incr_expire(&counter_key, window).await.unwrap(), 3);
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert_eq!(cache.incr_expire(&counter_key, window).await.unwrap(), 1);

        cache.delete(&counter_key).await.unwrap();
    }
}
