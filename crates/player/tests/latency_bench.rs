//! Stream-open -> first-frame latency benchmarks against a live Jellyfin
//! server, driven through the same headless CGL harness as the rest of
//! `tests/`. See `crates/player/LATENCY.md` for what is measured, the link
//! profiles, and how to read the output.
//!
//! Every test is `#[ignore]`d. The proxied ones need a TCP relay on
//! `127.0.0.1:8097` forwarding to the dev server on `:8096` with a fixed
//! per-direction delay (applied once per request, not per read chunk) and a
//! per-connection bandwidth cap:
//!
//! ```text
//! cargo test -p player --test latency_bench -- --ignored --nocapture --test-threads=1
//! ```
//!
//! Only the resolved media URL is rewritten to the relay; auth and
//! `PlaybackInfo` go direct (except in the click-to-first-frame tests), so
//! the injected delay lands on mpv's stream-open path.
//!
//! ## Long fixtures
//!
//! The standing corpus (`dev/corpus/build-matrix.sh`) is ~5s per file, too short
//! to resume into. The resume tests expect 240s, ~8Mbps fixtures in the dev
//! server's `dev/media/Movies/`, not checked in:
//!   - `92-h264-long-aac.ts`, `93-hevc8-long-aac.ts`: MPEG-TS on purpose. It
//!     has no global keyframe index, so a far seek is a binary search with a
//!     round trip per probe (Matroska's `Cues` make the same seek one lookup).
//!
//! Regenerate + re-ingest:
//! ```text
//! ffmpeg -y -f lavfi -i "testsrc2=size=1280x720:rate=24:duration=240" \
//!     -f lavfi -i "sine=frequency=440:duration=240" \
//!     -c:v libx264 -preset ultrafast -b:v 8000k -maxrate 8000k -bufsize 4000k \
//!     -pix_fmt yuv420p -c:a aac -b:a 96k -shortest \
//!     dev/media/Movies/92-h264-long-aac.ts
//! curl -X POST http://localhost:8096/Library/Refresh -H "Authorization: MediaBrowser Token=\"$TOKEN\""
//! ```
//! (swap `libx265 ... -tag:v hvc1` for the `93-hevc8-long-aac.ts` HEVC one).
//! Tests skip themselves with a message if a fixture isn't present.

#![cfg(target_os = "macos")]

mod common;

use std::time::{Duration, Instant};

use jellyfin_api::{ClientIdentity, ItemQuery, JellyfinClient};
use player::{LoadRequest, Player, PlayerEvent};

/// Direct (unproxied) control-plane URL -- auth + `PlaybackInfo` go here.
/// Override with `JELLYBEAM_DEV_SERVER_URL` for other dev server instances,
/// BUT NOTE: the opt-in Jellyfin 12.0 dev server (dev/README.md) publishes
/// on `localhost:8097`, which collides with `PROXY_AUTHORITY` below --
/// don't point this at :8097 while also running these tests through the
/// proxy, or the "proxied" and "direct" legs land on the same port.
fn direct_base_url() -> String {
    std::env::var("JELLYBEAM_DEV_SERVER_URL")
        .unwrap_or_else(|_| "http://localhost:8096".to_string())
}
/// The latency relay -- only the resolved media URL is rewritten to point
/// here (see module docs).
const PROXY_AUTHORITY: &str = "127.0.0.1:8097";

const USERNAME: &str = "jellybeam-admin";
const PASSWORD: &str = "jellybeam-test";

fn identity() -> ClientIdentity {
    ClientIdentity {
        client: "Jellybeam-LatencyBench".to_string(),
        device: "latency-bench".to_string(),
        device_id: "jellybeam-latency-bench-device".to_string(),
        version: "0.1.0".to_string(),
    }
}

async fn authenticated_client() -> JellyfinClient {
    let direct_base_url = direct_base_url();
    let (client, _auth) =
        JellyfinClient::authenticate_by_name(&direct_base_url, identity(), USERNAME, PASSWORD)
            .await
            .unwrap_or_else(|err| {
                panic!(
                    "failed to authenticate against {direct_base_url} as '{USERNAME}': {err:?}. \
                     Is the dev Jellyfin server running with the jellybeam-admin/jellybeam-test \
                     account seeded?"
                )
            });
    client
}

/// Same matching strategy as `live_contract.rs::find_item_id_by_filename`.
/// Returns `None` (rather than panicking) so resume-scenario tests can skip
/// themselves cleanly when the long synthetic fixtures haven't been
/// generated/ingested on this dev box yet.
async fn find_item_id_by_filename(client: &JellyfinClient, filename: &str) -> Option<String> {
    let result = client
        .get_items(&ItemQuery {
            parent_id: None,
            include_item_types: vec!["Movie".to_string()],
            recursive: true,
            sort_by: None,
            sort_order: None,
            fields: vec!["Path".to_string()],
            start_index: 0,
            limit: 500,
            ids: Vec::new(),
            is_missing: None,
            min_date_last_saved: None,
            ..ItemQuery::new()
        })
        .await
        .expect("list items from the live server (GET /Items)");

    result
        .items
        .into_iter()
        .find(|item| {
            item.path
                .as_deref()
                .and_then(|p| p.rsplit('/').next())
                .is_some_and(|basename| basename == filename)
        })
        .and_then(|item| item.id)
        .map(|id| id.to_string())
}

/// Resolves `filename` to a direct-play stream URL via the real
/// `PlaybackInfo` call (same auth + request shape as `live_contract.rs`),
/// then rewrites its scheme+authority to [`PROXY_AUTHORITY`] so only the
/// media goes through the relay.
///
/// Bypasses `jellyfin_core::decide_playback`: `transcoding_url` is forced to
/// `None` so this always builds the static direct-play URL, keeping the
/// server's transcode decision out of an mpv stream-open measurement.
async fn proxied_stream_url(client: &JellyfinClient, filename: &str) -> Option<String> {
    let item_id = find_item_id_by_filename(client, filename).await?;
    let profile = jellyfin_core::build_device_profile(None);
    let info = client
        .get_playback_info(&item_id, &profile, None)
        .await
        .expect("PlaybackInfo request to the live server");

    let mut source = info
        .media_sources
        .into_iter()
        .next()
        .unwrap_or_else(|| panic!("PlaybackInfo for '{filename}' returned no MediaSources"));
    source.transcoding_url = None;

    let direct_url = client.stream_url(&item_id, &source);
    Some(rewrite_authority(&direct_url, PROXY_AUTHORITY))
}

/// Replaces the `scheme://host:port` prefix of an `http(s)://...` URL with
/// `new_authority`, leaving the path/query untouched.
fn rewrite_authority(url: &str, new_authority: &str) -> String {
    let (scheme, rest) = url
        .split_once("://")
        .unwrap_or_else(|| panic!("expected an absolute http(s) URL, got {url:?}"));
    let path_start = rest.find('/').unwrap_or(rest.len());
    let path_and_query = &rest[path_start..];
    format!("{scheme}://{new_authority}{path_and_query}")
}

