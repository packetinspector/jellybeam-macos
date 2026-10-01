//! Playback flow: PlaybackInfo -> decide_playback -> Player::load ->
//! ReportingSession::start, run entirely on the tokio runtime (never on
//! GPUI's main thread -- see `session.rs` for the same pattern).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use jellyfin_api::JellyfinClient;
use jellyfin_core::{PlaybackDecision, ReportContext, ReportingSession};
use media_cache::Mirror;
use player::{LoadRequest, Player};

/// Wall-clock checkpoints for `play_item entry -> PlaybackInfo response ->
/// decide -> Player::load`, measured from `t0`. The remaining checkpoints
/// (mpv `FileLoaded`/first `Position`) happen on mpv's event thread, bridged
/// via `main.rs::spawn_player_events_task`; `t0` rides along in
/// `PlaybackStarted` for `root.rs::MainState::pending_latency` to finish the
/// timeline.
#[derive(Debug, Clone, Copy)]
pub(crate) struct LoadLatency {
    pub t0: Instant,
    pub playback_info_ms: u64,
    pub decide_ms: u64,
    pub pre_load_ms: u64,
}

/// Load pipeline progress, pushed to `root.rs` via `start_playback`'s
/// `stage_tx` watch channel for the loading overlay. Only these two stages:
/// everything after `player.load()` is covered by `PlayerUiState::loading`/
/// `render_playing`'s "Buffering..." overlay on mpv's own event thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LoadStage {
    /// `run()`'s very first stage: `GET /PlaybackInfo` in flight.
    ContactingServer,
    /// `PlaybackInfo` responded and `decide_playback` resolved a URL;
    /// `player.load()` is about to fire (or just did) -- mpv is opening the
    /// stream.
    OpeningStream,
}

impl LoadStage {
    pub(crate) fn label(self) -> &'static str {
        match self {
            LoadStage::ContactingServer => "Contacting server…",
            LoadStage::OpeningStream => "Opening stream…",
        }
    }
}

pub(crate) struct PlaybackStarted {
    pub reporting: ReportingSession,
    pub item_id: String,
    /// Kept for future UI (e.g. a Detail-page "media source" picker for
    /// multi-version items) -- read by the trickplay tile URL builder too.
    pub media_source_id: String,
    /// Human-readable "Direct Play" / "Transcode (reasons...)" summary for
    /// the now-playing bar and E2E log assertions.
    pub decision_summary: String,
    /// docs/UX-SPEC.md §6: "never silent" -- the info overlay (I key) shows these
    /// verbatim next to "Transcode" instead of just the summary string.
    pub is_direct_play: bool,
    pub decision_reasons: Vec<String>,
    pub container: Option<String>,
    /// The exact `MediaSourceInfo` `decide_playback` chose -- what
    /// `ui::spec_strip` renders, since a multi-version item's first source
    /// need not be the file actually playing.
    pub media_source: jellyfin_api::models::MediaSourceInfo,
    /// `(start_secs, title)` per chapter, server order (already ascending);
    /// not re-sorted defensively -- a malformed response just shows ticks
    /// out of order, not a crash.
    pub chapters: Vec<(f64, String)>,
    pub trickplay: Option<crate::trickplay::TrickplayMeta>,
    /// Handle for the "log hwdec_current ~5s in" task below -- caller must
    /// abort on stop/supersede, or a stale log line fires 5s after the fact.
    pub hwdec_log_handle: tokio::task::JoinHandle<()>,
    /// Handle for the periodic ~30s playback diagnostics log below
    /// (position/cache/frame drops). Same abort-on-stop-or-supersede
    /// contract as `hwdec_log_handle`.
    pub diag_log_handle: tokio::task::JoinHandle<()>,
    /// Streaming bitrate cap this session's `PlaybackInfo` request was built
    /// with; `None` means Auto (no cap). Surfaced verbatim by the info
    /// overlay rather than folded into `decision_summary`.
    pub max_bitrate: Option<u32>,
    /// Latency instrumentation -- see `LoadLatency`'s doc comment.
    pub load_latency: LoadLatency,
}

/// Returned instead of calling `player.load()` when a newer play (or a
/// stop) has bumped `playback_generation` while awaiting the server.
/// `handle_playback_outcome` discards a superseded outcome anyway -- the
/// string is only ever a log line; the point is mpv is never touched for an
/// item the user has already left.
pub(crate) const SUPERSEDED: &str = "playback flow superseded before load";

