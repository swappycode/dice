use std::time::{SystemTime, UNIX_EPOCH};

/// 2026-01-01T00:00:00Z — the Dice snowflake epoch (ADR-0004).
pub const DICE_EPOCH_MS: u64 = 1_767_225_600_000;

/// Milliseconds since the Unix epoch.
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before 1970")
        .as_millis() as u64
}

/// Milliseconds since the Dice epoch (saturating for pre-epoch clocks).
pub fn now_dice_ms() -> u64 {
    now_ms().saturating_sub(DICE_EPOCH_MS)
}
