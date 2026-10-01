#!/usr/bin/env bash
#
# build-vendor.sh — build libmpv (with -Dlibmpv=true) and its core chain
# (ffmpeg, libplacebo, libass) from source, vendored into vendor/prefix
# for embedding in Jellybeam.app.
#
# Usage:
#   scripts/build-vendor.sh              # build everything (idempotent)
#   scripts/build-vendor.sh clean        # remove vendor/build and vendor/prefix
#   scripts/build-vendor.sh <component>  # build/rebuild just one of:
#                                          brew ffmpeg libplacebo libass mpv
#
# Re-running is a no-op for components that already built successfully at
# the pinned version (tracked via marker files in vendor/build/.markers).
# Bump a *_TAG/*_COMMIT pair below and re-run to rebuild just that component
# (and anything after it, if you also clear its marker).
#
# Requires (see docs/BUILD.md): Xcode CLT, meson, ninja, nasm, cmake, automake,
# autoconf, pkg-config, python3, brew.

set -euo pipefail

# ---------------------------------------------------------------------------
# Pinned versions
# ---------------------------------------------------------------------------
FFMPEG_TAG="n8.1.2"        # https://github.com/FFmpeg/FFmpeg tags, stable point release
LIBPLACEBO_TAG="v7.360.1"  # https://github.com/haasn/libplacebo/releases (latest stable)
LIBASS_TAG="0.17.5"        # https://github.com/libass/libass/releases (latest stable)
MPV_TAG="v0.41.0"          # https://github.com/mpv-player/mpv/releases (latest stable)
# Tags are readable release labels; commits are the immutable build inputs.
FFMPEG_COMMIT="38b88335f99e76ed89ff3c93f877fdefce736c13"
LIBPLACEBO_COMMIT="cee9b076f2c63104ccfd497fa79c39a867293ec4"
LIBASS_COMMIT="4a05d8127f525943ebf45fdc6497c9e665947f0d"
MPV_COMMIT="41f6a645068483470267271e1d09966ca3b9f413"
BUILD_RECIPE_VERSION=3
FFMPEG_REF="$FFMPEG_TAG@$FFMPEG_COMMIT:recipe-$BUILD_RECIPE_VERSION"
LIBPLACEBO_REF="$LIBPLACEBO_TAG@$LIBPLACEBO_COMMIT:recipe-$BUILD_RECIPE_VERSION"
LIBASS_REF="$LIBASS_TAG@$LIBASS_COMMIT:recipe-$BUILD_RECIPE_VERSION"
MPV_REF="$MPV_TAG@$MPV_COMMIT:recipe-$BUILD_RECIPE_VERSION"

# Leaf build deps installed via brew (kept minimal — see docs/BUILD.md).
# zlib/bzip2 are keg-only (macOS ships them, but without .pc files) and are
# pulled in only because brew's freetype2.pc has Requires.private: zlib
# bzip2 — pkg-config refuses to resolve freetype2 without them being
# resolvable too, even though the actual symbols come from the system libs.
# NOTE: no Vulkan/MoltenVK/shaderc/spirv-cross here — libplacebo is built
# with the OpenGL GPU backend only (see docs/BUILD.md "libplacebo: OpenGL vs
# Vulkan" for why, and how to switch later).
BREW_LEAF_DEPS=(dav1d libsoxr freetype fribidi harfbuzz little-cms2 uchardet libunibreak zlib bzip2)

# ---------------------------------------------------------------------------
# Paths
# ---------------------------------------------------------------------------
ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
VENDOR_DIR="$ROOT_DIR/vendor"
SRC_DIR="$VENDOR_DIR/src"
BUILD_DIR="$VENDOR_DIR/build"
PREFIX="$VENDOR_DIR/prefix"
MARKERS_DIR="$BUILD_DIR/.markers"
LOG_DIR="$BUILD_DIR/.logs"

mkdir -p "$SRC_DIR" "$BUILD_DIR" "$PREFIX" "$MARKERS_DIR" "$LOG_DIR"

JOBS="$(sysctl -n hw.ncpu)"

