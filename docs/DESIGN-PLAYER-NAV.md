# Jellybeam — Player OSD & Navigation Design Spec

This spec covers two things:

1. **Part 1**: the player's on-screen display (OSD): what is on screen,
   where, how big, and when it shows.
2. **Part 2**: Series → Season → Episode navigation, including the
   next-episode flow, built on the mirror-first architecture
   (`docs/DATA.md`). Speed is a hard constraint.

The OSD is plain GPUI (`crates/app/src/player_ui.rs`) painted over the
libmpv video layer. Icons are embedded Lucide SVGs (§1.14, §1.15). Colours
and fonts come from `crates/app/src/theme.rs`. The numbers below are the
constants in the code.

---

## Part 1 — Media Player OSD

### 1.1 Design principles

- **Content first.** The OSD shows on activity and gets out of the way
  otherwise. Controls sit on gradient scrims, never on opaque panels, so the
  frame stays readable edge to edge.
- **Play/pause is the largest control.** The primary action is never the
  same size as a utility button.
- **One bottom bar.** A scrubber row, then one controls row. Nothing else
  is permanent chrome.
- **Feedback does not depend on the bar.** Keyboard actions (Space, ←/→)
  flash a centre glyph even when the bar is hidden, so the viewer always
  sees that the key took effect.
- **Fixed order at the edges.** Window-state controls (miniplayer,
  fullscreen) are always the outermost right-hand controls, so they are
  always in the same place.
- **Idle costs nothing.** Visibility is a single boolean driven by
  activity. A hidden OSD paints only the overlays that are independent of
  it (§1.9, skip pill, toasts, next-episode card).

### 1.2 Element inventory (what's on screen, and where)

```
┌─ top-left (with OSD) ─────────────────────────────────────┐
│ Series Name · S2 E4 · Episode Title   (§1.8, clickable §2.4) │
│ [info panel when toggled with I]                           │
└──────────────────────────────────────────────────────────┘

┌─ centre (transient, ~720 ms) ─────────────────────────────┐
│           ⏸ / ▶  or  ↺10  or  ↻10   (§1.9)                 │
└──────────────────────────────────────────────────────────┘

┌─ bottom scrim (gradient, §1.3) ───────────────────────────┐
│  0:42:11                                          -0:38:04  │  ← time labels
│  ─────────●━━━━━━━━━━━━━━━━━━━━━━━───────────────────────  │  ← scrubber
│  [🔊━━]      [■][⏮][↺10][ ▶ ][↻10][⏭]      [♫][CC] DIRECT PLAY [ⓘ] │ [⧉][⛶] │
└──────────────────────────────────────────────────────────┘
```

- **Top-left**: title/breadcrumb (§1.8) and, below it in the same anchored
  column, the info panel (`I`).
- **Centre**: transient play/pause/skip flash (§1.9).
- **Bottom**: time-label row, scrubber, controls row (§1.13).
- **Skip pill** (`render_skip_pill`): bottom-right, just above the control
  zone, shown while the playhead is inside a media segment whose action is
  Ask (`docs/UX-SPEC.md §4`).
- **Trickplay bubble** (`render_trickplay_preview`): above the scrubber at
  the hover position (§1.4).
- **Next-episode card**: bottom-right (§2.4).
- **Speed-hold chip**: top-right, while Option is held.
- **Toasts** (track cycle, auto-skip Undo): top-centre.

### 1.3 Bottom scrim

The bottom bar sits on a `linear_gradient` scrim: transparent at the top,
NOTTE at 0.8 alpha at the bottom edge, 140 px tall
(`BOTTOM_GRADIENT_HEIGHT`). A matching 120 px top scrim
(`GRADIENT_HEIGHT`) runs the other way behind the title. The controls sit in
the bottom of the band where it is near-opaque, so text contrast holds
without a second flat layer, and the video never shows a hard edge. The top
scrim also renders while the info panel is open with the OSD hidden, so the
panel never sits on bare video.

### 1.4 Scrubber anatomy

The scrubber spans the bar's width inside the 32 px side margins
(`SCRUB_MARGIN`). Layers, bottom to top (`scrub_track_bar`, `scrub_row`):

1. **Track**: 4 px at rest, 6 px on hover, PANNA at 10 %.
2. **Buffered range**: PANNA at 25 %, from 0 to
   `(position + demuxer-cache-duration) / duration`
   (`PlayerLiveInfo.cache`). It is never drawn short of the played fill.
   The viewer can see how far ahead the stream is safe to seek.
