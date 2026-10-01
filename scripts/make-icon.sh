#!/usr/bin/env bash
#
# make-icon.sh — builds Resources/AppIcon.icns from the Jellybeam brand icon
# master (crates/app/assets/brand/jellybeam/icon-512.png, opaque 512x512). The master
# is first composed into the macOS icon shape (scripts/render-icon.swift:
# 1024 canvas, artwork clipped to the centred 824px rounded square), then
# `sips` scales every .iconset slot from that 1024 render and `iconutil`
# packs them. The master's 512px source is the resolution ceiling
# (docs/DESIGN-GUIDE.md §A.7).
#
# Usage: scripts/make-icon.sh <output.icns>
#
# Called by scripts/bundle-app.sh; also runnable standalone.

set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUT_ICNS="${1:?usage: make-icon.sh <output.icns>}"

log() { printf '\n\033[1;34m==> %s\033[0m\n' "$*"; }
die() { printf '\033[1;31mFATAL: %s\033[0m\n' "$*" >&2; exit 1; }

command -v sips >/dev/null 2>&1 || die "sips not found"
command -v iconutil >/dev/null 2>&1 || die "iconutil not found"
command -v swift >/dev/null 2>&1 || die "swift not found (Xcode Command Line Tools)"

ICON_MASTER="$ROOT_DIR/crates/app/assets/brand/jellybeam/icon-512.png"
[ -f "$ICON_MASTER" ] || die "brand icon master missing: $ICON_MASTER"

WORK_DIR="$(mktemp -d "${TMPDIR:-/tmp}/jellybeam-icon.XXXXXX")"
trap 'rm -rf "$WORK_DIR"' EXIT

ICONSET_DIR="$WORK_DIR/AppIcon.iconset"
mkdir -p "$ICONSET_DIR"

SHAPED="$WORK_DIR/icon-1024.png"
log "Composing the master into the macOS icon shape (render-icon.swift)"
swift "$ROOT_DIR/scripts/render-icon.swift" "$ICON_MASTER" "$SHAPED"
[ -f "$SHAPED" ] || die "render-icon.swift did not produce $SHAPED"

log "Rendering .iconset sizes from the 1024x1024 shaped icon (sips)"
BASE_SIZES=(16 32 128 256 512)
for sz in "${BASE_SIZES[@]}"; do
  sips -z "$sz" "$sz" "$SHAPED" --out "$ICONSET_DIR/icon_${sz}x${sz}.png" >/dev/null
  sz2=$((sz * 2))
  sips -z "$sz2" "$sz2" "$SHAPED" --out "$ICONSET_DIR/icon_${sz}x${sz}@2x.png" >/dev/null
done

log "Packing .iconset -> $OUT_ICNS"
mkdir -p "$(dirname "$OUT_ICNS")"
iconutil -c icns "$ICONSET_DIR" -o "$OUT_ICNS"

[ -f "$OUT_ICNS" ] || die "iconutil did not produce $OUT_ICNS"
log "Wrote $OUT_ICNS ($(du -h "$OUT_ICNS" | cut -f1))"
