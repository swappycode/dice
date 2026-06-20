//! Access JWTs (EdDSA / Ed25519) and opaque refresh tokens
//! (docs/protocol.md §12).
//!
//! - Access tokens: 10-minute EdDSA JWTs with claims
//!   `{sub: user_id, sid: auth_session_id, iat, exp, iss: "dice", aud: "dice"}`.
//!   Only auth-service holds the private key; api-gateway uses
//!   [`JwtKeys::verify_only`] and can never mint tokens.
//! - Refresh tokens: opaque `drt_<base64url(32 random bytes)>` strings. The
//!   server stores only the SHA-256 of the full token string and compares
//!   stored vs presented hashes in constant time ([`refresh_hash_eq`]).

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use dice_common::id::{SessionId, UserId};
use jsonwebtoken::{Algorithm, DecodingKey, EncodingKey, Header, Validation};
use pkcs8::spki::EncodePublicKey as _;
use pkcs8::{EncodePrivateKey as _, LineEnding};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use subtle::ConstantTimeEq as _;

/// `iss` claim on every access token.
pub const ISS: &str = "dice";
/// `aud` claim on every access token.
pub const AUD: &str = "dice";
/// Access-token lifetime in seconds (10 minutes).
pub const ACCESS_TTL_SECS: u64 = 600;
/// Prefix of every refresh token.
pub const REFRESH_PREFIX: &str = "drt_";

/// Errors loading or generating Ed25519 key material.
#[derive(Debug, thiserror::Error)]
pub enum KeyError {
    /// The PEM bytes were not a valid Ed25519 key in the expected encoding
    /// (PKCS#8 `BEGIN PRIVATE KEY` / SPKI `BEGIN PUBLIC KEY`).
    #[error("invalid Ed25519 PEM key material: {0}")]
    InvalidPem(#[from] jsonwebtoken::errors::Error),
}

/// Errors signing or verifying access tokens.
#[derive(Debug, thiserror::Error)]
pub enum TokenError {
    /// [`sign_access`] was called on a [`JwtKeys::verify_only`] key. The
    /// gateway never signs; only auth-service loads the private half.
    #[error("this JwtKeys can only verify tokens (no private key loaded)")]
    VerifyOnly,
    /// Signing failed, or the token is invalid/expired/tampered/mis-issued.
    #[error("jwt error: {0}")]
    Jwt(#[from] jsonwebtoken::errors::Error),
    /// A verified token carried a claim that is not a well-formed id.
    #[error("token claim is not a valid id")]
    BadClaim,
}

/// Ed25519 key material for access JWTs.
///
/// Holds a decoding (public) key always, and an encoding (private) key only
/// when constructed via [`JwtKeys::from_pem`] / [`JwtKeys::generate_ephemeral`].
/// When the private half is present, the 32-byte signing seed is retained so a
/// symmetric app-encryption key can be derived from it
/// ([`JwtKeys::derive_symmetric_key`]) — no separate key store to manage.
pub struct JwtKeys {
    encoding: Option<EncodingKey>,
    decoding: DecodingKey,
    /// Ed25519 signing seed, `Some` iff the private half is loaded. Used only
    /// as HKDF input material — never serialized.
    seed: Option<[u8; 32]>,
}

impl std::fmt::Debug for JwtKeys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JwtKeys")
            .field("can_sign", &self.encoding.is_some())
            .finish_non_exhaustive()
    }
}

impl JwtKeys {
    /// Load a signing + verifying pair: PKCS#8 private-key PEM
    /// (`BEGIN PRIVATE KEY`) and SPKI public-key PEM (`BEGIN PUBLIC KEY`).
    pub fn from_pem(private_pem: &[u8], public_pem: &[u8]) -> Result<Self, KeyError> {
        Ok(Self {
            encoding: Some(EncodingKey::from_ed_pem(private_pem)?),
            decoding: DecodingKey::from_ed_pem(public_pem)?,
            // Best-effort: keep the seed for key derivation. A signable key whose
            // PKCS#8 seed we can't re-parse still signs fine; it just can't
            // derive a symmetric key (→ derive_symmetric_key returns None).
            seed: seed_from_pkcs8_pem(private_pem),
        })
    }

    /// Load only the public half (SPKI PEM). [`sign_access`] on the result
    /// returns [`TokenError::VerifyOnly`] — this is what api-gateway uses.
    pub fn verify_only(public_pem: &[u8]) -> Result<Self, KeyError> {
        Ok(Self {
            encoding: None,
            decoding: DecodingKey::from_ed_pem(public_pem)?,
            seed: None,
        })
    }

