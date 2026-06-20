//! REST surface (docs/protocol.md §10): protobuf bodies everywhere, errors
//! as encoded `dice.v1.Error` + the matching HTTP status, bearer-JWT
//! middleware on everything except `/v1/auth/*`, `/healthz` and the WS
//! upgrade. Served over the hand-rolled TLS loop in `dice-network-core`.
//!
//! Client IP: the accept loop injects the TLS peer address as a `PeerAddr`
//! request extension, so the auth endpoints pass the real source IP into the
//! per-IP rate limiter (X-Forwarded-For is deliberately NOT trusted — no proxy
//! in front yet). Requests served without the extension (none today) degrade
//! to `None` = the shared `noip` bucket.

use std::net::IpAddr;
use std::sync::Arc;

use auth_service::AuthError;
use axum::extract::ws::WebSocketUpgrade;
use axum::extract::{DefaultBodyLimit, FromRequest, Path, RawQuery, Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use bytes::Bytes;
use chat_service::HistoryCursor;
use dice_common::id::{ChannelId, GuildId, MediaId, UserId};
use dice_network_core::server::{FramedTransport, PeerAddr};
use dice_protocol::MAX_FRAME_BYTES;
use dice_protocol::v1::{self, ErrorCode};
use media_service::{MAX_MEDIA_BYTES, MediaError};
use prost::Message;

use crate::dispatch::chat_error_code;
use crate::ws::WsTransport;
use crate::{Gateway, session};

/// Request-body cap (1 MiB) for the protobuf REST surface + the realtime path.
pub(crate) const BODY_LIMIT: usize = 1024 * 1024;

/// Upload-body cap for `POST /v1/media` — the binary path, deliberately larger
/// than [`BODY_LIMIT`]. Sized to the media-service per-object cap plus protobuf
/// framing slack (filename/content_type + field headers).
pub(crate) const MEDIA_BODY_LIMIT: usize = MAX_MEDIA_BYTES + 64 * 1024;

const PROTOBUF: &str = "application/x-protobuf";

/// History `limit` default when the query omits it.
const DEFAULT_HISTORY_LIMIT: u8 = 50;

pub(crate) fn build_router(gw: Arc<Gateway>) -> axum::Router {
    let public = axum::Router::new()
        .route("/healthz", get(healthz))
        .route("/v1/auth/register", post(register))
        .route("/v1/auth/login", post(login))
        .route("/v1/auth/login/totp", post(complete_totp_login))
        .route("/v1/auth/verify-email", post(verify_email))
        .route(
            "/v1/auth/password-reset/request",
            post(password_reset_request),
        )
        .route(
            "/v1/auth/password-reset/confirm",
            post(password_reset_confirm),
        )
        .route("/v1/auth/refresh", post(refresh))
        .route("/v1/auth/logout", post(logout))
        .route("/gateway/v1", get(gateway_ws));

    let protected = axum::Router::new()
        .route("/v1/channels/{channel_id}/messages", get(message_history))
        .route("/v1/guilds", post(create_guild))
        .route("/v1/guilds/join", post(join_guild))
        .route("/v1/channels", post(create_channel))
        .route("/v1/dms", post(open_dm))
        .route("/v1/friends", get(list_friends).post(add_friend))
        .route("/v1/friends/{user_id}/accept", post(accept_friend))
        .route("/v1/friends/{user_id}/decline", post(decline_friend))
        .route("/v1/friends/{user_id}/remove", post(remove_friend))
        .route("/v1/unread", get(unread_counts))
        .route("/v1/channels/{channel_id}/read", post(mark_read))
        .route("/v1/channels/{channel_id}/voice", get(voice_roster))
        .route("/v1/channels/{channel_id}/voice/join", post(voice_join))
        .route("/v1/channels/{channel_id}/voice/leave", post(voice_leave))
        .route("/v1/channels/{channel_id}/voice/state", post(voice_state))
        .route("/v1/users/@me/avatar", put(set_avatar))
        .route("/v1/users/@me/totp/enroll", post(totp_enroll))
        .route("/v1/users/@me/totp/confirm", post(totp_confirm))
        .route("/v1/users/@me/totp/disable", post(totp_disable))
        .route("/v1/auth/verify-email/resend", post(resend_verification))
        // Download is a GET (no request body), so it rides the 1 MiB limit fine.
        .route("/v1/media/{media_id}", get(serve_media))
        .route_layer(axum::middleware::from_fn_with_state(
            Arc::clone(&gw),
            bearer_auth,
        ));

    // Upload is binary and gets its OWN, larger body limit (≠ the 1 MiB cap on
    // everything else). Merged AFTER the 1 MiB layer below so it is not also
    // capped to 1 MiB; both the extractor (`DefaultBodyLimit`) and the tower
    // limit layer are raised to `MEDIA_BODY_LIMIT`.
    let media_upload = axum::Router::new()
        .route("/v1/media", post(upload_media))
        .route_layer(axum::middleware::from_fn_with_state(
            Arc::clone(&gw),
            bearer_auth,
        ))
        .layer(DefaultBodyLimit::max(MEDIA_BODY_LIMIT))
        .layer(tower_http::limit::RequestBodyLimitLayer::new(
            MEDIA_BODY_LIMIT,
        ));

    public
        .merge(protected)
        // Last layer added = outermost: the 413 rewriter sees the limit
        // layer's short-circuit response and turns it into a proto Error.
        .layer(tower_http::limit::RequestBodyLimitLayer::new(BODY_LIMIT))
        .merge(media_upload)
        .layer(axum::middleware::from_fn(proto_payload_too_large))
        .with_state(gw)
}

/// `RequestBodyLimitLayer` short-circuits announced-oversize bodies with an
/// empty 413; rewrite that into the spec's encoded `dice.v1.Error` body.
async fn proto_payload_too_large(req: Request, next: Next) -> Response {
    let response = next.run(req).await;
    let already_proto = response
        .headers()
        .get(header::CONTENT_TYPE)
        .is_some_and(|ct| ct.as_bytes() == PROTOBUF.as_bytes());
    if response.status() == StatusCode::PAYLOAD_TOO_LARGE && !already_proto {
        return error_response(ErrorCode::PayloadTooLarge, "request body too large", 0);
    }
    response
}

// ----------------------------------------------------------- proto plumbing

fn proto_response<M: Message>(status: StatusCode, message: &M) -> Response {
    (
        status,
        [(header::CONTENT_TYPE, PROTOBUF)],
        message.encode_to_vec(),
    )
        .into_response()
}

fn http_status(code: ErrorCode) -> StatusCode {
    match code {
        ErrorCode::Unauthenticated | ErrorCode::InvalidSession => StatusCode::UNAUTHORIZED,
        ErrorCode::RateLimited => StatusCode::TOO_MANY_REQUESTS,
        ErrorCode::PermissionDenied => StatusCode::FORBIDDEN,
        ErrorCode::NotFound => StatusCode::NOT_FOUND,
        ErrorCode::InvalidArgument => StatusCode::BAD_REQUEST,
        ErrorCode::PayloadTooLarge => StatusCode::PAYLOAD_TOO_LARGE,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

fn error_response(code: ErrorCode, message: impl Into<String>, retry_after_ms: u32) -> Response {
    proto_response(
        http_status(code),
        &v1::Error {
            code: code as i32,
            message: message.into(),
            retry_after_ms,
            redirect_addr: String::new(),
        },
    )
}

fn map_auth_error(error: AuthError) -> Response {
    match error {
        AuthError::EmailTaken
        | AuthError::UsernameTaken
        | AuthError::InvalidArgument(_)
        | AuthError::TotpAlreadyEnabled
        | AuthError::TotpNotEnabled => {
            error_response(ErrorCode::InvalidArgument, error.to_string(), 0)
        }
        AuthError::InvalidCredentials
        | AuthError::EmailNotVerified
        | AuthError::InvalidToken
        | AuthError::InvalidTotp => {
            error_response(ErrorCode::Unauthenticated, error.to_string(), 0)
        }
        AuthError::RateLimited { retry_after_ms } => {
            error_response(ErrorCode::RateLimited, "rate limited", retry_after_ms)
        }
        AuthError::Internal(_) => {
            tracing::error!(%error, "auth call failed");
            error_response(ErrorCode::Internal, "internal error", 0)
        }
    }
}

fn map_chat_error(error: chat_service::ChatError) -> Response {
    let code = chat_error_code(&error);
    if code == ErrorCode::Internal {
        tracing::error!(%error, "chat call failed");
        return error_response(code, "internal error", 0);
    }
    error_response(code, error.to_string(), 0)
}

fn map_voice_error(error: voice_service::VoiceError) -> Response {
    use voice_service::VoiceError as E;
    match error {
        E::NotFound => error_response(ErrorCode::NotFound, "channel not found", 0),
        E::NotAVoiceChannel => error_response(ErrorCode::InvalidArgument, "not a voice channel", 0),
        E::NotAMember => {
            error_response(ErrorCode::PermissionDenied, "not a member of this guild", 0)
        }
        E::NotInChannel => {
            error_response(ErrorCode::InvalidArgument, "not in this voice channel", 0)
        }
        E::Internal(source) => {
            tracing::error!(error = %source, "voice call failed");
            error_response(ErrorCode::Internal, "internal error", 0)
        }
    }
}

fn map_media_error(error: MediaError) -> Response {
    match error {
        MediaError::NotFound => error_response(ErrorCode::NotFound, "media not found", 0),
        MediaError::TooLarge { max } => error_response(
            ErrorCode::PayloadTooLarge,
            format!("file exceeds {max} bytes"),
            0,
        ),
        MediaError::InvalidArgument(message) => {
            error_response(ErrorCode::InvalidArgument, message, 0)
        }
        MediaError::Internal(source) => {
            tracing::error!(error = %source, "media call failed");
            error_response(ErrorCode::Internal, "internal error", 0)
        }
    }
}

/// Protobuf body extractor: decodes `T` from the (1 MiB-capped) body.
/// Rejections are already protobuf `dice.v1.Error` responses.
pub(crate) struct Proto<T>(pub T);

impl<S, T> FromRequest<S> for Proto<T>
where
    S: Send + Sync,
    T: Message + Default,
{
    type Rejection = Response;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        let bytes = Bytes::from_request(req, state).await.map_err(|rejection| {
            if rejection.status() == StatusCode::PAYLOAD_TOO_LARGE {
                error_response(ErrorCode::PayloadTooLarge, "request body too large", 0)
            } else {
                error_response(ErrorCode::InvalidArgument, "unreadable request body", 0)
            }
        })?;
        T::decode(bytes)
            .map(Proto)
            .map_err(|e| error_response(ErrorCode::InvalidArgument, format!("protobuf: {e}"), 0))
    }
}

// ------------------------------------------------------------- middleware

/// Request extension inserted by [`bearer_auth`].
#[derive(Clone, Copy)]
struct Authed(UserId);

async fn bearer_auth(State(gw): State<Arc<Gateway>>, mut req: Request, next: Next) -> Response {
    let user = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .and_then(|token| dice_auth_core::token::verify_access(&gw.deps.jwt, token).ok())
        .and_then(|claims| claims.user_id());
    match user {
        Some(user) => {
            req.extensions_mut().insert(Authed(user));
            next.run(req).await
        }
        None => error_response(
            ErrorCode::Unauthenticated,
            "missing or invalid bearer token",
            0,
        ),
    }
}

// ---------------------------------------------------------------- handlers

async fn healthz() -> &'static str {
    "ok"
}

/// The client IP for per-IP rate limiting, from the `PeerAddr` extension the
/// TLS accept loop injects. `None` (extension absent) shares the `noip` bucket.
fn client_ip(peer: Option<axum::Extension<PeerAddr>>) -> Option<IpAddr> {
    peer.map(|axum::Extension(PeerAddr(addr))| addr.ip())
}

async fn register(
    State(gw): State<Arc<Gateway>>,
    peer: Option<axum::Extension<PeerAddr>>,
    Proto(req): Proto<v1::RegisterRequest>,
) -> Response {
    match gw
        .deps
        .auth
        .register(&req.email, &req.username, &req.password, client_ip(peer))
        .await
    {
        Ok(success) => proto_response(StatusCode::OK, &success),
        Err(error) => map_auth_error(error),
    }
}

async fn login(
    State(gw): State<Arc<Gateway>>,
    peer: Option<axum::Extension<PeerAddr>>,
    Proto(req): Proto<v1::LoginRequest>,
) -> Response {
    use auth_service::LoginOutcome;
    use v1::login_response::Outcome;
    match gw
        .deps
        .auth
        .login(&req.email, &req.password, client_ip(peer))
        .await
    {
        Ok(LoginOutcome::Success(success)) => proto_response(
            StatusCode::OK,
            &v1::LoginResponse {
                outcome: Some(Outcome::Success(*success)),
            },
        ),
        Ok(LoginOutcome::TotpRequired { ticket }) => proto_response(
            StatusCode::OK,
            &v1::LoginResponse {
                outcome: Some(Outcome::TotpRequired(v1::TotpChallenge { ticket })),
            },
        ),
        Err(error) => map_auth_error(error),
    }
}

/// `POST /v1/auth/login/totp` (public): finish a 2FA login by presenting the
/// challenge ticket + a TOTP or recovery code. Returns the full `AuthSuccess`.
async fn complete_totp_login(
    State(gw): State<Arc<Gateway>>,
    Proto(req): Proto<v1::CompleteTotpRequest>,
) -> Response {
    match gw
        .deps
        .auth
        .complete_totp_login(&req.ticket, &req.code)
        .await
    {
        Ok(success) => proto_response(StatusCode::OK, &success),
        Err(error) => map_auth_error(error),
    }
}

/// `POST /v1/auth/verify-email` (public): confirm an address with the mailed
/// token. 204.
async fn verify_email(
    State(gw): State<Arc<Gateway>>,
    Proto(req): Proto<v1::VerifyEmailRequest>,
) -> Response {
    match gw.deps.auth.verify_email(&req.token).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => map_auth_error(error),
    }
}

