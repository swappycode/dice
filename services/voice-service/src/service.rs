//! [`VoiceService`]: cache-backed voice-channel membership with bus fan-out.
//!
//! # Cache layout
//!
//! Values are protobuf bytes, never JSON:
//!
//! | Key                    | Value                              | TTL  |
//! |------------------------|------------------------------------|------|
//! | `voice:ch:{channel}`   | [`RosterState`] (repeated member)  | none |
//! | `voice:u:{user}`       | the user's current channel (LE u64)| none |
//!
//! There is no TTL: voice membership is explicit. The gateway calls
//! [`Voice::disconnect`](crate::Voice::disconnect) when a session ends (socket
//! closed AND resume window expired), exactly as it calls `presence.disconnect`,
//! so the only way to orphan a member is a gateway crash — an accepted phase-1
//! limitation (the on-hardware hardening phase adds a heartbeat-refreshed TTL).
//!
//! # Concurrency (phase-1 scope)
//!
//! The [`Cache`] trait has no compare-and-swap, so the roster read-modify-write
//! is not atomic across nodes. Voice runs single-node (monolith) in phase 1;
//! the window is kept small. Multi-node atomicity (Lua / `WATCH`) is deferred,
//! mirroring presence.

use std::sync::Arc;

use bytes::Bytes;
use dice_cache::Cache;
use dice_common::time::now_ms;
use dice_common::{ChannelId, GuildId, SnowflakeGenerator, UserId};
use dice_database::PgPool;
use dice_event_bus::{BusEvent, EventBus, Subject};
use dice_protocol::internal::v1::bus_event;
use dice_protocol::v1::{self, frame::Payload as FramePayload};
use prost::Message;

use crate::{Voice, VoiceError};

/// `origin` stamped on every published [`BusEvent`].
pub const ORIGIN: &str = "voice-service";

/// `channels.channel_type` value for a VOICE channel (dice.v1 ChannelKind,
/// stored verbatim).
const KIND_VOICE: i16 = v1::ChannelKind::Voice as i16;

/// Value of `voice:ch:{channel}`: the channel's current members.
#[derive(Clone, PartialEq, ::prost::Message)]
struct RosterState {
    #[prost(message, repeated, tag = "1")]
    members: Vec<v1::VoiceMember>,
}

fn channel_key(channel: ChannelId) -> String {
    format!("voice:ch:{channel}")
}

fn user_key(user: UserId) -> String {
    format!("voice:u:{user}")
}

fn internal<E>(e: E) -> VoiceError
where
    E: std::error::Error + Send + Sync + 'static,
{
    VoiceError::Internal(Box::new(e))
}

/// The one [`Voice`] implementation (phase-1 single-node).
pub struct VoiceService {
    cache: Arc<dyn Cache>,
    bus: Arc<dyn EventBus>,
    pool: PgPool,
    ids: Arc<SnowflakeGenerator>,
}

