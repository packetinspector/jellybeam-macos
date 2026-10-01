//! Browse queries: everything here must be servable from an index (docs/DATA.md
//! §1's query budget — no `SCAN items`, asserted via `EXPLAIN QUERY PLAN` in
//! the tests below). `item()` is the Detail-only DTO parse; `items()` is the
//! one explicitly batched library-construction exception. Neither belongs in
//! the grid/scroll render path.

use jellyfin_api::models::{BaseItemDto, UserItemDataDto};
use rusqlite::{params, Connection, Row};

use crate::{CardRow, Sort};

const CARD_COLUMNS: &str = "id, item_type, name, primary_tag, primary_blurhash, played, \
     playback_position_ticks, runtime_ticks, unplayed_item_count, production_year, \
     index_number, parent_index_number, series_id, series_primary_tag, \
     parent_backdrop_item_id, parent_backdrop_tag, last_played_date, overview, \
     premiere_date, is_virtual, series_name, library_id";

fn row_to_card(row: &Row<'_>) -> rusqlite::Result<CardRow> {
    Ok(CardRow {
        id: row.get(0)?,
        item_type: row.get(1)?,
        name: row.get::<_, Option<String>>(2)?.unwrap_or_default(),
        primary_tag: row.get(3)?,
        blurhash: row.get(4)?,
        played: row.get(5)?,
        position_ticks: row.get(6)?,
        runtime_ticks: row.get(7)?,
        unplayed_count: row.get(8)?,
        production_year: row.get(9)?,
        index_number: row.get(10)?,
        parent_index_number: row.get(11)?,
        series_id: row.get(12)?,
        series_primary_tag: row.get(13)?,
        parent_backdrop_item_id: row.get(14)?,
        parent_backdrop_tag: row.get(15)?,
        last_played_date: row.get(16)?,
        overview: row.get(17)?,
        premiere_date: row.get(18)?,
        is_virtual: row.get(19)?,
        series_name: row.get(20)?,
        library_id: row.get(21)?,
    })
}

/// Total row count in `items`, for the sidebar's "Syncing library — N items"
/// progress text. Not indexed-scan-asserted like the browse queries above --
/// a bare `COUNT(*)` reads from SQLite's own b-tree page count, not a
/// per-row scan.
pub(crate) fn item_count(conn: &Connection) -> i64 {
    conn.query_row("SELECT COUNT(*) FROM items", [], |row| row.get(0))
        .unwrap_or_else(|e| {
            tracing::error!(error = %e, "item_count query failed");
            0
        })
}

/// `kind` is derived from the stored `item_type` here, once, so every
/// caller of [`crate::Mirror::views`] sees the same [`crate::ViewKind`]
/// without each re-deriving it from a raw string.
pub(crate) fn views(conn: &Connection) -> Vec<crate::ViewSummary> {
    let result = (|| -> rusqlite::Result<Vec<crate::ViewSummary>> {
        let mut stmt =
            conn.prepare("SELECT id, name, item_type FROM views ORDER BY sort_index ASC")?;
        let rows = stmt.query_map([], |row| {
            Ok(crate::ViewSummary {
                id: row.get::<_, String>(0)?,
                name: row.get::<_, Option<String>>(1)?.unwrap_or_default(),
                kind: row
                    .get::<_, Option<String>>(2)?
                    .unwrap_or_default()
                    .as_str()
                    .into(),
            })
        })?;
        rows.collect()
    })();
    result.unwrap_or_else(|e| {
        tracing::error!(error = %e, "views query failed");
        Vec::new()
    })
}

fn sort_clause(sort: Sort) -> &'static str {
    match sort {
        Sort::NameAsc => "sort_name ASC",
        Sort::DateCreatedDesc => "date_created DESC",
        Sort::PremiereDateDesc => "premiere_date DESC",
        // Serves both Series -> Seasons and Season -> Episodes (see
        // `Sort::IndexNumber`'s doc comment). Column order matches
        // `idx_items_parent_order` exactly so this stays index-served.
        Sort::IndexNumber => "parent_index_number ASC, index_number ASC",
    }
}

