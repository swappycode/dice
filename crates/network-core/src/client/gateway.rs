//! The gateway driver: ONE task owning the transport and the connection
//! state machine (docs/protocol.md §3–§8), commanded through a bounded mpsc
//! and observed through a bounded event mpsc + a `watch` of [`ConnState`].
//!
//! Lifecycle per connection: connect (QUIC first with WSS fallback per the
//! [`TransportPolicy`]) → `Hello` (5 s) → `Identify` (fresh session) or
//! `Resume` (when resume state exists) → `Ready`/`Resumed` → pump. On QUIC
//! the first frame is sent BEFORE awaiting `Hello` — the server cannot see
//! the control stream until data flows on it (protocol §1).
//! Backoff is full-jitter `rand(0..=min(30 s, 500 ms·2^attempt))`; the
//! attempt counter resets only after a connection stayed Ready ≥ 60 s.
//! `Error{INVALID_SESSION}` while resuming degrades to a fresh `Identify`
//! on the SAME connection (protocol §3); close codes 4010/4011/4012 are resumable;
//! 4001/4002 during the handshake call the [`TokenProvider`] once more and
//! then fail for good.

use std::sync::Arc;
use std::time::Duration;

use dice_protocol::v1::frame::Payload;
use dice_protocol::v1::{self, ErrorCode, Frame};
use rand::Rng;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tokio::time::Instant;

use super::policy::{ConnectPlan, PreferredTransport, TransportPolicy, TransportSelector};
use super::quic::{QuicEndpoint, QuicTransport};
use super::tls::TlsOptions;
use super::token::{TokenError, TokenProvider};
use super::transport::{AnyTransport, TransportKind, WssTransport};

// ------------------------------------------------------------------ tuning

/// TCP+TLS+WS handshake budget per attempt.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// `Hello` must arrive promptly; `Ready`/`Resumed` share the server's
/// standing 5 s Identify deadline (protocol §3).
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
/// Command queue depth (design doc §1.5).
const COMMAND_QUEUE: usize = 64;
/// Event queue depth; the driver `await`s sends, so a stalled consumer
/// backpressures into the socket instead of growing memory.
const EVENT_QUEUE: usize = 256;
/// A connection Ready at least this long resets the backoff attempt counter.
const HEALTHY_READY: Duration = Duration::from_secs(60);
/// Full-jitter backoff parameters.
const BACKOFF_BASE_MS: u64 = 500;
const BACKOFF_CAP_MS: u64 = 30_000;
/// User-triggered reconnects drop the socket abruptly (no Close frame) and
/// come back after a small fixed delay — long enough for the gateway to
/// notice the dead socket and open the resume window.
const FORCE_RECONNECT_DELAY: Duration = Duration::from_millis(500);
/// Two missing heartbeat acks ⇒ drop the transport and resume (protocol §4).
const MAX_MISSED_ACKS: u32 = 2;

// ------------------------------------------------------------ public types

/// Everything [`connect`] needs.
pub struct GatewayClientConfig {
    /// `wss://host:port/gateway/v1` (the fallback transport).
    pub wss_url: url::Url,
    /// QUIC gateway endpoint (UDP 8444, ALPN `dice/1`). Required for
    /// [`TransportPolicy::QuicOnly`]; optional for `QuicFirst` (absent ⇒
    /// straight to WSS); ignored by `WssOnly`.
    pub quic: Option<QuicEndpoint>,
    /// Which transport(s) to try and in what order.
    pub policy: TransportPolicy,
    /// Last-good transport from a previous session (cache meta), fed back by
    /// the host so a WSS-bound network doesn't pay the QUIC timeout again.
    pub initial_preference: Option<PreferredTransport>,
    pub tls: TlsOptions,
    /// Fresh access token per (re-)Identify.
    pub token: Arc<dyn TokenProvider>,
    /// `Identify.properties` (client/version/os).
    pub properties: v1::ClientProperties,
}

/// Connection state, published on a `watch` channel.
#[derive(Debug, Clone, PartialEq)]
pub enum ConnState {
    Idle,
    Connecting {
        attempt: u32,
    },
    /// Sent Identify or Resume, awaiting Ready/Resumed.
    Authenticating,
    Ready {
        gateway_session_id: u64,
        /// Which transport this connection runs on.
        transport: TransportKind,
    },
    Backoff {
        until_ms: u64,
        attempt: u32,
    },
    /// No auto-retry (bad credentials, broken trust config); surface to the
    /// UI. `Command::ForceReconnect` starts a fresh cycle.
    Failed {
        reason: String,
    },
}

/// Allocation-free mirror of [`ConnState`] for the event stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnStateLite {
    Idle,
    Connecting,
    Authenticating,
    Ready {
        /// Active transport, so consumers can display + persist it.
        transport: TransportKind,
    },
    Backoff,
    Failed,
}

impl From<&ConnState> for ConnStateLite {
    fn from(state: &ConnState) -> Self {
        match state {
            ConnState::Idle => Self::Idle,
            ConnState::Connecting { .. } => Self::Connecting,
            ConnState::Authenticating => Self::Authenticating,
            ConnState::Ready { transport, .. } => Self::Ready {
                transport: *transport,
            },
            ConnState::Backoff { .. } => Self::Backoff,
            ConnState::Failed { .. } => Self::Failed,
        }
    }
}

