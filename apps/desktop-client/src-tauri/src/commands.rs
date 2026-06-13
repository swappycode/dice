//! `#[tauri::command]` shims — names and payload shapes match
//! `apps/desktop-client/src/lib/ipc.ts` EXACTLY. No logic lives here: every
//! body delegates to a plain async fn on [`ClientCore`] so the surface is
//! testable without Tauri.
//!
//! Conventions (the webview side of the contract):
//! - command names are snake_case (`invoke("send_message", …)`);
//! - argument KEYS arrive camelCase from JS and Tauri 2 maps them onto these
//!   snake_case parameters by default (`channelId` → `channel_id`) — exactly
//!   what ipc.ts sends, so no `rename_all` overrides are needed;
//! - every id crosses as a `String` (u64 snowflakes overflow JS numbers);
//! - errors cross as a plain user-presentable string (`CoreError::user_message`).

use std::sync::Arc;

use tauri::State;

use crate::dto::{
    AttachmentDto, BootstrapDto, ChannelDto, GuildDto, LoginResultDto, MessageDto, SessionDto,
    TotpEnrollDto, UnreadDto,
};
use crate::state::{ClientCore, CoreError};

type Core<'a> = State<'a, Arc<ClientCore>>;
type CmdResult<T> = Result<T, String>;

fn user(e: CoreError) -> String {
    e.user_message()
}

/// `getSession()`: resume from keystore + cache; `null` when logged out.
#[tauri::command]
pub async fn session_status(core: Core<'_>) -> CmdResult<Option<SessionDto>> {
    core.session_status().await.map_err(user)
}

#[tauri::command]
pub async fn login(core: Core<'_>, email: String, password: String) -> CmdResult<LoginResultDto> {
    core.login(&email, &password).await.map_err(user)
}

/// Finish a 2FA login: the challenge ticket + a TOTP or recovery code.
#[tauri::command]
pub async fn complete_totp_login(
    core: Core<'_>,
    ticket: String,
    code: String,
) -> CmdResult<SessionDto> {
    core.complete_totp_login(&ticket, &code).await.map_err(user)
}

/// Begin 2FA enrollment: returns the secret + `otpauth://` URI for the QR.
#[tauri::command]
pub async fn totp_enroll(core: Core<'_>) -> CmdResult<TotpEnrollDto> {
    core.totp_enroll().await.map_err(user)
}

/// Activate 2FA with a code from the enrolled secret; returns recovery codes.
#[tauri::command]
pub async fn totp_confirm(core: Core<'_>, code: String) -> CmdResult<Vec<String>> {
    core.totp_confirm(&code).await.map_err(user)
}

/// Disable 2FA (requires a current TOTP or recovery code).
#[tauri::command]
pub async fn totp_disable(core: Core<'_>, code: String) -> CmdResult<()> {
    core.totp_disable(&code).await.map_err(user)
}

/// Confirm an email address with a mailed verification token.
#[tauri::command]
pub async fn verify_email(core: Core<'_>, token: String) -> CmdResult<()> {
    core.verify_email(&token).await.map_err(user)
}

/// Re-send the verification mail to the signed-in user.
#[tauri::command]
pub async fn resend_verification(core: Core<'_>) -> CmdResult<()> {
    core.resend_verification().await.map_err(user)
}

/// Request a password-reset mail (always succeeds; no account enumeration).
#[tauri::command]
pub async fn request_password_reset(core: Core<'_>, email: String) -> CmdResult<()> {
    core.request_password_reset(&email).await.map_err(user)
}

/// Set a new password from a reset token.
#[tauri::command]
pub async fn reset_password(core: Core<'_>, token: String, new_password: String) -> CmdResult<()> {
    core.reset_password(&token, &new_password).await.map_err(user)
}

#[tauri::command]
pub async fn register(
    core: Core<'_>,
    email: String,
    username: String,
    password: String,
) -> CmdResult<SessionDto> {
    core.register(&email, &username, &password)
        .await
        .map_err(user)
}

#[tauri::command]
pub async fn logout(core: Core<'_>) -> CmdResult<()> {
    core.logout().await.map_err(user)
}

#[tauri::command]
pub async fn get_bootstrap(core: Core<'_>) -> CmdResult<BootstrapDto> {
    core.get_bootstrap().await.map_err(user)
}

