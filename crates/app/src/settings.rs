//! Settings sheet (docs/UX-SPEC.md §1: "Settings (sheet: server, playback,
//! subtitles, about)"): multi-server/user switcher, per-server playback
//! bitrate cap, and subtitle style overrides live in one modal sheet
//! reachable from the sidebar footer.
//!
//! Two independent pieces:
//! - [`AppSettings`]: the persisted preferences (subtitle style + per-server
//!   bitrate caps), one flat JSON file under Application Support -- same
//!   shape as `player_prefs.rs`'s `TrackPrefs` (load once, mutate in place,
//!   save on every change, best-effort).
//! - [`SettingsState`]: which section of the sheet is open -- UI-only,
//!   never persisted.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::time::Instant;

use gpui::{
    div, linear_color_stop, linear_gradient, prelude::*, px, rgb, rgba, svg, Context, SharedString,
    WeakEntity,
};
use jellyfin_api::models::MediaSegmentType;
use serde::{Deserialize, Serialize};

use crate::about;
use crate::discover;
use crate::keychain::StoredSessionList;
use crate::root::{Root, Screen};
use crate::theme;
use crate::ui::components::{
    button, chip_button, clamped_line, dense_label, dialog_panel, dialog_scrim, form_row,
    form_row_desc, keycap_row, list_row, status_dot, toggle_switch, ButtonSize, ButtonVariant,
};

/// Subtitle style overrides, serializable mirror of `player::SubtitleStyle`
/// (kept separate to avoid `serde` derives on the frozen `player` crate for
/// a UI-only concern).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub(crate) struct SubtitleStylePrefs {
    /// mpv `sub-pos` units: 0-150, percent of frame height measured from
    /// the top -- smaller is higher on screen; mpv's own default is ~100.
    /// `None` = leave mpv's default alone. Floored at [`SUBTITLE_POS_FLOOR`]
    /// (see [`Self::effective_pos`]) so an old stored value can't pin
    /// subtitles to the top.
    pub pos: Option<i64>,
    pub scale: f64,
    pub bold: bool,
    pub back_alpha: f64,
}

impl Default for SubtitleStylePrefs {
    fn default() -> Self {
        SubtitleStylePrefs {
            scale: 1.0,
            pos: None,
            bold: false,
            back_alpha: 0.0,
        }
    }
}

/// The lowest mpv `sub-pos` this app will ever hand mpv -- subtitles are
/// never raised past the vertical middle of the frame. Re-applied after the
/// OSD's own upward shift (`player_ui::effective_subtitle_pos`) so the OSD
/// can't push an effective position into the top half either.
pub(crate) const SUBTITLE_POS_FLOOR: i64 = 60;
/// mpv's documented `sub-pos` ceiling (mpv manual, "Subtitles": 0-150).
pub(crate) const SUBTITLE_POS_CEIL: i64 = 150;

impl SubtitleStylePrefs {
    /// [`Self::pos`] sanitized into the lower half of the frame -- the single
    /// place stored preference units become "an mpv `sub-pos` we are willing
    /// to push". Every apply path goes through this (directly, or via
    /// [`Self::to_player`]).
    pub(crate) fn effective_pos(self) -> Option<i64> {
        self.pos
            .map(|p| p.clamp(SUBTITLE_POS_FLOOR, SUBTITLE_POS_CEIL))
    }

    pub(crate) fn to_player(self) -> player::SubtitleStyle {
        player::SubtitleStyle {
            scale: self.scale,
            pos: self.effective_pos(),
            bold: self.bold,
            back_alpha: self.back_alpha,
        }
    }
}

/// Preset scale/position/opacity steps the Subtitles section cycles through
/// (GPUI 0.2.2 has no slider primitive, so docs/UX-SPEC.md's "size / position /
/// opacity" sliders are quantized presets instead of continuous drag).
pub(crate) const SCALE_PRESETS: [f64; 4] = [0.75, 1.0, 1.25, 1.5];
/// The vertical-position ladder, in mpv `sub-pos` units (percent of frame
/// height from the top -- smaller = higher on screen). Each step raises
/// subtitles further off mpv's bottom-ish default, stopping at
/// [`SUBTITLE_POS_FLOOR`], the vertical middle of the frame.
pub(crate) const POS_PRESETS: [(&str, Option<i64>); 4] = [
    ("Default", None),
    ("Raised", Some(88)),
    ("Higher", Some(74)),
    ("Highest", Some(SUBTITLE_POS_FLOOR)),
];
pub(crate) const BACK_ALPHA_PRESETS: [f64; 4] = [0.0, 0.25, 0.5, 0.75];

/// Per-server playback quality mode. Direct Play is the default (no
/// auto-transcode); a cap is strictly opt-in.
///
/// - `DirectPlay` (default): no bitrate cap is ever sent -- the server
///   never transcodes on Jellybeam's account, whatever the link.
/// - `Measured`: measure the link once per server per app run
///   (`playback.rs::auto_bitrate_cap`, Jellyfin's own BitrateTest) and cap
///   at 80% -- the server transcodes only files that exceed the link.
/// - `Cap`: fixed cap in bits/sec; server transcodes above it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BitrateMode {
    DirectPlay,
    Measured,
    Cap(u32),
}

/// Stored-map sentinel for [`BitrateMode::Measured`] (a literal 1 bps cap
/// is meaningless, so the value is unambiguous); `0` = Direct Play, any
/// other value = fixed cap in bps. Keeps `settings.json` shape unchanged.
const MEASURED_SENTINEL: u32 = 1;

/// Quality-mode presets for the Settings row, per [`BitrateMode`].
pub(crate) const BITRATE_PRESETS: [(&str, BitrateMode); 5] = [
    ("Direct Play", BitrateMode::DirectPlay),
    ("Auto", BitrateMode::Measured),
    ("20 Mbps", BitrateMode::Cap(20_000_000)),
    ("8 Mbps", BitrateMode::Cap(8_000_000)),
    ("3 Mbps", BitrateMode::Cap(3_000_000)),
];

/// Per-`MediaSegmentType` behavior: **Ask** shows the OSD pill; **AutoSkip**
/// seeks past the segment on entry, with a 5s "Undo" toast
/// (`player_ui.rs::SkipToastState`); **Off** ignores the segment entirely.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum SegmentAction {
    #[default]
    Ask,
    AutoSkip,
    Off,
}

/// Per-type skip behavior, one field per `MediaSegmentType` variant the
/// generated `jellyfin-api` model exposes (`Unknown`/`Unrecognized` have no
/// row and are always `Off`, see `action_for`). Defaults to `Ask` for every
/// type except `Commercial`, which defaults to `AutoSkip` -- the one
/// segment type nobody wants to be asked about every occurrence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SkipSegmentPrefs {
    pub intro: SegmentAction,
    pub outro: SegmentAction,
    pub recap: SegmentAction,
    pub preview: SegmentAction,
    pub commercial: SegmentAction,
}

impl Default for SkipSegmentPrefs {
    fn default() -> Self {
        SkipSegmentPrefs {
            intro: SegmentAction::Ask,
            outro: SegmentAction::Ask,
            recap: SegmentAction::Ask,
            preview: SegmentAction::Ask,
            commercial: SegmentAction::AutoSkip,
        }
    }
}

impl SkipSegmentPrefs {
    /// `Unknown`/`Unrecognized` (the generated enum's catch-all for a
    /// segment type this client's pinned model doesn't recognize, e.g. a
    /// future server-side addition) has no settings row and is always
    /// treated as `Off` -- there's nothing meaningful to skip/ask about for
    /// a type this client can't even label.
    pub(crate) fn action_for(&self, ty: MediaSegmentType) -> SegmentAction {
        match ty {
            MediaSegmentType::Intro => self.intro,
            MediaSegmentType::Outro => self.outro,
            MediaSegmentType::Recap => self.recap,
            MediaSegmentType::Preview => self.preview,
            MediaSegmentType::Commercial => self.commercial,
            MediaSegmentType::Unknown | MediaSegmentType::Unrecognized => SegmentAction::Off,
        }
    }

    pub(crate) fn set_action_for(&mut self, ty: MediaSegmentType, action: SegmentAction) {
        match ty {
            MediaSegmentType::Intro => self.intro = action,
            MediaSegmentType::Outro => self.outro = action,
            MediaSegmentType::Recap => self.recap = action,
            MediaSegmentType::Preview => self.preview = action,
            MediaSegmentType::Commercial => self.commercial = action,
            MediaSegmentType::Unknown | MediaSegmentType::Unrecognized => {}
        }
    }
}

/// "Autoplay next episode" toggle + delay. The next-episode card
/// (`player_ui.rs::render_next_episode_card`) always appears near the end
/// of an episode -- `enabled` only gates whether it counts down and
/// auto-advances (`Root::tick_next_episode`); off, the viewer must click
/// "Play Next" explicitly.
///
/// Default: `enabled = true`, `delay_secs = 10` (docs/DESIGN-GUIDE.md
/// C.6.5: long enough to dismiss, short enough to keep a binge moving).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct AutoplayPrefs {
    pub enabled: bool,
    pub delay_secs: u32,
}

impl Default for AutoplayPrefs {
    fn default() -> Self {
        AutoplayPrefs {
            enabled: true,
            delay_secs: 10,
        }
    }
}

pub(crate) const AUTOPLAY_DELAY_PRESETS: [u32; 3] = [5, 10, 15];

/// Configurable ←/→ skip lengths, back and forward independently.
/// Shift+←/→'s ±60s "big skip" stays a fixed constant, not configurable --
/// letting it converge with the plain arrow (e.g. both set to 60s) would
/// make the shift modifier pointless.
///
/// Default: 10s/10s, matching the previous hardcoded behavior exactly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SkipLengthPrefs {
    pub back_secs: u32,
    pub forward_secs: u32,
}

impl Default for SkipLengthPrefs {
    fn default() -> Self {
        SkipLengthPrefs {
            back_secs: 10,
            forward_secs: 10,
        }
    }
}

pub(crate) const SKIP_LENGTH_PRESETS: [u32; 5] = [5, 10, 15, 30, 60];

/// Subtitle behavior when no per-series memory applies yet
/// (`player_prefs.rs`'s `TrackPrefs` only helps from a series' *second*
/// episode on). See `player_ui::resolve_track_selection` for the decision
/// table each variant drives.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum SubtitleMode {
    /// Leave whatever the stream/server itself marked as the default
    /// subtitle track (today's behavior, unchanged) -- no preference is
    /// applied at all.
    #[default]
    Default,
    /// Always enable a subtitle track: the one matching
    /// `LanguagePrefs::subtitle` if set and present, else the stream's own
    /// default-flagged subtitle track, else simply the first one.
    Always,
    /// Only enable a subtitle track that is both flagged `forced` by the
    /// container *and* in the same language as the audio track actually
    /// playing (the classic "alien dialogue" burned-in-style case) --
    /// otherwise subtitles stay off.
    OnlyForced,
    /// Subtitles off, full stop, regardless of any stream/server default.
    None,
}

pub(crate) const SUBTITLE_MODE_PRESETS: [(&str, SubtitleMode); 4] = [
    ("Default", SubtitleMode::Default),
    ("Always", SubtitleMode::Always),
    ("Forced only", SubtitleMode::OnlyForced),
    ("Off", SubtitleMode::None),
];

/// Global preferred audio/subtitle language, applied at track-announcement
/// time whenever no per-series memory exists for the item's series (see
/// `player_ui::resolve_track_selection`; per-series always wins when
/// present). Language codes are ISO 639-2 as mpv reports them
/// (`Track::lang`); either "B" or "T" form is accepted on input and matched
/// tolerantly (`player_ui::lang_matches`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct LanguagePrefs {
    /// `None` = "Any" -- no audio-language preference is applied, the
    /// stream/server's own default audio track plays untouched.
    pub audio: Option<String>,
    /// The language `SubtitleMode::Always` prefers -- irrelevant to every
    /// other mode (`OnlyForced` matches the *audio* language instead, and
    /// `Default`/`None` don't pick a subtitle track by language at all).
    pub subtitle: Option<String>,
    pub subtitle_mode: SubtitleMode,
}

/// Common-language preset row for both the audio- and subtitle-language
/// pickers -- `None` is "Any"/off. ISO 639-2 "B" form throughout
/// (`"fre"`/`"ger"`/`"chi"`); `player_ui::lang_matches` treats either form
/// as equivalent, so this choice has no effect on matching.
///
/// Option-key speed-hold toggle. Wrapped in a one-field struct (rather than
/// a bare `bool` on `SettingsFile`) so its default -- on -- comes from a
/// real `impl Default`: `#[derive(Default)]` on `SettingsFile` would give a
/// bare `bool` field `false`, wrong for a fresh install or corrupt-file
/// fallback even with `#[serde(default)]` covering the "old file, new
/// field" case.
///
/// Defaults on: the gesture only engages while Option is physically held
/// (`option_speed_hold::decide_speed_rate`), so it can't fire by accident.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SpeedBoostPrefs {
    pub enabled: bool,
}

impl Default for SpeedBoostPrefs {
    fn default() -> Self {
        SpeedBoostPrefs { enabled: true }
    }
}

/// "Preload next item" toggle -- whether the app is allowed to open the
/// item the user is likely to play next as a dark (paused) stream ahead of
/// a click, so Play starts in tens of milliseconds instead of paying a full
/// PlaybackInfo + stream-open round trip. Same one-field-wrapper-with-a-
/// real-`impl Default` shape as [`SpeedBoostPrefs`], for the identical
/// reason (a bare `bool` on `SettingsFile` would default to `false` via
/// `#[derive(Default)]`).
///
/// Defaults on: bandwidth cost is bounded elsewhere (~6MB cap, a settle
/// signal gate, throttled hover retargets), so an on-by-default speculative
/// preload is the same "instant Play" tradeoff jellyfin-mpv-shim and
/// friends already make. Read [`AppSettings::preload`]; toggled through
/// [`AppSettings::set_preload`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PreloadPrefs {
    pub enabled: bool,
}

