//! Playback control: play/preload, OSD interaction (scrub, volume, seek),
//! track picker, skip segments, miniplayer drag, next-episode advance, and
//! the option-key speed-hold gesture. Keyboard dispatch fan-out
//! (`handle_global_keystroke`/`handle_remote_command`) stays on `Root` in
//! `root.rs`, per the crate's module map.

use std::sync::atomic::Ordering;
use std::time::Instant;

use gpui::Context;

use jellyfin_api::models::{BaseItemKind, LocationType};
use jellyfin_core::{ReportContext, ReportingSession};

use crate::nav::View;
use crate::player_ui::{PlayerUiState, SegmentDecision};
use crate::root::{
    adjacent_episode, decide_preload_retarget, resolve_play_target, ContentMode, EpisodeContext,
    EpisodeStep, PreloadRetargetDecision, PreloadTrigger, Root, Screen, EOF_EPSILON_SECS,
};

impl Root {
    // ---- Playback -----------------------------------------------------

    /// Starts playback via the PlaybackInfo -> decide -> load flow
    /// (`playback::start_playback`), bridged into GPUI via a oneshot
    /// channel + `cx.spawn`. `last_position_ticks` is captured
    /// synchronously here, before switching items: safe because it's
    /// written only by the mpv `Position` observer on this same
    /// single-threaded GPUI executor, which can't process the new item's
    /// first event until this call returns. Server-side resume policy
    /// (`MinResumeDurationSeconds`/`MinResumePct`/`MaxResumePct`) can still
    /// discard or round a Stopped report's position; see
    /// `e2e.rs::assert_switch_away_preserves_resume_point`.
    pub(crate) fn play_item(&mut self, item_id: String, item_name: String, cx: &mut Context<Self>) {
        self.play_item_inner(item_id, item_name, false, None, cx);
    }

    /// docs/PLUGIN-CHANNELS.md §2.3: channel browse's
    /// entry point for activating a recording row. Identical to `play_item`
    /// except `resume_ticks_hint` -- a channel recording is never mirrored
    /// (§2.1), so the caller passes the position straight off the
    /// recording's live-listing DTO instead of the usual mirror lookup.
    pub(crate) fn play_item_with_resume_hint(
        &mut self,
        item_id: String,
        item_name: String,
        resume_ticks_hint: Option<i64>,
        cx: &mut Context<Self>,
    ) {
        self.play_item_inner(item_id, item_name, false, resume_ticks_hint, cx);
    }

    /// `detail.rs::play_button_element`'s "Start from beginning": same flow
    /// as `play_item` with the resume position suppressed for this one
    /// start; `playback::run` simply doesn't read it, so declining to
    /// resume once never destroys the stored position.
    pub(crate) fn play_item_from_start(
        &mut self,
        item_id: String,
        item_name: String,
        cx: &mut Context<Self>,
    ) {
        self.play_item_inner(item_id, item_name, true, None, cx);
    }

    fn play_item_inner(
        &mut self,
        item_id: String,
        item_name: String,
        from_beginning: bool,
        resume_ticks_hint: Option<i64>,
        cx: &mut Context<Self>,
    ) {
        // Switching items must not just overwrite `state.reporting` -- the
        // old session's final Stopped report would never be sent, leaving
        // server-side `Sessions/Playing` state dangling (same class of bug
        // `ReportingSession`'s `#[must_use]` + `Drop` warning exists to
        // catch). Take the old session out and stop it with the last known
        // position first.
        let last_ticks = self.last_position_ticks.load(Ordering::Relaxed);
        let runtime_for_stop = self.runtime.clone();
        let Screen::Main(state) = &mut self.screen else {
            return;
        };
        // docs/UX-SPEC.md §6: play disabled while offline -- PlaybackInfo/stream
        // URLs need a live server round trip regardless of mirror cache, so
        // there's nothing to fall back to (unlike browse, which reads the
        // mirror directly).
        if state.offline {
            self.set_playback_error(
                "Can't play while offline -- server unreachable".to_string(),
                cx,
            );
            return;
        }
        // Defensive backstop, not the primary guard -- `cards.rs::
        // episode_card` and `detail.rs::play_button_element` already hide/
        // disable Play for a virtual (unaired/missing) item. Synchronous
        // mirror read, no network round trip needed.
        let dto = state.mirror.item(&item_id);
        if dto.as_ref().and_then(|d| d.location_type) == Some(LocationType::Virtual) {
            self.set_playback_error(
                "Can't play -- this episode isn't available yet".to_string(),
                cx,
            );
            return;
        }
        // `PlaybackInfo` on a folder-type id 500s server-side, and Series
        // rows are reachable from real play affordances (Home hero Resume,
        // series card Return). Resolve a Series to its next-up episode via
        // `detail::find_series_next_episode` instead of handing the server
        // an unplayable id; Season/BoxSet get a friendly error instead.
        match dto.as_ref().and_then(|d| d.type_) {
            Some(BaseItemKind::Series) => {
                let seasons =
                    state
                        .mirror
                        .children(&item_id, media_cache::Sort::IndexNumber, 0, 100);
                let next = crate::detail::find_series_next_episode(&state.mirror, &seasons);
                match next {
                    Some(ep) => {
                        let ep_name = ep.name.clone();
                        tracing::info!(
                            series = %item_name,
                            episode = %ep_name,
                            "play_item: resolved series to next-up episode"
                        );
                        self.play_item_inner(ep.id, ep_name, from_beginning, None, cx);
                    }
                    None => {
                        self.set_playback_error(
                            "No playable episodes in this series.".to_string(),
                            cx,
                        );
                    }
                }
                return;
            }
            Some(BaseItemKind::Season)
            | Some(BaseItemKind::BoxSet)
            | Some(BaseItemKind::Folder) => {
                self.set_playback_error("Open it and pick something to play".to_string(), cx);
                return;
            }
            _ => {}
        }
        // Bump so any other in-flight play flow's late outcome is
        // invalidated, and so its own pre-`player.load()` check sees itself
        // superseded before touching mpv. Deliberately not at the top of
        // `play_item`: the guards above early-return leaving the CURRENT
        // session playing, so bumping earlier would invalidate a live flow
        // on a rejected click.
        let playback_gen = state
            .playback_generation
            .fetch_add(1, Ordering::Relaxed)
            .wrapping_add(1);
        // The previous flow, if still awaiting the server, is now
        // superseded -- abort it so it can't reach `player.load()` or keep
        // its `Arc<Player>` clone alive. Same contract as the diagnostic
        // handles aborted below.
        if let Some(handle) = state.playback_task.take() {
            handle.abort();
        }
        if let Some(old_reporting) = state.reporting.take() {
            runtime_for_stop.spawn(async move {
                old_reporting.stop(last_ticks).await;
            });
        }
        // Apply the switched-away-from item's final position to the mirror
        // optimistically -- same reasoning as `stop_playback`'s call below.
        if let Some(old_item_id) = state.playing_item_id.clone() {
            let mirror = state.mirror.clone();
            runtime_for_stop.spawn(async move {
                mirror
                    .apply_local_user_data(&old_item_id, last_ticks, None)
                    .await;
            });
        }
        // `last_position_ticks` is a single global counter written only by
        // the mpv `Position` observer, and the new item hasn't produced one
        // yet -- re-baseline to 0 now (already consumed above) or a stop/
        // quit during load stamps the OLD item's position onto the new one,
        // falsely marking it watched (`writer::resolve_played_position`). 0
        // is the safe sentinel: both that fn and Jellyfin's `MinResumePct`
        // discard a spurious 0, unlike a spurious large value.
        self.last_position_ticks.store(0, Ordering::Relaxed);
        // Cancel the previous item's hwdec-log task too -- otherwise it
        // fires 5s later and logs a line attributed to the wrong item.
        if let Some(handle) = state.hwdec_log_handle.take() {
            handle.abort();
        }
        // Same reasoning for the periodic playback-diagnostics task.
        if let Some(handle) = state.diag_log_handle.take() {
            handle.abort();
        }
        // Series/season context for this playback session: Detail-page
        // derivation is the first choice (already has the loaded season
        // list), with the episode's own mirror dto as the universal
        // fallback -- it carries series_id/season_id/series_name/index
        // numbers regardless of navigation source, so Up Next, EOF auto-
        // advance, and `[`/`]` nav still work when started from a shelf or
        // search.
        let item_dto = state.mirror.item(&item_id);
        let series_id = state
            .detail
            .as_ref()
            .and_then(|d| {
                if d.is_series {
                    Some(d.item_id.clone())
                } else {
                    d.dto
                        .as_ref()
                        .and_then(|dto| dto.series_id)
                        .map(|u| u.to_string())
                }
            })
            .or_else(|| {
                item_dto
                    .as_ref()
                    .and_then(|d| d.series_id)
                    .map(|u| u.to_string())
            });
        // OSD breadcrumb metadata ("Series Name · S2 E4 · Episode Title"):
        // same Detail-page-first, dto-fallback reasoning as `series_id`
        // above; `None` fields degrade gracefully in
        // `PlayerUiState::breadcrumb_title`.
        let episode_ctx = state
            .detail
            .as_ref()
            .filter(|d| d.is_series)
            .map(|d| {
                let season = d.seasons.get(d.selected_season);
                EpisodeContext {
                    series_name: d.dto.as_ref().and_then(|dto| dto.name.clone()),
                    season_number: season.and_then(|s| s.index_number),
                    season_id: season.map(|s| s.id.clone()),
                    episode_number: d
                        .episodes
                        .iter()
                        .find(|ep| ep.id == item_id)
                        .and_then(|ep| ep.index_number),
                }
            })
            .or_else(|| {
                item_dto.as_ref().map(|d| EpisodeContext {
                    series_name: d.series_name.clone(),
                    season_number: d.parent_index_number,
                    season_id: d.season_id.map(|u| u.to_string()),
                    episode_number: d.index_number,
                })
            })
            .unwrap_or_default();
        // If this exact item is already dark-preloaded (paused stream open
        // in mpv, PlaybackInfo + enrichment cached), skip the Loading flow
        // and seed the session from the cache instead. Conditions: item
        // matches, resume intent matches (a "Start from beginning" against
        // a resume-offset preload falls back cold), and mpv verifiably
        // still holds the preloaded URL (state-drift guard).
        let promote_ready = {
            let matches = state.preload.as_ref().is_some_and(|p| {
                p.item_id == item_id && !(from_beginning && p.start_secs.is_some())
            });
            if matches {
                let mpv_path = self.video.player().current_path();
                let path_ok = state
                    .preload
                    .as_ref()
                    .is_some_and(|p| mpv_path.as_deref() == Some(p.url.as_str()));
                if !path_ok {
                    tracing::warn!(
                        item = %item_name,
                        "preload: preload path mismatch (mpv holds something else); cold start"
                    );
                }
                path_ok
            } else {
                false
            }
        };
        // Either way the stored preload is consumed now: promoted, or dead
        // the moment the cold path's `loadfile` below replaces the paused
        // file. An in-flight preload task is superseded by the generation
        // bump above; abort it too.
        if let Some(handle) = state.preload_task.take() {
            handle.abort();
        }
        // Playing/loading breaks the preload slot's idle contract
        // (`preload_target`'s doc comment) -- clear it either way so a
        // later idle-browse trigger doesn't think this item still preloads.
        state.preload_target = None;
        if promote_ready {
            let ready = state
                .preload
                .take()
                .expect("promote_ready implies a stored preload");
            self.promote_preload(ready, item_name, series_id, episode_ctx, playback_gen, cx);
            return;
        }
        discard_preload(state, self.video.player(), "different item played");
        state.mode = ContentMode::Loading {
            title: item_name.clone(),
        };
        state.playing_item_id = Some(item_id.clone());
        // Bulk sync yields to playback from the moment of commit, not first
        // frame -- see `Mirror::set_playback_active`.
        state.set_playback_active(true);
        state.player_ui = None;
        // The previous flow's stash, if any, describes a file being
        // replaced -- the generation stamp already excludes it; this just
        // avoids keeping a stale track list alive until then.
        state.pending_loaded = None;
        // Live loading-stage progress -- see `loading_stage_rx`'s doc
        // comment. Created here (not inside `playback::start_playback`) so
        // `render_content`'s `Loading` arm has something from frame one.
        let (stage_tx, stage_rx) =
            tokio::sync::watch::channel(crate::playback::LoadStage::ContactingServer);
        state.loading_stage_rx = Some(stage_rx.clone());
        let bitrate_mode = self.app_settings.bitrate_mode(&state.base_url);
        cx.notify();

        // Bridges the tokio `watch` channel into a GPUI `cx.notify()` so
        // the stage text repaints as it advances -- nothing else re-renders
        // during `ContentMode::Loading`. Self-terminating: `stage_tx` is
        // dropped once the spawned `playback::run` task finishes, ending
        // this loop.
        let mut notify_rx = stage_rx;
        cx.spawn(async move |this, cx| {
            while notify_rx.changed().await.is_ok() {
                if this.update(cx, |_, cx| cx.notify()).is_err() {
                    break;
                }
            }
        })
        .detach();

        // Apply the persisted subtitle style before load too (not just on
        // settings change -- see `set_subtitle_style`'s doc comment) so
        // it's already in effect for the first frame with subtitles on.
        if let Err(e) = self
            .video
            .player()
            .set_subtitle_style(&self.app_settings.subtitle.to_player())
        {
            tracing::warn!(error = %e, "set_subtitle_style on player init failed");
        }

        let (tx, rx) = tokio::sync::oneshot::channel();
        // The handle is stored (aborted by the next play/stop/quit), and
        // the generation pair lets `run()` bail before `player.load()` if
        // already superseded.
        let task = crate::playback::start_playback(
            &self.runtime,
            state.client.clone(),
            state.mirror.clone(),
            self.video.player().clone(),
            item_id,
            item_name.clone(),
            bitrate_mode,
            from_beginning,
            resume_ticks_hint,
            stage_tx,
            tx,
            state.playback_generation.clone(),
            playback_gen,
        );
        state.playback_task = Some(task);
        cx.spawn(async move |this, cx| {
            let outcome = rx
                .await
                .unwrap_or_else(|_| Err("playback task dropped".to_string()));
            this.update(cx, |root, cx| {
                root.handle_playback_outcome(
                    playback_gen,
                    item_name,
                    series_id,
                    episode_ctx,
                    outcome,
                    cx,
                )
            })
            .ok();
        })
        .detach();
    }

