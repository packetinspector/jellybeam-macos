//! Basic load / seek / pause / EOF integration tests against the synthetic
//! corpus (dev/media/, see docs/DATA.md / dev/corpus/README.md), driven headlessly
//! through the CGL harness in `tests/common`.

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

#[test]
fn load_reports_loaded_with_duration_and_track_kinds() {
    let _gl = common::GlContext::new_current().expect("headless CGL context");
    let player = Player::new(common::gl_get_proc_address).expect("Player::new");
    let events = common::collect_events(&player);

    player
        .load(simple_load(common::media("Movies/01-h264-aac.mkv")))
        .expect("load() should be accepted");

    let (duration_secs, tracks) = common::wait_for(&events, Duration::from_secs(20), |ev| {
        if let PlayerEvent::Loaded {
            duration_secs,
            tracks,
        } = ev
        {
            Some((*duration_secs, tracks.clone()))
        } else {
            None
        }
    })
    .expect("expected a Loaded event within 20s");

    assert!(
        duration_secs > 0.0,
        "duration_secs should be positive, got {duration_secs}"
    );
    assert!(
        tracks.iter().any(|t| t.kind == TrackKind::Video),
        "expected at least one video track, got {tracks:?}"
    );
    assert!(
        tracks.iter().any(|t| t.kind == TrackKind::Audio),
        "expected at least one audio track, got {tracks:?}"
    );
}

#[test]
fn seek_absolute_converges_on_target() {
    let _gl = common::GlContext::new_current().expect("headless CGL context");
    let player = Player::new(common::gl_get_proc_address).expect("Player::new");
    let events = common::collect_events(&player);

    player
        .load(simple_load(common::media("Movies/01-h264-aac.mkv")))
        .expect("load() should be accepted");

    let duration = common::wait_for(&events, Duration::from_secs(20), |ev| {
        if let PlayerEvent::Loaded { duration_secs, .. } = ev {
            Some(*duration_secs)
        } else {
            None
        }
    })
    .expect("expected a Loaded event within 20s");

    let target = (duration * 0.5).max(0.1);
    player
        .seek_absolute(target)
        .expect("seek_absolute should be accepted");

    // hr-seek should land within a small tolerance of the requested target;
    // `time-pos` is throttled (~4Hz) so give it a few beats to arrive.
    let converged = common::wait_for(&events, Duration::from_secs(15), |ev| {
        if let PlayerEvent::Position { secs } = ev {
            if (secs - target).abs() < 0.75 {
                return Some(*secs);
            }
        }
        None
    });

    assert!(
        converged.is_some(),
        "expected a Position event converging near {target:.2}s within tolerance"
    );
}

#[test]
fn set_paused_and_end_of_file() {
    let _gl = common::GlContext::new_current().expect("headless CGL context");
    let player = Player::new(common::gl_get_proc_address).expect("Player::new");
    let events = common::collect_events(&player);

    player
        .load(simple_load(common::media("Movies/01-h264-aac.mkv")))
        .expect("load() should be accepted");

    let duration = common::wait_for(&events, Duration::from_secs(20), |ev| {
        if let PlayerEvent::Loaded { duration_secs, .. } = ev {
            Some(*duration_secs)
        } else {
            None
        }
    })
    .expect("expected a Loaded event within 20s");

    player
        .set_paused(true)
        .expect("set_paused(true) should be accepted");
    let paused = common::wait_for(&events, Duration::from_secs(10), |ev| {
        if let PlayerEvent::PauseChanged { paused } = ev {
            Some(*paused)
        } else {
            None
        }
    });
    assert_eq!(paused, Some(true), "expected PauseChanged{{paused: true}}");

    player
        .set_paused(false)
        .expect("set_paused(false) should be accepted");
    let unpaused = common::wait_for(&events, Duration::from_secs(10), |ev| {
        if let PlayerEvent::PauseChanged { paused } = ev {
            if !*paused {
                return Some(*paused);
            }
        }
        None
    });
    assert_eq!(
        unpaused,
        Some(false),
        "expected PauseChanged{{paused: false}}"
    );

    // Seek to just before the end and let it play out to EOF (keep-open=yes
    // means mpv pauses-at-EOF rather than closing, but the eof-reached
    // property still flips, which is what our EndOfFile event is driven by).
    let near_end = (duration - 0.3).max(0.0);
    player
        .seek_absolute(near_end)
        .expect("seek_absolute near EOF should be accepted");

    let saw_eof = common::wait_for(&events, Duration::from_secs(20), |ev| {
        matches!(ev, PlayerEvent::EndOfFile).then_some(())
    });
    assert!(
        saw_eof.is_some(),
        "expected an EndOfFile event within 20s of seeking near the end"
    );
}