/// `POST /v1/auth/password-reset/request` (public): mail a reset token if the
/// address is registered. ALWAYS 204 (no account-enumeration oracle).
async fn password_reset_request(
    State(gw): State<Arc<Gateway>>,
    peer: Option<axum::Extension<PeerAddr>>,
    Proto(req): Proto<v1::PasswordResetRequest>,
) -> Response {
    match gw
        .deps
        .auth
        .request_password_reset(&req.email, client_ip(peer))
        .await
    {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => map_auth_error(error),
    }
}

/// `POST /v1/auth/password-reset/confirm` (public): set a new password from a
/// reset token; every session is revoked. 204.
async fn password_reset_confirm(
    State(gw): State<Arc<Gateway>>,
    Proto(req): Proto<v1::PasswordResetConfirm>,
) -> Response {
    match gw
        .deps
        .auth
        .reset_password(&req.token, &req.new_password)
        .await
    {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => map_auth_error(error),
    }
}

async fn refresh(
    State(gw): State<Arc<Gateway>>,
    Proto(req): Proto<v1::RefreshRequest>,
) -> Response {
    match gw.deps.auth.refresh(&req.refresh_token).await {
        Ok(success) => proto_response(StatusCode::OK, &success),
        Err(error) => map_auth_error(error),
    }
}

