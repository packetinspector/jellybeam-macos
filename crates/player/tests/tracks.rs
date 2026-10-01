//! Track-enumeration and track-selection tests: ASS subtitles, dual audio,
//! and a live subtitle-track switch.

#![cfg(target_os = "macos")]

mod common;

use std::time::Duration;

use player::{LoadRequest, Player, PlayerEvent, TrackKind};

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

fn load_and_wait(
    player: &Player,
    events: &std::sync::mpsc::Receiver<PlayerEvent>,
    path: String,
) -> Vec<player::Track> {
    player
        .load(simple_load(path))
        .expect("load() should be accepted");
    common::wait_for(events, Duration::from_secs(20), |ev| {
        if let PlayerEvent::Loaded { tracks, .. } = ev {
            Some(tracks.clone())
        } else {
            None
        }
    })
    .expect("expected a Loaded event within 20s")
}

/// The corpus doesn't have a single file that's *both* ASS-subtitled and
/// dual-audio (see dev/media/Movies: 27 has embedded ASS, 30 has dual
/// audio); this test covers both track-enumeration properties across the
/// two files that have them individually.
#[test]
fn ass_subtitle_and_dual_audio_track_enumeration() {
    let _gl = common::GlContext::new_current().expect("headless CGL context");

    {
        let player = Player::new(common::gl_get_proc_address).expect("Player::new");
        let events = common::collect_events(&player);
        let tracks = load_and_wait(
            &player,
            &events,
            common::media("Movies/27-h264-ass-subs-aac.mkv"),
        );

        let sub = tracks
            .iter()
            .find(|t| t.kind == TrackKind::Subtitle)
            .unwrap_or_else(|| panic!("expected a subtitle track in {tracks:?}"));
        assert_eq!(
            sub.codec.as_deref(),
            Some("ass"),
            "expected an ASS-coded subtitle track, got {sub:?}"
        );
    }

    {
        let player = Player::new(common::gl_get_proc_address).expect("Player::new");
        let events = common::collect_events(&player);
        let tracks = load_and_wait(
            &player,
            &events,
            common::media("Movies/30-h264-dual-audio.mkv"),
        );

        let audio_tracks: Vec<_> = tracks
            .iter()
            .filter(|t| t.kind == TrackKind::Audio)
            .collect();
        assert!(
            audio_tracks.len() >= 2,
            "expected at least 2 audio tracks for a dual-audio file, got {audio_tracks:?}"
        );
    }
}

#[test]
fn set_track_switches_subtitle_selection() {
    let _gl = common::GlContext::new_current().expect("headless CGL context");
    let player = Player::new(common::gl_get_proc_address).expect("Player::new");
    let events = common::collect_events(&player);

    let tracks = load_and_wait(
        &player,
        &events,
        common::media("Movies/27-h264-ass-subs-aac.mkv"),
    );
    let sub_id = tracks
        .iter()
        .find(|t| t.kind == TrackKind::Subtitle)
        .unwrap_or_else(|| panic!("expected a subtitle track in {tracks:?}"))
        .mpv_id;

    player
        .set_track(TrackKind::Subtitle, Some(sub_id))
        .expect("set_track(Subtitle, Some(id))");

    let selected = common::wait_for(&events, Duration::from_secs(10), |ev| {
        if let PlayerEvent::TracksChanged(tracks) = ev {
            let is_selected = tracks
                .iter()
                .any(|t| t.kind == TrackKind::Subtitle && t.mpv_id == sub_id && t.selected);
            if is_selected {
                return Some(());
            }
        }
        None
    });
    assert!(
        selected.is_some(),
        "expected TracksChanged to report subtitle track {sub_id} selected"
    );

    // Switching off should also succeed without error.
    player
        .set_track(TrackKind::Subtitle, None)
        .expect("set_track(Subtitle, None)");
}
