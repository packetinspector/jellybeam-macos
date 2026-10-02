# `app` crate architecture

The `app` crate is the Jellybeam binary: a GPUI window, an embedded libmpv
video surface, and every screen of the client. It owns UI, input, window
and video plumbing, and local preference files. It does not speak HTTP or
SQL directly; it builds on `jellyfin-api` (`JellyfinClient`),
`jellyfin-core` (event bus, `ReportingSession`, device profile, playback
decision), `media-cache` (`Mirror`, `ImageCache`), `player` (libmpv) and
`seerr-api` (Discover). Product behavior is in `docs/UX-SPEC.md`, the data
model in `docs/DATA.md`, system design in `docs/OVERVIEW.md`, player and
navigation design in `docs/DESIGN-PLAYER-NAV.md`, layout in
`docs/DESIGN-GUIDE.md`, and identity in `docs/DESIGN-GUIDE.md Part A`.

Browse surfaces read the mirror synchronously and never wait on the
network. Apart from images, live requests are limited to sign-in,
playback, Detail enrichment (`MediaStreams`, chapters, trickplay, people),
plugin channels (`docs/PLUGIN-CHANNELS.md`), media segments and Discover.

## View map

```
Root (the single GPUI entity, root.rs)
├── screen: Screen
│   ├── Connect(ConnectState)  login form and Quick Connect code flow
│   ├── Main(MainState)        a signed-in session
│   │   ├── nav: Nav           current View + back/forward stacks
│   │   ├── home               HomeState: hero + shelves
│   │   ├── library            Option<LibraryState>: grid/list, sort, filters
│   │   ├── channel_browse     Option<ChannelBrowseState>: live channel rows
│   │   ├── detail             Option<DetailState>
│   │   ├── discover           Option<DiscoverState>: Seerr sub-screens
│   │   ├── search             SearchState: modal overlay
│   │   ├── settings           SettingsState: sheet open + section
│   │   ├── player_ui          Option<PlayerUiState>: Some while a session is live
│   │   └── mode: ContentMode  Browse | Loading | Playing
│   └── Switching              transient, during a server/user switch
├── sessions                   StoredSessionList: every signed-in (server, user)
├── app_settings               AppSettings: persisted preferences
└── video                      Rc<VideoLayer>: mpv NSView + player, app lifetime
```

`nav::View` is `Home | Library | Channel | Detail | Discover`. Screens are
plain structs owned by `MainState`, not nested GPUI entities, so
cross-screen flows are ordinary `&mut self` calls on `Root`.
`render_main` lays out the sidebar and content pane; `render_content`
renders the current `View` (Browse), the load-stage overlay (Loading) or
the player OSD (Playing). In Miniplayer, Browse renders under the
Miniplayer chrome. Search, Settings, the server switcher and the `?`
overlay draw on top. `SidebarMode` is `Shown` (240px), `Collapsed` (56px
rail below 900px) or `Hidden` (OS fullscreen or fullscreen playback).

## Module map

