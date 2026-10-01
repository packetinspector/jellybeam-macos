//! SQLite mirror + image cache per docs/DATA.md. Disposable cache: schema_version
//! mismatch drops and rebuilds. Single-writer; readers never block on network.

use jellyfin_api::models::BaseItemDto;

mod image_cache;
#[cfg(test)]
mod mock_server;
mod pool;
mod query;
mod rows;
mod schema;
mod sync;
mod writer;

pub use image_cache::{ImageCache, ImageCacheStats};

/// Maps a view's `collection_type` to the root `item_type`(s) whose
/// `date_created` reflects "recently added" for that library, matching real
/// Jellyfin's Latest-Media behavior (e.g. a TV library's Latest surfaces
/// recently added Episodes, not Series). Used by `query::latest` and by
/// `sync::reconcile_view` to scope reconciliation's count comparison the
/// same way.
pub(crate) fn item_types_for_collection(collection_type: &str) -> &'static [&'static str] {
    match collection_type {
        "movies" => &["Movie"],
        "tvshows" => &["Episode"],
        "boxsets" => &["BoxSet"],
        "music" => &["Audio"],
        "musicvideos" => &["MusicVideo"],
        "homevideos" => &["Video", "Photo"],
        _ => &[],
    }
}

/// Bumped 1 -> 2: adds the `collection_members` table (BoxSet membership).
/// Disposable cache -- drop-and-rebuild on mismatch covers the migration.
///
/// Bumped 2 -> 3: fixes empty `parent_id` on existing Season/Episode rows
/// (see `rows::browse_parent_id`) -- reconciliation's count/date check would
/// never notice or heal this on its own.
///
/// Bumped 3 -> 4: adds the `series_primary_tag`/`parent_backdrop_item_id`/
/// `parent_backdrop_tag` artwork-fallback columns and the
/// `idx_items_parent_order` index `Sort::IndexNumber` needs
/// (docs/DESIGN-PLAYER-NAV.md §2.5/§2.7) -- an already-synced mirror has
/// neither.
///
/// Bumped 4 -> 5: adds `library_id` and reshapes `idx_items_latest` to lead
/// with it -- an already-synced mirror has every row's `library_id` as
/// `NULL`, which `latest()`'s per-library scoping would misread as "not in
/// any library" instead of triggering a resync.
///
/// Bumped 5 -> 6: adds `last_played_date` and
/// `idx_items_resume_by_last_played` -- `resume()` now sorts by it instead
/// of the local write clock, and an already-synced mirror has it `NULL`
/// until a resync backfills it from `UserData.LastPlayedDate`.
///
/// Bumped 6 -> 7: adds the `overview` column for the episode grid's 2-line
/// synopsis -- `NULL` until a resync backfills it.
///
/// Bumped 7 -> 8: adds `is_virtual`/`premiere_date` -- an already-synced
/// mirror defaults `is_virtual` to false, which would misreport every
/// unaired/missing episode as playable until a resync populates it from
/// `LocationType`.
///
/// Bumped 8 -> 9: adds `series_name` so a Continue Watching/Next Up card can
/// show the series name without an extra live-DTO fetch -- `NULL` until a
/// resync backfills it.
///
/// Bumped 9 -> 10: adds `views.item_type`, the `/UserViews` entry's own
/// `Type` as opposed to `collection_type` (docs/PLUGIN-CHANNELS.md
/// §2.1) -- `sync::current_views`/`reconcile_all` use it to
/// exclude `Channel` rows from every sync walk.
///
/// Bumped 10 -> 11: drops `community_rating`/`official_rating`/
/// `backdrop_tag`/`thumb_tag` -- written on every upsert but never read
/// back by any query.
pub const SCHEMA_VERSION: u32 = 11;

/// Read connections held open per `Mirror` (docs/DATA.md §1's "read-only pool").
const READ_POOL_SIZE: usize = 4;

/// A library row mirrored from `/UserViews` into the `views` table.
#[derive(Debug, Clone)]
pub(crate) struct ViewRow {
    pub id: String,
    pub name: String,
    pub collection_type: String,
    /// The `/UserViews` entry's own `Type` (e.g. `"UserView"`,
    /// `"CollectionFolder"`, `"Channel"`) -- distinct from `collection_type`
    /// (its `CollectionType`), which a `Channel` view never sets. See
    /// `SCHEMA_VERSION`'s 9 -> 10 doc comment for why this exists.
    pub item_type: String,
}