/// What the driver pushes to its single consumer (the bridge task).
#[derive(Debug)]
pub enum ClientEvent {
    /// Fresh session snapshot.
    Ready(Box<v1::Ready>),
    /// Session resumed. `replayed` is always 0 in this phase: the protocol
    /// has no end-of-replay marker, so replayed dispatches are
    /// indistinguishable from live ones — they all arrive as
    /// [`ClientEvent::Dispatch`] right after this event.
    Resumed { replayed: u32 },
    /// Resume was rejected; the caller must treat local caches as stale
    /// (HTTP re-sync). A fresh `Ready` follows on success.
    SessionInvalidated,
    /// TERMINAL: the stored credentials are dead (the server rejected the
    /// refresh token, or rejected the access token twice at the handshake).
    /// Unlike [`Self::SessionInvalidated`] there is no recovery on this
    /// connection — the host must clear the session and route to login. The
    /// driver parks in `Failed` afterwards (a fresh login spawns a new one).
    AuthExpired { reason: String },
    /// Sequenced dispatches (`seq > 0`) plus ephemeral `TypingStart`.
    Dispatch(Box<Frame>),
    /// `SendMessageAck` correlated to [`Command::SendMessage`] by nonce.
    Ack { nonce: u64, message: v1::Message },
    /// Request-scoped `Error` (nonce echoed), or a synthetic error when the
    /// connection died before the gateway replied.
    RequestError { nonce: u64, error: v1::Error },
    /// Mirror of every state transition (the `watch` carries the rich form).
    ConnState(ConnStateLite),
}

/// Outbound commands.
#[derive(Debug, Clone)]
pub enum Command {
    SendMessage {
        channel_id: u64,
        content: String,
        nonce: u64,
    },
    /// Edit (author-only) — confirmed by the broadcast MessageUpdate dispatch.
    EditMessage {
        channel_id: u64,
        message_id: u64,
        content: String,
        nonce: u64,
    },
    /// Delete — confirmed by the broadcast MessageDelete dispatch.
    DeleteMessage {
        channel_id: u64,
        message_id: u64,
        nonce: u64,
    },
    StartTyping {
        channel_id: u64,
    },
    UpdatePresence {
        status: i32,
    },
    /// Drop the transport WITHOUT a clean close and reconnect (resume state
    /// kept) — also the way out of `Failed`.
    ForceReconnect,
    /// Clean goodbye; the driver exits.
    Shutdown,
}

/// The driver task is gone (already shut down or panicked).
#[derive(Debug, thiserror::Error)]
#[error("gateway driver is gone")]
pub struct SendError;

/// Caller-side handle: command sink, event source, state watch.
pub struct GatewayHandle {
    cmds: mpsc::Sender<Command>,
    events: mpsc::Receiver<ClientEvent>,
    state: watch::Receiver<ConnState>,
    task: JoinHandle<()>,
}

impl GatewayHandle {
    pub async fn send(&self, cmd: Command) -> Result<(), SendError> {
        self.cmds.send(cmd).await.map_err(|_| SendError)
    }

    /// The bounded event stream (single consumer).
    pub fn events(&mut self) -> &mut mpsc::Receiver<ClientEvent> {
        &mut self.events
    }

    /// Watch of the rich connection state.
    pub fn state(&self) -> watch::Receiver<ConnState> {
        self.state.clone()
    }

    /// Clean shutdown: tell the driver to close and wait for it, draining
    /// events so it can never deadlock on the bounded queue while exiting.
    pub async fn shutdown(mut self) {
        let _ = self.cmds.send(Command::Shutdown).await;
        loop {
            tokio::select! {
                result = &mut self.task => {
                    if let Err(error) = result {
                        tracing::warn!(%error, "gateway driver task ended abnormally");
                    }
                    return;
                }
                event = self.events.recv() => {
                    if event.is_none() {
                        // Channel closed; only the task remains.
                        if let Err(error) = (&mut self.task).await {
                            tracing::warn!(%error, "gateway driver task ended abnormally");
                        }
                        return;
                    }
                }
            }
        }
    }
}

/// Spawn the driver on the GIVEN runtime handle (Tauri setup-hook safety:
/// never assumes an ambient runtime) and return immediately.
pub fn connect(cfg: GatewayClientConfig, rt: tokio::runtime::Handle) -> GatewayHandle {
    let (cmd_tx, cmd_rx) = mpsc::channel(COMMAND_QUEUE);
    let (event_tx, event_rx) = mpsc::channel(EVENT_QUEUE);
    let (state_tx, state_rx) = watch::channel(ConnState::Idle);
    let task = rt.spawn(async move {
        let (tls, quic) = match transport_setup(&cfg) {
            Ok(parts) => parts,
            Err(reason) => {
                let _ = state_tx.send_replace(ConnState::Failed {
                    reason: reason.clone(),
                });
                let _ = event_tx
                    .send(ClientEvent::ConnState(ConnStateLite::Failed))
                    .await;
                tracing::error!(%reason, "gateway driver could not start");
                return;
            }
        };
        let selector = TransportSelector::new(cfg.policy, quic.is_some(), cfg.initial_preference);
        let driver = Driver {
            cfg,
            tls,
            quic,
            selector,
            cmds: cmd_rx,
            events: event_tx,
            state: state_tx,
            resume: None,
            attempt: 0,
            auth_retry_used: false,
            pending: PendingSends::default(),
        };
        Box::pin(driver.run()).await;
    });
    GatewayHandle {
        cmds: cmd_tx,
        events: event_rx,
        state: state_rx,
        task,
    }
}

/// Ready-to-dial QUIC pieces, built once at driver start.
#[derive(Debug)]
struct QuicSetup {
    target: QuicEndpoint,
    config: quinn::ClientConfig,
}

