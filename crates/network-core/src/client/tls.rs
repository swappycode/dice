//! Client trust configuration: webpki real-world roots plus optional extra
//! anchors loaded from a PEM file — the dev CA in dev profiles
//! (`DICE_DEV_CA`, written by the monolith to `dev/certs/dev-ca.pem`).
//!
//! Full chain + hostname verification ALWAYS runs (critique resolution: dev
//! trust is an extra anchor, never a verification-off switch). One
//! [`TlsOptions`] feeds both the tungstenite rustls config and the reqwest
//! builder so WSS and HTTPS can never trust different roots.

use std::path::PathBuf;
use std::sync::Arc;

use crate::tls::{TlsError, load_certs, ring_provider};

/// Environment variable naming the extra CA PEM (dev profiles only).
pub const DEV_CA_ENV: &str = "DICE_DEV_CA";

/// Trust knobs for the client half.
#[derive(Debug, Clone, Default)]
pub struct TlsOptions {
    /// Extra trust anchors (PEM, may hold several certificates) appended to
    /// the webpki root store. `None` = real-world roots only.
    pub extra_ca_pem: Option<PathBuf>,
}

impl TlsOptions {
    /// Read [`DEV_CA_ENV`] (`DICE_DEV_CA`); unset/empty means no extra
    /// anchors.
    pub fn from_env() -> Self {
        Self::from_env_value(std::env::var_os(DEV_CA_ENV))
    }

    /// Testable core of [`Self::from_env`] (env mutation is unsafe in
    /// edition 2024, so tests inject the value instead).
    pub(crate) fn from_env_value(value: Option<std::ffi::OsString>) -> Self {
        Self {
            extra_ca_pem: value.filter(|v| !v.is_empty()).map(PathBuf::from),
        }
    }

    /// Root store = webpki-roots + the extra anchors, if any.
    pub fn root_store(&self) -> Result<rustls::RootCertStore, TlsError> {
        let mut roots = rustls::RootCertStore::empty();
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        if let Some(path) = &self.extra_ca_pem {
            for cert in load_certs(path)? {
                roots.add(cert)?;
            }
        }
        Ok(roots)
    }

    /// rustls client config for the WSS transport: ring provider, TLS 1.3
    /// only, ALPN `http/1.1` (the REST/WSS port negotiates http/1.1; the WS
    /// upgrade rides it). Shared via `Arc` so reconnects reuse it.
    pub fn client_config(&self) -> Result<Arc<rustls::ClientConfig>, TlsError> {
        let roots = self.root_store()?;
        let mut cfg = rustls::ClientConfig::builder_with_provider(ring_provider())
            .with_protocol_versions(&[&rustls::version::TLS13])?
            .with_root_certificates(roots)
            .with_no_client_auth();
        cfg.alpn_protocols = vec![b"http/1.1".to_vec()];
        Ok(Arc::new(cfg))
    }

    /// quinn client config for the QUIC transport: the SAME root store as
    /// WSS/HTTPS (webpki + extra anchors), ring provider, TLS 1.3 only, ALPN
    /// `dice/1`, and the protocol §1 transport tuning (keep-alive OFF, idle
    /// 90 s, 0-RTT disabled). Full chain + server-name verification always
    /// runs — there is deliberately NO bypass.
    pub fn quic_client_config(&self) -> Result<quinn::ClientConfig, TlsError> {
        crate::tls::quic_client_config(self.quic_tls_config()?)
    }

    /// rustls layer of [`Self::quic_client_config`] (split out so tests can
    /// assert the ALPN).
    fn quic_tls_config(&self) -> Result<Arc<rustls::ClientConfig>, TlsError> {
        let roots = self.root_store()?;
        let mut cfg = rustls::ClientConfig::builder_with_provider(ring_provider())
            .with_protocol_versions(&[&rustls::version::TLS13])?
            .with_root_certificates(roots)
            .with_no_client_auth();
        cfg.alpn_protocols = vec![dice_protocol::ALPN_GATEWAY.to_vec()];
        Ok(Arc::new(cfg))
    }

    /// Apply this trust configuration to a reqwest builder: built-in webpki
    /// roots stay ON and the extra anchors are added on top
    /// (`use_preconfigured_tls` is deliberately avoided — it pins reqwest to
    /// an exact rustls version and silently breaks on upgrades).
    pub fn apply_to_reqwest(
        &self,
        mut builder: reqwest::ClientBuilder,
    ) -> Result<reqwest::ClientBuilder, TlsError> {
        builder = builder.tls_built_in_root_certs(true);
        if let Some(path) = &self.extra_ca_pem {
            for cert in load_certs(path)? {
                let cert = reqwest::Certificate::from_der(&cert)
                    .map_err(|e| TlsError::Rustls(rustls::Error::General(e.to_string())))?;
                builder = builder.add_root_certificate(cert);
            }
        }
        Ok(builder)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::tls::generate_dev_certs;

    fn temp_dir(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "dice-client-tls-{tag}-{}-{nanos}",
            std::process::id()
        ))
    }

    #[test]
    fn from_env_value_handles_unset_empty_and_set() {
        assert!(TlsOptions::from_env_value(None).extra_ca_pem.is_none());
        assert!(
            TlsOptions::from_env_value(Some(std::ffi::OsString::new()))
                .extra_ca_pem
                .is_none()
        );
        let opts = TlsOptions::from_env_value(Some("dev/certs/dev-ca.pem".into()));
        assert_eq!(
            opts.extra_ca_pem.as_deref(),
            Some(std::path::Path::new("dev/certs/dev-ca.pem"))
        );
    }

    #[test]
    fn extra_anchor_lands_on_top_of_webpki_roots() {
        crate::tls::install_ring_provider();
        let dir = temp_dir("anchors");
        let certs = generate_dev_certs(&dir).unwrap();

        let plain = TlsOptions::default();
        let baseline = plain.root_store().unwrap().roots.len();
        assert!(baseline > 100, "webpki roots must be present");

        let with_dev = TlsOptions {
            extra_ca_pem: Some(certs.ca_pem.clone()),
        };
        let augmented = with_dev.root_store().unwrap().roots.len();
        assert_eq!(augmented, baseline + 1, "exactly the dev CA was appended");

        let cfg = with_dev.client_config().unwrap();
        assert_eq!(cfg.alpn_protocols, vec![b"http/1.1".to_vec()]);

        // The QUIC flavor shares the trust config but negotiates dice/1 and
        // keeps 0-RTT off; the quinn wrapper accepts it (TLS 1.3 only).
        let quic_tls = with_dev.quic_tls_config().unwrap();
        assert_eq!(
            quic_tls.alpn_protocols,
            vec![dice_protocol::ALPN_GATEWAY.to_vec()]
        );
        assert!(!quic_tls.enable_early_data, "0-RTT must stay disabled");
        with_dev.quic_client_config().unwrap();

        // reqwest accepts the same anchors.
        let builder = with_dev
            .apply_to_reqwest(reqwest::Client::builder())
            .unwrap();
        builder.build().unwrap();

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_pem_path_is_an_io_error() {
        let opts = TlsOptions {
            extra_ca_pem: Some(PathBuf::from("Z:/does/not/exist.pem")),
        };
        assert!(matches!(opts.root_store(), Err(TlsError::Io { .. })));
    }
}