3. **Played fill**: `theme::ACCENT`, 0 to `position / duration`.
4. **Chapter ticks**: 2 × 6 px marks at each chapter start after 0,
   centred on the track. Each tick is clickable (§1.16). They stay small
   because media segments get their own, larger skip pill.
5. **Playhead knob**: accent circle, 10 px at rest and 14 px on hover. It
   always tracks the playhead, never the pointer.
6. **Hover preview**: a thin translucent hairline at the pointer, plus a
   timestamp chip, plus the chapter name under the timestamp when the hover
   position is inside a chapter (`chapter_at`). With trickplay data, the
   trickplay tile renders above the bar with the same timestamp and
   chapter lines. Hovering never seeks. Only mouse-up (`commit_scrub`)
   moves the position. During a drag, debounced fast seeks keep the frame
   live.

The hit row is 24 px tall (`SCRUB_HIT_HEIGHT`) around the thin track. A
4 px target is too small to hit reliably with a mouse.

### 1.5 Time display convention

- The time labels have **their own row directly above the track**. Elapsed
  is on the left and the right-hand label is on the right. Because the
  labels are on a separate row, the knob can sit at 0 % or 100 % without
  covering a label.
- **Default**: elapsed / `-remaining`. Remaining time is what a viewer
  wants mid-film.
- Clicking the right-hand label toggles it to `elapsed / total`. The choice
  is stored globally in `player_prefs.rs` (`RemainingDisplay`), so it
  applies to every item and every session.
- Labels have a fixed width (60 px) and use tabular figures, so the row
  does not jitter as digits change or the hour boundary is crossed.

### 1.6 Volume control UX

The volume control sits at the left edge of the controls row
(`volume_control`):

- **Icon button**: toggles mute. It shows `volume-x` when muted or at 0,
  otherwise `volume-2`.
- **Slider**: expands to the icon's right on hover, 60 px wide
  (`VOLUME_SLIDER_PX`), with a white fill. It collapses 400 ms after the
  pointer leaves (`VOLUME_COLLAPSE_DELAY`), so slipping off it mid-drag
  does not snap it shut.
- **Drag is relative to the mouse-down point** (`volume_drag_anchor`). The
  slider's on-screen position depends on the buttons to its left, so an
  absolute mapping would jump.
- While the slider is expanded it is its own activity: the OSD does not
  auto-hide (§1.11).
- Keyboard: `M` mutes, `↑`/`↓` step by 5 (`docs/UX-SPEC.md §2`).

### 1.7 Audio/subtitle menu placement

The right cluster starts with **audio track, then subtitles**. The audio
language is chosen before captions for it. Each button anchors a popover
(`ui/popover.rs`) that opens up and to the left, towards the video rather
than off the bottom of the window. Pickers list title, language and codec.
The selected row has a check mark. Up/Down/Return drive the picker from the
keyboard, and a filter box appears once a list exceeds 10 tracks. While a
picker is open the OSD does not auto-hide, and Esc closes the picker
instead of stepping down a layer.

### 1.8 Title / episode metadata placement

The top-left title (`render_title`) shows and hides with the rest of the
OSD, driven by the same `osd_visible`/`note_activity` state with no
separate timer. Format (`breadcrumb_title`):

- `Series Name · S2 E4 · Episode Title` when series, season and episode
  numbers are known;
- `Series Name · Episode Title` when only the series is known;
- the item title otherwise (films).

Series context comes from the Detail page when playback started there.
Otherwise it comes from the episode's own mirror record, so it is present
whatever the entry point (shelf, search). The title is 20 px Semibold,
700 px wide, and truncates with an ellipsis. When a series is known, the
title is a link (§2.4).

### 1.9 Center transient feedback

Play/pause and skip actions trigger a centre flash (`render_center_flash`):
a 72 px icon on a 112 px translucent NOTTE circle. It fades in over 80 ms,
holds until 400 ms, then fades out, 720 ms in total (`FLASH_*`). Skip
flashes overlay the actual configured skip length as a numeral. The flash
is a `with_animation` opacity curve keyed by a sequence number, so repeated
presses restart it. It renders **whether or not the bar is visible**,
because it confirms a keyboard action that often happens while the bar is
hidden.

### 1.10 Hit-target minimums & spacing