/// Optimistic send: returns the PENDING message row (negative id, caller's
/// nonce); the `messageCreate` event later reconciles it by nonce.
#[tauri::command]
pub async fn send_message(
    core: Core<'_>,
    channel_id: String,
    content: String,
    reply_to_id: Option<String>,
    attachment_ids: Option<Vec<String>>,
    nonce: String,
) -> CmdResult<MessageDto> {
    core.send_message(
        &channel_id,
        &content,
        reply_to_id.as_deref(),
        &attachment_ids.unwrap_or_default(),
        &nonce,
    )
    .await
    .map_err(user)
}

/// Upload one file ahead of a send. `data_base64` is the raw bytes base64'd
/// (the JS side strips the `data:` prefix). Returns the stored attachment.
#[tauri::command]
pub async fn upload_attachment(
    core: Core<'_>,
    filename: String,
    content_type: String,
    data_base64: String,
) -> CmdResult<AttachmentDto> {
    core.upload_attachment(&filename, &content_type, &data_base64)
        .await
        .map_err(user)
}

/// Fetch an attachment's bytes as a `data:` URL the webview renders directly.
#[tauri::command]
pub async fn fetch_attachment(core: Core<'_>, media_id: String) -> CmdResult<String> {
    core.fetch_attachment(&media_id).await.map_err(user)
}

/// Set (or clear, `media_id = None`) the caller's avatar. Confirmed via the
/// `userUpdate` event the server broadcasts back.
#[tauri::command]
pub async fn set_avatar(core: Core<'_>, media_id: Option<String>) -> CmdResult<()> {
    core.set_avatar(media_id.as_deref()).await.map_err(user)
}

/// The caller's non-zero per-channel unread counts (for badges on boot).
#[tauri::command]
pub async fn fetch_unread(core: Core<'_>) -> CmdResult<Vec<UnreadDto>> {
    core.fetch_unread().await.map_err(user)
}

/// Clear a channel's unread badge (on open / read).
#[tauri::command]
pub async fn mark_read(core: Core<'_>, channel_id: String) -> CmdResult<()> {
    core.mark_read(&channel_id).await.map_err(user)
}

/// Toggle a reaction emoji on a message (server enforces membership).
#[tauri::command]
pub async fn react(
    core: Core<'_>,
    channel_id: String,
    message_id: String,
    emoji: String,
    add: bool,
) -> CmdResult<()> {
    core.react(&channel_id, &message_id, &emoji, add)
        .await
        .map_err(user)
}

#[tauri::command]
pub async fn fetch_messages(
    core: Core<'_>,
    channel_id: String,
    before: Option<String>,
    limit: Option<u32>,
) -> CmdResult<Vec<MessageDto>> {
    core.fetch_messages(&channel_id, before.as_deref(), limit)
        .await
        .map_err(user)
}

/// Edit a message (author-only). The `messageUpdate` event reconciles the UI.
#[tauri::command]
pub async fn edit_message(
    core: Core<'_>,
    channel_id: String,
    message_id: String,
    content: String,
) -> CmdResult<()> {
    core.edit_message(&channel_id, &message_id, &content)
        .await
        .map_err(user)
}

/// Delete a message. The `messageDelete` event removes it from the UI.
#[tauri::command]
pub async fn delete_message(
    core: Core<'_>,
    channel_id: String,
    message_id: String,
) -> CmdResult<()> {
    core.delete_message(&channel_id, &message_id)
        .await
        .map_err(user)
}

/// Host-throttled to 1 per 8 s per channel; lossy while disconnected.
#[tauri::command]
pub async fn start_typing(core: Core<'_>, channel_id: String) -> CmdResult<()> {
    core.start_typing(&channel_id).await.map_err(user)
}

#[tauri::command]
pub async fn set_presence(core: Core<'_>, status: String) -> CmdResult<()> {
    core.set_presence(&status).await.map_err(user)
}

#[tauri::command]
pub async fn create_guild(core: Core<'_>, name: String) -> CmdResult<GuildDto> {
    core.create_guild(&name).await.map_err(user)
}

#[tauri::command]
pub async fn join_guild(core: Core<'_>, code: String) -> CmdResult<GuildDto> {
    core.join_guild(&code).await.map_err(user)
}

#[tauri::command]
pub async fn open_dm(core: Core<'_>, recipient_id: String) -> CmdResult<ChannelDto> {
    core.open_dm(&recipient_id).await.map_err(user)
}

/// Pull-style mirror of the `connState` event stream
/// (`"idle" | "connecting" | "connected" | "reconnecting" | "offline"`).
#[tauri::command]
pub fn connection_state(core: Core<'_>) -> String {
    core.connection_state()
}