impl VoiceService {
    /// Construct from the shared cache, bus, pool, and id generator (the same
    /// four deps presence takes).
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
        }
    }

    /// Read a channel's roster (empty if absent; a corrupt blob logs and reads
    /// empty — the cache is rebuildable state).
    async fn read_roster(&self, channel: ChannelId) -> Result<RosterState, VoiceError> {
        let Some(raw) = self
            .cache
            .get(&channel_key(channel))
            .await
            .map_err(internal)?
        else {
            return Ok(RosterState::default());
        };
        match RosterState::decode(raw.as_ref()) {
            Ok(state) => Ok(state),
            Err(e) => {
                tracing::warn!(%channel, error = %e, "corrupt voice roster; treating as empty");
                Ok(RosterState::default())
            }
        }
    }

    /// Write a roster, or delete the key when it becomes empty.
    async fn write_roster(
        &self,
        channel: ChannelId,
        roster: &RosterState,
    ) -> Result<(), VoiceError> {
        let key = channel_key(channel);
        if roster.members.is_empty() {
            self.cache.delete(&key).await.map_err(internal)
        } else {
            self.cache
                .set(&key, Bytes::from(roster.encode_to_vec()), None)
                .await
                .map_err(internal)
        }
    }

    /// The single channel the user is currently in, if any.
    async fn read_user_channel(&self, user: UserId) -> Result<Option<ChannelId>, VoiceError> {
        Ok(self
            .cache
            .get(&user_key(user))
            .await
            .map_err(internal)?
            .and_then(|b| <[u8; 8]>::try_from(b.as_ref()).ok())
            .map(u64::from_le_bytes)
            .filter(|&raw| raw != 0)
            .map(ChannelId::from_raw))
    }

    async fn set_user_channel(&self, user: UserId, channel: ChannelId) -> Result<(), VoiceError> {
        self.cache
            .set(
                &user_key(user),
                Bytes::copy_from_slice(&channel.raw().to_le_bytes()),
                None,
            )
            .await
            .map_err(internal)
    }

    async fn clear_user_channel(&self, user: UserId) -> Result<(), VoiceError> {
        self.cache.delete(&user_key(user)).await.map_err(internal)
    }

    /// Validate that `channel` is a VOICE channel the `user` may join; returns
    /// its guild id.
    async fn validate_join(&self, user: UserId, channel: ChannelId) -> Result<GuildId, VoiceError> {
        let row = sqlx::query!(
            r#"SELECT c.channel_type, c.guild_id,
                      EXISTS(SELECT 1 FROM guild_members gm
                             WHERE gm.guild_id = c.guild_id AND gm.user_id = $2) AS "is_member!"
               FROM channels c WHERE c.id = $1"#,
            channel.as_i64(),
            user.as_i64(),
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(internal)?
        .ok_or(VoiceError::NotFound)?;
        if row.channel_type != KIND_VOICE {
            return Err(VoiceError::NotAVoiceChannel);
        }
        let guild_id = row.guild_id.ok_or(VoiceError::NotAVoiceChannel)?;
        if !row.is_member {
            return Err(VoiceError::NotAMember);
        }
        Ok(GuildId::from_i64(guild_id))
    }

    /// Resolve the `User` records for a set of member ids (the roster's warm
    /// dictionary, so clients never render a participant as "unknown").
    async fn resolve_users(&self, ids: &[i64]) -> Result<Vec<v1::User>, VoiceError> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let users = sqlx::query!(
            r#"SELECT id, username, display_name, flags, avatar_media_id FROM users
               WHERE id = ANY($1::bigint[]) ORDER BY id"#,
            ids
        )
        .fetch_all(&self.pool)
        .await
        .map_err(internal)?
        .into_iter()
        .map(|r| v1::User {
            id: r.id as u64,
            username: r.username,
            display_name: r.display_name.unwrap_or_default(),
            flags: r.flags as u32,
            avatar_id: r.avatar_media_id.map_or(0, |v| v as u64),
        })
        .collect();
        Ok(users)
    }

    /// Build the wire roster snapshot (members + resolved user dictionary).
    async fn snapshot(
        &self,
        channel: ChannelId,
        roster: &RosterState,
    ) -> Result<v1::VoiceRoster, VoiceError> {
        let ids: Vec<i64> = roster.members.iter().map(|m| m.user_id as i64).collect();
        Ok(v1::VoiceRoster {
            channel_id: channel.raw(),
            members: roster.members.clone(),
            users: self.resolve_users(&ids).await?,
        })
    }

    /// Publish one ephemeral voice dispatch to a guild's voice subject.
    async fn publish(&self, guild: GuildId, payload: FramePayload) {
        let event = BusEvent {
            event_id: self.ids.generate().0,
            emitted_at_ms: now_ms(),
            origin: ORIGIN.to_owned(),
            guild_id: guild.raw(),
            recipient_user_ids: Vec::new(),
            // Voice signaling is live state, not history: never replayed.
            ephemeral: true,
            payload: Some(bus_event::Payload::Frame(v1::Frame::dispatch(payload))),
        };
        if let Err(error) = self.bus.publish(Subject::GuildVoice(guild), event).await {
            tracing::error!(%error, %guild, "voice bus publish failed");
        }
    }
}

