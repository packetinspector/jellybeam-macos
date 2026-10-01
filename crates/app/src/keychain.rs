//! Session + device-id persistence.
//!
//! **File store only, unconditionally** — a plain JSON file under
//! Application Support with 0600 permissions (see [`use_file_store`]).
//! Previously defaulted to the macOS Keychain, dropped because every ad-hoc
//! re-sign (`scripts/bundle-app.sh`'s default, and every dev rebuild)
//! changes the bundle's code signature, and macOS's legacy Keychain grants
//! unprompted access to an existing item only when the requesting binary's
//! signature matches what created it -- so each rebuild triggered an
//! interactive authorization prompt (a hang in unattended/E2E runs; see
//! docs/BUILD.md "Keychain caveat"). `JELLYBEAM_KEYCHAIN=1` forces the real
//! Keychain at runtime as a deliberate, explicit opt-in for testing (still
//! has the re-signing/re-prompt hazard, so not for routine use).
//!
//! One JSON blob per account. The single-session blob (`SESSION_ACCOUNT`,
//! "session") is superseded by a list blob (`SESSION_LIST_ACCOUNT`,
//! "session-list") holding every signed-in `(server, user)` pair plus which
//! one is active -- [`load_sessions`] transparently migrates the old single
//! blob into a one-entry list the first time it's read on an install that
//! predates this, then writes the list blob going forward (the legacy key
//! is left in place, untouched). This migration never crosses backends --
//! it stays within whichever one `use_file_store` already selects.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

const SERVICE: &str = "tv.jellybeam.jellyfin";
const SESSION_ACCOUNT: &str = "session";
const SESSION_LIST_ACCOUNT: &str = "session-list";
const DEVICE_ID_ACCOUNT: &str = "device-id";

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct StoredSession {
    pub base_url: String,
    pub token: String,
    pub user_id: Option<String>,
    /// Display name for the multi-user switcher. `None` for sessions
    /// persisted before this field existed (`serde(default)` keeps old
    /// blobs loading); the switcher falls back to showing the server URL
    /// alone in that case rather than failing to render.
    #[serde(default)]
    pub username: Option<String>,
    /// The server's version at last successful `/System/Info/Public`
    /// refresh, as a plain `"major.minor.patch"` string (parsed back into a
    /// `jellyfin_api::ServerVersion` on load -- kept as a string here so an
    /// unparseable/future value never fails deserialization of the whole
    /// session). `None` for sessions persisted before this field existed,
    /// and for a session that hasn't completed its first refresh yet
    /// (`serde(default)` keeps old blobs loading). Last field, per the
    /// project's keychain-forward-compat convention.
    #[serde(default)]
    pub server_version: Option<String>,
}

impl std::fmt::Debug for StoredSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StoredSession")
            .field("base_url", &"<redacted>")
            .field("token", &"<redacted>")
            .field("user_id", &"<redacted>")
            .field("username", &"<redacted>")
            .field("server_version", &self.server_version)
            .finish()
    }
}

/// Every signed-in session plus which one is the foreground one. `active`
/// is a plain index into `sessions`, not an id -- kept in range by
/// every mutating method on this type ([`add_or_update`], [`remove`]) so
/// callers never have to re-clamp it themselves before indexing.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct StoredSessionList {
    pub sessions: Vec<StoredSession>,
    pub active: usize,
}

impl StoredSessionList {
    pub(crate) fn active_session(&self) -> Option<&StoredSession> {
        self.sessions.get(self.active)
    }

    /// Inserts `session`, or replaces an existing entry for the same
    /// `(base_url, user_id)` pair (re-authenticating an already-known
    /// account shouldn't duplicate it), and makes it the active session.
    pub(crate) fn add_or_update(&mut self, session: StoredSession) {
        if let Some(ix) = self
            .sessions
            .iter()
            .position(|s| s.base_url == session.base_url && s.user_id == session.user_id)
        {
            self.sessions[ix] = session;
            self.active = ix;
        } else {
            self.sessions.push(session);
            self.active = self.sessions.len() - 1;
        }
    }

