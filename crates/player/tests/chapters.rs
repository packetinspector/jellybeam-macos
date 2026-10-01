//! Chapter-event test: seeking past a chapter boundary should surface
//! `PlayerEvent::ChapterChanged`.

#![cfg(target_os = "macos")]

mod common;

use std::time::Duration;

use player::{LoadRequest, Player, PlayerEvent};

#[test]
fn seeking_past_a_chapter_boundary_emits_chapter_changed() {
    let _gl = common::GlContext::new_current().expect("headless CGL context");
    let player = Player::new(common::gl_get_proc_address).expect("Player::new");
    let events = common::collect_events(&player);

    let req = LoadRequest {
        url: common::media("Movies/29-h264-chapters-aac.mkv"),
        http_headers: Vec::new(),
        start_secs: None,
        external_subs: Vec::new(),
        start_paused: false,
        readahead_secs: None,
        max_bytes: None,
    };
    player.load(req).expect("load() should be accepted");

    let duration = common::wait_for(&events, Duration::from_secs(20), |ev| {
        if let PlayerEvent::Loaded { duration_secs, .. } = ev {
            Some(*duration_secs)
        } else {
            None
        }
    })
    .expect("expected a Loaded event within 20s");

    // Sweep across the file so we cross every chapter boundary the synthetic
    // corpus file has, without needing to know the exact chapter timestamps
    // up front.
    let mut saw_chapter_beyond_zero = false;
    for fraction in [0.3, 0.6, 0.9] {
        let target = (duration * fraction).max(0.05);
        player
            .seek_absolute(target)
            .expect("seek_absolute should be accepted");

        if let Some(index) = common::wait_for(&events, Duration::from_secs(10), |ev| {
            if let PlayerEvent::ChapterChanged { index, .. } = ev {
                Some(*index)
            } else {
                None
            }
        }) {
            if index > 0 {
                saw_chapter_beyond_zero = true;
            }
        }
    }

    assert!(
        saw_chapter_beyond_zero,
        "expected at least one ChapterChanged event with index > 0 while sweeping across the file"
    );
}
