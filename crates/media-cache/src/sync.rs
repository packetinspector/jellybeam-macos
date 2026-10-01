//! Sync engine per docs/DATA.md §2: paged initial sync (Resume/NextUp/Latest
//! first, then breadth), WebSocket deltas, and reconciliation. Every mutation
//! flows through `WriterHandle` (the single writer), so ordering is exactly
//! the order these `async fn`s issue writes in.

use std::sync::atomic::Ordering;
use std::sync::{Arc, Weak};

use jellyfin_api::models::{BaseItemDto, BaseItemKind};
use jellyfin_api::{ItemQuery, ServerEvent};
use jellyfin_core::BusEvent;
use tokio::sync::broadcast;

use crate::{MirrorState, SyncActivity};

const PAGE_SIZE: u32 = 500;
const WS_BATCH_SIZE: usize = 100;
/// Page size for [`reconcile_sweep`]'s id enumeration. Larger than
/// [`PAGE_SIZE`] since a sweep page carries no `fields`/images/user data
/// (see [`id_sweep_query`]), so fewer, fatter pages cost little bandwidth.
const ID_SWEEP_PAGE_SIZE: u32 = 1000;
/// Backstop for whatever the WS `LibraryChanged`/`NeedsReconcile` paths miss.
/// 5 minutes rather than 30: each tick is one cheap `TotalRecordCount` probe
/// per library, not a full resync, so tightening it doesn't multiply cost.
const RECONCILE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5 * 60);
/// How often a paused breadth walk re-checks `playback_active`. Cheap (one
/// atomic load per poll), so it can be short -- sync resumes promptly after
/// playback ends instead of idling seconds longer than it needs to.
const PLAYBACK_YIELD_POLL: std::time::Duration = std::time::Duration::from_millis(250);
/// Cap on WS deltas `bus_listener` buffers while initial sync is in
/// progress. Past this, a full reconcile is cheaper and safer than replaying
/// a huge, possibly-stale backlog.
const WS_DELTA_BUFFER_CAP: usize = 10_000;
/// Mirror `meta` key holding the incremental delta cursor -- see
/// [`delta_sync`]. An RFC 3339 / ISO 8601 UTC timestamp string, the
/// shape `minDateLastSaved` wants, deliberately unlike the debugging-only
/// Unix-millis `last_full_sync` (`now_iso`) it sits next to.
const LAST_DELTA_SYNC_KEY: &str = "last_delta_sync";
/// How far back of the stored cursor each delta query actually reaches.
///
/// The cursor is a CLIENT clock reading compared against SERVER clock
/// readings in the server's SQL, so this absorbs client-ahead-of-server
/// skew; re-fetching the overlap is free since every write here is an
/// idempotent upsert keyed on item id.
const DELTA_OVERLAP_SLACK: chrono::TimeDelta = chrono::TimeDelta::minutes(5);

/// Fields not returned by default that the mirror's columns/search index
/// need. Unlike `SeriesId`/`SeasonId`, the server only returns `ParentId`
/// when it's explicitly requested here -- see `rows::browse_parent_id` for
/// how the mirror maps it onto the browse-time `parent_id` column.
fn item_fields() -> Vec<String> {
    [
        "Overview",
        "OriginalTitle",
        "SeriesName",
        "DateCreated",
        "PremiereDate",
        "ImageBlurHashes",
        "ParentId",
        // docs/DESIGN-PLAYER-NAV.md §2.5: artwork fallback needs a
        // Season/Episode's series poster tag, and unlike the Parent*
        // backdrop fields, this one must be requested explicitly.
        "SeriesPrimaryImageTag",
    ]
    .into_iter()
    .map(String::from)
    .collect()
}

/// The "same universe" `ItemQuery` shape shared by `sync_library_breadth`,
/// `reconcile_view`'s count probe, and `id_sweep_query`: `parent_id` +
/// `recursive`, no `include_item_types`, no `is_missing` -- this
/// unrestricted, un-type-filtered recursive query is what defines "the sync
/// universe" for a library, and all three call sites match this exact shape
/// so they stay provably in sync. Each caller layers its own paging/fields
/// extras on top; only what defines the universe lives here.
fn library_universe_query(view_id: &str) -> ItemQuery {
    ItemQuery {
        parent_id: Some(view_id.to_string()),
        recursive: true,
        is_missing: None,
        min_date_last_saved: None,
        ..ItemQuery::new()
    }
}

/// The item type that should appear as a *direct child* of a view when
/// browsing into it (`Mirror::children(view_id, ..)`).
///
/// Jellyfin's own `/Items?ParentId=<view>` flattens through intermediate
/// physical-folder nodes server-side; the mirror's `parent_id` equality
/// query (docs/DATA.md §1) has no such flattening, so for this one item type per
/// view the sync engine forces `parent_id` to the view's id at write time.
/// Genuinely nested content (Season under Series, Episode under Season) is
/// untouched.
fn browse_root_item_type(collection_type: &str) -> Option<BaseItemKind> {
    match collection_type {
        "movies" => Some(BaseItemKind::Movie),
        "tvshows" => Some(BaseItemKind::Series),
        "boxsets" => Some(BaseItemKind::BoxSet),
        "music" => Some(BaseItemKind::MusicAlbum),
        "musicvideos" => Some(BaseItemKind::MusicVideo),
        "homevideos" => Some(BaseItemKind::Video),
        _ => None,
    }
}

/// Forces `parent_id` to `view_uuid` on every item in `items` whose type is
/// `root_type` -- see `browse_root_item_type`'s browse-flattening rule. A
/// `None` `root_type`/`view_uuid` is a no-op. Shared by the breadth walk,
/// cross-library delta upserts, and the reconcile sweep's missing-id fetch
/// so the three can't drift apart.
fn flatten_root_parents_in_place(
    items: &mut [BaseItemDto],
    root_type: Option<BaseItemKind>,
    view_uuid: Option<uuid::Uuid>,
) {
    let (Some(root_type), Some(view_uuid)) = (root_type, view_uuid) else {
        return;
    };
    for item in items {
        if item.type_ == Some(root_type) {
            item.parent_id = Some(view_uuid);
        }
    }
}

/// Spawn the sync engine: the startup pass (initial sync if the mirror was
/// empty, otherwise a light refresh + reconcile), the WebSocket delta
/// listener, and the idle reconciliation timer. All three hold only a
/// `Weak`, so a `Mirror` dropped right after `open()` isn't kept alive to
/// force a full initial sync to completion.
pub(crate) fn spawn(
    state: Arc<MirrorState>,
    bus: broadcast::Receiver<BusEvent>,
    needs_initial_sync: bool,
) {
    if needs_initial_sync {
        // Set synchronously, before `bus_listener` is spawned -- so there is
        // no window where a WS delta could reach `bus_listener` and be
        // applied directly before the startup task below flips this flag.
        state
            .initial_sync_in_progress
            .store(true, Ordering::Release);
    }

    let startup_weak = Arc::downgrade(&state);
    tokio::spawn(async move {
        if needs_initial_sync {
            initial_sync(&startup_weak).await;
        } else {
            let Some(s) = startup_weak.upgrade() else {
                return;
            };
            sync_views(&s).await;
            drop(s);

            let Some(s) = startup_weak.upgrade() else {
                return;
            };
            refresh_next_up(&s).await;
            // One-time-per-launch heal for rows an older delta-sync path
            // left under their physical folder id (see
            // `WriteCmd::FlattenRootParents`); idempotent, index-served.
            for (view_id, collection_type) in current_views(&s).await {
                if let Some(root_type) = browse_root_item_type(&collection_type) {
                    s.writer
                        .flatten_root_parents(view_id, root_type.to_string())
                        .await;
                }
            }
            drop(s);

            // Delta BEFORE reconcile, at every trigger: delta is the fast
            // path -- one small query that makes new and updated items
            // visible in seconds -- and reconcile is the backstop for what
            // delta structurally cannot see (deletions). Running delta
            // first also means reconcile's count probe usually finds the
            // library already in agreement and skips the breadth resync.
            let Some(s) = startup_weak.upgrade() else {
                return;
            };
            delta_sync(&s).await;
            drop(s);

            let Some(s) = startup_weak.upgrade() else {
                return;
            };
            user_data_sync(&s).await;
            drop(s);

            let Some(s) = startup_weak.upgrade() else {
                return;
            };
            reconcile_all(&s).await;
        }
    });

    tokio::spawn(bus_listener(Arc::downgrade(&state), bus));
    tokio::spawn(reconcile_timer(Arc::downgrade(&state)));
}

/// RAII accounting for [`MirrorState::breadth_syncs_in_flight`]. `enter`
/// increments (failing if the mirror is already gone); `Drop` decrements,
/// covering every exit path without each one having to remember to. The
/// terminal-`Idle` sites drop it explicitly first so the decrement is
/// ordered before the emission.
struct BreadthSyncGuard(Weak<MirrorState>);

impl BreadthSyncGuard {
    fn enter(state: &Weak<MirrorState>) -> Option<Self> {
        let s = state.upgrade()?;
        s.breadth_syncs_in_flight.fetch_add(1, Ordering::AcqRel);
        Some(Self(Weak::clone(state)))
    }
}

impl Drop for BreadthSyncGuard {
    fn drop(&mut self) {
        if let Some(s) = self.0.upgrade() {
            s.breadth_syncs_in_flight.fetch_sub(1, Ordering::AcqRel);
        }
    }
}

pub(crate) async fn initial_sync(state: &Weak<MirrorState>) {
    // Captured before the first fetch, stamped as the delta cursor at the
    // end: a full sync can take minutes, and anything the server saves
    // during that window may land in a page this walk already passed.
    // Stamping the *start* instant means the first delta pass re-covers
    // that whole window.
    let started_at = now_rfc3339();

    let Some(s) = state.upgrade() else { return };
    s.initial_sync_in_progress.store(true, Ordering::Release);
    sync_views(&s).await;
    drop(s);

    let Some(s) = state.upgrade() else { return };
    sync_resume(&s).await;
    drop(s);

    let Some(s) = state.upgrade() else { return };
    refresh_next_up(&s).await;
    drop(s);

    let views = {
        let Some(s) = state.upgrade() else { return };
        current_views(&s).await
    };

    for (view_id, collection_type) in views {
        if !sync_library_breadth(state, &view_id, &collection_type).await {
            // Mirror dropped mid-breadth-sync; abandon without ever clearing
            // `initial_sync_in_progress` -- nobody's listening for it.
            return;
        }
        if collection_type == "boxsets" {
            let Some(s) = state.upgrade() else { return };
            sync_boxsets_membership(&s, &view_id).await;
            drop(s);
        }
    }

    let Some(s) = state.upgrade() else { return };
    s.writer.set_meta("last_full_sync", now_iso()).await;
    s.writer.set_meta(LAST_DELTA_SYNC_KEY, started_at).await;
    s.initial_sync_in_progress.store(false, Ordering::Release);
    s.initial_sync_done.notify_waiters();
    // Each per-library breadth walk emitted its own terminal Idle while
    // `initial_sync_in_progress` was still set, so `!is_syncing()` gating
    // ignored them. Re-emit Idle now that the flag is cleared, since a
    // receiver may have coalesced every earlier Idle/Syncing pair away.
    s.sync_activity.send_replace(SyncActivity::Idle);
}

async fn sync_views(state: &MirrorState) {
    match state.client.get_user_views().await {
        Ok(items) => {
            let rows: Vec<crate::ViewRow> = items
                .into_iter()
                .filter_map(|v| {
                    Some(crate::ViewRow {
                        id: v.id?.to_string(),
                        name: v.name.unwrap_or_default(),
                        collection_type: v
                            .collection_type
                            .map(|c| c.to_string())
                            .unwrap_or_default(),
                        // docs/PLUGIN-CHANNELS.md §2.1:
                        // the /UserViews entry's own `Type` (e.g. "Channel",
                        // "CollectionFolder"), distinct from `collection_type`
                        // above -- a Channel view never sets CollectionType.
                        item_type: v.type_.map(|t| t.to_string()).unwrap_or_default(),
                    })
                })
                .collect();
            state.writer.upsert_views(rows).await;
        }
        Err(e) => tracing::error!(error = %e, "failed to fetch user views"),
    }
}

async fn sync_resume(state: &MirrorState) {
    match state.client.get_resume_items().await {
        Ok(result) => state.writer.upsert_items(result.items).await,
        Err(e) => tracing::error!(error = %e, "failed to fetch resume items"),
    }
}

/// Converts [`crate::NextUpOptions::cutoff_days`] into the RFC3339 UTC
/// instant `nextUpDateCutoff` expects: "now minus `days` days". Pure (no
/// clock dependency beyond `Utc::now()`) so the day-arithmetic itself is
/// unit-testable without a live clock.
fn next_up_date_cutoff(days: u32) -> String {
    (chrono::Utc::now() - chrono::Duration::days(i64::from(days)))
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

pub(crate) async fn refresh_next_up(state: &MirrorState) {
    let options = *state
        .next_up_options
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let api_options = jellyfin_api::NextUpOptions {
        date_cutoff: options.cutoff_days.map(next_up_date_cutoff),
        enable_rewatching: options.rewatching,
    };
    // Full `item_fields()`, same as every other fetch this module persists:
    // a field-less NextUp response omits `Overview` etc, which upserting it
    // whole would then clobber onto the mirror's row.
    match state.client.get_next_up(&item_fields(), &api_options).await {
        Ok(result) => {
            let ids: Vec<String> = result
                .items
                .iter()
                .filter_map(|i| i.id.map(|u| u.to_string()))
                .collect();
            state.writer.upsert_items(result.items).await;
            let json = serde_json::to_string(&ids).unwrap_or_else(|_| "[]".to_string());
            state.writer.set_meta("next_up_ids", json).await;
        }
        Err(e) => tracing::error!(error = %e, "failed to fetch next up"),
    }
}

/// Full recursive paged sync of one library (view), page size 500, bulk
/// upsert per page so the change-feed emits incrementally rather than only
/// after the whole library lands. Takes a `Weak` and re-upgrades once per
/// page, returning `false` if the mirror was dropped or a fetch failed so
/// the caller stops driving further views too.
///
/// Convergence: every id this walk returns accumulates into `seen_ids`, and
/// on normal completion that set becomes the library's new authoritative
/// membership via `writer::apply_prune_library` -- anything else stamped
/// with this `library_id` gets pruned. An early `return false` skips that
/// call, since `seen_ids` would be an incomplete snapshot.
async fn sync_library_breadth(
    state: &Weak<MirrorState>,
    view_id: &str,
    collection_type: &str,
) -> bool {
    let Some(guard) = BreadthSyncGuard::enter(state) else {
        tracing::debug!(
            view_id,
            "mirror dropped before breadth sync began; abandoning"
        );
        return false;
    };
    let root_type = browse_root_item_type(collection_type);
    let view_uuid = uuid::Uuid::parse_str(view_id).ok();

    let mut start_index = 0u32;
    let mut seen_ids: Vec<String> = Vec::new();
    let mut pages_done = 0u32;
    // `TotalRecordCount` from the first page's response; the sidebar's
    // progress bar denominator (None only before that).
    let mut total_items: Option<u32> = None;
    loop {
        let Some(s) = state.upgrade() else {
            tracing::debug!(view_id, "mirror dropped mid-breadth-sync; abandoning");
            return false;
        };
        // Yield to playback (see `MirrorState::playback_active`): hold this
        // walk between pages while a stream is active rather than competing
        // with it for the link.
        if s.playback_active.load(Ordering::Acquire) {
            drop(s);
            tokio::time::sleep(PLAYBACK_YIELD_POLL).await;
            continue;
        }

        // Both `initial_sync` and `reconcile_view`'s triggered resync go
        // through this function, so reporting here covers both. Sent
        // *before* the page fetch so a UI polling mid-fetch sees `Syncing`
        // for the page in flight, not the last-completed one.
        s.sync_activity.send_replace(SyncActivity::Syncing {
            library_name_or_id: view_id.to_string(),
            pages_done,
            items_done: start_index,
            total_items,
        });

        // Same "sync universe" shape as `reconcile_view`'s count probe and
        // `id_sweep_query`'s enumeration -- see `library_universe_query`.
        let query = ItemQuery {
            sort_by: Some("SortName".to_string()),
            fields: item_fields(),
            start_index,
            limit: PAGE_SIZE,
            ..library_universe_query(view_id)
        };
        let result = match s.client.get_items(&query).await {
            Ok(r) => r,
            Err(e) => {
                tracing::error!(error = %e, view_id, start_index, "library page fetch failed");
                // Decrement-before-Idle: an observer reacting to this Idle
                // must already see `is_syncing() == false`.
                drop(guard);
                s.sync_activity.send_replace(SyncActivity::Idle);
                return false;
            }
        };
        let page_len = result.items.len() as u32;
        let total = result
            .total_record_count
            .map(|n| n.max(0) as u32)
            .unwrap_or(page_len);
        total_items = Some(total);

        let mut items = result.items;
        flatten_root_parents_in_place(&mut items, root_type, view_uuid);
        seen_ids.extend(
            items
                .iter()
                .filter_map(|item| item.id.map(|u| u.to_string())),
        );
        // `/Items?parentId=<view_id>&recursive=true` returns every item
        // under this library, so stamping the whole page in one
        // authoritative batch is what lets `latest()` and reconciliation's
        // count comparison scope by library at all.
        s.writer
            .upsert_items_scoped(items, Some(view_id.to_string()))
            .await;

        pages_done += 1;
        start_index += page_len;
        if page_len == 0 || start_index >= total {
            // Enqueued after every page's upsert above (same writer, same
            // per-sender FIFO order -- see `WriteCmd::PruneLibrary`), so
            // every id in `seen_ids` is already committed by the time this
            // runs; nothing this fetch found can be pruned out from under
            // itself.
            s.writer.prune_library(view_id.to_string(), seen_ids).await;
            // Decrement-before-Idle -- see the error arm above.
            drop(guard);
            s.sync_activity.send_replace(SyncActivity::Idle);
            return true;
        }
    }
}

/// BoxSet membership isn't part of the breadth sync above (that syncs the
/// BoxSet's own row, not the *list of what's in it*, since `/Items` doesn't
/// expand collection membership). Called once a `boxsets` view's breadth
/// sync has landed the BoxSet rows themselves.
async fn sync_boxsets_membership(state: &MirrorState, view_id: &str) {
    // The caller's breadth sync fire-and-forget-sent the BoxSet rows' upsert
    // to the writer task; wait for it to commit before querying for BoxSet
    // ids below, or this would race the write and find nothing.
    state.writer.barrier().await;
    for boxset_id in current_boxset_ids(state, view_id).await {
        sync_one_boxset_membership(state, &boxset_id).await;
    }
}

/// Non-recursive `/Items?ParentId=<boxset_id>` -- BoxSet membership isn't
/// expected to page (collections are curated lists), so one page at
/// `PAGE_SIZE` covers everything. Also upserts the returned items
/// themselves, since a BoxSet can reference an item not otherwise in any
/// synced view.
async fn sync_one_boxset_membership(state: &MirrorState, boxset_id: &str) {
    let query = ItemQuery {
        sort_order: None,
        parent_id: Some(boxset_id.to_string()),
        include_item_types: Vec::new(),
        recursive: false,
        sort_by: None,
        fields: item_fields(),
        start_index: 0,
        limit: PAGE_SIZE,
        ids: Vec::new(),
        is_missing: None,
        min_date_last_saved: None,
        ..ItemQuery::new()
    };
    match state.client.get_items(&query).await {
        Ok(result) => {
            let members: Vec<(String, i64)> = result
                .items
                .iter()
                .enumerate()
                .filter_map(|(i, item)| item.id.map(|id| (id.to_string(), i as i64)))
                .collect();
            state.writer.upsert_items(result.items).await;
            state
                .writer
                .set_collection_members(boxset_id.to_string(), members)
                .await;
        }
        Err(e) => {
            tracing::error!(error = %e, boxset_id, "failed to fetch boxset membership")
        }
    }
}

/// Ids of the `BoxSet` items synced as direct children of `view_id`, per
/// `browse_root_item_type`'s parent_id-forcing for that type. Wrapped in
/// `spawn_blocking`: `pool.acquire()` is a blocking mutex/condvar wait and
/// must not run inline on a tokio worker.
async fn current_boxset_ids(state: &MirrorState, view_id: &str) -> Vec<String> {
    let pool = state.read_pool.clone();
    let view_id = view_id.to_string();
    tokio::task::spawn_blocking(move || {
        let conn = pool.acquire();
        let result: rusqlite::Result<Vec<String>> = (|| {
            let mut stmt =
                conn.prepare("SELECT id FROM items WHERE parent_id = ?1 AND item_type = 'BoxSet'")?;
            let rows = stmt.query_map([&view_id], |row| row.get(0))?;
            rows.collect()
        })();
        result.unwrap_or_else(|e| {
            tracing::error!(error = %e, "failed to read boxset ids for membership sync");
            Vec::new()
        })
    })
    .await
    .unwrap_or_else(|e| {
        tracing::error!(error = %e, "current_boxset_ids task panicked");
        Vec::new()
    })
}

/// `pool.acquire()` blocks the thread (see `current_boxset_ids`), so this
/// too runs via `spawn_blocking`.
///
/// docs/PLUGIN-CHANNELS.md §2.1: excludes `item_type =
/// 'Channel'` rows -- every caller here is a per-library sync walk, and a
/// channel's content is browsed live, never mirrored.
async fn current_views(state: &MirrorState) -> Vec<(String, String)> {
    let pool = state.read_pool.clone();
    tokio::task::spawn_blocking(move || {
        let conn = pool.acquire();
        let result: rusqlite::Result<Vec<(String, String)>> = (|| {
            let mut stmt = conn.prepare(
                "SELECT id, collection_type FROM views WHERE item_type IS NOT 'Channel'",
            )?;
            let rows = stmt.query_map([], |row| {
                Ok((
                    row.get(0)?,
                    row.get::<_, Option<String>>(1)?.unwrap_or_default(),
                ))
            })?;
            rows.collect()
        })();
        drop(conn);
        result.unwrap_or_else(|e| {
            tracing::error!(error = %e, "failed to read views for sync");
            Vec::new()
        })
    })
    .await
    .unwrap_or_else(|e| {
        tracing::error!(error = %e, "current_views task panicked");
        Vec::new()
    })
}

fn now_iso() -> String {
    // No chrono dependency needed for "debugging only" metadata (per
    // docs/DATA.md's comment on `meta`) -- Unix millis is enough.
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis().to_string())
        .unwrap_or_else(|_| "0".to_string())
}

