# Building Jellybeam

Jellybeam builds on Apple Silicon Macs with Xcode Command Line Tools, Rust
and Homebrew. The app runs on macOS 11 or later. Its interface is Rust;
libmpv, FFmpeg and the other native media libraries are C, built from
source by `scripts/build-vendor.sh`. Nothing in the bundle uses Swift.

## Quick start

```sh
xcode-select --install
brew install meson ninja nasm cmake automake autoconf pkg-config
scripts/build-vendor.sh
cargo build --locked -p app
scripts/bundle-app.sh
```

Rustup reads the pinned version and components from `rust-toolchain.toml`.
Homebrew supplies build tools only; every library in the bundle is compiled
from a pinned source release.

The debug binary is `target/debug/jellybeam`. The assembled app is
`target/bundle/Jellybeam.app`. Build output stays out of Git.

## Vendored components

`scripts/build-vendor.sh` pins both the upstream tag and the immutable
commit for each core component, and the release tarball and SHA-256 for
each leaf library. The script is the source of truth for these pins and
configure flags.

| Component | Tag |
|---|---|
| FFmpeg | `n8.1.2` |
| libplacebo | `v7.360.1` |
| libass | `0.17.5` |
| mpv | `v0.41.0` |

Leaf libraries, built first: libpng, FreeType, HarfBuzz, FriBidi,
libunibreak, uchardet, Little-CMS 2, dav1d and soxr. zlib, bzip2 and iconv
come from macOS.

Sources go into `vendor/src/`, builds and logs into `vendor/build/`, and
installed headers and libraries into `vendor/prefix/`. Completed-component
markers contain the tag, commit and build-recipe version; a changed pin or
recipe triggers a rebuild.
When a dependency changes, remove the dependent components' markers too.

```sh
scripts/build-vendor.sh deps        # the leaf libraries
scripts/build-vendor.sh ffmpeg      # one component
scripts/build-vendor.sh libplacebo
scripts/build-vendor.sh libass
scripts/build-vendor.sh mpv
scripts/build-vendor.sh verify      # every dylib targets the deployment target
scripts/build-vendor.sh clean       # removes build output and the prefix
```

Retain the corresponding source and notices for a distributed binary; see
[RELEASE.md](RELEASE.md) and [../THIRD-PARTY.md](../THIRD-PARTY.md).

Native compilation remaps source paths; FFmpeg and mpv diagnostic build
configuration strings are sanitized before compilation. Bundle validation
checks every Mach-O artifact for the developer home and checkout paths
after removing build-only runtime search paths.

### Deployment target

Every Mach-O in the bundle targets one macOS version, `DEPLOYMENT_TARGET`
in `scripts/build-vendor.sh` (11.0, the first macOS on Apple Silicon).
`bundle-app.sh` builds the Rust binary for the same target, writes it into
`LSMinimumSystemVersion`, and refuses to finish if any bundled file is
stamped for a newer macOS or links the Swift runtime. A bundle's real
minimum is its newest file, so a single Homebrew bottle (compiled for the
build machine's macOS) would silently raise it; that is why the leaf
libraries are built from source.

mpv is configured as `libmpv` only (`-Dcplayer=false`, `-Dswift-build=disabled`).
Its standalone player, Cocoa window backend, media-key and Touch Bar
integrations are Swift and are not built: Jellybeam owns the window, the
render context and Now Playing itself. Cocoa and `gl-cocoa` stay enabled
because VideoToolbox's OpenGL interop needs them.

Intel and universal builds are unsupported.

## libplacebo: OpenGL vs Vulkan

The embedded player uses mpv's OpenGL render API and a Core Profile
`NSOpenGLContext`. libplacebo is configured with its OpenGL backend;
Vulkan, MoltenVK and shaderc are not needed for this embedding. GPUI
renders the interface with Metal. See [OVERVIEW.md](OVERVIEW.md) and
[../crates/app/ARCHITECTURE.md](../crates/app/ARCHITECTURE.md).

## Bundling

`scripts/bundle-app.sh`:

1. Builds the release binary with machine-specific source paths remapped.
2. Creates the `.app`, fills `Info.plist` from `scripts/Info.plist.in` using
   the workspace version, generates the brand icon, and copies project,
   font, icon and core media-stack notices into Resources.
   Rust transitive and Homebrew leaf notices still need release review.
3. Walks the transitive vendor and Homebrew dylib dependencies, copies them
   into `Contents/Frameworks`, and rewrites their install names to `@rpath`.
4. Adds bundle-relative runtime search paths and checks for vendor and
   Homebrew load-command residue.
5. Signs each dylib and the bundle, then verifies the signatures.
6. Optionally notarizes and staples, when signing settings are supplied.
7. Launches the bundle's E2E harness, checks loaded-library provenance and
   requires `JELLYBEAM_E2E: PASS`.

The E2E step needs the synthetic dev server and corpus:

```sh
dev/server.sh corpus
dev/server.sh up
scripts/bundle-app.sh
```

`BUNDLE_E2E_TIMEOUT` sets the bounded wait in seconds (default 90).
`JELLYBEAM_E2E_SERVER`, `_USERNAME` and `_PASSWORD` can override the
synthetic test account. Never point the harness at a personal library.
It isolates application state; it still changes watched state on the
server it tests.

```sh
BUNDLE_SKIP_E2E=1 scripts/bundle-app.sh     # assembly and static checks only
BUNDLE_SKIP_BUILD=1 scripts/bundle-app.sh   # reuse an existing release binary
```

A skipped step is unverified. In particular, reusing a binary does not
prove it contains the current source or was built with path remapping.

## Keychain caveat

Session tokens use a private JSON file under Application Support by
default, including in signed releases. `JELLYBEAM_KEYCHAIN=1` opts into the
macOS Keychain at runtime; signing identity does not select the backend.
An ad-hoc identity changes on rebuild and can cause Keychain authorization
prompts, which is why the file store is the default.

Seerr credentials use a separate private JSON file. Settings, tokens and
pending reports are local state, never release inputs. Logs can contain
identifying data; review them before sharing.

## Signing and notarization

The default signature is ad-hoc, suitable for local testing. A public app
download needs Developer ID signing and notarization for normal Gatekeeper
acceptance.

The bundle script already enables the hardened runtime and timestamping
when a real signing identity is supplied. Set both values in the shell or
in the gitignored `signing.env`:

```sh
SIGNING_IDENTITY='Developer ID Application: <certificate identity>' \
NOTARY_PROFILE='<notarytool keychain profile>' \
  scripts/bundle-app.sh
```

Store notarization credentials with `xcrun notarytool store-credentials`;
use a Keychain profile rather than placing credentials in commands or Git.
With both settings, the script submits a temporary zip, staples the app,
checks Gatekeeper acceptance and removes the temporary zip. A signing
identity alone signs but does not notarize.

Create the downloadable archive **after** stapling so it includes the
ticket:

```sh
ditto -c -k --keepParent target/bundle/Jellybeam.app target/bundle/Jellybeam.zip
xcrun stapler validate target/bundle/Jellybeam.app
spctl -a -vv target/bundle/Jellybeam.app
shasum -a 256 target/bundle/Jellybeam.zip
```

See [RELEASE.md](RELEASE.md) for source publication, checks, notices and the
remaining manual validation before distribution.
