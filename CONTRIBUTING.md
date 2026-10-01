# Contributing to Jellybeam

Jellybeam is a native Jellyfin client for Apple Silicon Macs: pure Rust, GPUI
for the interface, libmpv for playback. This document covers getting a
working build, the gates a change has to pass, and the conventions the
codebase holds to.

## Requirements

- **Apple Silicon Mac** on macOS 13 or later. The `app` and `player` crates
  are macOS-only (they embed an `NSOpenGLContext` in a GPUI window and link
  the vendored libmpv). The other crates build anywhere.
- **Xcode Command Line Tools**: `xcode-select --install`.
- **Rust** via [rustup](https://rustup.rs); the toolchain is pinned in
  `rust-toolchain.toml` and rustup honours it automatically.
- **Homebrew** packages for the vendored media stack:
  `brew install meson ninja nasm cmake automake autoconf pkg-config`.
- **Docker** for the dev Jellyfin server the live tests use.

## Build

```sh
scripts/build-vendor.sh   # once: libmpv, FFmpeg, libplacebo, libass into vendor/prefix
cargo build -p app        # debug binary at target/debug/jellybeam
scripts/bundle-app.sh     # release build + self-contained target/bundle/Jellybeam.app
```

The vendored build is the one slow step and is cached afterwards; see
[docs/BUILD.md](docs/BUILD.md) for what it builds, the configure flags, dylib
relocation, and signing.

## The dev server

Live tests run against a real, pinned Jellyfin in Docker with a generated
synthetic corpus (no copyrighted media is in this repository):

```sh
dev/server.sh corpus   # generate dev/media/
dev/server.sh up       # Jellyfin at http://localhost:8096, seeded users and libraries
dev/server.sh 12-up    # opt-in second server on Jellyfin 12.x at http://localhost:8097
dev/server.sh doctor   # check the environment
```

`dev/README.md` has the details, including the seeded accounts.

## Before a commit

All of these must pass:

```sh
python3 scripts/check-public-tree.py
cargo fmt --check
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo test --locked --workspace
```

`cargo deny check licenses` must pass (`deny.toml` holds the allowlist);
`cargo deny check advisories` currently fails on unmaintained transitive
GPUI dependencies; review its full output for vulnerabilities and yanked
versions before every release. See [docs/RELEASE.md](docs/RELEASE.md).
Anything that touches the bundle, playback, or the UI is also verified by
running the real app; `scripts/bundle-app.sh` ends with `JELLYBEAM_E2E: PASS` when
the dev server is up.

## What the tests cover

| Layer | Where | Runs with | Covers |
|---|---|---|---|
| Unit and fixture tests | `crates/*/src`, `crates/*/tests` | `cargo test --workspace` | Parsing, sync, queries, playback decisions, reporting, pure UI decisions, against fixtures and a mock server |
| Headless player tests | `crates/player/tests` | `cargo test -p player` | Real media from the corpus through libmpv: rendered-frame assertions, hwdec, tracks, chapters, subtitle styling, load options |
| Live server tests | `crates/*/tests/live_*.rs` | `dev/server.sh up && dev/run-server-suite.sh` | Sign-in, Quick Connect, sync, and Direct Play decisions per corpus file against a real Jellyfin |
| GUI end-to-end | `crates/app/src/e2e.rs` | `JELLYBEAM_E2E=1 cargo run -p app` (also run by `scripts/bundle-app.sh`) | Sign-in → browse → play → hwdec → stop, driving the real window; isolates its own state directory |
| Fuzzing | `fuzz/` | `cargo +nightly fuzz run <target>` | The parsers that consume server data |
| Rendered layout, HDR output, real 4K files, multiple displays | none | by hand | What none of the above can see |

Anything that changes how a screen renders or how a file plays is verified
in the real app before it is committed, and the commit message says what
was checked.

## Commits

One commit per coherent change. Summary line in the voice the history uses:
`Area: what changed and why`, one paragraph; the area names a screen,
crate, or subsystem (`Player`, `media-cache`, `Sign-in`, `Bundle`),
followed by what changed, why, and what was verified. No AI-attribution
trailers of any kind.

## Never commit

- Real server addresses, hostnames, IPs, or device names.
- Access tokens, API keys, or credentials.
- Media titles, paths, or other identifying library content.
- Local filesystem paths, shell prompts, or unredacted diagnostic output.

Tests and examples use synthetic values only: `example.test`-style
hostnames, RFC-reserved addresses, and generated placeholder ids.

## Product rules that hold across the whole app

- **Server-configured names are shown verbatim.** Whatever a server calls a
  library, item, or account is what renders. Heuristics may choose an icon;
  they never rewrite a name.
- **Direct Play is the default.** Transcoding and bitrate caps are strictly
  opt-in, never a silent fallback.
- **Fail open.** Missing or unknown metadata shows content rather than
  hiding it.
- **No panics on recoverable paths.** `clippy::unwrap_used` is denied
  workspace-wide. Server responses, media files, and config files are
  untrusted input: parse defensively and fall back rather than crash.
- **Performance is a feature.** `docs/OVERVIEW.md` §5b records the budgets
  the UI is held to. If a change risks one of them, measure it with
  `JELLYBEAM_PERF=1`.
- **`crates/app/src/theme.rs` is the sole colour and font source.** No raw
  colour literals outside it (OSD-over-video alpha excepted).

## Code comments

Comments are short contracts, not narration: a spec citation where one
applies (`docs/DESIGN-PLAYER-NAV.md §2.5`), the rule, and the one reason it
exists; one sentence by default, three at most. They say what the code
cannot say by itself and never recount how it used to work.

## Fixing a behaviour bug

Extract the decision into a pure function and pin it with a test that
fails before the fix, rather than patching the symptom in place.

## Reporting bugs

Use the issue template. Logs are at `~/Library/Logs/Jellybeam/jellybeam.log`
with crash reports beside it in `panic.log`. Logs can include server
addresses, titles, local paths and error details. Review and redact them
before posting, including credentials echoed by a server or dependency.

## Security

See [SECURITY.md](SECURITY.md); please don't open a public issue for a
vulnerability.
