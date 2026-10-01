//! Shared test-only helpers. `#[cfg(test)]`-only module, wired into
//! `main.rs`'s mod list the same way.
//!
//! `HOME_ENV_LOCK`/`with_temp_home` exist because more than one module's
//! tests now point `HOME` at a scratch directory to exercise file-backed
//! persistence (`keychain.rs`'s session-list tests, `player_prefs.rs`'s
//! `TrackPrefs` round trip, `settings.rs`'s) --
//! `std::env::set_var("HOME", ...)` mutates real process-global state, and
//! `cargo test`'s default multi-threaded runner can interleave two such
//! tests from different modules. Before this shared lock existed, each
//! module's test only guarded against *itself* running concurrently with
//! *itself* (impossible -- one test fn, one thread) or had no guard at
//! all, on the (violated) assumption that no other test in the
//! crate touched `HOME`. A shared, crate-wide mutex is the actual fix.

use std::sync::Mutex;

pub(crate) static HOME_ENV_LOCK: Mutex<()> = Mutex::new(());

/// A minimal `media_cache::ViewSummary` fixture, consolidated here from
/// three near-identical copies (`home.rs`'s `view`, `settings.rs`'s
/// `test_view` and `test_channel_view`) that all built the same three-field
/// struct literal, differing only in which `ViewKind` they hard-coded (or,
/// for `home.rs`'s, took as a param already).
pub(crate) fn view_summary(
    id: &str,
    name: &str,
    kind: media_cache::ViewKind,
) -> media_cache::ViewSummary {
    media_cache::ViewSummary {
        id: id.to_string(),
        name: name.to_string(),
        kind,
    }
}

/// `BaseItemDto` has no `Default` impl (generated from the OpenAPI spec, not
/// hand-written) -- built through `serde_json` instead. Consolidated here
/// from two identical copies in `detail.rs`'s `merge_enrichment_tests` and
/// `episode_detail_tests` modules (the latter's was already dead code,
/// `#[allow(dead_code)]` and all -- removed there rather than duplicated
/// here unused).
pub(crate) fn dto_from(json: serde_json::Value) -> jellyfin_api::models::BaseItemDto {
    serde_json::from_value(json).expect("valid BaseItemDto fixture JSON")
}

/// Points `HOME` at a fresh temp directory for the duration of `f`,
/// serialized against every other `with_temp_home` call in this crate via
/// `HOME_ENV_LOCK` (see module doc). `label` becomes part of the temp
/// directory name (kept distinct per caller so two different modules'
/// tests, even if somehow interleaved by a bug in this helper, wouldn't
/// collide on the same files).
pub(crate) fn with_temp_home<T>(label: &str, f: impl FnOnce() -> T) -> T {
    let _guard = HOME_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = std::env::temp_dir().join(format!(
        "jellybeam-{label}-test-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&tmp);
    let previous = std::env::var_os("HOME");
    // SAFETY: serialized by `HOME_ENV_LOCK` above -- no other test in this
    // crate reads/writes `HOME` concurrently with this block.
    unsafe { std::env::set_var("HOME", &tmp) };
    let result = f();
    unsafe {
        match &previous {
            Some(home) => std::env::set_var("HOME", home),
            None => std::env::remove_var("HOME"),
        }
    }
    let _ = std::fs::remove_dir_all(&tmp);
    result
}
