//! TOTP (RFC 6238) second factor + one-time recovery codes.
//!
//! Pure CPU, no IO: auth-service owns the secret/state in Postgres and calls
//! these to (a) mint an enrollment secret + `otpauth://` URI, (b) verify a
//! presented code against a stored secret, and (c) generate/hash recovery codes.
//!
//! Verification is SHA-1 / 6-digit / 30-second with a ±1-step window (clock
//! skew). [`verify_code`] returns the *matched* time-step so the caller can
//! enforce single-use (RFC 6238 §5.2): reject any code whose step is not
//! strictly newer than the last one consumed, defeating same-window replay.

use sha2::{Digest as _, Sha256};
use subtle::ConstantTimeEq as _;
use totp_rs::{Algorithm, Secret, TOTP};

/// TOTP time-step (seconds).
pub const STEP: u64 = 30;
/// Code length in digits.
pub const DIGITS: usize = 6;
/// Accepted clock-skew window, in steps, on either side of "now".
pub const SKEW_STEPS: i64 = 1;
/// Issuer label shown in the authenticator app / encoded in the URI.
pub const ISSUER: &str = "Dice";
/// Secret length in bytes (160 bits — RFC 4226 recommends >= 128).
const SECRET_BYTES: usize = 20;
/// How many recovery codes a fresh enrollment mints.
pub const RECOVERY_CODE_COUNT: usize = 10;
/// Recovery-code alphabet: Crockford-ish, ambiguous glyphs (0/O/1/I/L) removed.
const RECOVERY_ALPHABET: &[u8] = b"ABCDEFGHJKMNPQRSTVWXYZ23456789";
/// Characters of raw entropy per recovery code (before the readability dash).
const RECOVERY_LEN: usize = 10;

/// A fresh enrollment: the base32 secret to persist + the provisioning URI.
#[derive(Debug, Clone)]
pub struct Enrollment {
    /// Base32 (RFC 4648, no padding) secret — store this, also show for manual
    /// entry.
    pub secret: String,
    /// `otpauth://totp/...` URI the client renders as a QR code.
    pub uri: String,
}

/// Begin enrollment for `account` (shown in the authenticator): random secret +
/// its `otpauth://` URI. Persist [`Enrollment::secret`]; activate only after a
/// code from it verifies.
///
/// # Panics
/// Only if the OS RNG fails (unrecoverable).
pub fn enroll(account: &str) -> Enrollment {
    let mut raw = [0u8; SECRET_BYTES];
    getrandom::fill(&mut raw).expect("operating-system RNG failure");
    let secret = match Secret::Raw(raw.to_vec()).to_encoded() {
        Secret::Encoded(s) => s,
        Secret::Raw(_) => unreachable!("to_encoded always yields Encoded"),
    };
    raw.fill(0);
    // A freshly built TOTP over a valid 160-bit secret and a ':'-free account
    // cannot fail construction; fall back to a bare URI defensively.
    let uri = build(&secret, account)
        .map(|t| t.get_url())
        .unwrap_or_default();
    Enrollment { secret, uri }
}

/// Verify `code` (a 6-digit TOTP) against `secret_b32` at `unix_secs`.
///
/// Returns `Some(step)` — the matched time-step — when a code within the skew
/// window matches, else `None`. The caller MUST reject a step that is not newer
/// than the last consumed one (replay guard) and then persist the returned step.
/// The digit comparison is constant-time.
pub fn verify_code(secret_b32: &str, code: &str, unix_secs: u64) -> Option<u64> {
    let code = code.trim();
    if code.len() != DIGITS || !code.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let totp = build(secret_b32, "verify").ok()?;
    let current = unix_secs / STEP;
    for delta in -SKEW_STEPS..=SKEW_STEPS {
        let step = current.checked_add_signed(delta)?;
        let expected = totp.generate(step * STEP);
        if expected.as_bytes().ct_eq(code.as_bytes()).into() {
            return Some(step);
        }
    }
    None
}

/// The code for `secret_b32` at `unix_secs`. `None` only on a malformed secret.
/// For tools and tests that drive the verifier from the same secret.
pub fn current_code(secret_b32: &str, unix_secs: u64) -> Option<String> {
    Some(build(secret_b32, "code").ok()?.generate(unix_secs))
}

/// Build a SHA-1 / 6-digit / 30 s TOTP over the base32 secret. `Err` only on a
/// malformed/too-short secret or a ':'-bearing account (rejected before storage).
fn build(secret_b32: &str, account: &str) -> Result<TOTP, totp_rs::TotpUrlError> {
    let bytes = Secret::Encoded(secret_b32.to_owned())
        .to_bytes()
        .map_err(|_| totp_rs::TotpUrlError::Secret(secret_b32.to_owned()))?;
    TOTP::new(
        Algorithm::SHA1,
        DIGITS,
        u8::try_from(SKEW_STEPS).unwrap_or(1),
        STEP,
        bytes,
        Some(ISSUER.to_owned()),
        account.to_owned(),
    )
}