| Element | Size | Note |
|---|---|---|
| Primary (play/pause) | 40 × 40 px hit, 24 px icon | Largest control |
| Secondary icon buttons | 32 × 32 px hit, 20 px icon | Everything else in the bar |
| Cluster gap | 8 px (`gap_2`) | |
| Bar side margin | 32 px (`SCRUB_MARGIN`) | |
| Scrubber hit height | 24 px | Visible track 4–6 px |
| Controls row height | 44 px | |
| Volume slider (expanded) | 60 × 20 px | |

Secondary targets are 32 px, not a bare 28 px minimum, because they sit
over unpredictable video (subtitles, credits) and a larger target means
fewer mis-clicks.

### 1.11 Auto-hide behavior — exact rules

`PlayerUiState::tick_auto_hide(paused)`, polled from the OSD tick:

- **Timeout**: 3 s of no activity (`OSD_IDLE_TIMEOUT`). The pointer hides
  together with the OSD in the full player (macOS brings it back on the
  next move).
- **Never hides while paused.** Hiding controls over a still frame helps
  no one.
- **Never hides while** scrubbing, a track picker is open, the info panel
  is open, the volume slider is expanded, or a track toast is up.
- **Re-show triggers**: any mouse move over the player (`render_osd`'s
  full-surface `on_mouse_move` → `note_osd_activity`), and every
  OSD-relevant key. Keyboard and mouse both go through `note_osd_activity`,
  so what counts as activity is decided in one place.
- **Show/hide is instant, not faded.** GPUI 0.2.2 has no transition
  primitive for a plain `div`. The centre flash, toasts and skip-Undo toast
  animate their own opacity.
- **Subtitles move with the bar.** While the OSD is visible in the full
  player, mpv's `sub-pos` is raised by 14 points
  (`SUBTITLE_POS_CONTROLS_SHIFT`) so the controls never cover subtitles. It
  is restored when the OSD hides (`sync_subtitle_baseline`).

### 1.12 Miniplayer hover controls

The miniplayer (`render_miniplayer`) has only the minimal set of controls:
**expand**, **close** (stops playback) and **play/pause**, plus a
non-interactive 4 px progress strip. They fade in on hover over a bottom
gradient scrim, the same scrim pattern as the main OSD. Audio/subtitle
pickers and the info panel are not in the miniplayer. The miniplayer is
for keeping an eye on playback while browsing. A click on the video
restores the full player, and a drag snaps it to the nearest corner. A
5 px threshold tells a click from a drag. The icons are the same Lucide
files as the main OSD (`play`/`pause`/`maximize`/`x`), so the two states
look alike.

### 1.13 ASCII layout mockup — 1280px window, Fullscreen-in-window

```
0                                                                        1280
┌──────────────────────────────────────────────────────────────────────────┐
│ ░ Series Name · S3 E7 · Episode Title       (top scrim, 120px) ░░░░░░░░ │  ← top 16 / left 32
│                                                                           │
│                                  ⏸                                        │  ← centre flash, transient
│                                                                           │
│  ░░░░░░░░░░░░░░░░░░░░░ (bottom scrim begins, 140px) ░░░░░░░░░░░░░░░░░░░ │
│▓ 12:41                                                           -38:04 ▓│  ← time labels, 4px above track
│▓ ─────────●━━━━━━━━━━━━━━━━━━━━━━━━━━━──────────────────────────────── ▓│  ← scrubber, 24px hit
│▓   ↑ played        ↑ buffered                                            ▓│
│▓ [🔊━━]        [■][⏮][↺][ ▶ ][↻][⏭]        [♫][CC] │ DIRECT PLAY [ⓘ] │ [⧉][⛶] ▓│  ← controls row, 44px
│▓  volume        transport, centred          pickers  status  info  window ▓│
│▓ pb 16px                                                                 ▓│
└──────────────────────────────────────────────────────────────────────────┘
```

The controls row has three clusters. **Volume** is at the left edge.
**Transport** is centred between two flex spacers, in the order stop,
prev-chapter, skip back, play/pause, skip forward, next-chapter. Stop is a
playback function, so it lives with transport. The **right cluster**
holds the audio and subtitle pickers, the playback-mode cell (a spec-strip
cell reading `DIRECT PLAY` or `TRANSCODE`, so a transcode is never
silent), the info toggle (held-down while the panel is open), a divider,
then miniplayer and fullscreen. Fullscreen is last so it sits in the
corner. The fullscreen icon switches between `maximize` and `minimize`
with the OS fullscreen state. The content zone is scrubber 20 + gap 8 +
controls 44 + bottom padding 16 = 88 px (`CONTROL_ZONE_HEIGHT`). The skip
pill, trickplay bubble and next-episode card anchor against it.