/// Card columns qualified with `items.` for queries that join `items`
/// against another table (the column list is otherwise ambiguous once a
/// second table with e.g. its own `id` is in scope).
fn qualified_card_columns() -> String {
    CARD_COLUMNS
        .split(", ")
        .map(|c| format!("items.{c}"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// A `BoxSet`'s children can't be expressed by `items.parent_id` (an item
/// can belong to many collections; membership isn't a parent/child
/// relationship), so browsing into one goes through the `collection_members`
/// join instead, in the server-provided curation order (`sort_index`)
/// rather than the caller's `Sort`.
fn children_via_collection_membership_checked(
    conn: &Connection,
    collection_id: &str,
    offset: u32,
    limit: u32,
) -> rusqlite::Result<Vec<CardRow>> {
    let sql = format!(
        "SELECT {cols} FROM collection_members cm \
         JOIN items ON items.id = cm.item_id \
         WHERE cm.collection_id = ?1 \
         ORDER BY cm.sort_index ASC LIMIT ?2 OFFSET ?3",
        cols = qualified_card_columns()
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params![collection_id, limit, offset], row_to_card)?;
    rows.collect()
}

pub(crate) fn children(
    conn: &Connection,
    parent_id: &str,
    sort: Sort,
    offset: u32,
    limit: u32,
) -> Vec<CardRow> {
    children_checked(conn, parent_id, sort, offset, limit).unwrap_or_else(|e| {
        tracing::error!(error = %e, "children query failed");
        Vec::new()
    })
}

/// [`children`] with the error surfaced instead of coerced to an empty
/// list. The library refresh path needs the distinction: at 8,000 items,
/// "the query failed" (keep the last good grid) and "the parent is empty"
/// (show an empty grid) are very different UI outcomes.
pub(crate) fn children_checked(
    conn: &Connection,
    parent_id: &str,
    sort: Sort,
    offset: u32,
    limit: u32,
) -> rusqlite::Result<Vec<CardRow>> {
    // One cheap PK lookup to tell a BoxSet or Season parent apart from a
    // regular one; still index-served, not a scan. `.ok()` treats "no such
    // row" and a failed lookup alike -- a real I/O error surfaces from the
    // main query below anyway. Also pulls `series_id`/`index_number` up
    // front: a Season parent needs both to resolve its episodes by season
    // semantics (see `episodes_of_season_checked`).
    let parent_row: Option<(String, Option<String>, Option<i32>)> = conn
        .query_row(
            "SELECT item_type, series_id, index_number FROM items WHERE id = ?1",
            [parent_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .ok();
    let parent_item_type = parent_row.as_ref().map(|(t, _, _)| t.as_str());
    if parent_item_type == Some("BoxSet") {
        return children_via_collection_membership_checked(conn, parent_id, offset, limit);
    }
    // A real Jellyfin server creates `Folder`-type items as literal
    // `parent_id` children of a Series whenever its episodes sit in
    // per-episode release directories on disk -- confirmed live. A plain
    // `parent_id = ?` listing (the fallback path below) would return those
    // junk Folder rows interleaved with the real Season rows. Gated on the
    // parent actually being a Series row so this never touches any other
    // browse level; checked ahead of the Season branch below because that
    // branch moves out of `parent_row`, invalidating this borrow.
    if parent_item_type == Some("Series") {
        return seasons_of_series_checked(conn, parent_id, sort, offset, limit);
    }
    // A real Jellyfin server can hold DUPLICATE Season objects for one
    // series after a rescan (confirmed live). `/Shows/{id}/Seasons` returns
    // only one of each duplicate, so that's the only Season row this mirror
    // stores -- but the season's episodes can have their server-side
    // `ParentId` pointing at the OTHER (unsynced) duplicate, since the
    // server never reconciles it away. The server itself resolves "episodes
    // of a season" by (series, season number) semantics, not literal
    // `ParentId` -- `episodes_of_season_checked` does the same locally.
    // Gated on the parent actually being a Season row so this never touches
    // the Series -> Seasons listing, which has no such duplicate problem.
    if parent_item_type == Some("Season") {
        let (_, series_id, season_index) = parent_row.expect("Some, matched above");
        if let (Some(series_id), Some(season_index)) = (series_id, season_index) {
            return episodes_of_season_checked(
                conn,
                parent_id,
                &series_id,
                season_index,
                sort,
                offset,
                limit,
            );
        }
        // Season row missing `series_id`/`index_number` -- shouldn't happen,
        // but never trust it enough to match every NULL-numbered episode in
        // the series against it. Falls through to plain `parent_id` equality.
    }

    // Pin the index explicitly rather than leaving it to the planner's cost
    // heuristics -- adding `idx_items_parent_order` gave the planner a
    // second index that also matches the bare `parent_id = ?` filter, and
    // it started preferring that one for the *other* sorts too (falling
    // back to a temp b-tree sort for `sort_name`) -- caught by
    // `children_query_uses_index_not_scan`. `INDEXED BY` makes each `Sort`
    // variant's index choice a guarantee, not a heuristic that can flip
    // when the schema grows another index.
    let index = match sort {
        Sort::IndexNumber => "idx_items_parent_order",
        Sort::NameAsc | Sort::DateCreatedDesc | Sort::PremiereDateDesc => "idx_items_browse",
    };
    let sql = format!(
        "SELECT {CARD_COLUMNS} FROM items INDEXED BY {index} WHERE parent_id = ?1 ORDER BY {} LIMIT ?2 OFFSET ?3",
        sort_clause(sort)
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params![parent_id, limit, offset], row_to_card)?;
    rows.collect()
}

/// `children_checked`'s Season branch: resolves `season_id`'s episodes by
/// (series, season number) instead of literal `parent_id` equality -- see
/// that call site's doc comment for the duplicate-Season bug this fixes.
///
/// The `OR`'s second arm is a `parent_id` fallback for episodes the server
/// sent with no `ParentIndexNumber` at all -- without it, such an episode
/// would silently vanish from every season's list. `INDEXED BY` is
/// deliberately *not* pinned here: the `OR` gives the planner two
/// legitimate ways to serve this from `idx_items_series`, and pinning one
/// risks a "no query solution" error if a future schema change makes the
/// other the only valid plan -- `episodes_of_season_query_does_not_scan_items`
/// below asserts the planner's choice never degrades to a full scan.
fn episodes_of_season_checked(
    conn: &Connection,
    season_id: &str,
    series_id: &str,
    season_index: i32,
    sort: Sort,
    offset: u32,
    limit: u32,
) -> rusqlite::Result<Vec<CardRow>> {
    let sql = format!(
        "SELECT {CARD_COLUMNS} FROM items \
         WHERE item_type = 'Episode' AND series_id = ?1 \
         AND (parent_index_number = ?2 \
              OR (parent_index_number IS NULL AND parent_id = ?3)) \
         ORDER BY {} LIMIT ?4 OFFSET ?5",
        sort_clause(sort)
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(
        params![series_id, season_index, season_id, limit, offset],
        row_to_card,
    )?;
    rows.collect()
}

/// `children_checked`'s Series branch: a Series' browse-children set must
/// be exactly its Seasons, never the `Folder` rows a real Jellyfin server
/// creates alongside them -- see that call site's doc comment for the bug.
///
/// Matches by (series) semantics rather than literal `parent_id` equality,
/// mirroring `episodes_of_season_checked`'s `OR` pattern: a Season's own
/// `series_id` column is the primary match, falling back to `parent_id`
/// only when `series_id` is absent. `INDEXED BY` is deliberately not
/// pinned, for the same reason as `episodes_of_season_checked` --
/// `series_children_query_does_not_scan_items` asserts the planner's choice
/// never degrades to a full scan.
///
/// Deliberately NOT filtered on `is_virtual`: unlike a virtual *episode*
/// (hidden from Latest, see `latest_grouped_series`), a real Jellyfin server
/// can mark a whole *Season* row Virtual even when it holds real playable
/// episodes, simply because there's no season directory on disk for it
/// (observed live). Excluding them here would hide a season with real
/// content from the strip entirely, not just mark it unaired.
fn seasons_of_series_checked(
    conn: &Connection,
    series_id: &str,
    sort: Sort,
    offset: u32,
    limit: u32,
) -> rusqlite::Result<Vec<CardRow>> {
    let sql = format!(
        "SELECT {CARD_COLUMNS} FROM items \
         WHERE item_type = 'Season' AND (series_id = ?1 \
              OR (series_id IS NULL AND parent_id = ?1)) \
         ORDER BY {} LIMIT ?2 OFFSET ?3",
        sort_clause(sort)
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params![series_id, limit, offset], row_to_card)?;
    rows.collect()
}

pub(crate) fn resume(conn: &Connection, limit: u32) -> Vec<CardRow> {
    // Server-authoritative `last_played_date`, not the local `updated_at`
    // write clock -- see `schema.rs`'s comment on that column. `NULLS LAST`
    // so a row with no `LastPlayedDate` sorts after every row that has one,
    // instead of `NULL` sorting first (SQLite's default) and jumping the
    // ribbon's queue.
    let sql = format!(
        "SELECT {CARD_COLUMNS} FROM items WHERE playback_position_ticks > 0 \
         ORDER BY last_played_date DESC NULLS LAST LIMIT ?1"
    );
    let result = (|| -> rusqlite::Result<Vec<CardRow>> {
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params![limit], row_to_card)?;
        rows.collect()
    })();
    result.unwrap_or_else(|e| {
        tracing::error!(error = %e, "resume query failed");
        Vec::new()
    })
}

/// "Next Up" isn't derivable from indexed item columns alone (it's a
/// per-series watch-progression algorithm the server already computed via
/// `/Shows/NextUp`); the sync engine mirrors that ordered id list into
/// `meta.next_up_ids` (JSON array) whenever it refreshes NextUp, and this
/// just resolves those ids back to rows, preserving server order. Lookup is
/// by primary key (`id IN (...)`), not a table scan.
pub(crate) fn next_up(conn: &Connection, limit: u32) -> Vec<CardRow> {
    let ids = match crate::schema::read_meta(conn, "next_up_ids") {
        Some(json) => serde_json::from_str::<Vec<String>>(&json).unwrap_or_default(),
        None => return Vec::new(),
    };
    let ids: Vec<String> = ids.into_iter().take(limit as usize).collect();
    if ids.is_empty() {
        return Vec::new();
    }

    let placeholders = std::iter::repeat_n("?", ids.len())
        .collect::<Vec<_>>()
        .join(",");
    let sql = format!("SELECT {CARD_COLUMNS} FROM items WHERE id IN ({placeholders})");
    let result = (|| -> rusqlite::Result<Vec<CardRow>> {
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(ids.iter()), row_to_card)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
    })();
    let mut rows = result.unwrap_or_else(|e| {
        tracing::error!(error = %e, "next_up query failed");
        Vec::new()
    });

    // Reorder to match the server-provided (already-prioritized) order --
    // `IN (...)` gives no ordering guarantee.
    let order: std::collections::HashMap<&str, usize> = ids
        .iter()
        .enumerate()
        .map(|(i, id)| (id.as_str(), i))
        .collect();
    rows.sort_by_key(|r| order.get(r.id.as_str()).copied().unwrap_or(usize::MAX));
    rows
}

/// Two libraries sharing a `collection_type` (e.g. two "Shows"
/// libraries, "Shows-Adults" and "Shows-Kids") would otherwise be
/// indistinguishable here -- filtering purely on `item_type` would make both
/// views' "Latest" shelves show the exact same rows. Scoped instead by the
/// `library_id` stamped at sync time (see
/// `schema.rs`'s comment on the column and `sync::sync_library_breadth`).
///
/// Also implements Jellyfin's own Latest-Media grouping for a `tvshows`
/// view (matching the server's `/Users/{id}/Items/Latest?GroupItems=true`):
/// one `CardRow` per *series*, not one per episode -- see
/// `latest_grouped_series`. Every other collection type stays per-item.
/// `hide_watched`: app Settings "Hide watched from Latest" -- when `true`,
/// adds `AND played = 0` to whichever branch below actually runs, via a
/// bound parameter rather than a second query (the row is either fetched
/// with the exclusion applied or it isn't; there's no separate "count
/// watched" pass to keep in sync). Deliberately not threaded into
/// `resume()`/`next_up()` -- both already only ever contain unwatched/
/// in-progress items by construction, so "hide watched" has nothing to do
/// there.
pub(crate) fn latest(
    conn: &Connection,
    view_id: &str,
    limit: u32,
    hide_watched: bool,
) -> Vec<CardRow> {
    let collection_type: Option<String> = conn
        .query_row(
            "SELECT collection_type FROM views WHERE id = ?1",
            [view_id],
            |row| row.get(0),
        )
        .ok();
    let Some(collection_type) = collection_type else {
        return Vec::new();
    };

    if collection_type == "tvshows" {
        return latest_grouped_series(conn, view_id, limit, hide_watched);
    }

    let item_types = crate::item_types_for_collection(&collection_type);
    if item_types.is_empty() {
        return Vec::new();
    }

    let placeholders = std::iter::repeat_n("?", item_types.len())
        .collect::<Vec<_>>()
        .join(",");
    let played_filter = if hide_watched { " AND played = 0" } else { "" };
    // `limit`/`item_types` are bound via `ToSql`, not interpolated, so this
    // query isn't the one place in the module that doesn't bind parameters.
    // Virtual placeholder rows (`DateCreated` == refresh time, see
    // `rows::extract_columns`'s `is_virtual` comment) must never surface on
    // a Latest shelf: their fresh `date_created` would otherwise make an
    // unreleased episode look like something that just got added.
    let sql = format!(
        "SELECT {CARD_COLUMNS} FROM items WHERE library_id = ? AND item_type IN ({placeholders}) AND is_virtual = 0{played_filter} ORDER BY date_created DESC LIMIT ?"
    );
    let result = (|| -> rusqlite::Result<Vec<CardRow>> {
        let mut stmt = conn.prepare(&sql)?;
        let mut params: Vec<&dyn rusqlite::ToSql> = vec![&view_id as &dyn rusqlite::ToSql];
        params.extend(item_types.iter().map(|s| s as &dyn rusqlite::ToSql));
        params.push(&limit);
        let rows = stmt.query_map(params.as_slice(), row_to_card)?;
        rows.collect()
    })();
    result.unwrap_or_else(|e| {
        tracing::error!(error = %e, "latest query failed");
        Vec::new()
    })
}

/// Jellyfin's Latest-Media semantics for a TV library: one tile per
/// *series*, ordered by that series' most-recently added episode, not one
/// tile per episode -- otherwise a binge-worthy season landing at once
/// floods the shelf with N tiles sharing the same series poster. Mirrors
/// the server's own `/Users/{id}/Items/Latest?GroupItems=true` behavior.
///
/// Implemented as a CTE: group this library's Episodes by `series_id`,
/// keeping each series' newest `date_created` and taking the top `limit`;
/// then join back to `items` to pull each series' own row. An episode whose
/// series was never synced simply can't join and is dropped from the group.
/// `hide_watched` is applied inside the grouping CTE, on the Episode rows
/// -- not the Series row this returns, since a Series row's own `played`
/// isn't a meaningful "has anyone finished this show" flag.
fn latest_grouped_series(
    conn: &Connection,
    view_id: &str,
    limit: u32,
    hide_watched: bool,
) -> Vec<CardRow> {
    let played_filter = if hide_watched { " AND played = 0" } else { "" };
    // Same `is_virtual = 0` reasoning as plain `latest()`, but it matters
    // more here: without it a series whose only "recent" episodes are all
    // virtual placeholders could surface a series that added nothing
    // watchable at all.
    let sql = format!(
        "WITH grouped AS (\
             SELECT series_id, MAX(date_created) AS newest_episode \
             FROM items INDEXED BY idx_items_latest \
             WHERE library_id = ?1 AND item_type = 'Episode' AND series_id IS NOT NULL AND is_virtual = 0{played_filter} \
             GROUP BY series_id \
             ORDER BY newest_episode DESC \
             LIMIT ?2\
         ) \
         SELECT {cols} FROM grouped JOIN items ON items.id = grouped.series_id \
         ORDER BY grouped.newest_episode DESC",
        cols = qualified_card_columns()
    );
    let result = (|| -> rusqlite::Result<Vec<CardRow>> {
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params![view_id, limit], row_to_card)?;
        rows.collect()
    })();
    result.unwrap_or_else(|e| {
        tracing::error!(error = %e, "latest_grouped_series query failed");
        Vec::new()
    })
}

/// Builds a safe FTS5 MATCH expression from free-text user input: split on
/// whitespace, quote each token (so stray `"`/`*`/`:` etc. in the query
/// can't be interpreted as FTS5 query syntax), append `*` for prefix
/// matching (search-as-you-type), AND them together.
fn fts_query(input: &str) -> Option<String> {
    let terms: Vec<String> = input
        .split_whitespace()
        .map(|term| format!("\"{}\"*", term.replace('"', "\"\"")))
        .collect();
    if terms.is_empty() {
        None
    } else {
        Some(terms.join(" "))
    }
}

pub(crate) fn search(conn: &Connection, query: &str, limit: u32) -> Vec<CardRow> {
    let Some(match_expr) = fts_query(query) else {
        return Vec::new();
    };
    let sql = format!(
        "SELECT {cols} FROM search JOIN items ON items.rowid = search.rowid \
         WHERE search MATCH ?1 ORDER BY rank LIMIT ?2",
        cols = qualified_card_columns()
    );
    let result = (|| -> rusqlite::Result<Vec<CardRow>> {
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params![match_expr, limit], row_to_card)?;
        rows.collect()
    })();
    result.unwrap_or_else(|e| {
        tracing::error!(error = %e, query, "search query failed");
        Vec::new()
    })
}

/// `writer::apply_local_user_data`/`writer::apply_user_data` both patch only
/// the dedicated `played`/`playback_position_ticks`/`play_count`/
/// `is_favorite`/`unplayed_item_count`/`last_played_date` columns, never the
/// `dto` blob itself. Every other browse query reads those columns directly
/// (`row_to_card`), so this one must overlay them onto the blob's `user_data`
/// too, or `item()` hands back a possibly-stale value -- concretely,
/// `playback::run`'s `resume_ticks` lookup would seek to wherever the blob
/// last said, not the position just seeked to/reported.
///
/// `item()`'s raw row shape: `(dto blob, played, playback_position_ticks,
/// play_count, is_favorite, unplayed_item_count, last_played_date)` --
/// named so the query below doesn't trip clippy's `type_complexity` lint.
type ItemUserDataRow = (Vec<u8>, bool, i64, i32, bool, Option<i32>, Option<String>);

pub(crate) fn item(conn: &Connection, id: &str) -> Option<BaseItemDto> {
    let row: Option<ItemUserDataRow> = conn
        .query_row(
            "SELECT dto, played, playback_position_ticks, play_count, is_favorite, \
             unplayed_item_count, last_played_date FROM items WHERE id = ?1",
            [id],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                ))
            },
        )
        .ok();
    let (blob, played, position_ticks, play_count, is_favorite, unplayed_count, last_played) = row?;
    let mut dto: BaseItemDto = serde_json::from_slice(&blob).ok()?;
    overlay_local_user_data(
        &mut dto,
        played,
        position_ticks,
        play_count,
        is_favorite,
        unplayed_count,
        last_played,
    );
    Some(dto)
}

