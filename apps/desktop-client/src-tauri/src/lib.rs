//! Dice desktop host (Tauri 2). The brain is [`state::ClientCore`] — every
//! command body is a plain async fn there so the whole surface tests headless
//! (`tests/host_gate.rs`); this crate root only wires the Tauri runtime.
//!
//! Lifecycle rules (docs/design/desktop-client.md §2.4):
//! - ONE tokio runtime, built FIRST and installed via
//!   [`tauri::async_runtime::set`] BEFORE the Builder exists — nothing here
//!   ever calls a bare `tokio::spawn` from the setup hook.
//! - ring is the only rustls crypto provider (workspace policy).
//! - single-instance: a second launch focuses the existing window.
//! - when the keystore already holds a session the gateway connects in the
//!   background at startup; otherwise the first `login`/`register` connects.

pub mod bridge;
pub mod cache;
pub mod commands;
pub mod dto;
pub mod emit;
pub mod keystore;
pub mod session;
pub mod state;

use std::sync::Arc;

use tauri::Manager;

use crate::emit::{Emitter, TauriEmitter};
use crate::keystore::OsKeyring;
use crate::state::{ClientCore, CoreConfig};

/// Entry point shared by `main.rs` and the bundler glue.
pub fn run() {
    dice_network_core::tls::install_ring_provider();
    init_tracing();

    // The ONE runtime. Built before anything Tauri so every spawn in this
    // process — Tauri's own async commands included — lands on it.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("build the tokio runtime");
    let rt_handle = rt.handle().clone();
    tauri::async_runtime::set(rt_handle.clone());

    tauri::Builder::default()
        // Registered first, per the plugin's docs: a second instance pings
        // this callback in the FIRST process; focus the existing window.
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.unminimize();
                let _ = window.set_focus();
            }
        }))
        .setup(move |app| {
            let cache_path = app.path().app_data_dir()?.join("cache.db");
            let cfg = CoreConfig::from_env(cache_path)?;
            let emitter: Arc<dyn Emitter> = Arc::new(TauriEmitter(app.handle().clone()));
            let core = Arc::new(ClientCore::new(
                cfg,
                Arc::new(OsKeyring::new()),
                emitter,
                rt_handle.clone(),
            )?);
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
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::session_status,
            commands::login,
            commands::register,
            commands::logout,
            commands::get_bootstrap,
            commands::send_message,
            commands::fetch_messages,
            commands::start_typing,
            commands::set_presence,
            commands::create_guild,
            commands::join_guild,
            commands::open_dm,
            commands::connection_state,
        ])
        .run(tauri::generate_context!())
        .expect("error while running the Dice desktop host");

    // The event loop ended: tear the runtime down without waiting for the
    // (deliberately long-lived) gateway/bridge tasks.
    rt.shutdown_background();
}

fn init_tracing() {
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,dice_desktop=debug,dice_network_core=debug"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(true)
        .init();
}
