//! [`AuthService`]: the concrete [`Auth`] implementation.
//!
//! Postgres holds users / auth_sessions / refresh_tokens (revocation truth
//! lives ONLY here — refresh tokens are never cached). The [`RateLimiter`]
//! (over `Arc<dyn Cache>`) enforces fixed-window limits, and the event bus
//! carries `SessionRevoked` so gateways drop live sockets immediately
//! (docs/protocol.md §12, backend-services.md §4).

use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use dice_auth_core::password;
use dice_auth_core::token::{self, ACCESS_TTL_SECS, JwtKeys};
use dice_cache::{Cache, RateLimiter};
use dice_common::id::{SessionId, SnowflakeGenerator, UserId};
use dice_common::time::now_ms;
use dice_event_bus::{BusError, BusEvent, EventBus, Subject};
use dice_protocol::internal::v1::{SessionRevoked, bus_event};
use dice_protocol::v1::{AuthSuccess, User};
use sqlx::PgPool;

use crate::{Auth, AuthError, validate};

/// Registration: 3 per hour per IP (`rl:auth.register:{ip}`).
const REGISTER_SCOPE: &str = "auth.register";
const REGISTER_LIMIT: u32 = 3;
const REGISTER_WINDOW: Duration = Duration::from_secs(60 * 60);
/// Login: 10 per 5 min per IP and 5 per 5 min per lowercased email.
const LOGIN_IP_SCOPE: &str = "auth.login.ip";
const LOGIN_IP_LIMIT: u32 = 10;
const LOGIN_EMAIL_SCOPE: &str = "auth.login.email";
const LOGIN_EMAIL_LIMIT: u32 = 5;
const LOGIN_WINDOW: Duration = Duration::from_secs(5 * 60);
/// Rate-limit principal when the caller has no source address.
const NO_IP: &str = "noip";
/// `origin` field on every [`BusEvent`] this service publishes.
const ORIGIN: &str = "auth-service";

/// Postgres-backed [`Auth`] implementation.
pub struct AuthService {
    pool: PgPool,
    /// Wraps the injected `Arc<dyn Cache>`; fixed-window `rl:*` counters.
    limiter: RateLimiter,
    jwt: Arc<JwtKeys>,
    ids: Arc<SnowflakeGenerator>,
    bus: Arc<dyn EventBus>,
}

impl AuthService {
    /// Wire the service. `jwt` must hold the PRIVATE half
    /// ([`JwtKeys::can_sign`]); only auth-service ever signs access tokens.
    pub fn new(
        pool: PgPool,
        cache: Arc<dyn Cache>,
        jwt: Arc<JwtKeys>,
        ids: Arc<SnowflakeGenerator>,
        bus: Arc<dyn EventBus>,
    ) -> Self {
        Self {
            pool,
            limiter: RateLimiter::new(cache),
            jwt,
            ids,
            bus,
        }
    }

    /// Count one request and translate a denial into [`AuthError::RateLimited`].
    async fn check_limit(
        &self,
        scope: &str,
        principal: &str,
        limit: u32,
        window: Duration,
    ) -> Result<(), AuthError> {
        let decision = self
            .limiter
            .check(scope, principal, limit, window)
            .await
            .map_err(|e| internal("rate-limit check", e))?;
        if decision.allowed {
            Ok(())
        } else {
            Err(AuthError::RateLimited {
                retry_after_ms: u32::try_from(decision.retry_after.as_millis()).unwrap_or(u32::MAX),
            })
        }
    }

