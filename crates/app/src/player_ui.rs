//! Player OSD state (docs/UX-SPEC.md §2 player keys, §3 miniplayer, §4 scrubber/
//! trickplay, §6 transcode visibility). Rendering lives in this same module
//! (`render_*` free functions near the bottom); the top half is pure state
//! plus small testable arithmetic (`format_time`, idle-timeout check).
//!
//! Rendering follows `docs/DESIGN-PLAYER-NAV.md` Part 1: gradient scrim,
//! Lucide SVG icons, a folded scrubber/time-label row with a buffered-range
//! layer, chapter-tick/prev-next navigation, a hover volume slider, a center
//! transient flash, and "never hide while paused" auto-hide.

use std::time::{Duration, Instant};

use gpui::{
    div, img, linear_color_stop, linear_gradient, point, prelude::*, px, rgb, rgba, svg, Animation,
    AnimationExt, App, Context, Div, FontWeight, IntoElement, MouseButton, Pixels, SharedString,
    Size, WeakEntity,
};
use jellyfin_api::models::{MediaSegmentDto, MediaSegmentType};
use media_cache::{CardRow, ImageKind};
use player::{Track, TrackKind};

use crate::gl_video::{miniplayer_rect_px, Corner, LayerMode};
use crate::image_store::{ImageStore, THUMB_WIDTH};
use crate::playback::PlaybackStarted;
use crate::player_prefs::RemainingDisplay;
use crate::root::Root;
use crate::settings::{AutoplayPrefs, SegmentAction, SkipSegmentPrefs};
use crate::theme;
use crate::trickplay::{TrickplayCache, TrickplayMeta};
use crate::ui::components::{clamped_line, empty_state};
use crate::ui::popover::{click_away_catcher, popover_panel, popover_row, popover_trigger};
use crate::ui::spec_strip::{
    classify, media_breakdown, spec_cell, spec_separator, MediaFacts, MONO_FAMILY,
};
/// Alias to disambiguate from `crate::gl_video::Corner` (the Miniplayer's
/// four-corner snap enum); this one is gpui's own `anchored()` corner type,
/// Part B §9's popover positioning primitive.
use gpui::Corner as AnchorCorner;

/// Gradient scrim height, top and bottom -- 120px per
/// spec, taller than the control content zone below it
/// (`CONTROL_ZONE_HEIGHT`, 88px) so controls sit on a fade rather than a
/// hard edge. `render_top_gradient` reuses this constant so the top and
/// bottom bands stay matched by construction.
const GRADIENT_HEIGHT: f32 = 120.0;
/// Bottom scrim height/peak alpha. Deliberately not
/// `GRADIENT_HEIGHT` -- the time labels fold onto their own row
/// above the track, so the bottom content zone exceeds `CONTROL_ZONE_HEIGHT`
/// and a 120px band would put the fade inside the controls. 0xcc is the
/// spec's 0.8 alpha.
const BOTTOM_GRADIENT_HEIGHT: f32 = 140.0;
const BOTTOM_SCRIM_ALPHA: u8 = 0xcc;
/// §1.13: scrubber row (20) + gap (8) + controls row (44) + bottom padding
/// (16) -- the content zone at the bottom of the gradient. Skip-pill/
/// trickplay overlays anchor just above this, not the full `GRADIENT_HEIGHT`.
const CONTROL_ZONE_HEIGHT: f32 = 88.0;
const SCRUB_MARGIN: f32 = 32.0;
/// mpv's documented `sub-pos` default (see `player::SubtitleStyle::pos`),
/// used when the user hasn't set an explicit override.
const SUBTITLE_POS_DEFAULT: i64 = 100;
/// `sub-pos` percentage-point shift approximating
/// `CONTROL_ZONE_HEIGHT` when controls are visible -- `sub-pos` has no
/// pixel unit, so this is hand-tuned rather than a pixel->percent
/// conversion.
const SUBTITLE_POS_CONTROLS_SHIFT: i64 = 14;
/// §1.5: fixed width per time label. No longer affects `bar_frac` geometry
/// (labels moved to their own row) but keeps the row's box stable
/// as digit count changes (`format_time` crosses the hour boundary
/// mid-playback).
const TIME_LABEL_W: f32 = 60.0;
/// Gap between the time-label row and the track below
/// it. Deliberately tight -- reads as the track's own readout.
const TIME_LABEL_ROW_GAP: f32 = 4.0;

/// Scrub track idle/hover heights, via
/// `group_hover("scrub-bar", ..)` on `scrub_row`'s track div. See
/// `SCRUB_HIT_HEIGHT` for that div's larger invisible hit target.
const SCRUB_TRACK_HEIGHT: f32 = 4.0;
const SCRUB_TRACK_HEIGHT_HOVER: f32 = 6.0;
/// Playhead knob idle/hover diameters.
const SCRUB_KNOB_SIZE: f32 = 10.0;
const SCRUB_KNOB_SIZE_HOVER: f32 = 14.0;
/// The clickable row around the thin visual track is generous even though
/// the track stays thin -- `scrub_row`'s outer `track` div uses this as its
/// height, same shape as a link's padded tap target exceeding its
/// underlined text.
const SCRUB_HIT_HEIGHT: f32 = 24.0;

/// §1.10: hit-target sizes. Primary (play/pause) is the largest control;
/// everything else in the bar is secondary.
const PRIMARY_HIT: f32 = 40.0;
const PRIMARY_ICON: f32 = 24.0;
const SECONDARY_HIT: f32 = 32.0;
const SECONDARY_ICON: f32 = 20.0;

/// §1.6: volume slider's relative-drag scale -- see
/// `PlayerUiState::volume_drag_anchor` for why dragging is delta-based
/// rather than absolute-position based.
pub(crate) const VOLUME_SLIDER_PX: f32 = 60.0;

/// §1.9: center transient flash timing -- fade in over 80ms, hold, fade out
/// over 320ms starting 400ms after it appears (~720ms total).
const FLASH_TOTAL_MS: f32 = 720.0;
const FLASH_FADE_IN_FRAC: f32 = 80.0 / FLASH_TOTAL_MS;
const FLASH_HOLD_END_FRAC: f32 = 400.0 / FLASH_TOTAL_MS;

/// docs/UX-SPEC.md §3's OSD idle fade, 3s. Also gates the cursor hide
/// (`render_playing`'s `CursorStyle::None` on `!osd_visible`) so the cursor
/// and OSD disappear together.
pub(crate) const OSD_IDLE_TIMEOUT: Duration = Duration::from_millis(3000);
/// How long the S/A track-cycle toast stays up.
pub(crate) const TOAST_DURATION: Duration = Duration::from_millis(1500);
/// How long an auto-skip's "Undo" toast stays actionable. Its own constant,
/// not `TOAST_DURATION` -- a skip is easier to regret than a track switch.
pub(crate) const SKIP_UNDO_WINDOW: Duration = Duration::from_millis(5000);
/// Part B §6's `motion.fade` (150ms) as a fraction of `TOAST_DURATION` --
/// `render_toast`'s opacity curve fades in/out over this fraction, same
/// shape as `FLASH_FADE_IN_FRAC`/`FLASH_HOLD_END_FRAC` above.
const TOAST_FADE_FRAC: f32 = 150.0 / 1500.0;
/// §1.6: how long after the pointer leaves the volume control before its
/// slider collapses back to just the mute icon.
pub(crate) const VOLUME_COLLAPSE_DELAY: Duration = Duration::from_millis(400);

/// How often `root.rs::on_position` pushes an optimistic progress update --
/// matches `ReportingSession`'s own report cadence so the mirror drifts no
/// further from the server than the server itself is updated.
pub(crate) const MIRROR_PROGRESS_INTERVAL: Duration = Duration::from_secs(10);

/// §2.4: seconds-remaining threshold for showing the next-episode card --
/// ~15% of runtime, clamped to [3, 30]s (floor keeps short test episodes
/// showing it before EOF; ceiling avoids showing it too early on long
/// movies).
pub(crate) fn next_episode_show_threshold(duration_secs: f64) -> f64 {
    (duration_secs * 0.15).clamp(3.0, 30.0)
}

/// Credits-aware next-up threshold for `Root::tick_next_episode`: the
/// outro's start when known, else `next_episode_show_threshold`. Under
/// `outro_auto_skip` the credits vanish the moment they start, so the card
/// comes forward by `countdown_secs` (the autoplay delay): it appears
/// credits + delay from the end and its countdown runs out where the skip
/// would land. A nonsensical outro start (negative, or at/after duration)
/// falls back to the fixed default.
pub(crate) fn next_episode_trigger_remaining_secs(
    duration_secs: f64,
    outro_start_secs: Option<f64>,
    outro_auto_skip: bool,
    countdown_secs: f64,
) -> f64 {
    match outro_start_secs {
        Some(start) if start >= 0.0 && start < duration_secs => {
            let credits = duration_secs - start;
            if outro_auto_skip {
                credits + countdown_secs.max(0.0)
            } else {
                credits
            }
        }
        _ => next_episode_show_threshold(duration_secs),
    }
}

/// What is left to play before the file ends or the credits are skipped:
/// to the outro start under auto-skip, else to EOF. Sizes the countdown so
/// it runs out exactly at the hand-over instant.
pub(crate) fn next_episode_playable_secs(
    remaining_secs: f64,
    position_secs: f64,
    outro_start_secs: Option<f64>,
    outro_auto_skip: bool,
) -> f64 {
    match outro_start_secs {
        Some(start) if outro_auto_skip => (start - position_secs).max(0.0),
        _ => remaining_secs.max(0.0),
    }
}

/// Whole seconds left for the numeral, ceiling so the first frame reads the
/// full total; `0` once past.
pub(crate) fn next_episode_remaining_whole_secs(total_secs: f64, elapsed_secs: f64) -> u64 {
    (total_secs - elapsed_secs).max(0.0).ceil() as u64
}

/// The card's countdown numeral: under a minute `8S`, else `m:ss`.
pub(crate) fn next_episode_numeral(remaining_whole_secs: u64) -> String {
    if remaining_whole_secs < 60 {
        format!("{remaining_whole_secs}S")
    } else {
        format!(
            "{}:{:02}",
            remaining_whole_secs / 60,
            remaining_whole_secs % 60
        )
    }
}

/// The card's third line: the next item's runtime as `22 MIN`, rounded to
/// the nearest minute; `None` without a runtime.
pub(crate) fn next_episode_runtime_label(runtime_ticks: Option<i64>) -> Option<String> {
    let ticks = runtime_ticks.filter(|t| *t > 0)?;
    let minutes = ((ticks as f64) / 600_000_000.0).round().max(1.0) as i64;
    Some(format!("{minutes} MIN"))
}

/// §2.4: Play Next button's countdown total --
/// `min(remaining, configured delay)`, captured once when the card appears
/// so `total - elapsed` gives the live countdown without re-reading
/// `remaining` (see `next_episode_countdown_remaining`). Reproduces the
/// earlier "auto-advance at real EOF" behavior when delay exceeds what's
/// left to play.
pub(crate) fn next_episode_countdown_total(remaining_secs: f64, delay_secs: f64) -> f64 {
    remaining_secs.max(0.0).min(delay_secs.max(0.0))
}

/// Countdown remaining seconds, clamped to `[0, total]` -- feeds the
/// auto-advance decision (`<= 0.0`) and the Play-Next fill fraction.
pub(crate) fn next_episode_countdown_remaining(total_secs: f64, elapsed_secs: f64) -> f64 {
    (total_secs - elapsed_secs).clamp(0.0, total_secs.max(0.0))
}

/// The segment decision machine: config x segment-type -> OSD action.
/// Thin wrapper over `SegmentAction` so `render_skip_pill`/
/// `Root::tick_auto_skip` read as "what to do" rather than a raw settings
/// enum value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SegmentDecision {
    /// Ask (default): show the skip pill, wait for a click/Return.
    Pill,
    /// Auto-skip: seek past the segment the instant playback enters it,
    /// with a 5s "Undo" toast.
    AutoSkip,
    /// Off: ignore the segment entirely.
    Nothing,
}

pub(crate) fn segment_decision(action: SegmentAction) -> SegmentDecision {
    match action {
        SegmentAction::Ask => SegmentDecision::Pill,
        SegmentAction::AutoSkip => SegmentDecision::AutoSkip,
        SegmentAction::Off => SegmentDecision::Nothing,
    }
}

/// Skip pill label per type (docs/UX-SPEC.md §4). `Unknown`/`Unrecognized` never
/// reach the pill (see `SkipSegmentPrefs::action_for`) but this stays total
/// with a generic "Skip" fallback.
pub(crate) fn skip_pill_label(ty: Option<MediaSegmentType>) -> &'static str {
    match ty {
        Some(MediaSegmentType::Intro) => "Skip Intro",
        Some(MediaSegmentType::Outro) => "Skip Credits",
        Some(MediaSegmentType::Recap) => "Skip Recap",
        Some(MediaSegmentType::Preview) => "Skip Preview",
        Some(MediaSegmentType::Commercial) => "Skip Commercial",
        _ => "Skip",
    }
}

/// The auto-skip "Undo" toast's label -- past tense, mirrors
/// `skip_pill_label`'s per-type wording.
pub(crate) fn skip_toast_label(ty: Option<MediaSegmentType>) -> &'static str {
    match ty {
        Some(MediaSegmentType::Intro) => "Skipped intro",
        Some(MediaSegmentType::Outro) => "Skipped credits",
        Some(MediaSegmentType::Recap) => "Skipped recap",
        Some(MediaSegmentType::Preview) => "Skipped preview",
        Some(MediaSegmentType::Commercial) => "Skipped commercial",
        _ => "Skipped segment",
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PickerKind {
    Audio,
    Subtitle,
}

/// §1.9: which icon/numeral the center flash shows. Skip variants carry the
/// real skip magnitude in seconds (configurable skip lengths, not a
/// hardcoded 10).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FlashKind {
    Play,
    Pause,
    SkipBack(u32),
    SkipForward(u32),
}

/// Auto-skip "Undo" toast state, separate from the S/A
/// track-cycle `toast` field since the two can be up simultaneously (e.g. a
/// viewer switches audio right as an auto-skip fires) with different
/// durations and actions.
#[derive(Debug, Clone)]
pub(crate) struct SkipToastState {
    /// e.g. "Skipped intro" (`skip_toast_label`).
    pub label: String,
    /// Playhead position (seconds) *before* the auto-skip -- what `U`/click
    /// seeks back to.
    pub resume_position_secs: f64,
    pub shown_at: Instant,
}

#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct ScrubState {
    pub dragging: bool,
    /// 0.0..=1.0 across the bar; set on hover or drag, drives the
    /// trickplay preview and live seek readout. Cleared when the pointer
    /// leaves with no drag in progress.
    pub hover_frac: Option<f32>,
    /// Wall-clock time of the last live `seek_absolute_fast` issued while
    /// drag-scrubbing (`root_playback.rs::scrub_hover`) -- debounces to roughly one
    /// per `root.rs::SCRUB_SEEK_DEBOUNCE` window instead of one per
    /// mouse-move.
    pub last_fast_seek_at: Option<Instant>,
}

/// Minimum on-screen displacement (viewport px) before a miniplayer
/// mouse-down/move/up sequence counts as a drag rather than a click;
/// without it, a click's stray sub-pixel move gets misclassified as a drag.
pub(crate) const MINIPLAYER_DRAG_THRESHOLD_PX: f32 = 5.0;

/// Pure so it's unit-testable without a GPUI window -- see
/// `root_playback.rs::note_miniplayer_drag_move`, its only caller.
pub(crate) fn miniplayer_drag_exceeds_threshold(down: (f32, f32), current: (f32, f32)) -> bool {
    let dx = current.0 - down.0;
    let dy = current.1 - down.1;
    (dx * dx + dy * dy) >= MINIPLAYER_DRAG_THRESHOLD_PX * MINIPLAYER_DRAG_THRESHOLD_PX
}

pub(crate) struct PlayerUiState {
    pub item_id: String,
    /// Set by `root.rs` once the currently-playing item's series id is
    /// known (Detail's `DetailState` for a series item), so S/A track
    /// choices can be persisted per-series (docs/UX-SPEC.md §5). `None` for movies.
    pub series_id: Option<String>,
    pub media_source_id: String,
    pub decision_summary: String,
    pub is_direct_play: bool,
    pub decision_reasons: Vec<String>,
    pub container: Option<String>,
    /// The media source this session is playing -- backs the info
    /// popover's spec strip and MediaInfo breakdown.
    pub media_source: jellyfin_api::models::MediaSourceInfo,
    /// The session's streaming-bitrate cap (bits/sec) if configured;
    /// `None` means Auto. Shown by the info overlay when active.
    pub max_bitrate: Option<u32>,

    /// §1.8: plain item title (episode/movie), kept here for the OSD's
    /// top-left breadcrumb.
    pub title: String,
    /// §1.8/§2.4: breadcrumb metadata, populated only when reached from a
    /// series' Detail page, `None` otherwise (e.g. Home's shelves, a
    /// movie). See `breadcrumb_title`.
    pub series_name: Option<String>,
    pub season_number: Option<i32>,
    pub episode_number: Option<i32>,
    /// §2.4: selected season's item id (companion to `series_id`), needed
    /// for mirror-only prev/next-episode navigation -- see
    /// `EpisodeContext::season_id` for why it can't be re-derived from
    /// `season_number` alone.
    pub season_id: Option<String>,

    pub osd_visible: bool,
    /// The `(osd_visible, layer_mode)` pair the subtitle baseline was last
    /// pushed to mpv for (`sync_subtitle_baseline`) -- `None` until the
    /// first push, keeping the per-tick sync a no-op while nothing
    /// relevant changed. `layer_mode` is part of the key
    /// too, since `sub-pos` is a global mpv property that survives every
    /// layer-mode swap while the OSD-shift reason doesn't (the miniplayer
    /// has no full OSD) -- keying on visibility alone left a stale value
    /// in force after a layer swap or fresh playback start, with no
    /// re-apply. With layer mode in the key, `on_osd_tick`'s regular poll
    /// re-pushes on the first tick after any swap.
    pub last_subtitle_sync: Option<(bool, crate::gl_video::LayerMode)>,
    pub last_activity: Instant,

    pub position_secs: f64,
    pub duration_secs: f64,

    pub volume: u8,
    pub muted: bool,
    pub pre_mute_volume: u8,
    /// §1.6: expands the volume slider on hover -- its own "own activity"
    /// exemption from the main OSD's idle auto-hide, same shape as
    /// `scrub.dragging`/`picker.is_some()`.
    pub volume_expanded: bool,
    /// Set when the pointer leaves the volume control; `tick_volume_collapse`
    /// collapses the slider once this deadline passes. `None` while hovering.
    pub volume_hover_deadline: Option<Instant>,
    /// `(pointer x, volume)` at mouse-down -- drags by relative pointer
    /// delta rather than absolute position, since (unlike the scrubber) the
    /// slider's on-screen x depends on how many buttons render to its left.
    pub volume_drag_anchor: Option<(f32, u8)>,

    /// §1.5: `-remaining` (default) vs. `duration` label convention,
    /// toggled by clicking the right-hand label; persisted globally
    /// (sticky across sessions, not per-item).
    pub remaining_display: RemainingDisplay,

    pub tracks: Vec<Track>,
    pub chapters: Vec<(f64, String)>,
    pub trickplay: Option<TrickplayMeta>,
    pub trickplay_cache: Option<TrickplayCache>,

    pub scrub: ScrubState,
    pub picker: Option<PickerKind>,
    /// Part B §9/§10: keyboard-nav highlight index within the open
    /// picker's filtered row list. `None` until Up/Down is pressed; reset
    /// on open/close or filter change (`open_picker`/`close_picker`).
    pub picker_highlight: Option<usize>,
    /// Part B §10: live search-filter text, consulted once track count
    /// exceeds `PICKER_FILTER_THRESHOLD`. No dedicated `TextInput` entity
    /// -- routed like `search.rs::SearchState`, since the picker is modal.
    pub picker_filter: String,
    pub toast: Option<(String, Instant)>,
    /// Bumped on every `show_toast` for a fresh `with_animation` element
    /// id, restarting rather than reusing a prior toast's animation state
    /// (same shape as `flash_seq`).
    pub toast_seq: u64,
    pub info_overlay: bool,

    pub segments: Vec<MediaSegmentDto>,
    /// Session copy of `AppSettings::skip_segments`, set at playback start
    /// and kept live-updatable by `Root::set_skip_segment_action` so a
    /// mid-playback Settings change takes effect on the next segment.
    pub skip_segment_prefs: SkipSegmentPrefs,
    /// Most recently auto-skipped segment's `(start_ticks, end_ticks)` --
    /// prevents `Root::tick_auto_skip` re-triggering each tick while the
    /// playhead is still inside it, and prevents an immediate re-skip after
    /// Undo lands back inside the same segment.
    pub last_auto_skip_segment: Option<(i64, i64)>,
    /// Auto-skip "Undo" toast, `Some` for `SKIP_UNDO_WINDOW` after an
    /// auto-skip fires.
    pub skip_toast: Option<SkipToastState>,
    /// Bumped on every `show_skip_toast`, same fresh-element-id shape as
    /// `toast_seq`/`flash_seq`.
    pub skip_toast_seq: u64,

    pub layer_mode: LayerMode,
    pub dragging_miniplayer: bool,
    /// Set once a miniplayer drag moves past its starting point -- used to
    /// tell "click to restore" apart from "drag to reposition" on release
    /// (see `root_playback.rs::end_miniplayer_drag`).
    pub drag_moved: bool,
    /// Viewport-px mouse-down position, recorded by `start_miniplayer_drag`,
    /// consumed by `note_miniplayer_drag_move`'s distance-threshold check;
    /// `None` when no drag is in progress.
    pub drag_down_pos: Option<(f32, f32)>,

    /// §1.9: center transient play/pause/skip flash -- fires independent of
    /// `osd_visible` (own activity, same precedent as the skip pill).
    pub last_flash: Option<(FlashKind, Instant)>,
    /// Bumped on every `trigger_flash` for a fresh `with_animation` element
    /// id, restarting rather than reusing the fade.
    pub flash_seq: u64,