/// While `initial_sync_in_progress` is set, incoming `BusEvent`s are
/// buffered here in arrival order instead of applied immediately -- a WS
/// delta for an item a later breadth-sync page hasn't reached yet could
/// otherwise be silently clobbered once that page's stale snapshot lands,
/// and nothing would notice since the snapshot write looks like a normal
/// newer upsert.
///
/// Once sync completes the buffer replays in order. If it overflowed
/// `WS_DELTA_BUFFER_CAP` mid-sync, it's dropped instead and
/// `reconcile_after_sync` is set so a single `reconcile_all` cleans up
/// rather than replaying a gap-y backlog.
async fn bus_listener(state: Weak<MirrorState>, mut bus: broadcast::Receiver<BusEvent>) {
    let mut buffer: Vec<BusEvent> = Vec::new();

    loop {
        let Some(s) = state.upgrade() else { return };

        if !s.initial_sync_in_progress.load(Ordering::Acquire) {
            if s.reconcile_after_sync.swap(false, Ordering::AcqRel) {
                tracing::info!(
                    "running a post-initial-sync reconciliation after the WS delta buffer overflowed"
                );
                reconcile_all(&s).await;
            } else if !buffer.is_empty() {
                tracing::debug!(
                    count = buffer.len(),
                    "replaying WS deltas buffered during initial sync"
                );
                for event in buffer.drain(..) {
                    apply_bus_event(&s, event).await;
                }
            }
        }

        // Clone the small `Arc<Notify>` handle (not the whole `MirrorState`)
        // and drop `s` before awaiting it -- otherwise the `Notified` future
        // would keep the entire `MirrorState` alive for as long as this task
        // waits, which is exactly what `Weak` is here to avoid. Registered
        // before `drop(s)` so a sync completing in that gap still wakes
        // this task promptly.
        let done = s.initial_sync_done.clone();
        let synced = done.notified();
        tokio::pin!(synced);
        drop(s);

        tokio::select! {
            biased;
            recv = bus.recv() => {
                let Some(s) = state.upgrade() else { return };
                match recv {
                    Ok(event) => {
                        if s.initial_sync_in_progress.load(Ordering::Acquire) {
                            if buffer.len() >= WS_DELTA_BUFFER_CAP {
                                tracing::warn!(
                                    buffered = buffer.len(),
                                    "WS delta buffer overflowed during initial sync; dropping it and scheduling a reconcile once sync completes"
                                );
                                buffer.clear();
                                s.reconcile_after_sync.store(true, Ordering::Release);
                            } else {
                                buffer.push(event);
                            }
                        } else {
                            apply_bus_event(&s, event).await;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(skipped)) => {
                        tracing::warn!(
                            skipped,
                            "mirror lagged behind the event bus; forcing reconciliation"
                        );
                        if s.initial_sync_in_progress.load(Ordering::Acquire) {
                            // The buffer itself may now have gaps too (we
                            // don't know what was skipped); dropping it and
                            // reconciling after sync is the same safe
                            // fallback as an outright overflow.
                            buffer.clear();
                            s.reconcile_after_sync.store(true, Ordering::Release);
                        } else {
                            reconcile_all(&s).await;
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => return,
                }
            }
            _ = &mut synced => {
                // Initial sync just completed; loop back around to drain
                // the buffer (or run the scheduled reconcile) at the top.
            }
        }
    }
}

async fn apply_bus_event(state: &MirrorState, event: BusEvent) {
    match event {
        BusEvent::Server(server_event) => apply_server_event(state, server_event).await,
        BusEvent::Connected => {}
        BusEvent::Disconnected => {}
        // Delta first, then reconcile -- see `spawn`'s startup path for why.
        // This is the reconnect signal, so the delta window covers exactly
        // the disconnected period.
        BusEvent::NeedsReconcile => {
            delta_sync(state).await;
            user_data_sync(state).await;
            reconcile_all(state).await;
        }
    }
}

pub(crate) async fn apply_server_event(state: &MirrorState, event: ServerEvent) {
    match event {
        ServerEvent::LibraryChanged {
            added,
            updated,
            removed,
        } => {
            let mut changed: Vec<String> = added.into_iter().chain(updated).collect();
            changed.sort();
            changed.dedup();

            // A LibraryChanged touching a BoxSet (ChildCount changed) needs
            // its membership re-fetched -- the item upsert above only
            // refreshes the BoxSet's own row, not what's inside it.
            let mut changed_boxset_ids: Vec<String> = Vec::new();
            for chunk in changed.chunks(WS_BATCH_SIZE) {
                let items = fetch_and_upsert_ids(state, chunk.to_vec()).await;
                changed_boxset_ids.extend(items.iter().filter_map(|item| {
                    (item.type_ == Some(BaseItemKind::BoxSet))
                        .then(|| item.id.map(|id| id.to_string()))
                        .flatten()
                }));
            }

            if !removed.is_empty() {
                state.writer.remove_items(removed).await;
            }

            if !changed_boxset_ids.is_empty() {
                // Wait for the BoxSet rows themselves to commit first (not
                // strictly required for the membership fetch itself, but
                // keeps write ordering predictable: the BoxSet's own row
                // always lands before its membership does).
                state.writer.barrier().await;
                for boxset_id in changed_boxset_ids {
                    sync_one_boxset_membership(state, &boxset_id).await;
                }
            }

            if !changed.is_empty() {
                // NextUp shifts whenever library contents change; cheap to
                // refresh opportunistically rather than wait for the next
                // reconcile. Wait for the upserts above to commit first so
                // this doesn't race a still-in-flight page write.
                state.writer.barrier().await;
                refresh_next_up(state).await;
            }
        }
        ServerEvent::UserDataChanged { item_userdata } => {
            state.writer.apply_user_data(item_userdata).await;
        }
        ServerEvent::ForceKeepAlive | ServerEvent::Ignored(_) => {}
    }
}

/// Fetches and upserts a batch of items by id, returning what was fetched
/// (the caller uses this to notice any of them are BoxSets).
///
/// Unlike `sync_library_breadth`'s per-view page, a `LibraryChanged` batch
/// spans several libraries at once, so each item's `library_id` is
/// resolved individually (`resolve_library_ids_for_changed_items`) and the
/// batch is grouped by resolved id so each same-library subgroup can go
/// through the writer as one authoritative `upsert_items_scoped` call.
async fn fetch_and_upsert_ids(state: &MirrorState, ids: Vec<String>) -> Vec<BaseItemDto> {
    if ids.is_empty() {
        return Vec::new();
    }
    let query = ItemQuery {
        sort_order: None,
        parent_id: None,
        include_item_types: Vec::new(),
        recursive: true,
        sort_by: None,
        fields: item_fields(),
        start_index: 0,
        limit: ids.len() as u32,
        ids,
        is_missing: None,
        min_date_last_saved: None,
        ..ItemQuery::new()
    };
    match state.client.get_items(&query).await {
        Ok(result) => {
            upsert_cross_library_items(state, &result.items).await;
            result.items
        }
        Err(e) => {
            tracing::error!(error = %e, "failed to fetch changed items by id");
            Vec::new()
        }
    }
}

/// Upserts a batch of items that may span several libraries: resolve each
/// item's owning view id individually (`resolve_library_ids_for_changed_items`),
/// group by the resolved id (including a "still unresolved" `None` group),
/// and send each same-library subgroup to the writer as one authoritative
/// `upsert_items_scoped` call.
///
/// Extracted from `fetch_and_upsert_ids` so [`delta_sync`] can reuse exactly
/// the WS `LibraryChanged` path's machinery: a `minDateLastSaved` page has
/// the same property that motivated this in the first place -- it is keyed on
/// a timestamp, not a parent, so it can carry a new Episode, an updated
/// Movie, and a re-saved BoxSet from three different libraries in one
/// response, and `sync_library_breadth`'s "stamp the whole page with one view
/// id" shortcut is simply wrong for it.
///
/// Every group goes through `WriteCmd::UpsertItems`, which emits
/// `MirrorChange::Upserted` for the rows it touched (`writer::run`), so Home
/// and any open library view refresh off this the moment the write commits --
/// which is the entire point of delta sync.
async fn upsert_cross_library_items(state: &MirrorState, items: &[BaseItemDto]) {
    if items.is_empty() {
        return;
    }
    let library_ids = resolve_library_ids_for_changed_items(state, items).await;
    let mut groups: std::collections::HashMap<Option<String>, Vec<BaseItemDto>> =
        std::collections::HashMap::new();
    for (item, library_id) in items.iter().cloned().zip(library_ids) {
        groups.entry(library_id).or_default().push(item);
    }
    // The same root-type parent flattening `sync_library_breadth` and the
    // reconcile missing-fetch apply (see `browse_root_item_type`): a Series
    // /Movie that arrives through THIS path (delta sync, WS LibraryChanged)
    // reports its physical folder as `ParentId`, and without forcing it
    // onto the view id the `children(view_id)` browse query never lists it
    // -- the user-reported "Castle is missing" bug (every show added since
    // the last full sync was invisible in its library grid while present
    // in the mirror and on Home).
    let views = current_views(state).await;
    for (library_id, group_items) in groups.iter_mut() {
        let Some(library_id) = library_id else {
            continue;
        };
        let Some((_, collection_type)) = views.iter().find(|(id, _)| id == library_id) else {
            continue;
        };
        let root_type = browse_root_item_type(collection_type);
        let view_uuid = uuid::Uuid::parse_str(library_id).ok();
        flatten_root_parents_in_place(group_items, root_type, view_uuid);
    }
    for (library_id, group_items) in groups {
        state
            .writer
            .upsert_items_scoped(group_items, library_id)
            .await;
    }
}

/// Resolves each item's owning library (view) id purely from already-synced
/// ancestor rows in the mirror -- no network calls. Tries `series_id`, then
/// `season_id`, then `parent_id`: an Episode's series/season are almost
/// always already synced by the time a delta lands, and a root-level item's
/// raw parent is the view id itself once `sync_library_breadth` has forced
/// it there. `None` means no resolvable ancestor yet (a brand new root-level
/// item with nothing local to walk to).
async fn resolve_library_ids_via_ancestors(
    state: &MirrorState,
    items: &[BaseItemDto],
) -> Vec<Option<String>> {
    let pool = state.read_pool.clone();
    let candidates: Vec<Vec<String>> = items
        .iter()
        .map(|item| {
            [
                item.series_id.map(|u| u.to_string()),
                item.season_id.map(|u| u.to_string()),
                item.parent_id.map(|u| u.to_string()),
            ]
            .into_iter()
            .flatten()
            .collect()
        })
        .collect();

    tokio::task::spawn_blocking(move || {
        let conn = pool.acquire();
        candidates
            .into_iter()
            .map(|ancestor_ids| {
                ancestor_ids.into_iter().find_map(|ancestor_id| {
                    conn.query_row(
                        "SELECT library_id FROM items WHERE id = ?1",
                        [&ancestor_id],
                        |row| row.get::<_, Option<String>>(0),
                    )
                    .ok()
                    .flatten()
                })
            })
            .collect()
    })
    .await
    .unwrap_or_else(|e| {
        tracing::error!(error = %e, "resolve_library_ids_via_ancestors task panicked");
        items.iter().map(|_| None).collect()
    })
}

/// After the ancestor-chain lookup above, anything still unresolved (a
/// brand new top-level item) is asked for directly: probe each known
/// library's recursive membership for the still-unresolved ids. Bounded by
/// the number of libraries, not by the number of changed items, and stops
/// as soon as everything's resolved or every view's been asked.
async fn resolve_library_ids_for_changed_items(
    state: &MirrorState,
    items: &[BaseItemDto],
) -> Vec<Option<String>> {
    let mut resolved = resolve_library_ids_via_ancestors(state, items).await;
    if !resolved.iter().any(Option::is_none) {
        return resolved;
    }

    let ids: Vec<Option<String>> = items.iter().map(|i| i.id.map(|u| u.to_string())).collect();

    for (view_id, _) in current_views(state).await {
        let unresolved_ids: Vec<String> = ids
            .iter()
            .zip(&resolved)
            .filter_map(|(id, lib)| lib.is_none().then(|| id.clone()).flatten())
            .collect();
        if unresolved_ids.is_empty() {
            break;
        }

        let query = ItemQuery {
            parent_id: Some(view_id.clone()),
            recursive: true,
            ids: unresolved_ids.clone(),
            limit: unresolved_ids.len() as u32,
            ..ItemQuery::new()
        };
        match state.client.get_items(&query).await {
            Ok(result) => {
                for found in &result.items {
                    let Some(found_id) = found.id.map(|u| u.to_string()) else {
                        continue;
                    };
                    if let Some(pos) = ids
                        .iter()
                        .position(|id| id.as_deref() == Some(found_id.as_str()))
                    {
                        if resolved[pos].is_none() {
                            resolved[pos] = Some(view_id.clone());
                        }
                    }
                }
            }
            Err(e) => tracing::error!(
                error = %e,
                view_id,
                "failed to probe view membership for unresolved changed items"
            ),
        }
    }
    resolved
}

/// Current UTC instant in the RFC 3339 / ISO 8601 shape `minDateLastSaved`
/// accepts and [`LAST_DELTA_SYNC_KEY`] stores.
/// Second granularity is deliberate: the cursor is only ever compared with a
/// 5-minute overlap slack applied, so sub-second precision would be noise.
fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// Pure cursor arithmetic, split out so the rule is directly unit-testable:
/// the stored cursor, minus [`DELTA_OVERLAP_SLACK`], is what the next query
/// asks the server for. `None` if the stored cursor isn't a timestamp we
/// wrote.
pub(crate) fn delta_query_since(cursor: &str) -> Option<String> {
    let parsed = chrono::DateTime::parse_from_rfc3339(cursor).ok()?;
    Some(
        (parsed.with_timezone(&chrono::Utc) - DELTA_OVERLAP_SLACK)
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
    )
}

/// Incremental delta sync: one small recursive `/Items` query for everything
/// the server has ADDED OR UPDATED since the stored cursor
/// (`minDateLastSaved`, see `ItemQuery::min_date_last_saved`), upserted
/// straight into the mirror. Unlike [`reconcile_view`]'s count/newest-id
/// probes, this also catches in-place updates that leave both unchanged.
///
/// # Cursor advance rule
///
/// Never move the cursor past a change we haven't stored. The server
/// exposes no `DateLastSaved` on items (filter-only column), so the cursor
/// is the CLIENT clock, captured once before the first request of a pass
/// and written only after a fully successful pass -- never "now at the
/// end" (would silently skip changes made mid-pass) and never derived from
/// the response. Any page erroring leaves the cursor unwritten so the next
/// pass redoes the whole window; an empty response is a success like any
/// other. [`DELTA_OVERLAP_SLACK`] absorbs client-ahead-of-server clock skew.
///
/// # What this does NOT cover
///
/// Deletions: an item removed server-side just stops appearing in queries,
/// so no delta can observe it -- that stays [`reconcile_view`]'s job, which
/// is why every trigger below runs delta *and then* reconcile.
///
/// # Concurrency
///
/// Single-flight, same pattern as [`reconcile_all`]: a trigger arriving
/// mid-pass sets `delta_pending` and the running pass reruns once, so it's
/// deferred rather than lost.
pub(crate) async fn delta_sync(state: &MirrorState) {
    // Initial sync owns first population AND the first cursor stamp (it
    // stamps the instant it *started*, so nothing saved during its long walk
    // falls between the two mechanisms). Running a delta alongside it would
    // both duplicate that work and race that stamp.
    if state.initial_sync_in_progress.load(Ordering::Acquire) {
        tracing::debug!("initial sync in progress; skipping delta sync");
        return;
    }
    single_flight(
        &state.delta_in_progress,
        &state.delta_pending,
        "delta sync",
        || delta_pass(state),
    )
    .await;
}

/// Single-flight with one coalesced rerun: a trigger landing mid-pass sets
/// `pending`, and the running pass reruns once instead of dropping it.
async fn single_flight<F, Fut>(
    in_progress: &std::sync::atomic::AtomicBool,
    pending: &std::sync::atomic::AtomicBool,
    what: &'static str,
    mut pass: F,
) where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    if in_progress.swap(true, Ordering::AcqRel) {
        tracing::debug!(what, "already in progress; deferring this trigger");
        pending.store(true, Ordering::Release);
        return;
    }
    loop {
        pass().await;
        if !pending.swap(false, Ordering::AcqRel) {
            break;
        }
        tracing::debug!(what, "rerunning for a trigger deferred mid-pass");
    }
    in_progress.store(false, Ordering::Release);
}

/// Item types whose rows carry per-user watched state the catch-up compares;
/// Series/Season rollups are refreshed as parents of changed rows instead.
const USER_DATA_LEAF_TYPES: [&str; 5] = ["Episode", "Movie", "Video", "MusicVideo", "Audio"];

/// Watched-state catch-up, run with [`delta_sync`] at launch and on every
/// WebSocket reconnect: Jellyfin keeps `Played`/`PlaybackPositionTicks` in a
/// per-user table that never touches an item's `DateLastSaved`, so a delta
/// pass cannot see a watch made elsewhere, and `UserDataChanged` pushes are
/// only delivered to sessions connected at that instant. Two ids-only
/// queries (`filters=IsPlayed`, `filters=IsResumable`) are diffed against
/// the mirror's own played/in-progress sets and only the differing rows are
/// refetched, plus their Series/Season parents so unplayed counts follow.
/// Same single-flight rule as `delta_sync`; skipped during initial sync,
/// which fetches fresh user data anyway.
pub(crate) async fn user_data_sync(state: &MirrorState) {
    if state.initial_sync_in_progress.load(Ordering::Acquire) {
        tracing::debug!("initial sync in progress; skipping watched-state catch-up");
        return;
    }
    single_flight(
        &state.user_data_in_progress,
        &state.user_data_pending,
        "watched-state catch-up",
        || user_data_pass(state),
    )
    .await;
}

/// Ids whose watched state differs between server and mirror: the symmetric
/// difference of the played sets plus that of the resumable sets, sorted.
pub(crate) fn user_data_changed_ids(
    server_played: &std::collections::HashSet<String>,
    server_resumable: &std::collections::HashSet<String>,
    local_played: &std::collections::HashSet<String>,
    local_resumable: &std::collections::HashSet<String>,
) -> Vec<String> {
    let mut changed: Vec<String> = server_played
        .symmetric_difference(local_played)
        .chain(server_resumable.symmetric_difference(local_resumable))
        .cloned()
        .collect();
    changed.sort();
    changed.dedup();
    changed
}

async fn user_data_pass(state: &MirrorState) {
    let Some(server_played) = server_user_data_ids(state, "IsPlayed").await else {
        return;
    };
    let Some(server_resumable) = server_user_data_ids(state, "IsResumable").await else {
        return;
    };
    let Some((local_played, local_resumable)) = local_user_data_ids(state).await else {
        return;
    };
    let changed = user_data_changed_ids(
        &server_played,
        &server_resumable,
        &local_played,
        &local_resumable,
    );
    if changed.is_empty() {
        return;
    }
    let mut parents = std::collections::BTreeSet::new();
    for chunk in changed.chunks(WS_BATCH_SIZE) {
        for item in fetch_and_upsert_ids(state, chunk.to_vec()).await {
            parents.extend(
                [item.series_id, item.season_id]
                    .into_iter()
                    .flatten()
                    .map(|id| id.to_string()),
            );
        }
    }
    let parents: Vec<String> = parents.into_iter().collect();
    for chunk in parents.chunks(WS_BATCH_SIZE) {
        fetch_and_upsert_ids(state, chunk.to_vec()).await;
    }
    // Played changes shift Next Up; barrier so its refresh sees the upserts.
    state.writer.barrier().await;
    refresh_next_up(state).await;
    tracing::info!(
        changed = changed.len(),
        parents = parents.len(),
        "watched-state catch-up applied user data changed elsewhere"
    );
}

/// Every leaf id the server lists under one user-data `filter`, paged like
/// the reconcile id sweep. `None` on any page failure (a partial set would
/// read as mass "unplayed elsewhere" and refetch half the library).
async fn server_user_data_ids(
    state: &MirrorState,
    filter: &str,
) -> Option<std::collections::HashSet<String>> {
    let mut ids = std::collections::HashSet::new();
    let mut start_index = 0u32;
    loop {
        while state.playback_active.load(Ordering::Acquire) {
            tokio::time::sleep(PLAYBACK_YIELD_POLL).await;
        }
        let query = ItemQuery {
            recursive: true,
            include_item_types: USER_DATA_LEAF_TYPES.iter().map(|t| t.to_string()).collect(),
            sort_by: Some("SortName".to_string()),
            start_index,
            limit: ID_SWEEP_PAGE_SIZE,
            enable_images: Some(false),
            enable_user_data: Some(false),
            filters: vec![filter.to_string()],
            ..ItemQuery::new()
        };
        let result = match state.client.get_items(&query).await {
            Ok(r) => r,
            Err(e) => {
                tracing::error!(error = %e, filter, start_index, "watched-state catch-up page failed");
                return None;
            }
        };
        let page_len = result.items.len() as u32;
        let total = result
            .total_record_count
            .map(|n| n.max(0) as u32)
            .unwrap_or(page_len);
        ids.extend(
            result
                .items
                .iter()
                .filter_map(|i| i.id.map(|u| u.to_string())),
        );
        start_index += page_len;
        if page_len == 0 || start_index >= total {
            break;
        }
    }
    Some(ids)
}

/// The mirror's own (played, in-progress) leaf id sets, the local side of
/// [`user_data_changed_ids`]. Barriers first so this pass's own delta
/// upserts are visible. `None` on a read failure (no diff, no writes).
async fn local_user_data_ids(
    state: &MirrorState,
) -> Option<(
    std::collections::HashSet<String>,
    std::collections::HashSet<String>,
)> {
    state.writer.barrier().await;
    let pool = state.read_pool.clone();
    tokio::task::spawn_blocking(move || {
        let conn = pool.acquire();
        let placeholders = std::iter::repeat_n("?", USER_DATA_LEAF_TYPES.len())
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "SELECT id, played, playback_position_ticks FROM items \
             WHERE item_type IN ({placeholders}) AND (played = 1 OR playback_position_ticks > 0)"
        );
        let result: rusqlite::Result<_> = (|| {
            let mut stmt = conn.prepare(&sql)?;
            let params: Vec<&dyn rusqlite::ToSql> = USER_DATA_LEAF_TYPES
                .iter()
                .map(|t| t as &dyn rusqlite::ToSql)
                .collect();
            let mut played = std::collections::HashSet::new();
            let mut resumable = std::collections::HashSet::new();
            for row in stmt.query_map(params.as_slice(), |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, bool>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            })? {
                let (id, is_played, position) = row?;
                if is_played {
                    played.insert(id.clone());
                }
                if position > 0 {
                    resumable.insert(id);
                }
            }
            Ok((played, resumable))
        })();
        drop(conn);
        match result {
            Ok(sets) => Some(sets),
            Err(e) => {
                tracing::error!(error = %e, "watched-state catch-up: failed to read local user data");
                None
            }
        }
    })
    .await
    .unwrap_or_else(|e| {
        tracing::error!(error = %e, "local_user_data_ids task panicked");
        None
    })
}