    /// Removes the session at `ix`. If it was the active one, activity
    /// falls back to index 0 (or stays "no sessions" if the list is now
    /// empty) -- there's always a well-defined active index as long as
    /// `sessions` is non-empty, never a dangling one pointing past the end.
    pub(crate) fn remove(&mut self, ix: usize) {
        if ix >= self.sessions.len() {
            return;
        }
        self.sessions.remove(ix);
        if self.sessions.is_empty() {
            self.active = 0;
        } else if self.active >= self.sessions.len() {
            self.active = self.sessions.len() - 1;
        } else if ix < self.active {
            self.active -= 1;
        }
    }
}

/// Keychain vs. file-store decision -- see the module doc comment. Always
/// the file store, unconditionally, *unless* `JELLYBEAM_KEYCHAIN=1` is set at
/// runtime -- a deliberate, explicit opt-in for exercising the Keychain
/// path in testing. No build-time signal (debug/release, signed/ad-hoc)
/// affects this anymore; every build shape gets the same, prompt-free
/// default.
fn use_file_store() -> bool {
    !std::env::var_os("JELLYBEAM_KEYCHAIN").is_some_and(|v| v == "1")
}

fn store_dir() -> PathBuf {
    crate::paths::state_root()
}

fn store_path(account: &str) -> PathBuf {
    store_dir().join(format!("dev-{account}.json"))
}

fn file_read(account: &str) -> Option<Vec<u8>> {
    std::fs::read(store_path(account)).ok()
}

fn file_write(account: &str, bytes: &[u8]) -> Result<(), String> {
    let dir = store_dir();
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    crate::paths::write_private(&store_path(account), bytes).map_err(|e| e.to_string())
}

fn read_blob(account: &str) -> Option<Vec<u8>> {
    if use_file_store() {
        file_read(account)
    } else {
        security_framework::passwords::get_generic_password(SERVICE, account).ok()
    }
}

fn write_blob(account: &str, bytes: &[u8]) -> Result<(), String> {
    if use_file_store() {
        file_write(account, bytes)
    } else {
        security_framework::passwords::set_generic_password(SERVICE, account, bytes)
            .map_err(|e| e.to_string())
    }
}

/// Loads the previously-stored session, if any, as a one-entry list -- kept
/// for the legacy-blob migration path in [`load_sessions`]. Not `pub`: every
/// other caller (main.rs, root.rs) should go through [`load_sessions`]
/// instead, which is guaranteed to reflect the migrated shape.
fn load_legacy_session() -> Option<StoredSession> {
    let bytes = read_blob(SESSION_ACCOUNT)?;
    serde_json::from_slice(&bytes).ok()
}

/// Loads the full multi-server/user session list. Reads the new
/// `SESSION_LIST_ACCOUNT` blob first; if that's absent (an install that
/// predates the multi-session list, or a fresh one), falls back to the legacy
/// single-session blob and wraps it into a one-entry list -- the caller
/// sees one consistent shape either way, and never needs to know which
/// blob it actually came from. Returns an empty list (never `None`) when
/// there's nothing resumable at all, so callers don't need a separate
/// "no sessions yet" branch beyond checking `.sessions.is_empty()`.
pub(crate) fn load_sessions() -> StoredSessionList {
    if let Some(bytes) = read_blob(SESSION_LIST_ACCOUNT) {
        match serde_json::from_slice::<StoredSessionList>(&bytes) {
            Ok(list) => return list,
            Err(e) => {
                // Logging loudly distinguishes "nothing stored" from
                // "something's there but it's corrupt", which would
                // otherwise silently drop every server/user this list
                // remembers back to the single-session legacy fallback below.
                tracing::warn!(
                    error = %e,
                    bytes_len = bytes.len(),
                    "stored session list blob failed to parse; falling back to \
                     the legacy single-session blob (or an empty session list)"
                );
            }
        }
    }
    match load_legacy_session() {
        Some(session) => StoredSessionList {
            sessions: vec![session],
            active: 0,
        },
        None => StoredSessionList::default(),
    }
}