/// Build the per-driver transport configs. A failure here (broken trust
/// config, quic-only without an endpoint) is unrecoverable: the driver
/// starts in `Failed`.
fn transport_setup(
    cfg: &GatewayClientConfig,
) -> Result<(Arc<rustls::ClientConfig>, Option<QuicSetup>), String> {
    let tls = cfg
        .tls
        .client_config()
        .map_err(|error| format!("tls configuration: {error}"))?;
    let quic = match (cfg.policy, &cfg.quic) {
        (TransportPolicy::WssOnly, _) | (TransportPolicy::QuicFirst { .. }, None) => None,
        (TransportPolicy::QuicOnly, None) => {
            return Err("transport policy is quic-only but no QUIC endpoint is configured".into());
        }
        (_, Some(target)) => Some(QuicSetup {
            target: target.clone(),
            config: cfg
                .tls
                .quic_client_config()
                .map_err(|error| format!("quic tls configuration: {error}"))?,
        }),
    };
    Ok((tls, quic))
}

// ------------------------------------------------------------------ driver

/// What it takes to resume the current session after a drop.
struct ResumeState {
    gateway_session_id: u64,
    resume_token: dice_protocol::bytes::Bytes,
    /// Cumulative ack cursor: highest dispatch seq actually received.
    last_seq: u64,
}

/// Nonces of in-flight `SendMessage` requests; drained into synthetic
/// [`ClientEvent::RequestError`]s when the connection dies before the reply.
#[derive(Default)]
struct PendingSends {
    nonces: Vec<u64>,
}

impl PendingSends {
    fn insert(&mut self, nonce: u64) {
        if nonce != 0 && !self.nonces.contains(&nonce) {
            self.nonces.push(nonce);
        }
    }

    /// Remove a completed nonce; `true` if it was actually pending.
    fn complete(&mut self, nonce: u64) -> bool {
        if let Some(idx) = self.nonces.iter().position(|&n| n == nonce) {
            self.nonces.remove(idx);
            true
        } else {
            false
        }
    }

    fn drain(&mut self) -> Vec<u64> {
        std::mem::take(&mut self.nonces)
    }
}

/// How one connection life ended.
enum Flow {
    /// Exit the driver.
    Shutdown,
    /// Reconnect; `None` = full-jitter backoff with attempt bump, `Some` =
    /// fixed delay without counting an attempt (user-forced reconnect, fresh
    /// auth retry).
    Retry { delay: Option<Duration> },
    /// Terminal because the credentials/session were rejected: emit
    /// [`ClientEvent::AuthExpired`] first so the host can clear them, then
    /// park until `ForceReconnect` (a fresh login spawns a new driver).
    /// (A broken TLS config is the only other terminal, and it is caught in
    /// [`connect`] before the driver loop ever starts.)
    AuthExpired(String),
}

enum FrameOutcome {
    Continue,
    Disconnect(Flow),
}

struct Driver {
    cfg: GatewayClientConfig,
    tls: Arc<rustls::ClientConfig>,
    /// Present iff a QUIC endpoint is configured and the policy may use it.
    quic: Option<QuicSetup>,
    /// The transport-selection state machine (design §1.3).
    selector: TransportSelector,
    cmds: mpsc::Receiver<Command>,
    events: mpsc::Sender<ClientEvent>,
    state: watch::Sender<ConnState>,
    resume: Option<ResumeState>,
    attempt: u32,
    /// One extra TokenProvider round per credential rejection (4001/4002 at
    /// the handshake). Cleared on Ready/Resumed.
    auth_retry_used: bool,
    pending: PendingSends,
}

impl Driver {
    async fn run(mut self) {
        loop {
            match self.connection_life().await {
                Flow::Shutdown => break,
                Flow::AuthExpired(reason) => {
                    // Tell the host FIRST (it clears credentials + routes to
                    // login), then park in Failed like any other terminal.
                    if !self
                        .emit(ClientEvent::AuthExpired {
                            reason: reason.clone(),
                        })
                        .await
                    {
                        break;
                    }
                    if !self.set_state(ConnState::Failed { reason }).await {
                        break;
                    }
                    if !self.idle_until_reconnect().await {
                        break;
                    }
                    self.attempt = 0;
                    self.auth_retry_used = false;
                }
                Flow::Retry { delay } => {
                    let delay = match delay {
                        Some(fixed) => fixed,
                        None => {
                            self.attempt += 1;
                            backoff_delay(self.attempt, &mut rand::rng())
                        }
                    };
                    let until_ms = now_ms().saturating_add(delay.as_millis() as u64);
                    if !self
                        .set_state(ConnState::Backoff {
                            until_ms,
                            attempt: self.attempt,
                        })
                        .await
                    {
                        break;
                    }
                    if !self.backoff_wait(delay).await {
                        break;
                    }
                }
            }
        }
        let _ = self.state.send_replace(ConnState::Idle);
    }