fn simple_load(url: String) -> LoadRequest {
    LoadRequest {
        url,
        http_headers: Vec::new(),
        start_secs: None,
        external_subs: Vec::new(),
        start_paused: false,
        readahead_secs: None,
        max_bytes: None,
    }
}

/// Runs one load and returns `(time to Loaded, time to first Position)`,
/// both from just before `Player::load`. First `Position` stands in for
/// "first frame"; the render loop is pumped anyway so the full
/// decode/render pipeline runs while measuring.
fn measure_load(url: String, start_secs: Option<f64>, label: &str) -> (Duration, Duration) {
    let _gl = common::GlContext::new_current().expect("headless CGL context");
    let fbo = common::Fbo::new(320, 240).expect("offscreen FBO");
    let player = Player::new(common::gl_get_proc_address).expect("Player::new");
    let events = common::collect_events(&player);

    let req = LoadRequest {
        start_secs,
        ..simple_load(url)
    };

    let t0 = Instant::now();
    player.load(req).expect("load() should be accepted");

    let deadline = t0 + Duration::from_secs(60);
    let mut loaded_at = None;
    let mut position_at = None;
    while Instant::now() < deadline && (loaded_at.is_none() || position_at.is_none()) {
        if player.needs_render() {
            let _ = player.render(fbo.fbo as i32, fbo.width, fbo.height);
            player.report_swap();
        }
        while let Ok(ev) = events.try_recv() {
            common::assert_not_player_error(&ev);
            match ev {
                PlayerEvent::Loaded { .. } if loaded_at.is_none() => {
                    loaded_at = Some(t0.elapsed());
                }
                PlayerEvent::Position { .. } if position_at.is_none() => {
                    position_at = Some(t0.elapsed());
                }
                _ => {}
            }
        }
        std::thread::sleep(Duration::from_millis(2));
    }

    let loaded_at = loaded_at.expect("expected a Loaded event within 60s (through the proxy)");
    let position_at =
        position_at.expect("expected a Position event within 60s (through the proxy)");

    println!("[open-bench] {label}: Loaded={loaded_at:?} first-Position={position_at:?}");
    (loaded_at, position_at)
}

fn tokio_rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build a current-thread tokio runtime for the auth/PlaybackInfo calls")
}

// --- (a) fresh load, h264 ------------------------------------------------

#[ignore = "requires a live Jellyfin server at localhost:8096 AND the latency relay on 127.0.0.1:8097"]
#[test]
fn bench_fresh_load_h264() {
    let rt = tokio_rt();
    let url = rt.block_on(async {
        let client = authenticated_client().await;
        proxied_stream_url(&client, "01-h264-aac.mkv")
            .await
            .expect("01-h264-aac.mkv should be in the live server's library")
    });
    measure_load(url, None, "fresh h264 (01-h264-aac.mkv)");
}

// --- (b) resume at +200s, h264-in-ts (see module docs re: why .ts) -------

#[ignore = "requires a live Jellyfin server at localhost:8096 AND the latency relay on 127.0.0.1:8097, \
            plus the 240s '92-h264-long-aac.ts' fixture (see module docs)"]
#[test]
fn bench_resume_200s_h264() {
    let rt = tokio_rt();
    let url = rt.block_on(async {
        let client = authenticated_client().await;
        proxied_stream_url(&client, "92-h264-long-aac.ts").await
    });
    let Some(url) = url else {
        eprintln!(
            "[open-bench] skipping: '92-h264-long-aac.ts' not found on the live server -- \
             generate it (see tests/latency_bench.rs module docs) and POST /Library/Refresh"
        );
        return;
    };
    measure_load(
        url,
        Some(200.0),
        "resume +200s h264-ts (92-h264-long-aac.ts)",
    );
}

// --- (a)/(b) HEVC ---------------------------------------------------------

#[ignore = "requires a live Jellyfin server at localhost:8096 AND the latency relay on 127.0.0.1:8097"]
#[test]
fn bench_fresh_load_hevc() {
    let rt = tokio_rt();
    let url = rt.block_on(async {
        let client = authenticated_client().await;
        proxied_stream_url(&client, "06-hevc8-aac.ts")
            .await
            .expect("06-hevc8-aac.ts should be in the live server's library")
    });
    measure_load(url, None, "fresh hevc (06-hevc8-aac.ts)");
}

#[ignore = "requires a live Jellyfin server at localhost:8096 AND the latency relay on 127.0.0.1:8097, \
            plus the 240s '93-hevc8-long-aac.ts' fixture (see module docs)"]
#[test]
fn bench_resume_200s_hevc() {
    let rt = tokio_rt();
    let url = rt.block_on(async {
        let client = authenticated_client().await;
        proxied_stream_url(&client, "93-hevc8-long-aac.ts").await
    });
    let Some(url) = url else {
        eprintln!(
            "[open-bench] skipping: '93-hevc8-long-aac.ts' not found on the live server -- \
             generate it (see tests/latency_bench.rs module docs) and POST /Library/Refresh"
        );
        return;
    };
    measure_load(
        url,
        Some(200.0),
        "resume +200s hevc-ts (93-hevc8-long-aac.ts)",
    );
}

// --- fresh vs. resume on the same file, no proxy -------------------------
//
// Same file, back to back, near-zero RTT: separates the fresh-vs-resume
// variable from file identity and network cost. `94-h264-long-aac.mkv` is
// the 240s/8Mbps recipe muxed as Matroska (Cues-indexed), the indexed
// counterpart to 92/93. Not checked in; regenerate via:
//
// ```text
// ffmpeg -y -f lavfi -i "testsrc2=size=1280x720:rate=24:duration=240" \
//     -f lavfi -i "sine=frequency=440:duration=240" \
//     -c:v libx264 -preset ultrafast -b:v 8000k -maxrate 8000k -bufsize 4000k \
//     -pix_fmt yuv420p -c:a aac -b:a 96k -shortest \
//     dev/media/Movies/94-h264-long-aac.mkv
// curl -X POST http://localhost:8096/Library/Refresh -H "Authorization: MediaBrowser Token=\"$TOKEN\""
// ```

