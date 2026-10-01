//! On-disk per-account Seerr config store: `<data_dir>/seerr.json`, a flat
//! list of entries keyed by the Jellyfin `(server_url, user_id)` identity.
//! Tolerant load -- a missing or corrupt file reads as "nothing
//! configured".
//!
//! Every entry point takes `data_dir` and the `(server_url, user_id)` pair
//! as plain arguments rather than resolving them from any ambient session
//! state; the app supplies its own active-session identity explicitly on
//! every call.

use std::path::{Path, PathBuf};

use crate::client::SeerrAuthMethod;

/// One saved Seerr connection. `secret` is a plaintext password/API key,
/// same storage contract as the app's own Jellyfin token persistence --
/// never logged, and never printed via [`std::fmt::Debug`] (see the manual
/// impl below, which deliberately does not derive `Debug`).
#[derive(Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SeerrConfigEntry {
    pub server_url: String,
    pub user_id: String,
    pub seerr_url: String,
    #[serde(default)]
    pub method: SeerrAuthMethod,
    #[serde(default)]
    pub identity: String,
    #[serde(default)]
    pub secret: String,
}

impl std::fmt::Debug for SeerrConfigEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SeerrConfigEntry")
            .field("server_url", &self.server_url)
            .field("user_id", &self.user_id)
            .field("seerr_url", &self.seerr_url)
            .field("method", &self.method)
            .field("identity", &self.identity)
            .field("secret", &"<redacted>")
            .finish()
    }
}

/// `<data_dir>/seerr.json`'s whole shape -- a flat list, no "active" index:
/// the caller decides which entry is active by passing the right
/// `(server_url, user_id)` to [`SeerrConfigStore::find`].
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SeerrConfigStore {
    #[serde(default)]
    entries: Vec<SeerrConfigEntry>,
}

fn config_path(data_dir: &Path) -> PathBuf {
    data_dir.join("seerr.json")
}

impl SeerrConfigStore {
    /// Tolerant load -- a missing or corrupt `seerr.json` reads as "nothing configured".
    pub fn load(data_dir: &Path) -> Self {
        std::fs::read(config_path(data_dir))
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, data_dir: &Path) -> std::io::Result<()> {
        use std::io::Write;

        let json = serde_json::to_vec_pretty(self)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        std::fs::create_dir_all(data_dir)?;
        // SECURITY.md: credentials use an owner-only atomic replacement so a crash cannot tear the store.
        let mut file = tempfile::NamedTempFile::new_in(data_dir)?;
        file.write_all(&json)?;
        file.as_file().sync_all()?;
        file.persist(config_path(data_dir)).map_err(|e| e.error)?;
        Ok(())
    }

    pub fn find(&self, server_url: &str, user_id: &str) -> Option<&SeerrConfigEntry> {
        self.entries
            .iter()
            .find(|e| e.server_url == server_url && e.user_id == user_id)
    }

    /// Inserts `entry`, or replaces an existing entry for the same
    /// `(server_url, user_id)` pair.
    pub fn upsert(&mut self, entry: SeerrConfigEntry) {
        if let Some(existing) = self
            .entries
            .iter_mut()
            .find(|e| e.server_url == entry.server_url && e.user_id == entry.user_id)
        {
            *existing = entry;
        } else {
            self.entries.push(entry);
        }
    }

    pub fn remove(&mut self, server_url: &str, user_id: &str) {
        self.entries
            .retain(|e| !(e.server_url == server_url && e.user_id == user_id));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_entry(server_url: &str, user_id: &str) -> SeerrConfigEntry {
        SeerrConfigEntry {
            server_url: server_url.to_string(),
            user_id: user_id.to_string(),
            seerr_url: "http://seerr.test/api/v1".to_string(),
            method: SeerrAuthMethod::Jellyfin,
            identity: "alice".to_string(),
            secret: "hunter2".to_string(),
        }
    }

    #[test]
    fn debug_never_prints_the_secret() {
        let entry = sample_entry("http://jf.test", "user-1");
        let rendered = format!("{entry:?}");
        assert!(!rendered.contains("hunter2"), "{rendered}");
        assert!(rendered.contains("<redacted>"), "{rendered}");
    }

    #[test]
    fn roundtrips_through_disk() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut store = SeerrConfigStore::default();
        store.upsert(sample_entry("http://jf.test", "user-1"));
        store.save(dir.path()).expect("save");

        let loaded = SeerrConfigStore::load(dir.path());
        assert_eq!(loaded, store);
    }