| File | Responsibility |
|---|---|
| `main.rs` | Process entry: panic hook, tracing, tokio runtime, window, video layer, event bridges, menus, quit hook, test-mode dispatch. |
| `root.rs` | `Root`, `Screen`, `MainState`, `ContentMode`; global keystroke dispatch; navigation; sidebar; top-level render; `bridge`/`bridge_or` helpers. |
| `root_focus.rs` | Arrow-key focus movement, hover retargeting and Return/click activation across browse screens. |
| `root_playback.rs` | Play, preload, stop, OSD interaction, track picker, segment skipping, Miniplayer drag, next-episode flow, speed hold, quit preparation. |
| `session.rs` | Connect, resume and Quick Connect flows; per-server scope dirs; mirror and event-bus listeners; server switching. |
| `nav.rs` | `View`, `DiscoverView`, `Nav` two-stack history. Pure data. |
| `focus_grid.rs` | `GridFocus`, `ShelfFocus`, `Direction`: focus arithmetic. Pure data. |
| `scroll_axis.rs` | Per-gesture scroll axis lock for horizontal shelves inside vertical pages. Pure data. |
| `home.rs` | `HomeState`: hero, Continue Watching, Next Up, Latest-per-library shelves. |
| `grid.rs` | Virtualized poster wall over `uniform_list`; column math; cancel-on-scroll hook. |
| `library_list.rs` | List projection of the same `LibraryState::items` the grid shows. |
| `channel_browse.rs` | Live plugin/recording channel listings, paged over `uniform_list`. |
| `detail.rs` | `DetailState`: movie, series, season/episode and episode pages; season tabs; episode grid. |
| `search.rs` | `SearchState`: modal overlay backed by `Mirror::search`. |
| `discover/mod.rs` | `DiscoverState`, Seerr session lifecycle, sub-screen dispatch. |
| `discover/home.rs` | Discover action row and shelves. |
| `discover/browse.rs` | Movies/TV browse grids with sort and genre chips. |
| `discover/search.rs` | Seerr search screen. |
| `discover/detail.rs` | Seerr title page with request actions. |
| `discover/person.rs` | Person page and credits grid. |
| `discover/requests.rs` | The account's own requests with status badges. |
| `cards.rs` | `poster_card`, `episode_card`, badges, title blocks, the card focus recipe, poster art fallback chain. |
| `backdrop.rs` | Shared backdrop plus scrim layers for the Home hero and Detail pages. |
| `image_store.rs` | Decode, decoded-texture LRU, blurhash placeholders, scrim variants, remote-URL images. |
| `image_warm.rs` | Background poster warming into the disk cache. |
| `trickplay.rs` | Trickplay sprite fetch, decode and crop for scrubber previews. |
| `playback.rs` | `start_playback`/`start_preload` tokio flows; `PlaybackStarted`; load stages; diagnostic log tasks. |
| `player_ui.rs` | `PlayerUiState` plus every OSD, Miniplayer, picker, info overlay, toast, skip pill and next-episode card renderer. |
| `gl_video.rs` | `VideoLayer`: mpv NSView embedding, GL render thread, geometry driver, Miniplayer mask, HDR output wiring. |
| `edr.rs` | HDR output decisions: float surface, EDR target, headroom quantization. Pure functions. |
| `edr_gl.rs` | `EdrPass`: the RGBA16F intermediate and extended-sRGB encode shader for HDR frames. |
| `now_playing.rs` | `MPNowPlayingInfoCenter` publishing and `MPRemoteCommandCenter` commands. |
| `option_speed_hold.rs` | Native `flagsChanged` monitor: right Option 2x, left Option 0.5x while held. |
| `power.rs` | `DisplaySleepGuard`: IOKit display-sleep assertion during playback. |
| `pending_report.rs` | Persists a quit-time Stopped report and replays it on next launch. |
| `player_prefs.rs` | `TrackPrefs`: per-series audio/subtitle choice and the remaining/total time toggle. |
| `settings.rs` | `AppSettings` (persisted) and the Settings sheet UI. |
| `keychain.rs` | Session list and device id persistence (file store by default). |
| `paths.rs` | State and cache roots, test isolation, owner-only atomic writes. |
| `text_input.rs` | Minimal single-line text field entity for the Connect form. |
| `shortcuts_overlay.rs` | The `?` keyboard reference card. |
| `menu.rs` | Native app and Window menus. |
| `about.rs` | The About window. |
| `assets.rs` | `AssetSource` over embedded `assets/` (icons, fonts, brand images). |
| `theme.rs` | Design tokens: colors, type, spacing, radii, focus constants, easing. |
| `ui/mod.rs` | Shared component library root. |
| `ui/components.rs` | Buttons, list rows, sheet shell, toast shell, section headers, empty states, `focus_ring`. |
| `ui/popover.rs` | Anchored popover primitive. |
| `ui/spec_strip.rs` | The technical-metadata strip used on Detail, grid hover and the OSD. |
| `ui/motion.rs` | Easing curves and clock helpers used by the About window. |
| `panic_log.rs` | Panic hook and `catch_and_log` for handlers called from AppKit callbacks. |
| `redact.rs` | Strips secrets from URLs before logging. |
| `perf.rs` | `JELLYBEAM_PERF` and `JELLYBEAM_LOADTEST` instrumentation modes. |
| `e2e.rs` | `JELLYBEAM_E2E` and `JELLYBEAM_E2E_QUIT_TEST` harnesses. |
| `test_support.rs` | `#[cfg(test)]` helpers: `with_temp_home` under a crate-wide lock. |

