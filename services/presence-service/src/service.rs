//! [`PresenceService`]: cache-backed session presence with bus fan-out.
//!
//! # Cache layout (backend design §6, amended by critique #12)
//!
//! All values are protobuf bytes (or a single status byte), never JSON:
//!
//! | Key                       | Value                                  | TTL  |
//! |---------------------------|----------------------------------------|------|
//! | `prs:s:{user}:{session}`  | 1 status byte (`PresenceStatus` value) | 90 s |
//! | `prs:u:{user}`            | [`SessionList`] protobuf               | 90 s |
//! | `prs:i:{user}:{session}`  | [`InterestList`] protobuf              | 90 s |
//!
//! Per-session key expiry is the source of truth for liveness: the session
//! list is pruned lazily whenever an aggregate is computed (dead entries are
//! dropped and the list is rewritten). All three keys are refreshed together
//! on every gateway heartbeat.
//!
//! # Aggregate status
//!
//! A user's visible status is the highest-precedence status among live
//! sessions — `DND > ONLINE > IDLE` — and `OFFLINE` when none survive.
//!
//! # Concurrency (M1 scope)
//!
//! The [`Cache`] trait offers no compare-and-swap, so the list/interest
//! read-modify-write windows here are not atomic across nodes. M1 runs
//! presence single-node (monolith); the windows are kept as small as
//! possible and multi-node atomicity (Lua/`WATCH` on Redis) is deferred.
//!
//! # INVISIBLE / settable OFFLINE
//!
//! The wire enum reserves `INVISIBLE = 5`, which the closed prost enum cannot
//! even represent — the gateway must reject the raw value at decode time. At
//! this layer, a request to *appear* `OFFLINE` while connected is the same
//! deferred masking feature, so `OFFLINE` is rejected with
//! [`PresenceError::InvisibleNotSupported`] too. `UNSPECIFIED` is treated as
//! `ONLINE` (sessions start online; open-enum safe default).

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use dice_cache::Cache;
use dice_common::time::now_ms;
use dice_common::{ChannelId, GuildId, SessionId, SnowflakeGenerator, UserId};
use dice_database::PgPool;
use dice_event_bus::{BusEvent, EventBus, Subject};
use dice_protocol::internal::v1::bus_event;
use dice_protocol::v1::{self, PresenceStatus, PresenceUpdate};
use prost::Message;

use crate::{Presence, PresenceError};

/// TTL of every presence cache key: 3 × the 30 s heartbeat interval, so dots
/// die naturally (lazily, on the next aggregate read) when heartbeats stop.
pub const PRESENCE_KEY_TTL: Duration = Duration::from_secs(90);

/// `origin` stamped on every published [`BusEvent`].
pub const ORIGIN: &str = "presence-service";

/// Value of `prs:u:{user}`: the user's known session ids. Entries whose
/// `prs:s:` key has expired are pruned on read.
#[derive(Clone, PartialEq, ::prost::Message)]
struct SessionList {
    #[prost(fixed64, repeated, tag = "1")]
    sessions: Vec<u64>,
}

/// Value of `prs:i:{user}:{session}`: the subjects this session's presence
/// fans out to (critique #12: guilds AND dm channels).
#[derive(Clone, PartialEq, ::prost::Message)]
struct InterestList {
    #[prost(fixed64, repeated, tag = "1")]
    guild_ids: Vec<u64>,
    #[prost(fixed64, repeated, tag = "2")]
    dm_channel_ids: Vec<u64>,
}

fn session_key(user: UserId, session: SessionId) -> String {
    format!("prs:s:{user}:{session}")
}

fn list_key(user: UserId) -> String {
    format!("prs:u:{user}")
}

fn interest_key(user: UserId, session: SessionId) -> String {
    format!("prs:i:{user}:{session}")
}

fn internal<E>(e: E) -> PresenceError
where
    E: std::error::Error + Send + Sync + 'static,
{
    PresenceError::Internal(Box::new(e))
}

/// Decode the 1-byte session status. Anything unexpected routes to the safe
/// default (`ONLINE`) — only `{ONLINE, IDLE, DND}` are written by clients;
/// `DISCONNECTED` is written by the gateway on detach.
fn status_from_byte(b: u8) -> PresenceStatus {
    match b {
        2 => PresenceStatus::Idle,
        3 => PresenceStatus::Dnd,
        6 => PresenceStatus::Disconnected,
        _ => PresenceStatus::Online,
    }
}

/// Aggregate precedence: DND > ONLINE > IDLE > DISCONNECTED (OFFLINE/UNSPECIFIED
/// never rank). A dropped-but-resumable session shows DISCONNECTED only when no
/// live session outranks it — a second device keeps the user online/idle/dnd.
fn rank(s: PresenceStatus) -> u8 {
    match s {
        PresenceStatus::Dnd => 4,
        PresenceStatus::Online => 3,
        PresenceStatus::Idle => 2,
        PresenceStatus::Disconnected => 1,
        PresenceStatus::Unspecified | PresenceStatus::Offline => 0,
    }
}

/// Highest-precedence status among live sessions; OFFLINE when none.
fn aggregate(live: &[(SessionId, PresenceStatus)]) -> PresenceStatus {
    live.iter()
        .map(|&(_, s)| s)
        .max_by_key(|&s| rank(s))
        .filter(|&s| rank(s) > 0)
        .unwrap_or(PresenceStatus::Offline)
}

/// Validate a client-settable status (see module docs).
fn settable(status: PresenceStatus) -> Result<PresenceStatus, PresenceError> {
    match status {
        PresenceStatus::Unspecified => Ok(PresenceStatus::Online),
        PresenceStatus::Online | PresenceStatus::Idle | PresenceStatus::Dnd => Ok(status),
        // OFFLINE is the reserved masking feature; DISCONNECTED is server-set
        // only (the gateway marks it on detach). Neither is client-settable.
        PresenceStatus::Offline | PresenceStatus::Disconnected => {
            Err(PresenceError::InvisibleNotSupported)
        }
    }
}

