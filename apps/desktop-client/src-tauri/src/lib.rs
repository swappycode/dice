//! Dice desktop host (Tauri 2). The brain is [`state::ClientCore`] — every
//! command body is a plain async fn there so the whole surface tests headless
//! (`tests/host_gate.rs`); this crate root only wires the Tauri runtime.
//!
//! Lifecycle rules (docs/design/desktop-client.md §2.4):
//! - ONE tokio runtime, built FIRST and installed via
//!   [`tauri::async_runtime::set`] BEFORE the Builder exists — nothing here
//!   ever calls a bare `tokio::spawn` from the setup hook.
//! - ring is the only rustls crypto provider (workspace policy).
//! - single-instance: a second launch focuses the existing window — UNLESS a
//!   `--profile <name>` / `DICE_CLIENT_PROFILE` is set, which gives that
//!   instance its own cache + keyring scope + WebView2 storage (so browser
//!   `localStorage` is isolated too) and lets it open its own window (run two
//!   profiles for local two-user testing).
//! - when the keystore already holds a session the gateway connects in the
//!   background at startup; otherwise the first `login`/`register` connects.

pub mod audio;
pub mod bridge;
pub mod cache;
pub mod commands;
pub mod dto;
pub mod emit;
pub mod keystore;
pub mod ptt;
pub mod session;
pub mod state;

use std::sync::Arc;

use tauri::{Manager, WebviewUrl, WebviewWindowBuilder};

use crate::emit::{Emitter, TauriEmitter};
use crate::keystore::{KeyStore, OsKeyring};
use crate::state::{ClientCore, CoreConfig};