/// One delta pass. See [`delta_sync`] for the cursor rule this implements;
/// the single-flight guard lives in the caller.
async fn delta_pass(state: &MirrorState) {
    let cursor = read_mirror_meta(state, LAST_DELTA_SYNC_KEY).await;
    let Some(cursor) = cursor else {
        // No cursor (a pre-delta-sync mirror, or a `meta` row lost to a
        // schema reset): adopt "now" and query nothing this pass. Querying
        // from the epoch would return the ENTIRE library -- strictly worse
        // than the resync this feature exists to avoid -- and changes older
        // than "now" are covered by whatever populated the mirror, with
        // `reconcile_view`'s probes as the backstop they already are.
        let now = now_rfc3339();
        tracing::info!(cursor = %now, "no delta cursor yet; adopting the current instant");
        state.writer.set_meta(LAST_DELTA_SYNC_KEY, now).await;
        return;
    };
    let Some(since) = delta_query_since(&cursor) else {
        // Only this module writes this key, in one fixed format, so this
        // means the value was corrupted. Re-stamping "now" is the same
        // recovery as the missing-cursor arm above rather than wedging
        // delta forever on an unparseable value.
        tracing::warn!(cursor = %cursor, "delta cursor is not a timestamp; resetting it to now");
        state
            .writer
            .set_meta(LAST_DELTA_SYNC_KEY, now_rfc3339())
            .await;
        return;
    };

    // Captured BEFORE the first request goes out -- see the cursor rule.
    let pass_started_at = now_rfc3339();

    let mut start_index = 0u32;
    let mut total_seen = 0usize;
    loop {
        // Yield to playback like `sync_library_breadth` does: a delta page
        // is normally tiny, but one landing after a server-side bulk
        // metadata refresh can be many pages and must not compete with an
        // active stream for the link.
        while state.playback_active.load(Ordering::Acquire) {
            tokio::time::sleep(PLAYBACK_YIELD_POLL).await;
        }

        let query = ItemQuery {
            // No `parent_id`: the whole point is one query covering every
            // library at once. `resolve_library_ids_for_changed_items` (via
            // `upsert_cross_library_items`) puts each returned item back in
            // its own library.
            parent_id: None,
            recursive: true,
            // Stable page ordering. The server's default ordering for this
            // query shape is unspecified, and paging an unstably-ordered
            // result set with `startIndex` can skip rows outright.
            sort_by: Some("SortName".to_string()),
            fields: item_fields(),
            start_index,
            limit: PAGE_SIZE,
            min_date_last_saved: Some(since.clone()),
            ..ItemQuery::new()
        };
        let result = match state.client.get_items(&query).await {
            Ok(r) => r,
            Err(e) => {
                // Deliberately returns WITHOUT writing the cursor: partial
                // progress is never banked (see the cursor rule).
                tracing::error!(error = %e, since = %since, start_index, "delta sync page failed");
                return;
            }
        };
        let page_len = result.items.len() as u32;
        let total = result
            .total_record_count
            .map(|n| n.max(0) as u32)
            .unwrap_or(page_len);

        total_seen += result.items.len();
        upsert_cross_library_items(state, &result.items).await;

        start_index += page_len;
        if page_len == 0 || start_index >= total {
            break;
        }
    }

    if total_seen > 0 {
        // Same rationale as the WS `LibraryChanged` path: the set of "next
        // up" episodes shifts whenever library contents do, and it's a
        // `meta` key that no item upsert refreshes on its own. Barrier
        // first so NextUp's own upsert can't race a still-in-flight page.
        state.writer.barrier().await;
        refresh_next_up(state).await;
        tracing::info!(
            changed = total_seen,
            since = %since,
            "delta sync applied server-side changes"
        );
    }

    state
        .writer
        .set_meta(LAST_DELTA_SYNC_KEY, pass_started_at)
        .await;
}

/// Reads one `meta` value off the read pool. `spawn_blocking` for the same
/// reason `current_boxset_ids` uses it. Barriers first so a `set_meta` this
/// pass's own caller just enqueued is visible rather than read stale.
async fn read_mirror_meta(state: &MirrorState, key: &'static str) -> Option<String> {
    state.writer.barrier().await;
    let pool = state.read_pool.clone();
    tokio::task::spawn_blocking(move || {
        let conn = pool.acquire();
        crate::schema::read_meta(&conn, key)
    })
    .await
    .unwrap_or_else(|e| {
        tracing::error!(error = %e, key, "read_mirror_meta task panicked");
        None
    })
}

/// Pure decision: does this library need a resync? Split out from the
/// network/DB glue so it's directly unit-testable.
pub(crate) fn needs_resync(
    local_count: i64,
    server_count: i32,
    local_newest_date_created: Option<&str>,
    server_date_last_media_added: Option<&str>,
) -> bool {
    if i64::from(server_count) != local_count {
        return true;
    }
    match (local_newest_date_created, server_date_last_media_added) {
        (Some(local), Some(server)) => local < server,
        (None, Some(_)) => true,
        _ => false,
    }
}

/// Single-flight guard: the startup path's explicit call can race
/// `reconcile_timer`'s first tick, and either could independently decide
/// "resync" for the same library. A pass that finds another already running
/// sets a pending flag and returns; the running pass reruns once on
/// completion, so a trigger arriving mid-pass is deferred, never lost.
async fn reconcile_all(state: &MirrorState) {
    if state.reconcile_in_progress.swap(true, Ordering::AcqRel) {
        tracing::debug!("reconcile already in progress; deferring this trigger");
        state.reconcile_pending.store(true, Ordering::Release);
        return;
    }
    loop {
        let views = match state.client.get_user_views().await {
            Ok(v) => v,
            Err(e) => {
                tracing::error!(error = %e, "reconciliation: failed to fetch views");
                break;
            }
        };
        for view in views {
            // docs/PLUGIN-CHANNELS.md §2.1: a Channel
            // view is browsed live, never mirrored -- skip it here rather
            // than relying on `reconcile_view`'s `item_types_for_collection`
            // early return, which keys off `collection_type`, empty for a
            // Channel view same as plenty of legitimately-unsupported kinds.
            if view.type_ == Some(BaseItemKind::Channel) {
                continue;
            }
            reconcile_view(state, &view).await;
        }
        if !state.reconcile_pending.swap(false, Ordering::AcqRel) {
            break;
        }
        tracing::debug!("rerunning reconcile for a trigger deferred mid-pass");
    }
    state.reconcile_in_progress.store(false, Ordering::Release);
}

async fn reconcile_view(state: &MirrorState, view: &BaseItemDto) {
    let Some(view_id) = view.id.map(|u| u.to_string()) else {
        return;
    };
    let collection_type = view
        .collection_type
        .map(|c| c.to_string())
        .unwrap_or_default();
    let item_types = crate::item_types_for_collection(&collection_type);
    if item_types.is_empty() {
        return;
    }

    // Restricting this probe to `includeItemTypes=Episode` (etc.) EXCLUDES
    // virtual/unaired placeholder episodes on a recursive query, while
    // `sync_library_breadth`'s unrestricted fetch INCLUDES them -- a
    // server-side default independent of `isMissing`. Rather than pin the
    // fix to that unconfirmed server behavior, the probe below uses the same
    // "sync universe" shape as `sync_library_breadth` -- see
    // `library_universe_query` -- so it holds regardless of which default is
    // in play. `local_summary` matches by counting every item type under
    // the library, not just `item_types`.
    let query = ItemQuery {
        limit: 1,
        ..library_universe_query(&view_id)
    };
    let server_count = match state.client.get_items(&query).await {
        Ok(r) => r.total_record_count.unwrap_or(0),
        Err(e) => {
            tracing::error!(error = %e, view_id, "reconciliation: failed to fetch item count");
            return;
        }
    };
    // Some servers return `DateLastMediaAdded` as DateTime.MinValue
    // ("0001-01-01T00:00:00") on every `/UserViews` entry, i.e. unpopulated;
    // the sentinel maps to `None` (probe unavailable) rather than a real
    // timestamp so it can't permanently mute the date branch.
    let server_newest = view
        .date_last_media_added
        .map(|d| d.to_rfc3339())
        .filter(|d| !d.starts_with("0001-"));

    let (local_count, local_newest) = local_summary(state, &view_id, item_types).await;

    let mut resync = needs_resync(
        local_count,
        server_count,
        local_newest.as_deref(),
        server_newest.as_deref(),
    );

    // Third probe -- newest-ids presence. The two probes above are blind to
    // a real item silently REPLACING a virtual placeholder (new id, same
    // count, no date movement if `DateLastMediaAdded` is unpopulated). When
    // the cheap probes say "in sync", fetch the server's 20 newest items and
    // check each id exists locally; any absent id is drift by definition.
    // 20 deep means a missed swap stays detectable until 20 newer items
    // arrive, which are themselves detectable events that trigger a resync.
    let mut newest_ids_missing = false;
    if !resync {
        let newest_query = ItemQuery {
            parent_id: Some(view_id.clone()),
            recursive: true,
            sort_by: Some("DateCreated".to_string()),
            sort_order: Some("Descending".to_string()),
            limit: 20,
            is_missing: None,
            min_date_last_saved: None,
            ..ItemQuery::new()
        };
        match state.client.get_items(&newest_query).await {
            Ok(r) => {
                let ids: Vec<String> = r
                    .items
                    .iter()
                    .filter_map(|i| i.id.map(|u| u.to_string()))
                    .collect();
                newest_ids_missing = !local_ids_all_present(state, &ids).await;
                if newest_ids_missing {
                    resync = true;
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, view_id, "reconciliation: newest-ids probe failed");
            }
        }
    }
    // Logged every pass, not just on mismatch, so a blind probe (wrong
    // universe / poisoned date) is distinguishable from reconcile never
    // running at all.
    tracing::info!(
        view_id,
        server_count,
        local_count,
        local_newest = local_newest.as_deref().unwrap_or("-"),
        server_newest = server_newest.as_deref().unwrap_or("-"),
        newest_ids_missing,
        resync,
        "reconcile decision"
    );
    if resync {
        match reconcile_sweep(state, &view_id, &collection_type).await {
            Some(outcome) => {
                tracing::info!(
                    view_id,
                    server_count,
                    local_count,
                    server_ids = outcome.server_ids,
                    orphans_removed = outcome.orphans_removed,
                    missing_fetched = outcome.missing_fetched,
                    "reconciliation mismatch; id sweep converged the library"
                );
                // Same post-resync side effect the full breadth walk had: a
                // `boxsets` library's BoxSet rows are only half the story,
                // so membership is re-fetched per BoxSet exactly as before.
                if collection_type == "boxsets" {
                    sync_boxsets_membership(state, &view_id).await;
                }
            }
            None => {
                // No fallback to the full breadth walk (see
                // `reconcile_sweep`'s doc): nothing was deleted, and the
                // mismatch is left standing for the next reconcile tick.
                tracing::warn!(
                    view_id,
                    server_count,
                    local_count,
                    "reconciliation mismatch; id sweep failed, keeping mirror state for the \
                     next reconcile pass"
                );
            }
        }
    }
}

/// Outcome of one successful [`reconcile_sweep`], for the log line that
/// replaced the old "resyncing library" one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SweepOutcome {
    /// Ids the server's enumeration returned for this library.
    server_ids: usize,
    /// Local rows the sweep pruned (present locally, gone server-side).
    orphans_removed: usize,
    /// Full DTOs fetched and upserted (present server-side, absent locally).
    missing_fetched: usize,
}

/// ID-level reconciliation sweep -- what a `reconcile_view` mismatch runs
/// instead of a full breadth resync. Finding drift and carrying the repair
/// payload are separate costs; only the second needs bytes, and only for
/// items that actually drifted.
///
///   1. Enumerate the library's ids with the same recursive query the
///      breadth walk uses, minus every byte that isn't an id
///      ([`id_sweep_query`]).
///   2. Diff against the mirror's own id set for that library
///      ([`local_library_ids`], the same `library_id` scoping
///      `local_summary` counts and `prune_library` deletes by).
///   3. Fetch full DTOs for only the missing ids, in [`WS_BATCH_SIZE`]
///      chunks.
///   4. Prune the orphans through the same
///      [`WriterHandle::prune_library`](crate::writer::WriterHandle::prune_library)
///      call the breadth walk issues, with the enumerated server id set as
///      `keep_ids`.
///
/// # Collection-membership deletion semantics
///
/// Step 4 is `prune_library`, not `remove_items(orphans)`: it only touches
/// rows stamped `library_id = <this view>` (see `writer::apply_prune_library`),
/// so an item living in another library that merely appears in this one's
/// collections is structurally out of reach, as is a BoxSet member with no
/// `library_id` at all. `keep_ids` is evaluated against `items` inside the
/// writer's transaction, so the deletion set is computed from committed
/// state at commit time.
///
/// # Error posture
///
/// Every network step is fail-closed and all reads happen before any
/// deletion: a failed page or chunk returns `None` having pruned nothing,
/// leaving the mismatch for the next reconcile tick to retry. No fallback
/// to the full breadth walk -- that would reintroduce the exact resync this
/// exists to remove, on the one path (a flaky link) least able to afford
/// it. Upserts from a chunk before a later failure stay in place, since
/// they're idempotent and additive.
///
/// # Concurrency contracts
///
/// Same contracts as the breadth walk it replaces: a [`BreadthSyncGuard`]
/// covers the whole sweep, `playback_active` is honored between every
/// page/chunk, and `SyncActivity::Syncing` is emitted per page/chunk. The
/// single-flight `reconcile_in_progress`/`reconcile_pending` pair lives one
/// level up, in [`reconcile_all`].
async fn reconcile_sweep(
    state: &MirrorState,
    view_id: &str,
    collection_type: &str,
) -> Option<SweepOutcome> {
    let Some(guard) = BreadthSyncGuard::enter(&state.self_weak) else {
        tracing::debug!(view_id, "mirror dropped before the reconcile sweep began");
        return None;
    };
    let outcome = reconcile_sweep_inner(state, view_id, collection_type).await;
    // Decrement-before-Idle on every exit path (success, network failure,
    // read failure): an observer reacting to this Idle must already see
    // `is_syncing() == false` -- see the `breadth_syncs_in_flight` docs and
    // `sync_library_breadth`'s matching pair of drop sites.
    drop(guard);
    state.sync_activity.send_replace(SyncActivity::Idle);
    outcome
}

