//! SQLite mirror schema per docs/DATA.md §1: WAL mode, `schema_version` in the
//! `meta` table checked at open, drop-and-rebuild on mismatch (disposable
//! cache, never a migration target).

use std::path::Path;

use rusqlite::Connection;

use crate::{CacheError, SCHEMA_VERSION};

/// DDL for the mirror. `CREATE ... IF NOT EXISTS` throughout so this can run
/// unconditionally right after (re)opening the file.
pub(crate) const SCHEMA_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS meta (
    key TEXT PRIMARY KEY,
    value TEXT
);

CREATE TABLE IF NOT EXISTS views (
    id TEXT PRIMARY KEY,
    name TEXT,
    collection_type TEXT,
    sort_index INTEGER,
    -- Channel-views decision (docs/PLUGIN-CHANNELS.md
    -- §2.1), schema v10: the `/UserViews` entry's own `Type` (e.g.
    -- "UserView", "CollectionFolder", "Channel") -- distinct from
    -- `collection_type` above (its `CollectionType`, which a `Channel` view
    -- never sets). `sync::current_views` excludes rows where this is
    -- 'Channel' from every per-library sync walk: that content is browsed
    -- live instead (see `jellyfin_api::JellyfinClient::live_children`).
    item_type TEXT
);

CREATE TABLE IF NOT EXISTS items (
    id TEXT PRIMARY KEY,
    parent_id TEXT,
    series_id TEXT,
    season_id TEXT,
    item_type TEXT NOT NULL,
    name TEXT,
    sort_name TEXT,
    index_number INTEGER,
    parent_index_number INTEGER,
    production_year INTEGER,
    premiere_date TEXT,
    runtime_ticks INTEGER,
    date_created TEXT,
    played INTEGER NOT NULL DEFAULT 0,
    playback_position_ticks INTEGER NOT NULL DEFAULT 0,
    play_count INTEGER NOT NULL DEFAULT 0,
    is_favorite INTEGER NOT NULL DEFAULT 0,
    unplayed_item_count INTEGER,
    primary_tag TEXT,
    primary_blurhash TEXT,
    -- docs/DESIGN-PLAYER-NAV.md §2.5: artwork fallback-chain columns.
    -- A Season/Episode row often has no image of its own -- these carry the
    -- ancestor image references `BaseItemDto` already supplies (per-item,
    -- server-computed) so `cards.rs`/`detail.rs` can fall back to the
    -- series poster / parent backdrop instead of a flat gray tile. See
    -- `rows::extract_columns`.
    series_primary_tag TEXT,
    parent_backdrop_item_id TEXT,
    parent_backdrop_tag TEXT,
    -- The owning library (`views.id`) for this item, so `latest()` and
    -- reconciliation can scope by library instead of item_type alone --
    -- without this, two libraries of the same `collection_type` (e.g. two
    -- "Shows" libraries) had no way to tell their items apart, so
    -- "Latest in <library>" showed identical results for both. Stamped by
    -- the sync engine (`sync::sync_library_breadth` -- a view's recursive
    -- breadth-sync page authoritatively knows every item in it belongs to
    -- that view), not derived from the `BaseItemDto` itself (the server
    -- doesn't return a "which UserView is this under" field). WS-delta
    -- writes that can't cheaply attribute a whole batch to one library bind
    -- `NULL` here; the upsert's `ON CONFLICT` uses `COALESCE(excluded, old)`
    -- so that never clobbers an already-known value -- see `writer.rs`.
    library_id TEXT,
    -- Server-authoritative `UserData.LastPlayedDate` (RFC3339),
    -- distinct from `updated_at` (this mirror's *local write clock*, "for
    -- debugging only" -- see `updated_at`'s own doc comment below). Startup
    -- reconciliation re-upserting rows over several seconds bumps
    -- `updated_at` on every one of them regardless of whether their watch
    -- state actually changed, which -- when Continue Watching sorted by
    -- `updated_at` -- made the ribbon visibly reorder itself throughout
    -- initial sync even though nothing the user did changed. Sorting by
    -- this column instead means only a *real* watch-state change (a fresh
    -- play/stop, local or server-pushed) moves an item, so Continue
    -- Watching follows what the user actually watched. `apply_local_user_data` stamps it to
    -- "now" on this client's own reports so a fresh stop sorts first without
    -- waiting on a server round trip.
    last_played_date TEXT,
    -- Visual-pass step 4 (§6): the episode grid's 2-line synopsis needs the
    -- item's own overview text available on the browse path (`CardRow`)
    -- without falling back to a live/blob fetch -- see `CardRow::overview`'s
    -- doc comment in lib.rs. Schema v7.
    overview TEXT,
    -- `BaseItemDto.LocationType == "Virtual"` --
    -- an unaired or missing episode the server lists as a placeholder, with
    -- no `MediaSources` to actually play. `premiere_date` (above, already
    -- present since before this column) distinguishes "unaired" (future
    -- date) from "missing" (past/no date) for the reason text `cards.rs`/
    -- `detail.rs` show -- see `CardRow::is_virtual`'s doc comment in lib.rs.
    -- Schema v8.
    is_virtual INTEGER NOT NULL DEFAULT 0,
    -- The owning series' *display name* (`SeriesName` on
    -- `BaseItemDto`), for a Season/Episode row. Previously only ever carried
    -- into the FTS5 `search` virtual table below (search-only, not
    -- retrievable as a plain column) -- a Continue Watching/Next Up episode
    -- card needs it on the browse path itself to print "series name at 60%
    -- opacity" underneath the episode title without an extra live-DTO fetch
    -- per card. See `CardRow::series_name`'s doc comment in lib.rs.
    -- Schema v9.
    series_name TEXT,
    dto BLOB NOT NULL,
    -- Local write clock (unix millis) -- when *this mirror* last wrote the
    -- row, not a server-meaningful timestamp. Debugging/diagnostics only;
    -- do not sort user-facing lists by it (see `last_played_date`, above,
    -- for the column that's actually safe to sort Continue Watching by).
    updated_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_items_browse ON items(parent_id, item_type, sort_name);
-- `library_id` first (single equality, the common filter for both
-- `latest()` and reconciliation's local/server count comparison), then
-- `item_type` (equality or small IN list), then `date_created DESC` so
-- `latest()`'s ORDER BY is index-served for the common single-item-type
-- case. Superset of a plain `(item_type, date_created DESC)` index --
-- a `latest()`/reconciliation call is never issued without a view id.
CREATE INDEX IF NOT EXISTS idx_items_latest ON items(library_id, item_type, date_created DESC);
CREATE INDEX IF NOT EXISTS idx_items_series ON items(series_id, parent_index_number, index_number);
CREATE INDEX IF NOT EXISTS idx_items_resume ON items(playback_position_ticks) WHERE playback_position_ticks > 0;
-- `resume()`'s ORDER BY moved from `updated_at` (local write
-- clock -- see `last_played_date`'s doc comment above) to this column, so it
-- needs its own partial index covering both the filter and the sort, same
-- shape as `idx_items_resume` just above.
CREATE INDEX IF NOT EXISTS idx_items_resume_by_last_played ON items(last_played_date DESC) WHERE playback_position_ticks > 0;
-- §2.7: `children()` with `Sort::IndexNumber` (Series -> Seasons,
-- Season -> Episodes) needs its own parent_id-scoped ordering index --
-- `idx_items_browse` sorts by `sort_name`, not `(parent_index_number,
-- index_number)`, so without this SQLite would fall back to a temp b-tree
-- sort on every Detail visit. Covers both the equality filter and the
-- ORDER BY, so EXPLAIN QUERY PLAN never needs a separate sort step.
CREATE INDEX IF NOT EXISTS idx_items_parent_order ON items(parent_id, parent_index_number, index_number);

CREATE VIRTUAL TABLE IF NOT EXISTS search USING fts5(
    name, original_title, series_name, overview,
    content='', tokenize='unicode61 remove_diacritics 2'
);

CREATE TABLE IF NOT EXISTS image_lru (
    key TEXT PRIMARY KEY,
    bytes INTEGER,
    last_access INTEGER
);

-- A BoxSet's children can't be expressed by the single
-- `items.parent_id` column (an item can belong to arbitrarily many
-- collections, and a BoxSet's server-defined membership isn't a parent/child
-- relationship at all). Populated per-BoxSet from a non-recursive
-- `/Items?ParentId=<boxset_id>` fetch; `sort_index` preserves the order the
-- server returned members in.
CREATE TABLE IF NOT EXISTS collection_members (
    collection_id TEXT NOT NULL,
    item_id TEXT NOT NULL,
    sort_index INTEGER,
    PRIMARY KEY (collection_id, item_id)
);
-- Covers both the `children()` membership join's filter (collection_id = ?)
-- and its ORDER BY sort_index, so EXPLAIN QUERY PLAN never needs a temp
-- b-tree sort or a scan for a BoxSet's children.
CREATE INDEX IF NOT EXISTS idx_collection_members_order ON collection_members(collection_id, sort_index);
"#;

fn db_err(e: impl std::fmt::Display) -> CacheError {
    CacheError::Db(e.to_string())
}

fn read_schema_version(conn: &Connection) -> Option<u32> {
    conn.query_row(
        "SELECT value FROM meta WHERE key = 'schema_version'",
        [],
        |row| row.get::<_, String>(0),
    )
    .ok()
    .and_then(|v| v.parse::<u32>().ok())
}

/// Remove the db file and any WAL/SHM/journal siblings. Best-effort: a
/// missing sibling file is not an error.
fn remove_db_files(path: &Path) {
    let _ = std::fs::remove_file(path);
    for suffix in ["-wal", "-shm", "-journal"] {
        let mut sibling = path.as_os_str().to_owned();
        sibling.push(suffix);
        let _ = std::fs::remove_file(std::path::PathBuf::from(sibling));
    }
}

/// Force owner-only (0600) permissions on the mirror db and its
/// WAL/SHM/journal sidecars. Best-effort — a missing sidecar or a perms
/// error is ignored (the mirror still works; this is defense-in-depth for
/// shared machines, matching the 0600 session store).
#[cfg(unix)]
fn harden_db_perms(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let owner_only = std::fs::Permissions::from_mode(0o600);
    let _ = std::fs::set_permissions(path, owner_only.clone());
    for suffix in ["-wal", "-shm", "-journal"] {
        let mut sibling = path.as_os_str().to_owned();
        sibling.push(suffix);
        let sibling = std::path::PathBuf::from(sibling);
        if sibling.exists() {
            let _ = std::fs::set_permissions(&sibling, owner_only.clone());
        }
    }
}

#[cfg(not(unix))]
fn harden_db_perms(_path: &Path) {}

/// Open the mirror at `path`, dropping and recreating it first if the file
/// exists but its `schema_version` doesn't match (or can't be read at all —
/// treated the same as a mismatch: this cache is disposable). Returns the
/// open read-write connection with the schema applied and `schema_version`
/// recorded, plus whether the `items` table was empty (drives whether the
/// sync engine needs a full initial sync).
pub(crate) fn open_and_prepare(path: &Path) -> Result<(Connection, bool), CacheError> {
    if path.exists() {
        let mismatched = match Connection::open(path) {
            Ok(probe) => read_schema_version(&probe) != Some(SCHEMA_VERSION),
            Err(_) => true,
        };
        if mismatched {
            tracing::info!(
                ?path,
                "schema_version mismatch (or unreadable); rebuilding mirror"
            );
            remove_db_files(path);
        }
    }

    let conn = Connection::open(path).map_err(db_err)?;
    conn.pragma_update(None, "journal_mode", "WAL")
        .map_err(db_err)?;
    conn.pragma_update(None, "synchronous", "NORMAL")
        .map_err(db_err)?;
    // The mirror holds the full library catalog (titles, series
    // names, overviews). Default file creation is world-readable (0644); on
    // a shared machine any other local user could read what the user's
    // libraries contain. Harden the db and its WAL/SHM sidecars (created by
    // the WAL pragma just above) to owner-only, matching the 0600 session
    // store. Best-effort: a perms failure must not stop the mirror opening.
    harden_db_perms(path);
    conn.execute_batch(SCHEMA_SQL).map_err(db_err)?;
    conn.execute(
        "INSERT INTO meta (key, value) VALUES ('schema_version', ?1)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        [SCHEMA_VERSION.to_string()],
    )
    .map_err(db_err)?;

    let is_empty: i64 = conn
        .query_row("SELECT COUNT(*) FROM items", [], |row| row.get(0))
        .map_err(db_err)?;

    Ok((conn, is_empty == 0))
}

/// Open one additional read-only connection against an already-prepared
/// mirror file (used to build the read pool).
pub(crate) fn open_reader(path: &Path) -> Result<Connection, CacheError> {
    let conn = Connection::open_with_flags(
        path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
    )
    .map_err(db_err)?;
    // Readers benefit from WAL's non-blocking reads; busy_timeout smooths
    // over the rare moment a reader opens mid-checkpoint.
    conn.busy_timeout(std::time::Duration::from_millis(2000))
        .map_err(db_err)?;
    Ok(conn)
}

pub(crate) fn upsert_meta(conn: &Connection, key: &str, value: &str) -> Result<(), CacheError> {
    conn.execute(
        "INSERT INTO meta (key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        rusqlite::params![key, value],
    )
    .map_err(db_err)?;
    Ok(())
}

pub(crate) fn read_meta(conn: &Connection, key: &str) -> Option<String> {
    conn.query_row("SELECT value FROM meta WHERE key = ?1", [key], |row| {
        row.get(0)
    })
    .ok()
}

/// Fresh temp-dir mirror for a test -- shared by `query`'s, `writer`'s, and
/// `sync`'s test modules so all three open a db the same way.
#[cfg(test)]
pub(crate) fn open_test_db() -> (tempfile::TempDir, Connection) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("mirror.db");
    let (conn, _) = open_and_prepare(&path).expect("open");
    (dir, conn)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_open_creates_schema_and_reports_empty() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("mirror.db");
        let (conn, empty) = open_and_prepare(&path).expect("open");
        assert!(empty);
        assert_eq!(read_schema_version(&conn), Some(SCHEMA_VERSION));
    }

    /// The mirror db and its WAL/SHM sidecars must be owner-only
    /// (0600), not the world-readable 0644 that default file creation gives.
    #[cfg(unix)]
    #[test]
    fn mirror_db_and_sidecars_are_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("mirror.db");
        let (_conn, _) = open_and_prepare(&path).expect("open");
        let mode = |p: &Path| std::fs::metadata(p).expect("stat").permissions().mode() & 0o777;
        assert_eq!(mode(&path), 0o600, "mirror.db must be owner-only");
        // WAL mode created these sidecars; whichever exist must be 0600 too.
        for suffix in ["-wal", "-shm"] {
            let mut sib = path.as_os_str().to_owned();
            sib.push(suffix);
            let sib = std::path::PathBuf::from(sib);
            if sib.exists() {
                assert_eq!(mode(&sib), 0o600, "{suffix} sidecar must be owner-only");
            }
        }
    }

    #[test]
    fn reopen_with_same_version_preserves_data() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("mirror.db");
        {
            let (conn, _) = open_and_prepare(&path).expect("open");
            conn.execute(
                "INSERT INTO items (id, item_type, dto, updated_at) VALUES ('x', 'Movie', '{}', 0)",
                [],
            )
            .expect("insert");
        }
        let (_conn, empty) = open_and_prepare(&path).expect("reopen");
        assert!(
            !empty,
            "existing row should survive a reopen at the same schema version"
        );
    }

    #[test]
    fn version_mismatch_drops_and_rebuilds() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("mirror.db");
        {
            let (conn, _) = open_and_prepare(&path).expect("open");
            conn.execute(
                "INSERT INTO items (id, item_type, dto, updated_at) VALUES ('x', 'Movie', '{}', 0)",
                [],
            )
            .expect("insert");
            upsert_meta(&conn, "schema_version", "999").expect("bump version");
        }
        let (_conn, empty) = open_and_prepare(&path).expect("reopen after mismatch");
        assert!(
            empty,
            "version mismatch must drop and rebuild, losing prior rows"
        );
    }

    #[test]
    fn corrupt_or_missing_meta_table_is_treated_as_mismatch() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("mirror.db");
        // A file exists but has no schema at all.
        {
            let conn = Connection::open(&path).expect("open raw");
            conn.execute("CREATE TABLE unrelated (x INTEGER)", [])
                .expect("create");
        }
        let (_conn, empty) = open_and_prepare(&path).expect("open despite garbage file");
        assert!(empty);
    }
}
