//! The seam between this crate and whoever owns credentials (the Tauri host's
//! session layer): both the gateway driver (re-Identify) and [`super::api`]
//! (bearer endpoints, 401 retry) pull fresh access tokens through it. The
//! provider owns refresh-token rotation; this crate never sees refresh tokens
//! outside the explicit auth endpoints.

use futures_util::future::BoxFuture;

#[derive(Debug, Clone, thiserror::Error)]
pub enum TokenError {
    /// No credentials are available (logged out / never logged in).
    #[error("no credentials available")]
    NoCredentials,
    /// The provider tried to refresh and failed (network, revoked family, …).
    #[error("token refresh failed: {0}")]
    Refresh(String),
}

/// Yields a current access JWT. Implementations are expected to refresh
/// through `POST /v1/auth/refresh` when the cached token is stale — callers
/// signal staleness simply by asking again (e.g. after a 401 or a gateway
/// `UNAUTHENTICATED` close).
pub trait TokenProvider: Send + Sync {
    fn access_token(&self) -> BoxFuture<'_, Result<String, TokenError>>;
}
