//! Jellybeam shell, built on GPUI: process entry, window, video layer and
//! the bridges between them. See crates/app/ARCHITECTURE.md.

mod about;
mod assets;
mod backdrop;
mod cards;
mod channel_browse;
mod detail;
mod discover;
mod e2e;
mod edr;
mod edr_gl;
mod focus_grid;
mod gl_video;
mod grid;
mod home;
mod image_store;
mod image_warm;
mod keychain;
mod library_list;
mod menu;
mod nav;
mod now_playing;
mod option_speed_hold;
mod panic_log;
mod paths;
mod pending_report;
mod perf;
mod playback;
mod player_prefs;
mod player_ui;
mod power;
mod redact;
mod root;
mod root_focus;
mod root_playback;
mod scroll_axis;
mod search;
mod session;
mod settings;
mod shortcuts_overlay;
#[cfg(test)]
mod test_support;
mod text_input;
mod theme;
mod trickplay;
mod ui;

use std::sync::atomic::AtomicI64;
use std::sync::Arc;

use gpui::{
    prelude::*, px, size, App, Application, Bounds, KeyBinding, WindowBackgroundAppearance,
    WindowBounds, WindowOptions,
};

/// Sets up tracing output to stderr (as before) plus a mirror to
/// `~/Library/Logs/Jellybeam/jellybeam.log` -- a Finder-launched session has no
/// attached terminal, so stderr-only output left those runs completely
/// log-blind (the non-panic-path analog of why `panic_log.rs` exists: the
/// crash that hit when Return was pressed on the Connect screen had no
/// terminal attached either). Returns the file writer's `WorkerGuard`,
/// which the caller must keep alive for the process's whole life --
/// dropping it early silently stops flushing buffered log lines to disk,
/// per `tracing_appender::non_blocking`'s own doc comment.
fn init_tracing() -> tracing_appender::non_blocking::WorkerGuard {
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;
    use tracing_subscriber::EnvFilter;

    let filter = || EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    let log_dir = panic_log::log_dir();
    let _ = std::fs::create_dir_all(&log_dir);
    // `rolling::never` -- a fixed "jellybeam.log" filename, appended to
    // forever rather than date-suffixed. Simplicity over rotation at this
    // gate; nothing here currently prunes old content.
    let file_appender = tracing_appender::rolling::never(&log_dir, "jellybeam.log");
    let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);

    let stderr_layer = tracing_subscriber::fmt::layer().with_writer(std::io::stderr);
    let file_layer = tracing_subscriber::fmt::layer()
        .with_writer(non_blocking)
        .with_ansi(false);

    tracing_subscriber::registry()
        .with(filter())
        .with(stderr_layer)
        .with(file_layer)
        .init();

    guard
}

fn build_identity() -> jellyfin_api::ClientIdentity {
    let device = std::env::var("HOSTNAME")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "Jellybeam-Mac".to_string());
    jellyfin_api::ClientIdentity {
        client: "Jellybeam".to_string(),
        device,
        device_id: keychain::device_id(),
        version: env!("CARGO_PKG_VERSION").to_string(),
    }
}