/// A browsable entry from the drawer's library list -- [`Mirror::views`]'s
/// public return type. `kind` lets the UI branch: a `Channel` view's
/// content is browsed live, never through [`Mirror::children`]. `name` is
/// the server-configured name, verbatim -- never rewritten.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViewSummary {
    pub id: String,
    pub name: String,
    pub kind: ViewKind,
}

/// See [`ViewSummary::kind`]. Defaults to `Library`; only a `Channel`
/// `/UserViews` entry maps to [`ViewKind::Channel`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ViewKind {
    #[default]
    Library,
    Channel,
}

impl From<&str> for ViewKind {
    fn from(item_type: &str) -> Self {
        if item_type == "Channel" {
            ViewKind::Channel
        } else {
            ViewKind::Library
        }
    }
}

/// Settings-panel-driven `/Shows/NextUp` filtering. `media-cache` must not
/// depend on the `app` crate, so the app pushes the current values down via
/// [`Mirror::set_next_up_options`] instead (same shape as
/// `MirrorState::playback_active`). `sync::refresh_next_up` reads the
/// stored value fresh on every call, so a settings change takes effect on
/// the very next refresh.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct NextUpOptions {
    /// `Some(days)` sends `nextUpDateCutoff` = "now minus `days` days" on
    /// the next `/Shows/NextUp` request (see `sync::next_up_date_cutoff`);
    /// `None` ("Off" in Settings) omits the param, today's unfiltered
    /// behavior.
    pub cutoff_days: Option<u32>,
    /// Mirrors `jellyfin_api::NextUpOptions::enable_rewatching` -- see its
    /// doc comment.
    pub rewatching: bool,
}

/// Shared state behind every `Mirror` clone: the single writer's command
/// queue, the read-only pool, the server connection (for the sync engine),
/// and the change-feed broadcaster.
pub(crate) struct MirrorState {
    pub(crate) client: jellyfin_api::JellyfinClient,
    pub(crate) writer: writer::WriterHandle,
    /// Arc'd so functions holding only `&MirrorState` can cheaply clone a
    /// handle to move into `spawn_blocking`: `pool.acquire()`'s blocking
    /// mutex/condvar wait must not run inline on a tokio worker thread.
    pub(crate) read_pool: std::sync::Arc<pool::ReadPool>,
    pub(crate) changes_tx: tokio::sync::broadcast::Sender<MirrorChange>,
    /// Set for the duration of the startup initial sync. While true,
    /// `bus_listener` buffers incoming `BusEvent`s instead of applying them,
    /// so a WS delta for an item a later sync page hasn't reached yet can't
    /// be clobbered once that page's stale snapshot lands. Replayed in
    /// order once initial sync completes.
    pub(crate) initial_sync_in_progress: std::sync::atomic::AtomicBool,
    /// Number of bulk library passes currently streaming pages into the
    /// writer: `sync::sync_library_breadth` walks and `sync::reconcile_sweep`
    /// id sweeps, neither of which `initial_sync_in_progress` alone covers.
    /// Folded into [`Mirror::is_syncing`]. Maintained by
    /// `sync::BreadthSyncGuard`, which decrements BEFORE the pass's terminal
    /// `SyncActivity::Idle` emission.
    pub(crate) breadth_syncs_in_flight: std::sync::atomic::AtomicUsize,
    /// Single-flight guard + deferred-rerun flag for `sync::reconcile_all`
    /// -- see its doc comment.
    pub(crate) reconcile_in_progress: std::sync::atomic::AtomicBool,
    pub(crate) reconcile_pending: std::sync::atomic::AtomicBool,
    /// Same single-flight + deferred-rerun pair as the two above, for
    /// `sync::delta_sync`. Needs its own rather than sharing
    /// `reconcile_in_progress`, since delta runs *before* reconcile at every
    /// trigger -- sharing one flag would make the first pass's delta
    /// swallow the reconcile that was supposed to follow it.
    pub(crate) delta_in_progress: std::sync::atomic::AtomicBool,
    pub(crate) delta_pending: std::sync::atomic::AtomicBool,
    pub(crate) user_data_in_progress: std::sync::atomic::AtomicBool,
    pub(crate) user_data_pending: std::sync::atomic::AtomicBool,
    /// Set by the app while a playback session is active. Breadth syncs
    /// pause between pages while this is set, so bulk metadata doesn't
    /// compete with the stream mpv is buffering. Sync resumes where it left
    /// off when playback stops; WS deltas and reconcile probes are
    /// unaffected.
    pub(crate) playback_active: std::sync::atomic::AtomicBool,
    /// Notified when `initial_sync_in_progress` flips back to `false`, so
    /// `bus_listener` can replay its buffer promptly even if the event bus
    /// goes quiet right after sync finishes. `Arc`'d separately so
    /// `bus_listener` can clone just this handle and drop its
    /// `Arc<MirrorState>` upgrade before awaiting it -- otherwise the
    /// `Notified` future would keep the whole `MirrorState` alive for as
    /// long as it's waiting on the next bus event.
    pub(crate) initial_sync_done: std::sync::Arc<tokio::sync::Notify>,
    /// Set (instead of replaying) when the WS delta buffer overflows its cap
    /// during initial sync; `bus_listener` consumes this once sync completes
    /// and runs one `reconcile_all` in place of a partial/lossy replay.
    pub(crate) reconcile_after_sync: std::sync::atomic::AtomicBool,
    /// Self-referential weak handle: lets any code holding only `&MirrorState`
    /// hand out a `Weak<MirrorState>` without threading an `Arc<MirrorState>`
    /// through every call site.
    pub(crate) self_weak: std::sync::Weak<MirrorState>,
    /// Sync activity observable -- see [`SyncActivity`]'s doc comment and
    /// [`Mirror::sync_activity`].
    pub(crate) sync_activity: tokio::sync::watch::Sender<SyncActivity>,
    /// Current Next Up filtering knobs -- see [`NextUpOptions`]'s doc
    /// comment. A plain `Mutex`, not an atomic: it's a two-field struct
    /// touched only on a rare settings change or once per
    /// `refresh_next_up` call, no hot-path reason for lock-free machinery.
    pub(crate) next_up_options: std::sync::Mutex<NextUpOptions>,
}

