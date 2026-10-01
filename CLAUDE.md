# Agent instructions

Jellybeam is a native macOS Jellyfin client: pure Rust, GPUI for the
interface, libmpv for playback, a SQLite mirror of the library for
navigation. Apple Silicon only.

## Where things are

| Path | What |
|---|---|
| `crates/app/` | The binary: GPUI UI, video layer, screens. `crates/app/assets/` holds the embedded icons, fonts and brand art; `crates/app/ARCHITECTURE.md` explains the crate. |
| `crates/jellyfin-api/` | Jellyfin HTTP, WebSocket and DNS client; generated models under `codegen/`. |
| `crates/jellyfin-core/` | Session, device profile, playback decision, reporting, event bus. |
| `crates/media-cache/` | SQLite mirror, sync, queries, image cache. |
| `crates/player/` | libmpv wrapper; `crates/player/LATENCY.md` covers stream-open latency. |
| `crates/seerr-api/` | Optional Jellyseerr/Overseerr client. |
| `docs/` | The specs: `START-HERE.md` (map), `OVERVIEW.md`, `RELEASE.md`, `BUILD.md`, `UX-SPEC.md`, `DATA.md`, `DESIGN-PLAYER-NAV.md`, `DESIGN-GUIDE.md`, `PLUGIN-CHANNELS.md`; `docs/brand/` holds README artwork. |
| `scripts/` | `build-vendor.sh` (libmpv/FFmpeg/libplacebo/libass), `bundle-app.sh` (Jellybeam.app), `make-icon.sh` + `render-icon.swift`, `Info.plist.in`. |
| `dev/` | The dev Jellyfin server: `server.sh`, `docker-compose.yml`, `setup-server.sh`, `run-server-suite.sh`, `corpus/` (synthetic media generator), `media/` (generated, ignored). |
| `fuzz/` | cargo-fuzz targets for the parsers that consume server data. |
| `vendor/`, `target/` | Build output, ignored. |

## Read order

1. `README.md`
2. `docs/START-HERE.md`
3. `docs/OVERVIEW.md`
4. `crates/app/ARCHITECTURE.md`
5. `CONTRIBUTING.md`
6. Whichever spec covers the area you're touching (the map in
   `docs/START-HERE.md`)

## Build and test

```sh
scripts/build-vendor.sh                       # once: the vendored media stack
cargo build -p app                            # debug binary
cargo test --workspace                        # unit + fixture tests (no server)
dev/server.sh up && dev/run-server-suite.sh   # live tests against the dev server
scripts/bundle-app.sh                         # Jellybeam.app; ends with JELLYBEAM_E2E: PASS
```

Pre-commit gate: `cargo fmt --check`, `cargo clippy --workspace
--all-targets -- -D warnings`, `cargo test --workspace`.

## Hard rules

- **Subagents never run git.** The primary session reviews diffs and
  commits.
- **Never add AI-attribution trailers** to commits, PR descriptions, or
  release notes.
- **Never push** without an explicit ask in the current conversation.
- **Never commit identifying or environment-specific info**: real names or
  usernames, account/item ids, server/device/host names, IP/MAC addresses,
  local filesystem paths, shell prompts or history, media titles or paths,
  credentials or tokens, or unredacted diagnostic output. Tests use
  synthetic values only.
- **Server-configured names are shown verbatim**; heuristics may pick an
  icon, never rewrite a name.
- **Direct Play is the default**; transcoding is strictly opt-in.
- **Fail open** on missing metadata: show content rather than hide it.
- **`crates/app/src/theme.rs` is the sole colour and font source**; no raw
  colour literals outside it (OSD-over-video alpha excepted).
- **Comments are short contracts**: cite the owning spec section where one
  applies (`docs/DESIGN-GUIDE.md §A.4`, `docs/UX-SPEC.md §3`), state the
  rule and its one reason, one sentence by default.
- **Extract behaviour decisions into pure functions** and pin them with
  tests, rather than patching a symptom in place.

## Local-only state

Keep anything that shouldn't ship in `internal/` (gitignored) or
`CLAUDE.local.md` (gitignored). `CLAUDE.md` is a committed copy of this
file.