fn main() {
    // As close to "exec" as this fn can capture -- threaded through to
    // `Root::new`/`Root::warm_start_at` so a launch-time auto-resume can
    // log "exec -> Main painted" (see that field's doc comment).
    // Deliberately the very first statement, before even the panic hook,
    // so nothing above it can skew the measurement.
    let launched_at = std::time::Instant::now();
    // Must be first: installs the panic hook before anything else (tracing
    // init, the tokio runtime, GPUI) gets a chance to panic without it --
    // see `panic_log::install_panic_hook`'s doc comment.
    panic_log::install_panic_hook();
    // Held for the rest of `main` (which doesn't return until the app
    // quits) -- see `init_tracing`'s doc comment on why dropping this
    // early would silently stop flushing the file log.
    let _tracing_guard = init_tracing();
    tracing::info!("jellybeam starting");

    // GPUI owns the main thread; every Jellyfin/mirror/mpv-client-API async
    // call runs on this multi-thread tokio runtime instead. Kept alive for the app's whole life inside `Root`.
    let runtime = Arc::new(
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("failed to build the tokio runtime"),
    );

    let identity = build_identity();
    let sessions = keychain::load_sessions();

    // Bakes `assets/icons/*.svg` (Lucide OSD icons) into the binary --
    // GPUI's default `AssetSource` is `()`, which resolves every `svg()`
    // path to nothing, so this must be wired before any window opens (see
    // `assets.rs`'s doc comment).
    Application::new()
        .with_assets(assets::Assets)
        .run(move |cx: &mut App| {
            // Brand §3's three faces, registered before the first window
            // opens (see `register_brand_fonts`) -- a window that renders
            // before this runs would lay its first frame out in the system
            // UI font and reflow when the real family arrives.
            register_brand_fonts(cx);

            // Connect screen Tab/Shift+Tab cycling -- see
            // `root::ConnectFocusNext`/`ConnectFocusPrev`'s doc comment for why
            // a global binding here is safe (only the Connect screen's own
            // render tree ever registers a listener for these).
            cx.bind_keys([
                KeyBinding::new("tab", root::ConnectFocusNext, None),
                KeyBinding::new("shift-tab", root::ConnectFocusPrev, None),
            ]);

            cx.on_window_closed(|cx| {
                if cx.windows().is_empty() {
                    cx.quit();
                }
            })
            .detach();

            let bounds = Bounds {
                origin: gpui::point(px(0.0), px(0.0)),
                size: size(px(1280.0), px(800.0)),
            };

            let window_handle = cx
                .open_window(
                    WindowOptions {
                        window_bounds: Some(WindowBounds::Windowed(bounds)),
                        window_background: WindowBackgroundAppearance::Transparent,
                        // No stock light-gray macOS title bar over a dark
                        // app. Transparent titlebar + full-size content view
                        // lets hero artwork run beneath the traffic lights;
                        // every screen paints its own dark surface up to the
                        // top edge, so the lights sit on app content.
                        titlebar: Some(gpui::TitlebarOptions {
                            title: Some("Jellybeam".into()),
                            appears_transparent: true,
                            ..Default::default()
                        }),
                        ..Default::default()
                    },
                    {
                        let runtime = runtime.clone();
                        let identity = identity.clone();
                        move |window, cx| {
                            let video = std::rc::Rc::new(
                                gl_video::VideoLayer::embed(window, cx)
                                    .expect("failed to embed the mpv video layer"),
                            );
                            let last_position_ticks = Arc::new(AtomicI64::new(0));

                            let root = cx.new(|cx| {
                                root::Root::new(
                                    cx,
                                    runtime,
                                    identity,
                                    video.clone(),
                                    last_position_ticks.clone(),
                                    sessions,
                                    launched_at,
                                )
                            });

                            spawn_player_events_task(
                                video,
                                last_position_ticks,
                                root.downgrade(),
                                cx,
                            );

                            root
                        }
                    },
                )
                .expect("failed to open the main window");

            let root_view = window_handle
                .update(cx, |_, _, cx| cx.entity())
                .expect("window should exist right after opening");

            // Now Playing / media-key integration (`now_playing.rs`): registered
            // once, right here, so it runs after the window (and GPUI's
            // `NSApplication` run loop) already exist -- see
            // `NowPlaying::register`'s doc comment on why that ordering matters.
            // Seeded with whatever skip lengths are already configured at
            // launch (`Root::set_skip_length` re-applies a later change
            // without a fresh `register`).
            let (np_tx, np_rx) = tokio::sync::mpsc::unbounded_channel();
            let skip_length = root_view.read(cx).app_settings.skip_length;
            let now_playing = now_playing::NowPlaying::register(
                np_tx,
                f64::from(skip_length.back_secs),
                f64::from(skip_length.forward_secs),
            );
            root_view.update(cx, |root, _cx| root.set_now_playing(now_playing));
            spawn_now_playing_task(np_rx, root_view.downgrade(), cx);

            // Option-key speed-hold monitor (`option_speed_hold.rs`):
            // registered here for the same reason Now Playing is -- after
            // the window (and GPUI's `NSApplication` run loop) already
            // exist, see `NowPlaying::register`'s doc comment, which
            // `OptionSpeedMonitor::register` mirrors exactly.
            let (speed_tx, speed_rx) = tokio::sync::mpsc::unbounded_channel();
            let option_speed_monitor = option_speed_hold::OptionSpeedMonitor::register(speed_tx);
            root_view.update(cx, |root, _cx| {
                root.set_option_speed_monitor(option_speed_monitor)
            });
            spawn_option_speed_task(speed_rx, root_view.downgrade(), cx);

            // Option-key speed-hold robustness: a local `NSEvent` monitor
            // (`option_speed_hold.rs`) only ever sees events already
            // destined for one of this app's own windows, so it stops
            // firing entirely once this window loses OS key-window status
            // (Cmd+Tab to another app, clicking another app's window) --
            // meaning a hold that outlives that transition has no key-up-
            // equivalent event left to notice the eventual release on.
            // `Window::is_window_active` flips false on exactly that
            // transition (driven by `NSWindow`'s become/resignKey via
            // gpui's mac backend, `on_active_status_change` in
            // `gpui-0.2.2/src/window.rs`), so resetting here closes that
            // gap -- see `Root::reset_speed_boost`'s doc comment for the
            // other two call sites covering stop/reload, and
            // `option_speed_hold.rs`'s module doc for why a purely local
            // monitor is the right tradeoff regardless.
            window_handle
                .update(cx, |_, window, cx| {
                    cx.observe_window_activation(window, |root, window, cx| {
                        if !window.is_window_active() {
                            root.reset_speed_boost(cx);
                        }
                    })
                    .detach();
                })
                .expect("window should exist right after opening");

            // Global Space (pause/resume) / Escape (stop) handling -- fires
            // regardless of which element currently has keyboard focus (see
            // `root.rs`'s `handle_global_keystroke` doc comment).
            //
            // `catch_and_log` around the actual dispatch: this closure runs
            // from inside AppKit's `extern "C" handle_key_event` (gpui's
            // `platform/mac/window.rs`) by the time gpui calls it, so a panic
            // that escapes this closure can't unwind across that boundary and
            // aborts the whole process instead of just failing this one
            // keystroke -- see `panic_log::catch_and_log`'s doc comment (this
            // is the same abort-not-panic shape as the crash that used to hit
            // when Return was pressed on the Connect screen).
            let keystroke_root = root_view.clone();
            // `observe_keystrokes` is app-global: it fires for keystrokes in
            // EVERY window, including the About window (`about.rs`). Without
            // this gate, Escape/arrows/`/`/⌘1-9 typed into About would also
            // drive the main window's navigation (Esc to dismiss About
            // silently stepped the library back). Only the main window's
            // keystrokes reach the main-window handler.
            let main_window_id = window_handle.window_id();
            cx.observe_keystrokes(move |event, window, cx| {
                if window.window_handle().window_id() != main_window_id {
                    return;
                }
                keystroke_root.update(cx, |root, cx| {
                    panic_log::catch_and_log(
                        "observe_keystrokes handler",
                        std::panic::AssertUnwindSafe(|| {
                            root.handle_global_keystroke(event, window, cx)
                        }),
                    );
                });
            })
            .detach();

            // The native menu bar (`menu.rs`): installed here, after the
            // main window exists, because `cx.set_menus` resolves each
            // item's shortcut glyph out of the keymap `menu::install`
            // populates one statement earlier, and because `About Jellybeam`
            // opens a window off the live `Root`. Registered before
            // `cx.activate` so the menu bar is already correct the first
            // time it is drawn.
            menu::install(root_view.clone(), cx);

            cx.activate(true);

            // M2: without this, quitting mid-playback (Cmd+Q, menu Quit, or the
            // OS terminating the app) skipped both the mpv stop and the final
            // Stopped playback report -- the server's `Sessions/Playing` state
            // for that session was left dangling until it timed out server-side.
            // `on_app_quit` gives registered futures a short grace window
            // (`gpui::SHUTDOWN_TIMEOUT`) to run before the process actually
            // exits; see `Root::prepare_for_quit`'s doc comment for how that
            // window is used.
            let quit_root = root_view.clone();
            cx.on_app_quit(move |cx| {
                let stop_future = quit_root.update(cx, |root, _cx| root.prepare_for_quit());
                async move {
                    if let Some(fut) = stop_future {
                        fut.await;
                    }
                }
            })
            .detach();

            if std::env::var("JELLYBEAM_E2E").as_deref() == Ok("1") {
                e2e::spawn(root_view, window_handle, cx);
            } else if std::env::var("JELLYBEAM_PERF").as_deref() == Ok("1") {
                perf::spawn(root_view, cx);
            } else if std::env::var("JELLYBEAM_LOADTEST").as_deref() == Ok("1") {
                // See `perf::spawn_loadtest`'s doc comment.
                perf::spawn_loadtest(root_view, cx);
            } else if std::env::var("JELLYBEAM_E2E_QUIT_TEST").as_deref() == Ok("1") {
                // See `e2e::spawn_quit_test`'s doc comment.
                e2e::spawn_quit_test(root_view, cx);
            }
        });
}