/// Entry point shared by `main.rs` and the bundler glue.
pub fn run() {
    dice_network_core::tls::install_ring_provider();

    // Resolve the profile first so the log file is profile-scoped (alice/bob get
    // their own log for the two-client voice test).
    let profile = active_profile();
    init_tracing(open_log_file(profile.as_deref()));
    if let Some(name) = &profile {
        tracing::info!(profile = %name, "isolated dev profile: own cache + keyring + window");
    }

    // The ONE runtime. Built before anything Tauri so every spawn in this
    // process — Tauri's own async commands included — lands on it.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("build the tokio runtime");
    let rt_handle = rt.handle().clone();
    tauri::async_runtime::set(rt_handle.clone());

    let mut builder = tauri::Builder::default();
    // OS toast notifications (item 14): the host shows them via the plugin's
    // Rust API from the `notify` command.
    builder = builder.plugin(tauri_plugin_notification::init());
    // Global push-to-talk: the handler mirrors the (single) bound shortcut's
    // press/release into the audio gate; the `set_ptt` command binds the key.
    builder = builder.plugin(
        tauri_plugin_global_shortcut::Builder::new()
            .with_handler(ptt::on_shortcut)
            .build(),
    );
    // Single-instance only for the DEFAULT profile: a second normal launch
    // focuses the existing window, but a named `--profile` is explicitly
    // allowed its own window (local two-user testing).
    if profile.is_none() {
        builder = builder.plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.unminimize();
                let _ = window.set_focus();
            }
        }));
    }

    let setup_profile = profile;
    builder
        .setup(move |app| {
            let base = app.path().app_data_dir()?;
            // A profile gets its own `<app-data>/profiles/<name>` subtree
            // (cache.db + WebView2 storage below) + a scoped keyring entry; the
            // default keeps `<app-data>/cache.db` + the default keyring + the
            // default WebView2 dir so an existing install's data still resolves.
            let profile_dir = match &setup_profile {
                Some(name) => {
                    let dir = base.join("profiles").join(name);
                    std::fs::create_dir_all(&dir)?;
                    Some(dir)
                }
                None => None,
            };
            let (cache_path, keystore): (std::path::PathBuf, Arc<dyn KeyStore>) =
                match (&setup_profile, &profile_dir) {
                    (Some(name), Some(dir)) => {
                        (dir.join("cache.db"), Arc::new(OsKeyring::for_profile(name)))
                    }
                    _ => (base.join("cache.db"), Arc::new(OsKeyring::new())),
                };
            let cfg = CoreConfig::from_env(cache_path)?;
            let emitter: Arc<dyn Emitter> = Arc::new(TauriEmitter(app.handle().clone()));
            let core = Arc::new(ClientCore::new(cfg, keystore, emitter, rt_handle.clone())?);
            // Stored session ⇒ resume + connect in the background; the
            // webview's own `session_status` call is idempotent over this.
            if core.has_stored_session() {
                let core = Arc::clone(&core);
                rt_handle.spawn(async move {
                    if let Err(error) = core.session_status().await {
                        tracing::warn!(%error, "startup session resume failed");
                    }
                });
            }
            app.manage(core);

            // A named profile puts its name in the title bar so two side-by-side
            // instances (e.g. `client-as alice` / `client-as bob`) are tellable
            // apart in Alt-Tab / the taskbar; the default app stays plain "Dice".
            let title = match &setup_profile {
                Some(name) => format!("Dice \u{2014} {name}"),
                None => "Dice".to_owned(),
            };

            // Create the main window HERE (not in tauri.conf.json) so the
            // managed `ClientCore` is guaranteed present before the webview's
            // first IPC call, and so the WebView2 browser args are tunable at
            // runtime via `DICE_WEBVIEW_ARGS` (one build, many RAM experiments).
            let mut window = WebviewWindowBuilder::new(app, "main", WebviewUrl::default())
                .title(title)
                .inner_size(1100.0, 720.0)
                .min_inner_size(800.0, 560.0)
                .resizable(true)
                .decorations(false)
                .shadow(true)
                .transparent(false)
                .additional_browser_args(&webview_args());
            // A named profile also gets its OWN WebView2 user-data-folder, so two
            // side-by-side instances don't share browser `localStorage` (theme +
            // perf prefs, the mock session key) — full isolation for the two-user
            // demo. The default app keeps WebView2's default location.
            if let Some(dir) = &profile_dir {
                window = window.data_directory(dir.join("webview"));
            }
            window.build()?;
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::session_status,
            commands::login,
            commands::complete_totp_login,
            commands::totp_enroll,
            commands::totp_confirm,
            commands::totp_disable,
            commands::verify_email,
            commands::resend_verification,
            commands::request_password_reset,
            commands::reset_password,
            commands::register,
            commands::logout,
            commands::get_bootstrap,
            commands::send_message,
            commands::upload_attachment,
            commands::fetch_attachment,
            commands::set_avatar,
            commands::fetch_unread,
            commands::mark_read,
            commands::edit_message,
            commands::delete_message,
            commands::request_guild_members,
            commands::request_users,
            commands::react,
            commands::fetch_messages,
            commands::start_typing,
            commands::set_presence,
            commands::create_guild,
            commands::join_guild,
            commands::open_dm,
            commands::list_friends,
            commands::add_friend,
            commands::accept_friend,
            commands::decline_friend,
            commands::remove_friend,
            commands::voice_join,
            commands::voice_leave,
            commands::voice_state,
            commands::voice_roster,
            commands::create_channel,
            commands::connection_state,
            commands::notify,
            commands::set_ptt,
            commands::list_audio_devices,
            commands::set_audio_devices,
        ])
        .run(tauri::generate_context!())
        .expect("error while running the Dice desktop host");

    // The event loop ended: tear the runtime down without waiting for the
    // (deliberately long-lived) gateway/bridge tasks.
    rt.shutdown_background();
}

fn init_tracing(log_file: Option<FileLog>) {
    use tracing_subscriber::prelude::*;
    use tracing_subscriber::{EnvFilter, fmt};

    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,dice_desktop=debug,dice_network_core=debug"));
    // The file layer (ANSI off) is a no-op when there's no file. Stdout stays so
    // `tauri dev` (which has a console) is unchanged; the file is what makes the
    // release build — a windowless GUI app with no console — debuggable.
    let file_layer = log_file.map(|log_file| {
        fmt::layer()
            .with_ansi(false)
            .with_target(true)
            .with_writer(move || log_file.clone())
    });
    tracing_subscriber::registry()
        .with(filter)
        .with(fmt::layer().with_target(true))
        .with(file_layer)
        .init();
}

/// A cloneable, thread-safe writer over a single log file, for the tracing file
/// layer (one lock per event; events are small and low-volume).
#[derive(Clone)]
struct FileLog(Arc<std::sync::Mutex<std::fs::File>>);

impl std::io::Write for FileLog {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().expect("log file lock").write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.0.lock().expect("log file lock").flush()
    }
}