    /// Turns a completed dark preload into a live playback session with
    /// zero network round trips: unpause, restore the full readahead
    /// target, start reporting, and hand a synthesized `PlaybackStarted` to
    /// the same `handle_playback_outcome` the cold path uses. The stashed
    /// dark `Loaded` event, if mpv already fired it, is replayed afterward
    /// so duration/track state isn't lost.
    fn promote_preload(
        &mut self,
        mut ready: crate::playback::PreloadReady,
        item_name: String,
        series_id: Option<String>,
        episode_ctx: EpisodeContext,
        generation: u64,
        cx: &mut Context<Self>,
    ) {
        let player = self.video.player().clone();
        let Screen::Main(state) = &mut self.screen else {
            return;
        };
        let t0 = Instant::now();
        if let Err(e) = player.set_readahead_secs(player::DEFAULT_READAHEAD_SECS) {
            tracing::warn!(error = %e, "promote: restoring readahead failed");
        }
        // Byte-ceiling sibling of the readahead restore above -- a
        // per-file mpv option that persists once set. Without this reset,
        // a promoted preload keeps the dark preload's ~6MB ceiling for its
        // whole playback, reading as stuttering on a slow link.
        if let Err(e) = player.set_max_bytes(player::DEFAULT_MAX_BYTES) {
            tracing::warn!(error = %e, "promote: restoring demuxer-max-bytes failed");
        }
        if let Err(e) = player.set_paused(false) {
            // mpv refusing a property write means the handle is in real
            // trouble -- surface it like any cold-start failure rather than
            // leaving the UI stuck thinking something is playing.
            tracing::warn!(error = %e, "promote: unpause failed; giving up on preload");
            state.preload = None;
            self.set_playback_error(format!("Playback failed to start: {e}"), cx);
            return;
        }
        let ctx = ReportContext {
            item_id: ready.item_id.clone(),
            media_source_id: ready.media_source_id.clone(),
            play_session_id: ready.play_session_id.clone(),
        };
        // Runs on the GPUI thread; `ReportingSession::start` and the two
        // diagnostic-log spawns all `tokio::spawn` internally, which panics
        // without an entered runtime context. Scoped so the enter-guard's
        // borrow of `self.runtime` ends before `handle_playback_outcome`
        // needs `&mut self` below.
        let (reporting, hwdec_log_handle, diag_log_handle) = {
            let _rt_guard = self.runtime.enter();
            (
                ReportingSession::start(state.client.clone(), ctx),
                crate::playback::spawn_hwdec_log(&player, &item_name),
                crate::playback::spawn_diag_log(&player, &item_name),
            )
        };
        self.promoted_preloads += 1;
        // Read AFTER the unpause command was queued but effectively "at
        // click": the banked fw-bytes here is head start the user did NOT
        // have to wait for -- the savings side of the discard accounting.
        let banked_bytes = player.cache_state().fw_bytes.unwrap_or(0).max(0);
        tracing::info!(
            item = %item_name,
            resume = ?ready.start_secs,
            banked_bytes,
            promoted_total = self.promoted_preloads,
            "preload: promoted preloaded session (prefetched PlaybackInfo, warm paused stream)"
        );
        let stashed_loaded = ready.loaded.take();
        state.playing_item_id = Some(ready.item_id.clone());
        // Same yield-to-playback signal as play_item_inner's cold path.
        state.set_playback_active(true);
        let started = crate::playback::PlaybackStarted {
            reporting,
            item_id: ready.item_id,
            media_source_id: ready.media_source_id,
            decision_summary: "Direct Play".to_string(),
            is_direct_play: true,
            decision_reasons: Vec::new(),
            container: ready.container,
            media_source: ready.media_source,
            chapters: ready.chapters,
            trickplay: ready.trickplay,
            max_bitrate: ready.max_bitrate,
            hwdec_log_handle,
            diag_log_handle,
            // The prefetch paid this cost before the click, so these
            // checkpoints are ~0; JELLYBEAM_LATENCY reports the true
            // click->first-Position time for promoted starts.
            load_latency: crate::playback::LoadLatency {
                t0,
                playback_info_ms: 0,
                decide_ms: 0,
                pre_load_ms: 0,
            },
        };
        self.handle_playback_outcome(
            generation,
            item_name,
            series_id,
            episode_ctx,
            Ok(started),
            cx,
        );
        if let Some((duration_secs, tracks)) = stashed_loaded {
            self.on_loaded(duration_secs, tracks, cx);
        }
    }

    /// How long a hover dwell on a different item must be the most recent
    /// before it's allowed to discard and replace the preload slot's
    /// contents -- a larger gate than `cards.rs`'s 350ms dwell timer, since
    /// this discards bandwidth already spent. A deliberate request
    /// (keyboard focus, Detail-page open) skips this gate.
    const HOVER_RETARGET_THROTTLE: std::time::Duration = std::time::Duration::from_millis(1000);

    /// Speculative dark preload -- opens the likely-next item (Detail page
    /// open, hover dwell, the once-per-session hero preload) as a PAUSED
    /// mpv stream with a capped readahead + byte ceiling and cached
    /// PlaybackInfo, so a matching Play click starts in tens of
    /// milliseconds (`promote_preload`). Only runs while fully idle and
    /// while "Preload next item" is on.
    ///
    /// `trigger` feeds `decide_preload_retarget`: a `Hover` that would
    /// discard an occupied slot is delayed by `HOVER_RETARGET_THROTTLE`;
    /// `Deliberate` acts immediately.
    pub(crate) fn preload_item(
        &mut self,
        item_id: String,
        trigger: PreloadTrigger,
        cx: &mut Context<Self>,
    ) {
        let bitrate_mode = {
            let Some(state) = self.main_state() else {
                return;
            };
            if preload_blocked(self.app_settings.preload, state) {
                return;
            }
            self.app_settings.bitrate_mode(&state.base_url)
        };
        let Some(state) = self.main_state_mut() else {
            return;
        };
        let Some((target_id, target_name)) = resolve_play_target(state, &item_id) else {
            return;
        };
        // Every request bumps the epoch, including a same-target no-op
        // below -- deliberate: it invalidates a previously scheduled hover
        // retarget for a different item when the pointer returns before
        // that timer fires (see `hover_retarget_epoch`'s doc comment).
        state.hover_retarget_epoch = state.hover_retarget_epoch.wrapping_add(1);
        let epoch = state.hover_retarget_epoch;
        // Already preloaded (or being preloaded) -- nothing to do. Checked
        // against `preload_target`, not `preload` (only `Some` once
        // filled), or a repeat hover on a still-in-flight target would
        // restart it from scratch on every dwell.
        if state.preload_target.as_deref() == Some(target_id.as_str()) {
            return;
        }
        match decide_preload_retarget(state.preload_target.as_deref(), &target_id, trigger) {
            PreloadRetargetDecision::ProceedImmediately
            | PreloadRetargetDecision::ReplaceImmediately => {
                self.start_preload_now(
                    target_id,
                    target_name,
                    bitrate_mode,
                    "replaced by a newer preload target",
                    cx,
                );
            }
            PreloadRetargetDecision::ScheduleReplace => {
                cx.spawn(async move |this, cx| {
                    cx.background_executor()
                        .timer(Self::HOVER_RETARGET_THROTTLE)
                        .await;
                    let _ = this.update(cx, |root, cx| {
                        root.fire_hover_retarget(epoch, target_id, target_name, bitrate_mode, cx)
                    });
                })
                .detach();
            }
        }
    }

    /// The delayed half of a throttled hover retarget -- see
    /// `HOVER_RETARGET_THROTTLE`'s doc comment. Re-validates everything
    /// `preload_item` already checked, plus the epoch stamp, so this is a
    /// no-op unless the same hover target is still the most recent request.
    fn fire_hover_retarget(
        &mut self,
        epoch: u64,
        target_id: String,
        target_name: String,
        bitrate_mode: crate::settings::BitrateMode,
        cx: &mut Context<Self>,
    ) {
        let Screen::Main(state) = &mut self.screen else {
            return;
        };
        if state.hover_retarget_epoch != epoch {
            // Superseded: another hover landed elsewhere, the pointer
            // returned to the preloaded card, or a deliberate request acted.
            tracing::debug!(item = %target_id, "preload: hover retarget superseded, not firing");
            return;
        }
        if preload_blocked(self.app_settings.preload, state)
            || state.preload_target.as_deref() == Some(target_id.as_str())
        {
            return;
        }
        self.start_preload_now(
            target_id,
            target_name,
            bitrate_mode,
            "hover dwell retarget (throttled 1000ms)",
            cx,
        );
    }