/// The sweep proper. Split from [`reconcile_sweep`] purely so the guard-drop
/// and terminal-`Idle` pair can wrap every `return` in one place rather than
/// being repeated at each early exit.
async fn reconcile_sweep_inner(
    state: &MirrorState,
    view_id: &str,
    collection_type: &str,
) -> Option<SweepOutcome> {
    // --- 1. Enumerate the server's ids for this library ------------------
    let mut server_ids: Vec<String> = Vec::new();
    let mut start_index = 0u32;
    let mut pages_done = 0u32;
    let mut total_items: Option<u32> = None;
    loop {
        // Yield to playback exactly like `sync_library_breadth` does. An
        // ids page is small, but "small" is relative to a stream that's
        // buffering on the same constrained link.
        while state.playback_active.load(Ordering::Acquire) {
            tokio::time::sleep(PLAYBACK_YIELD_POLL).await;
        }
        // Same "sent before the fetch, not after" rule as the breadth walk,
        // so a UI polling mid-fetch sees the page currently in flight.
        state.sync_activity.send_replace(SyncActivity::Syncing {
            library_name_or_id: view_id.to_string(),
            pages_done,
            items_done: start_index,
            total_items,
        });

        let result = match state
            .client
            .get_items(&id_sweep_query(view_id, start_index))
            .await
        {
            Ok(r) => r,
            Err(e) => {
                tracing::error!(
                    error = %e, view_id, start_index,
                    "reconcile sweep: id enumeration page failed"
                );
                return None;
            }
        };
        let page_len = result.items.len() as u32;
        let total = result
            .total_record_count
            .map(|n| n.max(0) as u32)
            .unwrap_or(page_len);
        total_items = Some(total);
        server_ids.extend(
            result
                .items
                .iter()
                .filter_map(|item| item.id.map(|u| u.to_string())),
        );

        pages_done += 1;
        start_index += page_len;
        if page_len == 0 || start_index >= total {
            break;
        }
    }

    // --- 2. Diff against the mirror's id set for this library -------------
    // `None` = the read itself failed; treat that as a failed sweep rather
    // than as "the mirror is empty", which would prune the entire library.
    let local_ids = local_library_ids(state, view_id).await?;
    let server_set: std::collections::HashSet<&str> =
        server_ids.iter().map(String::as_str).collect();
    let orphans = local_ids
        .iter()
        .filter(|id| !server_set.contains(id.as_str()))
        .count();
    let missing: Vec<String> = server_ids
        .iter()
        .filter(|id| !local_ids.contains(*id))
        .cloned()
        .collect();
    drop(server_set);

    // --- 3. Fetch full DTOs for JUST the missing ids ----------------------
    let root_type = browse_root_item_type(collection_type);
    let view_uuid = uuid::Uuid::parse_str(view_id).ok();
    let mut missing_fetched = 0usize;
    for (chunk_index, chunk) in missing.chunks(WS_BATCH_SIZE).enumerate() {
        while state.playback_active.load(Ordering::Acquire) {
            tokio::time::sleep(PLAYBACK_YIELD_POLL).await;
        }
        // The denominator switches to the missing set here: the enumeration
        // phase is done (its pages are all accounted for), and what's left to
        // do is exactly `missing.len()` items. Honest, and it keeps the pill's
        // bar meaningful instead of pinning it at 100% through the phase that
        // actually moves bytes.
        state.sync_activity.send_replace(SyncActivity::Syncing {
            library_name_or_id: view_id.to_string(),
            pages_done: pages_done + chunk_index as u32,
            items_done: missing_fetched as u32,
            total_items: Some(missing.len() as u32),
        });

        let query = ItemQuery {
            recursive: true,
            fields: item_fields(),
            limit: chunk.len() as u32,
            ids: chunk.to_vec(),
            ..ItemQuery::new()
        };
        let result = match state.client.get_items(&query).await {
            Ok(r) => r,
            Err(e) => {
                tracing::error!(
                    error = %e, view_id, chunk = chunk.len(),
                    "reconcile sweep: by-ids fetch of missing items failed"
                );
                return None;
            }
        };
        let mut items = result.items;
        // Same browse-flattening the breadth walk applies to its pages (see
        // `browse_root_item_type`): without it a freshly-fetched Movie/Series
        // would carry the server's intermediate physical-folder `ParentId`
        // and never show up under `children(view_id)`.
        flatten_root_parents_in_place(&mut items, root_type, view_uuid);
        missing_fetched += items.len();
        // Scoped to this view -- these ids came out of *this* library's
        // recursive enumeration, so the whole batch is attributable the same
        // way a breadth-sync page is (no `resolve_library_ids_for_changed_items`
        // round trips needed, unlike the WS/delta paths).
        state
            .writer
            .upsert_items_scoped(items, Some(view_id.to_string()))
            .await;
    }

    // --- 4. Prune the orphans (last: nothing is deleted until every fetch
    // above has succeeded) -------------------------------------------------
    let server_id_count = server_ids.len();
    if orphans > 0 {
        // Enqueued after the upserts above, same writer, same FIFO order, so
        // every id in `server_ids` that this sweep just fetched is already
        // committed and cannot be pruned out from under itself. Skipped
        // outright when nothing drifted out: `apply_prune_library` reads every
        // row's DTO blob for the library it scopes to, which is real work to
        // do for a guaranteed-empty delete set.
        state
            .writer
            .prune_library(view_id.to_string(), server_ids)
            .await;
    }

    Some(SweepOutcome {
        server_ids: server_id_count,
        orphans_removed: orphans,
        missing_fetched,
    })
}

/// One enumeration page of [`reconcile_sweep`]: same "sync universe" shape
/// as `sync_library_breadth`'s query -- see `library_universe_query` --
/// stripped of every byte that isn't needed to learn an id.
///
/// Used only as a set of ids, never written to the mirror:
/// `enableUserData=false` would make every item look unplayed to
/// `rows::extract_columns`. The sweep re-fetches full DTOs for the ids it
/// actually stores.
fn id_sweep_query(view_id: &str, start_index: u32) -> ItemQuery {
    ItemQuery {
        sort_by: Some("SortName".to_string()),
        start_index,
        limit: ID_SWEEP_PAGE_SIZE,
        enable_images: Some(false),
        enable_user_data: Some(false),
        ..library_universe_query(view_id)
    }
}

/// Every id the mirror currently holds for one library -- the same
/// `library_id` scoping `local_summary`'s `COUNT(*)` and `prune_library` use,
/// so the sweep's diff is over the same universe both work in. `None` on a
/// read failure (treated as "sweep failed, keep state", not "empty mirror,
/// prune everything").
///
/// Barriers first so any write this reconcile pass already enqueued is
/// visible rather than read stale.
async fn local_library_ids(
    state: &MirrorState,
    view_id: &str,
) -> Option<std::collections::HashSet<String>> {
    state.writer.barrier().await;
    let pool = state.read_pool.clone();
    let view_id = view_id.to_string();
    tokio::task::spawn_blocking(move || {
        let conn = pool.acquire();
        let result: rusqlite::Result<std::collections::HashSet<String>> = (|| {
            let mut stmt = conn.prepare("SELECT id FROM items WHERE library_id = ?1")?;
            let rows = stmt.query_map([&view_id], |row| row.get::<_, String>(0))?;
            rows.collect()
        })();
        drop(conn);
        match result {
            Ok(ids) => Some(ids),
            Err(e) => {
                tracing::error!(error = %e, "reconcile sweep: failed to read local library ids");
                None
            }
        }
    })
    .await
    .unwrap_or_else(|e| {
        tracing::error!(error = %e, "local_library_ids task panicked");
        None
    })
}

/// Newest-ids presence probe helper: true iff EVERY id is already a row in
/// `items`. Indexed primary-key lookups (one `IN` list of at most 20 ids),
/// `spawn_blocking` for the same reason as `local_summary` below. An empty
/// id list is trivially "all present" (a server returning zero items for
/// the newest-20 query has nothing to be missing).
async fn local_ids_all_present(state: &MirrorState, ids: &[String]) -> bool {
    if ids.is_empty() {
        return true;
    }
    let pool = state.read_pool.clone();
    let ids: Vec<String> = ids.to_vec();
    tokio::task::spawn_blocking(move || {
        let conn = pool.acquire();
        let placeholders = std::iter::repeat_n("?", ids.len())
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!("SELECT COUNT(*) FROM items WHERE id IN ({placeholders})");
        let params: Vec<&dyn rusqlite::ToSql> =
            ids.iter().map(|s| s as &dyn rusqlite::ToSql).collect();
        let found: rusqlite::Result<i64> =
            conn.query_row(&sql, params.as_slice(), |row| row.get(0));
        drop(conn);
        match found {
            Ok(n) => n as usize == ids.len(),
            Err(e) => {
                tracing::error!(error = %e, "newest-ids presence query failed");
                true // fail open: a broken read must not force resync loops
            }
        }
    })
    .await
    .unwrap_or(true)
}

/// `pool.acquire()` blocks the calling thread, so this runs via
/// `spawn_blocking` (same rationale as `current_views`). Scoped by
/// `library_id = view_id`, not just `item_type` -- otherwise two libraries
/// sharing a `collection_type` would compare the server's per-library count
/// against a local count summed across every library of that type.
///
/// Two aggregates, scoped differently within the same `WHERE library_id = ?`
/// (`idx_items_latest`'s leading column serves this as an index-only scan):
///
/// - `COUNT(*)` covers every item type under this library, matching
///   `reconcile_view`'s unrestricted server-count probe (see
///   `library_universe_query`), so the two counts are provably the same
///   universe.
/// - `MAX(date_created)` stays scoped to `item_types` and excludes
///   `is_virtual` rows: a virtual placeholder's `DateCreated` isn't tied to
///   real media landing and can exceed the server's `DateLastMediaAdded`
///   (which only advances for real media), which would otherwise poison
///   `needs_resync`'s date check into a permanent false "up to date".
async fn local_summary(
    state: &MirrorState,
    view_id: &str,
    item_types: &[&str],
) -> (i64, Option<String>) {
    let pool = state.read_pool.clone();
    let view_id = view_id.to_string();
    let item_types: Vec<String> = item_types.iter().map(|s| s.to_string()).collect();
    tokio::task::spawn_blocking(move || {
        let conn = pool.acquire();
        let placeholders = std::iter::repeat_n("?", item_types.len())
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "SELECT COUNT(*), \
                    MAX(CASE WHEN item_type IN ({placeholders}) AND is_virtual = 0 \
                             THEN date_created END) \
             FROM items WHERE library_id = ?"
        );
        let mut params: Vec<&dyn rusqlite::ToSql> = item_types
            .iter()
            .map(|s| s as &dyn rusqlite::ToSql)
            .collect();
        params.push(&view_id as &dyn rusqlite::ToSql);
        let result: rusqlite::Result<(i64, Option<String>)> =
            conn.query_row(&sql, params.as_slice(), |row| {
                Ok((row.get(0)?, row.get(1)?))
            });
        drop(conn);
        result.unwrap_or_else(|e| {
            tracing::error!(error = %e, "failed to read local summary for reconciliation");
            (0, None)
        })
    })
    .await
    .unwrap_or_else(|e| {
        tracing::error!(error = %e, "local_summary task panicked");
        (0, None)
    })
}