impl MirrorState {
    /// Shared constructor for `Mirror::open` and `sync`'s test-only
    /// `TestMirror::new`: everything below the five parameters starts out
    /// identical in both (freshly-opened, nothing in flight yet).
    pub(crate) fn new(
        client: jellyfin_api::JellyfinClient,
        writer: writer::WriterHandle,
        read_pool: std::sync::Arc<pool::ReadPool>,
        changes_tx: tokio::sync::broadcast::Sender<MirrorChange>,
        sync_activity: tokio::sync::watch::Sender<SyncActivity>,
        self_weak: std::sync::Weak<MirrorState>,
    ) -> Self {
        MirrorState {
            client,
            writer,
            read_pool,
            changes_tx,
            initial_sync_in_progress: std::sync::atomic::AtomicBool::new(false),
            breadth_syncs_in_flight: std::sync::atomic::AtomicUsize::new(0),
            reconcile_in_progress: std::sync::atomic::AtomicBool::new(false),
            reconcile_pending: std::sync::atomic::AtomicBool::new(false),
            delta_in_progress: std::sync::atomic::AtomicBool::new(false),
            delta_pending: std::sync::atomic::AtomicBool::new(false),
            user_data_in_progress: std::sync::atomic::AtomicBool::new(false),
            user_data_pending: std::sync::atomic::AtomicBool::new(false),
            playback_active: std::sync::atomic::AtomicBool::new(false),
            initial_sync_done: std::sync::Arc::new(tokio::sync::Notify::new()),
            reconcile_after_sync: std::sync::atomic::AtomicBool::new(false),
            self_weak,
            sync_activity,
            next_up_options: std::sync::Mutex::new(NextUpOptions::default()),
        }
    }
}

/// Sync activity observable for a UI status affordance. Broader than
/// [`Mirror::is_syncing`]'s initial-sync-only flag: also covers the
/// reconciliation-triggered id sweep and any breadth sync after startup.
/// Exactly two places update it -- `sync::sync_library_breadth` and
/// `sync::reconcile_sweep` -- both following the same "emit before the
/// request in flight, terminal `Idle` on every exit path" rule.
///
/// Updated via `watch::Sender::send_replace`, synchronous, non-blocking, and
/// infallible even with zero receivers, so the sync engine never `.await`s
/// on this and a UI that never subscribes can't stall or break a sync.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncActivity {
    /// No bulk library pass currently running.
    Idle,
    /// A bulk library pass (initial sync's breadth walk, or a reconciliation
    /// id sweep -- indistinguishable from here, deliberately: both mean
    /// "this library's rows are being reconverged against the server") is
    /// in progress.
    Syncing {
        /// The view/library's id, not a human-readable name: the sync
        /// engine doesn't otherwise look up a view's display name mid-sync,
        /// so resolving one is left to the caller (e.g. via
        /// [`Mirror::views`]).
        library_name_or_id: String,
        /// Pages already fetched for this library in the current pass (`0`
        /// while the first page is still in flight).
        pages_done: u32,
        /// Items already accounted for in the current pass. With
        /// `total_items` this makes the sidebar's sync affordance a real
        /// determinate progress bar. A sweep reports its enumeration cursor
        /// while enumerating, then restarts against the (much smaller)
        /// repair set -- see `sync::reconcile_sweep_inner`.
        items_done: u32,
        /// The denominator for `items_done`: the server's
        /// `TotalRecordCount` for this library during a breadth walk or a
        /// sweep's enumeration, and the size of the repair set during a
        /// sweep's by-ids phase. `None` only while the first page is still
        /// in flight (the total arrives with the first response).
        total_items: Option<u32>,
    },
}

