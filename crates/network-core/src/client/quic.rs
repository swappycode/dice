//! Client-side QUIC transport (docs/protocol.md §1): one connection, the
//! single CLIENT-opened bidi control stream, frames carried as
//! `u32 big-endian length ‖ Frame bytes` via the one codec in
//! `dice-protocol::framing`. TLS trust comes from the same [`super::tls::
//! TlsOptions`] root store the WSS transport uses — full chain + server-name
//! verification always runs; there is deliberately NO bypass.
//!
//! Close semantics: an explicit [`QuicTransport::close`] is the clean goodbye
//! (the server ends the session, no resume window). DROPPING the transport
//! without closing — the driver's abrupt-reconnect path — closes the
//! connection with `GOING_AWAY` (4011) instead, because quinn's implicit
//! drop-close (code 0) would otherwise read as a clean goodbye and destroy
//! the server-side resume window.

use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Duration;

use bytes::BytesMut;
use dice_protocol::framing::{FrameDecoder, encode_frame};
use dice_protocol::v1::{ErrorCode, Frame};

use super::transport::ClientTransportError;

/// Bounded read size feeding the frame decoder (one scratch buffer per
/// connection; the decoder itself caps accumulation at `MAX_FRAME_BYTES`
/// BEFORE buffering an announced payload).
const READ_CHUNK: usize = 8 * 1024;

/// Best-effort budget for flushing the CONNECTION_CLOSE packet on a clean
/// close before the endpoint is torn down.
const CLOSE_FLUSH: Duration = Duration::from_millis(500);

/// Where the QUIC gateway lives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuicEndpoint {
    /// TLS server name (SNI + certificate verification): a DNS name or an
    /// IP string (the dev leaf carries `127.0.0.1`/`::1` IP SANs).
    pub server_name: String,
    pub addr: QuicAddr,
}

/// Dial target: a resolved socket address or a `host:port` resolved on every
/// connect (IPv4 preferred — see [`QuicTransport::connect`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QuicAddr {
    Socket(SocketAddr),
    HostPort(String),
}

#[derive(Debug, thiserror::Error)]
#[error("invalid QUIC endpoint {0:?} (expected host:port)")]
pub struct InvalidQuicEndpoint(pub String);

impl QuicEndpoint {
    /// Parse `host:port` (`localhost:8444`, `[::1]:8444`, `203.0.113.7:8444`).
    /// The host becomes the TLS server name; IP literals dial directly,
    /// hostnames resolve per connect.
    pub fn from_host_port(value: &str) -> Result<Self, InvalidQuicEndpoint> {
        if let Ok(addr) = value.parse::<SocketAddr>() {
            return Ok(Self {
                server_name: addr.ip().to_string(),
                addr: QuicAddr::Socket(addr),
            });
        }
        let (host, port) = value
            .rsplit_once(':')
            .ok_or_else(|| InvalidQuicEndpoint(value.to_owned()))?;
        if host.is_empty() || port.parse::<u16>().is_err() {
            return Err(InvalidQuicEndpoint(value.to_owned()));
        }
        Ok(Self {
            server_name: host.to_owned(),
            addr: QuicAddr::HostPort(value.to_owned()),
        })
    }
}

/// One QUIC connection to the gateway, framed over the single bidi control
/// stream. Same inherent async API as [`super::transport::WssTransport`].
pub struct QuicTransport {
    /// Held so the connection stays driven for this transport's lifetime
    /// (quinn endpoints are cheap handles over a shared driver).
    endpoint: quinn::Endpoint,
    conn: quinn::Connection,
    send: quinn::SendStream,
    recv: quinn::RecvStream,
    decoder: FrameDecoder,
    scratch: Box<[u8]>,
    last_close_code: Option<u16>,
    closed_cleanly: bool,
}

impl QuicTransport {
    /// Dial `target` (resolving `host:port` with IPv4 preference), complete
    /// the TLS 1.3 handshake (ALPN `dice/1`, full verification against
    /// `config`'s root store) and open the single bidi control stream.
    ///
    /// The local endpoint binds the unspecified address of the target's
    /// family — `0.0.0.0:0` for IPv4 (including localhost), `[::]:0` for
    /// IPv6 — which is the reliable shape on Windows.
    pub async fn connect(
        target: &QuicEndpoint,
        config: quinn::ClientConfig,
    ) -> Result<Self, ClientTransportError> {
        let addr = match &target.addr {
            QuicAddr::Socket(addr) => *addr,
            QuicAddr::HostPort(host_port) => resolve_prefer_v4(host_port).await?,
        };
        let bind: SocketAddr = if addr.is_ipv4() {
            (Ipv4Addr::UNSPECIFIED, 0).into()
        } else {
            (Ipv6Addr::UNSPECIFIED, 0).into()
        };
        let endpoint = quinn::Endpoint::client(bind)?;
        let conn = endpoint
            .connect_with(config, addr, &target.server_name)?
            .await?;
        // The client opens the ONE control stream (protocol §1). quinn
        // resolves this locally under the server's stream budget; it
        // materializes on the wire with the first byte we send.
        let (send, recv) = conn.open_bi().await?;
        Ok(Self {
            endpoint,
            conn,
            send,
            recv,
            decoder: FrameDecoder::new(),
            scratch: vec![0u8; READ_CHUNK].into_boxed_slice(),
            last_close_code: None,
            closed_cleanly: false,
        })
    }

