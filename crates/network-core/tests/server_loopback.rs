//! Loopback tests for the server half: QUIC echo through `QuicTransport` and
//! HTTPS via the hand-rolled tokio-rustls + hyper accept loop. Self-contained
//! (ephemeral ports, throwaway dev certs) — no live infra needed.

#![allow(clippy::unwrap_used)]

use std::fs;
use std::io::BufReader;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use bytes::BytesMut;
use dice_network_core::server::{
    FramedTransport as _, QuicAcceptor, TransportKind, serve_https_on,
};
use dice_network_core::tls::{
    DevCertPaths, generate_dev_certs, install_ring_provider, load_server_config, quic_server_config,
};
use dice_protocol::framing::{FrameDecoder, encode_frame};
use dice_protocol::v1::{Frame, Hello, frame::Payload};
use dice_protocol::{ALPN_GATEWAY, MAX_FRAME_BYTES};
use tokio_util::sync::CancellationToken;

fn temp_dir(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!("dice-net-it-{tag}-{}-{nanos}", std::process::id()))
}

fn hello_frame() -> Frame {
    Frame::control(Payload::Hello(Hello {
        heartbeat_interval_ms: 30_000,
        resume_window_ms: 60_000,
        max_frame_bytes: MAX_FRAME_BYTES as u32,
    }))
}

/// Root store trusting only the throwaway dev CA.
fn dev_roots(paths: &DevCertPaths) -> rustls::RootCertStore {
    let mut roots = rustls::RootCertStore::empty();
    let file = fs::File::open(&paths.ca_pem).unwrap();
    for cert in rustls_pemfile::certs(&mut BufReader::new(file)) {
        roots.add(cert.unwrap()).unwrap();
    }
    roots
}

fn client_tls_config(paths: &DevCertPaths, alpn: &[&[u8]]) -> rustls::ClientConfig {
    let mut cfg = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_protocol_versions(&[&rustls::version::TLS13])
    .unwrap()
    .with_root_certificates(dev_roots(paths))
    .with_no_client_auth();
    cfg.alpn_protocols = alpn.iter().map(|p| p.to_vec()).collect();
    cfg
}

#[tokio::test]
async fn quic_hello_echo_round_trip() {
    install_ring_provider();
    let dir = temp_dir("quic");
    let paths = generate_dev_certs(&dir).unwrap();

    // Server: acceptor on an ephemeral port, ALPN dice/1.
    let tls = load_server_config(&paths.server_cert, &paths.server_key, &[ALPN_GATEWAY]).unwrap();
    let acceptor = QuicAcceptor::bind(
        "127.0.0.1:0".parse::<SocketAddr>().unwrap(),
        quic_server_config(tls).unwrap(),
    )
    .unwrap();
    let server_addr = acceptor.local_addr().unwrap();
    let ct = CancellationToken::new();
    let server_ct = ct.clone();
    let server = tokio::spawn(async move {
        let mut transport = acceptor
            .accept(&server_ct)
            .await
            .expect("one client connection");
        assert_eq!(transport.kind(), TransportKind::Quic);
        assert_eq!(transport.remote_addr().ip(), server_addr.ip());
        let frame = transport.recv().await.unwrap().expect("one frame");
        transport.send(&frame).await.unwrap();
        // The client closes the connection only AFTER reading the echo, so
        // this recv deterministically observes the application close — which
        // must surface as a clean Ok(None).
        let closed = transport.recv().await.unwrap();
        assert!(
            closed.is_none(),
            "app close must be Ok(None), got {closed:?}"
        );
    });

    // Client: quinn endpoint trusting the dev CA, SNI "localhost".
    let quic_client = quinn::crypto::rustls::QuicClientConfig::try_from(Arc::new(
        client_tls_config(&paths, &[ALPN_GATEWAY]),
    ))
    .unwrap();
    let mut endpoint = quinn::Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap();
    endpoint.set_default_client_config(quinn::ClientConfig::new(Arc::new(quic_client)));
    let conn = endpoint
        .connect(server_addr, "localhost")
        .unwrap()
        .await
        .unwrap();

    // Client opens the single bidi control stream and sends Hello via the
    // shared codec.
    let (mut send, mut recv) = conn.open_bi().await.unwrap();
    let hello = hello_frame();
    let mut buf = BytesMut::new();
    encode_frame(&hello, &mut buf).unwrap();
    send.write_all(&buf).await.unwrap();

    // Read the echo back through the shared decoder.
    let mut decoder = FrameDecoder::new();
    let mut chunk = [0u8; 4096];
    let echoed = loop {
        if let Some(frame) = decoder.try_next().unwrap() {
            break frame;
        }
        let n = recv.read(&mut chunk).await.unwrap().expect("stream open");
        decoder.extend(&chunk[..n]).unwrap();
    };
    assert_eq!(echoed, hello);

    // Client closes only after reading the echo; the server task then sees
    // the application close as Ok(None) and finishes.
    conn.close(quinn::VarInt::from_u32(0), b"done");
    server.await.unwrap();
    endpoint.wait_idle().await;
    ct.cancel();
    let _ = fs::remove_dir_all(&dir);
}

