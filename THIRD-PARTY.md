# Third-party dependencies

What ships inside `Jellybeam.app`, and the licence each part is distributed
under. Jellybeam's own code is GPL-3.0-or-later (see `LICENSE`); its name and
artwork are reserved (see `TRADEMARKS.md`).

## The vendored media stack

Built from source by `scripts/build-vendor.sh` at the pinned tags in that
script, and copied into `Contents/Frameworks` by `scripts/bundle-app.sh`.

| Library | Licence |
|---|---|
| mpv / libmpv (built with `-Dgpl=true`) | GPL-2.0-or-later |
| FFmpeg (built with `--enable-gpl --enable-version3`) | GPL-3.0-or-later |
| libplacebo | LGPL-2.1-or-later |
| libass | ISC |
| dav1d | BSD-2-Clause |
| libsoxr | LGPL-2.1-or-later |

FFmpeg with `--enable-gpl --enable-version3` and mpv with `-Dgpl=true` are
GPL, which is what makes the app as a whole GPL-3.0-or-later.

### Leaf libraries the media stack links

Installed from Homebrew for the build and relocated into the bundle.

| Library | Licence |
|---|---|
| FreeType | FTL (BSD-style) or GPL-2.0 |
| FriBidi | LGPL-2.1-or-later |
| HarfBuzz | MIT (old-style) |
| Little-CMS 2 | MIT |
| uchardet | MPL-1.1 / GPL-2.0-or-later / LGPL-2.1-or-later |
| libunibreak | Zlib |
| GLib and libintl (gettext) | LGPL-2.1-or-later |
| graphite2 | LGPL-2.1-or-later / MPL-1.1 / GPL-2.0-or-later |
| PCRE2 | BSD-3-Clause |
| libpng | PNG Reference Library License v2 |
| zlib | Zlib |

## Fonts

Embedded in the binary from `crates/app/assets/fonts/`; each family's OFL text ships
beside it.

| Font | Source | Licence |
|---|---|---|
| Archivo | Google Fonts | OFL-1.1 |
| Martian Mono | Google Fonts | OFL-1.1 |
| Bagel Fat One | Google Fonts | OFL-1.1 |

## Icons

The OSD and sidebar icons under `crates/app/assets/icons/` are from
[Lucide](https://lucide.dev), ISC, with MIT attribution for icons derived
from Feather. The full notices are beside the icons in
`crates/app/assets/icons/LICENSE`.

## Rust crates

Direct dependencies of the shipping crates (`app`, `jellyfin-api`,
`jellyfin-core`, `media-cache`, `player`, `seerr-api`). Versions are the
ones locked in `Cargo.lock`; licences are each crate's own `license` field
as reported by `cargo metadata`.

| Crate | Version | Licence |
|---|---|---|
| gpui | 0.2.2 | Apache-2.0 |
| objc2 | 0.6.4 | MIT |
| objc2-foundation | 0.3.2 | MIT |
| objc2-app-kit | 0.3.2 | Zlib OR Apache-2.0 OR MIT |
| objc2-quartz-core | 0.3.2 | Zlib OR Apache-2.0 OR MIT |
| objc2-core-graphics | 0.3.2 | Zlib OR Apache-2.0 OR MIT |
| objc2-media-player | 0.3.2 | Zlib OR Apache-2.0 OR MIT |
| block2 | 0.6.2 | MIT |
| raw-window-handle | 0.6.2 | MIT OR Apache-2.0 OR Zlib |
| security-framework | 3.7.0 | MIT OR Apache-2.0 |
| tokio | 1.53.1 | MIT |
| reqwest (rustls, no system OpenSSL) | 0.12.28 | MIT OR Apache-2.0 |
| tokio-tungstenite | 0.24.0 | MIT |
| futures-util | 0.3.33 | MIT OR Apache-2.0 |
| serde | 1.0.229 | MIT OR Apache-2.0 |
| serde_json | 1.0.151 | MIT OR Apache-2.0 |
| thiserror | 2.0.20 | MIT OR Apache-2.0 |
| tracing | 0.1.44 | MIT |
| tracing-subscriber | 0.3.23 | MIT |
| tracing-appender | 0.2.5 | MIT |
| chrono | 0.4.45 | MIT OR Apache-2.0 |
| url | 2.5.8 | MIT OR Apache-2.0 |
| uuid | 1.24.0 | Apache-2.0 OR MIT |
| rand | 0.8.7 | MIT OR Apache-2.0 |
| sha2 | 0.11.0 | MIT OR Apache-2.0 |
| lru | 0.18.2 | MIT |
| rusqlite (bundled SQLite) | 0.32.1 | MIT; the bundled SQLite C library is public domain |
| image | 0.25.10 | MIT OR Apache-2.0 |
| blurhash | 0.2.3 | Apache-2.0 OR MIT |
| smallvec | 1.15.2 | MIT OR Apache-2.0 |
| rust-embed | 8.12.0 | MIT |
| pkg-config (build script only) | 0.3.33 | MIT OR Apache-2.0 |

`tempfile` (MIT OR Apache-2.0) is also used at runtime for private atomic
Seerr config writes. GPUI's `test-support` feature is test-only. `cargo deny check` (see `deny.toml`)
verifies that every crate in the tree, transitive ones included, carries a
GPL-compatible licence.