async fn logout(State(gw): State<Arc<Gateway>>, Proto(req): Proto<v1::LogoutRequest>) -> Response {
    match gw.deps.auth.logout(&req.refresh_token).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => map_auth_error(error),
    }
}

async fn message_history(
    State(gw): State<Arc<Gateway>>,
    axum::Extension(Authed(user)): axum::Extension<Authed>,
    Path(channel_id): Path<String>,
    RawQuery(query): RawQuery,
) -> Response {
    let Ok(channel) = channel_id.parse::<ChannelId>() else {
        return error_response(ErrorCode::InvalidArgument, "invalid channel id", 0);
    };
    let (cursor, limit) = match parse_history_query(query.as_deref()) {
        Ok(parsed) => parsed,
        Err(message) => return error_response(ErrorCode::InvalidArgument, message, 0),
    };
    match gw
        .deps
        .chat
        .get_messages(user, channel, cursor, limit)
        .await
    {
        Ok(messages) => proto_response(StatusCode::OK, &v1::MessageHistory { messages }),
        Err(error) => map_chat_error(error),
    }
}

async fn create_guild(
    State(gw): State<Arc<Gateway>>,
    axum::Extension(Authed(user)): axum::Extension<Authed>,
    Proto(req): Proto<v1::CreateGuildRequest>,
) -> Response {
    match gw.deps.chat.create_guild(user, req.name).await {
        Ok(guild) => proto_response(StatusCode::OK, &guild),
        Err(error) => map_chat_error(error),
    }
}

