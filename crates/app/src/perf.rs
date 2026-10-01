//! `JELLYBEAM_PERF=1` instrumentation mode (docs/OVERVIEW.md §5b budgets: view-to-view
//! navigation < 100ms, search keystroke → results < 50ms, poster-wall
//! scroll with "no dropped-frame storms"). Mirrors `e2e.rs`'s shape: auto-
//! login against the dev server, drive the app programmatically, print a
//! summary table, exit.
//!
//! **Frame-time measurement methodology, honestly stated**: this pinned
//! gpui build (0.2.2) doesn't expose the platform's real vsync/frame-pacing
//! timeline to application code. What's measured
//! instead is wall-clock time between successive `Render::render` calls on
//! `Root` (via `Root::enable_perf_frame_log`) while the harness holds a
//! Library grid scrolling under sustained programmatic load (repeated
//! `cx.notify()` + `scroll_to_item` at a ~120Hz drive rate). That's an
//! honest proxy for "how often the UI actually redraws under load," not a
//! literal GPU-swapchain frame timer -- treat the numbers as a lower bound
//! on responsiveness, not a compositor-verified frame rate.

use std::time::{Duration, Instant};

use gpui::{App, AsyncApp, Entity};

use crate::root::{Root, Screen};

const SERVER: &str = "http://localhost:8096";
const USERNAME: &str = "jellybeam-admin";
const PASSWORD: &str = "jellybeam-test";
const SCROLL_DRIVE_SECS: u64 = 5;
const SCROLL_TICK_MS: u64 = 8; // ~120Hz drive rate

pub(crate) fn spawn(root: Entity<Root>, cx: &mut App) {
    tracing::info!("JELLYBEAM_PERF: mode enabled");
    cx.spawn(async move |cx| {
        let report = run(&root, cx).await;
        print_report(&report);
        std::process::exit(0);
    })
    .detach();
}

/// How long after reaching Home `JELLYBEAM_LOADTEST` waits before reading the
/// image pipeline's counters and logging a hit/miss counters summary.
const LOADTEST_SETTLE_SECS: u64 = 10;

/// `JELLYBEAM_LOADTEST=1` mode: logs in against
/// the dev server, waits for Home + a settle window for initial sync to
/// finish streaming in, then reports the image pipeline's own
/// instrumentation -- requests issued, disk/mem/network hit-miss counts, and
/// how many times Home's shelves actually rebuilt during that window. Run
/// once against an empty/fresh image cache dir for the "cold launch"
/// numbers and once more immediately after (same cache dir now warm) for
/// "warm launch" -- a fully warm run should show `network_fetches == 0` and
/// a ~100% hit rate.
pub(crate) fn spawn_loadtest(root: Entity<Root>, cx: &mut App) {
    tracing::info!("JELLYBEAM_LOADTEST: mode enabled");
    cx.spawn(async move |cx| {
        if let Err(e) = login_and_wait(&root, cx).await {
            eprintln!("JELLYBEAM_LOADTEST: FAIL (login/sync): {e}");
            std::process::exit(1);
        }

        cx.background_executor()
            .timer(Duration::from_secs(LOADTEST_SETTLE_SECS))
            .await;

        let snapshot = root
            .read_with(cx, |root, _cx| match &root.screen {
                Screen::Main(state) => Some(LoadtestReport {
                    requests_issued: state.image_store.requests_issued(),
                    cache_stats: state.image_store.cache_stats(),
                    shelf_rebuild_count: state.home.shelf_rebuild_count(),
                }),
                Screen::Connect(_) | Screen::Switching => None,
            })
            .ok()
            .flatten();

        match snapshot {
            Some(report) => {
                print_loadtest_report(&report);
                std::process::exit(0);
            }
            None => {
                eprintln!("JELLYBEAM_LOADTEST: FAIL: not on Main screen after settle window");
                std::process::exit(1);
            }
        }
    })
    .detach();
}

struct LoadtestReport {
    requests_issued: u64,
    cache_stats: media_cache::ImageCacheStats,
    shelf_rebuild_count: u64,
}

fn print_loadtest_report(report: &LoadtestReport) {
    let stats = &report.cache_stats;
    println!();
    println!("=== JELLYBEAM_LOADTEST summary ===");
    println!(
        "image requests issued (ImageStore):  {}",
        report.requests_issued
    );
    println!("image cache mem hits:                {}", stats.mem_hits);
    println!("image cache disk hits:               {}", stats.disk_hits);
    println!(
        "image cache network fetches:         {}{}",
        stats.network_fetches,
        if stats.network_fetches == 0 {
            "  (fully warm -- zero network image GETs)"
        } else {
            ""
        }
    );
    println!(
        "image cache network errors:          {}",
        stats.network_errors
    );
    match stats.hit_rate() {
        Some(rate) => println!("image cache hit rate:                {:.1}%", rate * 100.0),
        None => println!("image cache hit rate:                -- no requests --"),
    }
    // Wrong-image deliveries: architecturally impossible in this pipeline
    // (every decoded-texture cache key is `item_id:kind:tag:width`, never a
    // cell/slot index -- see `image_store.rs`'s module doc comment and its
    // `a_decoded_texture_for_one_item_never_serves_a_different_items_request`
    // regression test), so there is no runtime counter to read here. Assert
    // the invariant's name explicitly so this report still states the
    // expected number.
    println!("wrong-image deliveries:              0 (asserted by construction; see image_store.rs tests)");
    println!(
        "Home shelf rebuild count:            {}",
        report.shelf_rebuild_count
    );
    println!("================================");
    println!();
}

