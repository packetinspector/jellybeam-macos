//! Verifies that `hwdec=videotoolbox` (set in `INIT_OPTIONS`) actually takes
//! effect during playback, rather than mpv silently falling back to
//! software decoding because the test harness's GL context wasn't Core
//! Profile (see the pixel-format doc comments in `tests/common/mod.rs` and
//! the contract note on `player::Player::new`).

#![cfg(target_os = "macos")]

mod common;

use std::time::{Duration, Instant};

use player::{LoadRequest, Player, PlayerEvent};

#[test]
fn hwdec_current_reports_videotoolbox_during_hevc_playback() {
    let _gl = common::GlContext::new_current().expect("headless CGL context");
    let player = Player::new(common::gl_get_proc_address).expect("Player::new");
    let events = common::collect_events(&player);

    let req = LoadRequest {
        url: common::media("Movies/06-hevc8-aac.ts"),
        http_headers: Vec::new(),
        start_secs: None,
        external_subs: Vec::new(),
        start_paused: false,
        readahead_secs: None,
        max_bytes: None,
    };
    player.load(req).expect("load() should be accepted");

    common::wait_for(&events, Duration::from_secs(20), |ev| {
        matches!(ev, PlayerEvent::Loaded { .. }).then_some(())
    })
    .expect("expected a Loaded event within 20s");

    // `hwdec-current` only reflects the actual decoder choice once mpv has
    // actually started decoding frames, which can lag slightly behind
    // `Loaded` (which only needs track-list + duration); poll briefly
    // rather than requiring it on the very first read.
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut hwdec = None;
    while Instant::now() < deadline {
        hwdec = player.hwdec_current();
        if hwdec.is_some() {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    assert_eq!(
        hwdec.as_deref(),
        Some("videotoolbox"),
        "expected hwdec-current == \"videotoolbox\" during hevc playback \
         (a non-Core-Profile GL context makes VideoToolbox interop silently \
         fall back to software — see the contract note on Player::new); \
         got {hwdec:?}"
    );
}