/// `keep-open=yes` leaves mpv paused at EOF, and `pause` is global state
/// that survives `loadfile`; the per-file `pause=no` in
/// `build_loadfile_options` must make a file loaded after an EOF-paused one
/// actually play. Load A, run it to EOF, load B, and require B to advance.
#[test]
fn load_after_eof_starts_playing_not_paused() {
    let _gl = common::GlContext::new_current().expect("headless CGL context");
    let player = Player::new(common::gl_get_proc_address).expect("Player::new");
    let events = common::collect_events(&player);

    // --- A: load, run to EOF (mpv pauses internally via keep-open) -------
    player
        .load(simple_load(common::media("Movies/01-h264-aac.mkv")))
        .expect("load(A) should be accepted");

    let duration_a = common::wait_for(&events, Duration::from_secs(20), |ev| {
        if let PlayerEvent::Loaded { duration_secs, .. } = ev {
            Some(*duration_secs)
        } else {
            None
        }
    })
    .expect("expected a Loaded event for A within 20s");

    let near_end = (duration_a - 0.3).max(0.0);
    player
        .seek_absolute(near_end)
        .expect("seek_absolute near EOF should be accepted");

    // `EndOfFile` and `PauseChanged{paused: true}` (keep-open's documented
    // at-EOF behavior) can arrive in either order relative to each other --
    // watch for both in one pass rather than two sequential `wait_for`
    // calls, which would silently drop whichever one arrives first while
    // waiting on the other.
    let mut saw_eof = false;
    let mut saw_paused_true = false;
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while std::time::Instant::now() < deadline && !(saw_eof && saw_paused_true) {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        match events.recv_timeout(remaining) {
            Ok(ev) => {
                common::assert_not_player_error(&ev);
                match ev {
                    PlayerEvent::EndOfFile => saw_eof = true,
                    PlayerEvent::PauseChanged { paused: true } => saw_paused_true = true,
                    _ => {}
                }
            }
            Err(_) => break,
        }
    }
    assert!(saw_eof, "expected A to reach EndOfFile within 20s");
    assert!(
        saw_paused_true,
        "expected mpv to report paused=true at EOF (keep-open=yes premise of this test)"
    );

    // --- B: load a second file; it must actually start playing -----------
    player
        .load(simple_load(common::media("Movies/02-h264-ac3.mp4")))
        .expect("load(B) should be accepted");

    // Same "watch for both, either order" shape as the A/EOF wait above --
    // B's `Loaded` and its `PauseChanged{paused: false}` (fired once mpv
    // processes B's per-file `pause=no` option, the fix under test) can
    // arrive in either order relative to each other.
    let mut saw_loaded_b = false;
    let mut saw_unpaused_b = false;
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while std::time::Instant::now() < deadline && !(saw_loaded_b && saw_unpaused_b) {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        match events.recv_timeout(remaining) {
            Ok(ev) => {
                common::assert_not_player_error(&ev);
                match ev {
                    PlayerEvent::Loaded { .. } => saw_loaded_b = true,
                    PlayerEvent::PauseChanged { paused: false } => saw_unpaused_b = true,
                    _ => {}
                }
            }
            Err(_) => break,
        }
    }
    assert!(saw_loaded_b, "expected a Loaded event for B within 20s");
    assert!(
        saw_unpaused_b,
        "B should have unpaused via its per-file pause=no option, not stayed paused from A's EOF"
    );

    // And position should actually advance for B -- the end-to-end
    // "subsequent video fails to play" symptom this test guards against.
    let pos_a = common::wait_for(&events, Duration::from_secs(10), |ev| {
        if let PlayerEvent::Position { secs } = ev {
            Some(*secs)
        } else {
            None
        }
    })
    .expect("expected at least one Position event for B");

    let pos_b = common::wait_for(&events, Duration::from_secs(10), |ev| {
        if let PlayerEvent::Position { secs } = ev {
            if *secs > pos_a + 0.05 {
                return Some(*secs);
            }
        }
        None
    });
    assert!(
        pos_b.is_some(),
        "B's position did not advance beyond {pos_a:.3}s -- it looks stuck paused"
    );
}

/// `seek_absolute_fast` (keyframe/inexact) should land in the same
/// ballpark as `seek_absolute` (hr-seek/exact) for a mid-file target --
/// "fast" trades frame-accuracy for speed, not correctness of which part of
/// the file it lands in. A generous tolerance (keyframe interval-sized)
/// distinguishes this from `seek_absolute_converges_on_target`'s much
/// tighter check.
#[test]
fn seek_absolute_fast_converges_near_target() {
    let _gl = common::GlContext::new_current().expect("headless CGL context");
    let player = Player::new(common::gl_get_proc_address).expect("Player::new");
    let events = common::collect_events(&player);

    player
        .load(simple_load(common::media("Movies/01-h264-aac.mkv")))
        .expect("load() should be accepted");

    let duration = common::wait_for(&events, Duration::from_secs(20), |ev| {
        if let PlayerEvent::Loaded { duration_secs, .. } = ev {
            Some(*duration_secs)
        } else {
            None
        }
    })
    .expect("expected a Loaded event within 20s");

    let target = (duration * 0.5).max(0.1);
    player
        .seek_absolute_fast(target)
        .expect("seek_absolute_fast should be accepted");

    // Keyframe-only seeks can land noticeably before the target (up to
    // roughly one keyframe interval away); a few seconds' tolerance keeps
    // this a "landed in the right neighborhood" check, not a frame-exact
    // one (that's `seek_absolute_converges_on_target`'s job).
    let converged = common::wait_for(&events, Duration::from_secs(15), |ev| {
        if let PlayerEvent::Position { secs } = ev {
            if (secs - target).abs() < 6.0 {
                return Some(*secs);
            }
        }
        None
    });

    assert!(
        converged.is_some(),
        "expected a Position event converging near {target:.2}s (fast/keyframe tolerance) within 15s"
    );
}
