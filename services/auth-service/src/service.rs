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

use dice_auth_core::cipher::SecretCipher;
use dice_auth_core::token::{self, ACCESS_TTL_SECS, JwtKeys};
use dice_auth_core::{password, totp};
use dice_cache::{Cache, RateLimiter};
use dice_common::id::{SessionId, SnowflakeGenerator, UserId};
use dice_common::time::now_ms;
use dice_event_bus::{BusError, BusEvent, EventBus, Subject};
use dice_protocol::internal::v1::{SessionRevoked, bus_event};
use dice_protocol::v1::{AuthSuccess, User};
use sqlx::PgPool;

use crate::mailer::{LogMailer, Mail, Mailer};
use crate::{Auth, AuthError, LoginOutcome, TotpEnrollment, validate};

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
/// TOTP challenge: 5 code attempts per 5 min per user (brute-force guard on the
/// second factor — the ticket already proves the password step passed).
const TOTP_SCOPE: &str = "auth.totp";
const TOTP_LIMIT: u32 = 5;
const TOTP_WINDOW: Duration = Duration::from_secs(5 * 60);
/// Password-reset requests: 5 per 15 min per IP and 3 per 15 min per email.
const RESET_IP_SCOPE: &str = "auth.reset.ip";
const RESET_IP_LIMIT: u32 = 5;
const RESET_EMAIL_SCOPE: &str = "auth.reset.email";
const RESET_EMAIL_LIMIT: u32 = 3;
const RESET_WINDOW: Duration = Duration::from_secs(15 * 60);
/// Verification re-sends: 3 per 15 min per user.
const RESEND_SCOPE: &str = "auth.verify.resend";
const RESEND_LIMIT: u32 = 3;
const RESEND_WINDOW: Duration = Duration::from_secs(15 * 60);
/// Opaque-token prefixes + their `auth_tokens.purpose` discriminants.
const VERIFY_PREFIX: &str = "dvt_";
const RESET_PREFIX: &str = "drst_";
const PURPOSE_VERIFY: i16 = 1;
const PURPOSE_RESET: i16 = 2;
/// Rate-limit principal when the caller has no source address.
const NO_IP: &str = "noip";
/// `origin` field on every [`BusEvent`] this service publishes.
const ORIGIN: &str = "auth-service";
/// HKDF domain-separation label for the TOTP secret-at-rest key (derived from
/// the signing seed). Changing this string re-keys — only do so with a
/// re-encryption migration; encrypted secrets would otherwise fail to decrypt.
const TOTP_KEY_INFO: &[u8] = b"dice.totp.secret.v1";

/// Postgres-backed [`Auth`] implementation.
pub struct AuthService {
    pool: PgPool,
    /// Wraps the injected `Arc<dyn Cache>`; fixed-window `rl:*` counters.
    limiter: RateLimiter,
    jwt: Arc<JwtKeys>,
    ids: Arc<SnowflakeGenerator>,
    bus: Arc<dyn EventBus>,
    /// Transactional mail (verify/reset). Defaults to [`LogMailer`].
    mailer: Arc<dyn Mailer>,
    /// Encrypts the TOTP shared secret at rest, derived from the signing seed.
    /// `None` only for a verify-only signing key (never the case in practice —
    /// auth-service always holds the private half), in which case the secret is
    /// stored/read as legacy plaintext.
    totp_cipher: Option<SecretCipher>,
}

impl AuthService {
    /// Wire the service. `jwt` must hold the PRIVATE half
    /// ([`JwtKeys::can_sign`]); only auth-service ever signs access tokens. The
    /// mailer defaults to [`LogMailer`] — override with [`Self::with_mailer`].
    pub fn new(
        pool: PgPool,
        cache: Arc<dyn Cache>,
        jwt: Arc<JwtKeys>,
        ids: Arc<SnowflakeGenerator>,
        bus: Arc<dyn EventBus>,
    ) -> Self {
        // Derive the TOTP-secret encryption key from the signing seed up front
        // (no separate key to manage; identical in monolith + split mode).
        let totp_cipher = jwt
            .derive_symmetric_key(TOTP_KEY_INFO)
            .map(SecretCipher::from_key);
        Self {
            pool,
            limiter: RateLimiter::new(cache),
            jwt,
            ids,
            bus,
            mailer: Arc::new(LogMailer),
            totp_cipher,
        }
    }

