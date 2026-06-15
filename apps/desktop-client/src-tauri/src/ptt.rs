//! Global push-to-talk: bind ONE key as a system-wide shortcut whose press /
//! release gates the mic, so PTT works even when the Dice window isn't focused.
//!
//! Lives here (not in [`ClientCore`](crate::state::ClientCore)) because it needs
//! the Tauri `AppHandle` + the global-shortcut plugin; `ClientCore` only flips
//! the `VoiceControl` PTT bits the audio engine reads.

use std::sync::Arc;

use tauri::{AppHandle, Manager};
use tauri_plugin_global_shortcut::{
    Code, GlobalShortcutExt, Shortcut, ShortcutEvent, ShortcutState,
};

use crate::state::ClientCore;

/// Curated PTT keys (bare, no modifier) the UI offers. An explicit map keeps the
/// rebind a safe dropdown and avoids accelerator-string parsing surprises.
fn shortcut_for(key: &str) -> Option<Shortcut> {
    let code = match key {
        "Backquote" => Code::Backquote,
        "CapsLock" => Code::CapsLock,
        "Insert" => Code::Insert,
        "F8" => Code::F8,
        "F9" => Code::F9,
        "F10" => Code::F10,
        _ => return None,
    };
    Some(Shortcut::new(None, code))
}

/// (Re)bind the PTT shortcut. We only ever hold ONE binding, so clear all first;
/// disabling leaves none registered. Returns a user-facing error string.
pub fn apply(
    app: &AppHandle,
    core: &Arc<ClientCore>,
    enabled: bool,
    key: &str,
) -> Result<(), String> {
    let gs = app.global_shortcut();
    let _ = gs.unregister_all();
    core.set_ptt_held(false);
    if !enabled {
        core.set_ptt_enabled(false);
        return Ok(());
    }
    let shortcut =
        shortcut_for(key).ok_or_else(|| format!("unsupported push-to-talk key: {key}"))?;
    gs.register(shortcut).map_err(|e| e.to_string())?;
    core.set_ptt_enabled(true);
    Ok(())
}

/// Plugin handler: the only registered shortcut is PTT, so mirror its press /
/// release straight into the audio gate.
pub fn on_shortcut(app: &AppHandle, _shortcut: &Shortcut, event: ShortcutEvent) {
    if let Some(core) = app.try_state::<Arc<ClientCore>>() {
        core.set_ptt_held(event.state() == ShortcutState::Pressed);
    }
}