/// Registers brand §3's three faces (Archivo / Martian Mono / Bagel Fat One,
/// eight TTFs) with GPUI's text system from the embedded asset bundle.
///
/// Must run before `open_window`: `TextSystem::add_fonts` feeds font-kit's
/// in-memory source, which `load_family` consults *before* the system source
/// (`gpui-0.2.2/src/platform/mac/text_system.rs`), and a family that isn't
/// registered yet silently resolves to `.AppleSystemUIFont` for that frame
/// rather than erroring -- so a late registration would show as a one-frame
/// flash of the wrong typeface plus a full reflow, not as a failure.
///
/// A registration failure is logged, not fatal, for the same reason
/// `assets::brand_font_data` skips a missing file: the app in the system
/// face is degraded, the app with no window is broken.
fn register_brand_fonts(cx: &App) {
    match cx.text_system().add_fonts(assets::brand_font_data()) {
        Ok(()) => tracing::info!(
            count = assets::BRAND_FONTS.len(),
            "registered the brand fonts"
        ),
        Err(err) => tracing::error!(
            error = %err,
            "failed to register the brand fonts; falling back to the system UI face"
        ),
    }
}

/// Subscribes to `player.events()` exactly once (the frozen API only
/// returns a live receiver on the first call -- see its doc comment) for
/// the app's whole life: logs every event, keeps `last_position_ticks`
/// current for `root::Root::handle_global_keystroke`'s Esc path and the
/// JELLYBEAM_E2E "Position events advancing" assertion, and forwards
/// position/pause updates into the active `ReportingSession` via
/// `Root::on_position`/`on_pause_changed`.
fn spawn_player_events_task(
    video: std::rc::Rc<gl_video::VideoLayer>,
    last_position_ticks: Arc<AtomicI64>,
    root: gpui::WeakEntity<root::Root>,
    cx: &mut App,
) {
    let mut events_rx = video.player().events();
    cx.spawn(async move |cx| {
        while let Some(event) = events_rx.recv().await {
            match event {
                player::PlayerEvent::Position { secs } => {
                    let ticks = (secs * 10_000_000.0) as i64;
                    last_position_ticks.store(ticks, std::sync::atomic::Ordering::Relaxed);
                    // Debug, not info: ~4 log lines/sec for the whole of
                    // every playback session would be the noisiest thing in
                    // a production jellybeam.log, and every consumer that
                    // matters logs its own line at info.
                    tracing::debug!(secs, ticks, "position");
                    if root
                        .update(cx, |root, cx| {
                            panic_log::catch_and_log(
                                "player-event Position handler",
                                std::panic::AssertUnwindSafe(|| root.on_position(ticks, cx)),
                            );
                        })
                        .is_err()
                    {
                        break; // Root released -- app shutting down.
                    }
                }
                player::PlayerEvent::Loaded {
                    duration_secs,
                    tracks,
                } => {
                    tracing::info!(duration_secs, track_count = tracks.len(), "player loaded");
                    if root
                        .update(cx, |root, cx| {
                            panic_log::catch_and_log(
                                "player-event Loaded handler",
                                std::panic::AssertUnwindSafe(|| {
                                    root.on_loaded(duration_secs, tracks, cx)
                                }),
                            );
                        })
                        .is_err()
                    {
                        break;
                    }
                }
                player::PlayerEvent::PauseChanged { paused } => {
                    tracing::info!(paused, "pause changed");
                    if root
                        .update(cx, |root, cx| {
                            panic_log::catch_and_log(
                                "player-event PauseChanged handler",
                                std::panic::AssertUnwindSafe(|| root.on_pause_changed(paused, cx)),
                            );
                        })
                        .is_err()
                    {
                        break;
                    }
                }
                player::PlayerEvent::EndOfFile => {
                    tracing::info!("end of file");
                    // Mark the finished item played in the mirror right
                    // away -- see `Root::on_end_of_file`'s doc comment for
                    // why this can't just wait on a server push.
                    if root
                        .update(cx, |root, cx| {
                            panic_log::catch_and_log(
                                "player-event EndOfFile handler",
                                std::panic::AssertUnwindSafe(|| root.on_end_of_file(cx)),
                            );
                        })
                        .is_err()
                    {
                        break;
                    }
                }
                player::PlayerEvent::Buffering { active, percent } => {
                    tracing::debug!(active, percent, "buffering");
                    if root
                        .update(cx, |root, cx| {
                            panic_log::catch_and_log(
                                "player-event Buffering handler",
                                std::panic::AssertUnwindSafe(|| {
                                    root.on_buffering(active, percent, cx)
                                }),
                            );
                        })
                        .is_err()
                    {
                        break;
                    }
                }
                player::PlayerEvent::Error(msg) => {
                    tracing::warn!(error = %msg, "player error");
                }
                player::PlayerEvent::TracksChanged(tracks) => {
                    if root
                        .update(cx, |root, cx| {
                            panic_log::catch_and_log(
                                "player-event TracksChanged handler",
                                std::panic::AssertUnwindSafe(|| root.on_tracks_changed(tracks, cx)),
                            );
                        })
                        .is_err()
                    {
                        break;
                    }
                }
                player::PlayerEvent::ChapterChanged { .. } => {}
            }
        }
    })
    .detach();
}

