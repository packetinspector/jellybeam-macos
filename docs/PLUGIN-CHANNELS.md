# Plugin channels (TVHeadend recordings and other Jellyfin plugin libraries)

Jellybeam treats any Jellyfin library whose `/UserViews` entry reports
`Type: "Channel"` as a plugin channel — the TVHeadend recordings plugin is
the motivating case, but every rule below keys off server-reported facts,
not a plugin's name. This section documents what such a server sends and
the rules Jellybeam follows to list and play that library correctly.

---

## 1. What the server actually sends

Facts observed on a live server, all of which drive the design:

1. **The library appears in `/UserViews` with `Type: "Channel"`** and no
   `CollectionType`. Every ordinary library is `Type: "CollectionFolder"`
   (or `"UserView"`) with a `CollectionType` such as `movies`/`tvshows`.
   `CollectionType` being empty is *not* a usable signal on its own — plenty
   of legitimately unsupported library kinds also leave it empty. Key off the
   entry's own `Type`.
2. **Its children are folders of `Type: "ChannelFolderItem"`**, and inside
   those, recordings. A recording arrives as a plain `Video` (or an `Episode`
   for series recordings) whose `ParentId`, `SeriesId` and `SeasonId` are
   **all null**. Only a folder's own non-recursive listing places a recording
   anywhere.
3. **Neither the WebSocket delta feed nor `LibraryChanged` ever mentions a
   channel item.** A mirrored copy could never heal itself. DVR content also
   churns independently of any library scan (recordings appear and expire).
4. **`MediaSources[0].Id` is a short plugin-scoped id, not the item id.** For
   ordinary items the two are equal and the distinction never shows.
   `GET /Videos/{sourceId}/stream` answers **HTTP 400**;
   `GET /Videos/{itemId}/stream?static=true&mediaSourceId={sourceId}` answers
   206 and plays.
5. **Every `MediaStream` on a recording has `Codec: null`.** Because the
   device profile declares codecs at all, the server answers
   `SupportsDirectPlay: false` plus a `TranscodingUrl`. That verdict is not
   evidence of incompatibility — the server had nothing to judge with. The
   static stream serves the bytes as-is and plays fine (recordings
   direct-play, server session reporting `DirectPlay`).

---

## 2. Rules

### 2.1 List the view, never mirror its content

- `sync_views` persists the `/UserViews` entry's own `Type` into a new
  `views.item_type TEXT` column (our schema bump v10 → v11). It is distinct
  from `collection_type`.
- `current_views` (the source of truth for every per-library sync walk)
  excludes rows where `item_type = 'Channel'`. That single exclusion covers:
  initial breadth sync, the one-time root-parent heal, delta/WS root-type
  flattening, and the WS `LibraryChanged` membership probe.
- `reconcile_all` skips `Channel` views explicitly as well (do not rely on
  the `item_types_for_collection` early-return — it keys off
  `collection_type`, see fact 1).
- Home shelves (`home_snapshot` in our FFI) skip `Channel` views.
- The view still appears in the drawer/sidebar with its server-configured
  name verbatim, carrying a kind flag (`ViewKind::Channel` vs
  `ViewKind::Library`) so the UI knows which browse path to take.

### 2.2 Browse live, non-recursive, in server order

One core method, `live_children(parent_id, start_index, limit) -> Vec<Card>`:

```
GET /Users/{userId}/Items
    ?parentId={folderOrViewId}
    &recursive=false
    &startIndex=…&limit=…
    &fields=Overview,OriginalTitle,SeriesName,DateCreated,PremiereDate,
            ImageBlurHashes,ParentId,SeriesPrimaryImageTag
```

- **No `sortBy`/`sortOrder`.** Whatever the plugin returns right now is the
  only order that is ever correct for DVR content.
- The same call serves the view itself (children = `ChannelFolderItem`
  folders) and each folder (children = recordings). A `ChannelFolderItem`
  card opens another live grid level, not a detail page; everything else
  opens detail as usual.
- The UI re-queries the live listing whenever a channel grid becomes
  visible again (return from playback or a nested folder), since there is no
  mirror change event to react to. Fail soft on a transient error: keep what
  is on screen.
- Ordinary paging works (`startIndex`/`limit`).

### 2.3 Playback: item id in the path, codec-blind sources direct-play

