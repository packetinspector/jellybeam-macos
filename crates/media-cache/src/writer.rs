//! The single writer: owns the read-write `Connection`, drains a command
//! queue, and broadcasts the change-feed in commit order (docs/DATA.md §2's
//! "ordering rule" — every mutation, from initial sync, WS deltas, or
//! reconciliation, flows through here, so there's no interleaving hazard).

use rusqlite::{params, Connection, OptionalExtension};
use tokio::sync::{broadcast, mpsc, oneshot};

use jellyfin_api::models::{BaseItemDto, UserItemDataDto};

use crate::rows::{extract_columns, search_text, to_dto_bytes, SearchText};
use crate::{MirrorChange, ViewRow};

pub(crate) enum WriteCmd {
    UpsertViews(Vec<ViewRow>),
    UpsertItems {
        items: Vec<BaseItemDto>,
        /// The library every item in this batch belongs to, when the caller
        /// can attribute the whole batch to one library up front (e.g. a
        /// single view's breadth-sync page). `None` leaves each item's
        /// existing `library_id` untouched (`COALESCE` in the upsert SQL,
        /// see `apply_upsert_items_scoped`) rather than wiping it -- used
        /// where a batch can't be cheaply attributed to one library (Resume/
        /// NextUp span every library; a WS `LibraryChanged` batch is
        /// pre-split by `sync::resolve_library_ids_for_changed_items`, so
        /// `None` there means "still unresolved").
        library_id: Option<String>,
    },
    /// Persist one item's live per-visit enrichment (`MediaStreams` and
    /// friends) onto the row the bulk sync already wrote -- see
    /// [`crate::Mirror::upsert_enriched_item`] and
    /// [`apply_upsert_enriched_item`]. Deliberately NOT an `UpsertItems`
    /// with a one-item batch: this touches only the `dto` blob (plus
    /// `overview` and its FTS postings), so every column the enrichment
    /// projection can't speak for is preserved by construction rather than
    /// an SQL `COALESCE` per column.
    ///
    /// Boxed: every other variant carries a handful of bytes inline, while
    /// a bare `BaseItemDto` here is ~3KB and would set the size of every
    /// command in the queue.
    UpsertEnrichedItem(Box<BaseItemDto>),
    RemoveItems(Vec<String>),
    /// Deletes every local row stamped with `library_id` whose id isn't in
    /// `keep_ids` -- issued once, after a library's full recursive breadth
    /// sync has finished upserting every page. `keep_ids` is that fetch's
    /// complete, current membership snapshot, so anything else under this
    /// library the mirror still has is provably gone from the server (see
    /// `apply_prune_library`'s doc comment for why no parent/child cascade
    /// is needed here, unlike `RemoveItems`).
    ///
    /// Relies on the single writer task's strict FIFO ordering: every
    /// `UpsertItems` this same breadth sync sent is applied before this
    /// command reaches the front of the queue, so there is never a window
    /// where a page not yet landed looks pruned.
    PruneLibrary {
        library_id: String,
        keep_ids: Vec<String>,
    },
    /// Re-applies `sync.rs::browse_root_item_type`'s parent flattening to
    /// rows already in the mirror: every row of `item_type` stamped with
    /// `library_id` gets `parent_id = library_id`. Idempotent and cheap.
    /// Exists because the delta-sync path historically skipped the
    /// flattening, leaving newly-added Series/Movies under their physical
    /// folder's id where `children(view_id)` could never find them. Run
    /// once at sync start so existing mirrors heal without a full resync.
    FlattenRootParents {
        library_id: String,
        item_type: String,
    },
    ApplyUserData(Vec<(String, UserItemDataDto)>),
    /// Optimistic local application of one item's watch state, used when
    /// *we* just reported progress/stop/EOF to the server but can't rely on
    /// a `UserDataChanged` WS event coming back (see
    /// `Mirror::apply_local_user_data`'s doc comment for why).
    ApplyLocalUserData {
        item_id: String,
        position_ticks: i64,
        played: Option<bool>,
    },
    /// Replaces the full membership list of one BoxSet (`item_id`,
    /// `sort_index` pairs, in server order) in a single transaction --
    /// delete-then-reinsert, since the server is the sole source of truth
    /// for a collection's membership.
    SetCollectionMembers {
        collection_id: String,
        members: Vec<(String, i64)>,
    },
    SetMeta {
        key: &'static str,
        value: String,
    },
    /// Fired after enqueueing a batch the caller wants to know completed
    /// (commands are processed strictly in order, so this just needs to
    /// drain to this point in the queue).
    Barrier(oneshot::Sender<()>),
}

#[derive(Clone)]
pub(crate) struct WriterHandle {
    tx: mpsc::Sender<WriteCmd>,
}

impl WriterHandle {
    pub(crate) fn new(tx: mpsc::Sender<WriteCmd>) -> Self {
        Self { tx }
    }

    pub(crate) async fn upsert_views(&self, views: Vec<ViewRow>) {
        self.send(WriteCmd::UpsertViews(views)).await;
    }

    pub(crate) async fn upsert_items(&self, items: Vec<BaseItemDto>) {
        self.upsert_items_scoped(items, None).await;
    }

    /// See [`WriteCmd::UpsertItems`]'s `library_id` doc comment.
    pub(crate) async fn upsert_items_scoped(
        &self,
        items: Vec<BaseItemDto>,
        library_id: Option<String>,
    ) {
        if items.is_empty() {
            return;
        }
        self.send(WriteCmd::UpsertItems { items, library_id }).await;
    }

    /// See [`WriteCmd::UpsertEnrichedItem`].
    pub(crate) async fn upsert_enriched_item(&self, item: BaseItemDto) {
        self.send(WriteCmd::UpsertEnrichedItem(Box::new(item)))
            .await;
    }

    pub(crate) async fn remove_items(&self, ids: Vec<String>) {
        if ids.is_empty() {
            return;
        }
        self.send(WriteCmd::RemoveItems(ids)).await;
    }

    /// See [`WriteCmd::PruneLibrary`]. Not skipped when `keep_ids` is empty
    /// (unlike `remove_items`'s empty-ids no-op) -- an empty `keep_ids` is a
    /// legitimate "this library is now empty" snapshot, and must still prune
    /// every existing row stamped with `library_id`.
    pub(crate) async fn prune_library(&self, library_id: String, keep_ids: Vec<String>) {
        self.send(WriteCmd::PruneLibrary {
            library_id,
            keep_ids,
        })
        .await;
    }

    /// See [`WriteCmd::FlattenRootParents`].
    pub(crate) async fn flatten_root_parents(&self, library_id: String, item_type: String) {
        self.send(WriteCmd::FlattenRootParents {
            library_id,
            item_type,
        })
        .await;
    }

    pub(crate) async fn apply_user_data(&self, updates: Vec<(String, UserItemDataDto)>) {
        if updates.is_empty() {
            return;
        }
        self.send(WriteCmd::ApplyUserData(updates)).await;
    }

    /// See `WriteCmd::ApplyLocalUserData`.
    pub(crate) async fn apply_local_user_data(
        &self,
        item_id: String,
        position_ticks: i64,
        played: Option<bool>,
    ) {
        self.send(WriteCmd::ApplyLocalUserData {
            item_id,
            position_ticks,
            played,
        })
        .await;
    }

    pub(crate) async fn set_collection_members(
        &self,
        collection_id: String,
        members: Vec<(String, i64)>,
    ) {
        self.send(WriteCmd::SetCollectionMembers {
            collection_id,
            members,
        })
        .await;
    }

    pub(crate) async fn set_meta(&self, key: &'static str, value: String) {
        self.send(WriteCmd::SetMeta { key, value }).await;
    }

    /// Wait until every command enqueued before this call has been applied.
    pub(crate) async fn barrier(&self) {
        let (tx, rx) = oneshot::channel();
        self.send(WriteCmd::Barrier(tx)).await;
        let _ = rx.await;
    }

    async fn send(&self, cmd: WriteCmd) {
        if self.tx.send(cmd).await.is_err() {
            tracing::warn!("mirror writer task is gone; dropping write command");
        }
    }
}