impl Default for PreloadPrefs {
    fn default() -> Self {
        PreloadPrefs { enabled: true }
    }
}

pub(crate) const LANGUAGE_PRESETS: [(&str, Option<&str>); 11] = [
    ("Any", None),
    ("English", Some("eng")),
    ("Japanese", Some("jpn")),
    ("Spanish", Some("spa")),
    ("French", Some("fre")),
    ("German", Some("ger")),
    ("Italian", Some("ita")),
    ("Portuguese", Some("por")),
    ("Russian", Some("rus")),
    ("Korean", Some("kor")),
    ("Chinese", Some("chi")),
];

/// Which projection the Library screen paints its (identical) backing item
/// list through -- Grid/List toggle (`library_list.rs`).
///
/// Persisted per library view id, not globally: the two modes answer
/// genuinely different questions ("which poster do I recognise" vs. "what is
/// in here, densely, with its specs"), and a movie library and a show
/// library legitimately want different answers.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) enum LibraryViewMode {
    /// The virtualized poster wall (`grid.rs`) -- unchanged default.
    #[default]
    Grid,
    /// The virtualized dense row list (`library_list.rs`).
    List,
}

/// `/Shows/NextUp` cutoff + rewatching (Home section of the Settings
/// sheet). `cutoff_days`: `None` ("Off", default) means "don't send
/// `nextUpDateCutoff` at all" -- unfiltered; `Some(n)` restricts Next Up to
/// series with unwatched content added within the last `n` days.
/// `rewatching`: mirrors `jellyfin_api::NextUpOptions::enable_rewatching`
/// -- off by default (a fully-watched series stays off Next Up), on lets a
/// rewatch-from-the-start series' next episode count.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct NextUpPrefs {
    pub cutoff_days: Option<u32>,
    pub rewatching: bool,
}

impl NextUpPrefs {
    /// Converts to the media-cache-side options struct the mirror actually
    /// consumes -- kept as a method on the settings-side type (rather than a
    /// `From` impl in media-cache, which would need to know about `app`'s
    /// settings shape) since `app` already depends on `media_cache`, not the
    /// other way around.
    pub(crate) fn to_next_up_options(self) -> media_cache::NextUpOptions {
        media_cache::NextUpOptions {
            cutoff_days: self.cutoff_days,
            rewatching: self.rewatching,
        }
    }
}

/// Cutoff presets for the Next Up section's segmented row -- `None` is
/// "Off" (today's unfiltered behavior, and the default).
pub(crate) const NEXT_UP_CUTOFF_PRESETS: [(&str, Option<u32>); 5] = [
    ("Off", None),
    ("14 days", Some(14)),
    ("30 days", Some(30)),
    ("90 days", Some(90)),
    ("365 days", Some(365)),
];

/// Where the app lands right after connecting -- Home (default) or
/// straight into one specific library. `Library`'s `String` is a view id
/// (`views.id`, the same key `library_view_modes`/per-library visibility
/// use), not a display name, so a server-side rename doesn't strand the
/// stored preference -- see [`AppSettings::startup_screen`] for the
/// "library no longer exists" fallback.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum StartupScreen {
    #[default]
    Home,
    Library(String),
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct SettingsFile {
    #[serde(default)]
    subtitle: SubtitleStylePrefs,
    /// Keyed by server `base_url`. 0 means "no cap"; absent means the same
    /// (never configured) -- collapsed to one representation so
    /// `bitrate_cap_bps` has a single "is there a cap" branch instead of a
    /// nested `Option<Option<u32>>`.
    #[serde(default)]
    server_bitrate_caps: HashMap<String, u32>,
    #[serde(default)]
    skip_segments: SkipSegmentPrefs,
    #[serde(default)]
    autoplay: AutoplayPrefs,
    /// Keyed by library view id (the server's own library GUID). Absent means
    /// `LibraryViewMode::default()`, i.e. the poster wall -- so an existing
    /// settings file from before this key existed keeps today's behavior.
    #[serde(default)]
    library_view_modes: HashMap<String, LibraryViewMode>,
    /// Keyed by hostname (not `base_url` -- DNS is a property of the host,
    /// and two servers on the same box share an answer). See
    /// [`AppSettings::dns_seed`].
    #[serde(default)]
    dns_seed: HashMap<String, Vec<String>>,
    /// Next Up cutoff/rewatching -- see [`NextUpPrefs`].
    #[serde(default)]
    next_up: NextUpPrefs,
    /// Per-library-view-id Home visibility. Keyed the same way
    /// `library_view_modes` is; `true` means "hidden from Home" (a library
    /// nobody has toggled is absent, i.e. visible) -- see
    /// [`AppSettings::library_visible_on_home`].
    #[serde(default)]
    home_library_hidden: HashMap<String, bool>,
    /// "Hide watched from Latest" -- see [`AppSettings::hide_watched_latest`].
    #[serde(default)]
    hide_watched_latest: bool,
    /// Default screen on launch -- see [`StartupScreen`].
    #[serde(default)]
    startup_screen: StartupScreen,
    /// See [`SkipLengthPrefs`].
    #[serde(default)]
    skip_length: SkipLengthPrefs,
    /// See [`LanguagePrefs`].
    #[serde(default)]
    language: LanguagePrefs,
    /// Hold-Space-for-2x toggle -- see
    /// [`AppSettings::speed_boost`]/[`SpeedBoostPrefs`].
    #[serde(default)]
    speed_boost: SpeedBoostPrefs,
    /// "Preload next item" toggle -- see
    /// [`AppSettings::preload`]/[`PreloadPrefs`].
    #[serde(default)]
    preload: PreloadPrefs,
}

fn settings_path() -> PathBuf {
    crate::paths::state_root().join("settings.json")
}

#[derive(Debug, Clone)]
pub(crate) struct AppSettings {
    path: Option<PathBuf>,
    pub subtitle: SubtitleStylePrefs,
    server_bitrate_caps: HashMap<String, u32>,
    pub skip_segments: SkipSegmentPrefs,
    pub autoplay: AutoplayPrefs,
    library_view_modes: HashMap<String, LibraryViewMode>,
    dns_seed: HashMap<String, Vec<String>>,
    pub next_up: NextUpPrefs,
    home_library_hidden: HashMap<String, bool>,
    pub hide_watched_latest: bool,
    startup_screen: StartupScreen,
    /// See [`SkipLengthPrefs`].
    pub skip_length: SkipLengthPrefs,
    /// See [`LanguagePrefs`].
    pub language: LanguagePrefs,
    /// See [`SpeedBoostPrefs`]. Unwrapped to a plain `bool` here (unlike
    /// `skip_length`/`language`, exposed as their whole preset struct):
    /// every call site only wants "is the gesture on" -- see [`Self::save`]
    /// for where it gets re-wrapped.
    pub speed_boost: bool,
    /// "Preload next item" toggle -- see [`PreloadPrefs`] for why this is
    /// unwrapped to a plain `bool` (same reasoning as `speed_boost`).
    pub preload: bool,
}

impl AppSettings {
    pub(crate) fn load() -> Self {
        let path = settings_path();
        let file = std::fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<SettingsFile>(&bytes).ok())
            .unwrap_or_default();
        AppSettings {
            path: Some(path),
            subtitle: file.subtitle,
            server_bitrate_caps: file.server_bitrate_caps,
            skip_segments: file.skip_segments,
            autoplay: file.autoplay,
            library_view_modes: file.library_view_modes,
            dns_seed: file.dns_seed,
            next_up: file.next_up,
            home_library_hidden: file.home_library_hidden,
            hide_watched_latest: file.hide_watched_latest,
            startup_screen: file.startup_screen,
            skip_length: file.skip_length,
            language: file.language,
            speed_boost: file.speed_boost.enabled,
            preload: file.preload.enabled,
        }
    }

    /// Last known IP addresses for `host`, for
    /// `jellyfin_api::dns::ResolverHandle::seed` at startup -- the
    /// process-wide DNS cache starts empty every launch, so on a machine
    /// where the system resolver intermittently stalls ~5s on the server's
    /// hostname, the first lookup blocks every API call behind it.
    /// Persisting the answer means only a brand-new host can pay that
    /// stall.
    ///
    /// Parsed leniently: an unparseable entry is skipped, never an error --
    /// a wrong/empty one only costs the one ordinary blocking lookup that
    /// would have happened anyway.
    pub(crate) fn dns_seed(&self, host: &str) -> Vec<std::net::IpAddr> {
        self.dns_seed
            .get(host)
            .map(|addrs| addrs.iter().filter_map(|a| a.parse().ok()).collect())
            .unwrap_or_default()
    }

    /// Records `addrs` as `host`'s seed for the next launch. Writes (and
    /// saves) only on an actual change, so the opportunistic call sites
    /// (run on every connect) cost one map lookup in the common "nothing
    /// moved" case rather than a file write each time.
    pub(crate) fn set_dns_seed(&mut self, host: &str, addrs: &[std::net::IpAddr]) {
        if addrs.is_empty() {
            return;
        }
        let encoded: Vec<String> = addrs.iter().map(|a| a.to_string()).collect();
        if self.dns_seed.get(host) == Some(&encoded) {
            return;
        }
        self.dns_seed.insert(host.to_string(), encoded);
        self.save();
    }

    /// View toggle, persisted per library (see [`LibraryViewMode`]). An
    /// unknown/never-toggled library is `Grid`.
    pub(crate) fn library_view_mode(&self, view_id: &str) -> LibraryViewMode {
        self.library_view_modes
            .get(view_id)
            .copied()
            .unwrap_or_default()
    }

    pub(crate) fn set_library_view_mode(&mut self, view_id: &str, mode: LibraryViewMode) {
        self.library_view_modes.insert(view_id.to_string(), mode);
        self.save();
    }

    /// Per-server quality mode, decoded from the stored `u32` map:
    /// `0`/absent = Direct Play, [`MEASURED_SENTINEL`] = Measured, anything
    /// else = fixed cap in bits/sec.
    pub(crate) fn bitrate_mode(&self, base_url: &str) -> BitrateMode {
        match self.server_bitrate_caps.get(base_url).copied() {
            None | Some(0) => BitrateMode::DirectPlay,
            Some(MEASURED_SENTINEL) => BitrateMode::Measured,
            Some(cap) => BitrateMode::Cap(cap),
        }
    }

    pub(crate) fn set_bitrate_mode(&mut self, base_url: &str, mode: BitrateMode) {
        let stored = match mode {
            BitrateMode::DirectPlay => 0,
            BitrateMode::Measured => MEASURED_SENTINEL,
            BitrateMode::Cap(cap) => cap,
        };
        self.server_bitrate_caps
            .insert(base_url.to_string(), stored);
        self.save();
    }

    pub(crate) fn set_subtitle(&mut self, style: SubtitleStylePrefs) {
        self.subtitle = style;
        self.save();
    }

    pub(crate) fn set_skip_segments(&mut self, prefs: SkipSegmentPrefs) {
        self.skip_segments = prefs;
        self.save();
    }

    pub(crate) fn set_autoplay(&mut self, prefs: AutoplayPrefs) {
        self.autoplay = prefs;
        self.save();
    }

    fn save(&self) {
        let Some(path) = &self.path else { return };
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let file = SettingsFile {
            subtitle: self.subtitle,
            server_bitrate_caps: self.server_bitrate_caps.clone(),
            skip_segments: self.skip_segments,
            autoplay: self.autoplay,
            library_view_modes: self.library_view_modes.clone(),
            dns_seed: self.dns_seed.clone(),
            next_up: self.next_up,
            home_library_hidden: self.home_library_hidden.clone(),
            hide_watched_latest: self.hide_watched_latest,
            startup_screen: self.startup_screen.clone(),
            skip_length: self.skip_length,
            language: self.language.clone(),
            speed_boost: SpeedBoostPrefs {
                enabled: self.speed_boost,
            },
            preload: PreloadPrefs {
                enabled: self.preload,
            },
        };
        if let Ok(bytes) = serde_json::to_vec_pretty(&file) {
            let _ = crate::paths::write_private(path, &bytes);
        }
    }

    pub(crate) fn set_next_up(&mut self, prefs: NextUpPrefs) {
        self.next_up = prefs;
        self.save();
    }

    /// Per-library Home visibility. A library nobody has toggled reads as
    /// visible -- see [`SettingsFile::home_library_hidden`].
    pub(crate) fn library_visible_on_home(&self, view_id: &str) -> bool {
        !self
            .home_library_hidden
            .get(view_id)
            .copied()
            .unwrap_or(false)
    }

    pub(crate) fn set_library_visible_on_home(&mut self, view_id: &str, visible: bool) {
        self.home_library_hidden
            .insert(view_id.to_string(), !visible);
        self.save();
    }

    /// The full hidden set, for `home.rs`'s shelf-building/filtering pass --
    /// cheaper than one `library_visible_on_home` call per card. Only ids
    /// actually marked hidden are present (a never-toggled library is
    /// absent, i.e. visible).
    pub(crate) fn hidden_home_libraries(&self) -> std::collections::HashSet<String> {
        self.home_library_hidden
            .iter()
            .filter(|(_, hidden)| **hidden)
            .map(|(id, _)| id.clone())
            .collect()
    }

    pub(crate) fn set_hide_watched_latest(&mut self, hide: bool) {
        self.hide_watched_latest = hide;
        self.save();
    }

    /// Where to navigate right after connecting -- see [`StartupScreen`].
    /// `views` is the just-connected session's actual library list; if the
    /// stored `Library(id)` no longer names one of them (removed/renamed
    /// library, or a settings file carried over from a different server),
    /// this falls back to `Home` silently rather than landing on a dead nav
    /// target.
    ///
    /// docs/PLUGIN-CHANNELS.md §2.1: a `ViewKind::
    /// Channel` view is never a valid startup target -- it routes to the
    /// live channel browse screen, not the mirror-backed grid `Library(id)`
    /// means everywhere else. A settings file that somehow still names one
    /// fails safe to `Home`, same as a stored id that no longer resolves to
    /// any view at all.
    pub(crate) fn startup_screen(&self, views: &[media_cache::ViewSummary]) -> StartupScreen {
        match &self.startup_screen {
            StartupScreen::Library(id)
                if views
                    .iter()
                    .any(|v| &v.id == id && v.kind == media_cache::ViewKind::Library) =>
            {
                StartupScreen::Library(id.clone())
            }
            _ => StartupScreen::Home,
        }
    }

    pub(crate) fn set_startup_screen(&mut self, screen: StartupScreen) {
        self.startup_screen = screen;
        self.save();
    }

    /// See [`SkipLengthPrefs`].
    pub(crate) fn set_skip_length(&mut self, prefs: SkipLengthPrefs) {
        self.skip_length = prefs;
        self.save();
    }

    /// See [`LanguagePrefs`].
    pub(crate) fn set_language_prefs(&mut self, prefs: LanguagePrefs) {
        self.language = prefs;
        self.save();
    }

    /// See [`SpeedBoostPrefs`].
    pub(crate) fn set_speed_boost(&mut self, enabled: bool) {
        self.speed_boost = enabled;
        self.save();
    }

    /// "Preload next item" toggle change from Settings -> Playback.
    /// `Root::set_preload` is the caller that also discards any preload in
    /// flight the moment this flips off -- this method only persists the
    /// flag (mpv/live-session side effects live on `Root`, not
    /// `AppSettings`).
    pub(crate) fn set_preload(&mut self, enabled: bool) {
        self.preload = enabled;
        self.save();
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SettingsSection {
    Server,
    Playback,
    /// Next Up cutoff/rewatching, per-library Home visibility, "hide
    /// watched from Latest", and the startup screen picker -- everything
    /// that shapes what Home shows and where the app lands, grouped
    /// separately from Playback (which is about how a stream plays).
    Home,
    Subtitles,
    /// Status row, connect form, Connect/
    /// Disconnect -- Discover's own settings section.
    Discover,
    Shortcuts,
    About,
}

pub(crate) struct SettingsState {
    pub open: bool,
    pub section: SettingsSection,
}

impl SettingsState {
    pub(crate) fn new() -> Self {
        SettingsState {
            open: false,
            section: SettingsSection::Server,
        }
    }
}

impl Default for SettingsState {
    fn default() -> Self {
        Self::new()
    }
}

/// Icon table: each section gets a real vector icon in the rail.
fn section_icon_path(section: SettingsSection) -> &'static str {
    match section {
        SettingsSection::Server => "icons/server.svg",
        SettingsSection::Playback => "icons/sliders-horizontal.svg",
        // Same glyph the sidebar's Home row uses -- reads as "the settings
        // for that screen" rather than a second, unrelated icon.
        SettingsSection::Home => "icons/layout-grid.svg",
        SettingsSection::Subtitles => "icons/captions.svg",
        // No dedicated Discover glyph in the vendored Lucide subset.
        SettingsSection::Discover => "icons/search.svg",
        // No dedicated keyboard glyph in the vendored Lucide subset --
        // `list.svg` reused since this section's content is literally a list.
        SettingsSection::Shortcuts => "icons/list.svg",
        SettingsSection::About => "icons/info.svg",
    }
}

