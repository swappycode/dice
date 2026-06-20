//! App-level symmetric encryption for secrets stored at rest.
//!
//! Some credentials can't be hashed because the server must recover the
//! plaintext to use them — notably the TOTP shared secret (codes are
//! regenerated from it to verify). Those are encrypted with AES-256-GCM under a
//! key derived from the auth signing seed ([`crate::token::JwtKeys::derive_symmetric_key`]),
//! so there is no *separate* key to manage and the scheme is identical across
//! the monolith and the split auth-service (both load the same signing key).
//!
//! Stored form: `v1.<base64-no-pad(nonce ‖ ciphertext‖tag)>` with a fresh random
//! 96-bit nonce per value. A stored value WITHOUT the `v1.` tag is treated as
//! legacy plaintext and returned as-is, so enabling encryption needs no data
//! migration: pre-existing enrollments keep working until re-enrolled, and every
//! new write is encrypted.

use aes_gcm::aead::{Aead as _, KeyInit as _};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD_NO_PAD;

/// Version tag prefixing every encrypted value (see module docs).
const V1: &str = "v1.";
/// AES-GCM nonce length (96 bits — the standard for GCM).
const NONCE_LEN: usize = 12;

/// AES-256-GCM cipher over a 32-byte key, for secret-at-rest values.
#[derive(Clone)]
pub struct SecretCipher {
    key: [u8; 32],
}

/// Failure decrypting a stored value.
#[derive(Debug, thiserror::Error)]
pub enum CipherError {
    /// The `v1.` payload was not valid base64, or was too short to hold a nonce.
    #[error("ciphertext is malformed")]
    Malformed,
    /// AEAD authentication failed — wrong key or tampered data.
    #[error("decryption failed (wrong key or tampered data)")]
    Decrypt,
}

impl SecretCipher {
    /// Build a cipher over a raw 32-byte key (typically from
    /// [`crate::token::JwtKeys::derive_symmetric_key`]).
    #[must_use]
    pub fn from_key(key: [u8; 32]) -> Self {
        Self { key }
    }

    /// Encrypt `plaintext` into the stored form `v1.<base64(nonce‖ct)>`.
    ///
    /// # Panics
    /// Only if the OS RNG fails (unrecoverable). AES-GCM encryption itself
    /// cannot fail for in-bounds inputs.
    #[must_use]
    pub fn encrypt(&self, plaintext: &str) -> String {
        let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&self.key));
        let mut nonce = [0u8; NONCE_LEN];
        getrandom::fill(&mut nonce).expect("operating-system RNG failure");
        let ct = cipher
            .encrypt(Nonce::from_slice(&nonce), plaintext.as_bytes())
            .expect("AES-256-GCM encryption is infallible for valid inputs");
        let mut blob = Vec::with_capacity(NONCE_LEN + ct.len());
        blob.extend_from_slice(&nonce);
        blob.extend_from_slice(&ct);
        format!("{V1}{}", STANDARD_NO_PAD.encode(blob))
    }

    /// Decrypt a value produced by [`Self::encrypt`]. A value WITHOUT the `v1.`
    /// tag is legacy plaintext and is returned unchanged (no migration needed).
    pub fn decrypt(&self, stored: &str) -> Result<String, CipherError> {
        let Some(b64) = stored.strip_prefix(V1) else {
            return Ok(stored.to_owned()); // legacy plaintext (pre-encryption)
        };
        let blob = STANDARD_NO_PAD
            .decode(b64)
            .map_err(|_| CipherError::Malformed)?;
        if blob.len() < NONCE_LEN {
            return Err(CipherError::Malformed);
        }
        let (nonce, ct) = blob.split_at(NONCE_LEN);
        let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&self.key));
        let pt = cipher
            .decrypt(Nonce::from_slice(nonce), ct)
            .map_err(|_| CipherError::Decrypt)?;
        String::from_utf8(pt).map_err(|_| CipherError::Decrypt)
    }

    /// Whether `stored` is an encrypted (`v1.`) value rather than legacy plaintext.
    #[must_use]
    pub fn is_encrypted(stored: &str) -> bool {
        stored.starts_with(V1)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn cipher() -> SecretCipher {
        SecretCipher::from_key([7u8; 32])
    }

    #[test]
    fn round_trips_and_tags_the_ciphertext() {
        let c = cipher();
        let secret = "JBSWY3DPEHPK3PXP";
        let enc = c.encrypt(secret);
        assert!(SecretCipher::is_encrypted(&enc), "tagged v1.");
        assert!(!enc.contains(secret), "plaintext not present in ciphertext");
        assert_eq!(c.decrypt(&enc).unwrap(), secret);
    }

    #[test]
    fn nonce_is_random_per_encryption() {
        let c = cipher();
        assert_ne!(
            c.encrypt("same"),
            c.encrypt("same"),
            "fresh nonce each time"
        );
    }

    #[test]
    fn legacy_plaintext_passes_through() {
        // A pre-encryption base32 secret (no v1. tag) is returned as-is.
        let c = cipher();
        let legacy = "JBSWY3DPEHPK3PXP";
        assert!(!SecretCipher::is_encrypted(legacy));
        assert_eq!(c.decrypt(legacy).unwrap(), legacy);
    }

    #[test]
    fn wrong_key_fails_to_decrypt() {
        let enc = cipher().encrypt("secret");
        let other = SecretCipher::from_key([9u8; 32]);
        assert!(matches!(other.decrypt(&enc), Err(CipherError::Decrypt)));
    }

    #[test]
    fn tampered_ciphertext_is_rejected() {
        let c = cipher();
        let enc = c.encrypt("secret");
        // Flip a character in the base64 body.
        let mut bad = enc.clone();
        let i = bad.len() - 1;
        let last = bad.as_bytes()[i];
        bad.replace_range(i.., if last == b'A' { "B" } else { "A" });
        assert!(c.decrypt(&bad).is_err());
    }

    #[test]
    fn malformed_payload_rejected() {
        let c = cipher();
        assert!(matches!(
            c.decrypt("v1.!!!not-base64"),
            Err(CipherError::Malformed)
        ));
        assert!(
            matches!(c.decrypt("v1.AAAA"), Err(CipherError::Malformed)),
            "too short for a nonce"
        );
    }
}