/// Runs on a `spawn_blocking` task for the lifetime of the `Mirror`. Blocking
/// `rusqlite` calls belong here and nowhere else.
pub(crate) fn run(
    mut conn: Connection,
    mut rx: mpsc::Receiver<WriteCmd>,
    changes: broadcast::Sender<MirrorChange>,
) {
    while let Some(cmd) = rx.blocking_recv() {
        match cmd {
            WriteCmd::UpsertViews(views) => match apply_upsert_views(&mut conn, &views) {
                Ok(removed_items) => {
                    let _ = changes.send(MirrorChange::ViewsChanged);
                    // See `apply_upsert_views`'s doc comment: a revoked
                    // view's items were cascade-deleted in the same
                    // transaction. Tell the change feed too, so anything
                    // showing the now-gone items directly still refreshes.
                    if !removed_items.is_empty() {
                        let _ = changes.send(MirrorChange::Removed(removed_items));
                    }
                }
                Err(e) => tracing::error!(error = %e, "failed to upsert views"),
            },
            WriteCmd::UpsertItems { items, library_id } => {
                match apply_upsert_items_scoped(&mut conn, &items, library_id.as_deref()) {
                    Ok(ids) if !ids.is_empty() => {
                        let _ = changes.send(MirrorChange::Upserted(ids));
                    }
                    Ok(_) => {}
                    Err(e) => tracing::error!(error = %e, "failed to upsert items"),
                }
            }
            WriteCmd::UpsertEnrichedItem(item) => {
                match apply_upsert_enriched_item(&mut conn, &item) {
                    // `None` = nothing to do (unknown item, or the stored
                    // blob already carried this enrichment): no change
                    // event, so a second identical enrichment landing on an
                    // open Detail page can't start a refresh cycle.
                    Ok(Some(id)) => {
                        let _ = changes.send(MirrorChange::Upserted(vec![id]));
                    }
                    Ok(None) => {}
                    Err(e) => tracing::error!(error = %e, "failed to persist item enrichment"),
                }
            }
            WriteCmd::RemoveItems(ids) => match apply_remove_items(&mut conn, &ids) {
                Ok(removed) if !removed.is_empty() => {
                    let _ = changes.send(MirrorChange::Removed(removed));
                }
                Ok(_) => {}
                Err(e) => tracing::error!(error = %e, "failed to remove items"),
            },
            WriteCmd::PruneLibrary {
                library_id,
                keep_ids,
            } => match apply_prune_library(&mut conn, &library_id, &keep_ids) {
                Ok(removed) if !removed.is_empty() => {
                    let _ = changes.send(MirrorChange::Removed(removed));
                }
                Ok(_) => {}
                Err(e) => tracing::error!(error = %e, library_id, "failed to prune library"),
            },
            WriteCmd::FlattenRootParents {
                library_id,
                item_type,
            } => match apply_flatten_root_parents(&mut conn, &library_id, &item_type) {
                Ok(ids) if !ids.is_empty() => {
                    tracing::info!(
                        library_id,
                        item_type,
                        healed = ids.len(),
                        "re-parented root items onto their library view"
                    );
                    let _ = changes.send(MirrorChange::Upserted(ids));
                }
                Ok(_) => {}
                Err(e) => {
                    tracing::error!(error = %e, library_id, "failed to flatten root parents")
                }
            },
            WriteCmd::ApplyUserData(updates) => match apply_user_data(&mut conn, &updates) {
                Ok(ids) if !ids.is_empty() => {
                    let _ = changes.send(MirrorChange::Upserted(ids));
                }
                Ok(_) => {}
                Err(e) => tracing::error!(error = %e, "failed to apply user data changes"),
            },
            WriteCmd::ApplyLocalUserData {
                item_id,
                position_ticks,
                played,
            } => match apply_local_user_data(&mut conn, &item_id, position_ticks, played) {
                Ok(Some(id)) => {
                    let _ = changes.send(MirrorChange::Upserted(vec![id]));
                }
                Ok(None) => {}
                Err(e) => {
                    tracing::error!(error = %e, item_id, "failed to apply local user data")
                }
            },
            WriteCmd::SetCollectionMembers {
                collection_id,
                members,
            } => {
                match apply_set_collection_members(&mut conn, &collection_id, &members) {
                    Ok(()) => {
                        // The BoxSet's own row is unchanged, but its
                        // resolved children are -- tell the change feed so a
                        // UI browsing into it knows to re-query.
                        let _ = changes.send(MirrorChange::Upserted(vec![collection_id]));
                    }
                    Err(e) => {
                        tracing::error!(error = %e, collection_id, "failed to set collection members")
                    }
                }
            }
            WriteCmd::SetMeta { key, value } => {
                if let Err(e) = crate::schema::upsert_meta(&conn, key, &value) {
                    tracing::error!(error = %e, key, "failed to write meta");
                }
            }
            WriteCmd::Barrier(reply) => {
                let _ = reply.send(());
            }
        }
    }
    tracing::debug!("mirror writer task exiting (channel closed)");
}

