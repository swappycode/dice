//! Per-connection session driver: AwaitingIdentify(5 s) → Ready →
//! Detached(resume window) → Closed (docs/protocol.md §3–§6).
//!
//! One task owns the transport AND the session state for its whole life.
//! Producers (router fan-out) reach it through a bounded mpsc(128) of
//! ready-to-send dispatch frames; seq assignment and replay buffering happen
//! HERE, at delivery time, so the counter and ring have a single writer.

use std::sync::Arc;
use std::time::Duration;

use dice_common::id::{SessionId, UserId};
use dice_network_core::server::{FramedTransport, TransportError};
use dice_protocol::framing::FrameError;
use dice_protocol::v1::frame::Payload;
use dice_protocol::v1::{self, ErrorCode, Frame};
use dice_protocol::{FrameClass, MAX_FRAME_BYTES};
use parking_lot::Mutex;
use subtle::ConstantTimeEq as _;
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TrySendError;
use tokio::time::Instant;

use crate::dispatch::TokenBucket;
use crate::resume::{RESUME_TOKEN_LEN, ReplayBuffer, ResumeOffer, ResumeReply};
use crate::{Gateway, dispatch, handshake};

/// Per-session outbound queue depth (protocol §8: bounded 128; overflow ⇒
/// `Close{SLOW_CONSUMER}`).
pub(crate) const OUTBOUND_QUEUE: usize = 128;

/// Standing Identify deadline from `Hello` (protocol §3).
pub(crate) const IDENTIFY_DEADLINE: Duration = Duration::from_secs(5);

/// One fan-out unit: a dispatch frame (seq still 0) + whether it must skip
/// the replay buffer (`BusEvent.ephemeral` / typing).
pub(crate) struct Dispatch {
    pub(crate) frame: Frame,
    pub(crate) ephemeral: bool,
}

/// Level-style close signal from outside the session task (router overflow,
/// session revocation). Consumable: a SLOW_CONSUMER signal taken before a
/// detach does not poison the resumed life of the session, while a later
/// revocation still fires.
#[derive(Clone)]
pub(crate) struct KillSwitch {
    inner: Arc<KillInner>,
}

struct KillInner {
    notify: tokio::sync::Notify,
    slot: Mutex<Option<(ErrorCode, &'static str)>>,
}

impl KillSwitch {
    pub(crate) fn new() -> Self {
        Self {
            inner: Arc::new(KillInner {
                notify: tokio::sync::Notify::new(),
                slot: Mutex::new(None),
            }),
        }
    }

    /// Request a close. A pending UNAUTHENTICATED (revocation) is never
    /// downgraded by a racing SLOW_CONSUMER.
    pub(crate) fn force_close(&self, code: ErrorCode, reason: &'static str) {
        {
            let mut slot = self.inner.slot.lock();
            match *slot {
                Some((ErrorCode::Unauthenticated, _)) => {}
                _ => *slot = Some((code, reason)),
            }
        }
        self.inner.notify.notify_one();
    }

    /// Wait for (and consume) the next close signal.
    pub(crate) async fn triggered(&self) -> (ErrorCode, &'static str) {
        loop {
            let notified = self.inner.notify.notified();
            if let Some(signal) = self.inner.slot.lock().take() {
                return signal;
            }
            notified.await;
        }
    }
}

/// The router-side handle to a live (or detached) session.
#[derive(Clone)]
pub(crate) struct SessionSender {
    pub(crate) session_id: u64,
    pub(crate) user: UserId,
    pub(crate) auth_session: SessionId,
    tx: mpsc::Sender<Dispatch>,
    kill: KillSwitch,
}

impl SessionSender {
    pub(crate) fn new(
        session_id: u64,
        user: UserId,
        auth_session: SessionId,
        tx: mpsc::Sender<Dispatch>,
        kill: KillSwitch,
    ) -> Self {
        Self {
            session_id,
            user,
            auth_session,
            tx,
            kill,
        }
    }

    /// Fan one frame out to this session. Never blocks: a full queue is a
    /// slow consumer and the session is closed (4010) instead of buffering.
    pub(crate) fn dispatch(&self, frame: Frame, bus_ephemeral: bool) {
        let ephemeral = bus_ephemeral || frame.class() != FrameClass::Sequenced;
        match self.tx.try_send(Dispatch { frame, ephemeral }) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                self.kill
                    .force_close(ErrorCode::SlowConsumer, "outbound queue overflow");
            }
            Err(TrySendError::Closed(_)) => {} // session already gone
        }
    }

    pub(crate) fn force_close(&self, code: ErrorCode, reason: &'static str) {
        self.kill.force_close(code, reason);
    }
}

