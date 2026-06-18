//! One connection's lifecycle once the transport is up: drive Hello→Identify→
//! Ready, then hold the connection alive with app heartbeats until it drops or
//! the harness shuts down. The gateway keeps a session only as long as it sees a
//! Heartbeat within 2× the advertised interval (it closes idle sessions with
//! HEARTBEAT_TIMEOUT/4012, and QUIC idle-times-out at 90 s), so the heartbeat is
//! the ONLY thing holding 100k otherwise-silent connections open
//! (docs/protocol.md §§3-4).

use std::sync::Arc;
use std::time::Duration;

use dice_common::time::now_ms;
use dice_protocol::v1::{self, Frame, frame::Payload};
use tokio_util::sync::CancellationToken;

use crate::stats::Stats;
use crate::transport::{Rx, Tx};

/// Per-connection handshake inputs.
pub struct HandshakeParams {
    pub token: String,
    pub properties: v1::ClientProperties,
    pub capabilities: u64,
    pub handshake_timeout: Duration,
}

/// Why the hold loop returned.
enum HoldOutcome {
    /// The peer dropped the connection (close code, if any).
    Disconnected(Option<u32>),
    /// The harness asked everyone to stop.
    Shutdown,
}

/// Send Identify and read frames until Ready. Returns the heartbeat interval the
/// server advertised in Hello (or the protocol default if Hello never arrived,
/// which it always does first). Identify is sent immediately — on QUIC the
/// server can't even see the control stream until the client writes to it.
async fn handshake(tx: &mut Tx, rx: &mut Rx, p: &HandshakeParams) -> anyhow::Result<u32> {
    let identify = Frame::control(Payload::Identify(v1::Identify {
        access_token: p.token.clone(),
        properties: Some(p.properties.clone()),
        capabilities: p.capabilities,
        protocol_version: dice_protocol::PROTOCOL_VERSION,
    }));
    tx.send(&identify).await?;

    let mut heartbeat_ms = dice_protocol::HEARTBEAT_INTERVAL_MS;
    let deadline = tokio::time::sleep(p.handshake_timeout);
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            () = &mut deadline => anyhow::bail!("no Ready within handshake timeout"),
            frame = rx.recv() => match frame? {
                None => anyhow::bail!("closed during handshake (code {:?})", rx.closed_code()),
                Some(frame) => match frame.payload {
                    Some(Payload::Hello(hello)) => heartbeat_ms = hello.heartbeat_interval_ms,
                    Some(Payload::Ready(_)) => return Ok(heartbeat_ms),
                    Some(Payload::Close(_)) | Some(Payload::Error(_)) => {
                        anyhow::bail!("server rejected Identify")
                    }
                    _ => continue,
                },
            },
        }
    }
}

/// Hold the connection: heartbeat on `interval_ms`, drain inbound frames (so the
/// stream never backs up), and record heartbeat RTT from HeartbeatAcks.
async fn hold(
    tx: &mut Tx,
    rx: &mut Rx,
    interval_ms: u32,
    stats: &Stats,
    shutdown: &CancellationToken,
) -> HoldOutcome {
    let mut ticker = tokio::time::interval(Duration::from_millis(u64::from(interval_ms.max(1))));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // Consume the immediate first tick so the first beat lands ~one interval
    // after Ready (mirrors the real client jittering its first beat).
    ticker.tick().await;

    let mut last_seq: u64 = 0;
    loop {
        tokio::select! {
            _ = ticker.tick() => {
                let beat = Frame::control(Payload::Heartbeat(v1::Heartbeat {
                    last_seq,
                    client_time_ms: now_ms(),
                }));
                if tx.send(&beat).await.is_err() {
                    return HoldOutcome::Disconnected(rx.closed_code());
                }
                stats.hb_sent();
            }
            incoming = rx.recv() => match incoming {
                Ok(Some(frame)) => {
                    // Cumulative ack cursor: keep up with sequenced dispatches so
                    // a Heartbeat.last_seq correctly trims the server replay ring.
                    if frame.seq > last_seq {
                        last_seq = frame.seq;
                    }
                    if let Some(Payload::HeartbeatAck(ack)) = frame.payload {
                        stats.hb_rtt(now_ms().saturating_sub(ack.client_time_ms));
                    }
                }
                Ok(None) | Err(_) => return HoldOutcome::Disconnected(rx.closed_code()),
            },
            () = shutdown.cancelled() => return HoldOutcome::Shutdown,
        }
    }
}

/// Run one connection end to end. `transport_up` yields the already-established
/// transport halves (so this function is transport-agnostic); `on_shutdown_close`
/// performs the transport's clean close when the harness stops.
///
/// All outcomes are recorded into `stats`; errors are logged at debug to keep a
/// flood of expected mid-ramp failures from drowning the report.
pub async fn run_connection<C>(
    transport_up: anyhow::Result<(Tx, Rx)>,
    params: &HandshakeParams,
    heartbeat_override_ms: u32,
    stats: Arc<Stats>,
    shutdown: CancellationToken,
    close_on_exit: bool,
    on_shutdown_close: C,
) where
    C: FnOnce(),
{
    let (mut tx, mut rx) = match transport_up {
        Ok(halves) => halves,
        Err(err) => {
            tracing::debug!(error = %err, "connect failed");
            stats.connect_failed();
            return;
        }
    };

    let started = now_ms();
    let heartbeat_ms = match handshake(&mut tx, &mut rx, params).await {
        Ok(advertised) => {
            if heartbeat_override_ms > 0 {
                heartbeat_override_ms
            } else {
                advertised
            }
        }
        Err(err) => {
            tracing::debug!(error = %err, "handshake failed");
            stats.handshake_failed();
            return;
        }
    };
    stats.established(now_ms().saturating_sub(started));

    let outcome = hold(&mut tx, &mut rx, heartbeat_ms, &stats, &shutdown).await;
    match outcome {
        HoldOutcome::Disconnected(code) => stats.disconnected(code),
        HoldOutcome::Shutdown => {
            if close_on_exit {
                on_shutdown_close();
            }
        }
    }
    stats.ended();
}