/// Bridges `now_playing::RemoteCommand`s (Control Center / physical media
/// keys, delivered on whatever thread AppKit invokes the handler block on --
/// see `NowPlaying::register`'s doc comment) into `Root`, mirroring
/// `spawn_player_events_task`'s "raw channel -> `cx.spawn` -> `Root::on_*`"
/// shape.
fn spawn_now_playing_task(
    mut rx: tokio::sync::mpsc::UnboundedReceiver<now_playing::RemoteCommand>,
    root: gpui::WeakEntity<root::Root>,
    cx: &mut App,
) {
    cx.spawn(async move |cx| {
        while let Some(cmd) = rx.recv().await {
            if root
                .update(cx, |root, cx| root.handle_remote_command(cmd, cx))
                .is_err()
            {
                break;
            }
        }
    })
    .detach();
}

/// Bridges `option_speed_hold::OptionFlagsSample`s (raw `NSEvent
/// .modifierFlags` from the `flagsChanged` local monitor -- see
/// `option_speed_hold.rs`'s module doc for why a channel hop is used here
/// even though, unlike a Now Playing remote command, this handler is
/// always invoked on the main thread already) into `Root`, mirroring
/// `spawn_now_playing_task`'s "raw channel -> `cx.spawn` -> `Root::on_*`"
/// shape.
fn spawn_option_speed_task(
    mut rx: tokio::sync::mpsc::UnboundedReceiver<option_speed_hold::OptionFlagsSample>,
    root: gpui::WeakEntity<root::Root>,
    cx: &mut App,
) {
    cx.spawn(async move |cx| {
        while let Some(sample) = rx.recv().await {
            if root
                .update(cx, |root, cx| {
                    root.handle_option_flags_sample(sample.raw_flags, cx)
                })
                .is_err()
            {
                break;
            }
        }
    })
    .detach();
}
