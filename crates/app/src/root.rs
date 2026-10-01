//! The single root GPUI entity: owns the Connect/Main screen state directly
//! (no nested screen entities) plus the shared runtime/video-layer handles
//! that outlive both screens, the browse view map (Home/Library/Detail +
//! Search overlay, `nav.rs`'s `Nav`), keyboard-first navigation (docs/UX-SPEC.md
//! §2), and the image pipeline (`image_store.rs`). See `ARCHITECTURE.md`
//! for the module map.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

use gpui::{
    div, linear_color_stop, linear_gradient, point, prelude::*, px, rgb, rgba, svg, AnyElement,
    App, Context, Corner, Entity, FocusHandle, IntoElement, KeyDownEvent, ParentElement, Render,
    SharedString, Styled, Window,
};

use jellyfin_api::models::{BaseItemKind, LocationType};
use jellyfin_api::{ClientIdentity, JellyfinClient};
use jellyfin_core::ReportingSession;
use media_cache::{CardRow, Mirror, Sort};

use crate::detail::DetailState;
use crate::discover;
use crate::focus_grid::{self, Direction, GridFocus};
use crate::gl_video::VideoLayer;
use crate::grid::GridScroll;
use crate::home::HomeState;
use crate::image_store::{ImageStore, BACKDROP_WIDTH};
use crate::keychain::StoredSession;
use crate::nav::{Nav, View};
use crate::player_prefs::TrackPrefs;
use crate::player_ui::PlayerUiState;
use crate::search::SearchState;
use crate::session::ConnectedBundle;
use crate::settings::LibraryViewMode;
use crate::text_input::TextInput;
use crate::theme;
use crate::ui::components::{
    button, clamped_line, dense_label, empty_state, empty_state_mascot, list_row, status_dot,
    status_label, toggle_switch, tooltip_text, ButtonSize, ButtonVariant,
};
use crate::ui::popover::{click_away_catcher, popover_panel, popover_row, popover_trigger};

// Tab/Shift+Tab on the Connect screen. Bound to "tab"/"shift-tab" globally
// in `main.rs`; only `render_connect` registers listeners for them (via
// `on_connect_focus_next`/`_prev`), so Tab elsewhere (Main screen,
// fullscreen player, miniplayer) is unaffected.
gpui::actions!(connect, [ConnectFocusNext, ConnectFocusPrev]);

pub(crate) struct Root {
    pub(crate) runtime: Arc<tokio::runtime::Runtime>,
    pub(crate) identity: ClientIdentity,
    pub(crate) video: std::rc::Rc<VideoLayer>,
    /// The About window (`about.rs`), if open -- `None` before first open
    /// and after close (a closed handle fails `WindowHandle::update`).
    pub(crate) about_window: Option<gpui::WindowHandle<crate::about::AboutView>>,
    /// Last position mpv reported (100ns ticks), updated by the
    /// `player.events()` task -- read by `stop_playback` for an accurate
    /// final position on Esc.
    pub(crate) last_position_ticks: Arc<AtomicI64>,
    /// Monotonic id for the next-up card's hand-over timer
    /// (`arm_next_episode_handover`); the armed value lives on the card.
    pub(crate) next_episode_handover_gen: u64,
    pub(crate) screen: Screen,
    /// Set by `perf.rs` under `JELLYBEAM_PERF=1`: timestamps appended on every
    /// `Render::render` for frame-to-frame stats during a driven scroll.
    pub(crate) perf_frame_log: Option<Rc<RefCell<Vec<Instant>>>>,
    /// `None` if `now_playing::NowPlaying::register` failed (never fatal).
    pub(crate) now_playing: Option<crate::now_playing::NowPlaying>,
    /// How many playback sessions this process started via `promote_preload`
    /// vs. the cold `playback::run` path. Read by the E2E harness.
    pub(crate) promoted_preloads: u64,
    /// Monotonic id for off-thread library rebuilds. Root-owned so it
    /// survives MainState swaps.
    pub(crate) library_rebuild_seq: u64,
    /// Every signed-in `(server, user)` pair plus which one is active.
    /// Lives on `Root`, not `MainState`, so it survives a server switch and
    /// an "Add server" trip through `Screen::Connect`. Persisted to the
    /// Keychain on every change.
    pub(crate) sessions: crate::keychain::StoredSessionList,
    /// Subtitle style + per-server bitrate-cap preferences. Root-level (not
    /// `MainState`) so it outlives any one server's `MainState`.
    pub(crate) app_settings: crate::settings::AppSettings,
    /// Bumped by every login-ish attempt that can race another one --
    /// `handle_connect_outcome` discards a stale outcome whose value no
    /// longer matches, so a slow launch-time resume can't clobber a
    /// different login that finished first.
    pub(crate) connect_generation: u64,
    /// Process-start timestamp, set only when `Root::new` found a stored
    /// session to auto-resume at launch ("warm start"). Consumed (logged +
    /// cleared) on this resume's first `Screen::Main` transition, giving an
    /// "exec -> Main painted" timing that should stay under 300ms.
    pub(crate) warm_start_at: Option<Instant>,
    /// Option-key speed-hold. `None` if `OptionSpeedMonitor::register` failed
    /// (never fatal, same contract as `now_playing`).
    pub(crate) option_speed_monitor: Option<crate::option_speed_hold::OptionSpeedMonitor>,
    /// The active session's server version, last confirmed by a
    /// `/System/Info/Public` round trip or seeded from Keychain. Refreshed
    /// at sign-in, restore, switch, and every websocket reconnect. `None`
    /// until known; [`Self::server_at_least`] fails closed on `None`.
    pub(crate) server_version: Option<jellyfin_api::ServerVersion>,
}

pub(crate) enum Screen {
    // Boxed: `MainState` and `ConnectState` are large enough to trip
    // clippy::large_enum_variant.
    Connect(Box<ConnectState>),
    Main(Box<MainState>),
    /// Brief transitional state between "old server's `MainState` torn
    /// down" and "new server's `MainState` ready" (`Root::switch_to_session`).
    /// Distinct from `Connect` (the target session's token is already
    /// known) and from `Main`'s `ContentMode::Loading` (there's no old
    /// `MainState` to keep around: its mirror/image-store/nav are for the
    /// wrong server).
    Switching,
}

pub(crate) struct ConnectState {
    pub server: Entity<TextInput>,
    pub username: Entity<TextInput>,
    pub password: Entity<TextInput>,
    /// Tab stop 3, after the three fields above -- the Connect button isn't
    /// a `TextInput`, so it needs its own handle to join the same
    /// Tab/Shift+Tab cycle. See `render_connect`.
    pub connect_button_focus: FocusHandle,
    /// Set true the first time this screen renders, right after autofocusing
    /// the first empty field -- guards that from re-firing on later renders.
    pub autofocused: bool,
    pub status: SharedString,
    /// Set alongside `status` for a transport-class failure -- `render_connect`
    /// shows this as a second, dimmer line under the error.
    pub error_hint: Option<SharedString>,
    pub connecting: bool,
    /// "Use Quick Connect" toggle -- when set, the form shows the Quick
    /// Connect code flow instead of the username/password fields.
    pub quick_connect: bool,
    /// The code to show once `quick_connect_initiate` returns, and the
    /// secret the poll loop uses -- `None` until Initiate completes.
    pub qc_code: Option<String>,
    pub qc_secret: Option<String>,
    /// Bumped every time a fresh Quick Connect attempt starts; the poll loop
    /// captures its value at spawn time and stops silently once it no
    /// longer matches, so a stale task can't clobber state a newer one owns.
    pub qc_generation: u64,
    /// True only when this Connect form was opened *over* an existing
    /// signed-in session, so Cancel/Escape can resume it; first-run/sign-out
    /// screens have nothing to go back to and keep this false.
    pub can_cancel: bool,
}

pub(crate) struct LibraryState {
    pub view_id: String,
    /// Currently displayed rows: `raw_items` sorted, then unwatched/genre
    /// filters applied. What `poster_grid` actually renders.
    pub items: Rc<Vec<CardRow>>,
    /// The full library, sorted by `sort`, before any filter -- kept around
    /// so toggling/clearing a filter is a pure in-memory re-filter, not a
    /// fresh `Mirror::children` round trip.
    pub raw_items: Rc<Vec<CardRow>>,
    /// Genres per item id, for the client-side genre filter -- built once
    /// per `raw_items` fetch from a batched mirror lookup, never during
    /// rendering (`CardRow`/the mirror have no genre column).
    pub genre_by_id: Rc<std::collections::HashMap<String, Vec<String>>>,
    /// The poster-hover spec strip: condensed (resolution/HDR/audio) fields
    /// per item id, derived in the same chunked metadata pass the genre
    /// index above pays for. Only the *highlighted* cell's entry is ever
    /// rendered (`grid.rs::poster_grid`'s `focused_spec`).
    pub spec_by_id: Rc<std::collections::HashMap<String, Vec<crate::ui::spec_strip::SpecField>>>,
    /// `(item_id, backdrop_tag)` of the featured item this library page
    /// draws its blurred/scrimmed backdrop from. Chosen by
    /// `grid::featured_index` (day-seeded, stable for the whole day) among
    /// items with a resolvable backdrop; `None` if nothing does.
    pub featured_backdrop: Option<(String, String)>,
    /// The [`current_day`] the sticky `featured_backdrop` pick was made on;
    /// re-rolls across a day boundary.
    pub featured_day: u64,
    /// Every genre string in this library, sorted case-insensitively, for
    /// the filter dropdown.
    pub genres: Vec<String>,
    pub focus: GridFocus,
    /// Last-input-wins arbiter kept in lockstep with `focus.index` -- every
    /// keyboard move/mouse hover updates both (see
    /// `Root::hover_library_cell`, `focus_grid::HighlightPolicy`).
    pub highlight: focus_grid::HighlightPolicy,
    /// Same latch as `HomeState::focus_engaged`: the highlight ring only
    /// paints after an arrow key or real hover touches this library.
    pub focus_engaged: bool,
    pub scroll: GridScroll,
    /// The list view keeps its OWN scroll handle rather than sharing
    /// `scroll` above, since a wall row (`columns` items) and a list row
    /// (one item) address different indices; each projection preserves its
    /// own scroll position across mode switches.
    pub list_scroll: GridScroll,
    /// Grid/List toggle. Persisted per library id through
    /// `AppSettings::library_view_mode`, written back by
    /// `Root::set_library_view_mode`.
    pub view_mode: LibraryViewMode,
    pub sort: media_cache::Sort,
    pub unwatched_only: bool,
    pub genre: Option<String>,
    /// Filter popover (Unwatched toggle + genre list, one trigger).
    pub filter_menu_open: bool,
    /// Sort popover (Name/Date Added/Premiere).
    pub sort_menu_open: bool,
}

pub(crate) struct MainState {
    pub(crate) client: JellyfinClient,
    pub(crate) mirror: Mirror,
    /// Kept alive only to keep the WebSocket supervisor running; never read.
    pub(crate) _bus_handle: jellyfin_core::EventBusHandle,
    pub(crate) image_store: ImageStore,
    /// Background poster warming (`image_warm.rs`): trickles this server's
    /// library posters into the disk image cache so a first browse isn't a
    /// wall of blurhashes. Owned here (not `ImageStore`) since its lifetime
    /// is the session's; aborted by `teardown_main_state`/`prepare_for_quit`.
    pub(crate) image_warm: crate::image_warm::ImageWarmer,
    pub(crate) views: Vec<media_cache::ViewSummary>,
    pub(crate) nav: Nav,
    pub(crate) home: HomeState,
    pub(crate) library: Option<LibraryState>,
    /// docs/PLUGIN-CHANNELS.md §2.2: live state for a
    /// `ViewKind::Channel` view's browse screen (never mirror-backed). `Some`
    /// only while `nav.current` is `View::Channel`, rebuilt whenever the
    /// (view, folder) pair changes.
    pub(crate) channel_browse: Option<crate::channel_browse::ChannelBrowseState>,
    pub(crate) detail: Option<DetailState>,
    pub(crate) search: SearchState,
    pub(crate) mode: ContentMode,
    pub(crate) paused: bool,
    pub(crate) reporting: Option<ReportingSession>,
    /// Item ids whose stream head has already been warmed this session --
    /// warming is idempotent server-side but there's no reason to re-issue
    /// the ranged GET on every Detail page revisit.
    pub(crate) warmed_stream_ids: std::collections::HashSet<String>,
    /// Display-sleep/screensaver suppression, held only while actively
    /// playing: created on start/unpause, dropped on pause/stop (mpv's
    /// `keep-open` flips pause=yes at EOF, so this covers end-of-media too).
    pub(crate) display_sleep: Option<crate::power::DisplaySleepGuard>,
    pub(crate) error: Option<String>,
    /// True when the last `EventBus` event was `Disconnected`, or this
    /// `MainState` was built from a cache-first-only resume. Cleared on the
    /// bus's next `Connected`.
    pub(crate) offline: bool,
    /// The item id currently loaded in the player, if any -- lets Detail
    /// show "Playing.../Resume" instead of "Play" for its own item.
    pub(crate) playing_item_id: Option<String>,
    /// Invalidates in-flight `play_item` flows: bumped by every `play_item`
    /// AND `stop_playback`, compared by `handle_playback_outcome`, so a stop
    /// (or newer play) landing mid-load wins over that flow's late `Ok`.
    /// Shared (`Arc<AtomicU64>`) so the spawned `playback::run()` task can
    /// re-check it itself immediately before `player.load()` -- a post-hoc
    /// check alone is too late to stop a stale flow from having already
    /// reloaded mpv. Only mutated from the GPUI foreground thread; `Relaxed`
    /// is enough since it's a pure equality token, not a lock.
    pub(crate) playback_generation: Arc<AtomicU64>,
    /// Handle for the `playback::run()` task `start_playback` spawns.
    /// Aborted on stop/supersede/quit: that task holds an `Arc<Player>`
    /// clone across two unbounded network awaits, so leaving it running
    /// could let a superseded flow call `player.load()` on the shared
    /// player, or let quit teardown run while a clone is still held.
    pub(crate) playback_task: Option<tokio::task::JoinHandle<()>>,
    /// OSD/miniplayer/track-picker/info-overlay state -- `Some` for the
    /// lifetime of one playback session, rebuilt fresh on every new
    /// `play_item` so a stale trickplay cache/track list can't leak.
    pub(crate) player_ui: Option<PlayerUiState>,
    /// A cold play flow's mpv `Loaded` event that arrived on the GPUI
    /// thread BEFORE `handle_playback_outcome` installed this session's
    /// `player_ui`, stashed here for that method to replay -- same
    /// mechanism `promote_preload` uses for a dark preload's `Loaded`.
    /// `PlaybackStarted` returns over a oneshot channel applied on a later
    /// GPUI turn, so a fast direct-play open can deliver `FileLoaded`
    /// first; without this stash the duration would be lost for the whole
    /// session, silently disabling the OSD progress bar, seeking, and
    /// `tick_next_episode`. Stamped with `playback_generation` so a
    /// superseded flow's `Loaded` can't replay onto a newer session.
    pub(crate) pending_loaded: Option<(u64, f64, Vec<player::Track>)>,
    /// A completed dark preload (paused stream open in mpv + cached
    /// PlaybackInfo/enrichment), waiting for a matching Play click to
    /// promote or for anything else to replace it. Only `Some` while idle;
    /// `play_item`/`teardown_main_state` clear it.
    pub(crate) preload: Option<crate::playback::PreloadReady>,
    /// Handle for the in-flight `playback::run_preload` task -- same
    /// abort-on-supersede/teardown/quit contract as `playback_task`.
    pub(crate) preload_task: Option<tokio::task::JoinHandle<()>>,
    /// Bandwidth accounting: bytes a dark preload had banked that were
    /// thrown away un-promoted, i.e. the real spend of speculating. Logged
    /// with a running total at every discard; promoted preloads log their
    /// banked bytes as savings instead.
    pub(crate) preload_bytes_wasted: i64,
    /// Which item the preload slot currently represents -- set the instant
    /// a new preload starts, before the network round trip that fills
    /// `preload` even begins. The retarget-throttle decision
    /// (`decide_preload_retarget`) needs that answer synchronously on every
    /// hover dwell, which `preload` alone can't give while still filling.
    pub(crate) preload_target: Option<String>,
    /// Monotonic counter for the hover-retarget throttle. Bumped on every
    /// `preload_item` call; a scheduled hover-retarget capture compares its
    /// snapshot against the live value before acting, so any later call
    /// invalidates it. Deliberately separate from `playback_generation`,
    /// which must NOT be bumped just for a hover dwell under consideration.
    pub(crate) hover_retarget_epoch: u64,
    /// Whether this session has already made its one automatic attempt at
    /// preloading the Home hero's Resume item. Left `false` if skipped
    /// because the preload setting was off at settle time, so a later
    /// re-enable can still get its one shot.
    pub(crate) hero_preload_attempted: bool,
    /// Single-flight guard for `Root::spawn_library_rebuild`: at most one
    /// off-thread rebuild of the visible library runs at a time.
    pub(crate) library_rebuild_in_flight: bool,
    /// Set when a refresh fires while a rebuild is already in flight;
    /// `apply_library_rebuild` reruns once, so a storm of triggers costs
    /// sequential rebuilds, never a pile-up.
    pub(crate) library_rebuild_dirty: bool,
    /// Handle for `playback.rs`'s "log hwdec_current ~5s in" background
    /// task. Must be aborted on stop/switch, or a stopped/superseded
    /// session's task fires later and logs a stale line attributed to
    /// whatever item happens to be playing at that point.
    pub(crate) hwdec_log_handle: Option<tokio::task::JoinHandle<()>>,
    /// Same lifecycle contract as `hwdec_log_handle`, for `playback.rs`'s
    /// periodic ~30s playback-diagnostics log.
    pub(crate) diag_log_handle: Option<tokio::task::JoinHandle<()>>,
    pub(crate) track_prefs: TrackPrefs,
    /// This server's `base_url` -- the key into `Root::app_settings`'s
    /// per-server bitrate cap map, and shown in the sidebar footer/switcher
    /// alongside the active session's username.
    pub(crate) base_url: String,
    /// Settings sheet open/section state (`settings.rs`).
    pub(crate) settings: crate::settings::SettingsState,
    /// Whether the sidebar account-footer's Server Switcher popover is open
    /// -- UI-only, a plain bool the popover's `open` param reads directly.
    /// The full CRUD Server & Account settings section stays the source of
    /// truth for add/remove/rename; this is only a quick-switch shortcut.
    pub(crate) server_switcher_open: bool,
    /// Latency instrumentation: set by `handle_playback_outcome`, filled in
    /// further by `on_loaded` (the mpv-`Loaded`-event checkpoint), and
    /// consumed (logged + cleared) by the first `on_position` afterward.
    /// `None` once logged, or before any item has loaded.
    pub(crate) pending_latency: Option<(crate::playback::LoadLatency, Option<u64>)>,
    /// Set by `collapse_fullscreen_player_for_nav` when a navigation action
    /// collapses Fullscreen-in-window/OS-Fullscreen playback down to the
    /// Miniplayer. Leaving the *native* OS-Fullscreen Space needs
    /// `Window::toggle_fullscreen`, which those call sites have no `&mut
    /// Window` for, so the request is left here for `render_main` to carry
    /// out and clear, exactly once.
    pub(crate) want_exit_os_fullscreen: bool,
    /// Same shape as `want_exit_os_fullscreen`, but for the OSD's fullscreen
    /// icon button. Unlike that field (one-directional), this always toggles.
    pub(crate) want_toggle_os_fullscreen: bool,
    /// Counts every live `client.get_items` call from the Detail-nav path.
    /// `JELLYBEAM_E2E` reads this around a Series -> Season -> Episode nav
    /// sequence to assert it stayed at zero (zero network on that path).
    pub(crate) item_fetch_count: u64,
    /// Live progress for the current `play_item`'s pre-`Player::load`
    /// pipeline -- `Some` only while `mode` is `ContentMode::Loading`;
    /// cleared once `handle_playback_outcome` resolves. The pipeline's
    /// *later* stages (mpv opening/buffering) are covered by
    /// `PlayerUiState::loading` instead.
    pub(crate) loading_stage_rx: Option<tokio::sync::watch::Receiver<crate::playback::LoadStage>>,
    /// Whether the "?" keyboard-shortcuts overlay is up. Lives on
    /// `MainState` (not `PlayerUiState`) so it works identically whether
    /// browsing or playing.
    pub(crate) shortcuts_overlay_open: bool,
    /// Per `discover/mod.rs`'s "zero startup cost" rule: a local-file-
    /// only read done once per session establishment and again after every
    /// Settings -> Discover connect/disconnect. The sidebar's "Discover" row
    /// reads `discover::sidebar_visible(&self.seerr_status)` on every render.
    pub(crate) seerr_status: seerr_api::SeerrStatus,
    /// The live Discover section, built lazily the first time this session
    /// visits any Discover screen -- `None` until then. Dropped along with
    /// the rest of `MainState` on every session swap, so a previous
    /// account's live `SeerrSession` can never leak into a fresh one.
    pub(crate) discover: Option<Box<discover::DiscoverState>>,
    /// Settings -> Discover's connect form -- always present rather than
    /// built lazily like `discover` above.
    pub(crate) discover_connect: discover::DiscoverConnectFormState,
}

impl MainState {
    /// The one place a playback start/stop transition fans out to every
    /// background consumer that has to yield to the stream: mirror breadth
    /// sync pauses between pages, and the poster warmer's trickle pauses
    /// entirely. Called from all four transition sites (two starts, two
    /// stops) so the two consumers can't drift out of sync with each other.
    /// Idempotent on both sides.
    pub(crate) fn set_playback_active(&self, active: bool) {
        self.mirror.set_playback_active(active);
        self.image_warm.set_playback_active(active);
    }
}

pub(crate) enum ContentMode {
    Browse,
    Loading { title: String },
    Playing { title: String, decision: String },
}

/// Best-effort breadcrumb metadata threaded alongside `series_id` through
/// `play_item`'s `PlaybackInfo -> decide -> load` async hop into
/// `handle_playback_outcome`; only available when Play was reached from the
/// series' own Detail page. All-`None` (`Default`) degrades
/// `PlayerUiState::breadcrumb_title` to just the plain item title.
#[derive(Debug, Clone, Default)]
pub(crate) struct EpisodeContext {
    pub series_name: Option<String>,
    pub season_number: Option<i32>,
    pub episode_number: Option<i32>,
    /// The selected season's own item id -- needed (alongside `series_id`)
    /// so prev/next-episode navigation and the next-episode card know which
    /// season's episode list to walk without re-deriving it from
    /// `season_number` (a season's `IndexNumber` isn't guaranteed to equal
    /// its position in a `Mirror::children()` list, e.g. a "Specials"
    /// season numbered 0).
    pub season_id: Option<String>,
}

/// Which direction `play_adjacent_episode`/`adjacent_episode` walk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EpisodeStep {
    Next,
    Prev,
}

/// How close to the real end of a file (in seconds) `tick_next_episode`
/// treats "playback has actually reached EOF" -- looser than a single
/// `Position` tick's cadence (~250ms at 4Hz) so a slightly-early final tick
/// still counts, tight enough that it only fires once the file is
/// genuinely done.
pub(crate) const EOF_EPSILON_SECS: f64 = 0.35;

