# Jellybeam — Data & Sync Design

The local mirror that makes navigation instant (docs/OVERVIEW.md §5b). Cache is disposable;
server is the only source of truth. Nothing here migrates — version mismatch drops
and rebuilds.

## 1. SQLite mirror

One file per server: `~/Library/Application Support/Jellybeam/mirror-<server_id>.db`
(WAL mode). Single writer (one tokio task owns all writes); readers use a read-only
pool. `schema_version` pragma checked at open → mismatch = delete file, full resync.

```sql
CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT);
  -- schema_version, server_id, server_name, last_full_sync, user_id

CREATE TABLE views (            -- top-level libraries (sidebar)
  id TEXT PRIMARY KEY, name TEXT, collection_type TEXT, sort_index INTEGER);

CREATE TABLE items (
  id TEXT PRIMARY KEY,
  parent_id TEXT, series_id TEXT, season_id TEXT,
  item_type TEXT NOT NULL,            -- Movie|Series|Season|Episode|BoxSet|...
  name TEXT, sort_name TEXT,
  index_number INTEGER, parent_index_number INTEGER,   -- episode/season numbers
  production_year INTEGER, premiere_date TEXT,
  runtime_ticks INTEGER, date_created TEXT,
  community_rating REAL, official_rating TEXT,
  -- user data (hot for badges/progress; duplicated out of the blob)
  played INTEGER NOT NULL DEFAULT 0,
  playback_position_ticks INTEGER NOT NULL DEFAULT 0,
  play_count INTEGER NOT NULL DEFAULT 0,
  is_favorite INTEGER NOT NULL DEFAULT 0,
  unplayed_item_count INTEGER,         -- series/season rollup
  -- images
  primary_tag TEXT, backdrop_tag TEXT, thumb_tag TEXT, primary_blurhash TEXT,
  -- everything else rides in the blob (the decoded BaseItemDto re-serialized;
  -- fields our generated models don't know yet are NOT retained — to use a new
  -- server field: regen models, bump schema_version, rebuild. Consistent with
  -- the disposable-cache rule.)
  dto BLOB NOT NULL,
  updated_at INTEGER NOT NULL          -- local clock, for debugging only
);
CREATE INDEX idx_items_browse  ON items(parent_id, item_type, sort_name);
CREATE INDEX idx_items_latest  ON items(item_type, date_created DESC);
CREATE INDEX idx_items_series  ON items(series_id, parent_index_number, index_number);
CREATE INDEX idx_items_resume  ON items(playback_position_ticks) WHERE playback_position_ticks > 0;

CREATE VIRTUAL TABLE search USING fts5(  -- search overlay (<50ms budget)
  name, original_title, series_name, overview,
  content='', tokenize='unicode61 remove_diacritics 2');
  -- external-content mode; rows upserted alongside items; rowid = items.rowid

CREATE TABLE image_lru (                 -- disk image cache bookkeeping
  key TEXT PRIMARY KEY,                  -- item_id/type/tag/size
  bytes INTEGER, last_access INTEGER);
```

Blob-first rule: any field the UI newly needs is *promoted* to a column by bumping
`schema_version` (cheap — rebuild, not migrate). Never parse the blob on the browse
path; columns exist precisely so grids/rows are pure indexed SQL.

Query budget: every browse query must be satisfiable from an index above —
`EXPLAIN QUERY PLAN` asserted in tests (no SCAN on items for grid/row/resume/latest).

## 2. Sync protocol

**Initial sync** (first run / rebuild): enumerate `/Users/{id}/Views`; per library,
page `/Items` (`ParentId`, `Recursive=true`, `Fields=` the column set + blurhashes,
page size 500) → bulk upsert in one transaction per page. Home is renderable as soon
as the first pages land (sync order: Resume + NextUp + Latest first, then breadth).

**Live updates** (the "know right away" path): WebSocket session subscribes to
- `LibraryChanged` → `ItemsAdded`/`ItemsUpdated`/`ItemsRemoved`: batch-fetch changed
  ids via `/Items?Ids=...` (chunks of 100), upsert; removals delete (cascade by
  `series_id`/`parent_id` where the server signals folder removal).
