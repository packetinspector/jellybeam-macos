#!/usr/bin/env bash
#
# build-vendor.sh — build libmpv (with -Dlibmpv=true), its core chain
# (ffmpeg, libplacebo, libass) and every library they link (dav1d, soxr,
# freetype, harfbuzz, fribidi, libunibreak, uchardet, lcms2, libpng) from
# source, vendored into vendor/prefix for embedding in Jellybeam.app.
#
# Everything is compiled for one deployment target (MACOSX_DEPLOYMENT_TARGET
# below), so the bundle runs on every macOS it claims. Homebrew supplies
# build tools only: its bottles are compiled for the host's macOS and would
# raise the bundle's real minimum to whatever the build machine runs.
#
# Usage:
#   scripts/build-vendor.sh              # build everything (idempotent)
#   scripts/build-vendor.sh clean        # remove vendor/build and vendor/prefix
#   scripts/build-vendor.sh <component>  # build/rebuild just one of:
#                                          deps ffmpeg libplacebo libass mpv
#
# Re-running is a no-op for components that already built successfully at
# the pinned version (tracked via marker files in vendor/build/.markers).
# Bump a *_TAG/*_COMMIT pair below and re-run to rebuild just that component
# (and anything after it, if you also clear its marker).
#
# Requires (see docs/BUILD.md): Xcode CLT, meson, ninja, nasm, cmake, automake,
# autoconf, pkg-config, python3, curl.

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
BUILD_RECIPE_VERSION=4
FFMPEG_REF="$FFMPEG_TAG@$FFMPEG_COMMIT:recipe-$BUILD_RECIPE_VERSION"
LIBPLACEBO_REF="$LIBPLACEBO_TAG@$LIBPLACEBO_COMMIT:recipe-$BUILD_RECIPE_VERSION"
LIBASS_REF="$LIBASS_TAG@$LIBASS_COMMIT:recipe-$BUILD_RECIPE_VERSION"
MPV_REF="$MPV_TAG@$MPV_COMMIT:recipe-$BUILD_RECIPE_VERSION"

# The oldest macOS every bundled binary supports. Must match
# LSMinimumSystemVersion in scripts/Info.plist.in; bundle-app.sh checks every
# Mach-O in the bundle against it. 11.0 is the first macOS on Apple Silicon.
DEPLOYMENT_TARGET="11.0"

# Libraries the core chain links, built from source at DEPLOYMENT_TARGET.
# name|version|url|sha256 (the same releases Homebrew pins). zlib, bzip2 and
# iconv come from macOS itself. No Vulkan/MoltenVK/shaderc/spirv-cross:
# libplacebo uses the OpenGL backend only (docs/BUILD.md "libplacebo: OpenGL
# vs Vulkan").
LEAF_DEPS=(
  "libpng|1.6.58|https://downloads.sourceforge.net/project/libpng/libpng16/1.6.58/libpng-1.6.58.tar.xz|28eb403f51f0f7405249132cecfe82ea5c0ef97f1b32c5a65828814ae0d34775"
  "freetype|2.14.3|https://downloads.sourceforge.net/project/freetype/freetype2/2.14.3/freetype-2.14.3.tar.xz|36bc4f1cc413335368ee656c42afca65c5a3987e8768cc28cf11ba775e785a5f"
  "harfbuzz|14.5.0|https://github.com/harfbuzz/harfbuzz/releases/download/14.5.0/harfbuzz-14.5.0.tar.xz|b7132e148358a45185c9feafd049dbaf243649d3c44414b3534d9c95d18592b9"
  "fribidi|1.0.17|https://github.com/fribidi/fribidi/releases/download/v1.0.17/fribidi-1.0.17.tar.xz|6949dcde27d41cebad1fd741fcafc36d55a1020d2d872d4a6eb3914caabbada2"
  "libunibreak|8.0|https://github.com/adah1972/libunibreak/releases/download/libunibreak_8_0/libunibreak-8.0.tar.gz|9c4fad6e517338a098373acc9f35579ae2c325e6446666fb9ac2666ba15ceba4"
  "uchardet|0.0.8|https://www.freedesktop.org/software/uchardet/releases/uchardet-0.0.8.tar.xz|e97a60cfc00a1c147a674b097bb1422abd9fa78a2d9ce3f3fdcc2e78a34ac5f0"
  "lcms2|2.19.1|https://downloads.sourceforge.net/project/lcms/lcms/2.19.1/lcms2-2.19.1.tar.gz|bfc54f7bab59fbc921012014a8032e4cba4abd46db47d46b76416a8c0b2815c8"
  "dav1d|1.5.4|https://code.videolan.org/videolan/dav1d/-/archive/1.5.4/dav1d-1.5.4.tar.bz2|2abfb0c89212e6e4733a54e0ae509ec00a5b845a6360946f918806e14aedb011"
  "soxr|0.1.3|https://downloads.sourceforge.net/project/soxr/soxr-0.1.3-Source.tar.xz|b111c15fdc8c029989330ff559184198c161100a59312f5dc19ddeb9b5a15889"
)

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
export MACOSX_DEPLOYMENT_TARGET="$DEPLOYMENT_TARGET"
export CMAKE_OSX_DEPLOYMENT_TARGET="$DEPLOYMENT_TARGET"
export CC="$(xcrun -f clang)"
export CXX="$(xcrun -f clang++)"

