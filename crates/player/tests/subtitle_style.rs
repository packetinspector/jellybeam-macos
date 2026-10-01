//! Integration test for `Player::set_subtitle_style` (docs/OVERVIEW.md §6).
//! `back_color_hex`'s pure hex-mapping logic is
//! unit-tested in `src/lib.rs`; this covers the half that actually needs a
//! live mpv handle: the four `mpv_set_property*` calls must all succeed
//! (no `PlayerError`) against a real loaded file, both before and after
//! playback starts, and across the option's edge values.

#![cfg(target_os = "macos")]

mod common;

use std::time::Duration;

use player::{LoadRequest, Player, PlayerEvent, SubtitleStyle};

fn simple_load(path: String) -> LoadRequest {
    LoadRequest {
        url: path,
        http_headers: Vec::new(),
        start_secs: None,
        external_subs: Vec::new(),
        start_paused: false,
        readahead_secs: None,
        max_bytes: None,
    }
}

#[test]
fn set_subtitle_style_succeeds_before_and_after_load() {
    let _gl = common::GlContext::new_current().expect("headless CGL context");
    let player = Player::new(common::gl_get_proc_address).expect("Player::new");
    let events = common::collect_events(&player);

    // Before anything is loaded: mpv accepts these properties regardless of
    // playback state (per set_subtitle_style's doc comment) -- this is the
    // "apply on player init" call site's shape (root.rs applies persisted
    // settings before/alongside the first `load`).
    player
        .set_subtitle_style(&SubtitleStyle {
            scale: 1.25,
            pos: Some(90),
            bold: true,
            back_alpha: 0.5,
        })
        .expect("set_subtitle_style before load should succeed");

    player
        .load(simple_load(common::media(
            "Movies/27-h264-ass-subs-aac.mkv",
        )))
        .expect("load() should be accepted");
    common::wait_for(&events, Duration::from_secs(20), |ev| {
        matches!(ev, PlayerEvent::Loaded { .. }).then_some(())
    })
    .expect("expected a Loaded event within 20s");

    // After load, with edge values on both ends (min/max scale and alpha,
    // pos clamped range, bold off) -- the "apply on change" call site's
    // shape, verifying no edge value trips an mpv property-validation
    // error.
    for style in [
        SubtitleStyle {
            scale: 0.5,
            pos: Some(0),
            bold: false,
            back_alpha: 0.0,
        },
        SubtitleStyle {
            scale: 2.0,
            pos: Some(150),
            bold: true,
            back_alpha: 1.0,
        },
        // pos: None must leave mpv's sub-pos untouched, not error.
        SubtitleStyle {
            pos: None,
            ..Default::default()
        },
    ] {
        player
            .set_subtitle_style(&style)
            .unwrap_or_else(|e| panic!("set_subtitle_style({style:?}) after load failed: {e}"));
    }
}
