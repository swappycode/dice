//! Fuzz the hand-rolled REST history-query parser (`?before|after=<id>&limit=…`,
//! protocol §10) against arbitrary strings. It is a bespoke parser (kept off
//! serde deliberately) over an attacker-controlled query string, so it must
//! never panic on malformed input — only return an error.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(query) = std::str::from_utf8(data) {
        let _ = api_gateway::parse_history_query(Some(query));
    }
});
