//! Central roots for all persistent app state and caches.
//!
//! **Test-harness isolation**: without it, the E2E suites run the real
//! `jellybeam` binary against the user's real
//! `~/Library/Application Support/Jellybeam` -- each suite run would
//! silently revert real settings to defaults (surfacing as "skip intro
//! keeps reverting from Auto to Ask") and pollute the real session list
//! with dev-server logins (`jellybeam-admin @ localhost` rows in the user's
//! own Server & Account pane). Any run with `JELLYBEAM_E2E=1` /
//! `JELLYBEAM_E2E_QUIT_TEST=1` (or an explicit `JELLYBEAM_STATE_DIR`) now gets an
//! isolated state root; the user's real directories are only ever touched
//! by a plain, non-test launch.
//!
//! The E2E root is a FIXED temp path (not per-run random): a suite's own
//! multi-launch scenarios (quit/relaunch resume) need state to survive
//! within and across runs, mirroring how the real app behaves between
//! launches. Stale E2E state is as harmless there as it is for the real
//! app -- the suites authenticate fresh and the mirror rebuilds on schema
//! mismatch.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

static PRIVATE_WRITE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn e2e_mode() -> bool {
    std::env::var("JELLYBEAM_E2E").as_deref() == Ok("1")
        || std::env::var("JELLYBEAM_E2E_QUIT_TEST").as_deref() == Ok("1")
}

/// Root for persistent state: settings.json, the file-backed session
/// store, track-prefs.json, and the per-server mirror scope dirs.
pub(crate) fn state_root() -> PathBuf {
    if let Some(dir) = std::env::var_os("JELLYBEAM_STATE_DIR") {
        return PathBuf::from(dir);
    }
    if e2e_mode() {
        return std::env::temp_dir().join("jellybeam-e2e-state");
    }
    home().join("Library/Application Support/Jellybeam")
}

/// Root for disposable caches (image cache etc.) -- kept distinct from
/// [`state_root`] to preserve the macOS Application Support vs Caches
/// split for real launches.
pub(crate) fn cache_root() -> PathBuf {
    if let Some(dir) = std::env::var_os("JELLYBEAM_STATE_DIR") {
        return PathBuf::from(dir).join("caches");
    }
    if e2e_mode() {
        return std::env::temp_dir()
            .join("jellybeam-e2e-state")
            .join("caches");
    }
    home().join("Library/Caches/Jellybeam")
}

/// Atomically replace `path` with `bytes`, using owner-only (`0600`)
/// permissions. A crash or power loss can leave a temporary file behind,
/// but never a partially-truncated settings or preference file.
///
/// Security: state files under [`state_root`] can hold
/// mildly-sensitive data -- `settings.json` carries the per-server base URLs
/// the user connects to, and it sits beside the `0600` token store. Plain
/// `std::fs::write` creates a new file with the process umask (typically
/// world-readable `0644`), leaving that data readable by any other local
/// user. This mirrors the token store's own `OpenOptions::mode(0o600)`
/// pattern (`keychain::file_write`) so every state file gets the same
/// owner-only treatment. On an existing file `.mode()` is ignored by the OS,
/// so we also chmod after opening to harden files created before this
/// landed.
pub(crate) fn write_private(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let parent = path.parent().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "state path has no parent")
    })?;
    let file_name = path.file_name().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "state path has no file name",
        )
    })?;
    let sequence = PRIVATE_WRITE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let temporary = parent.join(format!(
        ".{}.tmp-{}-{sequence}",
        file_name.to_string_lossy(),
        std::process::id()
    ));

    let result = (|| {
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create_new(true).mode(0o600);
        let mut file = opts.open(&temporary)?;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        file.write_all(bytes)?;
        file.sync_all()?;
        std::fs::rename(&temporary, path)?;
        // Flush the directory entry as well as the file contents, so the
        // replacement remains durable across an abrupt restart.
        std::fs::File::open(parent)?.sync_all()
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The isolation contract itself: under either E2E env flag the state
    /// root must NOT be the user's real Application Support dir. (Env-var
    /// mutation in tests is process-global, so this asserts on the pure
    /// path shapes instead: the real root and the e2e root differ.)
    #[test]
    fn e2e_root_is_disjoint_from_the_real_state_root() {
        let real = home().join("Library/Application Support/Jellybeam");
        let e2e = std::env::temp_dir().join("jellybeam-e2e-state");
        assert_ne!(real, e2e);
        assert!(!e2e.starts_with(&real));
    }

    /// State files must be written owner-only, and the helper must
    /// re-harden a pre-existing world-readable file (the upgrade path for
    /// files created before this landed).
    #[test]
    fn write_private_sets_owner_only_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("jellybeam-writepriv-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let path = dir.join("settings.json");
        // Pre-seed a world-readable file to prove the chmod-after-open path.
        std::fs::write(&path, b"old").expect("seed");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644))
            .expect("chmod seed");

        write_private(&path, b"{\"k\":1}").expect("write_private");

        let mode = std::fs::metadata(&path).expect("stat").permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "state file must be owner-only, got {mode:o}");
        assert_eq!(std::fs::read(&path).expect("read"), b"{\"k\":1}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