/// Mint [`RECOVERY_CODE_COUNT`] formatted recovery codes (e.g. `ABCDE-FGHJK`).
/// Plaintext — shown to the user ONCE; the caller stores only their hashes.
///
/// # Panics
/// Only if the OS RNG fails (unrecoverable).
pub fn generate_recovery_codes() -> Vec<String> {
    (0..RECOVERY_CODE_COUNT)
        .map(|_| {
            let body: String = (0..RECOVERY_LEN)
                .map(|_| RECOVERY_ALPHABET[random_index(RECOVERY_ALPHABET.len())] as char)
                .collect();
            format!(
                "{}-{}",
                &body[..RECOVERY_LEN / 2],
                &body[RECOVERY_LEN / 2..]
            )
        })
        .collect()
}

/// SHA-256 of the normalized code (alphanumerics only, upper-cased) — the only
/// form persisted, and what a presented code is hashed to for lookup. Normalizing
/// lets a user retype with or without the dash / in any case.
pub fn hash_recovery_code(code: &str) -> [u8; 32] {
    let normalized: String = code
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .map(|c| c.to_ascii_uppercase())
        .collect();
    Sha256::digest(normalized.as_bytes()).into()
}

/// Unbiased index in `0..modulus` from the OS RNG (rejection sampling — no
/// modulo bias on the recovery-code charset).
///
/// # Panics
/// Only if the OS RNG fails (unrecoverable).
fn random_index(modulus: usize) -> usize {
    let m = modulus as u16;
    let limit = 256 - (256 % m); // largest multiple of m that fits in a byte
    loop {
        let mut b = [0u8; 1];
        getrandom::fill(&mut b).expect("operating-system RNG failure");
        if u16::from(b[0]) < limit {
            return usize::from(b[0] % m as u8);
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn enroll_then_verify_current_code() {
        let e = enroll("alice");
        assert!(e.uri.starts_with("otpauth://totp/"));
        assert!(e.uri.contains("issuer=Dice"));
        let totp = build(&e.secret, "alice").unwrap();
        let now = 1_700_000_000;
        let code = totp.generate(now);
        assert_eq!(verify_code(&e.secret, &code, now), Some(now / STEP));
    }

    #[test]
    fn skew_window_and_replay_step() {
        let e = enroll("bob");
        let totp = build(&e.secret, "bob").unwrap();
        let now = 1_700_000_000;
        // A code from the previous step still verifies (±1 skew) and reports
        // ITS step, so the caller can reject anything not strictly newer.
        let prev = totp.generate(now - STEP);
        assert_eq!(verify_code(&e.secret, &prev, now), Some(now / STEP - 1));
        // Two steps away is outside the window.
        let old = totp.generate(now - 3 * STEP);
        assert_eq!(verify_code(&e.secret, &old, now), None);
    }

    #[test]
    fn malformed_codes_rejected() {
        let e = enroll("carol");
        let now = 1_700_000_000;
        for bad in ["", "12345", "1234567", "abcdef", "12 456", "ABCDE-FGHJK"] {
            assert_eq!(verify_code(&e.secret, bad, now), None, "{bad:?}");
        }
    }

    #[test]
    fn wrong_secret_does_not_verify() {
        let a = enroll("dave");
        let b = enroll("dave");
        let totp_a = build(&a.secret, "dave").unwrap();
        let now = 1_700_000_000;
        let code = totp_a.generate(now);
        assert_eq!(verify_code(&b.secret, &code, now), None);
    }

    #[test]
    fn recovery_codes_unique_formatted_and_hash_normalizes() {
        let codes = generate_recovery_codes();
        assert_eq!(codes.len(), RECOVERY_CODE_COUNT);
        let set: std::collections::HashSet<_> = codes.iter().collect();
        assert_eq!(set.len(), codes.len(), "no duplicates");
        for c in &codes {
            assert_eq!(c.len(), RECOVERY_LEN + 1, "XXXXX-XXXXX");
            assert!(c.as_bytes()[RECOVERY_LEN / 2] == b'-');
        }
        // Dash / case / spacing are normalized away before hashing.
        let c = &codes[0];
        assert_eq!(
            hash_recovery_code(c),
            hash_recovery_code(&c.replace('-', "").to_lowercase())
        );
        assert_ne!(hash_recovery_code(&codes[0]), hash_recovery_code(&codes[1]));
    }
}