# Only our own prefix is visible to pkg-config, so nothing can silently link
# a Homebrew library compiled for the host's macOS.
export PKG_CONFIG_PATH="$PREFIX/lib/pkgconfig"
export PKG_CONFIG_LIBDIR="$PREFIX/lib/pkgconfig"

TARGET_FLAGS="-arch arm64 -mmacosx-version-min=$DEPLOYMENT_TARGET"
export CPPFLAGS="-I$PREFIX/include ${CPPFLAGS:-}"
export CFLAGS="$TARGET_FLAGS -O2 -ffile-prefix-map=$ROOT_DIR=/jellybeam -ffile-prefix-map=$HOME=/home ${CFLAGS:-}"
export CXXFLAGS="$TARGET_FLAGS -O2 -ffile-prefix-map=$ROOT_DIR=/jellybeam -ffile-prefix-map=$HOME=/home ${CXXFLAGS:-}"
export OBJCFLAGS="$CFLAGS ${OBJCFLAGS:-}"
export LDFLAGS="$TARGET_FLAGS -L$PREFIX/lib ${LDFLAGS:-}"

MESON_CROSS_ARGS=(--prefix "$PREFIX" --libdir lib --buildtype release -Ddefault_library=shared)

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

CMAKE_ARGS=(
  -DCMAKE_INSTALL_PREFIX="$PREFIX"
  -DCMAKE_INSTALL_NAME_DIR="$PREFIX/lib"
  -DCMAKE_BUILD_TYPE=Release
  -DCMAKE_OSX_ARCHITECTURES=arm64
  -DCMAKE_OSX_DEPLOYMENT_TARGET="$DEPLOYMENT_TARGET"
  -DBUILD_SHARED_LIBS=ON
  -DCMAKE_POLICY_VERSION_MINIMUM=3.5
)

# ---------------------------------------------------------------------------
# Step: leaf libraries
# ---------------------------------------------------------------------------
fetch_tarball() {
  # $1 = name, $2 = version, $3 = url, $4 = sha256 -> prints the source dir
  local name="$1" version="$2" url="$3" sha="$4"
  local archive="$SRC_DIR/${url##*/}" dest="$SRC_DIR/$name-$version"
  if [[ ! -f "$archive" ]] || [[ "$(shasum -a 256 "$archive" | cut -d' ' -f1)" != "$sha" ]]; then
    log "Downloading $name $version" >&2
    curl --fail --location --silent --show-error "$url" -o "$archive.part"
    mv "$archive.part" "$archive"
  fi
  [[ "$(shasum -a 256 "$archive" | cut -d' ' -f1)" == "$sha" ]] || die "$name: checksum mismatch for $archive"
  rm -rf "$dest" && mkdir -p "$dest"
  tar -xf "$archive" -C "$dest" --strip-components 1
  echo "$dest"
}

build_leaf() {
  # $1 = one LEAF_DEPS entry
  local name version url sha
  IFS='|' read -r name version url sha <<< "$1"
  local ref="$version:$DEPLOYMENT_TARGET:recipe-$BUILD_RECIPE_VERSION"
  if is_done "$name" "$ref"; then
    log "$name $version already built, skipping"
    return
  fi
  local src build="$BUILD_DIR/$name"
  src="$(fetch_tarball "$name" "$version" "$url" "$sha")"
  rm -rf "$build"
  log "Building $name $version for macOS $DEPLOYMENT_TARGET"
  case "$name" in
    libpng)
      cmake -S "$src" -B "$build" "${CMAKE_ARGS[@]}" -DPNG_STATIC=OFF -DPNG_TESTS=OFF \
        -DPNG_TOOLS=OFF -DPNG_FRAMEWORK=OFF ;;
    freetype)
      # No harfbuzz here: harfbuzz is built against freetype, not the reverse.
      meson setup "$build" "$src" "${MESON_CROSS_ARGS[@]}" -Dharfbuzz=disabled \
        -Dpng=enabled -Dzlib=system -Dbzip2=disabled -Dbrotli=disabled ;;
    harfbuzz)
      meson setup "$build" "$src" "${MESON_CROSS_ARGS[@]}" -Dfreetype=enabled \
        -Dcoretext=enabled -Dglib=disabled -Dgobject=disabled -Dcairo=disabled \
        -Dchafa=disabled -Dicu=disabled -Dgraphite2=disabled -Dintrospection=disabled \
        -Dtests=disabled -Ddocs=disabled -Dutilities=disabled -Dbenchmark=disabled ;;
    fribidi)
      meson setup "$build" "$src" "${MESON_CROSS_ARGS[@]}" -Ddocs=false -Dbin=false -Dtests=false ;;
    libunibreak)
      (cd "$src" && ./configure --prefix="$PREFIX" --enable-shared --disable-static) ;;
    uchardet)
      cmake -S "$src" -B "$build" "${CMAKE_ARGS[@]}" -DBUILD_BINARY=OFF -DBUILD_STATIC=OFF ;;
    lcms2)
      (cd "$src" && ./configure --prefix="$PREFIX" --enable-shared --disable-static \
        --without-jpeg --without-tiff) ;;
    dav1d)
      meson setup "$build" "$src" "${MESON_CROSS_ARGS[@]}" -Denable_tools=false \
        -Denable_tests=false -Denable_examples=false ;;
    soxr)
      cmake -S "$src" -B "$build" "${CMAKE_ARGS[@]}" -DBUILD_TESTS=OFF -DWITH_OPENMP=OFF \
        -DWITH_LSR_BINDINGS=OFF -DBUILD_EXAMPLES=OFF ;;
    *) die "no recipe for $name" ;;
  esac 2>&1 | tee "$LOG_DIR/$name-configure.log"
  case "$name" in
    libunibreak|lcms2) make -C "$src" -j"$JOBS" install ;;
    libpng|uchardet|soxr) cmake --build "$build" -j "$JOBS" && cmake --install "$build" ;;
    *) ninja -C "$build" -j"$JOBS" install ;;
  esac 2>&1 | tee "$LOG_DIR/$name-build.log"
  mark_done "$name" "$ref"
}