/// The other clean-close path: the client FINs its send side of the control
/// stream (reliable, retransmitted — unlike a connection close racing it).
#[tokio::test]
async fn quic_stream_fin_yields_none() {
    install_ring_provider();
    let dir = temp_dir("quic-fin");
    let paths = generate_dev_certs(&dir).unwrap();

    let tls = load_server_config(&paths.server_cert, &paths.server_key, &[ALPN_GATEWAY]).unwrap();
    let acceptor = QuicAcceptor::bind(
        "127.0.0.1:0".parse::<SocketAddr>().unwrap(),
        quic_server_config(tls).unwrap(),
    )
    .unwrap();
    let server_addr = acceptor.local_addr().unwrap();
    let ct = CancellationToken::new();
    let server_ct = ct.clone();
    let server = tokio::spawn(async move {
        let mut transport = acceptor.accept(&server_ct).await.expect("connection");
        let first = transport.recv().await.unwrap();
        assert!(first.is_some(), "expected the Hello before the FIN");
        // Stream FIN from the peer must surface as a clean None.
        transport.recv().await.unwrap()
    });

    let quic_client = quinn::crypto::rustls::QuicClientConfig::try_from(Arc::new(
        client_tls_config(&paths, &[ALPN_GATEWAY]),
    ))
    .unwrap();
    let mut endpoint = quinn::Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap();
    endpoint.set_default_client_config(quinn::ClientConfig::new(Arc::new(quic_client)));
    let conn = endpoint
        .connect(server_addr, "localhost")
        .unwrap()
        .await
        .unwrap();
    let (mut send, _recv) = conn.open_bi().await.unwrap();
    let mut buf = BytesMut::new();
    encode_frame(&hello_frame(), &mut buf).unwrap();
    send.write_all(&buf).await.unwrap();
    // FIN the send side but keep the connection open: stream data + FIN are
    // retransmitted until acked, so delivery is deterministic.
    send.finish().unwrap();

    let got = server.await.unwrap();
    assert!(got.is_none(), "stream FIN must be Ok(None), got {got:?}");
    conn.close(quinn::VarInt::from_u32(0), b"done");
    endpoint.wait_idle().await;
    ct.cancel();
    let _ = fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn serve_https_healthz_over_tls() {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    install_ring_provider();
    let dir = temp_dir("https");
    let paths = generate_dev_certs(&dir).unwrap();

    let tls = load_server_config(&paths.server_cert, &paths.server_key, &[]).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = axum::Router::new().route("/healthz", axum::routing::get(|| async { "ok" }));
    let ct = CancellationToken::new();
    let server = tokio::spawn(serve_https_on(listener, tls, router, ct.clone()));

    // Raw HTTP/1.1 over tokio-rustls, trusting only the dev CA.
    let connector = tokio_rustls::TlsConnector::from(Arc::new(client_tls_config(&paths, &[])));
    let tcp = tokio::net::TcpStream::connect(addr).await.unwrap();
    let server_name = rustls_pki_types::ServerName::try_from("localhost").unwrap();
    let mut stream = connector.connect(server_name, tcp).await.unwrap();
    stream
        .write_all(b"GET /healthz HTTP/1.1\r\nhost: localhost\r\nconnection: close\r\n\r\n")
        .await
        .unwrap();
    let mut response = Vec::new();
    stream.read_to_end(&mut response).await.unwrap();
    let text = String::from_utf8_lossy(&response);
    assert!(
        text.starts_with("HTTP/1.1 200"),
        "unexpected response: {text}"
    );
    assert!(text.ends_with("ok"), "unexpected body: {text}");

    // Graceful: cancel stops accepting and the serve future returns Ok.
    ct.cancel();
    server.await.unwrap().unwrap();
    let _ = fs::remove_dir_all(&dir);
}