/// Everything a Ready session owns (single writer: the session task).
pub(crate) struct SessionState {
    pub(crate) user: UserId,
    pub(crate) auth_session: SessionId,
    /// `gateway_session_id` (raw snowflake), minted at Identify.
    pub(crate) session_id: u64,
    pub(crate) resume_token: [u8; RESUME_TOKEN_LEN],
    pub(crate) outbound: mpsc::Receiver<Dispatch>,
    pub(crate) kill: KillSwitch,
    /// Next seq to assign (starts at 1; protocol §6).
    pub(crate) next_seq: u64,
    pub(crate) replay: ReplayBuffer,
    pub(crate) last_heartbeat: Instant,
    pub(crate) bucket: TokenBucket,
}

impl SessionState {
    pub(crate) fn gateway_session(&self) -> SessionId {
        SessionId::from_raw(self.session_id)
    }

    /// Assign this session's seq (sequenced class A only) and insert into the
    /// replay ring. Ephemeral/typing frames go out with seq=0, unbuffered.
    pub(crate) fn prepare(&mut self, dispatch: Dispatch) -> Frame {
        let mut frame = dispatch.frame;
        if !dispatch.ephemeral && frame.class() == FrameClass::Sequenced {
            frame.seq = self.next_seq;
            self.next_seq += 1;
            self.replay.push(frame.clone());
        } else {
            frame.seq = 0;
        }
        frame
    }
}

// ---------------------------------------------------------------- helpers

pub(crate) fn error_frame(nonce: u64, code: ErrorCode, message: &str) -> Frame {
    Frame::with_nonce(
        nonce,
        Payload::Error(v1::Error {
            code: code as i32,
            message: message.to_owned(),
            retry_after_ms: 0,
        }),
    )
}

pub(crate) fn rate_limited_frame(nonce: u64, retry_after_ms: u32) -> Frame {
    Frame::with_nonce(
        nonce,
        Payload::Error(v1::Error {
            code: ErrorCode::RateLimited as i32,
            message: "rate limited".to_owned(),
            retry_after_ms,
        }),
    )
}

fn close_payload(code: ErrorCode, reason: &str) -> Frame {
    Frame::control(Payload::Close(v1::Close {
        code: code.close_code(),
        reason: reason.to_owned(),
    }))
}

/// Best-effort `Close{code}` frame + transport close (4000 + code).
pub(crate) async fn close_with(transport: &mut dyn FramedTransport, code: ErrorCode, reason: &str) {
    let _ = transport.send(&close_payload(code, reason)).await;
    transport.close(code.close_code(), reason).await;
}

// ------------------------------------------------------- connection entry

/// How one Ready life of a session ended.
pub(crate) enum LoopEnd {
    /// Peer closed cleanly (transport EOF / WS close / client Close frame):
    /// no resume window.
    CleanClose,
    /// Unrecoverable (auth revoked, protocol violation): no resume window.
    Fatal,
    /// Abrupt loss / heartbeat timeout / slow consumer: enter the resume
    /// window.
    Detach,
    /// Process shutdown: just stop (presence heals via TTL).
    Shutdown,
}

