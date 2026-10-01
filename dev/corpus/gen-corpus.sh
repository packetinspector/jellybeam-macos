#!/usr/bin/env bash
# dev/corpus/gen-corpus.sh
#
# Generates the synthetic media corpus into dev/media/ using a containerized
# ffmpeg (linuxserver/ffmpeg, which bundles jellyfin-ffmpeg with libx264,
# libx265, libsvtav1/libaom, libvpx-vp9, aac/ac3/eac3/flac/opus, and libass).
# The host stays clean — nothing is installed locally.
#
# Idempotent: re-running skips any file that already exists. Pass --force to
# wipe dev/media/ first and regenerate everything from scratch.
#
# Usage: dev/corpus/gen-corpus.sh [--force]

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
MEDIA_DIR="$REPO_ROOT/dev/media"
FFMPEG_IMAGE="linuxserver/ffmpeg:latest"

if [[ "${1:-}" == "--force" ]]; then
  echo "[gen-corpus] --force: wiping $MEDIA_DIR"
  rm -rf "$MEDIA_DIR"
fi

mkdir -p "$MEDIA_DIR/Movies" "$MEDIA_DIR/Shows"

echo "[gen-corpus] pulling $FFMPEG_IMAGE (no-op if already present)..."
docker pull -q "$FFMPEG_IMAGE" >/dev/null

echo "[gen-corpus] generating corpus into $MEDIA_DIR ..."
docker run --rm \
  -v "$MEDIA_DIR":/media \
  -v "$SCRIPT_DIR/build-matrix.sh":/scripts/build-matrix.sh:ro \
  --entrypoint bash \
  "$FFMPEG_IMAGE" \
  /scripts/build-matrix.sh

movie_count=$(find "$MEDIA_DIR/Movies" -type f \( -name '*.mkv' -o -name '*.mp4' -o -name '*.ts' \) | wc -l | tr -d ' ')
episode_count=$(find "$MEDIA_DIR/Shows" -type f -name '*.mkv' | wc -l | tr -d ' ')

echo "[gen-corpus] done. ${movie_count} movie files, ${episode_count} episode files."