    #[cfg(unix)]
    #[test]
    fn save_replaces_a_permissive_store_with_owner_only_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("tempdir");
        let path = config_path(dir.path());
        std::fs::write(&path, b"old content").expect("write old store");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644))
            .expect("set old permissions");
        let mut store = SeerrConfigStore::default();
        store.upsert(sample_entry("http://jf.test", "user-1"));
        store.save(dir.path()).expect("save");

        assert_eq!(
            std::fs::metadata(&path)
                .expect("metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(SeerrConfigStore::load(dir.path()), store);
        assert_eq!(std::fs::read_dir(dir.path()).expect("entries").count(), 1);
    }

    #[test]
    fn load_returns_empty_when_no_file_exists() {
        let dir = tempfile::tempdir().expect("tempdir");
        let loaded = SeerrConfigStore::load(dir.path());
        assert!(loaded.find("anything", "anyone").is_none());
    }

    #[test]
    fn load_tolerates_corrupt_json() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(config_path(dir.path()), b"not json").expect("write garbage");
        let loaded = SeerrConfigStore::load(dir.path());
        assert!(loaded.find("anything", "anyone").is_none());
    }

    #[test]
    fn load_tolerates_unknown_fields_and_missing_optional_ones() {
        let dir = tempfile::tempdir().expect("tempdir");
        let raw = serde_json::json!({
            "entries": [
                {
                    "server_url": "http://jf.test",
                    "user_id": "user-1",
                    "seerr_url": "http://seerr.test/api/v1",
                    "bogus_future_field": {"anything": "goes here"},
                }
            ]
        });
        std::fs::write(
            config_path(dir.path()),
            serde_json::to_vec(&raw).expect("serialize"),
        )
        .expect("write seerr.json");

        let loaded = SeerrConfigStore::load(dir.path());
        let entry = loaded
            .find("http://jf.test", "user-1")
            .expect("entry should load despite the unknown field");
        assert_eq!(entry.seerr_url, "http://seerr.test/api/v1");
        // method/identity/secret were all omitted -- tolerant defaults.
        assert_eq!(entry.method, SeerrAuthMethod::Jellyfin);
        assert_eq!(entry.identity, "");
        assert_eq!(entry.secret, "");
    }

    #[test]
    fn keys_two_accounts_independently() {
        let mut store = SeerrConfigStore::default();
        let mut entry_a = sample_entry("http://jf-a.test", "user-a");
        entry_a.seerr_url = "http://seerr-a.test/api/v1".to_string();
        let mut entry_b = sample_entry("http://jf-b.test", "user-b");
        entry_b.seerr_url = "http://seerr-b.test/api/v1".to_string();
        store.upsert(entry_a.clone());
        store.upsert(entry_b.clone());

        assert_eq!(store.find("http://jf-a.test", "user-a"), Some(&entry_a));
        assert_eq!(store.find("http://jf-b.test", "user-b"), Some(&entry_b));
        assert!(store.find("http://jf-a.test", "user-b").is_none());
    }

    #[test]
    fn upsert_replaces_only_the_matching_account() {
        let mut store = SeerrConfigStore::default();
        store.upsert(sample_entry("http://jf-a.test", "user-a"));
        store.upsert(sample_entry("http://jf-b.test", "user-b"));

        let mut updated_a = sample_entry("http://jf-a.test", "user-a");
        updated_a.secret = "new-secret".to_string();
        store.upsert(updated_a);

        assert_eq!(
            store
                .find("http://jf-a.test", "user-a")
                .expect("entry a exists")
                .secret,
            "new-secret"
        );
        assert_eq!(
            store
                .find("http://jf-b.test", "user-b")
                .expect("entry b exists")
                .secret,
            "hunter2"
        );
    }

    #[test]
    fn remove_drops_only_the_named_account() {
        let mut store = SeerrConfigStore::default();
        store.upsert(sample_entry("http://jf-a.test", "user-a"));
        store.upsert(sample_entry("http://jf-b.test", "user-b"));

        store.remove("http://jf-a.test", "user-a");

        assert!(store.find("http://jf-a.test", "user-a").is_none());
        assert!(store.find("http://jf-b.test", "user-b").is_some());
    }
}