/// Sidebar rail row -- a vertical rail keeps every settings section in
/// view. Built on `ui::components::list_row` for its
/// hover/active-fill/left-accent-bar treatment.
fn section_row(
    label: &'static str,
    section: SettingsSection,
    active: SettingsSection,
    root: WeakEntity<Root>,
) -> impl IntoElement {
    let selected = section == active;
    list_row(
        SharedString::from(format!("settings-section-{label}")),
        selected,
        div()
            .flex()
            .items_center()
            .gap_2()
            .child(
                svg()
                    .path(section_icon_path(section))
                    .w(px(16.))
                    .h(px(16.))
                    .text_color(if selected {
                        rgb(theme::ACCENT)
                    } else {
                        rgba(theme::TEXT_TERTIARY)
                    }),
            )
            .child(
                div()
                    .text_size(theme::TEXT_BODY)
                    .text_color(if selected {
                        rgba(theme::TEXT_PRIMARY)
                    } else {
                        rgba(theme::TEXT_SECONDARY)
                    })
                    .child(label),
            ),
    )
    .cursor_pointer()
    .on_click(move |_event, _window, cx| {
        let _ = root.update(cx, |root, cx| root.set_settings_section(section, cx));
    })
}

type PickHandler<T> = std::rc::Rc<dyn Fn(T, &mut gpui::App)>;

/// Thin wrapper over `ui::components::chip_button` (shared segmented-preset
/// chip, sized to match `button()`'s `Sm` row) that adds the
/// click-dispatches-a-value behavior every preset row here needs.
fn preset_button<T: Clone + PartialEq + Send + Sync + 'static>(
    id: SharedString,
    label: SharedString,
    value: T,
    active: bool,
    on_pick: PickHandler<T>,
) -> impl IntoElement {
    chip_button(id, label, active).on_click(move |_event, _window, cx| on_pick(value.clone(), cx))
}

/// Sidebar+content sheet: a `680px`-wide panel split into a left rail
/// (`section_row`, per-section icon + left accent-bar active state) and a
/// flexible content pane, so the section list never scrolls away.
/// Built on `ui::components::dialog_scrim`/`dialog_panel`.
///
/// `views` is threaded through for the Home section's per-library
/// visibility list and startup-screen picker, which need this session's
/// actual library list -- same "one wide render entry point, not a struct
/// wrapper" shape as `home.rs::render_shelf`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn render(
    settings: &SettingsState,
    sessions: &StoredSessionList,
    app_settings: &AppSettings,
    current_base_url: &str,
    // The current session's libraries, verbatim server names -- same source
    // the sidebar nav reads, threaded through so the Home section's
    // per-library visibility list and startup-screen picker don't invent
    // their own copy of "what are this server's libraries". Includes
    // Channel views (docs/PLUGIN-CHANNELS.md §2.1); this
    // panel doesn't branch on `ViewKind`, so a Channel view shows up as an
    // ordinary toggle/preset row.
    views: &[media_cache::ViewSummary],
    // The panel caps itself at 80% of the actual window height instead of a
    // guessed constant, so a short window doesn't overflow the bottom edge.
    viewport_height: gpui::Pixels,
    // The local-file-only status this section
    // shows, and the always-present connect form.
    seerr_status: &seerr_api::SeerrStatus,
    discover_connect: &discover::DiscoverConnectFormState,
    root: WeakEntity<Root>,
    _cx: &mut Context<Root>,
) -> impl IntoElement {
    let rail = div()
        // 190px: "Server & Account" (the widest label) + 16px icon + gaps +
        // list_row padding needs ~185px at TEXT_BODY.
        .w(px(190.))
        .flex_shrink_0()
        .h_full()
        .bg(rgb(theme::SURFACE_RAISED))
        .p_1()
        .flex()
        .flex_col()
        .gap_0p5()
        .child(section_row(
            "Server & Account",
            SettingsSection::Server,
            settings.section,
            root.clone(),
        ))
        .child(section_row(
            "Playback",
            SettingsSection::Playback,
            settings.section,
            root.clone(),
        ))
        .child(section_row(
            "Home",
            SettingsSection::Home,
            settings.section,
            root.clone(),
        ))
        .child(section_row(
            "Subtitles",
            SettingsSection::Subtitles,
            settings.section,
            root.clone(),
        ))
        .child(section_row(
            "Discover",
            SettingsSection::Discover,
            settings.section,
            root.clone(),
        ))
        .child(section_row(
            "Shortcuts",
            SettingsSection::Shortcuts,
            settings.section,
            root.clone(),
        ))
        .child(section_row(
            "About",
            SettingsSection::About,
            settings.section,
            root.clone(),
        ));

    let body = match settings.section {
        SettingsSection::Server => render_server_section(sessions, root.clone()).into_any_element(),
        SettingsSection::Playback => {
            render_playback_section(app_settings, current_base_url, root.clone()).into_any_element()
        }
        SettingsSection::Home => {
            render_home_section(app_settings, views, root.clone()).into_any_element()
        }
        SettingsSection::Subtitles => {
            render_subtitles_section(app_settings, root.clone()).into_any_element()
        }
        SettingsSection::Discover => {
            render_discover_section(seerr_status, discover_connect, root.clone()).into_any_element()
        }
        SettingsSection::Shortcuts => render_shortcuts_section(app_settings).into_any_element(),
        SettingsSection::About => render_about_section().into_any_element(),
    };

    let close_root = root.clone();
    let dismiss_root = root.clone();

    dialog_scrim(
        "settings-overlay",
        Some(move |cx: &mut gpui::App| {
            let _ = dismiss_root.update(cx, |root, cx| root.close_settings(cx));
        }),
        dialog_panel(
            "settings-panel",
            px(680.),
            div()
                // The sheet is one constant size regardless of which section
                // is open, so switching sections never resizes the panel:
                // 640px, clamped to 80% of the window for small windows.
                // Tall panes scroll inside `#settings-body` (bottom fade
                // mask as the "there's more" affordance); short panes leave
                // empty space instead of collapsing the frame.
                .h((viewport_height * 0.8).min(px(640.)))
                .flex()
                .flex_col()
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .justify_between()
                        .px(theme::SPACE_DEFAULT)
                        .py_3()
                        // Dialog anatomy: header -> hairline divider -> body.
                        .border_b_1()
                        .border_color(rgb(theme::SURFACE_HAIRLINE))
                        // Title role (28px/Bold) for a screen-level header
                        // with no hero, not the smaller Section role
                        // `section_title()` provides (shelf/sub-section
                        // titles).
                        .child(
                            div()
                                .text_size(theme::TEXT_TITLE)
                                .font_weight(gpui::FontWeight::BOLD)
                                .text_color(rgba(theme::TEXT_PRIMARY))
                                .child("Settings"),
                        )
                        .child(
                            div()
                                .id("settings-close")
                                .flex()
                                .items_center()
                                .justify_center()
                                .size(px(28.))
                                .rounded_md()
                                .cursor_pointer()
                                .hover(|s| s.bg(rgb(theme::SURFACE_OVERLAY)))
                                .child(
                                    svg()
                                        .path("icons/x.svg")
                                        .w(px(16.))
                                        .h(px(16.))
                                        .text_color(rgba(theme::TEXT_SECONDARY)),
                                )
                                .on_click(move |_event, _window, cx| {
                                    let _ =
                                        close_root.update(cx, |root, cx| root.close_settings(cx));
                                }),
                        ),
                )
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .flex_1()
                        .min_h_0()
                        .child(rail)
                        .child(
                            // `.relative()` wrapper so the bottom fade mask
                            // below can paint as an absolutely positioned
                            // sibling above the scrolling body, pinned to
                            // the pane's bottom edge regardless of scroll.
                            div()
                                .relative()
                                .flex_1()
                                .min_w_0()
                                .min_h_0()
                                .child(
                                    div()
                                        .id("settings-body")
                                        .size_full()
                                        // Without min_w_0, a child that
                                        // refuses to shrink (a wide preset
                                        // cluster) widens the body past the
                                        // fixed panel width, and full-width
                                        // siblings clip at the panel edge
                                        // mid-word.
                                        .min_w_0()
                                        .flex()
                                        .flex_col()
                                        .p(theme::SPACE_DEFAULT)
                                        .gap_2()
                                        .overflow_y_scroll()
                                        .child(body),
                                )
                                // §6: a ~24px bottom fade (transparent ->
                                // `SURFACE_PANEL`, the pane's own bg) as the
                                // "there's more below, scroll" affordance --
                                // the Playback tab used to overflow the
                                // panel's bottom edge with a hard clip and no
                                // hint at all that anything was cut off.
                                // Always painted here regardless of whether
                                // this section's content actually overflows
                                // (matches the sidebar/hero fade bands
                                // elsewhere, which are likewise
                                // unconditional -- a short section just has
                                // an inert fade over empty space).
                                .child(
                                    div()
                                        .absolute()
                                        .bottom_0()
                                        .left_0()
                                        .right_0()
                                        .h(px(24.))
                                        .bg(linear_gradient(
                                            180.,
                                            linear_color_stop(rgba(theme::TRANSPARENT), 0.0),
                                            linear_color_stop(rgb(theme::SURFACE_PANEL), 1.0),
                                        )),
                                ),
                        ),
                ),
        ),
    )
}