    /// Shared register/login tail: create an auth_session + first refresh
    /// token of the family + access JWT, and assemble the [`AuthSuccess`].
    async fn mint_session(
        &self,
        user: &UserRow,
        ip: Option<IpAddr>,
    ) -> Result<AuthSuccess, AuthError> {
        let session_id = SessionId(self.ids.generate());
        let token_id = self.ids.generate();
        let (refresh_token, refresh_hash) = token::mint_refresh();
        let ip_text = ip.map(|i| i.to_string());

        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| internal("begin mint_session tx", e))?;
        sqlx::query!(
            "INSERT INTO auth_sessions (id, user_id, created_ip) \
             VALUES ($1, $2, ($3::text)::inet)",
            session_id.as_i64(),
            user.id,
            ip_text,
        )
        .execute(&mut *tx)
        .await
        .map_err(|e| internal("insert auth_session", e))?;
        sqlx::query!(
            "INSERT INTO refresh_tokens (id, session_id, token_hash, expires_at) \
             VALUES ($1, $2, $3, now() + interval '30 days')",
            token_id.as_i64(),
            session_id.as_i64(),
            &refresh_hash[..],
        )
        .execute(&mut *tx)
        .await
        .map_err(|e| internal("insert refresh_token", e))?;
        tx.commit()
            .await
            .map_err(|e| internal("commit mint_session tx", e))?;

        let access_token = token::sign_access(&self.jwt, UserId::from_i64(user.id), session_id)
            .map_err(|e| internal("sign access token", e))?;
        Ok(auth_success(access_token, refresh_token, user))
    }

    /// Publish `SessionRevoked` on the user's self subject so every gateway
    /// holding a live socket for this auth_session drops it.
    async fn publish_session_revoked(
        &self,
        user: UserId,
        session: SessionId,
    ) -> Result<(), BusError> {
        let event = BusEvent {
            event_id: self.ids.generate().0,
            emitted_at_ms: now_ms(),
            origin: ORIGIN.to_owned(),
            guild_id: 0,
            recipient_user_ids: vec![user.raw()],
            ephemeral: false,
            payload: Some(bus_event::Payload::SessionRevoked(SessionRevoked {
                user_id: user.raw(),
                auth_session_id: session.raw(),
            })),
        };
        self.bus.publish(Subject::User(user), event).await
    }
}

#[async_trait::async_trait]
impl Auth for AuthService {
    async fn register(
        &self,
        email: &str,
        username: &str,
        password: &str,
        ip: Option<IpAddr>,
    ) -> Result<AuthSuccess, AuthError> {
        validate::email(email)?;
        validate::username(username)?;
        validate::password(password)?;
        self.check_limit(
            REGISTER_SCOPE,
            &ip_principal(ip),
            REGISTER_LIMIT,
            REGISTER_WINDOW,
        )
        .await?;

        let pw = password.to_owned();
        let phc = tokio::task::spawn_blocking(move || password::hash(&pw))
            .await
            .map_err(|e| internal("join password-hash task", e))?
            .map_err(|e| internal("argon2 hash", e))?;

        let user_id = self.ids.generate();
        // display_name starts as the username; uniqueness rides on the
        // LOWER() unique indexes (race-safe — no pre-SELECT needed).
        let row = sqlx::query_as!(
            UserRow,
            "INSERT INTO users (id, username, display_name, email, password_hash) \
             VALUES ($1, $2, $2, $3, $4) \
             RETURNING id, username, display_name, flags",
            user_id.as_i64(),
            username,
            email,
            phc,
        )
        .fetch_one(&self.pool)
        .await
        .map_err(map_register_error)?;

        self.mint_session(&row, ip).await
    }

    async fn login(
        &self,
        email: &str,
        password: &str,
        ip: Option<IpAddr>,
    ) -> Result<AuthSuccess, AuthError> {
        self.check_limit(
            LOGIN_IP_SCOPE,
            &ip_principal(ip),
            LOGIN_IP_LIMIT,
            LOGIN_WINDOW,
        )
        .await?;
        self.check_limit(
            LOGIN_EMAIL_SCOPE,
            &email.to_lowercase(),
            LOGIN_EMAIL_LIMIT,
            LOGIN_WINDOW,
        )
        .await?;

        let row = sqlx::query!(
            "SELECT id, username, display_name, flags, password_hash \
             FROM users WHERE LOWER(email) = LOWER($1)",
            email,
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| internal("login user lookup", e))?;

        let Some(row) = row else {
            // Unknown account: burn one verification's worth of CPU so the
            // response time matches a real password check (no user-enumeration
            // timing oracle).
            tokio::task::spawn_blocking(password::dummy_verify)
                .await
                .map_err(|e| internal("join dummy-verify task", e))?;
            return Err(AuthError::InvalidCredentials);
        };

        let pw = password.to_owned();
        let phc = row.password_hash;
        let ok = tokio::task::spawn_blocking(move || password::verify(&pw, &phc))
            .await
            .map_err(|e| internal("join password-verify task", e))?
            .map_err(|e| internal("argon2 verify", e))?;
        if !ok {
            return Err(AuthError::InvalidCredentials);
        }

        let user = UserRow {
            id: row.id,
            username: row.username,
            display_name: row.display_name,
            flags: row.flags,
        };
        self.mint_session(&user, ip).await
    }