    /// One full connection attempt: connect → handshake → pump.
    async fn connection_life(&mut self) -> Flow {
        if !self
            .set_state(ConnState::Connecting {
                attempt: self.attempt,
            })
            .await
        {
            return Flow::Shutdown;
        }

        // Fetch the Identify token BEFORE connecting so a slow refresh
        // cannot eat the server's 5 s handshake deadline. The resume path
        // needs no token.
        let identify_token = if self.resume.is_none() {
            match self.cfg.token.access_token().await {
                Ok(token) => Some(token),
                Err(error) => return token_error_flow(error),
            }
        } else {
            None
        };

        let plan = self.selector.plan();
        let Some(mut transport) = self.establish(plan).await else {
            return Flow::Retry { delay: None };
        };
        let transport_kind = transport.kind();

        // Identify or Resume.
        let first = match (&self.resume, identify_token) {
            (Some(resume), _) => Frame::control(Payload::Resume(v1::Resume {
                gateway_session_id: resume.gateway_session_id,
                resume_token: resume.resume_token.clone(),
                last_seq: resume.last_seq,
            })),
            (None, Some(token)) => self.identify_frame(token),
            // Unreachable: identify_token is Some exactly when resume is None.
            (None, None) => return Flow::Retry { delay: None },
        };
        let mut resuming = self.resume.is_some();

        // QUIC: the server cannot SEE the control stream until data flows on
        // it (protocol §1) — Identify/Resume goes out immediately and Hello
        // arrives back concurrently. WSS: the server speaks first (§3).
        if transport_kind == TransportKind::Quic && transport.send(&first).await.is_err() {
            return Flow::Retry { delay: None };
        }

        // Hello within 5 s.
        let hello = loop {
            match tokio::time::timeout(HANDSHAKE_TIMEOUT, transport.recv()).await {
                Ok(Ok(Some(frame))) => match frame.payload {
                    Some(Payload::Hello(hello)) => break hello,
                    _ => continue, // pre-Hello noise: ignore
                },
                _ => return Flow::Retry { delay: None },
            }
        };

        if !self.set_state(ConnState::Authenticating).await {
            return Flow::Shutdown;
        }

        if transport_kind == TransportKind::Wss && transport.send(&first).await.is_err() {
            return Flow::Retry { delay: None };
        }

        // Await Ready / Resumed (server keeps a standing 5 s deadline).
        let session_id = loop {
            let frame = match tokio::time::timeout(HANDSHAKE_TIMEOUT, transport.recv()).await {
                Ok(Ok(Some(frame))) => frame,
                Ok(Ok(None)) => {
                    return self.handshake_close(transport.last_close_code());
                }
                _ => return Flow::Retry { delay: None },
            };
            match frame.payload {
                Some(Payload::Ready(ready)) => {
                    self.resume = Some(ResumeState {
                        gateway_session_id: ready.gateway_session_id,
                        resume_token: ready.resume_token.clone(),
                        last_seq: 0,
                    });
                    self.auth_retry_used = false;
                    let sid = ready.gateway_session_id;
                    if !self.emit(ClientEvent::Ready(Box::new(ready))).await {
                        return Flow::Shutdown;
                    }
                    break sid;
                }
                Some(Payload::Resumed(_)) => {
                    self.auth_retry_used = false;
                    let sid = self
                        .resume
                        .as_ref()
                        .map(|r| r.gateway_session_id)
                        .unwrap_or_default();
                    if !self.emit(ClientEvent::Resumed { replayed: 0 }).await {
                        return Flow::Shutdown;
                    }
                    break sid;
                }
                Some(Payload::Error(error))
                    if resuming && error.code == ErrorCode::InvalidSession as i32 =>
                {
                    // Protocol §3: the connection stays open; degrade to a
                    // fresh Identify on the SAME connection.
                    self.resume = None;
                    resuming = false;
                    if !self.emit(ClientEvent::SessionInvalidated).await {
                        return Flow::Shutdown;
                    }
                    let token = match self.cfg.token.access_token().await {
                        Ok(token) => token,
                        Err(error) => return token_error_flow(error),
                    };
                    let identify = self.identify_frame(token);
                    if transport.send(&identify).await.is_err() {
                        return Flow::Retry { delay: None };
                    }
                }
                Some(Payload::Close(close)) => {
                    return self.handshake_close(Some(close.code as u16));
                }
                Some(Payload::Error(error)) => {
                    tracing::warn!(code = error.code, message = %error.message,
                        "gateway error during handshake");
                    return Flow::Retry { delay: None };
                }
                _ => continue, // stray pre-Ready frames: ignore
            }
        };

        if !self
            .set_state(ConnState::Ready {
                gateway_session_id: session_id,
                transport: transport_kind,
            })
            .await
        {
            return Flow::Shutdown;
        }

        let ready_at = Instant::now();
        let flow = self.pump(&mut transport, &hello).await;
        drop(transport); // FIN now; pending drain below must not delay it

        // Requests the gateway never answered die with the connection.
        for nonce in self.pending.drain() {
            let lost = ClientEvent::RequestError {
                nonce,
                error: v1::Error {
                    code: ErrorCode::Unspecified as i32,
                    message: "connection lost before the gateway replied".to_owned(),
                    retry_after_ms: 0,
                },
            };
            if !self.emit(lost).await {
                return Flow::Shutdown;
            }
        }

        if ready_at.elapsed() >= HEALTHY_READY {
            self.attempt = 0; // protocol-mandated reset rule
        }
        flow
    }