/// B.7 glyph retirement: the `●`/`○` active-session dot pair
/// becomes `ui::components::status_dot` (a real styled div, not a glyph),
/// and the plain-text "Switch"/"Remove" actions become `ui::components::
/// button` (Secondary/GhostDanger, `sm`). Row container is `list_row` for
/// the same hover/active-fill/left-accent-bar treatment every other list
/// surface uses.
fn render_server_section(sessions: &StoredSessionList, root: WeakEntity<Root>) -> impl IntoElement {
    let rows = sessions
        .sessions
        .iter()
        .enumerate()
        .map(|(ix, session)| {
            let active = ix == sessions.active;
            let label = session
                .username
                .clone()
                .map(|u| format!("{u}  —  {}", session.base_url))
                .unwrap_or_else(|| session.base_url.clone());
            let switch_root = root.clone();
            let remove_root = root.clone();
            list_row(
                SharedString::from(format!("session-{ix}")),
                active,
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .w_full()
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_2()
                            .flex_1()
                            .min_w_0()
                            .text_color(if active {
                                rgba(theme::TEXT_PRIMARY)
                            } else {
                                rgba(theme::TEXT_SECONDARY)
                            })
                            .child(status_dot(active))
                            .child(
                                // A long server URL must truncate with an
                                // ellipsis instead of pushing/overlapping
                                // the Switch/Remove buttons.
                                clamped_line(label, px(20.)).flex_1().min_w_0(),
                            ),
                    )
                    .child(
                        // Fixed-width, right-aligned, never-shrinking column
                        // for Switch/Remove, reserved on every row (even
                        // active ones, which render no buttons into it) so
                        // every row's label column is the same width.
                        div()
                            .flex_shrink_0()
                            .w(px(168.))
                            .flex()
                            .flex_row()
                            .justify_end()
                            .gap_2()
                            .when(!active, |d| {
                                d.child(
                                    button(
                                        SharedString::from(format!("switch-{ix}")),
                                        "Switch",
                                        ButtonVariant::Secondary,
                                        ButtonSize::Sm,
                                        false,
                                    )
                                    .on_click(
                                        move |_event, _window, cx| {
                                            let _ = switch_root.update(cx, |root, cx| {
                                                root.switch_to_session(ix, cx)
                                            });
                                        },
                                    ),
                                )
                                .child(
                                    button(
                                        SharedString::from(format!("remove-{ix}")),
                                        "Remove",
                                        ButtonVariant::GhostDanger,
                                        ButtonSize::Sm,
                                        false,
                                    )
                                    .on_click(
                                        move |_event, _window, cx| {
                                            let _ = remove_root
                                                .update(cx, |root, cx| root.remove_session(ix, cx));
                                        },
                                    ),
                                )
                            }),
                    ),
            )
        })
        .collect::<Vec<_>>();

    let add_root = root.clone();
    div()
        .flex()
        .flex_col()
        .gap_1()
        .child(dense_label("Signed in"))
        .children(rows)
        .child(
            // Built directly rather than through `button()` (whose single-
            // `label`-child API has no leading-icon slot) so `plus.svg` can
            // lead the label.
            //
            // Secondary/outline style, not accent-filled: adding a server
            // is a low-frequency setup action, not the app's one primary
            // action. Mirrors `button()`'s own `Secondary` variant exactly.
            div()
                .id("add-server")
                .mt_3()
                .flex()
                .items_center()
                .justify_center()
                .gap_1()
                .h(px(36.))
                .px(px(16.))
                // Brand §5: "Buttons -- fully rounded, 999px", and the
                // Secondary variant is transparent with a hairline border
                // rather than a `surface.panel` fill.
                .rounded(theme::RADIUS_PILL)
                .cursor_pointer()
                .bg(rgba(theme::TRANSPARENT))
                .border_1()
                .border_color(rgb(theme::SURFACE_HAIRLINE))
                .hover(|s| s.bg(rgb(theme::SURFACE_RAISED)))
                .child(
                    svg()
                        .path("icons/plus.svg")
                        .w(px(14.))
                        .h(px(14.))
                        .text_color(rgba(theme::TEXT_PRIMARY)),
                )
                .child(
                    div()
                        .text_size(theme::TEXT_BODY)
                        .text_color(rgba(theme::TEXT_PRIMARY))
                        .child("Add server"),
                )
                .on_click(move |_event, _window, cx| {
                    let _ = add_root.update(cx, |root, cx| root.start_add_server(cx));
                }),
        )
}

/// The Playback section's full content: max streaming bitrate, "Skip
/// segments", and "Autoplay next episode". Every group below is always
/// visible -- no "Show Advanced" disclosure gate.
fn render_playback_section(
    app_settings: &AppSettings,
    current_base_url: &str,
    root: WeakEntity<Root>,
) -> impl IntoElement {
    let current_mode = app_settings.bitrate_mode(current_base_url);
    let base_url = current_base_url.to_string();
    let on_pick: PickHandler<BitrateMode> = {
        let root = root.clone();
        let base_url = base_url.clone();
        std::rc::Rc::new(move |mode: BitrateMode, cx: &mut gpui::App| {
            let _ = root.clone().update(cx, |root, cx| {
                root.set_bitrate_mode(base_url.clone(), mode, cx)
            });
        })
    };

    let buttons = BITRATE_PRESETS
        .iter()
        .map(|(label, mode)| {
            preset_button(
                SharedString::from(format!("bitrate-{label}")),
                SharedString::from(*label),
                *mode,
                *mode == current_mode,
                on_pick.clone(),
            )
        })
        .collect::<Vec<_>>();

    div()
        .flex()
        .flex_col()
        .gap_2()
        // Stacked block, not `form_row_desc`: five mode buttons beside a
        // label is wider than the panel's content column at any reasonable
        // panel size, and a `flex_shrink_0` control slot just clips.
        .child(
            div()
                .flex()
                .flex_col()
                .gap_1p5()
                .child(
                    div()
                        .text_size(theme::TEXT_BODY)
                        .text_color(rgba(theme::TEXT_PRIMARY))
                        .child("Playback quality"),
                )
                .child(
                    div()
                        .w_full()
                        .min_w_0()
                        .flex()
                        .flex_row()
                        .flex_wrap()
                        .gap_1()
                        .children(buttons),
                )
                .child(
                    div()
                        .w_full()
                        .min_w_0()
                        .text_size(theme::TEXT_CAPTION)
                        .text_color(rgba(theme::TEXT_TERTIARY))
                        .child(SharedString::from(format!(
                            "Direct Play never asks {current_base_url} to transcode, whatever \
                             your connection. Auto measures the link on first play and \
                             transcodes only files that exceed it; a bitrate preset forces a \
                             fixed cap. Files under the cap always Direct Play untouched.",
                        ))),
                ),
        )
        .child(render_skip_segments_section(
            app_settings.skip_segments,
            root.clone(),
        ))
        .child(render_autoplay_section(app_settings.autoplay, root.clone()))
        .child(render_skip_length_section(
            app_settings.skip_length,
            root.clone(),
        ))
        .child(render_speed_boost_section(
            app_settings.speed_boost,
            root.clone(),
        ))
        .child(render_preload_section(app_settings.preload, root))
}

/// Separate back/forward skip-length preset rows -- same segmented-preset-
/// row shape `render_autoplay_section`'s delay row uses. Writes through
/// `Root::set_skip_length` -> `AppSettings::set_skip_length` and, if a
/// session is playing, onto that session's own `PlayerUiState::
/// skip_back_secs`/`skip_forward_secs` so a mid-playback change takes
/// effect on the next arrow-key press or OSD skip-button click.
fn render_skip_length_section(prefs: SkipLengthPrefs, root: WeakEntity<Root>) -> impl IntoElement {
    let back_root = root.clone();
    let on_back: PickHandler<u32> = std::rc::Rc::new(move |back_secs: u32, cx| {
        let next = SkipLengthPrefs { back_secs, ..prefs };
        let _ = back_root
            .clone()
            .update(cx, |root, cx| root.set_skip_length(next, cx));
    });
    let back_buttons = SKIP_LENGTH_PRESETS
        .iter()
        .map(|secs| {
            preset_button(
                SharedString::from(format!("skip-back-{secs}")),
                SharedString::from(format!("{secs}s")),
                *secs,
                *secs == prefs.back_secs,
                on_back.clone(),
            )
        })
        .collect::<Vec<_>>();

    let fwd_root = root;
    let on_fwd: PickHandler<u32> = std::rc::Rc::new(move |forward_secs: u32, cx| {
        let next = SkipLengthPrefs {
            forward_secs,
            ..prefs
        };
        let _ = fwd_root
            .clone()
            .update(cx, |root, cx| root.set_skip_length(next, cx));
    });
    let fwd_buttons = SKIP_LENGTH_PRESETS
        .iter()
        .map(|secs| {
            preset_button(
                SharedString::from(format!("skip-forward-{secs}")),
                SharedString::from(format!("{secs}s")),
                *secs,
                *secs == prefs.forward_secs,
                on_fwd.clone(),
            )
        })
        .collect::<Vec<_>>();

    div()
        .flex()
        .flex_col()
        .gap_1()
        .child(dense_label("Skip length"))
        .child(form_row(
            "Back (←)",
            div()
                .flex()
                .flex_row()
                .flex_wrap()
                .gap_1()
                .children(back_buttons),
        ))
        .child(form_row(
            "Forward (→)",
            div()
                .flex()
                .flex_row()
                .flex_wrap()
                .gap_1()
                .children(fwd_buttons),
        ))
        .child(
            div()
                .mt_1()
                .text_size(theme::TEXT_METADATA)
                .text_color(rgba(theme::TEXT_QUATERNARY))
                .child(
                    "⇧←/⇧→ always jump a fixed ±60s regardless of these presets -- that's meant \
                     to read as a distinctly bigger skip than the plain arrow keys.",
                ),
        )
}

/// Builds a `toggle_switch` wired to flip a `Root`-side setting on click --
/// shared behind five Playback/Home on/off toggles (speed boost, preload,
/// autoplay, hide-watched-latest, next-up rewatching). `on_toggle` receives
/// the *new* (post-flip) value, since some callers fold it into a larger
/// prefs struct (`AutoplayPrefs`/`NextUpPrefs`) rather than pass a bare
/// bool straight through. Returned bare (not wrapped in a row) because
/// Autoplay's main toggle has no description of its own -- see
/// `labeled_toggle_row` below for the other four, which do.
fn wired_toggle(
    id: impl Into<gpui::ElementId>,
    checked: bool,
    root: WeakEntity<Root>,
    on_toggle: impl Fn(&mut Root, bool, &mut Context<Root>) + 'static,
) -> impl IntoElement {
    toggle_switch(id, checked).on_click(move |_event, _window, cx| {
        let _ = root
            .clone()
            .update(cx, |root, cx| on_toggle(root, !checked, cx));
    })
}

/// `wired_toggle` plus the label/desc `form_row_desc` wrapping shared by
/// its other four call sites (speed boost/preload/hide-watched-latest/
/// next-up-rewatching).
fn labeled_toggle_row(
    id: impl Into<gpui::ElementId>,
    checked: bool,
    label: &'static str,
    desc: &'static str,
    root: WeakEntity<Root>,
    on_toggle: impl Fn(&mut Root, bool, &mut Context<Root>) + 'static,
) -> impl IntoElement {
    form_row_desc(label, wired_toggle(id, checked, root, on_toggle), desc)
}

/// Same single-toggle-row shape as `render_autoplay_section`'s on/off row.
fn render_speed_boost_section(enabled: bool, root: WeakEntity<Root>) -> impl IntoElement {
    labeled_toggle_row(
        "speed-boost-toggle",
        enabled,
        "Hold Option to change speed",
        "Hold the right ⌥ key to play at 2× speed, or the left ⌥ key for 0.5×; release to \
         resume at normal speed. Space still just plays/pauses.",
        root,
        |root, checked, cx| root.set_speed_boost(checked, cx),
    )
}

/// Same single-toggle-row shape as `render_speed_boost_section`. Writes
/// through `Root::set_preload` -> `AppSettings::set_preload`, which also
/// discards any preload currently in flight the moment this flips off, so
/// turning it off mid-browse doesn't leave a stray dark stream open in mpv.
fn render_preload_section(enabled: bool, root: WeakEntity<Root>) -> impl IntoElement {
    labeled_toggle_row(
        "preload-toggle",
        enabled,
        "Preload next item",
        "Quietly buffers the likely next item so Play starts instantly. Uses bandwidth while \
         browsing.",
        root,
        |root, checked, cx| root.set_preload(checked, cx),
    )
}

/// One row per `MediaSegmentType` the server can return
/// (`SkipSegmentPrefs`'s own five fields), each a 3-way Ask/Auto/Off
/// segmented control built from `preset_button` (GPUI 0.2.2 has no
/// radio-group primitive). Values write straight through
/// `Root::set_skip_segment_action` -> `AppSettings::set_skip_segments` and,
/// if a session is playing, onto that session's own
/// `PlayerUiState::skip_segment_prefs` so a mid-playback change takes
/// effect on the next segment.
fn render_skip_segments_section(
    prefs: SkipSegmentPrefs,
    root: WeakEntity<Root>,
) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .child(dense_label("Skip segments"))
        .child(skip_segment_row(
            "Intro",
            MediaSegmentType::Intro,
            prefs.intro,
            root.clone(),
        ))
        .child(skip_segment_row(
            "Outro / Credits",
            MediaSegmentType::Outro,
            prefs.outro,
            root.clone(),
        ))
        .child(skip_segment_row(
            "Recap",
            MediaSegmentType::Recap,
            prefs.recap,
            root.clone(),
        ))
        .child(skip_segment_row(
            "Preview",
            MediaSegmentType::Preview,
            prefs.preview,
            root.clone(),
        ))
        .child(skip_segment_row(
            "Commercial",
            MediaSegmentType::Commercial,
            prefs.commercial,
            root,
        ))
        .child(
            div()
                .mt_1()
                .text_size(theme::TEXT_METADATA)
                .text_color(rgba(theme::TEXT_QUATERNARY))
                .child(
                    "Ask shows a pill to confirm each time; Auto skips instantly with a 5s Undo; \
                     Off never touches that segment type.",
                ),
        )
}

/// Built on the shared `form_row` grid -- same label-left/control-right row
/// every other Playback/Subtitles row uses.
fn skip_segment_row(
    label: &'static str,
    ty: MediaSegmentType,
    current: SegmentAction,
    root: WeakEntity<Root>,
) -> impl IntoElement {
    let on_pick: PickHandler<SegmentAction> = {
        let root = root.clone();
        std::rc::Rc::new(move |action: SegmentAction, cx: &mut gpui::App| {
            let _ = root
                .clone()
                .update(cx, |root, cx| root.set_skip_segment_action(ty, action, cx));
        })
    };
    let seg_button = |action: SegmentAction, text: &'static str| {
        preset_button(
            SharedString::from(format!("skip-{label}-{text}")),
            SharedString::from(text),
            action,
            action == current,
            on_pick.clone(),
        )
    };
    form_row(
        label,
        div()
            .flex()
            .flex_row()
            .flex_wrap()
            .gap_1()
            .child(seg_button(SegmentAction::Ask, "Ask"))
            .child(seg_button(SegmentAction::AutoSkip, "Auto"))
            .child(seg_button(SegmentAction::Off, "Off")),
    )
}

