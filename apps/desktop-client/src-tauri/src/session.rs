//! Session layer: refresh token in the keyring, access token in RAM only
//! (design §2.3). Implements the network-core [`TokenProvider`] seam — the
//! gateway driver (re-Identify) and ApiClient (bearer 401 retry) both pull
//! through here. Refresh is single-flight via the tokio mutex; the ROTATED
//! refresh token is persisted to the keyring before the new access token is
//! handed out.

use std::sync::Arc;
use std::time::{Duration, Instant};

use dice_network_core::client::{ApiClient, TokenError, TokenProvider};
use dice_protocol::v1;
use futures_util::future::BoxFuture;

use crate::keystore::KeyStore;

/// Refresh this long before nominal expiry so in-flight requests never carry
/// an about-to-die token.
const EXPIRY_MARGIN: Duration = Duration::from_secs(30);

#[derive(Default)]
struct Inner {
    access: Option<String>,
    expires_at: Option<Instant>,
    refresh: Option<String>,
    user: Option<v1::User>,
}

pub struct SessionManager {
    /// Bare client (no token provider) — used only for refresh/logout.
    api: ApiClient,
    keys: Arc<dyn KeyStore>,
    inner: tokio::sync::Mutex<Inner>,
}

impl SessionManager {
    pub fn new(api: ApiClient, keys: Arc<dyn KeyStore>) -> Self {
        Self {
            api,
            keys,
            inner: tokio::sync::Mutex::new(Inner::default()),
        }
    }

    /// Adopt a fresh login/register/refresh result: keyring first, RAM after.
    pub async fn install(&self, auth: &v1::AuthSuccess) {
        if let Err(error) = self.keys.set(&auth.refresh_token) {
            tracing::warn!(%error, "keyring write failed; session lives for this run only");
        }
        let mut inner = self.inner.lock().await;
        adopt(&mut inner, auth);
    }

    /// True when RAM or the keyring holds a refresh token.
    pub fn has_stored_session(&self) -> bool {
        self.keys.get().ok().flatten().is_some()
    }

    pub async fn current_user(&self) -> Option<v1::User> {
        self.inner.lock().await.user.clone()
    }

    /// The refresh token (RAM, else keyring) — for logout revocation.
    pub async fn refresh_token(&self) -> Option<String> {
        let inner = self.inner.lock().await;
        if let Some(token) = &inner.refresh {
            return Some(token.clone());
        }
        drop(inner);
        self.keys.get().ok().flatten()
    }

    /// Drop everything: RAM state and the keyring entry.
    pub async fn clear(&self) {
        if let Err(error) = self.keys.delete() {
            tracing::warn!(%error, "keyring delete failed");
        }
        *self.inner.lock().await = Inner::default();
    }

    /// Force a refresh (used at cold start when the cache is empty) and
    /// return the user the auth service reported.
    pub async fn refresh_user(&self) -> Result<v1::User, TokenError> {
        let mut inner = self.inner.lock().await;
        self.refresh_locked(&mut inner).await?;
        inner.user.clone().ok_or(TokenError::NoCredentials)
    }

    /// Core token path: cached access while fresh, single-flight refresh
    /// (rotating — the NEW refresh token hits the keyring first) otherwise.
    async fn token(&self) -> Result<String, TokenError> {
        let mut inner = self.inner.lock().await;
        if let (Some(access), Some(at)) = (&inner.access, inner.expires_at)
            && Instant::now() + EXPIRY_MARGIN < at
        {
            return Ok(access.clone());
        }
        self.refresh_locked(&mut inner).await?;
        inner.access.clone().ok_or(TokenError::NoCredentials)
    }

    async fn refresh_locked(&self, inner: &mut Inner) -> Result<(), TokenError> {
        let refresh = match inner.refresh.clone() {
            Some(token) => token,
            None => self
                .keys
                .get()
                .map_err(|e| TokenError::Refresh(format!("keyring read: {e}")))?
                .ok_or(TokenError::NoCredentials)?,
        };
        let auth = self.api.refresh(&refresh).await.map_err(|e| {
            // A 4xx means the server refused this refresh token (revoked,
            // rotated, expired) — terminal, the host must re-authenticate.
            // 5xx / transport errors are transient and stay retryable.
            if matches!(e.status(), Some(status) if (400..500).contains(&status)) {
                TokenError::Rejected(e.to_string())
            } else {
                TokenError::Refresh(e.to_string())
            }
        })?;
        // Rotation: persist the NEW refresh token before handing anything out.
        if let Err(error) = self.keys.set(&auth.refresh_token) {
            tracing::warn!(%error, "keyring write failed; rotated token is RAM-only");
        }
        adopt(inner, &auth);
        Ok(())
    }
}

fn adopt(inner: &mut Inner, auth: &v1::AuthSuccess) {
    inner.refresh = Some(auth.refresh_token.clone());
    inner.access = Some(auth.access_token.clone());
    inner.expires_at = Some(Instant::now() + Duration::from_secs(auth.access_expires_in_s.max(1)));
    if auth.user.is_some() {
        inner.user.clone_from(&auth.user);
    }
}

impl TokenProvider for SessionManager {
    fn access_token(&self) -> BoxFuture<'_, Result<String, TokenError>> {
        Box::pin(self.token())
    }
}