    /// Connect per the selector's plan. `None` = the whole attempt failed
    /// (caller backs off). QUIC outcomes feed the selector; the WSS fallback
    /// happens INSIDE the same attempt (design §1.3) so a blocked UDP path
    /// costs at most the QUIC budget, never an extra backoff cycle.
    async fn establish(&mut self, plan: ConnectPlan) -> Option<AnyTransport> {
        let (quic_budget, fall_back) = match plan {
            ConnectPlan::Wss => return self.connect_wss().await,
            // QuicOnly: the standard connect budget applies.
            ConnectPlan::Quic => (CONNECT_TIMEOUT, false),
            ConnectPlan::QuicThenWss => match self.cfg.policy {
                TransportPolicy::QuicFirst { quic_timeout } => (quic_timeout, true),
                // Unreachable: the selector only plans QuicThenWss for
                // QuicFirst. Defensive default.
                _ => (CONNECT_TIMEOUT, true),
            },
        };
        // Clone the dial parts out so the connect future borrows no `self`.
        let Some((target, config)) = self
            .quic
            .as_ref()
            .map(|setup| (setup.target.clone(), setup.config.clone()))
        else {
            // Unreachable: QUIC plans require a configured endpoint.
            return self.connect_wss().await;
        };
        match tokio::time::timeout(quic_budget, QuicTransport::connect(&target, config)).await {
            Ok(Ok(quic)) => {
                self.selector.note_quic_success();
                return Some(AnyTransport::Quic(Box::new(quic)));
            }
            Ok(Err(error)) => tracing::debug!(%error, "quic connect failed"),
            Err(_) => tracing::debug!(
                budget_ms = quic_budget.as_millis() as u64,
                "quic connect timed out"
            ),
        }
        self.selector.note_quic_failure();
        if fall_back {
            self.connect_wss().await
        } else {
            None
        }
    }

    async fn connect_wss(&self) -> Option<AnyTransport> {
        match tokio::time::timeout(
            CONNECT_TIMEOUT,
            WssTransport::connect(&self.cfg.wss_url, Arc::clone(&self.tls)),
        )
        .await
        {
            Ok(Ok(wss)) => Some(AnyTransport::Wss(Box::new(wss))),
            Ok(Err(error)) => {
                tracing::debug!(%error, "gateway connect failed");
                None
            }
            Err(_) => {
                tracing::debug!("gateway connect timed out");
                None
            }
        }
    }

    /// Ready-state pump: commands out, frames in, heartbeats on a timer.
    async fn pump(&mut self, transport: &mut AnyTransport, hello: &v1::Hello) -> Flow {
        let interval = Duration::from_millis(u64::from(hello.heartbeat_interval_ms.max(1)));
        let mut hb_deadline = Instant::now() + first_heartbeat(interval, &mut rand::rng());
        let mut unacked: u32 = 0;
        loop {
            tokio::select! {
                biased;
                cmd = self.cmds.recv() => match cmd {
                    None | Some(Command::Shutdown) => {
                        transport.close(1000, "client shutdown").await;
                        return Flow::Shutdown;
                    }
                    Some(Command::ForceReconnect) => {
                        // Abrupt by design: no Close frame, resume state kept.
                        return Flow::Retry { delay: Some(FORCE_RECONNECT_DELAY) };
                    }
                    Some(cmd) => {
                        if let Command::SendMessage { nonce, .. } = &cmd {
                            self.pending.insert(*nonce);
                        }
                        if let Some(frame) = command_frame(&cmd)
                            && transport.send(&frame).await.is_err()
                        {
                            return Flow::Retry { delay: None };
                        }
                    }
                },
                received = transport.recv() => match received {
                    Ok(Some(frame)) => match self.handle_frame(frame, &mut unacked).await {
                        FrameOutcome::Continue => {}
                        FrameOutcome::Disconnect(flow) => return flow,
                    },
                    Ok(None) => return self.pump_close(transport.last_close_code()),
                    Err(error) => {
                        tracing::debug!(%error, "gateway transport error");
                        return Flow::Retry { delay: None };
                    }
                },
                () = tokio::time::sleep_until(hb_deadline) => {
                    if unacked >= MAX_MISSED_ACKS {
                        tracing::debug!("2 heartbeat acks missing; reconnecting to resume");
                        return Flow::Retry { delay: None };
                    }
                    let beat = Frame::control(Payload::Heartbeat(v1::Heartbeat {
                        last_seq: self.last_seq(),
                        client_time_ms: now_ms(),
                    }));
                    if transport.send(&beat).await.is_err() {
                        return Flow::Retry { delay: None };
                    }
                    unacked += 1;
                    hb_deadline = Instant::now() + interval;
                }
            }
        }
    }

    /// One inbound frame while Ready.
    async fn handle_frame(&mut self, frame: Frame, unacked: &mut u32) -> FrameOutcome {
        if frame.seq > 0 {
            // Cumulative ack cursor advances for EVERY sequenced frame, even
            // payloads this build does not know (protocol §2).
            self.note_seq(frame.seq);
        }
        let event = match &frame.payload {
            Some(Payload::HeartbeatAck(_)) => {
                *unacked = 0;
                return FrameOutcome::Continue;
            }
            Some(Payload::SendMessageAck(ack)) => {
                self.pending.complete(frame.nonce);
                ClientEvent::Ack {
                    nonce: frame.nonce,
                    message: ack.message.clone().unwrap_or_default(),
                }
            }
            Some(Payload::Error(error)) if frame.nonce != 0 => {
                self.pending.complete(frame.nonce);
                ClientEvent::RequestError {
                    nonce: frame.nonce,
                    error: error.clone(),
                }
            }
            Some(Payload::Error(error)) => {
                // Connection-scoped advisory; a Close follows if it matters.
                tracing::warn!(code = error.code, message = %error.message, "gateway error");
                return FrameOutcome::Continue;
            }
            Some(Payload::Close(close)) => {
                let flow = self.close_code_flow(close.code as u16);
                return FrameOutcome::Disconnect(flow);
            }
            // Dispatches: everything sequenced plus ephemeral typing.
            Some(Payload::TypingStart(_)) => ClientEvent::Dispatch(Box::new(frame)),
            _ if frame.seq > 0 => ClientEvent::Dispatch(Box::new(frame)),
            // Stray control frames mid-pump (Hello/Ready/...): ignore.
            _ => return FrameOutcome::Continue,
        };
        if self.emit(event).await {
            FrameOutcome::Continue
        } else {
            FrameOutcome::Disconnect(Flow::Shutdown)
        }
    }