/// Open (truncate) this instance's `dice.log`, next to its `cache.db` in the
/// Tauri app-data dir (profile-scoped). Truncated per launch so a log reflects
/// exactly the current session. `None` (→ stdout only) if the dir is
/// unavailable — logging must never block startup.
fn open_log_file(profile: Option<&str>) -> Option<FileLog> {
    use std::io::Write as _;

    let base = app_data_dir()?;
    let dir = match profile {
        Some(name) => base.join("profiles").join(name),
        None => base,
    };
    std::fs::create_dir_all(&dir).ok()?;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(dir.join("dice.log"))
        .ok()?;
    // A marker so the user can tell a fresh run apart from a stale file.
    let _ = writeln!(file, "--- dice desktop log (truncated at launch) ---");
    Some(FileLog(Arc::new(std::sync::Mutex::new(file))))
}

/// `%APPDATA%\com.dice.app` — the dir Tauri's `app_data_dir()` resolves to on
/// Windows for our `com.dice.app` identifier (see tauri.conf.json). Mirrored
/// here so the log lands beside `cache.db`, before the Tauri app handle exists.
fn app_data_dir() -> Option<std::path::PathBuf> {
    let appdata = std::env::var_os("APPDATA")?;
    Some(std::path::Path::new(&appdata).join("com.dice.app"))
}

/// WebView2 browser arguments, applied at webview creation. Tuned for RAM:
/// M2 measurements (see WORKLOG) took the release client's idle login screen
/// from ~164 MB → ~117 MB private commit, almost entirely via `--in-process-gpu`.
///
/// Because we set `additional_browser_args` ourselves, wry stops applying its
/// own default (`--disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection`),
/// so we re-include those three and fold every feature-disable into the SINGLE
/// `--disable-features=` list Chromium honours (a second occurrence would
/// replace, not merge).
///
/// `--in-process-gpu` is the headline win: it folds the separate GPU process
/// (~47 MB private) into the browser process WHILE KEEPING hardware
/// acceleration — strictly better for us than `--disable-gpu` (software
/// rendering would punish the Aero glass blur for a ~3 MB smaller footprint).
/// The accepted tradeoff: a GPU driver crash takes the whole webview down
/// instead of being isolated. Override the whole string via `DICE_WEBVIEW_ARGS`.
const DEFAULT_WEBVIEW_ARGS: &str = concat!(
    "--disable-features=",
    "msWebOOUI,msPdfOOUI,msSmartScreenProtection,",
    "Translate,MediaRouter,OptimizationHints,OptimizationGuideModelDownloading,",
    "AutofillServerCommunication,InterestFeedContentSuggestions",
    " --disable-background-networking",
    " --disable-component-update",
    " --disable-sync",
    " --in-process-gpu",
);

/// WebView2 args: `DICE_WEBVIEW_ARGS` overrides wholesale (for measurement),
/// else [`DEFAULT_WEBVIEW_ARGS`]. Empty/whitespace env value falls back too.
fn webview_args() -> String {
    match std::env::var("DICE_WEBVIEW_ARGS") {
        Ok(v) if !v.trim().is_empty() => v,
        _ => DEFAULT_WEBVIEW_ARGS.to_owned(),
    }
}

/// The active dev profile: `--profile <name>` / `--profile=<name>` on the
/// command line, else `DICE_CLIENT_PROFILE`. `None` = the normal app.
fn active_profile() -> Option<String> {
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if let Some(name) = arg.strip_prefix("--profile=") {
            return sanitize_profile(name);
        }
        if arg == "--profile" {
            return args.next().and_then(|n| sanitize_profile(&n));
        }
    }
    std::env::var("DICE_CLIENT_PROFILE")
        .ok()
        .and_then(|n| sanitize_profile(&n))
}

/// Lowercase and keep `[a-z0-9_-]`, capped at 32 chars — safe as both a
/// directory name and a keyring account suffix. Empty input yields `None`.
fn sanitize_profile(raw: &str) -> Option<String> {
    let cleaned: String = raw
        .trim()
        .to_ascii_lowercase()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
        .take(32)
        .collect();
    (!cleaned.is_empty()).then_some(cleaned)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::sanitize_profile;

    #[test]
    fn sanitize_profile_is_filesystem_and_keyring_safe() {
        assert_eq!(sanitize_profile("Bob"), Some("bob".to_owned()));
        assert_eq!(sanitize_profile("  guest-2 "), Some("guest-2".to_owned()));
        assert_eq!(sanitize_profile("../../etc"), Some("etc".to_owned()));
        assert_eq!(sanitize_profile("a/b\\c:d"), Some("abcd".to_owned()));
        assert_eq!(sanitize_profile(""), None);
        assert_eq!(sanitize_profile("///"), None);
        assert_eq!(sanitize_profile(&"x".repeat(100)).unwrap().len(), 32);
    }
}
