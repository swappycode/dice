//! Split-mode NATS RPC for auth (mirror of `presence::rpc`). The monolith calls
//! [`AuthService`](crate::AuthService) directly; a split deployment puts
//! [`AuthNatsClient`] behind the same `Arc<dyn Auth>` seam in the gateway's REST
//! layer and runs [`serve`] in the auth-service bin. Both sides use the generic
//! envelope/transport in `dice_event_bus::rpc`; only the per-method payloads and
//! the error mapping live here.

use std::net::IpAddr;
use std::sync::Arc;

use dice_common::id::UserId;
use dice_event_bus::rpc::{RpcClient, RpcError, RpcFault};
use dice_protocol::internal::v1 as rpc;
use dice_protocol::prost::Message as _;
use dice_protocol::v1;

use crate::{Auth, AuthError, LoginOutcome, TotpEnrollment};

/// RPC service name (subject segment + queue group): `dice.rpc.auth.*`.
pub const SERVICE: &str = "auth";

// Fault codes carried over the wire so the client can rebuild the typed error.
const CODE_INTERNAL: u32 = 0;
const CODE_EMAIL_TAKEN: u32 = 1;
const CODE_USERNAME_TAKEN: u32 = 2;
const CODE_INVALID_ARGUMENT: u32 = 3;
const CODE_INVALID_CREDENTIALS: u32 = 4;
const CODE_INVALID_TOKEN: u32 = 5;
const CODE_INVALID_TOTP: u32 = 6;
const CODE_TOTP_ALREADY_ENABLED: u32 = 7;
const CODE_TOTP_NOT_ENABLED: u32 = 8;
const CODE_RATE_LIMITED: u32 = 9;
const CODE_EMAIL_NOT_VERIFIED: u32 = 10;

fn ip_to_str(ip: Option<IpAddr>) -> String {
    ip.map(|i| i.to_string()).unwrap_or_default()
}

fn ip_from_str(s: &str) -> Option<IpAddr> {
    if s.is_empty() { None } else { s.parse().ok() }
}

fn internal(e: impl std::fmt::Display) -> AuthError {
    AuthError::Internal(e.to_string().into())
}

// ---- server: AuthError -> RpcFault ----

fn to_fault(e: AuthError) -> RpcFault {
    let code = match e {
        AuthError::EmailTaken => CODE_EMAIL_TAKEN,
        AuthError::UsernameTaken => CODE_USERNAME_TAKEN,
        AuthError::InvalidArgument(_) => CODE_INVALID_ARGUMENT,
        AuthError::InvalidCredentials => CODE_INVALID_CREDENTIALS,
        AuthError::EmailNotVerified => CODE_EMAIL_NOT_VERIFIED,
        AuthError::InvalidToken => CODE_INVALID_TOKEN,
        AuthError::InvalidTotp => CODE_INVALID_TOTP,
        AuthError::TotpAlreadyEnabled => CODE_TOTP_ALREADY_ENABLED,
        AuthError::TotpNotEnabled => CODE_TOTP_NOT_ENABLED,
        AuthError::RateLimited { .. } => CODE_RATE_LIMITED,
        AuthError::Internal(_) => CODE_INTERNAL,
    };
    // `message` carries the only fields that don't fit in `code`: the
    // InvalidArgument detail and the RateLimited retry-after (as digits). The
    // internal variant is never detailed over the wire.
    let message = match e {
        AuthError::InvalidArgument(m) => m,
        AuthError::RateLimited { retry_after_ms } => retry_after_ms.to_string(),
        AuthError::Internal(_) => "internal auth error".to_owned(),
        other => other.to_string(),
    };
    RpcFault { code, message }
}

fn decode_fault(e: dice_protocol::prost::DecodeError) -> RpcFault {
    RpcFault::internal(format!("malformed request: {e}"))
}

// ---- client: RpcError -> AuthError ----