    /// Map a close during the Identify/Resume handshake.
    fn handshake_close(&mut self, code: Option<u16>) -> Flow {
        match code {
            // UNAUTHENTICATED / INVALID_SESSION: the token was rejected.
            Some(4001 | 4002) => {
                self.resume = None;
                if self.auth_retry_used {
                    Flow::AuthExpired(
                        "gateway rejected the access token twice; fresh login required".to_owned(),
                    )
                } else {
                    // Ask the provider ONCE more (it owns refresh/rotation)
                    // and retry immediately.
                    self.auth_retry_used = true;
                    Flow::Retry {
                        delay: Some(Duration::ZERO),
                    }
                }
            }
            _ => Flow::Retry { delay: None },
        }
    }

    /// Map a close while Ready (4010/4011/4012 resumable per protocol §8).
    fn close_code_flow(&mut self, code: u16) -> Flow {
        match code {
            // Slow consumer / going away / heartbeat timeout: reconnect with
            // backoff and resume.
            4010..=4012 => Flow::Retry { delay: None },
            // Revoked mid-session: the resume token is dead; re-identify
            // with whatever the provider gives next (repeated rejection at
            // the handshake fails for good via `handshake_close`).
            4001 | 4002 => {
                self.resume = None;
                Flow::Retry { delay: None }
            }
            _ => Flow::Retry { delay: None },
        }
    }

    /// Same mapping for an EOF whose WS close code we captured.
    fn pump_close(&mut self, code: Option<u16>) -> Flow {
        match code {
            Some(code) => self.close_code_flow(code),
            None => Flow::Retry { delay: None }, // abrupt loss: resume
        }
    }

    /// Wait out a backoff delay while staying responsive to commands.
    /// Returns `false` to shut the driver down.
    async fn backoff_wait(&mut self, delay: Duration) -> bool {
        let deadline = Instant::now() + delay;
        loop {
            tokio::select! {
                () = tokio::time::sleep_until(deadline) => return true,
                cmd = self.cmds.recv() => match cmd {
                    None | Some(Command::Shutdown) => return false,
                    Some(Command::ForceReconnect) => return true, // skip the wait
                    Some(Command::SendMessage { nonce, .. }) => {
                        if !self.reject_offline(nonce).await {
                            return false;
                        }
                    }
                    Some(_) => {} // typing/presence are lossy: drop while down
                },
            }
        }
    }

    /// Failed state: only `ForceReconnect` (true) or shutdown (false) move us.
    async fn idle_until_reconnect(&mut self) -> bool {
        loop {
            match self.cmds.recv().await {
                None | Some(Command::Shutdown) => return false,
                Some(Command::ForceReconnect) => return true,
                Some(Command::SendMessage { nonce, .. }) => {
                    if !self.reject_offline(nonce).await {
                        return false;
                    }
                }
                Some(_) => {}
            }
        }
    }

    async fn reject_offline(&mut self, nonce: u64) -> bool {
        self.emit(ClientEvent::RequestError {
            nonce,
            error: v1::Error {
                code: ErrorCode::Unspecified as i32,
                message: "not connected".to_owned(),
                retry_after_ms: 0,
            },
        })
        .await
    }

    fn identify_frame(&self, access_token: String) -> Frame {
        Frame::control(Payload::Identify(v1::Identify {
            access_token,
            properties: Some(self.cfg.properties.clone()),
            capabilities: 0,
            protocol_version: dice_protocol::PROTOCOL_VERSION,
        }))
    }

    fn last_seq(&self) -> u64 {
        self.resume.as_ref().map_or(0, |r| r.last_seq)
    }

    fn note_seq(&mut self, seq: u64) {
        if let Some(resume) = self.resume.as_mut() {
            resume.last_seq = resume.last_seq.max(seq);
        }
    }

    /// Publish a state transition on the watch AND mirror it as an event.
    /// `false` = the consumer is gone; shut down.
    async fn set_state(&mut self, next: ConnState) -> bool {
        let lite = ConnStateLite::from(&next);
        let _ = self.state.send_replace(next);
        self.emit(ClientEvent::ConnState(lite)).await
    }

    /// `false` = the consumer dropped the handle; shut down.
    async fn emit(&mut self, event: ClientEvent) -> bool {
        self.events.send(event).await.is_ok()
    }
}

// ----------------------------------------------------------- pure helpers

/// Map a [`TokenError`] from the credential provider onto a connection flow:
/// a transient refresh failure backs off and retries (the credentials may
/// still be good); a terminal one (no credentials, or the server refused
/// them) becomes [`Flow::AuthExpired`] so the host clears the session.
fn token_error_flow(error: TokenError) -> Flow {
    match error {
        TokenError::Refresh(reason) => {
            tracing::debug!(%reason, "token refresh failed transiently; backing off");
            Flow::Retry { delay: None }
        }
        terminal @ (TokenError::NoCredentials | TokenError::Rejected(_)) => {
            Flow::AuthExpired(format!("access token: {terminal}"))
        }
    }
}

