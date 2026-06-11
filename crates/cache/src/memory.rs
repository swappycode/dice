//! In-process cache backend: moka for values (per-entry TTL via an
//! [`Expiry`] policy) plus a `DashMap` of fixed-window counters.
//!
//! Used by the `dev-lite` profile and the monolith default (`DICE_CACHE=memory`).

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use bytes::Bytes;
use dashmap::DashMap;
use dashmap::mapref::entry::Entry;
use moka::Expiry;

use crate::{Cache, CacheError};

/// Sweep the counter map for expired windows every this many `incr_expire`
/// calls (cheap lazy purge; individual keys are also reset on access).
const COUNTER_SWEEP_EVERY: u64 = 1024;

/// Value stored in moka; `ttl` is consulted by the [`PerEntryTtl`] policy.
#[derive(Clone)]
struct ValueEntry {
    data: Bytes,
    ttl: Option<Duration>,
}

/// moka expiry policy: each entry carries its own TTL (`None` = no expiry).
/// moka checks expiration on read, so an expired entry is never returned
/// even before eviction housekeeping runs.
struct PerEntryTtl;

impl Expiry<String, ValueEntry> for PerEntryTtl {
    fn expire_after_create(
        &self,
        _key: &String,
        value: &ValueEntry,
        _created_at: Instant,
    ) -> Option<Duration> {
        value.ttl
    }

    fn expire_after_update(
        &self,
        _key: &String,
        value: &ValueEntry,
        _updated_at: Instant,
        _duration_until_expiry: Option<Duration>,
    ) -> Option<Duration> {
        // Overwrites replace the TTL with the new entry's TTL.
        value.ttl
    }
}

/// Fixed-window counter state for one key.
struct Counter {
    count: i64,
    deadline: Instant,
}

/// In-memory [`Cache`] backend. All operations are infallible.
pub struct MemoryCache {
    values: moka::future::Cache<String, ValueEntry>,
    counters: DashMap<String, Counter>,
    counter_ops: AtomicU64,
}

impl MemoryCache {
    /// Create an empty in-memory cache.
    pub fn new() -> Self {
        Self {
            // Unbounded: M1 keyspace (presence per online user + rate-limit
            // windows) is small and naturally TTL-bounded.
            values: moka::future::Cache::builder()
                .expire_after(PerEntryTtl)
                .build(),
            counters: DashMap::new(),
            counter_ops: AtomicU64::new(0),
        }
    }

    /// Occasionally drop counters whose window has passed so abandoned keys
    /// do not accumulate. Individual keys are reset on access regardless.
    fn maybe_sweep_counters(&self, now: Instant) {
        if self
            .counter_ops
            .fetch_add(1, Ordering::Relaxed)
            .is_multiple_of(COUNTER_SWEEP_EVERY)
        {
            self.counters.retain(|_, c| now < c.deadline);
        }
    }
}

impl Default for MemoryCache {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl Cache for MemoryCache {
    async fn get(&self, key: &str) -> Result<Option<Bytes>, CacheError> {
        Ok(self.values.get(key).await.map(|entry| entry.data))
    }

    async fn set(&self, key: &str, value: Bytes, ttl: Option<Duration>) -> Result<(), CacheError> {
        self.values
            .insert(key.to_owned(), ValueEntry { data: value, ttl })
            .await;
        Ok(())
    }

    async fn delete(&self, key: &str) -> Result<(), CacheError> {
        // Mirror Redis single-keyspace DEL: clear both namespaces.
        self.values.invalidate(key).await;
        self.counters.remove(key);
        Ok(())
    }