    /// Random Ed25519 pair for dev-lite boots (tokens die with the process).
    ///
    /// # Panics
    /// Only if the operating-system RNG fails or a freshly generated key
    /// cannot be PEM round-tripped — both unrecoverable environment failures.
    pub fn generate_ephemeral() -> Self {
        let (private_pem, public_pem) = Self::generate_pems();
        Self::from_pem(private_pem.as_bytes(), public_pem.as_bytes())
            .expect("freshly generated Ed25519 PEM pair must round-trip")
    }

    /// Generate a fresh Ed25519 pair and return `(private_pem, public_pem)`
    /// (PKCS#8 / SPKI) for the monolith's dev-keygen to persist to disk.
    ///
    /// # Panics
    /// Only if the operating-system RNG fails or PKCS#8/SPKI serialization of
    /// a valid key fails — both unrecoverable environment failures.
    pub fn generate_pems() -> (String, String) {
        // 32 bytes straight from the OS CSPRNG into dalek's seed constructor:
        // avoids the rand_core 0.6 (dalek) vs rand 0.9 (workspace) mismatch.
        let mut seed = [0u8; 32];
        getrandom::fill(&mut seed).expect("operating-system RNG failure");
        let signing = ed25519_dalek::SigningKey::from_bytes(&seed);
        seed.fill(0);
        let private_pem = signing
            .to_pkcs8_pem(LineEnding::LF)
            .expect("PKCS#8-encode freshly generated Ed25519 private key");
        let public_pem = signing
            .verifying_key()
            .to_public_key_pem(LineEnding::LF)
            .expect("SPKI-encode freshly generated Ed25519 public key");
        (private_pem.as_str().to_owned(), public_pem)
    }

    /// `true` when the private half is loaded (auth-service); `false` for
    /// [`JwtKeys::verify_only`] keys (api-gateway).
    pub fn can_sign(&self) -> bool {
        self.encoding.is_some()
    }

    /// Derive a 32-byte symmetric key from the private signing seed via
    /// HKDF-SHA256 with a caller-supplied domain-separation `info` string.
    ///
    /// `None` for a verify-only key (no private material) — so only the holder
    /// of the signing key can derive it. Used for app-level secret-at-rest
    /// (e.g. the TOTP shared secret) so no separate key needs managing and the
    /// derivation is identical in monolith and split-service deployments. A
    /// given `(signing key, info)` pair always yields the same key.
    #[must_use]
    pub fn derive_symmetric_key(&self, info: &[u8]) -> Option<[u8; 32]> {
        let seed = self.seed?;
        let mut out = [0u8; 32];
        hkdf::Hkdf::<Sha256>::new(None, &seed)
            .expand(info, &mut out)
            .expect("32 bytes is a valid HKDF-SHA256 output length");
        Some(out)
    }
}

/// Extract the 32-byte Ed25519 signing seed from a PKCS#8 private-key PEM, for
/// key derivation. `None` if the bytes aren't valid UTF-8 / PKCS#8 Ed25519.
fn seed_from_pkcs8_pem(private_pem: &[u8]) -> Option<[u8; 32]> {
    use pkcs8::DecodePrivateKey as _;
    let pem = std::str::from_utf8(private_pem).ok()?;
    Some(
        ed25519_dalek::SigningKey::from_pkcs8_pem(pem)
            .ok()?
            .to_bytes(),
    )
}

/// Access-token claims (docs/protocol.md §12). `sub`/`sid` are decimal
/// snowflake strings; `iat`/`exp` are Unix seconds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccessClaims {
    /// User id, decimal string.
    pub sub: String,
    /// `auth_session_id`, decimal string — minted by auth-service, lives as
    /// long as the refresh-token family. NOT the gateway session id.
    pub sid: String,
    /// Issued-at, Unix seconds.
    pub iat: u64,
    /// Expiry, Unix seconds (`iat + ACCESS_TTL_SECS`).
    pub exp: u64,
    /// Always [`ISS`].
    pub iss: String,
    /// Always [`AUD`].
    pub aud: String,
}

impl AccessClaims {
    /// Parse `sub` back into a typed id (`None` if malformed).
    pub fn user_id(&self) -> Option<UserId> {
        self.sub.parse().ok()
    }

    /// Parse `sid` back into a typed id (`None` if malformed).
    pub fn session_id(&self) -> Option<SessionId> {
        self.sid.parse().ok()
    }
}