fn to_err(e: RpcError) -> AuthError {
    match e {
        RpcError::Fault {
            code: CODE_EMAIL_TAKEN,
            ..
        } => AuthError::EmailTaken,
        RpcError::Fault {
            code: CODE_USERNAME_TAKEN,
            ..
        } => AuthError::UsernameTaken,
        RpcError::Fault {
            code: CODE_INVALID_ARGUMENT,
            message,
        } => AuthError::InvalidArgument(message),
        RpcError::Fault {
            code: CODE_INVALID_CREDENTIALS,
            ..
        } => AuthError::InvalidCredentials,
        RpcError::Fault {
            code: CODE_EMAIL_NOT_VERIFIED,
            ..
        } => AuthError::EmailNotVerified,
        RpcError::Fault {
            code: CODE_INVALID_TOKEN,
            ..
        } => AuthError::InvalidToken,
        RpcError::Fault {
            code: CODE_INVALID_TOTP,
            ..
        } => AuthError::InvalidTotp,
        RpcError::Fault {
            code: CODE_TOTP_ALREADY_ENABLED,
            ..
        } => AuthError::TotpAlreadyEnabled,
        RpcError::Fault {
            code: CODE_TOTP_NOT_ENABLED,
            ..
        } => AuthError::TotpNotEnabled,
        RpcError::Fault {
            code: CODE_RATE_LIMITED,
            message,
        } => AuthError::RateLimited {
            retry_after_ms: message.parse().unwrap_or(0),
        },
        other => AuthError::Internal(other.to_string().into()),
    }
}

fn login_response(outcome: LoginOutcome) -> v1::LoginResponse {
    use v1::login_response::Outcome;
    let outcome = match outcome {
        LoginOutcome::Success(success) => Outcome::Success(*success),
        LoginOutcome::TotpRequired { ticket } => {
            Outcome::TotpRequired(v1::TotpChallenge { ticket })
        }
    };
    v1::LoginResponse {
        outcome: Some(outcome),
    }
}

fn login_outcome(resp: v1::LoginResponse) -> Result<LoginOutcome, AuthError> {
    use v1::login_response::Outcome;
    match resp.outcome {
        Some(Outcome::Success(success)) => Ok(LoginOutcome::Success(Box::new(success))),
        Some(Outcome::TotpRequired(challenge)) => Ok(LoginOutcome::TotpRequired {
            ticket: challenge.ticket,
        }),
        None => Err(internal("login response missing outcome")),
    }
}