    /// Session copy of configured ←/→ skip lengths (`AppSettings::
    /// skip_length`), kept live-updatable by `Root::set_skip_length`;
    /// drives both the arrow-key handler and the OSD skip buttons.
    pub skip_back_secs: u32,
    pub skip_forward_secs: u32,

    /// Option-key speed hold (`option_speed_hold.rs`): `Some(rate)` while
    /// Option is held driving mpv's rate away from 1.0 -- `Some(2.0)`
    /// RIGHT, `Some(0.5)` LEFT, `None` at normal speed. Drives
    /// `render_speed_boost_chip`'s label directly off this rate.
    pub speed_boost_rate: Option<f64>,

    /// §2.4: dismissible next-episode card -- `Some` once `tick_next_episode`
    /// finds one within the show-threshold window of the end; `None` for a
    /// movie, before that window, or after dismissal.
    pub next_episode: Option<CardRow>,
    /// Set by `dismiss_next_episode_card`; stops the card re-appearing or
    /// auto-advancing for the rest of this session (§2.1 pass-out
    /// protection).
    pub next_episode_dismissed: bool,
    /// Session copy of `AppSettings::autoplay`, same live-updatable shape
    /// as `skip_segment_prefs`.
    pub autoplay_prefs: AutoplayPrefs,
    /// Instant `next_episode` was first populated -- the countdown's
    /// elapsed-time anchor (`next_episode_countdown_remaining`); `None`
    /// when `next_episode` is `None`.
    pub next_episode_shown_at: Option<Instant>,
    /// `next_episode_countdown_total`, captured once when the card appears.
    /// `None` when `autoplay_prefs.enabled` is false: the card shows no
    /// rule fill or numeral and `tick_next_episode` never auto-advances.
    pub next_episode_countdown_total_secs: Option<f64>,
    /// Set while playback is paused with a counting-down card, so resume
    /// can shift `next_episode_shown_at` by the pause length.
    pub next_episode_paused_at: Option<Instant>,
    /// `Root::next_episode_handover_gen` value this card's hand-over timer
    /// was armed with; a timer whose generation no longer matches is stale.
    pub next_episode_generation: u64,

    /// True from `ContentMode::Playing` entry until the first real
    /// `PlayerEvent::Position` tick, covering the gap between
    /// `ContentMode::Loading` ending and video actually appearing. Drives
    /// `render_playing`'s loading overlay; cleared by `on_position`'s
    /// first call.
    pub loading: bool,
    /// When this session's load began -- drives the "Opening stream... Ns"
    /// elapsed readout; elapsed time is used rather than mpv's cache-fill
    /// percent, which idles at 100 during OPEN.
    pub loading_started: std::time::Instant,
    /// Mirrors live `PlayerEvent::Buffering` state -- `Some(percent)` while
    /// mpv is actively buffering, `None` once it stops. Independent of
    /// `loading`: a mid-play stall sets this even after `loading` is
    /// false, telling `render_playing` to use the smaller stall
    /// presentation instead of the full-scrim initial-load one.
    pub buffering_percent: Option<f64>,

    /// Last time `on_position` pushed an optimistic mirror progress
    /// update -- throttles to `MIRROR_PROGRESS_INTERVAL` rather than every
    /// ~4Hz `Position` tick.
    pub last_mirror_progress_at: Instant,
}

impl PlayerUiState {
    pub(crate) fn new(started: &PlaybackStarted) -> Self {
        PlayerUiState {
            item_id: started.item_id.clone(),
            series_id: None,
            media_source_id: started.media_source_id.clone(),
            decision_summary: started.decision_summary.clone(),
            is_direct_play: started.is_direct_play,
            decision_reasons: started.decision_reasons.clone(),
            container: started.container.clone(),
            media_source: started.media_source.clone(),
            max_bitrate: started.max_bitrate,
            title: String::new(),
            series_name: None,
            season_number: None,
            episode_number: None,
            season_id: None,
            osd_visible: true,
            last_subtitle_sync: None,
            last_activity: Instant::now(),
            position_secs: 0.0,
            duration_secs: 0.0,
            volume: 100,
            muted: false,
            pre_mute_volume: 100,
            volume_expanded: false,
            volume_hover_deadline: None,
            volume_drag_anchor: None,
            remaining_display: RemainingDisplay::default(),
            tracks: Vec::new(),
            chapters: started.chapters.clone(),
            trickplay: started.trickplay.clone(),
            trickplay_cache: None,
            scrub: ScrubState::default(),
            picker: None,
            picker_highlight: None,
            picker_filter: String::new(),
            toast: None,
            toast_seq: 0,
            info_overlay: false,
            segments: Vec::new(),
            skip_segment_prefs: SkipSegmentPrefs::default(),
            last_auto_skip_segment: None,
            skip_toast: None,
            skip_toast_seq: 0,
            layer_mode: LayerMode::FullscreenInWindow,
            dragging_miniplayer: false,
            drag_moved: false,
            drag_down_pos: None,
            last_flash: None,
            flash_seq: 0,
            // Matches `SkipLengthPrefs::default()`; `handle_playback_outcome`
            // overwrites both right after construction, same as
            // `skip_segment_prefs`/`autoplay_prefs` above.
            skip_back_secs: 10,
            skip_forward_secs: 10,
            speed_boost_rate: None,
            next_episode: None,
            next_episode_dismissed: false,
            autoplay_prefs: AutoplayPrefs::default(),
            next_episode_shown_at: None,
            next_episode_countdown_total_secs: None,
            next_episode_generation: 0,
            next_episode_paused_at: None,
            // True until the first real `Position` tick; see the field's
            // doc comment.
            loading: true,
            loading_started: std::time::Instant::now(),
            buffering_percent: None,
            // Backdated so the first `on_position` tick pushes an initial
            // mirror update right away, since a full
            // `MIRROR_PROGRESS_INTERVAL` wait could outlast a short test clip.
            last_mirror_progress_at: Instant::now() - MIRROR_PROGRESS_INTERVAL,
        }
    }

    pub(crate) fn note_activity(&mut self) {
        self.last_activity = Instant::now();
        self.osd_visible = true;
    }

    /// Returns whether OSD visibility changed, so the caller only
    /// `cx.notify()`s when needed. Never hides during a drag/picker/
    /// info-overlay/toast/volume-hover, and never hides while paused
    /// (§1.11).
    pub(crate) fn tick_auto_hide(&mut self, paused: bool) -> bool {
        if !self.osd_visible {
            return false;
        }
        if paused
            || self.scrub.dragging
            || self.picker.is_some()
            || self.info_overlay
            || self.volume_expanded
        {
            return false;
        }
        if let Some((_, at)) = self.toast {
            if at.elapsed() < TOAST_DURATION {
                return false;
            }
        }
        if self.last_activity.elapsed() >= OSD_IDLE_TIMEOUT {
            self.osd_visible = false;
            return true;
        }
        false
    }

    /// §1.6: collapses the volume slider once `VOLUME_COLLAPSE_DELAY`
    /// passes after the pointer leaves it. Polled by the same
    /// `on_osd_tick` loop as `tick_auto_hide`/`expire_toast`.
    pub(crate) fn tick_volume_collapse(&mut self) -> bool {
        if let Some(deadline) = self.volume_hover_deadline {
            if Instant::now() >= deadline {
                self.volume_expanded = false;
                self.volume_hover_deadline = None;
                return true;
            }
        }
        false
    }

    pub(crate) fn expire_toast(&mut self) -> bool {
        if let Some((_, at)) = self.toast {
            if at.elapsed() >= TOAST_DURATION {
                self.toast = None;
                return true;
            }
        }
        false
    }

    pub(crate) fn show_toast(&mut self, text: String) {
        self.toast = Some((text, Instant::now()));
        self.toast_seq = self.toast_seq.wrapping_add(1);
    }

    /// Part B §9/§10: opens the S/A picker, resetting the keyboard-nav
    /// highlight and search filter from a previous open.
    pub(crate) fn open_picker(&mut self, kind: PickerKind) {
        self.picker = Some(kind);
        self.picker_highlight = None;
        self.picker_filter.clear();
    }

    /// Closes the picker (Esc, click-away, or a pick), clearing the same
    /// nav/filter state `open_picker` resets, so the next open starts
    /// clean.
    pub(crate) fn close_picker(&mut self) {
        self.picker = None;
        self.picker_highlight = None;
        self.picker_filter.clear();
    }

    /// Up/Down within the picker -- `row_count` is the filtered row count,
    /// recomputed by the caller each time (`filtered_tracks`) since
    /// filtering changes index positions.
    pub(crate) fn picker_move_highlight(&mut self, delta: i32, row_count: usize) {
        self.picker_highlight =
            crate::ui::popover::move_highlight(self.picker_highlight, delta, row_count);
    }

    /// Appends to the picker's search filter (consulted once track count
    /// crosses `PICKER_FILTER_THRESHOLD`); resets the highlight since the
    /// filtered set is about to change.
    pub(crate) fn picker_type_char(&mut self, ch: &str) {
        self.picker_filter.push_str(ch);
        self.picker_highlight = None;
    }

    pub(crate) fn picker_backspace(&mut self) {
        self.picker_filter.pop();
        self.picker_highlight = None;
    }

    /// §1.9: records a new center transient flash, restarting its fade
    /// (see `flash_seq`'s doc comment).
    pub(crate) fn trigger_flash(&mut self, kind: FlashKind) {
        self.flash_seq = self.flash_seq.wrapping_add(1);
        self.last_flash = Some((kind, Instant::now()));
    }

    /// §1.8: `"Series Name · S2 E4 · Episode Title"` for episodes reached
    /// from a series' own Detail page, `"Series Name · Episode Title"` if
    /// only the series is known (season/episode number missing), or just
    /// the plain item title otherwise (movies, or playback started from
    /// somewhere without series context -- e.g. Home's shelves).
    pub(crate) fn breadcrumb_title(&self) -> String {
        // Both halves get the wrapping-quote trim before
        // composition, since trimming the joined string could miss quotes
        // wrapping only one half (see `cards::display_title`).
        let title = crate::cards::display_title(&self.title);
        let series = self.series_name.as_deref().map(crate::cards::display_title);
        match (series, self.season_number, self.episode_number) {
            (Some(series), Some(s), Some(e)) => {
                format!("{series} · S{s} E{e} · {title}")
            }
            (Some(series), _, _) => format!("{series} · {title}"),
            _ => title,
        }
    }

    /// §2.4: clickable "go to series" link, only when `series_id` is known
    /// (reached from the series' Detail page), matching
    /// `open_series_from_player`'s gate.
    pub(crate) fn breadcrumb_clickable(&self) -> bool {
        self.series_id.is_some()
    }

    /// The Media Segment (if any) covering the current position -- docs/UX-SPEC.md
    /// §4's "Skip Intro/Credits" pill, active only while the play head is
    /// actually inside the segment.
    pub(crate) fn active_segment(&self) -> Option<&MediaSegmentDto> {
        let pos_ticks = (self.position_secs * 10_000_000.0) as i64;
        self.segments.iter().find(|s| {
            let start = s.start_ticks.unwrap_or(0);
            let end = s.end_ticks.unwrap_or(0);
            pos_ticks >= start && pos_ticks < end
        })
    }

    /// Segment covering the current position paired with what
    /// `skip_segment_prefs` says to do about it. `SegmentDecision::Nothing`
    /// is still returned (not filtered) so a caller can tell whether
    /// playback is inside any segment regardless of configured action.
    pub(crate) fn active_segment_decision(&self) -> Option<(&MediaSegmentDto, SegmentDecision)> {
        let seg = self.active_segment()?;
        let ty = seg.type_.unwrap_or(MediaSegmentType::Unknown);
        let action = self.skip_segment_prefs.action_for(ty);
        Some((seg, segment_decision(action)))
    }

    /// Outro/credits segment start (seconds), if the server returned one.
    /// Unlike `active_segment`, doesn't require the playhead to be inside
    /// it -- `tick_next_episode` needs this before playback reaches it
    /// (`next_episode_trigger_remaining_secs`).
    pub(crate) fn outro_segment_start_secs(&self) -> Option<f64> {
        self.segments
            .iter()
            .find(|s| s.type_ == Some(MediaSegmentType::Outro))
            .and_then(|s| s.start_ticks)
            .map(|ticks| ticks as f64 / 10_000_000.0)
    }

    /// Whether Outro is configured `AutoSkip` -- the precedence signal
    /// `next_episode_trigger_remaining_secs` uses to suppress outro-driven
    /// timing.
    pub(crate) fn outro_auto_skip(&self) -> bool {
        self.skip_segment_prefs.action_for(MediaSegmentType::Outro) == SegmentAction::AutoSkip
    }

    /// Records a new auto-skip "Undo" toast, restarting its fade
    /// (`skip_toast_seq`).
    pub(crate) fn show_skip_toast(&mut self, label: String, resume_position_secs: f64) {
        self.skip_toast = Some(SkipToastState {
            label,
            resume_position_secs,
            shown_at: Instant::now(),
        });
        self.skip_toast_seq = self.skip_toast_seq.wrapping_add(1);
    }

    /// Same shape as `expire_toast` with `SKIP_UNDO_WINDOW` instead of
    /// `TOAST_DURATION`; once it passes, `Root::undo_last_skip` can no
    /// longer undo.
    pub(crate) fn expire_skip_toast(&mut self) -> bool {
        if let Some(t) = &self.skip_toast {
            if t.shown_at.elapsed() >= SKIP_UNDO_WINDOW {
                self.skip_toast = None;
                return true;
            }
        }
        false
    }

    /// §1.4 item 6: the chapter (if any) covering `secs` -- the last
    /// chapter whose start is at or before it. Used by the trickplay hover
    /// bubble to show a chapter name under the timestamp.
    pub(crate) fn chapter_at(&self, secs: f64) -> Option<&str> {
        self.chapters
            .iter()
            .rev()
            .find(|(start, _)| *start <= secs)
            .map(|(_, name)| name.as_str())
    }

    pub(crate) fn next_corner(&self, current: Corner, dropped_near: (f32, f32)) -> Corner {
        let _ = current;
        let (fx, fy) = dropped_near;
        match (fx < 0.5, fy < 0.5) {
            (true, true) => Corner::TopLeft,
            (false, true) => Corner::TopRight,
            (true, false) => Corner::BottomLeft,
            (false, false) => Corner::BottomRight,
        }
    }
}

/// Persistence key for a `Track` (`player_prefs.rs`): language code if
/// present, else title -- mpv track ids aren't stable across
/// items/relaunches.
pub(crate) fn track_pref_key(track: &Track) -> Option<String> {
    track.lang.clone().or_else(|| track.title.clone())
}

/// Finds the first track of `kind` whose `track_pref_key` matches `key`
/// (used to re-apply a persisted per-series preference once real track ids
/// are known -- see `root.rs::apply_track_prefs`).
pub(crate) fn find_track_by_key(tracks: &[Track], kind: TrackKind, key: &str) -> Option<i64> {
    tracks
        .iter()
        .find(|t| t.kind == kind && track_pref_key(t).as_deref() == Some(key))
        .map(|t| t.mpv_id)
}

// ---- Global audio/subtitle language + subtitle-mode fallback -------------

/// Canonicalizes an ISO 639-2 language code to its "B" form for comparison,
/// so a stream tagged "deu" and a preference of "ger" match -- `Track::lang`
/// may carry either the B or T form depending on the muxing tool, while
/// `settings::LANGUAGE_PRESETS` only offers one spelling. Covers the B/T
/// pairs mpv/ffmpeg realistically emit; most languages have no B/T split,
/// so there is nothing to normalize for them.
fn normalize_lang(code: &str) -> String {
    let lower = code.trim().to_ascii_lowercase();
    let canonical = match lower.as_str() {
        "deu" => "ger",
        "fra" => "fre",
        "zho" => "chi",
        "nld" => "dut",
        "ces" => "cze",
        "ell" => "gre",
        "eus" => "baq",
        "fas" => "per",
        "isl" => "ice",
        "kat" => "geo",
        "mkd" => "mac",
        "mri" => "mao",
        "msa" => "may",
        "mya" => "bur",
        "ron" => "rum",
        "slk" => "slo",
        "sqi" => "alb",
        "hye" => "arm",
        "bod" => "tib",
        "cym" => "wel",
        other => other,
    };
    canonical.to_string()
}

/// Case-insensitive, B/T-tolerant language-code comparison -- see
/// `normalize_lang`'s doc comment.
pub(crate) fn lang_matches(a: &str, b: &str) -> bool {
    normalize_lang(a) == normalize_lang(b)
}

/// What to do about subtitles after track selection resolves -- three
/// outcomes because `SubtitleMode::None` must actively turn subtitles off
/// even when the stream marks one default, which a bare `Option<i64>`
/// can't express.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SubtitleDecision {
    /// Don't touch `sid` at all -- whatever the stream/server already
    /// selected (or mpv's own default) stays in effect.
    Leave,
    /// Explicitly disable subtitles (mpv `sid=no`).
    Off,
    /// Explicitly select this subtitle track.
    Track(i64),
}

/// The result of [`resolve_track_selection`]: which audio track (if any) to
/// switch to, and what to do about subtitles.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TrackDecision {
    /// `Some(id)` to switch to that audio track; `None` to leave the
    /// stream/server's own default audio track alone.
    pub audio: Option<i64>,
    pub subtitle: SubtitleDecision,
}

/// Track lookup shared by the audio and `OnlyForced` subtitle branches
/// below: the track that's actually playing absent any preference --
/// `selected` if mpv picked one, else `default`, else the first track of
/// that kind.
fn current_or_default_track(tracks: &[Track], kind: TrackKind) -> Option<&Track> {
    tracks
        .iter()
        .find(|t| t.kind == kind && t.selected)
        .or_else(|| tracks.iter().find(|t| t.kind == kind && t.default))
        .or_else(|| tracks.iter().find(|t| t.kind == kind))
}

/// Player-preferences feature 2's decision table -- pure, unit-testable,
/// called once per session after tracks are announced
/// (`root.rs::apply_track_prefs`). `series_pref` always wins over `global`
/// per field; `OnlyForced` ignores `global.subtitle` entirely, matching the
/// forced subtitle's language against the audio track that will actually
/// play (the burned-in "alien dialogue" subs case).
pub(crate) fn resolve_track_selection(
    tracks: &[Track],
    series_pref: Option<&crate::player_prefs::SeriesTrackPref>,
    global: &crate::settings::LanguagePrefs,
) -> TrackDecision {
    let audio = if let Some(key) = series_pref.and_then(|p| p.audio.as_ref()) {
        find_track_by_key(tracks, TrackKind::Audio, key)
    } else {
        global.audio.as_deref().and_then(|lang| {
            tracks
                .iter()
                .find(|t| {
                    t.kind == TrackKind::Audio
                        && t.lang.as_deref().is_some_and(|l| lang_matches(l, lang))
                })
                .map(|t| t.mpv_id)
        })
    };

    // Language of the audio track that will actually play, for
    // `OnlyForced` matching -- the resolved audio decision's language if
    // switching, else the already-selected/default one.
    let effective_audio_lang: Option<String> = audio
        .and_then(|id| tracks.iter().find(|t| t.mpv_id == id))
        .and_then(|t| t.lang.clone())
        .or_else(|| {
            current_or_default_track(tracks, TrackKind::Audio).and_then(|t| t.lang.clone())
        });

    let subtitle = if let Some(key) = series_pref.and_then(|p| p.subtitle.as_ref()) {
        find_track_by_key(tracks, TrackKind::Subtitle, key)
            .map(SubtitleDecision::Track)
            .unwrap_or(SubtitleDecision::Leave)
    } else {
        use crate::settings::SubtitleMode;
        match global.subtitle_mode {
            SubtitleMode::Default => SubtitleDecision::Leave,
            SubtitleMode::None => SubtitleDecision::Off,
            SubtitleMode::Always => {
                let matched = global.subtitle.as_deref().and_then(|lang| {
                    tracks.iter().find(|t| {
                        t.kind == TrackKind::Subtitle
                            && t.lang.as_deref().is_some_and(|l| lang_matches(l, lang))
                    })
                });
                let chosen = matched
                    .or_else(|| {
                        tracks
                            .iter()
                            .find(|t| t.kind == TrackKind::Subtitle && t.default)
                    })
                    .or_else(|| tracks.iter().find(|t| t.kind == TrackKind::Subtitle));
                chosen
                    .map(|t| SubtitleDecision::Track(t.mpv_id))
                    .unwrap_or(SubtitleDecision::Leave)
            }
            SubtitleMode::OnlyForced => {
                let forced = tracks.iter().find(|t| {
                    t.kind == TrackKind::Subtitle
                        && t.forced
                        && t.lang
                            .as_deref()
                            .zip(effective_audio_lang.as_deref())
                            .is_some_and(|(sl, al)| lang_matches(sl, al))
                });
                forced
                    .map(|t| SubtitleDecision::Track(t.mpv_id))
                    .unwrap_or(SubtitleDecision::Off)
            }
        }
    };

    TrackDecision { audio, subtitle }
}

/// Part B §10: the search-filter box only renders once a kind's
/// *unfiltered* track count exceeds this.
pub(crate) const PICKER_FILTER_THRESHOLD: usize = 10;

/// Part B §10's live filter: substring match against title/lang,
/// case-insensitive. Empty filter (always the case below
/// `PICKER_FILTER_THRESHOLD`) is a no-op pass-through preserving original
/// order.
pub(crate) fn filtered_tracks<'a>(
    tracks: &'a [Track],
    kind: TrackKind,
    filter: &str,
) -> Vec<&'a Track> {
    let needle = filter.trim().to_lowercase();
    tracks
        .iter()
        .filter(|t| t.kind == kind)
        .filter(|t| {
            if needle.is_empty() {
                return true;
            }
            let title = t.title.as_deref().unwrap_or_default().to_lowercase();
            let lang = t.lang.as_deref().unwrap_or_default().to_lowercase();
            title.contains(&needle) || lang.contains(&needle)
        })
        .collect()
}

/// Converts a window-space x into a `0.0..=1.0` fraction across the track,
/// using the same fixed-margin layout `scrub_row` paints with rather than a
/// live-measured hitbox (see ARCHITECTURE.md). Inset is
/// just the OSD bar's horizontal padding, since time labels moved to their
/// own row above the track.
fn bar_frac(x: Pixels, viewport_width: Pixels) -> f32 {
    let inset = px(SCRUB_MARGIN);
    let width = (viewport_width - inset - inset).max(px(1.0));
    ((x - inset) / width).clamp(0.0, 1.0)
}