/// Upserts the server's current view list, then reconciles the mirror
/// against it in the *same* transaction: any view row not in `views` was
/// removed or revoked, and is deleted here along with every item stamped
/// with it as `library_id` (same cascade `apply_prune_library` performs,
/// including keeping the FTS `search` index and `collection_members` in
/// sync -- `search`'s `MATCH` join has no view/library scoping of its own).
///
/// Deliberately not split into an upsert plus a separate
/// `WriteCmd::PruneLibrary`-style prune: that split exists for a breadth
/// sync only because its complete membership snapshot isn't known until the
/// last paged `UpsertItems` lands. `sync_views` already has the complete
/// view list (`GetUserViews` is unpaginated), so splitting here would only
/// open a crash window where a stale view sits between the two commands.
///
/// An empty `views` prunes every existing view and item: the fail-safe
/// split lives one level up, at the fetch -- a transport error never calls
/// this function, so it can't wipe the mirror. Once a fetch succeeds, an
/// empty result is a legitimate "zero accessible libraries" snapshot,
/// treated as authoritative the same way `PruneLibrary`'s empty `keep_ids`
/// means "prune everything" (see `WriterHandle::prune_library`'s doc).
///
/// Returns the ids of every item removed by the cascade, for the change feed.
fn apply_upsert_views(conn: &mut Connection, views: &[ViewRow]) -> rusqlite::Result<Vec<String>> {
    let tx = conn.transaction()?;
    for (idx, view) in views.iter().enumerate() {
        tx.execute(
            "INSERT INTO views (id, name, collection_type, sort_index, item_type) VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(id) DO UPDATE SET name = excluded.name, collection_type = excluded.collection_type,
                sort_index = excluded.sort_index, item_type = excluded.item_type",
            params![view.id, view.name, view.collection_type, idx as i64, view.item_type],
        )?;
    }

    let keep: std::collections::HashSet<&str> = views.iter().map(|v| v.id.as_str()).collect();
    let stale_view_ids: Vec<String> = {
        let mut stmt = tx.prepare("SELECT id FROM views")?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    }
    .into_iter()
    .filter(|id| !keep.contains(id.as_str()))
    .collect();

    let mut removed_items = Vec::new();
    for view_id in &stale_view_ids {
        // Same shape as `apply_prune_library`'s local scan/cascade below,
        // scoped to this one stale view's `library_id`.
        let local: Vec<(i64, String, Vec<u8>)> = {
            let mut stmt = tx.prepare("SELECT rowid, id, dto FROM items WHERE library_id = ?1")?;
            let rows =
                stmt.query_map([view_id], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        for (rowid, id, blob) in &local {
            delete_item_row(&tx, *rowid, id, blob)?;
            removed_items.push(id.clone());
        }
        tx.execute("DELETE FROM views WHERE id = ?1", [view_id])?;
    }

    tx.commit()?;
    Ok(removed_items)
}

const UPSERT_ITEM_SQL: &str = "
INSERT INTO items (
    id, parent_id, series_id, season_id, item_type, name, sort_name,
    index_number, parent_index_number, production_year, premiere_date,
    runtime_ticks, date_created,
    played, playback_position_ticks, play_count, is_favorite,
    unplayed_item_count, primary_tag, primary_blurhash,
    series_primary_tag, parent_backdrop_item_id, parent_backdrop_tag,
    library_id, last_played_date, overview, is_virtual, series_name, dto, updated_at
) VALUES (
    ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13,
    ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25, ?26, ?27, ?28, ?29, ?30
)
ON CONFLICT(id) DO UPDATE SET
    parent_id = excluded.parent_id, series_id = excluded.series_id, season_id = excluded.season_id,
    item_type = excluded.item_type, name = excluded.name, sort_name = excluded.sort_name,
    index_number = excluded.index_number, parent_index_number = excluded.parent_index_number,
    production_year = excluded.production_year, premiere_date = excluded.premiere_date,
    runtime_ticks = excluded.runtime_ticks, date_created = excluded.date_created,
    played = excluded.played, playback_position_ticks = excluded.playback_position_ticks,
    play_count = excluded.play_count, is_favorite = excluded.is_favorite,
    unplayed_item_count = excluded.unplayed_item_count, primary_tag = excluded.primary_tag,
    primary_blurhash = excluded.primary_blurhash,
    series_primary_tag = excluded.series_primary_tag,
    parent_backdrop_item_id = excluded.parent_backdrop_item_id,
    parent_backdrop_tag = excluded.parent_backdrop_tag,
    -- A `NULL` `excluded.library_id` (an upsert that couldn't
    -- attribute this item to one library -- see `WriteCmd::UpsertItems`'s
    -- doc comment) must never clobber an already-known value; a non-NULL
    -- one (an authoritative breadth-sync page, or a WS delta resolved to a
    -- specific library) always wins, including correcting a stale value if
    -- an item ever moves libraries.
    library_id = COALESCE(excluded.library_id, items.library_id),
    -- Like `played`/`playback_position_ticks` above, taken
    -- unconditionally from the DTO -- every synced snapshot carries the
    -- server's current `UserData.LastPlayedDate` (the sync engine always
    -- queries with a user id), so this is exactly as authoritative as
    -- those. Critically, re-upserting the *same* server snapshot (a
    -- reconcile pass touching a row whose watch state hasn't actually
    -- changed) writes back the same value -- unlike `updated_at`, this
    -- doesn't drift just because the mirror happened to re-write the row.
    last_played_date = excluded.last_played_date,
    overview = excluded.overview,
    is_virtual = excluded.is_virtual,
    series_name = excluded.series_name,
    dto = excluded.dto, updated_at = excluded.updated_at
RETURNING rowid
";

fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Convenience wrapper for callers (and the many existing tests) that don't
/// need to attribute this batch to one library -- equivalent to
/// `apply_upsert_items_scoped(conn, items, None)`, which per `UPSERT_ITEM_SQL`'s
/// `COALESCE` leaves each item's existing `library_id` untouched.
#[cfg(test)]
pub(crate) fn apply_upsert_items(
    conn: &mut Connection,
    items: &[BaseItemDto],
) -> rusqlite::Result<Vec<String>> {
    apply_upsert_items_scoped(conn, items, None)
}

/// Upsert a batch of items in one transaction, maintaining `search` (the
/// contentless FTS5 index) in lockstep: each write reads the row's *old*
/// blob first (if any) so it can issue a matching FTS `delete` before the
/// new `insert` — contentless FTS5 tables need the original column values
/// to remove the right postings, since they don't retain text themselves.
///
/// `library_id`, when `Some`, is stamped onto every item in `items` --
/// see `WriteCmd::UpsertItems`'s doc comment for when callers pass `None`
/// instead.
pub(crate) fn apply_upsert_items_scoped(
    conn: &mut Connection,
    items: &[BaseItemDto],
    library_id: Option<&str>,
) -> rusqlite::Result<Vec<String>> {
    let tx = conn.transaction()?;
    let mut upserted = Vec::with_capacity(items.len());
    for item in items {
        let Some(cols) = extract_columns(item) else {
            tracing::warn!("skipping item with no id");
            continue;
        };

        let old: Option<(i64, Vec<u8>)> = tx
            .query_row(
                "SELECT rowid, dto FROM items WHERE id = ?1",
                [&cols.id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;

        let dto_bytes = to_dto_bytes(item);
        let now = now_millis();

        let rowid: i64 = tx.query_row(
            UPSERT_ITEM_SQL,
            params![
                cols.id,
                cols.parent_id,
                cols.series_id,
                cols.season_id,
                cols.item_type,
                cols.name,
                cols.sort_name,
                cols.index_number,
                cols.parent_index_number,
                cols.production_year,
                cols.premiere_date,
                cols.runtime_ticks,
                cols.date_created,
                cols.played,
                cols.playback_position_ticks,
                cols.play_count,
                cols.is_favorite,
                cols.unplayed_item_count,
                cols.primary_tag,
                cols.primary_blurhash,
                cols.series_primary_tag,
                cols.parent_backdrop_item_id,
                cols.parent_backdrop_tag,
                library_id,
                cols.last_played_date,
                cols.overview,
                cols.is_virtual,
                cols.series_name,
                dto_bytes,
                now,
            ],
            |row| row.get(0),
        )?;

        if let Some((old_rowid, old_blob)) = old {
            debug_assert_eq!(
                old_rowid, rowid,
                "upsert must preserve rowid for stable FTS mapping"
            );
            if let Ok(old_item) = serde_json::from_slice::<BaseItemDto>(&old_blob) {
                fts_delete(&tx, rowid, &search_text(&old_item))?;
            }
        }
        fts_insert(&tx, rowid, &search_text(item))?;

        upserted.push(cols.id);
    }
    tx.commit()?;
    Ok(upserted)
}

/// Fold one item's live enrichment fetch into the row the bulk sync already
/// wrote, so the Detail page's spec strip paints in full from disk on every
/// revisit instead of waiting out a network round trip each time.
///
/// Three invariants, all of them the reason this isn't just a one-item
/// `apply_upsert_items_scoped` call:
///
///  1. **No column is rescoped.** Only `dto` (and `overview`) is written;
///     `library_id`, `parent_id`, image tags, and `date_created` keep the
///     values the enrichment projection never asked the server for.
///  2. **Unknown items are skipped**, not inserted -- a row the mirror
///     doesn't have would have to be invented from a partial projection.
///  3. **Idempotent.** A graft that changes nothing returns `None`, so no
///     `MirrorChange` is emitted.
///
/// Returns the item id exactly when the row actually changed.
pub(crate) fn apply_upsert_enriched_item(
    conn: &mut Connection,
    item: &BaseItemDto,
) -> rusqlite::Result<Option<String>> {
    let Some(id) = item.id.map(|u| u.to_string()) else {
        return Ok(None);
    };

    let tx = conn.transaction()?;
    let existing: Option<(i64, Vec<u8>)> = tx
        .query_row("SELECT rowid, dto FROM items WHERE id = ?1", [&id], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })
        .optional()?;
    let Some((rowid, stored_blob)) = existing else {
        tracing::debug!(
            id,
            "enrichment for an item the mirror doesn't have; skipping"
        );
        return Ok(None);
    };
    let Ok(mut stored) = serde_json::from_slice::<BaseItemDto>(&stored_blob) else {
        tracing::warn!(id, "stored dto blob is unparseable; skipping enrichment");
        return Ok(None);
    };

    let old_text = search_text(&stored);
    crate::rows::graft_enrichment(&mut stored, item);
    let new_blob = to_dto_bytes(&stored);
    if new_blob == stored_blob {
        return Ok(None);
    }

    tx.execute(
        "UPDATE items SET dto = ?1, overview = ?2, updated_at = ?3 WHERE id = ?4",
        params![new_blob, stored.overview, now_millis(), id],
    )?;

    // `overview` is an FTS column, so a first-time overview from the
    // enrichment fetch has to reach the index too -- same
    // delete-old-then-insert-new dance `apply_upsert_items_scoped` does.
    let new_text = search_text(&stored);
    if new_text != old_text {
        fts_delete(&tx, rowid, &old_text)?;
        fts_insert(&tx, rowid, &new_text)?;
    }

    tx.commit()?;
    Ok(Some(id))
}

fn fts_insert(
    tx: &rusqlite::Transaction<'_>,
    rowid: i64,
    text: &SearchText,
) -> rusqlite::Result<()> {
    tx.execute(
        "INSERT INTO search (rowid, name, original_title, series_name, overview) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![rowid, text.name, text.original_title, text.series_name, text.overview],
    )?;
    Ok(())
}

fn fts_delete(
    tx: &rusqlite::Transaction<'_>,
    rowid: i64,
    text: &SearchText,
) -> rusqlite::Result<()> {
    tx.execute(
        "INSERT INTO search (search, rowid, name, original_title, series_name, overview) VALUES ('delete', ?1, ?2, ?3, ?4, ?5)",
        params![rowid, text.name, text.original_title, text.series_name, text.overview],
    )?;
    Ok(())
}

/// Delete one item row in full: the matching FTS `delete` (parsed from its
/// stored blob), the `items` row itself, and any `collection_members` row
/// naming it either as a BoxSet (`collection_id`) or a member (`item_id`).
/// Shared by every cascade-delete path (`apply_upsert_views`'s stale-view
/// cascade, `apply_remove_items`, `apply_prune_library`) so the three can't
/// drift out of lockstep.
fn delete_item_row(
    tx: &rusqlite::Transaction<'_>,
    rowid: i64,
    id: &str,
    blob: &[u8],
) -> rusqlite::Result<()> {
    if let Ok(old_item) = serde_json::from_slice::<BaseItemDto>(blob) {
        fts_delete(tx, rowid, &search_text(&old_item))?;
    }
    tx.execute("DELETE FROM items WHERE id = ?1", [id])?;
    tx.execute(
        "DELETE FROM collection_members WHERE collection_id = ?1 OR item_id = ?1",
        [id],
    )?;
    Ok(())
}

/// Delete items by id, cascading to any rows that reference a removed id via
/// `parent_id`/`series_id`/`season_id` (docs/DATA.md §2: "removals delete
/// (cascade by series_id/parent_id where the server signals folder
/// removal)"). Returns every id actually removed (seed ids + cascaded).
pub(crate) fn apply_remove_items(
    conn: &mut Connection,
    ids: &[String],
) -> rusqlite::Result<Vec<String>> {
    let tx = conn.transaction()?;
    let mut to_delete: std::collections::HashSet<String> = ids.iter().cloned().collect();

    // Fixpoint over cascaded children; the parent/series/season graph in
    // practice is at most a few levels deep (Series -> Season -> Episode),
    // so this converges fast.
    loop {
        let frontier: Vec<String> = to_delete.iter().cloned().collect();
        let mut found = Vec::new();
        for column in ["parent_id", "series_id", "season_id"] {
            let sql = format!(
                "SELECT id FROM items WHERE {column} IN ({})",
                std::iter::repeat_n("?", frontier.len())
                    .collect::<Vec<_>>()
                    .join(",")
            );
            let mut stmt = tx.prepare(&sql)?;
            let rows = stmt.query_map(rusqlite::params_from_iter(frontier.iter()), |row| {
                row.get::<_, String>(0)
            })?;
            for r in rows {
                found.push(r?);
            }
        }
        let mut grew = false;
        for id in found {
            if to_delete.insert(id) {
                grew = true;
            }
        }
        if !grew {
            break;
        }
    }

    let mut removed = Vec::with_capacity(to_delete.len());
    for id in &to_delete {
        let old: Option<(i64, Vec<u8>)> = tx
            .query_row("SELECT rowid, dto FROM items WHERE id = ?1", [id], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
            .optional()?;
        let Some((rowid, blob)) = old else { continue };
        delete_item_row(&tx, rowid, id, &blob)?;
        removed.push(id.clone());
    }
    tx.commit()?;
    Ok(removed)
}

/// See [`WriteCmd::FlattenRootParents`]: returns the ids whose `parent_id`
/// actually changed, so the caller can announce exactly those rows.
pub(crate) fn apply_flatten_root_parents(
    conn: &mut Connection,
    library_id: &str,
    item_type: &str,
) -> rusqlite::Result<Vec<String>> {
    let tx = conn.transaction()?;
    let ids: Vec<String> = {
        let mut stmt = tx.prepare(
            "SELECT id FROM items WHERE library_id = ?1 AND item_type = ?2 \
             AND (parent_id IS NULL OR parent_id != ?1)",
        )?;
        let rows = stmt.query_map([library_id, item_type], |row| row.get(0))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    if !ids.is_empty() {
        tx.execute(
            "UPDATE items SET parent_id = ?1 WHERE library_id = ?1 AND item_type = ?2 \
             AND (parent_id IS NULL OR parent_id != ?1)",
            [library_id, item_type],
        )?;
    }
    tx.commit()?;
    Ok(ids)
}

/// Prune stale rows belonging to one library (see [`WriteCmd::PruneLibrary`]).
///
/// Deliberately not the cascade-by-parent/series/season logic
/// `apply_remove_items` uses: `keep_ids` comes from a completed recursive
/// breadth sync, already a complete membership snapshot, so a child whose
/// parent is gone is itself already missing from `keep_ids` too, no graph
/// walk required. Scoped to `library_id` throughout, so another library's
/// rows are never touched.
pub(crate) fn apply_prune_library(
    conn: &mut Connection,
    library_id: &str,
    keep_ids: &[String],
) -> rusqlite::Result<Vec<String>> {
    let tx = conn.transaction()?;

    // `idx_items_latest`'s leading column is `library_id` (schema.rs), so
    // this is an index range scan, not a full `items` table scan -- the
    // "rare full pass over one library's ids during a resync" the pruning
    // contract explicitly allows, not a hot browse-path query.
    let local: Vec<(i64, String, Vec<u8>)> = {
        let mut stmt = tx.prepare("SELECT rowid, id, dto FROM items WHERE library_id = ?1")?;
        let rows = stmt.query_map([library_id], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };

    let keep: std::collections::HashSet<&str> = keep_ids.iter().map(String::as_str).collect();
    let mut removed = Vec::new();
    for (rowid, id, blob) in &local {
        if keep.contains(id.as_str()) {
            continue;
        }
        delete_item_row(&tx, *rowid, id, blob)?;
        removed.push(id.clone());
    }

    tx.commit()?;
    Ok(removed)
}

/// Replace the full membership list of one BoxSet: delete-then-reinsert in
/// one transaction, since the server response this is built from is always
/// the complete, authoritative member list, not a delta. `pub(crate)` so
/// `query.rs`'s tests can seed membership directly.
pub(crate) fn apply_set_collection_members(
    conn: &mut Connection,
    collection_id: &str,
    members: &[(String, i64)],
) -> rusqlite::Result<()> {
    let tx = conn.transaction()?;
    tx.execute(
        "DELETE FROM collection_members WHERE collection_id = ?1",
        [collection_id],
    )?;
    for (item_id, sort_index) in members {
        tx.execute(
            "INSERT INTO collection_members (collection_id, item_id, sort_index) VALUES (?1, ?2, ?3)
             ON CONFLICT(collection_id, item_id) DO UPDATE SET sort_index = excluded.sort_index",
            params![collection_id, item_id, sort_index],
        )?;
    }
    tx.commit()
}

/// `UserDataChanged` application: patches only the columns present in the
/// event (docs/DATA.md §2), leaving everything else — including the blob — as
/// is. `COALESCE(?, column)` lets `None` fields pass through unchanged.
pub(crate) fn apply_user_data(
    conn: &mut Connection,
    updates: &[(String, UserItemDataDto)],
) -> rusqlite::Result<Vec<String>> {
    let tx = conn.transaction()?;
    let mut touched = Vec::with_capacity(updates.len());
    for (item_id, data) in updates {
        let changed = tx.execute(
            "UPDATE items SET
                played = COALESCE(?1, played),
                playback_position_ticks = COALESCE(?2, playback_position_ticks),
                play_count = COALESCE(?3, play_count),
                is_favorite = COALESCE(?4, is_favorite),
                unplayed_item_count = COALESCE(?5, unplayed_item_count),
                last_played_date = COALESCE(?6, last_played_date),
                updated_at = ?7
             WHERE id = ?8",
            params![
                data.played,
                data.playback_position_ticks,
                data.play_count,
                data.is_favorite,
                data.unplayed_item_count,
                data.last_played_date.map(|d| d.to_rfc3339()),
                now_millis(),
                item_id,
            ],
        )?;
        if changed > 0 {
            touched.push(item_id.clone());
        } else {
            // Not an error: the server can send UserData for an item this
            // mirror hasn't synced yet (a delta racing ahead of breadth
            // sync).
            tracing::debug!(
                item_id,
                "UserDataChanged for an item not present in the mirror; ignoring"
            );
        }
    }
    tx.commit()?;
    Ok(touched)
}

/// Jellyfin's server-side "mark played" threshold: a stop/progress report
/// whose position is at least this fraction of the item's runtime is treated
/// as a completed watch -- position resets to 0 and `played` flips true,
/// mirroring what the server itself does with `UserData` (jellyfin-server's
/// `UpdatePlayedStatus` / `PlaystateController`). Kept as a free function
/// (not inlined into `apply_local_user_data`) so it's unit-testable without
/// a database at all.
const PLAYED_THRESHOLD: f64 = 0.9;

/// Given a raw reported position and an optional explicit `played` override
/// (`Some(true)` at real EOF, `None` for an ordinary progress/stop report),
/// decides the position/played pair to actually write, mirroring the
/// server's own played-threshold behavior locally so the mirror doesn't
/// have to wait for a `UserDataChanged` push that doesn't reliably arrive
/// for the reporting session's own events.
///
/// `runtime_ticks` of `None` or `<= 0` (unknown runtime) can't be compared
/// against, so only the explicit override applies in that case.
fn resolve_played_position(
    position_ticks: i64,
    played_override: Option<bool>,
    runtime_ticks: Option<i64>,
    current_played: bool,
) -> (i64, bool) {
    let crossed_threshold = match runtime_ticks {
        Some(rt) if rt > 0 => (position_ticks as f64 / rt as f64) >= PLAYED_THRESHOLD,
        _ => false,
    };
    if played_override == Some(true) || crossed_threshold {
        (0, true)
    } else {
        (
            position_ticks.max(0),
            played_override.unwrap_or(current_played),
        )
    }
}

/// Optimistic local application of a watch-state update this client itself
/// just reported to the server. Unlike `apply_user_data` (which patches
/// only fields an incoming `UserDataChanged` event carries), this always
/// knows both `position_ticks` and whether the played-threshold was
/// crossed -- see `resolve_played_position`. Returns the item id if a row
/// was found and updated.
pub(crate) fn apply_local_user_data(
    conn: &mut Connection,
    item_id: &str,
    position_ticks: i64,
    played_override: Option<bool>,
) -> rusqlite::Result<Option<String>> {
    let tx = conn.transaction()?;
    let existing: Option<(Option<i64>, bool)> = tx
        .query_row(
            "SELECT runtime_ticks, played FROM items WHERE id = ?1",
            [item_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let Some((runtime_ticks, current_played)) = existing else {
        tracing::debug!(
            item_id,
            "apply_local_user_data for an item not present in the mirror; ignoring"
        );
        return Ok(None);
    };

    let (position_ticks, played) = resolve_played_position(
        position_ticks,
        played_override,
        runtime_ticks,
        current_played,
    );

    // Stamp `last_played_date` to "now" on our own reports -- `resume()`
    // sorts by this column, so a fresh stop/EOF must sort ahead immediately,
    // not wait on a server round trip that doesn't reliably arrive.
    let last_played_date = chrono::Utc::now().to_rfc3339();
    tx.execute(
        "UPDATE items SET playback_position_ticks = ?1, played = ?2, last_played_date = ?3, updated_at = ?4 WHERE id = ?5",
        params![position_ticks, played, last_played_date, now_millis(), item_id],
    )?;
    tx.commit()?;
    Ok(Some(item_id.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{open_and_prepare, open_test_db};
    use jellyfin_api::models::BaseItemKind;

    /// Pins: `barrier()` (chained behind `apply_local_user_data_and_wait`'s
    /// send) actually means "committed", not merely "enqueued" -- it rides
    /// the same strict-FIFO queue, so when it returns every earlier command
    /// has been applied. Asserted through a SECOND connection to the same
    /// db file, since the writer owns its own.
    #[tokio::test(flavor = "multi_thread")]
    async fn barrier_returns_only_after_earlier_commands_have_committed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("mirror.db");
        let (conn, _) = open_and_prepare(&path).expect("open");

        let (tx, rx) = mpsc::channel(64);
        let (changes, _changes_rx) = broadcast::channel(64);
        let writer = tokio::task::spawn_blocking(move || run(conn, rx, changes));
        let handle = WriterHandle::new(tx);

        let id = "e2f5a5f1-1a0b-4b3a-9c2e-000000000001";
        // A batch ahead of the position write, so the barrier has something
        // to actually wait behind (the quit-time shape this fixes).
        for n in 0..16 {
            handle
                .upsert_items(vec![item(id, &format!("A Movie {n}"))])
                .await;
        }
        handle
            .apply_local_user_data(id.to_string(), 4_200, None)
            .await;
        handle.barrier().await;

        let reader = Connection::open(&path).expect("second connection");
        let position: i64 = reader
            .query_row(
                "SELECT playback_position_ticks FROM items WHERE id = ?1",
                [id],
                |r| r.get(0),
            )
            .expect("row exists and the position write has committed");
        assert_eq!(position, 4_200);

        drop(handle);
        let _ = writer.await;
    }

    fn item(id: &str, name: &str) -> BaseItemDto {
        BaseItemDto {
            id: Some(uuid::Uuid::parse_str(id).expect("uuid")),
            name: Some(name.to_string()),
            type_: Some(BaseItemKind::Movie),
            ..Default::default()
        }
    }

    /// Pins: `FlattenRootParents` re-parents exactly the wrong rows of the named type onto the library id, and leaves correct rows (and other types) alone.
    #[test]
    fn flatten_root_parents_reparents_only_the_misfiled_rows_of_that_type() {
        let (_dir, mut conn) = open_test_db();
        let view = "b82b87a0-e136-9ef5-2c0b-02151e59416e";
        let folder = "b5eda0f3-63d6-50d2-de88-28c1ea665111";
        let view_uuid = uuid::Uuid::parse_str(view).expect("uuid");
        let folder_uuid = uuid::Uuid::parse_str(folder).expect("uuid");
        let mut ok = item("e2f5a5f1-1a0b-4b3a-9c2e-000000000010", "Suits");
        ok.type_ = Some(BaseItemKind::Series);
        ok.parent_id = Some(view_uuid);
        let mut misfiled = item("e2f5a5f1-1a0b-4b3a-9c2e-000000000011", "Castle");
        misfiled.type_ = Some(BaseItemKind::Series);
        misfiled.parent_id = Some(folder_uuid);
        // An Episode under a Season must never be touched by a Series heal.
        let mut episode = item(
            "e2f5a5f1-1a0b-4b3a-9c2e-000000000012",
            "Flowers for Your Grave",
        );
        episode.type_ = Some(BaseItemKind::Episode);
        episode.parent_id = Some(folder_uuid);
        apply_upsert_items_scoped(&mut conn, &[ok, misfiled, episode], Some(view)).expect("upsert");

        let healed = apply_flatten_root_parents(&mut conn, view, "Series").expect("heal");
        assert_eq!(
            healed,
            vec!["e2f5a5f1-1a0b-4b3a-9c2e-000000000011".to_string()]
        );

        let parent_of = |id: &str| -> String {
            conn.query_row("SELECT parent_id FROM items WHERE id = ?1", [id], |row| {
                row.get(0)
            })
            .expect("row")
        };
        assert_eq!(parent_of("e2f5a5f1-1a0b-4b3a-9c2e-000000000011"), view);
        assert_eq!(parent_of("e2f5a5f1-1a0b-4b3a-9c2e-000000000010"), view);
        assert_eq!(parent_of("e2f5a5f1-1a0b-4b3a-9c2e-000000000012"), folder);

        // Idempotent: a second pass changes nothing and reports nothing.
        let again = apply_flatten_root_parents(&mut conn, view, "Series").expect("heal");
        assert!(again.is_empty());
    }

    /// A `Fields=MediaStreams,...` enrichment response: the narrow
    /// projection the Detail page fetches. Note what it does NOT carry --
    /// no `DateCreated`, no `ParentId`, no image tags -- exactly why this
    /// can't be a plain one-item upsert.
    fn enrichment(id: &str) -> BaseItemDto {
        BaseItemDto {
            id: Some(uuid::Uuid::parse_str(id).expect("uuid")),
            media_streams: vec![Default::default()],
            media_sources: vec![Default::default()],
            overview: Some("A synopsis.".to_string()),
            genres: vec!["Drama".to_string()],
            ..Default::default()
        }
    }

    fn stored_dto(conn: &Connection, id: &str) -> BaseItemDto {
        let blob: Vec<u8> = conn
            .query_row("SELECT dto FROM items WHERE id = ?1", [id], |r| r.get(0))
            .expect("row exists");
        serde_json::from_slice(&blob).expect("stored dto parses")
    }

    /// Pins: enrichment adds streams to the blob and changes NOTHING else about the row (a rescoped `library_id`/`parent_id` would drop the item from queries).
    #[test]
    fn enriched_upsert_preserves_library_id_and_every_other_column() {
        let (_dir, mut conn) = open_test_db();
        let library_id = "e2f5a5f1-1a0b-4b3a-9c2e-0000000000f0";
        let id = "e2f5a5f1-1a0b-4b3a-9c2e-0000000000f1";

        let mut synced = item_in_library(id, "A Movie");
        synced.parent_id = Some(uuid::Uuid::parse_str(library_id).expect("uuid"));
        synced.date_created = Some("2020-01-01T00:00:00Z".parse().expect("timestamp"));
        apply_upsert_items_scoped(&mut conn, &[synced], Some(library_id)).expect("seed");

        let changed = apply_upsert_enriched_item(&mut conn, &enrichment(id)).expect("enrich");
        assert_eq!(changed.as_deref(), Some(id), "the row changed");

        let (stored_library, stored_parent, stored_created, stored_name): (
            Option<String>,
            Option<String>,
            Option<String>,
            Option<String>,
        ) = conn
            .query_row(
                "SELECT library_id, parent_id, date_created, name FROM items WHERE id = ?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .expect("row exists");
        assert_eq!(
            stored_library.as_deref(),
            Some(library_id),
            "enrichment must never rescope the row's library"
        );
        assert_eq!(stored_parent.as_deref(), Some(library_id));
        assert!(
            stored_created.is_some(),
            "columns the enrichment projection can't speak for must survive"
        );
        assert_eq!(stored_name.as_deref(), Some("A Movie"));

        // ...and the blob really did gain the enrichment fields, on top of
        // the synced row's own (the name it never re-sent).
        let dto = stored_dto(&conn, id);
        assert_eq!(dto.media_streams.len(), 1);
        assert_eq!(dto.media_sources.len(), 1);
        assert_eq!(dto.overview.as_deref(), Some("A synopsis."));
        assert_eq!(dto.genres, vec!["Drama".to_string()]);
        assert_eq!(
            dto.name.as_deref(),
            Some("A Movie"),
            "the stored row wins for every non-enrichment field"
        );
    }

    /// Pins: a second identical enrichment is a no-op that emits no change.
    #[test]
    fn enriched_upsert_is_idempotent_and_skips_unknown_items() {
        let (_dir, mut conn) = open_test_db();
        let id = "e2f5a5f1-1a0b-4b3a-9c2e-0000000000f2";
        apply_upsert_items(&mut conn, &[item(id, "A Movie")]).expect("seed");

        assert_eq!(
            apply_upsert_enriched_item(&mut conn, &enrichment(id)).expect("first enrich"),
            Some(id.to_string())
        );
        assert_eq!(
            apply_upsert_enriched_item(&mut conn, &enrichment(id)).expect("second enrich"),
            None,
            "an enrichment that changes nothing must emit no change event"
        );

        // An item the mirror has never synced is skipped outright rather
        // than invented from a partial projection.
        let unknown = "e2f5a5f1-1a0b-4b3a-9c2e-0000000000f3";
        assert_eq!(
            apply_upsert_enriched_item(&mut conn, &enrichment(unknown)).expect("unknown"),
            None
        );
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM items", [], |r| r.get(0))
            .expect("count");
        assert_eq!(count, 1, "no row was invented for the unknown item");
    }

    /// `overview` is both a column and an FTS field, and the enrichment
    /// fetch is often the first place it arrives -- both have to follow.
    #[test]
    fn enriched_upsert_updates_the_overview_column_and_search_index() {
        let (_dir, mut conn) = open_test_db();
        let id = "e2f5a5f1-1a0b-4b3a-9c2e-0000000000f4";
        apply_upsert_items(&mut conn, &[item(id, "A Movie")]).expect("seed");

        let synopsis_hits = |conn: &Connection| -> i64 {
            conn.query_row(
                "SELECT COUNT(*) FROM search WHERE search MATCH 'synopsis'",
                [],
                |r| r.get(0),
            )
            .expect("fts query")
        };
        assert_eq!(synopsis_hits(&conn), 0);

        apply_upsert_enriched_item(&mut conn, &enrichment(id)).expect("enrich");

        let overview: Option<String> = conn
            .query_row("SELECT overview FROM items WHERE id = ?1", [id], |r| {
                r.get(0)
            })
            .expect("row exists");
        assert_eq!(overview.as_deref(), Some("A synopsis."));
        assert_eq!(
            synopsis_hits(&conn),
            1,
            "an overview arriving via enrichment must reach the search index"
        );
    }

    #[test]
    fn upsert_then_query_roundtrips() {
        let (_dir, mut conn) = open_test_db();
        let ids = apply_upsert_items(
            &mut conn,
            &[item("e2f5a5f1-1a0b-4b3a-9c2e-000000000001", "A Movie")],
        )
        .expect("upsert");
        assert_eq!(
            ids,
            vec!["e2f5a5f1-1a0b-4b3a-9c2e-000000000001".to_string()]
        );

        let name: String = conn
            .query_row(
                "SELECT name FROM items WHERE id = ?1",
                ["e2f5a5f1-1a0b-4b3a-9c2e-000000000001"],
                |r| r.get(0),
            )
            .expect("row exists");
        assert_eq!(name, "A Movie");
    }

    #[test]
    fn upsert_preserves_rowid_across_updates() {
        let (_dir, mut conn) = open_test_db();
        let id = "e2f5a5f1-1a0b-4b3a-9c2e-000000000001";
        apply_upsert_items(&mut conn, &[item(id, "First")]).expect("insert");
        let rowid1: i64 = conn
            .query_row("SELECT rowid FROM items WHERE id = ?1", [id], |r| r.get(0))
            .expect("rowid");

        apply_upsert_items(&mut conn, &[item(id, "Renamed")]).expect("update");
        let rowid2: i64 = conn
            .query_row("SELECT rowid FROM items WHERE id = ?1", [id], |r| r.get(0))
            .expect("rowid");

        assert_eq!(
            rowid1, rowid2,
            "ON CONFLICT DO UPDATE must not reassign rowid (FTS depends on stable rowid)"
        );
    }

    #[test]
    fn remove_items_deletes_row() {
        let (_dir, mut conn) = open_test_db();
        let id = "e2f5a5f1-1a0b-4b3a-9c2e-000000000001".to_string();
        apply_upsert_items(&mut conn, &[item(&id, "Doomed")]).expect("insert");
        let removed = apply_remove_items(&mut conn, std::slice::from_ref(&id)).expect("remove");
        assert_eq!(removed, vec![id.clone()]);
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM items WHERE id = ?1", [&id], |r| {
                r.get(0)
            })
            .expect("count");
        assert_eq!(count, 0);
    }

    #[test]
    fn remove_cascades_to_children_by_series_and_parent() {
        let (_dir, mut conn) = open_test_db();
        let series_id = "e2f5a5f1-1a0b-4b3a-9c2e-000000000010";
        let season_id = "e2f5a5f1-1a0b-4b3a-9c2e-000000000011";
        let episode_id = "e2f5a5f1-1a0b-4b3a-9c2e-000000000012";

        let series = item(series_id, "The Show");
        let mut season = item(season_id, "Season 1");
        season.series_id = Some(uuid::Uuid::parse_str(series_id).expect("uuid"));
        let mut episode = item(episode_id, "Episode 1");
        episode.series_id = Some(uuid::Uuid::parse_str(series_id).expect("uuid"));
        episode.season_id = Some(uuid::Uuid::parse_str(season_id).expect("uuid"));

        apply_upsert_items(&mut conn, &[series, season, episode]).expect("insert all");

        let removed =
            apply_remove_items(&mut conn, &[series_id.to_string()]).expect("remove series");
        let mut removed_sorted = removed;
        removed_sorted.sort();
        let mut expected = vec![
            series_id.to_string(),
            season_id.to_string(),
            episode_id.to_string(),
        ];
        expected.sort();
        assert_eq!(removed_sorted, expected);

        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM items", [], |r| r.get(0))
            .expect("count");
        assert_eq!(count, 0);
    }

    fn item_in_library(id: &str, name: &str) -> BaseItemDto {
        item(id, name)
    }

    #[test]
    fn prune_library_deletes_rows_the_server_no_longer_returns() {
        let (_dir, mut conn) = open_test_db();
        let library_id = "e2f5a5f1-1a0b-4b3a-9c2e-0000000000a0";
        let keep_id = "e2f5a5f1-1a0b-4b3a-9c2e-0000000000a1".to_string();
        let stale_id = "e2f5a5f1-1a0b-4b3a-9c2e-0000000000a2".to_string();

        apply_upsert_items_scoped(
            &mut conn,
            &[
                item_in_library(&keep_id, "Kept"),
                item_in_library(&stale_id, "Orphaned virtual placeholder"),
            ],
            Some(library_id),
        )
        .expect("seed items");

        let removed = apply_prune_library(&mut conn, library_id, std::slice::from_ref(&keep_id))
            .expect("prune");
        assert_eq!(removed, vec![stale_id.clone()]);

        let ids: Vec<String> = {
            let mut stmt = conn
                .prepare("SELECT id FROM items ORDER BY id")
                .expect("prep");
            stmt.query_map([], |r| r.get(0))
                .expect("query")
                .collect::<rusqlite::Result<_>>()
                .expect("rows")
        };
        assert_eq!(ids, vec![keep_id]);
    }

    /// Pins: both a virtual placeholder and its real-item replacement coexist locally until a resync's `keep_ids` prunes the dead one.
    #[test]
    fn prune_library_converges_a_virtual_to_real_id_swap() {
        let (_dir, mut conn) = open_test_db();
        let library_id = "e2f5a5f1-1a0b-4b3a-9c2e-0000000000b0";
        let old_virtual_id = "e2f5a5f1-1a0b-4b3a-9c2e-0000000000b1".to_string();
        let new_real_id = "e2f5a5f1-1a0b-4b3a-9c2e-0000000000b2".to_string();

        let mut virtual_ep = item(&old_virtual_id, "S01E01");
        virtual_ep.location_type = Some(jellyfin_api::models::LocationType::Virtual);
        apply_upsert_items_scoped(&mut conn, &[virtual_ep], Some(library_id))
            .expect("seed virtual placeholder");

        // The resync's breadth fetch lands the new real item (a plain
        // upsert -- different id, so it doesn't overwrite the old row)...
        apply_upsert_items_scoped(&mut conn, &[item(&new_real_id, "S01E01")], Some(library_id))
            .expect("upsert real item");

        // ...and then, per `sync_library_breadth`, prunes with that fetch's
        // complete id set -- which no longer includes the dead virtual id.
        let removed =
            apply_prune_library(&mut conn, library_id, std::slice::from_ref(&new_real_id))
                .expect("prune");
        assert_eq!(removed, vec![old_virtual_id.clone()]);

        let (count, only_id, is_virtual): (i64, String, bool) = conn
            .query_row(
                "SELECT COUNT(*), MIN(id), MIN(is_virtual) FROM items",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get::<_, i64>(2)? != 0)),
            )
            .expect("row");
        assert_eq!(
            count, 1,
            "old virtual row must be gone, only the real one remains"
        );
        assert_eq!(only_id, new_real_id);
        assert!(
            !is_virtual,
            "surviving row must be the real (non-virtual) item"
        );
    }

    #[test]
    fn prune_library_is_scoped_to_one_library_id() {
        let (_dir, mut conn) = open_test_db();
        let library_a = "e2f5a5f1-1a0b-4b3a-9c2e-0000000000c0";
        let library_b = "e2f5a5f1-1a0b-4b3a-9c2e-0000000000c1";
        let item_a = "e2f5a5f1-1a0b-4b3a-9c2e-0000000000c2".to_string();
        let item_b = "e2f5a5f1-1a0b-4b3a-9c2e-0000000000c3".to_string();

        apply_upsert_items_scoped(&mut conn, &[item_in_library(&item_a, "A")], Some(library_a))
            .expect("seed a");
        apply_upsert_items_scoped(&mut conn, &[item_in_library(&item_b, "B")], Some(library_b))
            .expect("seed b");

        // Pruning library A with an empty keep set must delete item_a but
        // must never touch library B's row, even though it wasn't mentioned
        // at all.
        let removed = apply_prune_library(&mut conn, library_a, &[]).expect("prune a");
        assert_eq!(removed, vec![item_a]);

        let remaining: Vec<String> = {
            let mut stmt = conn.prepare("SELECT id FROM items").expect("prep");
            stmt.query_map([], |r| r.get(0))
                .expect("query")
                .collect::<rusqlite::Result<_>>()
                .expect("rows")
        };
        assert_eq!(
            remaining,
            vec![item_b],
            "library B's row must survive untouched"
        );
    }

    #[test]
    fn prune_library_cleans_up_fts_and_collection_membership() {
        let (_dir, mut conn) = open_test_db();
        let library_id = "e2f5a5f1-1a0b-4b3a-9c2e-0000000000d0";
        let stale_id = "e2f5a5f1-1a0b-4b3a-9c2e-0000000000d1".to_string();

        apply_upsert_items_scoped(
            &mut conn,
            &[item_in_library(&stale_id, "Quantum Static")],
            Some(library_id),
        )
        .expect("seed");
        apply_set_collection_members(&mut conn, "some-boxset", &[(stale_id.clone(), 0)])
            .expect("membership");

        apply_prune_library(&mut conn, library_id, &[]).expect("prune");

        let hits: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM search WHERE search MATCH 'Quantum'",
                [],
                |r| r.get(0),
            )
            .expect("fts query");
        assert_eq!(hits, 0, "pruned row's FTS postings must be removed");

        let members = member_ids(&conn, "some-boxset");
        assert!(
            members.is_empty(),
            "pruned item's collection membership rows must be removed too"
        );
    }

    #[test]
    fn prune_library_with_no_stale_rows_is_a_noop() {
        let (_dir, mut conn) = open_test_db();
        let library_id = "e2f5a5f1-1a0b-4b3a-9c2e-0000000000e0";
        let id = "e2f5a5f1-1a0b-4b3a-9c2e-0000000000e1".to_string();
        apply_upsert_items_scoped(
            &mut conn,
            &[item_in_library(&id, "Still There")],
            Some(library_id),
        )
        .expect("seed");

        let removed =
            apply_prune_library(&mut conn, library_id, std::slice::from_ref(&id)).expect("prune");
        assert!(removed.is_empty());

        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM items", [], |r| r.get(0))
            .expect("count");
        assert_eq!(count, 1);
    }

    fn view_row(id: &str, name: &str, collection_type: &str) -> ViewRow {
        ViewRow {
            id: id.to_string(),
            name: name.to_string(),
            collection_type: collection_type.to_string(),
            item_type: "CollectionFolder".to_string(),
        }
    }

    fn all_view_ids(conn: &Connection) -> Vec<String> {
        let mut stmt = conn
            .prepare("SELECT id FROM views ORDER BY sort_index ASC")
            .expect("prepare");
        stmt.query_map([], |r| r.get(0))
            .expect("query")
            .collect::<rusqlite::Result<Vec<_>>>()
            .expect("rows")
    }

    fn all_item_ids(conn: &Connection) -> Vec<String> {
        let mut stmt = conn
            .prepare("SELECT id FROM items ORDER BY id")
            .expect("prepare");
        stmt.query_map([], |r| r.get(0))
            .expect("query")
            .collect::<rusqlite::Result<Vec<_>>>()
            .expect("rows")
    }

    /// Pins: a view absent from the next successful `sync_views` is deleted with every item stamped as its `library_id`; a surviving view's items are untouched.
    #[test]
    fn upsert_views_prunes_a_view_absent_from_the_new_list_and_cascades_its_items() {
        let (_dir, mut conn) = open_test_db();
        let kept_view = "e2f5a5f1-1a0b-4b3a-9c2e-0000000000f0";
        let revoked_view = "e2f5a5f1-1a0b-4b3a-9c2e-0000000000f1";
        let kept_item = "e2f5a5f1-1a0b-4b3a-9c2e-0000000000f2".to_string();
        let revoked_item = "e2f5a5f1-1a0b-4b3a-9c2e-0000000000f3".to_string();

        // Both views exist locally, each with one item.
        apply_upsert_views(
            &mut conn,
            &[
                view_row(kept_view, "Movies", "movies"),
                view_row(revoked_view, "Shared TV", "tvshows"),
            ],
        )
        .expect("seed views");
        apply_upsert_items_scoped(
            &mut conn,
            &[item_in_library(&kept_item, "Kept Movie")],
            Some(kept_view),
        )
        .expect("seed kept item");
        apply_upsert_items_scoped(
            &mut conn,
            &[item_in_library(&revoked_item, "Revoked Show")],
            Some(revoked_view),
        )
        .expect("seed revoked item");

        // Next successful fetch only reports the surviving view.
        let removed = apply_upsert_views(&mut conn, &[view_row(kept_view, "Movies", "movies")])
            .expect("reconcile");
        assert_eq!(removed, vec![revoked_item.clone()]);

        assert_eq!(
            all_view_ids(&conn),
            vec![kept_view.to_string()],
            "revoked view must be gone from `views`"
        );
        assert_eq!(
            all_item_ids(&conn),
            vec![kept_item],
            "revoked view's item must be gone, kept view's item must survive untouched"
        );
    }

    /// Pins: `query::views()` no longer surfaces a pruned view.
    #[test]
    fn views_query_no_longer_returns_a_pruned_view() {
        let (_dir, mut conn) = open_test_db();
        let kept_view = "e2f5a5f1-1a0b-4b3a-9c2e-0000000000f4";
        let revoked_view = "e2f5a5f1-1a0b-4b3a-9c2e-0000000000f5";
        apply_upsert_views(
            &mut conn,
            &[
                view_row(kept_view, "Movies", "movies"),
                view_row(revoked_view, "Shared TV", "tvshows"),
            ],
        )
        .expect("seed views");

        apply_upsert_views(&mut conn, &[view_row(kept_view, "Movies", "movies")])
            .expect("reconcile");

        assert_eq!(
            crate::query::views(&conn),
            vec![crate::ViewSummary {
                id: kept_view.to_string(),
                name: "Movies".to_string(),
                kind: crate::ViewKind::Library,
            }]
        );
    }

    /// Pins: running the same authoritative list twice is idempotent.
    #[test]
    fn upsert_views_prune_is_idempotent() {
        let (_dir, mut conn) = open_test_db();
        let kept_view = "e2f5a5f1-1a0b-4b3a-9c2e-0000000000f6";
        let revoked_view = "e2f5a5f1-1a0b-4b3a-9c2e-0000000000f7";
        let revoked_item = "e2f5a5f1-1a0b-4b3a-9c2e-0000000000f8".to_string();
        apply_upsert_views(
            &mut conn,
            &[
                view_row(kept_view, "Movies", "movies"),
                view_row(revoked_view, "Shared TV", "tvshows"),
            ],
        )
        .expect("seed views");
        apply_upsert_items_scoped(
            &mut conn,
            &[item_in_library(&revoked_item, "Revoked Show")],
            Some(revoked_view),
        )
        .expect("seed revoked item");

        let first = apply_upsert_views(&mut conn, &[view_row(kept_view, "Movies", "movies")])
            .expect("first reconcile");
        assert_eq!(first, vec![revoked_item]);

        let second = apply_upsert_views(&mut conn, &[view_row(kept_view, "Movies", "movies")])
            .expect("second reconcile");
        assert!(second.is_empty(), "second identical pass must be a no-op");
        assert_eq!(all_view_ids(&conn), vec![kept_view.to_string()]);
    }

    /// Pins: the view-prune cascade closes the search leak -- `query::search()` has no view/library scoping of its own.
    #[test]
    fn search_no_longer_surfaces_items_from_a_pruned_view() {
        let (_dir, mut conn) = open_test_db();
        let revoked_view = "e2f5a5f1-1a0b-4b3a-9c2e-0000000000f9";
        let revoked_item = "e2f5a5f1-1a0b-4b3a-9c2e-0000000000fa".to_string();
        apply_upsert_views(&mut conn, &[view_row(revoked_view, "Shared TV", "tvshows")])
            .expect("seed view");
        apply_upsert_items_scoped(
            &mut conn,
            &[item_in_library(&revoked_item, "Quantum Static")],
            Some(revoked_view),
        )
        .expect("seed item");

        let hits_before: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM search WHERE search MATCH 'Quantum'",
                [],
                |r| r.get(0),
            )
            .expect("fts query");
        assert_eq!(hits_before, 1, "sanity: item is findable before revocation");

        apply_upsert_views(&mut conn, &[]).expect("revoke access to everything");

        let hits_after: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM search WHERE search MATCH 'Quantum'",
                [],
                |r| r.get(0),
            )
            .expect("fts query");
        assert_eq!(
            hits_after, 0,
            "revoked item's FTS postings must be gone, not just its `items` row"
        );
    }

    /// Pins: an EMPTY view list from a successful fetch is authoritative and prunes every view and item; a failed fetch never reaches this function.
    #[test]
    fn upsert_views_with_empty_successful_list_prunes_everything() {
        let (_dir, mut conn) = open_test_db();
        let view_a = "e2f5a5f1-1a0b-4b3a-9c2e-0000000000fb";
        let view_b = "e2f5a5f1-1a0b-4b3a-9c2e-0000000000fc";
        let item_a = "e2f5a5f1-1a0b-4b3a-9c2e-0000000000fd".to_string();
        let item_b = "e2f5a5f1-1a0b-4b3a-9c2e-0000000000fe".to_string();
        apply_upsert_views(
            &mut conn,
            &[
                view_row(view_a, "Movies", "movies"),
                view_row(view_b, "TV", "tvshows"),
            ],
        )
        .expect("seed views");
        apply_upsert_items_scoped(&mut conn, &[item_in_library(&item_a, "A")], Some(view_a))
            .expect("seed a");
        apply_upsert_items_scoped(&mut conn, &[item_in_library(&item_b, "B")], Some(view_b))
            .expect("seed b");

        let mut removed = apply_upsert_views(&mut conn, &[]).expect("empty reconcile");
        removed.sort();
        let mut expected = vec![item_a, item_b];
        expected.sort();
        assert_eq!(removed, expected);

        assert!(all_view_ids(&conn).is_empty());
        assert!(all_item_ids(&conn).is_empty());
    }

    /// Pins: pruning a view emits both `ViewsChanged` and `Removed` (cascaded item ids) on the change feed, end-to-end through the writer task.
    #[tokio::test(flavor = "multi_thread")]
    async fn upsert_views_emits_views_changed_and_removed_on_prune() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("mirror.db");
        let (conn, _) = open_and_prepare(&path).expect("open");

        let (tx, rx) = mpsc::channel(64);
        let (changes, mut changes_rx) = broadcast::channel(64);
        let writer = tokio::task::spawn_blocking(move || run(conn, rx, changes));
        let handle = WriterHandle::new(tx);

        let kept_view = "e2f5a5f1-1a0b-4b3a-9c2e-0000000000ff";
        let revoked_view = "e2f5a5f1-1a0b-4b3a-9c2e-000000000100";
        let revoked_item = "e2f5a5f1-1a0b-4b3a-9c2e-000000000101".to_string();

        handle
            .upsert_views(vec![
                ViewRow {
                    id: kept_view.to_string(),
                    name: "Movies".to_string(),
                    collection_type: "movies".to_string(),
                    item_type: "CollectionFolder".to_string(),
                },
                ViewRow {
                    id: revoked_view.to_string(),
                    name: "Shared TV".to_string(),
                    collection_type: "tvshows".to_string(),
                    item_type: "CollectionFolder".to_string(),
                },
            ])
            .await;
        handle
            .upsert_items_scoped(
                vec![item(&revoked_item, "Revoked Show")],
                Some(revoked_view.to_string()),
            )
            .await;
        handle.barrier().await;
        // Drain everything the setup above produced so only the prune's own
        // events remain to assert on below.
        while changes_rx.try_recv().is_ok() {}

        handle
            .upsert_views(vec![ViewRow {
                id: kept_view.to_string(),
                name: "Movies".to_string(),
                collection_type: "movies".to_string(),
                item_type: "CollectionFolder".to_string(),
            }])
            .await;
        handle.barrier().await;

        let mut saw_views_changed = false;
        let mut saw_removed = None;
        while let Ok(change) = changes_rx.try_recv() {
            match change {
                MirrorChange::ViewsChanged => saw_views_changed = true,
                MirrorChange::Removed(ids) => saw_removed = Some(ids),
                MirrorChange::Upserted(_) | MirrorChange::Refresh => {}
            }
        }
        assert!(saw_views_changed, "prune must emit ViewsChanged");
        assert_eq!(
            saw_removed,
            Some(vec![revoked_item]),
            "prune must emit Removed with the cascaded item ids"
        );

        drop(handle);
        let _ = writer.await;
    }

    #[test]
    fn apply_user_data_patches_only_present_fields() {
        let (_dir, mut conn) = open_test_db();
        let id = "e2f5a5f1-1a0b-4b3a-9c2e-000000000001".to_string();
        apply_upsert_items(&mut conn, &[item(&id, "Movie")]).expect("insert");

        let update = UserItemDataDto {
            played: Some(true),
            playback_position_ticks: None, // must NOT clobber existing value
            play_count: Some(1),
            is_favorite: None,
            unplayed_item_count: None,
            key: "k".to_string(),
            item_id: None,
            last_played_date: None,
            likes: None,
            played_percentage: None,
            rating: None,
        };
        // Seed a nonzero position first via a direct upsert-with-userdata.
        let mut with_pos = item(&id, "Movie");
        with_pos.user_data = Some(UserItemDataDto {
            played: Some(false),
            playback_position_ticks: Some(555),
            play_count: Some(0),
            is_favorite: Some(false),
            unplayed_item_count: None,
            key: "k".to_string(),
            item_id: None,
            last_played_date: None,
            likes: None,
            played_percentage: None,
            rating: None,
        });
        apply_upsert_items(&mut conn, &[with_pos]).expect("seed position");

        let touched = apply_user_data(&mut conn, &[(id.clone(), update)]).expect("apply");
        assert_eq!(touched, vec![id.clone()]);

        let (played, pos, play_count): (bool, i64, i64) = conn
            .query_row(
                "SELECT played, playback_position_ticks, play_count FROM items WHERE id = ?1",
                [&id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .expect("row");
        assert!(played);
        assert_eq!(pos, 555, "None field must leave existing value untouched");
        assert_eq!(play_count, 1);
    }

    #[test]
    fn apply_user_data_on_unknown_item_is_a_noop() {
        let (_dir, mut conn) = open_test_db();
        let update = UserItemDataDto {
            played: Some(true),
            playback_position_ticks: None,
            play_count: None,
            is_favorite: None,
            unplayed_item_count: None,
            key: "k".to_string(),
            item_id: None,
            last_played_date: None,
            likes: None,
            played_percentage: None,
            rating: None,
        };
        let touched =
            apply_user_data(&mut conn, &[("nonexistent".to_string(), update)]).expect("apply");
        assert!(touched.is_empty());
    }

    #[test]
    fn fts_search_finds_and_forgets_renamed_items() {
        let (_dir, mut conn) = open_test_db();
        let id = "e2f5a5f1-1a0b-4b3a-9c2e-000000000001".to_string();
        apply_upsert_items(&mut conn, &[item(&id, "Quantum Static")]).expect("insert");

        let hits: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM search WHERE search MATCH 'Quantum'",
                [],
                |r| r.get(0),
            )
            .expect("match old name");
        assert_eq!(hits, 1);

        apply_upsert_items(&mut conn, &[item(&id, "Glass Horizon")]).expect("rename");

        let old_hits: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM search WHERE search MATCH 'Quantum'",
                [],
                |r| r.get(0),
            )
            .expect("old name gone");
        assert_eq!(
            old_hits, 0,
            "renamed item must no longer match its old text"
        );

        let new_hits: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM search WHERE search MATCH 'Glass'",
                [],
                |r| r.get(0),
            )
            .expect("match new name");
        assert_eq!(new_hits, 1);
    }

    #[test]
    fn fts_removed_on_item_removal() {
        let (_dir, mut conn) = open_test_db();
        let id = "e2f5a5f1-1a0b-4b3a-9c2e-000000000001".to_string();
        apply_upsert_items(&mut conn, &[item(&id, "Quantum Static")]).expect("insert");
        apply_remove_items(&mut conn, &[id]).expect("remove");

        let hits: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM search WHERE search MATCH 'Quantum'",
                [],
                |r| r.get(0),
            )
            .expect("query");
        assert_eq!(hits, 0);
    }

    fn member_ids(conn: &Connection, collection_id: &str) -> Vec<String> {
        let mut stmt = conn
            .prepare("SELECT item_id FROM collection_members WHERE collection_id = ?1 ORDER BY sort_index")
            .expect("prepare");
        stmt.query_map([collection_id], |r| r.get(0))
            .expect("query")
            .collect::<rusqlite::Result<Vec<_>>>()
            .expect("rows")
    }

    #[test]
    fn set_collection_members_replaces_prior_membership() {
        let (_dir, mut conn) = open_test_db();
        let boxset = "e2f5a5f1-1a0b-4b3a-9c2e-0000000000b0";
        apply_set_collection_members(
            &mut conn,
            boxset,
            &[("a".to_string(), 0), ("b".to_string(), 1)],
        )
        .expect("first set");
        assert_eq!(member_ids(&conn, boxset), vec!["a", "b"]);

        // A second call is the full authoritative list, not a delta: "a" is
        // gone, "c" is new, and the order changed.
        apply_set_collection_members(
            &mut conn,
            boxset,
            &[("c".to_string(), 0), ("b".to_string(), 1)],
        )
        .expect("second set");
        assert_eq!(member_ids(&conn, boxset), vec!["c", "b"]);
    }

    #[test]
    fn removing_a_boxset_drops_its_membership_rows() {
        let (_dir, mut conn) = open_test_db();
        let boxset_id = "e2f5a5f1-1a0b-4b3a-9c2e-0000000000b1".to_string();
        let mut boxset = item(&boxset_id, "A Collection");
        boxset.type_ = Some(BaseItemKind::BoxSet);
        apply_upsert_items(&mut conn, &[boxset]).expect("insert boxset");
        apply_set_collection_members(&mut conn, &boxset_id, &[("member-1".to_string(), 0)])
            .expect("set members");

        apply_remove_items(&mut conn, std::slice::from_ref(&boxset_id)).expect("remove boxset");

        assert!(member_ids(&conn, &boxset_id).is_empty());
    }

    #[test]
    fn removing_a_member_item_drops_its_membership_row() {
        let (_dir, mut conn) = open_test_db();
        let boxset_id = "e2f5a5f1-1a0b-4b3a-9c2e-0000000000b2".to_string();
        let member_id = "e2f5a5f1-1a0b-4b3a-9c2e-0000000000b3".to_string();
        apply_upsert_items(&mut conn, &[item(&member_id, "A Movie")]).expect("insert member");
        apply_set_collection_members(&mut conn, &boxset_id, &[(member_id.clone(), 0)])
            .expect("set members");

        apply_remove_items(&mut conn, &[member_id]).expect("remove member");

        assert!(member_ids(&conn, &boxset_id).is_empty());
    }

    fn item_with_runtime(id: &str, name: &str, runtime_ticks: i64) -> BaseItemDto {
        let mut it = item(id, name);
        it.run_time_ticks = Some(runtime_ticks);
        it
    }

    #[test]
    fn resolve_played_position_below_threshold_keeps_position_and_played_flag() {
        // 50% through a 100-tick item, no explicit override: not played,
        // position passes through unchanged, and the *current* played flag
        // (here, already true from a previous watch) is preserved rather
        // than being reset -- an ordinary progress tick must not un-mark a
        // rewatch as unplayed.
        let (pos, played) = resolve_played_position(50, None, Some(100), true);
        assert_eq!(pos, 50);
        assert!(
            played,
            "None override must preserve the existing played flag"
        );
    }

    #[test]
    fn resolve_played_position_crossing_ninety_percent_marks_played_and_zeroes_position() {
        // Mirrors Jellyfin server's own played-threshold behavior.
        let (pos, played) = resolve_played_position(91, None, Some(100), false);
        assert_eq!(pos, 0);
        assert!(played);
    }

    #[test]
    fn resolve_played_position_just_under_threshold_stays_unplayed() {
        let (pos, played) = resolve_played_position(89, None, Some(100), false);
        assert_eq!(pos, 89);
        assert!(!played);
    }

    #[test]
    fn resolve_played_position_explicit_true_override_forces_played_even_early() {
        // EOF report: caller knows playback genuinely ended, regardless of
        // where the threshold math would land (e.g. runtime metadata is off
        // by a little).
        let (pos, played) = resolve_played_position(10, Some(true), Some(100), false);
        assert_eq!(pos, 0);
        assert!(played);
    }

    #[test]
    fn resolve_played_position_unknown_runtime_only_honors_explicit_override() {
        let (pos, played) = resolve_played_position(999_999, None, None, false);
        assert_eq!(
            pos, 999_999,
            "no runtime to compare against -- pass position through"
        );
        assert!(!played);
    }

    #[test]
    fn apply_local_user_data_updates_existing_item() {
        let (_dir, mut conn) = open_test_db();
        let id = "e2f5a5f1-1a0b-4b3a-9c2e-000000000001".to_string();
        apply_upsert_items(&mut conn, &[item_with_runtime(&id, "Movie", 1_000)]).expect("insert");

        let touched = apply_local_user_data(&mut conn, &id, 500, None).expect("apply");
        assert_eq!(touched, Some(id.clone()));

        let (pos, played): (i64, bool) = conn
            .query_row(
                "SELECT playback_position_ticks, played FROM items WHERE id = ?1",
                [&id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .expect("row");
        assert_eq!(pos, 500);
        assert!(!played);
    }

    #[test]
    fn apply_local_user_data_crossing_threshold_resets_position_in_db() {
        let (_dir, mut conn) = open_test_db();
        let id = "e2f5a5f1-1a0b-4b3a-9c2e-000000000002".to_string();
        apply_upsert_items(&mut conn, &[item_with_runtime(&id, "Movie", 1_000)]).expect("insert");

        apply_local_user_data(&mut conn, &id, 950, None).expect("apply");

        let (pos, played): (i64, bool) = conn
            .query_row(
                "SELECT playback_position_ticks, played FROM items WHERE id = ?1",
                [&id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .expect("row");
        assert_eq!(pos, 0);
        assert!(played);
    }

    #[test]
    fn apply_local_user_data_at_eof_forces_played() {
        let (_dir, mut conn) = open_test_db();
        let id = "e2f5a5f1-1a0b-4b3a-9c2e-000000000003".to_string();
        apply_upsert_items(&mut conn, &[item_with_runtime(&id, "Movie", 1_000)]).expect("insert");

        apply_local_user_data(&mut conn, &id, 1_000, Some(true)).expect("apply");

        let (pos, played): (i64, bool) = conn
            .query_row(
                "SELECT playback_position_ticks, played FROM items WHERE id = ?1",
                [&id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .expect("row");
        assert_eq!(pos, 0);
        assert!(played);
    }

    /// Pins the blast radius of a stale-position write: `apply_local_user_data`
    /// has no ceiling on `position_ticks` relative to the item's own
    /// runtime, so a caller handing it a position belonging to a different,
    /// longer item silently marks this one fully played.
    #[test]
    fn apply_local_user_data_with_a_position_past_runtime_marks_the_item_played() {
        let (_dir, mut conn) = open_test_db();
        let id = "e2f5a5f1-1a0b-4b3a-9c2e-000000000701".to_string();
        // 24 minutes.
        let dto = item_with_runtime(&id, "Short Episode", 24 * 60 * 10_000_000);
        apply_upsert_items(&mut conn, &[dto]).expect("insert");

        // A position that belongs to a 25-minutes-in watch of some OTHER,
        // longer item. No explicit `played` override is passed -- exactly
        // what `stop_playback`/`play_item` pass.
        apply_local_user_data(&mut conn, &id, 25 * 60 * 10_000_000, None).expect("apply");

        let (pos, played): (i64, bool) = conn
            .query_row(
                "SELECT playback_position_ticks, played FROM items WHERE id = ?1",
                [&id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .expect("row");
        assert_eq!(
            pos, 0,
            "position collapses to 0 once the threshold is crossed"
        );
        assert!(
            played,
            "an out-of-range position silently marks a never-watched item played"
        );
    }

    /// Companion to `apply_local_user_data_with_a_position_past_runtime_marks_the_item_played`:
    /// pins that a zeroed position with no explicit `played` override
    /// leaves a never-watched item unplayed.
    #[test]
    fn apply_local_user_data_with_a_zeroed_counter_does_not_mark_the_item_played() {
        let (_dir, mut conn) = open_test_db();
        let id = "e2f5a5f1-1a0b-4b3a-9c2e-000000000702".to_string();
        // 24 minutes -- same runtime as the damage test's item.
        let dto = item_with_runtime(&id, "Unwatched Episode", 24 * 60 * 10_000_000);
        apply_upsert_items(&mut conn, &[dto]).expect("insert");

        // The zeroed counter the fix guarantees at switch/stop time, with the
        // same `None` override `stop_playback`/`play_item` pass.
        apply_local_user_data(&mut conn, &id, 0, None).expect("apply");

        let (pos, played): (i64, bool) = conn
            .query_row(
                "SELECT playback_position_ticks, played FROM items WHERE id = ?1",
                [&id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .expect("row");
        assert_eq!(pos, 0, "a zeroed counter writes position 0");
        assert!(
            !played,
            "a zeroed position must NOT mark a never-watched item played"
        );
    }

    #[test]
    fn apply_local_user_data_on_unknown_item_is_a_noop() {
        let (_dir, mut conn) = open_test_db();
        let touched = apply_local_user_data(&mut conn, "nonexistent", 100, None).expect("apply");
        assert_eq!(touched, None);
    }
}