## Threading model

- **GPUI main thread**: all `Root` state, rendering, input and AppKit
  geometry. It never awaits the network.
- **Tokio runtime**: one multi-thread runtime, held by `Root` for the
  app's lifetime, runs all Jellyfin, mirror, Seerr and image I/O.
- **Re-entry**: results return through a oneshot channel awaited in
  `cx.spawn`, which updates `Root` through a weak handle. `Root::bridge`
  and `bridge_or` package this; playback, connect and preload use the same
  shape. Staleness checks (generations, id comparisons) run in the apply
  step.
- **mpv events**: `spawn_player_events_task` subscribes to
  `Player::events()` once and calls `Root::on_position`, `on_loaded`,
  `on_pause_changed`, `on_end_of_file`, `on_buffering` and
  `on_tracks_changed`, each inside `panic_log::catch_and_log`. The latest
  position also lands in the shared `last_position_ticks` atomic. Now
  Playing commands and Option-key samples arrive the same way.
- **Mirror and bus**: `MirrorChange`s are received on tokio (the debounce
  needs a tokio timer) and forwarded to a GPUI task. Event-bus transitions
  drive the offline state.
- **Video**: a dedicated GL render thread owns mpv's render context; a
  16ms GPUI foreground task owns NSView geometry. They share one
  `NSOpenGLContext` under `CGLLockContext`.

## State ownership

- `Root`: process-lifetime state (runtime, identity, video layer, session
  list, `AppSettings`, Now Playing and speed-hold monitors, About window,
  counters that must survive a server switch).
- `MainState`: one signed-in session (client, mirror, bus handle,
  `ImageStore`, poster warmer, screen states, `ReportingSession`, task
  handles, `TrackPrefs`). A server switch drops it via `Screen::Switching`.
- Each `(base_url, user_id)` pair gets its own mirror and image cache
  directory, named by a stable hash (`session::server_scope_dir`).
- `paths::state_root()` holds `settings.json`, `track-prefs.json`, the
  session store and per-server mirrors; `paths::cache_root()` holds image
  caches. Sessions use a file store unless `JELLYBEAM_KEYCHAIN=1`. Logs go
  to `~/Library/Logs/Jellybeam/`.

## Playback lifecycle

1. **Start.** `play_item` rejects offline, virtual and folder items and
   resolves a Series to its next-up episode. It bumps
   `playback_generation`, aborts the previous `playback_task`, stops the
   old `ReportingSession`, enters `ContentMode::Loading`, and calls
   `playback::start_playback`.
2. **Load.** On tokio, `run` fetches `PlaybackInfo`, calls
   `decide_playback` with the device profile and bitrate mode, and
   publishes `LoadStage` on a watch channel. `load_if_current` re-checks
   the generation just before `Player::load`.
3. **Commit.** `handle_playback_outcome` discards a stale-generation
   outcome: it stops mpv, clears to black, and abandons its reporting
   session so a zero-position stop cannot overwrite resume state.
   Otherwise it builds `PlayerUiState`, enters `Playing`, takes the
   display-sleep assertion, fetches media segments, and replays an mpv
   `Loaded` that arrived early (`pending_loaded`, generation-filtered).
4. **Progress.** `Position` events feed `ReportingSession::on_position`,
   which reports every 10 seconds. `on_position` also throttles an
   optimistic mirror update, updates Now Playing, and runs the auto-skip
   and next-episode checks.
5. **Stop.** `stop_playback` bumps the generation, aborts an in-flight
   load, stops mpv, clears to black, sends the final Stopped report from
   `last_position_ticks`, writes local user data, and returns to Browse.
6. **Quit.** `prepare_for_quit` aborts tasks, persists the Stopped report
   (`pending_report.rs`) and attempts a send within GPUI's shutdown window.
   The next launch replays the persisted report.

**Why a generation counter.** Play and stop run synchronously on the main
thread; a load in flight may finish afterwards. `playback_generation`
(`Arc<AtomicU64>`, shared with the tokio task) is bumped by every play,
preload and stop. The pre-load check and the outcome handler compare it
to the value captured at start, closing stop-during-load and rapid-switch
races whichever side resolves first.

