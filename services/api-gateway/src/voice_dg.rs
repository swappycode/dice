//! Voice datagram fan-out — the SFU's I/O half.
//!
//! A registry of voice-capable QUIC connections keyed by user, a per-connection
//! read pump that forwards each inbound voice datagram via voice-service, and a
//! [`VoiceSink`] that sends datagrams to a target's connections. The forwarding
//! *decision* lives in voice-service (`Voice::forward`, unit-tested); this
//! module is only the QUIC datagram plumbing the gateway owns.
//!
//! Lifecycle: [`VoiceDatagrams::attach`] registers a connection + spawns its
//! pump and hands back a [`VoiceAttach`] guard; dropping the guard (on session
//! detach / teardown) unregisters the connection and stops its pump.

use std::sync::Arc;

use bytes::Bytes;
use dashmap::DashMap;
use dice_common::id::UserId;
use tokio::task::JoinHandle;
use voice_service::{Voice, VoiceSink};

/// Live voice-capable QUIC connections, keyed by user (usually one per user;
/// multiple when a user has several QUIC sessions). Is itself the [`VoiceSink`].
pub(crate) struct VoiceDatagrams {
    voice: Arc<dyn Voice>,
    conns: DashMap<u64, Vec<quinn::Connection>>,
}

impl VoiceDatagrams {
    pub(crate) fn new(voice: Arc<dyn Voice>) -> Arc<Self> {
        Arc::new(Self {
            voice,
            conns: DashMap::new(),
        })
    }

    /// Register `conn` under `user` and spawn its read pump. `conn = None` (a
    /// non-QUIC session) yields an inert guard. The returned [`VoiceAttach`]
    /// unregisters + stops the pump on drop.
    pub(crate) fn attach(
        self: &Arc<Self>,
        user: UserId,
        conn: Option<quinn::Connection>,
    ) -> VoiceAttach {
        let Some(conn) = conn else {
            return VoiceAttach {
                registry: None,
                user,
                stable_id: 0,
                pump: None,
            };
        };
        let stable_id = conn.stable_id();
        self.conns.entry(user.raw()).or_default().push(conn.clone());
        let pump = tokio::spawn(Arc::clone(self).pump(user, conn));
        VoiceAttach {
            registry: Some(Arc::clone(self)),
            user,
            stable_id,
            pump: Some(pump),
        }
    }

    /// Per-connection read loop: forward each inbound voice datagram to the
    /// sender's co-members (voice-service decides the targets and fans out
    /// through `self` as the sink). Exits when the connection closes.
    async fn pump(self: Arc<Self>, user: UserId, conn: quinn::Connection) {
        loop {
            match conn.read_datagram().await {
                Ok(bytes) => {
                    if let Err(error) = self.voice.forward(user, bytes, &*self).await {
                        tracing::debug!(%error, %user, "voice datagram forward failed");
                    }
                }
                Err(_) => return, // connection closed
            }
        }
    }

    fn remove(&self, user: UserId, stable_id: usize) {
        if let Some(mut entry) = self.conns.get_mut(&user.raw()) {
            entry.retain(|c| c.stable_id() != stable_id);
            if entry.is_empty() {
                drop(entry);
                self.conns.remove(&user.raw());
            }
        }
    }
}

impl VoiceSink for VoiceDatagrams {
    fn deliver(&self, target: UserId, packet: Bytes) {
        if let Some(conns) = self.conns.get(&target.raw()) {
            for conn in conns.iter() {
                // Best-effort: voice is loss-tolerant, drop on any send error.
                let _ = conn.send_datagram(packet.clone());
            }
        }
    }
}

/// RAII registration: unregisters the connection and aborts its read pump when
/// dropped (session detach or teardown).
pub(crate) struct VoiceAttach {
    registry: Option<Arc<VoiceDatagrams>>,
    user: UserId,
    stable_id: usize,
    pump: Option<JoinHandle<()>>,
}

impl Drop for VoiceAttach {
    fn drop(&mut self) {
        if let Some(pump) = self.pump.take() {
            pump.abort();
        }
        if let Some(registry) = &self.registry {
            registry.remove(self.user, self.stable_id);
        }
    }
}