/// The one [`Presence`] implementation (M1 single-node).
pub struct PresenceService {
    cache: Arc<dyn Cache>,
    bus: Arc<dyn EventBus>,
    pool: PgPool,
    ids: Arc<SnowflakeGenerator>,
    /// Key TTL; [`PRESENCE_KEY_TTL`] in production, shortened in tests to
    /// exercise expiry-driven OFFLINE.
    ttl: Duration,
}

impl PresenceService {
    /// Production constructor: 90 s key TTLs.
    pub fn new(
        cache: Arc<dyn Cache>,
        bus: Arc<dyn EventBus>,
        pool: PgPool,
        ids: Arc<SnowflakeGenerator>,
    ) -> Self {
        Self {
            cache,
            bus,
            pool,
            ids,
            ttl: PRESENCE_KEY_TTL,
        }
    }

    /// Test-only constructor with a short TTL so expiry paths are testable
    /// without waiting 90 s.
    #[cfg(test)]
    fn with_ttl(
        cache: Arc<dyn Cache>,
        bus: Arc<dyn EventBus>,
        pool: PgPool,
        ids: Arc<SnowflakeGenerator>,
        ttl: Duration,
    ) -> Self {
        Self {
            cache,
            bus,
            pool,
            ids,
            ttl,
        }
    }

    /// Read the session list and prune entries whose status key has expired;
    /// rewrite the list if anything was pruned. Returns live `(session,
    /// status)` pairs. A corrupt list decodes as empty (logged) rather than
    /// erroring — the cache is rebuildable state.
    async fn load_live(
        &self,
        user: UserId,
    ) -> Result<Vec<(SessionId, PresenceStatus)>, PresenceError> {
        let Some(raw) = self.cache.get(&list_key(user)).await.map_err(internal)? else {
            return Ok(Vec::new());
        };
        let listed = match SessionList::decode(raw.as_ref()) {
            Ok(list) => list.sessions,
            Err(e) => {
                tracing::warn!(%user, error = %e, "corrupt prs:u list; treating as empty");
                Vec::new()
            }
        };
        let mut live = Vec::with_capacity(listed.len());
        let mut seen = BTreeSet::new();
        for sid in &listed {
            if !seen.insert(*sid) {
                continue;
            }
            let session = SessionId::from_raw(*sid);
            if let Some(b) = self
                .cache
                .get(&session_key(user, session))
                .await
                .map_err(internal)?
            {
                live.push((session, status_from_byte(b.first().copied().unwrap_or(0))));
            }
        }
        if live.len() != listed.len() {
            self.store_session_list(user, live.iter().map(|&(s, _)| s))
                .await?;
        }
        Ok(live)
    }

    /// Rewrite `prs:u:{user}` (TTL refresh); delete it when empty.
    async fn store_session_list(
        &self,
        user: UserId,
        sessions: impl IntoIterator<Item = SessionId>,
    ) -> Result<(), PresenceError> {
        let sessions: Vec<u64> = sessions.into_iter().map(SessionId::raw).collect();
        let key = list_key(user);
        if sessions.is_empty() {
            self.cache.delete(&key).await.map_err(internal)
        } else {
            let bytes = Bytes::from(SessionList { sessions }.encode_to_vec());
            self.cache
                .set(&key, bytes, Some(self.ttl))
                .await
                .map_err(internal)
        }
    }

    /// One session's interest list (empty if missing/corrupt).
    async fn read_interest(
        &self,
        user: UserId,
        session: SessionId,
    ) -> Result<InterestList, PresenceError> {
        let Some(raw) = self
            .cache
            .get(&interest_key(user, session))
            .await
            .map_err(internal)?
        else {
            return Ok(InterestList::default());
        };
        match InterestList::decode(raw.as_ref()) {
            Ok(il) => Ok(il),
            Err(e) => {
                tracing::warn!(%user, %session, error = %e, "corrupt prs:i list; treating as empty");
                Ok(InterestList::default())
            }
        }
    }

    async fn write_interest(
        &self,
        user: UserId,
        session: SessionId,
        interest: &InterestList,
    ) -> Result<(), PresenceError> {
        self.cache
            .set(
                &interest_key(user, session),
                Bytes::from(interest.encode_to_vec()),
                Some(self.ttl),
            )
            .await
            .map_err(internal)
    }

    /// Union of interest across the given sessions (deduped, ordered).
    async fn union_interest(
        &self,
        user: UserId,
        sessions: &[SessionId],
    ) -> Result<(BTreeSet<GuildId>, BTreeSet<ChannelId>), PresenceError> {
        let mut guilds = BTreeSet::new();
        let mut dms = BTreeSet::new();
        for &session in sessions {
            let il = self.read_interest(user, session).await?;
            guilds.extend(il.guild_ids.iter().copied().map(GuildId::from_raw));
            dms.extend(il.dm_channel_ids.iter().copied().map(ChannelId::from_raw));
        }
        Ok((guilds, dms))
    }

    fn bus_event(&self, frame: v1::Frame, guild_id: u64) -> BusEvent {
        BusEvent {
            event_id: self.ids.generate().0,
            emitted_at_ms: now_ms(),
            origin: ORIGIN.to_owned(),
            guild_id,
            recipient_user_ids: Vec::new(),
            // Presence dispatches are SEQUENCED class A (protocol §6): the
            // gateway must insert them into replay buffers, so NOT ephemeral.
            ephemeral: false,
            payload: Some(bus_event::Payload::Frame(frame)),
        }
    }

    /// Publish one `PresenceUpdate` dispatch per subject (one fresh event_id
    /// each; the guild routing hint is set only on guild subjects).
    async fn broadcast(
        &self,
        user: UserId,
        status: PresenceStatus,
        since_ms: u64,
        guilds: impl IntoIterator<Item = GuildId>,
        dms: impl IntoIterator<Item = ChannelId>,
    ) -> Result<(), PresenceError> {
        let frame = v1::Frame::dispatch(v1::frame::Payload::PresenceUpdate(PresenceUpdate {
            user_id: user.raw(),
            status: status as i32,
            since_ms,
        }));
        for guild in guilds {
            let event = self.bus_event(frame.clone(), guild.raw());
            self.bus
                .publish(Subject::GuildPresence(guild), event)
                .await
                .map_err(internal)?;
        }
        for channel in dms {
            let event = self.bus_event(frame.clone(), 0);
            self.bus
                .publish(Subject::DmPresence(channel), event)
                .await
                .map_err(internal)?;
        }
        Ok(())
    }
}