/// The episode adjacent to `item_id` (which belongs to `season_id`, under
/// `series_id`) in playback order, crossing a season boundary when
/// `item_id` is first/last in its own season. Every read is
/// `Mirror::children()` under `Sort::IndexNumber` (zero network, mirror-only)
/// -- cheap enough to call from both a one-off keyboard press
/// (`play_adjacent_episode`) and `tick_next_episode`'s ~4Hz poll.
pub(crate) fn adjacent_episode(
    mirror: &Mirror,
    series_id: &str,
    season_id: &str,
    item_id: &str,
    step: EpisodeStep,
) -> Option<CardRow> {
    let episodes = mirror.children(season_id, Sort::IndexNumber, 0, 500);
    let ix = episodes.iter().position(|e| e.id == item_id)?;
    if let Some(found) = adjacent_playable(&episodes, ix, step) {
        return Some(found);
    }

    // `item_id` was first/last in its season -- cross into the
    // next/previous season, if there is one, in *watch* order, so a
    // Specials season (number 0, sorts first in the raw list) isn't
    // treated as the season before season 1.
    let seasons = mirror.children(series_id, Sort::IndexNumber, 0, 200);
    let ordered = crate::detail::seasons_in_watch_order(&seasons);
    let season_ix = ordered.iter().position(|s| s.id == season_id)?;
    let adjacent_season = match step {
        EpisodeStep::Next => ordered.get(season_ix + 1)?,
        EpisodeStep::Prev => ordered.get(season_ix.checked_sub(1)?)?,
    };
    let episodes = mirror.children(&adjacent_season.id, Sort::IndexNumber, 0, 500);
    let mut playable = episodes.iter().filter(|e| !e.is_virtual);
    match step {
        EpisodeStep::Next => playable.next().cloned(),
        EpisodeStep::Prev => playable.next_back().cloned(),
    }
}

/// The nearest episode after (`Next`) or before (`Prev`) `ix` that actually
/// has media behind it. Virtual (unaired/missing) episodes sit inline in
/// `Mirror::children`'s ordering -- stepping onto one means `[`/`]` and EOF
/// auto-advance both land on "Can't play" instead of the next real episode.
/// Split out from `adjacent_episode` so it's testable without a live
/// `Mirror`.
fn adjacent_playable(episodes: &[CardRow], ix: usize, step: EpisodeStep) -> Option<CardRow> {
    match step {
        EpisodeStep::Next => episodes
            .get(ix + 1..)?
            .iter()
            .find(|e| !e.is_virtual)
            .cloned(),
        EpisodeStep::Prev => episodes
            .get(..ix)?
            .iter()
            .rev()
            .find(|e| !e.is_virtual)
            .cloned(),
    }
}

/// Sidebar visibility during Fullscreen-in-window playback, per docs/UX-SPEC.md §3.
/// Computed once by `Root::sidebar_mode` -- the single source of truth
/// `render_main` and `JELLYBEAM_E2E`'s layout-state assertion both read, so the
/// test can't drift from what's actually rendered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SidebarMode {
    /// Normal Browse, or Playing-while-Miniplayer: sidebar is live, in-flow
    /// chrome (docs/UX-SPEC.md §3: "browse/search fully usable behind it").
    Shown,
    /// Normal Browse, below `SIDEBAR_COLLAPSE_BREAKPOINT` width: sidebar is
    /// still live, in-flow chrome, but collapsed to a narrow icon rail
    /// (`SIDEBAR_RAIL_WIDTH`) instead of the full labeled sidebar. Same
    /// click targets/⌘N shortcuts as `Shown`, just icons + tooltips instead
    /// of icons + labels.
    Collapsed,
    /// Native OS-Fullscreen **or** Fullscreen-in-window playback: no
    /// sidebar, no reveal gesture -- "video + OSD own the whole screen"
    /// (docs/UX-SPEC.md's non-goals). A left-edge hover-reveal strip was tried and
    /// removed: it fired when the user aimed for the OSD's own left-side
    /// transport controls, sliding the sidebar over them instead.
    /// Fullscreen-in-window playback is still reachable without a reveal
    /// gesture -- Esc stops playback outright, ⌘1..9 jump straight to a
    /// sidebar destination (bypasses `SidebarMode`, see
    /// `handle_global_keystroke`), and leaving the player (`⌘M` to
    /// Miniplayer) restores it.
    Hidden,
}

impl SidebarMode {
    /// The actual in-flow width this sidebar shape claims, in px -- shared
    /// by `render_main`'s real layout math (`sidebar_width_for_layout`) and
    /// `JELLYBEAM_E2E`'s resize-sweep assertion (`assert_resize_sweep_layout_
    /// invariants`) so the two can never drift apart.
    pub(crate) fn width_px(self) -> f64 {
        match self {
            SidebarMode::Shown => crate::gl_video::SIDEBAR_WIDTH,
            SidebarMode::Collapsed => crate::gl_video::SIDEBAR_RAIL_WIDTH,
            SidebarMode::Hidden => 0.0,
        }
    }
}

/// What Return/click should do for the currently focused item -- computed
/// while a view's state is still borrowed, then acted on afterwards so the
/// borrow doesn't overlap the `&mut self` call it triggers.
pub(crate) enum Activate {
    OpenDetail(String),
    PlayItem(String, String),
    /// Return on the Episode Detail page's sibling rail switches to that
    /// episode's own page in place (`Root::open_episode_in_place`),
    /// mirroring the rail's own click behavior -- distinct from
    /// `OpenDetail` (which pushes nav history via `Nav::go`) and from
    /// `PlayItem` (what Return does on the *series* page's episode grid).
    SwitchEpisode(String),
    /// docs/PLUGIN-CHANNELS.md §2.2: Return on a
    /// level-1 channel row opens the folder (level 2) in place, mirroring
    /// its own click behavior (`Root::open_channel_folder`) -- never a
    /// detail page.
    OpenChannelFolder(String),
    /// §2.3: Return on a level-2 channel row plays the recording directly
    /// (`Root::play_item_with_resume_hint`), carrying the resume position
    /// read off its own live DTO since it is never mirrored.
    PlayRecording(String, String, Option<i64>),
}

/// Builds a fresh `ConnectState` bound to `root_weak` -- the shared
/// constructor for `Root::new`, the post-resume-failure fallback, and "Add
/// server", so all three stay in sync. Tab/Shift+Tab order: Server URL ->
/// Username -> Password -> Connect button, then wraps. Named constants
/// (rather than inline magic numbers) so the order is defined once and
/// `connect_tab_order_is_ascending_and_unique` below can check it.
const CONNECT_TAB_SERVER: isize = 0;
const CONNECT_TAB_USERNAME: isize = 1;
const CONNECT_TAB_PASSWORD: isize = 2;
const CONNECT_TAB_CONNECT_BUTTON: isize = 3;

/// Idle-state constructor -- see `new_connect_state_connecting` for the
/// "already resuming, show status immediately" variant. Both are thin,
/// named wrappers over `new_connect_state_with`.
pub(crate) fn new_connect_state(
    cx: &mut Context<Root>,
    root_weak: gpui::WeakEntity<Root>,
) -> ConnectState {
    new_connect_state_with(cx, root_weak, false)
}

/// Connecting-state constructor -- see `new_connect_state` just above.
pub(crate) fn new_connect_state_connecting(
    cx: &mut Context<Root>,
    root_weak: gpui::WeakEntity<Root>,
) -> ConnectState {
    new_connect_state_with(cx, root_weak, true)
}

fn new_connect_state_with(
    cx: &mut Context<Root>,
    root_weak: gpui::WeakEntity<Root>,
    connecting: bool,
) -> ConnectState {
    // Return submits from any of the three fields; `on_connect_clicked`
    // already guards against submitting while `connecting`/`quick_connect`.
    //
    // This closure runs from *inside* `TextInput::handle_key_down`, which
    // GPUI has already leased to dispatch this "enter" keystroke --
    // `EntityMap::lease`/`read` panics ("cannot read/update <T> while it is
    // already being updated") if the same entity is touched again before
    // that lease returns, and `on_connect_clicked` reads all three fields
    // including the one Enter was pressed in. This runs inside AppKit's
    // `extern "C" handle_key_event`, so the panic can't unwind across that
    // boundary and aborts the process. `cx.defer` schedules the callback
    // for the end of the current effect cycle, after every entity on the
    // stack has been returned to the app.
    let submit = move |root_weak: gpui::WeakEntity<Root>| {
        move |_window: &mut Window, cx: &mut Context<TextInput>| {
            let root_weak = root_weak.clone();
            cx.defer(move |cx| {
                let _ = root_weak.update(cx, |root, cx| root.on_connect_clicked(cx));
            });
        }
    };
    ConnectState {
        server: cx.new(|cx| {
            TextInput::new(cx, "http://localhost:8096")
                .tab_index(CONNECT_TAB_SERVER)
                .on_enter(submit(root_weak.clone()))
        }),
        username: cx.new(|cx| {
            TextInput::new(cx, "username")
                .tab_index(CONNECT_TAB_USERNAME)
                .on_enter(submit(root_weak.clone()))
        }),
        password: cx.new(|cx| {
            TextInput::new(cx, "password")
                .password()
                .tab_index(CONNECT_TAB_PASSWORD)
                .on_enter(submit(root_weak.clone()))
        }),
        connect_button_focus: cx
            .focus_handle()
            .tab_index(CONNECT_TAB_CONNECT_BUTTON)
            .tab_stop(true),
        autofocused: false,
        status: SharedString::default(),
        error_hint: None,
        connecting,
        quick_connect: false,
        qc_code: None,
        qc_secret: None,
        can_cancel: false,
        qc_generation: 0,
    }
}

/// Only [`jellyfin_api::ApiError::Transport`] failures get the Connect
/// screen's network-troubleshooting hint: a rejected login/decode error
/// doesn't call for VPN/hostname advice.
const TRANSPORT_ERROR_HINT: &str = "Check the address and any VPN (Tailscale etc.). If a \
     short hostname fails, try the full name (e.g. myhost.tailXXXX.ts.net) or the IP.";

/// Brand §5's error state: "Archivo, factual, one line". Everything that
/// isn't a transport failure keeps the server's own words rather than being
/// paraphrased into something vaguer -- the audience runs these servers and
/// the real message is the useful one.
pub(crate) fn connect_error_line(message: &str) -> String {
    if message.starts_with("transport: ") {
        "Server unreachable — check the server URL.".to_string()
    } else {
        format!("Could not sign in — {message}")
    }
}

/// For the Quick Connect paths, which still have the real
/// [`jellyfin_api::ApiError`] in hand.
pub(crate) fn transport_error_hint_for_api_error(
    err: &jellyfin_api::ApiError,
) -> Option<SharedString> {
    matches!(err, jellyfin_api::ApiError::Transport(_)).then(|| TRANSPORT_ERROR_HINT.into())
}

/// For `handle_connect_outcome`'s login/resume path: `session::connect_flow`/
/// `resume_flow` flatten every `ApiError` to a `String` before it reaches
/// `Root`, so the only signal left is `ApiError::Transport`'s own `Display`
/// prefix (`#[error("transport: {0}")]` in jellyfin-api's `lib.rs`).
pub(crate) fn transport_error_hint_for_message(message: &str) -> Option<SharedString> {
    message
        .starts_with("transport: ")
        .then(|| TRANSPORT_ERROR_HINT.into())
}

/// Featured-backdrop seed: whole days since the epoch. Pairs with the
/// library id inside `grid::featured_index` so a library's page keeps one
/// backdrop for a day's worth of visits and rotates after that, never
/// per-frame or per-visit randomness. Falls back to day 0 if the system
/// clock is before the epoch.
fn current_day() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() / 86_400)
        .unwrap_or(0)
}

/// Builds a `LibraryState` for `view_id`, sorted by `sort`, with no filters
/// applied yet. Also builds the genre/spec/backdrop metadata in one chunked
/// mirror pass. Synchronous (this is the click-to-open path); every LATER
/// refresh goes through `Root::spawn_library_rebuild` and never blocks the
/// GPUI thread. `view_mode` is passed in rather than read from
/// `AppSettings` since this fn is also the "sort changed, rebuild" path,
/// where the mode to keep is the one on screen, not the one last persisted.
fn build_library_state(
    mirror: &Mirror,
    view_id: String,
    sort: media_cache::Sort,
    view_mode: LibraryViewMode,
) -> LibraryState {
    // First build has no previous grid to preserve, so a failed query can
    // only degrade to an empty (already-logged) one.
    let raw_items = Rc::new(library_rows(mirror, &view_id, sort).unwrap_or_default());
    let metadata = compute_library_metadata(mirror, &raw_items);
    let mut library = LibraryState {
        view_id,
        items: raw_items.clone(),
        raw_items,
        genre_by_id: Rc::default(),
        spec_by_id: Rc::default(),
        featured_backdrop: None,
        featured_day: 0,
        genres: Vec::new(),
        focus: GridFocus::new(1),
        highlight: focus_grid::HighlightPolicy::new(),
        focus_engaged: false,
        scroll: GridScroll::new(),
        list_scroll: GridScroll::new(),
        view_mode,
        sort,
        unwatched_only: false,
        genre: None,
        filter_menu_open: false,
        sort_menu_open: false,
    };
    apply_library_metadata(&mut library, metadata);
    library
}

/// Load every child in one SQLite statement. This gives the poster wall a
/// consistent snapshot; OFFSET paging across separately acquired readers can
/// duplicate or omit rows while the sync writer commits between pages.
/// `None` means the query failed (already logged), NOT an empty library --
/// the refresh path keeps the previous grid in that case.
fn library_rows(mirror: &Mirror, view_id: &str, sort: media_cache::Sort) -> Option<Vec<CardRow>> {
    mirror.children_checked(view_id, sort, 0, u32::MAX)
}

/// The derived per-item decorations a library grid renders alongside its
/// rows. Plain owned data (no `Rc`) so it can be produced on a blocking
/// runtime thread and sent to the GPUI thread for `apply_library_metadata`.
struct LibraryMetadata {
    genre_by_id: std::collections::HashMap<String, Vec<String>>,
    spec_by_id: std::collections::HashMap<String, Vec<crate::ui::spec_strip::SpecField>>,
    backdrop_candidates: Vec<(String, String)>,
    genres: Vec<String>,
}

/// A finished off-thread library rebuild, ready to swap in (see
/// `Root::apply_library_rebuild`). Carries its own view/sort so a result
/// that raced a navigation or sort change can be recognized and discarded.
struct LibraryRebuild {
    view_id: String,
    sort: media_cache::Sort,
    rows: Vec<CardRow>,
    metadata: LibraryMetadata,
}

/// Chunk size for the metadata DTO pass -- matches the mirror's own SQL
/// parameter batch, so each `Mirror::items` call is a single statement.
const METADATA_CHUNK: usize = 900;

/// Genre index, poster-hover spec strips, and the featured-backdrop
/// candidate list all come out of one shared blob pass over the library's
/// DTOs. Chunked so each chunk's parsed DTOs are folded into the derived
/// maps and dropped before the next chunk loads, capping peak DTO memory
/// at one chunk. At 8k+ items this chunking is why refreshes run on a
/// blocking runtime thread, never the GPUI thread.
fn compute_library_metadata(mirror: &Mirror, rows: &[CardRow]) -> LibraryMetadata {
    let mut genre_by_id = std::collections::HashMap::with_capacity(rows.len());
    let mut genre_set: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut spec_by_id = std::collections::HashMap::with_capacity(rows.len());
    let mut backdrop_candidates: Vec<(String, String)> = Vec::new();
    for chunk in rows.chunks(METADATA_CHUNK) {
        let ids: Vec<String> = chunk.iter().map(|row| row.id.clone()).collect();
        let dtos = mirror.items(&ids);
        for row in chunk {
            let dto = dtos.get(&row.id);
            let genres = dto.map(|dto| dto.genres.clone()).unwrap_or_default();
            for g in &genres {
                genre_set.insert(g.clone());
            }
            genre_by_id.insert(row.id.clone(), genres);
            if let Some(dto) = dto {
                let fields = crate::ui::spec_strip::MediaFacts::from_dto(dto).condensed_fields();
                if !fields.is_empty() {
                    spec_by_id.insert(row.id.clone(), fields);
                }
                if let Some(source) = crate::detail::backdrop_source(dto) {
                    backdrop_candidates.push(source);
                }
            }
        }
    }
    LibraryMetadata {
        genre_by_id,
        spec_by_id,
        backdrop_candidates,
        genres: genre_set.into_iter().collect(),
    }
}

fn apply_library_metadata(library: &mut LibraryState, mut metadata: LibraryMetadata) {
    library.genre_by_id = Rc::new(metadata.genre_by_id);
    library.spec_by_id = Rc::new(metadata.spec_by_id);
    // Sticky within a day: re-picking from a growing candidate count would
    // jump the backdrop on every refresh. Re-rolls across a day boundary,
    // since a `LibraryState` can outlive midnight.
    let day = current_day();
    let featured_still_exists = library
        .featured_backdrop
        .as_ref()
        .is_some_and(|featured| metadata.backdrop_candidates.contains(featured));
    if !featured_still_exists || library.featured_day != day {
        library.featured_backdrop =
            crate::grid::featured_index(&library.view_id, day, metadata.backdrop_candidates.len())
                .map(|ix| metadata.backdrop_candidates.swap_remove(ix));
        library.featured_day = day;
    }
    library.genres = metadata.genres;
    // A refresh can retire the active genre filter (the last item carrying
    // it was removed or retagged). Clear the filter rather than leaving an
    // empty grid under a chip the dropdown no longer offers.
    if library
        .genre
        .as_ref()
        .is_some_and(|g| !library.genres.contains(g))
    {
        library.genre = None;
    }
}

/// The read half of an off-thread library refresh: one consistent-snapshot
/// row query plus the chunked DTO metadata pass. Runs via
/// `Runtime::spawn_blocking` (both halves are synchronous SQLite reads).
/// `None` means the row query failed -- the caller keeps the last good grid.
fn compute_library_rebuild(
    mirror: &Mirror,
    view_id: String,
    sort: media_cache::Sort,
) -> Option<LibraryRebuild> {
    let rows = library_rows(mirror, &view_id, sort)?;
    let metadata = compute_library_metadata(mirror, &rows);
    Some(LibraryRebuild {
        view_id,
        sort,
        rows,
        metadata,
    })
}

/// Sidebar sync pill: fill fraction for the determinate progress bar.
/// `None` (no bar) until the walk's first page has landed a denominator;
/// clamped so a server-side total that shrinks mid-walk can't overflow.
fn sync_progress_fraction(items_done: u32, total_items: Option<u32>) -> Option<f32> {
    let total = total_items.filter(|t| *t > 0)?;
    Some((items_done as f32 / total as f32).clamp(0.0, 1.0))
}

/// Applies `unwatched_only`/`genre` to `raw_items`, writing the result into
/// `items`. Pure in-memory filter (no I/O) -- called after every filter
/// toggle and after a `sort` change re-fetches `raw_items`.
fn apply_library_filters(lib: &mut LibraryState) {
    let filtered: Vec<CardRow> = lib
        .raw_items
        .iter()
        .filter(|row| !lib.unwatched_only || !row.played)
        .filter(|row| match &lib.genre {
            None => true,
            Some(g) => lib
                .genre_by_id
                .get(&row.id)
                .is_some_and(|genres| genres.iter().any(|item_genre| item_genre == g)),
        })
        .cloned()
        .collect();
    lib.items = Rc::new(filtered);
    lib.focus.clamp(lib.items.len());
}

/// Builds one server-switch/login flow's `MainState` from a freshly opened
/// `ConnectedBundle` -- the shared tail of `handle_connect_outcome` (fresh
/// login/resume-at-launch) and `handle_switch_outcome` (server switch), so
/// both build the exact same shape of fresh `MainState`.
pub(crate) fn main_state_from_bundle(
    bundle: ConnectedBundle,
    base_url: String,
    runtime: Arc<tokio::runtime::Runtime>,
    app_settings: &crate::settings::AppSettings,
    cx: &mut Context<Root>,
) -> MainState {
    // Per `discover/mod.rs`'s "zero startup cost" rule: a local JSON
    // read only (see `seerr_status`'s field doc comment).
    let seerr_user_id = bundle.client.user_id().unwrap_or_default().to_string();
    let seerr_status =
        seerr_api::status(&crate::paths::state_root(), &base_url, &seerr_user_id, None);
    let discover_connect = discover::DiscoverConnectFormState::new(cx);
    let image_cache = bundle.image_cache.clone();
    let image_store = ImageStore::new(bundle.image_cache, runtime.clone());
    // One warm pass is requested immediately to warm sidebar order (Home is
    // the initial view, no library open yet). A brand-new mirror simply
    // scans empty; the rescan on the first settled sync catches a
    // first-ever launch once there are rows to warm.
    let image_warm =
        crate::image_warm::ImageWarmer::spawn(bundle.mirror.clone(), image_cache, &runtime);
    image_warm.request_pass(None);
    // Push the currently-configured Next Up cutoff/rewatching into the
    // freshly-opened mirror right away -- `sync::refresh_next_up` reads
    // this on its very first call, so a non-default setting is already in
    // effect for the first Next Up fetch of the session.
    bundle
        .mirror
        .set_next_up_options(app_settings.next_up.to_next_up_options());
    let hidden_libraries = app_settings.hidden_home_libraries();
    let home = HomeState::load(
        &bundle.mirror,
        &bundle.views,
        &hidden_libraries,
        app_settings.hide_watched_latest,
    );
    let offline = bundle.offline;
    MainState {
        client: bundle.client,
        mirror: bundle.mirror,
        _bus_handle: bundle.bus_handle,
        image_store,
        image_warm,
        views: bundle.views,
        nav: Nav::new(),
        home,
        library: None,
        channel_browse: None,
        detail: None,
        search: SearchState::new(),
        mode: ContentMode::Browse,
        paused: false,
        reporting: None,
        warmed_stream_ids: std::collections::HashSet::new(),
        display_sleep: None,
        error: None,
        offline,
        playing_item_id: None,
        playback_generation: Arc::new(AtomicU64::new(0)),
        playback_task: None,
        preload: None,
        preload_task: None,
        preload_bytes_wasted: 0,
        preload_target: None,
        hover_retarget_epoch: 0,
        hero_preload_attempted: false,
        library_rebuild_in_flight: false,
        library_rebuild_dirty: false,
        player_ui: None,
        pending_loaded: None,
        hwdec_log_handle: None,
        diag_log_handle: None,
        track_prefs: TrackPrefs::load(),
        base_url,
        settings: crate::settings::SettingsState::new(),
        server_switcher_open: false,
        pending_latency: None,
        want_exit_os_fullscreen: false,
        want_toggle_os_fullscreen: false,
        item_fetch_count: 0,
        loading_stage_rx: None,
        shortcuts_overlay_open: false,
        seerr_status,
        discover: None,
        discover_connect,
    }
}