async fn join_guild(
    State(gw): State<Arc<Gateway>>,
    axum::Extension(Authed(user)): axum::Extension<Authed>,
    Proto(req): Proto<v1::JoinGuildRequest>,
) -> Response {
    match gw.deps.chat.join_guild(user, &req.code).await {
        Ok(guild) => proto_response(StatusCode::OK, &guild),
        Err(error) => map_chat_error(error),
    }
}

async fn create_channel(
    State(gw): State<Arc<Gateway>>,
    axum::Extension(Authed(user)): axum::Extension<Authed>,
    Proto(req): Proto<v1::CreateChannelRequest>,
) -> Response {
    let kind = req.kind();
    match gw
        .deps
        .chat
        .create_channel(user, GuildId::from_raw(req.guild_id), req.name, kind)
        .await
    {
        Ok(channel) => proto_response(StatusCode::OK, &channel),
        Err(error) => map_chat_error(error),
    }
}

async fn open_dm(
    State(gw): State<Arc<Gateway>>,
    axum::Extension(Authed(user)): axum::Extension<Authed>,
    Proto(req): Proto<v1::OpenDmRequest>,
) -> Response {
    match gw
        .deps
        .chat
        .open_dm(user, UserId::from_raw(req.recipient_id))
        .await
    {
        Ok(channel) => proto_response(StatusCode::OK, &channel),
        Err(error) => map_chat_error(error),
    }
}

