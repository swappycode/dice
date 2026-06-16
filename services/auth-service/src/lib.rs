//! Auth service: registration, login, refresh-token rotation, revocation.
//!
//! The [`Auth`] trait below is the BINDING contract consumed by api-gateway's
//! REST layer and by the monolith. Implementations must not change signatures.

pub mod mailer;
pub mod rpc;
mod service;
pub mod validate;

use std::net::IpAddr;

use dice_common::id::UserId;
use dice_protocol::v1::AuthSuccess;

pub use mailer::{LogMailer, Mail, MailError, Mailer};
pub use service::AuthService;

#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("email already registered")]
    EmailTaken,
    #[error("username already taken")]
    UsernameTaken,
    #[error("invalid argument: {0}")]
    InvalidArgument(String),
    #[error("invalid credentials")]
    InvalidCredentials,
    #[error("invalid or expired token")]
    InvalidToken,
    #[error("invalid two-factor code")]
    InvalidTotp,
    #[error("two-factor authentication is already enabled")]
    TotpAlreadyEnabled,
    #[error("two-factor authentication is not enabled")]
    TotpNotEnabled,
    #[error("rate limited; retry after {retry_after_ms} ms")]
    RateLimited { retry_after_ms: u32 },
    #[error("internal auth error")]
    Internal(#[source] Box<dyn std::error::Error + Send + Sync>),
}

/// Outcome of [`Auth::login`]: either fully authenticated, or — for a 2FA
/// account — a challenge the caller answers via [`Auth::complete_totp_login`].
/// `Success` is boxed so the variants stay similarly sized.
#[derive(Debug)]
pub enum LoginOutcome {
    Success(Box<AuthSuccess>),
    TotpRequired { ticket: String },
}

/// A freshly begun TOTP enrollment (not yet active). The client shows
/// [`otpauth_uri`](Self::otpauth_uri) as a QR and `secret` for manual entry.
#[derive(Debug, Clone)]
pub struct TotpEnrollment {
    pub secret: String,
    pub otpauth_uri: String,
}

/// All three success paths (register/login/refresh) return a full
/// [`AuthSuccess`] (access JWT + rotated refresh token + user) per ADR-0005.
#[async_trait::async_trait]
pub trait Auth: Send + Sync {
    async fn register(
        &self,
        email: &str,
        username: &str,
        password: &str,
        ip: Option<IpAddr>,
    ) -> Result<AuthSuccess, AuthError>;

    /// Verify the password. Returns [`LoginOutcome::Success`] for a non-2FA
    /// account, or [`LoginOutcome::TotpRequired`] (a short-lived ticket) when
    /// 2FA is on — no session/token is issued until the second factor passes.
    async fn login(
        &self,
        email: &str,
        password: &str,
        ip: Option<IpAddr>,
    ) -> Result<LoginOutcome, AuthError>;

    /// Complete a 2FA login: verify `code` (TOTP or recovery) against the
    /// ticket's user, then mint the session. Rate-limited per user.
    async fn complete_totp_login(&self, ticket: &str, code: &str)
    -> Result<AuthSuccess, AuthError>;

    /// Begin 2FA enrollment for an authenticated user: generate + persist a
    /// (not-yet-active) secret and return it with its `otpauth://` URI.
    /// [`AuthError::TotpAlreadyEnabled`] if 2FA is already active.
    async fn totp_enroll(&self, user: UserId) -> Result<TotpEnrollment, AuthError>;

    /// Activate 2FA by proving a `code` from the enrolled secret. Returns the
    /// one-time recovery codes (shown once). [`AuthError::InvalidTotp`] on a bad
    /// code, [`AuthError::TotpNotEnabled`] if enrollment was never begun.
    async fn totp_confirm(&self, user: UserId, code: &str) -> Result<Vec<String>, AuthError>;

    /// Disable 2FA, requiring a current `code` (TOTP or recovery) so a stolen
    /// access token alone cannot strip the second factor.
    async fn totp_disable(&self, user: UserId, code: &str) -> Result<(), AuthError>;

    /// Confirm an email address from the verification token mailed at register
    /// (or resend). Idempotent-ish: an unknown/expired/used token is
    /// [`AuthError::InvalidToken`].
    async fn verify_email(&self, token: &str) -> Result<(), AuthError>;

    /// Re-send the verification mail to an authenticated, not-yet-verified user.
    /// Rate-limited per user. No-op success if already verified.
    async fn resend_verification(&self, user: UserId) -> Result<(), AuthError>;

    /// Begin a password reset: mail a reset token to `email` if it exists.
    /// ALWAYS `Ok(())` regardless of whether the address is registered (no
    /// account-enumeration oracle). Rate-limited per IP and per email.
    async fn request_password_reset(
        &self,
        email: &str,
        ip: Option<IpAddr>,
    ) -> Result<(), AuthError>;

    /// Complete a password reset: set a new password from a valid reset token,
    /// then revoke every existing session for the account (a reset logs all
    /// devices out). [`AuthError::InvalidToken`] on a bad token,
    /// [`AuthError::InvalidArgument`] on a weak password.
    async fn reset_password(&self, token: &str, new_password: &str) -> Result<(), AuthError>;

    /// Rotation: marks the presented token used, mints a child in the same
    /// auth_session. Reuse of an already-rotated token revokes the whole
    /// session (theft detection) and publishes `SessionRevoked` on the bus.
    async fn refresh(&self, refresh_token: &str) -> Result<AuthSuccess, AuthError>;

    /// Revokes the auth_session owning this refresh token (idempotent,
    /// best-effort) and publishes `SessionRevoked`.
    async fn logout(&self, refresh_token: &str) -> Result<(), AuthError>;
}