impl Root {
    /// Builds the initial Connect-screen state. If `sessions` has an active
    /// entry (a Keychain-stored session was found), immediately kicks off
    /// the resume flow in the background and shows a "Resuming..." status
    /// instead of blank fields.
    pub(crate) fn new(
        cx: &mut Context<Self>,
        runtime: Arc<tokio::runtime::Runtime>,
        identity: ClientIdentity,
        video: std::rc::Rc<VideoLayer>,
        last_position_ticks: Arc<AtomicI64>,
        sessions: crate::keychain::StoredSessionList,
        launched_at: Instant,
    ) -> Self {
        let root_weak = cx.entity().downgrade();
        let resume = sessions.active_session().cloned();
        let connect = if resume.is_some() {
            new_connect_state_connecting(cx, root_weak)
        } else {
            new_connect_state(cx, root_weak)
        };
        let mut root = Root {
            runtime,
            identity,
            video,
            about_window: None,
            last_position_ticks,
            next_episode_handover_gen: 0,
            screen: Screen::Connect(Box::new(connect)),
            perf_frame_log: None,
            now_playing: None,
            promoted_preloads: 0,
            library_rebuild_seq: 0,
            sessions,
            app_settings: crate::settings::AppSettings::load(),
            connect_generation: 0,
            // Only the auto-resume-at-launch path is a "warm start" -- see
            // the field's doc comment.
            warm_start_at: None,
            option_speed_monitor: None,
            // Seeded from the resumed session's stored value just below, if
            // there is one -- otherwise there's no session at all yet, so
            // `None` (unknown) is correct until a fresh login completes.
            server_version: None,
        };
        root.server_version = root
            .sessions
            .active_session()
            .and_then(|s| s.server_version.as_deref())
            .and_then(|v| v.parse().ok());
        if let Some(stored) = resume {
            root.warm_start_at = Some(launched_at);
            root.spawn_resume(stored, cx);
        }
        root
    }

    /// Called once from `main.rs` right after the window opens (must run on
    /// the main thread with a live `NSApplication` run loop -- see
    /// `now_playing::NowPlaying::register`'s doc comment).
    pub(crate) fn set_now_playing(&mut self, np: Option<crate::now_playing::NowPlaying>) {
        self.now_playing = np;
    }

    /// Same "stash the registration result" shape as `set_now_playing` --
    /// see `option_speed_monitor`'s field doc.
    pub(crate) fn set_option_speed_monitor(
        &mut self,
        monitor: Option<crate::option_speed_hold::OptionSpeedMonitor>,
    ) {
        self.option_speed_monitor = monitor;
    }

    /// Version gate: `true` only once the version is confirmed AND `>=
    /// (major, minor)`. Fails closed on an unknown version -- callers must
    /// never treat "haven't checked yet" as "new enough".
    #[allow(dead_code)] // no version-gated feature exists yet; kept for the next one.
    pub(crate) fn server_at_least(&self, major: u32, minor: u32) -> bool {
        self.server_version
            .is_some_and(|v| v.at_least(major, minor))
    }

    /// `JELLYBEAM_PERF=1` hook (`perf.rs`): start recording a timestamp on
    /// every `Render::render` call, returning the shared log.
    pub(crate) fn enable_perf_frame_log(&mut self) -> Rc<RefCell<Vec<Instant>>> {
        let log = Rc::new(RefCell::new(Vec::new()));
        self.perf_frame_log = Some(log.clone());
        log
    }

    /// Plants this server's persisted DNS answer into the process-wide
    /// resolver **before** anything issues a request against it. The system
    /// resolver intermittently takes ~5s for the bare hostname, and the
    /// stale-while-revalidate cache starts empty every launch, so the first
    /// lookup would otherwise block every queued API call. Called from
    /// every path about to build a client for a server.
    pub(crate) fn seed_dns_for(&self, base_url: &str) {
        let Some(host) = jellyfin_api::dns::cacheable_host(base_url) else {
            // An IP-literal (or unparseable) base URL has nothing to resolve.
            return;
        };
        let addrs = self.app_settings.dns_seed(&host);
        if addrs.is_empty() {
            return;
        }
        jellyfin_api::dns::shared_dns_resolver().seed(&host, addrs);
    }

    /// The other half of [`Self::seed_dns_for`]: snapshot the resolver's
    /// current answer for the active server's host and store it for next
    /// launch. Best-effort and idempotent. Deliberately *not* run from the
    /// background connectivity check in `session.rs`: keeping every write
    /// on the GPUI thread means a seed update can never race a settings
    /// change the user just made.
    pub(crate) fn persist_dns_seed(&mut self) {
        let Some(state) = self.main_state() else {
            return;
        };
        let Some(host) = jellyfin_api::dns::cacheable_host(&state.base_url) else {
            return;
        };
        let Some(addrs) = jellyfin_api::dns::shared_dns_resolver().snapshot(&host) else {
            // Nothing resolved yet (or already evicted) -- nothing to
            // persist. The next `Connected` will find one.
            return;
        };
        self.app_settings.set_dns_seed(&host, &addrs);
    }

    /// A fresh `connect_flow`/`quick_connect_flow` login always mints a new
    /// `StoredSession` with `server_version: None`. Copies the previous
    /// confirmed value forward for this `(base_url, user_id)` pair so
    /// re-authenticating an already-known account doesn't forget it.
    /// `resume_flow`/`switch_to_session` never need this: they reuse the
    /// on-disk `StoredSession` unchanged.
    pub(crate) fn preserve_known_server_version(&self, stored: &mut StoredSession) {
        if stored.server_version.is_some() {
            return;
        }
        if let Some(previous) = self
            .sessions
            .sessions
            .iter()
            .find(|s| s.base_url == stored.base_url && s.user_id == stored.user_id)
        {
            stored.server_version = previous.server_version.clone();
        }
    }

    /// Seeds `self.server_version` from a `StoredSession`'s persisted
    /// string (sign-in/resume/switch), tolerating an absent or unparseable
    /// value by leaving/resetting it to `None` -- an unknown version must
    /// never be treated as "confirmed" (`server_at_least` fails closed).
    pub(crate) fn seed_server_version(&mut self, stored: &StoredSession) {
        self.server_version = stored
            .server_version
            .as_deref()
            .and_then(|v| v.parse().ok());
    }