/// `spawn`'s `needs_initial_sync = false` branch already calls
/// `reconcile_all` once, directly, before this timer's first tick ever
/// fires (see `populated_mirror_startup_runs_reconcile_at_t0_not_only_on_the_timer`).
/// This timer is purely the backstop for whatever that startup call, the WS
/// `LibraryChanged` path, and the reconnect `NeedsReconcile` signal all miss.
async fn reconcile_timer(state: Weak<MirrorState>) {
    let mut interval = tokio::time::interval(RECONCILE_INTERVAL);
    interval.tick().await; // first tick is immediate; startup already reconciles/syncs
    loop {
        interval.tick().await;
        let Some(state) = state.upgrade() else { break };
        // Delta first, then reconcile -- see `spawn`'s startup path.
        delta_sync(&state).await;
        reconcile_all(&state).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock_server::MockServer;
    use crate::pool::ReadPool;
    use crate::writer::WriterHandle;
    use jellyfin_api::{ClientIdentity, JellyfinClient};
    use serde_json::json;
    use tokio::sync::mpsc;

    fn identity() -> ClientIdentity {
        ClientIdentity {
            client: "Jellybeam Test".to_string(),
            device: "test".to_string(),
            device_id: "test-device".to_string(),
            version: "0.1.0".to_string(),
        }
    }

    /// Wires a real writer task + read pool against a fresh temp mirror, so
    /// sync-engine tests exercise the actual DB path, not a stub.
    struct TestMirror {
        _dir: tempfile::TempDir,
        state: Arc<MirrorState>,
        _writer_task: tokio::task::JoinHandle<()>,
    }

    impl TestMirror {
        fn new(client: JellyfinClient) -> Self {
            let dir = tempfile::tempdir().expect("tempdir");
            let path = dir.path().join("mirror.db");
            let (conn, _empty) = crate::schema::open_and_prepare(&path).expect("open");
            let (tx, rx) = mpsc::channel(64);
            let (changes_tx, _) = tokio::sync::broadcast::channel(64);
            let writer_changes_tx = changes_tx.clone();
            let writer_task = tokio::task::spawn_blocking(move || {
                crate::writer::run(conn, rx, writer_changes_tx)
            });
            let read_pool = Arc::new(ReadPool::open(&path, 2).expect("read pool"));
            let (sync_activity_tx, _) = tokio::sync::watch::channel(SyncActivity::Idle);
            let state = Arc::new_cyclic(|weak| {
                MirrorState::new(
                    client,
                    WriterHandle::new(tx),
                    read_pool,
                    changes_tx,
                    sync_activity_tx,
                    weak.clone(),
                )
            });
            Self {
                _dir: dir,
                state,
                _writer_task: writer_task,
            }
        }

        /// Convenience for call sites (like `initial_sync`) that take a `Weak<MirrorState>`.
        fn weak(&self) -> Weak<MirrorState> {
            Arc::downgrade(&self.state)
        }

        async fn item_count(&self) -> i64 {
            self.state.writer.barrier().await;
            let conn = self.state.read_pool.acquire();
            conn.query_row("SELECT COUNT(*) FROM items", [], |r| r.get(0))
                .expect("count")
        }

        /// Cheapest stand-in for "the row carries the server's current metadata".
        async fn item_name(&self, id: &str) -> Option<String> {
            use rusqlite::OptionalExtension;
            self.state.writer.barrier().await;
            let conn = self.state.read_pool.acquire();
            conn.query_row("SELECT name FROM items WHERE id = ?1", [id], |r| r.get(0))
                .optional()
                .expect("name query")
        }

        async fn meta(&self, key: &str) -> Option<String> {
            self.state.writer.barrier().await;
            let conn = self.state.read_pool.acquire();
            crate::schema::read_meta(&conn, key)
        }
    }

    fn movie_json(id: &str, name: &str) -> serde_json::Value {
        json!({ "Id": id, "Name": name, "Type": "Movie" })
    }

    /// Stages `reconcile_sweep`'s ids-only enumeration page (`id_sweep_query`),
    /// returning bare `{"Id": ...}` objects as a real no-fields response would.
    fn route_sweep_enumeration(server: &MockServer, view_id: &str, ids: &[&str]) {
        let limit = ID_SWEEP_PAGE_SIZE.to_string();
        server.route(
            "/Items",
            &[("parentId", view_id), ("limit", &limit)],
            json!({
                "Items": ids.iter().map(|id| json!({ "Id": id })).collect::<Vec<_>>(),
                "TotalRecordCount": ids.len(),
            }),
        );
    }

    /// Stages the sweep's by-ids repair fetch for exactly `items`.
    fn route_by_ids(server: &MockServer, items: &[serde_json::Value]) {
        let ids = items
            .iter()
            .filter_map(|i| i.get("Id").and_then(|v| v.as_str()))
            .collect::<Vec<_>>()
            .join(",");
        server.route(
            "/Items",
            &[("ids", &ids)],
            json!({ "Items": items, "TotalRecordCount": items.len() }),
        );
    }

    /// Request-shape markers the sweep tests share to distinguish request kinds.
    const SWEEP_PAGE_MARKER: &str = "enableImages=false";
    const FULL_DTO_MARKER: &str = "fields=Overview";
    const BY_IDS_MARKER: &str = "ids=";

    #[tokio::test]
    async fn initial_sync_populates_views_resume_next_up_and_breadth() {
        let server = MockServer::start().await;
        let client = JellyfinClient::from_token(&server.base_url, identity(), "tok");

        server.route(
            "/UserViews",
            &[],
            json!({ "Items": [{ "Id": "11111111-1111-1111-1111-111111111111", "Name": "Movies", "CollectionType": "movies" }] }),
        );
        server.route("/UserItems/Resume", &[], json!({ "Items": [] }));
        server.route("/Shows/NextUp", &[], json!({ "Items": [] }));
        server.route(
            "/Items",
            &[
                ("parentId", "11111111-1111-1111-1111-111111111111"),
                ("startIndex", "0"),
            ],
            json!({
                "Items": [
                    movie_json("22222222-2222-2222-2222-222222222222", "Movie A"),
                    movie_json("33333333-3333-3333-3333-333333333333", "Movie B"),
                ],
                "TotalRecordCount": 2
            }),
        );

        let mirror = TestMirror::new(client);
        initial_sync(&mirror.weak()).await;

        assert_eq!(mirror.item_count().await, 2);
        let conn = mirror.state.read_pool.acquire();
        let view_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM views", [], |r| r.get(0))
            .expect("views");
        assert_eq!(view_count, 1);
    }

    fn view_item_type(mirror: &TestMirror, view_id: &str) -> Option<String> {
        use rusqlite::OptionalExtension;
        let conn = mirror.state.read_pool.acquire();
        conn.query_row(
            "SELECT item_type FROM views WHERE id = ?1",
            [view_id],
            |r| r.get(0),
        )
        .optional()
        .expect("view item_type query")
    }

    /// Pins: `sync_views` persists `/UserViews`' own `Type` into `views.item_type`, distinct from `collection_type` (docs/PLUGIN-CHANNELS.md §2.1, §3).
    #[tokio::test]
    async fn sync_views_stores_item_type() {
        let server = MockServer::start().await;
        let client = JellyfinClient::from_token(&server.base_url, identity(), "tok");
        let movies_view = "11111111-1111-1111-1111-111111111111";
        let channel_view = "22222222-2222-2222-2222-222222222222";
        server.route(
            "/UserViews",
            &[],
            json!({ "Items": [
                { "Id": movies_view, "Name": "Movies", "CollectionType": "movies", "Type": "CollectionFolder" },
                { "Id": channel_view, "Name": "Recordings", "Type": "Channel" },
            ] }),
        );

        let mirror = TestMirror::new(client);
        sync_views(&mirror.state).await;
        mirror.state.writer.barrier().await;

        assert_eq!(
            view_item_type(&mirror, movies_view),
            Some("CollectionFolder".to_string())
        );
        assert_eq!(
            view_item_type(&mirror, channel_view),
            Some("Channel".to_string())
        );
    }

    /// Pins: `current_views` excludes `Channel` rows; every other view kind still comes through.
    #[tokio::test]
    async fn current_views_excludes_channel_rows() {
        let client = JellyfinClient::from_token("http://localhost:0", identity(), "tok");
        let movies_view = "11111111-1111-1111-1111-111111111111".to_string();
        let channel_view = "22222222-2222-2222-2222-222222222222".to_string();

        let mirror = TestMirror::new(client);
        mirror
            .state
            .writer
            .upsert_views(vec![
                crate::ViewRow {
                    id: movies_view.clone(),
                    name: "Movies".to_string(),
                    collection_type: "movies".to_string(),
                    item_type: "CollectionFolder".to_string(),
                },
                crate::ViewRow {
                    id: channel_view.clone(),
                    name: "Recordings".to_string(),
                    collection_type: String::new(),
                    item_type: "Channel".to_string(),
                },
            ])
            .await;
        mirror.state.writer.barrier().await;

        let views = current_views(&mirror.state).await;
        let ids: Vec<&String> = views.iter().map(|(id, _)| id).collect();
        assert!(
            ids.contains(&&movies_view),
            "non-channel views must still come through: {views:?}"
        );
        assert!(
            !ids.contains(&&channel_view),
            "a Channel view must never enter a per-library sync walk: {views:?}"
        );
    }

    /// Pins: `reconcile_all` never issues an `/Items` probe for a `Channel` view.
    #[tokio::test]
    async fn reconcile_all_never_probes_a_channel_view() {
        let server = MockServer::start().await;
        let client = JellyfinClient::from_token(&server.base_url, identity(), "tok");
        let movies_view = "11111111-1111-1111-1111-111111111111";
        let channel_view = "22222222-2222-2222-2222-222222222222";
        server.route(
            "/UserViews",
            &[],
            json!({ "Items": [
                { "Id": movies_view, "Name": "Movies", "CollectionType": "movies", "Type": "CollectionFolder" },
                { "Id": channel_view, "Name": "Recordings", "Type": "Channel" },
            ] }),
        );
        server.route(
            "/Items",
            &[
                ("parentId", movies_view),
                ("recursive", "true"),
                ("limit", "1"),
            ],
            json!({ "Items": [], "TotalRecordCount": 0 }),
        );

        let mirror = TestMirror::new(client);
        reconcile_all(&mirror.state).await;

        assert!(
            server.request_count_matching(&format!("parentId={movies_view}")) > 0,
            "a normal library must still be probed"
        );
        assert_eq!(
            server.request_count_matching(&format!("parentId={channel_view}")),
            0,
            "a Channel view must never be issued an /Items request by reconciliation"
        );
    }

    #[tokio::test]
    async fn initial_sync_pages_when_total_exceeds_page_size() {
        let server = MockServer::start().await;
        let client = JellyfinClient::from_token(&server.base_url, identity(), "tok");
        let view_id = "11111111-1111-1111-1111-111111111111";

        server.route(
            "/UserViews",
            &[],
            json!({ "Items": [{ "Id": view_id, "Name": "Movies", "CollectionType": "movies" }] }),
        );
        server.route("/UserItems/Resume", &[], json!({ "Items": [] }));
        server.route("/Shows/NextUp", &[], json!({ "Items": [] }));
        server.route(
            "/Items",
            &[("parentId", view_id), ("startIndex", "0")],
            json!({
                "Items": (0..PAGE_SIZE).map(|i| movie_json(&format!("00000000-0000-0000-0000-{i:012}"), &format!("Movie {i}"))).collect::<Vec<_>>(),
                "TotalRecordCount": PAGE_SIZE + 1
            }),
        );
        server.route(
            "/Items",
            &[
                ("parentId", view_id),
                ("startIndex", &PAGE_SIZE.to_string()),
            ],
            json!({
                "Items": [movie_json("99999999-9999-9999-9999-999999999999", "Last Movie")],
                "TotalRecordCount": PAGE_SIZE + 1
            }),
        );

        let mirror = TestMirror::new(client);
        initial_sync(&mirror.weak()).await;

        assert_eq!(mirror.item_count().await, (PAGE_SIZE + 1) as i64);
        assert_eq!(
            server.request_count("/Items?"),
            2,
            "must issue exactly two pages"
        );
    }

    #[tokio::test]
    async fn library_changed_added_fetches_and_upserts_ids() {
        let server = MockServer::start().await;
        let client = JellyfinClient::from_token(&server.base_url, identity(), "tok");
        let id = "22222222-2222-2222-2222-222222222222";
        server.route(
            "/Items",
            &[("ids", id)],
            json!({ "Items": [movie_json(id, "New Arrival")] }),
        );
        server.route("/Shows/NextUp", &[], json!({ "Items": [] }));

        let mirror = TestMirror::new(client);
        apply_server_event(
            &mirror.state,
            ServerEvent::LibraryChanged {
                added: vec![id.to_string()],
                updated: vec![],
                removed: vec![],
            },
        )
        .await;

        assert_eq!(mirror.item_count().await, 1);
    }

    fn collection_member_ids(mirror: &TestMirror, collection_id: &str) -> Vec<String> {
        let conn = mirror.state.read_pool.acquire();
        let mut stmt = conn
            .prepare(
                "SELECT item_id FROM collection_members WHERE collection_id = ?1 ORDER BY sort_index",
            )
            .expect("prepare");
        stmt.query_map([collection_id], |r| r.get(0))
            .expect("query")
            .collect::<rusqlite::Result<Vec<_>>>()
            .expect("rows")
    }

    /// Pins: syncing a `boxsets` view's breadth also triggers a per-BoxSet membership fetch.
    #[tokio::test]
    async fn initial_sync_of_a_boxsets_view_populates_membership() {
        let server = MockServer::start().await;
        let client = JellyfinClient::from_token(&server.base_url, identity(), "tok");
        let view_id = "11111111-1111-1111-1111-111111111111";
        let boxset_id = "22222222-2222-2222-2222-222222222222";
        let member_1 = "33333333-3333-3333-3333-333333333333";
        let member_2 = "44444444-4444-4444-4444-444444444444";

        server.route(
            "/UserViews",
            &[],
            json!({ "Items": [{ "Id": view_id, "Name": "Collections", "CollectionType": "boxsets" }] }),
        );
        server.route("/UserItems/Resume", &[], json!({ "Items": [] }));
        server.route("/Shows/NextUp", &[], json!({ "Items": [] }));
        server.route(
            "/Items",
            &[("parentId", view_id), ("startIndex", "0")],
            json!({
                "Items": [{ "Id": boxset_id, "Name": "Set A", "Type": "BoxSet" }],
                "TotalRecordCount": 1
            }),
        );
        server.route(
            "/Items",
            &[("parentId", boxset_id), ("recursive", "false")],
            json!({
                "Items": [
                    { "Id": member_1, "Name": "Member One", "Type": "Movie" },
                    { "Id": member_2, "Name": "Member Two", "Type": "Movie" },
                ]
            }),
        );

        let mirror = TestMirror::new(client);
        initial_sync(&mirror.weak()).await;
        mirror.state.writer.barrier().await;

        assert_eq!(
            collection_member_ids(&mirror, boxset_id),
            vec![member_1.to_string(), member_2.to_string()],
            "membership must be populated in server order"
        );
        assert_eq!(
            mirror.item_count().await,
            3,
            "boxset row + 2 members must all be present"
        );
    }

    /// Pins: a `LibraryChanged` touching a BoxSet re-fetches its membership, not just its own row.
    #[tokio::test]
    async fn library_changed_touching_a_boxset_refreshes_its_membership() {
        let server = MockServer::start().await;
        let client = JellyfinClient::from_token(&server.base_url, identity(), "tok");
        let boxset_id = "22222222-2222-2222-2222-222222222222";
        let member_id = "55555555-5555-5555-5555-555555555555";

        server.route(
            "/Items",
            &[("ids", boxset_id)],
            json!({ "Items": [{ "Id": boxset_id, "Name": "Set A", "Type": "BoxSet" }] }),
        );
        server.route(
            "/Items",
            &[("parentId", boxset_id), ("recursive", "false")],
            json!({ "Items": [{ "Id": member_id, "Name": "New Member", "Type": "Movie" }] }),
        );
        server.route("/Shows/NextUp", &[], json!({ "Items": [] }));

        let mirror = TestMirror::new(client);
        apply_server_event(
            &mirror.state,
            ServerEvent::LibraryChanged {
                added: vec![],
                updated: vec![boxset_id.to_string()],
                removed: vec![],
            },
        )
        .await;
        mirror.state.writer.barrier().await;

        assert_eq!(
            collection_member_ids(&mirror, boxset_id),
            vec![member_id.to_string()]
        );
    }

    #[tokio::test]
    async fn library_changed_removed_deletes_rows() {
        let server = MockServer::start().await;
        let client = JellyfinClient::from_token(&server.base_url, identity(), "tok");

        let mirror = TestMirror::new(client);
        let dto: BaseItemDto =
            serde_json::from_value(movie_json("22222222-2222-2222-2222-222222222222", "Doomed"))
                .expect("dto");
        mirror.state.writer.upsert_items(vec![dto]).await;
        assert_eq!(mirror.item_count().await, 1);

        apply_server_event(
            &mirror.state,
            ServerEvent::LibraryChanged {
                added: vec![],
                updated: vec![],
                removed: vec!["22222222-2222-2222-2222-222222222222".to_string()],
            },
        )
        .await;

        assert_eq!(mirror.item_count().await, 0);
    }

    #[tokio::test]
    async fn user_data_changed_updates_columns() {
        let server = MockServer::start().await;
        let client = JellyfinClient::from_token(&server.base_url, identity(), "tok");
        let mirror = TestMirror::new(client);

        let dto: BaseItemDto =
            serde_json::from_value(movie_json("22222222-2222-2222-2222-222222222222", "Movie"))
                .expect("dto");
        mirror.state.writer.upsert_items(vec![dto]).await;

        let user_data: jellyfin_api::models::UserItemDataDto = serde_json::from_value(json!({
            "Key": "k", "Played": true, "PlaybackPositionTicks": 12345
        }))
        .expect("user data");
        apply_server_event(
            &mirror.state,
            ServerEvent::UserDataChanged {
                item_userdata: vec![(
                    "22222222-2222-2222-2222-222222222222".to_string(),
                    user_data,
                )],
            },
        )
        .await;

        mirror.state.writer.barrier().await;
        let conn = mirror.state.read_pool.acquire();
        let (played, pos): (bool, i64) = conn
            .query_row(
                "SELECT played, playback_position_ticks FROM items WHERE id = '22222222-2222-2222-2222-222222222222'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .expect("row");
        assert!(played);
        assert_eq!(pos, 12345);
    }

    /// Pins: the catch-up diff is the symmetric difference of both user-data sets, sorted and deduplicated.
    #[test]
    fn user_data_changed_ids_is_the_symmetric_difference_of_both_sets() {
        let set = |ids: &[&str]| -> std::collections::HashSet<String> {
            ids.iter().map(|s| s.to_string()).collect()
        };
        let changed = user_data_changed_ids(
            &set(&["a", "b", "x"]),
            &set(&["c", "x"]),
            &set(&["b", "d"]),
            &set(&["c", "e", "x"]),
        );
        assert_eq!(changed, vec!["a", "d", "e", "x"]);
        assert!(
            user_data_changed_ids(&set(&["a"]), &set(&["b"]), &set(&["a"]), &set(&["b"]))
                .is_empty()
        );
    }

    /// Pins: an episode marked played elsewhere is refetched by the catch-up, and its series row is refreshed with it.
    #[tokio::test]
    async fn watched_state_catch_up_refetches_rows_the_server_marks_played_and_their_series() {
        let server = MockServer::start().await;
        let client = JellyfinClient::from_token(&server.base_url, identity(), "tok");
        let mirror = TestMirror::new(client);
        let ep = "22222222-2222-2222-2222-222222222222";
        let series = "33333333-3333-3333-3333-333333333333";
        let season = "44444444-4444-4444-4444-444444444444";
        let episode_json = |played: bool| {
            json!({
                "Id": ep, "Name": "Pilot", "Type": "Episode",
                "SeriesId": series, "SeasonId": season, "ParentId": season,
                "UserData": { "Key": "k", "Played": played, "PlaybackPositionTicks": 0 }
            })
        };
        let series_json = |name: &str| json!({ "Id": series, "Name": name, "Type": "Series" });
        let season_json =
            json!({ "Id": season, "Name": "Season 1", "Type": "Season", "SeriesId": series });

        let seed: Vec<BaseItemDto> = [
            episode_json(false),
            series_json("Old Name"),
            season_json.clone(),
        ]
        .into_iter()
        .map(|v| serde_json::from_value(v).expect("dto"))
        .collect();
        mirror.state.writer.upsert_items(seed).await;

        server.route(
            "/Items",
            &[("filters", "IsPlayed")],
            json!({ "Items": [{ "Id": ep }], "TotalRecordCount": 1 }),
        );
        server.route(
            "/Items",
            &[("filters", "IsResumable")],
            json!({ "Items": [], "TotalRecordCount": 0 }),
        );
        route_by_ids(&server, &[episode_json(true)]);
        route_by_ids(&server, &[series_json("New Name"), season_json]);

        user_data_sync(&mirror.state).await;

        mirror.state.writer.barrier().await;
        let conn = mirror.state.read_pool.acquire();
        let played: bool = conn
            .query_row("SELECT played FROM items WHERE id = ?1", [ep], |r| r.get(0))
            .expect("episode row");
        drop(conn);
        assert!(played, "episode should now be played");
        assert_eq!(mirror.item_name(series).await.as_deref(), Some("New Name"));
        assert_eq!(server.request_count_matching("filters=IsPlayed"), 1);
        assert_eq!(server.request_count_matching("filters=IsResumable"), 1);
    }

    /// Pins: a WS delta buffered during initial sync replays after and wins over a stale in-flight page snapshot.
    #[tokio::test]
    async fn ws_delta_buffered_during_initial_sync_replays_and_wins_over_stale_snapshot() {
        use rusqlite::OptionalExtension;
        use std::time::Duration;

        let server = MockServer::start().await;
        let client = JellyfinClient::from_token(&server.base_url, identity(), "tok");
        let view_id = "11111111-1111-1111-1111-111111111111";
        let item_id = "22222222-2222-2222-2222-222222222222";

        server.route(
            "/UserViews",
            &[],
            json!({ "Items": [{ "Id": view_id, "Name": "Movies", "CollectionType": "movies" }] }),
        );
        server.route("/UserItems/Resume", &[], json!({ "Items": [] }));
        server.route("/Shows/NextUp", &[], json!({ "Items": [] }));
        // The breadth-sync page for this item is slow; the "server
        // snapshot" it carries is already stale by the time it lands (the
        // WS delta below reports a newer playback state for the same item).
        server.route_delayed(
            "/Items",
            &[("parentId", view_id), ("startIndex", "0")],
            json!({
                "Items": [{
                    "Id": item_id, "Name": "Old Snapshot", "Type": "Movie",
                    "UserData": { "Key": "k", "Played": false, "PlaybackPositionTicks": 0 }
                }],
                "TotalRecordCount": 1
            }),
            Duration::from_millis(250),
        );

        let mirror = TestMirror::new(client);
        let (bus_tx, bus_rx) = broadcast::channel::<BusEvent>(16);
        tokio::spawn(bus_listener(mirror.weak(), bus_rx));
        tokio::spawn({
            let weak = mirror.weak();
            async move { initial_sync(&weak).await }
        });

        // Give initial_sync a moment to flip the in-progress flag and start
        // the (delayed) page fetch, then confirm it's actually still in
        // flight -- otherwise this test would pass vacuously.
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            mirror
                .state
                .initial_sync_in_progress
                .load(Ordering::Acquire),
            "sanity: initial sync should still be running (page fetch delayed 250ms)"
        );

        let user_data: jellyfin_api::models::UserItemDataDto = serde_json::from_value(json!({
            "Key": "k", "Played": true, "PlaybackPositionTicks": 99999
        }))
        .expect("user data");
        bus_tx
            .send(BusEvent::Server(ServerEvent::UserDataChanged {
                item_userdata: vec![(item_id.to_string(), user_data)],
            }))
            .expect("send bus event");

        // Poll for the WS delta's value: the delayed page snapshot commits
        // first (played=false), then the buffered delta must replay on top
        // of it once initial sync completes (played=true).
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            mirror.state.writer.barrier().await;
            let conn = mirror.state.read_pool.acquire();
            let row: Option<(bool, i64)> = conn
                .query_row(
                    "SELECT played, playback_position_ticks FROM items WHERE id = ?1",
                    [item_id],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()
                .expect("query");
            drop(conn);
            if row == Some((true, 99999)) {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "timed out waiting for the buffered WS delta to replay; last row: {row:?}"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        assert!(
            !mirror
                .state
                .initial_sync_in_progress
                .load(Ordering::Acquire),
            "initial sync should have completed by the time the replay landed"
        );
    }

    #[test]
    fn needs_resync_on_count_mismatch() {
        assert!(needs_resync(1, 2, None, None));
        assert!(!needs_resync(2, 2, None, None));
    }

    #[test]
    fn needs_resync_on_stale_newest_date() {
        assert!(needs_resync(
            2,
            2,
            Some("2024-01-01T00:00:00Z"),
            Some("2024-06-01T00:00:00Z")
        ));
        assert!(!needs_resync(
            2,
            2,
            Some("2024-06-01T00:00:00Z"),
            Some("2024-06-01T00:00:00Z")
        ));
    }

    #[test]
    fn needs_resync_when_local_has_no_items_but_server_reports_a_date() {
        assert!(needs_resync(0, 0, None, Some("2024-06-01T00:00:00Z")));
    }

    /// Pins: a reconcile mismatch's ID sweep fetches only the missing id, never re-downloading what the mirror already has.
    #[tokio::test]
    async fn reconciliation_mismatch_sweep_fetches_only_the_missing_ids() {
        let server = MockServer::start().await;
        let client = JellyfinClient::from_token(&server.base_url, identity(), "tok");
        let view_id = "11111111-1111-1111-1111-111111111111";
        let have = "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa";
        let missing = "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb";

        server.route(
            "/UserViews",
            &[],
            json!({ "Items": [{ "Id": view_id, "Name": "Movies", "CollectionType": "movies" }] }),
        );
        server.route(
            "/Items",
            &[("parentId", view_id), ("limit", "1")],
            json!({ "Items": [movie_json(have, "A")], "TotalRecordCount": 2 }),
        );
        route_sweep_enumeration(&server, view_id, &[have, missing]);
        route_by_ids(&server, &[movie_json(missing, "B")]);

        let mirror = TestMirror::new(client);
        mirror
            .state
            .writer
            .upsert_views(vec![crate::ViewRow {
                id: view_id.to_string(),
                name: "Movies".to_string(),
                collection_type: "movies".to_string(),
                item_type: "CollectionFolder".to_string(),
            }])
            .await;
        let seed: BaseItemDto = serde_json::from_value(movie_json(have, "A")).expect("dto");
        mirror
            .state
            .writer
            .upsert_items_scoped(vec![seed], Some(view_id.to_string()))
            .await;
        assert_eq!(mirror.item_count().await, 1);

        reconcile_all(&mirror.state).await;

        assert_eq!(
            mirror.item_count().await,
            2,
            "the sweep must fill in the missing item"
        );
        assert_eq!(
            mirror.item_name(missing).await.as_deref(),
            Some("B"),
            "the missing id must land as a real, fully-fetched row"
        );
        assert_eq!(
            server.request_count_matching(BY_IDS_MARKER),
            1,
            "exactly one by-ids repair fetch, covering only the missing id"
        );
        assert_eq!(
            server.request_count_matching(&format!("{BY_IDS_MARKER}{missing}")),
            1,
            "the repair fetch must ask for the missing id and nothing else"
        );
        assert_eq!(
            server.request_count_matching(&format!("limit={PAGE_SIZE}")),
            0,
            "no full breadth page may be requested any more"
        );
    }

    /// Pins: a reconcile mismatch converges a virtual-placeholder-to-real-episode id swap, pruning the dead virtual row.
    #[tokio::test]
    async fn reconcile_converges_a_virtual_to_real_episode_id_swap() {
        let server = MockServer::start().await;
        let client = JellyfinClient::from_token(&server.base_url, identity(), "tok");
        let view_id = "11111111-1111-1111-1111-111111111111";
        let old_virtual_id = "22222222-2222-2222-2222-222222222222";
        let new_real_id = "33333333-3333-3333-3333-333333333333";

        server.route(
            "/UserViews",
            &[],
            json!({ "Items": [{
                "Id": view_id, "Name": "Shows", "CollectionType": "tvshows",
                "DateLastMediaAdded": "2024-06-01T00:00:00Z"
            }] }),
        );
        server.route(
            "/Items",
            &[("parentId", view_id), ("limit", "1")],
            json!({ "Items": [], "TotalRecordCount": 1 }),
        );
        route_sweep_enumeration(&server, view_id, &[new_real_id]);
        route_by_ids(
            &server,
            &[json!({
                "Id": new_real_id, "Name": "S01E01", "Type": "Episode",
                "LocationType": "FileSystem"
            })],
        );

        let mirror = TestMirror::new(client);
        mirror
            .state
            .writer
            .upsert_views(vec![crate::ViewRow {
                id: view_id.to_string(),
                name: "Shows".to_string(),
                collection_type: "tvshows".to_string(),
                item_type: "CollectionFolder".to_string(),
            }])
            .await;
        let virtual_dto: BaseItemDto = serde_json::from_value(json!({
            "Id": old_virtual_id, "Name": "S01E01", "Type": "Episode",
            "LocationType": "Virtual"
        }))
        .expect("dto");
        mirror
            .state
            .writer
            .upsert_items_scoped(vec![virtual_dto], Some(view_id.to_string()))
            .await;
        assert_eq!(mirror.item_count().await, 1);

        reconcile_all(&mirror.state).await;
        mirror.state.writer.barrier().await;

        let conn = mirror.state.read_pool.acquire();
        let rows: Vec<(String, i64)> = {
            let mut stmt = conn
                .prepare("SELECT id, is_virtual FROM items ORDER BY id")
                .expect("prepare");
            stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
                .expect("query")
                .collect::<rusqlite::Result<_>>()
                .expect("rows")
        };
        assert_eq!(
            rows,
            vec![(new_real_id.to_string(), 0)],
            "old virtual row must be pruned; only the new real row (is_virtual=0) must remain"
        );
    }

    /// Pins: the newest-ids probe catches a count-neutral virtual-to-real swap that count and date probes are both blind to.
    #[tokio::test]
    async fn reconcile_newest_ids_probe_catches_count_neutral_swap_with_sentinel_date() {
        let server = MockServer::start().await;
        let client = JellyfinClient::from_token(&server.base_url, identity(), "tok");
        let view_id = "11111111-1111-1111-1111-111111111111";
        let old_virtual_id = "22222222-2222-2222-2222-222222222222";
        let new_real_id = "33333333-3333-3333-3333-333333333333";

        server.route(
            "/UserViews",
            &[],
            json!({ "Items": [{
                "Id": view_id, "Name": "Shows", "CollectionType": "tvshows",
                "DateLastMediaAdded": "0001-01-01T00:00:00Z"
            }] }),
        );
        // Count probe: server total == local total (the swap is
        // count-neutral by construction).
        server.route(
            "/Items",
            &[("parentId", view_id), ("limit", "1")],
            json!({ "Items": [], "TotalRecordCount": 1 }),
        );
        // Newest-ids probe (limit=20, DateCreated desc): the server's
        // newest item is the new real id -- absent locally.
        server.route(
            "/Items",
            &[("parentId", view_id), ("limit", "20")],
            json!({
                "Items": [{
                    "Id": new_real_id, "Name": "S01E01", "Type": "Episode",
                    "LocationType": "FileSystem"
                }],
                "TotalRecordCount": 1
            }),
        );
        // The triggered ID sweep: enumeration (new real id only) + by-ids
        // repair fetch for it.
        route_sweep_enumeration(&server, view_id, &[new_real_id]);
        route_by_ids(
            &server,
            &[json!({
                "Id": new_real_id, "Name": "S01E01", "Type": "Episode",
                "LocationType": "FileSystem"
            })],
        );

        let mirror = TestMirror::new(client);
        mirror
            .state
            .writer
            .upsert_views(vec![crate::ViewRow {
                id: view_id.to_string(),
                name: "Shows".to_string(),
                collection_type: "tvshows".to_string(),
                item_type: "CollectionFolder".to_string(),
            }])
            .await;
        // Seed the dead placeholder WITH a date_created -- that's what
        // makes the date probe's `(Some, None)` combination inert, per the
        // live mirror's own shape.
        let virtual_dto: BaseItemDto = serde_json::from_value(json!({
            "Id": old_virtual_id, "Name": "S01E01", "Type": "Episode",
            "LocationType": "Virtual", "DateCreated": "2026-08-12T07:37:25Z"
        }))
        .expect("dto");
        mirror
            .state
            .writer
            .upsert_items_scoped(vec![virtual_dto], Some(view_id.to_string()))
            .await;
        assert_eq!(mirror.item_count().await, 1);

        reconcile_all(&mirror.state).await;
        mirror.state.writer.barrier().await;

        let conn = mirror.state.read_pool.acquire();
        let rows: Vec<(String, i64)> = {
            let mut stmt = conn
                .prepare("SELECT id, is_virtual FROM items ORDER BY id")
                .expect("prepare");
            stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
                .expect("query")
                .collect::<rusqlite::Result<_>>()
                .expect("rows")
        };
        assert_eq!(
            rows,
            vec![(new_real_id.to_string(), 0)],
            "newest-ids probe must trigger the resync that lands the real \
             row and prunes the placeholder, with both cheap probes blind"
        );
    }

    /// Pins: a resync-triggered prune is scoped strictly to the library being resynced; an unrelated library's rows survive untouched.
    #[tokio::test]
    async fn reconcile_resync_prunes_only_the_mismatched_library() {
        let server = MockServer::start().await;
        let client = JellyfinClient::from_token(&server.base_url, identity(), "tok");
        let movies_view = "11111111-1111-1111-1111-111111111111";
        let shows_view = "44444444-4444-4444-4444-444444444444";
        let orphan_movie = "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa";
        let other_library_movie = "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb";

        server.route(
            "/UserViews",
            &[],
            json!({ "Items": [
                { "Id": movies_view, "Name": "Movies", "CollectionType": "movies" },
                { "Id": shows_view, "Name": "Shows", "CollectionType": "tvshows" },
            ] }),
        );
        // Movies: server now reports 0 items (the locally-seeded movie is an
        // orphan -- deleted server-side, nothing replaces it).
        server.route(
            "/Items",
            &[("parentId", movies_view), ("limit", "1")],
            json!({ "Items": [], "TotalRecordCount": 0 }),
        );
        route_sweep_enumeration(&server, movies_view, &[]);
        // Shows: in sync (count matches, newest ids all present locally), so
        // it must never sweep at all -- no enumeration route registered for
        // it.
        server.route(
            "/Items",
            &[("parentId", shows_view), ("limit", "1")],
            json!({ "Items": [], "TotalRecordCount": 1 }),
        );
        server.route(
            "/Items",
            &[("parentId", shows_view), ("limit", "20")],
            json!({ "Items": [{ "Id": other_library_movie }], "TotalRecordCount": 1 }),
        );

        let mirror = TestMirror::new(client);
        mirror
            .state
            .writer
            .upsert_views(vec![
                crate::ViewRow {
                    id: movies_view.to_string(),
                    name: "Movies".to_string(),
                    collection_type: "movies".to_string(),
                    item_type: "CollectionFolder".to_string(),
                },
                crate::ViewRow {
                    id: shows_view.to_string(),
                    name: "Shows".to_string(),
                    collection_type: "tvshows".to_string(),
                    item_type: "CollectionFolder".to_string(),
                },
            ])
            .await;
        let orphan: BaseItemDto =
            serde_json::from_value(movie_json(orphan_movie, "Orphan")).expect("dto");
        mirror
            .state
            .writer
            .upsert_items_scoped(vec![orphan], Some(movies_view.to_string()))
            .await;
        // A different library's row, deliberately left out of every route
        // above -- if pruning ever escaped its own library scope this would
        // get deleted too.
        let other: BaseItemDto =
            serde_json::from_value(movie_json(other_library_movie, "Untouched")).expect("dto");
        mirror
            .state
            .writer
            .upsert_items_scoped(vec![other], Some(shows_view.to_string()))
            .await;
        assert_eq!(mirror.item_count().await, 2);

        reconcile_all(&mirror.state).await;
        mirror.state.writer.barrier().await;

        let conn = mirror.state.read_pool.acquire();
        let ids: Vec<String> = {
            let mut stmt = conn.prepare("SELECT id FROM items").expect("prepare");
            stmt.query_map([], |r| r.get(0))
                .expect("query")
                .collect::<rusqlite::Result<_>>()
                .expect("rows")
        };
        assert_eq!(
            ids,
            vec![other_library_movie.to_string()],
            "the Movies orphan must be pruned but Shows' row must survive untouched"
        );
        // The field scenario this whole change exists for: a deletion-only
        // mismatch must cost ids-pages and nothing else -- no full DTOs at
        // all, in either direction.
        assert_eq!(
            server.request_count_matching(FULL_DTO_MARKER),
            0,
            "a deletion-only mismatch must not download a single full DTO"
        );
        assert_eq!(
            server.request_count_matching(BY_IDS_MARKER),
            0,
            "nothing is missing locally, so there is nothing to repair-fetch"
        );
        assert_eq!(
            server.request_count_matching(SWEEP_PAGE_MARKER),
            1,
            "exactly one ids-only enumeration page, for the drifted library only"
        );
    }

    /// Pins: a deletion-only mismatch removes exactly the deleted ids and issues no full-DTO request at all.
    #[tokio::test]
    async fn reconcile_sweep_removes_only_the_deleted_ids_without_fetching_any_dtos() {
        let server = MockServer::start().await;
        let client = JellyfinClient::from_token(&server.base_url, identity(), "tok");
        let view_id = "11111111-1111-1111-1111-111111111111";

        let all: Vec<String> = (0..12)
            .map(|i| format!("00000000-0000-0000-0000-{i:012}"))
            .collect();
        // The server dropped the last 3 (file upgrades: path-derived ids, so
        // a replaced file is a delete + an add -- here just the delete half,
        // the adds having already landed via delta sync).
        let survivors: Vec<&str> = all[..9].iter().map(String::as_str).collect();

        server.route(
            "/UserViews",
            &[],
            json!({ "Items": [{ "Id": view_id, "Name": "Shows", "CollectionType": "tvshows" }] }),
        );
        server.route(
            "/Items",
            &[("parentId", view_id), ("limit", "1")],
            json!({ "Items": [], "TotalRecordCount": survivors.len() }),
        );
        route_sweep_enumeration(&server, view_id, &survivors);

        let mirror = TestMirror::new(client);
        mirror
            .state
            .writer
            .upsert_views(vec![crate::ViewRow {
                id: view_id.to_string(),
                name: "Shows".to_string(),
                collection_type: "tvshows".to_string(),
                item_type: "CollectionFolder".to_string(),
            }])
            .await;
        let seeded: Vec<BaseItemDto> = all
            .iter()
            .map(|id| {
                serde_json::from_value(json!({
                    "Id": id, "Name": "Episode", "Type": "Episode",
                    "LocationType": "FileSystem"
                }))
                .expect("dto")
            })
            .collect();
        mirror
            .state
            .writer
            .upsert_items_scoped(seeded, Some(view_id.to_string()))
            .await;
        assert_eq!(mirror.item_count().await, 12);

        reconcile_all(&mirror.state).await;
        mirror.state.writer.barrier().await;

        let conn = mirror.state.read_pool.acquire();
        let remaining: Vec<String> = {
            let mut stmt = conn
                .prepare("SELECT id FROM items ORDER BY id")
                .expect("prepare");
            stmt.query_map([], |r| r.get(0))
                .expect("query")
                .collect::<rusqlite::Result<_>>()
                .expect("rows")
        };
        drop(conn);
        assert_eq!(
            remaining,
            survivors.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
            "exactly the 3 server-side deletions must be shed, nothing else"
        );
        assert_eq!(
            server.request_count_matching(FULL_DTO_MARKER),
            0,
            "a deletion-only mismatch must cost zero full DTOs"
        );
        assert_eq!(
            server.request_count_matching(BY_IDS_MARKER),
            0,
            "and zero by-ids repair fetches"
        );
        assert_eq!(
            server.request_count_matching(SWEEP_PAGE_MARKER),
            1,
            "one ids-only page covers a library this size"
        );

        // Convergence: a second reconcile pass must now find the library in
        // agreement and sweep nothing at all.
        let sweep_pages_after_first = server.request_count_matching(SWEEP_PAGE_MARKER);
        server.route(
            "/Items",
            &[("parentId", view_id), ("limit", "20")],
            json!({
                "Items": survivors.iter().rev().map(|id| json!({ "Id": id })).collect::<Vec<_>>(),
                "TotalRecordCount": survivors.len(),
            }),
        );
        reconcile_all(&mirror.state).await;
        assert_eq!(
            server.request_count_matching(SWEEP_PAGE_MARKER),
            sweep_pages_after_first,
            "the counts converged, so the next pass must not sweep again"
        );
    }

    /// Pins: the sweep prunes a dropped id and fetches a gained id in the same pass.
    #[tokio::test]
    async fn reconcile_sweep_handles_orphans_and_missing_ids_in_one_pass() {
        let server = MockServer::start().await;
        let client = JellyfinClient::from_token(&server.base_url, identity(), "tok");
        let view_id = "11111111-1111-1111-1111-111111111111";
        let kept = "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa";
        let orphan = "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb";
        let arrival = "cccccccc-cccc-cccc-cccc-cccccccccccc";

        server.route(
            "/UserViews",
            &[],
            json!({ "Items": [{ "Id": view_id, "Name": "Movies", "CollectionType": "movies" }] }),
        );
        // Count-neutral drift (one out, one in) -- caught by the newest-ids
        // probe, exactly like a virtual-to-real swap.
        server.route(
            "/Items",
            &[("parentId", view_id), ("limit", "1")],
            json!({ "Items": [], "TotalRecordCount": 2 }),
        );
        server.route(
            "/Items",
            &[("parentId", view_id), ("limit", "20")],
            json!({ "Items": [{ "Id": arrival }, { "Id": kept }], "TotalRecordCount": 2 }),
        );
        route_sweep_enumeration(&server, view_id, &[kept, arrival]);
        route_by_ids(&server, &[movie_json(arrival, "Brand New")]);

        let mirror = TestMirror::new(client);
        mirror
            .state
            .writer
            .upsert_views(vec![crate::ViewRow {
                id: view_id.to_string(),
                name: "Movies".to_string(),
                collection_type: "movies".to_string(),
                item_type: "CollectionFolder".to_string(),
            }])
            .await;
        let seeded: Vec<BaseItemDto> = [kept, orphan]
            .iter()
            .map(|id| serde_json::from_value(movie_json(id, "Seeded")).expect("dto"))
            .collect();
        mirror
            .state
            .writer
            .upsert_items_scoped(seeded, Some(view_id.to_string()))
            .await;
        assert_eq!(mirror.item_count().await, 2);

        reconcile_all(&mirror.state).await;
        mirror.state.writer.barrier().await;

        let conn = mirror.state.read_pool.acquire();
        let remaining: Vec<String> = {
            let mut stmt = conn
                .prepare("SELECT id FROM items ORDER BY id")
                .expect("prepare");
            stmt.query_map([], |r| r.get(0))
                .expect("query")
                .collect::<rusqlite::Result<_>>()
                .expect("rows")
        };
        drop(conn);
        assert_eq!(
            remaining,
            vec![kept.to_string(), arrival.to_string()],
            "the orphan must go, the arrival must land, the untouched row must stay"
        );
        assert_eq!(
            server.request_count_matching(BY_IDS_MARKER),
            1,
            "one repair fetch, covering only the arrival"
        );
        assert_eq!(
            mirror.item_name(kept).await.as_deref(),
            Some("Seeded"),
            "an id present on both sides must not be re-fetched or rewritten"
        );
    }

    /// Pins: a sweep that fails partway leaves the mirror unchanged and doesn't fall back to a full breadth walk; the mismatch stays retryable.
    #[tokio::test]
    async fn reconcile_sweep_failure_keeps_prior_state_and_stays_retryable() {
        let server = MockServer::start().await;
        let client = JellyfinClient::from_token(&server.base_url, identity(), "tok");
        let view_id = "11111111-1111-1111-1111-111111111111";
        let orphan = "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa";
        let arrival = "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb";

        server.route(
            "/UserViews",
            &[],
            json!({ "Items": [{ "Id": view_id, "Name": "Movies", "CollectionType": "movies" }] }),
        );
        server.route(
            "/Items",
            &[("parentId", view_id), ("limit", "1")],
            json!({ "Items": [], "TotalRecordCount": 1 }),
        );
        server.route(
            "/Items",
            &[("parentId", view_id), ("limit", "20")],
            json!({ "Items": [{ "Id": arrival }], "TotalRecordCount": 1 }),
        );
        route_sweep_enumeration(&server, view_id, &[arrival]);
        // Deliberately NO by-ids route yet: the repair fetch 404s.

        let mirror = TestMirror::new(client);
        mirror
            .state
            .writer
            .upsert_views(vec![crate::ViewRow {
                id: view_id.to_string(),
                name: "Movies".to_string(),
                collection_type: "movies".to_string(),
                item_type: "CollectionFolder".to_string(),
            }])
            .await;
        let seed: BaseItemDto = serde_json::from_value(movie_json(orphan, "Doomed")).expect("dto");
        mirror
            .state
            .writer
            .upsert_items_scoped(vec![seed], Some(view_id.to_string()))
            .await;

        reconcile_all(&mirror.state).await;
        mirror.state.writer.barrier().await;

        let conn = mirror.state.read_pool.acquire();
        let remaining: Vec<String> = {
            let mut stmt = conn
                .prepare("SELECT id FROM items ORDER BY id")
                .expect("prepare");
            stmt.query_map([], |r| r.get(0))
                .expect("query")
                .collect::<rusqlite::Result<_>>()
                .expect("rows")
        };
        drop(conn);
        assert_eq!(
            remaining,
            vec![orphan.to_string()],
            "a failed sweep must delete nothing -- the last-good mirror state stands"
        );
        assert_eq!(
            server.request_count_matching(&format!("limit={PAGE_SIZE}")),
            0,
            "a failed sweep must NOT fall back to the full breadth walk"
        );

        // The mismatch is still standing, so the next reconcile tick retries
        // -- and with the repair fetch now answerable, converges.
        route_by_ids(&server, &[movie_json(arrival, "Arrived")]);
        reconcile_all(&mirror.state).await;
        mirror.state.writer.barrier().await;

        let conn = mirror.state.read_pool.acquire();
        let remaining: Vec<String> = {
            let mut stmt = conn
                .prepare("SELECT id FROM items ORDER BY id")
                .expect("prepare");
            stmt.query_map([], |r| r.get(0))
                .expect("query")
                .collect::<rusqlite::Result<_>>()
                .expect("rows")
        };
        drop(conn);
        assert_eq!(
            remaining,
            vec![arrival.to_string()],
            "the retry must converge: orphan pruned, arrival fetched"
        );
    }

    /// Pins: a sweep of the collections library doesn't prune a BoxSet member that lives in another library (or has no `library_id`).
    #[tokio::test]
    async fn reconcile_sweep_does_not_delete_items_that_live_in_another_library() {
        let server = MockServer::start().await;
        let client = JellyfinClient::from_token(&server.base_url, identity(), "tok");
        let boxsets_view = "11111111-1111-1111-1111-111111111111";
        let movies_view = "22222222-2222-2222-2222-222222222222";
        let live_boxset = "33333333-3333-3333-3333-333333333333";
        let dead_boxset = "44444444-4444-4444-4444-444444444444";
        let shared_member = "55555555-5555-5555-5555-555555555555";

        server.route(
            "/UserViews",
            &[],
            json!({ "Items": [
                { "Id": boxsets_view, "Name": "Collections", "CollectionType": "boxsets" },
            ] }),
        );
        // One of the two locally-known BoxSets is gone server-side.
        server.route(
            "/Items",
            &[("parentId", boxsets_view), ("limit", "1")],
            json!({ "Items": [], "TotalRecordCount": 1 }),
        );
        route_sweep_enumeration(&server, boxsets_view, &[live_boxset]);
        // Post-sweep membership refresh for the surviving BoxSet.
        server.route(
            "/Items",
            &[("parentId", live_boxset), ("recursive", "false")],
            json!({ "Items": [{ "Id": shared_member, "Name": "Shared", "Type": "Movie" }] }),
        );

        let mirror = TestMirror::new(client);
        mirror
            .state
            .writer
            .upsert_views(vec![crate::ViewRow {
                id: boxsets_view.to_string(),
                name: "Collections".to_string(),
                collection_type: "boxsets".to_string(),
                item_type: "CollectionFolder".to_string(),
            }])
            .await;
        let boxsets: Vec<BaseItemDto> = [live_boxset, dead_boxset]
            .iter()
            .map(|id| {
                serde_json::from_value(json!({ "Id": id, "Name": "Set", "Type": "BoxSet" }))
                    .expect("dto")
            })
            .collect();
        mirror
            .state
            .writer
            .upsert_items_scoped(boxsets, Some(boxsets_view.to_string()))
            .await;
        // The member itself lives in the MOVIES library -- it is a member of
        // a collection in the boxsets library, but it is not stamped with it.
        let member: BaseItemDto =
            serde_json::from_value(movie_json(shared_member, "Shared")).expect("dto");
        mirror
            .state
            .writer
            .upsert_items_scoped(vec![member], Some(movies_view.to_string()))
            .await;
        mirror
            .state
            .writer
            .set_collection_members(
                live_boxset.to_string(),
                vec![(shared_member.to_string(), 0)],
            )
            .await;
        mirror
            .state
            .writer
            .set_collection_members(
                dead_boxset.to_string(),
                vec![(shared_member.to_string(), 0)],
            )
            .await;
        assert_eq!(mirror.item_count().await, 3);

        reconcile_all(&mirror.state).await;
        mirror.state.writer.barrier().await;

        let conn = mirror.state.read_pool.acquire();
        let remaining: Vec<String> = {
            let mut stmt = conn
                .prepare("SELECT id FROM items ORDER BY id")
                .expect("prepare");
            stmt.query_map([], |r| r.get(0))
                .expect("query")
                .collect::<rusqlite::Result<_>>()
                .expect("rows")
        };
        drop(conn);
        assert_eq!(
            remaining,
            vec![live_boxset.to_string(), shared_member.to_string()],
            "only the dead BoxSet may be pruned -- its member lives in another library and \
             must survive"
        );
        assert_eq!(
            collection_member_ids(&mirror, live_boxset),
            vec![shared_member.to_string()],
            "the surviving collection keeps its membership (and the sweep still runs the \
             post-resync membership refresh a boxsets library needs)"
        );
        assert!(
            collection_member_ids(&mirror, dead_boxset).is_empty(),
            "the pruned BoxSet's own membership rows must go with it"
        );
    }

    /// Pins: the sweep keeps the breadth walk's observability contracts -- `is_syncing()`, `SyncActivity::Syncing`/`Idle`, in-flight counter.
    #[tokio::test]
    async fn reconcile_sweep_reports_syncing_then_idle_and_counts_as_syncing() {
        use std::time::Duration;

        let server = MockServer::start().await;
        let client = JellyfinClient::from_token(&server.base_url, identity(), "tok");
        let view_id = "11111111-1111-1111-1111-111111111111";
        let orphan = "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa";

        server.route(
            "/UserViews",
            &[],
            json!({ "Items": [{ "Id": view_id, "Name": "Movies", "CollectionType": "movies" }] }),
        );
        server.route(
            "/Items",
            &[("parentId", view_id), ("limit", "1")],
            json!({ "Items": [], "TotalRecordCount": 0 }),
        );
        // Slow enumeration page, so the Syncing state is observable rather
        // than raced.
        server.route_delayed(
            "/Items",
            &[
                ("parentId", view_id),
                ("limit", &ID_SWEEP_PAGE_SIZE.to_string()),
            ],
            json!({ "Items": [], "TotalRecordCount": 0 }),
            Duration::from_millis(150),
        );

        let mirror = TestMirror::new(client);
        mirror
            .state
            .writer
            .upsert_views(vec![crate::ViewRow {
                id: view_id.to_string(),
                name: "Movies".to_string(),
                collection_type: "movies".to_string(),
                item_type: "CollectionFolder".to_string(),
            }])
            .await;
        let seed: BaseItemDto = serde_json::from_value(movie_json(orphan, "Doomed")).expect("dto");
        mirror
            .state
            .writer
            .upsert_items_scoped(vec![seed], Some(view_id.to_string()))
            .await;
        mirror.state.writer.barrier().await;

        let mut activity_rx = mirror.state.sync_activity.subscribe();
        assert_eq!(*activity_rx.borrow(), SyncActivity::Idle);

        let state = mirror.state.clone();
        let handle = tokio::spawn(async move { reconcile_all(&state).await });

        activity_rx
            .changed()
            .await
            .expect("activity changed to Syncing");
        assert_eq!(
            *activity_rx.borrow(),
            SyncActivity::Syncing {
                library_name_or_id: view_id.to_string(),
                pages_done: 0,
                items_done: 0,
                total_items: None,
            },
            "the sweep must report progress under the drifted library's id"
        );
        assert!(
            mirror.state.breadth_syncs_in_flight.load(Ordering::Acquire) > 0,
            "is_syncing() must cover the sweep while it runs"
        );

        handle.await.expect("reconcile task");
        activity_rx
            .changed()
            .await
            .expect("activity changed back to Idle");
        assert_eq!(*activity_rx.borrow(), SyncActivity::Idle);
        assert_eq!(
            mirror.state.breadth_syncs_in_flight.load(Ordering::Acquire),
            0,
            "the in-flight counter must be back to zero before the terminal Idle is observable"
        );
    }

    /// Pins: same playback-yield contract as `breadth_sync_yields_while_playback_is_active`, for the sweep.
    #[tokio::test]
    async fn reconcile_sweep_yields_while_playback_is_active() {
        let server = MockServer::start().await;
        let client = JellyfinClient::from_token(&server.base_url, identity(), "tok");
        let view_id = "11111111-1111-1111-1111-111111111111";
        let orphan = "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa";

        route_sweep_enumeration(&server, view_id, &[]);

        let mirror = TestMirror::new(client);
        let seed: BaseItemDto = serde_json::from_value(movie_json(orphan, "Doomed")).expect("dto");
        mirror
            .state
            .writer
            .upsert_items_scoped(vec![seed], Some(view_id.to_string()))
            .await;
        mirror.state.writer.barrier().await;
        mirror.state.playback_active.store(true, Ordering::Release);

        let state = mirror.state.clone();
        let view_id_owned = view_id.to_string();
        let handle =
            tokio::spawn(async move { reconcile_sweep(&state, &view_id_owned, "movies").await });

        // Several yield-poll cycles of real time: with playback active the
        // sweep must not have issued a single enumeration page.
        tokio::time::sleep(PLAYBACK_YIELD_POLL * 4).await;
        assert_eq!(
            server.request_count("/Items"),
            0,
            "no sweep page may be fetched while playback is active"
        );

        mirror.state.playback_active.store(false, Ordering::Release);
        let outcome = handle.await.expect("join").expect("sweep must succeed");
        assert_eq!(outcome.orphans_removed, 1);
        assert_eq!(mirror.item_count().await, 0, "the resumed sweep must prune");
    }

    /// Pins: `local_summary`'s `COUNT(*)` covers every item type (virtuals included), matching `reconcile_view`'s unrestricted server-count probe.
    #[tokio::test]
    async fn local_summary_counts_virtual_items_same_as_the_server_side_probe() {
        let client = JellyfinClient::from_token("http://127.0.0.1:0", identity(), "tok");
        let mirror = TestMirror::new(client);
        let view_id = "11111111-1111-1111-1111-111111111111";

        let virtual_dto: BaseItemDto = serde_json::from_value(json!({
            "Id": "22222222-2222-2222-2222-222222222222", "Name": "Unaired",
            "Type": "Episode", "LocationType": "Virtual"
        }))
        .expect("dto");
        mirror
            .state
            .writer
            .upsert_items_scoped(vec![virtual_dto], Some(view_id.to_string()))
            .await;
        mirror.state.writer.barrier().await;

        let (local_count, _) = local_summary(&mirror.state, view_id, &["Episode"]).await;
        assert_eq!(
            local_count, 1,
            "a virtual episode must count toward local_summary's total, matching the server's \
             own unfiltered recursive count"
        );
    }

    /// Pins: `local_summary`'s date probe excludes `is_virtual` rows, so a virtual row's future date can't win the MAX().
    #[tokio::test]
    async fn local_summary_date_probe_excludes_virtual_rows() {
        let client = JellyfinClient::from_token("http://127.0.0.1:0", identity(), "tok");
        let mirror = TestMirror::new(client);
        let view_id = "11111111-1111-1111-1111-111111111111";

        let real_dto: BaseItemDto = serde_json::from_value(json!({
            "Id": "22222222-2222-2222-2222-222222222222", "Name": "S01E01",
            "Type": "Episode", "LocationType": "FileSystem",
            "DateCreated": "2024-01-01T00:00:00Z"
        }))
        .expect("dto");
        let virtual_dto: BaseItemDto = serde_json::from_value(json!({
            "Id": "33333333-3333-3333-3333-333333333333", "Name": "S01E02",
            "Type": "Episode", "LocationType": "Virtual",
            "DateCreated": "2099-01-01T00:00:00Z"
        }))
        .expect("dto");
        mirror
            .state
            .writer
            .upsert_items_scoped(vec![real_dto, virtual_dto], Some(view_id.to_string()))
            .await;
        mirror.state.writer.barrier().await;

        let (local_count, local_newest) = local_summary(&mirror.state, view_id, &["Episode"]).await;
        assert_eq!(local_count, 2, "both rows count toward the total");
        assert_eq!(
            // `rows::extract_columns` stores `date_created` via
            // `DateTime::to_rfc3339()`, which normalizes a `Z`-suffixed
            // input to an explicit `+00:00` offset.
            local_newest.as_deref(),
            Some("2024-01-01T00:00:00+00:00"),
            "the virtual row's future date must not win the MAX() -- only the real episode's \
             date_created should surface"
        );
    }

    /// Pins: reconcile still catches a virtual-to-real swap when the virtual row's own `date_created` is in the future.
    #[tokio::test]
    async fn reconcile_catches_virtual_to_real_swap_even_with_a_future_virtual_date() {
        let server = MockServer::start().await;
        let client = JellyfinClient::from_token(&server.base_url, identity(), "tok");
        let view_id = "11111111-1111-1111-1111-111111111111";
        let old_virtual_id = "22222222-2222-2222-2222-222222222222";
        let new_real_id = "33333333-3333-3333-3333-333333333333";

        server.route(
            "/UserViews",
            &[],
            json!({ "Items": [{
                "Id": view_id, "Name": "Shows", "CollectionType": "tvshows",
                "DateLastMediaAdded": "2026-08-13T00:00:00Z"
            }] }),
        );
        server.route(
            "/Items",
            &[("parentId", view_id), ("limit", "1")],
            json!({ "Items": [], "TotalRecordCount": 1 }),
        );
        route_sweep_enumeration(&server, view_id, &[new_real_id]);
        route_by_ids(
            &server,
            &[json!({
                "Id": new_real_id, "Name": "S01E01", "Type": "Episode",
                "LocationType": "FileSystem", "DateCreated": "2026-08-13T00:00:00Z"
            })],
        );

        let mirror = TestMirror::new(client);
        mirror
            .state
            .writer
            .upsert_views(vec![crate::ViewRow {
                id: view_id.to_string(),
                name: "Shows".to_string(),
                collection_type: "tvshows".to_string(),
                item_type: "CollectionFolder".to_string(),
            }])
            .await;
        let virtual_dto: BaseItemDto = serde_json::from_value(json!({
            "Id": old_virtual_id, "Name": "S01E01", "Type": "Episode",
            "LocationType": "Virtual", "DateCreated": "2026-08-14T00:00:00Z"
        }))
        .expect("dto");
        mirror
            .state
            .writer
            .upsert_items_scoped(vec![virtual_dto], Some(view_id.to_string()))
            .await;
        assert_eq!(mirror.item_count().await, 1);

        reconcile_all(&mirror.state).await;
        mirror.state.writer.barrier().await;

        let conn = mirror.state.read_pool.acquire();
        let rows: Vec<(String, i64)> = {
            let mut stmt = conn
                .prepare("SELECT id, is_virtual FROM items ORDER BY id")
                .expect("prepare");
            stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
                .expect("query")
                .collect::<rusqlite::Result<_>>()
                .expect("rows")
        };
        assert_eq!(
            rows,
            vec![(new_real_id.to_string(), 0)],
            "the virtual-to-real swap must still be caught (and converged) even though the \
             virtual row's own date_created is in the future relative to the server's real \
             DateLastMediaAdded"
        );
    }

    /// Pins: `sync::spawn`'s non-initial-sync branch runs `reconcile_all` promptly at startup, not only on `reconcile_timer`'s first tick.
    #[tokio::test]
    async fn populated_mirror_startup_runs_reconcile_at_t0_not_only_on_the_timer() {
        let server = MockServer::start().await;
        let client = JellyfinClient::from_token(&server.base_url, identity(), "tok");
        let view_id = "11111111-1111-1111-1111-111111111111";

        server.route(
            "/UserViews",
            &[],
            json!({ "Items": [{ "Id": view_id, "Name": "Movies", "CollectionType": "movies" }] }),
        );
        server.route("/Shows/NextUp", &[], json!({ "Items": [] }));
        // Reconciliation's count probe: the server reports 1 item under this
        // library; the mirror below is seeded with 0 for it.
        server.route(
            "/Items",
            &[("parentId", view_id), ("limit", "1")],
            json!({ "Items": [], "TotalRecordCount": 1 }),
        );
        route_sweep_enumeration(&server, view_id, &["aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa"]);
        route_by_ids(
            &server,
            &[movie_json("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa", "A")],
        );

        let mirror = TestMirror::new(client);
        // A "populated, but this one library drifted" mirror: the view is
        // already known locally (as it would be after any prior sync), just
        // missing the item the server now reports.
        mirror
            .state
            .writer
            .upsert_views(vec![crate::ViewRow {
                id: view_id.to_string(),
                name: "Movies".to_string(),
                collection_type: "movies".to_string(),
                item_type: "CollectionFolder".to_string(),
            }])
            .await;
        assert_eq!(mirror.item_count().await, 0);

        let (_bus_tx, bus_rx) = broadcast::channel(16);
        // `needs_initial_sync = false` -- the branch `Mirror::open` takes for
        // any already-populated database, exactly like the real app on every
        // launch after the first.
        spawn(mirror.state.clone(), bus_rx, false);

        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if mirror.item_count().await == 1 {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "populated-mirror startup must run reconcile_all promptly -- the mismatched \
                 item never arrived within the timeout, so it isn't running at t=0"
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }

    /// Pins: `bus_listener` reconciles immediately on a `NeedsReconcile` reconnect signal, end-to-end, without waiting on `reconcile_timer`.
    #[tokio::test]
    async fn bus_listener_reconciles_immediately_on_reconnect_signal() {
        let server = MockServer::start().await;
        let client = JellyfinClient::from_token(&server.base_url, identity(), "tok");
        let view_id = "11111111-1111-1111-1111-111111111111";

        server.route(
            "/UserViews",
            &[],
            json!({ "Items": [{ "Id": view_id, "Name": "Movies", "CollectionType": "movies" }] }),
        );
        server.route(
            "/Items",
            &[("parentId", view_id), ("limit", "1")],
            json!({ "Items": [], "TotalRecordCount": 1 }),
        );
        route_sweep_enumeration(&server, view_id, &["aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa"]);
        route_by_ids(
            &server,
            &[movie_json("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa", "A")],
        );

        let mirror = TestMirror::new(client);
        mirror
            .state
            .writer
            .upsert_views(vec![crate::ViewRow {
                id: view_id.to_string(),
                name: "Movies".to_string(),
                collection_type: "movies".to_string(),
                item_type: "CollectionFolder".to_string(),
            }])
            .await;
        assert_eq!(mirror.item_count().await, 0);

        let (bus_tx, bus_rx) = broadcast::channel::<BusEvent>(16);
        tokio::spawn(bus_listener(mirror.weak(), bus_rx));

        // The exact sequence `event_bus::supervise` sends on every reconnect.
        bus_tx.send(BusEvent::Connected).expect("send Connected");
        bus_tx
            .send(BusEvent::NeedsReconcile)
            .expect("send NeedsReconcile");

        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if mirror.item_count().await == 1 {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the reconnect's NeedsReconcile signal must trigger reconcile_all immediately"
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }

    /// Pins: a brand-new nested item (Episode under an already-synced Series) gets stamped with the series ancestor's `library_id`, end-to-end through the WS `LibraryChanged` path.
    #[tokio::test]
    async fn library_changed_stamps_correct_library_id_for_a_nested_new_episode() {
        let server = MockServer::start().await;
        let client = JellyfinClient::from_token(&server.base_url, identity(), "tok");
        let view_id = "11111111-1111-1111-1111-111111111111";
        let series_id = "22222222-2222-2222-2222-222222222222";
        let new_episode_id = "33333333-3333-3333-3333-333333333333";

        server.route(
            "/Items",
            &[("ids", new_episode_id)],
            json!({ "Items": [{
                "Id": new_episode_id, "Name": "S01E01", "Type": "Episode",
                "SeriesId": series_id
            }] }),
        );
        server.route("/Shows/NextUp", &[], json!({ "Items": [] }));

        let mirror = TestMirror::new(client);
        let series_dto: BaseItemDto =
            serde_json::from_value(json!({ "Id": series_id, "Name": "Show", "Type": "Series" }))
                .expect("dto");
        mirror
            .state
            .writer
            .upsert_items_scoped(vec![series_dto], Some(view_id.to_string()))
            .await;
        // The series row must be committed before the ancestor-chain lookup
        // reads it off the read pool, or this races.
        mirror.state.writer.barrier().await;

        apply_server_event(
            &mirror.state,
            ServerEvent::LibraryChanged {
                added: vec![new_episode_id.to_string()],
                updated: vec![],
                removed: vec![],
            },
        )
        .await;
        mirror.state.writer.barrier().await;

        let conn = mirror.state.read_pool.acquire();
        let library_id: Option<String> = conn
            .query_row(
                "SELECT library_id FROM items WHERE id = ?1",
                [new_episode_id],
                |r| r.get(0),
            )
            .expect("row");
        assert_eq!(
            library_id.as_deref(),
            Some(view_id),
            "a new nested episode resolved via its series ancestor must be stamped with the \
             series' own library_id, not left NULL/unresolved"
        );
    }

    /// Pins: a breadth walk holds between pages while playback is active and resumes once it stops.
    #[tokio::test]
    async fn breadth_sync_yields_while_playback_is_active() {
        let server = MockServer::start().await;
        let client = JellyfinClient::from_token(&server.base_url, identity(), "tok");
        let view_id = "22222222-2222-2222-2222-222222222222";

        server.route(
            "/Items",
            &[("parentId", view_id), ("startIndex", "0")],
            json!({
                "Items": [movie_json("bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb", "B")],
                "TotalRecordCount": 1
            }),
        );

        let mirror = TestMirror::new(client);
        mirror.state.playback_active.store(true, Ordering::Release);

        let weak = mirror.weak();
        let view_id_owned = view_id.to_string();
        let handle =
            tokio::spawn(
                async move { sync_library_breadth(&weak, &view_id_owned, "movies").await },
            );

        // Give the walk several yield-poll cycles of real time: with
        // playback active it must not have issued a single page request.
        tokio::time::sleep(PLAYBACK_YIELD_POLL * 4).await;
        assert_eq!(
            server.request_count("/Items"),
            0,
            "no pages may be fetched while playback is active"
        );

        // Playback stops; the held walk must resume and complete.
        mirror.state.playback_active.store(false, Ordering::Release);
        assert!(handle.await.expect("join"), "breadth sync must complete");
        assert!(
            server.request_count("/Items") >= 1,
            "the walk must resume where it left off"
        );
        mirror.state.writer.barrier().await;
        let conn = mirror.state.read_pool.acquire();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM items", [], |r| r.get(0))
            .expect("count");
        assert_eq!(count, 1, "the resumed walk must commit its page");
    }

    /// Pins: `sync_activity()` transitions `Idle -> Syncing -> Idle` around a breadth sync.
    #[tokio::test]
    async fn breadth_sync_reports_syncing_then_idle_activity() {
        use std::time::Duration;

        let server = MockServer::start().await;
        let client = JellyfinClient::from_token(&server.base_url, identity(), "tok");
        let view_id = "11111111-1111-1111-1111-111111111111";

        server.route_delayed(
            "/Items",
            &[("parentId", view_id), ("startIndex", "0")],
            json!({
                "Items": [movie_json("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa", "A")],
                "TotalRecordCount": 1
            }),
            Duration::from_millis(150),
        );

        let mirror = TestMirror::new(client);
        let mut activity_rx = mirror.state.sync_activity.subscribe();
        assert_eq!(*activity_rx.borrow(), SyncActivity::Idle);

        let weak = mirror.weak();
        let view_id_owned = view_id.to_string();
        let handle =
            tokio::spawn(
                async move { sync_library_breadth(&weak, &view_id_owned, "movies").await },
            );

        // The page fetch is deliberately slow; wait for the transition
        // rather than racing it with a fixed sleep.
        activity_rx
            .changed()
            .await
            .expect("activity changed to Syncing");
        assert_eq!(
            *activity_rx.borrow(),
            SyncActivity::Syncing {
                library_name_or_id: view_id.to_string(),
                pages_done: 0,
                items_done: 0,
                // First page still in flight: the denominator isn't known
                // yet (it arrives with the first response).
                total_items: None,
            }
        );

        assert!(
            handle.await.expect("breadth sync task"),
            "breadth sync must complete normally"
        );

        activity_rx
            .changed()
            .await
            .expect("activity changed back to Idle");
        assert_eq!(*activity_rx.borrow(), SyncActivity::Idle);
    }

    const DELTA_ITEM_A: &str = "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa";
    const DELTA_ITEM_B: &str = "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb";

    /// Pins: the delta query window reaches `DELTA_OVERLAP_SLACK` past the stored cursor; an unwritten value is rejected.
    #[test]
    fn delta_query_since_applies_the_overlap_slack() {
        assert_eq!(
            delta_query_since("2026-08-01T00:00:00Z").as_deref(),
            Some("2026-07-31T23:55:00Z")
        );
        // Normalized to UTC, not left in the offset it arrived in.
        assert_eq!(
            delta_query_since("2026-08-01T02:00:00+02:00").as_deref(),
            Some("2026-07-31T23:55:00Z")
        );
        assert_eq!(delta_query_since("not a timestamp"), None);
        assert_eq!(delta_query_since(""), None);
    }

    /// Pins: `next_up_date_cutoff(days)` lands within seconds of "now minus `days` days", strictly older for larger `days`.
    #[test]
    fn next_up_date_cutoff_subtracts_days_from_now() {
        let zero = next_up_date_cutoff(0);
        let now = chrono::Utc::now();
        let parsed = chrono::DateTime::parse_from_rfc3339(&zero)
            .expect("valid RFC3339")
            .with_timezone(&chrono::Utc);
        assert!(
            (now - parsed).num_seconds().abs() < 5,
            "0-day cutoff should be ~now, got {zero}"
        );
        assert!(zero.ends_with('Z'), "must be UTC ('Z' suffix): {zero}");

        let fourteen = next_up_date_cutoff(14);
        let parsed_14 = chrono::DateTime::parse_from_rfc3339(&fourteen)
            .expect("valid RFC3339")
            .with_timezone(&chrono::Utc);
        let delta_days = (parsed - parsed_14).num_seconds() as f64 / 86_400.0;
        assert!(
            (delta_days - 14.0).abs() < 0.01,
            "expected ~14 days between the 0-day and 14-day cutoffs, got {delta_days}"
        );
    }

    /// Pins: one delta query picks up both a brand-new item and an in-place update neither reconcile probe can see, and advances the cursor.
    #[tokio::test]
    async fn delta_sync_upserts_new_and_updated_items_and_advances_the_cursor() {
        let server = MockServer::start().await;
        let client = JellyfinClient::from_token(&server.base_url, identity(), "tok");
        let mirror = TestMirror::new(client);

        // An already-mirrored item, carrying the stale name.
        let existing: BaseItemDto =
            serde_json::from_value(movie_json(DELTA_ITEM_A, "Existing Movie (720p)")).expect("dto");
        mirror.state.writer.upsert_items(vec![existing]).await;
        mirror
            .state
            .writer
            .set_meta(LAST_DELTA_SYNC_KEY, "2026-08-01T00:00:00Z".to_string())
            .await;

        server.route("/Shows/NextUp", &[], json!({ "Items": [] }));
        // Keyed on the exact expected window: cursor minus the 5-minute
        // overlap slack. If the slack math regresses, this route stops
        // matching and the test fails on the assertions below.
        server.route(
            "/Items",
            &[("minDateLastSaved", "2026-07-31T23:55:00Z")],
            json!({
                "Items": [
                    movie_json(DELTA_ITEM_A, "Existing Movie (1080p)"),
                    movie_json(DELTA_ITEM_B, "Brand New Movie"),
                ],
                "TotalRecordCount": 2
            }),
        );

        delta_sync(&mirror.state).await;

        assert_eq!(mirror.item_count().await, 2);
        assert_eq!(
            mirror.item_name(DELTA_ITEM_A).await.as_deref(),
            Some("Existing Movie (1080p)"),
            "an in-place update must overwrite the mirrored metadata"
        );
        assert_eq!(
            mirror.item_name(DELTA_ITEM_B).await.as_deref(),
            Some("Brand New Movie")
        );

        let cursor = mirror
            .meta(LAST_DELTA_SYNC_KEY)
            .await
            .expect("cursor is set after a successful pass");
        assert!(
            cursor.as_str() > "2026-08-01T00:00:00Z",
            "cursor should have advanced past the seeded value, got {cursor}"
        );
    }

    /// Pins: a delta page spanning several libraries resolves each item's own `library_id` individually, not one stamp for the whole page.
    #[tokio::test]
    async fn delta_sync_stamps_each_item_with_its_own_resolved_library() {
        let server = MockServer::start().await;
        let client = JellyfinClient::from_token(&server.base_url, identity(), "tok");
        let mirror = TestMirror::new(client);

        let tv_view = "11111111-1111-1111-1111-111111111111";
        let series_id = "22222222-2222-2222-2222-222222222222";
        let episode_id = "33333333-3333-3333-3333-333333333333";

        // The Series is already mirrored and stamped with the TV library.
        let series: BaseItemDto =
            serde_json::from_value(json!({ "Id": series_id, "Name": "Show", "Type": "Series" }))
                .expect("dto");
        mirror
            .state
            .writer
            .upsert_items_scoped(vec![series], Some(tv_view.to_string()))
            .await;
        mirror
            .state
            .writer
            .set_meta(LAST_DELTA_SYNC_KEY, "2026-08-01T00:00:00Z".to_string())
            .await;

        server.route("/Shows/NextUp", &[], json!({ "Items": [] }));
        server.route(
            "/Items",
            &[("minDateLastSaved", "2026-07-31T23:55:00Z")],
            json!({
                "Items": [{
                    "Id": episode_id, "Name": "New Episode", "Type": "Episode",
                    "SeriesId": series_id
                }],
                "TotalRecordCount": 1
            }),
        );

        delta_sync(&mirror.state).await;

        mirror.state.writer.barrier().await;
        let conn = mirror.state.read_pool.acquire();
        let library_id: Option<String> = conn
            .query_row(
                "SELECT library_id FROM items WHERE id = ?1",
                [episode_id],
                |r| r.get(0),
            )
            .expect("episode row");
        assert_eq!(
            library_id.as_deref(),
            Some(tv_view),
            "the new episode must inherit its series' library, so Latest/Home \
             for that library picks it up"
        );
    }

    /// Pins: with no cursor yet, a delta pass queries nothing but bootstraps a cursor so later passes work.
    #[tokio::test]
    async fn delta_sync_with_no_cursor_does_not_query_but_bootstraps_one() {
        let server = MockServer::start().await;
        let client = JellyfinClient::from_token(&server.base_url, identity(), "tok");
        let mirror = TestMirror::new(client);

        assert_eq!(mirror.meta(LAST_DELTA_SYNC_KEY).await, None);

        delta_sync(&mirror.state).await;

        assert_eq!(
            server.request_count("/Items"),
            0,
            "a cursorless delta must not issue any /Items query"
        );
        assert!(
            mirror.meta(LAST_DELTA_SYNC_KEY).await.is_some(),
            "the pass must adopt a cursor so the next one can run"
        );
    }

    /// Pins: a pass whose second page fails banks nothing -- the cursor stays put and the next pass redoes the whole window.
    #[tokio::test]
    async fn delta_sync_leaves_the_cursor_alone_when_a_page_fails() {
        let server = MockServer::start().await;
        let client = JellyfinClient::from_token(&server.base_url, identity(), "tok");
        let mirror = TestMirror::new(client);

        mirror
            .state
            .writer
            .set_meta(LAST_DELTA_SYNC_KEY, "2026-08-01T00:00:00Z".to_string())
            .await;
        server.route("/Shows/NextUp", &[], json!({ "Items": [] }));
        // Page 1 lands and claims there are more; page 2 (startIndex=1) has
        // no route, so the client sees a 404 and the pass aborts.
        server.route(
            "/Items",
            &[
                ("minDateLastSaved", "2026-07-31T23:55:00Z"),
                ("startIndex", "0"),
            ],
            json!({
                "Items": [movie_json(DELTA_ITEM_A, "Page One")],
                "TotalRecordCount": 2
            }),
        );

        delta_sync(&mirror.state).await;

        // What did land stays landed -- upserts are independent of the
        // cursor -- but the cursor itself must not have moved.
        assert_eq!(
            mirror.item_name(DELTA_ITEM_A).await.as_deref(),
            Some("Page One")
        );
        assert_eq!(
            mirror.meta(LAST_DELTA_SYNC_KEY).await.as_deref(),
            Some("2026-08-01T00:00:00Z"),
            "a partially-failed pass must not bank progress"
        );
    }

    /// Pins: the cursor is stamped as the pass's START instant, not its completion.
    #[tokio::test]
    async fn delta_sync_cursor_is_the_pass_start_not_its_completion() {
        use std::time::Duration;

        let server = MockServer::start().await;
        let client = JellyfinClient::from_token(&server.base_url, identity(), "tok");
        let mirror = TestMirror::new(client);

        mirror
            .state
            .writer
            .set_meta(LAST_DELTA_SYNC_KEY, "2026-08-01T00:00:00Z".to_string())
            .await;
        server.route("/Shows/NextUp", &[], json!({ "Items": [] }));
        server.route_delayed(
            "/Items",
            &[("minDateLastSaved", "2026-07-31T23:55:00Z")],
            json!({
                "Items": [movie_json(DELTA_ITEM_A, "Slow Page")],
                "TotalRecordCount": 1
            }),
            Duration::from_millis(2100),
        );

        delta_sync(&mirror.state).await;
        let finished_at = now_rfc3339();

        let cursor = mirror
            .meta(LAST_DELTA_SYNC_KEY)
            .await
            .expect("cursor after a successful pass");
        assert!(
            cursor.as_str() < finished_at.as_str(),
            "cursor {cursor} must predate the pass's completion at {finished_at}"
        );
    }

    /// Pins: re-fetching the same item across overlapping delta windows is a no-op, not a duplicate.
    #[tokio::test]
    async fn delta_sync_is_idempotent_across_overlapping_windows() {
        let server = MockServer::start().await;
        let client = JellyfinClient::from_token(&server.base_url, identity(), "tok");
        let mirror = TestMirror::new(client);

        mirror
            .state
            .writer
            .set_meta(LAST_DELTA_SYNC_KEY, "2026-08-01T00:00:00Z".to_string())
            .await;
        server.route("/Shows/NextUp", &[], json!({ "Items": [] }));
        // Matches any window, so the second pass (whose cursor has advanced
        // to "now") re-fetches the same payload the way a real overlap does.
        server.route(
            "/Items",
            &[("recursive", "true")],
            json!({
                "Items": [
                    movie_json(DELTA_ITEM_A, "Movie A"),
                    movie_json(DELTA_ITEM_B, "Movie B"),
                ],
                "TotalRecordCount": 2
            }),
        );

        delta_sync(&mirror.state).await;
        assert_eq!(mirror.item_count().await, 2);
        let after_first = mirror.meta(LAST_DELTA_SYNC_KEY).await;

        delta_sync(&mirror.state).await;

        assert!(
            server.request_count("/Items") >= 2,
            "sanity: the second pass must actually have re-queried"
        );
        assert_eq!(
            mirror.item_count().await,
            2,
            "re-applying an overlapping window must not duplicate rows"
        );
        assert_eq!(
            mirror.item_name(DELTA_ITEM_A).await.as_deref(),
            Some("Movie A")
        );
        assert!(mirror.meta(LAST_DELTA_SYNC_KEY).await >= after_first);
    }

    /// Pins: initial sync stamps the delta cursor itself, as the instant it STARTED.
    #[tokio::test]
    async fn initial_sync_stamps_the_delta_cursor_from_its_start_instant() {
        let server = MockServer::start().await;
        let client = JellyfinClient::from_token(&server.base_url, identity(), "tok");
        let view_id = "11111111-1111-1111-1111-111111111111";

        server.route(
            "/UserViews",
            &[],
            json!({ "Items": [{ "Id": view_id, "Name": "Movies", "CollectionType": "movies" }] }),
        );
        server.route("/UserItems/Resume", &[], json!({ "Items": [] }));
        server.route("/Shows/NextUp", &[], json!({ "Items": [] }));
        server.route(
            "/Items",
            &[("parentId", view_id)],
            json!({
                "Items": [movie_json(DELTA_ITEM_A, "Movie A")],
                "TotalRecordCount": 1
            }),
        );

        let mirror = TestMirror::new(client);
        let started_at = now_rfc3339();
        initial_sync(&mirror.weak()).await;
        let finished_at = now_rfc3339();

        let cursor = mirror
            .meta(LAST_DELTA_SYNC_KEY)
            .await
            .expect("initial sync must stamp the delta cursor");
        assert!(
            cursor.as_str() >= started_at.as_str() && cursor.as_str() <= finished_at.as_str(),
            "cursor {cursor} should sit in [{started_at}, {finished_at}]"
        );
    }

    /// Pins: a second `delta_sync` call entered mid-pass defers (one rerun) rather than running concurrently.
    #[tokio::test]
    async fn delta_sync_is_single_flight() {
        use std::time::Duration;

        let server = MockServer::start().await;
        let client = JellyfinClient::from_token(&server.base_url, identity(), "tok");
        let mirror = TestMirror::new(client);

        mirror
            .state
            .writer
            .set_meta(LAST_DELTA_SYNC_KEY, "2026-08-01T00:00:00Z".to_string())
            .await;
        server.route("/Shows/NextUp", &[], json!({ "Items": [] }));
        server.route_delayed(
            "/Items",
            &[("recursive", "true")],
            json!({
                "Items": [movie_json(DELTA_ITEM_A, "Movie A")],
                "TotalRecordCount": 1
            }),
            Duration::from_millis(300),
        );

        let state = mirror.state.clone();
        let first = tokio::spawn(async move { delta_sync(&state).await });
        tokio::time::sleep(Duration::from_millis(80)).await;
        assert!(
            mirror.state.delta_in_progress.load(Ordering::Acquire),
            "sanity: the first pass should still be in flight"
        );

        // Enters, finds the guard taken, defers.
        delta_sync(&mirror.state).await;
        assert!(
            mirror.state.delta_pending.load(Ordering::Acquire),
            "the deferred trigger must be recorded, not dropped"
        );

        first.await.expect("first delta pass");
        assert!(
            !mirror.state.delta_pending.load(Ordering::Acquire),
            "the running pass must consume the deferred trigger and rerun"
        );
        assert!(!mirror.state.delta_in_progress.load(Ordering::Acquire));
    }

    /// Pins: a timer tick landing mid-initial-sync doesn't fire a competing delta or bootstrap a cursor initial sync is about to stamp.
    #[tokio::test]
    async fn delta_sync_defers_to_an_in_progress_initial_sync() {
        let server = MockServer::start().await;
        let client = JellyfinClient::from_token(&server.base_url, identity(), "tok");
        let mirror = TestMirror::new(client);

        mirror
            .state
            .initial_sync_in_progress
            .store(true, Ordering::Release);

        delta_sync(&mirror.state).await;

        assert_eq!(server.request_count("/Items"), 0);
        assert_eq!(
            mirror.meta(LAST_DELTA_SYNC_KEY).await,
            None,
            "initial sync stamps the first cursor; delta must not race it"
        );
    }
}
