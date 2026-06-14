//! Server half (feature `"server"`): the framed-transport seam the gateway
//! codes against, the QUIC acceptor, and the shared-port HTTPS accept loop.
//!
//! axum WebSocket types deliberately stay OUT of this crate (critique
//! resolution #20): api-gateway provides its own `FramedTransport` impl over
//! `axum::extract::ws`, while [`serve_https`] here only carries the TLS TCP
//! side that REST and the WS upgrade share.

mod https;
mod quic;

pub use https::{PeerAddr, ServeError, serve_https, serve_https_on};
pub use quic::{QuicAcceptor, QuicTransport};

use dice_protocol::v1::Frame;

/// Which transport a session arrived on (drives metrics labels and close
/// semantics in the gateway).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportKind {
    Quic,
    Wss,
}

#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    /// The peer or the network tore the connection down un-cleanly
    /// (clean closes surface as `Ok(None)` from [`FramedTransport::recv`]).
    #[error("transport closed: {0}")]
    Closed(String),
    #[error("i/o: {0}")]
    Io(#[from] std::io::Error),
    /// Codec violation — oversized frame (gateway closes with
    /// `PAYLOAD_TOO_LARGE`) or undecodable protobuf.
    #[error(transparent)]
    Frame(#[from] dice_protocol::framing::FrameError),
}

/// One logical client connection delivering whole `dice.v1.Frame`s, QUIC or
/// WSS. The gateway session loop is written against this seam only.
#[async_trait::async_trait]
pub trait FramedTransport: Send {
    /// Next inbound frame; `Ok(None)` = clean close by the peer.
    async fn recv(&mut self) -> Result<Option<Frame>, TransportError>;
    /// Send one frame (encoded via the one codec in `dice-protocol`).
    async fn send(&mut self, frame: &Frame) -> Result<(), TransportError>;
    /// Close the connection with an application close code
    /// (`4000 + ErrorCode`, docs/protocol.md §8). Best-effort, never fails.
    async fn close(&mut self, code: u32, reason: &str);
    fn remote_addr(&self) -> std::net::SocketAddr;
    fn kind(&self) -> TransportKind;
    /// The underlying QUIC connection, for out-of-band datagram I/O (voice).
    /// `None` for transports without datagrams (WSS). The returned handle is a
    /// cheap clone; voice send/recv runs on it independently of the control
    /// stream this trait otherwise frames.
    fn quic_connection(&self) -> Option<quinn::Connection> {
        None
    }
}