    /// Runs `work` on `self.runtime` and applies the result back on the
    /// GPUI thread via `apply` -- the oneshot + spawn + `.detach()`
    /// plumbing shared by every network-fetch call site in this file. If
    /// the reply channel is dropped, `apply` is simply never called; see
    /// [`Self::bridge_or`] for a fallback `T`. Staleness guards are the
    /// caller's job, inside `work` and/or `apply`.
    pub(crate) fn bridge<T: Send + 'static>(
        &self,
        cx: &mut Context<Self>,
        work: impl std::future::Future<Output = T> + Send + 'static,
        apply: impl FnOnce(&mut Self, T, &mut Context<Self>) + 'static,
    ) {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.runtime.spawn(async move {
            let _ = tx.send(work.await);
        });
        cx.spawn(async move |this, cx| {
            if let Ok(result) = rx.await {
                let _ = this.update(cx, |root, cx| apply(root, result, cx));
            }
        })
        .detach();
    }

    /// Same bridging as [`Self::bridge`], but for call sites where a dropped
    /// reply channel must still resolve `apply` -- `on_dropped` synthesizes
    /// the fallback `T` (e.g. an `Err("... task dropped")`) so the UI still
    /// leaves whatever "in flight" state it set before spawning.
    pub(crate) fn bridge_or<T: Send + 'static>(
        &self,
        cx: &mut Context<Self>,
        work: impl std::future::Future<Output = T> + Send + 'static,
        on_dropped: impl FnOnce() -> T + Send + 'static,
        apply: impl FnOnce(&mut Self, T, &mut Context<Self>) + 'static,
    ) {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.runtime.spawn(async move {
            let _ = tx.send(work.await);
        });
        cx.spawn(async move |this, cx| {
            let result = rx.await.unwrap_or_else(|_| on_dropped());
            this.update(cx, |root, cx| apply(root, result, cx)).ok();
        })
        .detach();
    }

    /// The active `MainState`, if `self.screen` is currently `Screen::Main`
    /// -- a `Some`-returning alternative to the `let Screen::Main(state) =
    /// &self.screen else { return; }` guard repeated throughout this file.
    pub(crate) fn main_state(&self) -> Option<&MainState> {
        match &self.screen {
            Screen::Main(state) => Some(state),
            _ => None,
        }
    }

    /// Mutable counterpart to [`Self::main_state`].
    pub(crate) fn main_state_mut(&mut self) -> Option<&mut MainState> {
        match &mut self.screen {
            Screen::Main(state) => Some(state),
            _ => None,
        }
    }

    /// The active `DiscoverState`, if `self.screen` is `Screen::Main` and
    /// Discover has been opened for this account.
    pub(crate) fn discover_mut(&mut self) -> Option<&mut discover::DiscoverState> {
        self.main_state_mut()?.discover.as_deref_mut()
    }

    /// Kicks off a non-blocking `/System/Info/Public` refresh against the
    /// active session's client -- called after sign-in, session restore,
    /// account switch, and every websocket reconnect/reauthorization. A
    /// no-op if there's no active `MainState`. Never blocks the GPUI
    /// thread: runs on `self.runtime`, result via `bridge`.
    pub(crate) fn spawn_server_version_refresh(&self, cx: &mut Context<Self>) {
        let Some(state) = self.main_state() else {
            return;
        };
        let client = state.client.clone();
        let base_url = state.base_url.clone();
        let user_id = client.user_id().map(|s| s.to_string());
        // Task-dropped case: nothing to report; the stored value (if any)
        // stands -- `bridge`'s default of skipping `apply` on a dropped
        // reply channel is exactly that.
        self.bridge(
            cx,
            async move { client.refresh_public_system_info().await },
            move |root, result, cx| {
                root.handle_server_version_refresh(base_url, user_id, result, cx)
            },
        );
    }

    /// Applies the outcome of [`Self::spawn_server_version_refresh`]. A
    /// failed request or unparseable version is debug-logged and leaves
    /// `self.server_version` unchanged -- refresh failures never regress a
    /// previously-confirmed version back to unknown. A change is persisted
    /// to `self.server_version`, the matching `StoredSession`, and the
    /// Keychain, then `cx.notify()`.
    fn handle_server_version_refresh(
        &mut self,
        base_url: String,
        user_id: Option<String>,
        result: Result<jellyfin_api::PublicServerInfo, jellyfin_api::ApiError>,
        cx: &mut Context<Self>,
    ) {
        let info = match result {
            Ok(info) => info,
            Err(e) => {
                tracing::debug!(
                    error = %e,
                    base_url = %base_url,
                    "server version refresh failed; keeping the last known value"
                );
                return;
            }
        };
        let Some(version) = info.parsed_version() else {
            tracing::debug!(
                base_url = %base_url,
                version = ?info.version,
                "server version refresh returned an unparseable version; \
                 keeping the last known value"
            );
            return;
        };
        // A refresh started before a server switch must not stamp the new
        // session with the old server's version.
        if !self.main_state().is_some_and(|s| s.base_url == base_url) {
            return;
        }
        if self.server_version == Some(version) {
            return; // unchanged -- nothing to persist or notify.
        }
        tracing::info!(server_version = %version, "server version changed");
        self.server_version = Some(version);
        if let Some(session) = self
            .sessions
            .sessions
            .iter_mut()
            .find(|s| s.base_url == base_url && s.user_id == user_id)
        {
            session.server_version = Some(version.to_string());
        }
        if let Err(e) = crate::keychain::save_sessions(&self.sessions) {
            tracing::warn!(
                error = %e,
                "failed to persist refreshed server version to Keychain"
            );
        }
        cx.notify();
    }

    /// Rebuild only the library that is visibly open. Keeping a previous
    /// `LibraryState` cached while navigating Home/Detail is useful, but it
    /// must not turn every mirror update into off-screen DTO parsing.
    /// Skipped while any bulk library pass is still streaming (`is_syncing`);
    /// the final post-flag Idle is the one that runs the settled rebuild.
    pub(crate) fn refresh_active_library(&mut self, cx: &mut Context<Self>) {
        {
            let Some(state) = self.main_state() else {
                return;
            };
            if state.mirror.is_syncing() {
                return;
            }
        }
        self.spawn_library_rebuild(cx);
    }

    /// Kick off an off-thread rebuild of the visibly-open library (rows +
    /// derived metadata), applied by `apply_library_rebuild`. At most one
    /// runs at a time; a change landing meanwhile sets a dirty flag that
    /// reruns once the current one applies. The GPUI thread only ever swaps
    /// the finished product in, avoiding a multi-hundred-ms frame stall at
    /// 8k items from parsing every row's DTO on the render thread.
    pub(crate) fn spawn_library_rebuild(&mut self, cx: &mut Context<Self>) {
        let (mirror, view, sort) = {
            let Some(state) = self.main_state_mut() else {
                return;
            };
            let View::Library { view_id } = &state.nav.current else {
                return;
            };
            let Some(lib) = &state.library else {
                return;
            };
            if view_id != &lib.view_id {
                return;
            }
            if state.library_rebuild_in_flight {
                state.library_rebuild_dirty = true;
                return;
            }
            state.library_rebuild_in_flight = true;
            (state.mirror.clone(), lib.view_id.clone(), lib.sort)
        };
        // Root-owned (not MainState-owned) sequence: it survives server
        // switches, so a rebuild spawned against a torn-down MainState can
        // never pass for the current lineage's in-flight one.
        self.library_rebuild_seq += 1;
        let my_seq = self.library_rebuild_seq;
        let runtime = self.runtime.clone();
        cx.spawn(async move |this, cx| {
            let result = runtime
                .spawn_blocking(move || compute_library_rebuild(&mirror, view, sort))
                .await
                .ok()
                .flatten();
            let _ = this.update(cx, |root, cx| {
                root.apply_library_rebuild(my_seq, result, cx);
            });
        })
        .detach();
    }

    fn apply_library_rebuild(
        &mut self,
        seq: u64,
        result: Option<LibraryRebuild>,
        cx: &mut Context<Self>,
    ) {
        if seq != self.library_rebuild_seq {
            // A newer spawn superseded this result; that lineage owns the
            // flags now.
            return;
        }
        let mut dirty = false;
        if let Screen::Main(state) = &mut self.screen {
            state.library_rebuild_in_flight = false;
            dirty = std::mem::take(&mut state.library_rebuild_dirty);
            if let Some(rebuild) = result {
                if let Some(lib) = &mut state.library {
                    let still_current = lib.view_id == rebuild.view_id
                        && lib.sort == rebuild.sort
                        && matches!(&state.nav.current, View::Library { view_id } if *view_id == lib.view_id);
                    if still_current {
                        // Never assign `items` directly -- `apply_library_filters` owns that.
                        lib.raw_items = Rc::new(rebuild.rows);
                        apply_library_metadata(lib, rebuild.metadata);
                        apply_library_filters(lib);
                    }
                }
            }
            // A `None` result means the row query failed: keep the last good grid.
        }
        if dirty {
            self.spawn_library_rebuild(cx);
        }
        cx.notify();
    }

    /// docs/UX-SPEC.md §6: subscribes to the active session's `EventBus` (via
    /// `EventBusHandle::subscribe`, independent of the mirror's own
    /// receiver) to drive the offline banner directly off real
    /// connection-state transitions rather than inferring "offline" from
    /// whichever request fails next. `NeedsReconcile`/`Server(..)` are
    /// ignored here -- `Mirror`'s own sync engine handles those.
    pub(crate) fn spawn_bus_listener(&self, cx: &mut Context<Self>) {
        let Some(state) = self.main_state() else {
            return;
        };
        let mut rx = state._bus_handle.subscribe();
        cx.spawn(async move |this, cx| {
            while let Some(event) = jellyfin_core::recv_bus(&mut rx).await {
                if this
                    .update(cx, |root, cx| root.on_bus_event(event, cx))
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
    }

    fn on_bus_event(&mut self, event: jellyfin_core::BusEvent, cx: &mut Context<Self>) {
        let Some(state) = self.main_state_mut() else {
            return;
        };
        match event {
            jellyfin_core::BusEvent::Disconnected => {
                if !state.offline {
                    tracing::info!(
                        "event bus: Disconnected -- showing the offline banner \
                         (docs/UX-SPEC.md §6)"
                    );
                }
                state.offline = true;
                cx.notify();
            }
            jellyfin_core::BusEvent::Connected => {
                // `Connected` is the "reachable again" signal; `NeedsReconcile`
                // (sent alongside it) is what heals the mirror's data.
                if state.offline {
                    tracing::info!("event bus: Connected -- clearing the offline banner");
                }
                state.offline = false;
                cx.notify();
                // The server being reachable is also the moment its DNS
                // answer is most likely both present and correct. Cheap and
                // change-gated (see `persist_dns_seed`).
                self.persist_dns_seed();
                // Every reconnect re-runs auth, so this is also a
                // version-gate refresh point: a server upgrade mid-session
                // should be picked up promptly, not just at next sign-in.
                self.spawn_server_version_refresh(cx);
            }
            jellyfin_core::BusEvent::Server(_) | jellyfin_core::BusEvent::NeedsReconcile => {}
        }
    }

    /// Called by the `player.events()` subscriber task (spawned once in
    /// `main.rs`) on every `PlayerEvent::Position`/`PauseChanged` -- keeps
    /// the `ReportingSession`'s authoritative position/pause state current
    /// so periodic progress reports (and a later Esc-triggered stop) are
    /// accurate.
    pub(crate) fn on_position(&mut self, ticks: i64, cx: &mut Context<Self>) {
        // Throttled optimistic mirror progress update (see
        // `MIRROR_PROGRESS_INTERVAL`'s doc comment) -- computed while
        // `state` is borrowed below, applied after via `self.runtime` since
        // `Mirror::apply_local_user_data` is async.
        let mut mirror_progress: Option<(Mirror, String, i64)> = None;
        if let Screen::Main(state) = &mut self.screen {
            if let Some(reporting) = state.reporting.as_mut() {
                reporting.on_position(ticks);
            }
            if let Some(ui) = &mut state.player_ui {
                ui.position_secs = ticks as f64 / 10_000_000.0;
                // The loading overlay's reason to exist is "gone by the
                // real first frame" -- this is that frame.
                // Cheap to set unconditionally every tick rather than
                // guarding on `ui.loading` first (it's already `false` for
                // the ~4Hz remainder of the session).
                ui.loading = false;
                if ui.last_mirror_progress_at.elapsed()
                    >= crate::player_ui::MIRROR_PROGRESS_INTERVAL
                {
                    ui.last_mirror_progress_at = Instant::now();
                    mirror_progress = Some((state.mirror.clone(), ui.item_id.clone(), ticks));
                }
            }
            // Latency instrumentation: the first `Position` event after
            // a `play_item` completes the
            // `play_item -> PlaybackInfo -> decide -> Player::load ->
            // FileLoaded -> first Position` pipeline
            // -- log one summary line and clear `pending_latency` so later
            // Position events (there are ~4/sec for the rest of the
            // session) don't re-log it.
            if let Some((lat, loaded_ms)) = state.pending_latency.take() {
                let first_position_ms = lat.t0.elapsed().as_millis() as u64;
                tracing::info!(
                    playback_info_ms = lat.playback_info_ms,
                    decide_ms = lat.decide_ms,
                    pre_load_ms = lat.pre_load_ms,
                    ?loaded_ms,
                    first_position_ms,
                    "JELLYBEAM_LATENCY: play_item -> PlaybackInfo -> decide -> Player::load -> FileLoaded -> first Position"
                );
            }
        }
        if let Some((mirror, item_id, ticks)) = mirror_progress {
            self.runtime.spawn(async move {
                mirror.apply_local_user_data(&item_id, ticks, None).await;
            });
        }
        self.publish_now_playing(cx);
        // `AutoSkip`-configured segments seek past themselves the instant
        // playback enters them -- same ~4Hz tick, checked before the
        // next-episode affordance below.
        self.tick_auto_skip(cx);
        // "Play next episode" affordance, checked on the same ~4Hz tick
        // already driving the OSD (no new player-side event required).
        self.tick_next_episode(cx);
        cx.notify();
    }

    /// Real mpv EOF (`PlayerEvent::EndOfFile`, `main.rs`'s events task) --
    /// marks the just-finished item played in the mirror immediately, the
    /// same way the "at EOF" report to the server does, rather than waiting
    /// on a `UserDataChanged` push that doesn't reliably arrive for this
    /// client's own session. A no-op if nothing's playing.
    pub(crate) fn on_end_of_file(&mut self, cx: &mut Context<Self>) {
        let Some(state) = self.main_state() else {
            return;
        };
        let Some(ui) = &state.player_ui else {
            return;
        };
        let mirror = state.mirror.clone();
        let item_id = ui.item_id.clone();
        // Prefer the player's own duration (the true end-of-media tick
        // count) over `position_secs`, which can lag slightly behind actual
        // EOF at the ~4Hz `Position` polling cadence.
        let ticks = if ui.duration_secs > 0.0 {
            (ui.duration_secs * 10_000_000.0) as i64
        } else {
            (ui.position_secs * 10_000_000.0) as i64
        };
        self.runtime.spawn(async move {
            mirror
                .apply_local_user_data(&item_id, ticks, Some(true))
                .await;
        });
        // The Up Next card/auto-advance is normally armed by
        // `tick_next_episode` on Position events approaching the end, but an
        // outro AutoSkip that seeks straight into EOF can reach it without a
        // Position event landing in the arming window, leaving keep-open
        // paused forever with no card. EOF itself must therefore drive the
        // same advance: autoplay on plays the next episode now, autoplay
        // off arms the static card. `next_episode_dismissed` is respected.
        self.advance_next_episode_at_eof(cx);
        cx.notify();
    }

    /// See `on_end_of_file`'s comment. Split out for readability only.
    fn advance_next_episode_at_eof(&mut self, cx: &mut Context<Self>) {
        let Some(state) = self.main_state() else {
            return;
        };
        // Resurrection guard: mpv's EndOfFile event can arrive AFTER the
        // session it belongs to was already stopped (escape-stop right at
        // EOF) -- `player_ui`'s stale at-end position/duration would
        // otherwise sail through the position check below and start
        // playback the user just ended. Outside `Playing` there is nothing
        // to advance FROM; same "no-op outside Playing" rule
        // `handle_remote_command` follows.
        if !matches!(state.mode, ContentMode::Playing { .. }) {
            return;
        }
        let Some(ui) = &state.player_ui else {
            return;
        };
        if ui.next_episode_dismissed {
            return;
        }
        // Excludes a LATE EOF from a just-superseded file acting on the
        // fresh session. Those can only arrive during the new session's
        // load window (mpv emits the old file's teardown before the new
        // file starts delivering frames), which `ui.loading` covers exactly
        // -- a position-proximity check is unreliable since an outro
        // AutoSkip can reach EOF before any post-seek Position event lands.
        if ui.loading {
            return;
        }
        let autoplay_enabled = ui.autoplay_prefs.enabled;
        let next = ui.next_episode.clone().or_else(|| {
            let (Some(series_id), Some(season_id)) = (ui.series_id.clone(), ui.season_id.clone())
            else {
                return None;
            };
            adjacent_episode(
                &state.mirror,
                &series_id,
                &season_id,
                &ui.item_id,
                EpisodeStep::Next,
            )
        });
        let Some(next) = next else {
            return;
        };
        if autoplay_enabled {
            tracing::info!(next = %next.name, "EOF: auto-advancing to next episode");
            self.play_item(next.id, next.name, cx);
        } else {
            let Some(state) = self.main_state_mut() else {
                return;
            };
            if let Some(ui) = &mut state.player_ui {
                if ui.next_episode.is_none() {
                    ui.next_episode = Some(next);
                    ui.next_episode_shown_at = Some(Instant::now());
                    ui.next_episode_countdown_total_secs = None;
                }
            }
            cx.notify();
        }
    }

    pub(crate) fn on_pause_changed(&mut self, paused: bool, cx: &mut Context<Self>) {
        if let Screen::Main(state) = &mut self.screen {
            state.paused = paused;
            self.note_pause_for_next_episode(paused, cx);
            let Screen::Main(state) = &mut self.screen else {
                return;
            };
            // Hold the display-sleep assertion only while actually playing.
            // This bridge fires for every pause flip regardless of cause
            // (UI or mpv itself), so it's the one place that keeps the
            // guard honest. Gated on a live session so a stray event can't
            // take an assertion with nothing playing.
            state.display_sleep = if !paused && state.player_ui.is_some() {
                crate::power::DisplaySleepGuard::new()
            } else {
                None
            };
        }
        self.publish_now_playing(cx);
        cx.notify();
    }

    /// `PlayerEvent::Buffering` bridge (`main.rs`'s events task): mirrors
    /// mpv's live buffering state onto `PlayerUiState` so `render_playing`
    /// can show a "Buffering... n%" affordance both before the first frame
    /// and for a mid-play stall. `active == false` clears it regardless of
    /// `percent`.
    pub(crate) fn on_buffering(&mut self, active: bool, percent: f64, cx: &mut Context<Self>) {
        let Some(state) = self.main_state_mut() else {
            return;
        };
        let Some(ui) = &mut state.player_ui else {
            return;
        };
        ui.buffering_percent = active.then_some(percent);
        cx.notify();
    }

    /// `PlayerEvent::Loaded` bridge (`main.rs`'s events task): duration +
    /// the initial track list. Applies any persisted per-series track
    /// preference (docs/UX-SPEC.md §5) once the real track ids are known -- mpv
    /// track ids aren't predictable ahead of load, so this can only happen
    /// here, not at `play_item` time.
    pub(crate) fn on_loaded(
        &mut self,
        duration_secs: f64,
        tracks: Vec<player::Track>,
        cx: &mut Context<Self>,
    ) {
        let Some(state) = self.main_state_mut() else {
            return;
        };
        // A dark preload's `Loaded` -- there's no live session to apply it
        // to (every branch below is `player_ui`-gated), so stash it on the
        // preload for `promote_preload` to replay. Guarded to truly-idle so
        // a cold load's own `Loaded` can never be misfiled: the cold path
        // cleared `preload` before its `loadfile` was issued.
        if state.player_ui.is_none() && state.playing_item_id.is_none() {
            if let Some(p) = &mut state.preload {
                p.loaded = Some((duration_secs, tracks.clone()));
            }
        }
        // The cold path's own version of that stash (see `pending_loaded`'s
        // doc comment): a `Loaded` landing before `handle_playback_outcome`
        // installs the new `PlayerUiState` has nothing to apply itself to,
        // and every branch below is `player_ui`-gated.
        if state.player_ui.is_none() && state.playing_item_id.is_some() {
            state.pending_loaded = Some((
                state.playback_generation.load(Ordering::Relaxed),
                duration_secs,
                tracks.clone(),
            ));
        }
        if let Some(ui) = &mut state.player_ui {
            if duration_secs.is_finite() {
                ui.duration_secs = duration_secs;
            }
            ui.tracks = tracks;
        }
        // mpv `FileLoaded` checkpoint -- see `on_position`'s doc comment for
        // where this gets logged.
        if let Some((lat, loaded_ms)) = &mut state.pending_latency {
            if loaded_ms.is_none() {
                *loaded_ms = Some(lat.t0.elapsed().as_millis() as u64);
            }
        }
        self.apply_track_prefs(cx);
        cx.notify();
    }

    pub(crate) fn on_tracks_changed(&mut self, tracks: Vec<player::Track>, cx: &mut Context<Self>) {
        let Some(state) = self.main_state_mut() else {
            return;
        };
        if let Some(ui) = &mut state.player_ui {
            ui.tracks = tracks;
        }
        cx.notify();
    }

    /// Applies the persisted per-series audio/subtitle choice if there is
    /// one (matches on `Track::lang` first, falling back to `Track::title`
    /// -- mpv track ids/order aren't stable across items, so neither is a
    /// usable persistence key), otherwise falls back to the global
    /// `AppSettings::language` preference/subtitle-mode, covering a
    /// series' first episode (or a movie) where per-series memory can't
    /// help yet. The decision logic is the pure, unit-tested
    /// `player_ui::resolve_track_selection`; this method is just the
    /// "gather the inputs, issue the mpv commands" glue around it.
    pub(crate) fn apply_track_prefs(&mut self, _cx: &mut Context<Self>) {
        let video = self.video.clone();
        let Some(state) = self.main_state() else {
            return;
        };
        let Some(ui) = &state.player_ui else { return };
        let series_pref = ui
            .series_id
            .as_ref()
            .and_then(|id| state.track_prefs.get(id))
            .cloned();
        let decision = crate::player_ui::resolve_track_selection(
            &ui.tracks,
            series_pref.as_ref(),
            &self.app_settings.language,
        );
        if let Some(id) = decision.audio {
            let _ = video.player().set_track(player::TrackKind::Audio, Some(id));
        }
        match decision.subtitle {
            crate::player_ui::SubtitleDecision::Leave => {}
            crate::player_ui::SubtitleDecision::Off => {
                let _ = video.player().set_track(player::TrackKind::Subtitle, None);
            }
            crate::player_ui::SubtitleDecision::Track(id) => {
                let _ = video
                    .player()
                    .set_track(player::TrackKind::Subtitle, Some(id));
            }
        }
    }

    /// Republishes `MPNowPlayingInfoCenter` state (title/elapsed/duration/
    /// rate) -- see `now_playing.rs`. A no-op whenever nothing's playing.
    pub(crate) fn publish_now_playing(&mut self, _cx: &mut Context<Self>) {
        let Some(state) = self.main_state() else {
            return;
        };
        let Some(ui) = &state.player_ui else { return };
        let ContentMode::Playing { title, .. } = &state.mode else {
            return;
        };
        if let Some(np) = &self.now_playing {
            np.publish(crate::now_playing::NowPlayingState {
                title: title.clone(),
                elapsed_secs: ui.position_secs,
                duration_secs: ui.duration_secs,
                paused: state.paused,
            });
        }
    }

    // ---- Navigation -------------------------------------------------

    /// docs/UX-SPEC.md §3: navigating away during playback must transition
    /// Fullscreen-in-window -> Miniplayer, not block navigation. Called at
    /// the top of every nav entry point below: collapses to the Miniplayer
    /// first (a no-op if already there, in `Browse`, or nothing is
    /// playing). Native OS-Fullscreen is left via `want_exit_os_fullscreen`
    /// rather than `Window::toggle_fullscreen` directly, so this fn stays
    /// callable from nav entry points with no `&mut Window` in scope.
    pub(crate) fn collapse_fullscreen_player_for_nav(&mut self, cx: &mut Context<Self>) {
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
        if matches!(ui.layer_mode, crate::gl_video::LayerMode::Miniplayer(_)) {
            return;
        }
        ui.layer_mode =
            crate::gl_video::LayerMode::Miniplayer(crate::gl_video::Corner::BottomRight);
        ui.note_activity();
        video.set_layer_mode(ui.layer_mode);
        state.want_exit_os_fullscreen = true;
        cx.notify();
    }

    /// Single source of truth for whether/how the sidebar shows -- see
    /// `SidebarMode`'s doc comment for what each variant means. `&self`-only
    /// and `Window`-read-only (never mutates), so it's safe to call from
    /// `render_main` (already mid-render) and equally from `JELLYBEAM_E2E`'s
    /// assertions against the *real* window state, with no risk of drift.
    pub(crate) fn sidebar_mode(&self, window: &Window) -> SidebarMode {
        let Some(state) = self.main_state() else {
            return SidebarMode::Shown;
        };
        if window.is_fullscreen() {
            return SidebarMode::Hidden;
        }
        let fullscreen_in_window_playback = matches!(state.mode, ContentMode::Playing { .. })
            && matches!(
                state.player_ui.as_ref().map(|ui| ui.layer_mode),
                Some(crate::gl_video::LayerMode::FullscreenInWindow)
            );
        if fullscreen_in_window_playback {
            return SidebarMode::Hidden;
        }
        // Below the collapse breakpoint, the sidebar stays live in-flow
        // chrome but narrows to an icon rail rather than ever rendering a
        // partially-clipped full sidebar.
        if f64::from(window.viewport_size().width) < crate::gl_video::SIDEBAR_COLLAPSE_BREAKPOINT {
            SidebarMode::Collapsed
        } else {
            SidebarMode::Shown
        }
    }

    pub(crate) fn go_home(&mut self, cx: &mut Context<Self>) {
        self.collapse_fullscreen_player_for_nav(cx);
        let Some(state) = self.main_state_mut() else {
            return;
        };
        state.nav.go(View::Home);
        cx.notify();
    }

    /// Navigates to whichever library `AppSettings::startup_screen`
    /// currently resolves to, or does nothing when it resolves to `Home`
    /// (`Nav::new()` already starts every fresh `MainState` there). Called
    /// exactly once per fresh connect from `handle_connect_outcome`, after
    /// `state.views` is populated.
    pub(crate) fn apply_startup_screen(&mut self, cx: &mut Context<Self>) {
        let Some(state) = self.main_state() else {
            return;
        };
        // Resolved against THIS session's actual library list -- a stored
        // library id that no longer exists falls back to `Home`.
        let target = self.app_settings.startup_screen(&state.views);
        if let crate::settings::StartupScreen::Library(view_id) = target {
            self.open_library(view_id, cx);
        }
    }

    pub(crate) fn open_library(&mut self, view_id: String, cx: &mut Context<Self>) {
        self.collapse_fullscreen_player_for_nav(cx);
        let Some(state) = self.main_state_mut() else {
            return;
        };
        state.nav.go(View::Library { view_id });
        self.ensure_view_loaded(cx);
        cx.notify();
    }

    /// docs/PLUGIN-CHANNELS.md §2.1/§2.2: sidebar entry
    /// point for a `ViewKind::Channel` view -- opens level 1 (the view
    /// itself, its `ChannelFolderItem` folders), never the mirror-backed
    /// grid `open_library` targets.
    pub(crate) fn open_channel_view(&mut self, view_id: String, cx: &mut Context<Self>) {
        self.collapse_fullscreen_player_for_nav(cx);
        let Some(state) = self.main_state_mut() else {
            return;
        };
        state.nav.go(View::Channel {
            view_id,
            folder_id: None,
        });
        self.ensure_view_loaded(cx);
        cx.notify();
    }

    /// §2.2: opens a `ChannelFolderItem` row's own listing (level 2) --
    /// `channel_browse.rs`'s folder-row click/Return, never a detail page.
    /// A no-op if the current nav target somehow isn't a Channel view.
    pub(crate) fn open_channel_folder(&mut self, folder_id: String, cx: &mut Context<Self>) {
        self.collapse_fullscreen_player_for_nav(cx);
        let Some(state) = self.main_state_mut() else {
            return;
        };
        let View::Channel { view_id, .. } = state.nav.current.clone() else {
            return;
        };
        state.nav.go(View::Channel {
            view_id,
            folder_id: Some(folder_id),
        });
        self.ensure_view_loaded(cx);
        cx.notify();
    }

    pub(crate) fn open_detail(&mut self, item_id: String, cx: &mut Context<Self>) {
        self.open_detail_at_season(item_id, None, cx);
    }

    /// OSD breadcrumb click -> series Detail, pre-selected to the season the
    /// currently-playing episode belongs to (falls back to season 0 if
    /// `season_number` is `None` or matches no loaded season). `open_detail`
    /// above is just this with `None`. Zero-network path (§2.7):
    /// `DetailState::load` paints seasons/episodes straight from the
    /// mirror, so selecting a season here is a synchronous lookup.
    pub(crate) fn open_detail_at_season(
        &mut self,
        item_id: String,
        season_number: Option<i32>,
        cx: &mut Context<Self>,
    ) {
        let Some(state) = self.main_state_mut() else {
            return;
        };
        state.nav.go(View::Detail {
            item_id: item_id.clone(),
        });
        self.ensure_view_loaded(cx);
        self.spawn_detail_enrichment(item_id.clone(), cx);
        self.spawn_similar(item_id.clone(), cx);
        self.spawn_stream_warm(item_id.clone(), cx);
        // The user is one click from Play -- open the play target as a dark
        // paused preload so that click starts in tens of ms. Opening a
        // Detail page is a deliberate navigation, so it may replace an
        // existing preload immediately.
        self.preload_item(item_id, PreloadTrigger::Deliberate, cx);
        if let Some(season_number) = season_number {
            let Some(state) = self.main_state_mut() else {
                return;
            };
            let target_ix = state.detail.as_ref().and_then(|d| {
                d.seasons
                    .iter()
                    .position(|s| s.index_number == Some(season_number))
            });
            if let Some(ix) = target_ix {
                self.select_season(ix, cx);
            }
        }
        cx.notify();
    }

    /// docs/DESIGN-PLAYER-NAV.md Part 2: switches the Detail page from one
    /// episode to a sibling *in place* -- rail click/Return, or
    /// `browse_adjacent_episode`'s `[`/`]` keys. Uses `Nav::replace` (not
    /// `Nav::go`) so clicking through several sibling episodes doesn't
    /// pollute Back with every one visited -- Back should return to
    /// wherever the viewer navigated *into* the episode from. Otherwise
    /// identical to `open_detail`.
    pub(crate) fn open_episode_in_place(&mut self, item_id: String, cx: &mut Context<Self>) {
        let Some(state) = self.main_state_mut() else {
            return;
        };
        state.nav.replace(View::Detail {
            item_id: item_id.clone(),
        });
        self.ensure_view_loaded(cx);
        self.spawn_detail_enrichment(item_id.clone(), cx);
        self.spawn_similar(item_id.clone(), cx);
        self.spawn_stream_warm(item_id.clone(), cx);
        // Same one-click-from-Play reasoning as `open_detail_at_season` --
        // also a deliberate navigation.
        self.preload_item(item_id, PreloadTrigger::Deliberate, cx);
        cx.notify();
    }

    /// The Episode Detail page's own prev/next buttons and the `[`/`]`
    /// keyboard shortcuts (wired alongside the identical bindings
    /// `play_adjacent_episode` uses during playback). A no-op when the
    /// current Detail page isn't an Episode, its DTO hasn't loaded yet, or
    /// it has no known series/season context (or no adjacent episode).
    /// Reuses `adjacent_episode`, the same mirror-only lookup
    /// `play_adjacent_episode` uses during playback, so the two "prev/next
    /// episode" affordances can never disagree about what "adjacent" means.
    pub(crate) fn browse_adjacent_episode(&mut self, step: EpisodeStep, cx: &mut Context<Self>) {
        let Some(state) = self.main_state() else {
            return;
        };
        let Some(detail) = &state.detail else {
            return;
        };
        if !detail.is_episode {
            return;
        }
        let Some(dto) = &detail.dto else {
            return;
        };
        let (Some(series_id), Some(season_id)) = (
            dto.series_id.map(|u| u.to_string()),
            dto.season_id.map(|u| u.to_string()),
        ) else {
            return;
        };
        let item_id = detail.item_id.clone();
        let mirror = state.mirror.clone();
        let Some(target) = adjacent_episode(&mirror, &series_id, &season_id, &item_id, step) else {
            return;
        };
        self.open_episode_in_place(target.id, cx);
    }

    /// Rebuilds whichever of `state.library`/`state.detail` `nav.current`
    /// now points at, but only if it isn't already loaded for that id --
    /// preserves scroll/focus when back/forward returns to a view that was
    /// never replaced (docs/UX-SPEC.md §1: "Back/forward history per pane").
    pub(crate) fn ensure_view_loaded(&mut self, cx: &mut Context<Self>) {
        let Screen::Main(state) = &mut self.screen else {
            return;
        };
        match state.nav.current.clone() {
            View::Home => {}
            View::Library { view_id } => {
                let same = state
                    .library
                    .as_ref()
                    .map(|l| l.view_id == view_id)
                    .unwrap_or(false);
                if same {
                    // Reopening the cached grid: mirror changes that landed
                    // while it was off-screen were deliberately not applied
                    // then, so reconcile now, off-thread. The cached state
                    // still paints immediately.
                    self.spawn_library_rebuild(cx);
                    return;
                }
                // The persisted per-library mode is read exactly here -- the
                // one place a library is opened fresh.
                let view_mode = self.app_settings.library_view_mode(&view_id);
                state.library = Some(build_library_state(
                    &state.mirror,
                    view_id,
                    Sort::NameAsc,
                    view_mode,
                ));
            }
            View::Detail { item_id } => {
                let same = state
                    .detail
                    .as_ref()
                    .map(|d| d.item_id == item_id)
                    .unwrap_or(false);
                if !same {
                    let mirror = state.mirror.clone();
                    state.detail = Some(DetailState::load(&mirror, item_id));
                }
            }
            View::Channel { view_id, folder_id } => {
                let same = state
                    .channel_browse
                    .as_ref()
                    .is_some_and(|c| c.is_for(&view_id, folder_id.as_deref()));
                if !same {
                    let view_name = state
                        .views
                        .iter()
                        .find(|v| v.id == view_id)
                        .map(|v| v.name.clone())
                        .unwrap_or_default();
                    // Best-effort: the clicked folder's own name, read off
                    // whatever level-1 listing was still on screen at click
                    // time -- can miss (falls back to `view_name`) on a
                    // `Nav::forward()` redo.
                    let folder_name = folder_id.as_deref().and_then(|fid| {
                        state.channel_browse.as_ref().and_then(|old| {
                            old.items
                                .iter()
                                .find(|dto| dto.id.map(|u| u.to_string()).as_deref() == Some(fid))
                                .and_then(|dto| dto.name.clone())
                        })
                    });
                    state.channel_browse = Some(crate::channel_browse::ChannelBrowseState::new(
                        view_id,
                        view_name,
                        folder_id,
                        folder_name,
                    ));
                }
                // §2.2: unlike Library above, a Channel level ALWAYS
                // re-queries live here, since there is no mirror change
                // event to react to instead.
                self.spawn_channel_page(0, cx);
                return;
            }
            View::Discover(view) => {
                if state.discover.is_none() {
                    state.discover = Some(Box::new(discover::DiscoverState::new(cx)));
                }
                self.sync_discover_session(cx);
                self.dispatch_discover_view(view, cx);
                return;
            }
        }
        let _ = cx;
    }

    fn nav_back(&mut self, cx: &mut Context<Self>) {
        self.collapse_fullscreen_player_for_nav(cx);
        let moved = if let Screen::Main(state) = &mut self.screen {
            state.nav.back()
        } else {
            false
        };
        if moved {
            self.ensure_view_loaded(cx);
            cx.notify();
        }
    }

    fn nav_forward(&mut self, cx: &mut Context<Self>) {
        self.collapse_fullscreen_player_for_nav(cx);
        let moved = if let Screen::Main(state) = &mut self.screen {
            state.nav.forward()
        } else {
            false
        };
        if moved {
            self.ensure_view_loaded(cx);
            cx.notify();
        }
    }

    fn sidebar_jump(&mut self, n: usize, cx: &mut Context<Self>) {
        if n == 1 {
            self.go_home(cx);
            return;
        }
        let view_id = if let Screen::Main(state) = &self.screen {
            state.views.get(n - 2).map(|v| v.id.clone())
        } else {
            None
        };
        if let Some(id) = view_id {
            self.open_library(id, cx);
        }
    }

    // --- Library filter/sort toolbar ---------------------------------

    pub(crate) fn set_library_sort(&mut self, sort: media_cache::Sort, cx: &mut Context<Self>) {
        let Some(state) = self.main_state_mut() else {
            return;
        };
        let Some(lib) = &mut state.library else {
            return;
        };
        let view_id = lib.view_id.clone();
        // Re-fetch from the mirror at the new sort order (cheap, indexed)
        // rather than re-sorting `raw_items` client-side, so the sort
        // semantics match exactly whatever `Mirror::children` implements.
        let mut fresh = build_library_state(&state.mirror, view_id, sort, lib.view_mode);
        fresh.unwatched_only = lib.unwatched_only;
        fresh.genre = lib.genre.clone();
        apply_library_filters(&mut fresh);
        state.library = Some(fresh);
        cx.notify();
    }

    pub(crate) fn toggle_library_unwatched_only(&mut self, cx: &mut Context<Self>) {
        let Some(state) = self.main_state_mut() else {
            return;
        };
        let Some(lib) = &mut state.library else {
            return;
        };
        lib.unwatched_only = !lib.unwatched_only;
        apply_library_filters(lib);
        cx.notify();
    }

    /// Filter popover trigger, hosting both the Unwatched toggle and the
    /// genre list in one panel.
    pub(crate) fn toggle_library_filter_menu(&mut self, cx: &mut Context<Self>) {
        let Some(state) = self.main_state_mut() else {
            return;
        };
        let Some(lib) = &mut state.library else {
            return;
        };
        lib.filter_menu_open = !lib.filter_menu_open;
        lib.sort_menu_open = false;
        cx.notify();
    }

    pub(crate) fn close_library_filter_menu(&mut self, cx: &mut Context<Self>) {
        let Some(state) = self.main_state_mut() else {
            return;
        };
        let Some(lib) = &mut state.library else {
            return;
        };
        lib.filter_menu_open = false;
        cx.notify();
    }

    pub(crate) fn close_library_sort_menu(&mut self, cx: &mut Context<Self>) {
        let Some(state) = self.main_state_mut() else {
            return;
        };
        let Some(lib) = &mut state.library else {
            return;
        };
        lib.sort_menu_open = false;
        cx.notify();
    }

    /// Sort popover trigger.
    pub(crate) fn toggle_library_sort_menu(&mut self, cx: &mut Context<Self>) {
        let Some(state) = self.main_state_mut() else {
            return;
        };
        let Some(lib) = &mut state.library else {
            return;
        };
        lib.sort_menu_open = !lib.sort_menu_open;
        lib.filter_menu_open = false;
        cx.notify();
    }

    /// Grid/List toggle. A pure projection switch: `items`/`raw_items` and
    /// every derived map stay exactly as they are, so this never refetches,
    /// re-sorts or re-filters -- the two modes render from the same
    /// already-filtered slice. Persisted per library id (see
    /// [`crate::settings::LibraryViewMode`]). Focus is preserved by index;
    /// `render_main` re-derives `focus.columns` for the mode being entered
    /// on the next frame.
    pub(crate) fn set_library_view_mode(&mut self, mode: LibraryViewMode, cx: &mut Context<Self>) {
        let Some(state) = self.main_state_mut() else {
            return;
        };
        let Some(lib) = &mut state.library else {
            return;
        };
        if lib.view_mode == mode {
            return;
        }
        lib.view_mode = mode;
        let view_id = lib.view_id.clone();
        self.app_settings.set_library_view_mode(&view_id, mode);
        cx.notify();
    }

    /// Persists the new Next Up prefs, pushes them into the mirror, and asks
    /// for an immediate re-fetch so the shelf reflects the change right away
    /// rather than waiting for the next opportunistic trigger. The refresh
    /// runs on `self.runtime`; its result reaches the screen through the
    /// mirror's own change feed (`on_mirror_change`), not a return value.
    pub(crate) fn set_next_up_prefs(
        &mut self,
        prefs: crate::settings::NextUpPrefs,
        cx: &mut Context<Self>,
    ) {
        self.app_settings.set_next_up(prefs);
        let Some(state) = self.main_state() else {
            cx.notify();
            return;
        };
        let mirror = state.mirror.clone();
        mirror.set_next_up_options(prefs.to_next_up_options());
        self.runtime.spawn(async move {
            mirror.refresh_next_up().await;
        });
        cx.notify();
    }

    /// Persists the per-library Home visibility toggle, then immediately
    /// rebuilds Home's shelves against the new hidden set -- waiting for the
    /// next `MirrorChange` would leave a just-hidden library's shelf on
    /// screen indefinitely.
    pub(crate) fn set_library_visible_on_home(
        &mut self,
        view_id: String,
        visible: bool,
        cx: &mut Context<Self>,
    ) {
        self.app_settings
            .set_library_visible_on_home(&view_id, visible);
        self.rebuild_home(cx);
    }

    /// Same "persist, then rebuild Home immediately" shape as
    /// `set_library_visible_on_home` just above.
    pub(crate) fn set_hide_watched_latest(&mut self, hide: bool, cx: &mut Context<Self>) {
        self.app_settings.set_hide_watched_latest(hide);
        self.rebuild_home(cx);
    }

    /// Shared tail of the two Home-filtering setters above: re-runs
    /// `HomeState::load` from scratch. A full reload (not
    /// `HomeState::refresh`'s order-preserving merge, which exists to ride
    /// out *sync* churn) is the right shape for a deliberate, one-off
    /// settings change.
    fn rebuild_home(&mut self, cx: &mut Context<Self>) {
        let hidden_libraries = self.app_settings.hidden_home_libraries();
        let hide_watched_latest = self.app_settings.hide_watched_latest;
        let Some(state) = self.main_state_mut() else {
            return;
        };
        state.home = HomeState::load(
            &state.mirror,
            &state.views,
            &hidden_libraries,
            hide_watched_latest,
        );
        cx.notify();
    }

    /// Just persists -- see `apply_startup_screen` for where the stored
    /// value takes effect (next connect).
    pub(crate) fn set_startup_screen(
        &mut self,
        screen: crate::settings::StartupScreen,
        cx: &mut Context<Self>,
    ) {
        self.app_settings.set_startup_screen(screen);
        cx.notify();
    }

    pub(crate) fn set_library_genre_filter(
        &mut self,
        genre: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let Some(state) = self.main_state_mut() else {
            return;
        };
        let Some(lib) = &mut state.library else {
            return;
        };
        lib.genre = genre;
        lib.filter_menu_open = false;
        apply_library_filters(lib);
        cx.notify();
    }

    // ---- Keyboard-first navigation (docs/UX-SPEC.md §2) ------------------------

    /// Registered once via `cx.observe_keystrokes` in `main.rs` -- fires
    /// regardless of which element currently has keyboard focus. The Search
    /// overlay has no dedicated `TextInput`, so its query text entry lives
    /// here too.
    pub(crate) fn handle_global_keystroke(
        &mut self,
        event: &gpui::KeystrokeEvent,
        window: &mut gpui::Window,
        cx: &mut Context<Self>,
    ) {
        let key = event.keystroke.key.as_str();
        let cmd = event.keystroke.modifiers.platform;
        let shift = event.keystroke.modifiers.shift;
        let other_mods = event.keystroke.modifiers.control || event.keystroke.modifiers.alt;

        // Escape backs out of an "Add server" Connect form only -- first-run/
        // sign-out screens have `can_cancel = false`. Handled before the
        // `Screen::Main` early-return below, since Connect is never Main.
        if key == "escape"
            && !cmd
            && !other_mods
            && matches!(&self.screen, Screen::Connect(s) if s.can_cancel && !s.connecting)
        {
            self.cancel_add_server(cx);
            return;
        }

        let Some(state) = self.main_state() else {
            return;
        };

        // "?" keyboard-shortcuts overlay. Checked here, before every dispatch
        // branch below, so it behaves identically in Browse, Miniplayer, and
        // full Fullscreen-in-window/OS-Fullscreen playback:
        // - **Close** (Esc, or "?" again) takes priority over every other
        //   Esc handler below (next-episode card, fullscreen step-down,
        //   Miniplayer's Esc-stops-playback, `nav_back`) while the overlay
        //   is up.
        // - **Open** (`shift + /`) is gated off contexts that treat "/" as
        //   text entry instead: the Search overlay's query field and the
        //   playback track picker's live-filter field.
        if state.shortcuts_overlay_open {
            let is_close = (key == "escape" || (key == "/" && shift)) && !cmd && !other_mods;
            if is_close {
                self.close_shortcuts_overlay(cx);
                return;
            }
        } else if key == "/" && shift && !cmd && !other_mods {
            let picker_open = matches!(&state.player_ui, Some(ui) if ui.picker.is_some());
            if !state.search.open && !picker_open {
                self.open_shortcuts_overlay(cx);
                return;
            }
        }

        // docs/UX-SPEC.md §3: collapsed to the Miniplayer, only Cmd+M/Tab (restore)
        // are special; everything else falls through to normal Browse dispatch.
        let in_fullscreen_player = matches!(state.mode, ContentMode::Playing { .. })
            && !matches!(
                state.player_ui.as_ref().map(|ui| ui.layer_mode),
                Some(crate::gl_video::LayerMode::Miniplayer(_))
            );
        let is_miniplayer =
            matches!(state.mode, ContentMode::Playing { .. }) && !in_fullscreen_player;

        if (cmd && key == "m") || (in_fullscreen_player && key == "tab") {
            self.toggle_miniplayer(cx);
            return;
        }
        if is_miniplayer && key == "tab" {
            self.toggle_miniplayer(cx);
            return;
        }
        // Layered Esc step-down: Miniplayer is the bottom layer, so Esc here
        // stops playback. Handled directly (not falling through to Browse's
        // `nav_back`) or it would navigate the view underneath instead. The
        // fullscreen-in-window/OS-fullscreen layers step down one at a time
        // in `handle_playback_keystroke` below.
        if is_miniplayer && key == "escape" {
            self.stop_playback(cx);
            return;
        }

        // docs/UX-SPEC.md §3: ⌘F/⌘[/⌘]/⌘1..9 must work from every state, including
        // Fullscreen-in-window/OS-Fullscreen playback. Handled here, before
        // the `in_fullscreen_player` short-circuit below, which would
        // otherwise route these into `handle_playback_keystroke` and
        // silently swallow them. Each nav method calls
        // `collapse_fullscreen_player_for_nav` itself, so this is correct
        // regardless of `in_fullscreen_player`'s value.
        if cmd {
            match key {
                "f" => {
                    self.open_search(cx);
                    return;
                }
                "[" => {
                    self.nav_back(cx);
                    return;
                }
                "]" => {
                    self.nav_forward(cx);
                    return;
                }
                "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" => {
                    let n: usize = key.parse().unwrap_or(1);
                    self.sidebar_jump(n, cx);
                    return;
                }
                _ => {}
            }
        }

        if in_fullscreen_player {
            self.handle_playback_keystroke(
                key,
                event.keystroke.key_char.as_deref(),
                cmd,
                shift,
                other_mods,
                window,
                cx,
            );
            return;
        }

        if state.search.open {
            self.handle_search_keystroke(key, event.keystroke.key_char.as_deref(), other_mods, cx);
            return;
        }

        match key {
            "/" if !other_mods => self.open_search(cx),
            "escape" => self.nav_back(cx),
            "left" => self.move_focus(Direction::Left, cx),
            "right" => self.move_focus(Direction::Right, cx),
            "up" => self.move_focus(Direction::Up, cx),
            "down" => self.move_focus(Direction::Down, cx),
            "enter" | "return" => self.activate_focus(cx),
            // Same `[`/`]` bindings `handle_playback_keystroke` uses for
            // episode-level prev/next during playback, now also live while
            // *browsing* the Episode Detail page -- no-ops elsewhere.
            "[" if !other_mods => self.browse_adjacent_episode(EpisodeStep::Prev, cx),
            "]" if !other_mods => self.browse_adjacent_episode(EpisodeStep::Next, cx),
            _ => {}
        }
    }

    /// `now_playing.rs`'s bridge target -- Control Center / physical media
    /// keys drive the same `Player` calls the in-app OSD's keyboard
    /// shortcuts do. A no-op outside `ContentMode::Playing` (e.g. a stray
    /// command that arrives right as playback stops).
    pub(crate) fn handle_remote_command(
        &mut self,
        cmd: crate::now_playing::RemoteCommand,
        cx: &mut Context<Self>,
    ) {
        use crate::now_playing::RemoteCommand;
        // Debug breadcrumb: log every command MediaRemote hands us, to
        // distinguish an OS-originated pause from app logic.
        tracing::info!(?cmd, "remote command received");
        let video = self.video.clone();
        let Some(state) = self.main_state_mut() else {
            return;
        };
        if !matches!(state.mode, ContentMode::Playing { .. }) {
            return;
        }
        match cmd {
            RemoteCommand::Play => self.set_paused(false, cx),
            RemoteCommand::Pause => self.set_paused(true, cx),
            RemoteCommand::Toggle => {
                let now_paused = !state.paused;
                self.set_paused(now_paused, cx);
            }
            RemoteCommand::SeekRelative(delta) => {
                let _ = video.player().seek_relative(delta);
            }
            RemoteCommand::SeekAbsolute(secs) => {
                let _ = video.player().seek_absolute(secs);
            }
        }
        self.publish_now_playing(cx);
    }
}

/// Return/click on the Detail page's currently focused area. On the Episode
/// Detail page, `DetailArea::Episodes` is the sibling rail, not a series'
/// per-season episode grid -- Return there switches to that sibling's page
/// in place (`SwitchEpisode`); Return on the *series* page's episode grid
/// is `OpenDetail`, matching that grid's click handler.
/// Which trigger requested a preload -- distinguishes a deliberate,
/// already-considered action from a bare hover dwell, which needs its own
/// throttle. See `decide_preload_retarget`'s doc comment for the policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PreloadTrigger {
    /// A 350ms hover dwell (`cards.rs`'s dwell timer) landed on a different
    /// item than the preload slot currently represents. The mouse resting
    /// somewhere is a weak, easily-reversed signal -- it may retarget only
    /// after `Root::HOVER_RETARGET_THROTTLE` of staying the most recent
    /// hover.
    Hover,
    /// Everything else: a Detail page opening, or the once-per-session
    /// automatic hero preload making its one attempt. Each is an
    /// already-deliberate user action or a one-shot system decision, so
    /// both may replace an existing preload immediately.
    Deliberate,
}

/// Pure decision for whether a preload request for `requested_target`
/// should replace whatever the preload slot (`current_target`) holds right
/// now. Extracted out of `Root::preload_item` so the throttle policy has
/// direct unit-test coverage without spinning up GPUI/mpv.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PreloadRetargetDecision {
    /// Nothing occupies the slot, or the request already matches what does
    /// -- start (or leave alone) exactly as before this throttle existed.
    ProceedImmediately,
    /// A different target occupies the slot and this is a deliberate
    /// request -- discard and replace right now.
    ReplaceImmediately,
    /// A different target occupies the slot and this is a hover dwell --
    /// leave the existing preload alone for now; the caller schedules the
    /// replace for later, cancelled if anything supersedes it first.
    ScheduleReplace,
}

