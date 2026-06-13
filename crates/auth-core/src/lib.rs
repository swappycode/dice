//! # dice-auth-core
//!
//! Pure CPU/crypto primitives shared by auth-service (which signs tokens and
//! hashes passwords) and api-gateway (which only verifies):
//!
//! - [`password`] — Argon2id hashing/verification (OWASP parameters). These
//!   functions are CPU-bound; async callers MUST use `spawn_blocking`.
//! - [`token`] — EdDSA (Ed25519) access JWTs with `{sub, sid, iat, exp, iss,
//!   aud}` claims (docs/protocol.md §12) and opaque `drt_`-prefixed refresh
//!   tokens (server stores SHA-256 only; constant-time comparison).
//!
//! This crate performs no IO and has no async dependencies.

pub mod password;
pub mod token;
pub mod totp;