/// Entry point shared by QUIC accepts and WS upgrades.
pub(crate) async fn drive_connection(gw: Arc<Gateway>, mut transport: Box<dyn FramedTransport>) {
    let hello = Frame::control(Payload::Hello(v1::Hello {
        heartbeat_interval_ms: gw.heartbeat_interval.as_millis() as u32,
        resume_window_ms: gw.resume_window.as_millis() as u32,
        max_frame_bytes: MAX_FRAME_BYTES as u32,
    }));
    if transport.send(&hello).await.is_err() {
        return;
    }

    // Standing 5 s deadline: failed Resume attempts do NOT extend it.
    let deadline = Instant::now() + IDENTIFY_DEADLINE;
    loop {
        enum Pre {
            Frame(Frame),
            CleanClose,
            Error(TransportError),
            Timeout,
            Shutdown,
        }
        let event = tokio::select! {
            biased;
            () = gw.ct.cancelled() => Pre::Shutdown,
            () = tokio::time::sleep_until(deadline) => Pre::Timeout,
            received = transport.recv() => match received {
                Ok(Some(frame)) => Pre::Frame(frame),
                Ok(None) => Pre::CleanClose,
                Err(error) => Pre::Error(error),
            },
        };
        match event {
            Pre::Shutdown => {
                close_with(
                    &mut *transport,
                    ErrorCode::GoingAway,
                    "server shutting down",
                )
                .await;
                return;
            }
            Pre::Timeout => {
                close_with(
                    &mut *transport,
                    ErrorCode::Unauthenticated,
                    "no Identify within 5 s",
                )
                .await;
                return;
            }
            Pre::CleanClose => return,
            Pre::Error(error) => {
                let _ = close_on_transport_error(&mut *transport, error).await;
                return;
            }
            Pre::Frame(frame) => {
                let nonce = frame.nonce;
                match frame.payload {
                    Some(Payload::Identify(identify)) => {
                        handshake::identify(gw, transport, identify).await;
                        return;
                    }
                    Some(Payload::Resume(resume)) => {
                        match gw.resume.offer(resume, transport).await {
                            // Ownership transferred to the detached task.
                            None => return,
                            Some(returned) => {
                                transport = returned;
                                let invalid = error_frame(
                                    0,
                                    ErrorCode::InvalidSession,
                                    "cannot resume; send a fresh Identify",
                                );
                                if transport.send(&invalid).await.is_err() {
                                    return;
                                }
                                // Connection stays open (protocol §3).
                            }
                        }
                    }
                    None => {
                        if nonce != 0 {
                            let err =
                                error_frame(nonce, ErrorCode::UnknownOpcode, "unknown payload");
                            if transport.send(&err).await.is_err() {
                                return;
                            }
                        }
                    }
                    Some(_) => {
                        if nonce != 0 {
                            let err = error_frame(
                                nonce,
                                ErrorCode::UnknownOpcode,
                                "expected Identify or Resume",
                            );
                            if transport.send(&err).await.is_err() {
                                return;
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Map a transport error to the right close (protocol §1: oversize ⇒
/// PAYLOAD_TOO_LARGE; undecodable/text ⇒ protocol-error close; anything else
/// is an abrupt network loss ⇒ resumable).
async fn close_on_transport_error(
    transport: &mut dyn FramedTransport,
    error: TransportError,
) -> LoopEnd {
    match error {
        TransportError::Frame(FrameError::TooLarge(_)) => {
            close_with(
                transport,
                ErrorCode::PayloadTooLarge,
                "frame exceeds MAX_FRAME_BYTES",
            )
            .await;
            LoopEnd::Fatal
        }
        TransportError::Frame(_) => {
            close_with(transport, ErrorCode::InvalidArgument, "protocol error").await;
            LoopEnd::Fatal
        }
        _ => LoopEnd::Detach,
    }
}

// ------------------------------------------------------------- Ready life

/// Run the session from Ready until it is gone for good, cycling through
/// detach → resume as needed.
pub(crate) async fn run_ready(
    gw: Arc<Gateway>,
    mut st: SessionState,
    mut transport: Box<dyn FramedTransport>,
) {
    loop {
        match ready_loop(&gw, &mut st, &mut *transport).await {
            LoopEnd::Shutdown => return,
            LoopEnd::CleanClose | LoopEnd::Fatal => {
                cleanup(&gw, &st).await;
                return;
            }
            LoopEnd::Detach => {
                drop(transport);
                match detached_wait(&gw, &mut st).await {
                    Some(fresh) => {
                        transport = fresh;
                        st.last_heartbeat = Instant::now();
                        // Best effort: if the new transport dies mid-replay
                        // the next ready_loop recv errors and we re-detach
                        // (frames stay buffered until acked).
                        resume_replay(&mut st, &mut *transport).await;
                    }
                    None => {
                        cleanup(&gw, &st).await;
                        return;
                    }
                }
            }
        }
    }
}

async fn cleanup(gw: &Gateway, st: &SessionState) {
    gw.router.unregister_session(st.session_id);
    if let Err(error) = gw
        .deps
        .presence
        .disconnect(st.user, st.gateway_session())
        .await
    {
        tracing::debug!(%error, user = %st.user, "presence disconnect failed");
    }
    // Drop the user from any voice channel (mirrors presence: explicit
    // leave-on-teardown is how voice membership stays live).
    if let Err(error) = gw.deps.voice.disconnect(st.user).await {
        tracing::debug!(%error, user = %st.user, "voice disconnect failed");
    }
}

async fn ready_loop(
    gw: &Gateway,
    st: &mut SessionState,
    transport: &mut dyn FramedTransport,
) -> LoopEnd {
    loop {
        let hb_deadline = st.last_heartbeat + gw.heartbeat_interval * 2;
        enum Wake {
            Shutdown,
            Killed((ErrorCode, &'static str)),
            Out(Option<Dispatch>),
            In(Result<Option<Frame>, TransportError>),
            HbTimeout,
        }
        let wake = tokio::select! {
            biased;
            () = gw.ct.cancelled() => Wake::Shutdown,
            signal = st.kill.triggered() => Wake::Killed(signal),
            queued = st.outbound.recv() => Wake::Out(queued),
            received = transport.recv() => Wake::In(received),
            () = tokio::time::sleep_until(hb_deadline) => Wake::HbTimeout,
        };
        match wake {
            Wake::Shutdown => {
                close_with(transport, ErrorCode::GoingAway, "server shutting down").await;
                return LoopEnd::Shutdown;
            }
            Wake::Killed((code, reason)) => {
                close_with(transport, code, reason).await;
                return if code == ErrorCode::SlowConsumer {
                    LoopEnd::Detach // 4010 is resumable (protocol §8)
                } else {
                    LoopEnd::Fatal
                };
            }
            Wake::Out(Some(dispatch)) => {
                let frame = st.prepare(dispatch);
                if transport.send(&frame).await.is_err() {
                    return LoopEnd::Detach; // sequenced frames stay buffered
                }
            }
            Wake::Out(None) => {
                // All router senders dropped: only possible after unregister,
                // which never happens while we are live — defensive.
                close_with(transport, ErrorCode::Internal, "session routing lost").await;
                return LoopEnd::Fatal;
            }
            Wake::In(Ok(Some(frame))) => {
                if let Some(end) = dispatch::handle(gw, st, transport, frame).await {
                    return end;
                }
            }
            Wake::In(Ok(None)) => return LoopEnd::CleanClose,
            Wake::In(Err(error)) => return close_on_transport_error(transport, error).await,
            Wake::HbTimeout => {
                // 2 missed beats. Dedicated HEARTBEAT_TIMEOUT (4012) — distinct
                // from GOING_AWAY (server shutdown) for observability; the
                // client treats it as resumable (protocol §8) and reconnects.
                close_with(transport, ErrorCode::HeartbeatTimeout, "heartbeat timeout").await;
                return LoopEnd::Detach;
            }
        }
    }
}

// ------------------------------------------------------------ detached

fn offer_valid(st: &SessionState, offer: &ResumeOffer) -> bool {
    let token_ok = offer.token.len() == RESUME_TOKEN_LEN
        && bool::from(offer.token.as_ref().ct_eq(&st.resume_token));
    // The client cannot have seen seqs we never assigned, and the buffer must
    // still cover everything after last_seq.
    token_ok && offer.last_seq < st.next_seq && st.replay.covers(offer.last_seq)
}

/// Detached state: keep draining fan-out into the replay ring, wait for a
/// valid Resume offer or the window to expire. Returns the new transport on
/// successful re-attach.
async fn detached_wait(gw: &Gateway, st: &mut SessionState) -> Option<Box<dyn FramedTransport>> {
    let (offer_tx, mut offers) = mpsc::channel::<ResumeOffer>(4);
    gw.resume.insert(st.session_id, offer_tx);
    let expires = Instant::now() + gw.resume_window;

    let taken = loop {
        enum Wake {
            Gone,
            Killed(ErrorCode),
            Out(Option<Dispatch>),
            Offer(Option<ResumeOffer>),
        }
        let wake = tokio::select! {
            biased;
            () = gw.ct.cancelled() => Wake::Gone,
            (code, _) = st.kill.triggered() => Wake::Killed(code),
            () = tokio::time::sleep_until(expires) => Wake::Gone,
            queued = st.outbound.recv() => Wake::Out(queued),
            offer = offers.recv() => Wake::Offer(offer),
        };
        match wake {
            Wake::Gone => break None,
            // Already detached: further slow-consumer signals are stale.
            Wake::Killed(ErrorCode::SlowConsumer) => {}
            Wake::Killed(_) => break None, // revoked while detached
            Wake::Out(Some(dispatch)) => {
                // Assign seq + buffer (ephemeral frames are simply dropped).
                let _ = st.prepare(dispatch);
            }
            Wake::Out(None) => break None,
            Wake::Offer(None) => break None,
            Wake::Offer(Some(offer)) => {
                if offer_valid(st, &offer) {
                    let ResumeOffer {
                        last_seq,
                        transport,
                        reply,
                        ..
                    } = offer;
                    st.replay.ack(last_seq);
                    let _ = reply.send(ResumeReply::Accepted);
                    break Some(transport);
                }
                let ResumeOffer {
                    transport, reply, ..
                } = offer;
                let _ = reply.send(ResumeReply::Rejected(transport));
            }
        }
    };
    gw.resume.remove(st.session_id);
    taken
}

/// `Resumed` + replay of everything still buffered (original seqs).
async fn resume_replay(st: &mut SessionState, transport: &mut dyn FramedTransport) {
    let resumed = Frame::control(Payload::Resumed(v1::Resumed {}));
    if transport.send(&resumed).await.is_err() {
        return;
    }
    for frame in st.replay.iter() {
        if transport.send(frame).await.is_err() {
            return;
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use dice_protocol::v1::frame::Payload;

    fn state() -> SessionState {
        let (_tx, rx) = mpsc::channel(OUTBOUND_QUEUE);
        SessionState {
            user: UserId::from_raw(1),
            auth_session: SessionId::from_raw(2),
            session_id: 3,
            resume_token: [7u8; RESUME_TOKEN_LEN],
            outbound: rx,
            kill: KillSwitch::new(),
            next_seq: 1,
            replay: ReplayBuffer::new(),
            last_heartbeat: Instant::now(),
            bucket: TokenBucket::default(),
        }
    }

    fn message_dispatch(ephemeral: bool) -> Dispatch {
        Dispatch {
            frame: Frame::dispatch(Payload::MessageCreate(v1::MessageCreate::default())),
            ephemeral,
        }
    }

    #[tokio::test]
    async fn prepare_assigns_monotonic_seq_and_buffers() {
        let mut st = state();
        let f1 = st.prepare(message_dispatch(false));
        let f2 = st.prepare(message_dispatch(false));
        assert_eq!(f1.seq, 1);
        assert_eq!(f2.seq, 2);
        assert_eq!(st.next_seq, 3);
        assert_eq!(st.replay.iter().count(), 2);
    }

    #[tokio::test]
    async fn prepare_skips_seq_and_buffer_for_typing_and_ephemeral() {
        let mut st = state();
        let typing = Dispatch {
            frame: Frame::dispatch(Payload::TypingStart(v1::TypingStart {
                channel_id: 1,
                user_id: 2,
            })),
            ephemeral: false, // class B even without the bus flag
        };
        assert_eq!(st.prepare(typing).seq, 0);
        // Sequenced payload but flagged ephemeral on the bus: still seq 0.
        assert_eq!(st.prepare(message_dispatch(true)).seq, 0);
        assert_eq!(st.next_seq, 1);
        assert_eq!(st.replay.iter().count(), 0);
    }

    #[tokio::test]
    async fn kill_switch_is_consumable_and_revocation_wins() {
        let kill = KillSwitch::new();
        kill.force_close(ErrorCode::SlowConsumer, "overflow");
        assert_eq!(kill.triggered().await.0, ErrorCode::SlowConsumer);
        // Consumed: a later revocation still fires.
        kill.force_close(ErrorCode::Unauthenticated, "revoked");
        // SLOW_CONSUMER must not downgrade a pending revocation.
        kill.force_close(ErrorCode::SlowConsumer, "overflow");
        assert_eq!(kill.triggered().await.0, ErrorCode::Unauthenticated);
    }

    #[tokio::test]
    async fn offer_validation_token_and_coverage() {
        let mut st = state();
        for _ in 0..3 {
            let _ = st.prepare(message_dispatch(false)); // seqs 1..=3
        }
        let (reply, _rx) = tokio::sync::oneshot::channel();
        let offer = ResumeOffer {
            token: bytes::Bytes::copy_from_slice(&[7u8; RESUME_TOKEN_LEN]),
            last_seq: 1,
            transport: Box::new(crate::ws::tests::DummyTransport),
            reply,
        };
        assert!(offer_valid(&st, &offer));

        // Wrong token.
        let (reply, _rx) = tokio::sync::oneshot::channel();
        let bad_token = ResumeOffer {
            token: bytes::Bytes::copy_from_slice(&[8u8; RESUME_TOKEN_LEN]),
            last_seq: 1,
            transport: Box::new(crate::ws::tests::DummyTransport),
            reply,
        };
        assert!(!offer_valid(&st, &bad_token));

        // Client claims a future seq.
        let (reply, _rx) = tokio::sync::oneshot::channel();
        let ahead = ResumeOffer {
            token: bytes::Bytes::copy_from_slice(&[7u8; RESUME_TOKEN_LEN]),
            last_seq: 99,
            transport: Box::new(crate::ws::tests::DummyTransport),
            reply,
        };
        assert!(!offer_valid(&st, &ahead));

        // Buffer no longer covers last_seq (acked past it).
        st.replay.ack(2);
        let (reply, _rx) = tokio::sync::oneshot::channel();
        let gap = ResumeOffer {
            token: bytes::Bytes::copy_from_slice(&[7u8; RESUME_TOKEN_LEN]),
            last_seq: 1,
            transport: Box::new(crate::ws::tests::DummyTransport),
            reply,
        };
        assert!(!offer_valid(&st, &gap));
    }
}
