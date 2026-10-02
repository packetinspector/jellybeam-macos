#!/usr/bin/env bash
#
# bundle-app.sh — assemble a self-contained target/bundle/Jellybeam.app from
# the release build: builds `jellybeam`, vendors every non-system dylib it
# transitively needs (the core mpv/ffmpeg/libplacebo/libass chain from
# vendor/prefix, plus their brew-linked leaf deps and *their* transitive
# deps) into Contents/Frameworks, rewrites every install name / load
# command to be @rpath-relative, generates the brand app icon, writes
# Info.plist, ad-hoc codesigns everything, and (unless skipped) launches
# the bundle with JELLYBEAM_E2E=1 to prove it actually runs standalone.
#
# See docs/BUILD.md "Bundling" for the design writeup this implements.
#
# Usage:
#   scripts/bundle-app.sh              # build + assemble + verify + E2E
#   BUNDLE_SKIP_E2E=1 scripts/bundle-app.sh   # skip the launch/E2E step
#                                              # (e.g. no dev server running)
#   BUNDLE_SKIP_BUILD=1 scripts/bundle-app.sh # reuse the existing
#                                              # target/release/jellybeam
#                                              # instead of rebuilding
#   SIGNING_IDENTITY="Developer ID Application: ..." scripts/bundle-app.sh
#                                              # codesign with a real identity
#                                              # (hardened runtime) instead of
#                                              # the ad-hoc default (--sign -).
#   NOTARY_PROFILE=<notarytool keychain profile> # also notarize + staple
#
# Both variables can live in a gitignored `signing.env` at the repo root,
# which this script sources when present (docs/BUILD.md "Signing and
# notarization"). Codesigning has no effect on which session/device-id
# persistence backend crates/app/src/keychain.rs uses: that is the file
# store unless JELLYBEAM_KEYCHAIN=1 is set at runtime.
#
# Requires: everything docs/BUILD.md's vendoring step requires, plus Xcode
# Command Line Tools (install_name_tool, codesign, otool, sips, iconutil,
# swift) and python3 (only used for os.path.realpath — no third-party
# packages).

set -euo pipefail

# ---------------------------------------------------------------------------
# Paths / constants
# ---------------------------------------------------------------------------
ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
VENDOR_LIB="$ROOT_DIR/vendor/prefix/lib"
# One minimum macOS for the whole bundle, defined once in build-vendor.sh.
MIN_MACOS="$(sed -n 's/^DEPLOYMENT_TARGET="\(.*\)"$/\1/p' "$ROOT_DIR/scripts/build-vendor.sh")"

BUNDLE_ROOT="$ROOT_DIR/target/bundle"
APP_DIR="$BUNDLE_ROOT/Jellybeam.app"
CONTENTS_DIR="$APP_DIR/Contents"
MACOS_DIR="$CONTENTS_DIR/MacOS"
FRAMEWORKS_DIR="$CONTENTS_DIR/Frameworks"
RESOURCES_DIR="$CONTENTS_DIR/Resources"

BIN_NAME="jellybeam"
BUILT_BIN="$ROOT_DIR/target/release/$BIN_NAME"
BUNDLED_BIN="$MACOS_DIR/$BIN_NAME"

# CFBundleIdentifier/CFBundleName etc. live in scripts/Info.plist.in
# (single source of truth) -- BUNDLE_NAME here is just for log messages.
BUNDLE_NAME="Jellybeam"

DEV_SERVER_URL="${JELLYBEAM_E2E_SERVER:-http://localhost:8096}"

# Release signing material lives outside the tree (gitignored).
if [ -f "$ROOT_DIR/signing.env" ]; then
  # shellcheck disable=SC1091
  . "$ROOT_DIR/signing.env"
fi

WORK_DIR="$(mktemp -d "${TMPDIR:-/tmp}/jellybeam-bundle.XXXXXX")"
trap 'rm -rf "$WORK_DIR"' EXIT

# ---------------------------------------------------------------------------
# Logging
# ---------------------------------------------------------------------------
log() { printf '\n\033[1;34m==> %s\033[0m\n' "$*"; }
warn() { printf '\033[1;33m!! %s\033[0m\n' "$*" >&2; }
die() { printf '\033[1;31mFATAL: %s\033[0m\n' "$*" >&2; exit 1; }