- `UserDataChanged` → update `played`/`playback_position_ticks`/`play_count`/
  `is_favorite` columns directly (covers other clients' watch activity too).
- Mirror emits a change-feed (tokio broadcast of affected item/view ids) that the UI
  subscribes to for live cell updates — no polling, no view-wide refresh.

**Delta sync** (the fast catch-up path): one recursive `/Items` query filtered by
`minDateLastSaved` (+ `minDateLastSavedForUser`, a 10.10.x server-bug workaround)
returns everything ADDED or UPDATED since a stored cursor, across every library at
once, and upserts it. Runs before every reconcile trigger. Structurally cannot see
deletions — the server has no tombstones, so a deleted item simply stops appearing.

**Reconciliation** (belt & suspenders, and the only thing that catches DELETIONS):
on app activation, WebSocket (re)connect, and every 5 min idle: per library compare
server `TotalRecordCount`, max `DateLastMediaAdded`/newest `DateCreated`, and the
presence of the server's 20 newest ids against the mirror.

Mismatch → **ID sweep** of that library, *not* a full breadth resync. The sweep pages
the same recursive `/Items?parentId=<view>&recursive=true` query the breadth walk
uses, but asks for ids only (`enableImages=false`, `enableUserData=false`, no
`fields`, page size 1000 — tens of KB per page instead of megabytes), diffs that id
set against the mirror's rows for that `library_id`, then:

- **orphans** (local ∖ server) → the same `PruneLibrary` write the completed breadth
  walk issued, with the enumerated server ids as `keep_ids`. Scoped to one
  `library_id`, so an item that lives in another library and merely appears in this
  one's collections is never touched; a pruned id's `collection_members` rows go with
  it in the same transaction.
- **missing** (server ∖ local) → full DTOs fetched for just those ids, in chunks of
  100 (the same by-ids batch shape the WS path uses), upserted scoped to the view.

Fail-closed: every read happens before any deletion, so a failed enumeration page or
repair chunk leaves the last-good mirror state and the mismatch standing for the next
tick — there is deliberately no fallback to the full walk. The full breadth walk stays
in place for initial sync. A library that lost 8 of 5,247 items therefore costs ~6
ids-pages plus nothing else, seconds rather than minutes, and the sync pill flashes
instead of grinding. WebSocket outage therefore degrades to eventual consistency,
never to wrongness.

**Ordering rule**: all mutations (initial, ws, reconcile) flow through the single
writer task's queue — no interleaving hazards, and the change-feed is emitted in
commit order.

## 3. Image pipeline

- Disk layout: `Caches/Jellybeam/images/<server>/<item>-<type>-<tag>-<w>.jpg` — `tag`
  in the key makes entries self-invalidating (server changes artwork ⇒ new tag ⇒
  new key; old entry ages out via LRU).
- Request path: memory LRU (decoded textures, ~256 MB budget shared with GPUI's
  atlas) → disk → network (`/Items/{id}/Images/{type}?tag=&maxWidth=`), with
  request coalescing (N cells asking for one image = one fetch) and cancel-on-scroll
  (visible + 1 screen ahead keep priority; offscreen requests are dropped, not queued).
- Sizes: fetch at exactly the largest cell size in use (2:3 grid ⇒ one width bucket;
  detail backdrop separate). No client-side downscaling of oversized fetches.
- Blurhash from the mirror paints the placeholder in the same frame the cell appears.
- Disk budget 2 GB, LRU-evicted via `image_lru` (batch, on idle).

## 4. Prefetch rules (from docs/UX-SPEC.md)

- Focus/hover dwell 350 ms → fetch Detail payload (already in mirror — this warms
  *images*: backdrop + cast strip) at low priority.
- Playback start → prefetch trickplay sprite sheets for the item (all tiles, they're
  small) + next episode's Detail images (Next Up continuation is the common path).
- Home visible → warm first-screen images for each shelf.

## 5. What is *not* cached

Playback state (positions during playback go to the server via the reporting state
machine, mirror updated from its acks/`UserDataChanged`), auth tokens (Keychain),
settings (config file), PlaybackInfo/MediaSource decisions (always fresh per play —
they depend on live bitrate/codec context), trickplay/sprite images (plain disk cache,
same LRU as images).