    async fn refresh(&self, refresh_token: &str) -> Result<AuthSuccess, AuthError> {
        // Malformed tokens fail before touching storage.
        let presented = token::hash_refresh(refresh_token).ok_or(AuthError::InvalidToken)?;

        let row = sqlx::query!(
            r#"SELECT rt.id AS "token_id!",
                      rt.session_id AS "session_id!",
                      rt.token_hash AS "token_hash!",
                      (rt.rotated_at IS NOT NULL) AS "rotated!",
                      (rt.expires_at <= now()) AS "expired!",
                      (s.revoked_at IS NOT NULL) AS "session_revoked!",
                      s.user_id AS "user_id!",
                      u.username AS "username!",
                      u.display_name AS "display_name?",
                      u.flags AS "flags!"
               FROM refresh_tokens rt
               JOIN auth_sessions s ON s.id = rt.session_id
               JOIN users u ON u.id = s.user_id
               WHERE rt.token_hash = $1"#,
            &presented[..],
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| internal("refresh token lookup", e))?
        .ok_or(AuthError::InvalidToken)?;

        // The unique-index lookup already matched; this constant-time check
        // guards against any future lookup-path drift.
        let stored: [u8; 32] = row
            .token_hash
            .as_slice()
            .try_into()
            .map_err(|_| AuthError::InvalidToken)?;
        if !token::refresh_hash_eq(&stored, &presented) {
            return Err(AuthError::InvalidToken);
        }

        if row.session_revoked || row.expired {
            return Err(AuthError::InvalidToken);
        }

        let user_id = UserId::from_i64(row.user_id);
        let session_id = SessionId::from_i64(row.session_id);

        if row.rotated {
            // THEFT: an already-rotated token was presented again. Kill the
            // whole session (refresh-token family) and tell the gateways.
            let revoked = sqlx::query!(
                "UPDATE auth_sessions SET revoked_at = now() \
                 WHERE id = $1 AND revoked_at IS NULL",
                row.session_id,
            )
            .execute(&self.pool)
            .await
            .map_err(|e| internal("revoke session on reuse", e))?;
            tracing::warn!(
                %user_id,
                %session_id,
                rows = revoked.rows_affected(),
                "refresh-token reuse detected; auth_session revoked"
            );
            // The revocation is already durable; a publish failure must not
            // change the (deliberate) InvalidToken outcome — log and move on.
            if let Err(e) = self.publish_session_revoked(user_id, session_id).await {
                tracing::error!(error = %e, %user_id, "failed to publish SessionRevoked");
            }
            return Err(AuthError::InvalidToken);
        }

        // Rotate: insert the child first (replaced_by FK), then retire the
        // parent — one transaction, so a concurrent refresh of the same token
        // sees either nothing or the full rotation.
        let (new_token, new_hash) = token::mint_refresh();
        let new_id = self.ids.generate();
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| internal("begin rotation tx", e))?;
        sqlx::query!(
            "INSERT INTO refresh_tokens (id, session_id, token_hash, expires_at) \
             VALUES ($1, $2, $3, now() + interval '30 days')",
            new_id.as_i64(),
            row.session_id,
            &new_hash[..],
        )
        .execute(&mut *tx)
        .await
        .map_err(|e| internal("insert rotated refresh_token", e))?;
        let retired = sqlx::query!(
            "UPDATE refresh_tokens SET rotated_at = now(), replaced_by = $2 \
             WHERE id = $1 AND rotated_at IS NULL",
            row.token_id,
            new_id.as_i64(),
        )
        .execute(&mut *tx)
        .await
        .map_err(|e| internal("retire rotated refresh_token", e))?;
        if retired.rows_affected() != 1 {
            // Lost a rotation race: a concurrent call already rotated this
            // token. Their child stands; ours must not.
            if let Err(e) = tx.rollback().await {
                tracing::error!(error = %e, "rollback after lost rotation race failed");
            }
            return Err(AuthError::InvalidToken);
        }
        tx.commit()
            .await
            .map_err(|e| internal("commit rotation tx", e))?;