/// Strip cell value for the current playback decision.
/// Derived from `PlaybackStarted::decision_summary`'s leading segment
/// ("Direct Play" or "Transcode (reason; reason)"), uppercased -- reasons
/// stay in the info popover. `is_direct_play` is only the fallback for an
/// empty/unrecognized summary.
pub(crate) fn playback_mode_label(decision_summary: &str, is_direct_play: bool) -> String {
    let head = decision_summary
        .split(" (")
        .next()
        .unwrap_or_default()
        .trim();
    if head.is_empty() {
        return if is_direct_play {
            "DIRECT PLAY".to_string()
        } else {
            "TRANSCODE".to_string()
        };
    }
    head.to_uppercase()
}

/// `H:MM:SS` (or `M:SS` under an hour) -- shared by the scrubber's
/// timestamp/remaining readout and the trickplay hover label.
pub(crate) fn format_time(secs: f64) -> String {
    let secs = secs.max(0.0) as i64;
    let h = secs / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

/// mpv `sub-pos` (0-150, percent height from top, lower
/// = higher; `player::SubtitleStyle::pos`), clearing the OSD's control
/// zone when visible; pushed through [`effective_subtitle_pos`] for the
/// lower-half floor. Wired via `sync_subtitle_baseline` from the
/// two places `ui.osd_visible` flips, not `render_osd` -- re-entering
/// `WeakEntity<Root>::update` on the entity being rendered panics (GPUI
/// 0.2.2 has no reentrant-update primitive); no interpolation primitive,
/// so the transition is one stepwise call.
pub(crate) fn subtitle_pos_for_osd_state(base_pos: Option<i64>, controls_visible: bool) -> i64 {
    let base = base_pos.unwrap_or(SUBTITLE_POS_DEFAULT);
    if controls_visible {
        (base - SUBTITLE_POS_CONTROLS_SHIFT).clamp(0, 150)
    } else {
        base.clamp(0, 150)
    }
}

/// The value actually pushed to mpv, wrapping
/// [`subtitle_pos_for_osd_state`]'s geometry in the same lower-half floor
/// `SubtitleStylePrefs::effective_pos` applies, so the OSD shift can't push
/// a preference into the top half. Kept separate since
/// `subtitle_pos_for_osd_state`'s tests pin the raw mpv contract, not this
/// app's policy.
pub(crate) fn effective_subtitle_pos(base_pos: Option<i64>, controls_visible: bool) -> i64 {
    subtitle_pos_for_osd_state(base_pos, controls_visible).clamp(
        crate::settings::SUBTITLE_POS_FLOOR,
        crate::settings::SUBTITLE_POS_CEIL,
    )
}

// ==================== Rendering ====================
//
// Entry: `render_playing` (`root.rs::render_content`'s `ContentMode::
// Playing` arm), dispatching on `ui.layer_mode` -- full OSD (`render_osd`)
// for Fullscreen, or `render_miniplayer`'s small hover-OSD composed inside
// Browse so sidebar/grid keyboard nav stays live underneath (docs/UX-SPEC.md §3).

/// Live mpv playback stats for the info overlay (`I` key) -- codec/HDR,
/// bitrates, fps, dropped frames, cache depth. Rebuilt every render while
/// `ContentMode::Playing` (`PlayerEvent::Position` at ~4Hz). Also feeds the
/// scrubber's buffered-range layer (§1.4 item 2) via `cache`.
#[derive(Debug, Clone, Default)]
pub(crate) struct PlayerLiveInfo {
    pub hwdec: Option<String>,
    pub video_bitrate_bps: Option<f64>,
    pub audio_bitrate_bps: Option<f64>,
    pub container_fps: Option<f64>,
    pub video_params: player::VideoParams,
    pub frame_drops: player::FrameDropStats,
    pub cache: player::CacheState,
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn render_playing(
    ui: &PlayerUiState,
    paused: bool,
    viewport: Size<Pixels>,
    info: PlayerLiveInfo,
    is_fullscreen: bool,
    store: &ImageStore,
    root: WeakEntity<Root>,
    cx: &mut Context<Root>,
) -> impl IntoElement {
    // Pointer auto-hide: while the OSD is idle in fullscreen, ask for
    // `CursorStyle::None`; macOS maps this to `NSCursor
    // setHiddenUntilMouseMoves:YES`, so the OS restores it on the next
    // move, the same move that flips `osd_visible` back to true.
    // Miniplayer never hides the pointer -- the user is browsing around it.
    let hide_pointer = matches!(ui.layer_mode, LayerMode::FullscreenInWindow) && !ui.osd_visible;
    let base = div()
        .size_full()
        .relative()
        .when(hide_pointer, |d| d.cursor(gpui::CursorStyle::None));
    match ui.layer_mode {
        LayerMode::Miniplayer(_) => base, // video shows through; no OSD here.
        LayerMode::FullscreenInWindow => base
            .child(render_osd(
                ui,
                paused,
                viewport,
                &info,
                is_fullscreen,
                store,
                root.clone(),
                cx,
            ))
            // Part B §9: the popover panel is nested at the trigger site
            // (`render_controls_row`'s audio/subtitle buttons, via
            // `popover_trigger`); this is only the full-viewport
            // click-away catcher, mounted here since the trigger's own
            // wrapper div can't cover the whole viewport.
            .when(ui.picker.is_some(), |d| {
                let close_root = root.clone();
                d.child(click_away_catcher("osd-picker-catcher", move |cx| {
                    let _ = close_root.update(cx, |root, cx| root.close_picker(cx));
                }))
            })
            // From click to first frame (`ui.loading`), a full dim-scrim
            // overlay -- otherwise the surface sits black with no
            // feedback. Checked before the mid-play variant below since a
            // stall during this window is still "no first frame yet", not
            // a later stall.
            .when(ui.loading, |d| {
                // mpv's cache-fill metric idles at 100 during the initial
                // OPEN (not a cache ramp), so a percent there is
                // meaningless. Show a percent only during a real sub-100
                // ramp; otherwise use elapsed time.
                let genuine_ramp = ui.buffering_percent.filter(|p| *p < 100.0);
                let overlay = match genuine_ramp {
                    Some(_) => render_loading_overlay("Buffering…", genuine_ramp),
                    None => {
                        let secs = ui.loading_started.elapsed().as_secs();
                        let label = if secs >= 3 {
                            format!("Opening stream… {secs}s")
                        } else {
                            "Opening stream…".to_string()
                        };
                        render_loading_overlay(&label, None)
                    }
                };
                d.child(overlay)
            })
            // Mid-play stall (`PlayerEvent::Buffering` after the first
            // frame): small centered spinner, no full scrim -- the current
            // frame stays visible.
            .when(!ui.loading, |d| {
                d.children(ui.buffering_percent.map(render_buffering_stall_overlay))
            }),
    }
}

/// Three dots whose opacity cycles out of phase, looping via
/// `Animation::repeat()` -- GPUI's pinned 0.2.x `Styled` API has no
/// rotation primitive for a literal spinner, so this is the substitute.
fn pulsing_dots(id_prefix: &str) -> impl IntoElement {
    const DOT_CYCLE_MS: u64 = 1200;
    div().flex().flex_row().gap_1().children((0..3).map(|i| {
        let key = SharedString::from(format!("{id_prefix}-dot-{i}"));
        let phase = i as f32 / 3.0;
        div()
            .w(px(6.))
            .h(px(6.))
            .rounded_full()
            .bg(rgba(theme::TEXT_PRIMARY))
            .with_animation(
                key,
                Animation::new(Duration::from_millis(DOT_CYCLE_MS)).repeat(),
                move |el, delta| {
                    let t = (delta + phase).fract();
                    // Triangle wave 0.25..=1.0 so a dot never fully
                    // disappears (flicker would read as broken, not loading).
                    let opacity = if t < 0.5 { t * 2.0 } else { 2.0 - t * 2.0 };
                    el.opacity(0.25 + opacity * 0.75)
                },
            )
    }))
}

/// Full-surface dim scrim + `pulsing_dots` + stage text -- shared by
/// `ContentMode::Loading` and `render_playing`'s pre-first-frame window so
/// the visual language stays identical end to end.
pub(crate) fn render_loading_overlay(stage_label: &str, percent: Option<f64>) -> impl IntoElement {
    let label = match percent {
        Some(pct) if pct > 0.0 => format!("{stage_label} {}%", pct.round() as i64),
        _ => stage_label.to_string(),
    };
    div()
        .id("player-loading-overlay")
        .absolute()
        .inset_0()
        .flex()
        .items_center()
        .justify_center()
        // Wash over the video surface with no opaque panel -- the
        // documented OSD-over-video literal (`theme.rs`), not a
        // `surface.*` token, which assumes an opaque backing this doesn't
        // have.
        .bg(rgba(theme::tint(theme::NOTTE, 0x88)))
        .child(
            div()
                .flex()
                .flex_col()
                .items_center()
                .gap_3()
                .child(pulsing_dots("loading"))
                .child(
                    div()
                        .text_size(theme::TEXT_METADATA)
                        .text_color(rgba(theme::TEXT_PRIMARY))
                        .child(SharedString::from(label)),
                ),
        )
}

/// Smaller mid-play stall variant -- no scrim, small centered spinner +
/// "Buffering... n%" so it reads as a transient hiccup, not a mode change.
fn render_buffering_stall_overlay(percent: f64) -> impl IntoElement {
    div()
        .id("player-buffering-stall-overlay")
        .absolute()
        .inset_0()
        .flex()
        .items_center()
        .justify_center()
        .child(
            div()
                .flex()
                .flex_col()
                .items_center()
                .gap_2()
                .px_4()
                .py_3()
                .rounded_md()
                .bg(rgba(theme::tint(theme::NOTTE, 0x99)))
                .child(pulsing_dots("stall"))
                .child(
                    div()
                        .text_size(theme::TEXT_CAPTION)
                        .text_color(rgba(theme::TEXT_PRIMARY))
                        .child(SharedString::from(if percent > 0.0 {
                            format!("Buffering… {}%", percent.round() as i64)
                        } else {
                            "Buffering…".to_string()
                        })),
                ),
        )
}

/// Scrub track's painted bar -- idle groove, buffered range, played fill,
/// and playhead knob, in `docs/DESIGN-PLAYER-NAV.md` §1.4's stacking order.
/// Split out of `scrub_row` as a pure `(played_frac, buffered_frac) ->
/// element` function with no `Root`/`PlayerUiState` dependency so its
/// layout can be regression-tested against a live GPUI window
/// (`tests::scrub_track_layout`).
fn scrub_track_bar(played_frac: f32, buffered_frac: f32) -> Div {
    div()
        .absolute()
        .left(px(0.))
        .right(px(0.))
        .h(px(SCRUB_TRACK_HEIGHT))
        .group_hover("scrub-bar", |s| s.h(px(SCRUB_TRACK_HEIGHT_HOVER)))
        .rounded_full()
        // `.relative()` here silently demotes this div to
        // `position: relative` (GPUI's position setters are
        // last-write-wins), collapsing it to zero width and zeroing every
        // child percentage. Taffy resolves an absolutely-positioned child
        // against its direct parent regardless of `position: relative`, so
        // `.relative()` was never needed; regression-covered by
        // `scrub_track_layout::played_fill_and_knob_track_the_position`.
        //
        // Idle track fill: painted directly on video/gradient with no
        // opaque backing (`theme.rs`'s OSD-over-video exception). Played
        // (accent) / buffered (25% white) / remaining
        // (10% white), a 2.5x contrast delta that reads clearly over video.
        .bg(rgba(theme::tint(theme::PANNA, 0x1a)))
        .child(
            // Buffered/cache range (§1.4 item 2), from mpv's demuxer
            // read-ahead (`PlayerLiveInfo.cache`, threaded in by
            // `render_osd`).
            div()
                .absolute()
                .left(px(0.))
                .top(px(0.))
                .h_full()
                .rounded_full()
                .bg(rgba(theme::tint(theme::PANNA, 0x40)))
                .w(gpui::relative(buffered_frac)),
        )
        .child(
            // Played progress -- uses `theme::ACCENT` (not
            // `theme::PROGRESS`, which is the passive watched-indicator on
            // browse-mode cards).
            div()
                .h_full()
                .rounded_full()
                .bg(rgb(theme::ACCENT))
                .w(gpui::relative(played_frac)),
        )
        .child(
            // Real playhead knob -- tracks `played_frac`, never the
            // pointer (the ghost hover hairline below is the only
            // pointer-following element). Grows
            // `SCRUB_KNOB_SIZE` -> `SCRUB_KNOB_SIZE_HOVER` via the same
            // `group_hover("scrub-bar", ..)` as the track height,
            // recentered on the bar's own growing midpoint.
            div()
                .absolute()
                .left(gpui::relative(played_frac))
                .top(px(SCRUB_TRACK_HEIGHT / 2.0 - SCRUB_KNOB_SIZE / 2.0))
                .ml(px(-SCRUB_KNOB_SIZE / 2.0))
                .w(px(SCRUB_KNOB_SIZE))
                .h(px(SCRUB_KNOB_SIZE))
                .group_hover("scrub-bar", |s| {
                    s.top(px(
                        SCRUB_TRACK_HEIGHT_HOVER / 2.0 - SCRUB_KNOB_SIZE_HOVER / 2.0
                    ))
                    .ml(px(-SCRUB_KNOB_SIZE_HOVER / 2.0))
                    .w(px(SCRUB_KNOB_SIZE_HOVER))
                    .h(px(SCRUB_KNOB_SIZE_HOVER))
                })
                .rounded_full()
                .bg(rgb(theme::ACCENT)),
        )
}

/// §1.5/§1.4: the scrubber row -- fixed-width elapsed/remaining time
/// labels, then the track layered idle-bg -> buffered -> played -> chapter
/// ticks -> hover thumb, per `docs/DESIGN-PLAYER-NAV.md` §1.4's stacking
/// order.
fn scrub_row(
    ui: &PlayerUiState,
    viewport_w: Pixels,
    buffered_frac: f32,
    root: WeakEntity<Root>,
) -> impl IntoElement {
    let played_frac = if ui.duration_secs > 0.0 {
        (ui.position_secs / ui.duration_secs).clamp(0.0, 1.0) as f32
    } else {
        0.0
    };
    let buffered_frac = buffered_frac.max(played_frac);
    let chapter_ticks: Vec<(f32, f64)> = ui
        .chapters
        .iter()
        .filter(|(secs, _)| *secs > 0.0)
        .map(|(secs, _)| {
            (
                (secs / ui.duration_secs.max(1.0)).clamp(0.0, 1.0) as f32,
                *secs,
            )
        })
        .collect();

    // `theme::apply_tabular_nums` (OpenType tnum/lnum) keeps elapsed/
    // remaining digits from jittering in width as they tick over. Color is
    // a literal, not a surface-opacity token, since this is OSD-over-video
    // with no opaque backing (`theme.rs` C.6.1).
    let elapsed_label = theme::apply_tabular_nums(
        div()
            .w(px(TIME_LABEL_W))
            .flex()
            .justify_start()
            .text_color(rgb(theme::PANNA_2))
            .text_xs(),
    )
    .child(format_time(ui.position_secs));

    let remaining_secs = (ui.duration_secs - ui.position_secs).max(0.0);
    let right_text = match ui.remaining_display {
        RemainingDisplay::Remaining => format!("-{}", format_time(remaining_secs)),
        RemainingDisplay::Total => format!(
            "{} / {}",
            format_time(ui.position_secs),
            format_time(ui.duration_secs)
        ),
    };
    let toggle_root = root.clone();
    let remaining_label = theme::apply_tabular_nums(
        div()
            .id("osd-time-toggle")
            .min_w(px(TIME_LABEL_W + 16.))
            .flex()
            .justify_end()
            .cursor_pointer()
            .text_color(rgb(theme::PANNA_2))
            .text_xs()
            .hover(|s| s.text_color(rgb(theme::PANNA))),
    )
    .child(right_text)
    .on_click(move |_e, _w, cx| {
        let _ = toggle_root.update(cx, |root, cx| root.toggle_remaining_display(cx));
    });

    let down_root = root.clone();
    let move_root = root.clone();
    let up_root = root.clone();
    let up_out_root = root.clone();
    let leave_root = root.clone();
    let hover_frac = ui.scrub.hover_frac;

    let track = div()
        .id("osd-scrub-bar")
        // Drives `SCRUB_TRACK_HEIGHT_HOVER`/the knob's `group_hover`
        // reveal below; this outer div only owns the taller hit target
        // and the group name.
        .group("scrub-bar")
        .relative()
        // Full padded width; `bar_frac`'s inset matches
        // the label reservation drop.
        .w_full()
        .h(px(SCRUB_HIT_HEIGHT))
        .flex()
        .items_center()
        .child(scrub_track_bar(played_frac, buffered_frac))
        .children(chapter_ticks.iter().map(|(frac, secs)| {
            let secs = *secs;
            let jump_root = root.clone();
            // Chapter tick -- centered on the track's own vertical
            // midpoint (not the hit box's), sized to the track's hover
            // thickness so it stays visible at idle height too.
            div()
                .id(SharedString::from(format!("osd-chapter-tick-{secs}")))
                .absolute()
                .left(gpui::relative(*frac))
                .top(px(SCRUB_HIT_HEIGHT / 2.0 - SCRUB_TRACK_HEIGHT_HOVER / 2.0))
                .ml(px(-1.))
                .w(px(2.))
                .h(px(SCRUB_TRACK_HEIGHT_HOVER))
                .rounded_full()
                .bg(rgba(theme::tint(theme::NOTTE, 0xa6)))
                .cursor_pointer()
                .on_click(move |_e, _w, cx| {
                    let _ = jump_root.update(cx, |root, cx| root.seek_to_click(secs, cx));
                })
        }))
        .when_some(hover_frac, |d, f| {
            // Hover preview -- deliberately not the playhead knob
            // (translucent, thin, no fill) so hovering never reads as a
            // seek. Only an actual click or click-drag (`commit_scrub`, on
            // mouse-up) moves the real position.
            let hover_secs = (f as f64 * ui.duration_secs).max(0.0);
            // Improve the preview chip to include the chapter title when
            // hovering within one (Task 1's ask; mirrors
            // `render_trickplay_preview`'s own timestamp+chapter-name
            // stack, for the no-trickplay-data fallback case this chip
            // is). The wrapper's width/`ml` centering trick (same shape as
            // `TIME_LABEL_W`'s fixed-width labels elsewhere in this file)
            // needs a known width to center on, so it widens to a second
            // fixed value when a chapter title is present rather than
            // shrinking to fit -- simpler than measuring text, at the cost
            // of a long title getting wrapped inside a fixed box instead of
            // sized exactly to it.
            let chapter_name = ui.chapter_at(hover_secs);
            let has_chapter = chapter_name.is_some();
            let (chip_w, chip_ml) = if has_chapter {
                (220., -110.)
            } else {
                (32., -16.)
            };
            d.child(
                div()
                    .absolute()
                    .left(gpui::relative(f))
                    .top(px(-22.))
                    .when(has_chapter, |d| d.top(px(-38.)))
                    .ml(px(chip_ml))
                    .w(px(chip_w))
                    .flex()
                    .justify_center()
                    .child(
                        div()
                            .px_2()
                            .py_1()
                            .rounded(px(4.))
                            .bg(rgba(theme::tint(theme::NOTTE, 0xcc)))
                            .flex()
                            .flex_col()
                            .items_center()
                            .child(
                                div()
                                    .text_size(px(11.))
                                    .text_color(rgb(theme::PANNA))
                                    .child(format_time(hover_secs)),
                            )
                            .when_some(chapter_name, |d, name| {
                                d.child(
                                    div()
                                        .text_size(theme::TEXT_CAPTION)
                                        .text_color(rgba(theme::TEXT_TERTIARY))
                                        .child(SharedString::from(name.to_string())),
                                )
                            }),
                    ),
            )
            .child(
                div()
                    .absolute()
                    .left(gpui::relative(f))
                    .top(px(-3.))
                    .ml(px(-1.))
                    .w(px(2.))
                    .h(px(16.))
                    .rounded_full()
                    .bg(rgba(theme::tint(theme::PANNA, 0xb3))),
            )
        })
        .on_mouse_down(MouseButton::Left, move |event, _window, cx| {
            let f = bar_frac(event.position.x, viewport_w);
            let _ = down_root.update(cx, |root, cx| root.scrub_hover(Some(f), true, cx));
        })
        .on_mouse_move(move |event, _window, cx| {
            let f = bar_frac(event.position.x, viewport_w);
            let dragging = event.pressed_button == Some(MouseButton::Left);
            let _ = move_root.update(cx, |root, cx| root.scrub_hover(Some(f), dragging, cx));
        })
        .on_mouse_up(MouseButton::Left, move |_event, _window, cx| {
            let _ = up_root.update(cx, |root, cx| root.commit_scrub(cx));
        })
        .on_mouse_up_out(MouseButton::Left, move |_event, _window, cx| {
            let _ = up_out_root.update(cx, |root, cx| root.commit_scrub(cx));
        })
        .on_hover(move |hovering, _window, cx| {
            if !*hovering {
                let _ = leave_root.update(cx, |root, cx| root.clear_scrub_hover_preview(cx));
            }
        });

    // "Playhead handle collides with elapsed-time label":
    // the labels are their own row *above* the track now, not flankers on
    // it. The knob (up to 7px of radius at hover size, plus the ghost hover
    // hairline and its timestamp chip) can therefore sit at frac 0.0 or 1.0
    // without ever overlapping a label -- they're separated on the cross
    // axis, so no amount of horizontal travel can bring them together.
    div()
        .id("osd-scrub-row")
        .w_full()
        .flex()
        .flex_col()
        .gap(px(TIME_LABEL_ROW_GAP))
        .child(
            div()
                .w_full()
                .flex()
                .flex_row()
                .items_center()
                .justify_between()
                .child(elapsed_label)
                .child(remaining_label),
        )
        .child(track)
}

/// Full bottom OSD bar: scrubber row (§1.4/§1.5) then controls row (§1.13)
/// on a gradient scrim (§1.3), plus the title/breadcrumb (§1.8) and center
/// flash (§1.9), independent of `ui.osd_visible` where the spec calls for
/// it. Hard show/hide, not opacity-animated -- GPUI 0.2.2 has no
/// CSS-transition primitive for a plain `div()` (see `cards.rs`'s
/// focus-ring comment).
#[allow(clippy::too_many_arguments)]
fn render_osd(
    ui: &PlayerUiState,
    paused: bool,
    viewport: Size<Pixels>,
    info: &PlayerLiveInfo,
    is_fullscreen: bool,
    store: &ImageStore,
    root: WeakEntity<Root>,
    cx: &mut Context<Root>,
) -> impl IntoElement {
    let mouse_root = root.clone();
    // Covers the whole Fullscreen content pane so any mouse movement counts
    // as OSD activity (docs/UX-SPEC.md §3, same as `note_osd_activity`'s keyboard
    // path). Attached before the `osd_visible` branch so it fires even
    // while the OSD bar is hidden -- it's the gesture that brings it back.
    let wrap = div()
        .absolute()
        .left_0()
        .right_0()
        .bottom_0()
        .top_0()
        .on_mouse_move(move |_event, _window, cx| {
            let _ = mouse_root.update(cx, |root, cx| root.note_osd_activity(cx));
        });
    if !ui.osd_visible {
        // The info overlay (`I`) renders independent of OSD auto-hide, so
        // the top gradient scrim behind it must follow the same
        // visibility, or the overlay would sit on bare video once the OSD
        // fades.
        return wrap
            .children(ui.info_overlay.then(render_top_gradient))
            .children(render_top_left_stack(
                ui,
                info,
                false,
                viewport,
                root.clone(),
            ))
            .children(render_center_flash(ui))
            .children(render_speed_boost_chip(ui))
            .children(render_toast(ui))
            .children(render_skip_toast(ui, root.clone()))
            .children(render_next_episode_card(ui, store, root.clone(), false, cx))
            .children(render_skip_pill(ui, root))
            .into_any_element();
    }

    // §1.4 item 2: fraction demuxed ahead of position. `demuxer-cache-
    // duration` is a duration, not an absolute position, so the buffered
    // range's right edge is `position + cache_duration`.
    let buffered_frac = match (info.cache.demuxer_cache_duration_secs, ui.duration_secs) {
        (Some(cache_secs), dur) if dur > 0.0 => {
            (((ui.position_secs + cache_secs) / dur).clamp(0.0, 1.0)) as f32
        }
        _ => 0.0,
    };

    // Mirror of `render_top_gradient`'s stop pair,
    // reversed (opaque at the bottom edge). The control cluster is a child
    // of this div, so the scrim paints behind it by construction.
    let bar = div()
        .absolute()
        .left_0()
        .right_0()
        .bottom_0()
        .h(px(BOTTOM_GRADIENT_HEIGHT))
        .bg(linear_gradient(
            180.,
            linear_color_stop(rgba(theme::TRANSPARENT), 0.0),
            linear_color_stop(rgba(theme::tint(theme::NOTTE, BOTTOM_SCRIM_ALPHA)), 1.0),
        ))
        .flex()
        .flex_col()
        .justify_end()
        .px(px(SCRUB_MARGIN))
        .pb(px(16.))
        .gap_2()
        .child(scrub_row(ui, viewport.width, buffered_frac, root.clone()))
        .child(render_controls_row(ui, paused, is_fullscreen, root.clone()));

    wrap.child(render_top_gradient())
        .children(render_top_left_stack(
            ui,
            info,
            true,
            viewport,
            root.clone(),
        ))
        .child(bar)
        .children(render_center_flash(ui))
        .children(render_speed_boost_chip(ui))
        .children(render_trickplay_preview(ui, viewport))
        .children(render_toast(ui))
        .children(render_skip_toast(ui, root.clone()))
        .children(render_next_episode_card(ui, store, root.clone(), true, cx))
        .children(render_skip_pill(ui, root))
        .into_any_element()
}

/// Top gradient scrim, mirroring `render_osd`'s bottom
/// `bar` scrim -- opaque edge at the top of the screen (where the title/
/// breadcrumb stack needs contrast), fading to transparent at the band's
/// bottom edge.
fn render_top_gradient() -> impl IntoElement {
    div()
        .absolute()
        .left_0()
        .right_0()
        .top_0()
        .h(px(GRADIENT_HEIGHT))
        .bg(linear_gradient(
            180.,
            linear_color_stop(rgba(theme::tint(theme::NOTTE, 0xbf)), 0.0),
            linear_color_stop(rgba(theme::TRANSPARENT), 1.0),
        ))
}

/// §1.8: top-left title/breadcrumb, fades with the OSD. §2.4: clickable
/// when there's a series to go to -- collapses to Miniplayer and opens the
/// series' Detail page, pre-selected to this episode's season
/// (`Root::open_series_from_player`). Flex child of
/// `render_top_left_stack`'s anchored container.
fn render_title(ui: &PlayerUiState, root: WeakEntity<Root>) -> impl IntoElement {
    let clickable = ui.breadcrumb_clickable();
    // Section role (20px Semibold, Part B §1's ramp) -- the closest fit
    // for a prominent label that isn't a hero, giving weight contrast
    // against the Regular-weight time labels beside it. Color is the
    // literal `rgb(theme::PANNA)`, the OSD-over-video exemption also used
    // by the scrub row's time labels. Must truncate with a real ellipsis
    // rather than overflow the gradient bar.
    //
    // Regression note: `.max_w(700.)` alone with `clamped_line`'s
    // `.truncate()` collapsed the breadcrumb to a bare "…" -- this flex
    // child has no definite width for Taffy to resolve against, only an
    // upper bound, so truncate measured against zero. `.w(px(700.))` (a
    // real width) is what fixes it.
    let mut el = clamped_line(ui.breadcrumb_title(), px(28.))
        .id("osd-title")
        .w(px(700.))
        .text_color(rgb(theme::PANNA))
        .text_xl()
        .font_weight(FontWeight::SEMIBOLD);
    if clickable {
        // This is a control, not an indicator, so hover uses dim +
        // underline rather than accent color -- same "this is a link"
        // treatment as `detail.rs::breadcrumb_element`, which stays on
        // `theme::ACCENT` since page chrome reads differently from OSD
        // chrome over video.
        el = el
            .cursor_pointer()
            .hover(|s| s.text_color(rgba(theme::TEXT_SECONDARY)).underline())
            .on_click(move |_event, _window, cx| {
                let _ = root.update(cx, |root, cx| root.open_series_from_player(cx));
            });
    }
    el
}

/// C.6.2: OSD title and info overlay (`I`) are flex children of one
/// `flex_col` anchored once at `top(16)/left(SCRUB_MARGIN)`, avoiding two
/// hand-tuned offsets that collide. `show_title` is `false` from
/// `render_osd`'s `!ui.osd_visible` branch; the info overlay still renders
/// independent of OSD auto-hide, same precedent as the skip pill/toast.
fn render_top_left_stack(
    ui: &PlayerUiState,
    info: &PlayerLiveInfo,
    show_title: bool,
    viewport: Size<Pixels>,
    root: WeakEntity<Root>,
) -> Option<impl IntoElement> {
    if !show_title && !ui.info_overlay {
        return None;
    }
    Some(
        div()
            .absolute()
            .top(px(16.))
            .left(px(SCRUB_MARGIN))
            .flex()
            .flex_col()
            .gap(theme::SPACE_SNUG)
            .when(show_title, |d| d.child(render_title(ui, root)))
            .when(ui.info_overlay, |d| {
                d.child(render_info_overlay(
                    ui,
                    info,
                    info_overlay_max_h(viewport, show_title),
                ))
            }),
    )
}

/// How tall the info panel can get before it scrolls, the
/// tighter of two bounds: clear of the OSD (subtracting
/// `BOTTOM_GRADIENT_HEIGHT`, the top/bottom 16px margins, and the title
/// block's height when stacked above), and never more than 70% of the
/// window so the panel reads as an overlay, not a sidebar. Floored at
/// 160px, below which the panel would be a scroll-only sliver.
fn info_overlay_max_h(viewport: Size<Pixels>, show_title: bool) -> Pixels {
    /// `render_title`'s block plus the stack's `SPACE_SNUG` gap -- fixed
    /// rather than measured, since GPUI 0.2.2 has no layout-measure API
    /// (same limitation as `ui/spec_strip.rs`).
    const TITLE_ALLOWANCE: f32 = 72.0;
    let h = f32::from(viewport.height);
    let title = if show_title { TITLE_ALLOWANCE } else { 0.0 };
    let clear_of_osd = h - 16.0 - title - BOTTOM_GRADIENT_HEIGHT - 16.0;
    px(clear_of_osd.min(h * 0.7).max(160.0))
}

/// §1.9: center flash -- icon over a translucent circle, fade in/hold/fade
/// out (~720ms) via the same `with_animation` primitive
/// `cards.rs::art_element` uses. Fires independent of `ui.osd_visible`.
fn render_center_flash(ui: &PlayerUiState) -> Option<impl IntoElement> {
    let (kind, at) = ui.last_flash?;
    if at.elapsed() >= Duration::from_millis(FLASH_TOTAL_MS as u64) {
        return None;
    }
    let icon_path = match kind {
        FlashKind::Play => "icons/play.svg",
        FlashKind::Pause => "icons/pause.svg",
        FlashKind::SkipBack(_) => "icons/rotate-ccw.svg",
        FlashKind::SkipForward(_) => "icons/rotate-cw.svg",
    };
    let numeral = match kind {
        FlashKind::SkipBack(secs) | FlashKind::SkipForward(secs) => Some(secs),
        FlashKind::Play | FlashKind::Pause => None,
    };
    let key = SharedString::from(format!("osd-flash-{}", ui.flash_seq));

    let inner = div()
        .relative()
        .w(px(112.))
        .h(px(112.))
        .flex()
        .items_center()
        .justify_center()
        .rounded_full()
        .bg(rgba(theme::tint(theme::NOTTE, 0xaa)))
        .child(
            svg()
                .path(icon_path)
                .w(px(72.))
                .h(px(72.))
                .text_color(rgba(theme::TEXT_PRIMARY)),
        )
        .when_some(numeral, |d, secs| {
            d.child(
                div()
                    .absolute()
                    .inset_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_size(px(22.))
                    .text_color(rgba(theme::TEXT_PRIMARY))
                    .child(secs.to_string()),
            )
        })
        .with_animation(
            key,
            Animation::new(Duration::from_millis(FLASH_TOTAL_MS as u64)),
            |el, delta| {
                let opacity = if delta < FLASH_FADE_IN_FRAC {
                    delta / FLASH_FADE_IN_FRAC
                } else if delta < FLASH_HOLD_END_FRAC {
                    1.0
                } else {
                    let t = (delta - FLASH_HOLD_END_FRAC) / (1.0 - FLASH_HOLD_END_FRAC);
                    (1.0 - t).max(0.0)
                };
                el.opacity(opacity)
            },
        );

    Some(
        div()
            .absolute()
            .inset_0()
            .flex()
            .items_center()
            .justify_center()
            .child(inner),
    )
}

/// Option-key speed hold: persistent "2×"/"0.5×" chip driven by
/// `ui.speed_boost_rate` (no expiry check, unlike `render_center_flash`'s
/// timed fade). Top-right corner, rendered independent of `ui.osd_visible`
/// so it stays visible even once the OSD fades during a long hold.
fn render_speed_boost_chip(ui: &PlayerUiState) -> Option<impl IntoElement> {
    let rate = ui.speed_boost_rate?;
    Some(
        div()
            .id("osd-speed-boost-chip")
            .absolute()
            .top(px(SCRUB_MARGIN))
            .right(px(SCRUB_MARGIN))
            .px_3()
            .py_1()
            .rounded_full()
            .bg(rgba(theme::tint(theme::SURFACE_RAISED, 0xee)))
            .font_family(theme::FONT_MONO)
            .text_size(theme::TEXT_METADATA)
            .text_color(rgb(theme::ACCENT))
            .child(format_speed_boost_label(rate)),
    )
}

/// "2×" for a whole-number rate, "0.5×" for the fractional one -- explicit
/// branch rather than a plain `{rate}×` format so a future non-half-step
/// rate doesn't silently produce "2.5×"-style ugliness.
fn format_speed_boost_label(rate: f64) -> String {
    if (rate - rate.trunc()).abs() < f64::EPSILON {
        format!("{}×", rate as i64)
    } else {
        format!("{rate}×")
    }
}

/// §1.10/§1.14/§1.15: plain icon button -- `hit`x`hit` hit area, `icon`x
/// `icon` SVG tinted via `text_color` (alpha-channel mask,
/// `docs/DESIGN-PLAYER-NAV.md` §1.14).
fn icon_button(
    id: impl Into<SharedString>,
    icon_path: &'static str,
    hit: f32,
    icon: f32,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id.into())
        .relative()
        .w(px(hit))
        .h(px(hit))
        .flex()
        .items_center()
        .justify_center()
        .rounded_md()
        .cursor_pointer()
        .hover(|s| s.bg(rgba(theme::ICON_HOVER_FILL)))
        .child(
            svg()
                .path(icon_path)
                .w(px(icon))
                .h(px(icon))
                .text_color(rgba(theme::TEXT_PRIMARY)),
        )
}

fn secondary_button(
    id: impl Into<SharedString>,
    icon_path: &'static str,
) -> gpui::Stateful<gpui::Div> {
    icon_button(id, icon_path, SECONDARY_HIT, SECONDARY_ICON)
}

fn primary_button(
    id: impl Into<SharedString>,
    icon_path: &'static str,
) -> gpui::Stateful<gpui::Div> {
    icon_button(id, icon_path, PRIMARY_HIT, PRIMARY_ICON)
}

/// §1.14 rows 4/5: the numeral on a skip button is the configured skip
/// length, so the button always says how far it seeks.
fn skip_button_numeral(secs: u32) -> SharedString {
    SharedString::from(secs.to_string())
}

/// §1.14 rows 4/5: the skip buttons -- the rotate icon plus the skip-length
/// numeral overlaid as text (not baked into the icon file), so one icon
/// serves every configured length.
fn skip_button(
    id: impl Into<SharedString>,
    icon_path: &'static str,
    secs: u32,
) -> gpui::Stateful<gpui::Div> {
    secondary_button(id, icon_path).child(
        div()
            .absolute()
            .inset_0()
            .flex()
            .items_center()
            .justify_center()
            .text_size(px(9.))
            .text_color(rgba(theme::TEXT_PRIMARY))
            .child(skip_button_numeral(secs)),
    )
}

fn divider() -> impl IntoElement {
    div()
        .w(px(1.))
        .h(px(20.))
        .bg(rgba(theme::tint(theme::PANNA, 0x33)))
}

/// §1.6: mute icon button + a horizontal slider that expands on hover,
/// collapsing 400ms after the pointer leaves (`tick_volume_collapse`).
/// Percentage only shows while expanded, not as permanent chrome.
fn volume_control(ui: &PlayerUiState, root: WeakEntity<Root>) -> impl IntoElement {
    let is_muted = ui.muted || ui.volume == 0;
    let mute_icon = if is_muted {
        "icons/volume-x.svg"
    } else {
        "icons/volume-2.svg"
    };
    let frac = if ui.muted {
        0.0
    } else {
        ui.volume as f32 / 100.0
    };

    let mute_root = root.clone();
    let hover_root = root.clone();
    let down_root = root.clone();
    let move_root = root.clone();
    let up_root = root.clone();
    let up_out_root = root;

    let mute_btn = secondary_button("osd-mute", mute_icon).on_click(move |_e, _w, cx| {
        let _ = mute_root.update(cx, |root, cx| root.handle_remote_command_volume_toggle(cx));
    });

    let slider_w = if ui.volume_expanded {
        VOLUME_SLIDER_PX
    } else {
        0.0
    };
    let slider = div()
        .id("osd-volume-slider")
        .relative()
        .h(px(20.))
        .w(px(slider_w))
        .overflow_hidden()
        .flex()
        .items_center()
        .when(ui.volume_expanded, |d| {
            d.child(
                div()
                    .absolute()
                    .left(px(0.))
                    .right(px(0.))
                    .h(px(4.))
                    .rounded_full()
                    .bg(rgba(theme::tint(theme::PANNA, 0x33)))
                    .child(
                        // Volume fill is a control, not an indicator
                        // (unlike the scrub bar's played-progress fill,
                        // kept on `theme::ACCENT`), so it gets the same
                        // neutral-white treatment as other §2-audited
                        // control fills.
                        div()
                            .h_full()
                            .rounded_full()
                            .bg(rgba(theme::TEXT_PRIMARY))
                            .w(gpui::relative(frac)),
                    ),
            )
        })
        .on_mouse_down(MouseButton::Left, move |event, _window, cx| {
            let x = f32::from(event.position.x);
            let _ = down_root.update(cx, |root, cx| root.start_volume_drag(x, cx));
        })
        .on_mouse_move(move |event, _window, cx| {
            if event.pressed_button != Some(MouseButton::Left) {
                return;
            }
            let x = f32::from(event.position.x);
            let _ = move_root.update(cx, |root, cx| root.drag_volume(x, cx));
        })
        .on_mouse_up(MouseButton::Left, move |_event, _window, cx| {
            let _ = up_root.update(cx, |root, cx| root.end_volume_drag(cx));
        })
        .on_mouse_up_out(MouseButton::Left, move |_event, _window, cx| {
            let _ = up_out_root.update(cx, |root, cx| root.end_volume_drag(cx));
        });

    div()
        .id("osd-volume")
        .flex()
        .items_center()
        .gap_1()
        .on_hover(move |hovering, _window, cx| {
            let _ = hover_root.update(cx, |root, cx| root.set_volume_hover(*hovering, cx));
        })
        .child(mute_btn)
        .child(slider)
}

/// Three-cluster controls row -- volume (left edge),
/// transport (stop/prev-chapter/skip-back/play-pause/skip-forward/
/// next-chapter, centered), track pickers + decision-summary pill +
/// window-state (right edge, fullscreen last so it sits in the corner).
fn render_controls_row(
    ui: &PlayerUiState,
    paused: bool,
    is_fullscreen: bool,
    root: WeakEntity<Root>,
) -> impl IntoElement {
    let stop_root = root.clone();
    let prev_chapter_root = root.clone();
    let skip_back_root = root.clone();
    let play_root = root.clone();
    let skip_fwd_root = root.clone();
    let next_chapter_root = root.clone();
    let audio_root = root.clone();
    let sub_root = root.clone();
    let info_root = root.clone();
    let mini_root = root.clone();
    let fullscreen_root = root.clone();

    // Configured skip lengths, not a hardcoded ±10.0.
    let skip_back = f64::from(ui.skip_back_secs);
    let skip_forward = f64::from(ui.skip_forward_secs);

    let play_icon = if paused {
        "icons/play.svg"
    } else {
        "icons/pause.svg"
    };
    let fullscreen_icon = if is_fullscreen {
        "icons/minimize.svg"
    } else {
        "icons/maximize.svg"
    };

    let left_cluster = div()
        .flex()
        .items_center()
        .gap_2()
        .child(volume_control(ui, root.clone()));

    // Transport cluster, centered via the two `flex_1`
    // spacers in the row assembly below.
    let center_cluster = div()
        .flex()
        .items_center()
        .gap_2()
        // Stop lives in the transport cluster, not the utility cluster --
        // it IS a playback function. Leftmost: stop / prev / skip back /
        // play / skip forward / next.
        .child(
            secondary_button("osd-stop", "icons/square.svg").on_click(move |_e, _w, cx| {
                let _ = stop_root.update(cx, |root, cx| root.stop_playback(cx));
            }),
        )
        .child(
            secondary_button("osd-prev-chapter", "icons/skip-back.svg").on_click(
                move |_e, _w, cx| {
                    let _ = prev_chapter_root.update(cx, |root, cx| root.jump_chapter(false, cx));
                },
            ),
        )
        .child(
            // Honors the configured back length the ← key uses
            // (`ui.skip_back_secs`), not a hardcoded 10s.
            skip_button("osd-skip-back", "icons/rotate-ccw.svg", ui.skip_back_secs).on_click(
                move |_e, _w, cx| {
                    let _ = skip_back_root.update(cx, |root, cx| root.seek_delta(-skip_back, cx));
                },
            ),
        )
        .child(
            primary_button("osd-playpause", play_icon).on_click(move |_e, _w, cx| {
                let _ = play_root.update(cx, |root, cx| root.toggle_play_pause_click(cx));
            }),
        )
        .child(
            skip_button(
                "osd-skip-forward",
                "icons/rotate-cw.svg",
                ui.skip_forward_secs,
            )
            .on_click(move |_e, _w, cx| {
                let _ = skip_fwd_root.update(cx, |root, cx| root.seek_delta(skip_forward, cx));
            }),
        )
        .child(
            secondary_button("osd-next-chapter", "icons/skip-forward.svg").on_click(
                move |_e, _w, cx| {
                    let _ = next_chapter_root.update(cx, |root, cx| root.jump_chapter(true, cx));
                },
            ),
        );

    // Status text is a spec-strip cell (`ui::spec_strip`'s
    // `spec_separator`/`spec_cell`, shared with Detail pages and this
    // player's info popover), not a bare label. Emphasis comes from the
    // strip's own three-tier `classify` (DIRECT PLAY -> Notable/PANNA,
    // Transcode -> Baseline/GRIGIO), not a hand-picked color, per §6's
    // "never hard-code a weight per row".
    let status_pill = (!ui.decision_summary.is_empty()).then(|| {
        let label = playback_mode_label(&ui.decision_summary, ui.is_direct_play);
        let weight = classify(&label);
        div()
            .flex()
            .flex_row()
            .items_center()
            .font_family(MONO_FAMILY)
            .text_size(theme::TEXT_SPEC)
            .child(spec_separator())
            .child(spec_cell(label, weight))
    });

    // Part B §9: OSD audio/subtitle buttons anchor upward-left from the
    // bottom-bar button so the popover opens toward the video, not off the
    // bottom of the window. `AnchorCorner::BottomRight` + offset
    // `(SECONDARY_HIT, -space.tight)` lands the anchor at the button's
    // top-right corner; no live bounds query needed (`ui/popover.rs`).
    let popover_offset = point(px(SECONDARY_HIT), px(-8.));
    let audio_open = ui.picker == Some(PickerKind::Audio);
    let sub_open = ui.picker == Some(PickerKind::Subtitle);
    let audio_ui_tracks = ui.tracks.clone();
    let audio_ui_filter = ui.picker_filter.clone();
    let audio_ui_highlight = ui.picker_highlight;
    let sub_ui_tracks = ui.tracks.clone();
    let sub_ui_filter = ui.picker_filter.clone();
    let sub_ui_highlight = ui.picker_highlight;
    let audio_panel_root = root.clone();
    let sub_panel_root = root.clone();
    let right_cluster = div()
        .flex()
        .items_center()
        .gap_2()
        .child(popover_trigger(
            "osd-audio-popover",
            secondary_button("osd-audio", "icons/audio-lines.svg").on_click(move |_e, _w, cx| {
                let _ =
                    audio_root.update(cx, |root, cx| root.open_track_picker(TrackKind::Audio, cx));
            }),
            audio_open,
            AnchorCorner::BottomRight,
            popover_offset,
            move || {
                render_track_picker_panel_from_parts(
                    &audio_ui_tracks,
                    audio_ui_filter.as_str(),
                    audio_ui_highlight,
                    TrackKind::Audio,
                    audio_panel_root,
                )
            },
        ))
        .child(popover_trigger(
            "osd-subtitle-popover",
            secondary_button("osd-subtitle", "icons/captions.svg").on_click(move |_e, _w, cx| {
                let _ = sub_root.update(cx, |root, cx| {
                    root.open_track_picker(TrackKind::Subtitle, cx)
                });
            }),
            sub_open,
            AnchorCorner::BottomRight,
            popover_offset,
            move || {
                render_track_picker_panel_from_parts(
                    &sub_ui_tracks,
                    sub_ui_filter.as_str(),
                    sub_ui_highlight,
                    TrackKind::Subtitle,
                    sub_panel_root,
                )
            },
        ))
        .children(status_pill)
        .child(
            secondary_button("osd-info", "icons/info.svg")
                // Pins `ICON_HOVER_FILL` on while the info
                // panel is open, so the button reads as held-down -- the
                // one state a toggle needs to show.
                .when(ui.info_overlay, |d| d.bg(rgba(theme::ICON_HOVER_FILL)))
                .on_click(move |_e, _w, cx| {
                    let _ = info_root.update(cx, |root, cx| root.toggle_info_overlay(cx));
                }),
        )
        .child(divider())
        .child(
            secondary_button("osd-mini", "icons/picture-in-picture-2.svg").on_click(
                move |_e, _w, cx| {
                    let _ = mini_root.update(cx, |root, cx| root.toggle_miniplayer(cx));
                },
            ),
        )
        .child(
            secondary_button("osd-fullscreen", fullscreen_icon).on_click(move |_e, _w, cx| {
                let _ = fullscreen_root.update(cx, |root, cx| root.request_fullscreen_toggle(cx));
            }),
        );

    // Volume at the left edge, transport centered via two
    // `flex_1` spacers, track pickers/status/window-state at the right edge.
    div()
        .flex()
        .items_center()
        .h(px(44.))
        .child(left_cluster)
        .child(div().flex_1())
        .child(center_cluster)
        .child(div().flex_1())
        .child(right_cluster)
}

/// S/A track-cycle toast (docs/UX-SPEC.md item 2). Part B §13: `surface.panel` bg
/// via `ui::components::toast_shell`, plus a `motion.fade` (150ms)
/// fade-in/out as a single piecewise opacity curve over one `Animation`
/// spanning `TOAST_DURATION`, same shape `render_center_flash` uses.
/// `expire_toast`/`TOAST_DURATION` removes the toast right as the fade-out
/// finishes, so there's no separate "already faded" flag.
fn render_toast(ui: &PlayerUiState) -> Option<impl IntoElement> {
    let (text, _) = ui.toast.clone()?;
    let key = SharedString::from(format!("osd-toast-{}", ui.toast_seq));
    Some(
        div()
            .absolute()
            .top(px(24.))
            .left(px(0.))
            .right(px(0.))
            .flex()
            .justify_center()
            .child(
                crate::ui::components::toast_shell(div().text_sm().child(text)).with_animation(
                    key,
                    Animation::new(TOAST_DURATION),
                    |el, delta| {
                        let opacity = if delta < TOAST_FADE_FRAC {
                            delta / TOAST_FADE_FRAC
                        } else if delta > 1.0 - TOAST_FADE_FRAC {
                            ((1.0 - delta) / TOAST_FADE_FRAC).max(0.0)
                        } else {
                            1.0
                        };
                        el.opacity(opacity)
                    },
                ),
            ),
    )
}

/// Auto-skip "Undo" toast -- same visual shell as `render_toast`,
/// independent state/timer since the two can overlap. Clicking it, or
/// pressing `U`, calls `Root::undo_last_skip` to seek back to the pre-skip
/// position. Rendered independent of the OSD's idle-fade, same as
/// `render_toast`/`render_skip_pill`.
fn render_skip_toast(ui: &PlayerUiState, root: WeakEntity<Root>) -> Option<impl IntoElement> {
    let toast = ui.skip_toast.as_ref()?;
    let key = SharedString::from(format!("osd-skip-toast-{}", ui.skip_toast_seq));
    Some(
        div()
            .id("osd-skip-toast")
            .absolute()
            .top(px(24.))
            .left(px(0.))
            .right(px(0.))
            .flex()
            .justify_center()
            .child(
                crate::ui::components::toast_shell(
                    div()
                        .id("osd-skip-toast-inner")
                        .flex()
                        .items_center()
                        .gap_2()
                        .cursor_pointer()
                        .child(div().text_sm().child(toast.label.clone()))
                        // "Undo" is a control action inside the toast, not
                        // an indicator, so it gets an underline on
                        // `TEXT_PRIMARY` instead of accent color.
                        .child(
                            div()
                                .text_sm()
                                .text_color(rgba(theme::TEXT_PRIMARY))
                                .underline()
                                .child("Undo (U)"),
                        )
                        .on_click(move |_e, _w, cx| {
                            let _ = root.update(cx, |root, cx| root.undo_last_skip(cx));
                        }),
                )
                .with_animation(
                    key,
                    Animation::new(SKIP_UNDO_WINDOW),
                    |el, delta| {
                        let opacity = if delta < TOAST_FADE_FRAC {
                            delta / TOAST_FADE_FRAC
                        } else if delta > 1.0 - TOAST_FADE_FRAC {
                            ((1.0 - delta) / TOAST_FADE_FRAC).max(0.0)
                        } else {
                            1.0
                        };
                        el.opacity(opacity)
                    },
                ),
            ),
    )
}

/// §2.4's next-up card, design 1c: no container, the content sits on the
/// video over a radial NOTTE scrim pinned to the frame's bottom-right
/// corner; both ride up by the control zone only while the OSD shows.
/// Return/click plays next, Esc dismisses (`Root::handle_playback_keystroke`).
/// With autoplay off the rule shows its bare track and the numeral is
/// absent (§2.1 pass-out protection: never silent).
fn render_next_episode_card(
    ui: &PlayerUiState,
    store: &ImageStore,
    root: WeakEntity<Root>,
    osd_visible: bool,
    cx: &mut App,
) -> Option<impl IntoElement> {
    let next = ui.next_episode.as_ref()?;
    const CARD_W: f32 = 360.0;
    const CARD_RIGHT: f32 = 48.0;
    const CARD_BOTTOM: f32 = 96.0;
    const THUMB_W: f32 = 128.0;
    const THUMB_H: f32 = 72.0;
    const SCRIM_W: f32 = 550.0;
    const SCRIM_H: f32 = 310.0;
    const RULE_H: f32 = 2.0;
    let rise = if osd_visible {
        CONTROL_ZONE_HEIGHT
    } else {
        0.0
    };

    let thumb = crate::cards::art_element(
        &next.id,
        next.primary_tag.as_deref(),
        next.blurhash.as_deref(),
        ImageKind::Primary,
        THUMB_WIDTH,
        store,
        root.clone(),
        cx,
        true,
    );
    let title = crate::cards::display_title(&next.name);
    let runtime = next_episode_runtime_label(next.runtime_ticks);

    let elapsed = ui
        .next_episode_shown_at
        .map(|t| t.elapsed().as_secs_f64())
        .unwrap_or(0.0);
    let countdown = ui.next_episode_countdown_total_secs;
    let numeral = countdown
        .map(|total| next_episode_numeral(next_episode_remaining_whole_secs(total, elapsed)));
    let rule_fill = countdown.map(|total| {
        let key = SharedString::from(format!(
            "next-ep-rule-{}-{}",
            next.id, ui.next_episode_generation
        ));
        div()
            .absolute()
            .left_0()
            .top_0()
            .bottom_0()
            .bg(rgb(theme::PISTACCHIO))
            .with_animation(
                key,
                Animation::new(Duration::from_secs_f64(total.max(0.05))),
                |el, delta| el.w(gpui::relative((1.0 - delta).clamp(0.0, 1.0))),
            )
    });

    let mono = |size: f32, color: u32| {
        div()
            .font_family(theme::FONT_MONO)
            .text_size(px(size))
            .text_color(rgba(color))
    };
    let play_root = root.clone();
    let hint_root = root;
    let scrim = svg()
        .path("scrim/next-up.svg")
        .absolute()
        .right_0()
        .bottom(px(rise))
        .w(px(SCRIM_W))
        .h(px(SCRIM_H))
        .text_color(rgb(theme::NOTTE));
    let card = div()
        .id("osd-next-episode-card")
        .absolute()
        .right(px(CARD_RIGHT))
        .bottom(px(rise + CARD_BOTTOM))
        .w(px(CARD_W))
        .flex()
        .flex_col()
        .child(
            div()
                .id("osd-next-episode-play")
                .flex()
                .flex_row()
                .gap_3()
                .cursor_pointer()
                .on_click({
                    let play_root = play_root.clone();
                    move |_e, _w, cx| {
                        let _ = play_root.update(cx, |root, cx| root.play_next_episode_now(cx));
                    }
                })
                .child(
                    div()
                        .w(px(THUMB_W))
                        .h(px(THUMB_H))
                        .rounded(px(2.))
                        .overflow_hidden()
                        .flex_shrink_0()
                        .child(thumb),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .child(mono(12., theme::GRIGIO).child("UP NEXT"))
                        .child(
                            div()
                                .text_size(px(15.))
                                .line_height(px(17.4))
                                .font_weight(FontWeight::MEDIUM)
                                .text_color(rgb(theme::PANNA))
                                .child(title),
                        )
                        .children(runtime.map(|r| mono(11., theme::GRIGIO).child(r))),
                ),
        )
        .child(
            div()
                .mt_2()
                .relative()
                .w_full()
                .h(px(RULE_H))
                .bg(rgba(theme::tint(theme::PANNA, 0x29)))
                .children(rule_fill),
        )
        .child(
            div()
                .mt_2()
                .flex()
                .flex_row()
                .justify_between()
                .items_baseline()
                .child(
                    mono(11., theme::GRIGIO)
                        .id("osd-next-episode-dismiss")
                        .cursor_pointer()
                        .on_click(move |_e, _w, cx| {
                            let _ =
                                hint_root.update(cx, |root, cx| root.dismiss_next_episode_card(cx));
                        })
                        .child("ESC TO DISMISS"),
                )
                .child(
                    div()
                        .id("osd-next-episode-play-now")
                        .flex()
                        .flex_row()
                        .items_baseline()
                        .gap_2()
                        .cursor_pointer()
                        .on_click(move |_e, _w, cx| {
                            let _ = play_root.update(cx, |root, cx| root.play_next_episode_now(cx));
                        })
                        .child(mono(12., theme::GRIGIO).child("RETURN"))
                        .child(
                            div()
                                .text_size(px(14.))
                                .font_weight(FontWeight::BOLD)
                                .text_color(rgb(theme::PISTACCHIO))
                                .child("Play Next"),
                        )
                        .children(numeral.map(|n| {
                            mono(14., theme::PANNA)
                                .font_weight(FontWeight::BOLD)
                                .child(format!("IN {n}"))
                        })),
                ),
        );
    Some(div().absolute().inset_0().child(scrim).child(card))
}

/// docs/UX-SPEC.md §4: "Skip <type>" pill, shown while the playhead
/// is inside a Media Segment whose action is `Ask` (`AutoSkip`/`Off` never
/// show a pill). Click/Return seeks past it; unlike the rest of the OSD,
/// never tied to the idle-fade, so it stays available for the whole
/// active-segment window.
fn render_skip_pill(ui: &PlayerUiState, root: WeakEntity<Root>) -> Option<impl IntoElement> {
    let (seg, decision) = ui.active_segment_decision()?;
    if decision != SegmentDecision::Pill {
        return None;
    }
    let label = skip_pill_label(seg.type_);
    Some(
        div()
            .id("osd-skip-pill")
            .absolute()
            .right(px(SCRUB_MARGIN))
            .bottom(px(CONTROL_ZONE_HEIGHT + 16.))
            .px_3()
            .py_2()
            .rounded_md()
            .cursor_pointer()
            .bg(rgba(theme::tint(theme::SURFACE_RAISED, 0xee)))
            .text_color(rgba(theme::TEXT_PRIMARY))
            // The skip pill is a control, not an indicator, so hover gets
            // the same `surface.overlay` brighten every other hoverable
            // surface uses.
            .hover(|s| s.bg(rgb(theme::SURFACE_OVERLAY)))
            .child(label)
            .on_click(move |_e, _w, cx| {
                let _ = root.update(cx, |root, cx| root.skip_active_segment(cx));
            }),
    )
}

/// docs/UX-SPEC.md §4: trickplay preview tile above the cursor while hovering/
/// dragging the scrubber. `None` when there's no manifest for this item
/// (older server) -- the scrubber still works without the preview. §1.4
/// item 6: also shows the chapter name under the timestamp.
fn render_trickplay_preview(
    ui: &PlayerUiState,
    viewport: Size<Pixels>,
) -> Option<impl IntoElement> {
    let frac = ui.scrub.hover_frac?;
    let meta = ui.trickplay.as_ref()?;
    let cache = ui.trickplay_cache.as_ref()?;
    let time_secs = (frac as f64 * ui.duration_secs).max(0.0);
    let time_ms = (time_secs * 1000.0) as u32;
    // Synchronous cache peek only (no `cx`/fetch here) -- `root.rs`'s
    // scrub-hover handler drives `TrickplayCache::tile_for_ms`; this just
    // paints whatever's already resolved.
    let tile = cache.peek(meta, time_ms)?;
    let left = (viewport.width * frac).max(px(0.));
    let chapter_name = ui.chapter_at(time_secs);
    Some(
        div()
            .absolute()
            .left(left)
            .bottom(px(CONTROL_ZONE_HEIGHT + 12.))
            .flex()
            .flex_col()
            .items_center()
            .child(
                img(tile)
                    .w(px(meta.width as f32))
                    .h(px(meta.height as f32))
                    .rounded_md()
                    .border_2()
                    .border_color(rgba(theme::TEXT_PRIMARY)),
            )
            .child(
                div()
                    .mt_1()
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .bg(rgba(theme::tint(theme::NOTTE, 0xcc)))
                    .flex()
                    .flex_col()
                    .items_center()
                    .text_size(theme::TEXT_CAPTION)
                    .text_color(rgba(theme::TEXT_PRIMARY))
                    .child(format_time(time_secs))
                    .when_some(chapter_name, |d, name| {
                        d.child(
                            div()
                                .text_color(rgba(theme::TEXT_TERTIARY))
                                .child(SharedString::from(name.to_string())),
                        )
                    }),
            ),
    )
}

/// `bps` (bits/sec) as e.g. `"4.32 Mbps"`, or `"n/a"` if mpv hasn't
/// measured anything yet -- shared by the video/audio bitrate rows below.
fn fmt_bitrate(bps: Option<f64>) -> String {
    match bps {
        Some(bps) if bps > 0.0 => format!("{:.2} Mbps", bps / 1_000_000.0),
        _ => "n/a".to_string(),
    }
}

/// Info overlay's "Buffer" row -- pairs mpv's duration guess
/// (`demuxer-cache-duration`, "very unreliable" per the mpv manual) with
/// the actual byte count banked ahead (`fw-bytes`), so there's a real
/// number even when the duration guess is missing. Still renders a
/// `paused-for-cache` note when active, even with both values missing.
fn fmt_buffer(cache: &player::CacheState) -> String {
    let secs = cache
        .demuxer_cache_duration_secs
        .map(|s| format!("{s:.1}s ahead"))
        .unwrap_or_else(|| "? ahead".to_string());
    let bytes = cache
        .fw_bytes
        .map(|b| format!("{:.1} MB", b as f64 / 1_048_576.0))
        .unwrap_or_else(|| "? MB".to_string());
    let mut out = format!("{secs} ({bytes})");
    if cache.paused_for_cache {
        out.push_str(" -- buffering");
    }
    out
}

/// Info overlay's "Network" row -- mpv's measured cache-fill throughput
/// (`cache-speed`), the closest live "is the link keeping up" signal,
/// unlike `video_bitrate`/`audio_bitrate` which describe the content, not
/// the link.
fn fmt_network_speed(cache_speed_bps: Option<i64>) -> String {
    match cache_speed_bps {
        Some(bps) if bps > 0 => format!("{:.2} Mbit/s current", (bps as f64 * 8.0) / 1_000_000.0),
        _ => "n/a".to_string(),
    }
}

/// Info overlay's "Dropped frames" row -- `N (decoder M)`: `N` is the
/// VO-side count (the one that moves under this crate's default
/// `--framedrop=vo`, see `FrameDropStats`), `M` the decoder-side count,
/// itself a signal something unusual happened upstream.
fn fmt_frame_drops(stats: &player::FrameDropStats) -> String {
    match (stats.vo, stats.decoder) {
        (None, None) => "n/a".to_string(),
        (vo, decoder) => format!("{} (decoder {})", vo.unwrap_or(0), decoder.unwrap_or(0)),
    }
}

/// `I` key: item name, container/codec/resolution, DirectPlay vs.
/// Transcode with reasons, hwdec, HDR pipeline info, measured bitrates,
/// fps, dropped frames, and cache depth (docs/UX-SPEC.md §6: transcode fallback is
/// never silent). Anything mpv hasn't measured yet shows "n/a"/"unknown"
/// rather than omitting the row.
fn render_info_overlay(
    ui: &PlayerUiState,
    info: &PlayerLiveInfo,
    max_h: Pixels,
) -> impl IntoElement {
    // Decision line is just the mode; reasons move to
    // their own row, rendered only when transcoding, so Direct Play (the
    // common path) doesn't carry a noisy parenthetical.
    let decision_line = if ui.is_direct_play {
        "Direct Play".to_string()
    } else {
        "Transcoding".to_string()
    };
    let transcode_reason = (!ui.is_direct_play).then(|| {
        if ui.decision_reasons.is_empty() {
            // Transcode with no named reason is surfaced as an explicit
            // gap (docs/UX-SPEC.md §6 "never silent"), not omitted.
            "unspecified".to_string()
        } else {
            ui.decision_reasons.join("; ")
        }
    });
    let row = |label: &'static str, value: String| {
        div()
            .flex()
            .gap_2()
            .child(
                div()
                    .w(px(110.))
                    .text_color(rgba(theme::TEXT_TERTIARY))
                    .child(label),
            )
            .child(div().text_color(rgba(theme::TEXT_PRIMARY)).child(value))
    };

    // The live diagnostic rows below describe the running mpv pipeline
    // (hwdec, measured bitrates, buffer, network, frame drops); the
    // MediaInfo-style breakdown at the foot of the panel covers everything
    // server-side `MediaSourceInfo` can answer instead.
    let mut facts = MediaFacts::from_source(&ui.media_source);
    if facts.container.is_none() {
        // `PlaybackInfo`'s chosen source doesn't always carry a container
        // string; `ui.container` (the enrichment fetch's DTO-level one) is
        // the same fact from a second source, so it's the fallback.
        facts.container = ui.container.as_deref();
    }
    let breakdown = facts.breakdown();

    // HDR pipeline info: `gamma` (the transfer function/EOTF) is the most
    // direct "is this actually HDR" signal ("pq"/"hlg" vs. SDR's
    // "bt.1886"), so it leads; matrix/primaries follow for detail.
    let color = match (
        &info.video_params.gamma,
        &info.video_params.colormatrix,
        &info.video_params.primaries,
    ) {
        (None, None, None) => "unknown".to_string(),
        (gamma, matrix, primaries) => format!(
            "{} ({} / {})",
            gamma.as_deref().unwrap_or("?"),
            matrix.as_deref().unwrap_or("?"),
            primaries.as_deref().unwrap_or("?"),
        ),
    };

    // C.6.2/C.6.3: positioned by `render_top_left_stack`'s shared anchor
    // (both this and the title sit inside it), styled on `surface.panel`
    // chrome (`radius.panel`, `shadow E2`).
    div()
        // 420px -- the breakdown's 92px-label + value rows
        // (`media_breakdown`) read best at this width, and a file PATH row
        // is the widest thing in the panel.
        .id("osd-info-overlay")
        .w(px(420.))
        // `max_h` is computed by `render_top_left_stack`
        // from the live viewport with the OSD's bottom band subtracted;
        // anything past it scrolls inside the panel instead of colliding
        // with the controls.
        .max_h(max_h)
        .overflow_y_scroll()
        .p_3()
        .rounded_lg()
        // ~60% alpha on `surface.raised` (the darker of
        // the panel tokens, buying back text contrast at this alpha) so
        // the video reads through rather than sitting under a near-solid
        // slab; `shadow E2` still separates the panel from the video.
        .bg(rgba(theme::tint(theme::SURFACE_RAISED, 0x99)))
        .shadow(theme::shadow_e2())
        .flex()
        .flex_col()
        .gap_1()
        .text_sm()
        .child(row(
            "Playback",
            if ui.is_direct_play {
                decision_line.clone()
            } else {
                format!("⚠ {decision_line}")
            },
        ))
        // Transcode-only detail rows under the mode they qualify
        // -- reason first, then the targeted bitrate.
        .children(transcode_reason.map(|r| row("Transcode reason", r)))
        .children((!ui.is_direct_play).then(|| {
            row(
                "Target bitrate",
                match ui.max_bitrate {
                    Some(bps) => format!("{:.1} Mbps", bps as f64 / 1_000_000.0),
                    // No cap requested; naming "server default" is more
                    // honest than printing a number this client never sent.
                    None => "server default".to_string(),
                },
            )
        }))
        .child(row(
            "hwdec",
            info.hwdec.clone().unwrap_or_else(|| "unknown".into()),
        ))
        .child(row(
            "Pixel format",
            info.video_params
                .pixel_format
                .clone()
                .unwrap_or_else(|| "unknown".into()),
        ))
        .child(row("Color (gamma / matrix / primaries)", color))
        .child(row(
            "FPS",
            match info.container_fps {
                Some(fps) if fps > 0.0 => format!("{fps:.3}"),
                _ => "n/a".to_string(),
            },
        ))
        .child(row("Video bitrate", fmt_bitrate(info.video_bitrate_bps)))
        .child(row("Audio bitrate", fmt_bitrate(info.audio_bitrate_bps)))
        // Per-server playback preference -- shown whenever active,
        // not just when it happens to be the reason a
        // Transcode decision was made. Scoped to the Direct Play
        // path only: on a transcode the very same `max_bitrate` is already
        // shown, under the more precise name, by the "Target bitrate" row
        // above -- two rows carrying one number would be a duplication,
        // exactly the kind avoided elsewhere in this panel.
        .children(ui.is_direct_play.then(|| {
            row(
                "Bitrate cap",
                match ui.max_bitrate {
                    Some(bps) => format!("{:.1} Mbps", bps as f64 / 1_000_000.0),
                    None => "Auto".to_string(),
                },
            )
        }))
        // Feedback item 2 ("no insight into the buffer cache size or
        // fill")/item 3 ("no frame-drop stats at all") -- Buffer/Network/
        // Dropped frames below are the direct response to both.
        .child(row("Buffer", fmt_buffer(&info.cache)))
        .child(row(
            "Network",
            fmt_network_speed(info.cache.cache_speed_bps),
        ))
        .child(row("Dropped frames", fmt_frame_drops(&info.frame_drops)))
        .child(row("Media source", ui.media_source_id.clone()))
        // The same MediaInfo-style breakdown the Detail pages' strip
        // opens, inlined under the live rows rather than a second nested
        // popover -- this panel already is the "show me everything"
        // affordance.
        .when(!breakdown.is_empty(), |d| {
            d.child(
                div()
                    .mt_2()
                    .pt_2()
                    .border_t_1()
                    .border_color(rgb(theme::SURFACE_HAIRLINE))
                    .child(media_breakdown(&breakdown)),
            )
        })
}

