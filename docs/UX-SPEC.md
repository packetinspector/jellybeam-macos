# Jellybeam — UX Specification

The interaction contract for the `app` crate. Perf budgets in docs/OVERVIEW.md §5b apply to
every transition described here. Design language: one fast, beautiful dark theme;
content-forward (posters are the UI); chrome recedes.

## 1. View map

```
Window
├── Home            (default view: Continue Watching · Next Up · Latest by library)
├── Library         (one per server library: poster wall + filter/sort bar)
├── Detail          (movie or series; series → seasons → episodes inline)
├── Search          (overlay, not a page — summoned from anywhere)
├── Player          (three states: Fullscreen-in-window / OS-Fullscreen / Miniplayer)
└── Settings        (sheet: server, playback, subtitles, about)
```

Navigation model: a **sidebar** (Home + one entry per library + Settings) and a
**content pane**. Back/forward history per pane (⌘[ / ⌘]). No tabs, no breadcrumbs —
depth is at most Library → Detail.

## 2. Keyboard-first (a speed feature)

Full app drivable without the mouse. Focus is always visible (subtle scale + glow on
the focused poster — GPU-cheap).

| Key | Context | Action |
|---|---|---|
| Arrows | grids/rows | move focus (grid-aware, remembers column) |
| Return | focused item | open Detail; on Detail: play |
| Space | Detail/Player | play/pause |
| ⌘F or `/` | anywhere | Search overlay |
| Esc | anywhere | dismiss overlay/picker, else back (Browse) or step down one Player layer (see §3) |
| ⌘1..9 | anywhere | jump to sidebar entry N |
| F | Player | toggle fullscreen |
| M | Player | mute · ↑↓ volume |
| ←/→ | Player | seek −10s/+10s (⇧: −60s/+60s) |
| S / A | Player | subtitle / audio track cycler (hold or repeat opens picker) |
| I | Player | toggle info OSD (stats: codec, direct-play vs transcode, bitrate) |
| Tab | Player | focus escape hatch to miniplayer/browse |

Search overlay: type-to-filter against the local index (<50 ms budget), arrow +
Return to jump. First result row is "top matches" across all libraries.

## 3. Player states & miniplayer state machine

The video surface is one scene node that moves between three states — never
destroyed/recreated on transition (docs/OVERVIEW.md §5):

```
        play item              ⌘M / click browse UI / Esc
  (any) ────────► Fullscreen-in-window ──────────────► Miniplayer
                     │  ▲                                 │ ▲
                   F │  │ F/Esc            click video /  │ │ hover: controls
                     ▼  │                  Return on it   ▼ │
                  OS-Fullscreen ◄────────────────────── (restore)
```

- **Fullscreen-in-window**: video fills content pane; sidebar auto-hides; OSD
  (scrubber + tracks + chapters) on mouse-move/keys, fades after 2.5 s.
- **Miniplayer**: bottom-right floating node (16:9, ~380 px wide, draggable to
  corners), keeps playing with audio; browse/search fully usable behind it. Hover
  shows mini-OSD (play/pause, scrub strip, restore, close). Starting a *new* item
  from browse replaces the miniplayer content (no stacking).
- **OS-Fullscreen**: native macOS fullscreen space. Same OSD.
- Transitions are animated (150 ms, GPU transform only — no relayout of the video node).
- **Esc is a step-down, not a stop**: from OS-Fullscreen it leaves the fullscreen
  Space (→ Fullscreen-in-window); from Fullscreen-in-window it collapses to the
  Miniplayer; only from the Miniplayer — the bottom layer, nothing left to step
  down to — does Esc stop playback. (The next-episode card's own dismiss-once Esc,
  and closing an open picker/overlay, both still take priority over all of this.)
  Stop is always additionally reachable via the OSD stop button or the
  Miniplayer's close button, regardless of layer.

## 4. Scrubber & trickplay

- Scrubber hover: trickplay preview tile above cursor (from sprite-sheet cache,
  prefetched at playback start) + timestamp + chapter name.
- Chapter ticks on the scrubber; Media Segments render as a "Skip Intro/Credits"
  pill (appears during segment, Return/click activates, auto-hides).
- Seek commits use mpv `hr-seek`; scrub-drag shows trickplay frames only (no live
  decode thrash), commit on release.

## 5. Browse surfaces

- **Poster wall**: virtualized grid, fixed cell aspect (2:3 posters, 16:9 for
  episodes), scroll at 120 fps. Images fade in ≤100 ms from cache; blurhash
  placeholder first paint. Focus/hover raises the item (scale 1.05) and — after
  350 ms dwell — prefetches its Detail payload + backdrop.
- **Rows** (Home): horizontally scrolling shelves; same cells, same focus rules.
- **Detail**: backdrop art (dimmed), poster, metadata line (year · runtime ·
  codec badges: 4K/HDR/Atmos-style flags from MediaStreams), play/resume button
  (shows resume point), overview, cast strip, seasons/episodes for series,
  track pre-selection (audio/sub pickers remember per-series preference).
- Unwatched badges + progress bars on cells from mirror data; update live on
  WebSocket events (including from other clients' playback).

## 6. States & failure UX

- Offline server: banner, browse continues from mirror (read-only), play disabled
  with reason; auto-retry with backoff, banner clears on reconnect.
- Transcode fallback engaged: subtle "Transcoding (reason)" tag in OSD info —
  never silent (direct-play regressions must be visible, per DeviceProfile risk).
- Empty/loading: skeleton cells, never spinners on the critical path.
- First-run: server URL + credentials (Quick Connect added in v1), then immediate
  library sync with progress and streaming-in posters — first paint of Home under
  the launch budget even while sync continues.

## 7. Explicit non-goals (MVP)

No theming, no layout customization, no music/photo views, no admin surfaces.
One window (plus OS-fullscreen space). No menu-bar extra.