/// Batch form of [`item`], used by library construction. SQLite has a
/// default 999-parameter limit, so callers may pass any number of ids and
/// this function will safely split them into bounded queries.
pub(crate) fn items(
    conn: &Connection,
    ids: &[String],
) -> std::collections::HashMap<String, BaseItemDto> {
    const SQLITE_PARAMETER_BATCH: usize = 900;
    let mut result = std::collections::HashMap::with_capacity(ids.len());
    for ids in ids.chunks(SQLITE_PARAMETER_BATCH) {
        let placeholders = std::iter::repeat_n("?", ids.len())
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "SELECT id, dto, played, playback_position_ticks, play_count, is_favorite, \
             unplayed_item_count, last_played_date FROM items WHERE id IN ({placeholders})"
        );
        let rows = (|| -> rusqlite::Result<Vec<(String, ItemUserDataRow)>> {
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt.query_map(rusqlite::params_from_iter(ids), |row| {
                Ok((
                    row.get(0)?,
                    (
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                        row.get(6)?,
                        row.get(7)?,
                    ),
                ))
            })?;
            rows.collect()
        })();
        match rows {
            Ok(rows) => {
                for (
                    id,
                    (
                        blob,
                        played,
                        position_ticks,
                        play_count,
                        is_favorite,
                        unplayed_count,
                        last_played,
                    ),
                ) in rows
                {
                    if let Ok(mut dto) = serde_json::from_slice::<BaseItemDto>(&blob) {
                        overlay_local_user_data(
                            &mut dto,
                            played,
                            position_ticks,
                            play_count,
                            is_favorite,
                            unplayed_count,
                            last_played,
                        );
                        result.insert(id, dto);
                    }
                }
            }
            Err(error) => tracing::error!(error = %error, "batched item query failed"),
        }
    }
    result
}