/// Part B §10's S/A picker on `ui::popover`'s shared primitives (Part B
/// §9): leading `check.svg` on the selected row, row background as the
/// hover/keyboard-highlight state, `popover_panel`'s `max_h` capped at
/// 320px for this panel's per-row lang/codec metadata. Deviation from
/// spec: the "Subtitles"/"Forced-SDH" split is still out of scope, blocked
/// until `Track::forced` existed (`crates/player/src/lib.rs`). Takes
/// owned/cloned `PlayerUiState` pieces since it's built lazily in a `move
/// ||` closure at the call site in `render_controls_row`.
fn render_track_picker_panel_from_parts(
    tracks: &[Track],
    filter: &str,
    highlight: Option<usize>,
    kind: TrackKind,
    root: WeakEntity<Root>,
) -> impl IntoElement {
    let total = tracks.iter().filter(|t| t.kind == kind).count();
    let show_filter = total > PICKER_FILTER_THRESHOLD;
    let filtered = filtered_tracks(tracks, kind, filter);

    let rows = filtered
        .iter()
        .enumerate()
        .map(|(ix, t)| {
            let label = t
                .title
                .clone()
                .unwrap_or_else(|| format!("Track {}", t.mpv_id));
            let lang = t.lang.clone().unwrap_or_default();
            let codec = t.codec.clone().unwrap_or_default();
            let selected = t.selected;
            let highlighted = highlight == Some(ix);
            let pick_root = root.clone();
            let mpv_id = t.mpv_id;
            popover_row(
                SharedString::from(format!("picker-track-{}-{}", kind_tag(kind), mpv_id)),
                selected || highlighted,
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .w_full()
                    .child(div().w(px(14.)).flex_shrink_0().children(selected.then(|| {
                        // "This track is selected" is a control state, not
                        // an indicator, so the checkmark is `TEXT_PRIMARY`
                        // like the rest of the row's text.
                        svg()
                            .path("icons/check.svg")
                            .w(px(14.))
                            .h(px(14.))
                            .text_color(rgba(theme::TEXT_PRIMARY))
                    })))
                    .child(
                        // Long track titles truncate
                        // with a real ellipsis instead of pushing the
                        // lang/codec label off the popover's edge.
                        clamped_line(label, px(20.))
                            .flex_1()
                            .min_w_0()
                            .text_color(rgba(theme::TEXT_PRIMARY)),
                    )
                    .child(
                        // C.6.3's exact mapping: row labels `0x9a9aa2` ->
                        // `text.tertiary`.
                        div()
                            .flex_shrink_0()
                            .text_color(rgba(theme::TEXT_TERTIARY))
                            .text_xs()
                            .child(format!("{lang} {codec}")),
                    ),
            )
            .on_click(move |_e, _w, cx| {
                let _ = pick_root.update(cx, |root, cx| root.select_track(kind, Some(mpv_id), cx));
            })
        })
        .collect::<Vec<_>>();

    // §10: search-filter box, rendered once unfiltered track count crosses
    // `PICKER_FILTER_THRESHOLD`; routed like `search.rs`, no dedicated
    // `TextInput` entity (`root_playback.rs::handle_picker_keystroke`).
    let filter_box = show_filter.then(|| {
        let (text, color) = if filter.is_empty() {
            (
                SharedString::from("Filter tracks…"),
                rgba(theme::TEXT_QUATERNARY),
            )
        } else {
            (
                SharedString::from(filter.to_string()),
                rgba(theme::TEXT_PRIMARY),
            )
        };
        div()
            .mx_1()
            .mb_1()
            .px_2()
            .py_1()
            .rounded_md()
            .bg(rgb(theme::SURFACE_BASE))
            .border_1()
            .border_color(rgb(theme::SURFACE_HAIRLINE))
            .text_sm()
            .text_color(color)
            .child(text)
    });

    let empty = rows
        .is_empty()
        .then(|| empty_state("icons/captions.svg", "No tracks match that filter."));

    popover_panel(
        SharedString::from(format!("osd-picker-panel-{}", kind_tag(kind))),
        px(320.),
        div()
            .flex()
            .flex_col()
            .gap_0p5()
            .children(filter_box)
            .children(empty)
            .children(rows),
    )
}