        // Fresh access JWT under the SAME auth_session id.
        let access_token = token::sign_access(&self.jwt, user_id, session_id)
            .map_err(|e| internal("sign access token", e))?;
        let user = UserRow {
            id: row.user_id,
            username: row.username,
            display_name: row.display_name,
            flags: row.flags,
        };
        Ok(auth_success(access_token, new_token, &user))
    }

    async fn logout(&self, refresh_token: &str) -> Result<(), AuthError> {
        let presented = token::hash_refresh(refresh_token).ok_or(AuthError::InvalidToken)?;

        // Revoke-if-live in one statement; 0 rows = unknown token or already
        // revoked, both of which are an idempotent Ok.
        let revoked = sqlx::query!(
            r#"UPDATE auth_sessions s SET revoked_at = now()
               FROM refresh_tokens rt
               WHERE rt.token_hash = $1
                 AND rt.session_id = s.id
                 AND s.revoked_at IS NULL
               RETURNING s.id AS "session_id!", s.user_id AS "user_id!""#,
            &presented[..],
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| internal("logout revoke", e))?;

        if let Some(row) = revoked {
            self.publish_session_revoked(
                UserId::from_i64(row.user_id),
                SessionId::from_i64(row.session_id),
            )
            .await
            .map_err(|e| internal("publish SessionRevoked", e))?;
        }
        Ok(())
    }
}

/// `users` projection shared by register/login/refresh success paths.
struct UserRow {
    id: i64,
    username: String,
    display_name: Option<String>,
    flags: i64,
}

fn auth_success(access_token: String, refresh_token: String, user: &UserRow) -> AuthSuccess {
    AuthSuccess {
        access_token,
        refresh_token,
        access_expires_in_s: ACCESS_TTL_SECS,
        user: Some(User {
            id: UserId::from_i64(user.id).raw(),
            username: user.username.clone(),
            display_name: user
                .display_name
                .clone()
                .unwrap_or_else(|| user.username.clone()),
            flags: u32::try_from(user.flags).unwrap_or(0),
        }),
    }
}

fn ip_principal(ip: Option<IpAddr>) -> String {
    ip.map_or_else(|| NO_IP.to_owned(), |i| i.to_string())
}

/// Map register's INSERT failure: unique violations on the LOWER() indexes
/// become the typed taken-errors; anything else is internal.
fn map_register_error(e: sqlx::Error) -> AuthError {
    let constraint = e
        .as_database_error()
        .filter(|db| db.is_unique_violation())
        .and_then(|db| db.constraint().map(str::to_owned));
    match constraint.as_deref() {
        Some("users_email_lower_key") => AuthError::EmailTaken,
        Some("users_username_lower_key") => AuthError::UsernameTaken,
        _ => internal("register insert", e),
    }
}

/// Wrap any infrastructure failure as [`AuthError::Internal`], logging it at
/// the failure site (callers never see the underlying detail on the wire).
fn internal(context: &'static str, e: impl std::error::Error + Send + Sync + 'static) -> AuthError {
    tracing::error!(error = %e, context, "auth-service internal error");
    AuthError::Internal(Box::new(e))
}