/// Handle to one server's mirror. Clone-cheap. All queries are indexed —
/// EXPLAIN QUERY PLAN asserted in tests (no full scans on browse paths, docs/DATA.md §1).
#[derive(Clone)]
pub struct Mirror {
    inner: std::sync::Arc<MirrorState>,
}

/// Rows the UI binds to: extracted columns only — the DTO blob is NOT parsed on
/// the browse path.
#[derive(Debug, Clone)]
pub struct CardRow {
    pub id: String,
    pub item_type: String,
    pub name: String,
    pub primary_tag: Option<String>,
    pub blurhash: Option<String>,
    pub played: bool,
    pub position_ticks: i64,
    pub runtime_ticks: Option<i64>,
    pub unplayed_count: Option<i64>,
    pub production_year: Option<i32>,
    /// Season/Episode's own number (`IndexNumber`), for `episode_card`'s
    /// `"{n}. {name}"` title.
    pub index_number: Option<i32>,
    /// `PremiereDate` (RFC3339), used by a virtual (unaired/missing)
    /// episode's card to show "Airs <date>" without a live/blob fetch.
    /// `None` when the server has no premiere date for this item.
    pub premiere_date: Option<String>,
    /// The season's own number, for an Episode row.
    pub parent_index_number: Option<i32>,
    /// The owning series' id, for a Season/Episode row -- `SeriesId` on
    /// `BaseItemDto`. Used for the poster-fallback chain
    /// (`series_primary_tag`, below) and episode-context navigation.
    pub series_id: Option<String>,
    /// `SeriesPrimaryImageTag` -- the series' own poster tag, carried on
    /// every Season/Episode row so a poster-shaped slot can fall back to
    /// the series' poster instead of cropping a 16:9 still. `None` for a
    /// Movie/Series/BoxSet row.
    pub series_primary_tag: Option<String>,
    /// `ParentBackdropItemId`/`ParentBackdropImageTags[0]` -- the nearest
    /// ancestor (season, else series) that actually has a backdrop, for a
    /// Season/Episode row lacking its own.
    pub parent_backdrop_item_id: Option<String>,
    pub parent_backdrop_tag: Option<String>,
    /// `SeriesName`, the owning series' display name, for a Season/Episode
    /// row. Schema v9; `None` for a row synced before the v9 resync
    /// backfilled it. Distinct from `series_primary_tag`/`series_id`
    /// (artwork/navigation only) -- prints on a Continue Watching/Next Up
    /// card's second line without an extra live-DTO fetch.
    pub series_name: Option<String>,
    /// Server-authoritative `UserData.LastPlayedDate` (RFC3339) -- what
    /// `resume()` orders by, see `schema.rs`'s comment on the backing
    /// column.
    pub last_played_date: Option<String>,
    /// The item's overview text on the browse path, for the episode grid's
    /// 2-line synopsis -- previously only available via a live
    /// `MediaStreams` fetch or blob parse. Schema v7; `None` for a row
    /// synced before the v7 resync backfilled it, or with no overview text.
    pub overview: Option<String>,
    /// `BaseItemDto.LocationType == "Virtual"` -- a future (unaired) or
    /// missing episode the server lists as a placeholder with no
    /// `MediaSources` to play. `cards.rs`'s `episode_card` dims the artwork
    /// and swaps the runtime line for "Airs <date>"/"Missing"; `detail.rs`
    /// disables Play/Resume the same way; `root.rs::play_item` refuses to
    /// start playback as a defensive backstop.
    pub is_virtual: bool,
    /// Per-library home visibility (app Settings): the owning library's
    /// `views.id`, the same value `latest()`'s `view_id` scopes by. `None`
    /// for a row whose library couldn't be attributed yet. `home.rs` uses
    /// this to drop a Continue Watching/Next Up card whose library the user
    /// has hidden from Home.
    pub library_id: Option<String>,
}