**Preload.** When idle and enabled, `preload_item` opens a likely-next
item (Detail open, hover dwell, Home hero) as a paused, readahead-capped
stream; `promote_preload` starts a matching Play with no round trip.

**Media segments.** Each segment type is `Ask` (skip pill), `AutoSkip` or
`Off` (`SkipSegmentPrefs`). Auto-skip seeks past a segment on entry and
offers Undo (`U`) for `SKIP_UNDO_WINDOW`; `last_auto_skip_segment`
prevents re-firing.

**Next episode.** `adjacent_episode` finds the neighbor playable episode
in the mirror, crossing seasons in watch order. `tick_next_episode` shows
the next-up card at a point derived from the outro segment and autoplay
delay. With autoplay on, a one-shot timer stamped with
`next_episode_handover_gen` hands over when the countdown ends; pause
freezes it and resume re-arms it under a fresh generation. EOF also
advances or shows the card unless the session is gone, still loading, or
the card was dismissed.

## Video layer and miniplayer

The mpv `NSView` (`JellybeamVideoView`) is a permanent sibling below
GPUI's content view (`docs/OVERVIEW.md §2`), rendering through an OpenGL
3.2 Core `NSOpenGLContext` (required for VideoToolbox hwdec). Its
`hitTest:` returns nil, so all mouse events reach GPUI. `LayerMode` is
`FullscreenInWindow` or `Miniplayer(Corner)`; the geometry driver lerps
the frame between target rects over `LAYER_ANIM` (150ms). OS fullscreen
uses GPUI's `toggle_fullscreen`. Esc steps down one layer at a time; Esc
in Miniplayer stops playback.

**The cutout.** In Miniplayer, Browse paints opaquely over the whole
content pane. GPUI's Metal renderer has no subtractive primitive: an
element cannot clear what lies beneath it, and transparency only reveals
what GPUI itself painted. So `spawn_geometry_driver` sets a `CAShapeLayer`
mask on GPUI's content view: an even-odd path of the full bounds plus the
Miniplayer rect (rounded by `theme::RADIUS_MINIPLAYER`). Compositing drops
GPUI's pixels inside the rect and the video view shows through. The mask
lifts while the pointer hovers the Miniplayer (`set_miniplayer_hovering`)
so the hover controls painted there stay visible. Drops snap to the
nearest corner (`drop_miniplayer`).

**HDR output.** At startup, if any attached screen reports
`maximumPotentialExtendedDynamicRangeColorComponentValue` above 1.0, the
GL drawable is RGBA16F (`NSOpenGLPFAColorFloat`, 64-bit colour); otherwise,
or if that format is unavailable, it is RGBA8 as on any SDR Mac. The
format is fixed for the context's life, and a half-float drawable doubles
present bandwidth, so SDR-only Macs never pay for it. Every 250ms
(`edr::EDR_POLL`) the geometry driver reads the playing transfer
(`video-params/gamma`) and the window screen's headroom, and
`edr::output_target` picks mpv's colour target.

Before mpv has decoded a frame, the transfer comes from the server:
`playback.rs` sets `edr::RangeHint` (the Direct Play source's
`VideoRangeType`; a transcode gets none) immediately before both
`Player::load` calls, the real start and the dark preload, and the driver
re-decides on its next 16ms tick rather than its next poll. The first
frame therefore renders in the right range, so neither SDR nor HDR titles
switch target mid-play; mpv's decoded transfer overrides the hint once it
exists (`edr::effective_transfer`). The target is:

- PQ or HLG content on a float drawable whose screen has EDR potential:
  `target-trc=linear`, `target-prim=display-p3` (wide-gamut screen) or
  `bt.709`, and `target-peak` = 203 nits × current headroom
  (`maximumExtendedDynamicRangeColorComponentValue`, clamped to the
  potential and floored to 1/8 stop). 203 nits is mpv's reference white,
  so linear 1.0 lands on the display's SDR white and highlights use the
  headroom above it.
- Anything else: `auto` on all three, mpv's default SDR path, rendered
  straight to the drawable as on an SDR Mac.

