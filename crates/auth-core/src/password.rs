//! Argon2id password hashing (OWASP cheat-sheet parameters).
//!
//! Parameters: Argon2id, v19, m = 19456 KiB (19 MiB), t = 2, p = 1.
//!
//! # CPU cost — async callers MUST `spawn_blocking`
//!
//! [`hash`], [`verify`], and [`dummy_verify`] are **CPU-bound by design**:
//! each call performs the full Argon2id key derivation (tens of milliseconds,
//! 19 MiB of memory). Callers on an async runtime MUST wrap them in
//! `tokio::task::spawn_blocking` (or an equivalent dedicated blocking pool);
//! running them on a runtime worker thread stalls every connection scheduled
//! on that thread.
//!
//! The salt RNG is `password_hash`'s own getrandom-backed
//! [`rand_core::OsRng`](password_hash::rand_core::OsRng) — never feed the
//! workspace `rand` 0.9 RNG into these APIs (rand_core version mismatch).

use argon2::{Algorithm, Argon2, Params, Version};
use password_hash::{PasswordHash, PasswordHasher as _, PasswordVerifier as _, SaltString};

/// Argon2 memory cost in KiB (19 MiB).
pub const M_COST_KIB: u32 = 19_456;
/// Argon2 iteration count.
pub const T_COST: u32 = 2;
/// Argon2 degree of parallelism.
pub const P_COST: u32 = 1;

/// Errors from password hashing/verification.
///
/// Note: a *wrong password* is NOT an error — [`verify`] returns `Ok(false)`.
#[derive(Debug, thiserror::Error)]
pub enum PasswordError {
    /// The compile-time Argon2 parameters were rejected. Cannot happen with
    /// the shipped constants; surfaced instead of panicking.
    #[error("invalid argon2 parameters: {0}")]
    Params(argon2::Error),
    /// Hashing or verification failed for a reason other than "wrong password"
    /// (e.g. unsupported algorithm in the stored hash).
    #[error("password hashing failed: {0}")]
    Hash(password_hash::Error),
    /// The stored hash is not a parseable PHC string.
    #[error("stored password hash is not a valid PHC string: {0}")]
    InvalidPhc(password_hash::Error),
}

/// Argon2id context with the production parameters.
fn hasher() -> Result<Argon2<'static>, PasswordError> {
    let params = Params::new(M_COST_KIB, T_COST, P_COST, None).map_err(PasswordError::Params)?;
    Ok(Argon2::new(Algorithm::Argon2id, Version::V0x13, params))
}

/// Hash `password` with Argon2id (m=19456 KiB, t=2, p=1) and a fresh random
/// salt; returns the PHC string (`$argon2id$v=19$...`) to store verbatim.
///
/// CPU-bound — async callers MUST run this under `spawn_blocking` (see module
/// docs).
pub fn hash(password: &str) -> Result<String, PasswordError> {
    let salt = SaltString::generate(&mut password_hash::rand_core::OsRng);
    let phc = hasher()?
        .hash_password(password.as_bytes(), &salt)
        .map_err(PasswordError::Hash)?;
    Ok(phc.to_string())
}

/// Verify `password` against a stored PHC string.
///
/// Returns `Ok(true)` on match, `Ok(false)` on mismatch, and `Err(_)` only
/// when the stored hash itself is unusable. Parameters are taken from the PHC
/// string, so verification keeps working across future parameter bumps.
///
/// CPU-bound — async callers MUST run this under `spawn_blocking` (see module
/// docs).
pub fn verify(password: &str, phc: &str) -> Result<bool, PasswordError> {
    let parsed = PasswordHash::new(phc).map_err(PasswordError::InvalidPhc)?;
    match hasher()?.verify_password(password.as_bytes(), &parsed) {
        Ok(()) => Ok(true),
        Err(password_hash::Error::Password) => Ok(false),
        Err(e) => Err(PasswordError::Hash(e)),
    }
}

/// A structurally valid Argon2id PHC string with the production parameters,
/// an all-zero salt, and an all-zero (never-matching) output. Verifying
/// against it performs the full key derivation, so its timing matches a real
/// verification.
const DUMMY_PHC: &str = "$argon2id$v=19$m=19456,t=2,p=1$AAAAAAAAAAAAAAAAAAAAAA$AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";

/// Burn exactly one verification's worth of CPU and discard the result.
///
/// Call this on login when the user does NOT exist so the response time is
/// indistinguishable from a real password check (user-enumeration defense).
///
/// CPU-bound — async callers MUST run this under `spawn_blocking` (see module
/// docs).
pub fn dummy_verify() {
    let _ = verify("dice-dummy-password-for-constant-timing", DUMMY_PHC);
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn hash_verify_round_trip() {
        let phc = hash("correct horse battery staple").unwrap();
        assert!(verify("correct horse battery staple", &phc).unwrap());
    }

    #[test]
    fn wrong_password_is_ok_false() {
        let phc = hash("hunter2").unwrap();
        assert!(!verify("hunter3", &phc).unwrap());
        assert!(!verify("", &phc).unwrap());
    }

    #[test]
    fn phc_string_uses_owasp_parameters() {
        let phc = hash("pw").unwrap();
        assert!(
            phc.starts_with("$argon2id$v=19$m=19456,t=2,p=1$"),
            "unexpected PHC prefix: {phc}"
        );
    }

    #[test]
    fn dummy_phc_is_valid_and_dummy_verify_does_not_panic() {
        // The constant must stay parseable with the production parameters so
        // dummy_verify always burns real Argon2 work.
        let parsed = PasswordHash::new(DUMMY_PHC).unwrap();
        assert_eq!(parsed.algorithm.as_str(), "argon2id");
        dummy_verify();
    }

    #[test]
    fn malformed_phc_is_an_error_not_false() {
        assert!(matches!(
            verify("pw", "not-a-phc-string"),
            Err(PasswordError::InvalidPhc(_))
        ));
    }
}