/// `GET /v1/friends` (bearer): the caller's friends + pending requests.
async fn list_friends(
    State(gw): State<Arc<Gateway>>,
    axum::Extension(Authed(user)): axum::Extension<Authed>,
) -> Response {
    match gw.deps.chat.list_friends(user).await {
        Ok(list) => proto_response(StatusCode::OK, &list),
        Err(error) => map_chat_error(error),
    }
}

/// `POST /v1/friends` (bearer): send a friend request by username (accepts a
/// reverse-pending request if one exists). Returns the relationship.
async fn add_friend(
    State(gw): State<Arc<Gateway>>,
    axum::Extension(Authed(user)): axum::Extension<Authed>,
    Proto(req): Proto<v1::AddFriendRequest>,
) -> Response {
    match gw.deps.chat.add_friend(user, &req.username).await {
        Ok(friend) => proto_response(StatusCode::OK, &friend),
        Err(error) => map_chat_error(error),
    }
}

/// `POST /v1/friends/{user_id}/accept` (bearer): accept a pending incoming
/// request. Returns the accepted relationship.
async fn accept_friend(
    State(gw): State<Arc<Gateway>>,
    axum::Extension(Authed(user)): axum::Extension<Authed>,
    Path(user_id): Path<String>,
) -> Response {
    let Ok(other) = user_id.parse::<UserId>() else {
        return error_response(ErrorCode::InvalidArgument, "invalid user id", 0);
    };
    match gw.deps.chat.accept_friend(user, other).await {
        Ok(friend) => proto_response(StatusCode::OK, &friend),
        Err(error) => map_chat_error(error),
    }
}

/// `POST /v1/friends/{user_id}/decline` (bearer): decline an incoming or cancel
/// an outgoing pending request. 204.
async fn decline_friend(
    State(gw): State<Arc<Gateway>>,
    axum::Extension(Authed(user)): axum::Extension<Authed>,
    Path(user_id): Path<String>,
) -> Response {
    let Ok(other) = user_id.parse::<UserId>() else {
        return error_response(ErrorCode::InvalidArgument, "invalid user id", 0);
    };
    match gw.deps.chat.decline_friend(user, other).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => map_chat_error(error),
    }
}

/// `POST /v1/friends/{user_id}/remove` (bearer): remove an accepted friend. 204.
async fn remove_friend(
    State(gw): State<Arc<Gateway>>,
    axum::Extension(Authed(user)): axum::Extension<Authed>,
    Path(user_id): Path<String>,
) -> Response {
    let Ok(other) = user_id.parse::<UserId>() else {
        return error_response(ErrorCode::InvalidArgument, "invalid user id", 0);
    };
    match gw.deps.chat.remove_friend(user, other).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => map_chat_error(error),
    }
}

/// `GET /v1/unread` (bearer): the caller's non-zero per-channel unread counts.
/// Channels come from the user's sync state; counts from notification-service.
async fn unread_counts(
    State(gw): State<Arc<Gateway>>,
    axum::Extension(Authed(user)): axum::Extension<Authed>,
) -> Response {
    let state = match gw.deps.chat.sync_user_state(user).await {
        Ok(state) => state,
        Err(error) => return map_chat_error(error),
    };
    // Collect ids first so nothing borrows `state` across the await loop.
    let channels: Vec<u64> = state
        .guilds
        .iter()
        .flat_map(|g| g.channels.iter().map(|c| c.id))
        .chain(state.dm_channels.iter().map(|c| c.id))
        .collect();
    let mut entries = Vec::new();
    for channel in channels {
        match gw
            .deps
            .unread
            .count(user, ChannelId::from_raw(channel))
            .await
        {
            Ok(0) => {}
            Ok(count) => entries.push(v1::UnreadEntry {
                channel_id: channel,
                count,
            }),
            Err(error) => tracing::warn!(%error, channel, "unread count read failed"),
        }
    }
    proto_response(StatusCode::OK, &v1::UnreadCounts { entries })
}

