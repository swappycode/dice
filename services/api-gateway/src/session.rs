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
use dice_network_core::server::{FramedTransport, TransportError, TransportKind};
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
    pub(crate) replay: Box<dyn ReplayBuffer>,
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
            redirect_addr: String::new(),
        }),
    )
}

/// An `INVALID_SESSION` carrying an actionable cross-node redirect (ADR-0007
/// phase 0b): the resuming client should reconnect to `addr` — the reachable
/// address of `owner_node`, which still owns the detached session — and retry
/// Resume there. The connection stays open (protocol §3) so a client that does
/// not act on `redirect_addr` can still fall back to a fresh Identify.
pub(crate) fn redirect_frame(owner_node: u16, addr: String) -> Frame {
    Frame::with_nonce(
        0,
        Payload::Error(v1::Error {
            code: ErrorCode::InvalidSession as i32,
            message: format!("session owned by node {owner_node}; reconnect to {addr}"),
            retry_after_ms: 0,
            redirect_addr: addr,
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
            redirect_addr: String::new(),
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
    dice_metrics::counter!("dice_gateway_closes_total", "code" => code.close_code().to_string())
        .increment(1);
    let _ = transport.send(&close_payload(code, reason)).await;
    transport.close(code.close_code(), reason).await;
}

/// Stable metric label for a transport.
fn transport_label(kind: TransportKind) -> &'static str {
    match kind {
        TransportKind::Quic => "quic",
        TransportKind::Wss => "wss",
    }
}

/// Stable metric label for a frame class.
fn frame_class_label(class: FrameClass) -> &'static str {
    match class {
        FrameClass::Sequenced => "sequenced",
        FrameClass::Unsequenced => "unsequenced",
        FrameClass::Control => "control",
    }
}

/// RAII guard for `dice_gateway_connections{transport}`: a live transport is
/// counted for exactly the span this guard is held — one Ready connection.
/// Detached/resuming time (no transport) is deliberately not counted.
struct ConnGauge(&'static str);

impl ConnGauge {
    fn new(kind: TransportKind) -> Self {
        let label = transport_label(kind);
        dice_metrics::gauge!("dice_gateway_connections", "transport" => label).increment(1.0);
        Self(label)
    }
}

impl Drop for ConnGauge {
    fn drop(&mut self) {
        dice_metrics::gauge!("dice_gateway_connections", "transport" => self.0).decrement(1.0);
    }
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
            Frame(Box<Frame>),
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
                Ok(Some(frame)) => Pre::Frame(Box::new(frame)),
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
                        let session_id = resume.gateway_session_id;
                        let resume_token = resume.resume_token.clone();
                        let last_seq = resume.last_seq;
                        match gw.resume.offer(resume, transport).await {
                            // Ownership transferred to the detached task.
                            None => {
                                dice_metrics::counter!("dice_gateway_resume_total", "outcome" => "resumed").increment(1);
                                return;
                            }
                            Some(returned) => {
                                transport = returned;
                                // Local miss. If a LIVE owner (fresh lease) holds
                                // the session on another node, redirect there
                                // (ADR-0007 phase 0b). Otherwise the owner is gone:
                                // try to re-host from the durable snapshot (2b).
                                if let Ok(Some(owner)) = gw.directory.owner(session_id).await
                                    && owner.node_id != gw.node_id
                                {
                                    dice_metrics::counter!("dice_gateway_resume_total", "outcome" => "cross_node").increment(1);
                                    tracing::info!(
                                        session_id,
                                        owner_node = owner.node_id,
                                        this_node = gw.node_id,
                                        redirect = owner.addr.is_some(),
                                        "resume for a session owned by a live node; redirecting"
                                    );
                                    let invalid = match owner.addr {
                                        Some(addr) => redirect_frame(owner.node_id, addr),
                                        None => error_frame(
                                            0,
                                            ErrorCode::InvalidSession,
                                            "session lives on another node; reconnect via the sticky load balancer",
                                        ),
                                    };
                                    if transport.send(&invalid).await.is_err() {
                                        return;
                                    }
                                    continue;
                                }
                                // No live owner: re-host from the durable snapshot,
                                // or fall through to INVALID_SESSION if there's none
                                // (or its token / coverage / claim is no good).
                                transport = match try_rehost(
                                    &gw,
                                    transport,
                                    session_id,
                                    &resume_token,
                                    last_seq,
                                )
                                .await
                                {
                                    Ok(()) => return, // re-host took over + ran the session
                                    Err(returned) => returned,
                                };
                                dice_metrics::counter!("dice_gateway_resume_total", "outcome" => "gone").increment(1);
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
        // Register this connection for voice datagram fan-out (QUIC only); the
        // guard unregisters it + stops its read pump when the connection ends.
        let _voice_attach = gw.voice_dg.attach(st.user, transport.quic_connection());
        // Count this live transport for dice_gateway_connections{transport};
        // dropped on detach below and on every exit from the loop.
        let _conn = ConnGauge::new(transport.kind());
        match ready_loop(&gw, &mut st, &mut *transport).await {
            LoopEnd::Shutdown => return,
            LoopEnd::CleanClose | LoopEnd::Fatal => {
                cleanup(&gw, &st).await;
                return;
            }
            LoopEnd::Detach => {
                drop(_voice_attach); // unregister the dead connection promptly
                drop(_conn); // stop counting this transport while detached
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
                dice_metrics::counter!(
                    "dice_gateway_frames_total",
                    "dir" => "out",
                    "class" => frame_class_label(frame.class())
                )
                .increment(1);
            }
            Wake::Out(None) => {
                // All router senders dropped: only possible after unregister,
                // which never happens while we are live — defensive.
                close_with(transport, ErrorCode::Internal, "session routing lost").await;
                return LoopEnd::Fatal;
            }
            Wake::In(Ok(Some(frame))) => {
                dice_metrics::counter!(
                    "dice_gateway_frames_total",
                    "dir" => "in",
                    "class" => frame_class_label(frame.class())
                )
                .increment(1);
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
    // Publish a durable snapshot + an ownership LEASE so a reconnect that lands
    // on another node can be routed back to a LIVE owner (phase 0/0b) or, if this
    // owner dies, re-host the session from the snapshot (phase 2b). The lease is
    // short (refreshed below) so a dead owner's entry expires within the window;
    // the snapshot lives for the whole window. Both best-effort: a write failure
    // just degrades to the local INVALID_SESSION path.
    refresh_lease(gw, st.session_id).await;
    persist_snapshot(gw, st).await;
    let expires = Instant::now() + gw.resume_window;
    // Refresh the lease (and snapshot) every ⅛ window with a ¼-window TTL — 2×
    // headroom so a live owner's lease never lapses between refreshes, while a
    // dead owner's lease expires within ~⅛ window past its last refresh (the
    // snapshot, on a full-window TTL, outlives it, so re-host still replays).
    // Skip the immediate first tick since we just wrote both above.
    let refresh_period = (gw.resume_window / 8).max(Duration::from_millis(1));
    let mut refresh = tokio::time::interval_at(Instant::now() + refresh_period, refresh_period);

    let taken = loop {
        enum Wake {
            Gone,
            Killed(ErrorCode),
            Out(Option<Dispatch>),
            Offer(Option<ResumeOffer>),
            Refresh,
        }
        let wake = tokio::select! {
            biased;
            () = gw.ct.cancelled() => Wake::Gone,
            (code, _) = st.kill.triggered() => Wake::Killed(code),
            () = tokio::time::sleep_until(expires) => Wake::Gone,
            _ = refresh.tick() => Wake::Refresh,
            queued = st.outbound.recv() => Wake::Out(queued),
            offer = offers.recv() => Wake::Offer(offer),
        };
        match wake {
            Wake::Gone => break None,
            // Refresh the ownership lease + the snapshot so a dead owner is
            // detected within the window and re-host replays a fresh ring.
            Wake::Refresh => {
                refresh_lease(gw, st.session_id).await;
                persist_snapshot(gw, st).await;
            }
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
    if let Err(error) = gw.directory.clear(st.session_id).await {
        tracing::debug!(%error, session_id = st.session_id, "resume directory clear failed");
    }
    if let Err(error) = gw.durable.clear(st.session_id).await {
        tracing::debug!(%error, session_id = st.session_id, "durable snapshot clear failed");
    }
    taken
}

/// Refresh this node's ownership lease for `session_id` (cross-node resume): a
/// short TTL (¼ the window, re-recorded every ⅛ window by [`detached_wait`]) so
/// a LIVE owner keeps it fresh (a reconnect elsewhere redirects here, phase 0b)
/// while a DEAD owner's lease lapses within the window (a reconnect elsewhere
/// then re-hosts from the snapshot, phase 2b). The ¼-window TTL bounds the brief
/// tail where a reconnect could still redirect to a just-dead owner before the
/// lease expires (it then retries and re-hosts).
async fn refresh_lease(gw: &Gateway, session_id: u64) {
    if let Err(error) = gw
        .directory
        .record(
            session_id,
            gw.node_id,
            gw.advertised_addr.as_deref(),
            gw.resume_window / 4,
        )
        .await
    {
        tracing::debug!(%error, session_id, "resume lease refresh failed");
    }
}

/// Persist the detached session's durable snapshot (identity + next seq + ring),
/// expiring at the resume window, so another node can re-host it (phase 2b).
/// Seq-safe even though it lags the live ring: a detached client receives
/// nothing, so any seqs the origin assigns AFTER this snapshot were never
/// client-visible — a re-host continuing from the snapshot's `next_seq` cannot
/// regress the client (the gap is recovered via REST backfill).
async fn persist_snapshot(gw: &Gateway, st: &SessionState) {
    if let Err(error) = gw
        .durable
        .save(st.session_id, &snapshot_of(st), gw.resume_window)
        .await
    {
        tracing::debug!(%error, session_id = st.session_id, "durable snapshot save failed");
    }
}

/// Capture the session's current durable resume state from its single-writer
/// task (so the ring + `next_seq` are consistent).
fn snapshot_of(st: &SessionState) -> crate::durable::ResumeSnapshot {
    crate::durable::ResumeSnapshot {
        user: st.user.raw(),
        auth_session: st.auth_session.raw(),
        resume_token: st.resume_token,
        next_seq: st.next_seq,
        trimmed_to: st.replay.trimmed_to(),
        frames: st.replay.iter().cloned().collect(),
    }
}

/// `Resumed` + replay of everything still buffered (original seqs).
pub(crate) async fn resume_replay(st: &mut SessionState, transport: &mut dyn FramedTransport) {
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

/// Cross-node re-host (ADR-0007 phase 2b): the owner is gone, so try to revive
/// the session FROM the durable snapshot on this node. Validates the resume
/// token (constant time) + ring coverage (same invariants as `offer_valid`),
/// then wins a single-takeover claim so two nodes never re-host the same
/// session, and hands off to [`handshake::rehost`] (which runs the session to
/// completion). Returns the transport back — `Err(transport)` — when re-host is
/// declined (no snapshot, bad token, coverage miss, or claim lost) so the caller
/// sends `INVALID_SESSION` and keeps the connection open.
async fn try_rehost(
    gw: &Arc<Gateway>,
    transport: Box<dyn FramedTransport>,
    session_id: u64,
    resume_token: &[u8],
    last_seq: u64,
) -> Result<(), Box<dyn FramedTransport>> {
    let snapshot = match gw.durable.load(session_id).await {
        Ok(Some(snapshot)) => snapshot,
        Ok(None) => return Err(transport),
        Err(error) => {
            tracing::debug!(%error, session_id, "re-host: snapshot load failed");
            return Err(transport);
        }
    };
    // Same gate as a same-node resume: constant-time token match + the ring must
    // still cover everything after the client's last_seq.
    let token_ok = resume_token.len() == RESUME_TOKEN_LEN
        && bool::from(resume_token.ct_eq(snapshot.resume_token.as_slice()));
    if !token_ok || last_seq < snapshot.trimmed_to {
        return Err(transport);
    }
    // Single-takeover fence: exactly one node may re-host (seq monotonicity). A
    // SHORT claim TTL covers the re-host setup race then auto-expires, so a later
    // legitimate re-host of the same session is never blocked by a stale claim.
    let claim_ttl = gw.resume_window / 4;
    match gw.durable.try_claim(session_id, claim_ttl).await {
        Ok(true) => {}
        Ok(false) => return Err(transport), // another node won the claim
        Err(error) => {
            tracing::debug!(%error, session_id, "re-host: claim failed");
            return Err(transport);
        }
    }
    // The load→claim gap is not atomic: a concurrent owner exit could have
    // `clear()`ed the session (deleting snapshot + claim, which our `incr`
    // resurrected as a fresh winner). Re-verify the snapshot survived; if it is
    // gone, release the claim and decline rather than re-host a torn-down session.
    if !matches!(gw.durable.load(session_id).await, Ok(Some(_))) {
        let _ = gw.durable.release_claim(session_id).await;
        return Err(transport);
    }
    dice_metrics::counter!("dice_gateway_resume_total", "outcome" => "rehosted").increment(1);
    handshake::rehost(Arc::clone(gw), transport, session_id, snapshot, last_seq).await;
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::resume::LocalReplayBuffer;
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
            replay: Box::new(LocalReplayBuffer::new()),
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