pub(crate) fn decide_preload_retarget(
    current_target: Option<&str>,
    requested_target: &str,
    trigger: PreloadTrigger,
) -> PreloadRetargetDecision {
    match current_target {
        None => PreloadRetargetDecision::ProceedImmediately,
        Some(current) if current == requested_target => PreloadRetargetDecision::ProceedImmediately,
        Some(_) => match trigger {
            PreloadTrigger::Deliberate => PreloadRetargetDecision::ReplaceImmediately,
            PreloadTrigger::Hover => PreloadRetargetDecision::ScheduleReplace,
        },
    }
}

/// What would actually play if the user hit Play on `item_id` right now --
/// a Movie/Episode is itself, a Series resolves to its next-up episode via
/// the exact logic `play_item_inner`'s series branch uses. Mirror-only and
/// synchronous, so preload triggers can call it from any UI path. `None`
/// for virtual/unaired items and non-playable container types.
pub(crate) fn resolve_play_target(state: &MainState, item_id: &str) -> Option<(String, String)> {
    let dto = state.mirror.item(item_id)?;
    if dto.location_type == Some(LocationType::Virtual) {
        return None;
    }
    match dto.type_ {
        Some(BaseItemKind::Movie) | Some(BaseItemKind::Episode) => {
            Some((item_id.to_string(), dto.name.clone().unwrap_or_default()))
        }
        Some(BaseItemKind::Series) => {
            let seasons = state.mirror.children(item_id, Sort::IndexNumber, 0, 100);
            let ep = crate::detail::find_series_next_episode(&state.mirror, &seasons)?;
            Some((ep.id, ep.name))
        }
        _ => None,
    }
}

pub(crate) fn detail_activate_action(d: &DetailState) -> Option<Activate> {
    use crate::detail::DetailArea;
    match d.area {
        DetailArea::Play => {
            let name = d
                .dto
                .as_ref()
                .and_then(|dto| dto.name.clone())
                .unwrap_or_default();
            Some(Activate::PlayItem(d.item_id.clone(), name))
        }
        DetailArea::Seasons => None,
        DetailArea::Episodes => d.focused_episode().map(|c| {
            if d.is_episode {
                Activate::SwitchEpisode(c.id.clone())
            } else {
                Activate::OpenDetail(c.id.clone())
            }
        }),
    }
}

fn field_label(label: &'static str) -> impl IntoElement {
    div()
        .text_color(rgba(theme::TEXT_TERTIARY))
        .text_sm()
        .mt_1()
        .child(label)
}

impl Render for Root {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if let Some(log) = &self.perf_frame_log {
            log.borrow_mut().push(Instant::now());
        }
        let body = match &self.screen {
            Screen::Connect(_) => self.render_connect(window, cx).into_any_element(),
            Screen::Main(_) => self.render_main(window, cx).into_any_element(),
            // Brief transitional state during a server switch -- see
            // `Screen::Switching`'s doc comment.
            Screen::Switching => div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .bg(rgb(theme::SURFACE_BASE))
                .text_color(rgba(theme::TEXT_PRIMARY))
                .child("Switching server…")
                .into_any_element(),
        };
        // Brand §3: Archivo on every interface surface. Set exactly once,
        // here on the render root, so it cascades to every screen, popover
        // and overlay. Overridden only where §3 carves out `FONT_MONO`
        // (spec strips/shortcuts/status) and `FONT_DISPLAY` ("Jellybeam").
        div()
            .size_full()
            .relative()
            .font_family(theme::FONT_UI)
            .child(body)
    }
}

impl Root {
    fn render_connect(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Screen::Connect(state) = &mut self.screen else {
            unreachable!("render_connect only called when self.screen is Connect")
        };
        // Autofocus the first empty field the first time this screen
        // renders (guarded by `autofocused` so it doesn't keep stealing
        // focus back on every subsequent render).
        if !state.autofocused {
            state.autofocused = true;
            let first_empty = if state.server.read(cx).content.is_empty() {
                Some(state.server.clone())
            } else if state.username.read(cx).content.is_empty() {
                Some(state.username.clone())
            } else if state.password.read(cx).content.is_empty() {
                Some(state.password.clone())
            } else {
                None
            };
            if let Some(field) = first_empty {
                field.read(cx).focus(window);
            }
        }
        let server = state.server.clone();
        let username = state.username.clone();
        let password = state.password.clone();
        let connect_button_focus = state.connect_button_focus.clone();
        let status = state.status.clone();
        let error_hint = state.error_hint.clone();
        let connecting = state.connecting;
        let quick_connect = state.quick_connect;
        let qc_code = state.qc_code.clone();
        let can_cancel = state.can_cancel;
        let connect_focused = connect_button_focus.is_focused(window);
        let root_weak = cx.entity().downgrade();

        // Back-out affordance for "Add server" -- resumes the still-active
        // session. Secondary (not Ghost): a real action, just not primary.
        // Escape does the same via `handle_global_keystroke`.
        let cancel_button = can_cancel.then(|| {
            button(
                "connect-cancel",
                "Cancel",
                ButtonVariant::Secondary,
                ButtonSize::Lg,
                connecting,
            )
            .mt_1()
            .w_full()
            .on_click(cx.listener(|this, _event, _window, cx| {
                this.cancel_add_server(cx);
            }))
        });

        // docs/UX-SPEC.md §6 "First-run": "server URL + credentials (Quick Connect
        // added in v1)" -- the toggle sits right under the server field
        // (shared by both paths) and swaps everything below it between the
        // username/password fields and the Quick Connect code flow.
        let qc_toggle_root = root_weak.clone();
        let qc_toggle_row = div()
            .id("quick-connect-toggle")
            .mt_1()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .cursor_pointer()
            .child(toggle_switch("quick-connect-switch", quick_connect))
            .child(
                div()
                    .text_sm()
                    .text_color(rgba(theme::TEXT_SECONDARY))
                    .child("Use Quick Connect"),
            )
            .on_click(move |_event, _window, cx| {
                let _ = qc_toggle_root.update(cx, |root, cx| root.toggle_quick_connect(cx));
            });

        let lower_section = if quick_connect {
            let code_display = qc_code.unwrap_or_else(|| "------".to_string());
            let retry_root = root_weak.clone();
            div()
                .flex()
                .flex_col()
                .gap_2()
                .mt_2()
                // DESIGN-GUIDE.md §A.7's pairing-screen pose, on NOTTE
                // only -- there's no artwork on this screen to sit over.
                .child(
                    div()
                        .flex()
                        .justify_center()
                        .child(mascot_image(
                            "brand/jellybeam/jb_mascot_watching.png",
                            px(96.),
                        )),
                )
                .child(
                    div()
                        .id("quick-connect-code")
                        .flex()
                        .items_center()
                        .justify_center()
                        .h(px(56.))
                        .rounded_md()
                        .bg(rgb(theme::SURFACE_BASE))
                        .text_color(rgba(theme::TEXT_PRIMARY))
                        .text_2xl()
                        .child(SharedString::from(code_display)),
                )
                .child(
                    div()
                        .text_sm()
                        .text_color(rgba(theme::TEXT_TERTIARY))
                        .child("Enter this code in Quick Connect on another signed-in device or the server's web UI."),
                )
                .child(
                    button(
                        "quick-connect-retry",
                        "Request a new code",
                        ButtonVariant::Secondary,
                        ButtonSize::Md,
                        false,
                    )
                    .mt_1()
                    .w_full()
                    .on_click(move |_event, _window, cx| {
                        let _ = retry_root.update(cx, |root, cx| root.start_quick_connect(cx));
                    }),
                )
                .into_any_element()
        } else {
            div()
                .flex()
                .flex_col()
                .gap_1()
                .child(field_label("Username"))
                .child(username)
                .child(field_label("Password"))
                .child(password)
                .child(
                    // Primary/`lg` button via `ui::components::button`, with
                    // the extra keyboard-Tab-focus border chained on top
                    // (`connect_button_focus` -- the generic component only
                    // knows hover, not a `FocusHandle`-driven focus ring).
                    button(
                        "connect-button",
                        if connecting {
                            "Connecting..."
                        } else {
                            "Connect"
                        },
                        ButtonVariant::Primary,
                        ButtonSize::Lg,
                        connecting,
                    )
                    .track_focus(&connect_button_focus)
                    .relative()
                    .mt_3()
                    .w_full()
                    // The button is a Tab stop, so it needs the same focus
                    // ring every other focusable thing in the app gets:
                    // `ui::components::focus_ring`, layout-neutral (an
                    // absolutely positioned overlay outside the pill's box).
                    .when(connect_focused, |d| {
                        d.child(crate::ui::components::focus_ring(theme::RADIUS_PILL))
                    })
                    .on_click(cx.listener(|this, _event, _window, cx| {
                        this.on_connect_clicked(cx);
                    }))
                    // Return submits when focus has tabbed onto the button
                    // itself, not just from a text field's own `on_enter`.
                    .on_key_down(cx.listener(
                        |this, event: &KeyDownEvent, _window, cx| {
                            if matches!(event.keystroke.key.as_str(), "enter" | "return") {
                                this.on_connect_clicked(cx);
                            }
                        },
                    )),
                )
                .into_any_element()
        };

        div()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .bg(rgb(theme::SURFACE_BASE))
            // Tab/Shift+Tab cycle Server URL -> Username -> Password ->
            // Connect -> wrap. `ConnectFocusNext`/`Prev` are bound globally
            // in `main.rs`, but only this subtree has listeners for them.
            .on_action(cx.listener(Self::on_connect_focus_next))
            .on_action(cx.listener(Self::on_connect_focus_prev))
            .child(
                div()
                    .w(px(360.))
                    .flex()
                    .flex_col()
                    .gap_1()
                    .p_6()
                    .rounded_lg()
                    .bg(rgb(theme::SURFACE_RAISED))
                    .child(
                        // The app's one identity moment before login: the
                        // same mascot-plus-wordmark lockup the sidebar
                        // header carries.
                        div().mb(theme::SPACE_LOOSE).child(brand_lockup()),
                    )
                    .child(field_label("Server URL"))
                    .child(server)
                    .child(qc_toggle_row)
                    .child(lower_section)
                    .children(cancel_button)
                    .child(
                        div()
                            .mt_2()
                            .flex()
                            .flex_col()
                            .gap_1()
                            // Brand §5: an error state is "Archivo, factual,
                            // one line" -- primary-weight warm text
                            // (`theme::DANGER`), the sentence carries the signal.
                            .child(div().text_color(rgb(theme::DANGER)).text_sm().child(status))
                            .children(error_hint.map(|hint| {
                                div()
                                    .text_color(rgba(theme::TEXT_TERTIARY))
                                    .text_xs()
                                    .child(hint)
                            })),
                    ),
            )
    }

    fn on_connect_focus_next(
        &mut self,
        _: &ConnectFocusNext,
        window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
        window.focus_next();
    }

    fn on_connect_focus_prev(
        &mut self,
        _: &ConnectFocusPrev,
        window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
        window.focus_prev();
    }