#[derive(Debug, Clone)]
pub enum MirrorChange {
    Upserted(Vec<String>),
    Removed(Vec<String>),
    ViewsChanged,
    /// Synthetic signal (never sent by the writer directly) meaning "you
    /// missed some commits, throw away incremental state and re-query."
    /// Produced by [`recv_changes`] when the broadcast receiver reports
    /// `Lagged`.
    Refresh,
}

impl Mirror {
    /// Open (or drop-and-recreate on version mismatch). Spawns the writer task
    /// and the sync engine (initial sync if empty; subscribes to the EventBus for
    /// live LibraryChanged/UserDataChanged deltas + NeedsReconcile).
    ///
    /// `dir` is a directory, not a filename: per-server scoping is assumed
    /// already baked into it by the caller (one directory per server); this
    /// just creates `mirror.db` inside it.
    pub async fn open(
        dir: std::path::PathBuf,
        client: jellyfin_api::JellyfinClient,
        bus: tokio::sync::broadcast::Receiver<jellyfin_core::BusEvent>,
    ) -> Result<Self, CacheError> {
        tokio::fs::create_dir_all(&dir)
            .await
            .map_err(|e| CacheError::Db(e.to_string()))?;
        let db_path = dir.join("mirror.db");

        let open_path = db_path.clone();
        let (conn, is_empty) =
            tokio::task::spawn_blocking(move || schema::open_and_prepare(&open_path))
                .await
                .map_err(|e| CacheError::Db(e.to_string()))??;

        let pool_path = db_path.clone();
        let read_pool = tokio::task::spawn_blocking(move || {
            pool::ReadPool::open(&pool_path, READ_POOL_SIZE).map(std::sync::Arc::new)
        })
        .await
        .map_err(|e| CacheError::Db(e.to_string()))??;

        let (write_tx, write_rx) = tokio::sync::mpsc::channel(256);
        let (changes_tx, _) = tokio::sync::broadcast::channel(256);
        let writer_changes_tx = changes_tx.clone();
        tokio::task::spawn_blocking(move || writer::run(conn, write_rx, writer_changes_tx));
        let (sync_activity_tx, _) = tokio::sync::watch::channel(SyncActivity::Idle);

        let state = std::sync::Arc::new_cyclic(|weak| {
            MirrorState::new(
                client,
                writer::WriterHandle::new(write_tx),
                read_pool,
                changes_tx,
                sync_activity_tx,
                weak.clone(),
            )
        });

        sync::spawn(state.clone(), bus, is_empty);

        Ok(Mirror { inner: state })
    }

    /// Change feed for live UI updates (commit-ordered). Consumers MUST read
    /// from this via [`recv_changes`], not `.recv()` directly -- see its doc
    /// comment for why a raw `Lagged` can't just be ignored.
    pub fn changes(&self) -> tokio::sync::broadcast::Receiver<MirrorChange> {
        self.inner.changes_tx.subscribe()
    }

    /// True while bulk writes are streaming into the mirror -- startup
    /// initial sync (or a schema-rebuild resync, same path), any
    /// `sync_library_breadth` walk, and `sync::reconcile_sweep`'s id-level
    /// repair. Drives Home's shelf order-freeze
    /// (`home.rs::HomeState::refresh`) and the active library's refresh
    /// gating (`root.rs::on_mirror_change`) so the UI rides out the churn
    /// instead of reordering tiles mid-write.
    pub fn is_syncing(&self) -> bool {
        self.inner
            .initial_sync_in_progress
            .load(std::sync::atomic::Ordering::Acquire)
            || self
                .inner
                .breadth_syncs_in_flight
                .load(std::sync::atomic::Ordering::Acquire)
                > 0
    }

    /// Sync activity observable -- see [`SyncActivity`]'s doc comment. Every
    /// call (and every `Mirror` clone) shares the same underlying `watch`
    /// channel, so subscribing repeatedly is cheap and never misses an
    /// update between calls to `borrow`/`changed`.
    pub fn sync_activity(&self) -> tokio::sync::watch::Receiver<SyncActivity> {
        self.inner.sync_activity.subscribe()
    }

    /// App-reported playback state: while `true`, breadth syncs pause
    /// between pages so bulk metadata never competes with the stream mpv is
    /// buffering (see `MirrorState::playback_active`). Idempotent; the app
    /// calls it on every playback start/stop transition.
    pub fn set_playback_active(&self, active: bool) {
        self.inner
            .playback_active
            .store(active, std::sync::atomic::Ordering::Release);
    }