# ---------------------------------------------------------------------------
# Step: build
# ---------------------------------------------------------------------------
step_build() {
  if [ "${BUNDLE_SKIP_BUILD:-0}" = "1" ]; then
    warn "BUNDLE_SKIP_BUILD=1 — reusing existing $BUILT_BIN"
    [ -x "$BUILT_BIN" ] || die "BUNDLE_SKIP_BUILD=1 but $BUILT_BIN does not exist/isn't executable"
    return
  fi
  # Debug info and panic locations embed source paths; remap the machine-
  # specific prefixes so the shipped binary carries none of them.
  local remap
  remap="--remap-path-prefix=${CARGO_HOME:-$HOME/.cargo}=/cargo"
  remap="$remap --remap-path-prefix=${RUSTUP_HOME:-$HOME/.rustup}=/rustup"
  remap="$remap --remap-path-prefix=$ROOT_DIR=/jellybeam"
  remap="$remap --remap-path-prefix=$HOME=/home"
  log "cargo build --release -p app for macOS $MIN_MACOS (with --remap-path-prefix)"
  ( cd "$ROOT_DIR" && MACOSX_DEPLOYMENT_TARGET="$MIN_MACOS" RUSTFLAGS="${RUSTFLAGS:-} $remap" \
      cargo build --release -p app )
  [ -x "$BUILT_BIN" ] || die "build succeeded but $BUILT_BIN not found"

}

# ---------------------------------------------------------------------------
# Step: bundle skeleton
# ---------------------------------------------------------------------------
step_skeleton() {
  log "Assembling bundle skeleton at $APP_DIR"
  rm -rf "$APP_DIR"
  mkdir -p "$MACOS_DIR" "$FRAMEWORKS_DIR" "$RESOURCES_DIR"

  cp "$BUILT_BIN" "$BUNDLED_BIN"
  chmod +w "$BUNDLED_BIN"
  chmod 755 "$BUNDLED_BIN"
}