    fn render_main(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Recompute the Library grid's column count against the current
        // window width before rendering anything -- keeps `GridFocus`'s
        // up/down math correct across resizes without a separate resize
        // observer. Computed off whichever sidebar width is actually about
        // to render (full/rail/none).
        let sidebar_mode = self.sidebar_mode(window);
        let content_width =
            (window.viewport_size().width - px(sidebar_mode.width_px() as f32) - px(48.))
                .max(px(0.));
        let columns = crate::grid::columns_for_width(content_width);
        // The Library wall runs its own (flex-width, 7-at-the-reference-width)
        // column math, while `columns` above stays the fixed-`CELL_WIDTH`
        // answer `detail.rs`'s season/episode grids lay
        // out against -- see `grid::library_columns_for_width`.
        let library_columns = crate::grid::library_columns_for_width(content_width);
        let video_for_fullscreen = self.video.clone();
        if let Screen::Main(state) = &mut self.screen {
            if let Some(lib) = &mut state.library {
                // The list view is a ONE-column grid as far as focus
                // arithmetic is concerned: Up/Down become ±1, `row()`
                // becomes the item index itself, and Left/Right degrade to
                // the same ±1 step -- one focus model for both projections.
                lib.focus.columns = match lib.view_mode {
                    LibraryViewMode::Grid => library_columns,
                    LibraryViewMode::List => 1,
                };
                lib.focus.clamp(lib.items.len());
            }
            // `detail.episode_focus.columns` must stay synced to the episode
            // grid's actual rendered column count, or `GridFocus::row()`
            // treats every index as its own row and Left/Right scrolls the
            // whole page instead of moving within a row.
            if let Some(detail) = &mut state.detail {
                detail.episode_focus.columns = columns;
                detail.episode_focus.clamp(detail.episodes.len());
            }
            // `collapse_fullscreen_player_for_nav` can't call
            // `Window::toggle_fullscreen` itself -- most callers have no
            // `&mut Window` in scope -- so it leaves the request here for
            // `render_main`, which always has one, to carry out once.
            if state.want_exit_os_fullscreen {
                state.want_exit_os_fullscreen = false;
                if window.is_fullscreen() {
                    window.toggle_fullscreen();
                }
            }
            // Same shape, but for the OSD's fullscreen icon button.
            if state.want_toggle_os_fullscreen {
                state.want_toggle_os_fullscreen = false;
                window.toggle_fullscreen();
                if let Some(ui) = &mut state.player_ui {
                    ui.layer_mode = crate::gl_video::LayerMode::FullscreenInWindow;
                    video_for_fullscreen.set_layer_mode(ui.layer_mode);
                }
            }
        }
        // The OSD fullscreen icon button needs the *current* state, which
        // only `Window` knows -- sampled once here, after the toggle above
        // ran for this frame, threaded down through the render calls below.
        let is_fullscreen = window.is_fullscreen();

        let viewport = window.viewport_size();
        // Sampled fresh every render (cheap mpv property reads).
        let live_info = crate::player_ui::PlayerLiveInfo {
            hwdec: self.video.player().hwdec_current(),
            video_bitrate_bps: self.video.player().video_bitrate(),
            audio_bitrate_bps: self.video.player().audio_bitrate(),
            container_fps: self.video.player().container_fps(),
            video_params: self.video.player().video_params(),
            frame_drops: self.video.player().frame_drop_stats(),
            cache: self.video.player().cache_state(),
        };

        let Some(state) = self.main_state() else {
            unreachable!("render_main only called when self.screen is Main")
        };

        let root_weak = cx.entity().downgrade();

        // Library names are server-configured labels and must be displayed
        // verbatim. Home remains the one client-owned fixed label.
        let sidebar_rows = std::iter::once((
            "__home".to_string(),
            "Home".to_string(),
            media_cache::ViewKind::Library,
        ))
        .chain(
            state
                .views
                .iter()
                .map(|v| (v.id.clone(), v.name.clone(), v.kind)),
        )
        .enumerate()
        .map(|(ix, (id, raw_name, kind))| {
            let selected = match &state.nav.current {
                View::Home => id == "__home",
                View::Library { view_id } => view_id == &id,
                // A Channel view's sidebar row is selected while browsing
                // either of its levels -- both share the same `view_id`.
                View::Channel { view_id, .. } => view_id == &id,
                View::Detail { .. } | View::Discover(_) => false,
            };
            let is_home = id == "__home";
            let is_channel = kind == media_cache::ViewKind::Channel;
            let display_name = raw_name.clone();
            let root = root_weak.clone();
            // A library type with no vendored icon (Music, Photos, ...)
            // renders no icon slot rather than a placeholder.
            let icon_path = if is_home {
                Some("icons/layout-grid.svg")
            } else {
                library_icon_path(&raw_name)
            };
            let row_content = div()
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .flex_1()
                .min_w_0()
                .children(icon_path.map(|path| {
                    svg()
                        .path(path)
                        .w(px(16.))
                        .h(px(16.))
                        .flex_shrink_0()
                        .text_color(if selected {
                            rgba(theme::TEXT_PRIMARY)
                        } else {
                            rgba(theme::TEXT_SYNOPSIS)
                        })
                }))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .overflow_hidden()
                        .whitespace_nowrap()
                        // `min_w_0()` above lets the flex item shrink below
                        // its text's natural width so this ellipsis can fire.
                        .text_ellipsis()
                        .text_color(if selected {
                            rgba(theme::TEXT_PRIMARY)
                        } else {
                            rgba(theme::TEXT_SYNOPSIS)
                        })
                        .child(SharedString::from(display_name.clone())),
                )
                .child(
                    div()
                        .flex_shrink_0()
                        .text_size(theme::TEXT_CAPTION)
                        .text_color(rgba(theme::TEXT_HINT))
                        .child(SharedString::from(format!("⌘{}", ix + 1))),
                );
            list_row(
                SharedString::from(format!("view-{id}")),
                selected,
                row_content,
            )
            .cursor_pointer()
            .on_click(move |_event, _window, cx| {
                let _ = root.update(cx, |root, cx| {
                    if is_home {
                        root.go_home(cx);
                    } else if is_channel {
                        root.open_channel_view(id.clone(), cx);
                    } else {
                        root.open_library(id.clone(), cx);
                    }
                });
            })
        })
        .collect::<Vec<_>>();

        // "Discover" entry, below the
        // libraries, shown only when `discover::sidebar_visible` reads
        // `self.seerr_status.configured` -- re-evaluated every render, so a
        // connect/disconnect or session swap takes effect the next paint
        // with no separate wiring. `search.svg` stands in for a dedicated
        // Discover glyph -- none exists in the vendored Lucide subset.
        let discover_row = discover::sidebar_visible(&state.seerr_status).then(|| {
            let root = root_weak.clone();
            let selected = matches!(state.nav.current, View::Discover(_));
            list_row(
                "view-discover",
                selected,
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .child(
                        svg()
                            .path("icons/search.svg")
                            .w(px(16.))
                            .h(px(16.))
                            .text_color(if selected {
                                rgba(theme::TEXT_PRIMARY)
                            } else {
                                rgba(theme::TEXT_SYNOPSIS)
                            }),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_color(if selected {
                                rgba(theme::TEXT_PRIMARY)
                            } else {
                                rgba(theme::TEXT_SYNOPSIS)
                            })
                            .child("Discover"),
                    ),
            )
            .cursor_pointer()
            .on_click(move |_event, _window, cx| {
                let _ = root.update(cx, |root, cx| root.open_discover(cx));
            })
        });

        // docs/UX-SPEC.md §1: "Back/forward history per pane (⌘[ / ⌘])" -- a small
        // clickable affordance for mouse users, dimmed when there's nowhere
        // to go (keyboard-first is the primary path; this is a convenience).
        let can_back = state.nav.can_go_back();
        let can_forward = state.nav.can_go_forward();
        let back_root = root_weak.clone();
        let forward_root = root_weak.clone();
        let history_row = div()
            .flex()
            .flex_row()
            .gap_2()
            .mb_2()
            .child(
                div()
                    .id("nav-back")
                    .flex()
                    .items_center()
                    .justify_center()
                    .size(px(28.))
                    .rounded_md()
                    .when(can_back, |d| {
                        d.cursor_pointer()
                            .hover(|s| s.bg(rgba(theme::ICON_HOVER_FILL)))
                    })
                    .child(
                        svg()
                            .path("icons/arrow-left.svg")
                            .w(px(16.))
                            .h(px(16.))
                            .text_color(if can_back {
                                rgba(theme::TEXT_SECONDARY)
                            } else {
                                rgba(theme::TEXT_QUATERNARY)
                            }),
                    )
                    .on_click(move |_event, _window, cx| {
                        let _ = back_root.update(cx, |root, cx| root.nav_back(cx));
                    }),
            )
            .child(
                div()
                    .id("nav-forward")
                    .flex()
                    .items_center()
                    .justify_center()
                    .size(px(28.))
                    .rounded_md()
                    .when(can_forward, |d| {
                        d.cursor_pointer()
                            .hover(|s| s.bg(rgba(theme::ICON_HOVER_FILL)))
                    })
                    .child(
                        svg()
                            .path("icons/arrow-right.svg")
                            .w(px(16.))
                            .h(px(16.))
                            .text_color(if can_forward {
                                rgba(theme::TEXT_SECONDARY)
                            } else {
                                rgba(theme::TEXT_QUATERNARY)
                            }),
                    )
                    .on_click(move |_event, _window, cx| {
                        let _ = forward_root.update(cx, |root, cx| root.nav_forward(cx));
                    }),
            );

        // Sidebar footer: current server/user, click opens the Settings
        // sheet (docs/UX-SPEC.md §1's Settings entry point + the multi-server/user
        // switcher, both live in `settings.rs`'s "Server & Account" section).
        let active_label = self
            .sessions
            .active_session()
            .map(|s| s.username.clone().unwrap_or_else(|| s.base_url.clone()))
            .unwrap_or_else(|| "Account".to_string());
        // The collapsed rail's own account button needs this too, cloned
        // before `active_label` is moved into the full sidebar's footer row.
        let active_label_rail = active_label.clone();
        // Server Switcher, off the sidebar account footer. A click toggles
        // the popover instead of jumping straight to Settings; the full CRUD
        // UI stays in Settings -> Server & Account as a shortcut inside the
        // popover's own footer row.
        let footer_toggle_root = root_weak.clone();
        let account_footer_trigger = div()
            .id("sidebar-account-footer")
            .px_2()
            .py_2()
            .rounded_md()
            .cursor_pointer()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .hover(|s| s.bg(rgb(theme::SURFACE_OVERLAY)))
            .child(
                svg()
                    .path("icons/circle-user.svg")
                    .w(px(16.))
                    .h(px(16.))
                    .text_color(rgba(theme::TEXT_TERTIARY)),
            )
            .child(
                // VISUAL PASS 3 §1 sweep: this label (username, or the
                // server's own base URL when no username is set) had a bare
                // `overflow_hidden()` with neither `.whitespace_nowrap()`
                // nor `.text_ellipsis()` -- with no height cap either, a
                // long value didn't clip so much as silently wrap and grow
                // the footer row taller, which reads just as wrong as a
                // mid-word cut once the value is a full URL. `clamped_line`
                // (needs its own `.flex_1().min_w_0()` per that fn's own
                // doc comment -- a flex item's default min-width is its
                // content size, which would otherwise defeat the ellipsis).
                clamped_line(active_label, px(20.))
                    .flex_1()
                    .min_w_0()
                    .text_sm()
                    .text_color(rgba(theme::TEXT_SECONDARY)),
            )
            .on_click(move |_event, _window, cx| {
                let _ = footer_toggle_root.update(cx, |root, cx| root.toggle_server_switcher(cx));
            });

        let switcher_open = state.server_switcher_open;
        let sessions_for_switcher = self.sessions.clone();
        let switcher_panel_root = root_weak.clone();
        let account_footer = popover_trigger(
            "sidebar-account-footer-popover",
            account_footer_trigger,
            switcher_open,
            // The footer sits at the bottom of the sidebar, so the popover
            // must open *upward* -- `BottomLeft` anchor pins the panel's
            // own bottom-left corner to the footer's top-left corner (via
            // the offset below), so it extends up from there.
            Corner::BottomLeft,
            point(px(0.), px(-8.)),
            move || {
                render_server_switcher_panel(&sessions_for_switcher, switcher_panel_root.clone())
            },
        );

        // Persistent status pill, fixed above the account footer regardless
        // of which view is open. Three states, error taking visual priority
        // over offline; online isn't hidden either -- a persistent-but-quiet
        // pill is more honest than a banner that vanishes and reappears with
        // no positive reconnect confirmation. Brand §5: connected is the
        // literal "6px PISTACCHIO dot plus Martian Mono 10px label" pill;
        // offline and error are the same shape with a hollow dot and a
        // warm-neutral mono line, distinguished by wording/weight rather
        // than colour (§2 forbids a second hue for status).
        let status_pill = if let Some(err) = state.error.as_ref() {
            let dismiss_root = root_weak.clone();
            div()
                .id("sidebar-status-pill")
                .flex()
                .items_center()
                .gap_2()
                .px_2()
                .py_1()
                .mb_1()
                .rounded(theme::RADIUS_PILL)
                .child(status_dot(false))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .overflow_hidden()
                        .child(status_label(
                            SharedString::from(format!("PLAYBACK ERROR — {err}")),
                            true,
                        )),
                )
                .child(
                    div()
                        .id("sidebar-status-pill-dismiss")
                        .cursor_pointer()
                        .flex_shrink_0()
                        .child(
                            // This glyph is a *control* (dismiss), not decoration.
                            svg()
                                .path("icons/x.svg")
                                .w(px(14.))
                                .h(px(14.))
                                .text_color(rgba(theme::TEXT_TERTIARY)),
                        )
                        .on_click(move |_e, _w, cx| {
                            let _ = dismiss_root.update(cx, |root, cx| root.dismiss_error(cx));
                        }),
                )
                .into_any_element()
        } else if state.offline {
            div()
                .id("sidebar-status-pill")
                .flex()
                .items_center()
                .gap_2()
                .px_2()
                .py_1()
                .mb_1()
                .rounded(theme::RADIUS_PILL)
                .child(status_dot(false))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .overflow_hidden()
                        .child(status_label(
                            SharedString::from(format!(
                                "OFFLINE — {} FROM CACHE",
                                server_host_label(&state.base_url).to_uppercase()
                            )),
                            false,
                        )),
                )
                .into_any_element()
        } else {
            div()
                .id("sidebar-status-pill")
                .flex()
                .items_center()
                .gap_2()
                .px_2()
                .py_1()
                .mb_1()
                .rounded(theme::RADIUS_PILL)
                .child(status_dot(true))
                .child(status_label("CONNECTED", false))
                .into_any_element()
        };

        // Subtle determinate progress affordance while the mirror is doing
        // a bulk library pass (initial breadth sync, reconcile mismatch
        // sweep, WS-reconnect catch-up). A separate, complementary pill
        // above the status pill -- syncing is orthogonal to
        // online/offline/error.
        let sync_pill = match state.mirror.sync_activity().borrow().clone() {
            media_cache::SyncActivity::Idle => None,
            media_cache::SyncActivity::Syncing {
                library_name_or_id,
                items_done,
                total_items,
                ..
            } => {
                let library_label = state
                    .views
                    .iter()
                    .find(|v| v.id == library_name_or_id)
                    .map(|v| v.name.clone())
                    .unwrap_or_else(|| "library".to_string());
                // `items_done`/`total_items` come straight from the breadth
                // walk; the denominator is unknown only while the first
                // page is in flight, where the bar is omitted rather than
                // faked.
                let progress = sync_progress_fraction(items_done, total_items);
                let label_text = match total_items {
                    Some(total) if total > 0 => format!(
                        "SYNCING {} — {items_done} OF {total}",
                        library_label.to_uppercase()
                    ),
                    _ => format!(
                        "SYNCING {} — {} ITEMS",
                        library_label.to_uppercase(),
                        state.mirror.item_count()
                    ),
                };
                Some(
                    div()
                        .id("sidebar-sync-pill")
                        .px_2()
                        .py_1()
                        .mb_1()
                        .rounded(theme::RADIUS_PILL)
                        .bg(rgb(theme::SURFACE_RAISED))
                        .font_family(theme::FONT_MONO)
                        .text_size(theme::TEXT_SPEC)
                        .text_color(rgb(theme::GRIGIO))
                        .overflow_hidden()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .child(SharedString::from(label_text))
                        .children(progress.map(|frac| {
                            // Track + fill via percentage WIDTH on an
                            // in-flow child (never a percentage inset, and
                            // no .relative() after .absolute(): gpui's
                            // position setters are last-write-wins).
                            div()
                                .w_full()
                                .h(px(3.))
                                .rounded_full()
                                .bg(rgba(theme::CONTROL_SELECTED_FILL))
                                .child(
                                    div()
                                        .h_full()
                                        .rounded_full()
                                        .bg(rgb(theme::ACCENT))
                                        .w(gpui::relative(frac)),
                                )
                        })),
                )
            }
        };

        let sidebar_footer = div()
            .mt_auto()
            .flex()
            .flex_col()
            .children(sync_pill)
            .child(status_pill)
            .child(account_footer);

        let sidebar = div()
            .id("sidebar")
            .w(px(crate::gl_video::SIDEBAR_WIDTH as f32))
            // A `div().w(px(N))` flex item has CSS's default `flex-shrink: 1`
            // unless told otherwise, so wide content (e.g. Detail's hero
            // block) could squeeze the sidebar narrower than its own
            // children expect, misaligning fixed-offset art. `flex_shrink_0()`
            // is the other half of the fix alongside `min_w_0()` on the
            // content pane below.
            .flex_shrink_0()
            .h_full()
            .flex()
            .flex_col()
            .px_3()
            .pb_3()
            // The title bar is `titlebarAppearsTransparent` with a
            // full-size content view, so the macOS traffic lights float
            // over this sidebar's top-left corner. Nothing interactive or
            // textual may sit under them, and the region behind them must
            // stay `SURFACE` so the lights sit on the app's own near-black.
            .pt(TRAFFIC_LIGHT_INSET)
            .gap_1()
            .bg(rgb(theme::SURFACE_RAISED))
            .child(
                // The sidebar header is the mascot product mark's home,
                // beside the wordmark -- see `brand_lockup`.
                div().mb_2().child(brand_lockup()),
            )
            .child(history_row)
            .children(sidebar_rows)
            .children(discover_row)
            .child(sidebar_footer);

        // The collapsed icon rail -- same rows, click handlers, and ⌘N
        // shortcuts as the full sidebar above, just icons + tooltips
        // instead of icons + labels. Built unconditionally and picked
        // between by the `row_sidebar` match below.
        let rail_rows = std::iter::once((
            "__home".to_string(),
            "Home".to_string(),
            media_cache::ViewKind::Library,
        ))
        .chain(
            state
                .views
                .iter()
                .map(|v| (v.id.clone(), v.name.clone(), v.kind)),
        )
        .enumerate()
        .map(|(ix, (id, raw_name, kind))| {
            let selected = match &state.nav.current {
                View::Home => id == "__home",
                View::Library { view_id } => view_id == &id,
                View::Channel { view_id, .. } => view_id == &id,
                View::Detail { .. } | View::Discover(_) => false,
            };
            let is_home = id == "__home";
            let is_channel = kind == media_cache::ViewKind::Channel;
            let display_name = raw_name.clone();
            let root = root_weak.clone();
            let tooltip_label = SharedString::from(format!("{display_name}  ⌘{}", ix + 1));
            let icon = if is_home {
                Some("icons/layout-grid.svg")
            } else {
                // A library type with no vendored icon falls back to the
                // monogram below.
                library_icon_path(&raw_name)
            };
            let icon = match icon {
                Some(path) => svg()
                    .path(path)
                    .w(px(18.))
                    .h(px(18.))
                    .text_color(if selected {
                        rgba(theme::TEXT_PRIMARY)
                    } else {
                        rgba(theme::TEXT_SECONDARY)
                    })
                    .into_any_element(),
                None => {
                    // No icon asset exists for this type -- a monogram of
                    // the library's own name is a legible, always-available
                    // stand-in.
                    let initial = display_name
                        .chars()
                        .next()
                        .map(|c| c.to_uppercase().to_string())
                        .unwrap_or_else(|| "?".to_string());
                    div()
                        .text_sm()
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(if selected {
                            rgba(theme::TEXT_PRIMARY)
                        } else {
                            rgba(theme::TEXT_SECONDARY)
                        })
                        .child(SharedString::from(initial))
                        .into_any_element()
                }
            };
            div()
                .id(SharedString::from(format!("rail-{id}")))
                .w_full()
                .h(px(40.))
                .flex()
                .items_center()
                .justify_center()
                .rounded_md()
                .cursor_pointer()
                .when(selected, |d| d.bg(rgb(theme::SURFACE_OVERLAY)))
                .hover(|s| s.bg(rgb(theme::SURFACE_OVERLAY)))
                .child(icon)
                .tooltip(tooltip_text(tooltip_label))
                .on_click(move |_event, _window, cx| {
                    let _ = root.update(cx, |root, cx| {
                        if is_home {
                            root.go_home(cx);
                        } else if is_channel {
                            root.open_channel_view(id.clone(), cx);
                        } else {
                            root.open_library(id.clone(), cx);
                        }
                    });
                })
        })
        .collect::<Vec<_>>();

        // Rail's own Discover icon -- same gating/click target as the full
        // sidebar's `discover_row` above, just icon-only per the rail's own
        // shape.
        let discover_rail_row = discover::sidebar_visible(&state.seerr_status).then(|| {
            let root = root_weak.clone();
            let selected = matches!(state.nav.current, View::Discover(_));
            div()
                .id("rail-discover")
                .w_full()
                .h(px(40.))
                .flex()
                .items_center()
                .justify_center()
                .rounded_md()
                .cursor_pointer()
                .when(selected, |d| d.bg(rgb(theme::SURFACE_OVERLAY)))
                .hover(|s| s.bg(rgb(theme::SURFACE_OVERLAY)))
                .child(
                    svg()
                        .path("icons/search.svg")
                        .w(px(18.))
                        .h(px(18.))
                        .text_color(if selected {
                            rgba(theme::TEXT_PRIMARY)
                        } else {
                            rgba(theme::TEXT_SECONDARY)
                        }),
                )
                .tooltip(tooltip_text(SharedString::from("Discover")))
                .on_click(move |_event, _window, cx| {
                    let _ = root.update(cx, |root, cx| root.open_discover(cx));
                })
        });

        // Rail's own compact account trigger -- same Server Switcher popover
        // as the full sidebar's footer, just an icon button (no room for a
        // label at 56px). Fresh clones since the full sidebar's clones were
        // already moved into `account_footer`.
        let rail_switcher_root = root_weak.clone();
        let rail_account_trigger = div()
            .id("sidebar-rail-account")
            .size(px(36.))
            .rounded_md()
            .flex()
            .items_center()
            .justify_center()
            .cursor_pointer()
            .hover(|s| s.bg(rgb(theme::SURFACE_OVERLAY)))
            .child(
                svg()
                    .path("icons/circle-user.svg")
                    .w(px(16.))
                    .h(px(16.))
                    .text_color(rgba(theme::TEXT_TERTIARY)),
            )
            .tooltip(tooltip_text(active_label_rail))
            .on_click(move |_event, _window, cx| {
                let _ = rail_switcher_root.update(cx, |root, cx| root.toggle_server_switcher(cx));
            });
        let rail_sessions_for_switcher = self.sessions.clone();
        let rail_switcher_panel_root = root_weak.clone();
        let rail_account_footer = popover_trigger(
            "sidebar-rail-account-popover",
            rail_account_trigger,
            switcher_open,
            Corner::BottomLeft,
            point(px(0.), px(-8.)),
            move || {
                render_server_switcher_panel(
                    &rail_sessions_for_switcher,
                    rail_switcher_panel_root.clone(),
                )
            },
        );

        let sidebar_rail = div()
            .id("sidebar-rail")
            .w(px(crate::gl_video::SIDEBAR_RAIL_WIDTH as f32))
            // Same fix as `sidebar` above, same reason.
            .flex_shrink_0()
            .h_full()
            .flex()
            .flex_col()
            .items_center()
            .pb_3()
            // The rail occupies the same top-left corner the traffic lights
            // float over -- same inset as the full sidebar.
            .pt(TRAFFIC_LIGHT_INSET)
            .gap_1()
            .bg(rgb(theme::SURFACE_RAISED))
            .children(rail_rows)
            .children(discover_rail_row)
            .child(div().flex_1())
            .child(rail_account_footer);

        // Sidebar placement follows `sidebar_mode` -- `Shown` is the full
        // labeled sidebar, `Collapsed` is the icon rail, `Hidden` renders
        // neither.
        let row_sidebar = match sidebar_mode {
            SidebarMode::Shown => Some(sidebar.into_any_element()),
            SidebarMode::Collapsed => Some(sidebar_rail.into_any_element()),
            SidebarMode::Hidden => None,
        };

        let content = render_content(
            state,
            library_columns,
            content_width,
            viewport,
            live_info,
            is_fullscreen,
            cx,
        );
        let search_overlay = state
            .search
            .open
            .then(|| crate::search::render(&state.search, root_weak.clone(), cx));
        let settings_overlay = state.settings.open.then(|| {
            crate::settings::render(
                &state.settings,
                &self.sessions,
                &self.app_settings,
                &state.base_url,
                &state.views,
                viewport.height,
                &state.seerr_status,
                &state.discover_connect,
                root_weak.clone(),
                cx,
            )
        });
        // docs/UX-SPEC.md §3: the Miniplayer's hover-OSD lives at the *window*
        // level (not nested inside `content`) so its rect lines up with the
        // real video NSView's frame, positioned relative to the whole
        // content view, not the sidebar-offset browse pane.
        let miniplayer_overlay = state.player_ui.as_ref().and_then(|ui| {
            matches!(ui.layer_mode, crate::gl_video::LayerMode::Miniplayer(_)).then(|| {
                crate::player_ui::render_miniplayer(ui, state.paused, viewport, root_weak.clone())
            })
        });
        // "?" keyboard-shortcuts overlay -- mounted last among the
        // window-level overlays below so it paints on top of everything,
        // including a playing Miniplayer.
        let shortcuts_overlay = state
            .shortcuts_overlay_open
            .then(|| crate::shortcuts_overlay::render(viewport.height, root_weak.clone()));

        // The window is `WindowBackgroundAppearance::Transparent` (`main.rs`)
        // so the embedded mpv video NSView shows through unpainted regions
        // during Fullscreen-in-window playback. This flex row backstops the
        // whole window as an opaque background otherwise; it must stay OFF
        // exactly when that playback needs this pane transparent for the
        // video cutout. Miniplayer keeps it on (its own smaller CALayer-masked
        // hole is unrelated to this pane's paint).
        let needs_video_transparency = matches!(state.mode, ContentMode::Playing { .. })
            && !matches!(
                state.player_ui.as_ref().map(|ui| ui.layer_mode),
                Some(crate::gl_video::LayerMode::Miniplayer(_))
            );

        div()
            .size_full()
            .relative()
            .child(
                div()
                    .size_full()
                    .flex()
                    .flex_row()
                    .when(!needs_video_transparency, |d| {
                        d.bg(rgb(theme::SURFACE_BASE))
                    })
                    .children(row_sidebar)
                    // `content` must be sized as "the remaining space next
                    // to the sidebar's fixed width", not a sibling also
                    // claiming 100% of the row's width -- `flex_1()` makes
                    // it claim exactly the row's remaining main-axis space.
                    // `min_w_0()` is the horizontal counterpart to `min_h_0()`:
                    // without it a wide Detail page could force this box
                    // wider than "viewport minus sidebar", shrinking the
                    // sidebar sibling instead. With both, the content pane
                    // can be smaller than its children's intrinsic size and
                    // the sidebar never moves.
                    .child(
                        div()
                            .flex_1()
                            .min_h_0()
                            .min_w_0()
                            // `min_w_0` lets this box be SMALLER than its
                            // children, but GPUI only masks child PAINT when
                            // overflow is hidden -- without this, an
                            // oversized Detail backdrop still rasterizes
                            // over the sidebar painted before it.
                            .overflow_hidden()
                            .size_full()
                            .child(content),
                    )
                    .children(search_overlay)
                    .children(settings_overlay),
            )
            .children(miniplayer_overlay)
            .children(shortcuts_overlay)
            // Click-away catcher for the Server Switcher popover -- mounted
            // here (this outer div is already `.size_full()`); the panel
            // itself is still nested at the trigger site in `account_footer`.
            .when(switcher_open, |d| {
                let close_root = root_weak.clone();
                d.child(click_away_catcher("server-switcher-catcher", move |cx| {
                    let _ = close_root.update(cx, |root, cx| root.close_server_switcher(cx));
                }))
            })
    }
}