fn kind_tag(kind: TrackKind) -> &'static str {
    match kind {
        TrackKind::Audio => "audio",
        TrackKind::Subtitle => "subtitle",
        TrackKind::Video => "video",
    }
}

/// §7's shadow_e3 at the Miniplayer's rect (`gl_video::miniplayer_rect_px`):
/// a plain `.shadow(theme::shadow_e3())` div with no background -- GPUI's
/// premultiplied `SourceOver` blending still paints the shadow with no
/// visible fill, and this div's interior coincides with the CALayer
/// mask's cutout hole (`gl_video.rs::spawn_geometry_driver`), so only the
/// shadow's outward blur is visible.
fn render_miniplayer_shadow(left: Pixels, top: Pixels, w: Pixels, h: Pixels) -> impl IntoElement {
    div()
        .absolute()
        .left(left)
        .top(top)
        .w(w)
        .h(h)
        .rounded(theme::RADIUS_MINIPLAYER)
        .shadow(theme::shadow_e3())
}

pub(crate) fn render_miniplayer(
    ui: &PlayerUiState,
    paused: bool,
    viewport: Size<Pixels>,
    root: WeakEntity<Root>,
) -> impl IntoElement {
    let corner = match ui.layer_mode {
        LayerMode::Miniplayer(c) => c,
        LayerMode::FullscreenInWindow => Corner::BottomRight,
    };
    let (left, top, w, h) = miniplayer_rect_px(viewport, corner);

    let close_root = root.clone();
    let play_root = root.clone();
    let expand_root = root.clone();
    let down_root = root.clone();
    let move_root = root.clone();
    let hover_root = root.clone();
    let up_root = root;
    let play_icon = if paused {
        "icons/play.svg"
    } else {
        "icons/pause.svg"
    };
    let frac = if ui.duration_secs > 0.0 {
        (ui.position_secs / ui.duration_secs).clamp(0.0, 1.0) as f32
    } else {
        0.0
    };

    // §7: icon button, matching `mini-close`'s shape -- calls the same
    // `Root::restore_from_miniplayer` a plain click on the video already
    // triggers, giving the spec's "controls" inventory an explicit
    // affordance instead of relying only on "click anywhere that isn't a
    // button."
    let expand_button = div()
        .id("mini-expand")
        .w(px(24.))
        .h(px(24.))
        .flex()
        .items_center()
        .justify_center()
        .rounded_md()
        .cursor_pointer()
        .hover(|s| s.bg(rgba(theme::ICON_HOVER_FILL)))
        .child(
            svg()
                .path("icons/maximize.svg")
                .w(px(14.))
                .h(px(14.))
                .text_color(rgba(theme::TEXT_PRIMARY)),
        )
        // Stops down/up events reaching the parent surface's drag/restore
        // handlers -- without this, a click here also ran the parent's
        // click-to-restore logic underneath it.
        .on_mouse_down(MouseButton::Left, |_e, _w, cx| cx.stop_propagation())
        .on_mouse_up(MouseButton::Left, |_e, _w, cx| cx.stop_propagation())
        .on_click(move |_e, _w, cx| {
            let _ = expand_root.update(cx, |root, cx| root.restore_from_miniplayer(cx));
        });

    // REGRESSION FIX: this wrapper must stay `.absolute().inset_0()` -- an
    // unpositioned div participates in the root container's layout flow
    // and, following a `.size_full()` sibling, lands below the viewport,
    // taking every `.absolute()` child (surface, buttons, shadow) offscreen
    // with it while the video (positioned independently via AppKit's
    // CALayer) keeps rendering fine, making the bug invisible in
    // screenshots. A bare container with no listeners/id adds no hitbox of
    // its own -- everything outside the miniplayer rect stays click-through.
    div()
        .absolute()
        .inset_0()
        .child(render_miniplayer_shadow(left, top, w, h))
        .child(
            div()
                .id("miniplayer")
                .absolute()
                .left(left)
                .top(top)
                .w(w)
                .h(h)
                .cursor_pointer()
                // §7: rounded corners + hairline border matching the
                // video's CALayer mask cutout (`gl_video.rs`) -- painted
                // directly over video with no opaque backing, the
                // OSD-over-video exception `theme.rs` names for "the
                // volume/miniplayer dividers"; `theme::tint(theme::PANNA,
                // ..)` still names the hue rather than a bare literal.
                .rounded(theme::RADIUS_MINIPLAYER)
                .border_1()
                .border_color(rgba(theme::tint(theme::PANNA, 0x1a)))
                // Drives `gl_video`'s video-visibility CALayer mask off/on
                // (`gl_video.rs`'s "Miniplayer video visibility"); must
                // cover this exact rect (it does: same left/top/w/h this
                // element is positioned at).
                .on_hover(move |hovering, _window, cx| {
                    // Field-debug breadcrumb: proves in jellybeam.log whether
                    // hover over the surface is detected -- absent lines
                    // while hovering mean the surface isn't hit-tested
                    // (paint order/rect mismatch), which no synthetic test
                    // reproduces.
                    tracing::info!(hovering = *hovering, "miniplayer hover flip");
                    let _ = hover_root.update(cx, |root, _cx| {
                        root.video.set_miniplayer_hovering(*hovering);
                    });
                })
                // Drag-to-corner vs. click-to-restore (docs/UX-SPEC.md §3) -- see
                // `root_playback.rs::end_miniplayer_drag`'s doc comment for how the two are
                // told apart.
                .on_mouse_down(MouseButton::Left, move |event, _w, cx| {
                    let pos = (f32::from(event.position.x), f32::from(event.position.y));
                    let _ = down_root.update(cx, |root, cx| root.start_miniplayer_drag(pos, cx));
                })
                .on_mouse_move(move |event, _w, cx| {
                    let pos = (f32::from(event.position.x), f32::from(event.position.y));
                    let _ =
                        move_root.update(cx, |root, cx| root.note_miniplayer_drag_move(pos, cx));
                })
                .on_mouse_up(MouseButton::Left, move |event, _window, cx| {
                    let f = (
                        (event.position.x / viewport.width).clamp(0.0, 1.0),
                        (event.position.y / viewport.height).clamp(0.0, 1.0),
                    );
                    let _ = up_root.update(cx, |root, cx| root.end_miniplayer_drag(f, cx));
                })
                .child(
                    div()
                        .size_full()
                        .relative()
                        .overflow_hidden()
                        .rounded(theme::RADIUS_MINIPLAYER)
                        .flex()
                        .flex_col()
                        .justify_between()
                        .opacity(0.0)
                        // §7: controls fade in over a bottom gradient scrim
                        // on hover, matching the main OSD's own
                        // gradient-scrim pattern rather than a flat
                        // translucent fill.
                        .hover(|s| s.opacity(1.0))
                        .child(div().absolute().inset_0().bg(linear_gradient(
                            180.,
                            linear_color_stop(rgba(theme::TRANSPARENT), 0.0),
                            linear_color_stop(rgba(theme::tint(theme::NOTTE, 0xe6)), 1.0),
                        )))
                        .child(
                            div()
                                .relative()
                                .flex_1()
                                .flex()
                                .flex_col()
                                .justify_between()
                                .p_2()
                                .child(
                                    div().flex().justify_between().child(expand_button).child(
                                        div()
                                            .id("mini-close")
                                            .w(px(24.))
                                            .h(px(24.))
                                            .flex()
                                            .items_center()
                                            .justify_center()
                                            .rounded_md()
                                            .cursor_pointer()
                                            // Unified with the main OSD's
                                            // `icon_button` hover fill
                                            // (`theme::ICON_HOVER_FILL`) --
                                            // same control, same feedback
                                            // weight.
                                            .hover(|s| s.bg(rgba(theme::ICON_HOVER_FILL)))
                                            .child(
                                                svg()
                                                    .path("icons/x.svg")
                                                    .w(px(16.))
                                                    .h(px(16.))
                                                    .text_color(rgba(theme::TEXT_PRIMARY)),
                                            )
                                            // See `expand_button`'s comment above.
                                            .on_mouse_down(MouseButton::Left, |_e, _w, cx| {
                                                cx.stop_propagation()
                                            })
                                            .on_mouse_up(MouseButton::Left, |_e, _w, cx| {
                                                cx.stop_propagation()
                                            })
                                            .on_click(move |_e, _w, cx| {
                                                let _ = close_root
                                                    .update(cx, |root, cx| root.stop_playback(cx));
                                            }),
                                    ),
                                )
                                .child(
                                    div().flex().items_center().justify_center().child(
                                        div()
                                            .id("mini-playpause")
                                            .w(px(32.))
                                            .h(px(32.))
                                            .flex()
                                            .items_center()
                                            .justify_center()
                                            .rounded_md()
                                            .cursor_pointer()
                                            .hover(|s| s.bg(rgba(theme::ICON_HOVER_FILL)))
                                            .child(
                                                svg()
                                                    .path(play_icon)
                                                    .w(px(20.))
                                                    .h(px(20.))
                                                    .text_color(rgba(theme::TEXT_PRIMARY)),
                                            )
                                            // See `expand_button`'s comment above -- this is
                                            // the button most directly hit by the reported
                                            // "can't be paused" symptom.
                                            .on_mouse_down(MouseButton::Left, |_e, _w, cx| {
                                                cx.stop_propagation()
                                            })
                                            .on_mouse_up(MouseButton::Left, |_e, _w, cx| {
                                                cx.stop_propagation()
                                            })
                                            .on_click(move |_e, _w, cx| {
                                                let _ = play_root.update(cx, |root, cx| {
                                                    root.toggle_play_pause_click(cx)
                                                });
                                            }),
                                    ),
                                )
                                .child(
                                    div()
                                        .h(px(4.))
                                        .rounded_full()
                                        .bg(rgba(theme::tint(theme::PANNA, 0x44)))
                                        .child(
                                            div()
                                                .h_full()
                                                .rounded_full()
                                                // §7 token pass: other call sites flagged for
                                                // follow-up (skip pill hover, volume slider,
                                                // title hover, undo toast, track-picker
                                                // checkmark) moved to neutral tokens in the §C
                                                // debt pass; this one keeps `theme::ACCENT`
                                                // since it's a trickplay-preview scrub
                                                // indicator, not a plain control.
                                                .bg(rgb(theme::ACCENT))
                                                .w(gpui::relative(frac)),
                                        ),
                                ),
                        ),
                ),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skip_button_numeral_shows_the_configured_length() {
        for secs in crate::settings::SKIP_LENGTH_PRESETS {
            assert_eq!(skip_button_numeral(secs).as_ref(), secs.to_string());
        }
        assert_eq!(skip_button_numeral(30).as_ref(), "30");
    }

    // ---- The spec-strip playback-mode cell ------------------------------

    /// Real `playback::run` strings render as strip cells: leading segment,
    /// uppercased, reasons dropped (own row in the info popover).
    #[test]
    fn playback_mode_label_uppercases_the_decision_summary() {
        assert_eq!(playback_mode_label("Direct Play", true), "DIRECT PLAY");
        assert_eq!(
            playback_mode_label("Transcode (container not supported)", false),
            "TRANSCODE"
        );
    }

    /// No summary yet (a session whose `PlaybackStarted` hasn't landed, or
    /// an older recorded state): fall back to the boolean rather than
    /// rendering an empty cell.
    #[test]
    fn playback_mode_label_falls_back_to_the_direct_play_flag() {
        assert_eq!(playback_mode_label("", true), "DIRECT PLAY");
        assert_eq!(playback_mode_label("", false), "TRANSCODE");
    }

    /// Scrub track spans the OSD bar's full padded width, so the
    /// click-to-fraction mapping's ends land on the bar's margins and its
    /// midpoint on the viewport's.
    #[test]
    fn bar_frac_spans_the_full_padded_width() {
        let w = px(1000.);
        assert_eq!(bar_frac(px(SCRUB_MARGIN), w), 0.0);
        assert_eq!(bar_frac(px(1000. - SCRUB_MARGIN), w), 1.0);
        assert!((bar_frac(px(500.), w) - 0.5).abs() < 1e-6);
    }

    // ---- Miniplayer drag-vs-click distance threshold --------------------

    #[test]
    fn miniplayer_drag_threshold_not_exceeded_by_a_sub_pixel_move() {
        // A real click's stray sub-pixel move event between down and up
        // must NOT be classified as a drag.
        assert!(!miniplayer_drag_exceeds_threshold(
            (100.0, 100.0),
            (100.4, 99.7)
        ));
        assert!(!miniplayer_drag_exceeds_threshold(
            (100.0, 100.0),
            (100.0, 100.0)
        ));
    }

    #[test]
    fn miniplayer_drag_threshold_exceeded_by_a_real_drag() {
        assert!(miniplayer_drag_exceeds_threshold(
            (100.0, 100.0),
            (110.0, 100.0)
        ));
        assert!(miniplayer_drag_exceeds_threshold(
            (100.0, 100.0),
            (100.0, -10.0)
        ));
        // Diagonal displacement whose magnitude clears the threshold even
        // though neither axis alone would.
        assert!(miniplayer_drag_exceeds_threshold((0.0, 0.0), (4.0, 4.0)));
    }

    #[test]
    fn miniplayer_drag_threshold_is_exact_at_the_boundary() {
        // Exactly `MINIPLAYER_DRAG_THRESHOLD_PX` away counts as a drag
        // (`>=`, not `>`).
        assert!(miniplayer_drag_exceeds_threshold(
            (0.0, 0.0),
            (MINIPLAYER_DRAG_THRESHOLD_PX, 0.0)
        ));
    }

    #[test]
    fn format_time_examples() {
        assert_eq!(format_time(0.0), "0:00");
        assert_eq!(format_time(65.0), "1:05");
        assert_eq!(format_time(3661.0), "1:01:01");
        assert_eq!(format_time(-5.0), "0:00");
    }

    // ---- subtitle baseline shift (`sub-pos`) math -----

    #[test]
    fn subtitle_pos_leaves_mpv_default_alone_when_controls_hidden() {
        assert_eq!(
            subtitle_pos_for_osd_state(None, false),
            SUBTITLE_POS_DEFAULT
        );
    }

    #[test]
    fn subtitle_pos_shifts_up_by_the_control_zone_constant_when_visible() {
        assert_eq!(
            subtitle_pos_for_osd_state(None, true),
            SUBTITLE_POS_DEFAULT - SUBTITLE_POS_CONTROLS_SHIFT
        );
    }

    #[test]
    fn subtitle_pos_respects_a_user_override_base() {
        assert_eq!(subtitle_pos_for_osd_state(Some(70), false), 70);
        assert_eq!(
            subtitle_pos_for_osd_state(Some(70), true),
            70 - SUBTITLE_POS_CONTROLS_SHIFT
        );
    }

    #[test]
    fn subtitle_pos_clamps_to_mpvs_documented_range() {
        // A low user override plus the controls-visible shift must not go
        // negative -- mpv's `sub-pos` is documented 0-150.
        assert_eq!(subtitle_pos_for_osd_state(Some(5), true), 0);
        assert_eq!(subtitle_pos_for_osd_state(Some(200), false), 150);
    }

    // ---- Subtitles must never render in the top half --------------------

    /// The regression: the old Subtitles pane stored raw `sub-pos` values
    /// down to 20 ("High"), putting subtitles a fifth down from the top
    /// edge (6 with the OSD shift). Whatever is in `settings.json`, the
    /// pushed value stays in the lower half.
    #[test]
    fn effective_subtitle_pos_never_reaches_the_top_half() {
        for base in [None, Some(0), Some(5), Some(20), Some(59), Some(-40)] {
            for visible in [false, true] {
                let pos = effective_subtitle_pos(base, visible);
                assert!(
                    pos >= crate::settings::SUBTITLE_POS_FLOOR,
                    "base {base:?} / controls {visible} produced sub-pos {pos}, \
                     which is in the top half of the frame"
                );
                assert!(pos <= crate::settings::SUBTITLE_POS_CEIL);
            }
        }
    }

    /// The floor absorbs the OSD shift only where the shift would breach it;
    /// everywhere else the baseline lift is untouched.
    #[test]
    fn effective_subtitle_pos_still_lifts_clear_of_the_control_zone() {
        assert_eq!(
            effective_subtitle_pos(None, true),
            SUBTITLE_POS_DEFAULT - SUBTITLE_POS_CONTROLS_SHIFT
        );
        assert_eq!(effective_subtitle_pos(None, false), SUBTITLE_POS_DEFAULT);
        assert_eq!(
            effective_subtitle_pos(Some(88), true),
            88 - SUBTITLE_POS_CONTROLS_SHIFT
        );
        // The lowest step the pane offers is already clear of the controls,
        // so the shift is absorbed by the floor rather than applied.
        assert_eq!(
            effective_subtitle_pos(Some(crate::settings::SUBTITLE_POS_FLOOR), true),
            crate::settings::SUBTITLE_POS_FLOOR
        );
    }

    /// Every preset the Subtitles pane offers survives the OSD shift intact
    /// or floored -- none can be pushed into the top half.
    #[test]
    fn every_settings_preset_stays_in_the_lower_half_under_the_osd_shift() {
        for (label, pos) in crate::settings::POS_PRESETS {
            for visible in [false, true] {
                let effective = effective_subtitle_pos(pos, visible);
                assert!(
                    effective >= crate::settings::SUBTITLE_POS_FLOOR,
                    "preset {label} ({pos:?}) with controls {visible} \
                     produced sub-pos {effective}"
                );
            }
        }
    }

    // ---- Info popover height cap -----------------------------------------

    /// The panel never runs into the OSD's bottom band (the reported
    /// collision with the elapsed-time label) and never exceeds 70% of the
    /// window.
    #[test]
    fn info_overlay_max_h_clears_the_osd_and_caps_at_seventy_percent() {
        let tall = Size {
            width: px(1600.),
            height: px(1200.),
        };
        let h = f32::from(info_overlay_max_h(tall, true));
        assert!(h <= 1200.0 * 0.7 + 0.01, "{h} exceeded the 70% cap");

        // A short window is bounded by the OSD clearance, not the 70% cap:
        // top anchor + panel + bottom gradient must fit the viewport.
        let short = Size {
            width: px(900.),
            height: px(600.),
        };
        let h = f32::from(info_overlay_max_h(short, true));
        assert!(
            16.0 + h + BOTTOM_GRADIENT_HEIGHT <= 600.0,
            "{h} would overlap the OSD control band"
        );
    }

    /// Degenerate window: the cap floors rather than going to zero or
    /// negative (a negative `max_h` is not a meaningful GPUI constraint).
    #[test]
    fn info_overlay_max_h_floors_on_a_tiny_window() {
        let tiny = Size {
            width: px(400.),
            height: px(200.),
        };
        assert_eq!(f32::from(info_overlay_max_h(tiny, true)), 160.0);
    }

    // ---- Info overlay's Buffer/Network/Dropped-frames rows ----------------

    #[test]
    fn fmt_buffer_shows_duration_and_bytes_when_both_known() {
        let cache = player::CacheState {
            demuxer_cache_duration_secs: Some(12.3),
            paused_for_cache: false,
            fw_bytes: Some(5_242_880), // 5 MiB
            cache_speed_bps: None,
        };
        assert_eq!(fmt_buffer(&cache), "12.3s ahead (5.0 MB)");
    }

    #[test]
    fn fmt_buffer_falls_back_to_placeholders_when_unknown() {
        let cache = player::CacheState::default();
        assert_eq!(fmt_buffer(&cache), "? ahead (? MB)");
    }

    #[test]
    fn fmt_buffer_notes_when_stalled_on_cache() {
        let cache = player::CacheState {
            demuxer_cache_duration_secs: Some(0.2),
            paused_for_cache: true,
            fw_bytes: Some(1024),
            cache_speed_bps: None,
        };
        assert_eq!(fmt_buffer(&cache), "0.2s ahead (0.0 MB) -- buffering");
    }

    #[test]
    fn fmt_network_speed_converts_bytes_to_megabits() {
        // 1_000_000 bytes/s * 8 / 1_000_000 = 8.00 Mbit/s.
        assert_eq!(fmt_network_speed(Some(1_000_000)), "8.00 Mbit/s current");
        assert_eq!(fmt_network_speed(Some(0)), "n/a");
        assert_eq!(fmt_network_speed(None), "n/a");
    }

    #[test]
    fn fmt_frame_drops_shows_both_sides_and_defaults_missing_to_zero() {
        assert_eq!(
            fmt_frame_drops(&player::FrameDropStats {
                vo: Some(3),
                decoder: Some(1),
            }),
            "3 (decoder 1)"
        );
        assert_eq!(
            fmt_frame_drops(&player::FrameDropStats {
                vo: Some(5),
                decoder: None,
            }),
            "5 (decoder 0)"
        );
        assert_eq!(
            fmt_frame_drops(&player::FrameDropStats {
                vo: None,
                decoder: None,
            }),
            "n/a"
        );
    }

    #[test]
    fn active_segment_matches_position_window() {
        let mut state = blank_state();
        state.segments.push(MediaSegmentDto {
            start_ticks: Some(0),
            end_ticks: Some(50_000_000), // 5s
            ..Default::default()
        });
        state.position_secs = 2.0;
        assert!(state.active_segment().is_some());
        state.position_secs = 10.0;
        assert!(state.active_segment().is_none());
    }

    // ---- Per-type skip decision machine ---------------------------------

    #[test]
    fn segment_decision_maps_every_action() {
        assert_eq!(segment_decision(SegmentAction::Ask), SegmentDecision::Pill);
        assert_eq!(
            segment_decision(SegmentAction::AutoSkip),
            SegmentDecision::AutoSkip
        );
        assert_eq!(
            segment_decision(SegmentAction::Off),
            SegmentDecision::Nothing
        );
    }

    /// Full config x segment-type decision matrix, driven through
    /// `active_segment_decision` (not just the pure `segment_decision`
    /// mapping above) so the per-type lookup and position-window match are
    /// exercised together, matching how `root.rs` actually consumes this.
    #[test]
    fn active_segment_decision_matrix() {
        let mut state = blank_state();
        state.skip_segment_prefs = SkipSegmentPrefs {
            intro: SegmentAction::Ask,
            outro: SegmentAction::AutoSkip,
            recap: SegmentAction::Off,
            preview: SegmentAction::Ask,
            commercial: SegmentAction::AutoSkip,
        };
        state.position_secs = 2.0;

        let case = |ty: MediaSegmentType| MediaSegmentDto {
            start_ticks: Some(0),
            end_ticks: Some(50_000_000),
            type_: Some(ty),
            ..Default::default()
        };

        state.segments = vec![case(MediaSegmentType::Intro)];
        assert_eq!(
            state.active_segment_decision().map(|(_, d)| d),
            Some(SegmentDecision::Pill)
        );

        state.segments = vec![case(MediaSegmentType::Outro)];
        assert_eq!(
            state.active_segment_decision().map(|(_, d)| d),
            Some(SegmentDecision::AutoSkip)
        );

        state.segments = vec![case(MediaSegmentType::Recap)];
        assert_eq!(
            state.active_segment_decision().map(|(_, d)| d),
            Some(SegmentDecision::Nothing)
        );

        state.segments = vec![case(MediaSegmentType::Commercial)];
        assert_eq!(
            state.active_segment_decision().map(|(_, d)| d),
            Some(SegmentDecision::AutoSkip)
        );

        // No segment covering the current position at all.
        state.position_secs = 100.0;
        assert!(state.active_segment_decision().is_none());
    }

    #[test]
    fn skip_pill_and_toast_labels_are_per_type() {
        assert_eq!(skip_pill_label(Some(MediaSegmentType::Intro)), "Skip Intro");
        assert_eq!(
            skip_pill_label(Some(MediaSegmentType::Outro)),
            "Skip Credits"
        );
        assert_eq!(
            skip_pill_label(Some(MediaSegmentType::Commercial)),
            "Skip Commercial"
        );
        assert_eq!(skip_pill_label(None), "Skip");

        assert_eq!(
            skip_toast_label(Some(MediaSegmentType::Intro)),
            "Skipped intro"
        );
        assert_eq!(
            skip_toast_label(Some(MediaSegmentType::Commercial)),
            "Skipped commercial"
        );
        assert_eq!(skip_toast_label(None), "Skipped segment");
    }

    /// The 5s undo window: still undoable just before it elapses, gone
    /// after.
    #[test]
    fn skip_toast_expires_after_undo_window() {
        let mut state = blank_state();
        state.show_skip_toast("Skipped intro".to_string(), 12.5);
        assert!(state.skip_toast.is_some());
        assert!(!state.expire_skip_toast());
        assert!(state.skip_toast.is_some());

        if let Some(toast) = state.skip_toast.as_mut() {
            toast.shown_at = Instant::now() - SKIP_UNDO_WINDOW - Duration::from_millis(10);
        }
        assert!(state.expire_skip_toast());
        assert!(state.skip_toast.is_none());
    }

    #[test]
    fn show_skip_toast_bumps_seq_for_a_fresh_animation() {
        let mut state = blank_state();
        state.show_skip_toast("Skipped intro".to_string(), 0.0);
        assert_eq!(state.skip_toast_seq, 1);
        state.show_skip_toast("Skipped credits".to_string(), 90.0);
        assert_eq!(state.skip_toast_seq, 2);
        assert_eq!(
            state.skip_toast.as_ref().map(|t| t.resume_position_secs),
            Some(90.0)
        );
    }

    // ---- Next-episode countdown state machine ---------------------------

    #[test]
    fn next_episode_countdown_total_is_the_min_of_remaining_and_delay() {
        // Plenty of runway left -- the configured delay wins.
        assert_eq!(next_episode_countdown_total(120.0, 10.0), 10.0);
        // Less than a full delay's worth of episode left -- remaining wins,
        // reproducing the earlier "advance exactly at EOF" behavior.
        assert_eq!(next_episode_countdown_total(4.0, 10.0), 4.0);
        // Negative inputs (shouldn't happen, but clamp defensively).
        assert_eq!(next_episode_countdown_total(-1.0, 10.0), 0.0);
    }

    #[test]
    fn next_episode_countdown_remaining_counts_down_to_zero() {
        assert_eq!(next_episode_countdown_remaining(10.0, 0.0), 10.0);
        assert_eq!(next_episode_countdown_remaining(10.0, 6.0), 4.0);
        // Never goes negative once elapsed exceeds total.
        assert_eq!(next_episode_countdown_remaining(10.0, 15.0), 0.0);
    }

    #[test]
    fn tick_auto_hide_never_hides_while_paused() {
        let mut state = blank_state();
        state.last_activity = Instant::now() - OSD_IDLE_TIMEOUT - Duration::from_millis(100);
        // Unpaused: idle timeout elapsed, so it hides.
        assert!(state.tick_auto_hide(false));
        assert!(!state.osd_visible);

        // Reset and try again, this time paused -- must never hide.
        state.osd_visible = true;
        state.last_activity = Instant::now() - OSD_IDLE_TIMEOUT - Duration::from_millis(100);
        assert!(!state.tick_auto_hide(true));
        assert!(state.osd_visible);
    }

    #[test]
    fn tick_auto_hide_exempts_volume_expanded() {
        let mut state = blank_state();
        state.volume_expanded = true;
        state.last_activity = Instant::now() - OSD_IDLE_TIMEOUT - Duration::from_millis(100);
        assert!(!state.tick_auto_hide(false));
        assert!(state.osd_visible);
    }

    #[test]
    fn tick_volume_collapse_waits_for_deadline() {
        let mut state = blank_state();
        state.volume_expanded = true;
        state.volume_hover_deadline = Some(Instant::now() + Duration::from_millis(50));
        assert!(!state.tick_volume_collapse());
        assert!(state.volume_expanded);

        state.volume_hover_deadline = Some(Instant::now() - Duration::from_millis(1));
        assert!(state.tick_volume_collapse());
        assert!(!state.volume_expanded);
    }

    #[test]
    fn breadcrumb_title_degrades_gracefully() {
        let mut state = blank_state();
        state.title = "One Minute".to_string();
        assert_eq!(state.breadcrumb_title(), "One Minute");

        state.series_name = Some("Breaking Bad".to_string());
        assert_eq!(state.breadcrumb_title(), "Breaking Bad · One Minute");

        state.season_number = Some(3);
        state.episode_number = Some(7);
        assert_eq!(
            state.breadcrumb_title(),
            "Breaking Bad · S3 E7 · One Minute"
        );
    }

    /// docs/PLUGIN-CHANNELS.md §2.3/§4: a TVHeadend
    /// recording can arrive as `Type: "Episode"` with `SeriesId`/`SeasonId`
    /// both null; `root.rs`'s `EpisodeContext` derivation leaves series/
    /// season/episode all `None`, same as any entry point with no series
    /// context -- pinned here explicitly rather than only incidentally
    /// covered.
    #[test]
    fn breadcrumb_title_handles_a_recording_with_no_series_linkage() {
        let mut state = blank_state();
        state.title = "News at Six -- 2026-09-09 18:00".to_string();
        state.series_name = None;
        state.season_number = None;
        state.episode_number = None;
        assert_eq!(state.breadcrumb_title(), "News at Six -- 2026-09-09 18:00");
    }

    /// §2.4: the breadcrumb is only a "go to series" link when there's
    /// actually a series to go to.
    #[test]
    fn breadcrumb_clickable_requires_series_id() {
        let mut state = blank_state();
        assert!(!state.breadcrumb_clickable());
        state.series_id = Some("series-1".to_string());
        assert!(state.breadcrumb_clickable());
    }

    /// §2.4: show-threshold clamps to [3, 30]s and otherwise scales with
    /// episode length (see `next_episode_show_threshold`).
    #[test]
    fn next_episode_show_threshold_clamps_to_expected_range() {
        // A ~10s corpus test episode: 15% would be 1.5s, floored to 3.0s.
        assert_eq!(next_episode_show_threshold(10.0), 3.0);
        // A 45-minute (2700s) episode: 15% = 405s, ceilinged to 30.0s.
        assert_eq!(next_episode_show_threshold(2700.0), 30.0);
        // A mid-length episode within the unclamped range.
        assert_eq!(next_episode_show_threshold(120.0), 18.0);
    }

    /// Pins: no outro segment falls back to the fixed default, auto-skip or not.
    #[test]
    fn trigger_remaining_falls_back_to_fixed_default_with_no_outro_segment() {
        assert_eq!(
            next_episode_trigger_remaining_secs(120.0, None, false, 10.0),
            next_episode_show_threshold(120.0)
        );
        assert_eq!(
            next_episode_trigger_remaining_secs(120.0, None, true, 10.0),
            next_episode_show_threshold(120.0)
        );
    }

    /// Pins: a known outro start drives the trigger directly (45-minute episode, credits at 42:00).
    #[test]
    fn trigger_remaining_uses_outro_start_when_known() {
        assert_eq!(
            next_episode_trigger_remaining_secs(2700.0, Some(2520.0), false, 10.0),
            180.0
        );
    }

    /// Pins: past the outro's start, `remaining <= trigger` already holds -- no special case needed.
    #[test]
    fn trigger_remaining_shows_immediately_once_outro_start_has_passed() {
        let duration = 1200.0;
        let outro_start = 1100.0;
        let trigger = next_episode_trigger_remaining_secs(duration, Some(outro_start), false, 10.0);
        assert_eq!(trigger, 100.0);
        let remaining = duration - (outro_start + 30.0);
        assert!(remaining <= trigger, "expected an immediate show");
    }

    /// Pins: auto-skip brings the card forward by the countdown (credits + delay); a non-positive delay adds nothing.
    #[test]
    fn trigger_remaining_adds_the_countdown_when_outro_is_auto_skip() {
        assert_eq!(
            next_episode_trigger_remaining_secs(2700.0, Some(2520.0), true, 10.0),
            190.0
        );
        assert_eq!(
            next_episode_trigger_remaining_secs(2700.0, Some(2520.0), true, -3.0),
            180.0
        );
    }

    /// Pins: a negative or at/after-duration outro start is treated as no segment.
    #[test]
    fn trigger_remaining_falls_back_on_nonsensical_outro_start() {
        assert_eq!(
            next_episode_trigger_remaining_secs(120.0, Some(-5.0), false, 10.0),
            next_episode_show_threshold(120.0)
        );
        assert_eq!(
            next_episode_trigger_remaining_secs(120.0, Some(120.0), true, 10.0),
            next_episode_show_threshold(120.0)
        );
        assert_eq!(
            next_episode_trigger_remaining_secs(120.0, Some(500.0), false, 10.0),
            next_episode_show_threshold(120.0)
        );
    }

    /// Pins: the countdown is sized to the outro under auto-skip, else to EOF, never negative.
    #[test]
    fn playable_secs_runs_to_the_outro_under_auto_skip_else_to_eof() {
        assert_eq!(
            next_episode_playable_secs(190.0, 2510.0, Some(2520.0), true),
            10.0
        );
        assert_eq!(
            next_episode_playable_secs(190.0, 2510.0, Some(2520.0), false),
            190.0
        );
        assert_eq!(next_episode_playable_secs(190.0, 2510.0, None, true), 190.0);
        assert_eq!(
            next_episode_playable_secs(50.0, 2530.0, Some(2520.0), true),
            0.0
        );
    }

    /// Pins: the ceiling numeral and its `8S` / `m:ss` format.
    #[test]
    fn countdown_numeral_counts_down_to_zero() {
        assert_eq!(next_episode_remaining_whole_secs(10.0, 0.0), 10);
        assert_eq!(next_episode_remaining_whole_secs(10.0, 2.1), 8);
        assert_eq!(next_episode_remaining_whole_secs(10.0, 11.0), 0);
        assert_eq!(next_episode_numeral(8), "8S");
        assert_eq!(next_episode_numeral(0), "0S");
        assert_eq!(next_episode_numeral(90), "1:30");
        assert_eq!(next_episode_numeral(600), "10:00");
    }

    /// Pins: the runtime line rounds to whole minutes and is absent without a runtime.
    #[test]
    fn runtime_label_rounds_to_minutes() {
        assert_eq!(
            next_episode_runtime_label(Some(22 * 600_000_000 + 20 * 10_000_000)).as_deref(),
            Some("22 MIN")
        );
        assert_eq!(
            next_episode_runtime_label(Some(5_000_000)).as_deref(),
            Some("1 MIN")
        );
        assert_eq!(next_episode_runtime_label(None), None);
        assert_eq!(next_episode_runtime_label(Some(0)), None);
    }

    /// `outro_segment_start_secs` finds the Outro segment regardless of
    /// playhead position (unlike `active_segment`) and converts ticks to
    /// seconds.
    #[test]
    fn outro_segment_start_secs_finds_the_outro_regardless_of_position() {
        let mut state = blank_state();
        state.position_secs = 0.0;
        state.segments = vec![
            MediaSegmentDto {
                type_: Some(MediaSegmentType::Intro),
                start_ticks: Some(0),
                end_ticks: Some(300_000_000),
                ..Default::default()
            },
            MediaSegmentDto {
                type_: Some(MediaSegmentType::Outro),
                start_ticks: Some(25_200_000_000),
                end_ticks: Some(27_000_000_000),
                ..Default::default()
            },
        ];
        assert_eq!(state.outro_segment_start_secs(), Some(2520.0));
    }

    #[test]
    fn outro_segment_start_secs_is_none_without_an_outro_segment() {
        let mut state = blank_state();
        state.segments = vec![MediaSegmentDto {
            type_: Some(MediaSegmentType::Intro),
            start_ticks: Some(0),
            end_ticks: Some(300_000_000),
            ..Default::default()
        }];
        assert_eq!(state.outro_segment_start_secs(), None);
    }

    /// `PlayerUiState::outro_auto_skip` reads through to
    /// `SkipSegmentPrefs::action_for(Outro)`.
    #[test]
    fn outro_auto_skip_reflects_configured_prefs() {
        let mut state = blank_state();
        state.skip_segment_prefs = SkipSegmentPrefs {
            outro: SegmentAction::AutoSkip,
            ..SkipSegmentPrefs::default()
        };
        assert!(state.outro_auto_skip());

        state.skip_segment_prefs = SkipSegmentPrefs {
            outro: SegmentAction::Ask,
            ..SkipSegmentPrefs::default()
        };
        assert!(!state.outro_auto_skip());
    }

    #[test]
    fn chapter_at_finds_the_covering_chapter() {
        let mut state = blank_state();
        state.chapters = vec![
            (0.0, "Cold Open".to_string()),
            (120.0, "Main Titles".to_string()),
            (180.0, "Act One".to_string()),
        ];
        assert_eq!(state.chapter_at(0.0), Some("Cold Open"));
        assert_eq!(state.chapter_at(150.0), Some("Main Titles"));
        assert_eq!(state.chapter_at(200.0), Some("Act One"));
    }

    #[test]
    fn trigger_flash_bumps_seq_and_records_kind() {
        let mut state = blank_state();
        assert!(state.last_flash.is_none());
        state.trigger_flash(FlashKind::Play);
        assert_eq!(state.flash_seq, 1);
        assert_eq!(state.last_flash.map(|(k, _)| k), Some(FlashKind::Play));
        state.trigger_flash(FlashKind::Pause);
        assert_eq!(state.flash_seq, 2);
        assert_eq!(state.last_flash.map(|(k, _)| k), Some(FlashKind::Pause));
    }

    fn blank_state() -> PlayerUiState {
        PlayerUiState {
            item_id: "i".into(),
            series_id: None,
            media_source_id: "m".into(),
            decision_summary: "Direct Play".into(),
            is_direct_play: true,
            decision_reasons: Vec::new(),
            container: None,
            // An empty `MediaSourceInfo` -- these focus/flash/scrub tests
            // never render the info popover, and the OpenAPI-generated
            // model has no `Default` impl to reach for.
            media_source: serde_json::from_value(serde_json::json!({}))
                .expect("empty media source"),
            max_bitrate: None,
            title: String::new(),
            series_name: None,
            season_number: None,
            episode_number: None,
            season_id: None,
            osd_visible: true,
            last_subtitle_sync: None,
            last_activity: Instant::now(),
            position_secs: 0.0,
            duration_secs: 100.0,
            volume: 100,
            muted: false,
            pre_mute_volume: 100,
            volume_expanded: false,
            volume_hover_deadline: None,
            volume_drag_anchor: None,
            remaining_display: RemainingDisplay::default(),
            tracks: Vec::new(),
            chapters: Vec::new(),
            trickplay: None,
            trickplay_cache: None,
            scrub: ScrubState::default(),
            picker: None,
            picker_highlight: None,
            picker_filter: String::new(),
            toast: None,
            toast_seq: 0,
            info_overlay: false,
            segments: Vec::new(),
            skip_segment_prefs: SkipSegmentPrefs::default(),
            last_auto_skip_segment: None,
            skip_toast: None,
            skip_toast_seq: 0,
            layer_mode: LayerMode::FullscreenInWindow,
            dragging_miniplayer: false,
            drag_moved: false,
            drag_down_pos: None,
            last_flash: None,
            flash_seq: 0,
            skip_back_secs: 10,
            skip_forward_secs: 10,
            speed_boost_rate: None,
            next_episode: None,
            next_episode_dismissed: false,
            autoplay_prefs: AutoplayPrefs::default(),
            next_episode_shown_at: None,
            next_episode_countdown_total_secs: None,
            next_episode_generation: 0,
            next_episode_paused_at: None,
            loading: false,
            loading_started: std::time::Instant::now(),
            buffering_percent: None,
            last_mirror_progress_at: Instant::now() - MIRROR_PROGRESS_INTERVAL,
        }
    }

    // ---- Miniplayer regression: does GPUI 0.2.2 let a stopped-propagation
    // child button's `on_click` fire? --------------------------------------
    //
    // `render_miniplayer`'s hover buttons can't be tested directly -- they
    // close over a `WeakEntity<Root>`, and a real `Root` needs a live
    // AppKit window (see `text_input.rs`'s `entity_lease_regression`). This
    // reproduces the exact interaction pattern (a stop_propagation child
    // button's on_click, nested in a non-stopping parent) against a real
    // GPUI test window via `TestAppContext::simulate_mouse_down/up`.
    //
    // Root cause: sound in GPUI 0.2.2. `Interactivity::paint_mouse_listeners`
    // registers the click-detection listener after the element's own
    // `on_mouse_up` in the same paint call, and `Window::dispatch_mouse_
    // event`'s Bubble phase runs listeners in reverse registration order --
    // so an element's own click-firing listener always runs before its own
    // `stop_propagation` closure; `stop_propagation` only ever blocks
    // ancestor listeners, exactly the effect the buttons want.
    mod miniplayer_click_dispatch_mechanism {
        use super::*;
        use gpui::{point, Modifiers, Render, TestAppContext, VisualTestContext, Window};
        use std::cell::Cell;
        use std::rc::Rc;

        struct DispatchProbe {
            parent_mouse_down: Rc<Cell<u32>>,
            parent_mouse_up: Rc<Cell<u32>>,
            button_mouse_down: Rc<Cell<u32>>,
            button_clicks: Rc<Cell<u32>>,
        }

        impl Render for DispatchProbe {
            fn render(
                &mut self,
                _window: &mut Window,
                _cx: &mut Context<Self>,
            ) -> impl IntoElement {
                let parent_down = self.parent_mouse_down.clone();
                let parent_up = self.parent_mouse_up.clone();
                let button_down = self.button_mouse_down.clone();
                let button_clicks = self.button_clicks.clone();
                div()
                    .id("parent-surface")
                    .size_full()
                    // Mirrors `#miniplayer`'s drag wiring: plain listeners,
                    // no stop_propagation of their own.
                    .on_mouse_down(MouseButton::Left, move |_e, _w, _cx| {
                        parent_down.set(parent_down.get() + 1);
                    })
                    .on_mouse_up(MouseButton::Left, move |_e, _w, _cx| {
                        parent_up.set(parent_up.get() + 1);
                    })
                    .child(
                        div()
                            // Mirrors `mini-playpause`/`mini-close`/
                            // `mini-expand`'s shape: stop_propagation on
                            // both mouse events plus its own on_click.
                            .id("child-button")
                            .absolute()
                            .left(px(10.))
                            .top(px(10.))
                            .w(px(24.))
                            .h(px(24.))
                            .on_mouse_down(MouseButton::Left, move |_e, _w, cx| {
                                button_down.set(button_down.get() + 1);
                                cx.stop_propagation();
                            })
                            .on_mouse_up(MouseButton::Left, |_e, _w, cx| {
                                cx.stop_propagation();
                            })
                            .on_click(move |_e, _w, _cx| {
                                button_clicks.set(button_clicks.get() + 1);
                            }),
                    )
            }
        }

        #[gpui::test]
        fn stop_propagation_on_down_and_up_still_lets_on_click_fire(cx: &mut TestAppContext) {
            let parent_mouse_down = Rc::new(Cell::new(0));
            let parent_mouse_up = Rc::new(Cell::new(0));
            let button_mouse_down = Rc::new(Cell::new(0));
            let button_clicks = Rc::new(Cell::new(0));
            let window = cx.add_window(|_window, _cx| DispatchProbe {
                parent_mouse_down: parent_mouse_down.clone(),
                parent_mouse_up: parent_mouse_up.clone(),
                button_mouse_down: button_mouse_down.clone(),
                button_clicks: button_clicks.clone(),
            });
            let mut cx = VisualTestContext::from_window(window.into(), cx);

            // Plain click inside the 24x24 button at (10,10) -- the same
            // interaction a user makes.
            let inside_button = point(px(20.), px(20.));
            cx.simulate_click(inside_button, Modifiers::none());
            cx.run_until_parked();

            assert_eq!(
                button_clicks.get(),
                1,
                "the child button's own on_click must fire even though both of its mouse \
                 listeners call stop_propagation -- GPUI's click-detection listener for this \
                 same element runs before that stop_propagation closure in the Bubble phase"
            );
            assert_eq!(
                button_mouse_down.get(),
                1,
                "the button's own on_mouse_down must still fire (stop_propagation only blocks \
                 listeners further up the tree, not the element's own remaining listeners)"
            );
            assert_eq!(
                parent_mouse_down.get(),
                0,
                "the parent surface's on_mouse_down (start_miniplayer_drag's analogue) must be \
                 blocked by the button's stop_propagation"
            );
            assert_eq!(
                parent_mouse_up.get(),
                0,
                "the parent surface's on_mouse_up (end_miniplayer_drag's analogue) must be \
                 blocked by the button's stop_propagation"
            );

            // Sanity check: a click outside the button but inside the
            // parent must reach the parent normally, proving the zero
            // counts above are from propagation being stopped, not broken
            // listeners.
            let outside_button = point(px(200.), px(200.));
            cx.simulate_click(outside_button, Modifiers::none());
            cx.run_until_parked();
            assert_eq!(parent_mouse_down.get(), 1);
            assert_eq!(parent_mouse_up.get(), 1);
            assert_eq!(
                button_clicks.get(),
                1,
                "clicking outside the button must not also fire its on_click"
            );
        }
    }

    /// Regression coverage: a stray `.relative()` after `.absolute()`
    /// demotes the painted bar to an in-flow `width: auto` flex item,
    /// collapsing it to zero width (see `scrub_track_bar`'s own comment).
    /// Not observable from `PlayerUiState` state alone, so these tests
    /// assert the declared position and the resolved geometry of the real
    /// `scrub_track_bar` against a live GPUI window.
    mod scrub_track_layout {
        use super::*;
        use gpui::{canvas, Position, Render, TestAppContext, VisualTestContext, Window};
        use std::cell::Cell;
        use std::rc::Rc;

        const TRACK_W: f32 = 1000.;

        /// Caught without a window: the bar must declare `position:
        /// absolute` -- with `left: 0`/`right: 0`, that's what stretches it
        /// to the track's full width.
        #[test]
        fn painted_bar_stays_absolutely_positioned() {
            assert_eq!(
                scrub_track_bar(0.5, 0.5).style().position,
                Some(Position::Absolute),
                "the scrub bar must stay `position: absolute` -- as `position: relative` it \
                 becomes an auto-width in-flow flex item and collapses to zero width, taking \
                 the played fill and the playhead knob with it"
            );
        }

        /// The consequence, measured for real: lays `scrub_track_bar` out
        /// in the same wrapper `scrub_row` uses and reads its resolved
        /// width via a `gpui::canvas` bounds spy (`.absolute()` so it can't
        /// perturb the flex line being measured). Driven through a real
        /// `Render` view since the bar's `group_hover` needs a rendering
        /// view on the stack. The wrapper takes an explicit `TRACK_W` so
        /// the assertion doesn't depend on the test window's size.
        struct BarProbe {
            played_frac: f32,
            bar_w: Rc<Cell<f32>>,
        }

        impl Render for BarProbe {
            fn render(
                &mut self,
                _window: &mut Window,
                _cx: &mut Context<Self>,
            ) -> impl IntoElement {
                let spy = self.bar_w.clone();
                div()
                    .group("scrub-bar")
                    .relative()
                    .w(px(TRACK_W))
                    .h(px(SCRUB_HIT_HEIGHT))
                    .flex()
                    .items_center()
                    .child(
                        scrub_track_bar(self.played_frac, 0.9).child(
                            canvas(
                                move |bounds, _w, _cx| spy.set(f32::from(bounds.size.width)),
                                |_b, _t, _w, _cx| {},
                            )
                            .absolute()
                            .size_full(),
                        ),
                    )
            }
        }

        #[gpui::test]
        fn painted_bar_spans_the_full_track_width(cx: &mut TestAppContext) {
            for played_frac in [0.0, 0.5, 0.853, 1.0] {
                let bar_w = Rc::new(Cell::new(-1.0));
                let window = cx.add_window(|_w, _cx| BarProbe {
                    played_frac,
                    bar_w: bar_w.clone(),
                });
                let cx = VisualTestContext::from_window(window.into(), cx);
                cx.run_until_parked();
                let measured = bar_w.get();
                assert!(
                    (measured - TRACK_W).abs() < 1.0,
                    "at played_frac={played_frac} the painted bar resolved to {measured}px \
                     instead of the track's {TRACK_W}px -- a 0 here is exactly the \
                     zero-width collapse that pinned the playhead knob to the left edge \
                     and erased the amber progress fill"
                );
            }
        }

        /// Belt-and-braces: inside a definite-width parent, a percentage
        /// width and a percentage inset both resolve to `frac * width` and
        /// both travel as the fraction advances (reported symptom: "the
        /// dot never moves").
        #[gpui::test]
        fn percentage_width_and_inset_both_track_the_fraction(cx: &mut TestAppContext) {
            for (frac, expected) in [(0.0, 0.), (0.5, 500.), (0.853, 853.), (1.0, 1000.)] {
                let fill_w = Rc::new(Cell::new(-1.0));
                let knob_x = Rc::new(Cell::new(-1.0));
                let (fw, kx) = (fill_w.clone(), knob_x.clone());
                let window = cx.add_window(|_w, _cx| gpui::Empty);
                let mut cx = VisualTestContext::from_window(*window, cx);
                cx.draw(
                    gpui::point(px(0.), px(0.)),
                    gpui::size(px(TRACK_W), px(600.)),
                    move |_w, _cx| {
                        div()
                            .relative()
                            .w_full()
                            .h(px(SCRUB_TRACK_HEIGHT))
                            .flex()
                            .child(
                                // played fill: percentage WIDTH
                                div().h_full().w(gpui::relative(frac)).child(
                                    canvas(
                                        move |b, _w, _cx| fw.set(f32::from(b.size.width)),
                                        |_b, _t, _w, _cx| {},
                                    )
                                    .absolute()
                                    .size_full(),
                                ),
                            )
                            .child(
                                // knob: percentage INSET
                                div()
                                    .absolute()
                                    .left(gpui::relative(frac))
                                    .w(px(SCRUB_KNOB_SIZE))
                                    .h(px(SCRUB_KNOB_SIZE))
                                    .child(
                                        canvas(
                                            move |b, _w, _cx| kx.set(f32::from(b.origin.x)),
                                            |_b, _t, _w, _cx| {},
                                        )
                                        .absolute()
                                        .size_full(),
                                    ),
                            )
                    },
                );
                cx.run_until_parked();
                assert!(
                    (fill_w.get() - expected).abs() < 1.0,
                    "percentage width at frac={frac}: expected {expected}px, got {}px",
                    fill_w.get()
                );
                assert!(
                    (knob_x.get() - expected).abs() < 1.0,
                    "percentage inset at frac={frac}: expected {expected}px, got {}px",
                    knob_x.get()
                );
            }
        }
    }

    // ---- Language normalization -------------------------------------------

    #[test]
    fn lang_matches_is_case_insensitive() {
        assert!(lang_matches("ENG", "eng"));
        assert!(lang_matches("Eng", "eNG"));
    }

    #[test]
    fn lang_matches_treats_bibliographic_and_terminological_forms_as_equal() {
        // The three pairs the spec calls out.
        assert!(lang_matches("deu", "ger"));
        assert!(lang_matches("ger", "deu"));
        assert!(lang_matches("fra", "fre"));
        assert!(lang_matches("fre", "fra"));
        assert!(lang_matches("zho", "chi"));
        assert!(lang_matches("chi", "zho"));
    }

    #[test]
    fn lang_matches_rejects_genuinely_different_languages() {
        assert!(!lang_matches("eng", "spa"));
        assert!(!lang_matches("jpn", "kor"));
    }

    // ---- Track selection ---------------------------------------------------

    fn track(kind: TrackKind, mpv_id: i64, lang: Option<&str>) -> Track {
        Track {
            mpv_id,
            kind,
            title: None,
            lang: lang.map(str::to_string),
            codec: None,
            default: false,
            selected: false,
            forced: false,
        }
    }

    fn default_flag(mut t: Track) -> Track {
        t.default = true;
        t
    }

    fn selected_flag(mut t: Track) -> Track {
        t.selected = true;
        t
    }

    fn forced_flag(mut t: Track) -> Track {
        t.forced = true;
        t
    }

    fn no_global_prefs() -> crate::settings::LanguagePrefs {
        crate::settings::LanguagePrefs::default()
    }

    #[test]
    fn resolve_track_selection_with_no_prefs_at_all_leaves_everything_alone() {
        let tracks = vec![
            track(TrackKind::Audio, 1, Some("eng")),
            track(TrackKind::Subtitle, 2, Some("eng")),
        ];
        let decision = resolve_track_selection(&tracks, None, &no_global_prefs());
        assert_eq!(decision.audio, None);
        assert_eq!(decision.subtitle, SubtitleDecision::Leave);
    }

    #[test]
    fn resolve_track_selection_matches_preferred_audio_language() {
        let tracks = vec![
            track(TrackKind::Audio, 1, Some("eng")),
            track(TrackKind::Audio, 2, Some("jpn")),
        ];
        let global = crate::settings::LanguagePrefs {
            audio: Some("jpn".to_string()),
            ..no_global_prefs()
        };
        let decision = resolve_track_selection(&tracks, None, &global);
        assert_eq!(decision.audio, Some(2));
    }

    #[test]
    fn resolve_track_selection_leaves_audio_alone_when_preference_has_no_match() {
        let tracks = vec![track(TrackKind::Audio, 1, Some("eng"))];
        let global = crate::settings::LanguagePrefs {
            audio: Some("kor".to_string()),
            ..no_global_prefs()
        };
        let decision = resolve_track_selection(&tracks, None, &global);
        assert_eq!(
            decision.audio, None,
            "no match must leave the default alone"
        );
    }

    #[test]
    fn resolve_track_selection_default_mode_never_touches_subtitles() {
        let tracks = vec![default_flag(track(TrackKind::Subtitle, 1, Some("eng")))];
        let global = crate::settings::LanguagePrefs {
            subtitle: Some("eng".to_string()),
            subtitle_mode: crate::settings::SubtitleMode::Default,
            ..no_global_prefs()
        };
        let decision = resolve_track_selection(&tracks, None, &global);
        assert_eq!(decision.subtitle, SubtitleDecision::Leave);
    }

    #[test]
    fn resolve_track_selection_always_mode_matches_preferred_subtitle_language() {
        let tracks = vec![
            track(TrackKind::Subtitle, 1, Some("eng")),
            track(TrackKind::Subtitle, 2, Some("spa")),
        ];
        let global = crate::settings::LanguagePrefs {
            subtitle: Some("spa".to_string()),
            subtitle_mode: crate::settings::SubtitleMode::Always,
            ..no_global_prefs()
        };
        let decision = resolve_track_selection(&tracks, None, &global);
        assert_eq!(decision.subtitle, SubtitleDecision::Track(2));
    }

    #[test]
    fn resolve_track_selection_always_mode_falls_back_to_default_flagged_track() {
        let tracks = vec![
            track(TrackKind::Subtitle, 1, Some("eng")),
            default_flag(track(TrackKind::Subtitle, 2, Some("spa"))),
        ];
        // No language set at all -- falls to the default-flagged track.
        let global = crate::settings::LanguagePrefs {
            subtitle_mode: crate::settings::SubtitleMode::Always,
            ..no_global_prefs()
        };
        let decision = resolve_track_selection(&tracks, None, &global);
        assert_eq!(decision.subtitle, SubtitleDecision::Track(2));
    }

    #[test]
    fn resolve_track_selection_always_mode_falls_back_to_first_track_absent_default() {
        let tracks = vec![
            track(TrackKind::Subtitle, 5, Some("eng")),
            track(TrackKind::Subtitle, 6, Some("spa")),
        ];
        // Preference set but no track matches, and none is default-flagged
        // -- falls to the first subtitle track.
        let global = crate::settings::LanguagePrefs {
            subtitle: Some("kor".to_string()),
            subtitle_mode: crate::settings::SubtitleMode::Always,
            ..no_global_prefs()
        };
        let decision = resolve_track_selection(&tracks, None, &global);
        assert_eq!(decision.subtitle, SubtitleDecision::Track(5));
    }

    #[test]
    fn resolve_track_selection_only_forced_matches_the_playing_audio_language() {
        let tracks = vec![
            selected_flag(track(TrackKind::Audio, 1, Some("jpn"))),
            forced_flag(track(TrackKind::Subtitle, 2, Some("jpn"))),
            track(TrackKind::Subtitle, 3, Some("eng")),
        ];
        let global = crate::settings::LanguagePrefs {
            subtitle_mode: crate::settings::SubtitleMode::OnlyForced,
            ..no_global_prefs()
        };
        let decision = resolve_track_selection(&tracks, None, &global);
        assert_eq!(decision.subtitle, SubtitleDecision::Track(2));
    }

    #[test]
    fn resolve_track_selection_only_forced_matches_the_newly_selected_audio_preference() {
        // Audio preference switches to "jpn"; the forced sub must match
        // THAT language, not whatever was selected before the switch.
        let tracks = vec![
            selected_flag(track(TrackKind::Audio, 1, Some("eng"))),
            track(TrackKind::Audio, 4, Some("jpn")),
            forced_flag(track(TrackKind::Subtitle, 2, Some("jpn"))),
        ];
        let global = crate::settings::LanguagePrefs {
            audio: Some("jpn".to_string()),
            subtitle_mode: crate::settings::SubtitleMode::OnlyForced,
            ..no_global_prefs()
        };
        let decision = resolve_track_selection(&tracks, None, &global);
        assert_eq!(decision.audio, Some(4));
        assert_eq!(decision.subtitle, SubtitleDecision::Track(2));
    }

    #[test]
    fn resolve_track_selection_only_forced_turns_off_without_a_matching_forced_track() {
        let tracks = vec![
            selected_flag(track(TrackKind::Audio, 1, Some("eng"))),
            // Forced, but wrong language.
            forced_flag(track(TrackKind::Subtitle, 2, Some("jpn"))),
        ];
        let global = crate::settings::LanguagePrefs {
            subtitle_mode: crate::settings::SubtitleMode::OnlyForced,
            ..no_global_prefs()
        };
        let decision = resolve_track_selection(&tracks, None, &global);
        assert_eq!(decision.subtitle, SubtitleDecision::Off);
    }

    #[test]
    fn resolve_track_selection_none_mode_always_turns_subtitles_off() {
        let tracks = vec![default_flag(track(TrackKind::Subtitle, 1, Some("eng")))];
        let global = crate::settings::LanguagePrefs {
            subtitle_mode: crate::settings::SubtitleMode::None,
            ..no_global_prefs()
        };
        let decision = resolve_track_selection(&tracks, None, &global);
        assert_eq!(decision.subtitle, SubtitleDecision::Off);
    }

    #[test]
    fn resolve_track_selection_per_series_pref_overrides_globals() {
        let tracks = vec![
            track(TrackKind::Audio, 1, Some("eng")),
            track(TrackKind::Audio, 2, Some("jpn")),
            track(TrackKind::Subtitle, 3, Some("eng")),
            track(TrackKind::Subtitle, 4, Some("spa")),
        ];
        // Global prefs point at jpn/eng, but the per-series memory says
        // eng/spa -- the series memory must win outright, not blend.
        let series_pref = crate::player_prefs::SeriesTrackPref {
            audio: Some("eng".to_string()),
            subtitle: Some("spa".to_string()),
        };
        let global = crate::settings::LanguagePrefs {
            audio: Some("jpn".to_string()),
            subtitle: Some("eng".to_string()),
            subtitle_mode: crate::settings::SubtitleMode::Always,
        };
        let decision = resolve_track_selection(&tracks, Some(&series_pref), &global);
        assert_eq!(decision.audio, Some(1));
        assert_eq!(decision.subtitle, SubtitleDecision::Track(4));
    }
}