/// Sign a fresh 10-minute access token for `user` / auth session `session`.
///
/// Fails with [`TokenError::VerifyOnly`] when `keys` has no private half.
pub fn sign_access(keys: &JwtKeys, user: UserId, session: SessionId) -> Result<String, TokenError> {
    let iat = dice_common::time::now_ms() / 1000;
    sign_access_with_exp(keys, user, session, iat, iat + ACCESS_TTL_SECS)
}

/// Internal signer with explicit timestamps; production code always goes
/// through [`sign_access`]. Kept callable from unit tests to mint
/// already-expired tokens.
fn sign_access_with_exp(
    keys: &JwtKeys,
    user: UserId,
    session: SessionId,
    iat: u64,
    exp: u64,
) -> Result<String, TokenError> {
    let encoding = keys.encoding.as_ref().ok_or(TokenError::VerifyOnly)?;
    let claims = AccessClaims {
        sub: user.to_string(),
        sid: session.to_string(),
        iat,
        exp,
        iss: ISS.to_owned(),
        aud: AUD.to_owned(),
    };
    Ok(jsonwebtoken::encode(
        &Header::new(Algorithm::EdDSA),
        &claims,
        encoding,
    )?)
}

/// Verify an access token: EdDSA signature, `exp` (with jsonwebtoken's
/// default 60 s leeway), `iss == "dice"`, `aud == "dice"`.
pub fn verify_access(keys: &JwtKeys, jwt: &str) -> Result<AccessClaims, TokenError> {
    let mut validation = Validation::new(Algorithm::EdDSA);
    validation.validate_exp = true;
    validation.set_issuer(&[ISS]);
    validation.set_audience(&[AUD]);
    let data = jsonwebtoken::decode::<AccessClaims>(jwt, &keys.decoding, &validation)?;
    Ok(data.claims)
}

/// `aud` on a TOTP login ticket. DISTINCT from [`AUD`] so a ticket can never be
/// used where an access token is expected (and vice-versa) — the audience check
/// rejects the crossover.
pub const TOTP_TICKET_AUD: &str = "dice-totp";
/// Login-ticket lifetime (5 minutes): the window to answer the 2FA challenge.
pub const TOTP_TICKET_TTL_SECS: u64 = 300;

/// Claims on a TOTP login ticket: a bare `sub` + lifetime, audience-tagged so it
/// is only ever accepted by [`verify_totp_ticket`]. No `sid` — no session exists
/// until the second factor passes.
#[derive(Debug, Serialize, Deserialize)]
struct TotpTicketClaims {
    sub: String,
    iat: u64,
    exp: u64,
    iss: String,
    aud: String,
}

/// Sign a short-lived ticket proving `user` cleared the password step. Presented
/// with a TOTP/recovery code to complete login. Fails [`TokenError::VerifyOnly`]
/// without a private key (only auth-service mints these).
pub fn sign_totp_ticket(keys: &JwtKeys, user: UserId) -> Result<String, TokenError> {
    let encoding = keys.encoding.as_ref().ok_or(TokenError::VerifyOnly)?;
    let iat = dice_common::time::now_ms() / 1000;
    let claims = TotpTicketClaims {
        sub: user.to_string(),
        iat,
        exp: iat + TOTP_TICKET_TTL_SECS,
        iss: ISS.to_owned(),
        aud: TOTP_TICKET_AUD.to_owned(),
    };
    Ok(jsonwebtoken::encode(
        &Header::new(Algorithm::EdDSA),
        &claims,
        encoding,
    )?)
}

/// Verify a TOTP login ticket (signature, `exp`, `iss`, the ticket `aud`) and
/// return the bound user. An access token (different `aud`) is rejected here.
pub fn verify_totp_ticket(keys: &JwtKeys, jwt: &str) -> Result<UserId, TokenError> {
    let mut validation = Validation::new(Algorithm::EdDSA);
    validation.validate_exp = true;
    validation.set_issuer(&[ISS]);
    validation.set_audience(&[TOTP_TICKET_AUD]);
    let data = jsonwebtoken::decode::<TotpTicketClaims>(jwt, &keys.decoding, &validation)?;
    data.claims.sub.parse().map_err(|_| TokenError::BadClaim)
}

/// Mint a `prefix`-tagged opaque token. Returns `(token, sha256)` where `token`
/// is `prefix + base64url-no-pad(32 random bytes)` (sent to the client/by mail
/// once) and `sha256` is the SHA-256 of the FULL token string (the only thing
/// the server stores). Used for refresh tokens and for email-verify /
/// password-reset tokens.
///
/// # Panics
/// Only if the operating-system RNG fails (unrecoverable).
pub fn mint_prefixed(prefix: &str) -> (String, [u8; 32]) {
    let mut raw = [0u8; 32];
    getrandom::fill(&mut raw).expect("operating-system RNG failure");
    let token = format!("{prefix}{}", URL_SAFE_NO_PAD.encode(raw));
    let digest = sha256(token.as_bytes());
    (token, digest)
}

