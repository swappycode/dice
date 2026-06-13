//! Refcounted interest map (backend-services.md §3.6, critique #13/#14):
//! ONE bus subscription per subject per process, created on the 0→1
//! transition and dropped on 1→0. Every Ready session is always subscribed
//! to its own `dice.evt.user.{uid}` subject.
//!
//! Consume loops fan each `BusEvent` out to the interested sessions'
//! bounded queues (per-session seq is assigned by the session task at
//! delivery). Special cases handled here:
//!
//! - `session_revoked` on a user subject ⇒ force-close that auth session's
//!   sockets with 4001.
//! - `GuildCreate` / `DmChannelCreate` on a user subject ⇒ register the new
//!   guild/dm interest AND `presence.add_interest` BEFORE forwarding the
//!   frame (critique #14: mid-session joins must work).

use std::collections::HashSet;
use std::future::Future;
use std::sync::Arc;

use dashmap::DashMap;
use dashmap::mapref::entry::Entry;
use dice_common::id::{ChannelId, GuildId, SessionId};
use dice_common::shutdown::CancellationToken;
use dice_event_bus::{BusError, BusEvent, EventBus, Subject};
use dice_protocol::internal::v1::bus_event::Payload as BusPayload;
use dice_protocol::v1::ErrorCode;
use dice_protocol::v1::frame::Payload as FramePayload;
use presence_service::Presence;
use tokio_util::task::TaskTracker;

use crate::session::SessionSender;

pub(crate) struct Router {
    inner: Arc<RouterInner>,
}

struct RouterInner {
    bus: Arc<dyn EventBus>,
    presence: Arc<dyn Presence>,
    subjects: DashMap<Subject, SubjectEntry>,
    sessions: DashMap<u64, SessionRecord>,
    ct: CancellationToken,
    tracker: TaskTracker,
}

struct SubjectEntry {
    senders: Vec<SessionSender>,
    /// Cancelling stops the consume task, dropping the bus subscription.
    stop: CancellationToken,
}

#[derive(Default)]
struct SessionRecord {
    interests: HashSet<Subject>,
}

fn guild_subjects(guild: GuildId) -> [Subject; 3] {
    [
        Subject::GuildMsg(guild),
        Subject::GuildTyping(guild),
        Subject::GuildPresence(guild),
    ]
}

fn dm_subjects(channel: ChannelId) -> [Subject; 3] {
    [
        Subject::DmMsg(channel),
        Subject::DmTyping(channel),
        Subject::DmPresence(channel),
    ]
}

impl Router {
    pub(crate) fn new(
        bus: Arc<dyn EventBus>,
        presence: Arc<dyn Presence>,
        ct: CancellationToken,
        tracker: TaskTracker,
    ) -> Self {
        Self {
            inner: Arc::new(RouterInner {
                bus,
                presence,
                subjects: DashMap::new(),
                sessions: DashMap::new(),
                ct,
                tracker,
            }),
        }
    }

    /// Register a fresh session's full interest set (user subject + all
    /// guild/dm subjects). On error the caller must `unregister_session` to
    /// undo the partial registration.
    pub(crate) async fn register_session(
        &self,
        sender: &SessionSender,
        guilds: &[GuildId],
        dms: &[ChannelId],
    ) -> Result<(), BusError> {
        self.inner
            .sessions
            .insert(sender.session_id, SessionRecord::default());
        add_subject(&self.inner, Subject::User(sender.user), sender).await?;
        for &guild in guilds {
            for subject in guild_subjects(guild) {
                add_subject(&self.inner, subject, sender).await?;
            }
        }
        for &dm in dms {
            for subject in dm_subjects(dm) {
                add_subject(&self.inner, subject, sender).await?;
            }
        }
        Ok(())
    }

    /// Drop every interest of `session_id`; subjects whose refcount hits 0
    /// lose their bus subscription.
    pub(crate) fn unregister_session(&self, session_id: u64) {
        let Some((_, record)) = self.inner.sessions.remove(&session_id) else {
            return;
        };
        for subject in record.interests {
            remove_from_subject(&self.inner, subject, session_id);
        }
    }
}

