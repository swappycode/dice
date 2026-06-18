//! QUIC acceptor + framed transport over the single bidi control stream
//! (docs/protocol.md §1: the CLIENT opens the stream after connecting).

use std::net::SocketAddr;
use std::time::Duration;

use bytes::BytesMut;
use dice_protocol::framing::{FrameDecoder, encode_frame};
use dice_protocol::v1::Frame;
use tokio_util::sync::CancellationToken;

use super::{FramedTransport, TransportError, TransportKind};

/// How long the server waits after the QUIC handshake for the client to open
/// the control stream. Matches the standing 5 s Identify deadline
/// (docs/protocol.md §3) — a client that hasn't even opened the stream by then
/// could never identify in time.
const CONTROL_STREAM_DEADLINE: Duration = Duration::from_secs(5);

/// Bounded read size feeding the frame decoder (one scratch buffer per
/// connection; the decoder itself caps accumulation at `MAX_FRAME_BYTES`).
const READ_CHUNK: usize = 8 * 1024;

#[derive(Debug, thiserror::Error)]
enum EstablishError {
    #[error("connection: {0}")]
    Connection(#[from] quinn::ConnectionError),
    #[error("client did not open the control stream within {CONTROL_STREAM_DEADLINE:?}")]
    ControlStreamTimeout,
}

/// Listens for QUIC connections and yields ready [`QuicTransport`]s — i.e.
/// connections whose TLS handshake completed AND whose client opened the
/// single bidi control stream.
pub struct QuicAcceptor {
    endpoint: quinn::Endpoint,
}

impl QuicAcceptor {
    /// Bind a server endpoint. `cfg` comes from
    /// [`crate::tls::quic_server_config`] (ALPN `dice/1`, protocol §1 tuning).
    pub fn bind(addr: SocketAddr, cfg: quinn::ServerConfig) -> std::io::Result<Self> {
        Ok(Self {
            endpoint: quinn::Endpoint::server(cfg, addr)?,
        })
    }

    /// Bind with optional UDP socket buffer sizing (the 100k-benchmark knob).
    /// When both buffer sizes are `None` this is exactly [`Self::bind`];
    /// otherwise it constructs the UDP socket via `socket2` so SO_SNDBUF /
    /// SO_RCVBUF can be set before quinn takes ownership. GSO/GRO are still
    /// auto-detected by quinn-udp on Linux regardless of how the socket is made —
    /// the larger buffers just keep the kernel from dropping batched datagrams at
    /// scale. Must run inside a tokio runtime (quinn wraps the socket with it).
    pub fn bind_tuned(
        addr: SocketAddr,
        cfg: quinn::ServerConfig,
        send_buffer: Option<usize>,
        recv_buffer: Option<usize>,
    ) -> std::io::Result<Self> {
        if send_buffer.is_none() && recv_buffer.is_none() {
            return Self::bind(addr, cfg);
        }
        use socket2::{Domain, Protocol, Socket, Type};
        let socket = Socket::new(Domain::for_address(addr), Type::DGRAM, Some(Protocol::UDP))?;
        // Set buffer sizes BEFORE bind (the kernel applies them at/after bind).
        if let Some(n) = recv_buffer {
            socket.set_recv_buffer_size(n)?;
        }
        if let Some(n) = send_buffer {
            socket.set_send_buffer_size(n)?;
        }
        socket.set_nonblocking(true)?; // quinn/tokio require a non-blocking socket
        socket.bind(&addr.into())?;
        let runtime = quinn::default_runtime()
            .ok_or_else(|| std::io::Error::other("no async runtime for the QUIC endpoint"))?;
        let endpoint = quinn::Endpoint::new(
            quinn::EndpointConfig::default(),
            Some(cfg),
            socket.into(),
            runtime,
        )?;
        Ok(Self { endpoint })
    }

    /// The actual bound address (port 0 resolves here).
    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.endpoint.local_addr()
    }