/// Overlays the mirror's own authoritative `played`/`playback_position_ticks`/
/// `play_count`/`is_favorite`/`unplayed_item_count`/`last_played_date`
/// columns onto the parsed blob's `user_data`, creating one if the blob had
/// none at all (e.g. an item synced before the user ever interacted with
/// it). `Key` has no local equivalent (the server-side `UserData` dedup
/// key) -- left empty, matching the JSON default `#[serde(default)]` would
/// produce; nothing in this app reads it.
fn overlay_local_user_data(
    dto: &mut BaseItemDto,
    played: bool,
    position_ticks: i64,
    play_count: i32,
    is_favorite: bool,
    unplayed_count: Option<i32>,
    last_played_date: Option<String>,
) {
    let last_played_date = last_played_date.and_then(|s| {
        chrono::DateTime::parse_from_rfc3339(&s)
            .ok()
            .map(|d| d.with_timezone(&chrono::Utc))
    });
    let user_data = dto.user_data.get_or_insert_with(|| UserItemDataDto {
        is_favorite: None,
        item_id: None,
        key: String::new(),
        last_played_date: None,
        likes: None,
        play_count: None,
        playback_position_ticks: None,
        played: None,
        played_percentage: None,
        rating: None,
        unplayed_item_count: None,
    });
    user_data.played = Some(played);
    user_data.playback_position_ticks = Some(position_ticks);
    user_data.play_count = Some(play_count);
    user_data.is_favorite = Some(is_favorite);
    user_data.unplayed_item_count = unplayed_count;
    if last_played_date.is_some() {
        user_data.last_played_date = last_played_date;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::open_test_db;
    use crate::writer::{apply_local_user_data, apply_upsert_items, apply_upsert_items_scoped};
    use jellyfin_api::models::BaseItemKind;

    fn item_dto(id: &str, name: &str, parent: Option<&str>, kind: BaseItemKind) -> BaseItemDto {
        BaseItemDto {
            id: Some(uuid::Uuid::parse_str(id).expect("uuid")),
            name: Some(name.to_string()),
            sort_name: Some(name.to_string()),
            type_: Some(kind),
            parent_id: parent.map(|p| uuid::Uuid::parse_str(p).expect("uuid")),
            ..Default::default()
        }
    }

    fn uuid_n(n: u8) -> String {
        format!("e2f5a5f1-1a0b-4b3a-9c2e-{n:012}")
    }

    fn explain(conn: &Connection, sql: &str, params: &[&dyn rusqlite::ToSql]) -> Vec<String> {
        let plan_sql = format!("EXPLAIN QUERY PLAN {sql}");
        let mut stmt = conn.prepare(&plan_sql).expect("prepare explain");
        let rows: Vec<String> = stmt
            .query_map(params, |row| row.get::<_, String>(3))
            .expect("query")
            .collect::<Result<_, _>>()
            .expect("rows");
        rows
    }

    fn assert_no_items_scan(plan: &[String]) {
        for line in plan {
            assert!(
                !line.contains("SCAN items"),
                "expected no full scan of items, got plan line: {line:?} (full plan: {plan:?})"
            );
        }
    }

    #[test]
    fn children_query_uses_index_not_scan() {
        let (_dir, conn) = open_test_db();
        // Matches the exact SQL `children()` issues for `Sort::NameAsc` --
        // `INDEXED BY` pinned explicitly (see `children`'s doc comment)
        // rather than left to the planner. `idx_items_browse`'s middle
        // column (`item_type`) is unconstrained here, so this still needs a
        // small temp b-tree to finish the `sort_name` order -- only the
        // *scan* itself is asserted against.
        let plan = explain(
            &conn,
            "SELECT id FROM items INDEXED BY idx_items_browse \
             WHERE parent_id = ?1 ORDER BY sort_name ASC LIMIT ?2 OFFSET ?3",
            params![String::from("p"), 10u32, 0u32],
        );
        assert_no_items_scan(&plan);
        assert!(
            plan.iter().any(|l| l.contains("idx_items_browse")),
            "plan: {plan:?}"
        );
    }

    /// Pins: Detail's Series -> Seasons / Season -> Episodes browse stays
    /// index-served under `Sort::IndexNumber`, not a temp b-tree sort.
    #[test]
    fn children_query_with_index_number_sort_uses_index_not_scan() {
        let (_dir, conn) = open_test_db();
        let plan = explain(
            &conn,
            "SELECT id FROM items WHERE parent_id = ?1 \
             ORDER BY parent_index_number ASC, index_number ASC LIMIT ?2 OFFSET ?3",
            params![String::from("p"), 10u32, 0u32],
        );
        assert_no_items_scan(&plan);
        assert!(
            !plan.iter().any(|l| l.contains("USE TEMP B-TREE")),
            "expected the index to satisfy ORDER BY directly, plan: {plan:?}"
        );
        assert!(
            plan.iter().any(|l| l.contains("idx_items_parent_order")),
            "plan: {plan:?}"
        );
    }

    #[test]
    fn resume_query_uses_partial_index_not_scan() {
        let (_dir, conn) = open_test_db();
        // Matches the exact SQL `resume()` issues --
        // `last_played_date DESC`, served by `idx_items_resume_by_last_played`.
        // Unlike the other index-usage assertions in this file, this one
        // doesn't reuse `assert_no_items_scan`: `last_played_date` (not
        // `playback_position_ticks`) leads this index, so SQLite can't turn
        // `playback_position_ticks > 0` into a B-tree search key against it
        // -- the row filter is instead satisfied entirely by the index's own
        // partial-index predicate (identical to the WHERE clause), so the
        // planner reports a full walk of that (small) partial index, in its
        // native `last_played_date DESC` order, as "SCAN items USING INDEX
        // idx_items_resume_by_last_played" rather than "SEARCH". That's not
        // a real table scan (it never touches `items` rows outside the
        // partial index's own `playback_position_ticks > 0` subset, and
        // needs no separate sort step) -- exactly what this test actually
        // wants to assert: no full `items` scan, and no `TEMP B-TREE` sort.
        let plan = explain(
            &conn,
            "SELECT id FROM items WHERE playback_position_ticks > 0 \
             ORDER BY last_played_date DESC NULLS LAST LIMIT ?1",
            params![10u32],
        );
        assert!(
            plan.iter()
                .any(|l| l.contains("idx_items_resume_by_last_played")),
            "plan: {plan:?}"
        );
        assert!(
            !plan.iter().any(|l| l.contains("TEMP B-TREE")),
            "must not need a separate sort step -- the index is already in \
             last_played_date order: plan: {plan:?}"
        );
    }

    #[test]
    fn latest_query_uses_index_not_scan() {
        let (_dir, conn) = open_test_db();
        // Matches the exact SQL `latest()` issues for a non-tvshows view --
        // `library_id` leads the filter; `is_virtual = 0` is a residual
        // filter on the same index, not a new predicate column.
        let plan = explain(
            &conn,
            "SELECT id FROM items WHERE library_id = ?1 AND item_type IN ('Movie') AND is_virtual = 0 ORDER BY date_created DESC LIMIT ?2",
            params![String::from("lib1"), 10u32],
        );
        assert_no_items_scan(&plan);
        assert!(
            plan.iter().any(|l| l.contains("idx_items_latest")),
            "plan: {plan:?}"
        );
    }

    /// Pins: the CTE `latest_grouped_series` issues stays index-served for
    /// its base scan over Episodes, same budget as every other browse query.
    #[test]
    fn latest_grouped_series_query_uses_index_not_scan() {
        let (_dir, conn) = open_test_db();
        // The `is_virtual = 0` exclusion (virtual episodes must not drive a
        // series' recency rank) is a residual filter alongside the existing
        // predicates, same index expected.
        let plan = explain(
            &conn,
            "WITH grouped AS (\
                 SELECT series_id, MAX(date_created) AS newest_episode \
                 FROM items INDEXED BY idx_items_latest \
                 WHERE library_id = ?1 AND item_type = 'Episode' AND series_id IS NOT NULL AND is_virtual = 0 \
                 GROUP BY series_id \
                 ORDER BY newest_episode DESC \
                 LIMIT ?2\
             ) \
             SELECT items.id FROM grouped JOIN items ON items.id = grouped.series_id \
             ORDER BY grouped.newest_episode DESC",
            params![String::from("lib1"), 10u32],
        );
        assert_no_items_scan(&plan);
        assert!(
            plan.iter().any(|l| l.contains("idx_items_latest")),
            "plan: {plan:?}"
        );
    }

    #[test]
    fn collection_membership_children_query_uses_index_not_scan() {
        let (_dir, conn) = open_test_db();
        let plan = explain(
            &conn,
            "SELECT items.id FROM collection_members cm JOIN items ON items.id = cm.item_id \
             WHERE cm.collection_id = ?1 ORDER BY cm.sort_index ASC LIMIT ?2 OFFSET ?3",
            params![String::from("boxset-1"), 10u32, 0u32],
        );
        assert_no_items_scan(&plan);
        assert!(
            !plan.iter().any(|l| l.contains("SCAN cm")),
            "expected no full scan of collection_members, plan: {plan:?}"
        );
        assert!(
            plan.iter()
                .any(|l| l.contains("idx_collection_members_order")),
            "plan: {plan:?}"
        );
    }

    #[test]
    fn next_up_lookup_is_pk_search_not_scan() {
        let (_dir, conn) = open_test_db();
        let plan = explain(
            &conn,
            "SELECT id FROM items WHERE id IN (?1, ?2)",
            params!["a", "b"],
        );
        assert_no_items_scan(&plan);
    }

    #[test]
    fn search_join_does_not_scan_items() {
        let (_dir, conn) = open_test_db();
        let plan = explain(
            &conn,
            "SELECT items.id FROM search JOIN items ON items.rowid = search.rowid WHERE search MATCH ?1 ORDER BY rank LIMIT ?2",
            params!["hello", 10u32],
        );
        assert_no_items_scan(&plan);
    }

    #[test]
    fn children_returns_rows_sorted_by_name() {
        let (_dir, mut conn) = open_test_db();
        let parent = uuid_n(1);
        apply_upsert_items(
            &mut conn,
            &[
                item_dto(&uuid_n(2), "Bravo", Some(&parent), BaseItemKind::Movie),
                item_dto(&uuid_n(3), "Alpha", Some(&parent), BaseItemKind::Movie),
            ],
        )
        .expect("insert");

        let rows = children(&conn, &parent, Sort::NameAsc, 0, 10);
        assert_eq!(
            rows.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(),
            vec!["Alpha", "Bravo"]
        );
    }

    /// Pins: `Sort::IndexNumber` orders by `index_number`, not name (an
    /// alphabetical sort would put "Episode 10" before "Episode 2").
    #[test]
    fn children_with_index_number_sort_orders_by_episode_number_not_name() {
        let (_dir, mut conn) = open_test_db();
        let season = uuid_n(1);
        let mut ep2 = item_dto(
            &uuid_n(2),
            "Zeta Episode",
            Some(&season),
            BaseItemKind::Episode,
        );
        ep2.index_number = Some(2);
        let mut ep10 = item_dto(
            &uuid_n(3),
            "Alpha Episode",
            Some(&season),
            BaseItemKind::Episode,
        );
        ep10.index_number = Some(10);
        let mut ep1 = item_dto(
            &uuid_n(4),
            "Mid Episode",
            Some(&season),
            BaseItemKind::Episode,
        );
        ep1.index_number = Some(1);
        apply_upsert_items(&mut conn, &[ep2, ep10, ep1]).expect("insert");

        let rows = children(&conn, &season, Sort::IndexNumber, 0, 10);
        assert_eq!(
            rows.iter().map(|r| r.index_number).collect::<Vec<_>>(),
            vec![Some(1), Some(2), Some(10)],
            "must sort by index_number, not name -- got: {:?}",
            rows.iter().map(|r| &r.name).collect::<Vec<_>>()
        );
    }

    /// Pins: `CardRow`'s artwork fallback-chain fields round-trip through a
    /// real upsert + `children()` read.
    #[test]
    fn children_carries_artwork_fallback_fields() {
        let (_dir, mut conn) = open_test_db();
        let series = uuid_n(1);
        let mut ep = item_dto(&uuid_n(2), "Ep", Some(&series), BaseItemKind::Episode);
        ep.series_id = Some(uuid::Uuid::parse_str(&series).expect("uuid"));
        ep.series_primary_image_tag = Some("series-poster".to_string());
        ep.parent_backdrop_item_id = Some(uuid::Uuid::parse_str(&uuid_n(9)).expect("uuid"));
        ep.parent_backdrop_image_tags = vec!["parent-backdrop".to_string()];
        apply_upsert_items(&mut conn, &[ep]).expect("insert");

        let rows = children(&conn, &series, Sort::IndexNumber, 0, 10);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].series_id.as_deref(), Some(series.as_str()));
        assert_eq!(rows[0].series_primary_tag.as_deref(), Some("series-poster"));
        assert_eq!(
            rows[0].parent_backdrop_item_id.as_deref(),
            Some(uuid_n(9).as_str())
        );
        assert_eq!(
            rows[0].parent_backdrop_tag.as_deref(),
            Some("parent-backdrop")
        );
    }

    /// Pins: `LocationType == "Virtual"` becomes `CardRow::is_virtual: true`, with `premiere_date` carried as RFC3339.
    #[test]
    fn children_carries_virtual_and_premiere_date_fields() {
        let (_dir, mut conn) = open_test_db();
        let series = uuid_n(1);
        let mut ep = item_dto(
            &uuid_n(2),
            "Unaired Ep",
            Some(&series),
            BaseItemKind::Episode,
        );
        ep.series_id = Some(uuid::Uuid::parse_str(&series).expect("uuid"));
        ep.location_type = Some(jellyfin_api::models::LocationType::Virtual);
        ep.premiere_date = Some("2099-01-15T00:00:00Z".parse().expect("datetime"));
        apply_upsert_items(&mut conn, &[ep]).expect("insert");

        let rows = children(&conn, &series, Sort::IndexNumber, 0, 10);
        assert_eq!(rows.len(), 1);
        assert!(rows[0].is_virtual);
        assert_eq!(
            rows[0].premiere_date.as_deref(),
            Some("2099-01-15T00:00:00+00:00")
        );
    }

    #[test]
    fn children_of_a_non_virtual_item_is_not_virtual() {
        let (_dir, mut conn) = open_test_db();
        let series = uuid_n(1);
        let mut ep = item_dto(&uuid_n(2), "Aired Ep", Some(&series), BaseItemKind::Episode);
        ep.series_id = Some(uuid::Uuid::parse_str(&series).expect("uuid"));
        ep.location_type = Some(jellyfin_api::models::LocationType::FileSystem);
        apply_upsert_items(&mut conn, &[ep]).expect("insert");

        let rows = children(&conn, &series, Sort::IndexNumber, 0, 10);
        assert_eq!(rows.len(), 1);
        assert!(!rows[0].is_virtual);
    }

    #[test]
    fn children_of_a_boxset_resolves_through_collection_membership() {
        let (_dir, mut conn) = open_test_db();
        let boxset_id = uuid_n(1);
        let movie_a = uuid_n(2);
        let movie_b = uuid_n(3);
        let unrelated = uuid_n(4);

        apply_upsert_items(
            &mut conn,
            &[
                item_dto(&boxset_id, "A Collection", None, BaseItemKind::BoxSet),
                // Movies live under their library's parent_id, NOT the
                // boxset's -- membership is a separate relation.
                item_dto(&movie_a, "Zeta", None, BaseItemKind::Movie),
                item_dto(&movie_b, "Alpha", None, BaseItemKind::Movie),
                item_dto(&unrelated, "Not In The Set", None, BaseItemKind::Movie),
            ],
        )
        .expect("insert");
        crate::writer::apply_set_collection_members(
            &mut conn,
            &boxset_id,
            // Server curation order deliberately not alphabetical.
            &[(movie_a.clone(), 0), (movie_b.clone(), 1)],
        )
        .expect("set members");

        // parent_id equality would find nothing (children's parent_id is
        // None here); the membership join must be what resolves this.
        let rows = children(&conn, &boxset_id, Sort::NameAsc, 0, 10);
        assert_eq!(
            rows.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
            vec![movie_a.as_str(), movie_b.as_str()],
            "must follow collection_members' sort_index, not alphabetical Sort"
        );
    }

    #[test]
    fn children_of_a_non_boxset_parent_ignores_collection_membership() {
        let (_dir, mut conn) = open_test_db();
        let parent = uuid_n(1);
        let child = uuid_n(2);
        // `CollectionFolder` deliberately, not `Series` -- `children_checked`
        // now special-cases a Series parent to return only its Season
        // children (see `seasons_of_series_checked`), which isn't what this
        // test is exercising; this just needs a generic non-BoxSet parent
        // type that falls through to the plain `parent_id` path.
        apply_upsert_items(
            &mut conn,
            &[
                item_dto(&parent, "Home Videos", None, BaseItemKind::CollectionFolder),
                item_dto(&child, "Clip", Some(&parent), BaseItemKind::Video),
            ],
        )
        .expect("insert");

        let rows = children(&conn, &parent, Sort::NameAsc, 0, 10);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, child);
    }

    #[test]
    fn resume_only_returns_items_with_progress() {
        let (_dir, mut conn) = open_test_db();
        let mut watched = item_dto(&uuid_n(1), "Watched", None, BaseItemKind::Movie);
        watched.user_data = Some(jellyfin_api::models::UserItemDataDto {
            played: Some(false),
            playback_position_ticks: Some(1000),
            play_count: Some(0),
            is_favorite: None,
            unplayed_item_count: None,
            key: "k".to_string(),
            item_id: None,
            last_played_date: None,
            likes: None,
            played_percentage: None,
            rating: None,
        });
        let unwatched = item_dto(&uuid_n(2), "Fresh", None, BaseItemKind::Movie);
        apply_upsert_items(&mut conn, &[watched, unwatched]).expect("insert");

        let rows = resume(&conn, 10);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].name, "Watched");
    }

    fn user_data_with_progress(
        position_ticks: i64,
        last_played_date: Option<::chrono::DateTime<::chrono::Utc>>,
    ) -> jellyfin_api::models::UserItemDataDto {
        jellyfin_api::models::UserItemDataDto {
            played: Some(false),
            playback_position_ticks: Some(position_ticks),
            play_count: Some(0),
            is_favorite: None,
            unplayed_item_count: None,
            key: "k".to_string(),
            item_id: None,
            last_played_date,
            likes: None,
            played_percentage: None,
            rating: None,
        }
    }

    /// Pins: `resume()` orders by `UserData.LastPlayedDate`, not the local write clock (see `schema.rs`'s comment on `last_played_date`).
    #[test]
    fn resume_orders_by_last_played_date_not_local_write_clock() {
        let (_dir, mut conn) = open_test_db();
        let older = "2024-01-01T00:00:00Z".parse().expect("date");
        let newer = "2024-06-01T00:00:00Z".parse().expect("date");

        let mut a = item_dto(&uuid_n(1), "A", None, BaseItemKind::Movie);
        a.user_data = Some(user_data_with_progress(1000, Some(older)));
        let mut b = item_dto(&uuid_n(2), "B", None, BaseItemKind::Movie);
        b.user_data = Some(user_data_with_progress(2000, Some(newer)));
        apply_upsert_items(&mut conn, &[a.clone(), b.clone()]).expect("insert");

        let rows = resume(&conn, 10);
        assert_eq!(
            rows.iter().map(|r| r.id.clone()).collect::<Vec<_>>(),
            vec![b.id.expect("id").to_string(), a.id.expect("id").to_string()],
            "newer LastPlayedDate must sort first"
        );
    }

    /// Pins: re-upserting a row with identical user-state must not reorder `resume()`, even though it still bumps `updated_at`.
    #[test]
    fn resume_order_is_stable_across_reupserts_of_identical_user_state() {
        let (_dir, mut conn) = open_test_db();
        let older = "2024-01-01T00:00:00Z".parse().expect("date");
        let newer = "2024-06-01T00:00:00Z".parse().expect("date");

        let mut a = item_dto(&uuid_n(1), "A", None, BaseItemKind::Movie);
        a.user_data = Some(user_data_with_progress(1000, Some(older)));
        let mut b = item_dto(&uuid_n(2), "B", None, BaseItemKind::Movie);
        b.user_data = Some(user_data_with_progress(2000, Some(newer)));
        apply_upsert_items(&mut conn, &[a.clone(), b.clone()]).expect("insert");

        let before = resume(&conn, 10)
            .iter()
            .map(|r| r.id.clone())
            .collect::<Vec<_>>();

        // Simulate a reconciliation pass re-upserting "A" with the exact
        // same UserData (same position, same LastPlayedDate) -- this still
        // bumps `updated_at` (every upsert does), but must NOT move "A"
        // ahead of "B" in `resume()`'s ordering.
        std::thread::sleep(std::time::Duration::from_millis(2));
        apply_upsert_items(&mut conn, &[a]).expect("re-upsert identical state");

        let after = resume(&conn, 10)
            .iter()
            .map(|r| r.id.clone())
            .collect::<Vec<_>>();

        assert_eq!(
            before, after,
            "re-upserting identical user-state must not reorder resume()"
        );
    }

    #[test]
    fn latest_maps_view_collection_type_to_item_type() {
        let (_dir, mut conn) = open_test_db();
        conn.execute(
            "INSERT INTO views (id, name, collection_type, sort_index) VALUES ('lib1', 'Movies', 'movies', 0)",
            [],
        )
        .expect("insert view");
        apply_upsert_items_scoped(
            &mut conn,
            &[item_dto(&uuid_n(1), "A Movie", None, BaseItemKind::Movie)],
            Some("lib1"),
        )
        .expect("insert");
        apply_upsert_items_scoped(
            &mut conn,
            &[item_dto(&uuid_n(2), "A Series", None, BaseItemKind::Series)],
            Some("lib1"),
        )
        .expect("insert");

        let rows = latest(&conn, "lib1", 10, false);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].name, "A Movie");
    }

    /// Pins: two libraries sharing a `collection_type` have fully disjoint `latest()` results, scoped by library, not just `item_type`.
    #[test]
    fn latest_scopes_by_library_not_just_item_type() {
        let (_dir, mut conn) = open_test_db();
        conn.execute(
            "INSERT INTO views (id, name, collection_type, sort_index) VALUES ('adults', 'Shows-Adults', 'movies', 0)",
            [],
        )
        .expect("insert view");
        conn.execute(
            "INSERT INTO views (id, name, collection_type, sort_index) VALUES ('kids', 'Shows-Kids', 'movies', 1)",
            [],
        )
        .expect("insert view");
        apply_upsert_items_scoped(
            &mut conn,
            &[item_dto(
                &uuid_n(1),
                "Adult Movie",
                None,
                BaseItemKind::Movie,
            )],
            Some("adults"),
        )
        .expect("insert");
        apply_upsert_items_scoped(
            &mut conn,
            &[item_dto(
                &uuid_n(2),
                "Kids Movie",
                None,
                BaseItemKind::Movie,
            )],
            Some("kids"),
        )
        .expect("insert");

        let adults_rows = latest(&conn, "adults", 10, false);
        let kids_rows = latest(&conn, "kids", 10, false);
        assert_eq!(
            adults_rows
                .iter()
                .map(|r| r.name.as_str())
                .collect::<Vec<_>>(),
            vec!["Adult Movie"]
        );
        assert_eq!(
            kids_rows
                .iter()
                .map(|r| r.name.as_str())
                .collect::<Vec<_>>(),
            vec!["Kids Movie"]
        );
    }

    /// Pins: an item with no `library_id` stamped never shows up in any library's "Latest" shelf.
    #[test]
    fn latest_excludes_items_with_no_library_id() {
        let (_dir, mut conn) = open_test_db();
        conn.execute(
            "INSERT INTO views (id, name, collection_type, sort_index) VALUES ('lib1', 'Movies', 'movies', 0)",
            [],
        )
        .expect("insert view");
        apply_upsert_items(
            &mut conn,
            &[item_dto(
                &uuid_n(1),
                "Unscoped Movie",
                None,
                BaseItemKind::Movie,
            )],
        )
        .expect("insert");

        assert!(latest(&conn, "lib1", 10, false).is_empty());
    }

    /// Pins: a virtual placeholder item never surfaces on a Latest shelf, even when its `date_created` is the newest in the library.
    #[test]
    fn latest_excludes_virtual_items() {
        let (_dir, mut conn) = open_test_db();
        conn.execute(
            "INSERT INTO views (id, name, collection_type, sort_index) VALUES ('lib1', 'Movies', 'movies', 0)",
            [],
        )
        .expect("insert view");

        let mut real = item_dto(&uuid_n(1), "Real Movie", None, BaseItemKind::Movie);
        real.date_created = Some("2024-01-01T00:00:00Z".parse().expect("date"));

        let mut virtual_movie = item_dto(&uuid_n(2), "Announced Movie", None, BaseItemKind::Movie);
        virtual_movie.date_created = Some("2099-01-01T00:00:00Z".parse().expect("date"));
        virtual_movie.location_type = Some(jellyfin_api::models::LocationType::Virtual);

        apply_upsert_items_scoped(&mut conn, &[real, virtual_movie], Some("lib1")).expect("insert");

        let rows = latest(&conn, "lib1", 10, false);
        assert_eq!(
            rows.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(),
            vec!["Real Movie"],
            "the virtual item's newer date_created must not surface it on \
             Latest -- got {rows:?}"
        );
    }

    /// Pins: `hide_watched = true` excludes an already-played item; `false` still shows it.
    #[test]
    fn latest_hides_watched_items_only_when_asked() {
        let (_dir, mut conn) = open_test_db();
        conn.execute(
            "INSERT INTO views (id, name, collection_type, sort_index) VALUES ('lib1', 'Movies', 'movies', 0)",
            [],
        )
        .expect("insert view");
        let mut watched = item_dto(&uuid_n(1), "Watched Movie", None, BaseItemKind::Movie);
        watched.user_data = Some(jellyfin_api::models::UserItemDataDto {
            played: Some(true),
            playback_position_ticks: Some(0),
            play_count: Some(1),
            is_favorite: None,
            unplayed_item_count: None,
            key: "k".to_string(),
            item_id: None,
            last_played_date: None,
            likes: None,
            played_percentage: None,
            rating: None,
        });
        let unwatched = item_dto(&uuid_n(2), "Unwatched Movie", None, BaseItemKind::Movie);
        apply_upsert_items_scoped(&mut conn, &[watched, unwatched], Some("lib1")).expect("insert");

        let with_watched = latest(&conn, "lib1", 10, false);
        assert_eq!(
            with_watched
                .iter()
                .map(|r| r.name.as_str())
                .collect::<std::collections::HashSet<_>>(),
            std::collections::HashSet::from(["Watched Movie", "Unwatched Movie"]),
            "hide_watched=false must keep today's behavior -- both items show"
        );

        let hidden = latest(&conn, "lib1", 10, true);
        assert_eq!(
            hidden.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(),
            vec!["Unwatched Movie"],
            "hide_watched=true must exclude the already-played item"
        );
    }

    /// Pins: a TV library's "Latest" shelf is one tile per series, ordered by each series' most recently added episode, not one tile per episode.
    #[test]
    fn latest_groups_tvshows_by_series_ordered_by_newest_episode() {
        let (_dir, mut conn) = open_test_db();
        conn.execute(
            "INSERT INTO views (id, name, collection_type, sort_index) VALUES ('shows', 'Shows', 'tvshows', 0)",
            [],
        )
        .expect("insert view");

        let series_a = uuid_n(1);
        let series_b = uuid_n(2);
        apply_upsert_items_scoped(
            &mut conn,
            &[
                item_dto(&series_a, "Series A", None, BaseItemKind::Series),
                item_dto(&series_b, "Series B", None, BaseItemKind::Series),
            ],
            Some("shows"),
        )
        .expect("insert series");

        let mut ep_a1 = item_dto(&uuid_n(3), "A S1E1", Some(&series_a), BaseItemKind::Episode);
        ep_a1.series_id = Some(uuid::Uuid::parse_str(&series_a).expect("uuid"));
        ep_a1.date_created = Some("2024-01-01T00:00:00Z".parse().expect("date"));
        let mut ep_a2 = item_dto(&uuid_n(4), "A S1E2", Some(&series_a), BaseItemKind::Episode);
        ep_a2.series_id = Some(uuid::Uuid::parse_str(&series_a).expect("uuid"));
        ep_a2.date_created = Some("2024-06-01T00:00:00Z".parse().expect("date"));
        let mut ep_b1 = item_dto(&uuid_n(5), "B S1E1", Some(&series_b), BaseItemKind::Episode);
        ep_b1.series_id = Some(uuid::Uuid::parse_str(&series_b).expect("uuid"));
        ep_b1.date_created = Some("2024-03-01T00:00:00Z".parse().expect("date"));

        apply_upsert_items_scoped(&mut conn, &[ep_a1, ep_a2, ep_b1], Some("shows"))
            .expect("insert episodes");

        let rows = latest(&conn, "shows", 10, false);
        assert_eq!(
            rows.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(),
            vec!["Series A", "Series B"],
            "Series A's newest episode (2024-06-01) beats Series B's (2024-03-01); \
             each series must appear exactly once regardless of episode count -- got {rows:?}"
        );
    }

    /// Pins: in the grouped (tvshows) branch, a series whose only recent episode is watched drops out entirely; a mixed series surfaces via its newest unwatched episode.
    #[test]
    fn latest_grouped_series_hides_watched_episodes_only_when_asked() {
        let (_dir, mut conn) = open_test_db();
        conn.execute(
            "INSERT INTO views (id, name, collection_type, sort_index) VALUES ('shows', 'Shows', 'tvshows', 0)",
            [],
        )
        .expect("insert view");

        let series_a = uuid_n(1); // fully watched -- must vanish when hidden
        let series_b = uuid_n(2); // mixed -- newest UNWATCHED episode wins
        apply_upsert_items_scoped(
            &mut conn,
            &[
                item_dto(&series_a, "Series A", None, BaseItemKind::Series),
                item_dto(&series_b, "Series B", None, BaseItemKind::Series),
            ],
            Some("shows"),
        )
        .expect("insert series");

        let watched_data = |ticks: i64| {
            Some(jellyfin_api::models::UserItemDataDto {
                played: Some(true),
                playback_position_ticks: Some(0),
                play_count: Some(1),
                is_favorite: None,
                unplayed_item_count: None,
                key: format!("k{ticks}"),
                item_id: None,
                last_played_date: None,
                likes: None,
                played_percentage: None,
                rating: None,
            })
        };

        let mut ep_a1 = item_dto(&uuid_n(3), "A S1E1", Some(&series_a), BaseItemKind::Episode);
        ep_a1.series_id = Some(uuid::Uuid::parse_str(&series_a).expect("uuid"));
        ep_a1.date_created = Some("2024-06-01T00:00:00Z".parse().expect("date"));
        ep_a1.user_data = watched_data(1);

        let mut ep_b1 = item_dto(&uuid_n(4), "B S1E1", Some(&series_b), BaseItemKind::Episode);
        ep_b1.series_id = Some(uuid::Uuid::parse_str(&series_b).expect("uuid"));
        ep_b1.date_created = Some("2024-05-01T00:00:00Z".parse().expect("date"));
        ep_b1.user_data = watched_data(2);
        let mut ep_b2 = item_dto(&uuid_n(5), "B S1E2", Some(&series_b), BaseItemKind::Episode);
        ep_b2.series_id = Some(uuid::Uuid::parse_str(&series_b).expect("uuid"));
        ep_b2.date_created = Some("2024-03-01T00:00:00Z".parse().expect("date"));
        // ep_b2 left unwatched.

        apply_upsert_items_scoped(&mut conn, &[ep_a1, ep_b1, ep_b2], Some("shows"))
            .expect("insert episodes");

        let with_watched = latest(&conn, "shows", 10, false);
        assert_eq!(
            with_watched
                .iter()
                .map(|r| r.name.as_str())
                .collect::<Vec<_>>(),
            vec!["Series A", "Series B"],
            "hide_watched=false must keep today's behavior -- both series show"
        );

        let hidden = latest(&conn, "shows", 10, true);
        assert_eq!(
            hidden.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(),
            vec!["Series B"],
            "Series A had only a watched episode and must vanish entirely; \
             Series B must still surface via its unwatched episode"
        );
    }

    /// Pins: a virtual episode's fresh `date_created` doesn't drive its series' recency rank; a series with only virtual episodes doesn't appear at all.
    #[test]
    fn latest_grouped_series_ignores_virtual_episodes() {
        let (_dir, mut conn) = open_test_db();
        conn.execute(
            "INSERT INTO views (id, name, collection_type, sort_index) VALUES ('shows', 'Shows', 'tvshows', 0)",
            [],
        )
        .expect("insert view");

        let series_a = uuid_n(1); // old real episode + brand-new virtual episode
        let series_b = uuid_n(2); // mid-age real episode only
        let series_c = uuid_n(3); // only virtual episodes -- must not appear
        apply_upsert_items_scoped(
            &mut conn,
            &[
                item_dto(&series_a, "Series A", None, BaseItemKind::Series),
                item_dto(&series_b, "Series B", None, BaseItemKind::Series),
                item_dto(&series_c, "Series C", None, BaseItemKind::Series),
            ],
            Some("shows"),
        )
        .expect("insert series");

        let mut ep_a_real = item_dto(&uuid_n(4), "A S1E1", Some(&series_a), BaseItemKind::Episode);
        ep_a_real.series_id = Some(uuid::Uuid::parse_str(&series_a).expect("uuid"));
        ep_a_real.date_created = Some("2024-01-01T00:00:00Z".parse().expect("date"));

        let mut ep_a_virtual = item_dto(
            &uuid_n(5),
            "A S1E2 (unaired)",
            Some(&series_a),
            BaseItemKind::Episode,
        );
        ep_a_virtual.series_id = Some(uuid::Uuid::parse_str(&series_a).expect("uuid"));
        ep_a_virtual.date_created = Some("2099-01-01T00:00:00Z".parse().expect("date"));
        ep_a_virtual.location_type = Some(jellyfin_api::models::LocationType::Virtual);

        let mut ep_b_real = item_dto(&uuid_n(6), "B S1E1", Some(&series_b), BaseItemKind::Episode);
        ep_b_real.series_id = Some(uuid::Uuid::parse_str(&series_b).expect("uuid"));
        ep_b_real.date_created = Some("2024-06-01T00:00:00Z".parse().expect("date"));

        let mut ep_c_virtual = item_dto(
            &uuid_n(7),
            "C S1E1 (unaired)",
            Some(&series_c),
            BaseItemKind::Episode,
        );
        ep_c_virtual.series_id = Some(uuid::Uuid::parse_str(&series_c).expect("uuid"));
        ep_c_virtual.date_created = Some("2099-06-01T00:00:00Z".parse().expect("date"));
        ep_c_virtual.location_type = Some(jellyfin_api::models::LocationType::Virtual);

        apply_upsert_items_scoped(
            &mut conn,
            &[ep_a_real, ep_a_virtual, ep_b_real, ep_c_virtual],
            Some("shows"),
        )
        .expect("insert episodes");

        let rows = latest(&conn, "shows", 10, false);
        assert_eq!(
            rows.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(),
            vec!["Series B", "Series A"],
            "Series A's recency must come from its real episode (2024-01-01) \
             only, ranking behind Series B's (2024-06-01); Series C has only \
             virtual episodes and must not appear at all -- got {rows:?}"
        );
    }

    #[test]
    fn search_finds_by_name_prefix() {
        let (_dir, mut conn) = open_test_db();
        apply_upsert_items(
            &mut conn,
            &[item_dto(
                &uuid_n(1),
                "Quantum Static",
                None,
                BaseItemKind::Series,
            )],
        )
        .expect("insert");

        let rows = search(&conn, "Quant", 10);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].name, "Quantum Static");
    }

    #[test]
    fn search_with_empty_query_returns_nothing() {
        let (_dir, conn) = open_test_db();
        assert!(search(&conn, "", 10).is_empty());
        assert!(search(&conn, "   ", 10).is_empty());
    }

    #[test]
    fn search_query_with_quotes_does_not_error() {
        let (_dir, conn) = open_test_db();
        // Must not panic or error out even with FTS5-syntax-looking input.
        let rows = search(&conn, "\"weird\" OR *query", 10);
        assert!(rows.is_empty());
    }

    #[test]
    fn item_returns_full_dto() {
        let (_dir, mut conn) = open_test_db();
        let id = uuid_n(1);
        apply_upsert_items(
            &mut conn,
            &[item_dto(&id, "Detail Me", None, BaseItemKind::Movie)],
        )
        .expect("insert");
        let dto = item(&conn, &id).expect("found");
        assert_eq!(dto.name.as_deref(), Some("Detail Me"));
    }

    /// Pins: `item()` reflects `apply_local_user_data`'s write even though it never rewrites the dto blob.
    #[test]
    fn item_reflects_apply_local_user_data_even_though_the_blob_itself_is_never_rewritten() {
        let (_dir, mut conn) = open_test_db();
        let id = uuid_n(1);
        apply_upsert_items(
            &mut conn,
            &[item_dto(&id, "Resume Me", None, BaseItemKind::Movie)],
        )
        .expect("insert");
        assert!(
            item(&conn, &id)
                .expect("found")
                .user_data
                .and_then(|u| u.playback_position_ticks)
                .unwrap_or(0)
                == 0,
            "sanity check: a freshly-synced item starts with no resume position"
        );

        apply_local_user_data(&mut conn, &id, 12_345, None).expect("apply");

        let dto = item(&conn, &id).expect("found");
        assert_eq!(
            dto.user_data.and_then(|u| u.playback_position_ticks),
            Some(12_345),
            "item() must reflect apply_local_user_data's position even though \
             it never rewrites the dto blob"
        );
    }

    #[test]
    fn item_missing_returns_none() {
        let (_dir, conn) = open_test_db();
        assert!(item(&conn, "nope").is_none());
    }

    /// `id` isn't a valid `uuid_n` format (n as u8 caps at 255), so batched
    /// tests below build their own zero-padded hex ids instead of reusing
    /// `uuid_n`.
    fn uuid_wide(n: u32) -> String {
        format!("e2f5a5f1-1a0b-4b3a-9c2e-{n:012}")
    }

    /// Pins the `SQLITE_PARAMETER_BATCH` (900) chunk boundary: fewer than
    /// 901 ids leave the chunking logic untested.
    #[test]
    fn items_returns_all_rows_across_the_parameter_chunk_boundary() {
        let (_dir, mut conn) = open_test_db();
        const N: u32 = 950;
        let dtos: Vec<BaseItemDto> = (0..N)
            .map(|n| item_dto(&uuid_wide(n), "Item", None, BaseItemKind::Movie))
            .collect();
        apply_upsert_items(&mut conn, &dtos).expect("insert");

        let ids: Vec<String> = (0..N).map(uuid_wide).collect();
        let result = items(&conn, &ids);

        assert_eq!(
            result.len(),
            N as usize,
            "must return every row, including those past the 900-parameter \
             chunk boundary"
        );
        for id in &ids {
            assert!(result.contains_key(id), "missing id {id} in result");
        }
    }

    /// Pins: `items()` resolves what it can and silently drops missing/duplicate ids without erroring.
    #[test]
    fn items_skips_missing_and_duplicate_ids_cleanly() {
        let (_dir, mut conn) = open_test_db();
        let present_a = uuid_n(1);
        let present_b = uuid_n(2);
        let absent = uuid_n(3);
        apply_upsert_items(
            &mut conn,
            &[
                item_dto(&present_a, "A", None, BaseItemKind::Movie),
                item_dto(&present_b, "B", None, BaseItemKind::Movie),
            ],
        )
        .expect("insert");

        let ids = vec![
            present_a.clone(),
            absent.clone(),
            present_b.clone(),
            present_a.clone(),
        ];
        let result = items(&conn, &ids);

        assert_eq!(
            result.len(),
            2,
            "duplicate/absent ids must not inflate or error"
        );
        assert_eq!(
            result.get(&present_a).and_then(|d| d.name.as_deref()),
            Some("A")
        );
        assert_eq!(
            result.get(&present_b).and_then(|d| d.name.as_deref()),
            Some("B")
        );
        assert!(
            !result.contains_key(&absent),
            "an id with no matching row must simply be absent from the map"
        );
    }

    /// `items()`'s row mapper hand-maintains its own column indices,
    /// separate from `item()`'s (the batched SELECT prepends `id`, shifting
    /// every `row.get(n)` by one). Pins that both functions decode the same
    /// row into the same `BaseItemDto`, so a future column reorder without a
    /// matching `row.get` reorder fails loudly instead of silently
    /// misassigning fields.
    #[test]
    fn items_field_alignment_matches_item() {
        let (_dir, mut conn) = open_test_db();
        let id = uuid_n(1);
        let mut dto = item_dto(&id, "Distinctive Name", None, BaseItemKind::Movie);
        dto.genres = vec!["Noir".to_string(), "Sci-Fi".to_string()];
        apply_upsert_items(&mut conn, &[dto]).expect("insert");
        apply_local_user_data(&mut conn, &id, 54_321, Some(true)).expect("apply user data");

        let via_item = item(&conn, &id).expect("item() found row");
        let via_items = items(&conn, std::slice::from_ref(&id))
            .remove(&id)
            .expect("items() found row");

        assert_eq!(via_items.name, via_item.name);
        assert_eq!(via_items.type_, via_item.type_);
        assert_eq!(via_items.genres, via_item.genres);
        assert_eq!(
            via_items.genres,
            vec!["Noir".to_string(), "Sci-Fi".to_string()]
        );

        let item_ud = via_item.user_data.expect("item() user_data");
        let items_ud = via_items.user_data.expect("items() user_data");
        assert_eq!(items_ud.played, item_ud.played);
        assert_eq!(
            items_ud.playback_position_ticks,
            item_ud.playback_position_ticks
        );
        assert_eq!(items_ud.play_count, item_ud.play_count);
        assert_eq!(items_ud.is_favorite, item_ud.is_favorite);
        assert_eq!(items_ud.unplayed_item_count, item_ud.unplayed_item_count);
        assert_eq!(items_ud.last_played_date, item_ud.last_played_date);
        // Not just equal to each other -- pinned to the actual value written,
        // so a bug that shifts BOTH functions' indices identically (and thus
        // wouldn't be caught by the equality checks above) still fails.
        assert_eq!(items_ud.played, Some(true));
        assert_eq!(items_ud.playback_position_ticks, Some(0));
    }

    // `app`'s episode navigation (`root.rs::adjacent_episode`,
    // `detail.rs::find_series_next_episode`) treats a
    // `children(.., Sort::IndexNumber, ..)` list as "playback order" and
    // indexes into it with `+1`/`-1`. These tests pin what that ordering
    // actually is for the three shapes real Jellyfin libraries produce.

    /// SQLite orders `NULL` FIRST under a plain `ASC` (no `NULLS LAST`), so
    /// an episode the server sent without an `IndexNumber` sorts *ahead* of
    /// episode 1 -- not after the numbered ones.
    #[test]
    fn children_with_index_number_sort_places_null_index_number_first() {
        let (_dir, mut conn) = open_test_db();
        let season = uuid_n(1);
        let mut ep1 = item_dto(&uuid_n(2), "Ep One", Some(&season), BaseItemKind::Episode);
        ep1.index_number = Some(1);
        let mut ep2 = item_dto(&uuid_n(3), "Ep Two", Some(&season), BaseItemKind::Episode);
        ep2.index_number = Some(2);
        // No index_number at all (real: a badly-tagged file the server
        // couldn't number).
        let unnumbered = item_dto(
            &uuid_n(4),
            "Unnumbered",
            Some(&season),
            BaseItemKind::Episode,
        );
        apply_upsert_items(&mut conn, &[ep1, ep2, unnumbered]).expect("insert");

        let rows = children(&conn, &season, Sort::IndexNumber, 0, 10);
        assert_eq!(
            rows.iter().map(|r| r.index_number).collect::<Vec<_>>(),
            vec![None, Some(1), Some(2)],
            "an un-numbered episode sorts FIRST under Sort::IndexNumber"
        );
    }

    /// Pins: a "Specials" season (`IndexNumber == 0`) sorts ahead of season 1 -- the list `find_series_next_episode`/`adjacent_episode` walk in order.
    #[test]
    fn children_with_index_number_sort_places_season_zero_specials_first() {
        let (_dir, mut conn) = open_test_db();
        let series = uuid_n(1);
        let mut specials = item_dto(&uuid_n(2), "Specials", Some(&series), BaseItemKind::Season);
        specials.index_number = Some(0);
        let mut s1 = item_dto(&uuid_n(3), "Season 1", Some(&series), BaseItemKind::Season);
        s1.index_number = Some(1);
        let mut s2 = item_dto(&uuid_n(4), "Season 2", Some(&series), BaseItemKind::Season);
        s2.index_number = Some(2);
        apply_upsert_items(&mut conn, &[s1, s2, specials]).expect("insert");

        let rows = children(&conn, &series, Sort::IndexNumber, 0, 10);
        assert_eq!(
            rows.iter().map(|r| r.index_number).collect::<Vec<_>>(),
            vec![Some(0), Some(1), Some(2)],
            "Specials (season 0) sorts ahead of season 1"
        );
    }

    /// Pins: virtual episodes are NOT filtered out of a `children()` list -- every consumer must check `CardRow::is_virtual` itself.
    #[test]
    fn children_with_index_number_sort_keeps_virtual_episodes_in_line() {
        let (_dir, mut conn) = open_test_db();
        let season = uuid_n(1);
        let mut aired = item_dto(&uuid_n(2), "Aired", Some(&season), BaseItemKind::Episode);
        aired.index_number = Some(1);
        let mut unaired = item_dto(&uuid_n(3), "Unaired", Some(&season), BaseItemKind::Episode);
        unaired.index_number = Some(2);
        unaired.location_type = Some(jellyfin_api::models::LocationType::Virtual);
        apply_upsert_items(&mut conn, &[aired, unaired]).expect("insert");

        let rows = children(&conn, &season, Sort::IndexNumber, 0, 10);
        assert_eq!(
            rows.iter()
                .map(|r| (r.index_number, r.is_virtual))
                .collect::<Vec<_>>(),
            vec![(Some(1), false), (Some(2), true)],
            "a virtual episode stays in the ordered children list"
        );
    }

    // Duplicate-Season episode resolution: a real server can hold two
    // "Season 1" items after a rescan. `/Shows/{id}/Seasons` returns only
    // one, so that's the only Season row this mirror stores, but every
    // episode's `ParentId` points at the OTHER (unsynced) duplicate. See
    // `episodes_of_season_checked`'s doc comment for the fix.

    /// Pins: a season whose episodes' `parent_id` all point at an unsynced duplicate still returns all episodes, in order, with adjacency intact.
    #[test]
    fn episodes_of_season_resolve_by_series_and_season_number_not_literal_parent_id() {
        let (_dir, mut conn) = open_test_db();
        let series = uuid_n(1);
        let season_a = uuid_n(2); // the only Season row the mirror stores
        let season_b = uuid_n(3); // every episode's ParentId -- never synced

        let mut season_a_dto = item_dto(&season_a, "Season 1", Some(&series), BaseItemKind::Season);
        season_a_dto.series_id = Some(uuid::Uuid::parse_str(&series).expect("uuid"));
        season_a_dto.index_number = Some(1);
        apply_upsert_items(&mut conn, &[season_a_dto]).expect("insert season");

        let episodes: Vec<BaseItemDto> = (1..=22u8)
            .map(|n| {
                let mut ep = item_dto(
                    &uuid_n(100 + n),
                    &format!("S1E{n}"),
                    Some(&season_b),
                    BaseItemKind::Episode,
                );
                ep.series_id = Some(uuid::Uuid::parse_str(&series).expect("uuid"));
                // The live bug's exact shape: `SeasonId` (and thus, via
                // `rows::browse_parent_id`, the mirror's own `parent_id`
                // column) names the OTHER, never-synced duplicate -- not
                // `season_a`, the one this mirror actually stores as a
                // Season row.
                ep.season_id = Some(uuid::Uuid::parse_str(&season_b).expect("uuid"));
                ep.parent_index_number = Some(1);
                ep.index_number = Some(i32::from(n));
                ep
            })
            .collect();
        apply_upsert_items(&mut conn, &episodes).expect("insert episodes");

        let rows = children(&conn, &season_a, Sort::IndexNumber, 0, 50);
        assert_eq!(
            rows.iter().map(|r| r.index_number).collect::<Vec<_>>(),
            (1..=22).map(Some).collect::<Vec<_>>(),
            "must return all 22 episodes in index order, resolved via \
             series_id + parent_index_number rather than the literal \
             (unsynced) parent_id -- got {} rows: {:?}",
            rows.len(),
            rows.iter().map(|r| &r.name).collect::<Vec<_>>()
        );

        // Adjacency: `app::root::adjacent_episode` does exactly this --
        // `position()` the current episode, then step by one -- over a
        // `Mirror::children(season_id, ..)` list.
        let ix = rows
            .iter()
            .position(|r| r.id == uuid_n(100 + 10))
            .expect("episode 10 present");
        assert_eq!(rows[ix].index_number, Some(10));
        assert_eq!(
            rows[ix + 1].index_number,
            Some(11),
            "next-episode navigation must land on episode 11"
        );
        assert_eq!(
            rows[ix - 1].index_number,
            Some(9),
            "prev-episode navigation must land on episode 9"
        );
    }

    /// Pins: an episode with no `ParentIndexNumber` falls back to `parent_id` equality instead of vanishing from every season's list.
    #[test]
    fn episodes_of_season_falls_back_to_parent_id_when_parent_index_number_is_null() {
        let (_dir, mut conn) = open_test_db();
        let series = uuid_n(1);
        let season = uuid_n(2);

        let mut season_dto = item_dto(&season, "Season 1", Some(&series), BaseItemKind::Season);
        season_dto.series_id = Some(uuid::Uuid::parse_str(&series).expect("uuid"));
        season_dto.index_number = Some(1);
        apply_upsert_items(&mut conn, &[season_dto]).expect("insert season");

        let mut unnumbered = item_dto(
            &uuid_n(3),
            "Mystery Episode",
            Some(&season),
            BaseItemKind::Episode,
        );
        unnumbered.series_id = Some(uuid::Uuid::parse_str(&series).expect("uuid"));
        unnumbered.season_id = Some(uuid::Uuid::parse_str(&season).expect("uuid"));
        // Deliberately no `parent_index_number` -- the fallback case.
        apply_upsert_items(&mut conn, &[unnumbered]).expect("insert episode");

        let rows = children(&conn, &season, Sort::IndexNumber, 0, 50);
        assert_eq!(
            rows.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(),
            vec!["Mystery Episode"],
            "a NULL parent_index_number episode must still resolve via its \
             parent_id"
        );
    }

    /// Pins: two seasons of the same series have fully disjoint episode lists.
    #[test]
    fn episodes_of_season_does_not_bleed_across_seasons() {
        let (_dir, mut conn) = open_test_db();
        let series = uuid_n(1);
        let season1 = uuid_n(2);
        let season2 = uuid_n(3);

        let mut season1_dto = item_dto(&season1, "Season 1", Some(&series), BaseItemKind::Season);
        season1_dto.series_id = Some(uuid::Uuid::parse_str(&series).expect("uuid"));
        season1_dto.index_number = Some(1);
        let mut season2_dto = item_dto(&season2, "Season 2", Some(&series), BaseItemKind::Season);
        season2_dto.series_id = Some(uuid::Uuid::parse_str(&series).expect("uuid"));
        season2_dto.index_number = Some(2);
        apply_upsert_items(&mut conn, &[season1_dto, season2_dto]).expect("insert seasons");

        let mut ep_s1 = item_dto(&uuid_n(4), "S1E1", Some(&season1), BaseItemKind::Episode);
        ep_s1.series_id = Some(uuid::Uuid::parse_str(&series).expect("uuid"));
        ep_s1.season_id = Some(uuid::Uuid::parse_str(&season1).expect("uuid"));
        ep_s1.parent_index_number = Some(1);
        ep_s1.index_number = Some(1);
        let mut ep_s2 = item_dto(&uuid_n(5), "S2E1", Some(&season2), BaseItemKind::Episode);
        ep_s2.series_id = Some(uuid::Uuid::parse_str(&series).expect("uuid"));
        ep_s2.season_id = Some(uuid::Uuid::parse_str(&season2).expect("uuid"));
        ep_s2.parent_index_number = Some(2);
        ep_s2.index_number = Some(1);
        apply_upsert_items(&mut conn, &[ep_s1, ep_s2]).expect("insert episodes");

        let season1_rows = children(&conn, &season1, Sort::IndexNumber, 0, 50);
        let season2_rows = children(&conn, &season2, Sort::IndexNumber, 0, 50);
        assert_eq!(
            season1_rows
                .iter()
                .map(|r| r.name.as_str())
                .collect::<Vec<_>>(),
            vec!["S1E1"]
        );
        assert_eq!(
            season2_rows
                .iter()
                .map(|r| r.name.as_str())
                .collect::<Vec<_>>(),
            vec!["S2E1"]
        );
    }

    /// Pins: resolving a season's episodes by `series_id` + `parent_index_number` doesn't degrade to a full `items` scan.
    #[test]
    fn episodes_of_season_query_does_not_scan_items() {
        let (_dir, conn) = open_test_db();
        let plan = explain(
            &conn,
            "SELECT id FROM items \
             WHERE item_type = 'Episode' AND series_id = ?1 \
             AND (parent_index_number = ?2 \
                  OR (parent_index_number IS NULL AND parent_id = ?3)) \
             ORDER BY parent_index_number ASC, index_number ASC LIMIT ?4 OFFSET ?5",
            params![
                String::from("series-1"),
                1i32,
                String::from("season-a"),
                10u32,
                0u32
            ],
        );
        assert_no_items_scan(&plan);
    }

    /// Pins: a Series' browse-children set is exactly its Seasons -- not the `Folder`/Episode rows a real server can attach alongside them, and including a Virtual season.
    #[test]
    fn series_children_returns_only_seasons() {
        let (_dir, mut conn) = open_test_db();
        let series = uuid_n(1);
        apply_upsert_items(
            &mut conn,
            &[item_dto(&series, "A Show", None, BaseItemKind::Series)],
        )
        .expect("insert series");

        let mut season1 = item_dto(&uuid_n(2), "Season 1", Some(&series), BaseItemKind::Season);
        season1.series_id = Some(uuid::Uuid::parse_str(&series).expect("uuid"));
        season1.index_number = Some(1);

        let mut season5_virtual =
            item_dto(&uuid_n(3), "Season 5", Some(&series), BaseItemKind::Season);
        season5_virtual.series_id = Some(uuid::Uuid::parse_str(&series).expect("uuid"));
        season5_virtual.index_number = Some(5);
        season5_virtual.location_type = Some(jellyfin_api::models::LocationType::Virtual);

        let mut release_dir_1 = item_dto(
            &uuid_n(4),
            "A.Show.S05E01.1080p.WEB-MeGusta",
            Some(&series),
            BaseItemKind::Folder,
        );
        release_dir_1.series_id = None;
        let mut release_dir_2 = item_dto(
            &uuid_n(5),
            "A.Show.S05E02.1080p.WEB-MeGusta",
            Some(&series),
            BaseItemKind::Folder,
        );
        release_dir_2.series_id = None;

        let mut stray_episode = item_dto(
            &uuid_n(6),
            "Directly Attached Ep",
            Some(&series),
            BaseItemKind::Episode,
        );
        stray_episode.series_id = Some(uuid::Uuid::parse_str(&series).expect("uuid"));

        apply_upsert_items(
            &mut conn,
            &[
                season1,
                season5_virtual,
                release_dir_1,
                release_dir_2,
                stray_episode,
            ],
        )
        .expect("insert children");

        let rows = children(&conn, &series, Sort::IndexNumber, 0, 50);
        assert_eq!(
            rows.iter()
                .map(|r| (r.item_type.as_str(), r.index_number))
                .collect::<Vec<_>>(),
            vec![("Season", Some(1)), ("Season", Some(5))],
            "must return exactly the two Seasons, ordered by index_number, \
             including the virtual one, excluding the Folder and Episode \
             rows -- got {rows:?}"
        );
    }

    /// Pins: resolving a Series' Seasons doesn't degrade to a full `items` scan -- `idx_items_series` exists for exactly this lookup.
    #[test]
    fn series_children_query_does_not_scan_items() {
        let (_dir, conn) = open_test_db();
        let plan = explain(
            &conn,
            "SELECT id FROM items \
             WHERE item_type = 'Season' AND (series_id = ?1 \
                  OR (series_id IS NULL AND parent_id = ?1)) \
             ORDER BY parent_index_number ASC, index_number ASC LIMIT ?2 OFFSET ?3",
            params![String::from("series-1"), 10u32, 0u32],
        );
        assert_no_items_scan(&plan);
    }

    #[test]
    fn views_returns_sorted_by_sort_index() {
        let (_dir, conn) = open_test_db();
        conn.execute("INSERT INTO views (id, name, collection_type, sort_index, item_type) VALUES ('b', 'B', 'movies', 1, 'CollectionFolder')", [])
            .expect("insert");
        conn.execute("INSERT INTO views (id, name, collection_type, sort_index, item_type) VALUES ('a', 'A', 'movies', 0, 'CollectionFolder')", [])
            .expect("insert");
        assert_eq!(
            views(&conn),
            vec![
                crate::ViewSummary {
                    id: "a".to_string(),
                    name: "A".to_string(),
                    kind: crate::ViewKind::Library,
                },
                crate::ViewSummary {
                    id: "b".to_string(),
                    name: "B".to_string(),
                    kind: crate::ViewKind::Library,
                },
            ]
        );
    }

    /// Pins: a `Channel` view's `item_type` maps to `ViewKind::Channel` (docs/PLUGIN-CHANNELS.md §2.1).
    #[test]
    fn views_maps_channel_item_type_to_channel_kind() {
        let (_dir, conn) = open_test_db();
        conn.execute("INSERT INTO views (id, name, collection_type, sort_index, item_type) VALUES ('c', 'Recordings', '', 0, 'Channel')", [])
            .expect("insert");
        let result = views(&conn);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].name, "Recordings");
        assert_eq!(result[0].kind, crate::ViewKind::Channel);
    }
}