#[async_trait::async_trait]
impl Presence for PresenceService {
    async fn connect(
        &self,
        user: UserId,
        session: SessionId,
        guild_ids: &[GuildId],
        dm_channel_ids: &[ChannelId],
        status: PresenceStatus,
    ) -> Result<(), PresenceError> {
        let status = settable(status)?;
        let mut live = self.load_live(user).await?;
        let old_agg = aggregate(&live);

        // Session status byte + this session's interest, then the list.
        self.cache
            .set(
                &session_key(user, session),
                Bytes::copy_from_slice(&[status as u8]),
                Some(self.ttl),
            )
            .await
            .map_err(internal)?;
        let interest = InterestList {
            guild_ids: guild_ids
                .iter()
                .map(|g| g.raw())
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect(),
            dm_channel_ids: dm_channel_ids
                .iter()
                .map(|c| c.raw())
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect(),
        };
        self.write_interest(user, session, &interest).await?;

        match live.iter_mut().find(|(s, _)| *s == session) {
            Some(entry) => entry.1 = status,
            None => live.push((session, status)),
        }
        self.store_session_list(user, live.iter().map(|&(s, _)| s))
            .await?;

        let new_agg = aggregate(&live);
        if new_agg != old_agg {
            let sessions: Vec<SessionId> = live.iter().map(|&(s, _)| s).collect();
            let (guilds, dms) = self.union_interest(user, &sessions).await?;
            self.broadcast(user, new_agg, now_ms(), guilds, dms).await?;
        }
        Ok(())
    }

    async fn heartbeat(&self, user: UserId, session: SessionId) -> Result<(), PresenceError> {
        let skey = session_key(user, session);
        let Some(status) = self.cache.get(&skey).await.map_err(internal)? else {
            // Session expired (or never existed): the gateway full-reconnects
            // presence on the next Identify. Never broadcast from here.
            return Err(PresenceError::UnknownSession);
        };
        self.cache
            .set(&skey, status, Some(self.ttl))
            .await
            .map_err(internal)?;

        // Refresh the interest key alongside (written together at connect).
        let ikey = interest_key(user, session);
        if let Some(interest) = self.cache.get(&ikey).await.map_err(internal)? {
            self.cache
                .set(&ikey, interest, Some(self.ttl))
                .await
                .map_err(internal)?;
        }

        // Refresh the session list TTL; recreate it (with at least this
        // session) if it vanished while the session key survived.
        let mut sessions: Vec<SessionId> = match self
            .cache
            .get(&list_key(user))
            .await
            .map_err(internal)?
            .and_then(|raw| SessionList::decode(raw.as_ref()).ok())
        {
            Some(list) => list.sessions.into_iter().map(SessionId::from_raw).collect(),
            None => Vec::new(),
        };
        if !sessions.contains(&session) {
            sessions.push(session);
        }
        self.store_session_list(user, sessions).await
    }

    async fn set_status(
        &self,
        user: UserId,
        session: SessionId,
        status: PresenceStatus,
    ) -> Result<(), PresenceError> {
        let status = settable(status)?;
        let mut live = self.load_live(user).await?;
        if !live.iter().any(|(s, _)| *s == session) {
            return Err(PresenceError::UnknownSession);
        }
        let old_agg = aggregate(&live);

        self.cache
            .set(
                &session_key(user, session),
                Bytes::copy_from_slice(&[status as u8]),
                Some(self.ttl),
            )
            .await
            .map_err(internal)?;
        for entry in live.iter_mut().filter(|(s, _)| *s == session) {
            entry.1 = status;
        }

        let new_agg = aggregate(&live);
        if new_agg != old_agg {
            let sessions: Vec<SessionId> = live.iter().map(|&(s, _)| s).collect();
            let (guilds, dms) = self.union_interest(user, &sessions).await?;
            self.broadcast(user, new_agg, now_ms(), guilds, dms).await?;
        }
        Ok(())
    }

    async fn detach(&self, user: UserId, session: SessionId) -> Result<(), PresenceError> {
        let mut live = self.load_live(user).await?;
        // Idempotent: if the session already expired off the live list there is
        // nothing to mark (it is heading OFFLINE via TTL / disconnect anyway).
        if !live.iter().any(|(s, _)| *s == session) {
            return Ok(());
        }
        let old_agg = aggregate(&live);

        // Mark this session DISCONNECTED (server-internal; bypasses `settable`)
        // and refresh the TTL so the entry survives the resume window.
        self.cache
            .set(
                &session_key(user, session),
                Bytes::copy_from_slice(&[PresenceStatus::Disconnected as u8]),
                Some(self.ttl),
            )
            .await
            .map_err(internal)?;
        for entry in live.iter_mut().filter(|(s, _)| *s == session) {
            entry.1 = PresenceStatus::Disconnected;
        }

        let new_agg = aggregate(&live);
        if new_agg != old_agg {
            let sessions: Vec<SessionId> = live.iter().map(|&(s, _)| s).collect();
            let (guilds, dms) = self.union_interest(user, &sessions).await?;
            self.broadcast(user, new_agg, now_ms(), guilds, dms).await?;
        }
        Ok(())
    }