/// Fire-and-forget: spawns the PlaybackInfo -> load -> report-start flow on
/// `runtime`, delivering the outcome through `result_tx` (bridged back into
/// GPUI via `cx.spawn` in `root.rs`). `bitrate_mode` is the server's quality
/// mode -- see `settings.rs::BitrateMode`.
///
/// The returned `JoinHandle` must be stored (`MainState::playback_task`) and
/// `.abort()`ed on the next play/stop/quit, like `hwdec_log_handle`/
/// `diag_log_handle` -- otherwise a superseded flow keeps running and can
/// call `player.load()` on the shared player after the user has moved on.
#[allow(clippy::too_many_arguments)] // plain data parameters, no natural grouping.
pub(crate) fn start_playback(
    runtime: &tokio::runtime::Runtime,
    client: JellyfinClient,
    mirror: Mirror,
    player: Arc<Player>,
    item_id: String,
    item_name: String,
    bitrate_mode: crate::settings::BitrateMode,
    // Suppress this item's stored resume position for this one start
    // (the Detail page's "Start from beginning" control). Purely a read-side
    // skip -- see `run`'s own `resume_ticks`.
    from_beginning: bool,
    // docs/PLUGIN-CHANNELS.md §2.1/§2.3: `run`'s mirror
    // lookup is always `None` for a channel/plugin recording (never
    // mirrored). `root_playback.rs::play_item_with_resume_hint` passes the position
    // read off the recording's live-listing DTO here instead; other callers
    // pass `None`, falling through to the mirror lookup.
    resume_ticks_hint: Option<i64>,
    stage_tx: tokio::sync::watch::Sender<LoadStage>,
    result_tx: tokio::sync::oneshot::Sender<Result<PlaybackStarted, String>>,
    playback_generation: Arc<AtomicU64>,
    my_generation: u64,
) -> tokio::task::JoinHandle<()> {
    runtime.spawn(async move {
        let outcome = run(
            client,
            mirror,
            player,
            item_id,
            item_name,
            bitrate_mode,
            from_beginning,
            resume_ticks_hint,
            stage_tx,
            playback_generation,
            my_generation,
        )
        .await;
        // The receiver is gone (the GPUI task awaiting this outcome was
        // dropped -- window teardown/app exit). `send` hands the value
        // back, and simply dropping it would drop a live `ReportingSession`
        // with it: no `stop()`, no `abandon()`, just the `Drop` warning and
        // a `Sessions/Playing` record dangling on the server. Nobody can
        // ever make this session active now, which is exactly
        // `handle_playback_outcome`'s superseded case -- so end it the same
        // way it does, and abort the two diagnostic tasks that would
        // otherwise outlive their session.
        if let Err(Ok(started)) = result_tx.send(outcome) {
            tracing::warn!(
                item_id = %started.item_id,
                "playback outcome had nowhere to go (receiver dropped); discarding session"
            );
            started.hwdec_log_handle.abort();
            started.diag_log_handle.abort();
            started.reporting.abandon();
        }
    })
}

/// `run()`'s pre-load gate, extracted so the supersede decision is
/// unit-testable without a live server/player. Returns `Err(SUPERSEDED)`
/// without invoking `load` when `playback_generation` was bumped past
/// `my_generation` (a newer play, or a stop) while awaiting the server;
/// otherwise runs `load` and propagates its result.
fn load_if_current<F>(
    playback_generation: &AtomicU64,
    my_generation: u64,
    load: F,
) -> Result<(), String>
where
    F: FnOnce() -> Result<(), String>,
{
    if playback_generation.load(Ordering::Relaxed) != my_generation {
        return Err(SUPERSEDED.to_string());
    }
    load()
}

