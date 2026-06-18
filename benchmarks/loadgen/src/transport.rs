//! Thin per-connection frame I/O over either transport, reusing the one
//! sanctioned codec in `dice_protocol::framing` (never reimplemented). QUIC
//! frames the control stream with a u32-BE length prefix; WSS sends one bare
//! `Frame` per binary message (docs/protocol.md §1).
//!
//! The QUIC path deliberately does NOT use `dice_network_core`'s `QuicTransport`
//! (which binds a fresh `quinn::Endpoint` — i.e. a UDP socket + driver — per
//! connection). At 100k that would be 100k sockets and would defeat GSO batching.
//! Here connections SHARE a small pool of endpoints, dialed via raw quinn.

use std::net::SocketAddr;
use std::time::Duration;

use anyhow::Context as _;
use bytes::BytesMut;
use dice_protocol::framing::{FrameDecoder, decode_frame_bare, encode_frame, encode_frame_bare};
use dice_protocol::v1::Frame;
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt as _, StreamExt as _};
use quinn::{Connection, Endpoint, RecvStream, SendStream};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{Connector, MaybeTlsStream, WebSocketStream};

/// Bounded read scratch per QUIC connection. Handshake/heartbeat frames are
/// tiny and an unseeded Ready is a few dozen bytes, so 2 KiB is ample — and at
/// 100k connections it keeps the loadgen's own footprint modest (the decoder
/// still caps accumulation at `MAX_FRAME_BYTES`).
const READ_SCRATCH: usize = 2 * 1024;

type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// Outbound half of a connection.
pub enum Tx {
    Quic(SendStream),
    Wss(SplitSink<WsStream, Message>),
}

impl Tx {
    pub async fn send(&mut self, frame: &Frame) -> anyhow::Result<()> {
        match self {
            Tx::Quic(send) => {
                let mut buf = BytesMut::new();
                encode_frame(frame, &mut buf)?;
                send.write_all(&buf).await.context("quic write")?;
                Ok(())
            }
            Tx::Wss(sink) => {
                let bytes = encode_frame_bare(frame)?;
                sink.send(Message::Binary(bytes))
                    .await
                    .context("wss send")?;
                Ok(())
            }
        }
    }
}

/// Inbound half of a connection. Tracks the peer's close code (if any) so the
/// hold loop can attribute disconnects (4012 heartbeat-timeout, 4010 slow
/// consumer, etc.).
pub enum Rx {
    Quic {
        recv: RecvStream,
        decoder: FrameDecoder,
        scratch: Box<[u8]>,
        closed_code: Option<u32>,
    },
    Wss {
        stream: SplitStream<WsStream>,
        closed_code: Option<u32>,
    },
}

impl Rx {
    fn quic(recv: RecvStream) -> Self {
        Rx::Quic {
            recv,
            decoder: FrameDecoder::new(),
            scratch: vec![0u8; READ_SCRATCH].into_boxed_slice(),
            closed_code: None,
        }
    }

    fn wss(stream: SplitStream<WsStream>) -> Self {
        Rx::Wss {
            stream,
            closed_code: None,
        }
    }

    /// Next frame, or `Ok(None)` when the peer closed the connection (clean FIN
    /// or coded close — see [`Self::closed_code`]). `Err` is a protocol/decode
    /// fault. Never returns an error merely because the connection ended.
    pub async fn recv(&mut self) -> anyhow::Result<Option<Frame>> {
        match self {
            Rx::Quic {
                recv,
                decoder,
                scratch,
                closed_code,
            } => loop {
                if let Some(frame) = decoder.try_next()? {
                    return Ok(Some(frame));
                }
                match recv.read(scratch).await {
                    Ok(Some(n)) => decoder.extend(&scratch[..n])?,
                    Ok(None) => return Ok(None), // FIN: clean
                    Err(err) => {
                        *closed_code = quic_close_code(&err);
                        return Ok(None);
                    }
                }
            },
            Rx::Wss {
                stream,
                closed_code,
            } => loop {
                match stream.next().await {
                    Some(Ok(Message::Binary(data))) => return Ok(Some(decode_frame_bare(&data)?)),
                    Some(Ok(Message::Close(frame))) => {
                        *closed_code = frame.map(|f| u16::from(f.code) as u32);
                        return Ok(None);
                    }
                    // Ping/Pong/Text/etc.: tungstenite auto-pongs; ignore.
                    Some(Ok(_)) => continue,
                    Some(Err(_)) | None => return Ok(None),
                }
            },
        }
    }

    pub fn closed_code(&self) -> Option<u32> {
        match self {
            Rx::Quic { closed_code, .. } | Rx::Wss { closed_code, .. } => *closed_code,
        }
    }
}

/// The QUIC application close code, if the peer closed with one. quinn's
/// implicit drop-close (code 0) and a clean local close read as `None`.
fn quic_close_code(err: &quinn::ReadError) -> Option<u32> {
    use quinn::{ConnectionError, ReadError};
    match err {
        ReadError::ConnectionLost(ConnectionError::ApplicationClosed(close)) => {
            match close.error_code.into_inner() {
                0 => None,
                code => u32::try_from(code).ok(),
            }
        }
        _ => None,
    }
}

/// Dial one QUIC connection over a SHARED endpoint and open the single bidi
/// control stream. Returns the `Connection` (kept alive by the caller; used for
/// the clean close on shutdown) plus the frame halves.
pub async fn quic_connect(
    endpoint: &Endpoint,
    cfg: quinn::ClientConfig,
    addr: SocketAddr,
    server_name: &str,
    connect_timeout: Duration,
) -> anyhow::Result<(Connection, Tx, Rx)> {
    let connecting = endpoint
        .connect_with(cfg, addr, server_name)
        .context("quic connect_with")?;
    let conn = tokio::time::timeout(connect_timeout, connecting)
        .await
        .context("quic handshake timed out")?
        .context("quic handshake failed")?;
    // The client opens the ONE control stream; it materialises on the wire with
    // the first byte (the Identify frame the handshake sends next).
    let (send, recv) = conn.open_bi().await.context("open control stream")?;
    Ok((conn, Tx::Quic(send), Rx::quic(recv)))
}

/// Dial one WSS connection (`/gateway/v1`) using the dev-CA-trusting rustls
/// config. Each WSS connection is its own TCP socket — no endpoint sharing.
pub async fn wss_connect(
    url: &str,
    tls: std::sync::Arc<rustls::ClientConfig>,
    connect_timeout: Duration,
) -> anyhow::Result<(Tx, Rx)> {
    let connector = Connector::Rustls(tls);
    let (ws, _resp) = tokio::time::timeout(
        connect_timeout,
        tokio_tungstenite::connect_async_tls_with_config(url, None, false, Some(connector)),
    )
    .await
    .context("wss connect timed out")?
    .context("wss connect failed")?;
    let (sink, stream) = ws.split();
    Ok((Tx::Wss(sink), Rx::wss(stream)))
}