    async fn disconnect(&self, user: UserId, session: SessionId) -> Result<(), PresenceError> {
        let live = self.load_live(user).await?;
        let old_agg = aggregate(&live);

        // Union interest BEFORE deleting any keys; include the closing
        // session even if its status key already expired off the live list.
        let mut sessions: Vec<SessionId> = live.iter().map(|&(s, _)| s).collect();
        if !sessions.contains(&session) {
            sessions.push(session);
        }
        let (guilds, dms) = self.union_interest(user, &sessions).await?;

        self.cache
            .delete(&session_key(user, session))
            .await
            .map_err(internal)?;
        self.cache
            .delete(&interest_key(user, session))
            .await
            .map_err(internal)?;
        let remaining: Vec<(SessionId, PresenceStatus)> =
            live.into_iter().filter(|&(s, _)| s != session).collect();
        self.store_session_list(user, remaining.iter().map(|&(s, _)| s))
            .await?;

        let new_agg = aggregate(&remaining);
        if new_agg == old_agg {
            // Not the last session and no precedence change (or the session
            // had already expired): nothing to announce. Idempotent.
            return Ok(());
        }

        // Last session gone: record last-seen (the documented cross-service
        // column exception). The broadcast still goes out if the write fails
        // — dot correctness beats last_seen freshness — but the error is
        // propagated so the gateway logs it.
        let mut db_err = None;
        if new_agg == PresenceStatus::Offline
            && let Err(e) = sqlx::query!(
                "UPDATE users SET last_seen_at = now() WHERE id = $1",
                user.as_i64()
            )
            .execute(&self.pool)
            .await
        {
            tracing::warn!(%user, error = %e, "last_seen_at write failed");
            db_err = Some(e);
        }
        self.broadcast(user, new_agg, now_ms(), guilds, dms).await?;
        match db_err {
            Some(e) => Err(internal(e)),
            None => Ok(()),
        }
    }

    async fn add_interest(
        &self,
        user: UserId,
        session: SessionId,
        guild_ids: &[GuildId],
        dm_channel_ids: &[ChannelId],
    ) -> Result<(), PresenceError> {
        let live = self.load_live(user).await?;
        if !live.iter().any(|(s, _)| *s == session) {
            return Err(PresenceError::UnknownSession);
        }
        let sessions: Vec<SessionId> = live.iter().map(|&(s, _)| s).collect();
        let (existing_guilds, existing_dms) = self.union_interest(user, &sessions).await?;

        // Newly-added relative to the union across ALL the user's sessions:
        // subjects another session already covers have the dot already.
        let new_guilds: BTreeSet<GuildId> = guild_ids
            .iter()
            .copied()
            .filter(|g| !existing_guilds.contains(g))
            .collect();
        let new_dms: BTreeSet<ChannelId> = dm_channel_ids
            .iter()
            .copied()
            .filter(|c| !existing_dms.contains(c))
            .collect();

        // Merge everything requested into THIS session's interest key.
        let current = self.read_interest(user, session).await?;
        let mut merged_guilds: BTreeSet<u64> = current.guild_ids.iter().copied().collect();
        merged_guilds.extend(guild_ids.iter().map(|g| g.raw()));
        let mut merged_dms: BTreeSet<u64> = current.dm_channel_ids.iter().copied().collect();
        merged_dms.extend(dm_channel_ids.iter().map(|c| c.raw()));
        let merged = InterestList {
            guild_ids: merged_guilds.into_iter().collect(),
            dm_channel_ids: merged_dms.into_iter().collect(),
        };
        self.write_interest(user, session, &merged).await?;

        // Existing members of ONLY the new subjects see the current dot
        // immediately (#14: mid-session guild join / DM open).
        let agg = aggregate(&live);
        if agg != PresenceStatus::Offline && (!new_guilds.is_empty() || !new_dms.is_empty()) {
            self.broadcast(user, agg, now_ms(), new_guilds, new_dms)
                .await?;
        }
        Ok(())
    }