# ---------------------------------------------------------------------------
# Toolchain / environment
# ---------------------------------------------------------------------------
export SDKROOT="$(xcrun --show-sdk-path)"
export MACOSX_DEPLOYMENT_TARGET="11.0"
export CC="$(xcrun -f clang)"
export CXX="$(xcrun -f clang++)"

BREW_PREFIX="$(brew --prefix)"
# zlib is keg-only on macOS (system provides the lib, brew only adds the
# .pc file), so its pkgconfig dir isn't under lib/pkgconfig.
ZLIB_PKGCONFIG="$(brew --prefix zlib 2>/dev/null || echo "$BREW_PREFIX/opt/zlib")/lib/pkgconfig"
BZIP2_PREFIX="$(brew --prefix bzip2 2>/dev/null || echo "$BREW_PREFIX/opt/bzip2")"

# Upstream bzip2 ships no .pc file at all (brew's formula doesn't add one
# either), yet brew's freetype2.pc has `Requires.private: ... bzip2`, which
# makes pkg-config refuse to resolve freetype2 unless *some* bzip2.pc
# exists. Write a minimal shim pointing at brew's bzip2 keg.
SHIM_PKGCONFIG_DIR="$BUILD_DIR/.pkgconfig-shims"
mkdir -p "$SHIM_PKGCONFIG_DIR"
cat > "$SHIM_PKGCONFIG_DIR/bzip2.pc" <<EOF
prefix=$BZIP2_PREFIX
libdir=\${prefix}/lib
includedir=\${prefix}/include

Name: bzip2
Description: bzip2 compression library (pkg-config shim; upstream ships none)
Version: 1.0.8
Libs: -L\${libdir} -lbz2
Cflags: -I\${includedir}
EOF

# Our own vendored prefix takes priority over brew's leaf deps, which take
# priority over anything else pkg-config might find on the system.
export PKG_CONFIG_PATH="$PREFIX/lib/pkgconfig:$BREW_PREFIX/lib/pkgconfig:$ZLIB_PKGCONFIG:$SHIM_PKGCONFIG_DIR:${PKG_CONFIG_PATH:-}"
export PKG_CONFIG_LIBDIR="$PREFIX/lib/pkgconfig:$BREW_PREFIX/lib/pkgconfig:$ZLIB_PKGCONFIG:$SHIM_PKGCONFIG_DIR"

export CPPFLAGS="-I$PREFIX/include -I$BREW_PREFIX/include ${CPPFLAGS:-}"
export CFLAGS="-arch arm64 -O2 -ffile-prefix-map=$ROOT_DIR=/jellybeam -ffile-prefix-map=$HOME=/home ${CFLAGS:-}"
export CXXFLAGS="-arch arm64 -O2 -ffile-prefix-map=$ROOT_DIR=/jellybeam -ffile-prefix-map=$HOME=/home ${CXXFLAGS:-}"
export OBJCFLAGS="$CFLAGS ${OBJCFLAGS:-}"
export LDFLAGS="-arch arm64 -L$PREFIX/lib -L$BREW_PREFIX/lib ${LDFLAGS:-}"

MESON_CROSS_ARGS=(--prefix "$PREFIX" --libdir lib --buildtype release)

log() { printf '\n\033[1;34m==> %s\033[0m\n' "$*"; }
warn() { printf '\033[1;33m!! %s\033[0m\n' "$*" >&2; }
die() { printf '\033[1;31mFATAL: %s\033[0m\n' "$*" >&2; exit 1; }

marker_path() { echo "$MARKERS_DIR/$1.done"; }

is_done() {
  # $1 = component name, $2 = pinned version/tag
  local m; m="$(marker_path "$1")"
  [[ -f "$m" ]] && [[ "$(cat "$m")" == "$2" ]]
}

mark_done() {
  # $1 = component name, $2 = pinned version/tag
  echo "$2" > "$(marker_path "$1")"
}