fn command_frame(cmd: &Command) -> Option<Frame> {
    match cmd {
        Command::SendMessage {
            channel_id,
            content,
            nonce,
        } => Some(Frame::with_nonce(
            *nonce,
            Payload::SendMessage(v1::SendMessageRequest {
                channel_id: *channel_id,
                content: content.clone(),
            }),
        )),
        Command::EditMessage {
            channel_id,
            message_id,
            content,
            nonce,
        } => Some(Frame::with_nonce(
            *nonce,
            Payload::EditMessage(v1::EditMessageRequest {
                channel_id: *channel_id,
                message_id: *message_id,
                content: content.clone(),
            }),
        )),
        Command::DeleteMessage {
            channel_id,
            message_id,
            nonce,
        } => Some(Frame::with_nonce(
            *nonce,
            Payload::DeleteMessage(v1::DeleteMessageRequest {
                channel_id: *channel_id,
                message_id: *message_id,
            }),
        )),
        Command::StartTyping { channel_id } => Some(Frame::control(Payload::StartTyping(
            v1::StartTypingRequest {
                channel_id: *channel_id,
            },
        ))),
        Command::UpdatePresence { status } => Some(Frame::control(Payload::UpdatePresence(
            v1::UpdatePresenceRequest { status: *status },
        ))),
        Command::ForceReconnect | Command::Shutdown => None,
    }
}

/// Full-jitter cap for an attempt: `min(30 s, 500 ms · 2^attempt)`.
fn backoff_cap_ms(attempt: u32) -> u64 {
    BACKOFF_BASE_MS
        .saturating_mul(1u64 << attempt.min(16))
        .min(BACKOFF_CAP_MS)
}

/// Full jitter: `rand(0..=cap)`.
fn backoff_delay(attempt: u32, rng: &mut impl Rng) -> Duration {
    Duration::from_millis(rng.random_range(0..=backoff_cap_ms(attempt)))
}