#[allow(clippy::too_many_arguments)]
async fn run(
    client: JellyfinClient,
    mirror: Mirror,
    player: Arc<Player>,
    item_id: String,
    item_name: String,
    bitrate_mode: crate::settings::BitrateMode,
    from_beginning: bool,
    resume_ticks_hint: Option<i64>,
    stage_tx: tokio::sync::watch::Sender<LoadStage>,
    playback_generation: Arc<AtomicU64>,
    my_generation: u64,
) -> Result<PlaybackStarted, String> {
    let t0 = Instant::now();

    // Resume position: `resume_ticks_hint` first (see `start_playback`'s doc
    // comment), falling back to the mirror's cached user_data for ordinary
    // items.
    // `from_beginning` skips the read entirely -- the stored
    // position is untouched, so "Start from beginning" is a one-shot choice,
    // not a destructive reset.
    let resume_ticks = if from_beginning {
        None
    } else {
        resume_ticks_hint.filter(|ticks| *ticks > 0).or_else(|| {
            mirror
                .item(&item_id)
                .and_then(|dto| dto.user_data)
                .and_then(|ud| ud.playback_position_ticks)
                .filter(|ticks| *ticks > 0)
        })
    };

    // Quality mode (user decision): `DirectPlay` (the default)
    // sends NO cap and never measures -- the server never transcodes on
    // this client's account, full stop, even on a link slower than the
    // media (the user prefers full quality + manual control over
    // auto-switching). `Measured` is the opt-in adaptive
    // behavior: measure once per server per app run, cap at 80%, server
    // transcodes only what exceeds the link. `Cap` is a fixed ceiling.
    let effective_bitrate = match bitrate_mode {
        crate::settings::BitrateMode::DirectPlay => None,
        crate::settings::BitrateMode::Measured => auto_bitrate_cap(&client).await,
        crate::settings::BitrateMode::Cap(cap) => Some(cap),
    };
    let profile = jellyfin_core::build_device_profile(effective_bitrate);
    let info = client
        .get_playback_info(&item_id, &profile, resume_ticks)
        .await
        .map_err(|e| e.to_string())?;
    let playback_info_ms = t0.elapsed().as_millis() as u64;
    // `PlaybackInfo` landed -- tell the UI we've moved past
    // "Contacting server..." before doing the (cheap, synchronous)
    // decide_playback + starting player.load() below. A closed receiver
    // (root.rs no longer cares, e.g. this play was already superseded) is
    // not an error here -- `send` failing just means nobody's listening.
    let _ = stage_tx.send(LoadStage::OpeningStream);
    let decision =
        jellyfin_core::decide_playback(&client, &item_id, &info).map_err(|e| e.to_string())?;
    let decide_ms = t0.elapsed().as_millis() as u64;

    // `JellyfinClient::stream_url` (used by `decide_playback` to build both
    // the direct-play and transcode URLs) already embeds the access token
    // as an `ApiKey` query param -- see its doc comment: "no Authorization
    // header involved". `LoadRequest::http_headers` therefore carries no
    // auth; its only use is the explicit `Host` below when the cached-IP
    // rewrite fires.
    let (source, url, decision_summary) = match &decision {
        PlaybackDecision::DirectPlay { source, url } => {
            tracing::info!(item = %item_name, url = %crate::redact::redact_url(url), "playback decision: DirectPlay");
            (source.clone(), url.clone(), "Direct Play".to_string())
        }
        PlaybackDecision::Transcode {
            source,
            hls_url,
            reasons,
        } => {
            tracing::info!(item = %item_name, url = %crate::redact::redact_url(hls_url), ?reasons, "playback decision: Transcode");
            (
                source.clone(),
                hls_url.clone(),
                format!("Transcode ({})", reasons.join("; ")),
            )
        }
    };

    // mpv resolves hostnames with its own getaddrinfo -- the app's reqwest
    // DNS cache can't cover it, and the field-observed intermittent ~5s
    // resolver stall can hit every stream (re)open (an mkv open can reopen
    // the connection mid-header: two stalls were measured inside one 11s
    // playback start). The cache is warm here -- PlaybackInfo just resolved
    // this host -- so hand mpv a literal IP, with the original authority as
    // an explicit Host header so a name-based reverse proxy still routes
    // it. Plain-http only; see `rewrite_http_host_to_cached_ip`'s docs.
    let target = jellyfin_api::dns::rewrite_http_host_to_cached_ip(&url);
    let url = target.url;
    let stream_headers: Vec<(String, String)> = target
        .host_header
        .map(|host| vec![("Host".to_string(), host)])
        .unwrap_or_default();
    let media_source_id = source.id.clone().unwrap_or_default();
    let play_session_id = info.play_session_id.clone().unwrap_or_default();
    let start_secs = resume_ticks.map(|ticks| ticks as f64 / 10_000_000.0);
    let is_direct_play = matches!(decision, PlaybackDecision::DirectPlay { .. });
    let decision_reasons = match &decision {
        PlaybackDecision::Transcode { reasons, .. } => reasons.clone(),
        PlaybackDecision::DirectPlay { .. } => Vec::new(),
    };

    // Latency fix: `Player::load` used to be issued *after* the
    // enrichment fetch below (chapters/trickplay/container) -- an entire
    // extra HTTP round trip mpv's own network start had no reason to wait
    // on, since none of that data is needed until the `Loaded` event fires
    // well after mpv has started buffering. Firing `load` here instead
    // means mpv starts fetching/decoding the stream immediately after
    // `decide_playback` resolves the URL, in parallel with (rather than
    // strictly before) the enrichment fetch below.
    //
    // Defense-in-depth: everything above this point was awaited on
    // the network (`auto_bitrate_cap`, `get_playback_info`), so the user may
    // have stopped or started a different item in the meantime. The caller
    // aborts this task on both of those paths, but abort delivery is not
    // instantaneous -- an in-flight task only stops at its next await point,
    // and there is none between here and `player.load()`. Re-read the shared
    // generation immediately before touching the shared `Arc<Player>` so a
    // superseded flow can never reload/hijack mpv behind the user's back.
    match load_if_current(&playback_generation, my_generation, || {
        player
            .load(LoadRequest {
                url,
                http_headers: stream_headers,
                start_secs,
                external_subs: Vec::new(),
                start_paused: false,
                readahead_secs: None,
                // Normal playback is never byte-capped -- only a
                // dark preload (`run_preload` below) passes
                // `Some(PRELOAD_MAX_BYTES)`. `None` leaves `INIT_OPTIONS`'
                // global `demuxer-max-bytes=150MiB` in effect.
                max_bytes: None,
            })
            .map_err(|e| e.to_string())
    }) {
        Ok(()) => {}
        Err(e) if e == SUPERSEDED => {
            tracing::info!(item = %item_name, "playback superseded before Player::load; not loading");
            return Err(e);
        }
        Err(e) => return Err(e),
    }
    let pre_load_ms = t0.elapsed().as_millis() as u64;
    let load_latency = LoadLatency {
        t0,
        playback_info_ms,
        decide_ms,
        pre_load_ms,
    };
    tracing::info!(
        item = %item_name,
        playback_info_ms,
        decide_ms,
        pre_load_ms,
        "JELLYBEAM_LATENCY: play_item -> Player::load"
    );

    // OSD needs (chapters, trickplay manifest, container) that the mirror's
    // bulk sync doesn't carry -- same live `Fields=MediaStreams,...` fetch
    // Detail's codec badges use (`detail.rs::fetch_media_streams`'s doc
    // comment), reused here rather than duplicated. Best-effort: a failed
    // fetch just means the OSD shows no chapter ticks/trickplay/container
    // for this session, not a playback failure. Deliberately AFTER
    // `player.load` above (see that call's comment) -- this fetch no longer
    // sits on the critical path to mpv actually starting to buffer/decode.
    let enrichment = crate::detail::fetch_media_streams(client.clone(), item_id.clone()).await;
    let chapters: Vec<(f64, String)> = enrichment
        .as_ref()
        .map(|dto| {
            dto.chapters
                .iter()
                .map(|c| {
                    let secs = c.start_position_ticks.unwrap_or(0) as f64 / 10_000_000.0;
                    (secs, c.name.clone().unwrap_or_default())
                })
                .collect()
        })
        .unwrap_or_default();
    let trickplay = enrichment
        .as_ref()
        .and_then(|dto| crate::trickplay::resolve_trickplay(dto, &item_id, &media_source_id));
    let container = enrichment.as_ref().and_then(|dto| dto.container.clone());

    let hwdec_log_handle = spawn_hwdec_log(&player, &item_name);
    let diag_log_handle = spawn_diag_log(&player, &item_name);

    // Deliberately the LAST thing this fn does, with no `.await` between
    // here and the `Ok(..)` that hands the session off to the caller.
    //
    // This used to sit above the enrichment fetch -- i.e. with a live
    // `ReportingSession` (its actor's `Start` report already on the wire)
    // held across an `await`. `run()` is a `tokio::spawn`ed task that
    // `root.rs` aborts unconditionally the moment a newer play or a stop
    // supersedes this flow (`play_item_inner`/`stop_playback`/
    // `prepare_for_quit` all `playback_task.take().abort()`).
    // An abort landing in that window cancels the future and drops the
    // session where it stands -- no `stop()`, no `abandon()` -- which is
    // precisely the `ReportingSession dropped without calling stop()`
    // warning seen on next-episode auto-advance: EOF fires `play_item` for
    // the next episode, `playing_item_id` is set synchronously, and a stop
    // that lands while this fetch is still in flight kills the task mid-
    // session.
    //
    // With the session created after the final await there is no
    // cancellation point left to lose it in: an abort either happens
    // before it exists at all, or the task runs to completion and the
    // session reaches `handle_playback_outcome`, which owns both endings
    // (install it, or `abandon()` it as superseded).
    let reporting = ReportingSession::start(
        client.clone(),
        ReportContext {
            item_id: item_id.clone(),
            media_source_id: media_source_id.clone(),
            play_session_id,
        },
    );

    Ok(PlaybackStarted {
        reporting,
        item_id,
        media_source_id,
        decision_summary,
        is_direct_play,
        decision_reasons,
        container,
        media_source: source,
        chapters,
        trickplay,
        // The cap the PlaybackInfo request was actually built with --
        // configured, or the Auto-measured link cap (so the info overlay
        // shows the truth, not "no cap" while one was applied).
        max_bitrate: effective_bitrate,
        hwdec_log_handle,
        diag_log_handle,
        load_latency,
    })
}