clone_pinned() {
  # $1 = repo url, $2 = readable tag, $3 = immutable commit, $4 = dest name
  local url="$1" tag="$2" commit="$3" name="$4"
  local dest="$SRC_DIR/$name"
  if [[ -d "$dest/.git" ]]; then
    local current
    current="$(git -C "$dest" rev-parse HEAD 2>/dev/null || true)"
    if [[ "$current" == "$commit" ]]; then
      log "$name: source already at $tag ($commit), skipping clone"
      return
    fi
    warn "$name: checked-out commit ($current) != pinned ($commit), re-cloning"
    rm -rf "$dest"
  fi
  log "Cloning $name @ $tag ($commit)"
  git init --quiet "$dest"
  git -C "$dest" remote add origin "$url"
  git -C "$dest" fetch --quiet --depth 1 origin "$commit"
  git -C "$dest" checkout --quiet --detach "$commit"
  [[ "$(git -C "$dest" rev-parse HEAD)" == "$commit" ]] || die "$name checkout did not reach pinned commit"
}

# ---------------------------------------------------------------------------
# Step: brew leaf deps
# ---------------------------------------------------------------------------
step_brew() {
  log "Installing brew leaf deps: ${BREW_LEAF_DEPS[*]}"
  for pkg in "${BREW_LEAF_DEPS[@]}"; do
    if brew list --formula --versions "$pkg" >/dev/null 2>&1; then
      echo "  - $pkg already installed ($(brew list --formula --versions "$pkg"))"
    else
      brew install "$pkg"
    fi
  done
}

# ---------------------------------------------------------------------------
# Step: ffmpeg
# ---------------------------------------------------------------------------
step_ffmpeg() {
  if is_done ffmpeg "$FFMPEG_REF"; then
    log "ffmpeg $FFMPEG_TAG already built, skipping"
    return
  fi

  clone_pinned https://github.com/FFmpeg/FFmpeg.git "$FFMPEG_TAG" "$FFMPEG_COMMIT" ffmpeg

  local build="$BUILD_DIR/ffmpeg"
  rm -rf "$build"
  mkdir -p "$build"

  log "Configuring ffmpeg $FFMPEG_TAG"
  (
    cd "$build"
    "$SRC_DIR/ffmpeg/configure" \
      --prefix="$PREFIX" \
      --arch=arm64 \
      --target-os=darwin \
      --enable-cross-compile \
      --sysroot="$SDKROOT" \
      --cc="$CC" \
      --cxx="$CXX" \
      --enable-shared \
      --disable-static \
      --enable-pic \
      --enable-gpl \
      --enable-version3 \
      --disable-nonfree \
      --disable-programs \
      --disable-doc \
      --disable-debug \
      --disable-avdevice \
      --disable-sdl2 \
      --disable-libxcb \
      --disable-xlib \
      --disable-lzma \
      --enable-videotoolbox \
      --enable-audiotoolbox \
      --enable-securetransport \
      --enable-libdav1d \
      --enable-libsoxr \
      --extra-cflags="$CFLAGS $CPPFLAGS" \
      --extra-cxxflags="$CXXFLAGS $CPPFLAGS" \
      --extra-ldflags="$LDFLAGS" \
      --pkg-config=pkg-config \
      2>&1 | tee "$LOG_DIR/ffmpeg-configure.log"
  )

  python3 "$ROOT_DIR/scripts/sanitize-build-config.py" "$build/config.h" FFMPEG_CONFIGURATION "$ROOT_DIR"

  log "Building ffmpeg $FFMPEG_TAG (-j$JOBS)"
  make -C "$build" -j"$JOBS" 2>&1 | tee "$LOG_DIR/ffmpeg-build.log"

  log "Installing ffmpeg $FFMPEG_TAG"
  make -C "$build" install 2>&1 | tee "$LOG_DIR/ffmpeg-install.log"

  mark_done ffmpeg "$FFMPEG_REF"
}

