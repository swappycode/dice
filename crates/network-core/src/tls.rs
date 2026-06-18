//! Shared TLS building blocks — ring provider ONLY (workspace policy: nothing
//! that pulls aws-lc-sys). Used by both the server half (quinn + tokio-rustls
//! accept loops) and, later, the client half.

use std::fs;
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use rustls_pki_types::{CertificateDer, PrivateKeyDer};

#[derive(Debug, thiserror::Error)]
pub enum TlsError {
    #[error("i/o on {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("no certificates found in {0}")]
    NoCertificate(PathBuf),
    #[error("no private key found in {0}")]
    NoPrivateKey(PathBuf),
    #[error("rustls: {0}")]
    Rustls(#[from] rustls::Error),
    #[error("dev certificate generation: {0}")]
    Keygen(#[from] rcgen::Error),
    #[error("TLS config unusable for QUIC (TLS 1.3 cipher suites required): {0}")]
    Quic(#[from] quinn::crypto::rustls::NoInitialCipherSuite),
    #[error("QUIC transport parameter out of range")]
    TransportParam,
}

/// Install the rustls **ring** provider as the process-default crypto
/// provider. Idempotent: a second call (or a racing sibling) is a no-op.
pub fn install_ring_provider() {
    // Err(_) == AlreadyInstalled. Only ring is compiled into this workspace,
    // so "already installed" can only mean ring is already the default.
    let _ = rustls::crypto::ring::default_provider().install_default();
}

pub(crate) fn ring_provider() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

fn io_err(path: &Path) -> impl FnOnce(std::io::Error) -> TlsError + '_ {
    move |source| TlsError::Io {
        path: path.to_path_buf(),
        source,
    }
}

/// Load certificate chain + private key PEM files into a TLS 1.3-only rustls
/// server config (no client auth) with the given ALPN protocols.
pub fn load_server_config(
    cert_pem_path: &Path,
    key_pem_path: &Path,
    alpn: &[&[u8]],
) -> Result<Arc<rustls::ServerConfig>, TlsError> {
    let certs = load_certs(cert_pem_path)?;
    let key = load_private_key(key_pem_path)?;

    let mut cfg = rustls::ServerConfig::builder_with_provider(ring_provider())
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .with_no_client_auth()
        .with_single_cert(certs, key)?;
    cfg.alpn_protocols = alpn.iter().map(|p| p.to_vec()).collect();
    Ok(Arc::new(cfg))
}

/// All certificates from a PEM file (leaf first for a server chain).
pub fn load_certs(path: &Path) -> Result<Vec<CertificateDer<'static>>, TlsError> {
    let file = fs::File::open(path).map_err(io_err(path))?;
    let certs = rustls_pemfile::certs(&mut BufReader::new(file))
        .collect::<Result<Vec<_>, _>>()
        .map_err(io_err(path))?;
    if certs.is_empty() {
        return Err(TlsError::NoCertificate(path.to_path_buf()));
    }
    Ok(certs)
}

fn load_private_key(path: &Path) -> Result<PrivateKeyDer<'static>, TlsError> {
    let file = fs::File::open(path).map_err(io_err(path))?;
    rustls_pemfile::private_key(&mut BufReader::new(file))
        .map_err(io_err(path))?
        .ok_or_else(|| TlsError::NoPrivateKey(path.to_path_buf()))
}

/// Where [`generate_dev_certs`] put (or found) the dev TLS assets.
///
/// Canonical dev location is `dev/certs/` (critique resolution #22); the
/// monolith passes that directory in dev profiles.
#[derive(Debug, Clone)]
pub struct DevCertPaths {
    /// `dev-ca.pem` — the root the CLIENT must trust (`DICE_DEV_CA`).
    pub ca_pem: PathBuf,
    /// `server.crt` — leaf + CA chain, PEM.
    pub server_cert: PathBuf,
    /// `server.key` — leaf private key, PKCS#8 PEM.
    pub server_key: PathBuf,
}