Two independent fixes in `jellyfin-core`/`jellyfin-api`:

1. **`stream_url(item_id, &source)`** — the `/Videos/{id}/stream` path
   segment must be the real item id; `mediaSourceId` is the only place
   `source.id` belongs. Thread `item_id` through `decide_playback` and
   `decide_playback_with_source` rather than reading it off the chosen source
   (fact 4).
2. **Codec-blind rule** in the source chooser (`playback.rs`):

   ```
   is_codec_blind(source) :=
       no Video or Audio MediaStream carries a non-empty Codec
       (zero streams counts as blind; Subtitle/other streams never count)
   ```

   Chooser order:
   - a source the server marks `SupportsDirectPlay: true` → Direct Play
     (unchanged, and still wins over a later codec-blind source);
   - else the first source that has a `TranscodingUrl` **and** is codec-blind
     → **Direct Play via the static stream**, not the transcode;
   - else a source with real codec facts and no direct play → transcode
     (opt-in, as today);
   - else no playable source.

   When a Direct Play choice lands on a source that carries a
   `TranscodingUrl`, build the URL from a copy with `transcoding_url` cleared
   so `stream_url` falls through to the static form. Log the codec-blind
   override at info level; it is a deliberate deviation from the server's
   verdict.

Direct Play stays the default; transcoding stays strictly opt-in. The
codec-blind rule widens *nothing* for sources that have codec facts — the
regression test `does_not_widen_rule_to_a_source_with_real_codec_facts`
pins that.

### 2.4 Detail page: single-file video kinds get Play/Resume

`Video`, `MusicVideo` and Live-TV `Recording` item types get the same
Play/Resume primary action as a `Movie` (same resume semantics). Before this,
a recording's detail page had no Play at all.

---

## 3. Tests

media-cache:
- `sync_views_stores_item_type` — `Type: "Channel"` lands in `views.item_type`.
- `current_views_excludes_channel_rows`; `reconcile_all` skips a Channel view.

jellyfin-api / jellyfin-core:
- `stream_url_uses_item_id_for_path_and_source_id_for_query_when_they_differ`.
- `decide_playback_end_to_end_uses_item_id_when_source_id_differs`.
- `picks_codec_blind_source_over_transcoding_when_server_had_no_codec_facts`.
- `does_not_widen_rule_to_a_source_with_real_codec_facts`.
- `a_real_direct_play_source_wins_over_a_later_codec_blind_one`.
- `zero_media_streams_counts_as_codec_blind`;
  `subtitle_only_codec_info_still_counts_as_blind`.
- Live-browse contract: request has `parentId=…`, `recursive=false`, no
  `sortBy`.

UI:
- a `ChannelFolderItem` card routes to a nested live grid, not detail;
- a Channel view is never subscribed to mirror change events and re-lists
  on return;
- `Video`/`MusicVideo`/`Recording` resolve to Play/Resume.

---

## 4. Hazards we hit

- **Do not sync a Channel view "just to see".** With no parent linkage
  (fact 2) the mirror stores orphans it can never place or delete.
- **Do not build the stream path from `MediaSourceInfo.id`** anywhere — it
  is only equal to the item id by coincidence for library items.
- **Do not treat an empty `CollectionType` as "channel".** Use `Type`.
- **A blind Direct Play verdict is not a transcode signal.** Check codec
  facts before honouring `SupportsDirectPlay: false`.
- Recordings can carry `Type: "Episode"` with null series linkage; anything
  that assumes an Episode has a Series/Season needs a null-safe path on the
  detail/OSD side (our OSD breadcrumb joins only the fields present, so a
  bare item name renders alone).
- Test fixtures must use synthetic ids/hostnames only; never paste real
  server payloads into the repo.

---

## 5. Verification

1. Server with the TVHeadend plugin enabled and at least one recording.
2. `/UserViews` shows the recordings library with `Type: "Channel"`.
3. Drawer lists it verbatim; opening it lists `ChannelFolderItem` folders in
   plugin order; a folder lists recordings; returning after a new recording
   completes shows it without restart.
4. Playing a recording: session reports `DirectPlay`, URL is
   `/Videos/{itemId}/stream?static=true&mediaSourceId=<short id>`, HTTP 206.
5. Home shelves and the offline mirror contain no channel items after a full
   sync.