    /// Pushes the app's current Next Up filtering preferences down into the
    /// sync engine -- see [`NextUpOptions`]'s doc comment for why this is a
    /// push. Does not itself trigger a refresh -- the next opportunistic
    /// `sync::refresh_next_up` call picks up the new value; a caller that
    /// wants it to take effect immediately should also poke a refresh
    /// (`app`'s settings setter does, via `Root::refresh_next_up_now`).
    pub fn set_next_up_options(&self, options: NextUpOptions) {
        *self
            .inner
            .next_up_options
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = options;
    }

    /// Total item count, for the sidebar's "Syncing library — N items"
    /// progress text. A plain `COUNT(*)` -- cheap and only polled at UI
    /// refresh cadence, not per-item.
    pub fn item_count(&self) -> i64 {
        query::item_count(&self.inner.read_pool.acquire())
    }

    // Browse queries (all served from indexes, sync-fast, called from UI thread pool)
    pub fn views(&self) -> Vec<ViewSummary> {
        query::views(&self.inner.read_pool.acquire())
    }

    /// If `parent_id` names a `BoxSet`, this resolves through
    /// `collection_members` (server curation order) instead of plain
    /// `parent_id` equality -- see `query::children`.
    pub fn children(&self, parent_id: &str, sort: Sort, offset: u32, limit: u32) -> Vec<CardRow> {
        query::children(
            &self.inner.read_pool.acquire(),
            parent_id,
            sort,
            offset,
            limit,
        )
    }

    /// [`Mirror::children`] with query failure surfaced as `None` (logged
    /// here) instead of coerced to an empty list. The library refresh path
    /// uses this so a transient SQLite error keeps the last good grid
    /// rather than blanking a populated poster wall; `Some(vec![])` still
    /// means a genuinely empty parent.
    pub fn children_checked(
        &self,
        parent_id: &str,
        sort: Sort,
        offset: u32,
        limit: u32,
    ) -> Option<Vec<CardRow>> {
        query::children_checked(
            &self.inner.read_pool.acquire(),
            parent_id,
            sort,
            offset,
            limit,
        )
        .map_err(|e| tracing::error!(error = %e, "children query failed"))
        .ok()
    }

    pub fn resume(&self, limit: u32) -> Vec<CardRow> {
        query::resume(&self.inner.read_pool.acquire(), limit)
    }

    pub fn next_up(&self, limit: u32) -> Vec<CardRow> {
        query::next_up(&self.inner.read_pool.acquire(), limit)
    }

    /// Re-fetches `/Shows/NextUp` against whatever [`NextUpOptions`] are
    /// currently in effect, right now, rather than waiting for the next
    /// opportunistic trigger. The app's Settings sheet calls this
    /// immediately after changing the cutoff/rewatching knobs.
    pub async fn refresh_next_up(&self) {
        sync::refresh_next_up(&self.inner).await;
    }

    /// `hide_watched`: app Settings "Hide watched from Latest" -- when
    /// `true`, excludes items already marked played from the result (see
    /// `query::latest`'s doc comment). Does not affect
    /// [`Self::resume`]/[`Self::next_up`] -- both are inherently
    /// unwatched/in-progress already.
    pub fn latest(&self, view_id: &str, limit: u32, hide_watched: bool) -> Vec<CardRow> {
        query::latest(
            &self.inner.read_pool.acquire(),
            view_id,
            limit,
            hide_watched,
        )
    }

    pub fn search(&self, query_str: &str, limit: u32) -> Vec<CardRow> {
        query::search(&self.inner.read_pool.acquire(), query_str, limit)
    }

    /// Full DTO for Detail view (blob parse allowed here).
    pub fn item(&self, id: &str) -> Option<BaseItemDto> {
        query::item(&self.inner.read_pool.acquire(), id)
    }

    /// Batched detail metadata for a browse result. This keeps optional
    /// library decorations (genres, compact media facts, and a backdrop)
    /// out of the render path while avoiding one read-pool checkout and SQL
    /// statement per card.
    pub fn items(&self, ids: &[String]) -> std::collections::HashMap<String, BaseItemDto> {
        query::items(&self.inner.read_pool.acquire(), ids)
    }