/// Right-of-sidebar content area. In `Browse` mode this shows whichever
/// view `nav.current` points at, opaque. In `Playing` mode while NOT
/// collapsed to the Miniplayer, it's mostly transparent so the embedded mpv
/// video NSView shows through except the OSD bar. Collapsed to the
/// Miniplayer, this renders ordinary opaque Browse UI (docs/UX-SPEC.md §3) -- the
/// video shows through the small Miniplayer rect via a CALayer mask on
/// GPUI's content view, not because anything here leaves it unpainted.
fn render_content(
    state: &MainState,
    library_columns: usize,
    content_width: gpui::Pixels,
    viewport: gpui::Size<gpui::Pixels>,
    live_info: crate::player_ui::PlayerLiveInfo,
    is_fullscreen: bool,
    cx: &mut Context<Root>,
) -> impl IntoElement {
    let is_miniplayer = matches!(
        state.player_ui.as_ref().map(|ui| ui.layer_mode),
        Some(crate::gl_video::LayerMode::Miniplayer(_))
    );
    match &state.mode {
        ContentMode::Browse => {
            render_browse(state, library_columns, content_width, viewport, cx).into_any_element()
        }
        ContentMode::Loading { title } => {
            // Stage text driven live by `loading_stage_rx` -- falls back to
            // "Contacting server..." if the channel isn't there (shouldn't
            // happen) rather than panicking.
            let stage = state
                .loading_stage_rx
                .as_ref()
                .map(|rx| *rx.borrow())
                .unwrap_or(crate::playback::LoadStage::ContactingServer);
            div()
                .size_full()
                .relative()
                .bg(rgb(theme::SURFACE_BASE))
                .child(crate::player_ui::render_loading_overlay(
                    &format!("{title} — {}", stage.label()),
                    None,
                ))
                .into_any_element()
        }
        ContentMode::Playing { .. } if is_miniplayer => {
            render_browse(state, library_columns, content_width, viewport, cx).into_any_element()
        }
        ContentMode::Playing { .. } => {
            let Some(ui) = &state.player_ui else {
                return div().size_full().into_any_element();
            };
            let root_weak = cx.entity().downgrade();
            crate::player_ui::render_playing(
                ui,
                state.paused,
                viewport,
                live_info,
                is_fullscreen,
                &state.image_store,
                root_weak,
                cx,
            )
            .into_any_element()
        }
    }
}

/// Library toolbar: Sort popover (exactly `media_cache::Sort`'s three
/// variants), Filter popover (Unwatched toggle + genre list from
/// `LibraryState::genres`), and a Grid/List view toggle pinned right. Built
/// entirely from the shared popover primitive.
fn library_toolbar(lib: &LibraryState, root_weak: gpui::WeakEntity<Root>) -> impl IntoElement {
    let sort_label = match lib.sort {
        media_cache::Sort::NameAsc => "Name",
        media_cache::Sort::DateCreatedDesc => "Date Added",
        media_cache::Sort::PremiereDateDesc => "Premiere",
        _ => "Name",
    };
    let sort_trigger_root = root_weak.clone();
    let sort_trigger = div()
        .id("sort-trigger")
        .flex()
        .items_center()
        .gap_1()
        .h(px(36.))
        .px(px(16.))
        // Brand §5: "Buttons -- fully rounded, 999px." This is a
        // Secondary button in all but name (bordered, transparent-ish,
        // beside a Primary), so it takes the same pill and the same
        // transparent-plus-hairline treatment `ui::components::button`'s
        // `Secondary` variant now uses.
        .rounded(theme::RADIUS_PILL)
        .cursor_pointer()
        .bg(rgba(theme::TRANSPARENT))
        .border_1()
        .border_color(rgb(theme::SURFACE_HAIRLINE))
        .text_sm()
        .text_color(rgba(theme::TEXT_PRIMARY))
        .hover(|s| s.bg(rgb(theme::SURFACE_OVERLAY)))
        .child(
            svg()
                .path("icons/arrow-up-down.svg")
                .w(px(14.))
                .h(px(14.))
                .text_color(rgba(theme::TEXT_TERTIARY)),
        )
        .child(SharedString::from(format!("Sort: {sort_label}")))
        .child(
            svg()
                .path("icons/chevron-down.svg")
                .w(px(12.))
                .h(px(12.))
                .text_color(rgba(theme::TEXT_TERTIARY)),
        )
        .on_click(move |_event, _window, cx| {
            let _ = sort_trigger_root.update(cx, |root, cx| root.toggle_library_sort_menu(cx));
        });

    let sort_options = [
        ("Name", media_cache::Sort::NameAsc),
        ("Date Added", media_cache::Sort::DateCreatedDesc),
        ("Premiere", media_cache::Sort::PremiereDateDesc),
    ];
    let sort_menu_open = lib.sort_menu_open;
    let lib_sort = lib.sort;
    let sort_popover_root = root_weak.clone();
    let sort_button = popover_trigger(
        "library-sort-popover",
        sort_trigger,
        sort_menu_open,
        Corner::TopLeft,
        point(px(0.), px(8.)),
        move || {
            let rows = sort_options
                .into_iter()
                .map(|(label, sort)| {
                    let active = std::mem::discriminant(&lib_sort) == std::mem::discriminant(&sort);
                    let row_root = sort_popover_root.clone();
                    popover_row(
                        SharedString::from(format!("sort-opt-{label}")),
                        active,
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .w_full()
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(rgba(theme::TEXT_PRIMARY))
                                    .child(label),
                            )
                            // `TEXT_PRIMARY` rather than accent: amber is
                            // reserved for the one primary action, not a
                            // control-state checkmark.
                            .children(active.then(|| {
                                svg()
                                    .path("icons/check.svg")
                                    .w(px(14.))
                                    .h(px(14.))
                                    .text_color(rgba(theme::TEXT_PRIMARY))
                            })),
                    )
                    .on_click(move |_e, _w, cx| {
                        let _ = row_root.update(cx, |root, cx| root.set_library_sort(sort, cx));
                    })
                    .into_any_element()
                })
                .collect::<Vec<_>>();
            popover_panel(
                "sort-popover-panel",
                px(240.),
                div().flex().flex_col().children(rows),
            )
        },
    );

    let filter_count = lib.unwatched_only as usize + lib.genre.is_some() as usize;
    let filter_label = if filter_count > 0 {
        format!("Filter ({filter_count})")
    } else {
        "Filter".to_string()
    };
    let filter_trigger_root = root_weak.clone();
    let filter_active = filter_count > 0;
    let filter_trigger = div()
        .id("filter-trigger")
        .flex()
        .items_center()
        .gap_1()
        .h(px(36.))
        .px(px(16.))
        // Brand §5: "Buttons -- fully rounded, 999px." A Secondary button
        // in all but name, so it takes `ui::components::button`'s
        // `Secondary` transparent-plus-hairline treatment.
        .rounded(theme::RADIUS_PILL)
        .cursor_pointer()
        // "A filter is active" is a control state, not an indicator: the
        // accent is reserved for Play/Resume, so this uses the same neutral
        // "selected chip" grammar every other selected control uses.
        .bg(if filter_active {
            rgba(theme::CONTROL_SELECTED_FILL)
        } else {
            rgba(theme::TRANSPARENT)
        })
        .border_1()
        .border_color(if filter_active {
            rgba(theme::CONTROL_SELECTED_TOP_HIGHLIGHT)
        } else {
            rgb(theme::SURFACE_HAIRLINE)
        })
        .text_sm()
        .text_color(rgba(theme::TEXT_PRIMARY))
        .hover(|s| s.bg(rgb(theme::SURFACE_OVERLAY)))
        .child(
            svg()
                .path("icons/list-filter.svg")
                .w(px(14.))
                .h(px(14.))
                .text_color(rgba(theme::TEXT_TERTIARY)),
        )
        .child(SharedString::from(filter_label))
        .child(
            svg()
                .path("icons/chevron-down.svg")
                .w(px(12.))
                .h(px(12.))
                .text_color(rgba(theme::TEXT_TERTIARY)),
        )
        .on_click(move |_event, _window, cx| {
            let _ = filter_trigger_root.update(cx, |root, cx| root.toggle_library_filter_menu(cx));
        });

    let filter_menu_open = lib.filter_menu_open;
    let unwatched_only = lib.unwatched_only;
    let lib_genre = lib.genre.clone();
    let lib_genres = lib.genres.clone();
    let filter_popover_root = root_weak.clone();
    let filter_button = popover_trigger(
        "library-filter-popover",
        filter_trigger,
        filter_menu_open,
        Corner::TopLeft,
        point(px(0.), px(8.)),
        move || {
            let unwatched_root = filter_popover_root.clone();
            let unwatched_row = popover_row(
                "filter-unwatched",
                unwatched_only,
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .w_full()
                    .child(
                        div()
                            .text_sm()
                            .text_color(rgba(theme::TEXT_PRIMARY))
                            .child("Unwatched only"),
                    )
                    .children(unwatched_only.then(|| {
                        svg()
                            .path("icons/check.svg")
                            .w(px(14.))
                            .h(px(14.))
                            .text_color(rgba(theme::TEXT_PRIMARY))
                    })),
            )
            .on_click(move |_e, _w, cx| {
                let _ =
                    unwatched_root.update(cx, |root, cx| root.toggle_library_unwatched_only(cx));
            });

            let all_root = filter_popover_root.clone();
            let all_active = lib_genre.is_none();
            let all_row = popover_row(
                "genre-all",
                all_active,
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .w_full()
                    .child(
                        div()
                            .text_sm()
                            .text_color(rgba(theme::TEXT_PRIMARY))
                            .child("All Genres"),
                    )
                    .children(all_active.then(|| {
                        svg()
                            .path("icons/check.svg")
                            .w(px(14.))
                            .h(px(14.))
                            .text_color(rgba(theme::TEXT_PRIMARY))
                    })),
            )
            .on_click(move |_e, _w, cx| {
                let _ = all_root.update(cx, |root, cx| root.set_library_genre_filter(None, cx));
            });

            let genre_rows = lib_genres.iter().map(|genre| {
                let active = lib_genre.as_deref() == Some(genre.as_str());
                let pick_root = filter_popover_root.clone();
                let genre_for_click = genre.clone();
                popover_row(
                    SharedString::from(format!("genre-{genre}")),
                    active,
                    div()
                        .flex()
                        .items_center()
                        .justify_between()
                        .w_full()
                        .child(
                            div()
                                .text_sm()
                                .text_color(rgba(theme::TEXT_PRIMARY))
                                .child(SharedString::from(genre.clone())),
                        )
                        .children(active.then(|| {
                            svg()
                                .path("icons/check.svg")
                                .w(px(14.))
                                .h(px(14.))
                                .text_color(rgba(theme::TEXT_PRIMARY))
                        })),
                )
                .on_click(move |_e, _w, cx| {
                    let _ = pick_root.update(cx, |root, cx| {
                        root.set_library_genre_filter(Some(genre_for_click.clone()), cx)
                    });
                })
                .into_any_element()
            });

            popover_panel(
                "filter-popover-panel",
                px(360.),
                div()
                    .flex()
                    .flex_col()
                    .child(unwatched_row)
                    .child(div().my_1().h(px(1.)).bg(rgb(theme::SURFACE_HAIRLINE)))
                    .child(dense_label("Genre"))
                    .child(all_row)
                    .children(genre_rows),
            )
        },
    );

    // Grid/List view toggle, mutually exclusive active state. "Which view
    // mode is active" is a control state, not the primary-action amber:
    // active reads as the same neutral selected-chip grammar every other
    // selected control uses; inactive lifts to the same fill on hover.
    let view_mode = lib.view_mode;
    let view_toggle_button = |id: &'static str,
                              icon: &'static str,
                              mode: LibraryViewMode,
                              root_weak: gpui::WeakEntity<Root>| {
        let active = view_mode == mode;
        div()
            .id(id)
            .flex()
            .items_center()
            .justify_center()
            .size(px(32.))
            .rounded_md()
            .cursor_pointer()
            .when(active, |d| d.bg(rgb(theme::SURFACE_OVERLAY)))
            .when(!active, |d| d.hover(|s| s.bg(rgb(theme::SURFACE_OVERLAY))))
            .child(
                svg()
                    .path(icon)
                    .w(px(16.))
                    .h(px(16.))
                    .text_color(rgba(if active {
                        theme::TEXT_PRIMARY
                    } else {
                        theme::TEXT_TERTIARY
                    })),
            )
            .on_click(move |_event, _window, cx| {
                let _ = root_weak.update(cx, |root, cx| root.set_library_view_mode(mode, cx));
            })
    };
    let view_toggle = div()
        .flex()
        .items_center()
        .gap_1()
        .ml_auto()
        .child(view_toggle_button(
            "library-view-grid",
            "icons/layout-grid.svg",
            LibraryViewMode::Grid,
            root_weak.clone(),
        ))
        .child(view_toggle_button(
            "library-view-list",
            "icons/list.svg",
            LibraryViewMode::List,
            root_weak.clone(),
        ));

    div()
        .flex()
        .flex_row()
        .items_center()
        .gap_2()
        .px(theme::SPACE_SECTION)
        .py_2()
        .child(sort_button)
        .child(filter_button)
        .child(
            div()
                .ml_2()
                .text_sm()
                .text_color(rgba(theme::TEXT_TERTIARY))
                .child(format!("{} items", lib.items.len())),
        )
        .child(view_toggle)
}

/// A short, human-friendly server label for the offline pill -- strips the
/// scheme and any path/query. Falls back to the raw `base_url` unchanged if
/// it doesn't look like a URL at all.
/// Server Switcher panel content: one row per stored session, active
/// session gets a `check.svg` instead of a "Switch" button. The footer
/// "Settings" row opens the full Settings sheet pre-navigated to Server &
/// Account; deliberately no "About" row (reached from the native menu).
fn render_server_switcher_panel(
    sessions: &crate::keychain::StoredSessionList,
    root: gpui::WeakEntity<Root>,
) -> impl IntoElement {
    let rows = sessions
        .sessions
        .iter()
        .enumerate()
        .map(|(ix, session)| {
            let active = ix == sessions.active;
            let label = session
                .username
                .clone()
                .map(|u| format!("{u} — {}", server_host_label(&session.base_url)))
                .unwrap_or_else(|| server_host_label(&session.base_url).to_string());
            let switch_root = root.clone();
            popover_row(
                SharedString::from(format!("switcher-session-{ix}")),
                active,
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .w_full()
                    .child(
                        svg()
                            .path("icons/server.svg")
                            .w(px(14.))
                            .h(px(14.))
                            .text_color(rgba(theme::TEXT_TERTIARY)),
                    )
                    .child(
                        // `min_w_0()` lets this "username — host" label
                        // clip cleanly instead of bleeding past the
                        // popover's `max_w(360.)` (Y-axis-only scroll
                        // containment). Same `clamped_line` treatment
                        // `settings.rs::render_server_section` uses.
                        clamped_line(label, px(20.))
                            .flex_1()
                            .min_w_0()
                            .text_sm()
                            .text_color(if active {
                                rgba(theme::TEXT_PRIMARY)
                            } else {
                                rgba(theme::TEXT_SECONDARY)
                            }),
                    )
                    .children(active.then(|| {
                        svg()
                            .path("icons/check.svg")
                            .w(px(14.))
                            .h(px(14.))
                            .flex_shrink_0()
                            .text_color(rgba(theme::TEXT_PRIMARY))
                    })),
            )
            .when(!active, |d| {
                d.on_click(move |_e, _w, cx| {
                    let _ = switch_root.update(cx, |root, cx| root.switch_to_session(ix, cx));
                })
            })
        })
        .collect::<Vec<_>>();

    let manage_root = root.clone();
    let manage_row = popover_row(
        "switcher-manage",
        false,
        div()
            .text_sm()
            .text_color(rgba(theme::TEXT_SECONDARY))
            .child("Settings"),
    )
    .on_click(move |_e, _w, cx| {
        let _ = manage_root.update(cx, |root, cx| root.open_settings_from_switcher(cx));
    });

    popover_panel(
        "server-switcher-panel",
        px(320.),
        div()
            .flex()
            .flex_col()
            .gap_0p5()
            .children(rows)
            .child(div().my_1().h(px(1.)).bg(rgb(theme::SURFACE_HAIRLINE)))
            .child(manage_row),
    )
}

fn server_host_label(base_url: &str) -> &str {
    let without_scheme = base_url
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(base_url);
    without_scheme
        .split(['/', '?', '#'])
        .next()
        .unwrap_or(without_scheme)
}

/// The mascot at `size` (a square box; `img()`'s default
/// `ObjectFit::Contain` keeps the source's real aspect ratio inside it, so
/// nothing is cropped or stretched) -- DESIGN-GUIDE.md §A.4. No tint,
/// no recolour: every UI-state and header placement of a `jb_mascot_*` pose
/// goes through this one helper.
pub(crate) fn mascot_image(asset: &'static str, size: gpui::Pixels) -> gpui::Img {
    gpui::img(asset).w(size).h(size)
}

/// The brand lockup: `jb_mascot_base` beside the wordmark
/// (DESIGN-GUIDE.md §A.4). The launcher icon (`scripts/make-icon.sh`) is
/// the icon-tier mascot and never appears inside the app UI.
///
/// Proportioned per `docs/DESIGN-GUIDE.md` §A.4 -- mark 35 / gap 6 /
/// wordmark 20, scaled by this wordmark's own 22px size (x1.1). The mark
/// overhangs the row instead of growing it: `LOCKUP_ROW_H` is the row's
/// fixed footprint, and the extra height paints above/below via negative
/// margins, leaning toward the top in a 7:6 split.
const LOCKUP_WORDMARK_SIZE: f32 = 22.0;
const LOCKUP_ROW_H: f32 = 32.0;
const LOCKUP_MARK_SIZE: f32 = 38.0;
const LOCKUP_MARK_OVERHANG_TOP: f32 = 4.0;
const LOCKUP_MARK_OVERHANG_BOTTOM: f32 = 2.0;
const LOCKUP_GAP: f32 = 7.0;
const _: () = assert!(
    LOCKUP_MARK_SIZE - LOCKUP_MARK_OVERHANG_TOP - LOCKUP_MARK_OVERHANG_BOTTOM == LOCKUP_ROW_H
);