    async fn incr_expire(&self, key: &str, ttl: Duration) -> Result<i64, CacheError> {
        let now = Instant::now();
        // The DashMap entry guard holds the shard write lock, making
        // check-reset-increment atomic per key.
        let count = match self.counters.entry(key.to_owned()) {
            Entry::Occupied(mut occupied) => {
                let counter = occupied.get_mut();
                if now >= counter.deadline {
                    // Window elapsed: this access starts a fresh window.
                    counter.count = 1;
                    counter.deadline = now + ttl;
                } else {
                    counter.count = counter.count.saturating_add(1);
                }
                counter.count
            }
            Entry::Vacant(vacant) => {
                vacant.insert(Counter {
                    count: 1,
                    deadline: now + ttl,
                });
                1
            }
        };
        self.maybe_sweep_counters(now);
        Ok(count)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn set_get_round_trip() {
        let cache = MemoryCache::new();
        cache
            .set("k", Bytes::from_static(b"hello"), None)
            .await
            .unwrap();
        let got = cache.get("k").await.unwrap();
        assert_eq!(got, Some(Bytes::from_static(b"hello")));
    }

    #[tokio::test]
    async fn get_missing_key_is_none() {
        let cache = MemoryCache::new();
        assert_eq!(cache.get("nope").await.unwrap(), None);
    }

    #[tokio::test]
    async fn ttl_expiry_hides_entry() {
        let cache = MemoryCache::new();
        cache
            .set(
                "p",
                Bytes::from_static(b"v"),
                Some(Duration::from_millis(50)),
            )
            .await
            .unwrap();
        assert!(cache.get("p").await.unwrap().is_some());

        tokio::time::sleep(Duration::from_millis(80)).await;
        assert_eq!(
            cache.get("p").await.unwrap(),
            None,
            "expired entry returned"
        );
    }

    #[tokio::test]
    async fn overwrite_replaces_value_and_ttl() {
        let cache = MemoryCache::new();
        cache
            .set(
                "k",
                Bytes::from_static(b"old"),
                Some(Duration::from_millis(50)),
            )
            .await
            .unwrap();
        // Overwrite with no TTL: must survive past the original deadline.
        cache
            .set("k", Bytes::from_static(b"new"), None)
            .await
            .unwrap();

        tokio::time::sleep(Duration::from_millis(80)).await;
        assert_eq!(
            cache.get("k").await.unwrap(),
            Some(Bytes::from_static(b"new"))
        );
    }

    #[tokio::test]
    async fn delete_removes_entry() {
        let cache = MemoryCache::new();
        cache
            .set("k", Bytes::from_static(b"v"), None)
            .await
            .unwrap();
        cache.delete("k").await.unwrap();
        assert_eq!(cache.get("k").await.unwrap(), None);
        // Idempotent.
        cache.delete("k").await.unwrap();
    }

    #[tokio::test]
    async fn incr_expire_counts_within_window_and_resets_after() {
        let cache = MemoryCache::new();
        let window = Duration::from_millis(50);

        assert_eq!(cache.incr_expire("c", window).await.unwrap(), 1);
        assert_eq!(cache.incr_expire("c", window).await.unwrap(), 2);
        assert_eq!(cache.incr_expire("c", window).await.unwrap(), 3);

        tokio::time::sleep(Duration::from_millis(80)).await;
        assert_eq!(
            cache.incr_expire("c", window).await.unwrap(),
            1,
            "window elapsed, counter must restart at 1"
        );
    }

    #[tokio::test]
    async fn incr_expire_does_not_extend_window_on_increment() {
        let cache = MemoryCache::new();
        let window = Duration::from_millis(60);

        assert_eq!(cache.incr_expire("c", window).await.unwrap(), 1);
        tokio::time::sleep(Duration::from_millis(40)).await;
        // Mid-window increment must NOT push the deadline out (expiry is set
        // only when the key is created).
        assert_eq!(cache.incr_expire("c", window).await.unwrap(), 2);
        tokio::time::sleep(Duration::from_millis(40)).await;
        // 80 ms since window start > 60 ms window: fresh window.
        assert_eq!(cache.incr_expire("c", window).await.unwrap(), 1);
    }

    #[tokio::test]
    async fn delete_clears_counters_too() {
        let cache = MemoryCache::new();
        let window = Duration::from_secs(60);
        assert_eq!(cache.incr_expire("c", window).await.unwrap(), 1);
        assert_eq!(cache.incr_expire("c", window).await.unwrap(), 2);
        cache.delete("c").await.unwrap();
        assert_eq!(cache.incr_expire("c", window).await.unwrap(), 1);
    }

    #[tokio::test]
    async fn concurrent_incr_expire_is_atomic() {
        use std::sync::Arc;
        let cache = Arc::new(MemoryCache::new());
        let window = Duration::from_secs(60);

        let mut tasks = Vec::new();
        for _ in 0..8 {
            let cache = Arc::clone(&cache);
            tasks.push(tokio::spawn(async move {
                for _ in 0..100 {
                    cache.incr_expire("hot", window).await.unwrap();
                }
            }));
        }
        for t in tasks {
            t.await.unwrap();
        }
        assert_eq!(cache.incr_expire("hot", window).await.unwrap(), 801);
    }
}
