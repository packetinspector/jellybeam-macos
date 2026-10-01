//! Fuzz target: raw WebSocket text frame -> ServerEvent decode.
//!
//! `decode_message` is the single funnel every server-pushed WS frame goes
//! through; it must never panic on arbitrary text. Exported for fuzzing
//! only via the `cfg(fuzzing)` seam in ws.rs (cargo-fuzz builds with
//! `--cfg fuzzing`; normal builds don't see the symbol).

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(text) = std::str::from_utf8(data) {
        let _ = jellyfin_api::fuzzing::decode_message(text);
    }
});