# ---------------------------------------------------------------------------
# Step: Info.plist
# ---------------------------------------------------------------------------
workspace_version() {
  # Cargo.toml's [workspace.package] version = "X.Y.Z" — every crate
  # (including `app`) inherits this via `version.workspace = true`.
  awk '
    /^\[workspace\.package\]/ { in_block=1; next }
    /^\[/ { in_block=0 }
    in_block && /^version[[:space:]]*=/ {
      match($0, /"[^"]*"/)
      v = substr($0, RSTART+1, RLENGTH-2)
      print v
      exit
    }
  ' "$ROOT_DIR/Cargo.toml"
}

step_info_plist() {
  local version
  version="$(workspace_version)"
  [ -n "$version" ] || die "could not read [workspace.package] version from Cargo.toml"
  local template="$ROOT_DIR/scripts/Info.plist.in"
  [ -f "$template" ] || die "missing $template"
  log "Writing Info.plist from scripts/Info.plist.in (CFBundleVersion $version)"

  sed -e "s/@JELLYBEAM_VERSION@/$version/g" -e "s/@JELLYBEAM_MIN_MACOS@/$MIN_MACOS/g" \
    "$template" > "$CONTENTS_DIR/Info.plist"
}

# ---------------------------------------------------------------------------
# Step: icon
# ---------------------------------------------------------------------------
step_icon() {
  log "Generating brand AppIcon.icns (DESIGN-GUIDE.md §A.4)"
  "$ROOT_DIR/scripts/make-icon.sh" "$RESOURCES_DIR/AppIcon.icns"
}

step_notices() {
  log "Copying project, font, icon and native-library notices"
  local notices="$RESOURCES_DIR/licenses" component source file
  mkdir -p "$notices/fonts" "$notices/icons"
  cp "$ROOT_DIR/LICENSE" "$ROOT_DIR/THIRD-PARTY.md" "$ROOT_DIR/TRADEMARKS.md" "$notices/"
  cp "$ROOT_DIR/crates/app/assets/fonts"/OFL-*.txt "$notices/fonts/"
  cp "$ROOT_DIR/crates/app/assets/icons/LICENSE" "$notices/icons/"
  # Every vendored source tree: the core chain plus the leaf libraries.
  for source in "$ROOT_DIR"/vendor/src/*/; do
    source="${source%/}"
    component="$(basename "$source")"
    mkdir -p "$notices/$component"
    for file in "$source"/COPYING* "$source"/LICENSE* "$source"/Copyright "$source"/docs/LICENSE.TXT "$source"/docs/FTL.TXT; do
      [ -f "$file" ] || continue
      cp "$file" "$notices/$component/"
    done
  done
}

# ---------------------------------------------------------------------------
# Step: dylib relocation (the real work)
# ---------------------------------------------------------------------------
# Resolve a path through any symlinks to the real file underneath (macOS
# bash 3.2 has no `readlink -f`; python3's os.path.realpath is the portable
# option available on every Mac this targets).
realpath_py() {
  python3 -c 'import os, sys; print(os.path.realpath(sys.argv[1]))' "$1"
}

# All LC_LOAD_DYLIB / LC_ID_DYLIB paths in $1 that point outside the system
# (i.e. into vendor/prefix or a brew keg) — one per line.
non_system_deps() {
  local file="$1"
  otool -L "$file" | tail -n +2 | awk '{print $1}' | grep -E "^(${VENDOR_LIB}/|/opt/homebrew/)" || true
}

# A dylib's own install name (LC_ID_DYLIB, i.e. `otool -D`'s second line)
# is what every *other* file's LC_LOAD_DYLIB entry references it as -- e.g.
# vendor/prefix's libavcodec.62.28.102.dylib carries the id
# ".../libavcodec.62.dylib" (the unversioned SONAME-style name), and every
# dependent links against that short form, never the fully-versioned disk
# filename. So the canonical bundled basename must come from the ID, not
# from whatever path/filename we happened to discover the file through --
# otherwise the same library gets vendored twice under two names (one dead
# copy nothing actually loads).
canonical_basename() {
  local real="$1" fallback="$2"
  local id
  id="$(otool -D "$real" | tail -n +2 | head -1)"
  if [ -n "$id" ]; then
    basename "$id"
  else
    basename "$fallback"
  fi
}

# Copies the transitive closure of vendor/prefix + brew-linked dylibs that
# the app binary needs (directly, or via the mpv/ffmpeg/libplacebo/libass
# chain, or via *their* brew-side transitive deps -- e.g.
# harfbuzz -> libpng/glib/graphite2, glib -> gettext/pcre2) into
# Contents/Frameworks, by recursively scanning each newly-copied dylib's
# own `otool -L` output until the queue is empty.
relocate_dylibs() {
  log "Discovering + copying the transitive vendor/brew dylib closure into Contents/Frameworks"

  local processed_list="$WORK_DIR/processed.txt"
  : > "$processed_list"

  # Seed with the app binary's own direct vendor/brew deps (today just
  # libmpv.2.dylib; generic in case that ever changes).
  local queue=()
  while IFS= read -r f; do
    queue+=("$f")
  done < <(non_system_deps "$BUILT_BIN")

  [ "${#queue[@]}" -gt 0 ] || die "$BUILT_BIN has no vendor/homebrew dylib dependencies -- expected at least libmpv"

  while [ "${#queue[@]}" -gt 0 ]; do
    local src="${queue[0]}"
    queue=("${queue[@]:1}")

    local real
    real="$(realpath_py "$src")"
    [ -f "$real" ] || die "dependency resolved to nonexistent file: $src -> $real"

    local base
    base="$(canonical_basename "$real" "$src")"

    if grep -qxF "$base" "$processed_list" 2>/dev/null; then
      continue
    fi

    cp -f "$real" "$FRAMEWORKS_DIR/$base"
    chmod +w "$FRAMEWORKS_DIR/$base"
    echo "$base" >> "$processed_list"
    echo "  vendored: $base  (from $real)"

    while IFS= read -r dep; do
      [ -z "$dep" ] && continue
      queue+=("$dep")
    done < <(non_system_deps "$real")
  done

  local count
  count="$(wc -l < "$processed_list" | tr -d ' ')"
  log "Vendored $count dylib(s) into Contents/Frameworks"
}

# Rewrites $1's own install name (if it's a dylib, not the main binary) to
# @rpath/<basename>, and every LC_LOAD_DYLIB entry pointing at vendor/prefix
# or a brew keg to @rpath/<that dependency's basename>.
rewrite_install_names() {
  local file="$1"
  local is_dylib="$2" # "1" or "0"

  if [ "$is_dylib" = "1" ]; then
    local base
    base="$(basename "$file")"
    install_name_tool -id "@rpath/$base" "$file"
  fi

  local dep
  while IFS= read -r dep; do
    [ -z "$dep" ] && continue
    local depbase
    depbase="$(basename "$dep")"
    install_name_tool -change "$dep" "@rpath/$depbase" "$file"
  done < <(non_system_deps "$file")
}

add_rpath_if_missing() {
  local file="$1" rpath="$2"
  if otool -l "$file" | grep -A2 LC_RPATH | grep -qF "$rpath"; then
    return
  fi
  install_name_tool -add_rpath "$rpath" "$file"
}

remove_build_rpaths() {
  local file="$1" rpath
  while IFS= read -r rpath; do
    case "$rpath" in
      /usr/lib/*|/System/Library/*|@*) ;;
      /*) install_name_tool -delete_rpath "$rpath" "$file" ;;
    esac
  done < <(otool -l "$file" | awk '/cmd LC_RPATH/ { rpath=1; next } rpath && /path / { sub(/^.*path /, ""); sub(/ \(offset.*$/, ""); print; rpath=0 }')
}

step_relocate() {
  relocate_dylibs

  log "Rewriting install names / load commands to @rpath (install_name_tool)"
  local f
  for f in "$FRAMEWORKS_DIR"/*.dylib; do
    [ -e "$f" ] || continue
    rewrite_install_names "$f" 1
    remove_build_rpaths "$f"
  done
  rewrite_install_names "$BUNDLED_BIN" 0
  remove_build_rpaths "$BUNDLED_BIN"

  log "Adding LC_RPATH @executable_path/../Frameworks to $BIN_NAME"
  add_rpath_if_missing "$BUNDLED_BIN" "@executable_path/../Frameworks"

  # Every bundled dylib also needs to find its @rpath-relative siblings;
  # give each one an rpath pointing at its own directory (they all live
  # flat in Contents/Frameworks together).
  for f in "$FRAMEWORKS_DIR"/*.dylib; do
    [ -e "$f" ] || continue
    add_rpath_if_missing "$f" "@loader_path"
  done
}

# ---------------------------------------------------------------------------
# Step: verify no /opt/homebrew or vendor/prefix residue remains
# ---------------------------------------------------------------------------
step_verify_no_residue() {
  log "Verifying no /opt/homebrew or vendor/prefix paths remain (otool -L audit)"

  local targets=("$BUNDLED_BIN")
  local f
  for f in "$FRAMEWORKS_DIR"/*.dylib; do
    [ -e "$f" ] || continue
    targets+=("$f")
  done

  local bad=0
  for f in "${targets[@]}"; do
    local hits
    hits="$(otool -L "$f" | tail -n +2 | grep -E "/opt/homebrew|${VENDOR_LIB}|${ROOT_DIR}/vendor" || true)"
    if [ -n "$hits" ]; then
      warn "residue in ${f#$ROOT_DIR/}:"
      echo "$hits" >&2
      bad=1
    fi
    # docs/BUILD.md: scan the assembled artifacts after relocation, when legitimate build-time load paths are gone.
    strings "$f" > "$WORK_DIR/artifact-strings.txt"
    if grep -qF "$HOME" "$WORK_DIR/artifact-strings.txt" || grep -qF "$ROOT_DIR" "$WORK_DIR/artifact-strings.txt"; then
      warn "embedded developer path in $(basename "$f"); rebuild the native stack with scripts/build-vendor.sh"
      bad=1
    fi
  done

  # docs/BUILD.md: the bundle's real minimum is its newest Mach-O, so every
  # file must target LSMinimumSystemVersion or older and none may need the
  # Swift runtime, which older macOS releases ship without newer symbols.
  local minos
  for f in "${targets[@]}"; do
    minos="$(vtool -show-build "$f" | awk '/minos/ {print $2; exit}')"
    if [ -z "$minos" ] || [ "$(printf '%s\n%s\n' "$minos" "$MIN_MACOS" | sort -V | tail -1)" != "$MIN_MACOS" ]; then
      warn "$(basename "$f") requires macOS ${minos:-unknown}, newer than the bundle's $MIN_MACOS"
      bad=1
    fi
    if otool -L "$f" | grep -q "libswift"; then
      warn "$(basename "$f") links the Swift runtime"
      bad=1
    fi
  done

  if [ "$bad" -ne 0 ]; then
    die "bundle verification FAILED (see above)"
  fi

  log "verify_no_residue: OK — ${#targets[@]} file(s) checked (binary + $(( ${#targets[@]} - 1 )) Frameworks dylibs), no residue"
}

# ---------------------------------------------------------------------------
# Step: codesign
# ---------------------------------------------------------------------------
step_codesign() {
  # `-` (ad-hoc) unless a real identity was supplied -- see this script's
  # usage comment. Codesigning only: it has no effect on which
  # session/device-id persistence backend the built binary uses (always the
  # file store, unless overridden at runtime with JELLYBEAM_KEYCHAIN=1 -- see
  # crates/app/src/keychain.rs's doc comment).
  local identity="${SIGNING_IDENTITY:--}"
  local -a opts=()
  if [ "$identity" = "-" ]; then
    log "Ad-hoc codesigning each relocated dylib (install_name_tool invalidates existing signatures)"
  else
    # The hardened runtime is required for notarization.
    opts=(--options runtime --timestamp)
    log "Codesigning each relocated dylib with SIGNING_IDENTITY ($identity)"
  fi
  local f
  for f in "$FRAMEWORKS_DIR"/*.dylib; do
    [ -e "$f" ] || continue
    codesign --force ${opts[@]+"${opts[@]}"} --sign "$identity" "$f"
  done

  if [ "$identity" = "-" ]; then
    log "Ad-hoc codesigning the whole bundle (codesign --force --deep --sign -)"
  else
    log "Codesigning the whole bundle with SIGNING_IDENTITY ($identity)"
  fi
  codesign --force --deep ${opts[@]+"${opts[@]}"} --sign "$identity" "$APP_DIR"

  log "Verifying signature (codesign -v)"
  codesign -v --deep --strict "$APP_DIR" || die "codesign -v FAILED"
  log "codesign -v: OK"
}

# ---------------------------------------------------------------------------
# Step: notarize + staple (only with a real identity and NOTARY_PROFILE)
# ---------------------------------------------------------------------------
step_notarize() {
  [ -n "${NOTARY_PROFILE:-}" ] || return 0
  [ "${SIGNING_IDENTITY:--}" != "-" ] || die "NOTARY_PROFILE set but SIGNING_IDENTITY is ad-hoc; notarization needs a Developer ID signature"
  local zip="$BUNDLE_ROOT/$BUNDLE_NAME.zip"
  log "Notarizing (xcrun notarytool submit --wait, profile $NOTARY_PROFILE)"
  rm -f "$zip"
  ditto -c -k --keepParent "$APP_DIR" "$zip"
  xcrun notarytool submit "$zip" --keychain-profile "$NOTARY_PROFILE" --wait || die "notarization failed"
  log "Stapling the notarization ticket"
  xcrun stapler staple "$APP_DIR" || die "stapler failed"
  spctl -a -vv "$APP_DIR" || die "Gatekeeper assessment failed after stapling"
  rm -f "$zip"
}

# ---------------------------------------------------------------------------
# Step: E2E launch verification
# ---------------------------------------------------------------------------
step_e2e_verify() {
  if [ "${BUNDLE_SKIP_E2E:-0}" = "1" ]; then
    warn "BUNDLE_SKIP_E2E=1 — skipping JELLYBEAM_E2E launch verification"
    return
  fi

  log "Checking dev server at $DEV_SERVER_URL"
  if ! curl -fsS -o /dev/null "$DEV_SERVER_URL/System/Info/Public"; then
    die "dev server not reachable at $DEV_SERVER_URL — start it (dev/server.sh up) or set BUNDLE_SKIP_E2E=1"
  fi

  mkdir -p "$BUNDLE_ROOT"
  local dyld_log="$BUNDLE_ROOT/dyld-e2e.log"
  local timeout_secs="${BUNDLE_E2E_TIMEOUT:-90}"
  log "Launching bundled $BUNDLE_NAME.app with JELLYBEAM_E2E=1 (DYLD_PRINT_LIBRARIES capture -> ${dyld_log#$ROOT_DIR/}, ${timeout_secs}s timeout)"

  # Run in the background under our own control rather than a blocking
  # foreground call: dyld resolves *every* direct+transitive LC_LOAD_DYLIB
  # (the whole libmpv/ffmpeg/libplacebo/libass/brew-leaf chain) before
  # jumping to Rust's `main`, so the provenance data we need is already
  # fully captured in $dyld_log the moment the process starts running --
  # independent of whatever happens afterward. That lets us bound the wait
  # instead of risking an indefinite hang if e.g. a first-run Keychain
  # access prompt (crates/app/src/keychain.rs, unrelated to bundling) needs
  # a human click that nobody is present to give in a non-interactive run.
  : > "$dyld_log"
  set +e
  # No JELLYBEAM_KEYCHAIN=1 here on purpose: crates/app/src/keychain.rs's
  # session/device-id persistence is always the file store unless that
  # runtime override is set, so this launch never touches the real Keychain
  # (and can't hit the interactive authorization-prompt hang that used to
  # require a file-store override here -- see docs/BUILD.md's Keychain note).
  DYLD_PRINT_LIBRARIES=1 JELLYBEAM_E2E=1 "$BUNDLED_BIN" > "$dyld_log" 2>&1 &
  local e2e_pid=$!
  local waited=0 timed_out=0
  while kill -0 "$e2e_pid" 2>/dev/null; do
    if [ "$waited" -ge "$timeout_secs" ]; then
      timed_out=1
      warn "jellybeam (pid $e2e_pid) still running after ${timeout_secs}s — sending SIGTERM"
      kill -TERM "$e2e_pid" 2>/dev/null
      sleep 2
      kill -KILL "$e2e_pid" 2>/dev/null
      break
    fi
    sleep 1
    waited=$((waited + 1))
  done
  wait "$e2e_pid" 2>/dev/null
  local status=$?
  set -e

  # --- DYLD provenance check: always run, timeout or not (see comment above
  # for why the captured data is valid either way). This is the load-bearing
  # assertion that the bundle is self-contained at runtime. ---
  log "Checking DYLD_PRINT_LIBRARIES provenance ($dyld_log)"
  local bad_lines
  bad_lines="$(grep -E "/opt/homebrew|${ROOT_DIR}/vendor/prefix" "$dyld_log" || true)"
  if [ -n "$bad_lines" ]; then
    warn "DYLD_PRINT_LIBRARIES shows loads from outside the bundle:"
    echo "$bad_lines" >&2
    die "DYLD provenance check FAILED — bundle is not self-contained at runtime"
  fi

  local fw_count
  fw_count="$(grep -c "Contents/Frameworks/" "$dyld_log" || true)"
  fw_count="${fw_count:-0}"
  if [ "$fw_count" -lt 5 ]; then
    warn "---- tail of $dyld_log ----"
    tail -40 "$dyld_log" >&2 || true
    die "DYLD provenance check FAILED — expected several loads from Contents/Frameworks/, saw $fw_count"
  fi
  log "DYLD provenance check: OK ($fw_count libraries loaded from Contents/Frameworks/, 0 from /opt/homebrew or vendor/prefix)"

  # --- JELLYBEAM_E2E functional assertion ---
  if [ "$timed_out" -eq 1 ]; then
    warn "---- tail of $dyld_log ----"
    tail -40 "$dyld_log" >&2 || true
    die "JELLYBEAM_E2E did not complete within ${timeout_secs}s (killed). DYLD provenance above still passed" \
        " (dyld resolves the whole dependency graph before Rust main() runs), but the functional" \
        " login->play->hwdec assertion could not be confirmed. A hang inside" \
        " keychain::device_id()/SecItemCopyMatching usually means a stale Keychain ACL entry for" \
        " service 'tv.jellybeam.jellyfin' from a previous build (different path/signature) is forcing" \
        " an interactive Keychain access prompt -- rerun from a normal interactive Terminal session" \
        " and approve it once (or clear that Keychain item), then re-run."
  fi

  if [ "$status" -ne 0 ]; then
    warn "---- tail of $dyld_log ----"
    tail -80 "$dyld_log" >&2 || true
    die "JELLYBEAM_E2E FAILED (jellybeam exited $status) — see $dyld_log"
  fi
  if ! grep -q "JELLYBEAM_E2E: PASS" "$dyld_log"; then
    die "jellybeam exited 0 but 'JELLYBEAM_E2E: PASS' was not found in $dyld_log — treating as failure"
  fi
  log "JELLYBEAM_E2E: PASS (bundled binary exited 0)"
}

# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------
main() {
  step_build
  step_skeleton
  step_info_plist
  step_icon
  step_notices
  step_relocate
  step_verify_no_residue
  step_codesign
  step_notarize
  step_e2e_verify

  log "Done: $APP_DIR"
  du -sh "$APP_DIR" 2>/dev/null || true
}

main "$@"
