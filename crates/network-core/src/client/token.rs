//! The seam between this crate and whoever owns credentials (the Tauri host's
//! session layer): both the gateway driver (re-Identify) and [`super::api`]
//! (bearer endpoints, 401 retry) pull fresh access tokens through it. The
//! provider owns refresh-token rotation; this crate never sees refresh tokens
//! outside the explicit auth endpoints.

use futures_util::future::BoxFuture;

#[derive(Debug, Clone, thiserror::Error)]
pub enum TokenError {
    /// No credentials are available (logged out / never logged in). TERMINAL:
    /// the user must authenticate.
    #[error("no credentials available")]
    NoCredentials,
    /// The server refused the credentials (refresh token revoked, rotated, or
    /// otherwise invalid — a 4xx from `POST /v1/auth/refresh`). TERMINAL: the
    /// stored session is dead; the host must re-authenticate.
    #[error("credentials rejected: {0}")]
    Rejected(String),
    /// A refresh attempt failed for a TRANSIENT reason (network, TLS, 5xx).
    /// Retryable — the credentials may still be good once the server is back.
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