### 1.14 Icon inventory (17 icons) — Lucide, ISC license

The OSD and miniplayer use Lucide icons, vendored unmodified under
`crates/app/assets/icons/` (licence in `icons/LICENSE`, ISC, with MIT for
the icons inherited from Feather). All are 24 × 24 viewBox, 2 px stroke,
`stroke="currentColor"`. GPUI rasterises an SVG to an alpha mask and tints
it with the element's `text_color`, so only the stroke's alpha matters.

| # | Icon | File | Used for |
|---|---|---|---|
| 1 | Play | `play.svg` | Play/resume; centre flash; miniplayer |
| 2 | Pause | `pause.svg` | Pause; centre flash; miniplayer |
| 3 | Stop | `square.svg` | Stop |
| 4 | Skip back | `rotate-ccw.svg` | Seek back (numeral overlaid as text, not baked into the icon) |
| 5 | Skip forward | `rotate-cw.svg` | Seek forward (same numeral overlay) |
| 6 | Previous chapter | `skip-back.svg` | Previous chapter boundary |
| 7 | Next chapter | `skip-forward.svg` | Next chapter boundary |
| 8 | Volume | `volume-2.svg` | Unmuted |
| 9 | Muted | `volume-x.svg` | Muted or volume 0 |
| 10 | Subtitles | `captions.svg` | Subtitle picker |
| 11 | Audio | `audio-lines.svg` | Audio picker |
| 12 | Info | `info.svg` | Info panel (`I`) |
| 13 | Miniplayer | `picture-in-picture-2.svg` | Collapse to miniplayer |
| 14 | Fullscreen | `maximize.svg` | Enter OS fullscreen; miniplayer expand |
| 15 | Exit fullscreen | `minimize.svg` | Leave OS fullscreen |
| 16 | Close | `x.svg` | Miniplayer close |
| 17 | Selected | `check.svg` | Selected row in a track picker |

The numeral sits in a separate text layer, so the icon file stays
unmodified and the numeral stays sharp at any size.

### 1.15 GPUI wiring for the icons

GPUI's default `AssetSource` is `()`, which resolves every path to nothing.
`crates/app/src/assets.rs` embeds all of `crates/app/assets/` (icons, the
next-up scrim, brand fonts, mascot art) with one `rust_embed` derive,
`#[folder = "assets"] struct Embedded`. A public `Assets` type implements
`AssetSource` over it, and `main.rs` installs it with
`Application::new().with_assets(assets::Assets)`. Assets are baked into the
binary: no network and no runtime file I/O, so the UI never waits on disk
or network for them. Unit tests in `assets.rs` check that every icon path
referenced in the source and every brand font is actually embedded. A
typo would otherwise paint nothing, with no error.

Buttons go through one helper, `icon_button(id, path, hit, icon)`, with
`primary_button` (40/24) and `secondary_button` (32/20) wrappers. Each is a
fixed-size rounded box with a hover fill (`theme::ICON_HOVER_FILL`) around
an `svg().path(..)` tinted `TEXT_PRIMARY`.

### 1.16 Scrubber interaction rules

- **Hover previews, click commits.** Hover shows the hairline, timestamp,
  chapter name and trickplay tile without seeking. Mouse-down plus drag
  scrubs with debounced fast seeks. Mouse-up (inside or outside the bar)
  commits.
- **Chapter ticks jump.** Clicking a tick seeks straight to that chapter's
  start (`seek_to_click`), with the same commit-on-click model as the bar.
- **Prev/next-chapter buttons** give a second way to reach a boundary
  (`jump_chapter`). "Next" goes to the first chapter more than 0.5 s ahead.
  "Previous" goes to the last chapter more than 0.5 s behind, so it does
  not re-select the current one. It falls back to 0 before the first
  chapter.
- **The right-hand time label is a toggle** (§1.5).
- **The buffered layer** is painted from `PlayerLiveInfo.cache` behind the
  played fill (§1.4).
- **The OSD stays up while dragging and while paused** (§1.11).

---