/// Generate-and-persist a dev CA (CN "Dice Dev CA") plus a CA-signed leaf with
/// SANs `DNS:localhost`, `IP:127.0.0.1`, `IP:::1` into `dir`.
///
/// Idempotent: if all three files already exist the paths are returned without
/// regenerating (client trust and server identity survive restarts).
pub fn generate_dev_certs(dir: &Path) -> Result<DevCertPaths, TlsError> {
    let paths = DevCertPaths {
        ca_pem: dir.join("dev-ca.pem"),
        server_cert: dir.join("server.crt"),
        server_key: dir.join("server.key"),
    };
    if paths.ca_pem.is_file() && paths.server_cert.is_file() && paths.server_key.is_file() {
        return Ok(paths);
    }
    fs::create_dir_all(dir).map_err(io_err(dir))?;

    // CA: self-signed, CN "Dice Dev CA".
    let mut ca_params = rcgen::CertificateParams::default();
    ca_params
        .distinguished_name
        .push(rcgen::DnType::CommonName, "Dice Dev CA");
    ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    ca_params.key_usages = vec![
        rcgen::KeyUsagePurpose::KeyCertSign,
        rcgen::KeyUsagePurpose::CrlSign,
    ];
    let ca_key = rcgen::KeyPair::generate()?;
    let ca_cert = ca_params.self_signed(&ca_key)?;

    // Leaf signed by the CA. rcgen parses IP-shaped strings into IP SANs.
    let mut leaf_params = rcgen::CertificateParams::new(vec![
        "localhost".to_string(),
        "127.0.0.1".to_string(),
        "::1".to_string(),
    ])?;
    leaf_params
        .distinguished_name
        .push(rcgen::DnType::CommonName, "localhost");
    let leaf_key = rcgen::KeyPair::generate()?;
    let leaf_cert = leaf_params.signed_by(&leaf_key, &ca_cert, &ca_key)?;

    fs::write(&paths.ca_pem, ca_cert.pem()).map_err(io_err(&paths.ca_pem))?;
    fs::write(
        &paths.server_cert,
        format!("{}{}", leaf_cert.pem(), ca_cert.pem()),
    )
    .map_err(io_err(&paths.server_cert))?;
    fs::write(&paths.server_key, leaf_key.serialize_pem()).map_err(io_err(&paths.server_key))?;
    Ok(paths)
}

/// QUIC **server** transport tuning. [`Default`] reproduces the docs/protocol.md
/// §1 production values exactly, so `quic_server_config_tuned(tls,
/// &QuicServerTuning::default())` is byte-for-byte equivalent to the historical
/// [`quic_server_config`]. The api-gateway threads env-driven overrides through
/// here for the 100k-connection benchmark (M4 scaling): the per-connection
/// `receive_window` is the dominant memory term at scale, disabling `datagrams`
/// drops the per-connection voice buffers, and the UDP `socket_*_buffer` sizes
/// keep the kernel from dropping GSO-batched sends/receives on Linux.
#[derive(Debug, Clone)]
pub struct QuicServerTuning {
    /// Per-stream flow-control receive window (bytes). Default 1 MiB.
    pub stream_receive_window: u32,
    /// Per-connection flow-control receive window (bytes). Default 4 MiB — the
    /// dominant per-connection memory ceiling at 100k; shrink it for the bench.
    pub receive_window: u32,
    /// Transport idle timeout (ms). Default 90_000.
    pub max_idle_timeout_ms: u32,
    /// Max concurrent bidi streams the peer may open. Default 4 (Dice uses one
    /// control stream).
    pub max_concurrent_bidi_streams: u32,
    /// Max concurrent uni streams the peer may open. `None` leaves quinn's
    /// default (100); Dice opens none, so `Some(0)` shaves per-connection state.
    pub max_concurrent_uni_streams: Option<u32>,
    /// QUIC datagrams (voice transport). `true` keeps the 64 KiB datagram
    /// buffers; `false` disables datagram support entirely — no voice, but it
    /// saves ~128 KiB/conn, worth it for a control-only connection benchmark.
    pub datagrams: bool,
    /// UDP socket send buffer (SO_SNDBUF) in bytes; `None` = OS default.
    /// Applied at endpoint bind time (see [`crate::server::QuicAcceptor`]).
    pub socket_send_buffer: Option<usize>,
    /// UDP socket receive buffer (SO_RCVBUF) in bytes; `None` = OS default.
    pub socket_recv_buffer: Option<usize>,
}