/// Add `sender` to `subject`, creating the (single) bus subscription on the
/// 0→1 transition.
///
/// Returns a boxed future: `consume` → `deliver` → `add_subject` → (spawn)
/// `consume` is type-recursive, and the explicit `dyn … + Send` cut here is
/// what lets the compiler prove the whole chain `Send`.
fn add_subject<'a>(
    inner: &'a Arc<RouterInner>,
    subject: Subject,
    sender: &'a SessionSender,
) -> std::pin::Pin<Box<dyn Future<Output = Result<(), BusError>> + Send + 'a>> {
    Box::pin(async move {
        // Record the interest first so unregister always sees it.
        match inner.sessions.get_mut(&sender.session_id) {
            Some(mut record) => {
                if !record.interests.insert(subject) {
                    return Ok(()); // already interested
                }
            }
            None => return Ok(()), // session unregistered concurrently
        }

        // Fast path: subject already live.
        if let Some(mut entry) = inner.subjects.get_mut(&subject) {
            push_sender(&mut entry.senders, sender);
            return Ok(());
        }

        // Slow path: subscribe first (cannot await under a shard lock), then
        // insert; a racing creator wins and our extra subscription is dropped.
        let subscription = inner.bus.subscribe(subject).await?;
        match inner.subjects.entry(subject) {
            Entry::Occupied(mut occupied) => {
                drop(subscription);
                push_sender(&mut occupied.get_mut().senders, sender);
            }
            Entry::Vacant(vacant) => {
                let stop = inner.ct.child_token();
                vacant.insert(SubjectEntry {
                    senders: vec![sender.clone()],
                    stop: stop.clone(),
                });
                inner
                    .tracker
                    .spawn(consume(Arc::clone(inner), subject, subscription, stop));
            }
        }
        Ok(())
    })
}

fn push_sender(senders: &mut Vec<SessionSender>, sender: &SessionSender) {
    if !senders.iter().any(|s| s.session_id == sender.session_id) {
        senders.push(sender.clone());
    }
}

fn remove_from_subject(inner: &Arc<RouterInner>, subject: Subject, session_id: u64) {
    if let Entry::Occupied(mut occupied) = inner.subjects.entry(subject) {
        occupied
            .get_mut()
            .senders
            .retain(|s| s.session_id != session_id);
        if occupied.get().senders.is_empty() {
            occupied.get().stop.cancel();
            occupied.remove();
        }
    }
}

/// One consume loop per live subject.
async fn consume(
    inner: Arc<RouterInner>,
    subject: Subject,
    mut subscription: dice_event_bus::BusSubscription,
    stop: CancellationToken,
) {
    loop {
        let event = tokio::select! {
            () = stop.cancelled() => return,
            event = subscription.recv() => match event {
                Some(event) => event,
                None => return, // bus shut down
            },
        };
        deliver(&inner, subject, event).await;
    }
}

async fn deliver(inner: &Arc<RouterInner>, subject: Subject, event: BusEvent) {
    let Some(payload) = event.payload else { return };
    // Clone the interested senders out so no shard lock is held across await.
    let senders: Vec<SessionSender> = match inner.subjects.get(&subject) {
        Some(entry) => entry.senders.clone(),
        None => return,
    };
    match payload {
        BusPayload::SessionRevoked(revoked) => {
            for sender in &senders {
                if sender.user.raw() == revoked.user_id
                    && (revoked.auth_session_id == 0
                        || sender.auth_session.raw() == revoked.auth_session_id)
                {
                    sender.force_close(ErrorCode::Unauthenticated, "session revoked");
                }
            }
        }
        BusPayload::Frame(frame) => {
            // Critique #14: a guild/DM born mid-session needs interest wired
            // BEFORE the announcing frame reaches the client.
            if matches!(subject, Subject::User(_)) {
                match &frame.payload {
                    Some(FramePayload::GuildCreate(gc)) => {
                        if let Some(guild) = &gc.guild {
                            let guild_id = GuildId::from_raw(guild.id);
                            for sender in &senders {
                                add_guild_interest(inner, sender, guild_id).await;
                            }
                        }
                    }
                    Some(FramePayload::DmChannelCreate(dc)) => {
                        if let Some(channel) = &dc.channel {
                            let channel_id = ChannelId::from_raw(channel.id);
                            for sender in &senders {
                                add_dm_interest(inner, sender, channel_id).await;
                            }
                        }
                    }
                    _ => {}
                }
            }
            for sender in &senders {
                sender.dispatch(frame.clone(), event.ephemeral);
            }
        }
    }
}