/// First heartbeat is jittered ±10% around the interval (protocol §4).
fn first_heartbeat(interval: Duration, rng: &mut impl Rng) -> Duration {
    let ms = interval.as_millis() as u64;
    let span = ms / 10;
    Duration::from_millis((ms - span) + rng.random_range(0..=span * 2))
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn backoff_caps_follow_the_formula() {
        assert_eq!(backoff_cap_ms(0), 500);
        assert_eq!(backoff_cap_ms(1), 1_000);
        assert_eq!(backoff_cap_ms(2), 2_000);
        assert_eq!(backoff_cap_ms(5), 16_000);
        assert_eq!(backoff_cap_ms(6), 30_000, "capped at 30 s");
        assert_eq!(backoff_cap_ms(60), 30_000, "no shift overflow");
    }

    #[test]
    fn backoff_delay_is_full_jitter_within_bounds() {
        let mut rng = rand::rng();
        for attempt in 0..8 {
            let cap = backoff_cap_ms(attempt);
            let mut distinct = std::collections::HashSet::new();
            for _ in 0..200 {
                let d = backoff_delay(attempt, &mut rng).as_millis() as u64;
                assert!(d <= cap, "attempt {attempt}: {d} > cap {cap}");
                distinct.insert(d);
            }
            assert!(distinct.len() > 1, "jitter must actually vary");
        }
    }

    #[test]
    fn first_heartbeat_is_within_ten_percent() {
        let mut rng = rand::rng();
        let interval = Duration::from_millis(30_000);
        for _ in 0..200 {
            let d = first_heartbeat(interval, &mut rng);
            assert!(d >= Duration::from_millis(27_000));
            assert!(d <= Duration::from_millis(33_000));
        }
    }

    /// The reset rule: the attempt counter survives short-lived Ready states
    /// and resets only after 60 s healthy (mirrors `connection_life`).
    #[test]
    fn attempt_resets_only_after_sixty_seconds_healthy() {
        let next = |attempt: u32, healthy: Duration| -> u32 {
            let attempt = if healthy >= HEALTHY_READY { 0 } else { attempt };
            attempt + 1 // Flow::Retry{delay: None} bump
        };
        assert_eq!(next(7, Duration::from_secs(59)), 8, "not yet healthy");
        assert_eq!(next(7, Duration::from_secs(60)), 1, "healthy resets");
        assert_eq!(next(0, Duration::from_secs(1)), 1);
    }

    #[test]
    fn pending_sends_correlate_by_nonce() {
        let mut pending = PendingSends::default();
        pending.insert(42);
        pending.insert(42); // dedup
        pending.insert(0); // 0 = "no nonce", never tracked
        pending.insert(7);
        assert!(pending.complete(42));
        assert!(!pending.complete(42), "already completed");
        assert!(!pending.complete(99), "never pending");
        assert_eq!(pending.drain(), vec![7]);
        assert!(pending.drain().is_empty());
    }

    #[test]
    fn conn_state_lite_mirrors_every_variant() {
        let pairs = [
            (ConnState::Idle, ConnStateLite::Idle),
            (
                ConnState::Connecting { attempt: 3 },
                ConnStateLite::Connecting,
            ),
            (ConnState::Authenticating, ConnStateLite::Authenticating),
            (
                ConnState::Ready {
                    gateway_session_id: 9,
                    transport: TransportKind::Quic,
                },
                ConnStateLite::Ready {
                    transport: TransportKind::Quic,
                },
            ),
            (
                ConnState::Ready {
                    gateway_session_id: 9,
                    transport: TransportKind::Wss,
                },
                ConnStateLite::Ready {
                    transport: TransportKind::Wss,
                },
            ),
            (
                ConnState::Backoff {
                    until_ms: 1,
                    attempt: 2,
                },
                ConnStateLite::Backoff,
            ),
            (
                ConnState::Failed { reason: "x".into() },
                ConnStateLite::Failed,
            ),
        ];
        for (rich, lite) in pairs {
            assert_eq!(ConnStateLite::from(&rich), lite);
        }
    }

    /// Close-code → flow mapping (protocol §8): 4010/4011 keep resume state,
    /// credential codes clear it, the handshake variant fails after one
    /// retried token.
    #[test]
    fn close_code_mapping() {
        let mut driver = test_driver();
        driver.resume = Some(ResumeState {
            gateway_session_id: 1,
            resume_token: dice_protocol::bytes::Bytes::from_static(&[1; 32]),
            last_seq: 5,
        });
        assert!(matches!(
            driver.close_code_flow(4010),
            Flow::Retry { delay: None }
        ));
        assert!(driver.resume.is_some(), "slow consumer is resumable");
        assert!(matches!(
            driver.close_code_flow(4011),
            Flow::Retry { delay: None }
        ));
        assert!(driver.resume.is_some(), "going away is resumable");
        assert!(matches!(
            driver.close_code_flow(4012),
            Flow::Retry { delay: None }
        ));
        assert!(driver.resume.is_some(), "heartbeat timeout is resumable");
        assert!(matches!(
            driver.close_code_flow(4001),
            Flow::Retry { delay: None }
        ));
        assert!(driver.resume.is_none(), "revocation kills resume state");

        // Handshake: first credential rejection retries once, second fails.
        assert!(matches!(
            driver.handshake_close(Some(4001)),
            Flow::Retry {
                delay: Some(Duration::ZERO)
            }
        ));
        assert!(driver.auth_retry_used);
        assert!(matches!(
            driver.handshake_close(Some(4001)),
            Flow::AuthExpired(_)
        ));
        // Non-credential closes during the handshake just back off.
        assert!(matches!(
            driver.handshake_close(Some(4011)),
            Flow::Retry { delay: None }
        ));
        assert!(matches!(
            driver.handshake_close(None),
            Flow::Retry { delay: None }
        ));
    }

    struct NoToken;
    impl TokenProvider for NoToken {
        fn access_token(
            &self,
        ) -> futures_util::future::BoxFuture<'_, Result<String, super::super::token::TokenError>>
        {
            Box::pin(async { Err(super::super::token::TokenError::NoCredentials) })
        }
    }

    fn test_driver() -> Driver {
        let (_cmd_tx, cmds) = mpsc::channel(1);
        let (events, _event_rx) = mpsc::channel(1);
        let (state, _state_rx) = watch::channel(ConnState::Idle);
        Driver {
            cfg: GatewayClientConfig {
                wss_url: url::Url::parse("wss://localhost:1/gateway/v1").unwrap(),
                quic: None,
                policy: TransportPolicy::WssOnly,
                initial_preference: None,
                tls: TlsOptions::default(),
                token: Arc::new(NoToken),
                properties: v1::ClientProperties::default(),
            },
            tls: TlsOptions::default().client_config().unwrap(),
            quic: None,
            selector: TransportSelector::new(TransportPolicy::WssOnly, false, None),
            cmds,
            events,
            state,
            resume: None,
            attempt: 0,
            auth_retry_used: false,
            pending: PendingSends::default(),
        }
    }

    /// `transport_setup` startup validation: quic-only without an endpoint
    /// is unrecoverable; quic-first without one quietly degrades to WSS.
    #[test]
    fn transport_setup_validates_the_policy() {
        let cfg = |policy: TransportPolicy, quic: Option<QuicEndpoint>| GatewayClientConfig {
            wss_url: url::Url::parse("wss://localhost:1/gateway/v1").unwrap(),
            quic,
            policy,
            initial_preference: None,
            tls: TlsOptions::default(),
            token: Arc::new(NoToken),
            properties: v1::ClientProperties::default(),
        };
        let endpoint = QuicEndpoint::from_host_port("localhost:8444").unwrap();

        let err = transport_setup(&cfg(TransportPolicy::QuicOnly, None)).unwrap_err();
        assert!(err.contains("quic-only"), "{err}");

        let (_, quic) = transport_setup(&cfg(TransportPolicy::default(), None)).unwrap();
        assert!(quic.is_none(), "quic-first without endpoint = wss only");

        let (_, quic) =
            transport_setup(&cfg(TransportPolicy::default(), Some(endpoint.clone()))).unwrap();
        assert!(quic.is_some());

        let (_, quic) = transport_setup(&cfg(TransportPolicy::WssOnly, Some(endpoint))).unwrap();
        assert!(quic.is_none(), "wss-only never dials quic");
    }

    /// Credential-provider failures split into transient (retry) vs terminal
    /// (auth-expired), the distinction Issue 1 hinges on.
    #[test]
    fn token_errors_split_transient_from_terminal() {
        assert!(matches!(
            token_error_flow(TokenError::Refresh("connection reset".into())),
            Flow::Retry { delay: None }
        ));
        assert!(matches!(
            token_error_flow(TokenError::NoCredentials),
            Flow::AuthExpired(_)
        ));
        assert!(matches!(
            token_error_flow(TokenError::Rejected("HTTP 401".into())),
            Flow::AuthExpired(_)
        ));
    }

    /// `note_seq` keeps the cumulative-ack cursor monotonic.
    #[test]
    fn seq_cursor_is_monotonic() {
        let mut driver = test_driver();
        driver.resume = Some(ResumeState {
            gateway_session_id: 1,
            resume_token: dice_protocol::bytes::Bytes::from_static(&[1; 32]),
            last_seq: 0,
        });
        driver.note_seq(3);
        driver.note_seq(1); // out-of-order replay must not regress
        driver.note_seq(7);
        assert_eq!(driver.last_seq(), 7);
    }
}
