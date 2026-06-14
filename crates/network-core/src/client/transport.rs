//! Client-side framed transports: QUIC (primary) and WSS (fallback).
//! [`AnyTransport`] is an enum (not the server's dyn trait) so dispatch is
//! static — no boxing, no async-trait dependency.

use std::sync::Arc;

use dice_protocol::MAX_FRAME_BYTES;
use dice_protocol::framing::{decode_frame_bare, encode_frame_bare};
use dice_protocol::v1::Frame;
use futures_util::{SinkExt as _, StreamExt as _};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::{CloseFrame, Message, WebSocketConfig};
use tokio_tungstenite::{Connector, MaybeTlsStream, WebSocketStream};

use super::quic::QuicTransport;

/// Which concrete transport a connection runs on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportKind {
    Quic,
    Wss,
}

impl TransportKind {
    /// Stable lowercase name (`"quic"` / `"wss"`) for logs, UI events and
    /// persistence (cache `meta."last_transport"`).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Quic => "quic",
            Self::Wss => "wss",
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ClientTransportError {
    #[error("websocket: {0}")]
    Ws(#[from] tokio_tungstenite::tungstenite::Error),
    /// Codec violation: oversized frame or undecodable protobuf.
    #[error(transparent)]
    Frame(#[from] dice_protocol::framing::FrameError),
    /// Text WS frames are a protocol violation (docs/protocol.md §1).
    #[error("server sent a text WS frame (protocol violation)")]
    TextFrame,
    /// Endpoint bind / DNS resolution.
    #[error("i/o: {0}")]
    Io(#[from] std::io::Error),
    #[error("quic connect: {0}")]
    QuicConnect(#[from] quinn::ConnectError),
    #[error("quic connection: {0}")]
    QuicConnection(#[from] quinn::ConnectionError),
    #[error("quic stream read: {0}")]
    QuicRead(#[from] quinn::ReadError),
    #[error("quic stream write: {0}")]
    QuicWrite(#[from] quinn::WriteError),
    /// Anything else QUIC (empty DNS answer, …).
    #[error("quic: {0}")]
    Quic(String),
}

/// One WSS connection to `/gateway/v1`: one binary WS message = one bare
/// `dice.v1.Frame`. Ping/pong is auto-handled by tungstenite; liveness is the
/// app heartbeat (driver's job, not ours).
pub struct WssTransport {
    stream: WebSocketStream<MaybeTlsStream<TcpStream>>,
    last_close_code: Option<u16>,
}

impl WssTransport {
    /// TCP + TLS + WS handshake against `url` (e.g.
    /// `wss://host:8443/gateway/v1`) with the given rustls trust config.
    /// Inbound messages are capped at `MAX_FRAME_BYTES` (256 KiB) before
    /// allocation, both per-message and per-WS-frame.
    pub async fn connect(
        url: &url::Url,
        tls: Arc<rustls::ClientConfig>,
    ) -> Result<Self, ClientTransportError> {
        let ws_config = WebSocketConfig::default()
            .max_message_size(Some(MAX_FRAME_BYTES))
            .max_frame_size(Some(MAX_FRAME_BYTES));
        let (stream, _response) = tokio_tungstenite::connect_async_tls_with_config(
            url.as_str(),
            Some(ws_config),
            true, // disable Nagle: heartbeats and typing must not coalesce
            Some(Connector::Rustls(tls)),
        )
        .await?;
        Ok(Self {
            stream,
            last_close_code: None,
        })
    }

    /// Next inbound frame; `Ok(None)` = peer closed (close code, if any, is
    /// retained — see [`Self::last_close_code`]).
    pub async fn recv(&mut self) -> Result<Option<Frame>, ClientTransportError> {
        loop {
            match self.stream.next().await {
                None => return Ok(None),
                Some(Ok(Message::Binary(bytes))) => return Ok(Some(decode_frame_bare(&bytes)?)),
                Some(Ok(Message::Text(_))) => return Err(ClientTransportError::TextFrame),
                Some(Ok(Message::Ping(_) | Message::Pong(_))) => continue, // auto-handled
                Some(Ok(Message::Close(frame))) => {
                    self.last_close_code = frame.map(|f| u16::from(f.code));
                    return Ok(None);
                }
                // Never surfaced on reads per tungstenite docs; defensive.
                Some(Ok(Message::Frame(_))) => continue,
                Some(Err(tokio_tungstenite::tungstenite::Error::ConnectionClosed)) => {
                    return Ok(None);
                }
                Some(Err(error)) => return Err(error.into()),
            }
        }
    }

    /// Send one frame (encoded via the one codec in `dice-protocol`).
    pub async fn send(&mut self, frame: &Frame) -> Result<(), ClientTransportError> {
        let bytes = encode_frame_bare(frame)?;
        self.stream
            .send(Message::Binary(bytes))
            .await
            .map_err(ClientTransportError::from)
    }

    /// Best-effort close with an application close code
    /// (`4000 + ErrorCode`, or 1000 for a clean client goodbye).
    pub async fn close(&mut self, code: u32, reason: &str) {
        let frame = CloseFrame {
            code: CloseCode::from(code as u16),
            reason: reason.into(),
        };
        let _ = self.stream.close(Some(frame)).await;
    }

    /// WS close code from the most recent peer close, if one was seen.
    /// 4010/4011 are the resumable reconnect hints (docs/protocol.md §8).
    pub fn last_close_code(&self) -> Option<u16> {
        self.last_close_code
    }
}

/// Static dispatch over the client transports. Both variants are boxed (one
/// allocation per connection): the transports differ ~6x in size and either
/// would dominate the enum footprint (clippy::large_enum_variant).
pub enum AnyTransport {
    Quic(Box<QuicTransport>),
    Wss(Box<WssTransport>),
}

impl AnyTransport {
    /// Next inbound frame; `Ok(None)` = clean close by the peer.
    pub async fn recv(&mut self) -> Result<Option<Frame>, ClientTransportError> {
        match self {
            Self::Quic(t) => t.recv().await,
            Self::Wss(t) => t.recv().await,
        }
    }

    /// Send one frame.
    pub async fn send(&mut self, frame: &Frame) -> Result<(), ClientTransportError> {
        match self {
            Self::Quic(t) => t.send(frame).await,
            Self::Wss(t) => t.send(frame).await,
        }
    }

    /// Best-effort close with an application close code.
    pub async fn close(&mut self, code: u32, reason: &str) {
        match self {
            Self::Quic(t) => t.close(code, reason).await,
            Self::Wss(t) => t.close(code, reason).await,
        }
    }

    /// Close code from the most recent peer close, if any.
    pub fn last_close_code(&self) -> Option<u16> {
        match self {
            Self::Quic(t) => t.last_close_code(),
            Self::Wss(t) => t.last_close_code(),
        }
    }

    /// Which transport this connection runs on.
    pub fn kind(&self) -> TransportKind {
        match self {
            Self::Quic(_) => TransportKind::Quic,
            Self::Wss(_) => TransportKind::Wss,
        }
    }

    /// The QUIC connection for voice datagram I/O, or `None` over WSS.
    pub fn quic_connection(&self) -> Option<quinn::Connection> {
        match self {
            Self::Quic(t) => Some(t.connection()),
            Self::Wss(_) => None,
        }
    }
}