    /// Persist one item's live per-visit enrichment fetch (`MediaStreams`,
    /// `MediaSources`, `Chapters`, `Trickplay`, `People`, `Overview`,
    /// `Genres` -- fields docs/DATA.md's browse-focused bulk sync never
    /// requests), so the *next* visit to the item paints fully from disk
    /// immediately instead of the spec strip's pills appearing 1-5s later.
    ///
    /// Routed through the single writer (docs/DATA.md §2's ordering rule) and
    /// folded into the existing row -- see `writer::apply_upsert_enriched_item`
    /// for the invariants (no rescoping, unknown items skipped, idempotent).
    pub async fn upsert_enriched_item(&self, dto: BaseItemDto) {
        self.inner.writer.upsert_enriched_item(dto).await;
    }

    /// Optimistically apply a watch-state update *this client* just
    /// reported to the server (progress tick / stop / EOF), without waiting
    /// for the `UserDataChanged` WS event: Jellyfin doesn't reliably push
    /// that back to its own originating session, which would otherwise
    /// leave Resume/Continue Watching stale until a full resync.
    ///
    /// `played`:
    /// - `None` -- an ordinary progress tick or a stop before the
    ///   played-threshold: position is written as reported, and the played
    ///   flag only flips past ~90% runtime (see
    ///   `writer::resolve_played_position`).
    /// - `Some(true)` -- a real EOF: always marks played (position reset to
    ///   0).
    ///
    /// Routed through the single writer (docs/DATA.md §2's ordering rule) and
    /// emits `MirrorChange::Upserted` on success.
    pub async fn apply_local_user_data(
        &self,
        item_id: &str,
        position_ticks: i64,
        played: Option<bool>,
    ) {
        self.inner
            .writer
            .apply_local_user_data(item_id.to_string(), position_ticks, played)
            .await;
    }