/// `POST /v1/channels/{id}/read` (bearer): advance the caller's read marker to
/// the channel's newest message and clear its unread badge. 204. chat-service
/// persists the marker + broadcasts `ReadMarkerUpdate` (multi-device sync); the
/// unread counter is cleared here.
async fn mark_read(
    State(gw): State<Arc<Gateway>>,
    axum::Extension(Authed(user)): axum::Extension<Authed>,
    Path(channel_id): Path<String>,
) -> Response {
    let Ok(channel) = channel_id.parse::<ChannelId>() else {
        return error_response(ErrorCode::InvalidArgument, "invalid channel id", 0);
    };
    if let Err(error) = gw.deps.chat.mark_read(user, channel).await {
        return map_chat_error(error);
    }
    // Marker is persisted + broadcast; a counter-clear hiccup is non-fatal (the
    // ReadMarkerUpdate dispatch clears the badge on every device regardless).
    if let Err(error) = gw.deps.unread.clear(user, channel).await {
        tracing::warn!(%error, "unread clear failed (marker still recorded)");
    }
    StatusCode::NO_CONTENT.into_response()
}

/// `POST /v1/channels/{id}/voice/join` (bearer): join a VOICE channel. Returns
/// the current roster; voice-service publishes `VoiceJoin` to the guild.
async fn voice_join(
    State(gw): State<Arc<Gateway>>,
    axum::Extension(Authed(user)): axum::Extension<Authed>,
    Path(channel_id): Path<String>,
    Proto(req): Proto<v1::VoiceJoinRequest>,
) -> Response {
    let Ok(channel) = channel_id.parse::<ChannelId>() else {
        return error_response(ErrorCode::InvalidArgument, "invalid channel id", 0);
    };
    match gw
        .deps
        .voice
        .join(user, channel, req.muted, req.deafened)
        .await
    {
        Ok(roster) => proto_response(StatusCode::OK, &roster),
        Err(error) => map_voice_error(error),
    }
}

/// `POST /v1/channels/{id}/voice/leave` (bearer): leave a voice channel. 204.
/// Idempotent; publishes `VoiceLeave` if the caller was in it.
async fn voice_leave(
    State(gw): State<Arc<Gateway>>,
    axum::Extension(Authed(user)): axum::Extension<Authed>,
    Path(channel_id): Path<String>,
) -> Response {
    let Ok(channel) = channel_id.parse::<ChannelId>() else {
        return error_response(ErrorCode::InvalidArgument, "invalid channel id", 0);
    };
    match gw.deps.voice.leave(user, channel).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => map_voice_error(error),
    }
}

/// `POST /v1/channels/{id}/voice/state` (bearer): update the caller's own
/// mute/deafen/speaking flags. 204; publishes `VoiceState`.
async fn voice_state(
    State(gw): State<Arc<Gateway>>,
    axum::Extension(Authed(user)): axum::Extension<Authed>,
    Path(channel_id): Path<String>,
    Proto(req): Proto<v1::VoiceStateRequest>,
) -> Response {
    let Ok(channel) = channel_id.parse::<ChannelId>() else {
        return error_response(ErrorCode::InvalidArgument, "invalid channel id", 0);
    };
    match gw
        .deps
        .voice
        .update_state(user, channel, req.muted, req.deafened, req.speaking)
        .await
    {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => map_voice_error(error),
    }
}

/// `GET /v1/channels/{id}/voice` (bearer): the channel's current voice roster.
async fn voice_roster(
    State(gw): State<Arc<Gateway>>,
    axum::Extension(Authed(_user)): axum::Extension<Authed>,
    Path(channel_id): Path<String>,
) -> Response {
    let Ok(channel) = channel_id.parse::<ChannelId>() else {
        return error_response(ErrorCode::InvalidArgument, "invalid channel id", 0);
    };
    match gw.deps.voice.roster(channel).await {
        Ok(roster) => proto_response(StatusCode::OK, &roster),
        Err(error) => map_voice_error(error),
    }
}

/// `PUT /v1/users/@me/avatar` (bearer): set or clear the caller's avatar.
/// `media_id = 0` clears it. The `UserUpdate` broadcast (incl. the caller's own
/// subject) propagates the change, so this just returns 204.
async fn set_avatar(
    State(gw): State<Arc<Gateway>>,
    axum::Extension(Authed(user)): axum::Extension<Authed>,
    Proto(req): Proto<v1::SetAvatarRequest>,
) -> Response {
    let media = (req.media_id != 0).then(|| MediaId::from_raw(req.media_id));
    match gw.deps.chat.set_avatar(user, media).await {
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => map_chat_error(error),
    }
}