# ---------------------------------------------------------------------------
# Step: libplacebo
# ---------------------------------------------------------------------------
step_libplacebo() {
  if is_done libplacebo "$LIBPLACEBO_REF"; then
    log "libplacebo $LIBPLACEBO_TAG already built, skipping"
    return
  fi

  clone_pinned https://github.com/haasn/libplacebo.git "$LIBPLACEBO_TAG" "$LIBPLACEBO_COMMIT" libplacebo
  # libplacebo uses git submodules for a couple of bundled dependencies
  # (e.g. fast_float, glad) used by the demo programs / tests, which we
  # disable — but `3rdparty/` for glad is still referenced by meson.build
  # unconditionally in some versions, so sync submodules defensively.
  git -C "$SRC_DIR/libplacebo" submodule update --init --depth 1 2>&1 | tee "$LOG_DIR/libplacebo-submodules.log" || true

  local build="$BUILD_DIR/libplacebo"
  rm -rf "$build"

  log "Configuring libplacebo $LIBPLACEBO_TAG (OpenGL GPU backend, Vulkan disabled)"
  meson setup "$build" "$SRC_DIR/libplacebo" \
    "${MESON_CROSS_ARGS[@]}" \
    -Dvulkan=disabled \
    -Dvk-proc-addr=disabled \
    -Dopengl=enabled \
    -Dgl-proc-addr=enabled \
    -Dd3d11=disabled \
    -Dglslang=disabled \
    -Dshaderc=disabled \
    -Dlcms=enabled \
    -Ddovi=enabled \
    -Dlibdovi=disabled \
    -Ddemos=false \
    -Dtests=false \
    -Dbench=false \
    -Dunwind=disabled \
    2>&1 | tee "$LOG_DIR/libplacebo-configure.log"

  log "Building libplacebo $LIBPLACEBO_TAG (-j$JOBS)"
  ninja -C "$build" -j"$JOBS" 2>&1 | tee "$LOG_DIR/libplacebo-build.log"

  log "Installing libplacebo $LIBPLACEBO_TAG"
  ninja -C "$build" install 2>&1 | tee "$LOG_DIR/libplacebo-install.log"

  mark_done libplacebo "$LIBPLACEBO_REF"
}

# ---------------------------------------------------------------------------
# Step: libass
# ---------------------------------------------------------------------------
step_libass() {
  if is_done libass "$LIBASS_REF"; then
    log "libass $LIBASS_TAG already built, skipping"
    return
  fi

  clone_pinned https://github.com/libass/libass.git "$LIBASS_TAG" "$LIBASS_COMMIT" libass

  local build="$BUILD_DIR/libass"
  rm -rf "$build"

  log "Configuring libass $LIBASS_TAG (Core Text font provider, fontconfig disabled)"
  # libass's own meson.build defaults default_library to 'static' — override
  # it, since we want a dylib like everything else in vendor/prefix.
  meson setup "$build" "$SRC_DIR/libass" \
    "${MESON_CROSS_ARGS[@]}" \
    -Ddefault_library=shared \
    -Dfontconfig=disabled \
    -Dcoretext=enabled \
    -Dasm=enabled \
    -Dlibunibreak=enabled \
    -Dtest=disabled \
    -Dcompare=disabled \
    -Dprofile=disabled \
    -Dfuzz=disabled \
    2>&1 | tee "$LOG_DIR/libass-configure.log"

  log "Building libass $LIBASS_TAG (-j$JOBS)"
  ninja -C "$build" -j"$JOBS" 2>&1 | tee "$LOG_DIR/libass-build.log"

  log "Installing libass $LIBASS_TAG"
  ninja -C "$build" install 2>&1 | tee "$LOG_DIR/libass-install.log"

  mark_done libass "$LIBASS_REF"
}