#[async_trait::async_trait]
impl Voice for VoiceService {
    async fn join(
        &self,
        user: UserId,
        channel: ChannelId,
        muted: bool,
        deafened: bool,
    ) -> Result<v1::VoiceRoster, VoiceError> {
        let guild = self.validate_join(user, channel).await?;

        // One voice channel at a time: leave any other current channel first.
        if let Some(current) = self.read_user_channel(user).await?
            && current != channel
        {
            self.leave(user, current).await?;
        }

        let ssrc = (self.ids.generate().0 & 0xFFFF_FFFF) as u32;
        let member = v1::VoiceMember {
            user_id: user.raw(),
            channel_id: channel.raw(),
            guild_id: guild.raw(),
            ssrc,
            muted,
            deafened,
            speaking: false,
        };

        let mut roster = self.read_roster(channel).await?;
        roster.members.retain(|m| m.user_id != user.raw()); // re-join replaces
        roster.members.push(member); // VoiceMember is Copy (all scalar)
        self.write_roster(channel, &roster).await?;
        self.set_user_channel(user, channel).await?;

        let joiner = self
            .resolve_users(&[user.as_i64()])
            .await?
            .into_iter()
            .next();
        self.publish(
            guild,
            FramePayload::VoiceJoin(v1::VoiceJoin {
                member: Some(member),
                user: joiner,
            }),
        )
        .await;

        self.snapshot(channel, &roster).await
    }

    async fn leave(&self, user: UserId, channel: ChannelId) -> Result<(), VoiceError> {
        let mut roster = self.read_roster(channel).await?;
        let Some(pos) = roster.members.iter().position(|m| m.user_id == user.raw()) else {
            // Not a member; still clear a stale self-pointer at this channel.
            if self.read_user_channel(user).await? == Some(channel) {
                self.clear_user_channel(user).await?;
            }
            return Ok(());
        };
        let removed = roster.members.remove(pos);
        self.write_roster(channel, &roster).await?;
        if self.read_user_channel(user).await? == Some(channel) {
            self.clear_user_channel(user).await?;
        }
        self.publish(
            GuildId::from_raw(removed.guild_id),
            FramePayload::VoiceLeave(v1::VoiceLeave {
                channel_id: channel.raw(),
                user_id: user.raw(),
                guild_id: removed.guild_id,
            }),
        )
        .await;
        Ok(())
    }

    async fn update_state(
        &self,
        user: UserId,
        channel: ChannelId,
        muted: bool,
        deafened: bool,
        speaking: bool,
    ) -> Result<(), VoiceError> {
        let mut roster = self.read_roster(channel).await?;
        let Some(member) = roster.members.iter_mut().find(|m| m.user_id == user.raw()) else {
            return Err(VoiceError::NotInChannel);
        };
        member.muted = muted;
        member.deafened = deafened;
        member.speaking = speaking;
        let updated = *member;
        self.write_roster(channel, &roster).await?;
        self.publish(
            GuildId::from_raw(updated.guild_id),
            FramePayload::VoiceState(v1::VoiceState {
                member: Some(updated),
            }),
        )
        .await;
        Ok(())
    }

    async fn roster(&self, channel: ChannelId) -> Result<v1::VoiceRoster, VoiceError> {
        let roster = self.read_roster(channel).await?;
        self.snapshot(channel, &roster).await
    }

    async fn disconnect(&self, user: UserId) -> Result<(), VoiceError> {
        if let Some(channel) = self.read_user_channel(user).await? {
            self.leave(user, channel).await?;
        }
        Ok(())
    }