step_deps() {
  local entry
  for entry in "${LEAF_DEPS[@]}"; do
    build_leaf "$entry"
  done
}

# Every dylib in the prefix must be stamped for DEPLOYMENT_TARGET or older.
step_verify() {
  log "Verifying every vendored dylib targets macOS $DEPLOYMENT_TARGET or older"
  local f minos bad=0
  for f in "$PREFIX"/lib/*.dylib; do
    [[ -L "$f" ]] && continue
    minos="$(vtool -show-build "$f" | awk '/minos/ {print $2; exit}')"
    if [[ "$(printf '%s\n%s\n' "$minos" "$DEPLOYMENT_TARGET" | sort -V | tail -1)" != "$DEPLOYMENT_TARGET" ]]; then
      warn "$(basename "$f") requires macOS $minos"
      bad=1
    fi
    if otool -L "$f" | grep -qE "/opt/homebrew|libswift"; then
      warn "$(basename "$f") links Homebrew or the Swift runtime"
      bad=1
    fi
  done
  [[ "$bad" -eq 0 ]] || die "vendored libraries do not all target macOS $DEPLOYMENT_TARGET"
  python3 "$ROOT_DIR/scripts/check-dynamic-symbols.py" "$PREFIX/lib" \
    || die "a vendored library references a symbol nothing provides (see above)"
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
  # mpv assumes Cocoa implies Swift: with Swift off, its clipboard backend,
  # media-key hooks and app-bridge calls would reference Swift-only functions
  # that link as null. The patch compiles those paths only in Swift builds;
  # Cocoa itself stays on for VideoToolbox's GL interop.
  local patch="$ROOT_DIR/scripts/patches/mpv-cocoa-without-swift.patch"
  if ! git -C "$SRC_DIR/mpv" apply --reverse --check "$patch" 2>/dev/null; then
    git -C "$SRC_DIR/mpv" apply "$patch" || die "mpv: $patch does not apply"
  fi

  local build="$BUILD_DIR/mpv"
  rm -rf "$build"

  log "Configuring mpv $MPV_TAG (-Dlibmpv=true)"
  meson setup "$build" "$SRC_DIR/mpv" \
    "${MESON_CROSS_ARGS[@]}" \
    --sysconfdir=/etc \
    -Dlibmpv=true \
    -Dcplayer=false \
    -Dgpl=true \
    -Dtests=false \
    \
    -Dcocoa=enabled \
    -Dcoreaudio=enabled \
    -Davfoundation=enabled \
    -Dgl=enabled \
    -Dgl-cocoa=enabled \
    -Dplain-gl=enabled \
    -Dvideotoolbox-gl=enabled \
    \
    -Dswift-build=disabled \
    -Dmacos-cocoa-cb=disabled \
    -Dmacos-media-player=disabled \
    -Dmacos-touchbar=disabled \
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
  deps) step_deps ;;
  ffmpeg) step_deps; step_ffmpeg ;;
  libplacebo) step_deps; step_libplacebo ;;
  libass) step_deps; step_libass ;;
  mpv) step_deps; step_mpv ;;
  verify) step_verify ;;
  all)
    step_deps
    step_ffmpeg
    step_libplacebo
    step_libass
    step_mpv
    step_verify
    log "Done. vendor/prefix contents:"
    find "$PREFIX/lib" -maxdepth 1 -name '*.dylib' | sort
    echo
    log "pkg-config modversion mpv: $(PKG_CONFIG_PATH="$PREFIX/lib/pkgconfig" pkg-config --modversion mpv 2>&1 || echo 'FAILED')"
    ;;
  *)
    die "unknown command: $cmd (expected: all|clean|deps|ffmpeg|libplacebo|libass|mpv|verify)"
    ;;
esac