/// Same as `proxied_stream_url` but leaves the URL pointed at the direct
/// server -- no proxy rewrite. Returns `None` (rather than panicking) so
/// callers can skip cleanly if a fixture isn't present on this dev box.
async fn direct_stream_url(client: &JellyfinClient, filename: &str) -> Option<String> {
    let item_id = find_item_id_by_filename(client, filename).await?;
    let profile = jellyfin_core::build_device_profile(None);
    let info = client
        .get_playback_info(&item_id, &profile, None)
        .await
        .expect("PlaybackInfo request to the live server");
    let mut source = info
        .media_sources
        .into_iter()
        .next()
        .unwrap_or_else(|| panic!("PlaybackInfo for '{filename}' returned no MediaSources"));
    source.transcoding_url = None;
    Some(client.stream_url(&item_id, &source))
}

/// Loads `url` `n` times back to back, each with a new `Player` (so each
/// pays hwdec setup), and prints `(Loaded, Position)` per run tagged
/// `label`.
fn measure_n(url: &str, start_secs: Option<f64>, label: &str, n: usize) {
    for i in 0..n {
        measure_load(
            url.to_string(),
            start_secs,
            &format!("{label} [run {}/{}]", i + 1, n),
        );
    }
}

/// Runs `filename` fresh (from 0:00) and resumed (at `resume_secs`) back to
/// back, `n` times each, all through the direct (unproxied) server
/// connection. Prints the timings; asserts no ordering (a measurement, not
/// a regression gate). See `crates/player/LATENCY.md`, "Measurements".
fn compare_fresh_vs_resume(filename: &str, resume_secs: f64, n: usize) {
    let rt = tokio_rt();
    let url = rt.block_on(async {
        let client = authenticated_client().await;
        direct_stream_url(&client, filename).await
    });
    let Some(url) = url else {
        eprintln!(
            "[resume-bench] skipping: '{filename}' not found on the live server -- \
             generate it (see tests/latency_bench.rs module docs) and POST /Library/Refresh"
        );
        return;
    };
    println!("[resume-bench] === {filename}: fresh (start=0:00) ===");
    measure_n(&url, None, &format!("fresh ({filename})"), n);
    println!("[resume-bench] === {filename}: resume (+{resume_secs}s) ===");
    measure_n(
        &url,
        Some(resume_secs),
        &format!("resume +{resume_secs}s ({filename})"),
        n,
    );
}

#[ignore = "requires a live Jellyfin server at localhost:8096 (no proxy needed)"]
#[test]
fn bench_fresh_vs_resume_short_indexed_mkv() {
    // 01-h264-aac.mkv: ~5s, Cues-indexed. Resume target is necessarily
    // shallow (there's only ~5s of file), but exercises the same
    // fresh-vs-resume code path as the long fixtures below with a
    // realistically-tiny GOP structure.
    compare_fresh_vs_resume("01-h264-aac.mkv", 2.0, 3);
}

#[ignore = "requires a live Jellyfin server at localhost:8096 (no proxy needed), \
            plus the 240s '92-h264-long-aac.ts' fixture (see module docs)"]
#[test]
fn bench_fresh_vs_resume_long_unindexed_ts_h264() {
    compare_fresh_vs_resume("92-h264-long-aac.ts", 120.0, 3);
}

#[ignore = "requires a live Jellyfin server at localhost:8096 (no proxy needed), \
            plus the 240s '93-hevc8-long-aac.ts' fixture (see module docs)"]
#[test]
fn bench_fresh_vs_resume_long_unindexed_ts_hevc() {
    compare_fresh_vs_resume("93-hevc8-long-aac.ts", 120.0, 3);
}