mpv renders through vo_gpu's renderer, whose non-linear output transfers
clamp at 1.0, so linear is the only way to carry values above SDR white.
The compositor reads an untagged float GL surface as display-referred,
sRGB-encoded values, so HDR frames go to an RGBA16F intermediate
(`mpv_opengl_fbo.internal_format = GL_RGBA16F`, same flip as the
drawable) and `edr_gl::EdrPass` writes them to the drawable through the
sRGB curve continued past 1.0. Subtitles ride the same pass.

AppKit only honours `wantsExtendedDynamicRangeOpenGLSurface` when a view
first receives its GL surface, and the request raises display power, so
the video view starts without it. The first HDR item swaps in a fresh
`JellybeamVideoView` carrying the request, at the same frame and
z-order, and moves the context onto it; the request then stays for the
process. Headroom ramps up over about two seconds after that, and the poll
follows it, the brightness slider, and moves between screens. Both APIs
exist on macOS 11 (10.11 and 10.15); the view selector is probed with
`respondsToSelector`, and its absence keeps the RGBA8 path.

**OSD hit geometry is computed, not measured.** GPUI 0.2.2 has no
mid-render query for committed element bounds. `player_ui.rs::bar_frac`
maps a window x onto the scrubber with the same fixed `SCRUB_MARGIN` inset
`scrub_row` paints with. The volume slider uses relative drag deltas
because its x is not fixed. The Miniplayer hover rect comes from
`gl_video::miniplayer_rect_px`, the same geometry as the mask. OSD
rendering follows `docs/DESIGN-PLAYER-NAV.md` Part 1; trickplay sprites
bypass `ImageCache` (`TrickplayCache`).

## Keyboard and focus model

`Root::handle_global_keystroke` checks, in order: Esc on a cancellable
Connect form; the `?` overlay; `⌘M`/Tab (Miniplayer toggle) and Esc in
Miniplayer; `⌘F`, `⌘[`, `⌘]`, `⌘1`–`⌘9`, which work in every state;
fullscreen playback (`handle_playback_keystroke`: Space, Esc, arrows, `S`,
`A`, `I`, `M`, `F`, `U`, `[`, `]`, Return, with an open track picker
first); the modal Search overlay, which edits its query directly; then
Browse (arrows to `move_focus`, Return to `activate_focus`, Esc back, `/`
search). Key tables are in `docs/UX-SPEC.md §2`.

Browse focus is app state, not GPUI `FocusHandle`s: `ShelfFocus` on Home,
`GridFocus` on grids, `DetailArea::{Play, Seasons, Episodes}` on Detail.
Mouse hover retargets the same state (`root_focus.rs`). Shelves route
wheel deltas through `scroll_axis::AxisLock`, because GPUI delivers each
wheel event to both the shelf and the page.

**Card focus-ring recipe** (`cards.rs::focus_art_box`). Cards never use
border-only focus. On focus or hover these animate together (enter 180ms,
exit 240ms, emphasized easing):

- Art grows to `FOCUS_SCALE` (1.02): the outer slot keeps its size, and an
  absolutely positioned inner box grows and re-centers, so neighbors never
  move.
- Brightness rises to `FOCUS_BRIGHTNESS` (1.08) via a white overlay;
  `img()` has no filter. Art carries no shadow.
- Row siblings dim to `FOCUS_SIBLING_DIM` (0.5) (`apply_row_dim`).
- `ui::components::focus_ring` draws a 2px accent stroke 2px outside the
  art, inset widened by the scale overshoot. GPUI has no outline
  primitive, so the ring is an absolutely positioned sibling.

`with_animation` cannot reverse, so each transition mints an element id
keyed on the boolean. Non-card controls use `focus_ring` alone: text
fields, the Detail Play button, and the season-tab pill. Season tabs show
selection with a per-tab underline (`season_tab_underline`) instead of a
fill, and keep the ring for keyboard focus.

## Image pipeline

`ImageStore` (one per `MainState`, an `Rc` clone) sits on
`media_cache::ImageCache`, which owns disk and network fetch, coalescing
and cancellation (`docs/DATA.md §3`).

- `ImageStore::get` is synchronous: a cached decoded `Arc<RenderImage>`,
  or `None` after starting one de-duplicated fetch and decode on tokio. The
  cell paints its blurhash that frame; completion notifies `Root`.