    /// Accept the next usable connection: drives the handshake and waits (5 s)
    /// for the client's control stream. Connections that fail along the way
    /// are dropped and the loop continues. Returns `None` once `ct` is
    /// cancelled or the endpoint is closed.
    pub async fn accept(&self, ct: &CancellationToken) -> Option<QuicTransport> {
        loop {
            let incoming = tokio::select! {
                () = ct.cancelled() => return None,
                incoming = self.endpoint.accept() => incoming?,
            };
            let established = tokio::select! {
                () = ct.cancelled() => return None,
                res = Self::establish(incoming) => res,
            };
            match established {
                Ok(transport) => return Some(transport),
                Err(err) => {
                    tracing::debug!(error = %err, "QUIC connection failed before control stream");
                }
            }
        }
    }

    async fn establish(incoming: quinn::Incoming) -> Result<QuicTransport, EstablishError> {
        let conn = incoming.await?;
        let bi = tokio::time::timeout(CONTROL_STREAM_DEADLINE, conn.accept_bi()).await;
        let (send, recv) = match bi {
            Ok(res) => res?,
            Err(_elapsed) => {
                conn.close(
                    quinn::VarInt::from_u32(
                        dice_protocol::v1::ErrorCode::Unauthenticated.close_code(),
                    ),
                    b"control stream not opened",
                );
                return Err(EstablishError::ControlStreamTimeout);
            }
        };
        let remote = conn.remote_address();
        Ok(QuicTransport {
            conn,
            send,
            recv,
            decoder: FrameDecoder::new(),
            scratch: vec![0u8; READ_CHUNK].into_boxed_slice(),
            remote,
        })
    }
}

/// [`FramedTransport`] over the QUIC control stream, using the one codec
/// (`u32`-BE length prefix) from `dice-protocol::framing`.
pub struct QuicTransport {
    conn: quinn::Connection,
    send: quinn::SendStream,
    recv: quinn::RecvStream,
    decoder: FrameDecoder,
    scratch: Box<[u8]>,
    remote: SocketAddr,
}

impl QuicTransport {
    /// The underlying connection (gateway uses it for e.g. RTT stats).
    pub fn connection(&self) -> &quinn::Connection {
        &self.conn
    }
}

fn read_error(err: quinn::ReadError) -> Option<TransportError> {
    use quinn::{ConnectionError, ReadError};
    match err {
        // We closed: clean.
        ReadError::ConnectionLost(ConnectionError::LocallyClosed) => None,
        // The client closed the connection deliberately. Codes 0 (quinn's
        // implicit drop-close) and 1000 (the driver's clean goodbye) are a
        // clean close — the session ends, no resume window. Anything else —
        // notably GOING_AWAY (4011) from a client about to resume — surfaces
        // as a transport error so the session DETACHES into the resume
        // window instead (protocol §5/§8).
        ReadError::ConnectionLost(ConnectionError::ApplicationClosed(close)) => {
            match close.error_code.into_inner() {
                0 | 1000 => None,
                code => Some(TransportError::Closed(format!(
                    "client application close {code}"
                ))),
            }
        }
        other => Some(TransportError::Closed(other.to_string())),
    }
}

#[async_trait::async_trait]
impl FramedTransport for QuicTransport {
    async fn recv(&mut self) -> Result<Option<Frame>, TransportError> {
        loop {
            if let Some(frame) = self.decoder.try_next()? {
                return Ok(Some(frame));
            }
            match self.recv.read(&mut self.scratch).await {
                Ok(Some(n)) => self.decoder.extend(&self.scratch[..n])?,
                // FIN: the client finished its send side — clean close.
                Ok(None) => return Ok(None),
                Err(err) => {
                    return match read_error(err) {
                        None => Ok(None),
                        Some(e) => Err(e),
                    };
                }
            }
        }
    }

    async fn send(&mut self, frame: &Frame) -> Result<(), TransportError> {
        let mut buf = BytesMut::new();
        encode_frame(frame, &mut buf)?;
        self.send
            .write_all(&buf)
            .await
            .map_err(|err| TransportError::Closed(err.to_string()))
    }

    async fn close(&mut self, code: u32, reason: &str) {
        // Best-effort flush of the send side before the connection-level close
        // (quinn discards un-flushed stream data on close).
        let _ = self.send.finish();
        self.conn
            .close(quinn::VarInt::from_u32(code), reason.as_bytes());
    }

    fn remote_addr(&self) -> SocketAddr {
        self.remote
    }

    fn kind(&self) -> TransportKind {
        TransportKind::Quic
    }

    fn quic_connection(&self) -> Option<quinn::Connection> {
        Some(self.conn.clone())
    }
}
