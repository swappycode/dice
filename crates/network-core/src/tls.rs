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

fn ring_provider() -> Arc<rustls::crypto::CryptoProvider> {
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

/// Wrap a rustls server config for quinn with the transport tuning from
/// docs/protocol.md §1: `max_concurrent_bidi_streams=4`, stream rx window
/// 1 MiB, connection window 4 MiB, QUIC keep-alive OFF (the 30 s app heartbeat
/// is the keep-alive), `max_idle_timeout` 90 s.
pub fn quic_server_config(tls: Arc<rustls::ServerConfig>) -> Result<quinn::ServerConfig, TlsError> {
    let crypto = quinn::crypto::rustls::QuicServerConfig::try_from(tls)?;
    let mut cfg = quinn::ServerConfig::with_crypto(Arc::new(crypto));

    let mut transport = quinn::TransportConfig::default();
    transport.max_concurrent_bidi_streams(quinn::VarInt::from_u32(4));
    transport.stream_receive_window(quinn::VarInt::from_u32(1024 * 1024));
    transport.receive_window(quinn::VarInt::from_u32(4 * 1024 * 1024));
    transport.keep_alive_interval(None);
    transport.max_idle_timeout(Some(
        quinn::IdleTimeout::try_from(Duration::from_secs(90))
            .map_err(|_| TlsError::TransportParam)?,
    ));
    cfg.transport_config(Arc::new(transport));
    Ok(cfg)
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
}