/// "Autoplay next episode" toggle + delay (5/10/15s, segmented). The
/// next-episode card (`player_ui.rs::render_next_episode_card`) always
/// appears regardless of this setting -- the toggle only gates the
/// countdown/auto-advance behavior, never the card's own visibility.
///
/// The on/off toggle and delay presets are separate rows; the delay row
/// dims to 40% opacity and drops its `on_click` handlers entirely (not
/// just visually disabling) when autoplay is off, so a click on a dimmed
/// chip is a genuine no-op.
fn render_autoplay_section(prefs: AutoplayPrefs, root: WeakEntity<Root>) -> impl IntoElement {
    let toggle = wired_toggle(
        "autoplay-toggle",
        prefs.enabled,
        root.clone(),
        move |root, checked, cx| {
            let toggled = AutoplayPrefs {
                enabled: checked,
                ..prefs
            };
            root.set_autoplay_prefs(toggled, cx);
        },
    );

    let delay_root = root;
    let on_delay: PickHandler<u32> = std::rc::Rc::new(move |secs: u32, cx: &mut gpui::App| {
        let next = AutoplayPrefs {
            delay_secs: secs,
            ..prefs
        };
        let _ = delay_root
            .clone()
            .update(cx, |root, cx| root.set_autoplay_prefs(next, cx));
    });
    let enabled = prefs.enabled;
    let delay_buttons = AUTOPLAY_DELAY_PRESETS
        .iter()
        .map(|secs| {
            let secs = *secs;
            let chip = chip_button(
                SharedString::from(format!("autoplay-delay-{secs}")),
                SharedString::from(format!("{secs}s")),
                secs == prefs.delay_secs,
            );
            if enabled {
                let on_delay = on_delay.clone();
                chip.on_click(move |_event, _window, cx| on_delay(secs, cx))
            } else {
                chip.cursor_default()
            }
        })
        .collect::<Vec<_>>();

    div()
        .flex()
        .flex_col()
        .gap_1()
        .child(form_row("Autoplay next episode", toggle))
        .child(form_row_desc(
            "Delay",
            div()
                .flex()
                .flex_row()
                .flex_wrap()
                .gap_1()
                .when(!enabled, |d| d.opacity(0.4))
                .children(delay_buttons),
            "When off, the Up Next card still appears near the end of an episode, but won't count \
             down or auto-advance -- click Play Next yourself.",
        ))
}

/// The "Home" section's full content: Next Up cutoff/rewatching,
/// per-library Home visibility, "hide watched from Latest", and the
/// startup screen picker -- see each sub-fn/`AppSettings` field's own doc
/// comment for the persisted shape. `views` is this session's actual
/// library list; the per-library visibility list and startup-screen
/// picker's options are simply empty if it is.
fn render_home_section(
    app_settings: &AppSettings,
    views: &[media_cache::ViewSummary],
    root: WeakEntity<Root>,
) -> impl IntoElement {
    let hide_watched = app_settings.hide_watched_latest;
    let hide_watched_row = labeled_toggle_row(
        "hide-watched-latest-toggle",
        hide_watched,
        "Hide watched from Latest",
        "Already-watched items no longer appear in any \"Latest in X\" shelf. Continue \
         Watching and Next Up are unaffected -- both already show only unwatched/in-progress \
         items.",
        root.clone(),
        |root, checked, cx| root.set_hide_watched_latest(checked, cx),
    );

    div()
        .flex()
        .flex_col()
        .gap_2()
        .child(render_next_up_section(app_settings.next_up, root.clone()))
        .child(render_library_visibility_section(
            app_settings,
            views,
            root.clone(),
        ))
        .child(hide_watched_row)
        .child(render_startup_screen_section(app_settings, views, root))
}

/// Cutoff preset row + rewatching toggle, same segmented-preset-row +
/// `toggle_switch` shapes `render_playback_section`'s bitrate/autoplay
/// rows use.
fn render_next_up_section(prefs: NextUpPrefs, root: WeakEntity<Root>) -> impl IntoElement {
    let cutoff_root = root.clone();
    let on_cutoff: PickHandler<Option<u32>> = std::rc::Rc::new(move |cutoff_days, cx| {
        let next = NextUpPrefs {
            cutoff_days,
            ..prefs
        };
        let _ = cutoff_root
            .clone()
            .update(cx, |root, cx| root.set_next_up_prefs(next, cx));
    });
    let cutoff_buttons = NEXT_UP_CUTOFF_PRESETS
        .iter()
        .map(|(label, days)| {
            preset_button(
                SharedString::from(format!("next-up-cutoff-{label}")),
                SharedString::from(*label),
                *days,
                *days == prefs.cutoff_days,
                on_cutoff.clone(),
            )
        })
        .collect::<Vec<_>>();

    let rewatch_row = labeled_toggle_row(
        "next-up-rewatching-toggle",
        prefs.rewatching,
        "Include rewatches",
        "When on, a series you're rewatching from the start counts toward Next Up too, not \
         just a series with genuinely unwatched episodes.",
        root,
        move |root, checked, cx| {
            let toggled = NextUpPrefs {
                rewatching: checked,
                ..prefs
            };
            root.set_next_up_prefs(toggled, cx);
        },
    );

    div()
        .flex()
        .flex_col()
        .gap_1()
        .child(dense_label("Next Up"))
        .child(form_row_desc(
            "Cutoff",
            div()
                .flex()
                .flex_row()
                .flex_wrap()
                .gap_1()
                .children(cutoff_buttons),
            "Only series with unwatched content added within this window appear in Next Up. \
             Off (default) shows everything, unfiltered.",
        ))
        .child(rewatch_row)
}

/// One row per library, verbatim server name (never rewritten -- same rule
/// `root.rs`'s sidebar uses for this data), each with its own on/off
/// toggle. Not built on `list_row`: there's no "selected" row concept
/// here, every row is an independent toggle.
fn render_library_visibility_section(
    app_settings: &AppSettings,
    views: &[media_cache::ViewSummary],
    root: WeakEntity<Root>,
) -> impl IntoElement {
    let rows = views
        .iter()
        .map(|view| {
            let id = &view.id;
            let name = &view.name;
            let visible = app_settings.library_visible_on_home(id);
            let view_id = id.clone();
            let toggle_root = root.clone();
            // `form_row`'s own label column already truncates -- no need for
            // a separate `clamped_line` wrapper the way the Server section's
            // free-form row content needs one.
            form_row(
                name.clone(),
                toggle_switch(
                    SharedString::from(format!("home-library-toggle-{id}")),
                    visible,
                )
                .on_click(move |_event, _window, cx| {
                    let _ = toggle_root.update(cx, |root, cx| {
                        root.set_library_visible_on_home(view_id.clone(), !visible, cx)
                    });
                }),
            )
        })
        .collect::<Vec<_>>();

    div()
        .flex()
        .flex_col()
        .gap_1()
        .child(dense_label("Show on Home"))
        .children(rows)
        .child(
            div()
                .mt_1()
                .text_size(theme::TEXT_METADATA)
                .text_color(rgba(theme::TEXT_QUATERNARY))
                .child(
                    "A hidden library's \"Latest\" shelf disappears from Home, and its items no \
                     longer appear in Continue Watching or Next Up.",
                ),
        )
}

/// Home + one preset button per library -- same segmented-preset shape as
/// [`render_next_up_section`]'s cutoff row, with a dynamic per-session
/// option list instead of a fixed const array.
fn render_startup_screen_section(
    app_settings: &AppSettings,
    views: &[media_cache::ViewSummary],
    root: WeakEntity<Root>,
) -> impl IntoElement {
    let current = app_settings.startup_screen(views);
    let on_pick: PickHandler<StartupScreen> = {
        let root = root.clone();
        std::rc::Rc::new(move |screen: StartupScreen, cx: &mut gpui::App| {
            let _ = root
                .clone()
                .update(cx, |root, cx| root.set_startup_screen(screen, cx));
        })
    };

    let mut options: Vec<(SharedString, SharedString, StartupScreen)> = vec![(
        SharedString::from("startup-screen-home"),
        SharedString::from("Home"),
        StartupScreen::Home,
    )];
    // docs/PLUGIN-CHANNELS.md §2.1: a Channel view routes
    // to the live channel browse screen, never the mirror-backed grid
    // `StartupScreen::Library(id)` targets -- offering it here would build a
    // dead option a click could never actually select.
    options.extend(
        views
            .iter()
            .filter(|view| view.kind == media_cache::ViewKind::Library)
            .map(|view| {
                (
                    SharedString::from(format!("startup-screen-{}", view.id)),
                    SharedString::from(view.name.clone()),
                    StartupScreen::Library(view.id.clone()),
                )
            }),
    );
    let buttons = options
        .into_iter()
        .map(|(id, label, value)| {
            let active = current == value;
            preset_button(id, label, value, active, on_pick.clone())
        })
        .collect::<Vec<_>>();

    form_row_desc(
        "Startup screen",
        div()
            .flex()
            .flex_row()
            .flex_wrap()
            .gap_1()
            .children(buttons),
        "Where Jellybeam lands right after connecting. If the chosen library no longer exists, \
         this falls back to Home.",
    )
}

fn render_subtitles_section(
    app_settings: &AppSettings,
    root: WeakEntity<Root>,
) -> impl IntoElement {
    let style = app_settings.subtitle;

    let scale_root = root.clone();
    let on_scale: PickHandler<u32> = std::rc::Rc::new(move |scale_pct: u32, cx| {
        let mut next = style;
        next.scale = scale_pct as f64 / 100.0;
        let _ = scale_root
            .clone()
            .update(cx, |root, cx| root.set_subtitle_style(next, cx));
    });
    let scale_buttons = SCALE_PRESETS
        .iter()
        .map(|s| {
            let pct = (*s * 100.0).round() as u32;
            preset_button(
                SharedString::from(format!("sub-scale-{pct}")),
                SharedString::from(format!("{pct}%")),
                pct,
                (style.scale - *s).abs() < 0.01,
                on_scale.clone(),
            )
        })
        .collect::<Vec<_>>();

    let pos_root = root.clone();
    let on_pos: PickHandler<Option<i64>> = std::rc::Rc::new(move |pos, cx| {
        let mut next = style;
        next.pos = pos;
        let _ = pos_root
            .clone()
            .update(cx, |root, cx| root.set_subtitle_style(next, cx));
    });
    let pos_buttons = POS_PRESETS
        .iter()
        .map(|(label, pos)| {
            preset_button(
                SharedString::from(format!("sub-pos-{label}")),
                SharedString::from(*label),
                *pos,
                // Compared through `effective_pos` so a `settings.json`
                // still holding the old top-half ladder's value lights up
                // the step it migrated onto, rather than showing none.
                style.effective_pos() == *pos,
                on_pos.clone(),
            )
        })
        .collect::<Vec<_>>();

    let bold_root = root.clone();
    let bold_button = {
        let mut next = style;
        next.bold = !style.bold;
        chip_button(
            "sub-bold-toggle",
            if style.bold { "On" } else { "Off" },
            style.bold,
        )
        .on_click(move |_event, _window, cx| {
            let _ = bold_root.update(cx, |root, cx| root.set_subtitle_style(next, cx));
        })
    };

    let alpha_root = root.clone();
    let on_alpha: PickHandler<u32> = std::rc::Rc::new(move |pct: u32, cx| {
        let mut next = style;
        next.back_alpha = pct as f64 / 100.0;
        let _ = alpha_root
            .clone()
            .update(cx, |root, cx| root.set_subtitle_style(next, cx));
    });
    let alpha_buttons = BACK_ALPHA_PRESETS
        .iter()
        .map(|a| {
            let pct = (*a * 100.0).round() as u32;
            preset_button(
                SharedString::from(format!("sub-alpha-{pct}")),
                SharedString::from(format!("{pct}%")),
                pct,
                (style.back_alpha - *a).abs() < 0.01,
                on_alpha.clone(),
            )
        })
        .collect::<Vec<_>>();

    div()
        .flex()
        .flex_col()
        .child(form_row(
            "Size",
            div()
                .flex()
                .flex_row()
                .flex_wrap()
                .gap_1()
                .children(scale_buttons),
        ))
        .child(form_row(
            "Vertical position",
            div()
                .flex()
                .flex_row()
                .flex_wrap()
                .gap_1()
                .children(pos_buttons),
        ))
        .child(form_row("Bold text", bold_button))
        .child(form_row(
            "Background opacity",
            div()
                .flex()
                .flex_row()
                .flex_wrap()
                .gap_1()
                .children(alpha_buttons),
        ))
        .child(
            div()
                .mt_2()
                .text_size(theme::TEXT_METADATA)
                .text_color(rgba(theme::TEXT_QUATERNARY))
                .child("Applies immediately to the current playback session, and to every session after."),
        )
        .child(render_language_prefs_section(app_settings.language.clone(), root))
}