    /// Encrypt a freshly minted TOTP secret for storage (legacy plaintext if no
    /// cipher — verify-only key only, never in production).
    fn encrypt_totp_secret(&self, secret_b32: &str) -> String {
        match &self.totp_cipher {
            Some(cipher) => cipher.encrypt(secret_b32),
            None => secret_b32.to_owned(),
        }
    }

    /// Decrypt a stored TOTP secret back to its base32 form. A pre-encryption
    /// (legacy) plaintext value passes through unchanged.
    fn decrypt_totp_secret(&self, stored: &str) -> Result<String, AuthError> {
        match &self.totp_cipher {
            Some(cipher) => cipher
                .decrypt(stored)
                .map_err(|e| internal("decrypt totp secret", e)),
            None => Ok(stored.to_owned()),
        }
    }

    /// Swap the mailer (e.g. a real SMTP transport in production). Defaults to
    /// [`LogMailer`], which logs the message instead of sending it.
    #[must_use]
    pub fn with_mailer(mut self, mailer: Arc<dyn Mailer>) -> Self {
        self.mailer = mailer;
        self
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

    /// Atomically claim one unused recovery code matching `code` for `user_id`
    /// (high-entropy => a direct hashed lookup, like refresh tokens). `Ok(true)`
    /// if one was consumed (marked used), `Ok(false)` if none matched.
    async fn consume_recovery_code(&self, user_id: i64, code: &str) -> Result<bool, AuthError> {
        let hash = totp::hash_recovery_code(code);
        let consumed = sqlx::query!(
            r#"UPDATE totp_recovery_codes SET used_at = now()
               WHERE id = (
                 SELECT id FROM totp_recovery_codes
                 WHERE user_id = $1 AND code_hash = $2 AND used_at IS NULL
                 LIMIT 1
               )
               RETURNING id"#,
            user_id,
            &hash[..],
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| internal("consume recovery code", e))?;
        Ok(consumed.is_some())
    }

    /// Mint + persist a single-use opaque token (`purpose`, `prefix`, `ttl` as a
    /// Postgres interval string e.g. "24 hours"); returns the plaintext to mail.
    async fn issue_token(
        &self,
        user_id: i64,
        purpose: i16,
        prefix: &str,
        ttl: &str,
    ) -> Result<String, AuthError> {
        let (token, hash) = token::mint_prefixed(prefix);
        sqlx::query!(
            "INSERT INTO auth_tokens (id, user_id, purpose, token_hash, expires_at) \
             VALUES ($1, $2, $3, $4, now() + ($5::text)::interval)",
            self.ids.generate().as_i64(),
            user_id,
            purpose,
            &hash[..],
            ttl,
        )
        .execute(&self.pool)
        .await
        .map_err(|e| internal("issue auth token", e))?;
        Ok(token)
    }

    /// Atomically claim a live token of `purpose` matching `presented` (right
    /// prefix, unused, unexpired); returns its user. [`AuthError::InvalidToken`]
    /// on any miss.
    async fn consume_token(
        &self,
        prefix: &str,
        purpose: i16,
        presented: &str,
    ) -> Result<i64, AuthError> {
        let hash = token::hash_prefixed(prefix, presented).ok_or(AuthError::InvalidToken)?;
        let row = sqlx::query!(
            r#"UPDATE auth_tokens SET used_at = now()
               WHERE id = (
                 SELECT id FROM auth_tokens
                 WHERE token_hash = $1 AND purpose = $2
                   AND used_at IS NULL AND expires_at > now()
                 LIMIT 1
               )
               RETURNING user_id"#,
            &hash[..],
            purpose,
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| internal("consume auth token", e))?;
        row.map(|r| r.user_id).ok_or(AuthError::InvalidToken)
    }

    /// Issue a verification token and mail it (LogMailer logs it in dev).
    async fn send_verification(&self, user_id: i64, email: &str) -> Result<(), AuthError> {
        let token = self
            .issue_token(user_id, PURPOSE_VERIFY, VERIFY_PREFIX, "24 hours")
            .await?;
        self.mailer
            .send(Mail {
                to: email.to_owned(),
                subject: "Verify your Dice email".to_owned(),
                body: format!(
                    "Welcome to Dice!\n\nYour verification token:\n\n  {token}\n\n\
                     Enter it under Security \u{2192} Verify email. It expires in 24 hours."
                ),
            })
            .await
            .map_err(|e| internal("send verification mail", e))
    }

    /// Revoke every live session for `user` (e.g. after a password reset) and
    /// publish a `SessionRevoked` per killed session so gateways drop sockets.
    async fn revoke_all_sessions(&self, user: UserId) -> Result<(), AuthError> {
        let rows = sqlx::query!(
            "UPDATE auth_sessions SET revoked_at = now() \
             WHERE user_id = $1 AND revoked_at IS NULL RETURNING id",
            user.as_i64(),
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|e| internal("revoke all sessions", e))?;
        for r in rows {
            if let Err(e) = self
                .publish_session_revoked(user, SessionId::from_i64(r.id))
                .await
            {
                tracing::error!(error = %e, %user, "failed to publish SessionRevoked on reset");
            }
        }
        Ok(())
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
             RETURNING id, username, display_name, flags, avatar_media_id",
            user_id.as_i64(),
            username,
            email,
            phc,
        )
        .fetch_one(&self.pool)
        .await
        .map_err(map_register_error)?;

        // Best-effort verification mail; a mail hiccup must not fail signup
        // (the user can resend later from Security).
        if let Err(e) = self.send_verification(row.id, email).await {
            tracing::warn!(error = %e, "verification mail failed at register");
        }

        self.mint_session(&row, ip).await
    }