- Decoding mirrors GPUI's `img()` loader (RGBA to BGRA). Blurhashes decode
  at 32x48 on the main thread and are cached unbounded (they are tiny).
- Width buckets: posters 320, thumbs 400, backdrops 1280, portraits 150.
- Decoded textures live in an LRU keyed by `(item, kind, tag, width)`,
  bounded by `DECODED_CACHE_BUDGET_BYTES` (192 MB) and
  `DECODED_CACHE_MAX_COUNT` (4000). Entries track BGRA size and a
  last-used tick; eviction after each insert is a linear scan, cheap next
  to a decode. Evicted images re-decode from the disk cache.
- The same LRU holds backdrop scrim variants (CPU downscale, blur, darken;
  `backdrop.rs`) and Discover's URL-keyed images (`get_remote`). Entries
  also carry an ambient color.
- **Cancel-on-scroll**: when `uniform_list` reports a new visible range,
  the grid bumps the store generation and calls `cancel_below_priority`
  with a trailing horizon. Home shelves request only cells near the
  viewport or the focused column.
- Posters fade in once (`take_fresh_arrival`). `image_warm.rs` fills the
  disk cache with posters in the background, one fetch at a time, paused
  during playback.

## Theme and shared components

`theme.rs` holds every design token: the brand palette
(`docs/DESIGN-GUIDE.md §A.2`), three faces registered at startup (§3),
spacing, radii, focus constants and easing. `ui/` holds stateless builders
(buttons, rows, sheet and toast shells, the anchored popover, the spec
strip of brand §5). `cards.rs` is the one cell renderer for every item
surface.

## E2E harness

`JELLYBEAM_E2E=1` runs the real binary against a Jellyfin dev server
(`JELLYBEAM_E2E_SERVER`/`_USERNAME`/`_PASSWORD` override the defaults),
signs in through Quick Connect, and drives `Root` from a GPUI task with
synthetic `KeystrokeEvent`s and direct calls. It asserts on `Root` state,
the video NSView's real frame (`VideoLayer::current_frame`) and
`Window::is_fullscreen`, covering browse, playback, Miniplayer, tracks,
stop/switch races, resume, episodes, skip settings, multi-server sign-in
and a resize sweep. No live element-bounds query exists here
either, so layout checks recompute widths with `render_main`'s formulas.
`JELLYBEAM_E2E_QUIT_TEST=1` quits mid-playback to exercise
`prepare_for_quit`.

**State isolation**: in either E2E mode `paths::state_root()` is a fixed
temp directory, so tests never touch real settings or sessions; fixed, not
random, so quit/relaunch runs keep state. `JELLYBEAM_STATE_DIR` sets an
explicit root. `JELLYBEAM_PERF`/`JELLYBEAM_LOADTEST` are measurement modes.

## Known simplifications

- **Detail episode grid width**: columns come from the viewport width
  minus the sidebar and a fixed 48px padding (`render_main`), not a
  measured element. GPUI 0.2.2 cannot read a subtree's committed layout
  mid-render. Correct across resizes, but it would drift if the pane's
  padding changed independently. The same gap leaves `detail.rs` with a
  constant `EPISODE_ROW_HEIGHT` for its min-height spacer, synopsis
  heights estimated from character count, and a per-tab underline fade
  instead of a sliding indicator.
- **Library scroll memory**: one `LibraryState` slot. Returning to the
  same library keeps its scroll; switching libraries rebuilds.
- **Coarse mirror refresh**: any `MirrorChange` re-runs the queries behind
  visible views instead of applying deltas. The sidebar's view list is
  re-read on every change, so a session starting from an empty mirror
  fills in as sync lands. Library rebuilds wait for sync to settle and
  run off-thread.
- **Frame timing**: `JELLYBEAM_PERF` times `Root::render` intervals, not
  compositor vsync; treat it as a lower bound.
- **Visual approximations**: frame-lerp Miniplayer moves, overlay
  brightness, CPU-precomputed backdrop blur, stacked two-stop gradients.
- **Text entry**: `TextInput` is append and backspace only (no IME).
- **Channel and Discover content** is never mirrored; it loads live and
  needs a reachable server.