/// Status row, URL / method chips / identity /
/// secret fields (the house `TextInput` recipe), Connect (spinner + inline
/// error), Disconnect. Every value writes through on Connect only -- there
/// is no separate Save action, and nothing calls Seerr until Connect is
/// clicked.
fn render_discover_section(
    status: &seerr_api::SeerrStatus,
    form: &discover::DiscoverConnectFormState,
    root: WeakEntity<Root>,
) -> impl IntoElement {
    let status_row = div()
        .flex()
        .flex_row()
        .items_center()
        .gap_2()
        .child(status_dot(status.configured))
        .child(
            div()
                .text_size(theme::TEXT_BODY)
                .text_color(rgba(theme::TEXT_PRIMARY))
                .child(if status.configured {
                    status
                        .app_title
                        .clone()
                        .or_else(|| status.seerr_url.clone())
                        .unwrap_or_else(|| "Connected".to_string())
                } else {
                    "Not connected".to_string()
                }),
        );

    let method_row = {
        let method_chip = |label: &'static str, value: seerr_api::SeerrAuthMethod| {
            let active = form.method == value;
            let root = root.clone();
            chip_button(
                SharedString::from(format!("discover-method-{label}")),
                label,
                active,
            )
            .on_click(move |_event, _window, cx| {
                let _ = root
                    .clone()
                    .update(cx, |root, cx| root.set_discover_connect_method(value, cx));
            })
        };
        div()
            .flex()
            .flex_row()
            .gap_2()
            .child(method_chip(
                "Jellyfin",
                seerr_api::SeerrAuthMethod::Jellyfin,
            ))
            .child(method_chip("Local", seerr_api::SeerrAuthMethod::Local))
            .child(method_chip("API Key", seerr_api::SeerrAuthMethod::ApiKey))
    };

    let identity_row = discover::identity_field_visible(form.method)
        .then(|| form_row(discover::identity_label(form.method), form.identity.clone()));

    let connect_root = root.clone();
    let connect_button = button(
        "discover-connect",
        if form.connecting {
            "Connecting..."
        } else {
            "Connect"
        },
        ButtonVariant::Primary,
        ButtonSize::Md,
        form.connecting,
    )
    .on_click(move |_event, _window, cx| {
        let _ = connect_root.update(cx, |root, cx| root.connect_discover(cx));
    });

    let disconnect_button = status.configured.then(|| {
        let root = root.clone();
        button(
            "discover-disconnect",
            "Disconnect",
            ButtonVariant::GhostDanger,
            ButtonSize::Md,
            false,
        )
        .on_click(move |_event, _window, cx| {
            let _ = root
                .clone()
                .update(cx, |root, cx| root.disconnect_discover(cx));
        })
    });

    div()
        .flex()
        .flex_col()
        .gap(theme::SPACE_DEFAULT)
        .child(dense_label("Discover (Seerr / Jellyseerr)"))
        .child(status_row)
        .child(form_row("Server URL", form.url.clone()))
        .child(method_row)
        .children(identity_row)
        .child(form_row(
            discover::secret_label(form.method),
            form.secret.clone(),
        ))
        .children(form.error.clone().map(|e| {
            div()
                .text_color(rgb(theme::DANGER))
                .text_size(theme::TEXT_METADATA)
                .child(e)
        }))
        .child(
            div()
                .flex()
                .flex_row()
                .gap_2()
                .child(connect_button)
                .children(disconnect_button),
        )
}

/// Global preferred audio/subtitle language + subtitle mode -- the
/// fallback for a series' first play, before per-series memory
/// (`player_prefs.rs`) applies. Same segmented-preset-row shapes every
/// other row in this file uses; see `player_ui::resolve_track_selection`
/// for how these three values get applied.
fn render_language_prefs_section(prefs: LanguagePrefs, root: WeakEntity<Root>) -> impl IntoElement {
    let audio_root = root.clone();
    let on_audio: PickHandler<Option<&'static str>> = {
        let prefs = prefs.clone();
        std::rc::Rc::new(move |lang: Option<&'static str>, cx| {
            let next = LanguagePrefs {
                audio: lang.map(str::to_string),
                ..prefs.clone()
            };
            let _ = audio_root
                .clone()
                .update(cx, |root, cx| root.set_language_prefs(next, cx));
        })
    };
    let audio_buttons = LANGUAGE_PRESETS
        .iter()
        .map(|(label, lang)| {
            preset_button(
                SharedString::from(format!("lang-audio-{label}")),
                SharedString::from(*label),
                *lang,
                prefs.audio.as_deref() == *lang,
                on_audio.clone(),
            )
        })
        .collect::<Vec<_>>();

    let sub_lang_root = root.clone();
    let on_sub_lang: PickHandler<Option<&'static str>> = {
        let prefs = prefs.clone();
        std::rc::Rc::new(move |lang: Option<&'static str>, cx| {
            let next = LanguagePrefs {
                subtitle: lang.map(str::to_string),
                ..prefs.clone()
            };
            let _ = sub_lang_root
                .clone()
                .update(cx, |root, cx| root.set_language_prefs(next, cx));
        })
    };
    let sub_lang_buttons = LANGUAGE_PRESETS
        .iter()
        .map(|(label, lang)| {
            preset_button(
                SharedString::from(format!("lang-subtitle-{label}")),
                SharedString::from(*label),
                *lang,
                prefs.subtitle.as_deref() == *lang,
                on_sub_lang.clone(),
            )
        })
        .collect::<Vec<_>>();

    let mode_root = root;
    let on_mode: PickHandler<SubtitleMode> = {
        let prefs = prefs.clone();
        std::rc::Rc::new(move |subtitle_mode: SubtitleMode, cx| {
            let next = LanguagePrefs {
                subtitle_mode,
                ..prefs.clone()
            };
            let _ = mode_root
                .clone()
                .update(cx, |root, cx| root.set_language_prefs(next, cx));
        })
    };
    let mode_buttons = SUBTITLE_MODE_PRESETS
        .iter()
        .map(|(label, mode)| {
            preset_button(
                SharedString::from(format!("lang-sub-mode-{label}")),
                SharedString::from(*label),
                *mode,
                *mode == prefs.subtitle_mode,
                on_mode.clone(),
            )
        })
        .collect::<Vec<_>>();

    div()
        .flex()
        .flex_col()
        .gap_1()
        .mt_2()
        .child(dense_label("Languages"))
        .child(form_row(
            "Preferred audio",
            div()
                .flex()
                .flex_row()
                .flex_wrap()
                .gap_1()
                .children(audio_buttons),
        ))
        .child(form_row(
            "Subtitle mode",
            div()
                .flex()
                .flex_row()
                .flex_wrap()
                .gap_1()
                .children(mode_buttons),
        ))
        .child(form_row(
            "Preferred subtitle (used by \"Always\")",
            div()
                .flex()
                .flex_row()
                .flex_wrap()
                .gap_1()
                .children(sub_lang_buttons),
        ))
        .child(
            div()
                .mt_1()
                .text_size(theme::TEXT_METADATA)
                .text_color(rgba(theme::TEXT_QUATERNARY))
                .child(
                    "Only applies on the first play of a series (or any movie) -- once you pick a \
                     track by hand, this app remembers that per-series choice from then on, \
                     ahead of these global preferences. \"Forced only\" ignores the subtitle \
                     language above and instead matches whatever language the audio track \
                     itself ends up playing.",
                ),
        )
}

/// One shortcut row: description left (`form_row`'s label column), one or
/// more [`keycap_row`] chips right-aligned. `keys` takes `&[&str]` rather
/// than a single string so a chord (`⇧S`) or a two-binding row (`⌘M`/`Tab`
/// both toggle the miniplayer) render as separate chips instead of one
/// chip's text awkwardly spelling out "⌘M / Tab".
// `label` takes `impl Into<SharedString>` rather than a plain `&'static
// str` -- the "Seek −{back}s / +{forward}s" row needs a `format!`ed label,
// and every other call site's string literal converts just as well.
fn shortcut_row(label: impl Into<SharedString>, keys: &'static [&'static str]) -> impl IntoElement {
    form_row(
        label,
        keycap_row(keys.iter().map(|k| SharedString::from(*k))),
    )
}

/// `shortcut_row` plus a muted description line underneath
/// (`form_row_desc`) -- for the one row here (`Esc`'s step-down) whose full
/// behavior doesn't fit a short label without truncating against
/// `form_row`'s fixed-width label column.
fn shortcut_row_desc(
    label: &'static str,
    keys: &'static [&'static str],
    description: &'static str,
) -> impl IntoElement {
    form_row_desc(
        label,
        keycap_row(keys.iter().map(|k| SharedString::from(*k))),
        description,
    )
}

/// Static reference, no rebinding UI -- rows are read straight off
/// `Root::handle_global_keystroke`/`handle_playback_keystroke`'s actual
/// `match`es, not docs/UX-SPEC.md's §2 table (which predates a few of these).
/// **Navigation** is `handle_global_keystroke`'s browse-mode dispatch
/// (live outside Fullscreen-in-window/OS-Fullscreen); **Playback** is
/// `handle_playback_keystroke`, live only while the Player owns the
/// keyboard.
fn render_shortcuts_section(app_settings: &AppSettings) -> impl IntoElement {
    let skip = app_settings.skip_length;
    let seek_label = format!("Seek −{}s / +{}s", skip.back_secs, skip.forward_secs);
    // Play/pause is always a plain Space row; the two speed-hold rows are
    // appended separately, only while the setting is on.
    let speed_hold_rows: Vec<gpui::AnyElement> = if app_settings.speed_boost {
        vec![
            shortcut_row_desc(
                "2× speed (hold)",
                &["Right ⌥"],
                "Hold the right Option key to play at 2× until you let go.",
            )
            .into_any_element(),
            shortcut_row_desc(
                "0.5× speed (hold)",
                &["Left ⌥"],
                "Hold the left Option key to play at 0.5× until you let go.",
            )
            .into_any_element(),
        ]
    } else {
        Vec::new()
    };

    div()
        .flex()
        .flex_col()
        .gap_1()
        .child(dense_label("Navigation"))
        .child(shortcut_row("Jump to sidebar item", &["⌘1–9"]))
        .child(shortcut_row("Search", &["⌘F", "/"]))
        .child(shortcut_row("Back", &["⌘["]))
        .child(shortcut_row("Forward", &["⌘]"]))
        .child(shortcut_row("Move focus", &["←", "→", "↑", "↓"]))
        .child(shortcut_row("Open focused item", &["Return"]))
        .child(shortcut_row("Back / dismiss", &["Esc"]))
        .child(shortcut_row(
            "Previous / next episode (Episode page)",
            &["[", "]"],
        ))
        .child(dense_label("Playback"))
        .child(shortcut_row("Play / pause", &["Space"]))
        .children(speed_hold_rows)
        .child(shortcut_row(seek_label, &["←", "→"]))
        .child(shortcut_row("Seek −60s / +60s", &["⇧←", "⇧→"]))
        .child(shortcut_row("Toggle fullscreen", &["F"]))
        .child(shortcut_row("Miniplayer", &["⌘M", "Tab"]))
        .child(shortcut_row_desc(
            "Step back",
            &["Esc"],
            "OS-Fullscreen → Fullscreen-in-window → Miniplayer → stop, one layer at a time.",
        ))
        .child(shortcut_row("Volume up / down", &["↑", "↓"]))
        .child(shortcut_row("Mute", &["M"]))
        .child(shortcut_row("Cycle subtitle track", &["S"]))
        .child(shortcut_row("Open subtitle track picker", &["⇧S"]))
        .child(shortcut_row("Cycle audio track", &["A"]))
        .child(shortcut_row("Open audio track picker", &["⇧A"]))
        .child(shortcut_row("Toggle info overlay", &["I"]))
        .child(shortcut_row("Undo last skip", &["U"]))
        .child(shortcut_row("Previous / next episode", &["[", "]"]))
        .child(
            div()
                .mt_2()
                .text_size(theme::TEXT_METADATA)
                .text_color(rgba(theme::TEXT_QUATERNARY))
                .child(
                    "Playback shortcuts apply while the video fills the window or an OS \
                     fullscreen Space; Navigation shortcuts work everywhere else, including \
                     while a Miniplayer keeps playing in the corner.",
                ),
        )
}

fn render_about_section() -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .gap_2()
        .child(
            // Brand §3/§4: "Jellybeam" is Bagel Fat One, always -- same
            // `brand_lockup` the sidebar header and Connect screen use.
            crate::root::brand_lockup(),
        )
        .child(
            div()
                .text_size(theme::TEXT_METADATA)
                .text_color(rgba(theme::TEXT_TERTIARY))
                .child(format!("Version {}", env!("CARGO_PKG_VERSION"))),
        )
        .child(
            div()
                .mt_2()
                .text_size(theme::TEXT_METADATA)
                .text_color(rgba(theme::TEXT_QUATERNARY))
                // §1's descriptor, verbatim (facts, never adjectives).
                .child("A Jellyfin client for macOS."),
        )
        .child(
            div()
                .font_family(theme::FONT_MONO)
                .text_size(theme::TEXT_SPEC)
                .text_color(rgba(theme::TEXT_QUATERNARY))
                // §1's technical line, verbatim.
                .child("Rust · Apple Silicon · Direct Play · mpv"),
        )
}

// ---- Settings/prefs glue (moved from root.rs) --------------------------

impl Root {
    // --- Settings sheet ---------------------------------------------------

    pub(crate) fn open_settings(&mut self, cx: &mut Context<Self>) {
        self.collapse_fullscreen_player_for_nav(cx);
        let Some(state) = self.main_state_mut() else {
            return;
        };
        state.settings.open = true;
        cx.notify();
    }

    pub(crate) fn close_settings(&mut self, cx: &mut Context<Self>) {
        let Some(state) = self.main_state_mut() else {
            return;
        };
        state.settings.open = false;
        cx.notify();
    }

    /// The "?" keyboard-shortcuts overlay (`shortcuts_overlay.rs`), toggled
    /// by `handle_global_keystroke`'s `shift + /` case. Deliberately does
    /// nothing beyond flipping the flag -- no `collapse_fullscreen_player_
    /// for_nav` (unlike `open_settings` above), no pause, no layer change --
    /// the overlay must be summonable over live playback as a purely visual
    /// reference card, per this feature's own spec.
    pub(crate) fn open_shortcuts_overlay(&mut self, cx: &mut Context<Self>) {
        let Some(state) = self.main_state_mut() else {
            return;
        };
        state.shortcuts_overlay_open = true;
        cx.notify();
    }

    /// Closes the shortcuts overlay -- Esc, a second "?", a click on the
    /// scrim, or the panel's own close button all route here (see
    /// `handle_global_keystroke` and `shortcuts_overlay::render`).
    pub(crate) fn close_shortcuts_overlay(&mut self, cx: &mut Context<Self>) {
        let Some(state) = self.main_state_mut() else {
            return;
        };
        state.shortcuts_overlay_open = false;
        cx.notify();
    }