/// `POST /v1/users/@me/totp/enroll` (bearer): begin 2FA enrollment; returns the
/// new secret + `otpauth://` URI. Inactive until `confirm`.
async fn totp_enroll(
    State(gw): State<Arc<Gateway>>,
    axum::Extension(Authed(user)): axum::Extension<Authed>,
) -> Response {
    match gw.deps.auth.totp_enroll(user).await {
        Ok(e) => proto_response(
            StatusCode::OK,
            &v1::TotpEnrollResponse {
                secret: e.secret,
                otpauth_uri: e.otpauth_uri,
            },
        ),
        Err(error) => map_auth_error(error),
    }
}

/// `POST /v1/users/@me/totp/confirm` (bearer): activate 2FA with a code from the
/// enrolled secret; returns the one-time recovery codes.
async fn totp_confirm(
    State(gw): State<Arc<Gateway>>,
    axum::Extension(Authed(user)): axum::Extension<Authed>,
    Proto(req): Proto<v1::TotpConfirmRequest>,
) -> Response {
    match gw.deps.auth.totp_confirm(user, &req.code).await {
        Ok(recovery_codes) => {
            proto_response(StatusCode::OK, &v1::TotpConfirmResponse { recovery_codes })
        }
        Err(error) => map_auth_error(error),
    }
}

/// `POST /v1/users/@me/totp/disable` (bearer): turn 2FA off, requiring a current
/// TOTP or recovery code. 204.
async fn totp_disable(
    State(gw): State<Arc<Gateway>>,
    axum::Extension(Authed(user)): axum::Extension<Authed>,
    Proto(req): Proto<v1::TotpDisableRequest>,
) -> Response {
    match gw.deps.auth.totp_disable(user, &req.code).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => map_auth_error(error),
    }
}

/// `POST /v1/auth/verify-email/resend` (bearer, no body): re-send the
/// verification mail to the signed-in user. 204.
async fn resend_verification(
    State(gw): State<Arc<Gateway>>,
    axum::Extension(Authed(user)): axum::Extension<Authed>,
) -> Response {
    match gw.deps.auth.resend_verification(user).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => map_auth_error(error),
    }
}

/// `POST /v1/media` (bearer): upload one file (protobuf `UploadMediaRequest`
/// body under [`MEDIA_BODY_LIMIT`]). Returns the stored `Attachment`, whose id
/// the client then references in `SendMessageRequest.attachment_ids`.
async fn upload_media(
    State(gw): State<Arc<Gateway>>,
    axum::Extension(Authed(user)): axum::Extension<Authed>,
    Proto(req): Proto<v1::UploadMediaRequest>,
) -> Response {
    let v1::UploadMediaRequest {
        filename,
        content_type,
        data,
    } = req;
    match gw
        .deps
        .media
        .upload(user, &filename, &content_type, data)
        .await
    {
        Ok(obj) => proto_response(
            StatusCode::OK,
            &v1::UploadMediaResponse {
                attachment: Some(obj.to_attachment()),
            },
        ),
        Err(error) => map_media_error(error),
    }
}

/// `GET /v1/media/{id}` (bearer): stream the stored bytes with their MIME type.
/// Auth is enforced by the route's bearer middleware; any authenticated user
/// may fetch by the (unguessable snowflake) id. A channel-scoped ACL — only
/// members of a channel the media is attached to — is a future hardening.
async fn serve_media(State(gw): State<Arc<Gateway>>, Path(media_id): Path<String>) -> Response {
    let Ok(id) = media_id.parse::<MediaId>() else {
        return error_response(ErrorCode::InvalidArgument, "invalid media id", 0);
    };
    match gw.deps.media.read(id).await {
        Ok((obj, bytes)) => {
            let content_type = HeaderValue::from_str(&obj.content_type)
                .unwrap_or_else(|_| HeaderValue::from_static("application/octet-stream"));
            (
                StatusCode::OK,
                [
                    (header::CONTENT_TYPE, content_type),
                    (
                        header::CACHE_CONTROL,
                        HeaderValue::from_static("private, max-age=31536000, immutable"),
                    ),
                ],
                bytes,
            )
                .into_response()
        }
        Err(error) => map_media_error(error),
    }
}