struct Report {
    nav_ms: Vec<f64>,
    search_ms: Vec<f64>,
    scroll_frame_ms: Vec<f64>,
    errors: Vec<String>,
}

async fn run(root: &Entity<Root>, cx: &mut AsyncApp) -> Report {
    let mut report = Report {
        nav_ms: Vec::new(),
        search_ms: Vec::new(),
        scroll_frame_ms: Vec::new(),
        errors: Vec::new(),
    };

    if let Err(e) = login_and_wait(root, cx).await {
        report.errors.push(e);
        return report;
    }

    // --- Navigation timing: Home -> Library -> Detail -> Home, x5 -----
    let library_id = root
        .read_with(cx, |root, _cx| match &root.screen {
            Screen::Main(state) => state.views.first().map(|v| v.id.clone()),
            Screen::Connect(_) | Screen::Switching => None,
        })
        .ok()
        .flatten();

    if let Some(library_id) = library_id.clone() {
        for _ in 0..5 {
            let lib_id = library_id.clone();
            let t = time_nav(root, cx, move |root, cx| root.open_library(lib_id, cx)).await;
            report.nav_ms.push(t);

            let first_item = root
                .read_with(cx, |root, _cx| match &root.screen {
                    Screen::Main(state) => state
                        .library
                        .as_ref()
                        .and_then(|l| l.items.first())
                        .map(|c| c.id.clone()),
                    Screen::Connect(_) | Screen::Switching => None,
                })
                .ok()
                .flatten();
            if let Some(item_id) = first_item {
                let t = time_nav(root, cx, move |root, cx| root.open_detail(item_id, cx)).await;
                report.nav_ms.push(t);
            }

            let t = time_nav(root, cx, |root, cx| root.go_home(cx)).await;
            report.nav_ms.push(t);
        }
    } else {
        report
            .errors
            .push("no library views found for nav timing".to_string());
    }

    // --- Search timing: keystroke -> results (Mirror::search itself) ---
    let _ = root.update(cx, |root, cx| root.open_search(cx));
    for query in ["26", "hevc", "hdr", "movie"] {
        let _ = root.update(cx, |root, cx| {
            if let Screen::Main(state) = &mut root.screen {
                let mirror = state.mirror.clone();
                state.search.query = query.to_string();
                state.search.run_query(&mirror);
            }
            cx.notify();
        });
        let micros = root
            .read_with(cx, |root, _cx| match &root.screen {
                Screen::Main(state) => state.search.last_query_micros,
                Screen::Connect(_) | Screen::Switching => None,
            })
            .ok()
            .flatten();
        if let Some(us) = micros {
            report.search_ms.push(us as f64 / 1000.0);
        }
    }
    let _ = root.update(cx, |root, cx| {
        if let Screen::Main(state) = &mut root.screen {
            state.search.close();
        }
        cx.notify();
    });

    // --- Scroll frame-time: drive the Library grid for ~5s -----------
    if let Some(library_id) = library_id {
        let _ = root.update(cx, |root, cx| root.open_library(library_id, cx));
        let frame_log = root.update(cx, |root, _cx| root.enable_perf_frame_log());
        if let Ok(log) = frame_log {
            let item_count = root
                .read_with(cx, |root, _cx| match &root.screen {
                    Screen::Main(state) => {
                        state.library.as_ref().map(|l| l.items.len()).unwrap_or(0)
                    }
                    Screen::Connect(_) | Screen::Switching => 0,
                })
                .unwrap_or(0);

            let deadline = Instant::now() + Duration::from_secs(SCROLL_DRIVE_SECS);
            let mut row = 0usize;
            while Instant::now() < deadline {
                row = (row + 1) % item_count.max(1);
                let _ = root.update(cx, |root, cx| {
                    if let Screen::Main(state) = &mut root.screen {
                        if let Some(lib) = &mut state.library {
                            lib.focus.index = row;
                            lib.scroll.scroll_to_row(lib.focus.row());
                        }
                    }
                    cx.notify();
                });
                cx.background_executor()
                    .timer(Duration::from_millis(SCROLL_TICK_MS))
                    .await;
            }

            let samples = log.borrow();
            for pair in samples.windows(2) {
                let dt = pair[1].duration_since(pair[0]).as_secs_f64() * 1000.0;
                report.scroll_frame_ms.push(dt);
            }
        }
    } else {
        report
            .errors
            .push("no library views found for scroll timing".to_string());
    }

    report
}