    /// Part B §11: sidebar account-footer click opens/closes the Server
    /// Switcher popover -- replaces the old "always jump straight to
    /// Settings" behavior: quick switching deserves one click, not
    /// Settings -> tab -> row.
    pub(crate) fn toggle_server_switcher(&mut self, cx: &mut Context<Self>) {
        let Some(state) = self.main_state_mut() else {
            return;
        };
        state.server_switcher_open = !state.server_switcher_open;
        cx.notify();
    }

    pub(crate) fn close_server_switcher(&mut self, cx: &mut Context<Self>) {
        let Some(state) = self.main_state_mut() else {
            return;
        };
        state.server_switcher_open = false;
        cx.notify();
    }

    /// The switcher popover's "Settings" footer row: closes the popover and
    /// opens the full Settings sheet, pre-navigated to Server & Account.
    /// The popover is a shortcut, not a replacement -- the existing full
    /// CRUD UI stays in the Settings sheet.
    pub(crate) fn open_settings_from_switcher(&mut self, cx: &mut Context<Self>) {
        {
            let Some(state) = self.main_state_mut() else {
                return;
            };
            state.server_switcher_open = false;
            state.settings.section = crate::settings::SettingsSection::Server;
        }
        self.open_settings(cx);
    }

    /// Opens the About window (`about.rs`), or focuses it if one is
    /// already open. `WindowHandle::update` returning `Err` is how a
    /// closed window's staleness surfaces (gpui has no separate "is this
    /// handle still live" query), so that's the signal used to decide
    /// "reuse" vs. "recreate."
    pub(crate) fn open_about_window(&mut self, cx: &mut Context<Self>) {
        if let Some(handle) = &self.about_window {
            let focused = handle
                .update(cx, |_, window, _cx| window.activate_window())
                .is_ok();
            if focused {
                return;
            }
        }

        // Read once at open time rather than stored on `Root` -- cheap,
        // static-for-the-process-lifetime facts, and re-reading them fresh
        // means a mid-session "Reduce Motion" toggle takes effect on the
        // next open, not requiring a restart.
        let reduce_motion = about::reduce_motion_enabled();
        let mpv_version = self.video.player().mpv_version();

        match cx.open_window(about::window_options(), move |window, cx| {
            cx.new(|cx| about::AboutView::new(window, cx, reduce_motion, mpv_version.clone()))
        }) {
            Ok(handle) => self.about_window = Some(handle),
            Err(err) => {
                tracing::warn!("failed to open the About window: {err}");
            }
        }
    }

    pub(crate) fn set_settings_section(
        &mut self,
        section: crate::settings::SettingsSection,
        cx: &mut Context<Self>,
    ) {
        let Some(state) = self.main_state_mut() else {
            return;
        };
        state.settings.section = section;
        cx.notify();
    }

    /// A per-type skip-segment action change from Settings -> Playback ->
    /// "Skip segments" -- persists immediately and, if a session is
    /// playing, updates that session's `PlayerUiState::skip_segment_prefs`
    /// too so the change takes effect on the next segment rather than only
    /// the next item.
    pub(crate) fn set_skip_segment_action(
        &mut self,
        ty: MediaSegmentType,
        action: SegmentAction,
        cx: &mut Context<Self>,
    ) {
        let mut prefs = self.app_settings.skip_segments;
        prefs.set_action_for(ty, action);
        self.app_settings.set_skip_segments(prefs);
        if let Screen::Main(state) = &mut self.screen {
            if let Some(ui) = &mut state.player_ui {
                ui.skip_segment_prefs = prefs;
            }
        }
        cx.notify();
    }

    /// "Autoplay next episode" toggle + delay -- same immediate-persist +
    /// live-session-update shape as `set_skip_segment_action`.
    pub(crate) fn set_autoplay_prefs(&mut self, prefs: AutoplayPrefs, cx: &mut Context<Self>) {
        self.app_settings.set_autoplay(prefs);
        if let Screen::Main(state) = &mut self.screen {
            if let Some(ui) = &mut state.player_ui {
                ui.autoplay_prefs = prefs;
                // Keep the already-showing card's fill/countdown honest if
                // this setting changes mid-card: off clears the countdown
                // so the fill stops animating; on restarts it from *now*
                // against the live remaining time, same formula
                // `tick_next_episode` uses when the card first appears.
                if ui.next_episode.is_some() {
                    if prefs.enabled {
                        let remaining = (ui.duration_secs - ui.position_secs).max(0.0);
                        ui.next_episode_shown_at = Some(Instant::now());
                        ui.next_episode_countdown_total_secs =
                            Some(crate::player_ui::next_episode_countdown_total(
                                remaining,
                                prefs.delay_secs as f64,
                            ));
                    } else {
                        ui.next_episode_shown_at = None;
                        ui.next_episode_countdown_total_secs = None;
                    }
                }
            }
        }
        cx.notify();
    }

    pub(crate) fn set_bitrate_mode(
        &mut self,
        base_url: String,
        mode: crate::settings::BitrateMode,
        cx: &mut Context<Self>,
    ) {
        self.app_settings.set_bitrate_mode(&base_url, mode);
        cx.notify();
    }

    /// Applies immediately to the live player (if a session is loaded) as
    /// well as persisting -- the Settings sheet is meant to feel live, not
    /// "apply on next playback".
    pub(crate) fn set_subtitle_style(
        &mut self,
        style: crate::settings::SubtitleStylePrefs,
        cx: &mut Context<Self>,
    ) {
        self.app_settings.set_subtitle(style);
        if let Err(e) = self.video.player().set_subtitle_style(&style.to_player()) {
            tracing::warn!(error = %e, "set_subtitle_style on change failed");
        }
        cx.notify();
    }

    /// Separate ←/→ skip-length preset change from Settings -> Playback ->
    /// "Skip length" -- same immediate-persist + live-session-update shape
    /// `set_skip_segment_action` uses.
    pub(crate) fn set_skip_length(&mut self, prefs: SkipLengthPrefs, cx: &mut Context<Self>) {
        self.app_settings.set_skip_length(prefs);
        if let Screen::Main(state) = &mut self.screen {
            if let Some(ui) = &mut state.player_ui {
                ui.skip_back_secs = prefs.back_secs;
                ui.skip_forward_secs = prefs.forward_secs;
            }
        }
        if let Some(np) = &self.now_playing {
            np.set_skip_intervals(f64::from(prefs.back_secs), f64::from(prefs.forward_secs));
        }
        cx.notify();
    }

    /// Global preferred audio/subtitle language + subtitle mode change
    /// from Settings -> Subtitles -> "Languages". Persists immediately;
    /// also re-runs `apply_track_prefs` against the current session's
    /// already-announced tracks so a change while something is playing
    /// takes effect right away, not just on the next item.
    pub(crate) fn set_language_prefs(&mut self, prefs: LanguagePrefs, cx: &mut Context<Self>) {
        self.app_settings.set_language_prefs(prefs);
        self.apply_track_prefs(cx);
        cx.notify();
    }

    pub(crate) fn set_speed_boost(&mut self, enabled: bool, cx: &mut Context<Self>) {
        self.app_settings.set_speed_boost(enabled);
        cx.notify();
    }

