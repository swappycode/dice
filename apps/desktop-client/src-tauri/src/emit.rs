//! The webview-emit seam. The bridge talks to this trait so headless tests
//! can capture events without a Tauri runtime or webview.

use std::sync::Arc;

use crate::dto::{DiceEvent, EVENT_CHANNEL};

pub trait Emitter: Send + Sync {
    fn emit(&self, event: &str, payload: serde_json::Value);
}

/// Serialize and push one `DiceEvent` onto the single frontend channel.
pub fn emit_dice(emitter: &Arc<dyn Emitter>, event: &DiceEvent) {
    match serde_json::to_value(event) {
        Ok(json) => emitter.emit(EVENT_CHANNEL, json),
        Err(error) => tracing::error!(%error, "DiceEvent serialization failed"),
    }
}

/// Production emitter: forwards to every webview via the Tauri app handle.
pub struct TauriEmitter(pub tauri::AppHandle);

impl Emitter for TauriEmitter {
    fn emit(&self, event: &str, payload: serde_json::Value) {
        use tauri::Emitter as _;
        if let Err(error) = self.0.emit(event, payload) {
            tracing::warn!(%error, event, "tauri emit failed");
        }
    }
}
