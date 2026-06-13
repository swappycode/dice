//! The event pump (design §2.2): ONE task consuming the gateway driver's
//! event stream. Ordering rule: **cache first, then emit** — the webview can
//! never observe state the cache does not have, so offline restarts are
//! always self-consistent. Presence is coalesced on a 100 ms tick.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use dice_network_core::client::{ClientEvent, Command, ConnStateLite, GatewayHandle};
use dice_protocol::v1;
use dice_protocol::v1::frame::Payload;
use tokio::sync::{mpsc, oneshot, watch};

use crate::cache::Cache;
use crate::dto::{
    ChannelDto, DiceEvent, GuildDto, MessageDto, RESYNC_CHANNEL, UserDto, id_str, presence_str,
};
use crate::emit::{Emitter, emit_dice};
use crate::session::SessionManager;

/// Whether the pump should keep running after an event.
enum Pump {
    Continue,
    /// Terminal: tear the driver down and end the bridge task.
    Stop,
}

/// Presence updates are batched on this tick to keep webview wakeups low.
const PRESENCE_TICK: Duration = Duration::from_millis(100);

/// One in-flight `SendMessage`, keyed by WIRE nonce (u64). The frontend's
/// nonce is an opaque string (`crypto.randomUUID()`), so the host owns the
/// string↔u64 mapping.
pub struct PendingSend {
    pub client_nonce: String,
    pub channel_id: u64,
    /// Resolves the awaiting `send_message` command.
    pub waiter: Option<oneshot::Sender<Result<v1::Message, String>>>,
    /// The own-echo dispatch arrived before the ack (don't emit twice).
    pub dispatched: bool,
}

pub type PendingMap = Arc<StdMutex<HashMap<u64, PendingSend>>>;
pub type PresenceMap = Arc<StdMutex<HashMap<u64, i32>>>;

pub fn conn_state_str(state: ConnStateLite) -> &'static str {
    match state {
        ConnStateLite::Idle => "idle",
        ConnStateLite::Connecting | ConnStateLite::Authenticating => "connecting",
        ConnStateLite::Ready { .. } => "connected",
        ConnStateLite::Backoff => "reconnecting",
        ConnStateLite::Failed => "offline",
    }
}

pub struct Bridge {
    cache: Cache,
    emitter: Arc<dyn Emitter>,
    /// Cleared (keyring + RAM) when the gateway reports the session is dead.
    session: Arc<SessionManager>,
    presence: PresenceMap,
    pending: PendingMap,
    current_user: Arc<StdMutex<Option<v1::User>>>,
    ready_counter: Arc<watch::Sender<u64>>,
    rt: tokio::runtime::Handle,
    /// Set on `SessionInvalidated`; the next applied Ready emits
    /// `cache://resynced`.
    resync_pending: bool,
    presence_buf: Arc<StdMutex<HashMap<u64, i32>>>,
    flush_scheduled: Arc<AtomicBool>,
}