/// Run the auth RPC responder until dropped/aborted (the auth-service bin spawns
/// this). Decodes each `dice.rpc.auth.{method}`, calls `auth`, and replies with
/// the encoded response or a mapped fault.
pub async fn serve(client: RpcClient, auth: Arc<dyn Auth>) -> Result<(), RpcError> {
    client
        .serve(SERVICE, move |method, body| {
            let auth = Arc::clone(&auth);
            async move {
                match method.as_str() {
                    "register" => {
                        let r =
                            rpc::AuthRegisterReq::decode(body.as_slice()).map_err(decode_fault)?;
                        let s = auth
                            .register(&r.email, &r.username, &r.password, ip_from_str(&r.ip))
                            .await
                            .map_err(to_fault)?;
                        Ok(s.encode_to_vec())
                    }
                    "login" => {
                        let r = rpc::AuthLoginReq::decode(body.as_slice()).map_err(decode_fault)?;
                        let outcome = auth
                            .login(&r.email, &r.password, ip_from_str(&r.ip))
                            .await
                            .map_err(to_fault)?;
                        Ok(login_response(outcome).encode_to_vec())
                    }
                    "complete_totp_login" => {
                        let r = v1::CompleteTotpRequest::decode(body.as_slice())
                            .map_err(decode_fault)?;
                        let s = auth
                            .complete_totp_login(&r.ticket, &r.code)
                            .await
                            .map_err(to_fault)?;
                        Ok(s.encode_to_vec())
                    }
                    "totp_enroll" => {
                        let r = rpc::AuthUserReq::decode(body.as_slice()).map_err(decode_fault)?;
                        let e = auth
                            .totp_enroll(UserId::from_raw(r.user))
                            .await
                            .map_err(to_fault)?;
                        Ok(v1::TotpEnrollResponse {
                            secret: e.secret,
                            otpauth_uri: e.otpauth_uri,
                        }
                        .encode_to_vec())
                    }
                    "totp_confirm" => {
                        let r =
                            rpc::AuthUserCodeReq::decode(body.as_slice()).map_err(decode_fault)?;
                        let codes = auth
                            .totp_confirm(UserId::from_raw(r.user), &r.code)
                            .await
                            .map_err(to_fault)?;
                        Ok(v1::TotpConfirmResponse {
                            recovery_codes: codes,
                        }
                        .encode_to_vec())
                    }
                    "totp_disable" => {
                        let r =
                            rpc::AuthUserCodeReq::decode(body.as_slice()).map_err(decode_fault)?;
                        auth.totp_disable(UserId::from_raw(r.user), &r.code)
                            .await
                            .map(|()| Vec::new())
                            .map_err(to_fault)
                    }
                    "verify_email" => {
                        let r = v1::VerifyEmailRequest::decode(body.as_slice())
                            .map_err(decode_fault)?;
                        auth.verify_email(&r.token)
                            .await
                            .map(|()| Vec::new())
                            .map_err(to_fault)
                    }
                    "resend_verification" => {
                        let r = rpc::AuthUserReq::decode(body.as_slice()).map_err(decode_fault)?;
                        auth.resend_verification(UserId::from_raw(r.user))
                            .await
                            .map(|()| Vec::new())
                            .map_err(to_fault)
                    }
                    "request_password_reset" => {
                        let r = rpc::AuthPasswordResetReq::decode(body.as_slice())
                            .map_err(decode_fault)?;
                        auth.request_password_reset(&r.email, ip_from_str(&r.ip))
                            .await
                            .map(|()| Vec::new())
                            .map_err(to_fault)
                    }
                    "reset_password" => {
                        let r = v1::PasswordResetConfirm::decode(body.as_slice())
                            .map_err(decode_fault)?;
                        auth.reset_password(&r.token, &r.new_password)
                            .await
                            .map(|()| Vec::new())
                            .map_err(to_fault)
                    }
                    "refresh" => {
                        let r =
                            v1::RefreshRequest::decode(body.as_slice()).map_err(decode_fault)?;
                        let s = auth.refresh(&r.refresh_token).await.map_err(to_fault)?;
                        Ok(s.encode_to_vec())
                    }
                    "logout" => {
                        let r = v1::LogoutRequest::decode(body.as_slice()).map_err(decode_fault)?;
                        auth.logout(&r.refresh_token)
                            .await
                            .map(|()| Vec::new())
                            .map_err(to_fault)
                    }
                    other => Err(RpcFault::internal(format!("unknown method {other}"))),
                }
            }
        })
        .await
}

/// Gateway-side stub: speaks the [`Auth`] trait by issuing NATS RPC, so it drops
/// into the gateway's `Arc<dyn Auth>` seam unchanged in a split deployment.
pub struct AuthNatsClient {
    rpc: RpcClient,
}

impl AuthNatsClient {
    #[must_use]
    pub fn new(rpc: RpcClient) -> Self {
        Self { rpc }
    }

    async fn unit_call(&self, method: &str, req: Vec<u8>) -> Result<(), AuthError> {
        self.rpc.call(SERVICE, method, req).await.map_err(to_err)?;
        Ok(())
    }

    async fn success_call(&self, method: &str, req: Vec<u8>) -> Result<v1::AuthSuccess, AuthError> {
        let bytes = self.rpc.call(SERVICE, method, req).await.map_err(to_err)?;
        v1::AuthSuccess::decode(bytes.as_slice()).map_err(internal)
    }
}

#[async_trait::async_trait]
impl Auth for AuthNatsClient {
    async fn register(
        &self,
        email: &str,
        username: &str,
        password: &str,
        ip: Option<IpAddr>,
    ) -> Result<v1::AuthSuccess, AuthError> {
        let req = rpc::AuthRegisterReq {
            email: email.to_owned(),
            username: username.to_owned(),
            password: password.to_owned(),
            ip: ip_to_str(ip),
        };
        self.success_call("register", req.encode_to_vec()).await
    }