# ---------------------------------------------------------------------------
# Step: mpv (libmpv)
# ---------------------------------------------------------------------------
step_mpv() {
  if is_done mpv "$MPV_REF"; then
    log "mpv $MPV_TAG already built, skipping"
    return
  fi

  clone_pinned https://github.com/mpv-player/mpv.git "$MPV_TAG" "$MPV_COMMIT" mpv

  local build="$BUILD_DIR/mpv"
  rm -rf "$build"

  log "Configuring mpv $MPV_TAG (-Dlibmpv=true)"
  meson setup "$build" "$SRC_DIR/mpv" \
    "${MESON_CROSS_ARGS[@]}" \
    --sysconfdir=/etc \
    "-Dswift-flags=-Xcc -ffile-prefix-map=$ROOT_DIR=/jellybeam -Xcc -ffile-prefix-map=$HOME=/home" \
    -Dlibmpv=true \
    -Dcplayer=true \
    -Dgpl=true \
    -Dtests=false \
    \
    -Dcocoa=enabled \
    -Dcoreaudio=enabled \
    -Davfoundation=enabled \
    -Dgl=enabled \
    -Dgl-cocoa=enabled \
    -Dplain-gl=enabled \
    -Dmacos-cocoa-cb=enabled \
    \
    -Dvulkan=disabled \
    -Dshaderc=disabled \
    -Dspirv-cross=disabled \
    -Dgl-x11=disabled \
    -Dx11=disabled \
    -Dwayland=disabled \
    -Dvdpau=disabled \
    -Dvdpau-gl-x11=disabled \
    -Dvaapi=disabled \
    -Ddrm=disabled \
    -Degl=disabled \
    -Degl-x11=disabled \
    -Degl-wayland=disabled \
    -Degl-drm=disabled \
    -Dgbm=disabled \
    -Dsixel=disabled \
    -Dcaca=disabled \
    -Dd3d11=disabled \
    -Ddirect3d=disabled \
    \
    -Dlua=disabled \
    -Djavascript=disabled \
    \
    -Dcdda=disabled \
    -Ddvdnav=disabled \
    -Dlibbluray=disabled \
    -Dlibavdevice=disabled \
    \
    -Djack=disabled \
    -Dpulse=disabled \
    -Dpipewire=disabled \
    -Dsndio=disabled \
    -Dalsa=disabled \
    -Doss-audio=disabled \
    \
    -Duchardet=enabled \
    -Dlcms2=enabled \
    -Dzlib=enabled \
    -Diconv=enabled \
    \
    -Drubberband=disabled \
    -Dzimg=disabled \
    -Djpeg=disabled \
    -Dlibarchive=disabled \
    -Dvapoursynth=disabled \
    -Dsdl2-video=disabled \
    -Dsdl2-audio=disabled \
    -Dsdl2-gamepad=disabled \
    \
    2>&1 | tee "$LOG_DIR/mpv-configure.log"

  python3 "$ROOT_DIR/scripts/sanitize-build-config.py" "$build/config.h" CONFIGURATION "$ROOT_DIR"

  log "Building mpv $MPV_TAG (-j$JOBS)"
  ninja -C "$build" -j"$JOBS" 2>&1 | tee "$LOG_DIR/mpv-build.log"

  log "Installing mpv $MPV_TAG"
  ninja -C "$build" install 2>&1 | tee "$LOG_DIR/mpv-install.log"

  mark_done mpv "$MPV_REF"
}

# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------
cmd="${1:-all}"

case "$cmd" in
  clean)
    log "Removing $BUILD_DIR and $PREFIX"
    rm -rf "$BUILD_DIR" "$PREFIX"
    ;;
  brew) step_brew ;;
  ffmpeg) step_brew; step_ffmpeg ;;
  libplacebo) step_brew; step_libplacebo ;;
  libass) step_brew; step_libass ;;
  mpv) step_brew; step_mpv ;;
  all)
    step_brew
    step_ffmpeg
    step_libplacebo
    step_libass
    step_mpv
    log "Done. vendor/prefix contents:"
    find "$PREFIX/lib" -maxdepth 1 -name '*.dylib' | sort
    echo
    log "pkg-config modversion mpv: $(PKG_CONFIG_PATH="$PREFIX/lib/pkgconfig" pkg-config --modversion mpv 2>&1 || echo 'FAILED')"
    ;;
  *)
    die "unknown command: $cmd (expected: all|clean|brew|ffmpeg|libplacebo|libass|mpv)"
    ;;
esac