/// Log hwdec_current ~5s into playback -- long enough for mpv to have
/// actually started decoding real frames (Core Profile contract note on
/// `player::Player::new`: "videotoolbox" here confirms hardware decode
/// actually engaged, not just requested). Factored out of `run()` so
/// the preload-promote path (`root_playback.rs::promote_preload`) spawns the exact
/// same task; both callers own the abort-on-stop-or-supersede contract via
/// `PlaybackStarted::hwdec_log_handle`.
pub(crate) fn spawn_hwdec_log(
    player: &Arc<Player>,
    item_name: &str,
) -> tokio::task::JoinHandle<()> {
    let player = player.clone();
    let item_name = item_name.to_string();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(5)).await;
        match player.hwdec_current() {
            Some(hwdec) => tracing::info!(item = %item_name, hwdec = %hwdec, "hwdec_current"),
            None => tracing::info!(item = %item_name, hwdec = "unknown", "hwdec_current"),
        }
    })
}

/// Feedback item 3: periodic (~30s) playback diagnostics -- position, cache
/// depth in both mpv's own duration guess and real bytes, cache fill
/// throughput, and dropped-frame counts (VO + decoder), all in one INFO
/// line. Before this, jellybeam.log had literally nothing to look at for "it
/// dropped frames" -- the info overlay is live-only, nobody screenshots it
/// mid-stutter. Same "abort on stop/supersede" contract and `tokio::spawn`
/// shape as `spawn_hwdec_log` just above; started immediately (not after an
/// initial delay) since the first tick is itself 30s out, which is already
/// long enough for playback to be well underway. Factored out of `run()`
/// for the same dark-preload promote-path reuse as `spawn_hwdec_log`.
pub(crate) fn spawn_diag_log(player: &Arc<Player>, item_name: &str) -> tokio::task::JoinHandle<()> {
    let player = player.clone();
    let item_name = item_name.to_string();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(30));
        // The first `tick()` fires immediately (tokio::time::interval's
        // documented behavior); skip it so the first real log line lands
        // ~30s in, not at t=0 before playback has even started.
        interval.tick().await;
        loop {
            interval.tick().await;
            let position_secs = player.position_secs();
            let cache = player.cache_state();
            let drops = player.frame_drop_stats();
            tracing::info!(
                item = %item_name,
                position_secs,
                cache_duration_secs = cache.demuxer_cache_duration_secs,
                cache_fw_bytes = cache.fw_bytes,
                cache_speed_bps = cache.cache_speed_bps,
                paused_for_cache = cache.paused_for_cache,
                frame_drops_vo = drops.vo,
                frame_drops_decoder = drops.decoder,
                "JELLYBEAM_PLAYBACK_DIAG"
            );
        }
    })
}