pub(crate) fn brand_lockup() -> impl IntoElement {
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(LOCKUP_GAP))
        .child(
            mascot_image("brand/jellybeam/jb_mascot_base.png", px(LOCKUP_MARK_SIZE))
                .flex_shrink_0()
                .mt(px(-LOCKUP_MARK_OVERHANG_TOP))
                .mb(px(-LOCKUP_MARK_OVERHANG_BOTTOM)),
        )
        .child(wordmark(px(LOCKUP_WORDMARK_SIZE)))
}

/// §3/§4's wordmark: "Jelly" in `PANNA`, "beam" in `PISTACCHIO`, one
/// baseline. GPUI 0.2.2's `TextRun` carries no per-run letter-spacing, so
/// the spec's one kern (−0.03em between the `y` and the `b`) is
/// approximated as a negative left margin on the "beam" span rather than
/// true glyph kerning -- nothing else is tracked or kerned.
pub(crate) fn wordmark(size: gpui::Pixels) -> impl IntoElement {
    let size_val = f32::from(size);
    div()
        .flex()
        .flex_row()
        .font_family(theme::FONT_DISPLAY)
        .text_size(size)
        // §3's "line-height 0.92" for the display face.
        .line_height(px(size_val * 0.92))
        .child(div().text_color(rgb(theme::PANNA)).child("Jelly"))
        .child(
            div()
                .text_color(rgb(theme::PISTACCHIO))
                .ml(px(-(size_val * 0.03)))
                .child("beam"),
        )
}

/// Which of the vendored Lucide icons (`crates/app/assets/icons/`) best
/// represents a library, decided off the library's name. Keyword match only
/// picks the *icon*: "movie" -> film.svg, "tv"/"show" -> tv.svg. Every other
/// library type (Music, Photos, Home Videos, Books, ...) has no matching
/// icon and falls back to the sidebar rail's monogram-letter treatment. Call
/// sites pass the raw server-configured name, never rewritten for display.
fn library_icon_path(library_name: &str) -> Option<&'static str> {
    let lower = library_name.to_lowercase();
    if lower.contains("movie") {
        Some("icons/film.svg")
    } else if lower.contains("tv") || lower.contains("show") {
        Some("icons/tv.svg")
    } else {
        None
    }
}

/// Top inset reserved for the macOS traffic lights, which float over the
/// window's own content (`titlebarAppearsTransparent` + full-size content
/// view). 40px clears their bottom edge with breathing room.
const TRAFFIC_LIGHT_INSET: gpui::Pixels = px(40.);

/// The featured backdrop's band height at the top of a Library page.
/// Deliberately shorter than Home's 42%-of-viewport hero -- atmosphere
/// behind a working surface, not a hero in its own right.
const LIBRARY_BACKDROP_HEIGHT: gpui::Pixels = px(420.);
/// Extra flat scrim over that band: `backdrop::layer`'s gradients are tuned
/// for a hero with large display type, so a poster wall needs the art
/// pushed further back before posters can read against it.
const LIBRARY_BACKDROP_SCRIM_ALPHA: u8 = 0x99;
/// The fade over the grid's clipped bottom edge.
const GRID_BOTTOM_FADE_HEIGHT: gpui::Pixels = px(80.);

/// The blurred/scrimmed featured backdrop band. Returns `None` when this
/// library has no item with a resolvable backdrop -- the page then renders
/// flat `SURFACE_BASE`.
fn library_backdrop_layer(
    lib: &LibraryState,
    store: &ImageStore,
    cx: &mut Context<Root>,
) -> Option<AnyElement> {
    let (item_id, tag) = lib.featured_backdrop.clone()?;
    let root = cx.entity().downgrade();
    let sharp = store.get(
        &item_id,
        media_cache::ImageKind::Backdrop,
        &tag,
        BACKDROP_WIDTH,
        root.clone(),
        cx,
    );
    let blurred = store.get_scrim(
        &item_id,
        media_cache::ImageKind::Backdrop,
        &tag,
        BACKDROP_WIDTH,
        root,
        cx,
    );
    Some(
        div()
            .absolute()
            .top_0()
            .left_0()
            .right_0()
            .h(LIBRARY_BACKDROP_HEIGHT)
            .overflow_hidden()
            .child(crate::backdrop::layer(sharp, blurred))
            .child(div().absolute().inset_0().bg(rgba(theme::tint(
                theme::SURFACE_BASE,
                LIBRARY_BACKDROP_SCRIM_ALPHA,
            ))))
            // §12's "no hard seams" rule.
            .child(
                div()
                    .absolute()
                    .bottom_0()
                    .left_0()
                    .right_0()
                    .h(px(160.))
                    .bg(linear_gradient(
                        180.,
                        linear_color_stop(rgba(theme::TRANSPARENT), 0.0),
                        linear_color_stop(rgb(theme::SURFACE_BASE), 1.0),
                    )),
            )
            .into_any_element(),
    )
}

fn render_browse(
    state: &MainState,
    library_columns: usize,
    content_width: gpui::Pixels,
    viewport: gpui::Size<gpui::Pixels>,
    cx: &mut Context<Root>,
) -> impl IntoElement {
    let root_weak = cx.entity().downgrade();

    let body = match &state.nav.current {
        View::Home => crate::home::render(
            &state.home,
            &state.image_store,
            &state.mirror,
            content_width,
            root_weak,
            cx,
        )
        .into_any_element(),
        View::Library { view_id } => {
            if let Some(lib) = &state.library {
                let store = state.image_store.clone();
                let on_open: crate::grid::ItemAction = {
                    let root = root_weak.clone();
                    Rc::new(move |item: &CardRow, cx: &mut App| {
                        let id = item.id.clone();
                        let _ = root.update(cx, |root, cx| root.open_detail(id, cx));
                    })
                };
                let on_dwell: crate::grid::ItemAction = {
                    let root = root_weak.clone();
                    Rc::new(move |item: &CardRow, cx: &mut App| {
                        let id = item.id.clone();
                        let _ = root.update(cx, |root, cx| root.prefetch_detail(id, cx));
                    })
                };
                // A real mouse-move over cell `ix` re-takes the highlight
                // -- see `Root::hover_library_cell`'s doc comment.
                let on_hover: crate::grid::HoverAction = {
                    let root = root_weak.clone();
                    Rc::new(move |ix: usize, cx: &mut App| {
                        let _ = root.update(cx, |root, cx| root.hover_library_cell(ix, cx));
                    })
                };
                // Keep the server-configured name verbatim in every location.
                let title = state
                    .views
                    .iter()
                    .find(|v| &v.id == view_id)
                    .map(|v| v.name.clone())
                    .unwrap_or_else(|| "Library".to_string());
                let close_menus_root = root_weak.clone();
                let any_menu_open = lib.sort_menu_open || lib.filter_menu_open;
                // A featured item's backdrop, blurred + scrimmed through the
                // same `backdrop::layer` stack the Home hero and every
                // Detail page use (there is no CSS blur for a `div()` in
                // this GPUI version; the blur is the CPU-side `get_scrim`
                // variant).
                let library_backdrop = library_backdrop_layer(lib, &state.image_store, cx);
                let focused_spec = Rc::new(
                    lib.items
                        .get(lib.highlight.slot())
                        .and_then(|item| lib.spec_by_id.get(&item.id))
                        .cloned()
                        .unwrap_or_default(),
                );
                let cell_width = crate::grid::library_cell_width(content_width, library_columns);
                div()
                    .id("library")
                    .relative()
                    .size_full()
                    .flex()
                    .flex_col()
                    .bg(rgb(theme::SURFACE_BASE))
                    .children(library_backdrop)
                    .child(
                        div()
                            .pt(theme::SPACE_LOOSE)
                            .px(theme::SPACE_SECTION)
                            .text_size(theme::TEXT_TITLE)
                            .font_weight(gpui::FontWeight::BOLD)
                            .text_color(rgba(theme::TEXT_PRIMARY))
                            .child(SharedString::from(title)),
                    )
                    .child(library_toolbar(lib, root_weak.clone()))
                    // Brand §5's empty state: one line, Archivo, factual,
                    // no illustration except a truly empty library's tier-2
                    // mascot (DESIGN-GUIDE.md §A.7's "empty library"
                    // pose) -- a filter excluding everything is a different
                    // state and stays text-only.
                    .when(lib.items.is_empty(), |d| {
                        let empty_body = if lib.raw_items.is_empty() {
                            empty_state_mascot(
                                "brand/jellybeam/jb_mascot_curious.png",
                                "No items in this library.",
                            )
                            .into_any_element()
                        } else {
                            empty_state("icons/search.svg", "No items match the current filters.")
                                .into_any_element()
                        };
                        d.child(div().flex_1().min_h_0().px_5().child(empty_body))
                    })
                    // The list view renders the SAME `lib.items` slice the
                    // wall does -- already sorted/filtered
                    // (`apply_library_filters` owns that once, for both).
                    // Switching modes is a projection change, nothing else.
                    .when(
                        !lib.items.is_empty() && lib.view_mode == LibraryViewMode::List,
                        |d| {
                            d.child(
                                div()
                                    .flex_1()
                                    .min_h_0()
                                    .px(px(20.) - theme::FOCUS_RING_CLEARANCE)
                                    .mt(-theme::LIST_TOP_CLIP_SLACK)
                                    .pb_5()
                                    .child(crate::library_list::library_list(
                                        "library-list",
                                        lib.items.clone(),
                                        lib.spec_by_id.clone(),
                                        lib.focus_engaged.then(|| lib.highlight.slot()),
                                        store.clone(),
                                        root_weak.clone(),
                                        lib.list_scroll.clone(),
                                        on_open.clone(),
                                        on_dwell.clone(),
                                        on_hover.clone(),
                                    )),
                            )
                        },
                    )
                    .when(
                        !lib.items.is_empty() && lib.view_mode == LibraryViewMode::Grid,
                        |d| {
                            d.child(
                                div()
                                    .flex_1()
                                    .min_h_0()
                                    .px(px(20.) - theme::FOCUS_RING_CLEARANCE)
                                    .mt(-theme::LIST_TOP_CLIP_SLACK)
                                    .pb_5()
                                    .child(crate::grid::poster_grid(
                                        "library-grid",
                                        lib.items.clone(),
                                        library_columns,
                                        cell_width,
                                        // Render off `highlight.slot()` (the
                                        // last-input-wins result), not
                                        // `focus.index` directly, so the
                                        // policy is the one thing that
                                        // decides what's highlighted.
                                        lib.focus_engaged.then(|| lib.highlight.slot()),
                                        focused_spec,
                                        store,
                                        root_weak,
                                        lib.scroll.clone(),
                                        on_open,
                                        on_dwell,
                                        on_hover,
                                    )),
                            )
                        },
                    )
                    // A page-color ramp over the wall's clipped last row,
                    // so a partially-scrolled poster dissolves into the
                    // page. No `id`/handlers, so it never eats a click.
                    .child(
                        div()
                            .absolute()
                            .bottom_0()
                            .left_0()
                            .right_0()
                            .h(GRID_BOTTOM_FADE_HEIGHT)
                            .bg(linear_gradient(
                                180.,
                                linear_color_stop(rgba(theme::TRANSPARENT), 0.0),
                                linear_color_stop(rgb(theme::SURFACE_BASE), 1.0),
                            )),
                    )
                    .when(any_menu_open, |d| {
                        d.child(click_away_catcher(
                            "library-toolbar-menu-catcher",
                            move |cx| {
                                let _ = close_menus_root.update(cx, |root, cx| {
                                    root.close_library_filter_menu(cx);
                                    root.close_library_sort_menu(cx);
                                });
                            },
                        ))
                    })
                    .into_any_element()
            } else {
                div()
                    .size_full()
                    .bg(rgb(theme::SURFACE_BASE))
                    .into_any_element()
            }
        }
        View::Detail { .. } => {
            if let Some(detail) = &state.detail {
                let playing_this =
                    state.playing_item_id.as_deref() == Some(detail.item_id.as_str());
                crate::detail::render(
                    detail,
                    &state.mirror,
                    &state.image_store,
                    root_weak,
                    playing_this,
                    state.paused,
                    content_width,
                    viewport.height,
                    state.offline,
                    cx,
                )
                .into_any_element()
            } else {
                div()
                    .size_full()
                    .bg(rgb(theme::SURFACE_BASE))
                    .into_any_element()
            }
        }
        View::Channel { .. } => {
            if let Some(cb) = &state.channel_browse {
                crate::channel_browse::render(cb, root_weak).into_any_element()
            } else {
                div()
                    .size_full()
                    .bg(rgb(theme::SURFACE_BASE))
                    .into_any_element()
            }
        }
        View::Discover(view) => {
            let view = *view;
            if let Some(ds) = &state.discover {
                discover::render(ds, view, content_width, &state.image_store, root_weak, cx)
            } else {
                div()
                    .size_full()
                    .bg(rgb(theme::SURFACE_BASE))
                    .into_any_element()
            }
        }
    };

    div()
        .id("browse-content")
        .size_full()
        .flex()
        .flex_col()
        .bg(rgb(theme::SURFACE_BASE))
        .child(div().flex_1().min_h_0().child(body))
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- Discover connect completion: stale-account guard ----------------

    #[test]
    fn discover_connect_identity_matches_the_same_account() {
        let identity = ("http://jf.test".to_string(), "user-1".to_string());
        assert!(Root::discover_connect_identity_still_current(
            &identity, &identity
        ));
    }

    #[test]
    fn discover_connect_identity_rejects_a_different_user_on_the_same_server() {
        let spawned = ("http://jf.test".to_string(), "user-1".to_string());
        let current = ("http://jf.test".to_string(), "user-2".to_string());
        assert!(!Root::discover_connect_identity_still_current(
            &spawned, &current
        ));
    }

    #[test]
    fn discover_connect_identity_rejects_a_server_switch() {
        let spawned = ("http://jf-a.test".to_string(), "user-1".to_string());
        let current = ("http://jf-b.test".to_string(), "user-1".to_string());
        assert!(!Root::discover_connect_identity_still_current(
            &spawned, &current
        ));
    }

    // ---- Sidebar sync pill: determinate progress fraction ---------------

    #[test]
    fn sync_progress_fraction_is_none_until_a_real_denominator_exists() {
        assert_eq!(sync_progress_fraction(100, None), None);
        assert_eq!(sync_progress_fraction(100, Some(0)), None);
    }

    #[test]
    fn sync_progress_fraction_tracks_and_clamps() {
        assert_eq!(sync_progress_fraction(0, Some(5237)), Some(0.0));
        assert_eq!(sync_progress_fraction(2500, Some(5000)), Some(0.5));
        assert_eq!(sync_progress_fraction(5237, Some(5237)), Some(1.0));
        // A total that shrank mid-walk must clamp, not overflow the track.
        assert_eq!(sync_progress_fraction(600, Some(500)), Some(1.0));
    }

    // ---- library_icon_path (icons only -- library names themselves
    // render verbatim from the server) -------------------------

    #[test]
    fn library_icon_path_maps_movies_and_tv_keywords_from_raw_server_names() {
        assert_eq!(library_icon_path("Movies"), Some("icons/film.svg"));
        assert_eq!(library_icon_path("Movies-Adults"), Some("icons/film.svg"));
        assert_eq!(library_icon_path("TV"), Some("icons/tv.svg"));
        // "show" keyword: names with no "tv" substring are still TV libraries.
        assert_eq!(library_icon_path("Shows-Kids"), Some("icons/tv.svg"));
        assert_eq!(library_icon_path("Shows-Adults"), Some("icons/tv.svg"));
    }

    #[test]
    fn library_icon_path_is_none_for_an_unmapped_library_type() {
        assert_eq!(library_icon_path("Music"), None);
        assert_eq!(library_icon_path("Home Videos"), None);
    }

    /// Must strip the scheme and any trailing path, but never panic on
    /// input that doesn't look like a URL at all.
    #[test]
    fn server_host_label_strips_scheme_and_path() {
        assert_eq!(
            server_host_label("https://media.example.com:8443/"),
            "media.example.com:8443"
        );
        assert_eq!(
            server_host_label("http://192.0.2.50:8096"),
            "192.0.2.50:8096"
        );
        assert_eq!(server_host_label("localhost:8096"), "localhost:8096");
        assert_eq!(server_host_label(""), "");
    }

    /// Pins the Connect screen's Tab/Shift+Tab order data (the four
    /// `CONNECT_TAB_*` constants) so an accidental swap/duplicate is
    /// caught, even without a live GPUI window to test against directly.
    #[test]
    fn connect_tab_order_is_ascending_and_unique() {
        let order = [
            CONNECT_TAB_SERVER,
            CONNECT_TAB_USERNAME,
            CONNECT_TAB_PASSWORD,
            CONNECT_TAB_CONNECT_BUTTON,
        ];
        let mut sorted = order;
        sorted.sort_unstable();
        assert_eq!(
            order, sorted,
            "Server URL -> Username -> Password -> Connect must already be \
             ascending -- GPUI's tab-stop machinery walks tab_index in \
             ascending order and wraps (Window::focus_next/focus_prev)"
        );
        let mut deduped = order.to_vec();
        deduped.dedup();
        assert_eq!(
            deduped.len(),
            order.len(),
            "each field must have its own distinct tab_index"
        );
    }

    /// Only a transport-class failure gets the network-troubleshooting hint.
    /// `session::connect_flow`/`resume_flow` flatten `ApiError` to a
    /// `String`, so `handle_connect_outcome` relies on the `"transport: "`
    /// `Display` prefix still being present in that string.
    #[test]
    fn transport_error_hint_for_message_matches_transport_prefix_only() {
        assert!(transport_error_hint_for_message(
            "transport: error sending request for url (http://x/): client error (Connect)"
        )
        .is_some());
        assert!(transport_error_hint_for_message("unauthorized").is_none());
        assert!(transport_error_hint_for_message("decode: missing field `Id`").is_none());
        assert!(
            transport_error_hint_for_message("http status 500: internal server error").is_none()
        );
    }

    /// Same distinction as above, but for the Quick Connect paths, which
    /// still hold the real `ApiError` rather than a pre-flattened `String`.
    #[test]
    fn transport_error_hint_for_api_error_matches_transport_variant_only() {
        assert!(
            transport_error_hint_for_api_error(&jellyfin_api::ApiError::Transport(
                "connection refused".to_string()
            ))
            .is_some()
        );
        assert!(
            transport_error_hint_for_api_error(&jellyfin_api::ApiError::Unauthorized).is_none()
        );
        assert!(
            transport_error_hint_for_api_error(&jellyfin_api::ApiError::Decode(
                "bad json".to_string()
            ))
            .is_none()
        );
        assert!(
            transport_error_hint_for_api_error(&jellyfin_api::ApiError::Status {
                code: 500,
                body: String::new()
            })
            .is_none()
        );
    }

    // ---- `[`/`]` + EOF auto-advance must not step onto a virtual
    // (unaired/missing) episode ----------------------------------

    fn episode_row(id: &str, index: i32, is_virtual: bool) -> CardRow {
        CardRow {
            id: id.to_string(),
            item_type: "Episode".to_string(),
            name: id.to_string(),
            primary_tag: None,
            blurhash: None,
            played: false,
            position_ticks: 0,
            runtime_ticks: None,
            unplayed_count: None,
            production_year: None,
            index_number: Some(index),
            parent_index_number: None,
            series_id: None,
            series_primary_tag: None,
            parent_backdrop_item_id: None,
            parent_backdrop_tag: None,
            last_played_date: None,
            overview: None,
            premiere_date: None,
            is_virtual,
            series_name: None,
            library_id: None,
        }
    }

    /// The plain case is unchanged: the very next episode wins.
    #[test]
    fn adjacent_playable_steps_to_the_immediate_neighbour() {
        let episodes = [
            episode_row("e1", 1, false),
            episode_row("e2", 2, false),
            episode_row("e3", 3, false),
        ];
        assert_eq!(
            adjacent_playable(&episodes, 1, EpisodeStep::Next).map(|e| e.id),
            Some("e3".to_string())
        );
        assert_eq!(
            adjacent_playable(&episodes, 1, EpisodeStep::Prev).map(|e| e.id),
            Some("e1".to_string())
        );
    }

    /// With autoplay on, stepping onto an unaired placeholder ends playback
    /// on "Can't play" instead of continuing the season.
    #[test]
    fn adjacent_playable_skips_over_unaired_episodes() {
        let episodes = [
            episode_row("e1", 1, false),
            episode_row("e2-unaired", 2, true),
            episode_row("e3", 3, false),
        ];
        assert_eq!(
            adjacent_playable(&episodes, 0, EpisodeStep::Next).map(|e| e.id),
            Some("e3".to_string())
        );
        assert_eq!(
            adjacent_playable(&episodes, 2, EpisodeStep::Prev).map(|e| e.id),
            Some("e1".to_string())
        );
    }

    /// The last aired episode of a partially-aired season arms nothing --
    /// the Up Next card stays hidden rather than offering an episode that
    /// doesn't exist yet.
    #[test]
    fn adjacent_playable_is_none_when_only_unaired_episodes_remain() {
        let episodes = [
            episode_row("e1", 1, false),
            episode_row("e2-unaired", 2, true),
            episode_row("e3-unaired", 3, true),
        ];
        assert!(adjacent_playable(&episodes, 0, EpisodeStep::Next).is_none());
    }

    /// Boundaries: first episode has no `Prev`, last has no `Next`, and
    /// neither may panic (this runs from a ~4Hz poll during playback).
    #[test]
    fn adjacent_playable_handles_both_ends_of_a_season() {
        let episodes = [episode_row("e1", 1, false), episode_row("e2", 2, false)];
        assert!(adjacent_playable(&episodes, 0, EpisodeStep::Prev).is_none());
        assert!(adjacent_playable(&episodes, 1, EpisodeStep::Next).is_none());
        assert!(adjacent_playable(&[], 0, EpisodeStep::Next).is_none());
    }

    // ---- `decide_preload_retarget` --------------------------

    #[test]
    fn decide_preload_retarget_proceeds_immediately_when_the_slot_is_empty() {
        assert_eq!(
            decide_preload_retarget(None, "item-a", PreloadTrigger::Hover),
            PreloadRetargetDecision::ProceedImmediately
        );
        assert_eq!(
            decide_preload_retarget(None, "item-a", PreloadTrigger::Deliberate),
            PreloadRetargetDecision::ProceedImmediately
        );
    }

    #[test]
    fn decide_preload_retarget_proceeds_immediately_when_the_target_already_matches() {
        // Neither trigger should ever be treated as a "replace" when the
        // slot already holds this exact target.
        assert_eq!(
            decide_preload_retarget(Some("item-a"), "item-a", PreloadTrigger::Hover),
            PreloadRetargetDecision::ProceedImmediately
        );
        assert_eq!(
            decide_preload_retarget(Some("item-a"), "item-a", PreloadTrigger::Deliberate),
            PreloadRetargetDecision::ProceedImmediately
        );
    }

    #[test]
    fn decide_preload_retarget_deliberate_requests_replace_a_different_target_immediately() {
        assert_eq!(
            decide_preload_retarget(Some("item-a"), "item-b", PreloadTrigger::Deliberate),
            PreloadRetargetDecision::ReplaceImmediately
        );
    }

    #[test]
    fn decide_preload_retarget_hover_on_a_different_target_schedules_instead_of_replacing() {
        assert_eq!(
            decide_preload_retarget(Some("item-a"), "item-b", PreloadTrigger::Hover),
            PreloadRetargetDecision::ScheduleReplace
        );
    }
}