/// Hash a presented `prefix`-tagged opaque token for a database lookup. Returns
/// `None` unless it carries `prefix` and the remainder decodes (base64url, no
/// padding) to exactly 32 bytes — malformed input is rejected before touching
/// storage. On success: SHA-256 of the full token string.
pub fn hash_prefixed(prefix: &str, presented: &str) -> Option<[u8; 32]> {
    let encoded = presented.strip_prefix(prefix)?;
    let raw = URL_SAFE_NO_PAD.decode(encoded).ok()?;
    if raw.len() != 32 {
        return None;
    }
    Some(sha256(presented.as_bytes()))
}

/// Mint a refresh token (`drt_`-prefixed). See [`mint_prefixed`].
///
/// # Panics
/// Only if the operating-system RNG fails (unrecoverable).
pub fn mint_refresh() -> (String, [u8; 32]) {
    mint_prefixed(REFRESH_PREFIX)
}

/// Hash a client-presented refresh token for the database lookup. See
/// [`hash_prefixed`]; byte-identical to [`mint_refresh`]'s second element.
pub fn hash_refresh(presented: &str) -> Option<[u8; 32]> {
    hash_prefixed(REFRESH_PREFIX, presented)
}

/// Constant-time equality for stored vs presented refresh-token hashes.
/// Always use this (never `==`) when matching a presented token against the
/// database row.
pub fn refresh_hash_eq(stored: &[u8; 32], presented: &[u8; 32]) -> bool {
    stored[..].ct_eq(&presented[..]).into()
}

