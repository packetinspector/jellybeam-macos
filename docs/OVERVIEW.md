# Jellybeam — Overview

**Jellybeam** is a fast, memory-efficient, open-source Jellyfin client for
Apple Silicon Macs. The thesis: **keep an mpv-class playback engine and put a
GPU-native UI on top of it.** A media-center UI is a small widget vocabulary
(poster grids, rows, focus states, text, settings forms, one video layer);
that is a GPU scene, and the whole app is built as one.

Section numbers are stable: code comments cite this document as
`docs/OVERVIEW.md §x`. Sections that no longer apply are left out rather
than renumbered, so the numbering has gaps.

## 2. Playback engine: libmpv

### The GPU story — why libmpv over raw ffmpeg

Two GPU stages; they are not the same decision:

| Stage | raw ffmpeg | libmpv |
|---|---|---|
| **Decode** | VideoToolbox hwaccel: H.264/HEVC/ProRes/VP9, AV1 on M3+; NEON dav1d software fallback. Frames land in IOSurface-backed CVPixelBuffers (GPU memory). | Identical — mpv's decoder *is* ffmpeg with the same hwaccel. |
| **Presentation** | Decoded frames and nothing else. Zero-copy texture import, chroma upsampling, scaling shaders, debanding, HDR→EDR tone mapping, subtitle compositing — all to be written in Metal. | All included: the `vo=gpu` pipeline (libplacebo). GPU end-to-end from bitstream to pixels, subtitles composited on the GPU. |

So libmpv gives full-GPU decode **and** render; raw ffmpeg gives the decode
half only. AVPlayer is disqualified as the core engine (no MKV, no ASS, no
codec plugins).

### Subtitles (hard requirement: ASS, PGS, SRT)

- **ASS/SSA** via libass — reference-quality rendering (full styling,
  positioning, karaoke).
- **PGS** (+ DVD/DVB bitmap subs) via mpv's bitmap subtitle pipeline.
- **SRT/VTT/etc.**, including sidecar loading (`sub-file`) for external subs
  served by Jellyfin.
- All GPU-composited into the frame — correct under scaling and HDR.

### Direct-play matrix (Apple Silicon + mpv)

Transcoding is the fallback of last resort — the hardware plays essentially
everything:

| Format | Path |
|---|---|
| H.264, **HEVC/x265 8+10-bit (4K)**, ProRes, VP9 | VideoToolbox **hardware decode**, all Apple Silicon |
| AV1 | Hardware on M3+; dav1d NEON software decode on M1/M2 (handles 4K) |
| **HDR10 / HLG** | Yes — on an EDR-capable display mpv tone-maps into the screen's current EDR headroom (XDR displays get real HDR); elsewhere it tone-maps to SDR |
| Dolby Vision | Profile 8 plays; Profile 5 depends on upstream mpv/libplacebo support |
| **AC3 / E-AC3**, DTS, TrueHD, AAC, FLAC, Opus | ffmpeg software decode, multichannel out via CoreAudio |
| MKV, MP4, TS, AVI, WebM… | All — ffmpeg demuxers |

Transcode fallback (HLS) stays in scope but triggers only for
bandwidth-constrained remote streaming or corrupt/exotic files — never for
HEVC/HDR/AC3, which are table stakes. The DeviceProfile reported to the
server is what enforces this, and it is contract-tested against the media
corpus (`crates/jellyfin-core/tests/live_contract.rs`).

### Embedding

libmpv's render API is OpenGL-only for embedders. The app renders through
the render API into an `NSOpenGLContext` attached to an `NSView` it owns.
On a Mac with an EDR-capable display that drawable is 16-bit float, and HDR
items render as extended-range linear light, encoded for the compositor
and shown with `wantsExtendedDynamicRangeOpenGLSurface` (details in
`crates/app/ARCHITECTURE.md`, "HDR output"). Deprecated but working; all
render glue is isolated in `crates/app/src/gl_video.rs` and its `edr*`
helpers so it can move when mpv grows a Vulkan/Metal embedder path.

## 3. UI stack: GPUI, pure Rust

GPUI (Zed's framework: direct Metal, 120 fps-class) renders exactly the
class of scene a media-center UI is. Two pieces of platform integration sit
beside it, both through `objc2`: the mpv video layer as a child `NSView`
under GPUI's window, and macOS media integration (media keys,
`MPNowPlayingInfoCenter`, fullscreen behaviour).

GPUI is pre-1.0 and its API moves. The version is pinned, and everything
that touches GPUI stays in the `app` crate so a renderer change never
touches the core.

## 4. Rust core design (renderer-independent)