    async fn forward(
        &self,
        sender: UserId,
        packet: Bytes,
        sink: &dyn crate::VoiceSink,
    ) -> Result<(), VoiceError> {
        let Some(channel) = self.read_user_channel(sender).await? else {
            return Ok(()); // sender isn't in a voice channel — drop
        };
        let roster = self.read_roster(channel).await?;
        for member in &roster.members {
            if member.user_id != sender.raw() {
                sink.deliver(UserId::from_raw(member.user_id), packet.clone());
            }
        }
        Ok(())
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

    /// Matches d:\Dice\.env (cargo test does not load .env at runtime).
    const DEV_DB_URL: &str = "postgres://dice:dice_dev@localhost:5433/dice";

    fn database_url() -> String {
        std::env::var("DATABASE_URL").unwrap_or_else(|_| DEV_DB_URL.to_owned())
    }

    fn idgen() -> Arc<SnowflakeGenerator> {
        static GENERATOR: OnceLock<Arc<SnowflakeGenerator>> = OnceLock::new();
        GENERATOR
            .get_or_init(|| {
                Arc::new(SnowflakeGenerator::new((std::process::id() % 1024) as u16).unwrap())
            })
            .clone()
    }

    struct Harness {
        svc: VoiceService,
        bus: Arc<dyn EventBus>,
        pool: PgPool,
    }

    async fn harness() -> Harness {
        let cache = dice_cache::connect(CacheConfig::Memory).await.unwrap();
        let bus = dice_event_bus::connect(BusConfig::Local { capacity: 1024 })
            .await
            .unwrap();
        let pool = PgPoolOptions::new().connect_lazy(&database_url()).unwrap();
        let svc = VoiceService::new(cache, Arc::clone(&bus), pool.clone(), idgen());
        Harness { svc, bus, pool }
    }

    async fn recv(sub: &mut BusSubscription) -> BusEvent {
        timeout(Duration::from_secs(2), sub.recv())
            .await
            .expect("bus recv timed out")
            .expect("bus closed")
    }

    fn payload_of(ev: &BusEvent) -> FramePayload {
        let Some(bus_event::Payload::Frame(frame)) = &ev.payload else {
            panic!("bus event payload must be a Frame");
        };
        assert_eq!(frame.seq, 0, "service-published frames carry seq=0");
        assert!(ev.ephemeral, "voice dispatches are ephemeral");
        frame.payload.clone().expect("frame has a payload")
    }

    /// Insert a user, a guild owned by the first user, a voice channel, and
    /// memberships for every given user. Returns the channel id.
    async fn seed(pool: &PgPool, users: &[UserId]) -> (GuildId, ChannelId) {
        let guild = GuildId::from(idgen().generate());
        let channel = ChannelId::from(idgen().generate());
        for (i, u) in users.iter().enumerate() {
            let name = format!("vc{}", u.raw());
            sqlx::query!(
                "INSERT INTO users (id, username, email, password_hash) VALUES ($1, $2, $3, 'x')",
                u.as_i64(),
                name,
                format!("{name}@test.dice"),
            )
            .execute(pool)
            .await
            .unwrap();
            if i == 0 {
                sqlx::query!(
                    "INSERT INTO guilds (id, name, owner_id, invite_code) VALUES ($1, 'vt', $2, $3)",
                    guild.as_i64(),
                    u.as_i64(),
                    format!("inv{}", guild.raw()),
                )
                .execute(pool)
                .await
                .unwrap();
                sqlx::query!(
                    "INSERT INTO channels (id, channel_type, guild_id, name, position) \
                     VALUES ($1, $2, $3, 'Lounge', 0)",
                    channel.as_i64(),
                    KIND_VOICE,
                    guild.as_i64(),
                )
                .execute(pool)
                .await
                .unwrap();
            }
            sqlx::query!(
                "INSERT INTO guild_members (guild_id, user_id, permissions) VALUES ($1, $2, 0)",
                guild.as_i64(),
                u.as_i64(),
            )
            .execute(pool)
            .await
            .unwrap();
        }
        (guild, channel)
    }

    async fn cleanup(pool: &PgPool, guild: GuildId) {
        // FKs cascade channels + memberships; delete the now-unreferenced users.
        let owner: Vec<i64> = sqlx::query_scalar!(
            "SELECT user_id FROM guild_members WHERE guild_id = $1",
            guild.as_i64()
        )
        .fetch_all(pool)
        .await
        .unwrap();
        sqlx::query!("DELETE FROM guilds WHERE id = $1", guild.as_i64())
            .execute(pool)
            .await
            .unwrap();
        for id in owner {
            let _ = sqlx::query!("DELETE FROM users WHERE id = $1", id)
                .execute(pool)
                .await;
        }
    }

    #[tokio::test]
    async fn join_leave_speaking_round_trips_over_the_bus() {
        let h = harness().await;
        let (a, b) = (
            UserId::from(idgen().generate()),
            UserId::from(idgen().generate()),
        );
        let (guild, channel) = seed(&h.pool, &[a, b]).await;
        let mut sub = h.bus.subscribe(Subject::GuildVoice(guild)).await.unwrap();

        // A joins: roster has A, a VoiceJoin fans out carrying A's user record.
        let roster = h.svc.join(a, channel, false, false).await.unwrap();
        assert_eq!(roster.channel_id, channel.raw());
        assert_eq!(roster.members.len(), 1);
        assert_eq!(roster.members[0].user_id, a.raw());
        assert!(roster.members[0].ssrc != 0);
        assert_eq!(roster.users.len(), 1, "warm dictionary for A");
        let FramePayload::VoiceJoin(vj) = payload_of(&recv(&mut sub).await) else {
            panic!("expected VoiceJoin");
        };
        assert_eq!(vj.member.unwrap().user_id, a.raw());
        assert_eq!(vj.user.unwrap().id, a.raw());

        // B joins: roster now has both; B's VoiceJoin fans out.
        let roster = h.svc.join(b, channel, true, false).await.unwrap();
        assert_eq!(roster.members.len(), 2);
        assert!(
            roster
                .members
                .iter()
                .any(|m| m.user_id == b.raw() && m.muted)
        );
        let FramePayload::VoiceJoin(vj) = payload_of(&recv(&mut sub).await) else {
            panic!("expected VoiceJoin for B");
        };
        assert_eq!(vj.member.unwrap().user_id, b.raw());

        // A starts speaking: VoiceState fans out with the updated flag.
        h.svc
            .update_state(a, channel, false, false, true)
            .await
            .unwrap();
        let FramePayload::VoiceState(vs) = payload_of(&recv(&mut sub).await) else {
            panic!("expected VoiceState");
        };
        let m = vs.member.unwrap();
        assert_eq!(m.user_id, a.raw());
        assert!(m.speaking);
        // The roster reflects the new speaking flag.
        assert!(
            h.svc
                .roster(channel)
                .await
                .unwrap()
                .members
                .iter()
                .any(|m| m.user_id == a.raw() && m.speaking)
        );

        // State update without joining is rejected.
        let c = UserId::from(idgen().generate());
        assert!(matches!(
            h.svc.update_state(c, channel, false, false, true).await,
            Err(VoiceError::NotInChannel)
        ));

        // A leaves (explicitly); VoiceLeave fans out; roster shrinks to B.
        h.svc.leave(a, channel).await.unwrap();
        let FramePayload::VoiceLeave(vl) = payload_of(&recv(&mut sub).await) else {
            panic!("expected VoiceLeave");
        };
        assert_eq!(vl.user_id, a.raw());
        assert_eq!(vl.channel_id, channel.raw());
        assert_eq!(h.svc.roster(channel).await.unwrap().members.len(), 1);

        // B disconnects (session teardown): leaves implicitly; roster empties.
        h.svc.disconnect(b).await.unwrap();
        let FramePayload::VoiceLeave(vl) = payload_of(&recv(&mut sub).await) else {
            panic!("expected VoiceLeave from disconnect");
        };
        assert_eq!(vl.user_id, b.raw());
        assert!(h.svc.roster(channel).await.unwrap().members.is_empty());

        cleanup(&h.pool, guild).await;
    }

    #[tokio::test]
    async fn rejects_non_voice_and_non_member() {
        let h = harness().await;
        let a = UserId::from(idgen().generate());
        let (guild, voice) = seed(&h.pool, &[a]).await;

        // Unknown channel.
        assert!(matches!(
            h.svc.join(a, ChannelId::from_raw(1), false, false).await,
            Err(VoiceError::NotFound)
        ));

        // A non-member of the guild can't join its voice channel.
        let outsider = UserId::from(idgen().generate());
        sqlx::query!(
            "INSERT INTO users (id, username, email, password_hash) VALUES ($1, $2, $3, 'x')",
            outsider.as_i64(),
            format!("out{}", outsider.raw()),
            format!("out{}@test.dice", outsider.raw()),
        )
        .execute(&h.pool)
        .await
        .unwrap();
        assert!(matches!(
            h.svc.join(outsider, voice, false, false).await,
            Err(VoiceError::NotAMember)
        ));

        // A GUILD_TEXT channel is not joinable as voice.
        let text = ChannelId::from(idgen().generate());
        sqlx::query!(
            "INSERT INTO channels (id, channel_type, guild_id, name, position) \
             VALUES ($1, 1, $2, 'general', 1)",
            text.as_i64(),
            guild.as_i64(),
        )
        .execute(&h.pool)
        .await
        .unwrap();
        assert!(matches!(
            h.svc.join(a, text, false, false).await,
            Err(VoiceError::NotAVoiceChannel)
        ));

        let _ = sqlx::query!("DELETE FROM users WHERE id = $1", outsider.as_i64())
            .execute(&h.pool)
            .await;
        cleanup(&h.pool, guild).await;
    }

    /// A recording [`crate::VoiceSink`] for the loopback test.
    #[derive(Default)]
    struct RecordingSink {
        delivered: std::sync::Mutex<Vec<(UserId, Bytes)>>,
    }

    impl crate::VoiceSink for RecordingSink {
        fn deliver(&self, target: UserId, packet: Bytes) {
            self.delivered.lock().unwrap().push((target, packet));
        }
    }

    /// The phase-2 SFU gate: feed an encoded voice frame in, assert it fans out
    /// to every other channel member (and never echoes to the sender).
    #[tokio::test]
    async fn forward_fans_out_to_co_members_only() {
        use dice_voice_core::VoiceFrame;

        let h = harness().await;
        let (a, b, c) = (
            UserId::from(idgen().generate()),
            UserId::from(idgen().generate()),
            UserId::from(idgen().generate()),
        );
        let (guild, channel) = seed(&h.pool, &[a, b, c]).await;
        for u in [a, b, c] {
            h.svc.join(u, channel, false, false).await.unwrap();
        }

        let packet = VoiceFrame {
            ssrc: 42,
            seq: 7,
            timestamp: 6720,
            marker: true,
            payload: Bytes::from_static(b"synthetic-opus"),
        }
        .encode();

        let sink = RecordingSink::default();
        h.svc.forward(a, packet.clone(), &sink).await.unwrap();

        // Snapshot to owned values so no lock guard is held across an await.
        let got: Vec<(UserId, Bytes)> = sink.delivered.lock().unwrap().clone();
        assert_eq!(got.len(), 2, "fans out to the two co-members");
        let targets: Vec<UserId> = got.iter().map(|(t, _)| *t).collect();
        assert!(targets.contains(&b) && targets.contains(&c));
        assert!(!targets.contains(&a), "never echoes to the sender");
        assert!(
            got.iter().all(|(_, p)| *p == packet),
            "packet forwarded verbatim"
        );

        // A sender not in any voice channel is a silent no-op.
        let outsider = UserId::from(idgen().generate());
        let sink2 = RecordingSink::default();
        h.svc.forward(outsider, packet, &sink2).await.unwrap();
        let delivered2 = sink2.delivered.lock().unwrap().len();
        assert_eq!(delivered2, 0);

        cleanup(&h.pool, guild).await;
    }

    #[tokio::test]
    async fn joining_a_second_channel_leaves_the_first() {
        let h = harness().await;
        let a = UserId::from(idgen().generate());
        let (guild, first) = seed(&h.pool, &[a]).await;
        // A second voice channel in the same guild.
        let second = ChannelId::from(idgen().generate());
        sqlx::query!(
            "INSERT INTO channels (id, channel_type, guild_id, name, position) \
             VALUES ($1, $2, $3, 'Lounge2', 1)",
            second.as_i64(),
            KIND_VOICE,
            guild.as_i64(),
        )
        .execute(&h.pool)
        .await
        .unwrap();
        let mut sub = h.bus.subscribe(Subject::GuildVoice(guild)).await.unwrap();

        h.svc.join(a, first, false, false).await.unwrap();
        let _ = recv(&mut sub).await; // VoiceJoin(first)

        // Joining the second channel emits VoiceLeave(first) then VoiceJoin(second).
        h.svc.join(a, second, false, false).await.unwrap();
        let FramePayload::VoiceLeave(vl) = payload_of(&recv(&mut sub).await) else {
            panic!("expected VoiceLeave(first)");
        };
        assert_eq!(vl.channel_id, first.raw());
        let FramePayload::VoiceJoin(vj) = payload_of(&recv(&mut sub).await) else {
            panic!("expected VoiceJoin(second)");
        };
        assert_eq!(vj.member.unwrap().channel_id, second.raw());

        assert!(h.svc.roster(first).await.unwrap().members.is_empty());
        assert_eq!(h.svc.roster(second).await.unwrap().members.len(), 1);

        cleanup(&h.pool, guild).await;
    }
}