#[ignore = "requires a live Jellyfin server at localhost:8096 (no proxy needed), \
            plus the 240s '94-h264-long-aac.mkv' fixture (see this section's module docs)"]
#[test]
fn bench_fresh_vs_resume_long_indexed_mkv_h264() {
    compare_fresh_vs_resume("94-h264-long-aac.mkv", 120.0, 3);
}

/// Same as `compare_fresh_vs_resume`, but through the latency relay.
fn compare_fresh_vs_resume_proxied(filename: &str, resume_secs: f64, n: usize) {
    let rt = tokio_rt();
    let url = rt.block_on(async {
        let client = authenticated_client().await;
        proxied_stream_url(&client, filename).await
    });
    let Some(url) = url else {
        eprintln!(
            "[resume-bench] skipping: '{filename}' not found on the live server -- \
             generate it (see tests/latency_bench.rs module docs) and POST /Library/Refresh"
        );
        return;
    };
    println!("[resume-bench-proxied] === {filename}: fresh (start=0:00) ===");
    measure_n(&url, None, &format!("fresh-proxied ({filename})"), n);
    println!("[resume-bench-proxied] === {filename}: resume (+{resume_secs}s) ===");
    measure_n(
        &url,
        Some(resume_secs),
        &format!("resume-proxied +{resume_secs}s ({filename})"),
        n,
    );
}

#[ignore = "requires a live Jellyfin server at localhost:8096 AND the latency relay on 127.0.0.1:8097, \
            plus the 240s '92-h264-long-aac.ts' fixture (see module docs)"]
#[test]
fn bench_fresh_vs_resume_proxied_long_unindexed_ts_h264() {
    compare_fresh_vs_resume_proxied("92-h264-long-aac.ts", 120.0, 3);
}

#[ignore = "requires a live Jellyfin server at localhost:8096 AND the latency relay on 127.0.0.1:8097, \
            plus the 240s '94-h264-long-aac.mkv' fixture (see this section's module docs)"]
#[test]
fn bench_fresh_vs_resume_proxied_long_indexed_mkv_h264() {
    compare_fresh_vs_resume_proxied("94-h264-long-aac.mkv", 120.0, 3);
}

// --- server-side transcode start (HLS), fresh vs. resume ------------------
//
// Times raw HTTP GETs of the HLS master playlist, media playlist and first
// segment, with no mpv involved: how long the server's transcode takes to
// produce playable output from 0:00 versus from an `-ss` resume offset.

/// Forces a transcode decision (bitrate cap well under the ~8Mbps fixture
/// bitrate) and returns the resolved HLS `TranscodingUrl` (or panics if the
/// server didn't offer one -- that would mean the bitrate cap failed to force
/// a transcode, a test-setup bug, not a real skip condition).
async fn transcode_hls_url(
    client: &JellyfinClient,
    filename: &str,
    start_ticks: Option<i64>,
) -> String {
    let item_id = find_item_id_by_filename(client, filename)
        .await
        .unwrap_or_else(|| panic!("{filename} should be in the live server's library"));
    // 500kbps: comfortably under the ~8Mbps fixtures, forces `decide_playback`
    // into its Transcode branch every time.
    let profile = jellyfin_core::build_device_profile(Some(500_000));
    let info = client
        .get_playback_info(&item_id, &profile, start_ticks)
        .await
        .expect("PlaybackInfo request to the live server");
    let decision = jellyfin_core::decide_playback(client, &item_id, &info)
        .expect("decide_playback should succeed for a known-good fixture");
    match decision {
        jellyfin_core::PlaybackDecision::Transcode { hls_url, .. } => hls_url,
        jellyfin_core::PlaybackDecision::DirectPlay { .. } => panic!(
            "expected a Transcode decision for '{filename}' under a 500kbps cap -- \
             the bitrate-cap test setup didn't force transcode as intended"
        ),
    }
}

/// GETs `master_url`, extracts the first media-playlist reference (an HLS
/// master playlist is a text file whose non-`#`-prefixed lines are relative
/// URLs to per-quality-level media playlists), resolves it against
/// `master_url`'s own authority, fetches that, and finally fetches the
/// first `#EXTINF`-preceded segment URL it names. Returns elapsed time from
/// just before the master-playlist GET to the first segment response's
/// first byte -- the server-side analogue of "stream-open to first frame"
/// for a transcoded source, since nothing is playable client-side until at
/// least that first segment exists.
async fn time_to_first_hls_segment(http: &reqwest::Client, master_url: &str) -> Duration {
    let t0 = Instant::now();
    let master_body = http
        .get(master_url)
        .send()
        .await
        .expect("GET master playlist")
        .text()
        .await
        .expect("read master playlist body");
    let media_rel = master_body
        .lines()
        .find(|l| !l.trim().is_empty() && !l.starts_with('#'))
        .unwrap_or_else(|| panic!("master playlist had no media-playlist line:\n{master_body}"));
    let media_url = resolve_relative(master_url, media_rel);

    let media_body = http
        .get(&media_url)
        .send()
        .await
        .expect("GET media playlist")
        .text()
        .await
        .expect("read media playlist body");
    let seg_rel = media_body
        .lines()
        .find(|l| !l.trim().is_empty() && !l.starts_with('#'))
        .unwrap_or_else(|| panic!("media playlist had no segment line:\n{media_body}"));
    let seg_url = resolve_relative(&media_url, seg_rel);

    let resp = http.get(&seg_url).send().await.expect("GET first segment");
    // First byte of the response is what actually matters (mirrors "first
    // frame" -- a client could start feeding mpv the instant bytes start
    // arriving), not the full segment body, but `reqwest` only exposes
    // "response headers received" vs "full body read" -- headers-received
    // is close enough (segment bodies are a few hundred KB at most over
    // localhost, sub-millisecond to stream once headers are back) and is
    // what `.send()`'s returned future resolves at.
    drop(resp);
    t0.elapsed()
}

/// Resolves a (possibly relative) HLS playlist reference against the URL it
/// was found in -- standard "relative to the containing playlist's own
/// directory" resolution (RFC 3986 style), which is what Jellyfin's HLS
/// playlists assume (their entries are e.g. `main.m3u8?...` or
/// `hls1/main/0.ts?...`, relative to wherever the *referencing* playlist
/// itself lives, not to `/videos/`). An absolute `rel` (starts with
/// `http(s)://`) is returned unchanged.
fn resolve_relative(base: &str, rel: &str) -> String {
    if rel.starts_with("http://") || rel.starts_with("https://") {
        return rel.to_string();
    }
    let base_path = base.split('?').next().unwrap_or(base);
    let dir = match base_path.rsplit_once('/') {
        Some((dir, _file)) => dir,
        None => base_path,
    };
    format!("{dir}/{rel}")
}

#[ignore = "requires a live Jellyfin server at localhost:8096 (no proxy needed), \
            plus the 240s '92-h264-long-aac.ts' fixture (see module docs)"]
#[test]
fn bench_fresh_vs_resume_transcode_server_side() {
    let rt = tokio_rt();
    rt.block_on(async {
        let client = authenticated_client().await;
        let http = reqwest::Client::new();
        const N: usize = 3;

        println!("[resume-bench-transcode] === 92-h264-long-aac.ts: fresh (start=0:00) ===");
        for i in 0..N {
            let url = transcode_hls_url(&client, "92-h264-long-aac.ts", None).await;
            let d = time_to_first_hls_segment(&http, &url).await;
            println!(
                "[resume-bench-transcode] fresh [run {}/{N}]: first-segment={d:?}",
                i + 1
            );
        }

        println!("[resume-bench-transcode] === 92-h264-long-aac.ts: resume (+120s) ===");
        for i in 0..N {
            let url = transcode_hls_url(&client, "92-h264-long-aac.ts", Some(1_200_000_000)).await;
            let d = time_to_first_hls_segment(&http, &url).await;
            println!(
                "[resume-bench-transcode] resume +120s [run {}/{N}]: first-segment={d:?}",
                i + 1
            );
        }
    });
}

// --- cold fresh vs. warm resume -------------------------------------------
//
// A file's first open in a session pays server file-open, page-cache and
// connection warm-up; a resume is almost always of a recently played file.
// "Cold fresh" opens a file this process has not touched, "warm resume"
// reopens it right after at an offset. Each file is used for exactly one
// cold measurement.

/// Loads `filename` cold (this call is that file's first-ever open this
/// process) with `start_secs = None`, immediately followed by a second load
/// of the SAME file (now warm) with `start_secs = Some(resume_secs)`.
/// Prints both `(Loaded, Position)` pairs tagged `cold-fresh`/`warm-resume`.
fn compare_cold_fresh_vs_warm_resume(filename: &str, resume_secs: f64) {
    let rt = tokio_rt();
    let url = rt.block_on(async {
        let client = authenticated_client().await;
        direct_stream_url(&client, filename).await
    });
    let Some(url) = url else {
        eprintln!("[cache-bench] skipping: '{filename}' not found on the live server");
        return;
    };
    measure_load(url.clone(), None, &format!("cold-fresh ({filename})"));
    measure_load(
        url,
        Some(resume_secs),
        &format!("warm-resume +{resume_secs}s ({filename}, same process, right after cold-fresh)"),
    );
}

#[ignore = "requires a live Jellyfin server at localhost:8096 (no proxy needed)"]
#[test]
fn bench_cold_fresh_vs_warm_resume_across_untouched_files() {
    // Every filename here must be one this test binary/process hasn't
    // opened elsewhere in this file (including other #[ignore] tests run in
    // the SAME `cargo test` invocation would share page-cache state across
    // tests too, but each test process here is independent -- `cargo test`
    // runs each `#[test]` fn in-process sequentially within one binary, so
    // to keep this test's own "cold" numbers honest, it uses files no other
    // test in this module touches).
    for (filename, resume_secs) in [
        ("02-h264-ac3.mp4", 2.0),
        ("03-h264-eac3.ts", 2.0),
        ("04-h264-flac.mkv", 2.0),
        ("05-h264-opus.mp4", 2.0),
        ("07-hevc8-ac3.mkv", 2.0),
        ("08-hevc8-eac3.mp4", 2.0),
    ] {
        compare_cold_fresh_vs_warm_resume(filename, resume_secs);
    }
}

// --- `start=+0` present vs. absent -----------------------------------------
//
// `build_loadfile_options` omits `start` for a fresh load. This isolates
// that one variable (absent vs. `start=+0`) on the same warm file, with
// interleaved runs so warm-up drift cannot favour either arm.
#[ignore = "requires a live Jellyfin server at localhost:8096 (no proxy needed)"]
#[test]
fn bench_start_option_presence_vs_absence_same_target() {
    let rt = tokio_rt();
    let url = rt.block_on(async {
        let client = authenticated_client().await;
        direct_stream_url(&client, "94-h264-long-aac.mkv")
            .await
            .expect("94-h264-long-aac.mkv should be in the live server's library (see module docs)")
    });
    // Warm the connection/cache/JIT first with a couple of throwaway loads
    // so the comparison below isn't dominated by session warm-up noise.
    measure_load(url.clone(), None, "warm-up (discarded)");
    measure_load(url.clone(), None, "warm-up (discarded)");

    // Interleaved, not blocked, so a monotonic warm-up drift over the whole
    // run can't systematically favor one arm.
    for i in 0..4 {
        measure_load(
            url.clone(),
            None,
            &format!("start OMITTED (fresh, no option) [run {}/4]", i + 1),
        );
        measure_load(
            url.clone(),
            Some(0.0),
            &format!("start=+0 (option PRESENT, same target) [run {}/4]", i + 1),
        );
    }
}

// --- moov-at-tail vs. faststart MP4 ----------------------------------------
//
// An MP4 muxed without `-movflags +faststart` has `moov` at the end, so an
// open needs a second ranged request for the index before mpv can report
// streams. Fixtures (not checked in):
//   - `95-h264-long-aac-tail.mp4`: 240s, moov at the END.
//   - `96-h264-long-aac-faststart.mp4`: same encode, moov right after `ftyp`.
//   - `97-h264-episode-tail.mp4` / `98-h264-episode-faststart.mp4`: the
//     same pair at `duration=2400`, for a ~10x larger moov.
//
// ```text
// ffmpeg -y -f lavfi -i "testsrc2=size=1280x720:rate=24:duration=240" \
//     -f lavfi -i "sine=frequency=440:duration=240" \
//     -c:v libx264 -preset ultrafast -b:v 8000k -maxrate 8000k -bufsize 4000k \
//     -pix_fmt yuv420p -c:a aac -b:a 96k -shortest \
//     dev/media/Movies/95-h264-long-aac-tail.mp4
// # add -movflags +faststart for 96; swap duration=2400 for 97/98
// curl -X POST http://localhost:8096/Library/Refresh -H "Authorization: MediaBrowser Token=\"$TOKEN\""
// ```
//
// Verify moov placement with a top-level atom scan (near the end of `95`,
// right after `ftyp` in `96`):
//
// ```text
// python3 -c '
// import struct
// with open("dev/media/Movies/95-h264-long-aac-tail.mp4","rb") as f:
//     off = 0
//     while True:
//         hdr = f.read(8)
//         if len(hdr) < 8: break
//         size, typ = struct.unpack(">I4s", hdr)
//         print(off, size, typ)
//         f.seek(off + size); off += size
// '
// ```
//
// Relay profile: ~118ms RTT, 16Mbps (above the 8Mbps encode, so round
// trips, not bandwidth, are what is measured).

#[ignore = "requires a live Jellyfin server at localhost:8096 AND the latency relay on 127.0.0.1:8097, \
            plus the 240s '95-h264-long-aac-tail.mp4'/'96-h264-long-aac-faststart.mp4' fixtures \
            (see this section's module docs)"]
#[test]
fn bench_moov_tail_vs_faststart_mp4_proxied() {
    let rt = tokio_rt();
    let tail_url = rt.block_on(async {
        let client = authenticated_client().await;
        proxied_stream_url(&client, "95-h264-long-aac-tail.mp4").await
    });
    let Some(tail_url) = tail_url else {
        eprintln!(
            "[moov-bench] skipping: '95-h264-long-aac-tail.mp4' not found on the live server -- \
             generate it (see this section's module docs) and POST /Library/Refresh"
        );
        return;
    };
    let faststart_url = rt.block_on(async {
        let client = authenticated_client().await;
        proxied_stream_url(&client, "96-h264-long-aac-faststart.mp4").await
    });
    let Some(faststart_url) = faststart_url else {
        eprintln!(
            "[moov-bench] skipping: '96-h264-long-aac-faststart.mp4' not found on the live \
             server -- generate it (see this section's module docs) and POST /Library/Refresh"
        );
        return;
    };

    println!("[moov-bench] === 95-h264-long-aac-tail.mp4 (moov at END, fresh open) ===");
    measure_n(&tail_url, None, "moov-tail (95, fresh open)", 3);
    println!("[moov-bench] === 96-h264-long-aac-faststart.mp4 (moov at FRONT, fresh open) ===");
    measure_n(&faststart_url, None, "faststart (96, fresh open)", 3);
}

/// Same comparison, no proxy: separates round-trip cost from CPU-bound
/// parsing cost.
#[ignore = "requires a live Jellyfin server at localhost:8096 (no proxy needed), \
            plus the 240s '95-h264-long-aac-tail.mp4'/'96-h264-long-aac-faststart.mp4' fixtures \
            (see this section's module docs)"]
#[test]
fn bench_moov_tail_vs_faststart_mp4_direct() {
    let rt = tokio_rt();
    let tail_url = rt.block_on(async {
        let client = authenticated_client().await;
        direct_stream_url(&client, "95-h264-long-aac-tail.mp4").await
    });
    let Some(tail_url) = tail_url else {
        eprintln!(
            "[moov-bench] skipping: '95-h264-long-aac-tail.mp4' not found on the live server"
        );
        return;
    };
    let faststart_url = rt.block_on(async {
        let client = authenticated_client().await;
        direct_stream_url(&client, "96-h264-long-aac-faststart.mp4").await
    });
    let Some(faststart_url) = faststart_url else {
        eprintln!(
            "[moov-bench] skipping: '96-h264-long-aac-faststart.mp4' not found on the live server"
        );
        return;
    };

    println!("[moov-bench-direct] === 95-h264-long-aac-tail.mp4 (moov at END, fresh open) ===");
    measure_n(&tail_url, None, "moov-tail (95, fresh open, direct)", 3);
    println!(
        "[moov-bench-direct] === 96-h264-long-aac-faststart.mp4 (moov at FRONT, fresh open) ==="
    );
    measure_n(
        &faststart_url,
        None,
        "faststart (96, fresh open, direct)",
        3,
    );
}

/// Same comparison as `bench_moov_tail_vs_faststart_mp4_proxied` on the
/// 2400s pair (97/98, ~1.4MB moov vs. ~140KB): does the tail tax scale with
/// moov size?
#[ignore = "requires a live Jellyfin server at localhost:8096 AND the latency relay on 127.0.0.1:8097, \
            plus the 2400s '97-h264-episode-tail.mp4'/'98-h264-episode-faststart.mp4' fixtures \
            (see this section's module docs; same recipe as 95/96 with duration=2400)"]
#[test]
fn bench_moov_tail_vs_faststart_mp4_proxied_episode_length() {
    let rt = tokio_rt();
    let tail_url = rt.block_on(async {
        let client = authenticated_client().await;
        proxied_stream_url(&client, "97-h264-episode-tail.mp4").await
    });
    let Some(tail_url) = tail_url else {
        eprintln!(
            "[moov-bench] skipping: '97-h264-episode-tail.mp4' not found on the live server -- \
             generate it (see this section's module docs) and POST /Library/Refresh"
        );
        return;
    };
    let faststart_url = rt.block_on(async {
        let client = authenticated_client().await;
        proxied_stream_url(&client, "98-h264-episode-faststart.mp4").await
    });
    let Some(faststart_url) = faststart_url else {
        eprintln!(
            "[moov-bench] skipping: '98-h264-episode-faststart.mp4' not found on the live \
             server -- generate it (see this section's module docs) and POST /Library/Refresh"
        );
        return;
    };

    println!("[moov-bench] === 97-h264-episode-tail.mp4 (2400s, moov at END, fresh open) ===");
    measure_n(&tail_url, None, "moov-tail (97, 2400s, fresh open)", 3);
    println!(
        "[moov-bench] === 98-h264-episode-faststart.mp4 (2400s, moov at FRONT, fresh open) ==="
    );
    measure_n(&faststart_url, None, "faststart (98, 2400s, fresh open)", 3);
}

/// Moov placement should cost a resume the same as a fresh open, since
/// both need the index before seeking.
#[ignore = "requires a live Jellyfin server at localhost:8096 AND the latency relay on 127.0.0.1:8097, \
            plus the 240s '95-h264-long-aac-tail.mp4' fixture (see this section's module docs)"]
#[test]
fn bench_moov_tail_mp4_fresh_vs_resume_proxied() {
    compare_fresh_vs_resume_proxied("95-h264-long-aac-tail.mp4", 120.0, 3);
}

#[ignore = "requires a live Jellyfin server at localhost:8096 AND the latency relay on 127.0.0.1:8097, \
            plus the 240s '96-h264-long-aac-faststart.mp4' fixture (see this section's module docs)"]
#[test]
fn bench_faststart_mp4_fresh_vs_resume_proxied() {
    compare_fresh_vs_resume_proxied("96-h264-long-aac-faststart.mp4", 120.0, 3);
}

// --- click -> first frame: cold vs. preload/promote ------------------------
//
// Models the app's whole click pipeline, with `PlaybackInfo` also sent
// through the relay: click -> PlaybackInfo -> stream_url -> Player::load ->
// first new frame. Relay profile: ~118ms RTT, 12Mbps (~1.5MB/s).
//
//  - `bench_click_to_first_frame_cold`: no preload.
//  - `bench_click_to_first_frame_preload_promote`: PlaybackInfo prefetched
//    (untimed), stream opened paused with a capped readahead N ms before the
//    click, then promoted (unpause + full readahead). Click -> motion is
//    what the user feels.

/// Relay-facing control-plane base: the click benches send PlaybackInfo
/// through the relay too, since the app pays the link's RTT for it.
const PROXY_BASE_URL: &str = "http://127.0.0.1:8097";

/// Preload's capped readahead (seconds); mirrors the app-side value.
const PRELOAD_READAHEAD_SECS: u32 = 15;

struct ColdClickRun {
    playback_info: Duration,
    loaded: Duration,
    first_frame: Duration,
    first_position: Duration,
}

/// One cold-click run against whatever server `client` points at (the
/// relay, or a real remote server for
/// `bench_real_server_cold_vs_promote`): times PlaybackInfo -> load -> first
/// frame. The `Player` is built before the clock starts, as the app's
/// is long-lived.
fn cold_click_run(
    rt: &tokio::runtime::Runtime,
    client: &JellyfinClient,
    item_id: &str,
    start_secs: Option<f64>,
) -> ColdClickRun {
    let _gl = common::GlContext::new_current().expect("headless CGL context");
    let fbo = common::Fbo::new(320, 240).expect("offscreen FBO");
    let player = Player::new(common::gl_get_proc_address).expect("Player::new");
    let events = common::collect_events(&player);

    let t0 = Instant::now();
    let profile = jellyfin_core::build_device_profile(None);
    let info = rt
        .block_on(client.get_playback_info(item_id, &profile, None))
        .expect("PlaybackInfo");
    let playback_info = t0.elapsed();

    let mut source = info
        .media_sources
        .into_iter()
        .next()
        .unwrap_or_else(|| panic!("PlaybackInfo for '{item_id}' returned no MediaSources"));
    source.transcoding_url = None;
    let url = client.stream_url(item_id, &source);

    player
        .load(LoadRequest {
            start_secs,
            ..simple_load(url)
        })
        .expect("load() should be accepted");

    let deadline = t0 + Duration::from_secs(60);
    let mut loaded = None;
    let mut first_frame = None;
    let mut first_position = None;
    while Instant::now() < deadline
        && (loaded.is_none() || first_frame.is_none() || first_position.is_none())
    {
        if player.needs_render() {
            if first_frame.is_none() {
                first_frame = Some(t0.elapsed());
            }
            let _ = player.render(fbo.fbo as i32, fbo.width, fbo.height);
            player.report_swap();
        }
        while let Ok(ev) = events.try_recv() {
            // Real-world media can produce benign error-level demuxer logs
            // (e.g. "Referenced QT chapter track not found" on some MP4s)
            // that mpv recovers from and the app itself only logs -- these
            // benches measure latency on real files, so mirror the app.
            if let PlayerEvent::Error(msg) = &ev {
                eprintln!("[click-bench] mpv error event (continuing): {msg}");
            }
            match ev {
                PlayerEvent::Loaded { .. } if loaded.is_none() => loaded = Some(t0.elapsed()),
                PlayerEvent::Position { .. } if first_position.is_none() => {
                    first_position = Some(t0.elapsed())
                }
                _ => {}
            }
        }
        std::thread::sleep(Duration::from_millis(2));
    }

    ColdClickRun {
        playback_info,
        loaded: loaded.expect("Loaded within 60s"),
        first_frame: first_frame.expect("a first frame within 60s"),
        first_position: first_position.expect("a Position within 60s"),
    }
}

/// One preload/promote run against whatever server `client` points at:
/// PlaybackInfo is done UNTIMED (models the app's detail-open prefetch),
/// the stream is opened `pause=yes` with the capped readahead, the harness
/// pumps render/events for `head_start_ms`, then "clicks": unpause +
/// restore full readahead, and times click->first NEW frame and
/// click->position actually advancing.
fn preload_promote_run(
    rt: &tokio::runtime::Runtime,
    client: &JellyfinClient,
    item_id: &str,
    start_secs: Option<f64>,
    head_start_ms: u64,
) -> (Duration, Duration, bool) {
    let profile = jellyfin_core::build_device_profile(None);
    let info = rt
        .block_on(client.get_playback_info(item_id, &profile, None))
        .expect("PlaybackInfo");
    let mut source = info
        .media_sources
        .into_iter()
        .next()
        .unwrap_or_else(|| panic!("PlaybackInfo for '{item_id}' returned no MediaSources"));
    source.transcoding_url = None;
    let url = client.stream_url(item_id, &source);

    let _gl = common::GlContext::new_current().expect("headless CGL context");
    let fbo = common::Fbo::new(320, 240).expect("offscreen FBO");
    let player = Player::new(common::gl_get_proc_address).expect("Player::new");
    let events = common::collect_events(&player);

    player
        .load(LoadRequest {
            start_secs,
            start_paused: true,
            readahead_secs: Some(PRELOAD_READAHEAD_SECS),
            ..simple_load(url)
        })
        .expect("load() should be accepted");

    // Dark phase: pump render + drain events for the head start, tracking
    // whether the paused first frame ever got decoded/rendered (it should --
    // mpv shows the first frame of a paused load).
    let dark_deadline = Instant::now() + Duration::from_millis(head_start_ms);
    let mut preload_frame_seen = false;
    while Instant::now() < dark_deadline {
        if player.needs_render() {
            preload_frame_seen = true;
            let _ = player.render(fbo.fbo as i32, fbo.width, fbo.height);
            player.report_swap();
        }
        while let Ok(ev) = events.try_recv() {
            if let PlayerEvent::Error(msg) = &ev {
                eprintln!("[click-bench] mpv error event during preload (continuing): {msg}");
            }
        }
        std::thread::sleep(Duration::from_millis(2));
    }

    // The click. Drain any pending render flag first so a stale paused-frame
    // update can't fake a ~0ms result.
    if player.needs_render() {
        let _ = player.render(fbo.fbo as i32, fbo.width, fbo.height);
        player.report_swap();
    }
    while events.try_recv().is_ok() {}
    let baseline_pos = player.position_secs().unwrap_or(start_secs.unwrap_or(0.0));

    let t0 = Instant::now();
    player.set_paused(false).expect("unpause");
    player
        .set_readahead_secs(60)
        .expect("restore full readahead");

    let deadline = t0 + Duration::from_secs(60);
    let mut first_frame = None;
    let mut pos_advance = None;
    while Instant::now() < deadline && (first_frame.is_none() || pos_advance.is_none()) {
        if player.needs_render() {
            if first_frame.is_none() {
                first_frame = Some(t0.elapsed());
            }
            let _ = player.render(fbo.fbo as i32, fbo.width, fbo.height);
            player.report_swap();
        }
        while let Ok(ev) = events.try_recv() {
            if let PlayerEvent::Error(msg) = &ev {
                eprintln!("[click-bench] mpv error event after click (continuing): {msg}");
            }
            if let PlayerEvent::Position { secs } = ev {
                if pos_advance.is_none() && secs > baseline_pos + 0.01 {
                    pos_advance = Some(t0.elapsed());
                }
            }
        }
        std::thread::sleep(Duration::from_millis(2));
    }

    (
        first_frame.expect("a post-click frame within 60s"),
        pos_advance.expect("position advance within 60s"),
        preload_frame_seen,
    )
}

/// Resolves the relay-facing client + fixture item id for the click
/// benches: auth + item lookup go direct (untimed either way), the returned
/// client's own base URL is the proxy so both PlaybackInfo and the media
/// stream pay the simulated link.
fn relay_client(rt: &tokio::runtime::Runtime, filename: &str) -> Option<(JellyfinClient, String)> {
    let direct = rt.block_on(authenticated_client());
    let item_id = rt.block_on(find_item_id_by_filename(&direct, filename))?;
    let proxied = rt.block_on(async {
        let (client, _auth) =
            JellyfinClient::authenticate_by_name(PROXY_BASE_URL, identity(), USERNAME, PASSWORD)
                .await
                .expect("authenticate through the proxy (is the latency relay running?)");
        client
    });
    Some((proxied, item_id))
}

const CLICK_BENCH_FILES: &[(&str, &str)] = &[
    ("94-h264-long-aac.mkv", "mkv/indexed"),
    ("95-h264-long-aac-tail.mp4", "mp4/moov-tail"),
    ("96-h264-long-aac-faststart.mp4", "mp4/faststart"),
];

#[ignore = "requires a live Jellyfin server at localhost:8096 AND the latency relay on 127.0.0.1:8097 \
            at ~118ms RTT / 12Mbps, plus fixtures 94/95/96"]
#[test]
fn bench_click_to_first_frame_cold() {
    // One runtime for the whole test: reqwest's pooled connections are
    // driven by the runtime they were established on -- using the same
    // client from a second runtime leaves the pooled connection undriven
    // and every follow-up request times out (localhost hides this, because
    // a dropped runtime closes its sockets and forces clean reconnects).
    let rt = tokio_rt();
    for (file, label) in CLICK_BENCH_FILES {
        let Some((client, item_id)) = relay_client(&rt, file) else {
            eprintln!("[click-bench] skipping {file}: not found on the live server");
            continue;
        };
        for run in 1..=3 {
            let c = cold_click_run(&rt, &client, &item_id, None);
            println!(
                "[click-bench] COLD {label} run{run}: playback_info={:?} loaded={:?} first_frame={:?} first_position={:?}",
                c.playback_info, c.loaded, c.first_frame, c.first_position
            );
        }
    }
}

#[ignore = "requires a live Jellyfin server at localhost:8096 AND the latency relay on 127.0.0.1:8097 \
            at ~118ms RTT / 12Mbps, plus fixtures 94/95/96"]
#[test]
fn bench_click_to_first_frame_preload_promote() {
    // Same single-runtime rule as `bench_click_to_first_frame_cold`.
    let rt = tokio_rt();
    for (file, label) in CLICK_BENCH_FILES {
        let Some((client, item_id)) = relay_client(&rt, file) else {
            eprintln!("[click-bench] skipping {file}: not found on the live server");
            continue;
        };
        for head_start_ms in [0u64, 500, 1500, 3000] {
            let (frame, pos, preload_frame_seen) =
                preload_promote_run(&rt, &client, &item_id, None, head_start_ms);
            println!(
                "[click-bench] WARM {label} head_start={head_start_ms}ms: click->frame={frame:?} \
                 click->pos_advance={pos:?} (frame decoded during preload: {preload_frame_seen})"
            );
        }
    }
}

/// The real-world confirmation: cold vs promote against a REAL Jellyfin
/// server over its real link (no proxy, no synthetic fixtures -- whatever
/// MKV and MP4 the library actually holds). Credentials come from the
/// environment so nothing server-specific is committed:
///
/// ```text
/// JELLYBEAM_BENCH_URL=http://<server>:8096 JELLYBEAM_BENCH_TOKEN=<token> \
///   cargo test -p player --test latency_bench -- --ignored --nocapture \
///   bench_real_server_cold_vs_promote
/// ```
///
/// Optional `JELLYBEAM_BENCH_MATCH_MKV` / `JELLYBEAM_BENCH_MATCH_MP4` narrow the
/// item choice to a path substring (e.g. to pick a small file on a slow
/// link). Prints item ids, never media paths, so transcripts stay clean.
#[ignore = "hits a REAL Jellyfin server over its real link -- set JELLYBEAM_BENCH_URL + JELLYBEAM_BENCH_TOKEN"]
#[test]
fn bench_real_server_cold_vs_promote() {
    let (Ok(base), Ok(token)) = (
        std::env::var("JELLYBEAM_BENCH_URL"),
        std::env::var("JELLYBEAM_BENCH_TOKEN"),
    ) else {
        eprintln!("[click-bench-real] skipping: JELLYBEAM_BENCH_URL / JELLYBEAM_BENCH_TOKEN unset");
        return;
    };
    let rt = tokio_rt();
    // `with_user_id` is not optional decoration: PlaybackInfo POSTs without
    // user context stall (>30s, request-timeout) on at least one real
    // Jellyfin deployment, while the identical request with a UserId
    // answers in ~100ms (see jellyfin-core/examples/pbinfo_probe.rs; the
    // app's own clients always carry one too).
    let user_id = std::env::var("JELLYBEAM_BENCH_USER_ID").ok();
    let mut client = JellyfinClient::from_token(&base, identity(), &token);
    if let Some(user_id) = &user_id {
        client = client.with_user_id(user_id);
    }

    let mut picks: Vec<(&str, String)> = Vec::new();
    for (ext, env_override) in [
        (".mkv", "JELLYBEAM_BENCH_MATCH_MKV"),
        (".mp4", "JELLYBEAM_BENCH_MATCH_MP4"),
    ] {
        let want = std::env::var(env_override).ok();
        let found = rt.block_on(async {
            for kind in ["Movie", "Episode"] {
                let result = client
                    .get_items(&ItemQuery {
                        parent_id: None,
                        include_item_types: vec![kind.to_string()],
                        recursive: true,
                        sort_by: None,
                        sort_order: None,
                        fields: vec!["Path".to_string()],
                        start_index: 0,
                        limit: 300,
                        ids: Vec::new(),
                        is_missing: None,
                        min_date_last_saved: None,
                        ..ItemQuery::new()
                    })
                    .await
                    .expect("GET /Items from the real server");
                let hit = result.items.into_iter().find(|i| {
                    i.path.as_deref().is_some_and(|p| {
                        p.to_ascii_lowercase().ends_with(ext)
                            && want.as_deref().is_none_or(|w| p.contains(w))
                    })
                });
                if let Some(item) = hit {
                    return item.id.map(|id| id.to_string());
                }
            }
            None
        });
        match found {
            Some(id) => picks.push((ext, id)),
            None => {
                eprintln!("[click-bench-real] no {ext} item found in the first 300/type; skipping")
            }
        }
    }

    for (label, item_id) in picks {
        for run in 1..=2 {
            let c = cold_click_run(&rt, &client, &item_id, None);
            println!(
                "[click-bench-real] COLD {label} run{run}: playback_info={:?} loaded={:?} first_position={:?}",
                c.playback_info, c.loaded, c.first_position
            );
        }
        // Two head starts: a fast click (1.5s) and a realistic detail-page
        // dwell (8s). A heavy open (moov-at-tail MP4 over a slow link) can
        // exceed the short head start -- the promote then simply finishes
        // the open, i.e. it degrades to cold-minus-PlaybackInfo, never
        // worse; the long dwell shows the fully-warm behavior.
        for head_start_ms in [1500u64, 8000] {
            for run in 1..=2 {
                let (frame, pos, preload_frame_seen) =
                    preload_promote_run(&rt, &client, &item_id, None, head_start_ms);
                println!(
                    "[click-bench-real] WARM {label} run{run} head_start={head_start_ms}ms: \
                     click->frame={frame:?} click->pos_advance={pos:?} \
                     (frame decoded during preload: {preload_frame_seen})"
                );
            }
        }
    }
}

#[cfg(test)]
mod unit_tests {
    use super::{resolve_relative, rewrite_authority};

    #[test]
    fn rewrite_authority_swaps_scheme_and_host_keeps_path_and_query() {
        let out = rewrite_authority(
            "http://localhost:8096/Videos/abc/stream?static=true&ApiKey=tok",
            "127.0.0.1:8097",
        );
        assert_eq!(
            out,
            "http://127.0.0.1:8097/Videos/abc/stream?static=true&ApiKey=tok"
        );
    }

    #[test]
    fn resolve_relative_joins_against_the_referencing_playlists_own_directory() {
        // Real shape: master.m3u8's own body names "main.m3u8?...", which
        // must resolve alongside master.m3u8 (same directory), NOT relative
        // to some fixed `/videos/` root -- the bug this test guards against
        // joined every relative playlist entry straight onto the server
        // root, producing a 404.
        let out = resolve_relative(
            "http://localhost:8096/videos/abc-123/master.m3u8?ApiKey=tok",
            "main.m3u8?ApiKey=tok&Foo=1",
        );
        assert_eq!(
            out,
            "http://localhost:8096/videos/abc-123/main.m3u8?ApiKey=tok&Foo=1"
        );
    }

    #[test]
    fn resolve_relative_handles_a_nested_relative_segment_path() {
        let out = resolve_relative(
            "http://localhost:8096/videos/abc-123/main.m3u8?ApiKey=tok",
            "hls1/main/0.ts?ApiKey=tok",
        );
        assert_eq!(
            out,
            "http://localhost:8096/videos/abc-123/hls1/main/0.ts?ApiKey=tok"
        );
    }

    #[test]
    fn resolve_relative_passes_through_an_absolute_url_unchanged() {
        let out = resolve_relative(
            "http://localhost:8096/videos/abc-123/master.m3u8",
            "https://cdn.example.com/seg0.ts",
        );
        assert_eq!(out, "https://cdn.example.com/seg0.ts");
    }
}
