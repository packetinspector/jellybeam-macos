# Start here

## What Jellybeam is

A native macOS Jellyfin client for Apple Silicon: a GPUI interface in pure
Rust over a libmpv playback engine, with a SQLite mirror of the library so
browsing is instant and Direct Play stays the default. `README.md` has the
pitch and the install steps.

The docs are the project's living spec. They describe what the app does and
why, not how a change came to be. Section numbers are stable once a code
comment cites them; don't renumber a doc's existing sections when editing.

## Docs map

Root:

- `README.md`, `CONTRIBUTING.md`, `SECURITY.md`, `TRADEMARKS.md`,
  `THIRD-PARTY.md`, `AGENTS.md` (`CLAUDE.md` is a copy)

`docs/`:

- `OVERVIEW.md`: thesis, playback engine, crate layout, performance
  budgets, scope
- `RELEASE.md`: source sanitization, release gates and app distribution
- `BUILD.md`: the vendored libmpv/FFmpeg/libplacebo/libass build, bundling,
  signing and notarization
- `UX-SPEC.md`: view map, keyboard model, player states and the miniplayer
  state machine, scrubber and trickplay, browse surfaces, failure UX
- `DATA.md`: the SQLite mirror schema, sync protocol, image pipeline,
  prefetch rules
- `DESIGN-PLAYER-NAV.md`: the player OSD (elements, scrubber, auto-hide,
  icons) and navigation (series and episode pages, artwork rules, back
  semantics)
- `DESIGN-GUIDE.md`: brand (name, colour tokens, wordmark, mascot usage),
  what GPUI can render, the design system (type, colour, spacing, motion,
  components), per-screen specs and the component map
- `PLUGIN-CHANNELS.md`: how plugin-channel libraries (TVHeadend recordings
  and the like) are listed and played

Per crate:

- `crates/app/ARCHITECTURE.md`: modules, threading, state ownership,
  playback lifecycle, the video layer and miniplayer, the E2E harness
- `crates/player/LATENCY.md`: stream-open latency, the mpv load options,
  the benchmark
- `crates/jellyfin-api/codegen/DRIFT.md`: the OpenAPI pin and how spec drift
  is caught
- `dev/README.md`, `dev/corpus/README.md`: the dev server and the synthetic
  media corpus

## Reading order for a new contributor

1. `README.md`
2. this file
3. `docs/OVERVIEW.md`
4. `crates/app/ARCHITECTURE.md`
5. `CONTRIBUTING.md`
6. Whichever spec covers the area you're about to touch

## Where to change things

| Work area | Where | Spec |
|---|---|---|
| Home shelves, continue watching, next up | `crates/app/src/home.rs` | UX-SPEC §5, DESIGN-GUIDE C.4 |
| Library grid, list view, sort, filter | `grid.rs`, `library_list.rs`, `focus_grid.rs` | UX-SPEC §5, DESIGN-GUIDE C.5 |
| Detail pages, episode rail | `detail.rs`, `cards.rs` | DESIGN-PLAYER-NAV Part 2, DESIGN-GUIDE C.9 |
| Cards, posters, focus ring | `cards.rs`, `image_store.rs`, `image_warm.rs` | DATA §3, DESIGN-GUIDE Part B |
| Sidebar, navigation, search | `root.rs`, `nav.rs`, `search.rs`, `root_focus.rs` | UX-SPEC §1–2, DESIGN-PLAYER-NAV §2.6 |
| Player OSD, skips, next-up, miniplayer | `player_ui.rs`, `root_playback.rs`, `playback.rs`, `gl_video.rs` | DESIGN-PLAYER-NAV Part 1, UX-SPEC §3–4 |
| Settings, servers, accounts | `settings.rs`, `session.rs`, `keychain.rs`, `player_prefs.rs` | DESIGN-GUIDE C.7 |
| Seerr Discover | `crates/app/src/discover/`, `crates/seerr-api/` | (module docs) |
| Plugin channels | `channel_browse.rs`, `crates/media-cache/` | PLUGIN-CHANNELS |
| Theme, shared components | `theme.rs`, `ui/` | DESIGN-GUIDE Parts A and B |
| Playback decisions, device profile, reporting | `crates/jellyfin-core/` | OVERVIEW §2, §4 |
| Jellyfin HTTP, WebSocket, DNS | `crates/jellyfin-api/` | OVERVIEW §4, DRIFT |
| Library mirror, sync, queries, image cache | `crates/media-cache/` | DATA |
| libmpv wrapper, load options | `crates/player/` | LATENCY |
| Bundling, signing, vendored build | `scripts/` | BUILD |

Bare file names are under `crates/app/src/`.
