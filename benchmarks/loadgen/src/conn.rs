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

/// Send Identify and read frames until Ready. Returns `Some(heartbeat_interval)`
/// the server advertised in Hello (or the protocol default if Hello never
/// arrived, which it always does first), or `None` if the harness shut down
/// mid-handshake (a clean abort — the connection never established). Identify is
/// sent immediately — on QUIC the server can't even see the control stream until
/// the client writes to it.
async fn handshake(
    tx: &mut Tx,
    rx: &mut Rx,
    p: &HandshakeParams,
    shutdown: &CancellationToken,
) -> anyhow::Result<Option<u32>> {
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
            () = shutdown.cancelled() => return Ok(None),
            () = &mut deadline => anyhow::bail!("no Ready within handshake timeout"),
            frame = rx.recv() => match frame? {
                None => anyhow::bail!("closed during handshake (code {:?})", rx.closed_code()),
                Some(frame) => match frame.payload {
                    Some(Payload::Hello(hello)) => heartbeat_ms = hello.heartbeat_interval_ms,
                    Some(Payload::Ready(_)) => return Ok(Some(heartbeat_ms)),
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
    let interval = Duration::from_millis(u64::from(interval_ms.max(1)));
    // Jitter the FIRST beat across [0, interval) so connections that reach Ready
    // in the same ramp batch don't beat in lock-step. protocol.md §4 requires the
    // client to jitter the first beat (±10%); a full random phase de-syncs the
    // herd so the gateway's heartbeat + presence-cache load — the very thing the
    // benchmark measures — stays at true steady state instead of 30 s spikes.
    let jitter = Duration::from_millis(rand::random::<u64>() % (interval.as_millis() as u64));
    let mut ticker = tokio::time::interval_at(tokio::time::Instant::now() + jitter, interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // First tick fires after `jitter` — do NOT consume it.

    let mut last_seq: u64 = 0;
    loop {
        tokio::select! {
            // Biased: a shutdown wins ties so an orderly drain is classified as
            // Shutdown, not misattributed as a peer Disconnect in the report.
            biased;
            () = shutdown.cancelled() => return HoldOutcome::Shutdown,
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
    let heartbeat_ms = match handshake(&mut tx, &mut rx, params, &shutdown).await {
        Ok(Some(advertised)) => {
            if heartbeat_override_ms > 0 {
                heartbeat_override_ms
            } else {
                advertised
            }
        }
        // Shutdown mid-handshake: clean abort, never reached Ready — count nothing.
        Ok(None) => return,
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