/// A dark preload's capped `demuxer-readahead-secs` -- deep enough
/// that a promote a few seconds after detail-open has real data banked,
/// shallow enough that browsing detail pages doesn't quietly stream whole
/// episodes over a slow link (~15s of an 8Mbps file is ~15MB, fetched once,
/// then the demuxer idles). Promotion restores the player crate's full
/// 60s target ([`player::DEFAULT_READAHEAD_SECS`]).
pub(crate) const PRELOAD_READAHEAD_SECS: u32 = 15;

/// A hard byte ceiling on every dark
/// preload, independent of `PRELOAD_READAHEAD_SECS` above. The problem this
/// solves: a hero preload on a remote ~12-25Mbit link pulled 34.8MB
/// in 18s before being discarded -- `PRELOAD_READAHEAD_SECS`'s 15s target is
/// a TIME cap, and time-times-bitrate scales with the source, so 15s of a
/// high-bitrate concert film/4K remux is tens of MB even though 15s of a
/// modest 720p episode is only a couple. The only byte ceiling that existed
/// before this was the global `demuxer-max-bytes=150MiB`
/// (`player::INIT_OPTIONS`), which a 15s preload never gets remotely close
/// to, so it never actually engaged as a limit.
///
/// ~6MB bounds a preload's cost regardless of bitrate: on a 12Mbit link,
/// that's roughly 4s of fetch, which is exactly deep
/// enough to have the first GOP or two decoded (so a promote's first frame
/// is instant) without ever approaching the tens-of-MB the time-only cap
/// allowed. `demuxer-readahead-secs=15` stays in effect alongside this --
/// whichever of the two limits the demuxer hits first stops it; a slow link
/// hits the byte cap first, a fast link hits the time cap first, and either
/// way the preload can't run away.
pub(crate) const PRELOAD_MAX_BYTES: u64 = 6 * 1024 * 1024;