```
crates/
  jellyfin-api/      # Models generated from the official OpenAPI spec +
                     # a hand-written client layer (HTTP, WebSocket, DNS)
  jellyfin-core/     # Session, DeviceProfile builder, playback decision +
                     # reporting state machine, event bus
  media-cache/       # SQLite library mirror + sync, image pipeline:
                     # disk+memory cache, request coalescing, prefetch,
                     # blurhash placeholders
  player/            # Typed libmpv wrapper: property observation, track
                     # selection, chapter/segment events, render-API glue
  seerr-api/         # Optional Jellyseerr/Overseerr client (Discover)
  app/               # GPUI shell — the only crate that knows the renderer
```

- Models are generated from
  `https://api.jellyfin.org/openapi/jellyfin-openapi-stable.json`; client
  methods are hand-written. Stack: `reqwest` + `serde` + `tokio` +
  `tokio-tungstenite`.
- **DeviceProfile is the highest-leverage correctness work in the project.**
  It declares direct-play capability; wrong means silent failures or
  needless server transcodes. With mpv the profile declares near-everything
  and falls back to HLS transcode only for bandwidth constraints or broken
  files.
- Known sharp edge: progress reports 400 and silently drop watch history if
  `VolumeLevel` isn't an integer. Reporting payloads are contract-tested.

## 5. Architecture requirement: the video surface is a scene node

**Browse-while-playing (the in-app miniplayer) is an architectural
constraint, not a feature.** The video surface is a first-class,
reparentable, scalable, animatable node in the compositor, so "miniplayer in
the corner while browsing the library" is just moving a node — and
fullscreen and inline detail-page playback fall out of the same design.

System-level picture-in-picture (floating above other apps) is explicitly
not wanted; the in-app miniplayer is the product.

## 5b. Performance budgets (navigation is the product)

Cache-first: the UI **never blocks on the network to navigate**. A local
SQLite mirror of library metadata plus a disk image cache serve every view
instantly; background revalidation (REST + WebSocket library-change events)
keeps them fresh. Prefetch is speculative: hovering or focusing an item
warms its detail data and images.

### Cache design: disposable mirror, real-time freshness

- **New media shows up right away.** Jellyfin's WebSocket pushes
  `LibraryChanged` events the moment a scan completes; the app is subscribed
  whenever it runs, applies the delta to the mirror, and the UI updates
  live. On top of that: revalidate-on-focus (app activation triggers a cheap
  changed-since query) and periodic reconciliation (compare server item
  counts and `DateLastMediaAdded` per library; resync a library on
  mismatch). A WebSocket drop reconnects automatically with a reconciliation
  pass, so missed events cannot strand stale state.
- **Schema drift is a non-event.** The mirror is a **disposable cache, never
  a source of truth**. Each row stores the item's raw server JSON plus a
  handful of extracted, indexed columns. Server adds fields → they ride
  along in the blob. Server changes shape → the cache carries a
  `schema_version` pragma, and **on any mismatch the mirror is dropped and
  rebuilt from the server**. No migrations. Nothing of value lives only in
  the cache: tokens are in the Keychain or the local session store, settings
  in a config file, watch state on the server.

Budgets the UI is held to (`JELLYBEAM_PERF=1` instruments them):

| Metric | Budget |
|---|---|
| Cold launch → interactive library | < 500 ms |
| View-to-view navigation (warm) | < 100 ms, zero network on the critical path |
| Poster-wall scroll | 120 fps on ProMotion, no dropped frames at full flick |
| Search keystroke → results | < 50 ms (local index) |
| Click → first video frame (LAN direct play) | < 250 ms |
| Seek (with trickplay + `hr-seek`) | feels instant; preview tiles < 1 frame |
| Memory: browsing / playback | < 250 MB / < 500 MB |

## 6. Scope

**Core**
- Password and Quick Connect sign-in; multiple servers and users
- Library: Movies, Shows, Continue Watching, Next Up, collections, plugin
  channels, search, filter and sort
- Direct play via mpv: full track and subtitle selection (ASS/PGS/SRT),
  chapters, precise seek
- HLS transcode fallback (edge cases only — see the direct-play matrix)
- Progress reporting and resume positions
- In-app miniplayer (browse while playing) — per §5
- Media keys + Now Playing, fullscreen
- Trickplay scrubbing previews
- Media Segments: skip intro/credits, next-episode flow
- Subtitle style overrides, per-server playback preferences
- Optional Jellyseerr/Overseerr Discover (browse, search, request)
- One fast dark theme; no theming system

**v2 — deliberately deferred**
- Music playback, photos, playlists, theming, remote-control target ("cast
  to this Mac")
- SyncPlay
- Live TV/DVR
- Downloads/offline

**Not planned**
- System-level PiP, metadata editing/admin (the server's web UI exists)
