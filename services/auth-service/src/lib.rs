//! Auth service: registration, login, refresh-token rotation, revocation.
//!
//! The [`Auth`] trait below is the BINDING contract consumed by api-gateway's
//! REST layer and by the monolith. Implementations must not change signatures.

use std::net::IpAddr;

use dice_protocol::v1::AuthSuccess;

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
    #[error("rate limited; retry after {retry_after_ms} ms")]
    RateLimited { retry_after_ms: u32 },
    #[error("internal auth error")]
    Internal(#[source] Box<dyn std::error::Error + Send + Sync>),
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

    async fn login(
        &self,
        email: &str,
        password: &str,
        ip: Option<IpAddr>,
    ) -> Result<AuthSuccess, AuthError>;

    /// Rotation: marks the presented token used, mints a child in the same
    /// auth_session. Reuse of an already-rotated token revokes the whole
    /// session (theft detection) and publishes `SessionRevoked` on the bus.
    async fn refresh(&self, refresh_token: &str) -> Result<AuthSuccess, AuthError>;

    /// Revokes the auth_session owning this refresh token (idempotent,
    /// best-effort) and publishes `SessionRevoked`.
    async fn logout(&self, refresh_token: &str) -> Result<(), AuthError>;
}
