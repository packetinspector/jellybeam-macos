//! The render-path test: proves the full decode -> GPU render pipeline by
//! rendering an actual frame into an offscreen FBO and reading the pixels
//! back, with no window.

#![cfg(target_os = "macos")]

mod common;

use std::time::{Duration, Instant};

use player::{LoadRequest, Player, PlayerEvent};

#[test]
fn renders_a_real_frame_into_an_fbo() {
    // Order matters: the GL context must be current *before* `Player::new`,
    // since mpv's render API resolves GL functions during
    // mpv_render_context_create itself (see render.h's threading docs, and
    // the doc comment on `Player::new`).
    let gl = common::GlContext::new_current().expect("headless CGL context");
    let fbo = common::Fbo::new(320, 240).expect("offscreen FBO");

    let player = Player::new(common::gl_get_proc_address).expect("Player::new");
    let events = common::collect_events(&player);

    let req = LoadRequest {
        url: common::media("Movies/01-h264-aac.mkv"),
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

    let deadline = Instant::now() + Duration::from_secs(10);
    let mut render_count = 0u32;
    while Instant::now() < deadline {
        if player.needs_render() {
            player
                .render(fbo.fbo as i32, fbo.width, fbo.height)
                .expect("render() should succeed");
            player.report_swap();
            render_count += 1;
            if render_count >= 5 {
                break;
            }
        } else {
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    assert!(
        render_count > 0,
        "expected needs_render()/render() to fire at least once within 10s"
    );

    let pixels = fbo.read_pixels();
    assert!(
        common::has_pixel_variation(&pixels),
        "rendered frame should not be a single uniform color \
         (decode -> render pipeline likely produced nothing)"
    );

    drop(fbo);
    drop(gl);
}