impl Bridge {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        cache: Cache,
        emitter: Arc<dyn Emitter>,
        session: Arc<SessionManager>,
        presence: PresenceMap,
        pending: PendingMap,
        current_user: Arc<StdMutex<Option<v1::User>>>,
        ready_counter: Arc<watch::Sender<u64>>,
        rt: tokio::runtime::Handle,
    ) -> Self {
        Self {
            cache,
            emitter,
            session,
            presence,
            pending,
            current_user,
            ready_counter,
            rt,
            resync_pending: false,
            presence_buf: Arc::default(),
            flush_scheduled: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Own the gateway handle: pump events (priority) and relay outbound
    /// commands. `Command::Shutdown` (or the relay closing) cleanly shuts the
    /// driver down.
    pub async fn run(mut self, mut handle: GatewayHandle, mut cmds: mpsc::Receiver<Command>) {
        enum Next {
            Event(Option<ClientEvent>),
            Cmd(Option<Command>),
        }
        loop {
            let next = tokio::select! {
                biased;
                event = handle.events().recv() => Next::Event(event),
                cmd = cmds.recv() => Next::Cmd(cmd),
            };
            match next {
                Next::Event(Some(event)) => {
                    if let Pump::Stop = self.on_event(event).await {
                        // Terminal (session expired): stop the parked driver
                        // so the gateway slot frees up for the next login.
                        handle.shutdown().await;
                        return;
                    }
                }
                Next::Event(None) => return, // driver gone
                Next::Cmd(None) | Next::Cmd(Some(Command::Shutdown)) => {
                    handle.shutdown().await;
                    return;
                }
                Next::Cmd(Some(cmd)) => {
                    if handle.send(cmd).await.is_err() {
                        return;
                    }
                }
            }
        }
    }

    async fn on_event(&mut self, event: ClientEvent) -> Pump {
        match event {
            ClientEvent::Ready(ready) => self.on_ready(*ready).await,
            ClientEvent::Resumed { .. } => {} // replayed dispatches follow normally
            ClientEvent::SessionInvalidated => {
                // NOT terminal: the resume token was stale but a fresh Identify
                // is in flight on the same connection. Just mark caches stale.
                if let Err(error) = self.cache.mark_all_stale().await {
                    tracing::warn!(%error, "mark_all_stale failed");
                }
                self.resync_pending = true;
            }
            ClientEvent::AuthExpired { reason } => {
                self.on_auth_expired(reason).await;
                return Pump::Stop;
            }
            ClientEvent::Ack { nonce, message } => self.on_ack(nonce, message).await,
            ClientEvent::RequestError { nonce, error } => {
                let entry = self.pending.lock().expect("pending lock").remove(&nonce);
                if let Some(mut pending) = entry {
                    if let Err(error) = self.cache.mark_failed(pending.client_nonce.clone()).await {
                        tracing::warn!(%error, "mark_failed failed");
                    }
                    if let Some(waiter) = pending.waiter.take() {
                        let _ = waiter.send(Err(error.message));
                    }
                }
            }
            ClientEvent::Dispatch(frame) => {
                if let Some(payload) = frame.payload {
                    self.on_dispatch(payload).await;
                }
            }
            ClientEvent::ConnState(state) => {
                // Persist the last-good transport so the next start skips the
                // QUIC probe on WSS-bound networks (and vice versa).
                let transport = if let ConnStateLite::Ready { transport } = state {
                    let name = dice_network_core::client::PreferredTransport::from(transport)
                        .as_str()
                        .to_owned();
                    if let Err(error) = self
                        .cache
                        .set_meta("last_transport".to_owned(), name.clone())
                        .await
                    {
                        tracing::warn!(%error, "persisting last_transport failed");
                    }
                    Some(name)
                } else {
                    None
                };
                emit_dice(
                    &self.emitter,
                    &DiceEvent::ConnState {
                        state: conn_state_str(state).to_owned(),
                        transport,
                    },
                );
            }
        }
        Pump::Continue
    }

    /// The server rejected the stored session for good (Issue 1). Clear
    /// credentials + the cache locally and tell the webview to show login,
    /// so a monolith restart that wiped server-side sessions can't leave the
    /// client stranded on an "Offline" shell with no way back.
    async fn on_auth_expired(&mut self, reason: String) {
        tracing::warn!(%reason, "gateway session expired; clearing local credentials");
        self.session.clear().await;
        if let Err(error) = self.cache.wipe().await {
            tracing::warn!(%error, "cache wipe after session expiry failed");
        }
        self.pending.lock().expect("pending lock").clear();
        self.presence.lock().expect("presence lock").clear();
        *self.current_user.lock().expect("user lock") = None;
        emit_dice(&self.emitter, &DiceEvent::SessionExpired);
    }

    async fn on_ready(&mut self, ready: v1::Ready) {
        if let Some(user) = &ready.user {
            *self.current_user.lock().expect("user lock") = Some(user.clone());
        }
        {
            let mut presence = self.presence.lock().expect("presence lock");
            presence.clear();
            for update in &ready.presences {
                presence.insert(update.user_id, update.status);
            }
        }
        let presences = ready.presences.clone();
        if let Err(error) = self.cache.apply_ready(ready).await {
            tracing::error!(%error, "apply_ready failed");
        }
        // Cache applied — only now may anyone observe the new session.
        self.ready_counter.send_modify(|n| *n += 1);
        if std::mem::take(&mut self.resync_pending) {
            self.emitter.emit(RESYNC_CHANNEL, serde_json::json!({}));
        }
        // Keep live presence orbs right after a reconnect (coalesced).
        for update in &presences {
            self.queue_presence(update.user_id, update.status);
        }
    }

    async fn on_ack(&mut self, nonce: u64, message: v1::Message) {
        let entry = self.pending.lock().expect("pending lock").remove(&nonce);
        let Some(mut pending) = entry else {
            tracing::debug!(nonce, "ack without a pending mapping; ignoring");
            return;
        };
        if let Err(error) = self
            .cache
            .reconcile_by_nonce(pending.client_nonce.clone(), message.clone())
            .await
        {
            tracing::warn!(%error, "reconcile_by_nonce failed");
        }
        if !pending.dispatched {
            emit_dice(
                &self.emitter,
                &DiceEvent::MessageCreate {
                    message: MessageDto::from_wire(&message, Some(pending.client_nonce.clone())),
                    nonce: Some(pending.client_nonce.clone()),
                },
            );
        }
        if let Some(waiter) = pending.waiter.take() {
            let _ = waiter.send(Ok(message));
        }
    }

    async fn on_dispatch(&mut self, payload: Payload) {
        match &payload {
            Payload::MessageCreate(mc) => {
                let Some(message) = mc.message.clone() else {
                    return;
                };
                // Our own echo? Map the wire nonce back to the frontend's
                // string nonce (entry stays until the Ack resolves it).
                let client_nonce = if mc.nonce != 0 {
                    let mut map = self.pending.lock().expect("pending lock");
                    map.get_mut(&mc.nonce).map(|p| {
                        p.dispatched = true;
                        p.client_nonce.clone()
                    })
                } else {
                    None
                };
                let cached = match &client_nonce {
                    Some(nonce) => {
                        self.cache
                            .reconcile_by_nonce(nonce.clone(), message.clone())
                            .await
                    }
                    None => self.cache.apply_event(payload.clone()).await,
                };
                if let Err(error) = cached {
                    tracing::warn!(%error, "message cache write failed");
                }
                emit_dice(
                    &self.emitter,
                    &DiceEvent::MessageCreate {
                        message: MessageDto::from_wire(&message, client_nonce.clone()),
                        nonce: client_nonce,
                    },
                );
            }
            Payload::MessageUpdate(mu) => {
                let Some(message) = mu.message.clone() else {
                    return;
                };
                if let Err(error) = self.cache.apply_event(payload.clone()).await {
                    tracing::warn!(%error, "message-update cache write failed");
                }
                emit_dice(
                    &self.emitter,
                    &DiceEvent::MessageUpdate {
                        message: MessageDto::from_wire(&message, None),
                    },
                );
            }
            Payload::MessageDelete(md) => {
                if let Err(error) = self.cache.apply_event(payload.clone()).await {
                    tracing::warn!(%error, "message-delete cache write failed");
                }
                emit_dice(
                    &self.emitter,
                    &DiceEvent::MessageDelete {
                        channel_id: id_str(md.channel_id),
                        message_id: id_str(md.message_id),
                    },
                );
            }
            Payload::ReactionUpdate(ru) => {
                let me = self
                    .current_user
                    .lock()
                    .expect("user lock")
                    .as_ref()
                    .is_some_and(|u| u.id == ru.user_id);
                if let Err(error) = self
                    .cache
                    .apply_reaction_delta(ru.message_id, ru.emoji.clone(), me, ru.added)
                    .await
                {
                    tracing::warn!(%error, "reaction cache write failed");
                }
                emit_dice(
                    &self.emitter,
                    &DiceEvent::ReactionUpdate {
                        channel_id: id_str(ru.channel_id),
                        message_id: id_str(ru.message_id),
                        emoji: ru.emoji.clone(),
                        user_id: id_str(ru.user_id),
                        added: ru.added,
                    },
                );
            }
            Payload::TypingStart(typing) => {
                // Ephemeral: never cached.
                emit_dice(
                    &self.emitter,
                    &DiceEvent::TypingStart {
                        channel_id: id_str(typing.channel_id),
                        user_id: id_str(typing.user_id),
                    },
                );
            }
            Payload::PresenceUpdate(update) => {
                self.queue_presence(update.user_id, update.status);
            }
            Payload::GuildCreate(gc) => {
                if let Err(error) = self.cache.apply_event(payload.clone()).await {
                    tracing::warn!(%error, "guild cache write failed");
                }
                if let Some(guild) = &gc.guild {
                    emit_dice(
                        &self.emitter,
                        &DiceEvent::GuildCreate {
                            guild: GuildDto::from(guild),
                            channels: guild.channels.iter().map(ChannelDto::from).collect(),
                        },
                    );
                }
            }
            Payload::DmChannelCreate(dc) => {
                if let Err(error) = self.cache.apply_event(payload.clone()).await {
                    tracing::warn!(%error, "dm cache write failed");
                }
                if let Some(channel) = &dc.channel {
                    let users = self
                        .cache
                        .get_users(channel.recipient_ids.clone())
                        .await
                        .unwrap_or_default();
                    emit_dice(
                        &self.emitter,
                        &DiceEvent::DmChannelCreate {
                            channel: ChannelDto::from(channel),
                            users,
                        },
                    );
                }
            }
            Payload::UserUpdate(uu) => {
                if let Err(error) = self.cache.apply_event(payload.clone()).await {
                    tracing::warn!(%error, "user-update cache write failed");
                }
                if let Some(user) = &uu.user {
                    emit_dice(
                        &self.emitter,
                        &DiceEvent::UserUpdate {
                            user: UserDto::from(user),
                        },
                    );
                }
            }
            Payload::ReadMarkerUpdate(rm) => {
                if let Err(error) = self.cache.apply_event(payload.clone()).await {
                    tracing::warn!(%error, "read-marker cache write failed");
                }
                emit_dice(
                    &self.emitter,
                    &DiceEvent::ReadMarkerUpdate {
                        channel_id: id_str(rm.channel_id),
                        last_read_message_id: id_str(rm.last_read_message_id),
                    },
                );
            }
            // Cache-only dispatches (no dedicated frontend event in M1).
            _ => {
                if let Err(error) = self.cache.apply_event(payload).await {
                    tracing::warn!(%error, "dispatch cache write failed");
                }
            }
        }
    }

    /// Coalesce presence: update the RAM map immediately, buffer the emit,
    /// flush at most every 100 ms with only the LATEST status per user.
    fn queue_presence(&self, user_id: u64, status: i32) {
        self.presence
            .lock()
            .expect("presence lock")
            .insert(user_id, status);
        self.presence_buf
            .lock()
            .expect("presence buf lock")
            .insert(user_id, status);
        if self.flush_scheduled.swap(true, Ordering::AcqRel) {
            return; // a flush is already scheduled
        }
        let buf = Arc::clone(&self.presence_buf);
        let flag = Arc::clone(&self.flush_scheduled);
        let emitter = Arc::clone(&self.emitter);
        self.rt.spawn(async move {
            tokio::time::sleep(PRESENCE_TICK).await;
            flag.store(false, Ordering::Release);
            let drained: Vec<(u64, i32)> = {
                let mut buf = buf.lock().expect("presence buf lock");
                buf.drain().collect()
            };
            for (user_id, status) in drained {
                emit_dice(
                    &emitter,
                    &DiceEvent::PresenceUpdate {
                        user_id: id_str(user_id),
                        status: presence_str(status).to_owned(),
                    },
                );
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conn_state_maps_to_the_frontend_vocabulary() {
        use dice_network_core::client::TransportKind;
        assert_eq!(conn_state_str(ConnStateLite::Idle), "idle");
        assert_eq!(conn_state_str(ConnStateLite::Connecting), "connecting");
        assert_eq!(conn_state_str(ConnStateLite::Authenticating), "connecting");
        assert_eq!(
            conn_state_str(ConnStateLite::Ready {
                transport: TransportKind::Wss
            }),
            "connected"
        );
        assert_eq!(
            conn_state_str(ConnStateLite::Ready {
                transport: TransportKind::Quic
            }),
            "connected"
        );
        assert_eq!(conn_state_str(ConnStateLite::Backoff), "reconnecting");
        assert_eq!(conn_state_str(ConnStateLite::Failed), "offline");
    }
}