    /// [`Self::apply_local_user_data`], but the returned future only
    /// resolves once the write is actually *committed*, not merely
    /// enqueued -- the app-quit path awaits this inside GPUI's ~100ms
    /// shutdown grace window and then dies, so "in the channel" is not the
    /// same as "survived the quit". Costs one extra round trip through the
    /// writer's strict-FIFO queue (`WriterHandle::barrier`).
    pub async fn apply_local_user_data_and_wait(
        &self,
        item_id: &str,
        position_ticks: i64,
        played: Option<bool>,
    ) {
        self.apply_local_user_data(item_id, position_ticks, played)
            .await;
        self.inner.writer.barrier().await;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sort {
    NameAsc,
    DateCreatedDesc,
    PremiereDateDesc,
    /// `ORDER BY parent_index_number, index_number` -- serves both Series ->
    /// Seasons and Season -> Episodes (a season's own `index_number` is its
    /// season number; an episode's `parent_index_number` is its season
    /// number, `index_number` its episode number). Backed by
    /// `idx_items_parent_order` (schema.rs) so `children()` stays
    /// index-served.
    IndexNumber,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ImageKind {
    Primary,
    Backdrop,
    Thumb,
    Trickplay,
}

#[derive(Debug, thiserror::Error)]
pub enum CacheError {
    #[error("db: {0}")]
    Db(String),
    #[error(transparent)]
    Api(#[from] jellyfin_api::ApiError),
    #[error("cancelled")]
    Cancelled,
    /// `item_id`/`tag` are used to build on-disk cache paths; a value
    /// containing `/` or `..` is rejected at the `get()` boundary rather
    /// than allowed anywhere near a `Path::join`.
    #[error("invalid image cache key component: {0:?}")]
    InvalidKey(String),
}

/// How long [`recv_changes`] coalesces a burst of rapid-fire
/// [`MirrorChange`]s into a single returned signal. Initial sync streams
/// items in as many small `Upserted` batches, each of which used to trigger
/// its own full `on_mirror_change` re-query -- items visibly
/// reordering/reshuffling every few dozen milliseconds while the mirror
/// backfills. 250ms keeps live single-event latency imperceptible while
/// collapsing a sync burst into one UI refresh.
const CHANGE_DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(250);

/// Reads the next change from a [`Mirror::changes`] receiver, collapsing a
/// `Lagged` gap into a single synthetic [`MirrorChange::Refresh`] instead of
/// surfacing the raw broadcast error. UI consumers MUST call this instead of
/// `rx.recv()` directly: a raw `Lagged` means the receiver missed commits,
/// so `Refresh` tells it to re-query instead of silently drifting from the
/// mirror. Returns `None` once the channel is closed.
///
/// Also debounces (see [`CHANGE_DEBOUNCE`]): after the first change arrives,
/// drains and discards any further changes within the debounce window,
/// bounding a caller's re-query rate to at most once per `CHANGE_DEBOUNCE`.
/// Every caller treats the returned value as a pure "something changed,
/// re-query" wakeup and ignores its payload, so coalescing distinct
/// variants loses no information any consumer reads.
pub async fn recv_changes(
    rx: &mut tokio::sync::broadcast::Receiver<MirrorChange>,
) -> Option<MirrorChange> {
    let first = match rx.recv().await {
        Ok(change) => change,
        Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
            tracing::warn!(
                skipped,
                "MirrorChange receiver lagged; issuing a Refresh signal"
            );
            MirrorChange::Refresh
        }
        Err(tokio::sync::broadcast::error::RecvError::Closed) => return None,
    };

    // Drain (and discard) anything else that arrives before the debounce
    // window elapses -- a fixed window from the first event, not a
    // reset-on-every-event quiet timer, so a sustained stream of changes
    // (e.g. a very large library's initial sync) still surfaces a refresh
    // at a bounded ~4Hz rather than being starved indefinitely.
    let deadline = tokio::time::Instant::now() + CHANGE_DEBOUNCE;
    loop {
        tokio::select! {
            biased;
            _ = tokio::time::sleep_until(deadline) => break,
            res = rx.recv() => match res {
                Ok(_) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                    tracing::warn!(
                        skipped,
                        "MirrorChange receiver lagged while debouncing; issuing a Refresh signal"
                    );
                    return Some(MirrorChange::Refresh);
                }
                // Channel closed mid-drain: still hand back the change we
                // already have. The caller's next `recv_changes` call will
                // see the close and return `None` to end its loop.
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            },
        }
    }

    Some(first)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pins: a bump to `SCHEMA_VERSION` must be deliberate.
    #[test]
    fn schema_version_is_stable_constant() {
        assert_eq!(SCHEMA_VERSION, 11);
    }

    // `start_paused = true`: lets tests resolve instantly instead of
    // burning 250ms of real wall time each on `CHANGE_DEBOUNCE`.
    #[tokio::test(start_paused = true)]
    async fn recv_changes_passes_through_ok_values() {
        let (tx, mut rx) = tokio::sync::broadcast::channel(4);
        tx.send(MirrorChange::ViewsChanged).expect("send");
        let change = recv_changes(&mut rx).await;
        assert!(matches!(change, Some(MirrorChange::ViewsChanged)));
    }

    #[tokio::test(start_paused = true)]
    async fn recv_changes_maps_lagged_to_refresh() {
        let (tx, mut rx) = tokio::sync::broadcast::channel(2);
        tx.send(MirrorChange::ViewsChanged).expect("send");
        tx.send(MirrorChange::ViewsChanged).expect("send");
        tx.send(MirrorChange::ViewsChanged).expect("send");
        let change = recv_changes(&mut rx).await;
        assert!(matches!(change, Some(MirrorChange::Refresh)));
    }

    #[tokio::test(start_paused = true)]
    async fn recv_changes_returns_none_when_closed() {
        let (tx, mut rx) = tokio::sync::broadcast::channel::<MirrorChange>(4);
        drop(tx);
        assert!(recv_changes(&mut rx).await.is_none());
    }

    /// Pins: a burst of rapid `Upserted` events collapses into exactly one returned signal per `CHANGE_DEBOUNCE` window.
    #[tokio::test(start_paused = true)]
    async fn recv_changes_coalesces_a_burst_into_one_signal() {
        let (tx, mut rx) = tokio::sync::broadcast::channel(16);
        for i in 0..5 {
            tx.send(MirrorChange::Upserted(vec![format!("item-{i}")]))
                .expect("send");
        }

        let change = recv_changes(&mut rx).await;
        assert!(matches!(change, Some(MirrorChange::Upserted(_))));
        assert_eq!(
            rx.len(),
            0,
            "all 5 sends must be drained into the single returned signal, \
             not left queued for one-by-one delivery"
        );
    }

    /// Pins: a quiet channel after a debounced burst still delivers a later, separate change.
    #[tokio::test(start_paused = true)]
    async fn recv_changes_still_delivers_a_later_event_after_debouncing() {
        let (tx, mut rx) = tokio::sync::broadcast::channel(16);
        tx.send(MirrorChange::ViewsChanged).expect("send");
        let first = recv_changes(&mut rx).await;
        assert!(matches!(first, Some(MirrorChange::ViewsChanged)));

        tx.send(MirrorChange::Removed(vec!["gone".to_string()]))
            .expect("send");
        let second = recv_changes(&mut rx).await;
        assert!(matches!(second, Some(MirrorChange::Removed(_))));
    }
}