pub(crate) fn save_sessions(list: &StoredSessionList) -> Result<(), String> {
    let bytes = serde_json::to_vec(list).map_err(|e| e.to_string())?;
    write_blob(SESSION_LIST_ACCOUNT, &bytes)
}

#[allow(dead_code)] // no full "remove all sessions" flow yet.
pub(crate) fn clear_sessions() {
    if use_file_store() {
        let _ = std::fs::remove_file(store_path(SESSION_LIST_ACCOUNT));
        let _ = std::fs::remove_file(store_path(SESSION_ACCOUNT));
    } else {
        let _ =
            security_framework::passwords::delete_generic_password(SERVICE, SESSION_LIST_ACCOUNT);
        let _ = security_framework::passwords::delete_generic_password(SERVICE, SESSION_ACCOUNT);
    }
}

/// A stable per-install device id (`ClientIdentity::device_id`), generated
/// once and persisted thereafter. Not cryptographically sensitive -- just
/// needs to stay stable across launches so the server recognizes this as
/// "the same device" (session/capabilities caching).
pub(crate) fn device_id() -> String {
    let existing = if use_file_store() {
        file_read(DEVICE_ID_ACCOUNT)
    } else {
        security_framework::passwords::get_generic_password(SERVICE, DEVICE_ID_ACCOUNT).ok()
    };
    if let Some(bytes) = existing {
        if let Ok(id) = String::from_utf8(bytes) {
            if !id.is_empty() {
                return id;
            }
        }
    }
    let id = generate_id();
    if use_file_store() {
        let _ = file_write(DEVICE_ID_ACCOUNT, id.as_bytes());
    } else {
        let _ = security_framework::passwords::set_generic_password(
            SERVICE,
            DEVICE_ID_ACCOUNT,
            id.as_bytes(),
        );
    }
    id
}