    async fn login(
        &self,
        email: &str,
        password: &str,
        ip: Option<IpAddr>,
    ) -> Result<LoginOutcome, AuthError> {
        let req = rpc::AuthLoginReq {
            email: email.to_owned(),
            password: password.to_owned(),
            ip: ip_to_str(ip),
        };
        let bytes = self
            .rpc
            .call(SERVICE, "login", req.encode_to_vec())
            .await
            .map_err(to_err)?;
        login_outcome(v1::LoginResponse::decode(bytes.as_slice()).map_err(internal)?)
    }

    async fn complete_totp_login(
        &self,
        ticket: &str,
        code: &str,
    ) -> Result<v1::AuthSuccess, AuthError> {
        let req = v1::CompleteTotpRequest {
            ticket: ticket.to_owned(),
            code: code.to_owned(),
        };
        self.success_call("complete_totp_login", req.encode_to_vec())
            .await
    }

    async fn totp_enroll(&self, user: UserId) -> Result<TotpEnrollment, AuthError> {
        let req = rpc::AuthUserReq { user: user.raw() };
        let bytes = self
            .rpc
            .call(SERVICE, "totp_enroll", req.encode_to_vec())
            .await
            .map_err(to_err)?;
        let r = v1::TotpEnrollResponse::decode(bytes.as_slice()).map_err(internal)?;
        Ok(TotpEnrollment {
            secret: r.secret,
            otpauth_uri: r.otpauth_uri,
        })
    }

    async fn totp_confirm(&self, user: UserId, code: &str) -> Result<Vec<String>, AuthError> {
        let req = rpc::AuthUserCodeReq {
            user: user.raw(),
            code: code.to_owned(),
        };
        let bytes = self
            .rpc
            .call(SERVICE, "totp_confirm", req.encode_to_vec())
            .await
            .map_err(to_err)?;
        Ok(v1::TotpConfirmResponse::decode(bytes.as_slice())
            .map_err(internal)?
            .recovery_codes)
    }

    async fn totp_disable(&self, user: UserId, code: &str) -> Result<(), AuthError> {
        let req = rpc::AuthUserCodeReq {
            user: user.raw(),
            code: code.to_owned(),
        };
        self.unit_call("totp_disable", req.encode_to_vec()).await
    }

    async fn verify_email(&self, token: &str) -> Result<(), AuthError> {
        let req = v1::VerifyEmailRequest {
            token: token.to_owned(),
        };
        self.unit_call("verify_email", req.encode_to_vec()).await
    }

    async fn resend_verification(&self, user: UserId) -> Result<(), AuthError> {
        let req = rpc::AuthUserReq { user: user.raw() };
        self.unit_call("resend_verification", req.encode_to_vec())
            .await
    }

    async fn request_password_reset(
        &self,
        email: &str,
        ip: Option<IpAddr>,
    ) -> Result<(), AuthError> {
        let req = rpc::AuthPasswordResetReq {
            email: email.to_owned(),
            ip: ip_to_str(ip),
        };
        self.unit_call("request_password_reset", req.encode_to_vec())
            .await
    }

    async fn reset_password(&self, token: &str, new_password: &str) -> Result<(), AuthError> {
        let req = v1::PasswordResetConfirm {
            token: token.to_owned(),
            new_password: new_password.to_owned(),
        };
        self.unit_call("reset_password", req.encode_to_vec()).await
    }

    async fn refresh(&self, refresh_token: &str) -> Result<v1::AuthSuccess, AuthError> {
        let req = v1::RefreshRequest {
            refresh_token: refresh_token.to_owned(),
        };
        self.success_call("refresh", req.encode_to_vec()).await
    }

    async fn logout(&self, refresh_token: &str) -> Result<(), AuthError> {
        let req = v1::LogoutRequest {
            refresh_token: refresh_token.to_owned(),
        };
        self.unit_call("logout", req.encode_to_vec()).await
    }
}
