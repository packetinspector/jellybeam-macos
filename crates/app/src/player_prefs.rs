//! Per-series audio/subtitle track preference persistence (docs/UX-SPEC.md §5:
//! "track pre-selection (audio/sub pickers remember per-series
//! preference)"). A simple JSON store is sufficient here. Separate file
//! from `keychain.rs`'s Keychain-backed session store -- this is
//! non-sensitive UI preference data, so a plain JSON file under Application
//! Support is the simplest fit (no Keychain round-trip needed for a picker
//! selection).
//!
//! One flat `HashMap<series_id, SeriesTrackPref>` keyed by the *series*
//! item id (movies fall back to remembering nothing -- there's no series to
//! key by, and a single movie's own per-item choice isn't worth persisting
//! across app runs).

use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct SeriesTrackPref {
    /// Preferred audio track language/title key (see `player_ui.rs`'s
    /// `track_pref_key` -- matches on `Track::lang` first, falling back to
    /// `Track::title`, since mpv track ids aren't stable across items).
    pub audio: Option<String>,
    pub subtitle: Option<String>,
}

/// DESIGN-PLAYER-NAV.md §1.5: the scrubber's right-hand time label toggles
/// between `-remaining` (default) and `duration` (`12:34 / 58:10`) on
/// click -- a single
/// global choice ("sticky across sessions, not per-item"), not a per-series
/// one, so it lives at the top level of `PrefsFile` rather than inside
/// `SeriesTrackPref`.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) enum RemainingDisplay {
    #[default]
    Remaining,
    Total,
}

impl RemainingDisplay {
    pub(crate) fn toggled(self) -> Self {
        match self {
            RemainingDisplay::Remaining => RemainingDisplay::Total,
            RemainingDisplay::Total => RemainingDisplay::Remaining,
        }
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct PrefsFile {
    #[serde(default)]
    series: HashMap<String, SeriesTrackPref>,
    #[serde(default)]
    remaining_display: RemainingDisplay,
}

/// `~/Library/Application Support/Jellybeam/track-prefs.json`. Falls back to
/// the current directory (only relevant for tests / a misconfigured `HOME`)
/// rather than failing outright -- preference persistence is a nice-to-have,
/// never load-bearing for playback itself.
fn prefs_path() -> PathBuf {
    crate::paths::state_root().join("track-prefs.json")
}

/// Loaded once into `MainState` at connect time and mutated in place; kept
/// tiny (one series worth of two `Option<String>`s per entry) so
/// read-modify-write-whole-file on every change is cheap enough not to
/// warrant a smarter storage layer.
#[derive(Debug, Default, Clone)]
pub(crate) struct TrackPrefs {
    path: Option<PathBuf>,
    entries: HashMap<String, SeriesTrackPref>,
    remaining_display: RemainingDisplay,
}

impl TrackPrefs {
    pub(crate) fn load() -> Self {
        let path = prefs_path();
        let file = std::fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<PrefsFile>(&bytes).ok())
            .unwrap_or_default();
        TrackPrefs {
            path: Some(path),
            entries: file.series,
            remaining_display: file.remaining_display,
        }
    }

    pub(crate) fn remaining_display(&self) -> RemainingDisplay {
        self.remaining_display
    }

    pub(crate) fn set_remaining_display(&mut self, value: RemainingDisplay) {
        self.remaining_display = value;
        self.save();
    }

    pub(crate) fn get(&self, series_id: &str) -> Option<&SeriesTrackPref> {
        self.entries.get(series_id)
    }

    /// Persists an updated preference for `series_id`, best-effort (a
    /// failed write -- e.g. read-only home dir -- just means the
    /// preference doesn't survive a restart, not a playback error).
    pub(crate) fn set_audio(&mut self, series_id: &str, key: Option<String>) {
        self.entries.entry(series_id.to_string()).or_default().audio = key;
        self.save();
    }

    pub(crate) fn set_subtitle(&mut self, series_id: &str, key: Option<String>) {
        self.entries
            .entry(series_id.to_string())
            .or_default()
            .subtitle = key;
        self.save();
    }

    fn save(&self) {
        let Some(path) = &self.path else { return };
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let file = PrefsFile {
            series: self.entries.clone(),
            remaining_display: self.remaining_display,
        };
        if let Ok(bytes) = serde_json::to_vec_pretty(&file) {
            let _ = crate::paths::write_private(path, &bytes);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_through_a_temp_home() {
        // `crate::test_support::with_temp_home`: `HOME` is process-
        // global state, and this crate now has more than one module
        // pointing it at a scratch dir for its own file-backed-persistence
        // tests (`keychain.rs`'s session-list tests too) -- serialized via
        // a shared crate-wide lock so `cargo test`'s default multi-threaded
        // runner can't interleave two such tests and race on `HOME`.
        crate::test_support::with_temp_home("prefs", || {
            let mut prefs = TrackPrefs::load();
            assert!(prefs.get("series-1").is_none());
            prefs.set_audio("series-1", Some("eng".to_string()));
            prefs.set_subtitle("series-1", Some("spa".to_string()));

            let reloaded = TrackPrefs::load();
            let pref = reloaded.get("series-1").expect("saved pref should reload");
            assert_eq!(pref.audio.as_deref(), Some("eng"));
            assert_eq!(pref.subtitle.as_deref(), Some("spa"));
        });
    }

    #[test]
    fn remaining_display_defaults_to_remaining_and_round_trips() {
        crate::test_support::with_temp_home("prefs-remaining", || {
            let mut prefs = TrackPrefs::load();
            assert_eq!(prefs.remaining_display(), RemainingDisplay::Remaining);
            prefs.set_remaining_display(RemainingDisplay::Total);

            let reloaded = TrackPrefs::load();
            assert_eq!(reloaded.remaining_display(), RemainingDisplay::Total);
        });
    }

    #[test]
    fn remaining_display_toggles() {
        assert_eq!(
            RemainingDisplay::Remaining.toggled(),
            RemainingDisplay::Total
        );
        assert_eq!(
            RemainingDisplay::Total.toggled(),
            RemainingDisplay::Remaining
        );
    }
}
