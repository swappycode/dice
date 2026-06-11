//! `FramedTransport` adapter over `axum::extract::ws` (critique #20: axum
//! types stay out of network-core; the adapter lives here).
//!
//! One binary WS message = one bare `Frame` (`encode_frame_bare` /
//! `decode_frame_bare` — the one codec). Text frames are a protocol
//! violation (protocol §1). Close maps to a WS close frame carrying the
//! `4000 + ErrorCode` application code.

use std::net::SocketAddr;

use axum::extract::ws::{CloseFrame, Message, WebSocket};
use dice_network_core::server::{FramedTransport, TransportError, TransportKind};
use dice_protocol::framing::{decode_frame_bare, encode_frame_bare};
use dice_protocol::v1::Frame;

pub(crate) struct WsTransport {
    socket: WebSocket,
    /// The hand-rolled HTTPS accept loop does not thread the peer address
    /// through hyper, so WS sessions report the unspecified address in M1.
    remote: SocketAddr,
}

impl WsTransport {
    pub(crate) fn new(socket: WebSocket) -> Self {
        Self {
            socket,
            remote: SocketAddr::from(([0, 0, 0, 0], 0)),
        }
    }
}

#[async_trait::async_trait]
impl FramedTransport for WsTransport {
    async fn recv(&mut self) -> Result<Option<Frame>, TransportError> {
        loop {
            match self.socket.recv().await {
                None => return Ok(None),
                Some(Ok(Message::Binary(bytes))) => {
                    return Ok(Some(decode_frame_bare(&bytes)?));
                }
                Some(Ok(Message::Text(_))) => {
                    // Protocol §1: text frames ⇒ protocol-error close. Close
                    // right here (4006 = 4000 + INVALID_ARGUMENT) and report
                    // EOF so the session tears down without a resume window.
                    self.close(
                        dice_protocol::v1::ErrorCode::InvalidArgument.close_code(),
                        "text WS frames are a protocol violation",
                    )
                    .await;
                    return Ok(None);
                }
                Some(Ok(Message::Ping(_) | Message::Pong(_))) => continue, // auto-handled
                Some(Ok(Message::Close(_))) => return Ok(None),
                Some(Err(error)) => return Err(TransportError::Closed(error.to_string())),
            }
        }
    }

    async fn send(&mut self, frame: &Frame) -> Result<(), TransportError> {
        let bytes = encode_frame_bare(frame)?;
        self.socket
            .send(Message::Binary(bytes))
            .await
            .map_err(|error| TransportError::Closed(error.to_string()))
    }

    async fn close(&mut self, code: u32, reason: &str) {
        let close = Message::Close(Some(CloseFrame {
            code: code as u16,
            reason: reason.into(),
        }));
        let _ = self.socket.send(close).await;
    }

    fn remote_addr(&self) -> SocketAddr {
        self.remote
    }

    fn kind(&self) -> TransportKind {
        TransportKind::Wss
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
pub(crate) mod tests {
    use super::*;

    /// Transport stub for unit tests that never touch the wire.
    pub(crate) struct DummyTransport;

    #[async_trait::async_trait]
    impl FramedTransport for DummyTransport {
        async fn recv(&mut self) -> Result<Option<Frame>, TransportError> {
            Ok(None)
        }
        async fn send(&mut self, _frame: &Frame) -> Result<(), TransportError> {
            Ok(())
        }
        async fn close(&mut self, _code: u32, _reason: &str) {}
        fn remote_addr(&self) -> SocketAddr {
            SocketAddr::from(([0, 0, 0, 0], 0))
        }
        fn kind(&self) -> TransportKind {
            TransportKind::Wss
        }
    }
}