    async fn login(
        &self,
        email: &str,
        password: &str,
        ip: Option<IpAddr>,
    ) -> Result<LoginOutcome, AuthError> {
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
            "SELECT id, username, display_name, flags, avatar_media_id, password_hash, totp_enabled \
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

        // Password OK. With 2FA on, hand back a short-lived ticket instead of a
        // session — no token is issued until the second factor passes.
        if row.totp_enabled {
            let ticket = token::sign_totp_ticket(&self.jwt, UserId::from_i64(row.id))
                .map_err(|e| internal("sign totp ticket", e))?;
            return Ok(LoginOutcome::TotpRequired { ticket });
        }

        let user = UserRow {
            id: row.id,
            username: row.username,
            display_name: row.display_name,
            flags: row.flags,
            avatar_media_id: row.avatar_media_id,
        };
        Ok(LoginOutcome::Success(Box::new(
            self.mint_session(&user, ip).await?,
        )))
    }

    async fn complete_totp_login(
        &self,
        ticket: &str,
        code: &str,
    ) -> Result<AuthSuccess, AuthError> {
        let user_id =
            token::verify_totp_ticket(&self.jwt, ticket).map_err(|_| AuthError::InvalidToken)?;
        // Brute-force guard on the 6-digit second factor (per user).
        self.check_limit(TOTP_SCOPE, &user_id.to_string(), TOTP_LIMIT, TOTP_WINDOW)
            .await?;

        let row = sqlx::query!(
            "SELECT id, username, display_name, flags, avatar_media_id, \
                    totp_enabled, totp_secret, totp_last_step \
             FROM users WHERE id = $1",
            user_id.as_i64(),
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| internal("totp login user lookup", e))?
        .ok_or(AuthError::InvalidToken)?;

        // 2FA turned off (or never finished) since the ticket was minted.
        if !row.totp_enabled {
            return Err(AuthError::InvalidToken);
        }
        let secret = row
            .totp_secret
            .ok_or_else(|| internal_msg("totp_enabled row has no secret"))?;
        let secret = self.decrypt_totp_secret(&secret)?;

        let user = UserRow {
            id: row.id,
            username: row.username,
            display_name: row.display_name,
            flags: row.flags,
            avatar_media_id: row.avatar_media_id,
        };

        let code = code.trim();
        let now_step = now_ms() / 1000;
        match totp::verify_code(&secret, code, now_step) {
            Some(step) => {
                let step = i64::try_from(step).unwrap_or(i64::MAX);
                // Single-use: a code at a step we've already consumed is a replay.
                if row.totp_last_step.is_some_and(|last| step <= last) {
                    return Err(AuthError::InvalidTotp);
                }
                sqlx::query!(
                    "UPDATE users SET totp_last_step = $2 WHERE id = $1",
                    user.id,
                    step,
                )
                .execute(&self.pool)
                .await
                .map_err(|e| internal("advance totp_last_step", e))?;
            }
            // Not a TOTP code — try a one-time recovery code.
            None if self.consume_recovery_code(user.id, code).await? => {}
            None => return Err(AuthError::InvalidTotp),
        }

        self.mint_session(&user, None).await
    }

