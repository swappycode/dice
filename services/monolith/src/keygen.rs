//! dev-keygen (critique resolution #22): when the configured TLS or JWT key
//! files are MISSING the monolith generates and **persists** them, so client
//! trust and minted tokens survive restarts. Canonical dev locations:
//! `dev/certs/{dev-ca.pem,server.crt,server.key}` and
//! `dev/keys/jwt_ed25519{,.pub}.pem` (all gitignored). Production points the
//! `DICE_TLS_*` / `DICE_JWT_*` vars at real files, which are then never
//! touched.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Context as _;
use dice_auth_core::token::JwtKeys;
use dice_network_core::tls::generate_dev_certs;

/// Effective TLS file paths after dev-keygen.
pub struct TlsPaths {
    pub cert: PathBuf,
    pub key: PathBuf,
    /// `Some` when dev certs were generated/found by the generator — the
    /// root the desktop client must trust (`DICE_DEV_CA`).
    pub dev_ca: Option<PathBuf>,
}

/// Use the configured cert/key when both exist; otherwise generate a dev CA
/// plus a CA-signed leaf (SANs `localhost`, `127.0.0.1`, `::1`) into the
/// cert's parent directory and use the canonical generated names.
pub fn ensure_tls(cert: &Path, key: &Path) -> anyhow::Result<TlsPaths> {
    if cert.is_file() && key.is_file() {
        return Ok(TlsPaths {
            cert: cert.to_path_buf(),
            key: key.to_path_buf(),
            dev_ca: None,
        });
    }
    let dir = cert
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("dev/certs"));
    let generated = generate_dev_certs(dir)
        .with_context(|| format!("generate dev TLS certs into {}", dir.display()))?;
    tracing::info!(
        ca = %generated.ca_pem.display(),
        cert = %generated.server_cert.display(),
        "dev TLS assets generated (configured files were missing)"
    );
    if generated.server_cert != cert || generated.server_key != key {
        tracing::warn!(
            configured_cert = %cert.display(),
            effective_cert = %generated.server_cert.display(),
            "configured TLS paths missing; using generated dev cert paths"
        );
    }
    Ok(TlsPaths {
        cert: generated.server_cert,
        key: generated.server_key,
        dev_ca: Some(generated.ca_pem),
    })
}

/// Load the Ed25519 JWT pair from the configured PEM files, generating and
/// persisting a fresh pair first when either file is missing.
///
/// The monolith mounts auth-service in-process, so the loaded keys carry the
/// PRIVATE half (signing); the gateway only ever calls verify.
pub fn ensure_jwt(private_pem: &Path, public_pem: &Path) -> anyhow::Result<Arc<JwtKeys>> {
    if !(private_pem.is_file() && public_pem.is_file()) {
        let (priv_pem, pub_pem) = JwtKeys::generate_pems();
        for (path, contents) in [(private_pem, &priv_pem), (public_pem, &pub_pem)] {
            if let Some(dir) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
                std::fs::create_dir_all(dir)
                    .with_context(|| format!("create key directory {}", dir.display()))?;
            }
            std::fs::write(path, contents).with_context(|| format!("write {}", path.display()))?;
        }
        tracing::info!(
            private = %private_pem.display(),
            public = %public_pem.display(),
            "dev JWT keypair generated (configured files were missing)"
        );
    }
    let private =
        std::fs::read(private_pem).with_context(|| format!("read {}", private_pem.display()))?;
    let public =
        std::fs::read(public_pem).with_context(|| format!("read {}", public_pem.display()))?;
    let keys = JwtKeys::from_pem(&private, &public).context("parse JWT PEM pair")?;
    anyhow::ensure!(
        keys.can_sign(),
        "JWT private key did not load as signing-capable"
    );
    Ok(Arc::new(keys))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use dice_auth_core::token::{sign_access, verify_access};
    use dice_common::{SessionId, UserId};

    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "dice-monolith-{tag}-{}-{nanos}",
            std::process::id()
        ))
    }

    #[test]
    fn ensure_tls_generates_then_reuses() {
        let dir = temp_dir("tls");
        let cert = dir.join("server.crt");
        let key = dir.join("server.key");

        let first = ensure_tls(&cert, &key).unwrap();
        assert!(first.cert.is_file());
        assert!(first.key.is_file());
        let ca = first.dev_ca.expect("generated run reports the dev CA");
        assert!(ca.is_file());
        let cert_bytes = std::fs::read(&first.cert).unwrap();

        // Second boot: files exist, nothing is regenerated.
        let second = ensure_tls(&cert, &key).unwrap();
        assert_eq!(second.cert, cert);
        assert!(second.dev_ca.is_none());
        assert_eq!(std::fs::read(&second.cert).unwrap(), cert_bytes);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ensure_jwt_generates_persists_and_signs() {
        let dir = temp_dir("jwt");
        let private = dir.join("jwt_ed25519.pem");
        let public = dir.join("jwt_ed25519.pub.pem");

        let keys = ensure_jwt(&private, &public).unwrap();
        assert!(keys.can_sign());
        assert!(private.is_file());
        assert!(public.is_file());

        // Round trip: a token signed now verifies with a SECOND load of the
        // same files (persistence is the whole point of #22).
        let token = sign_access(&keys, UserId::from_raw(7), SessionId::from_raw(9)).unwrap();
        let reloaded = ensure_jwt(&private, &public).unwrap();
        let claims = verify_access(&reloaded, &token).unwrap();
        assert_eq!(claims.user_id(), Some(UserId::from_raw(7)));

        // And the second load did not rewrite the files.
        let priv_bytes = std::fs::read(&private).unwrap();
        let _ = ensure_jwt(&private, &public).unwrap();
        assert_eq!(std::fs::read(&private).unwrap(), priv_bytes);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