/// Everything `root.rs` needs to promote a dark preload into a real
/// playback session without any further network round trips -- the cached
/// `PlaybackInfo` decision plus the enrichment `run()` would otherwise
/// fetch after load. Produced by `start_preload`; consumed by
/// `root_playback.rs::promote_preload` (matched click) or simply dropped (any other
/// play/stop/teardown replaces the paused file in mpv anyway).
pub(crate) struct PreloadReady {
    pub item_id: String,
    pub item_name: String,
    pub media_source: jellyfin_api::models::MediaSourceInfo,
    pub media_source_id: String,
    pub play_session_id: String,
    /// The exact URL `player.load()` was issued with -- promote verifies
    /// mpv still holds this file (`Player::current_path`) before trusting
    /// the warm state.
    pub url: String,
    /// The resume offset the preload loaded at (`None` = from the start).
    /// A later "Start from beginning" click mismatches this and falls back
    /// to a cold load rather than silently resuming.
    pub start_secs: Option<f64>,
    pub max_bitrate: Option<u32>,
    pub chapters: Vec<(f64, String)>,
    pub trickplay: Option<crate::trickplay::TrickplayMeta>,
    pub container: Option<String>,
    /// Stashed by `root.rs::on_loaded` when mpv's `Loaded` fires while the
    /// preload is still dark (no live session to apply it to); replayed
    /// into the freshly-promoted session so duration/tracks aren't lost.
    /// `None` if promote happens before mpv finishes opening -- the real
    /// event then arrives post-promote and flows normally.
    pub loaded: Option<(f64, Vec<player::Track>)>,
}

/// Fire-and-forget dark preload -- same spawn/oneshot/generation shape
/// as [`start_playback`], but the outcome is a [`PreloadReady`] for
/// `root.rs` to hold until the user actually clicks Play. The caller must
/// store and abort the returned handle exactly like `playback_task`
/// (contract: it holds an `Arc<Player>` clone across
/// network awaits).
#[allow(clippy::too_many_arguments)] // plain data parameters, same as start_playback.
pub(crate) fn start_preload(
    runtime: &tokio::runtime::Runtime,
    client: JellyfinClient,
    mirror: Mirror,
    player: Arc<Player>,
    item_id: String,
    item_name: String,
    bitrate_mode: crate::settings::BitrateMode,
    result_tx: tokio::sync::oneshot::Sender<Result<PreloadReady, String>>,
    playback_generation: Arc<AtomicU64>,
    my_generation: u64,
) -> tokio::task::JoinHandle<()> {
    runtime.spawn(async move {
        let outcome = run_preload(
            client,
            mirror,
            player,
            item_id,
            item_name,
            bitrate_mode,
            playback_generation,
            my_generation,
        )
        .await;
        let _ = result_tx.send(outcome);
    })
}