    async fn totp_enroll(&self, user: UserId) -> Result<TotpEnrollment, AuthError> {
        let row = sqlx::query!(
            "SELECT username, totp_enabled FROM users WHERE id = $1",
            user.as_i64(),
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| internal("totp enroll lookup", e))?
        .ok_or(AuthError::InvalidToken)?;
        if row.totp_enabled {
            return Err(AuthError::TotpAlreadyEnabled);
        }

        let enrollment = totp::enroll(&row.username);
        // Persist the not-yet-active secret ENCRYPTED at rest; reset the replay
        // step for a clean start. Stays inactive until totp_confirm proves a
        // code from it. (The plaintext is still returned below for the QR.)
        let stored_secret = self.encrypt_totp_secret(&enrollment.secret);
        sqlx::query!(
            "UPDATE users SET totp_secret = $2, totp_last_step = NULL WHERE id = $1",
            user.as_i64(),
            stored_secret,
        )
        .execute(&self.pool)
        .await
        .map_err(|e| internal("persist totp secret", e))?;

        Ok(TotpEnrollment {
            secret: enrollment.secret,
            otpauth_uri: enrollment.uri,
        })
    }

    async fn totp_confirm(&self, user: UserId, code: &str) -> Result<Vec<String>, AuthError> {
        let row = sqlx::query!(
            "SELECT totp_secret, totp_enabled FROM users WHERE id = $1",
            user.as_i64(),
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| internal("totp confirm lookup", e))?
        .ok_or(AuthError::InvalidToken)?;
        if row.totp_enabled {
            return Err(AuthError::TotpAlreadyEnabled);
        }
        // No secret => enrollment was never begun.
        let secret = row.totp_secret.ok_or(AuthError::TotpNotEnabled)?;
        let secret = self.decrypt_totp_secret(&secret)?;

        let step = totp::verify_code(&secret, code.trim(), now_ms() / 1000)
            .ok_or(AuthError::InvalidTotp)?;
        let step = i64::try_from(step).unwrap_or(i64::MAX);

        let codes = totp::generate_recovery_codes();
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| internal("begin totp confirm tx", e))?;
        // Activate, and record the confirm step so it can't be replayed at the
        // first login.
        sqlx::query!(
            "UPDATE users SET totp_enabled = TRUE, totp_last_step = $2 WHERE id = $1",
            user.as_i64(),
            step,
        )
        .execute(&mut *tx)
        .await
        .map_err(|e| internal("activate totp", e))?;
        // Fresh enrollment owns a fresh recovery set.
        sqlx::query!(
            "DELETE FROM totp_recovery_codes WHERE user_id = $1",
            user.as_i64(),
        )
        .execute(&mut *tx)
        .await
        .map_err(|e| internal("clear old recovery codes", e))?;
        for code in &codes {
            let hash = totp::hash_recovery_code(code);
            sqlx::query!(
                "INSERT INTO totp_recovery_codes (id, user_id, code_hash) VALUES ($1, $2, $3)",
                self.ids.generate().as_i64(),
                user.as_i64(),
                &hash[..],
            )
            .execute(&mut *tx)
            .await
            .map_err(|e| internal("insert recovery code", e))?;
        }
        tx.commit()
            .await
            .map_err(|e| internal("commit totp confirm tx", e))?;