    async fn snapshot(&self, users: &[UserId]) -> Result<Vec<PresenceUpdate>, PresenceError> {
        let mut out = Vec::with_capacity(users.len());
        let mut seen = BTreeSet::new();
        let mut offline_ids = Vec::new();
        for &user in users {
            if !seen.insert(user) {
                continue;
            }
            let agg = aggregate(&self.load_live(user).await?);
            if agg == PresenceStatus::Offline {
                offline_ids.push(user.as_i64());
            }
            out.push(PresenceUpdate {
                user_id: user.raw(),
                status: agg as i32,
                // OFFLINE entries get last_seen below; live entries report 0
                // (the cache does not record when the status last changed).
                since_ms: 0,
            });
        }

        if !offline_ids.is_empty() {
            let rows = sqlx::query!(
                "SELECT id, last_seen_at FROM users WHERE id = ANY($1)",
                &offline_ids[..]
            )
            .fetch_all(&self.pool)
            .await
            .map_err(internal)?;
            for row in rows {
                let Some(last_seen) = row.last_seen_at else {
                    continue;
                };
                let since_ms =
                    u64::try_from(last_seen.unix_timestamp_nanos() / 1_000_000).unwrap_or_default();
                if let Some(entry) = out.iter_mut().find(|p| {
                    p.user_id == UserId::from_i64(row.id).raw()
                        && p.status == PresenceStatus::Offline as i32
                }) {
                    entry.since_ms = since_ms;
                }
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::sync::OnceLock;
    use std::time::Duration;

    use dice_cache::CacheConfig;
    use dice_event_bus::{BusConfig, BusSubscription};
    use sqlx::postgres::PgPoolOptions;
    use tokio::time::timeout;

    use super::*;

    /// Matches d:\Dice\.env; tests fall back to it because `cargo test` does
    /// not load .env files at runtime.
    const DEV_DB_URL: &str = "postgres://dice:dice_dev@localhost:5433/dice";

    fn database_url() -> String {
        std::env::var("DATABASE_URL").unwrap_or_else(|_| DEV_DB_URL.to_owned())
    }

    /// One process-wide generator: ids stay unique within the test process,
    /// and the pid-derived node id de-conflicts concurrent test runs.
    fn idgen() -> Arc<SnowflakeGenerator> {
        static GENERATOR: OnceLock<Arc<SnowflakeGenerator>> = OnceLock::new();
        GENERATOR
            .get_or_init(|| {
                Arc::new(SnowflakeGenerator::new((std::process::id() % 1024) as u16).unwrap())
            })
            .clone()
    }

    struct Harness {
        svc: PresenceService,
        bus: Arc<dyn EventBus>,
        pool: PgPool,
    }

    async fn harness(ttl: Duration) -> Harness {
        let cache = dice_cache::connect(CacheConfig::Memory).await.unwrap();
        let bus = dice_event_bus::connect(BusConfig::Local { capacity: 1024 })
            .await
            .unwrap();
        // Lazy: tests that never touch Postgres never connect to it.
        let pool = PgPoolOptions::new().connect_lazy(&database_url()).unwrap();
        let svc = PresenceService::with_ttl(cache, Arc::clone(&bus), pool.clone(), idgen(), ttl);
        Harness { svc, bus, pool }
    }

    fn uid() -> UserId {
        UserId::from(idgen().generate())
    }

    fn sid() -> SessionId {
        SessionId::from(idgen().generate())
    }

    async fn recv(sub: &mut BusSubscription) -> BusEvent {
        timeout(Duration::from_secs(2), sub.recv())
            .await
            .expect("bus recv timed out")
            .expect("bus closed")
    }

    /// LocalBus publishes synchronously inside the awaited service call, so
    /// after the call returns any event is already buffered; a short timeout
    /// proves absence.
    async fn expect_silent(sub: &mut BusSubscription) {
        assert!(
            timeout(Duration::from_millis(100), sub.recv())
                .await
                .is_err(),
            "expected no event on this subject"
        );
    }

    fn presence_of(ev: &BusEvent) -> PresenceUpdate {
        let Some(bus_event::Payload::Frame(frame)) = &ev.payload else {
            panic!("bus event payload must be a Frame");
        };
        assert_eq!(frame.seq, 0, "service-published frames carry seq=0");
        assert_eq!(frame.nonce, 0);
        let Some(v1::frame::Payload::PresenceUpdate(p)) = &frame.payload else {
            panic!("frame payload must be PresenceUpdate");
        };
        *p
    }

    async fn insert_user(pool: &PgPool, id: UserId, with_last_seen: bool) {
        let name = format!("prs{}", id.raw());
        let email = format!("{name}@test.dice");
        if with_last_seen {
            sqlx::query!(
                "INSERT INTO users (id, username, email, password_hash, last_seen_at)
                 VALUES ($1, $2, $3, 'x', now() - interval '1 hour')",
                id.as_i64(),
                name,
                email,
            )
            .execute(pool)
            .await
            .unwrap();
        } else {
            sqlx::query!(
                "INSERT INTO users (id, username, email, password_hash) VALUES ($1, $2, $3, 'x')",
                id.as_i64(),
                name,
                email,
            )
            .execute(pool)
            .await
            .unwrap();
        }
    }

    async fn delete_user(pool: &PgPool, id: UserId) {
        sqlx::query!("DELETE FROM users WHERE id = $1", id.as_i64())
            .execute(pool)
            .await
            .unwrap();
    }

    // ---- pure logic ----

    #[test]
    fn aggregate_precedence_dnd_online_idle_offline() {
        let s = sid;
        assert_eq!(aggregate(&[]), PresenceStatus::Offline);
        // A lone dropped session shows DISCONNECTED; any live session outranks it.
        assert_eq!(
            aggregate(&[(s(), PresenceStatus::Disconnected)]),
            PresenceStatus::Disconnected
        );
        assert_eq!(
            aggregate(&[
                (s(), PresenceStatus::Disconnected),
                (s(), PresenceStatus::Idle),
            ]),
            PresenceStatus::Idle
        );
        assert_eq!(
            aggregate(&[(s(), PresenceStatus::Idle)]),
            PresenceStatus::Idle
        );
        assert_eq!(
            aggregate(&[(s(), PresenceStatus::Idle), (s(), PresenceStatus::Online)]),
            PresenceStatus::Online
        );
        assert_eq!(
            aggregate(&[
                (s(), PresenceStatus::Online),
                (s(), PresenceStatus::Dnd),
                (s(), PresenceStatus::Idle),
            ]),
            PresenceStatus::Dnd
        );
    }

    #[test]
    fn settable_normalizes_and_rejects() {
        assert_eq!(
            settable(PresenceStatus::Unspecified).unwrap(),
            PresenceStatus::Online
        );
        assert_eq!(settable(PresenceStatus::Dnd).unwrap(), PresenceStatus::Dnd);
        assert!(matches!(
            settable(PresenceStatus::Offline),
            Err(PresenceError::InvisibleNotSupported)
        ));
        // DISCONNECTED is server-set only; a client may not set it.
        assert!(matches!(
            settable(PresenceStatus::Disconnected),
            Err(PresenceError::InvisibleNotSupported)
        ));
    }

    #[test]
    fn status_byte_round_trip() {
        for s in [
            PresenceStatus::Online,
            PresenceStatus::Idle,
            PresenceStatus::Dnd,
        ] {
            assert_eq!(status_from_byte(s as u8), s);
        }
        // DISCONNECTED (gateway-written) round-trips too.
        assert_eq!(
            status_from_byte(PresenceStatus::Disconnected as u8),
            PresenceStatus::Disconnected
        );
        // Unknown bytes route to the safe default.
        assert_eq!(status_from_byte(0), PresenceStatus::Online);
        assert_eq!(status_from_byte(99), PresenceStatus::Online);
    }

    #[test]
    fn key_formats_are_stable() {
        let u = UserId::from_raw(1);
        let s = SessionId::from_raw(2);
        assert_eq!(session_key(u, s), "prs:s:1:2");
        assert_eq!(list_key(u), "prs:u:1");
        assert_eq!(interest_key(u, s), "prs:i:1:2");
    }

    // ---- connect / fan-out ----

    #[tokio::test]
    async fn connect_broadcasts_online_to_guild_and_dm_subjects() {
        let h = harness(PRESENCE_KEY_TTL).await;
        let (user, session) = (uid(), sid());
        let (g1, g2) = (
            GuildId::from(idgen().generate()),
            GuildId::from(idgen().generate()),
        );
        let dm = ChannelId::from(idgen().generate());

        let mut sub_g1 = h.bus.subscribe(Subject::GuildPresence(g1)).await.unwrap();
        let mut sub_g2 = h.bus.subscribe(Subject::GuildPresence(g2)).await.unwrap();
        let mut sub_dm = h.bus.subscribe(Subject::DmPresence(dm)).await.unwrap();

        h.svc
            .connect(user, session, &[g1, g2], &[dm], PresenceStatus::Online)
            .await
            .unwrap();

        let before = now_ms();
        let ev_g1 = recv(&mut sub_g1).await;
        let ev_g2 = recv(&mut sub_g2).await;
        let ev_dm = recv(&mut sub_dm).await;

        for ev in [&ev_g1, &ev_g2, &ev_dm] {
            assert!(
                !ev.ephemeral,
                "presence is sequenced class A, not ephemeral"
            );
            assert_eq!(ev.origin, ORIGIN);
            assert_ne!(ev.event_id, 0);
            assert!(ev.recipient_user_ids.is_empty());
            let p = presence_of(ev);
            assert_eq!(p.user_id, user.raw());
            assert_eq!(p.status, PresenceStatus::Online as i32);
            assert!(p.since_ms > 0 && p.since_ms <= before);
        }
        // Routing hint: set on guild subjects, zero on DM subjects.
        assert_eq!(ev_g1.guild_id, g1.raw());
        assert_eq!(ev_g2.guild_id, g2.raw());
        assert_eq!(ev_dm.guild_id, 0);
        // One BusEvent per subject, each with its own id.
        assert_ne!(ev_g1.event_id, ev_g2.event_id);
    }

    #[tokio::test]
    async fn unspecified_status_connects_as_online() {
        let h = harness(PRESENCE_KEY_TTL).await;
        let (user, g) = (uid(), GuildId::from(idgen().generate()));
        let mut sub = h.bus.subscribe(Subject::GuildPresence(g)).await.unwrap();

        h.svc
            .connect(user, sid(), &[g], &[], PresenceStatus::Unspecified)
            .await
            .unwrap();
        assert_eq!(
            presence_of(&recv(&mut sub).await).status,
            PresenceStatus::Online as i32
        );
    }

    #[tokio::test]
    async fn second_session_does_not_rebroadcast_unchanged_aggregate() {
        let h = harness(PRESENCE_KEY_TTL).await;
        let (user, g) = (uid(), GuildId::from(idgen().generate()));
        let mut sub = h.bus.subscribe(Subject::GuildPresence(g)).await.unwrap();

        h.svc
            .connect(user, sid(), &[g], &[], PresenceStatus::Online)
            .await
            .unwrap();
        recv(&mut sub).await; // first session: ONLINE

        // Same status: aggregate unchanged.
        h.svc
            .connect(user, sid(), &[g], &[], PresenceStatus::Online)
            .await
            .unwrap();
        // Lower precedence: aggregate still ONLINE.
        h.svc
            .connect(user, sid(), &[g], &[], PresenceStatus::Idle)
            .await
            .unwrap();
        expect_silent(&mut sub).await;
    }

    #[tokio::test]
    async fn invisible_masking_rejected_on_connect_and_set_status() {
        let h = harness(PRESENCE_KEY_TTL).await;
        let (user, session, g) = (uid(), sid(), GuildId::from(idgen().generate()));
        let mut sub = h.bus.subscribe(Subject::GuildPresence(g)).await.unwrap();

        // OFFLINE-as-settable is the reserved INVISIBLE masking feature
        // (the closed enum cannot carry wire value 5; see module docs).
        assert!(matches!(
            h.svc
                .connect(user, session, &[g], &[], PresenceStatus::Offline)
                .await,
            Err(PresenceError::InvisibleNotSupported)
        ));
        expect_silent(&mut sub).await;

        h.svc
            .connect(user, session, &[g], &[], PresenceStatus::Online)
            .await
            .unwrap();
        recv(&mut sub).await;
        assert!(matches!(
            h.svc
                .set_status(user, session, PresenceStatus::Offline)
                .await,
            Err(PresenceError::InvisibleNotSupported)
        ));
        expect_silent(&mut sub).await;
    }

    // ---- set_status / precedence ----

    #[tokio::test]
    async fn dnd_session_wins_precedence_via_set_status() {
        let h = harness(PRESENCE_KEY_TTL).await;
        let (user, s1, s2) = (uid(), sid(), sid());
        let g = GuildId::from(idgen().generate());
        let mut sub = h.bus.subscribe(Subject::GuildPresence(g)).await.unwrap();

        h.svc
            .connect(user, s1, &[g], &[], PresenceStatus::Online)
            .await
            .unwrap();
        assert_eq!(
            presence_of(&recv(&mut sub).await).status,
            PresenceStatus::Online as i32
        );
        h.svc
            .connect(user, s2, &[g], &[], PresenceStatus::Idle)
            .await
            .unwrap();

        // s2 -> DND outranks s1's ONLINE: aggregate changes, broadcast.
        h.svc
            .set_status(user, s2, PresenceStatus::Dnd)
            .await
            .unwrap();
        assert_eq!(
            presence_of(&recv(&mut sub).await).status,
            PresenceStatus::Dnd as i32
        );

        // s2 back to IDLE: aggregate falls back to s1's ONLINE.
        h.svc
            .set_status(user, s2, PresenceStatus::Idle)
            .await
            .unwrap();
        assert_eq!(
            presence_of(&recv(&mut sub).await).status,
            PresenceStatus::Online as i32
        );

        // No-op change on the non-dominant session: silent.
        h.svc
            .set_status(user, s2, PresenceStatus::Idle)
            .await
            .unwrap();
        expect_silent(&mut sub).await;
    }

    #[tokio::test]
    async fn set_status_on_dead_session_is_unknown() {
        let h = harness(PRESENCE_KEY_TTL).await;
        assert!(matches!(
            h.svc.set_status(uid(), sid(), PresenceStatus::Dnd).await,
            Err(PresenceError::UnknownSession)
        ));
    }

    // ---- heartbeat ----

    #[tokio::test]
    async fn heartbeat_on_vanished_session_is_unknown_session() {
        let h = harness(PRESENCE_KEY_TTL).await;
        assert!(matches!(
            h.svc.heartbeat(uid(), sid()).await,
            Err(PresenceError::UnknownSession)
        ));
    }

    #[tokio::test]
    async fn heartbeat_keeps_session_alive_past_ttl_then_expiry_goes_offline() {
        // Short TTL via the test constructor (90 s is untestable).
        let ttl = Duration::from_millis(400);
        let h = harness(ttl).await;
        let (user, session, g) = (uid(), sid(), GuildId::from(idgen().generate()));
        let mut sub = h.bus.subscribe(Subject::GuildPresence(g)).await.unwrap();

        h.svc
            .connect(user, session, &[g], &[], PresenceStatus::Online)
            .await
            .unwrap();
        recv(&mut sub).await;

        // 8 × 150 ms = 1.2 s total: well past the 400 ms TTL, kept alive only
        // by heartbeats (which also refresh interest + list keys).
        for _ in 0..8 {
            tokio::time::sleep(Duration::from_millis(150)).await;
            h.svc.heartbeat(user, session).await.unwrap();
        }
        let snap = h.svc.snapshot(&[user]).await.unwrap();
        assert_eq!(snap[0].status, PresenceStatus::Online as i32);

        // Stop heartbeating: the session expires; snapshot lazily reports
        // OFFLINE (expiry never broadcasts).
        tokio::time::sleep(Duration::from_millis(900)).await;
        let snap = h.svc.snapshot(&[user]).await.unwrap();
        assert_eq!(snap[0].status, PresenceStatus::Offline as i32);
        assert_eq!(snap[0].since_ms, 0, "user has no users row: last_seen 0");
        expect_silent(&mut sub).await;

        // Heartbeat after expiry: gateway must full-reconnect.
        assert!(matches!(
            h.svc.heartbeat(user, session).await,
            Err(PresenceError::UnknownSession)
        ));
    }

    // ---- disconnect ----

    #[tokio::test]
    async fn disconnect_last_session_broadcasts_offline_and_writes_last_seen() {
        let h = harness(PRESENCE_KEY_TTL).await;
        let (user, session) = (uid(), sid());
        let g = GuildId::from(idgen().generate());
        let dm = ChannelId::from(idgen().generate());
        insert_user(&h.pool, user, false).await;

        let mut sub_g = h.bus.subscribe(Subject::GuildPresence(g)).await.unwrap();
        let mut sub_dm = h.bus.subscribe(Subject::DmPresence(dm)).await.unwrap();

        h.svc
            .connect(user, session, &[g], &[dm], PresenceStatus::Online)
            .await
            .unwrap();
        recv(&mut sub_g).await;
        recv(&mut sub_dm).await;

        h.svc.disconnect(user, session).await.unwrap();

        for sub in [&mut sub_g, &mut sub_dm] {
            let p = presence_of(&recv(sub).await);
            assert_eq!(p.status, PresenceStatus::Offline as i32);
            assert!(p.since_ms > 0);
        }

        let row = sqlx::query!(
            "SELECT last_seen_at FROM users WHERE id = $1",
            user.as_i64()
        )
        .fetch_one(&h.pool)
        .await
        .unwrap();
        assert!(row.last_seen_at.is_some(), "last_seen_at must be written");

        delete_user(&h.pool, user).await;
    }

    #[tokio::test]
    async fn disconnect_non_last_session_broadcasts_new_aggregate_not_offline() {
        let h = harness(PRESENCE_KEY_TTL).await;
        let (user, s1, s2) = (uid(), sid(), sid());
        let g = GuildId::from(idgen().generate());
        let mut sub = h.bus.subscribe(Subject::GuildPresence(g)).await.unwrap();

        h.svc
            .connect(user, s1, &[g], &[], PresenceStatus::Online)
            .await
            .unwrap();
        recv(&mut sub).await; // ONLINE
        h.svc
            .connect(user, s2, &[g], &[], PresenceStatus::Dnd)
            .await
            .unwrap();
        recv(&mut sub).await; // DND

        // Dropping the DND session demotes the aggregate to ONLINE — members
        // must see the change, but it is NOT an OFFLINE transition.
        h.svc.disconnect(user, s2).await.unwrap();
        let p = presence_of(&recv(&mut sub).await);
        assert_eq!(p.status, PresenceStatus::Online as i32);

        // Dropping an equal-status session changes nothing: silent.
        let s3 = sid();
        h.svc
            .connect(user, s3, &[g], &[], PresenceStatus::Online)
            .await
            .unwrap();
        h.svc.disconnect(user, s3).await.unwrap();
        expect_silent(&mut sub).await;
    }

    // ---- detach (disconnected presence) ----

    #[tokio::test]
    async fn detach_broadcasts_disconnected_then_disconnect_goes_offline() {
        let h = harness(PRESENCE_KEY_TTL).await;
        let (user, session) = (uid(), sid());
        let g = GuildId::from(idgen().generate());
        insert_user(&h.pool, user, false).await;
        let mut sub = h.bus.subscribe(Subject::GuildPresence(g)).await.unwrap();

        h.svc
            .connect(user, session, &[g], &[], PresenceStatus::Online)
            .await
            .unwrap();
        assert_eq!(
            presence_of(&recv(&mut sub).await).status,
            PresenceStatus::Online as i32
        );

        // Connection dropped (resume window): others see DISCONNECTED.
        h.svc.detach(user, session).await.unwrap();
        let p = presence_of(&recv(&mut sub).await);
        assert_eq!(p.status, PresenceStatus::Disconnected as i32);
        assert!(p.since_ms > 0);

        // Window expired -> OFFLINE.
        h.svc.disconnect(user, session).await.unwrap();
        assert_eq!(
            presence_of(&recv(&mut sub).await).status,
            PresenceStatus::Offline as i32
        );

        delete_user(&h.pool, user).await;
    }

    #[tokio::test]
    async fn detach_with_a_live_second_session_stays_online() {
        let h = harness(PRESENCE_KEY_TTL).await;
        let (user, s1, s2) = (uid(), sid(), sid());
        let g = GuildId::from(idgen().generate());
        let mut sub = h.bus.subscribe(Subject::GuildPresence(g)).await.unwrap();

        h.svc
            .connect(user, s1, &[g], &[], PresenceStatus::Online)
            .await
            .unwrap();
        recv(&mut sub).await; // ONLINE
        h.svc
            .connect(user, s2, &[g], &[], PresenceStatus::Online)
            .await
            .unwrap();

        // s1 drops but s2 is still live ONLINE: aggregate unchanged -> silent.
        h.svc.detach(user, s1).await.unwrap();
        expect_silent(&mut sub).await;
    }

    #[tokio::test]
    async fn resume_set_status_online_restores_after_detach() {
        let h = harness(PRESENCE_KEY_TTL).await;
        let (user, session) = (uid(), sid());
        let g = GuildId::from(idgen().generate());
        let mut sub = h.bus.subscribe(Subject::GuildPresence(g)).await.unwrap();

        h.svc
            .connect(user, session, &[g], &[], PresenceStatus::Online)
            .await
            .unwrap();
        recv(&mut sub).await; // ONLINE
        h.svc.detach(user, session).await.unwrap();
        assert_eq!(
            presence_of(&recv(&mut sub).await).status,
            PresenceStatus::Disconnected as i32
        );

        // The gateway restores ONLINE on resume (reuses set_status).
        h.svc
            .set_status(user, session, PresenceStatus::Online)
            .await
            .unwrap();
        assert_eq!(
            presence_of(&recv(&mut sub).await).status,
            PresenceStatus::Online as i32
        );
    }

    #[tokio::test]
    async fn detach_on_expired_session_is_idempotent() {
        let h = harness(PRESENCE_KEY_TTL).await;
        // No connect: the session was never live -> detach is a no-op Ok.
        h.svc.detach(uid(), sid()).await.unwrap();
    }

    // ---- add_interest ----

    #[tokio::test]
    async fn add_interest_broadcasts_current_status_to_only_new_subjects() {
        let h = harness(PRESENCE_KEY_TTL).await;
        let (user, session) = (uid(), sid());
        let (g1, g2) = (
            GuildId::from(idgen().generate()),
            GuildId::from(idgen().generate()),
        );
        let dm = ChannelId::from(idgen().generate());

        let mut sub_g1 = h.bus.subscribe(Subject::GuildPresence(g1)).await.unwrap();
        let mut sub_g2 = h.bus.subscribe(Subject::GuildPresence(g2)).await.unwrap();
        let mut sub_dm = h.bus.subscribe(Subject::DmPresence(dm)).await.unwrap();

        h.svc
            .connect(user, session, &[g1], &[], PresenceStatus::Dnd)
            .await
            .unwrap();
        recv(&mut sub_g1).await;

        // Mid-session join of g2 + DM open; g1 is already covered.
        h.svc
            .add_interest(user, session, &[g1, g2], &[dm])
            .await
            .unwrap();

        let p_g2 = presence_of(&recv(&mut sub_g2).await);
        assert_eq!(p_g2.status, PresenceStatus::Dnd as i32);
        let p_dm = presence_of(&recv(&mut sub_dm).await);
        assert_eq!(p_dm.status, PresenceStatus::Dnd as i32);
        expect_silent(&mut sub_g1).await;

        // The merged interest is used for later transitions: disconnect
        // reaches all three subjects.
        h.svc.disconnect(user, session).await.unwrap();
        assert_eq!(
            presence_of(&recv(&mut sub_g1).await).status,
            PresenceStatus::Offline as i32
        );
        assert_eq!(
            presence_of(&recv(&mut sub_g2).await).status,
            PresenceStatus::Offline as i32
        );
        assert_eq!(
            presence_of(&recv(&mut sub_dm).await).status,
            PresenceStatus::Offline as i32
        );
    }

    #[tokio::test]
    async fn add_interest_on_dead_session_is_unknown() {
        let h = harness(PRESENCE_KEY_TTL).await;
        assert!(matches!(
            h.svc
                .add_interest(uid(), sid(), &[GuildId::from_raw(1)], &[])
                .await,
            Err(PresenceError::UnknownSession)
        ));
    }

    // ---- snapshot ----

    #[tokio::test]
    async fn snapshot_mixes_online_and_offline_with_last_seen() {
        let h = harness(PRESENCE_KEY_TTL).await;
        let online_user = uid();
        let offline_user = uid(); // has a users row with last_seen_at
        let ghost_user = uid(); // no users row at all
        insert_user(&h.pool, offline_user, true).await;

        h.svc
            .connect(
                online_user,
                sid(),
                &[GuildId::from(idgen().generate())],
                &[],
                PresenceStatus::Idle,
            )
            .await
            .unwrap();

        let snap = h
            .svc
            .snapshot(&[online_user, offline_user, ghost_user, online_user])
            .await
            .unwrap();

        // Duplicates collapse; input order is preserved.
        assert_eq!(snap.len(), 3);
        assert_eq!(snap[0].user_id, online_user.raw());
        assert_eq!(snap[0].status, PresenceStatus::Idle as i32);
        assert_eq!(snap[0].since_ms, 0);

        assert_eq!(snap[1].user_id, offline_user.raw());
        assert_eq!(snap[1].status, PresenceStatus::Offline as i32);
        assert!(
            snap[1].since_ms > 0,
            "since_ms = users.last_seen_at epoch ms"
        );

        assert_eq!(snap[2].user_id, ghost_user.raw());
        assert_eq!(snap[2].status, PresenceStatus::Offline as i32);
        assert_eq!(snap[2].since_ms, 0);

        delete_user(&h.pool, offline_user).await;
    }

    #[tokio::test]
    async fn snapshot_reports_expired_sessions_offline() {
        let ttl = Duration::from_millis(200);
        let h = harness(ttl).await;
        let user = uid();

        h.svc
            .connect(user, sid(), &[], &[], PresenceStatus::Online)
            .await
            .unwrap();
        let snap = h.svc.snapshot(&[user]).await.unwrap();
        assert_eq!(snap[0].status, PresenceStatus::Online as i32);

        tokio::time::sleep(Duration::from_millis(500)).await;
        let snap = h.svc.snapshot(&[user]).await.unwrap();
        assert_eq!(snap[0].status, PresenceStatus::Offline as i32);
    }
}