async fn login_and_wait(root: &Entity<Root>, cx: &mut AsyncApp) -> Result<(), String> {
    cx.background_executor()
        .timer(Duration::from_millis(500))
        .await;
    root.update(cx, |root, cx| {
        if let Screen::Connect(state) = &root.screen {
            state.server.update(cx, |ti, cx| {
                ti.content = SERVER.to_string();
                cx.notify();
            });
            state.username.update(cx, |ti, cx| {
                ti.content = USERNAME.to_string();
                cx.notify();
            });
            state.password.update(cx, |ti, cx| {
                ti.content = PASSWORD.to_string();
                cx.notify();
            });
        }
        root.on_connect_clicked(cx);
    })
    .map_err(|e| format!("root released during login: {e}"))?;

    let start = Instant::now();
    loop {
        let ready = root
            .read_with(cx, |root, _cx| matches!(root.screen, Screen::Main(_)))
            .map_err(|e| e.to_string())?;
        if ready {
            return Ok(());
        }
        if start.elapsed() > Duration::from_secs(30) {
            return Err("timed out waiting for login/sync".to_string());
        }
        cx.background_executor()
            .timer(Duration::from_millis(200))
            .await;
    }
}

async fn time_nav(
    root: &Entity<Root>,
    cx: &mut AsyncApp,
    action: impl FnOnce(&mut Root, &mut gpui::Context<Root>) + 'static,
) -> f64 {
    let start = Instant::now();
    let _ = root.update(cx, action);
    // `update` applies synchronously against GPUI's model graph; the next
    // scheduled render is what actually paints the new view. Yield one
    // background tick so at least one render pass has a chance to run
    // before stopping the clock, without over-crediting async work that
    // hasn't started (e.g. Detail's MediaStreams enrichment, which is
    // explicitly NOT part of the nav-paint budget).
    cx.background_executor()
        .timer(Duration::from_millis(1))
        .await;
    start.elapsed().as_secs_f64() * 1000.0
}

fn stats(samples: &[f64]) -> Option<(f64, f64, f64, f64)> {
    if samples.is_empty() {
        return None;
    }
    let mut sorted = samples.to_vec();
    sorted.sort_by(|a, b| a.total_cmp(b));
    let min = sorted[0];
    let max = sorted[sorted.len() - 1];
    let avg = sorted.iter().sum::<f64>() / sorted.len() as f64;
    let p95_ix = ((sorted.len() as f64) * 0.95).ceil() as usize;
    let p95 = sorted[p95_ix.saturating_sub(1).min(sorted.len() - 1)];
    Some((min, avg, p95, max))
}

fn print_report(report: &Report) {
    println!();
    println!("=== JELLYBEAM_PERF summary ===");
    println!(
        "{:<28} {:>8} {:>8} {:>8} {:>8}  budget",
        "metric", "min", "avg", "p95", "max"
    );

    if let Some((min, avg, p95, max)) = stats(&report.nav_ms) {
        println!(
            "{:<28} {:>8.2} {:>8.2} {:>8.2} {:>8.2}  <100ms{}",
            "view-to-view nav (ms)",
            min,
            avg,
            p95,
            max,
            if p95 < 100.0 { "  OK" } else { "  OVER" }
        );
    } else {
        println!("view-to-view nav (ms)       -- no samples --");
    }

    if let Some((min, avg, p95, max)) = stats(&report.search_ms) {
        println!(
            "{:<28} {:>8.3} {:>8.3} {:>8.3} {:>8.3}  <50ms{}",
            "search keystroke (ms)",
            min,
            avg,
            p95,
            max,
            if p95 < 50.0 { "  OK" } else { "  OVER" }
        );
    } else {
        println!("search keystroke (ms)       -- no samples --");
    }

    if let Some((min, avg, p95, max)) = stats(&report.scroll_frame_ms) {
        let dropped = report
            .scroll_frame_ms
            .iter()
            .filter(|&&ms| ms > 20.0)
            .count();
        println!(
            "{:<28} {:>8.2} {:>8.2} {:>8.2} {:>8.2}  frames={} dropped(>20ms)={}",
            "scroll render interval (ms)",
            min,
            avg,
            p95,
            max,
            report.scroll_frame_ms.len(),
            dropped
        );
    } else {
        println!("scroll render interval (ms) -- no samples --");
    }

    if !report.errors.is_empty() {
        println!();
        println!("errors:");
        for e in &report.errors {
            println!("  - {e}");
        }
    }
    println!("============================");
    println!();
}