    /// Discards whatever's in the preload slot and starts a new preload --
    /// shared by `preload_item`'s immediate path and
    /// `fire_hover_retarget`'s delayed one. `discard_reason` is only
    /// logged when there was something in `preload` to discard.
    fn start_preload_now(
        &mut self,
        target_id: String,
        target_name: String,
        bitrate_mode: crate::settings::BitrateMode,
        discard_reason: &'static str,
        cx: &mut Context<Self>,
    ) {
        let player = self.video.player().clone();
        let Screen::Main(state) = &mut self.screen else {
            return;
        };
        // Supersede any previous/in-flight preload the same way `play_item`
        // does: bump the shared generation and abort the task. Deliberately
        // `playback_generation`, not `hover_retarget_epoch` -- a real mpv
        // operation (a new `loadfile`) is about to happen, which is exactly
        // what `playback_generation` exists to gate.
        let my_generation = state
            .playback_generation
            .fetch_add(1, Ordering::Relaxed)
            .wrapping_add(1);
        if let Some(handle) = state.preload_task.take() {
            handle.abort();
        }
        discard_preload(state, &player, discard_reason);
        state.preload_target = Some(target_id.clone());
        // Same pre-load subtitle-style application as `play_item_inner` --
        // the promoted session's first frames must already honor it.
        if let Err(e) = player.set_subtitle_style(&self.app_settings.subtitle.to_player()) {
            tracing::warn!(error = %e, "set_subtitle_style before preload failed");
        }
        let (tx, rx) = tokio::sync::oneshot::channel();
        let task = crate::playback::start_preload(
            &self.runtime,
            state.client.clone(),
            state.mirror.clone(),
            player,
            target_id,
            target_name,
            bitrate_mode,
            tx,
            state.playback_generation.clone(),
            my_generation,
        );
        state.preload_task = Some(task);
        cx.spawn(async move |this, cx| {
            if let Ok(outcome) = rx.await {
                this.update(cx, |root, cx| {
                    root.handle_preload_outcome(my_generation, outcome, cx)
                })
                .ok();
            }
        })
        .detach();
    }

    /// Stores a completed preload for `play_item_inner`'s promote check --
    /// unless the generation bumped while it was in flight, in which case
    /// mpv no longer holds this preload's file and the result is dropped.
    fn handle_preload_outcome(
        &mut self,
        generation: u64,
        outcome: Result<crate::playback::PreloadReady, String>,
        _cx: &mut Context<Self>,
    ) {
        let Some(state) = self.main_state_mut() else {
            return;
        };
        state.preload_task = None;
        if state.playback_generation.load(Ordering::Relaxed) != generation {
            tracing::debug!("preload: stale preload outcome discarded");
            return;
        }
        match outcome {
            Ok(ready) => {
                tracing::info!(item = %ready.item_name, "preload: preload ready (paused, awaiting Play)");
                state.preload = Some(ready);
            }
            // Expected for transcode decisions and superseded flows --
            // debug, not warn. Clear `preload_target` so a later request on
            // the same item doesn't think it's still in flight.
            Err(e) => {
                tracing::debug!(error = %e, "preload: preload skipped");
                state.preload_target = None;
            }
        }
    }

    fn handle_playback_outcome(
        &mut self,
        generation: u64,
        item_name: String,
        series_id: Option<String>,
        episode_ctx: EpisodeContext,
        outcome: Result<crate::playback::PlaybackStarted, String>,
        cx: &mut Context<Self>,
    ) {
        let video = self.video.clone();
        let Screen::Main(state) = &mut self.screen else {
            return;
        };
        // Superseded by a stop or a newer play while this flow was loading
        // -- tear down whatever mpv state it started and drop its
        // reporting session with a zero-position stop (never watched).
        if state.playback_generation.load(Ordering::Relaxed) != generation {
            tracing::info!(item = %item_name, "stale playback outcome superseded; discarding");
            state.loading_stage_rx = None;
            if let Ok(started) = outcome {
                if let Err(e) = video.player().stop() {
                    tracing::warn!(error = %e, "stopping superseded playback failed");
                }
                video.clear_to_black();
                // Superseded before becoming active: a later Stopped(0)
                // could overwrite the new session's resume position for
                // this item, so discard this reporting session instead.
                started.reporting.abandon();
            }
            cx.notify();
            return;
        }
        // `loading_stage_rx` is only meaningful during `ContentMode::
        // Loading` -- clear it regardless of outcome so a stale value
        // can't linger into `Playing`/`Browse`.
        state.loading_stage_rx = None;
        match outcome {
            Ok(started) => {
                tracing::info!(item = %item_name, decision = %started.decision_summary, "playback started");
                state.paused = false;
                state.error = None;
                video.set_layer_mode(crate::gl_video::LayerMode::FullscreenInWindow);

                // mpv's `speed` is per-player, not per-file, and survives
                // `player.load()` with no automatic reset -- a freshly
                // started item must never inherit a stale boosted rate.
                // Covers both playback-start paths: cold `playback::run`
                // and `promote_preload`'s replay funnel through this arm.
                if let Err(e) = video.player().set_speed(1.0) {
                    tracing::warn!(
                        error = %e,
                        "handle_playback_outcome: resetting speed to 1.0 failed"
                    );
                }

                let mut ui = PlayerUiState::new(&started);
                ui.series_id = series_id;
                ui.title = item_name.clone();
                ui.series_name = episode_ctx.series_name;
                ui.season_number = episode_ctx.season_number;
                ui.episode_number = episode_ctx.episode_number;
                ui.season_id = episode_ctx.season_id;
                ui.remaining_display = state.track_prefs.remaining_display();
                // This session's copy of the per-type skip/autoplay
                // config, kept live-updatable by `set_skip_segment_action`/
                // `set_autoplay_prefs` for a mid-playback Settings change.
                ui.skip_segment_prefs = self.app_settings.skip_segments;
                ui.autoplay_prefs = self.app_settings.autoplay;
                // This session's copy of the configured skip lengths, same
                // live-updatable shape as the two fields above
                // (`Root::set_skip_length` keeps it in sync).
                ui.skip_back_secs = self.app_settings.skip_length.back_secs;
                ui.skip_forward_secs = self.app_settings.skip_length.forward_secs;
                ui.trickplay_cache = Some(crate::trickplay::TrickplayCache::new(
                    state.client.clone(),
                    self.runtime.clone(),
                ));
                let item_id = started.item_id.clone();
                state.pending_latency = Some((started.load_latency, None));
                state.reporting = Some(started.reporting);
                // Playback always starts unpaused (per-file pause=no
                // override in the player crate), so take the display-sleep
                // assertion here; `on_pause_changed` manages it from then on.
                state.display_sleep = crate::power::DisplaySleepGuard::new();
                state.hwdec_log_handle = Some(started.hwdec_log_handle);
                state.diag_log_handle = Some(started.diag_log_handle);
                state.player_ui = Some(ui);
                state.mode = ContentMode::Playing {
                    title: item_name,
                    decision: started.decision_summary,
                };
                // See `pending_loaded`'s doc comment: mpv's `Loaded` may
                // have fired while `player_ui` was `None`. Replay it
                // through `on_loaded`, generation-filtered so a superseded
                // flow's event can't apply here.
                let replay_loaded = state
                    .pending_loaded
                    .take()
                    .filter(|(gen, _, _)| *gen == generation)
                    .map(|(_, duration_secs, tracks)| (duration_secs, tracks));
                self.spawn_media_segments(item_id, cx);
                self.spawn_osd_tick(cx);
                if let Some((duration_secs, tracks)) = replay_loaded {
                    self.on_loaded(duration_secs, tracks, cx);
                }
            }
            Err(e) => {
                tracing::warn!(item = %item_name, error = %e, "playback failed to start");
                state.mode = ContentMode::Browse;
                // Nothing is playing anymore -- don't let a previous
                // session's display-sleep assertion outlive it into Browse.
                state.display_sleep = None;
                state.playing_item_id = None;
                state.set_playback_active(false);
                state.player_ui = None;
                self.set_playback_error(e, cx);
                return;
            }
        }
        cx.notify();
    }