## Part 2 — Navigation

### 2.1 Next-episode and navigation principles

- **Sidebar, not drawer.** Libraries, Search and Settings are always
  reachable from the sidebar (`docs/UX-SPEC.md §1`). That suits a mouse-
  and-keyboard desktop app.
- **Continue Watching and Next Up are separate shelves**, in that fixed
  order (`home.rs`: `mirror.resume(30)`, `mirror.next_up(30)`). Resuming
  and starting fresh are different intents.
- **Pass-out protection.** Auto-advance is never silent. It always runs
  through the visible next-episode card (§2.4) and its countdown. Autoplay
  is a setting. With autoplay off, the card still shows but never counts
  down. A dismissal stops the card and auto-advance for the rest of that
  item's playback, so a sleeping viewer is not carried through a season.
- **Chapters are one click away** during playback: prev/next-chapter
  buttons and clickable ticks (§1.16).
- **Series → Season → Episode** is the browse hierarchy. An episode can be
  played in one click from the rail, or opened for its own page (§2.4).

### 2.2 Home and browse shelves

- **Home** is a stack of horizontal shelves: Continue Watching, Next Up,
  then Latest per library. Continue Watching and Next Up use a resume card.
  An episode shows its own 16:9 still, not the series poster, so two
  episodes of one show are distinguishable. The three-line title block is
  title / series name / `S{s} E{e} · {remaining} left`.
- **Watched state** is a check-mark badge. In-progress items carry a
  progress bar. Unwatched counts badge series posters.
- **Episodes are a grid, not a list.** The Series page lays a season's
  episodes out as a responsive grid of 16:9 cells
  (`columns_for_width`/`CELL_WIDTH`). Long seasons stay scannable without
  a single very tall column.
- **Artwork is never distorted.** Every slot has a fixed aspect and a
  per-type fallback chain (§2.5), so a missing image falls back to a
  parent's image of the right shape, never to a crop or stretch.

### 2.3 Series Detail page anatomy

`detail.rs::render`, all painted from the mirror (§2.7):

- **Backdrop** (dimmed, with the §2.5 fallback chain), **poster** and
  **title** as text. Titles are always rendered as text, not logo art.
- **Metadata line and overview** at the series level.
- **Primary CTA**: a series-scoped **Resume** or **Up Next** button
  (`find_series_next_episode`). It walks the seasons in watch order, with
  Specials moved after the regular seasons. It picks the first in-progress
  episode, else the first unwatched one, and skips virtual (unaired or
  missing) episodes. All reads are local `Mirror::children` calls.
- **Season selector**: a horizontal row of tabs, not a dropdown. A tab row
  reads at a glance and is one click per season. Left/Right move between
  tabs from the keyboard (`DetailArea::Seasons`). The strip scrolls
  horizontally with edge fades when a show has more seasons than fit.
- **Episode grid**: 16:9 stills (`cards.rs::episode_card`) with progress
  bar and watched badge, an `E{n} · Title` line, a **runtime** line and a
  two-line synopsis. Virtual episodes are dimmed and show `Airs <date>` or
  `Missing` instead of a runtime.

### 2.4 Episode context — linking up, prev/next, play-next