/// The dark half of `run()`: PlaybackInfo -> decide -> paused `player.load`
/// with a capped readahead -> enrichment, and NO reporting session (the
/// server must not see a "playing" session for something the user hasn't
/// started). Direct Play only -- a transcode decision would spin up
/// server-side ffmpeg for a speculative open, so it returns `Err` and the
/// click pays today's cold path instead.
#[allow(clippy::too_many_arguments)]
async fn run_preload(
    client: JellyfinClient,
    mirror: Mirror,
    player: Arc<Player>,
    item_id: String,
    item_name: String,
    bitrate_mode: crate::settings::BitrateMode,
    playback_generation: Arc<AtomicU64>,
    my_generation: u64,
) -> Result<PreloadReady, String> {
    // Same mirror-side resume read as `run()` -- the preload must open at
    // the same offset the Play button will resume at, or the warm state is
    // useless for the common resume click.
    let resume_ticks = mirror
        .item(&item_id)
        .and_then(|dto| dto.user_data)
        .and_then(|ud| ud.playback_position_ticks)
        .filter(|ticks| *ticks > 0);

    let effective_bitrate = match bitrate_mode {
        crate::settings::BitrateMode::DirectPlay => None,
        crate::settings::BitrateMode::Measured => auto_bitrate_cap(&client).await,
        crate::settings::BitrateMode::Cap(cap) => Some(cap),
    };
    let profile = jellyfin_core::build_device_profile(effective_bitrate);
    let info = client
        .get_playback_info(&item_id, &profile, resume_ticks)
        .await
        .map_err(|e| e.to_string())?;
    let decision =
        jellyfin_core::decide_playback(&client, &item_id, &info).map_err(|e| e.to_string())?;
    let (source, url) = match &decision {
        PlaybackDecision::DirectPlay { source, url } => (source.clone(), url.clone()),
        PlaybackDecision::Transcode { .. } => {
            return Err("transcode decision -- preload only speculates on Direct Play".to_string())
        }
    };
    // Same cached-IP substitution (plus Host preservation) as `run()` --
    // and it must happen HERE, before the URL is both loaded and stored in
    // `PreloadReady`, so promote's `current_path == ready.url` warm-state
    // check compares like with like.
    let target = jellyfin_api::dns::rewrite_http_host_to_cached_ip(&url);
    let url = target.url;
    let stream_headers: Vec<(String, String)> = target
        .host_header
        .map(|host| vec![("Host".to_string(), host)])
        .unwrap_or_default();
    let media_source_id = source.id.clone().unwrap_or_default();
    let play_session_id = info.play_session_id.clone().unwrap_or_default();
    let start_secs = resume_ticks.map(|ticks| ticks as f64 / 10_000_000.0);

    // Same immediately-before-load gate as `run()` -- a click or a
    // newer preload may have superseded this flow during the awaits above.
    load_if_current(&playback_generation, my_generation, || {
        player
            .load(LoadRequest {
                url: url.clone(),
                http_headers: stream_headers,
                start_secs,
                external_subs: Vec::new(),
                start_paused: true,
                readahead_secs: Some(PRELOAD_READAHEAD_SECS),
                max_bytes: Some(PRELOAD_MAX_BYTES),
            })
            .map_err(|e| e.to_string())
    })?;
    tracing::info!(
        item = %item_name,
        url = %crate::redact::redact_url(&url),
        resume = ?start_secs,
        max_bytes = PRELOAD_MAX_BYTES,
        "preload: dark preload opened (paused, capped readahead + byte ceiling)"
    );

    // Same enrichment `run()` fetches after its own load -- doing it now
    // means a promote needs zero network round trips at click time.
    let enrichment = crate::detail::fetch_media_streams(client.clone(), item_id.clone()).await;
    let chapters: Vec<(f64, String)> = enrichment
        .as_ref()
        .map(|dto| {
            dto.chapters
                .iter()
                .map(|c| {
                    let secs = c.start_position_ticks.unwrap_or(0) as f64 / 10_000_000.0;
                    (secs, c.name.clone().unwrap_or_default())
                })
                .collect()
        })
        .unwrap_or_default();
    let trickplay = enrichment
        .as_ref()
        .and_then(|dto| crate::trickplay::resolve_trickplay(dto, &item_id, &media_source_id));
    let container = enrichment.as_ref().and_then(|dto| dto.container.clone());

    Ok(PreloadReady {
        item_id,
        item_name,
        media_source: source,
        media_source_id,
        play_session_id,
        url,
        start_secs,
        max_bitrate: effective_bitrate,
        chapters,
        trickplay,
        container,
        loaded: None,
    })
}