    /// "Preload next item" toggle change from Settings -> Playback. Turning
    /// it off discards whatever preload is already in flight/ready right
    /// now, not just future ones: bumps the generation so an in-flight
    /// `run_preload` bails at its next `load_if_current` check, aborts the
    /// task handle, stops the paused stream if one is sitting in mpv, and
    /// logs the same discard-accounting line every other preload-loss path
    /// uses. Turning it back on does nothing here -- the next idle-browse
    /// trigger picks it back up on its own.
    pub(crate) fn set_preload(&mut self, enabled: bool, cx: &mut Context<Self>) {
        self.app_settings.set_preload(enabled);
        if !enabled {
            if let Screen::Main(state) = &mut self.screen {
                state.playback_generation.fetch_add(1, Ordering::Relaxed);
                if let Some(handle) = state.preload_task.take() {
                    handle.abort();
                }
                state.preload_target = None;
                let had_preload = state.preload.is_some();
                crate::root_playback::discard_preload(
                    state,
                    self.video.player(),
                    "preload disabled by setting",
                );
                if had_preload {
                    if let Err(e) = self.video.player().stop() {
                        tracing::warn!(
                            error = %e,
                            "stopping idle preload after disabling preload setting failed"
                        );
                    }
                }
            }
        }
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_view(id: &str, name: &str) -> media_cache::ViewSummary {
        crate::test_support::view_summary(id, name, media_cache::ViewKind::Library)
    }

    fn test_channel_view(id: &str, name: &str) -> media_cache::ViewSummary {
        crate::test_support::view_summary(id, name, media_cache::ViewKind::Channel)
    }

    // ---- Subtitle vertical position --------------------------------------

    /// The ladder itself is the root cause: the old presets stored raw
    /// mpv `sub-pos` values of 60 and 20, i.e. mid-frame and top-of-frame,
    /// behind labels ("Mid"/"High") that read as gentle raise steps. No step
    /// may sit above the vertical middle any more.
    #[test]
    fn every_position_preset_stays_in_the_lower_half_of_the_frame() {
        for (label, pos) in POS_PRESETS {
            let Some(pos) = pos else { continue };
            assert!(
                (SUBTITLE_POS_FLOOR..=SUBTITLE_POS_CEIL).contains(&pos),
                "preset {label} stores sub-pos {pos}, outside the safe band"
            );
        }
    }

    /// The ladder must actually be a ladder -- strictly raising, so the
    /// labels describe what the numbers do.
    #[test]
    fn position_presets_are_a_monotonic_raise_ladder() {
        let steps: Vec<i64> = POS_PRESETS.iter().filter_map(|(_, p)| *p).collect();
        assert!(
            steps.windows(2).all(|w| w[0] > w[1]),
            "presets {steps:?} are not monotonically raising"
        );
        assert_eq!(steps.last().copied(), Some(SUBTITLE_POS_FLOOR));
    }

    /// Migration: a `settings.json` written by the old ladder (or hand
    /// edited) never reaches mpv unsanitized.
    #[test]
    fn effective_pos_floors_a_legacy_top_half_preference() {
        let legacy = SubtitleStylePrefs {
            pos: Some(20),
            ..SubtitleStylePrefs::default()
        };
        assert_eq!(legacy.effective_pos(), Some(SUBTITLE_POS_FLOOR));
        assert_eq!(legacy.to_player().pos, Some(SUBTITLE_POS_FLOOR));
    }

    /// ...and a value already inside the band is passed through untouched,
    /// including `None` ("leave mpv's own default alone", per
    /// `player::SubtitleStyle::pos`).
    #[test]
    fn effective_pos_passes_through_a_valid_preference() {
        for pos in [None, Some(60), Some(88), Some(100), Some(150)] {
            let prefs = SubtitleStylePrefs {
                pos,
                ..SubtitleStylePrefs::default()
            };
            assert_eq!(prefs.effective_pos(), pos);
        }
        let over = SubtitleStylePrefs {
            pos: Some(400),
            ..SubtitleStylePrefs::default()
        };
        assert_eq!(over.effective_pos(), Some(SUBTITLE_POS_CEIL));
    }

    /// `to_player` must sanitize -- it is the conversion every apply path
    /// (settings change, player init) funnels through.
    #[test]
    fn to_player_carries_the_other_fields_verbatim() {
        let prefs = SubtitleStylePrefs {
            scale: 1.25,
            pos: Some(0),
            bold: true,
            back_alpha: 0.5,
        };
        let mapped = prefs.to_player();
        assert_eq!(mapped.scale, 1.25);
        assert!(mapped.bold);
        assert_eq!(mapped.back_alpha, 0.5);
        assert_eq!(mapped.pos, Some(SUBTITLE_POS_FLOOR));
    }

    /// Defaults per `SkipSegmentPrefs`/`AutoplayPrefs`'s own doc comments:
    /// everything `Ask` except `Commercial` (`AutoSkip`); autoplay on at a
    /// 10s delay.
    #[test]
    fn skip_segment_defaults_match_spec() {
        let prefs = SkipSegmentPrefs::default();
        assert_eq!(prefs.intro, SegmentAction::Ask);
        assert_eq!(prefs.outro, SegmentAction::Ask);
        assert_eq!(prefs.recap, SegmentAction::Ask);
        assert_eq!(prefs.preview, SegmentAction::Ask);
        assert_eq!(prefs.commercial, SegmentAction::AutoSkip);

        let autoplay = AutoplayPrefs::default();
        assert!(autoplay.enabled);
        assert_eq!(autoplay.delay_secs, 10);
    }

    #[test]
    fn skip_segment_action_for_covers_every_configurable_type() {
        let mut prefs = SkipSegmentPrefs::default();
        prefs.set_action_for(MediaSegmentType::Intro, SegmentAction::Off);
        prefs.set_action_for(MediaSegmentType::Outro, SegmentAction::AutoSkip);
        assert_eq!(
            prefs.action_for(MediaSegmentType::Intro),
            SegmentAction::Off
        );
        assert_eq!(
            prefs.action_for(MediaSegmentType::Outro),
            SegmentAction::AutoSkip
        );
        assert_eq!(
            prefs.action_for(MediaSegmentType::Recap),
            SegmentAction::Ask
        );
        // No settings row exists for Unknown/Unrecognized -- always Off,
        // and `set_action_for` on them is a documented no-op.
        assert_eq!(
            prefs.action_for(MediaSegmentType::Unknown),
            SegmentAction::Off
        );
        prefs.set_action_for(MediaSegmentType::Unrecognized, SegmentAction::Ask);
        assert_eq!(
            prefs.action_for(MediaSegmentType::Unrecognized),
            SegmentAction::Off
        );
    }

    /// Config-file round-trip, verified directly against disk (same shape
    /// as `player_prefs.rs`'s `round_trips_through_a_temp_home`) rather
    /// than only through a live GPUI session.
    #[test]
    fn skip_segments_and_autoplay_round_trip_through_a_temp_home() {
        crate::test_support::with_temp_home("app-settings-skip-segments", || {
            let mut settings = AppSettings::load();
            assert_eq!(settings.skip_segments, SkipSegmentPrefs::default());
            assert_eq!(settings.autoplay, AutoplayPrefs::default());

            let mut prefs = settings.skip_segments;
            prefs.set_action_for(MediaSegmentType::Intro, SegmentAction::AutoSkip);
            prefs.set_action_for(MediaSegmentType::Commercial, SegmentAction::Off);
            settings.set_skip_segments(prefs);
            settings.set_autoplay(AutoplayPrefs {
                enabled: false,
                delay_secs: 5,
            });

            let reloaded = AppSettings::load();
            assert_eq!(
                reloaded.skip_segments.action_for(MediaSegmentType::Intro),
                SegmentAction::AutoSkip
            );
            assert_eq!(
                reloaded
                    .skip_segments
                    .action_for(MediaSegmentType::Commercial),
                SegmentAction::Off
            );
            // Untouched types keep their default.
            assert_eq!(
                reloaded.skip_segments.action_for(MediaSegmentType::Recap),
                SegmentAction::Ask
            );
            assert!(!reloaded.autoplay.enabled);
            assert_eq!(reloaded.autoplay.delay_secs, 5);
        });
    }

    /// The DNS seed only pays off if it survives a relaunch. Also pins the
    /// lenient-parse and change-detection contracts the seeding call sites
    /// rely on.
    #[test]
    fn dns_seed_round_trips_per_host_through_a_temp_home() {
        crate::test_support::with_temp_home("app-settings-dns-seed", || {
            use std::net::IpAddr;

            let mut settings = AppSettings::load();
            assert!(
                settings.dns_seed("storeserver").is_empty(),
                "an unknown host has no seed"
            );

            let addrs: Vec<IpAddr> =
                vec!["192.0.2.7".parse().expect("v4"), "::1".parse().expect("v6")];
            settings.set_dns_seed("storeserver", &addrs);

            let mut reloaded = AppSettings::load();
            assert_eq!(reloaded.dns_seed("storeserver"), addrs);
            assert!(reloaded.dns_seed("elsewhere").is_empty());

            // An empty snapshot never erases a good seed (the resolver
            // simply had nothing cached at the moment we looked).
            reloaded.set_dns_seed("storeserver", &[]);
            assert_eq!(AppSettings::load().dns_seed("storeserver"), addrs);

            // A moved server overwrites it.
            let moved: Vec<IpAddr> = vec!["192.0.2.9".parse().expect("v4")];
            reloaded.set_dns_seed("storeserver", &moved);
            assert_eq!(AppSettings::load().dns_seed("storeserver"), moved);
        });
    }

    /// Junk in the file must degrade to "no seed", never to a panic or a
    /// bogus address -- this is a performance hint, not state worth
    /// failing a launch over.
    #[test]
    fn dns_seed_parses_leniently() {
        crate::test_support::with_temp_home("app-settings-dns-seed-junk", || {
            let path = settings_path();
            std::fs::create_dir_all(path.parent().expect("state root")).expect("mkdir");
            std::fs::write(
                &path,
                br#"{"dns_seed":{"mediaserver":["not-an-ip","192.0.2.7",""]}}"#,
            )
            .expect("write settings");

            let settings = AppSettings::load();
            assert_eq!(
                settings.dns_seed("mediaserver"),
                vec!["192.0.2.7".parse::<std::net::IpAddr>().expect("v4")],
                "junk entries are skipped, valid ones survive"
            );
        });
    }

    /// The toggle must survive a relaunch, and it must do so per library --
    /// two libraries carrying different modes at once is the whole reason
    /// this is a map and not a single scalar.
    #[test]
    fn library_view_mode_round_trips_per_library_through_a_temp_home() {
        crate::test_support::with_temp_home("app-settings-library-view-mode", || {
            let mut settings = AppSettings::load();
            // A library nobody has toggled reads as the poster wall, so an
            // existing settings file from before this key predates it.
            assert_eq!(settings.library_view_mode("movies"), LibraryViewMode::Grid);

            settings.set_library_view_mode("movies", LibraryViewMode::List);

            let reloaded = AppSettings::load();
            assert_eq!(reloaded.library_view_mode("movies"), LibraryViewMode::List);
            assert_eq!(reloaded.library_view_mode("shows"), LibraryViewMode::Grid);
        });
    }

    // ---- Next Up cutoff/rewatching ----------------------------------------

    /// Default is "Off" (no cutoff sent, no rewatching) -- an existing
    /// settings file from before this key existed keeps today's unfiltered
    /// `/Shows/NextUp` behavior.
    #[test]
    fn next_up_prefs_default_to_off() {
        let prefs = NextUpPrefs::default();
        assert_eq!(prefs.cutoff_days, None);
        assert!(!prefs.rewatching);
    }

    #[test]
    fn next_up_prefs_convert_straight_through_to_media_cache_options() {
        let prefs = NextUpPrefs {
            cutoff_days: Some(30),
            rewatching: true,
        };
        let options = prefs.to_next_up_options();
        assert_eq!(options.cutoff_days, Some(30));
        assert!(options.rewatching);
    }

    #[test]
    fn next_up_cutoff_presets_start_with_off() {
        assert_eq!(NEXT_UP_CUTOFF_PRESETS[0], ("Off", None));
    }

    #[test]
    fn next_up_prefs_round_trip_through_a_temp_home() {
        crate::test_support::with_temp_home("app-settings-next-up", || {
            let mut settings = AppSettings::load();
            assert_eq!(settings.next_up, NextUpPrefs::default());

            settings.set_next_up(NextUpPrefs {
                cutoff_days: Some(14),
                rewatching: true,
            });

            let reloaded = AppSettings::load();
            assert_eq!(reloaded.next_up.cutoff_days, Some(14));
            assert!(reloaded.next_up.rewatching);
        });
    }

    // ---- Per-library Home visibility ---------------------------------------

    #[test]
    fn library_visible_on_home_round_trips_per_library_through_a_temp_home() {
        crate::test_support::with_temp_home("app-settings-home-visibility", || {
            let mut settings = AppSettings::load();
            // A library nobody has toggled is visible -- same "absent means
            // default" shape as `library_view_mode`.
            assert!(settings.library_visible_on_home("kids"));
            assert!(settings.hidden_home_libraries().is_empty());

            settings.set_library_visible_on_home("kids", false);

            let reloaded = AppSettings::load();
            assert!(!reloaded.library_visible_on_home("kids"));
            assert!(
                reloaded.library_visible_on_home("movies"),
                "untouched library stays visible"
            );
            assert_eq!(
                reloaded.hidden_home_libraries(),
                std::collections::HashSet::from(["kids".to_string()])
            );

            // Toggling back on removes it from the hidden set again, not
            // just flips a bool that `hidden_home_libraries` still counts.
            let mut reloaded = reloaded;
            reloaded.set_library_visible_on_home("kids", true);
            assert!(AppSettings::load().hidden_home_libraries().is_empty());
        });
    }

    // ---- Hide watched from Latest ------------------------------------------

    #[test]
    fn hide_watched_latest_defaults_off_and_round_trips_through_a_temp_home() {
        crate::test_support::with_temp_home("app-settings-hide-watched-latest", || {
            let mut settings = AppSettings::load();
            assert!(!settings.hide_watched_latest);

            settings.set_hide_watched_latest(true);
            assert!(AppSettings::load().hide_watched_latest);
        });
    }

    // ---- Startup screen -----------------------------------------------------

    /// A stored library id that names a real library in `views` resolves
    /// to that library; one that doesn't (removed/renamed, or a settings
    /// file from a different server) falls back to `Home` silently.
    #[test]
    fn startup_screen_falls_back_to_home_when_the_stored_library_is_gone() {
        crate::test_support::with_temp_home("app-settings-startup-screen-fallback", || {
            let mut settings = AppSettings::load();
            settings.set_startup_screen(StartupScreen::Library("movies".to_string()));

            let views = vec![
                test_view("movies", "Movies"),
                test_view("shows", "TV Shows"),
            ];
            assert_eq!(
                settings.startup_screen(&views),
                StartupScreen::Library("movies".to_string()),
                "a library that still exists in `views` must resolve as-is"
            );

            let views_without_movies = vec![test_view("shows", "TV Shows")];
            assert_eq!(
                settings.startup_screen(&views_without_movies),
                StartupScreen::Home,
                "a stored library id absent from `views` must fall back to Home, not panic or \
                 dangle"
            );
        });
    }

    /// docs/PLUGIN-CHANNELS.md §2.1: a `ViewKind::
    /// Channel` view routes to the live channel browse screen, never the
    /// mirror-backed grid `Library(id)` targets -- so a stored id that
    /// resolves to a Channel view must fail safe to `Home`, same as one
    /// that resolves to nothing at all.
    #[test]
    fn startup_screen_falls_back_to_home_for_a_channel_view() {
        crate::test_support::with_temp_home("app-settings-startup-screen-channel-fallback", || {
            let mut settings = AppSettings::load();
            settings.set_startup_screen(StartupScreen::Library("recordings".to_string()));

            let views = vec![
                test_view("movies", "Movies"),
                test_channel_view("recordings", "Recordings"),
            ];
            assert_eq!(
                settings.startup_screen(&views),
                StartupScreen::Home,
                "a stored id that names a Channel view must fall back to Home, never route to \
                 the mirror-backed grid a Channel view has none of"
            );
        });
    }

    #[test]
    fn startup_screen_defaults_to_home() {
        crate::test_support::with_temp_home("app-settings-startup-screen-default", || {
            let settings = AppSettings::load();
            assert_eq!(settings.startup_screen(&[]), StartupScreen::Home);
        });
    }

    #[test]
    fn startup_screen_round_trips_through_a_temp_home() {
        crate::test_support::with_temp_home("app-settings-startup-screen-round-trip", || {
            let mut settings = AppSettings::load();
            settings.set_startup_screen(StartupScreen::Library("shows".to_string()));

            let views = vec![test_view("shows", "TV Shows")];
            assert_eq!(
                AppSettings::load().startup_screen(&views),
                StartupScreen::Library("shows".to_string())
            );
        });
    }

    // ---- Skip length --------------------------------------------------------

    /// Default 10s/10s -- matches the pre-feature hardcoded `±10.0` exactly,
    /// so an existing settings file sees no behavior change until touched.
    #[test]
    fn skip_length_defaults_match_pre_feature_hardcoded_behavior() {
        let prefs = SkipLengthPrefs::default();
        assert_eq!(prefs.back_secs, 10);
        assert_eq!(prefs.forward_secs, 10);
    }

    #[test]
    fn skip_length_round_trips_through_a_temp_home() {
        crate::test_support::with_temp_home("app-settings-skip-length", || {
            let mut settings = AppSettings::load();
            assert_eq!(settings.skip_length, SkipLengthPrefs::default());

            settings.set_skip_length(SkipLengthPrefs {
                back_secs: 5,
                forward_secs: 30,
            });

            let reloaded = AppSettings::load();
            assert_eq!(reloaded.skip_length.back_secs, 5);
            assert_eq!(reloaded.skip_length.forward_secs, 30);
        });
    }

    // ---- Language prefs ------------------------------------------------------

    #[test]
    fn language_prefs_default_to_any_and_default_subtitle_mode() {
        let prefs = LanguagePrefs::default();
        assert_eq!(prefs.audio, None);
        assert_eq!(prefs.subtitle, None);
        assert_eq!(prefs.subtitle_mode, SubtitleMode::Default);
    }

    #[test]
    fn language_prefs_round_trip_through_a_temp_home() {
        crate::test_support::with_temp_home("app-settings-language-prefs", || {
            let mut settings = AppSettings::load();
            assert_eq!(settings.language, LanguagePrefs::default());

            settings.set_language_prefs(LanguagePrefs {
                audio: Some("jpn".to_string()),
                subtitle: Some("eng".to_string()),
                subtitle_mode: SubtitleMode::Always,
            });

            let reloaded = AppSettings::load();
            assert_eq!(reloaded.language.audio.as_deref(), Some("jpn"));
            assert_eq!(reloaded.language.subtitle.as_deref(), Some("eng"));
            assert_eq!(reloaded.language.subtitle_mode, SubtitleMode::Always);
        });
    }

    #[test]
    fn language_presets_start_with_any() {
        assert_eq!(LANGUAGE_PRESETS[0], ("Any", None));
    }

    // ---- Speed boost -----------------------------------------------------

    /// The load-fallback case this wrapper type exists for: a fresh
    /// install/corrupt file must still come up with the gesture on, not
    /// `bool::default()`'s `false`.
    #[test]
    fn speed_boost_defaults_on_for_a_fresh_or_missing_settings_file() {
        crate::test_support::with_temp_home("app-settings-speed-boost-default", || {
            assert!(AppSettings::load().speed_boost);
        });
    }

    #[test]
    fn speed_boost_round_trips_through_a_temp_home() {
        crate::test_support::with_temp_home("app-settings-speed-boost", || {
            let mut settings = AppSettings::load();
            assert!(settings.speed_boost);

            settings.set_speed_boost(false);
            assert!(!AppSettings::load().speed_boost);

            let mut reloaded = AppSettings::load();
            reloaded.set_speed_boost(true);
            assert!(AppSettings::load().speed_boost);
        });
    }

    /// Same load-fallback footgun as `SpeedBoostPrefs` -- a fresh
    /// install/corrupt file must come up with preloading on, not
    /// `bool::default()`'s `false`.
    #[test]
    fn preload_defaults_on_for_a_fresh_or_missing_settings_file() {
        crate::test_support::with_temp_home("app-settings-preload-default", || {
            assert!(AppSettings::load().preload);
        });
    }

    #[test]
    fn preload_round_trips_through_a_temp_home() {
        crate::test_support::with_temp_home("app-settings-preload", || {
            let mut settings = AppSettings::load();
            assert!(settings.preload);

            settings.set_preload(false);
            assert!(!AppSettings::load().preload);

            let mut reloaded = AppSettings::load();
            reloaded.set_preload(true);
            assert!(AppSettings::load().preload);
        });
    }
}