/// WS upgrade: no bearer here — the socket authenticates via Identify.
async fn gateway_ws(State(gw): State<Arc<Gateway>>, ws: WebSocketUpgrade) -> Response {
    ws.max_message_size(MAX_FRAME_BYTES)
        .max_frame_size(MAX_FRAME_BYTES)
        .on_upgrade(move |socket| async move {
            let transport: Box<dyn FramedTransport> = Box::new(WsTransport::new(socket));
            let session = session::drive_connection(Arc::clone(&gw), transport);
            gw.tracker.track_future(session).await;
        })
}

// ------------------------------------------------------------ query parse

/// `?before|after=<id>&limit=1..100` (protocol §10). `before` and `after`
/// are mutually exclusive; ids are plain decimal snowflakes (never
/// percent-encoded), so a hand parser keeps serde off the request path.
fn parse_history_query(query: Option<&str>) -> Result<(HistoryCursor, u8), String> {
    let mut before: Option<u64> = None;
    let mut after: Option<u64> = None;
    let mut limit = DEFAULT_HISTORY_LIMIT;
    for pair in query.unwrap_or("").split('&').filter(|p| !p.is_empty()) {
        let (key, value) = pair.split_once('=').ok_or("malformed query parameter")?;
        match key {
            "before" => {
                before = Some(value.parse().map_err(|_| "invalid `before` id")?);
            }
            "after" => {
                after = Some(value.parse().map_err(|_| "invalid `after` id")?);
            }
            "limit" => {
                let parsed: u8 = value.parse().map_err(|_| "invalid `limit`")?;
                if !(1..=100).contains(&parsed) {
                    return Err("`limit` must be 1..=100".to_owned());
                }
                limit = parsed;
            }
            _ => {} // unknown params ignored (forward compat)
        }
    }
    let cursor = match (before, after) {
        (Some(_), Some(_)) => return Err("`before` and `after` are mutually exclusive".to_owned()),
        (Some(b), None) => HistoryCursor::Before(dice_common::id::MessageId::from_raw(b)),
        (None, Some(a)) => HistoryCursor::After(dice_common::id::MessageId::from_raw(a)),
        (None, None) => HistoryCursor::Latest,
    };
    Ok((cursor, limit))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn history_query_defaults() {
        let (cursor, limit) = parse_history_query(None).unwrap();
        assert!(matches!(cursor, HistoryCursor::Latest));
        assert_eq!(limit, DEFAULT_HISTORY_LIMIT);
        let (cursor, _) = parse_history_query(Some("")).unwrap();
        assert!(matches!(cursor, HistoryCursor::Latest));
    }

    #[test]
    fn history_query_before_after_limit() {
        let (cursor, limit) = parse_history_query(Some("before=42&limit=10")).unwrap();
        match cursor {
            HistoryCursor::Before(id) => assert_eq!(id.raw(), 42),
            other => panic!("expected Before, got {other:?}"),
        }
        assert_eq!(limit, 10);

        let (cursor, limit) = parse_history_query(Some("after=7")).unwrap();
        match cursor {
            HistoryCursor::After(id) => assert_eq!(id.raw(), 7),
            other => panic!("expected After, got {other:?}"),
        }
        assert_eq!(limit, DEFAULT_HISTORY_LIMIT);
    }

    #[test]
    fn history_query_rejects_bad_input() {
        assert!(parse_history_query(Some("before=1&after=2")).is_err());
        assert!(parse_history_query(Some("before=x")).is_err());
        assert!(parse_history_query(Some("limit=0")).is_err());
        assert!(parse_history_query(Some("limit=101")).is_err());
        assert!(parse_history_query(Some("limit")).is_err());
    }

    #[test]
    fn http_statuses_match_the_spec_table() {
        assert_eq!(
            http_status(ErrorCode::Unauthenticated),
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            http_status(ErrorCode::InvalidSession),
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            http_status(ErrorCode::RateLimited),
            StatusCode::TOO_MANY_REQUESTS
        );
        assert_eq!(
            http_status(ErrorCode::PermissionDenied),
            StatusCode::FORBIDDEN
        );
        assert_eq!(http_status(ErrorCode::NotFound), StatusCode::NOT_FOUND);
        assert_eq!(
            http_status(ErrorCode::InvalidArgument),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            http_status(ErrorCode::PayloadTooLarge),
            StatusCode::PAYLOAD_TOO_LARGE
        );
        assert_eq!(
            http_status(ErrorCode::Internal),
            StatusCode::INTERNAL_SERVER_ERROR
        );
    }
}