/// One measured link-speed cap per server per app run, at 80% of the
/// throughput Jellyfin's own `/Playback/BitrateTest` reports (3 MB test --
/// see `JellyfinClient::measure_bitrate`'s TCP-slow-start caveat; 80%
/// leaves headroom for bitrate spikes and concurrent traffic). `None` (and
/// no cache entry, so a later play retries) if the test fails -- playback
/// then proceeds uncapped, exactly the old behavior. The cache means the
/// one-time cost (~2s on a genuinely slow link, negligible on a LAN) is
/// paid on the first play only.
async fn auto_bitrate_cap(client: &JellyfinClient) -> Option<u32> {
    use std::collections::HashMap;
    use std::sync::Mutex;
    static MEASURED: Mutex<Option<HashMap<String, u32>>> = Mutex::new(None);
    const TEST_BYTES: u64 = 3 * 1024 * 1024;
    const SAFETY_FACTOR: f64 = 0.8;

    let base_url = client.base_url().to_string();
    // `unwrap_or_else(into_inner)`: a poisoned lock just means another
    // thread panicked mid-insert -- the map itself is still usable.
    if let Some(cached) = MEASURED
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .as_ref()
        .and_then(|m| m.get(&base_url).copied())
    {
        return Some(cached);
    }
    match client.measure_bitrate(TEST_BYTES).await {
        Ok(bps) => {
            let cap = ((bps as f64) * SAFETY_FACTOR).min(u32::MAX as f64) as u32;
            tracing::info!(
                measured_bps = bps,
                cap_bps = cap,
                "auto bitrate: measured link speed"
            );
            MEASURED
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .get_or_insert_with(HashMap::new)
                .insert(base_url, cap);
            Some(cap)
        }
        Err(e) => {
            tracing::warn!(error = %e, "auto bitrate: test failed; playing uncapped");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pins the exact stage-label text the loading overlay
    /// shows -- a plain data check, but worth catching a typo/regression in
    /// since it's user-visible copy with no other test coverage (the
    /// overlay's rendering itself needs a live GPUI window).
    #[test]
    fn load_stage_labels_are_stable() {
        assert_eq!(LoadStage::ContactingServer.label(), "Contacting server…");
        assert_eq!(LoadStage::OpeningStream.label(), "Opening stream…");
    }

    /// The gate `run()` evaluates immediately
    /// before `player.load()`. When a newer play (or a stop) bumped the
    /// shared generation mid-flight, the gate must return `SUPERSEDED` and
    /// must NOT invoke the load closure -- mpv is never touched on behalf of
    /// an item the user has already navigated away from.
    #[test]
    fn superseded_generation_returns_superseded_and_skips_load() {
        let generation = Arc::new(AtomicU64::new(7));
        let my_generation = generation.load(Ordering::Relaxed);
        let load_called = Arc::new(std::sync::atomic::AtomicBool::new(false));

        // A newer play/stop bumps the shared counter while this flow was
        // still awaiting PlaybackInfo (i.e. before the gate is reached).
        generation.fetch_add(1, Ordering::Relaxed);

        let flag = load_called.clone();
        let outcome = load_if_current(&generation, my_generation, || {
            flag.store(true, Ordering::Relaxed);
            Ok(())
        });

        assert_eq!(outcome, Err(SUPERSEDED.to_string()));
        assert!(
            !load_called.load(Ordering::Relaxed),
            "player.load must not run once the flow is superseded"
        );
    }

    /// The complement: an un-bumped generation runs the load exactly once
    /// and propagates its `Ok` result, so the fix does not break the normal
    /// (non-superseded) play path.
    #[test]
    fn current_generation_runs_the_load() {
        let generation = Arc::new(AtomicU64::new(3));
        let my_generation = generation.load(Ordering::Relaxed);
        let load_called = Arc::new(std::sync::atomic::AtomicBool::new(false));

        let flag = load_called.clone();
        let outcome = load_if_current(&generation, my_generation, || {
            flag.store(true, Ordering::Relaxed);
            Ok(())
        });

        assert_eq!(outcome, Ok(()));
        assert!(
            load_called.load(Ordering::Relaxed),
            "load must run when the generation is current"
        );
    }
}