impl Default for QuicServerTuning {
    fn default() -> Self {
        Self {
            stream_receive_window: 1024 * 1024,
            receive_window: 4 * 1024 * 1024,
            max_idle_timeout_ms: 90_000,
            max_concurrent_bidi_streams: 4,
            max_concurrent_uni_streams: None,
            datagrams: true,
            socket_send_buffer: None,
            socket_recv_buffer: None,
        }
    }
}

/// Wrap a rustls server config for quinn with the default docs/protocol.md §1
/// transport tuning (`max_concurrent_bidi_streams=4`, stream rx window 1 MiB,
/// connection window 4 MiB, QUIC keep-alive OFF, `max_idle_timeout` 90 s).
pub fn quic_server_config(tls: Arc<rustls::ServerConfig>) -> Result<quinn::ServerConfig, TlsError> {
    quic_server_config_tuned(tls, &QuicServerTuning::default())
}

/// Wrap a rustls server config for quinn with explicit [`QuicServerTuning`] (the
/// 100k-benchmark knobs). The UDP socket-buffer fields are applied at endpoint
/// bind time, not here (see [`crate::server::QuicAcceptor::bind_tuned`]).
pub fn quic_server_config_tuned(
    tls: Arc<rustls::ServerConfig>,
    tuning: &QuicServerTuning,
) -> Result<quinn::ServerConfig, TlsError> {
    let crypto = quinn::crypto::rustls::QuicServerConfig::try_from(tls)?;
    let mut cfg = quinn::ServerConfig::with_crypto(Arc::new(crypto));
    cfg.transport_config(Arc::new(server_transport_config(tuning)?));
    Ok(cfg)
}

/// Build the server-side [`quinn::TransportConfig`] from the tuning struct.
fn server_transport_config(t: &QuicServerTuning) -> Result<quinn::TransportConfig, TlsError> {
    let mut transport = quinn::TransportConfig::default();
    transport.max_concurrent_bidi_streams(quinn::VarInt::from_u32(t.max_concurrent_bidi_streams));
    if let Some(uni) = t.max_concurrent_uni_streams {
        transport.max_concurrent_uni_streams(quinn::VarInt::from_u32(uni));
    }
    transport.stream_receive_window(quinn::VarInt::from_u32(t.stream_receive_window));
    transport.receive_window(quinn::VarInt::from_u32(t.receive_window));
    transport.keep_alive_interval(None);
    transport.max_idle_timeout(Some(
        quinn::IdleTimeout::try_from(Duration::from_millis(u64::from(t.max_idle_timeout_ms)))
            .map_err(|_| TlsError::TransportParam)?,
    ));
    if t.datagrams {
        transport.datagram_receive_buffer_size(Some(64 * 1024));
        transport.datagram_send_buffer_size(64 * 1024);
    } else {
        // `None` disables datagram support (advertises no datagram transport
        // parameter, so the peer never sends one).
        transport.datagram_receive_buffer_size(None);
    }
    Ok(transport)
}

/// Wrap a rustls client config (TLS 1.3, ALPN `dice/1`) for quinn with the
/// client half of the docs/protocol.md §1 tuning: QUIC keep-alive OFF (the
/// 30 s app heartbeat is the keep-alive), `max_idle_timeout` 90 s, 0-RTT
/// disabled (an Identify token must never be replayable; rustls keeps early
/// data off by default and quinn only attempts it via the opt-in
/// `into_0rtt`, which the client transport never calls).
pub fn quic_client_config(tls: Arc<rustls::ClientConfig>) -> Result<quinn::ClientConfig, TlsError> {
    let crypto = quinn::crypto::rustls::QuicClientConfig::try_from(tls)?;
    let mut cfg = quinn::ClientConfig::new(Arc::new(crypto));
    cfg.transport_config(Arc::new(quic_transport_config()?));
    Ok(cfg)
}