fn sha256(data: &[u8]) -> [u8; 32] {
    Sha256::digest(data).into()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use dice_common::id::Snowflake;

    fn ids() -> (UserId, SessionId) {
        (
            UserId(Snowflake(123_456_789)),
            SessionId(Snowflake(987_654_321)),
        )
    }

    #[test]
    fn ephemeral_sign_verify_round_trip() {
        let keys = JwtKeys::generate_ephemeral();
        let (user, session) = ids();
        let jwt = sign_access(&keys, user, session).unwrap();
        let claims = verify_access(&keys, &jwt).unwrap();
        assert_eq!(claims.sub, "123456789");
        assert_eq!(claims.sid, "987654321");
        assert_eq!(claims.user_id(), Some(user));
        assert_eq!(claims.session_id(), Some(session));
        assert_eq!(claims.iss, ISS);
        assert_eq!(claims.aud, AUD);
        assert_eq!(claims.exp - claims.iat, ACCESS_TTL_SECS);
    }

    #[test]
    fn verify_only_verifies_but_refuses_to_sign() {
        let (private_pem, public_pem) = JwtKeys::generate_pems();
        let full = JwtKeys::from_pem(private_pem.as_bytes(), public_pem.as_bytes()).unwrap();
        let gateway = JwtKeys::verify_only(public_pem.as_bytes()).unwrap();
        assert!(full.can_sign());
        assert!(!gateway.can_sign());

        let (user, session) = ids();
        let jwt = sign_access(&full, user, session).unwrap();
        let claims = verify_access(&gateway, &jwt).unwrap();
        assert_eq!(claims.user_id(), Some(user));

        assert!(matches!(
            sign_access(&gateway, user, session),
            Err(TokenError::VerifyOnly)
        ));
    }

    #[test]
    fn tampered_token_rejected() {
        let keys = JwtKeys::generate_ephemeral();
        let (user, session) = ids();
        let jwt = sign_access(&keys, user, session).unwrap();
        // Flip one character of the signature segment.
        let mut tampered = jwt.clone();
        let last = tampered.pop().unwrap();
        tampered.push(if last == 'A' { 'B' } else { 'A' });
        assert!(verify_access(&keys, &tampered).is_err());
        // And of the payload segment.
        let parts: Vec<&str> = jwt.split('.').collect();
        let mut payload = parts[1].to_owned();
        payload.replace_range(0..1, if &payload[0..1] == "a" { "b" } else { "a" });
        let forged = format!("{}.{}.{}", parts[0], payload, parts[2]);
        assert!(verify_access(&keys, &forged).is_err());
    }

    #[test]
    fn wrong_key_rejected() {
        let signer = JwtKeys::generate_ephemeral();
        let other = JwtKeys::generate_ephemeral();
        let (user, session) = ids();
        let jwt = sign_access(&signer, user, session).unwrap();
        assert!(verify_access(&other, &jwt).is_err());
    }

    #[test]
    fn expired_token_rejected() {
        let keys = JwtKeys::generate_ephemeral();
        let (user, session) = ids();
        let now = dice_common::time::now_ms() / 1000;
        // Expired an hour ago — far beyond jsonwebtoken's 60 s default leeway.
        let jwt = sign_access_with_exp(&keys, user, session, now - 7200, now - 3600).unwrap();
        let err = verify_access(&keys, &jwt).unwrap_err();
        match err {
            TokenError::Jwt(e) => {
                assert_eq!(*e.kind(), jsonwebtoken::errors::ErrorKind::ExpiredSignature)
            }
            other => panic!("expected expired-signature error, got {other:?}"),
        }
    }

    #[test]
    fn totp_ticket_round_trip_and_audience_isolation() {
        let keys = JwtKeys::generate_ephemeral();
        let (user, session) = ids();
        let ticket = sign_totp_ticket(&keys, user).unwrap();
        assert_eq!(verify_totp_ticket(&keys, &ticket).unwrap(), user);

        // A ticket is NOT an access token (wrong audience) and vice-versa.
        assert!(verify_access(&keys, &ticket).is_err());
        let access = sign_access(&keys, user, session).unwrap();
        assert!(verify_totp_ticket(&keys, &access).is_err());

        // verify_only cannot mint a ticket.
        let (_, public_pem) = JwtKeys::generate_pems();
        let gateway = JwtKeys::verify_only(public_pem.as_bytes()).unwrap();
        assert!(matches!(
            sign_totp_ticket(&gateway, user),
            Err(TokenError::VerifyOnly)
        ));
    }

    #[test]
    fn derive_symmetric_key_is_stable_private_and_domain_separated() {
        let (private_pem, public_pem) = JwtKeys::generate_pems();
        let a = JwtKeys::from_pem(private_pem.as_bytes(), public_pem.as_bytes()).unwrap();
        let b = JwtKeys::from_pem(private_pem.as_bytes(), public_pem.as_bytes()).unwrap();
        let ka = a.derive_symmetric_key(b"dice.totp.secret.v1").unwrap();
        // Same signing key + same info ⇒ same key (deterministic across restarts).
        assert_eq!(ka, b.derive_symmetric_key(b"dice.totp.secret.v1").unwrap());
        // Different info ⇒ different key (domain separation).
        assert_ne!(ka, a.derive_symmetric_key(b"other.context").unwrap());
        // A different signing key ⇒ a different derived key.
        let other = JwtKeys::generate_ephemeral();
        assert_ne!(
            ka,
            other.derive_symmetric_key(b"dice.totp.secret.v1").unwrap()
        );
        // A verify-only key has no private material to derive from.
        let gateway = JwtKeys::verify_only(public_pem.as_bytes()).unwrap();
        assert!(
            gateway
                .derive_symmetric_key(b"dice.totp.secret.v1")
                .is_none()
        );
    }

    #[test]
    fn refresh_mint_hash_round_trip() {
        let (token, stored) = mint_refresh();
        assert!(token.starts_with(REFRESH_PREFIX));
        assert_eq!(token.len(), REFRESH_PREFIX.len() + 43); // 32 bytes b64url-no-pad
        let presented = hash_refresh(&token).unwrap();
        assert_eq!(presented, stored);
        assert!(refresh_hash_eq(&stored, &presented));

        let (token2, stored2) = mint_refresh();
        assert_ne!(token, token2, "two mints must differ");
        assert!(!refresh_hash_eq(&stored, &stored2));
    }

    #[test]
    fn refresh_malformed_rejected() {
        assert_eq!(hash_refresh(""), None);
        assert_eq!(hash_refresh("drt_"), None);
        assert_eq!(hash_refresh("not-a-token"), None);
        // Wrong prefix on otherwise-valid material.
        let (token, _) = mint_refresh();
        assert_eq!(hash_refresh(&token.replace("drt_", "xrt_")), None);
        // Valid base64 but wrong decoded length (16 bytes).
        let short = format!("drt_{}", URL_SAFE_NO_PAD.encode([0u8; 16]));
        assert_eq!(hash_refresh(&short), None);
        // Not base64url at all.
        assert_eq!(hash_refresh("drt_!!!!not-base64!!!!"), None);
        // Padded base64 is rejected by the no-pad engine.
        let padded = format!("drt_{}=", URL_SAFE_NO_PAD.encode([0u8; 32]));
        assert_eq!(hash_refresh(&padded), None);
    }
}