/// A random-enough hex id. Not security-sensitive (see `device_id` doc) --
/// just needs to be stable-once-generated and practically unique on this
/// machine, so a simple time+pid-seeded generator is sufficient; no need to
/// pull in a UUID/RNG crate for this alone.
fn generate_id() -> String {
    use std::hash::BuildHasher;
    use std::time::{SystemTime, UNIX_EPOCH};

    // `RandomState::new()` is seeded from the OS's own randomness source
    // each time it's constructed; two independently-seeded hashers over the
    // same (time, pid) input is enough entropy for a non-security-sensitive
    // device id (see this fn's caller doc comment) without pulling in a
    // dedicated RNG/UUID crate for it alone.
    let seed = (
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default(),
        std::process::id(),
    );

    let h1 = std::collections::hash_map::RandomState::new().hash_one(seed);
    let h2 = std::collections::hash_map::RandomState::new().hash_one((seed, "jellybeam"));

    format!("{h1:016x}{h2:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Thin wrapper binding `crate::test_support::with_temp_home`'s shared
    /// (crate-wide -- see its doc comment on why "shared" matters)
    /// `HOME`-pointing helper to this module's label.
    fn with_temp_home<T>(f: impl FnOnce() -> T) -> T {
        crate::test_support::with_temp_home("keychain", f)
    }

    /// `JELLYBEAM_KEYCHAIN` is process-global env state, same hazard
    /// `test_support::with_temp_home`'s doc comment describes for `HOME`
    /// (`cargo test`'s default multi-threaded runner can interleave this
    /// with any other test in the crate that reads it) -- reusing
    /// `with_temp_home` here piggybacks on its shared, crate-wide
    /// `HOME_ENV_LOCK` for that serialization even though this test doesn't
    /// care about `HOME` itself. Deliberately only exercises the pure
    /// decision function, never the Keychain branch's actual
    /// `security-framework` calls -- doing that from an automated test run
    /// risks the exact interactive authorization prompt this whole gate
    /// exists to avoid hitting unattended (see docs/BUILD.md "Keychain
    /// caveat").
    #[test]
    fn use_file_store_defaults_to_true_and_only_jellybeam_keychain_flips_it() {
        with_temp_home(|| {
            // SAFETY: serialized by `with_temp_home`'s `HOME_ENV_LOCK` --
            // no other test in this crate reads/writes `JELLYBEAM_KEYCHAIN`
            // concurrently with this block.
            unsafe { std::env::remove_var("JELLYBEAM_KEYCHAIN") };
            assert!(
                use_file_store(),
                "unset JELLYBEAM_KEYCHAIN must default to the file store"
            );

            unsafe { std::env::set_var("JELLYBEAM_KEYCHAIN", "1") };
            assert!(
                !use_file_store(),
                "JELLYBEAM_KEYCHAIN=1 must opt into the real Keychain"
            );

            unsafe { std::env::set_var("JELLYBEAM_KEYCHAIN", "0") };
            assert!(
                use_file_store(),
                "any value other than exactly \"1\" must not opt into the Keychain"
            );

            unsafe { std::env::remove_var("JELLYBEAM_KEYCHAIN") };
        });
    }

    fn sample(base_url: &str, user: &str) -> StoredSession {
        StoredSession {
            base_url: base_url.to_string(),
            token: "tok".to_string(),
            user_id: Some(user.to_string()),
            username: Some(user.to_string()),
            server_version: None,
        }
    }

    #[test]
    fn stored_session_debug_redacts_credentials_and_identity() {
        let mut session = sample("https://private.example.test", "synthetic-user");
        session.token = "synthetic-bearer-value".to_string();
        let text = format!("{session:?}");
        for sensitive in [
            &session.base_url,
            &session.token,
            session.user_id.as_ref().expect("user"),
        ] {
            assert!(!text.contains(sensitive));
        }
        assert!(text.contains("<redacted>"));
    }

    #[test]
    fn load_sessions_migrates_a_legacy_single_session_blob() {
        with_temp_home(|| {
            let legacy = sample("http://localhost:8096", "user-1");
            save_session_legacy_for_test(&legacy);

            let list = load_sessions();
            assert_eq!(list.sessions.len(), 1);
            assert_eq!(list.active, 0);
            assert_eq!(list.sessions[0].base_url, "http://localhost:8096");

            // The migrated shape should now also be readable back out as a
            // list once saved -- simulating the app re-persisting after
            // migration.
            save_sessions(&list).expect("save migrated list");
            let reloaded = load_sessions();
            assert_eq!(reloaded.sessions.len(), 1);
        });
    }

    #[test]
    fn load_sessions_returns_empty_list_when_nothing_stored() {
        with_temp_home(|| {
            let list = load_sessions();
            assert!(list.sessions.is_empty());
        });
    }

    /// `server_version` round-trips through serde, and -- the actual point
    /// of this test -- a pre-existing blob serialized *without* the field
    /// (an install from before it existed) still deserializes, with
    /// `server_version` defaulting to `None` rather than failing the whole
    /// load.
    #[test]
    fn stored_session_server_version_round_trips_and_defaults_on_old_blobs() {
        let mut with_version = sample("http://localhost:8096", "u1");
        with_version.server_version = Some("12.0.0".to_string());
        let json = serde_json::to_string(&with_version).expect("serialize");
        let back: StoredSession = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.server_version.as_deref(), Some("12.0.0"));

        // Simulate a pre-field blob: the same JSON with the key removed.
        let without_field = r#"{
            "base_url": "http://localhost:8096",
            "token": "tok",
            "user_id": "u1",
            "username": "u1"
        }"#;
        let old: StoredSession = serde_json::from_str(without_field)
            .expect("old blob without server_version must still deserialize");
        assert_eq!(old.server_version, None);

        let list = StoredSessionList {
            sessions: vec![with_version.clone(), old],
            active: 0,
        };
        let list_json = serde_json::to_string(&list).expect("serialize list");
        let reloaded: StoredSessionList =
            serde_json::from_str(&list_json).expect("deserialize list");
        assert_eq!(
            reloaded.sessions[0].server_version.as_deref(),
            Some("12.0.0")
        );
        assert_eq!(reloaded.sessions[1].server_version, None);
    }

    /// Pins: `file_write` replaces the blob atomically (temp + rename), keeps the store owner-only, and leaves no temp file behind.
    #[test]
    fn file_write_replaces_atomically_with_owner_only_perms_and_no_temp_residue() {
        with_temp_home(|| {
            let mut list = StoredSessionList::default();
            list.add_or_update(sample("http://s1", "u1"));
            save_sessions(&list).expect("first save");

            // Overwrite with a second save -- the reload must see exactly
            // the new content (rename landed), not a blank/torn file.
            list.add_or_update(sample("http://s2", "u2"));
            save_sessions(&list).expect("second save");
            let reloaded = load_sessions();
            assert_eq!(reloaded.sessions.len(), 2);

            use std::os::unix::fs::PermissionsExt;
            let dir = store_dir();
            let mut saw_blob = false;
            for entry in std::fs::read_dir(&dir).expect("read store dir") {
                let entry = entry.expect("dir entry");
                let name = entry.file_name().to_string_lossy().into_owned();
                assert!(
                    !name.contains(".tmp-"),
                    "temp file left behind in session store: {name}"
                );
                let mode = entry.metadata().expect("stat").permissions().mode() & 0o777;
                assert_eq!(mode, 0o600, "{name} must be owner-only, got {mode:o}");
                saw_blob = true;
            }
            assert!(saw_blob, "expected at least one stored blob");
        });
    }

    #[test]
    fn add_or_update_replaces_same_server_and_user_rather_than_duplicating() {
        let mut list = StoredSessionList::default();
        list.add_or_update(sample("http://s1", "u1"));
        list.add_or_update(sample("http://s2", "u1"));
        assert_eq!(list.sessions.len(), 2);
        assert_eq!(list.active, 1);

        let mut updated = sample("http://s1", "u1");
        updated.token = "new-token".to_string();
        list.add_or_update(updated);
        assert_eq!(
            list.sessions.len(),
            2,
            "re-adding the same server+user must not duplicate"
        );
        assert_eq!(list.active, 0, "re-added session becomes active");
        assert_eq!(list.sessions[0].token, "new-token");
    }

    #[test]
    fn remove_keeps_active_index_in_bounds() {
        let mut list = StoredSessionList::default();
        list.add_or_update(sample("http://s1", "u1"));
        list.add_or_update(sample("http://s2", "u1"));
        list.add_or_update(sample("http://s3", "u1"));
        list.active = 2;

        list.remove(0);
        assert_eq!(list.sessions.len(), 2);
        assert_eq!(
            list.active, 1,
            "removing before active shifts it down by one"
        );

        list.remove(1);
        assert_eq!(list.sessions.len(), 1);
        assert_eq!(list.active, 0, "active clamps to the last valid index");

        list.remove(0);
        assert!(list.sessions.is_empty());
        assert_eq!(list.active, 0);
    }

    /// Test-only helper writing the legacy single-session blob shape
    /// directly (bypassing the now-list-only public `save_sessions`), so
    /// `load_sessions_migrates_a_legacy_single_session_blob` can exercise
    /// the real migration path.
    fn save_session_legacy_for_test(session: &StoredSession) {
        let bytes = serde_json::to_vec(session).expect("serialize");
        write_blob(SESSION_ACCOUNT, &bytes).expect("write legacy blob");
    }
}