        Ok(codes)
    }

    async fn totp_disable(&self, user: UserId, code: &str) -> Result<(), AuthError> {
        let row = sqlx::query!(
            "SELECT totp_secret, totp_enabled FROM users WHERE id = $1",
            user.as_i64(),
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| internal("totp disable lookup", e))?
        .ok_or(AuthError::InvalidToken)?;
        if !row.totp_enabled {
            return Err(AuthError::TotpNotEnabled);
        }
        let secret = row
            .totp_secret
            .ok_or_else(|| internal_msg("totp_enabled row has no secret"))?;
        let secret = self.decrypt_totp_secret(&secret)?;

        // Tearing down — any currently-valid TOTP code (replay step irrelevant)
        // or an unused recovery code authorizes it.
        let code = code.trim();
        let ok = totp::verify_code(&secret, code, now_ms() / 1000).is_some()
            || self.consume_recovery_code(user.as_i64(), code).await?;
        if !ok {
            return Err(AuthError::InvalidTotp);
        }

        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| internal("begin totp disable tx", e))?;
        sqlx::query!(
            "UPDATE users SET totp_enabled = FALSE, totp_secret = NULL, totp_last_step = NULL \
             WHERE id = $1",
            user.as_i64(),
        )
        .execute(&mut *tx)
        .await
        .map_err(|e| internal("clear totp", e))?;
        sqlx::query!(
            "DELETE FROM totp_recovery_codes WHERE user_id = $1",
            user.as_i64(),
        )
        .execute(&mut *tx)
        .await
        .map_err(|e| internal("delete recovery codes", e))?;
        tx.commit()
            .await
            .map_err(|e| internal("commit totp disable tx", e))?;
        Ok(())
    }

    async fn verify_email(&self, token: &str) -> Result<(), AuthError> {
        let user_id = self
            .consume_token(VERIFY_PREFIX, PURPOSE_VERIFY, token)
            .await?;
        sqlx::query!(
            "UPDATE users SET email_verified = TRUE WHERE id = $1",
            user_id,
        )
        .execute(&self.pool)
        .await
        .map_err(|e| internal("mark email verified", e))?;
        Ok(())
    }

    async fn resend_verification(&self, user: UserId) -> Result<(), AuthError> {
        self.check_limit(RESEND_SCOPE, &user.to_string(), RESEND_LIMIT, RESEND_WINDOW)
            .await?;
        let row = sqlx::query!(
            "SELECT email, email_verified FROM users WHERE id = $1",
            user.as_i64(),
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| internal("resend lookup", e))?
        .ok_or(AuthError::InvalidToken)?;
        if row.email_verified {
            return Ok(()); // already verified — nothing to do
        }
        self.send_verification(user.as_i64(), &row.email).await
    }

    async fn request_password_reset(
        &self,
        email: &str,
        ip: Option<IpAddr>,
    ) -> Result<(), AuthError> {
        self.check_limit(
            RESET_IP_SCOPE,
            &ip_principal(ip),
            RESET_IP_LIMIT,
            RESET_WINDOW,
        )
        .await?;
        self.check_limit(
            RESET_EMAIL_SCOPE,
            &email.to_lowercase(),
            RESET_EMAIL_LIMIT,
            RESET_WINDOW,
        )
        .await?;

        // Look up WITHOUT leaking existence: any outcome (found or not, mail OK
        // or not) returns Ok(()). Only a registered address gets a token mailed.
        let row = sqlx::query!("SELECT id FROM users WHERE LOWER(email) = LOWER($1)", email,)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| internal("reset lookup", e))?;
        if let Some(row) = row {
            match self
                .issue_token(row.id, PURPOSE_RESET, RESET_PREFIX, "30 minutes")
                .await
            {
                Ok(token) => {
                    let mail = Mail {
                        to: email.to_owned(),
                        subject: "Reset your Dice password".to_owned(),
                        body: format!(
                            "Someone asked to reset your Dice password.\n\n\
                             Your reset token:\n\n  {token}\n\n\
                             Enter it with a new password on the login screen. It \
                             expires in 30 minutes. If this wasn't you, ignore this mail."
                        ),
                    };
                    if let Err(e) = self.mailer.send(mail).await {
                        tracing::error!(error = %e, "send reset mail");
                    }
                }
                Err(e) => tracing::error!(error = %e, "issue reset token"),
            }
        }
        Ok(())
    }

    async fn reset_password(&self, token: &str, new_password: &str) -> Result<(), AuthError> {
        // Validate the new password BEFORE burning the single-use token.
        validate::password(new_password)?;
        let user_id = self
            .consume_token(RESET_PREFIX, PURPOSE_RESET, token)
            .await?;

        let pw = new_password.to_owned();
        let phc = tokio::task::spawn_blocking(move || password::hash(&pw))
            .await
            .map_err(|e| internal("join password-hash task", e))?
            .map_err(|e| internal("argon2 hash", e))?;
        sqlx::query!(
            "UPDATE users SET password_hash = $2 WHERE id = $1",
            user_id,
            phc,
        )
        .execute(&self.pool)
        .await
        .map_err(|e| internal("update password on reset", e))?;

        // A reset logs every device out.
        self.revoke_all_sessions(UserId::from_i64(user_id)).await?;
        Ok(())
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
                      u.flags AS "flags!",
                      u.avatar_media_id AS "avatar_media_id?"
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
            avatar_media_id: row.avatar_media_id,
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
    avatar_media_id: Option<i64>,
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
            avatar_id: user.avatar_media_id.map_or(0, |v| v as u64),
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

/// [`AuthError::Internal`] from a bare message — for "can't happen" invariants
/// (e.g. an enabled-2FA row missing its secret) that have no source error.
fn internal_msg(context: &'static str) -> AuthError {
    tracing::error!(context, "auth-service internal invariant violated");
    AuthError::Internal(context.into())
}