**Episode Detail page.** Clicking an episode cell opens that episode's own
Detail page (`render_episode`): an uncropped 16:9 hero, a clickable
`S2 E4 · Series Name` breadcrumb, and a rail of the season's other
episodes. A hover play glyph on every cell plays the episode in one click,
so the page never adds a step for a viewer who only wants to watch. On the
Episode page's own rail, clicking a sibling swaps the page in place. The
season number and series name in the breadcrumb both open the series page
pre-selected to that season (`open_detail_at_season`, matched by the
season's own number so Specials (season 0) resolves correctly).

**Link up while playing.** The OSD title (§1.8) is the player's link up.
When a series is known, clicking it collapses the player to the miniplayer
and opens the series page at the playing episode's season
(`open_series_from_player`). Playback continues. Collapsing instead of
stopping follows the single-video-surface model (`docs/UX-SPEC.md §3`).

**Prev/next episode during playback.** `[` and `]` play the previous or
next episode (`play_adjacent_episode`). The arrow keys already seek (plain
←/→ by the configured skip length, Shift by ±60 s). There is no OSD button
for this, which keeps the bar at the §1.10 density. `adjacent_episode`
reads the season's episodes from the mirror in index order. It skips
virtual episodes and crosses season boundaries in watch order.

**Next-episode card.** A dismissible card at the end of an episode
(`render_next_episode_card`, `Root::tick_next_episode`):

1. **Detection** runs on the existing ~4 Hz position tick, with no extra
   player event. Real EOF (`on_end_of_file`) is a backstop: with autoplay
   on it advances, with autoplay off it shows the static card.
2. **Timing** (`next_episode_trigger_remaining_secs`): the card appears
   when the credits start (Outro media segment). Without a segment, it
   appears at 15 % of runtime from the end, clamped to 3–30 s. When
   credits are set to Auto-skip, the card comes forward by the autoplay
   delay: it appears delay + credits from the end. Its countdown is sized
   to the playable time (`next_episode_playable_secs`, up to the outro
   under auto-skip, else to EOF), so the next episode starts where the
   credits would have been skipped. The auto-skip seek stands down while
   the card counts down. A seek that lands inside auto-skipped credits
   before the card showed advances at once with no card.
3. **Countdown**: `min(playable, autoplay delay)`, delay 5/10/15 s,
   default 10 s. The countdown is captured when the card appears. The
   hand-over is a one-shot timer at the countdown's end
   (`arm_next_episode_handover`, generation-guarded), not the 4 Hz tick,
   so the empty rule, `0S` and the next episode's start happen together.
   Pausing freezes the countdown, and resuming re-arms it with the time
   remaining.
4. **Look**: no container (no fill, border, radius or shadow). The content
   sits on the video over a 550 × 310 px NOTTE radial scrim pinned to the
   bottom-right corner (`assets/scrim/next-up.svg`, with its top and left
   64 px faded so the clipped ellipse leaves no seam). The card sits at
   right 48 / bottom 96. Card and scrim move up by the control zone while
   the OSD shows. The card is a 360 px column:
   - A 128 × 72 still (blurhash placeholder while it loads) beside a text
     column: `UP NEXT` eyebrow (Martian Mono 12, GRIGIO); the title
     (Archivo 15 Medium, PANNA, wraps, never truncated); a runtime line
     (`22 MIN`, Mono 11 GRIGIO), omitted when there is no runtime.
   - An 8 px gap, then a 2 px rule: PANNA 16 % track, PISTACCHIO fill
     anchored left and depleting right to left over the countdown. Only
     the track shows when nothing counts down.
   - An 8 px gap, then a text-only action row: `ESC TO DISMISS` on the
     left; on the right, `RETURN` + `Play Next` (Archivo 14 Bold
     PISTACCHIO) + `IN 8S` (Mono 14 Bold PANNA, `m:ss` from a minute up).
     The numeral is absent when nothing counts down.
5. **Actions**: Return, or a click on the still, title or Play Next, plays
   next. Esc, or a click on the dismiss hint, dismisses. While the card is
   up, the first Esc dismisses it instead of stepping down a player layer.
6. **Films** have no next episode, so no card. The card needs a series and
   season context.

### 2.5 Artwork correctness rules per item type

Fields come from the generated `BaseItemDto`
(`crates/jellyfin-api/src/models.rs`). The mirror's `CardRow` carries the
fallback tags, so every chain resolves from data already in memory, with
no extra fetch.

| Item type | Poster slot (2:3) | Rail slot (16:9) | Backdrop |
|---|---|---|---|
| **Movie / Series** | Own `Primary` | — | Own `BackdropImageTags[0]` |
| **Season** | Own `Primary`, else series poster (`SeriesPrimaryImageTag` on `SeriesId`) | — | Own, else `ParentBackdropImageTags[0]` |
| **Episode** | **Always** the series poster, never a cropped still | Own `Primary` (an episode's Primary is landscape), else `ParentBackdropImageTags[0]` on `ParentBackdropItemId` | Own, else `ParentBackdropImageTags[0]` |

Chains, in code:

- **Poster** (`cards::poster_art_source`): own `Primary` (non-episodes) →
  series poster → flat placeholder. An episode never crops its 16:9 still
  into a 2:3 slot.
- **16:9 rail** (`cards::rail_art_source`): own `Primary` → nearest
  ancestor backdrop (season, else series; already 16:9, so nothing is
  stretched) → a placeholder tile showing the episode's name, so a
  missing-art cell still says which episode it is.
- **Backdrop** (`detail::backdrop_source`): own `BackdropImageTags[0]` →
  `ParentBackdropImageTags[0]` on `ParentBackdropItemId`, which the server
  already resolves through the season → series chain → placeholder.

The background poster warmer (`image_warm.rs`) resolves through the same
`poster_art_source`, so what it warms has the same cache key as what a
rendered cell requests.

### 2.6 Breadcrumb / back semantics

- **Navigation history** is `nav.rs`'s `Nav::go`/`back`/`forward`:
  browser-style back/forward stacks, `⌘[`/`⌘]` (`docs/UX-SPEC.md §2`).
  Browse depth stays shallow. The visible breadcrumbs are the Episode
  page's `S2 E4 · Series Name` crumb (§2.4) and the player's OSD title
  (§1.8). The player is a state over Browse, not a browse depth
  (`docs/UX-SPEC.md §3`).
- **The player composes over Browse; it never replaces it.** Whatever page
  started playback is still current underneath, so leaving the player
  lands back on it.
- **Esc steps down one player layer at a time**: OS fullscreen →
  fullscreen-in-window → miniplayer. Only Esc in the miniplayer stops
  playback. The stop button and the miniplayer close button stop at any
  time. Pickers and the next-episode card take the first Esc for
  themselves.
- **Navigating while the full player is up** (sidebar, breadcrumb)
  collapses it to the miniplayer first (`collapse_fullscreen_player_for_nav`),
  so navigation is never blocked and playback never silently stops.

### 2.7 What "fast" means here

The budget is **view-to-view navigation (warm) < 100 ms, zero network on
the critical path** (`docs/OVERVIEW.md §5b`). Part 2 meets it:

- **Detail paints entirely from the mirror.** `DetailState::load` reads
  the item (`Mirror::item`), a series' seasons, and the selected season's
  episodes (`Mirror::children` under `Sort::IndexNumber`, which orders by
  `parent_index_number, index_number` and is backed by
  `idx_items_parent_order`). The only live fetch on the page is
  `MediaStreams` for codec badges, which the bulk sync does not carry. It
  fills in after first paint. The Similar row is a live fetch below the
  fold.
- **Everything computed for navigation reads data already resident**: the
  series Up Next CTA, `[`/`]` prev/next, the next-episode card's
  candidate, and the artwork fallback chains. `adjacent_episode` is cheap
  enough to call from the 4 Hz tick.
- **Episode context never waits on the network.** The breadcrumb and
  next-episode context come from the Detail state or the item's mirror
  record, both local.
- **Images** go through the image cache with blurhash placeholders, so a
  cold image never blocks a paint. The Episode page prefetches its
  neighbours' hero images (`prefetch_adjacent_episode_hero`).

---

## Appendix — files this spec covers

| File | What it holds |
|---|---|
| `crates/app/src/player_ui.rs` | OSD state and rendering: scrims (§1.3), scrubber (§1.4, §1.16), time labels (§1.5), volume (§1.6), pickers (§1.7), title (§1.8), centre flash (§1.9), auto-hide (§1.11), miniplayer (§1.12), controls row (§1.13), next-episode card and its timing functions (§2.4) |
| `crates/app/src/root_playback.rs` | Playback actions: scrub commit, chapter jumps, volume drag, keyboard map, Esc layering, breadcrumb click, `[`/`]`, next-episode tick and hand-over (§2.4) |
| `crates/app/src/root.rs` | `EpisodeContext`, `adjacent_episode`, EOF backstop, navigation collapse (§2.4, §2.6) |
| `crates/app/src/player_prefs.rs` | `RemainingDisplay` persistence (§1.5) |
| `crates/app/src/settings.rs` | Autoplay and skip-length preferences (§2.4) |
| `crates/app/src/assets.rs`, `crates/app/assets/` | Embedded icons, scrim, fonts (§1.14, §1.15) |
| `crates/app/src/detail.rs` | Series and Episode Detail pages, Up Next CTA, backdrop chain (§2.3–§2.5, §2.7) |
| `crates/app/src/cards.rs` | Episode cells, resume cards, poster and rail art chains (§2.2, §2.5) |
| `crates/app/src/home.rs` | Home shelves (§2.1, §2.2) |
| `crates/app/src/nav.rs` | Back/forward history (§2.6) |
| `crates/media-cache/src/lib.rs` | `Mirror::children`, `Sort::IndexNumber`, `CardRow` (§2.7) |
