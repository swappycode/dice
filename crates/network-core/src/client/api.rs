//! `ApiClient`: the REST half (docs/protocol.md §10) — protobuf bodies
//! (`application/x-protobuf`) over HTTPS, errors as `dice.v1.Error` + HTTP
//! status. Auth endpoints need no token; bearer endpoints pull access tokens
//! from a [`TokenProvider`] and do exactly ONE refresh-and-retry on 401
//! (the provider owns refresh-token rotation).

use std::sync::Arc;

use dice_protocol::prost::Message;
use dice_protocol::v1::{self, ErrorCode};
use reqwest::StatusCode;
use reqwest::header::CONTENT_TYPE;

use super::tls::TlsOptions;
use super::token::{TokenError, TokenProvider};
use crate::tls::TlsError;

const PROTOBUF: &str = "application/x-protobuf";

#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    /// The server answered with a `dice.v1.Error` body.
    #[error("api error {status}: {} ({})", error.message, error.code)]
    Api { status: u16, error: v1::Error },
    /// Transport-level failure (DNS, TLS, connect, body read).
    #[error("http: {0}")]
    Http(#[from] reqwest::Error),
    /// 2xx body that did not decode as the expected message.
    #[error("response decode: {0}")]
    Decode(#[from] dice_protocol::prost::DecodeError),
    /// The token provider could not supply an access token.
    #[error(transparent)]
    Token(#[from] TokenError),
    /// Trust configuration problem (bad extra-CA path, …).
    #[error(transparent)]
    Tls(#[from] TlsError),
    /// `base.join(path)` failed (base URL cannot be a base).
    #[error("url: {0}")]
    Url(#[from] url::ParseError),
}

impl ApiError {
    /// HTTP status when the server answered with an error body.
    pub fn status(&self) -> Option<u16> {
        match self {
            Self::Api { status, .. } => Some(*status),
            _ => None,
        }
    }
}

/// Result of [`ApiClient::login`]: either fully authenticated, or a 2FA account
/// that must answer the challenge via [`ApiClient::complete_totp_login`].
#[derive(Debug)]
pub enum LoginOutcome {
    Success(v1::AuthSuccess),
    TotpRequired { ticket: String },
}

/// Protobuf-over-HTTPS client for the gateway's REST surface.
#[derive(Clone)]
pub struct ApiClient {
    http: reqwest::Client,
    base: url::Url,
    token: Option<Arc<dyn TokenProvider>>,
}

impl ApiClient {
    /// `base` is the scheme+host+port root, e.g. `https://localhost:8443`.
    /// Trust = webpki roots + the extra anchors in `tls`.
    pub fn new(base: url::Url, tls: &TlsOptions) -> Result<Self, ApiError> {
        let builder = tls.apply_to_reqwest(reqwest::Client::builder())?;
        Ok(Self {
            http: builder.build()?,
            base,
            token: None,
        })
    }

    /// Attach the token provider the bearer endpoints will use.
    #[must_use]
    pub fn with_token_provider(mut self, provider: Arc<dyn TokenProvider>) -> Self {
        self.token = Some(provider);
        self
    }

    // ------------------------------------------------------ auth endpoints

    pub async fn register(
        &self,
        email: &str,
        username: &str,
        password: &str,
    ) -> Result<v1::AuthSuccess, ApiError> {
        self.post_public(
            "/v1/auth/register",
            &v1::RegisterRequest {
                email: email.to_owned(),
                username: username.to_owned(),
                password: password.to_owned(),
            },
        )
        .await
    }

    /// `POST /v1/auth/login`. Returns [`LoginOutcome::TotpRequired`] (a ticket
    /// for [`Self::complete_totp_login`]) when the account has 2FA enabled.
    pub async fn login(&self, email: &str, password: &str) -> Result<LoginOutcome, ApiError> {
        let resp: v1::LoginResponse = self
            .post_public(
                "/v1/auth/login",
                &v1::LoginRequest {
                    email: email.to_owned(),
                    password: password.to_owned(),
                },
            )
            .await?;
        match resp.outcome {
            Some(v1::login_response::Outcome::Success(success)) => {
                Ok(LoginOutcome::Success(success))
            }
            Some(v1::login_response::Outcome::TotpRequired(c)) => {
                Ok(LoginOutcome::TotpRequired { ticket: c.ticket })
            }
            None => Err(ApiError::Api {
                status: 502,
                error: v1::Error {
                    code: ErrorCode::Internal as i32,
                    message: "login response carried no outcome".to_owned(),
                    retry_after_ms: 0,
                },
            }),
        }
    }

    /// `POST /v1/auth/login/totp` — finish a 2FA login with the challenge ticket
    /// plus a TOTP or recovery `code`.
    pub async fn complete_totp_login(
        &self,
        ticket: &str,
        code: &str,
    ) -> Result<v1::AuthSuccess, ApiError> {
        self.post_public(
            "/v1/auth/login/totp",
            &v1::CompleteTotpRequest {
                ticket: ticket.to_owned(),
                code: code.to_owned(),
            },
        )
        .await
    }

    /// `POST /v1/users/@me/totp/enroll` (bearer, no body) — begin 2FA enrollment.
    pub async fn totp_enroll(&self) -> Result<v1::TotpEnrollResponse, ApiError> {
        let url = self.url("/v1/users/@me/totp/enroll")?;
        let response = self
            .bearer_send(|token| self.http.post(url.clone()).bearer_auth(token))
            .await?;
        decode_response(response).await
    }

    /// `POST /v1/users/@me/totp/confirm` (bearer) — activate 2FA; returns the
    /// one-time recovery codes.
    pub async fn totp_confirm(&self, code: &str) -> Result<Vec<String>, ApiError> {
        let resp: v1::TotpConfirmResponse = self
            .post_bearer(
                "/v1/users/@me/totp/confirm",
                &v1::TotpConfirmRequest {
                    code: code.to_owned(),
                },
            )
            .await?;
        Ok(resp.recovery_codes)
    }

    /// `POST /v1/users/@me/totp/disable` (bearer) — turn 2FA off with a current
    /// TOTP or recovery `code` (204).
    pub async fn totp_disable(&self, code: &str) -> Result<(), ApiError> {
        let url = self.url("/v1/users/@me/totp/disable")?;
        let body = v1::TotpDisableRequest {
            code: code.to_owned(),
        }
        .encode_to_vec();
        let response = self
            .bearer_send(|token| {
                self.http
                    .post(url.clone())
                    .header(CONTENT_TYPE, PROTOBUF)
                    .body(body.clone())
                    .bearer_auth(token)
            })
            .await?;
        let status = response.status();
        if status.is_success() {
            return Ok(());
        }
        Err(error_from(status, response.bytes().await?.as_ref()))
    }

    /// Rotates the refresh token; the old one is dead afterwards.
    pub async fn refresh(&self, refresh_token: &str) -> Result<v1::AuthSuccess, ApiError> {
        self.post_public(
            "/v1/auth/refresh",
            &v1::RefreshRequest {
                refresh_token: refresh_token.to_owned(),
            },
        )
        .await
    }

    /// `POST /v1/auth/verify-email` (public): confirm an address with a mailed
    /// token.
    pub async fn verify_email(&self, token: &str) -> Result<(), ApiError> {
        self.post_public_unit(
            "/v1/auth/verify-email",
            &v1::VerifyEmailRequest {
                token: token.to_owned(),
            },
        )
        .await
    }

    /// `POST /v1/auth/password-reset/request` (public): always succeeds (no
    /// account-enumeration oracle) — a token is mailed only if `email` exists.
    pub async fn request_password_reset(&self, email: &str) -> Result<(), ApiError> {
        self.post_public_unit(
            "/v1/auth/password-reset/request",
            &v1::PasswordResetRequest {
                email: email.to_owned(),
            },
        )
        .await
    }

    /// `POST /v1/auth/password-reset/confirm` (public): set a new password from a
    /// reset token (all sessions are revoked server-side).
    pub async fn reset_password(&self, token: &str, new_password: &str) -> Result<(), ApiError> {
        self.post_public_unit(
            "/v1/auth/password-reset/confirm",
            &v1::PasswordResetConfirm {
                token: token.to_owned(),
                new_password: new_password.to_owned(),
            },
        )
        .await
    }

    /// `POST /v1/auth/verify-email/resend` (bearer, no body): re-send the
    /// verification mail to the signed-in user.
    pub async fn resend_verification(&self) -> Result<(), ApiError> {
        let url = self.url("/v1/auth/verify-email/resend")?;
        let response = self
            .bearer_send(|token| self.http.post(url.clone()).bearer_auth(token))
            .await?;
        let status = response.status();
        if status.is_success() {
            return Ok(());
        }
        Err(error_from(status, response.bytes().await?.as_ref()))
    }

    /// Revokes the refresh-token family (204 on success).
    pub async fn logout(&self, refresh_token: &str) -> Result<(), ApiError> {
        let url = self.url("/v1/auth/logout")?;
        let body = v1::LogoutRequest {
            refresh_token: refresh_token.to_owned(),
        }
        .encode_to_vec();
        let response = self
            .http
            .post(url)
            .header(CONTENT_TYPE, PROTOBUF)
            .body(body)
            .send()
            .await?;
        let status = response.status();
        if status.is_success() {
            return Ok(());
        }
        Err(error_from(status, response.bytes().await?.as_ref()))
    }

    // ---------------------------------------------------- bearer endpoints

    /// `GET /v1/channels/{id}/messages?before|after=<id>&limit=1..100`,
    /// newest first.
    pub async fn fetch_messages(
        &self,
        channel_id: u64,
        before: Option<u64>,
        after: Option<u64>,
        limit: u8,
    ) -> Result<Vec<v1::Message>, ApiError> {
        let mut url = self.url(&format!("/v1/channels/{channel_id}/messages"))?;
        {
            let mut query = url.query_pairs_mut();
            if let Some(before) = before {
                query.append_pair("before", &before.to_string());
            }
            if let Some(after) = after {
                query.append_pair("after", &after.to_string());
            }
            query.append_pair("limit", &limit.to_string());
        }
        let response = self
            .bearer_send(|token| self.http.get(url.clone()).bearer_auth(token))
            .await?;
        let history: v1::MessageHistory = decode_response(response).await?;
        Ok(history.messages)
    }

    /// `POST /v1/guilds` — auto-creates `#general`.
    pub async fn create_guild(&self, name: &str) -> Result<v1::Guild, ApiError> {
        self.post_bearer(
            "/v1/guilds",
            &v1::CreateGuildRequest {
                name: name.to_owned(),
            },
        )
        .await
    }

    /// `POST /v1/guilds/join` by invite code.
    pub async fn join_guild(&self, code: &str) -> Result<v1::Guild, ApiError> {
        self.post_bearer(
            "/v1/guilds/join",
            &v1::JoinGuildRequest {
                code: code.to_owned(),
            },
        )
        .await
    }

    /// `POST /v1/dms` — idempotent per recipient pair.
    pub async fn open_dm(&self, recipient_id: u64) -> Result<v1::Channel, ApiError> {
        self.post_bearer("/v1/dms", &v1::OpenDmRequest { recipient_id })
            .await
    }

    /// `POST /v1/media` — upload one file (protobuf body, larger limit than the
    /// realtime path). Returns the stored attachment metadata; its id is then
    /// referenced in `SendMessage { attachment_ids }`.
    pub async fn upload_media(
        &self,
        filename: &str,
        content_type: &str,
        data: Vec<u8>,
    ) -> Result<v1::Attachment, ApiError> {
        let resp: v1::UploadMediaResponse = self
            .post_bearer(
                "/v1/media",
                &v1::UploadMediaRequest {
                    filename: filename.to_owned(),
                    content_type: content_type.to_owned(),
                    data: data.into(),
                },
            )
            .await?;
        resp.attachment.ok_or_else(|| ApiError::Api {
            status: 502,
            error: v1::Error {
                code: ErrorCode::Internal as i32,
                message: "upload response missing attachment".to_owned(),
                retry_after_ms: 0,
            },
        })
    }

    /// `GET /v1/unread` — the caller's non-zero per-channel unread counts as
    /// `(channel_id, count)` pairs.
    pub async fn fetch_unread(&self) -> Result<Vec<(u64, u64)>, ApiError> {
        let url = self.url("/v1/unread")?;
        let response = self
            .bearer_send(|token| self.http.get(url.clone()).bearer_auth(token))
            .await?;
        let counts: v1::UnreadCounts = decode_response(response).await?;
        Ok(counts
            .entries
            .into_iter()
            .map(|e| (e.channel_id, e.count))
            .collect())
    }

    /// `POST /v1/channels/{id}/read` — clear the caller's unread badge for the
    /// channel (204).
    pub async fn mark_read(&self, channel_id: u64) -> Result<(), ApiError> {
        let url = self.url(&format!("/v1/channels/{channel_id}/read"))?;
        let response = self
            .bearer_send(|token| self.http.post(url.clone()).bearer_auth(token))
            .await?;
        let status = response.status();
        if status.is_success() {
            return Ok(());
        }
        Err(error_from(status, response.bytes().await?.as_ref()))
    }

    /// `PUT /v1/users/@me/avatar` — set (`media_id`) or clear (`0`) the avatar.
    /// 204 on success; the change propagates via the `UserUpdate` dispatch.
    pub async fn set_avatar(&self, media_id: u64) -> Result<(), ApiError> {
        let url = self.url("/v1/users/@me/avatar")?;
        let body = v1::SetAvatarRequest { media_id }.encode_to_vec();
        let response = self
            .bearer_send(|token| {
                self.http
                    .put(url.clone())
                    .header(CONTENT_TYPE, PROTOBUF)
                    .body(body.clone())
                    .bearer_auth(token)
            })
            .await?;
        let status = response.status();
        if status.is_success() {
            return Ok(());
        }
        Err(error_from(status, response.bytes().await?.as_ref()))
    }

    /// `GET /v1/media/{id}` — fetch the raw bytes + their MIME type for display.
    pub async fn download_media(&self, id: u64) -> Result<(String, bytes::Bytes), ApiError> {
        let url = self.url(&format!("/v1/media/{id}"))?;
        let response = self
            .bearer_send(|token| self.http.get(url.clone()).bearer_auth(token))
            .await?;
        let status = response.status();
        if !status.is_success() {
            return Err(error_from(status, response.bytes().await?.as_ref()));
        }
        let content_type = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("application/octet-stream")
            .to_owned();
        Ok((content_type, response.bytes().await?))
    }

    // ------------------------------------------------------------ plumbing

    fn url(&self, path: &str) -> Result<url::Url, ApiError> {
        Ok(self.base.join(path)?)
    }

    async fn post_public<Req: Message, Resp: Message + Default>(
        &self,
        path: &str,
        request: &Req,
    ) -> Result<Resp, ApiError> {
        let url = self.url(path)?;
        let response = self
            .http
            .post(url)
            .header(CONTENT_TYPE, PROTOBUF)
            .body(request.encode_to_vec())
            .send()
            .await?;
        decode_response(response).await
    }

    /// Public POST whose success carries no body (204) — verify-email / reset.
    async fn post_public_unit<Req: Message>(
        &self,
        path: &str,
        request: &Req,
    ) -> Result<(), ApiError> {
        let url = self.url(path)?;
        let response = self
            .http
            .post(url)
            .header(CONTENT_TYPE, PROTOBUF)
            .body(request.encode_to_vec())
            .send()
            .await?;
        let status = response.status();
        if status.is_success() {
            return Ok(());
        }
        Err(error_from(status, response.bytes().await?.as_ref()))
    }

    async fn post_bearer<Req: Message, Resp: Message + Default>(
        &self,
        path: &str,
        request: &Req,
    ) -> Result<Resp, ApiError> {
        let url = self.url(path)?;
        let body = request.encode_to_vec();
        let response = self
            .bearer_send(|token| {
                self.http
                    .post(url.clone())
                    .header(CONTENT_TYPE, PROTOBUF)
                    .body(body.clone())
                    .bearer_auth(token)
            })
            .await?;
        decode_response(response).await
    }

    /// Send with a bearer token; on 401, ask the provider ONCE more (it owns
    /// rotation/refresh) and retry exactly once.
    async fn bearer_send(
        &self,
        build: impl Fn(&str) -> reqwest::RequestBuilder,
    ) -> Result<reqwest::Response, ApiError> {
        let provider = self.token.as_ref().ok_or(TokenError::NoCredentials)?;
        let token = provider.access_token().await?;
        let response = build(&token).send().await?;
        if response.status() != StatusCode::UNAUTHORIZED {
            return Ok(response);
        }
        let token = provider.access_token().await?;
        Ok(build(&token).send().await?)
    }
}

/// 2xx ⇒ decode `Resp`; otherwise decode (or synthesize) the `dice.v1.Error`.
async fn decode_response<Resp: Message + Default>(
    response: reqwest::Response,
) -> Result<Resp, ApiError> {
    let status = response.status();
    let bytes = response.bytes().await?;
    if status.is_success() {
        return Ok(Resp::decode(bytes.as_ref())?);
    }
    Err(error_from(status, &bytes))
}

fn error_from(status: StatusCode, body: &[u8]) -> ApiError {
    let error = v1::Error::decode(body).unwrap_or_else(|_| v1::Error {
        code: ErrorCode::Unspecified as i32,
        message: format!("HTTP {status} with undecodable error body"),
        retry_after_ms: 0,
    });
    ApiError::Api {
        status: status.as_u16(),
        error,
    }
}