async fn add_guild_interest(inner: &Arc<RouterInner>, sender: &SessionSender, guild: GuildId) {
    for subject in guild_subjects(guild) {
        if let Err(error) = add_subject(inner, subject, sender).await {
            tracing::warn!(%error, %subject, "mid-session guild interest failed");
        }
    }
    if let Err(error) = inner
        .presence
        .add_interest(
            sender.user,
            SessionId::from_raw(sender.session_id),
            &[guild],
            &[],
        )
        .await
    {
        tracing::warn!(%error, user = %sender.user, "presence add_interest (guild) failed");
    }
}

async fn add_dm_interest(inner: &Arc<RouterInner>, sender: &SessionSender, channel: ChannelId) {
    for subject in dm_subjects(channel) {
        if let Err(error) = add_subject(inner, subject, sender).await {
            tracing::warn!(%error, %subject, "mid-session dm interest failed");
        }
    }
    if let Err(error) = inner
        .presence
        .add_interest(
            sender.user,
            SessionId::from_raw(sender.session_id),
            &[],
            &[channel],
        )
        .await
    {
        tracing::warn!(%error, user = %sender.user, "presence add_interest (dm) failed");
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use dice_common::id::UserId;
    use dice_event_bus::BusConfig;
    use dice_protocol::v1::{self, Frame, frame::Payload};
    use presence_service::PresenceError;
    use std::sync::Mutex;

    use crate::session::{Dispatch, KillSwitch, SessionSender};

    /// (user, guild_ids, dm_channel_ids) captured from `add_interest`.
    type AddedInterest = (u64, Vec<u64>, Vec<u64>);

    /// Records add_interest calls; everything else is a no-op.
    struct NullPresence {
        added: Mutex<Vec<AddedInterest>>,
    }

    #[async_trait::async_trait]
    impl Presence for NullPresence {
        async fn connect(
            &self,
            _user: UserId,
            _session: SessionId,
            _guild_ids: &[GuildId],
            _dm_channel_ids: &[ChannelId],
            _status: v1::PresenceStatus,
        ) -> Result<(), PresenceError> {
            Ok(())
        }
        async fn heartbeat(&self, _: UserId, _: SessionId) -> Result<(), PresenceError> {
            Ok(())
        }
        async fn set_status(
            &self,
            _: UserId,
            _: SessionId,
            _: v1::PresenceStatus,
        ) -> Result<(), PresenceError> {
            Ok(())
        }
        async fn disconnect(&self, _: UserId, _: SessionId) -> Result<(), PresenceError> {
            Ok(())
        }
        async fn add_interest(
            &self,
            user: UserId,
            _session: SessionId,
            guild_ids: &[GuildId],
            dm_channel_ids: &[ChannelId],
        ) -> Result<(), PresenceError> {
            self.added.lock().unwrap().push((
                user.raw(),
                guild_ids.iter().map(|g| g.raw()).collect(),
                dm_channel_ids.iter().map(|c| c.raw()).collect(),
            ));
            Ok(())
        }
        async fn snapshot(
            &self,
            _users: &[UserId],
        ) -> Result<Vec<v1::PresenceUpdate>, PresenceError> {
            Ok(vec![])
        }
    }

    struct Harness {
        router: Router,
        bus: Arc<dyn EventBus>,
        presence: Arc<NullPresence>,
    }

    async fn harness() -> Harness {
        let bus = dice_event_bus::connect(BusConfig::Local { capacity: 64 })
            .await
            .unwrap();
        let presence = Arc::new(NullPresence {
            added: Mutex::new(vec![]),
        });
        let router = Router::new(
            bus.clone(),
            presence.clone(),
            CancellationToken::new(),
            TaskTracker::new(),
        );
        Harness {
            router,
            bus,
            presence,
        }
    }

    fn sender(
        session_id: u64,
        user: u64,
    ) -> (SessionSender, tokio::sync::mpsc::Receiver<Dispatch>) {
        let (tx, rx) = tokio::sync::mpsc::channel(crate::session::OUTBOUND_QUEUE);
        (
            SessionSender::new(
                session_id,
                UserId::from_raw(user),
                SessionId::from_raw(1000 + session_id),
                tx,
                KillSwitch::new(),
            ),
            rx,
        )
    }

    fn message_event(content: &str, ephemeral: bool) -> BusEvent {
        BusEvent {
            event_id: 1,
            emitted_at_ms: 0,
            origin: "test".into(),
            guild_id: 0,
            recipient_user_ids: vec![],
            ephemeral,
            payload: Some(BusPayload::Frame(Frame::dispatch(Payload::MessageCreate(
                v1::MessageCreate {
                    message: Some(v1::Message {
                        id: 9,
                        channel_id: 5,
                        author_id: 1,
                        content: content.to_owned(),
                        edited_at_ms: 0,
                        reply_to_id: 0,
                        reactions: Vec::new(),
                        attachments: Vec::new(),
                    }),
                    nonce: 0,
                },
            )))),
        }
    }

    async fn recv_dispatch(rx: &mut tokio::sync::mpsc::Receiver<Dispatch>) -> Dispatch {
        tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
            .await
            .expect("timed out waiting for dispatch")
            .expect("queue closed")
    }

    #[tokio::test]
    async fn fan_out_reaches_registered_sessions() {
        let h = harness().await;
        let guild = GuildId::from_raw(7);
        let (s1, mut rx1) = sender(1, 100);
        let (s2, mut rx2) = sender(2, 200);
        h.router.register_session(&s1, &[guild], &[]).await.unwrap();
        h.router.register_session(&s2, &[guild], &[]).await.unwrap();

        h.bus
            .publish(Subject::GuildMsg(guild), message_event("hello", false))
            .await
            .unwrap();

        let d1 = recv_dispatch(&mut rx1).await;
        let d2 = recv_dispatch(&mut rx2).await;
        assert!(!d1.ephemeral);
        assert!(!d2.ephemeral);
        assert_eq!(d1.frame.seq, 0, "seq is assigned by the session, not here");
    }

    #[tokio::test]
    async fn guild_create_on_user_subject_adds_interest_before_forwarding() {
        let h = harness().await;
        let user = 300u64;
        let (s, mut rx) = sender(3, user);
        h.router.register_session(&s, &[], &[]).await.unwrap();

        let guild = GuildId::from_raw(42);
        let event = BusEvent {
            event_id: 2,
            emitted_at_ms: 0,
            origin: "test".into(),
            guild_id: guild.raw(),
            recipient_user_ids: vec![user],
            ephemeral: false,
            payload: Some(BusPayload::Frame(Frame::dispatch(Payload::GuildCreate(
                v1::GuildCreate {
                    guild: Some(v1::Guild {
                        id: guild.raw(),
                        name: "g".into(),
                        owner_id: user,
                        channels: vec![],
                        invite_code: "abc".into(),
                        members: vec![],
                    }),
                },
            )))),
        };
        h.bus
            .publish(Subject::User(UserId::from_raw(user)), event)
            .await
            .unwrap();

        // The GuildCreate frame arrives...
        let d = recv_dispatch(&mut rx).await;
        assert!(matches!(d.frame.payload, Some(Payload::GuildCreate(_))));
        // ...and by then the guild interest is live: a guild-subject publish
        // reaches this session.
        h.bus
            .publish(Subject::GuildMsg(guild), message_event("post-join", false))
            .await
            .unwrap();
        let d2 = recv_dispatch(&mut rx).await;
        assert!(matches!(d2.frame.payload, Some(Payload::MessageCreate(_))));
        // presence.add_interest was called with the new guild.
        let added = h.presence.added.lock().unwrap().clone();
        assert_eq!(added, vec![(user, vec![guild.raw()], vec![])]);
    }

    #[tokio::test]
    async fn unregister_drops_subscription_on_last_session() {
        let h = harness().await;
        let guild = GuildId::from_raw(8);
        let (s1, _rx1) = sender(10, 1);
        let (s2, _rx2) = sender(11, 2);
        h.router.register_session(&s1, &[guild], &[]).await.unwrap();
        h.router.register_session(&s2, &[guild], &[]).await.unwrap();
        assert!(
            h.router
                .inner
                .subjects
                .contains_key(&Subject::GuildMsg(guild))
        );

        h.router.unregister_session(10);
        assert!(
            h.router
                .inner
                .subjects
                .contains_key(&Subject::GuildMsg(guild)),
            "still one interested session"
        );
        h.router.unregister_session(11);
        assert!(
            !h.router
                .inner
                .subjects
                .contains_key(&Subject::GuildMsg(guild)),
            "refcount hit zero"
        );
        assert!(h.router.inner.subjects.is_empty());
    }
}