    /// Part C §10's status pill error state: sets `state.error` and
    /// auto-dismisses it after 6s unless a newer error replaced it
    /// (`dismiss_error` is the explicit dismiss path). Every error path
    /// routes through this for consistent auto-dismiss behavior.
    fn set_playback_error(&mut self, message: String, cx: &mut Context<Self>) {
        let Some(state) = self.main_state_mut() else {
            return;
        };
        state.error = Some(message.clone());
        cx.notify();
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_secs(6))
                .await;
            let _ = this.update(cx, |root, cx| root.dismiss_error_if(&message, cx));
        })
        .detach();
    }

    /// Part C §10's status pill explicit `x.svg` dismiss.
    pub(crate) fn dismiss_error(&mut self, cx: &mut Context<Self>) {
        let Some(state) = self.main_state_mut() else {
            return;
        };
        state.error = None;
        cx.notify();
    }

    /// Clears `state.error` only if it's still exactly the message that
    /// scheduled this auto-dismiss -- a newer error replacing it in the
    /// meantime must not be clobbered by an older timer firing late.
    fn dismiss_error_if(&mut self, message: &str, cx: &mut Context<Self>) {
        let Some(state) = self.main_state_mut() else {
            return;
        };
        if state.error.as_deref() == Some(message) {
            state.error = None;
            cx.notify();
        }
    }

    /// docs/UX-SPEC.md §4 "Skip Intro/Credits" pill: fetched once per session, best
    /// effort -- `get_media_segments` already turns 404/unsupported into
    /// `Ok(vec![])`, so there's no separate "not supported" branch here.
    fn spawn_media_segments(&mut self, item_id: String, cx: &mut Context<Self>) {
        let Some(state) = self.main_state_mut() else {
            return;
        };
        let client = state.client.clone();
        self.bridge(
            cx,
            async move {
                let segments = client
                    .get_media_segments(&item_id, &[])
                    .await
                    .unwrap_or_default();
                (item_id, segments)
            },
            move |root, (id, segments), cx| root.apply_media_segments(id, segments, cx),
        );
    }

    fn apply_media_segments(
        &mut self,
        item_id: String,
        segments: Vec<jellyfin_api::models::MediaSegmentDto>,
        cx: &mut Context<Self>,
    ) {
        let Some(state) = self.main_state_mut() else {
            return;
        };
        if let Some(ui) = &mut state.player_ui {
            if ui.item_id == item_id {
                tracing::info!(item = %item_id, count = segments.len(), "JELLYBEAM: media segments loaded");
                ui.segments = segments;
                cx.notify();
            }
        }
    }

    /// Drives the OSD's idle auto-hide (`OSD_IDLE_TIMEOUT`) and the S/A toast's auto-
    /// dismiss (docs/UX-SPEC.md §3) -- GPUI has no "wake me up in N ms" primitive
    /// tied to view state, so this polls until `ContentMode` leaves
    /// `Playing`.
    fn spawn_osd_tick(&self, cx: &Context<Self>) {
        cx.spawn(async move |this, cx| loop {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(200))
                .await;
            let still_playing = this
                .update(cx, |root, cx| root.on_osd_tick(cx))
                .unwrap_or(false);
            if !still_playing {
                break;
            }
        })
        .detach();
    }

    fn on_osd_tick(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(state) = self.main_state_mut() else {
            return false;
        };
        if !matches!(state.mode, ContentMode::Playing { .. }) {
            return false;
        }
        let paused = state.paused;
        let Some(ui) = &mut state.player_ui else {
            return true;
        };
        // §1.11: "never hide while paused" -- `paused` short-circuits
        // inside `tick_auto_hide`, same as the `scrub.dragging` exemption.
        let mut changed = ui.tick_auto_hide(paused);
        changed |= ui.expire_toast();
        // The auto-skip "Undo" toast's own `SKIP_UNDO_WINDOW` expiry --
        // independent of `expire_toast`'s S/A toast (see
        // `SkipToastState`'s doc comment for why they're two fields).
        changed |= ui.expire_skip_toast();
        // §1.6: the volume slider's own "collapse 400ms after pointer
        // leaves" timer -- polled here, same shape as the two ticks above.
        changed |= ui.tick_volume_collapse();
        // Auto-hide is one of the two places OSD visibility can
        // flip -- keep the subtitle baseline in sync (see `sync_subtitle_baseline`).
        self.sync_subtitle_baseline();
        if changed {
            cx.notify();
        }
        true
    }

    // ---- OSD interaction (mouse) --------------------------------------

    /// Minimum spacing between the live `seek_absolute_fast` calls
    /// `scrub_hover` issues while drag-scrubbing -- a fast drag can
    /// deliver more mouse-move events than mpv can service, and a
    /// last-target-wins debounce (only the most recent hover position per
    /// window seeks) keeps feedback responsive without queuing a backlog.
    const SCRUB_SEEK_DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(80);

    /// Scrubber hover/drag: `frac` is `0.0..=1.0` across the bar. Updates
    /// the trickplay preview + hover-time readout, and while dragging
    /// issues a debounced fast (keyframe) seek so the picture tracks the
    /// pointer; `commit_scrub` does the one exact (hr) seek once the drag
    /// ends.
    pub(crate) fn scrub_hover(
        &mut self,
        frac: Option<f32>,
        dragging: bool,
        cx: &mut Context<Self>,
    ) {
        let root = cx.entity().downgrade();
        let video = self.video.clone();
        let Some(state) = self.main_state_mut() else {
            return;
        };
        let Some(ui) = &mut state.player_ui else {
            return;
        };
        ui.scrub.hover_frac = frac;
        if dragging {
            ui.scrub.dragging = true;
        }
        if ui.scrub.dragging {
            if let Some(f) = frac {
                let now = Instant::now();
                let due = ui
                    .scrub
                    .last_fast_seek_at
                    .is_none_or(|at| now.duration_since(at) >= Self::SCRUB_SEEK_DEBOUNCE);
                if due {
                    let target = f as f64 * ui.duration_secs;
                    let _ = video.player().seek_absolute_fast(target.max(0.0));
                    ui.scrub.last_fast_seek_at = Some(now);
                }
            }
        }
        ui.note_activity();
        // Warm the trickplay cache for this hover position (docs/UX-SPEC.md §4) --
        // `render_trickplay_preview` only *peeks* it; this is the one place
        // with the `Context<Root>` a real fetch+notify needs.
        if let (Some(f), Some(meta), Some(cache)) =
            (frac, ui.trickplay.as_ref(), ui.trickplay_cache.as_ref())
        {
            let time_ms = (f as f64 * ui.duration_secs * 1000.0).max(0.0) as u32;
            cache.tile_for_ms(meta, time_ms, root, cx);
        }
        cx.notify();
    }

    /// Clears the hover-preview ghost marker once the pointer leaves the
    /// bar, via `scrub_row`'s `on_hover` -- `scrub_hover` only ever *sets*
    /// `hover_frac`, so without this the last hover position sticks around
    /// indefinitely. Skipped mid-drag: the ghost is also the live
    /// drag-feedback marker then, and must survive the pointer straying
    /// outside the narrow track hitbox while a button's held.
    pub(crate) fn clear_scrub_hover_preview(&mut self, cx: &mut Context<Self>) {
        let Some(state) = self.main_state_mut() else {
            return;
        };
        let Some(ui) = &mut state.player_ui else {
            return;
        };
        if ui.scrub.dragging {
            return;
        }
        if ui.scrub.hover_frac.take().is_some() {
            cx.notify();
        }
    }

    pub(crate) fn commit_scrub(&mut self, cx: &mut Context<Self>) {
        let video = self.video.clone();
        let Some(state) = self.main_state_mut() else {
            return;
        };
        let Some(ui) = &mut state.player_ui else {
            return;
        };
        if !ui.scrub.dragging {
            return;
        }
        ui.scrub.dragging = false;
        ui.scrub.last_fast_seek_at = None;
        if let Some(frac) = ui.scrub.hover_frac {
            let target = (frac as f64 * ui.duration_secs).max(0.0);
            // The live fast seeks during the drag already walked the
            // decoder near this point, so the exact (`hr-seek`) landing
            // seek here is cheap even far into a long file. Known
            // exception: MPEG-TS sources with no Cues/keyframe index still
            // pay a real per-seek scan cost regardless of the hr flag --
            // see `crates/player/src/lib.rs::build_loadfile_options`.
            let _ = video.player().seek_absolute(target);
        }
        ui.note_activity();
        cx.notify();
    }

    /// Shared by `handle_playback_keystroke`'s Left/Right arrow handler and
    /// the OSD's skip ±10s icon buttons (§1.14 rows 4/5) -- fast (keyframe)
    /// seek plus the center transient skip flash (§1.9). Same MPEG-TS
    /// exception as `commit_scrub`: see `build_loadfile_options`.
    pub(crate) fn seek_delta(&mut self, delta: f64, cx: &mut Context<Self>) {
        let target = match &self.screen {
            Screen::Main(state) => state
                .player_ui
                .as_ref()
                .map(|ui| (ui.position_secs + delta).clamp(0.0, ui.duration_secs.max(0.0))),
            Screen::Connect(_) | Screen::Switching => None,
        };
        let _ = match target {
            Some(target) => self.video.player().seek_absolute_fast(target),
            None => self.video.player().seek_relative(delta),
        };
        if let Screen::Main(state) = &mut self.screen {
            if let Some(ui) = &mut state.player_ui {
                // The flash's numeral is the real skip magnitude, not a
                // hardcoded "10" -- see `FlashKind`'s doc comment.
                let secs = delta.abs().round() as u32;
                ui.trigger_flash(if delta < 0.0 {
                    crate::player_ui::FlashKind::SkipBack(secs)
                } else {
                    crate::player_ui::FlashKind::SkipForward(secs)
                });
            }
        }
        self.note_osd_activity(cx);
    }

    /// OSD prev/next-chapter buttons (§1.14 rows 6/7). Jumps to the
    /// nearest chapter boundary strictly before/after the current position
    /// (an epsilon avoids "prev" re-selecting the current chapter); "prev"
    /// before the first chapter seeks to the start instead of no-op'ing.
    pub(crate) fn jump_chapter(&mut self, forward: bool, cx: &mut Context<Self>) {
        let video = self.video.clone();
        let Some(state) = self.main_state_mut() else {
            return;
        };
        let Some(ui) = &mut state.player_ui else {
            return;
        };
        let pos = ui.position_secs;
        let target = if forward {
            ui.chapters
                .iter()
                .map(|(secs, _)| *secs)
                .find(|secs| *secs > pos + 0.5)
        } else {
            ui.chapters
                .iter()
                .map(|(secs, _)| *secs)
                .rev()
                .find(|secs| *secs < pos - 0.5)
                .or(Some(0.0))
        };
        if let Some(target) = target {
            let _ = video.player().seek_absolute_fast(target.max(0.0));
        }
        ui.note_activity();
        cx.notify();
    }

    /// Scrubber chapter-tick click (§1.16: "clicking a tick to jump isn't
    /// wired either, worth doing in the same pass") -- jumps straight to
    /// that chapter's start.
    pub(crate) fn seek_to_click(&mut self, secs: f64, cx: &mut Context<Self>) {
        let _ = self.video.player().seek_absolute_fast(secs.max(0.0));
        self.note_osd_activity(cx);
    }

    /// §1.5: the scrubber row's right-hand time label toggles between
    /// `-remaining` and `duration` on click, persisted globally (not
    /// per-item) via `player_prefs.rs`.
    pub(crate) fn toggle_remaining_display(&mut self, cx: &mut Context<Self>) {
        let Some(state) = self.main_state_mut() else {
            return;
        };
        let next = state.track_prefs.remaining_display().toggled();
        state.track_prefs.set_remaining_display(next);
        if let Some(ui) = &mut state.player_ui {
            ui.remaining_display = next;
            ui.note_activity();
        }
        cx.notify();
    }

    /// §1.6: hovering the volume control expands the slider and exempts
    /// the OSD from idle auto-hide; collapse is deferred 400ms
    /// (`tick_volume_collapse`) so crossing off mid-drag doesn't collapse
    /// it under the pointer.
    pub(crate) fn set_volume_hover(&mut self, hovering: bool, cx: &mut Context<Self>) {
        let Some(state) = self.main_state_mut() else {
            return;
        };
        let Some(ui) = &mut state.player_ui else {
            return;
        };
        if hovering {
            ui.volume_expanded = true;
            ui.volume_hover_deadline = None;
            ui.note_activity();
        } else {
            ui.volume_hover_deadline =
                Some(Instant::now() + crate::player_ui::VOLUME_COLLAPSE_DELAY);
        }
        cx.notify();
    }

    /// §1.6 volume slider drag start -- relative-delta based, not
    /// absolute-position, since the control's on-screen x isn't fixed the
    /// way the scrubber's is (see `volume_drag_anchor`'s doc comment).
    pub(crate) fn start_volume_drag(&mut self, x: f32, cx: &mut Context<Self>) {
        let Some(state) = self.main_state_mut() else {
            return;
        };
        let Some(ui) = &mut state.player_ui else {
            return;
        };
        let start_vol = if ui.muted { 0 } else { ui.volume };
        ui.volume_drag_anchor = Some((x, start_vol));
        ui.muted = false;
        ui.note_activity();
        cx.notify();
    }

    pub(crate) fn drag_volume(&mut self, x: f32, cx: &mut Context<Self>) {
        let video = self.video.clone();
        let Some(state) = self.main_state_mut() else {
            return;
        };
        let Some(ui) = &mut state.player_ui else {
            return;
        };
        let Some((anchor_x, anchor_vol)) = ui.volume_drag_anchor else {
            return;
        };
        let delta_pct = (x - anchor_x) / crate::player_ui::VOLUME_SLIDER_PX * 100.0;
        let vol = (anchor_vol as f32 + delta_pct).clamp(0.0, 100.0).round() as u8;
        ui.volume = vol;
        let _ = video.player().set_volume(vol);
        ui.note_activity();
        cx.notify();
    }

    pub(crate) fn end_volume_drag(&mut self, cx: &mut Context<Self>) {
        let Some(state) = self.main_state_mut() else {
            return;
        };
        if let Some(ui) = &mut state.player_ui {
            ui.volume_drag_anchor = None;
        }
        cx.notify();
    }

    /// OSD fullscreen icon button (§1.13) -- a mouse handler only has
    /// `cx`, not `Window`, so this leaves the request for `render_main`
    /// (see `MainState::want_toggle_os_fullscreen`'s doc comment).
    pub(crate) fn request_fullscreen_toggle(&mut self, cx: &mut Context<Self>) {
        let Some(state) = self.main_state_mut() else {
            return;
        };
        state.want_toggle_os_fullscreen = true;
        if let Some(ui) = &mut state.player_ui {
            ui.note_activity();
        }
        cx.notify();
    }

    pub(crate) fn toggle_play_pause_click(&mut self, cx: &mut Context<Self>) {
        let paused = {
            let Some(state) = self.main_state() else {
                return;
            };
            !state.paused
        };
        self.set_paused(paused, cx);
        self.note_osd_activity(cx);
        self.publish_now_playing(cx);
    }

    pub(crate) fn select_track(
        &mut self,
        kind: player::TrackKind,
        mpv_id: Option<i64>,
        cx: &mut Context<Self>,
    ) {
        let video = self.video.clone();
        let Some(state) = self.main_state_mut() else {
            return;
        };
        let Some(ui) = &mut state.player_ui else {
            return;
        };
        let _ = video.player().set_track(kind, mpv_id);
        let key = mpv_id.and_then(|id| {
            ui.tracks
                .iter()
                .find(|t| t.mpv_id == id)
                .and_then(crate::player_ui::track_pref_key)
        });
        if let Some(series_id) = ui.series_id.clone() {
            match kind {
                player::TrackKind::Subtitle => state.track_prefs.set_subtitle(&series_id, key),
                _ => state.track_prefs.set_audio(&series_id, key),
            }
        }
        ui.close_picker();
        ui.note_activity();
        cx.notify();
    }

    pub(crate) fn close_picker(&mut self, cx: &mut Context<Self>) {
        let Some(state) = self.main_state_mut() else {
            return;
        };
        if let Some(ui) = &mut state.player_ui {
            ui.close_picker();
        }
        cx.notify();
    }

    /// OSD mute button: same effect as the `M` key.
    pub(crate) fn handle_remote_command_volume_toggle(&mut self, cx: &mut Context<Self>) {
        self.toggle_mute(cx);
    }

    /// OSD "Audio"/"Subtitles" buttons: open the picker directly (mouse
    /// equivalent of Shift+S/Shift+A -- see `handle_playback_keystroke`'s
    /// doc comment).
    pub(crate) fn open_track_picker(&mut self, kind: player::TrackKind, cx: &mut Context<Self>) {
        self.cycle_or_pick_track(kind, true, cx);
    }

    pub(crate) fn toggle_info_overlay(&mut self, cx: &mut Context<Self>) {
        let Some(state) = self.main_state_mut() else {
            return;
        };
        if let Some(ui) = &mut state.player_ui {
            ui.info_overlay = !ui.info_overlay;
            ui.note_activity();
        }
        cx.notify();
    }

    /// docs/UX-SPEC.md §4: click on the Skip Intro/Outro pill seeks past the
    /// segment (the `Ask` path -- see `tick_auto_skip` for `AutoSkip`).
    pub(crate) fn skip_active_segment(&mut self, cx: &mut Context<Self>) {
        let video = self.video.clone();
        let Some(state) = self.main_state_mut() else {
            return;
        };
        let Some(ui) = &mut state.player_ui else {
            return;
        };
        if let Some(seg) = ui.active_segment() {
            let end_secs = seg.end_ticks.unwrap_or(0) as f64 / 10_000_000.0;
            let _ = video.player().seek_absolute(end_secs);
        }
        ui.note_activity();
        cx.notify();
    }

    /// `AutoSkip` path: called every `PlayerEvent::Position` tick (~4Hz) --
    /// seeks past a segment the instant playback enters one, with a
    /// `SKIP_UNDO_WINDOW` "Undo" toast (`U` key -> `undo_last_skip`).
    /// `last_auto_skip_segment` is the idempotency guard: without it this
    /// would refire every tick for the ~250ms the seek takes to move past
    /// the segment, and it deliberately also blocks an immediate re-skip
    /// after Undo.
    pub(crate) fn tick_auto_skip(&mut self, cx: &mut Context<Self>) {
        let (end_secs, resume_secs, seg_key, label) = {
            let Some(state) = self.main_state() else {
                return;
            };
            let Some(ui) = &state.player_ui else {
                return;
            };
            // A counting-down next-up card owns the hand-over at the outro's
            // start; a seek on the dying session would only race it.
            if ui.next_episode.is_some() && ui.next_episode_countdown_total_secs.is_some() {
                return;
            }
            let Some((seg, SegmentDecision::AutoSkip)) = ui.active_segment_decision() else {
                return;
            };
            let seg_key = (seg.start_ticks.unwrap_or(0), seg.end_ticks.unwrap_or(0));
            if ui.last_auto_skip_segment == Some(seg_key) {
                return;
            }
            let end_secs = seg.end_ticks.unwrap_or(0) as f64 / 10_000_000.0;
            let label = crate::player_ui::skip_toast_label(seg.type_).to_string();
            (end_secs, ui.position_secs, seg_key, label)
        };
        let video = self.video.clone();
        let _ = video.player().seek_absolute(end_secs);
        let Some(state) = self.main_state_mut() else {
            return;
        };
        if let Some(ui) = &mut state.player_ui {
            ui.last_auto_skip_segment = Some(seg_key);
            ui.show_skip_toast(label, resume_secs);
            tracing::info!(?seg_key, "JELLYBEAM: auto-skipped segment");
        }
        cx.notify();
    }

    /// `U` key or a click on the auto-skip toast -- seeks back to the
    /// position the skip fired from. Taking `skip_toast` (not just
    /// clearing it) leaves `last_auto_skip_segment` untouched, so
    /// `tick_auto_skip` doesn't immediately re-skip the same segment.
    pub(crate) fn undo_last_skip(&mut self, cx: &mut Context<Self>) {
        let video = self.video.clone();
        let Some(state) = self.main_state_mut() else {
            return;
        };
        let Some(ui) = &mut state.player_ui else {
            return;
        };
        let Some(toast) = ui.skip_toast.take() else {
            return;
        };
        let _ = video.player().seek_absolute(toast.resume_position_secs);
        ui.note_activity();
        cx.notify();
    }

    /// Miniplayer drag-to-corner: `dropped_frac` is the drop point as a
    /// `(x, y)` fraction of the viewport -- snapped to the nearest of the
    /// four corners (docs/UX-SPEC.md §3: "draggable to corners").
    pub(crate) fn drop_miniplayer(&mut self, dropped_frac: (f32, f32), cx: &mut Context<Self>) {
        let video = self.video.clone();
        let Some(state) = self.main_state_mut() else {
            return;
        };
        let Some(ui) = &mut state.player_ui else {
            return;
        };
        let current = match ui.layer_mode {
            crate::gl_video::LayerMode::Miniplayer(c) => c,
            crate::gl_video::LayerMode::FullscreenInWindow => crate::gl_video::Corner::BottomRight,
        };
        let corner = ui.next_corner(current, dropped_frac);
        ui.layer_mode = crate::gl_video::LayerMode::Miniplayer(corner);
        ui.dragging_miniplayer = false;
        video.set_layer_mode(ui.layer_mode);
        cx.notify();
    }

    pub(crate) fn start_miniplayer_drag(&mut self, pos: (f32, f32), cx: &mut Context<Self>) {
        let Some(state) = self.main_state_mut() else {
            return;
        };
        if let Some(ui) = &mut state.player_ui {
            ui.dragging_miniplayer = true;
            ui.drag_moved = false;
            ui.drag_down_pos = Some(pos);
        }
        cx.notify();
    }

    /// Only flips `drag_moved` (and calls `cx.notify()`) once the pointer
    /// has moved past `player_ui::MINIPLAYER_DRAG_THRESHOLD_PX` from the
    /// mouse-down position -- see that const's doc comment for why a
    /// threshold is required at all.
    pub(crate) fn note_miniplayer_drag_move(&mut self, pos: (f32, f32), cx: &mut Context<Self>) {
        let Some(state) = self.main_state_mut() else {
            return;
        };
        let Some(ui) = &mut state.player_ui else {
            return;
        };
        if !ui.dragging_miniplayer || ui.drag_moved {
            return;
        }
        let Some(down) = ui.drag_down_pos else {
            return;
        };
        if crate::player_ui::miniplayer_drag_exceeds_threshold(down, pos) {
            ui.drag_moved = true;
            cx.notify();
        }
    }

    /// Mouse-up on the Miniplayer: a plain click (no intervening move)
    /// restores to Fullscreen-in-window (docs/UX-SPEC.md §3: "click video... on
    /// it"); an actual drag snaps to the nearest corner instead.
    pub(crate) fn end_miniplayer_drag(&mut self, dropped_frac: (f32, f32), cx: &mut Context<Self>) {
        let was_drag = {
            let Some(state) = self.main_state() else {
                return;
            };
            state
                .player_ui
                .as_ref()
                .map(|ui| ui.drag_moved)
                .unwrap_or(false)
        };
        // Field-debug breadcrumb: records how every mouse-up was
        // classified -- `was_drag=true` on an expected-restore click means
        // threshold misclassification.
        tracing::info!(was_drag, "miniplayer mouse-up classified");
        if was_drag {
            self.drop_miniplayer(dropped_frac, cx);
        } else {
            self.restore_from_miniplayer(cx);
        }
        if let Screen::Main(state) = &mut self.screen {
            if let Some(ui) = &mut state.player_ui {
                ui.dragging_miniplayer = false;
                ui.drag_moved = false;
                ui.drag_down_pos = None;
            }
        }
    }

    /// docs/UX-SPEC.md §3: "click video / Return on it" restores from Miniplayer.
    pub(crate) fn restore_from_miniplayer(&mut self, cx: &mut Context<Self>) {
        let video = self.video.clone();
        let Some(state) = self.main_state_mut() else {
            return;
        };
        let Some(ui) = &mut state.player_ui else {
            return;
        };
        ui.layer_mode = crate::gl_video::LayerMode::FullscreenInWindow;
        video.set_layer_mode(ui.layer_mode);
        cx.notify();
    }

    /// docs/UX-SPEC.md §3: `⌘M`/`Tab` toggles Fullscreen-in-window <-> Miniplayer.
    /// Native OS-Fullscreen is untouched -- toggling into Miniplayer while
    /// OS-Fullscreen would be a confusing double-transition (`F` handles
    /// leaving the fullscreen Space).
    pub(crate) fn toggle_miniplayer(&mut self, cx: &mut Context<Self>) {
        let video = self.video.clone();
        let Some(state) = self.main_state_mut() else {
            return;
        };
        let Some(ui) = &mut state.player_ui else {
            return;
        };
        let next = match ui.layer_mode {
            crate::gl_video::LayerMode::Miniplayer(_) => {
                crate::gl_video::LayerMode::FullscreenInWindow
            }
            crate::gl_video::LayerMode::FullscreenInWindow => {
                crate::gl_video::LayerMode::Miniplayer(crate::gl_video::Corner::BottomRight)
            }
        };
        ui.layer_mode = next;
        ui.note_activity();
        video.set_layer_mode(next);
        cx.notify();
    }

    /// Shared by `handle_playback_keystroke`'s Space handler and
    /// `handle_remote_command`'s Play/Pause/Toggle.
    pub(crate) fn set_paused(&mut self, paused: bool, cx: &mut Context<Self>) {
        // Field-debug breadcrumb: logs every app-initiated pause write, so
        // jellybeam.log distinguishes "our code paused mpv" from "mpv paused
        // itself" (no preceding line).
        tracing::info!(paused, "set_paused called");
        let video = self.video.clone();
        let Some(state) = self.main_state_mut() else {
            return;
        };
        state.paused = paused;
        if let Err(e) = video.player().set_paused(paused) {
            tracing::warn!(error = %e, "set_paused failed");
        }
        if let Some(reporting) = state.reporting.as_mut() {
            reporting.on_pause(paused);
        }
        // §1.9: center play/pause flash -- every path that changes pause
        // state funnels through here, so this is the one place to trigger it.
        if let Some(ui) = &mut state.player_ui {
            ui.trigger_flash(if paused {
                crate::player_ui::FlashKind::Pause
            } else {
                crate::player_ui::FlashKind::Play
            });
        }
        cx.notify();
    }

    // ---- Option-key speed hold ------------------------------------------

    /// Gate for whether the Option-key speed-hold monitor should act on a
    /// `flagsChanged` sample, rather than treating it as ordinary modifier
    /// noise:
    /// - The `speed_boost` setting must be on.
    /// - Only while Fullscreen-in-window/OS-Fullscreen owns the keyboard --
    ///   never in the Miniplayer or plain Browse.
    /// - Never while the shortcuts overlay or the S/A track picker is open.
    fn should_engage_option_speed_hold(&self) -> bool {
        if !self.app_settings.speed_boost {
            return false;
        }
        let Some(state) = self.main_state() else {
            return false;
        };
        let in_fullscreen_player = matches!(state.mode, ContentMode::Playing { .. })
            && !matches!(
                state.player_ui.as_ref().map(|ui| ui.layer_mode),
                Some(crate::gl_video::LayerMode::Miniplayer(_))
            );
        if !in_fullscreen_player || state.shortcuts_overlay_open {
            return false;
        }
        !matches!(&state.player_ui, Some(ui) if ui.picker.is_some())
    }

    /// Entry point for every `flagsChanged` sample the native NSEvent
    /// monitor forwards (bridged in via `main.rs::spawn_option_speed_task`).
    /// Delegates to `option_speed_hold::decide_speed_rate`, a pure function
    /// of the raw flags plus `should_engage_option_speed_hold`'s gate, kept
    /// separate so it's unit-testable without GPUI/AppKit machinery.
    pub(crate) fn handle_option_flags_sample(&mut self, raw_flags: u64, cx: &mut Context<Self>) {
        let gate_open = self.should_engage_option_speed_hold();
        let rate = crate::option_speed_hold::decide_speed_rate(raw_flags, gate_open);
        self.apply_option_speed_rate(rate, cx);
    }

    /// Applies a decided rate (`1.0`, `0.5`, or `2.0`), pushing it into
    /// both mpv and the OSD chip. Idempotent: a sample that doesn't change
    /// the decided rate is a no-op. `1.0` routes through
    /// `reset_speed_boost` so every "back to normal" path funnels through
    /// the one place that resets mpv's `speed`, even with no
    /// `PlayerUiState` to carry a chip.
    fn apply_option_speed_rate(&mut self, rate: f64, cx: &mut Context<Self>) {
        if (rate - 1.0).abs() < f64::EPSILON {
            self.reset_speed_boost(cx);
            return;
        }
        let Some(state) = self.main_state_mut() else {
            return;
        };
        let Some(ui) = &mut state.player_ui else {
            return;
        };
        if ui.speed_boost_rate == Some(rate) {
            return;
        }
        ui.speed_boost_rate = Some(rate);
        ui.note_activity();
        if let Err(e) = self.video.player().set_speed(rate) {
            tracing::warn!(error = %e, rate, "set_speed for option-key speed hold failed");
        }
        cx.notify();
    }

    /// Restores mpv's playback rate to 1.0 and clears the OSD chip,
    /// regardless of whether a `PlayerUiState` exists. mpv's `speed` is
    /// per-player and survives `stop()`/`player.load()` with no automatic
    /// reset -- called from `apply_option_speed_rate`, `stop_playback`,
    /// `handle_playback_outcome`'s success arm, and `main.rs`'s
    /// window-activation hook (losing key-window status stops the NSEvent
    /// monitor, so a hold outliving focus loss needs this as its release).
    pub(crate) fn reset_speed_boost(&mut self, cx: &mut Context<Self>) {
        if let Err(e) = self.video.player().set_speed(1.0) {
            tracing::warn!(error = %e, "reset_speed_boost: set_speed(1.0) failed");
        }
        let Some(state) = self.main_state_mut() else {
            return;
        };
        if let Some(ui) = &mut state.player_ui {
            if ui.speed_boost_rate.take().is_some() {
                ui.note_activity();
                cx.notify();
            }
        }
    }

    /// docs/UX-SPEC.md §2's Player row, dispatched whenever `ContentMode::Playing`
    /// and the layer is Fullscreen-in-window/OS-Fullscreen (Miniplayer
    /// routes to the normal Browse dispatch -- see `handle_global_keystroke`).
    /// `S`/`A`'s "hold or repeat opens picker" is approximated as
    /// Shift+S/Shift+A since this event stream is key-down only.
    #[allow(clippy::too_many_arguments)] // plain data parameters, no natural grouping.
    pub(crate) fn handle_playback_keystroke(
        &mut self,
        key: &str,
        key_char: Option<&str>,
        cmd: bool,
        shift: bool,
        other_mods: bool,
        window: &gpui::Window,
        cx: &mut Context<Self>,
    ) {
        // Part B §9: while the S/A picker is open it owns the keyboard,
        // same modal precedent as `handle_search_keystroke`'s Search
        // overlay -- must run before the `key == "escape"` case below, or
        // Esc would stop playback instead of closing the popover.
        if !cmd {
            let picker_open = matches!(&self.screen, Screen::Main(state)
                if matches!(&state.player_ui, Some(ui) if ui.picker.is_some()));
            if picker_open && self.handle_picker_keystroke(key, key_char, other_mods, cx) {
                return;
            }
        }
        if key == "space" {
            let paused = {
                let Some(state) = self.main_state() else {
                    return;
                };
                !state.paused
            };
            self.set_paused(paused, cx);
            self.note_osd_activity(cx);
            self.publish_now_playing(cx);
            return;
        }
        if key == "escape" {
            // While the next-episode card is up, the first Esc dismisses
            // it instead of stepping down a layer; dismissing clears
            // `ui.next_episode`, so a second Esc falls through normally.
            let card_showing = matches!(&self.screen, Screen::Main(state)
                if matches!(&state.player_ui, Some(ui) if ui.next_episode.is_some()));
            if card_showing {
                self.dismiss_next_episode_card(cx);
                return;
            }
            // Esc steps down one layer at a time (OS-Fullscreen ->
            // Fullscreen-in-window -> Miniplayer) rather than stopping
            // playback outright; Miniplayer + Esc still stops (no lower
            // layer left). Stop remains reachable via the OSD stop button
            // or the Miniplayer's close button.
            if window.is_fullscreen() {
                // OS-Fullscreen -> Fullscreen-in-window; playback untouched.
                window.toggle_fullscreen();
                self.note_osd_activity(cx);
                return;
            }
            // Fullscreen-in-window -> Miniplayer: playback continues,
            // `toggle_miniplayer` handles the transition.
            self.toggle_miniplayer(cx);
            return;
        }
        // `U` undoes the most recent auto-skip while its toast is up
        // (`undo_last_skip` itself checks `skip_toast.is_some()`).
        if key == "u" && !cmd {
            self.undo_last_skip(cx);
            return;
        }
        if key == "f" && !cmd {
            window.toggle_fullscreen();
            let video = self.video.clone();
            let now_fullscreen = window.is_fullscreen();
            if let Screen::Main(state) = &mut self.screen {
                if let Some(ui) = &mut state.player_ui {
                    // OS-Fullscreen and Fullscreen-in-window share the same
                    // target geometry (see `LayerMode`'s doc comment), so
                    // this only needs to leave a Miniplayer corner.
                    ui.layer_mode = crate::gl_video::LayerMode::FullscreenInWindow;
                    video.set_layer_mode(ui.layer_mode);
                }
            }
            tracing::info!(now_fullscreen, "f: OS-fullscreen toggled");
            self.note_osd_activity(cx);
            return;
        }
        if key == "left" || key == "right" {
            // The un-shifted arrow follows the configured skip length;
            // Shift+arrow stays a fixed ±60s "big skip", deliberately not
            // configurable, so it reads as distinctly bigger (see
            // `settings::SkipLengthPrefs`'s doc comment).
            let magnitude = if shift {
                60.0
            } else {
                let Some(state) = self.main_state() else {
                    return;
                };
                let secs = match &state.player_ui {
                    Some(ui) if key == "left" => ui.skip_back_secs,
                    Some(ui) => ui.skip_forward_secs,
                    None => 10,
                };
                f64::from(secs)
            };
            let delta = if key == "left" { -magnitude } else { magnitude };
            self.seek_delta(delta, cx);
            return;
        }
        if key == "m" && !cmd {
            self.toggle_mute(cx);
            return;
        }
        if key == "up" || key == "down" {
            self.nudge_volume(if key == "up" { 5 } else { -5 }, cx);
            return;
        }
        if key == "s" {
            self.cycle_or_pick_track(player::TrackKind::Subtitle, shift, cx);
            return;
        }
        if key == "a" {
            self.cycle_or_pick_track(player::TrackKind::Audio, shift, cx);
            return;
        }
        if key == "i" {
            let Some(state) = self.main_state_mut() else {
                return;
            };
            if let Some(ui) = &mut state.player_ui {
                ui.info_overlay = !ui.info_overlay;
                ui.note_activity();
            }
            cx.notify();
            return;
        }
        // §2.4: episode-level prev/next uses `[`/`]`, not the spec's
        // `⇧←`/`⇧→` -- that's already the ±60s seek binding above.
        if key == "[" && !cmd {
            self.play_adjacent_episode(EpisodeStep::Prev, cx);
            self.note_osd_activity(cx);
            return;
        }
        if key == "]" && !cmd {
            self.play_adjacent_episode(EpisodeStep::Next, cx);
            self.note_osd_activity(cx);
            return;
        }
        // §2.4's next-episode card: Return/click plays next. A no-op
        // (`play_next_episode_now` itself checks) when the card isn't up.
        if (key == "enter" || key == "return") && !cmd {
            self.play_next_episode_now(cx);
        }
    }

    /// Part B §9's popover nav for the S/A picker: Up/Down move the
    /// highlight, Return activates it, Esc closes, typing edits the filter
    /// (past `PICKER_FILTER_THRESHOLD` tracks). Consumes the key whenever
    /// the picker is open, so the caller never falls through.
    fn handle_picker_keystroke(
        &mut self,
        key: &str,
        key_char: Option<&str>,
        other_mods: bool,
        cx: &mut Context<Self>,
    ) -> bool {
        let kind = {
            let Some(state) = self.main_state() else {
                return false;
            };
            let Some(ui) = &state.player_ui else {
                return false;
            };
            let Some(picker) = ui.picker else {
                return false;
            };
            match picker {
                crate::player_ui::PickerKind::Audio => player::TrackKind::Audio,
                crate::player_ui::PickerKind::Subtitle => player::TrackKind::Subtitle,
            }
        };
        match key {
            "escape" => {
                if let Screen::Main(state) = &mut self.screen {
                    if let Some(ui) = &mut state.player_ui {
                        ui.close_picker();
                        ui.note_activity();
                    }
                }
                cx.notify();
                true
            }
            "up" | "down" => {
                if let Screen::Main(state) = &mut self.screen {
                    if let Some(ui) = &mut state.player_ui {
                        let row_count =
                            crate::player_ui::filtered_tracks(&ui.tracks, kind, &ui.picker_filter)
                                .len();
                        let delta = if key == "up" { -1 } else { 1 };
                        ui.picker_move_highlight(delta, row_count);
                        ui.note_activity();
                    }
                }
                cx.notify();
                true
            }
            "enter" | "return" => {
                let mpv_id = {
                    let Some(state) = self.main_state() else {
                        return true;
                    };
                    let Some(ui) = &state.player_ui else {
                        return true;
                    };
                    ui.picker_highlight.and_then(|ix| {
                        crate::player_ui::filtered_tracks(&ui.tracks, kind, &ui.picker_filter)
                            .get(ix)
                            .map(|t| t.mpv_id)
                    })
                };
                if let Some(mpv_id) = mpv_id {
                    self.select_track(kind, Some(mpv_id), cx);
                }
                true
            }
            "backspace" => {
                if let Screen::Main(state) = &mut self.screen {
                    if let Some(ui) = &mut state.player_ui {
                        let track_count = ui.tracks.iter().filter(|t| t.kind == kind).count();
                        if track_count > crate::player_ui::PICKER_FILTER_THRESHOLD {
                            ui.picker_backspace();
                        }
                    }
                }
                cx.notify();
                true
            }
            _ => {
                if !other_mods {
                    if let Some(ch) = key_char {
                        if !ch.is_empty() && ch.chars().all(|c| !c.is_control()) {
                            if let Screen::Main(state) = &mut self.screen {
                                if let Some(ui) = &mut state.player_ui {
                                    let track_count =
                                        ui.tracks.iter().filter(|t| t.kind == kind).count();
                                    if track_count > crate::player_ui::PICKER_FILTER_THRESHOLD {
                                        ui.picker_type_char(ch);
                                    }
                                }
                            }
                            cx.notify();
                        }
                    }
                }
                true
            }
        }
    }

    /// The shared "OSD activity" entry point -- both keyboard and mouse
    /// (`player_ui.rs::render_osd`'s `on_mouse_move`) funnel into this one
    /// fn, so there's exactly one place that decides what counts as
    /// activity. `pub(crate)` so `JELLYBEAM_E2E` can call it directly: GPUI
    /// has no central window-level mouse observer to inject a raw
    /// mouse-move through, unlike `handle_global_keystroke` for keys.
    pub(crate) fn note_osd_activity(&mut self, cx: &mut Context<Self>) {
        let Some(state) = self.main_state_mut() else {
            return;
        };
        if let Some(ui) = &mut state.player_ui {
            ui.note_activity();
        }
        self.sync_subtitle_baseline();
        cx.notify();
    }

    /// Lift mpv-rendered subtitles above the control bar whenever
    /// the OSD is visible, restore when it hides. One `sub-pos` push per
    /// transition (`last_subtitle_sync` dedupes on `(osd_visible,
    /// layer_mode)`, not visibility alone, so a miniplayer<->fullscreen
    /// swap or fresh playback start re-pushes rather than inheriting a
    /// stale value), applied from both places OSD visibility can change:
    /// `note_osd_activity` and `on_osd_tick`. The value comes from
    /// `player_ui::effective_subtitle_pos`, floored at
    /// `settings::SUBTITLE_POS_FLOOR` so subtitles can't land in the top
    /// half of the frame.
    fn sync_subtitle_baseline(&mut self) {
        let style_base = self.app_settings.subtitle;
        let video = self.video.clone();
        let Some(state) = self.main_state_mut() else {
            return;
        };
        if !matches!(state.mode, ContentMode::Playing { .. }) {
            return;
        }
        let Some(ui) = &mut state.player_ui else {
            return;
        };
        let key = (ui.osd_visible, ui.layer_mode);
        if ui.last_subtitle_sync == Some(key) {
            return;
        }
        ui.last_subtitle_sync = Some(key);
        // The miniplayer has no OSD control zone over the video (its own
        // hover chrome is tiny and sits inside the floating rect), so the
        // control-bar shift only applies in the fullscreen-in-window layer.
        let controls_over_video = matches!(
            ui.layer_mode,
            crate::gl_video::LayerMode::FullscreenInWindow
        ) && ui.osd_visible;
        let mut style = style_base.to_player();
        style.pos = Some(crate::player_ui::effective_subtitle_pos(
            style_base.pos,
            controls_over_video,
        ));
        if let Err(e) = video.player().set_subtitle_style(&style) {
            tracing::warn!(error = %e, "subtitle baseline sync failed");
        }
    }

    pub(crate) fn stop_playback(&mut self, cx: &mut Context<Self>) {
        // mpv's `speed` is per-player, not per-file, and survives `stop()`
        // with no automatic reset -- called before the early `Browse`
        // return since it must run even when the next thing to play (e.g.
        // a preload) skips `handle_playback_outcome`'s own reset.
        self.reset_speed_boost(cx);
        let video = self.video.clone();
        let runtime = self.runtime.clone();
        let last_ticks = self.last_position_ticks.load(Ordering::Relaxed);

        let Screen::Main(state) = &mut self.screen else {
            return;
        };
        // Loading counts as stoppable: a stop during an in-flight load must
        // both cancel the visible loading state and invalidate the flow's
        // eventual outcome (see `playback_generation`).
        if matches!(state.mode, ContentMode::Browse) {
            return;
        }
        // docs/PLUGIN-CHANNELS.md §2.2: a channel
        // recording never pushes a `Nav` entry, so returning from playback
        // doesn't already flow through `ensure_view_loaded` via
        // `nav.back()` -- captured now, acted on after bookkeeping below.
        let return_to_channel = matches!(state.nav.current, View::Channel { .. });
        state.playback_generation.fetch_add(1, Ordering::Relaxed);
        // A stop during an in-flight load must actually cancel that load,
        // not just invalidate its outcome -- an un-aborted `run()` reaches
        // `player.load()` after `player.stop()` below and silently
        // resurrects playback.
        if let Some(handle) = state.playback_task.take() {
            handle.abort();
        }
        state.loading_stage_rx = None;
        if let Err(e) = video.player().stop() {
            tracing::warn!(error = %e, "player.stop failed");
        }
        // docs/UX-SPEC.md: stop must show black, not a frozen frame -- the GL
        // front buffer still holds the last swapped-in frame until
        // something paints over it (see `clear_to_black`'s doc comment).
        video.clear_to_black();
        if let Some(reporting) = state.reporting.take() {
            runtime.spawn(async move {
                reporting.stop(last_ticks).await;
            });
        }
        state.display_sleep = None;
        // Same optimistic mirror update as the server-side stop report
        // above -- see `Mirror::apply_local_user_data`'s doc comment.
        if let Some(item_id) = state.playing_item_id.clone() {
            let mirror = state.mirror.clone();
            runtime.spawn(async move {
                mirror
                    .apply_local_user_data(&item_id, last_ticks, None)
                    .await;
            });
        }
        if let Some(handle) = state.hwdec_log_handle.take() {
            handle.abort();
        }
        if let Some(handle) = state.diag_log_handle.take() {
            handle.abort();
        }
        // Nothing is playing from here on, so the global position counter
        // no longer describes anything -- clear it so a later
        // `prepare_for_quit` or stop-during-load can't stamp this item's
        // position onto a different one. See `play_item`'s identical reset.
        self.last_position_ticks.store(0, Ordering::Relaxed);
        video.set_layer_mode(crate::gl_video::LayerMode::FullscreenInWindow);
        state.mode = ContentMode::Browse;
        state.playing_item_id = None;
        // Paused breadth syncs may resume (see Mirror::set_playback_active).
        state.set_playback_active(false);
        state.player_ui = None;
        // Same hygiene as `play_item_inner`: nothing is playing, so a
        // stashed `Loaded` no longer describes anything.
        state.pending_loaded = None;
        if let Some(np) = &self.now_playing {
            np.clear();
        }
        tracing::info!(position_ticks = last_ticks, "escape: playback stopped");
        cx.notify();
        if return_to_channel {
            // No mirror change event exists for channel content -- a
            // just-watched (or server-expired) recording needs a live re-list.
            self.ensure_view_loaded(cx);
        }
    }

    // ---- Episode navigation (§2.4) --------------------------------------

    /// §2.4: OSD breadcrumb click -> series Detail, pre-selected to the
    /// currently-playing episode's season. Collapses to Miniplayer first
    /// (see `collapse_fullscreen_player_for_nav`'s doc comment), since the
    /// breadcrumb is part of the fullscreen player's own UI. No-op when
    /// nothing's playing or the item has no known series.
    pub(crate) fn open_series_from_player(&mut self, cx: &mut Context<Self>) {
        let Some(state) = self.main_state() else {
            return;
        };
        let Some(ui) = &state.player_ui else {
            return;
        };
        let Some(series_id) = ui.series_id.clone() else {
            return;
        };
        let season_number = ui.season_number;
        self.collapse_fullscreen_player_for_nav(cx);
        self.open_detail_at_season(series_id, season_number, cx);
    }

    /// §2.4: episode-level prev/next during playback (`[`/`]`, not the
    /// spec's `⇧←`/`⇧→`, which is already the ±60s seek binding). No-op
    /// when nothing's playing, the item has no series/season context, or
    /// there's no adjacent episode.
    pub(crate) fn play_adjacent_episode(&mut self, step: EpisodeStep, cx: &mut Context<Self>) {
        let Some(state) = self.main_state() else {
            return;
        };
        let Some(ui) = &state.player_ui else {
            return;
        };
        let (Some(series_id), Some(season_id)) = (ui.series_id.clone(), ui.season_id.clone())
        else {
            return;
        };
        let item_id = ui.item_id.clone();
        let mirror = state.mirror.clone();
        let Some(target) = adjacent_episode(&mirror, &series_id, &season_id, &item_id, step) else {
            return;
        };
        self.play_item(target.id, target.name, cx);
    }

    /// §2.4's dismissible next-episode card: "Dismiss" -- clears the card
    /// for the rest of this session (`next_episode_dismissed` stops
    /// `tick_next_episode` from re-showing it or auto-advancing at EOF).
    pub(crate) fn dismiss_next_episode_card(&mut self, cx: &mut Context<Self>) {
        let Some(state) = self.main_state_mut() else {
            return;
        };
        let Some(ui) = &mut state.player_ui else {
            return;
        };
        ui.next_episode = None;
        ui.next_episode_dismissed = true;
        ui.next_episode_shown_at = None;
        ui.next_episode_countdown_total_secs = None;
        cx.notify();
    }

    /// §2.4's dismissible next-episode card: "Play now" -- jumps ahead of
    /// `tick_next_episode`'s own EOF auto-advance.
    pub(crate) fn play_next_episode_now(&mut self, cx: &mut Context<Self>) {
        let Some(state) = self.main_state() else {
            return;
        };
        let Some(ui) = &state.player_ui else {
            return;
        };
        let Some(next) = ui.next_episode.clone() else {
            return;
        };
        self.play_item(next.id, next.name, cx);
    }

    /// §2.4's "play next episode" affordance, on every ~4Hz `Position`
    /// tick. Phase 1: once remaining time enters the trigger window
    /// (`next_episode_trigger_remaining_secs`, credits-aware) cache
    /// `ui.next_episode`; with autoplay on, size the countdown to the
    /// playable time (`next_episode_playable_secs`) and arm the hand-over
    /// timer, or advance at once with no card when the playhead is already
    /// inside auto-skipped credits (a seek landed there). Phase 2 is the
    /// backstop: advance if the timer somehow missed or real EOF arrives
    /// first. Autoplay off leaves the card up with no countdown.
    pub(crate) fn tick_next_episode(&mut self, cx: &mut Context<Self>) {
        let Some(state) = self.main_state() else {
            return;
        };
        let Some(ui) = &state.player_ui else {
            return;
        };
        if ui.duration_secs <= 0.0 || ui.next_episode_dismissed {
            return;
        }
        let remaining = ui.duration_secs - ui.position_secs;

        if let Some(next) = ui.next_episode.clone() {
            if !ui.autoplay_prefs.enabled {
                return;
            }
            let countdown_done = match (
                ui.next_episode_shown_at,
                ui.next_episode_countdown_total_secs,
            ) {
                (Some(shown_at), Some(total)) => {
                    crate::player_ui::next_episode_countdown_remaining(
                        total,
                        shown_at.elapsed().as_secs_f64(),
                    ) <= 0.0
                }
                _ => false,
            };
            if countdown_done || remaining <= EOF_EPSILON_SECS {
                self.play_item(next.id, next.name, cx);
            }
            return;
        }

        let autoplay_enabled = ui.autoplay_prefs.enabled;
        let delay_secs = ui.autoplay_prefs.delay_secs as f64;
        let outro_start = ui.outro_segment_start_secs();
        let outro_auto_skip = ui.outro_auto_skip();
        let trigger_remaining = crate::player_ui::next_episode_trigger_remaining_secs(
            ui.duration_secs,
            outro_start,
            outro_auto_skip,
            delay_secs,
        );
        if remaining > trigger_remaining {
            return;
        }
        let (Some(series_id), Some(season_id)) = (ui.series_id.clone(), ui.season_id.clone())
        else {
            return;
        };
        let item_id = ui.item_id.clone();
        let mirror = state.mirror.clone();
        let Some(candidate) =
            adjacent_episode(&mirror, &series_id, &season_id, &item_id, EpisodeStep::Next)
        else {
            return;
        };
        let playable = crate::player_ui::next_episode_playable_secs(
            remaining,
            ui.position_secs,
            outro_start,
            outro_auto_skip,
        );
        if autoplay_enabled && outro_auto_skip && outro_start.is_some() && playable <= 0.0 {
            self.play_item(candidate.id, candidate.name, cx);
            return;
        }
        let total = autoplay_enabled
            .then(|| crate::player_ui::next_episode_countdown_total(playable, delay_secs));
        self.next_episode_handover_gen = self.next_episode_handover_gen.wrapping_add(1);
        let generation = self.next_episode_handover_gen;
        let Some(state) = self.main_state_mut() else {
            return;
        };
        if let Some(ui) = &mut state.player_ui {
            ui.next_episode = Some(candidate);
            ui.next_episode_shown_at = Some(Instant::now());
            ui.next_episode_countdown_total_secs = total;
            ui.next_episode_generation = generation;
        }
        if let Some(total) = total {
            self.arm_next_episode_handover(generation, total, cx);
        }
        cx.notify();
    }

    /// One-shot timer for the countdown's hand-over instant, so the empty
    /// rule, `0S` and the next episode's start coincide instead of waiting
    /// for the ~4Hz tick. `generation` guards against a card dismissed,
    /// replaced or outlived by a new session before it fires.
    fn arm_next_episode_handover(&self, generation: u64, total_secs: f64, cx: &Context<Self>) {
        let delay = std::time::Duration::from_secs_f64(total_secs.max(0.0));
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(delay).await;
            let _ = this.update(cx, |root, cx| {
                root.next_episode_countdown_elapsed(generation, cx)
            });
        })
        .detach();
    }

    /// The hand-over itself; a no-op unless the same card is still counting
    /// down in a live, unpaused session (a pause re-arms it on resume, see
    /// `note_pause_for_next_episode`).
    fn next_episode_countdown_elapsed(&mut self, generation: u64, cx: &mut Context<Self>) {
        let Some(state) = self.main_state() else {
            return;
        };
        if !matches!(state.mode, ContentMode::Playing { .. }) || state.paused {
            return;
        }
        let Some(ui) = &state.player_ui else {
            return;
        };
        if ui.next_episode_generation != generation
            || ui.next_episode_dismissed
            || ui.next_episode_countdown_total_secs.is_none()
        {
            return;
        }
        let Some(next) = ui.next_episode.clone() else {
            return;
        };
        self.play_item(next.id, next.name, cx);
    }

    /// Pause freezes the countdown: the card's clock is wall time, so on
    /// resume `next_episode_shown_at` moves forward by the pause length and
    /// the hand-over timer is re-armed under a fresh generation (the timer
    /// that fired mid-pause bailed, one still pending must not fire early).
    pub(crate) fn note_pause_for_next_episode(&mut self, paused: bool, cx: &mut Context<Self>) {
        let Some(state) = self.main_state_mut() else {
            return;
        };
        let Some(ui) = &mut state.player_ui else {
            return;
        };
        if ui.next_episode.is_none() || ui.next_episode_countdown_total_secs.is_none() {
            return;
        }
        if paused {
            ui.next_episode_paused_at = Some(Instant::now());
            return;
        }
        let Some(paused_at) = ui.next_episode_paused_at.take() else {
            return;
        };
        let pause_len = paused_at.elapsed();
        if let Some(shown_at) = &mut ui.next_episode_shown_at {
            *shown_at += pause_len;
        }
        let remaining = match (
            ui.next_episode_shown_at,
            ui.next_episode_countdown_total_secs,
        ) {
            (Some(shown_at), Some(total)) => crate::player_ui::next_episode_countdown_remaining(
                total,
                shown_at.elapsed().as_secs_f64(),
            ),
            _ => return,
        };
        self.next_episode_handover_gen = self.next_episode_handover_gen.wrapping_add(1);
        let generation = self.next_episode_handover_gen;
        if let Some(ui) = self.main_state_mut().and_then(|s| s.player_ui.as_mut()) {
            ui.next_episode_generation = generation;
        }
        self.arm_next_episode_handover(generation, remaining, cx);
    }

    /// Called from `main.rs`'s `cx.on_app_quit` handler. GPUI gives a
    /// short grace window (`gpui::SHUTDOWN_TIMEOUT`, 100ms) before exit, so
    /// this does synchronous teardown (stop mpv, take the reporting
    /// session, persist the pending stop report) on the main thread, then
    /// hands the async work back as a future to await within that window.
    /// The on-disk report is the durable delivery path -- network delivery
    /// in the window is a LAN-only bonus. Returns `None` when nothing was
    /// playing.
    pub(crate) fn prepare_for_quit(&mut self) -> Option<impl std::future::Future<Output = ()>> {
        let last_ticks = self.last_position_ticks.load(Ordering::Relaxed);
        let video = self.video.clone();
        let runtime = self.runtime.clone();
        let state = self.main_state_mut()?;
        if let Some(handle) = state.hwdec_log_handle.take() {
            handle.abort();
        }
        if let Some(handle) = state.diag_log_handle.take() {
            handle.abort();
        }
        // Aborted before the `reporting.take()?` early return below --
        // an idle quit takes that return, exactly when the poster warmer
        // is most likely to be mid-pass.
        state.image_warm.abort();
        // Aborted before `reporting.take()?` below -- a still-loading
        // session has no `ReportingSession` yet, and that's exactly when
        // an un-aborted `run()` task holds an `Arc<Player>` clone into
        // `VideoLayer::drop`'s teardown.
        if let Some(handle) = state.playback_task.take() {
            handle.abort();
        }
        // Same contract for an in-flight dark preload (an idle-with-
        // preload quit also hits the `reporting.take()?` return below).
        if let Some(handle) = state.preload_task.take() {
            handle.abort();
        }
        let reporting = state.reporting.take()?;
        // The one quit-path delivery that is actually durable: persist the
        // final report to disk (local fsync, comfortably inside the grace
        // window) before attempting any network delivery. Replayed by the
        // next launch; deleted there on success or expiry.
        let ctx = reporting.context().clone();
        crate::pending_report::persist(&crate::pending_report::PendingStopReport {
            base_url: state.client.base_url().to_string(),
            item_id: ctx.item_id,
            media_source_id: ctx.media_source_id,
            play_session_id: ctx.play_session_id,
            position_ticks: last_ticks,
            saved_at_unix_secs: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
        });
        // Same optimistic mirror update as `stop_playback` -- captured
        // here so it can ride along in the grace-window future below.
        let mirror_update = state
            .playing_item_id
            .clone()
            .map(|item_id| (state.mirror.clone(), item_id));
        if let Err(e) = video.player().stop() {
            tracing::warn!(error = %e, "player.stop failed during app quit");
        }
        tracing::info!(
            position_ticks = last_ticks,
            "app quit: stopping playback report"
        );
        // Spawn on the long-lived tokio runtime rather than polling the
        // future directly from GPUI's executor -- `ReportingSession`
        // internals need an entered Tokio runtime context, which
        // `Runtime::spawn` guarantees regardless of who awaits the handle.
        let handle = runtime.spawn(async move {
            reporting.stop(last_ticks).await;
        });
        // `apply_local_user_data` alone resolves once the command is
        // *enqueued*, not committed -- awaiting it in the ~100ms grace
        // window proved nothing. `_and_wait` barriers behind the same
        // command, so this await means "committed".
        let mirror_handle = mirror_update.map(|(mirror, item_id)| {
            runtime.spawn(async move {
                mirror
                    .apply_local_user_data_and_wait(&item_id, last_ticks, None)
                    .await;
            })
        });
        Some(async move {
            // Join rather than await serially -- both tasks already run on
            // the runtime, so joining keeps the mirror-commit confirmation
            // from being starved when `reporting.stop` uses the whole
            // grace window.
            match mirror_handle {
                Some(h) => {
                    let _ = tokio::join!(handle, h);
                }
                None => {
                    let _ = handle.await;
                }
            }
        })
    }

    pub(crate) fn toggle_mute(&mut self, cx: &mut Context<Self>) {
        let video = self.video.clone();
        let Some(state) = self.main_state_mut() else {
            return;
        };
        let Some(ui) = &mut state.player_ui else {
            return;
        };
        if ui.muted {
            ui.muted = false;
            let _ = video.player().set_volume(ui.pre_mute_volume);
            ui.show_toast(format!("Volume {}%", ui.pre_mute_volume));
        } else {
            ui.pre_mute_volume = ui.volume;
            ui.muted = true;
            let _ = video.player().set_volume(0);
            ui.show_toast("Muted".to_string());
        }
        ui.note_activity();
        cx.notify();
    }

    fn nudge_volume(&mut self, delta: i32, cx: &mut Context<Self>) {
        let video = self.video.clone();
        let Some(state) = self.main_state_mut() else {
            return;
        };
        let Some(ui) = &mut state.player_ui else {
            return;
        };
        ui.muted = false;
        ui.volume = (ui.volume as i32 + delta).clamp(0, 100) as u8;
        let _ = video.player().set_volume(ui.volume);
        ui.show_toast(format!("Volume {}%", ui.volume));
        ui.note_activity();
        cx.notify();
    }

    /// Plain S/A: cycle to the next track of `kind` and toast its name.
    /// Shift+S/Shift+A: open the full picker instead (see
    /// `handle_playback_keystroke`'s doc comment on the hold-vs-repeat
    /// approximation).
    fn cycle_or_pick_track(
        &mut self,
        kind: player::TrackKind,
        open_picker: bool,
        cx: &mut Context<Self>,
    ) {
        let video = self.video.clone();
        let Some(state) = self.main_state_mut() else {
            return;
        };
        let Some(ui) = &mut state.player_ui else {
            return;
        };
        if open_picker {
            ui.open_picker(match kind {
                player::TrackKind::Subtitle => crate::player_ui::PickerKind::Subtitle,
                _ => crate::player_ui::PickerKind::Audio,
            });
            ui.note_activity();
            cx.notify();
            return;
        }
        let candidates: Vec<&player::Track> = ui.tracks.iter().filter(|t| t.kind == kind).collect();
        if candidates.is_empty() {
            return;
        }
        let current = candidates.iter().position(|t| t.selected).unwrap_or(0);
        let next = candidates[(current + 1) % candidates.len()];
        let label = next
            .title
            .clone()
            .or_else(|| next.lang.clone())
            .unwrap_or_else(|| format!("Track {}", next.mpv_id));
        let mpv_id = next.mpv_id;
        let pref_key = crate::player_ui::track_pref_key(next);
        let series_id = ui.series_id.clone();
        let kind_label = if kind == player::TrackKind::Subtitle {
            "Subtitle"
        } else {
            "Audio"
        };
        ui.show_toast(format!("{kind_label}: {label}"));
        ui.note_activity();
        let _ = video.player().set_track(kind, Some(mpv_id));
        if let (Some(series_id), Some(key)) = (series_id, pref_key) {
            match kind {
                player::TrackKind::Subtitle => {
                    state.track_prefs.set_subtitle(&series_id, Some(key))
                }
                _ => state.track_prefs.set_audio(&series_id, Some(key)),
            }
        }
        cx.notify();
    }
}

/// Whether a dark preload may start: the setting is a hard gate, and only
/// an online session idling in Browse with nothing playing qualifies.
fn preload_blocked(preload_enabled: bool, state: &crate::root::MainState) -> bool {
    !preload_enabled
        || state.offline
        || !matches!(state.mode, ContentMode::Browse)
        || state.playing_item_id.is_some()
}

/// Drops the preload slot and banks what the discarded speculation cost:
/// `fw_bytes` is exactly that until the next `loadfile` replaces the file.
pub(crate) fn discard_preload(
    state: &mut crate::root::MainState,
    player: &player::Player,
    reason: &'static str,
) {
    let Some(p) = state.preload.take() else {
        return;
    };
    let banked = player.cache_state().fw_bytes.unwrap_or(0).max(0);
    state.preload_bytes_wasted += banked;
    tracing::info!(
        item = %p.item_name,
        banked_bytes = banked,
        session_total_wasted_bytes = state.preload_bytes_wasted,
        reason,
        "preload: preload discarded unused"
    );
}