/// The §1 transport knobs for the **client** half: keep-alive OFF, idle
/// timeout 90 s. (The server half is [`server_transport_config`], which the
/// gateway tunes for the 100k benchmark — these stay fixed for the desktop
/// client.) Stream/window limits only bind the receive side; the defaults
/// comfortably exceed one 256 KiB control stream.
fn quic_transport_config() -> Result<quinn::TransportConfig, TlsError> {
    let mut transport = quinn::TransportConfig::default();
    transport.max_concurrent_bidi_streams(quinn::VarInt::from_u32(4));
    transport.stream_receive_window(quinn::VarInt::from_u32(1024 * 1024));
    transport.receive_window(quinn::VarInt::from_u32(4 * 1024 * 1024));
    transport.keep_alive_interval(None);
    transport.max_idle_timeout(Some(
        quinn::IdleTimeout::try_from(Duration::from_secs(90))
            .map_err(|_| TlsError::TransportParam)?,
    ));
    // Voice rides QUIC datagrams (M3, docs/protocol.md §1: 64 KiB datagram
    // receive buffer). Setting a receive buffer advertises datagram support in
    // the transport parameters, so the peer may send; both directions enable it.
    transport.datagram_receive_buffer_size(Some(64 * 1024));
    transport.datagram_send_buffer_size(64 * 1024);
    Ok(transport)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use rustls::client::danger::ServerCertVerifier as _;
    use rustls_pki_types::{ServerName, UnixTime};

    fn temp_dir(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "dice-network-core-{tag}-{}-{nanos}",
            std::process::id()
        ))
    }

    #[test]
    fn dev_certs_created_and_idempotent() {
        let dir = temp_dir("devcerts");
        let paths = generate_dev_certs(&dir).unwrap();
        assert!(paths.ca_pem.is_file());
        assert!(paths.server_cert.is_file());
        assert!(paths.server_key.is_file());

        let first_cert = fs::read(&paths.server_cert).unwrap();
        let first_ca = fs::read(&paths.ca_pem).unwrap();
        // Second call must not regenerate (same bytes => same keys/serials).
        let again = generate_dev_certs(&dir).unwrap();
        assert_eq!(fs::read(&again.server_cert).unwrap(), first_cert);
        assert_eq!(fs::read(&again.ca_pem).unwrap(), first_ca);

        // server.crt carries leaf + CA chain.
        assert_eq!(load_certs(&paths.server_cert).unwrap().len(), 2);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_server_config_round_trip() {
        let dir = temp_dir("loadcfg");
        let paths = generate_dev_certs(&dir).unwrap();
        let cfg = load_server_config(
            &paths.server_cert,
            &paths.server_key,
            &[dice_protocol::ALPN_GATEWAY, b"h3"],
        )
        .unwrap();
        assert_eq!(cfg.alpn_protocols, vec![b"dice/1".to_vec(), b"h3".to_vec()]);
        let _ = fs::remove_dir_all(&dir);
    }

    /// Real SAN validation via webpki: the leaf must verify against the dev CA
    /// for DNS:localhost, IP:127.0.0.1 and IP:::1 — and NOT for other names.
    #[test]
    fn leaf_sans_verify_against_dev_ca() {
        install_ring_provider();
        let dir = temp_dir("sans");
        let paths = generate_dev_certs(&dir).unwrap();

        let mut roots = rustls::RootCertStore::empty();
        for cert in load_certs(&paths.ca_pem).unwrap() {
            roots.add(cert).unwrap();
        }
        let verifier = rustls::client::WebPkiServerVerifier::builder_with_provider(
            Arc::new(roots),
            ring_provider(),
        )
        .build()
        .unwrap();

        let chain = load_certs(&paths.server_cert).unwrap();
        let (leaf, intermediates) = chain.split_first().unwrap();
        let now = UnixTime::now();

        for name in ["localhost", "127.0.0.1", "::1"] {
            let server_name = ServerName::try_from(name).unwrap();
            verifier
                .verify_server_cert(leaf, intermediates, &server_name, &[], now)
                .unwrap_or_else(|e| panic!("SAN {name} should verify: {e}"));
        }
        let wrong = ServerName::try_from("example.com").unwrap();
        assert!(
            verifier
                .verify_server_cert(leaf, intermediates, &wrong, &[], now)
                .is_err()
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn quic_server_config_builds() {
        install_ring_provider();
        let dir = temp_dir("quiccfg");
        let paths = generate_dev_certs(&dir).unwrap();
        let tls = load_server_config(
            &paths.server_cert,
            &paths.server_key,
            &[dice_protocol::ALPN_GATEWAY],
        )
        .unwrap();
        quic_server_config(tls).unwrap();
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn quic_client_config_builds_from_a_tls13_config() {
        install_ring_provider();
        let dir = temp_dir("quicclientcfg");
        let paths = generate_dev_certs(&dir).unwrap();
        let mut roots = rustls::RootCertStore::empty();
        for cert in load_certs(&paths.ca_pem).unwrap() {
            roots.add(cert).unwrap();
        }
        let mut tls = rustls::ClientConfig::builder_with_provider(ring_provider())
            .with_protocol_versions(&[&rustls::version::TLS13])
            .unwrap()
            .with_root_certificates(roots)
            .with_no_client_auth();
        tls.alpn_protocols = vec![dice_protocol::ALPN_GATEWAY.to_vec()];
        assert!(!tls.enable_early_data, "0-RTT must stay disabled");
        quic_client_config(Arc::new(tls)).unwrap();
        let _ = fs::remove_dir_all(&dir);
    }

    /// Voice rides QUIC datagrams; prove the real server+client transport
    /// configs advertise datagram support and a packet flows both directions
    /// over a live connection (the device-free half of the SFU transport).
    #[tokio::test]
    async fn quic_datagrams_round_trip_over_the_real_configs() {
        use std::net::Ipv4Addr;

        use bytes::Bytes;

        install_ring_provider();
        let dir = temp_dir("quicdatagram");
        let paths = generate_dev_certs(&dir).unwrap();

        let server_tls = load_server_config(
            &paths.server_cert,
            &paths.server_key,
            &[dice_protocol::ALPN_GATEWAY],
        )
        .unwrap();
        let server = quinn::Endpoint::server(
            quic_server_config(server_tls).unwrap(),
            (Ipv4Addr::LOCALHOST, 0).into(),
        )
        .unwrap();
        let server_addr = server.local_addr().unwrap();

        let mut roots = rustls::RootCertStore::empty();
        for cert in load_certs(&paths.ca_pem).unwrap() {
            roots.add(cert).unwrap();
        }
        let mut client_tls = rustls::ClientConfig::builder_with_provider(ring_provider())
            .with_protocol_versions(&[&rustls::version::TLS13])
            .unwrap()
            .with_root_certificates(roots)
            .with_no_client_auth();
        client_tls.alpn_protocols = vec![dice_protocol::ALPN_GATEWAY.to_vec()];
        let mut client = quinn::Endpoint::client((Ipv4Addr::UNSPECIFIED, 0).into()).unwrap();
        client.set_default_client_config(quic_client_config(Arc::new(client_tls)).unwrap());

        // The dev leaf carries a 127.0.0.1 IP SAN, so verify against that name.
        let (client_conn, server_conn) = tokio::join!(
            async {
                client
                    .connect(server_addr, "127.0.0.1")
                    .unwrap()
                    .await
                    .unwrap()
            },
            async { server.accept().await.unwrap().await.unwrap() },
        );

        client_conn
            .send_datagram(Bytes::from_static(b"ping"))
            .unwrap();
        let got = tokio::time::timeout(Duration::from_secs(2), server_conn.read_datagram())
            .await
            .expect("server datagram timed out")
            .unwrap();
        assert_eq!(&got[..], b"ping");

        server_conn
            .send_datagram(Bytes::from_static(b"pong"))
            .unwrap();
        let got = tokio::time::timeout(Duration::from_secs(2), client_conn.read_datagram())
            .await
            .expect("client datagram timed out")
            .unwrap();
        assert_eq!(&got[..], b"pong");

        let _ = fs::remove_dir_all(&dir);
    }
}