    /// Next inbound frame; `Ok(None)` = the gateway closed deliberately
    /// (stream FIN or application close — the close code, if any, is
    /// retained, see [`Self::last_close_code`]).
    pub async fn recv(&mut self) -> Result<Option<Frame>, ClientTransportError> {
        use quinn::{ConnectionError, ReadError};
        loop {
            if let Some(frame) = self.decoder.try_next()? {
                return Ok(Some(frame));
            }
            match self.recv.read(&mut self.scratch).await {
                Ok(Some(n)) => self.decoder.extend(&self.scratch[..n])?,
                // FIN: the gateway finished its send side — clean close.
                Ok(None) => return Ok(None),
                Err(ReadError::ConnectionLost(ConnectionError::ApplicationClosed(close))) => {
                    // 4000+ErrorCode application closes mirror WS close codes
                    // (protocol §8); the driver reads 4010/4011 as resumable.
                    self.last_close_code = u16::try_from(close.error_code.into_inner()).ok();
                    return Ok(None);
                }
                Err(ReadError::ConnectionLost(ConnectionError::LocallyClosed)) => {
                    return Ok(None);
                }
                Err(error) => return Err(error.into()),
            }
        }
    }

    /// Send one frame (length-prefixed via the one codec in `dice-protocol`).
    pub async fn send(&mut self, frame: &Frame) -> Result<(), ClientTransportError> {
        let mut buf = BytesMut::new();
        encode_frame(frame, &mut buf)?;
        self.send
            .write_all(&buf)
            .await
            .map_err(ClientTransportError::from)
    }

    /// Clean goodbye: FIN the control stream, close the connection with
    /// `code` (1000 for a clean client goodbye, `4000 + ErrorCode`
    /// otherwise) and give the endpoint a bounded moment to flush the
    /// CONNECTION_CLOSE packet. The server ends the session for codes
    /// 0/1000 — no resume window.
    pub async fn close(&mut self, code: u32, reason: &str) {
        self.closed_cleanly = true;
        let _ = self.send.finish();
        self.conn
            .close(quinn::VarInt::from_u32(code), reason.as_bytes());
        let _ = tokio::time::timeout(CLOSE_FLUSH, self.endpoint.wait_idle()).await;
    }

    /// Application close code from the most recent peer close, if one was
    /// seen. 4010/4011 are the resumable reconnect hints (protocol §8).
    pub fn last_close_code(&self) -> Option<u16> {
        self.last_close_code
    }
}

impl Drop for QuicTransport {
    /// Dropping without [`Self::close`] is the abrupt path (forced
    /// reconnects, dead connections): close with `GOING_AWAY` (4011) so a
    /// still-listening gateway detaches into the resume window. On an
    /// already-dead connection this is a no-op.
    fn drop(&mut self) {
        if !self.closed_cleanly {
            self.conn.close(
                quinn::VarInt::from_u32(ErrorCode::GoingAway.close_code()),
                b"transport dropped; resuming",
            );
        }
    }
}

/// Resolve `host:port`, preferring the first IPv4 address (Windows resolvers
/// often list `::1` first for localhost, while the gateway's dev bind and
/// the client's default endpoint family are IPv4).
async fn resolve_prefer_v4(host_port: &str) -> Result<SocketAddr, ClientTransportError> {
    let mut fallback = None;
    for addr in tokio::net::lookup_host(host_port).await? {
        if addr.is_ipv4() {
            return Ok(addr);
        }
        fallback.get_or_insert(addr);
    }
    fallback.ok_or_else(|| ClientTransportError::Quic(format!("no addresses for {host_port}")))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn from_host_port_accepts_hostnames_and_ip_literals() {
        let named = QuicEndpoint::from_host_port("localhost:8444").unwrap();
        assert_eq!(named.server_name, "localhost");
        assert_eq!(named.addr, QuicAddr::HostPort("localhost:8444".into()));

        let v4 = QuicEndpoint::from_host_port("127.0.0.1:8444").unwrap();
        assert_eq!(v4.server_name, "127.0.0.1");
        assert_eq!(v4.addr, QuicAddr::Socket("127.0.0.1:8444".parse().unwrap()));

        let v6 = QuicEndpoint::from_host_port("[::1]:8444").unwrap();
        assert_eq!(v6.server_name, "::1");
        assert_eq!(v6.addr, QuicAddr::Socket("[::1]:8444".parse().unwrap()));
    }

    #[test]
    fn from_host_port_rejects_garbage() {
        for bad in ["", "localhost", "localhost:", ":8444", "localhost:port"] {
            assert!(
                QuicEndpoint::from_host_port(bad).is_err(),
                "{bad:?} must be rejected"
            );
        }
    }

    #[tokio::test]
    async fn resolution_prefers_ipv4() {
        // "localhost" resolves to ::1 and/or 127.0.0.1 depending on the OS;
        // whenever a v4 address exists it must win.
        let addr = resolve_prefer_v4("localhost:1").await.unwrap();
        if addr.is_ipv6() {
            // Pure-v6 hosts are legal; the fallback path returned the only
            // family available.
            assert_eq!(addr.ip().to_string(), "::1");
        } else {
            assert_eq!(addr.ip().to_string(), "127.0.0.1");
        }
        assert_eq!(addr.port(), 1);
    }
}
