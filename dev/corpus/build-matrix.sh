#!/usr/bin/env bash
# dev/corpus/build-matrix.sh
#
# Runs *inside* the linuxserver/ffmpeg container (see gen-corpus.sh, which
# bind-mounts this file to /scripts/build-matrix.sh and dev/media to
# /media). Generates the full synthetic corpus with ffmpeg's testsrc2/sine
# generators — no host ffmpeg install required.
#
# Not meant to be run directly on the host.

set -euo pipefail

MEDIA=/media
WORK=/tmp/corpus-work
mkdir -p "$MEDIA/Movies" "$MEDIA/Shows" "$WORK"

FF="ffmpeg -y -hide_banner -loglevel error -nostdin"

# ---------------------------------------------------------------------------
# Codec parameter tables
# ---------------------------------------------------------------------------

# Populates global array VA with ffmpeg video-encode args for a given key.
video_args() {
  case "$1" in
    h264)   VA=(-c:v libx264 -preset ultrafast -crf 28 -pix_fmt yuv420p) ;;
    hevc8)  VA=(-c:v libx265 -preset ultrafast -crf 28 -pix_fmt yuv420p -tag:v hvc1) ;;
    hevc10) VA=(-c:v libx265 -preset ultrafast -crf 28 -pix_fmt yuv420p10le -tag:v hvc1) ;;
    av1)    VA=(-c:v libsvtav1 -preset 12 -crf 40 -pix_fmt yuv420p) ;;
    vp9)    VA=(-c:v libvpx-vp9 -deadline realtime -cpu-used 8 -crf 40 -b:v 0 -pix_fmt yuv420p) ;;
    *) echo "unknown video key: $1" >&2; exit 1 ;;
  esac
}

# Populates global array AA with ffmpeg audio-encode args for a given key.
audio_args() {
  case "$1" in
    aac)  AA=(-c:a aac -b:a 96k) ;;
    ac3)  AA=(-c:a ac3 -b:a 192k) ;;
    eac3) AA=(-c:a eac3 -b:a 192k) ;;
    flac) AA=(-c:a flac) ;;
    opus) AA=(-c:a libopus -b:a 96k) ;;
    *) echo "unknown audio key: $1" >&2; exit 1 ;;
  esac
}

skip_if_exists() {
  if [[ -f "$1" ]]; then
    echo "  skip (exists): $1"
    return 0
  fi
  return 1
}

# ---------------------------------------------------------------------------
# 1. Sparse video x audio matrix (25 files): every {h264,hevc8,hevc10,av1,vp9}
#    x {aac,ac3,eac3,flac,opus} pair appears exactly once (full 5x5 video x
#    audio coverage), cycled across {mkv,mp4,ts} containers so every
#    container is exercised repeatedly without doing the full 3-way cross
#    product (5x5x3=75).
#
#    Files are prefixed with a zero-padded index (01-, 02-, ...). This is
#    NOT cosmetic: Jellyfin's built-in local filename parser recognizes
#    bare "aac"/"ac3" tokens as scene-release audio-codec tags and strips
#    them when deriving the movie title, so e.g. "h264-aac.mkv" and
#    "h264-ac3.mp4" would otherwise both be indexed with the identical
#    title "h264" (verified against a live server — see dev/README.md).
#    The index prefix guarantees every item gets a distinct title so each
#    is unambiguously selectable/identifiable in Jellyfin regardless of
#    which tokens its parser happens to strip.
# ---------------------------------------------------------------------------

VIDEOS=(h264 hevc8 hevc10 av1 vp9)
AUDIOS=(aac ac3 eac3 flac opus)
CONTAINERS=(mkv mp4 ts)

echo "== base video x audio matrix =="
i=0
for v in "${VIDEOS[@]}"; do
  for a in "${AUDIOS[@]}"; do
    c="${CONTAINERS[$((i % 3))]}"
    idx=$(printf '%02d' $((i + 1)))
    out="$MEDIA/Movies/${idx}-${v}-${a}.${c}"
    i=$((i + 1))
    skip_if_exists "$out" && continue
    video_args "$v"; audio_args "$a"
    echo "  -> $out"
    $FF -f lavfi -i "testsrc2=size=640x360:rate=24:duration=5" \
        -f lavfi -i "sine=frequency=440:duration=5" \
        "${VA[@]}" "${AA[@]}" -shortest "$out"
  done
done

# ---------------------------------------------------------------------------
# 2. Special-case files
# ---------------------------------------------------------------------------

echo "== special files =="

# --- HDR10: hevc 10-bit with static HDR10 mastering-display / MaxCLL SEI ---
out="$MEDIA/Movies/26-hevc10-hdr10-ac3.mkv"
if ! skip_if_exists "$out"; then
  echo "  -> $out (HDR10 static metadata)"
  # Note: ffmpeg's generic -color_primaries/-color_trc/-colorspace output
  # options don't reliably reach libx265's VUI in this ffmpeg build (only
  # matrix coefficients came through in testing) — so we set the
  # mastering-display/MaxCLL SEI via -x265-params, and then force the VUI
  # colour flags explicitly with the hevc_metadata bitstream filter so
  # color_primaries/color_transfer/color_space all read correctly (verified
  # with ffprobe: bt2020/smpte2084/bt2020nc).
  $FF -f lavfi -i "testsrc2=size=640x360:rate=24:duration=5" \
      -f lavfi -i "sine=frequency=440:duration=5" \
      -c:v libx265 -preset ultrafast -crf 28 -pix_fmt yuv420p10le \
      -x265-params "hdr10=1:hdr10-opt=1:repeat-headers=1:master-display=G(13250,34500)B(7500,3000)R(34000,16000)WP(15635,16450)L(10000000,1):max-cll=1000,400" \
      -bsf:v "hevc_metadata=colour_primaries=9:transfer_characteristics=16:matrix_coefficients=9:video_full_range_flag=0" \
      -c:a ac3 -b:a 192k -shortest "$out"
fi

# --- HDR10 brightness ladder: six neutral bars at known PQ luminance ---
# Left to right 100, 203, 400, 1000, 2000, 4000 nits (10-bit limited-range
# luma 509, 573, 636, 723, 789, 855; chroma neutral). With EDR engaged the
# 203-nit bar matches the app's own white UI and every bar to its right is
# visibly brighter, up to the display's headroom; tone-mapped to SDR, the
# right-hand bars converge on white.
out="$MEDIA/Movies/34-hevc10-hdr10-nits-ladder-aac.mkv"
if ! skip_if_exists "$out"; then
  echo "  -> $out (HDR10 luminance ladder)"
  $FF -f lavfi -i "nullsrc=size=1920x1080:rate=24:duration=20" \
      -f lavfi -i "anullsrc=channel_layout=stereo:sample_rate=48000" \
      -vf "format=yuv420p10le,geq=lum='st(0,floor(X/320));if(eq(ld(0),0),509,if(eq(ld(0),1),573,if(eq(ld(0),2),636,if(eq(ld(0),3),723,if(eq(ld(0),4),789,855)))))':cb=512:cr=512" \
      -c:v libx265 -preset ultrafast -crf 12 -pix_fmt yuv420p10le \
      -x265-params "hdr10=1:hdr10-opt=1:repeat-headers=1:colorprim=bt2020:transfer=smpte2084:colormatrix=bt2020nc:master-display=G(13250,34500)B(7500,3000)R(34000,16000)WP(15635,16450)L(40000000,1):max-cll=4000,1500" \
      -bsf:v "hevc_metadata=colour_primaries=9:transfer_characteristics=16:matrix_coefficients=9:video_full_range_flag=0" \
      -c:a aac -b:a 64k -shortest "$out"
fi

# --- Embedded styled ASS subtitles ---
out="$MEDIA/Movies/27-h264-ass-subs-aac.mkv"
if ! skip_if_exists "$out"; then
  echo "  -> $out (embedded styled ASS subs)"
  cat > "$WORK/subs.ass" <<'ASS'
[Script Info]
Title: Jellybeam test subs
ScriptType: v4.00+
PlayResX: 640
PlayResY: 360

[V4+ Styles]
Format: Name, Fontname, Fontsize, PrimaryColour, SecondaryColour, OutlineColour, BackColour, Bold, Italic, Underline, StrikeOut, ScaleX, ScaleY, Spacing, Angle, BorderStyle, Outline, Shadow, Alignment, MarginL, MarginR, MarginV, Encoding
Style: Jellybeam,DejaVu Sans,28,&H0000D7FF,&H000000FF,&H00202020,&H80000000,1,0,0,0,100,100,0,0,1,2,1,2,20,20,20,1

[Events]
Format: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text
Dialogue: 0,0:00:00.50,0:00:02.50,Jellybeam,,0,0,0,,Styled caption line one
Dialogue: 0,0:00:03.00,0:00:05.50,Jellybeam,,0,0,0,,{\b1}Bold{\b0} styled caption line two
ASS
  $FF -f lavfi -i "testsrc2=size=640x360:rate=24:duration=6" \
      -f lavfi -i "sine=frequency=440:duration=6" \
      -i "$WORK/subs.ass" \
      -map 0:v -map 1:a -map 2:s \
      -c:v libx264 -preset ultrafast -crf 28 -pix_fmt yuv420p \
      -c:a aac -b:a 96k -c:s ass \
      -shortest "$out"
fi

# --- External .srt sidecar (same basename as the video, Jellyfin auto-pairs it) ---
out="$MEDIA/Movies/28-h264-srt-sidecar-aac.mkv"
srt="$MEDIA/Movies/28-h264-srt-sidecar-aac.srt"
if ! skip_if_exists "$out"; then
  echo "  -> $out (+ external .srt sidecar)"
  video_args h264; audio_args aac
  $FF -f lavfi -i "testsrc2=size=640x360:rate=24:duration=5" \
      -f lavfi -i "sine=frequency=440:duration=5" \
      "${VA[@]}" "${AA[@]}" -shortest "$out"
fi
if [[ ! -f "$srt" ]]; then
  cat > "$srt" <<'SRT'
1
00:00:00,500 --> 00:00:02,500
External sidecar subtitle line one

2
00:00:03,000 --> 00:00:05,000
External sidecar subtitle line two
SRT
fi

# --- Chapters (ffmetadata) ---
out="$MEDIA/Movies/29-h264-chapters-aac.mkv"
if ! skip_if_exists "$out"; then
  echo "  -> $out (chapters)"
  cat > "$WORK/chapters.txt" <<'META'
;FFMETADATA1
[CHAPTER]
TIMEBASE=1/1000
START=0
END=3000
title=Chapter One

[CHAPTER]
TIMEBASE=1/1000
START=3000
END=6000
title=Chapter Two

[CHAPTER]
TIMEBASE=1/1000
START=6000
END=9000
title=Chapter Three
META
  $FF -f lavfi -i "testsrc2=size=640x360:rate=24:duration=9" \
      -f lavfi -i "sine=frequency=440:duration=9" \
      -i "$WORK/chapters.txt" \
      -map_metadata 2 -map 0:v -map 1:a \
      -c:v libx264 -preset ultrafast -crf 28 -pix_fmt yuv420p -c:a aac -b:a 96k \
      -shortest "$out"
fi

# --- Two audio tracks (English AAC + Spanish AC-3) ---
out="$MEDIA/Movies/30-h264-dual-audio.mkv"
if ! skip_if_exists "$out"; then
  echo "  -> $out (dual audio tracks)"
  $FF -f lavfi -i "testsrc2=size=640x360:rate=24:duration=5" \
      -f lavfi -i "sine=frequency=440:duration=5" \
      -f lavfi -i "sine=frequency=880:duration=5" \
      -map 0:v -map 1:a -map 2:a \
      -c:v libx264 -preset ultrafast -crf 28 -pix_fmt yuv420p \
      -c:a:0 aac -b:a:0 96k -metadata:s:a:0 language=eng -metadata:s:a:0 title="English" \
      -c:a:1 ac3 -b:a:1 192k -metadata:s:a:1 language=spa -metadata:s:a:1 title="Spanish" \
      -shortest "$out"
fi

# --- 5.1 surround AC-3 ---
out="$MEDIA/Movies/31-h264-ac3-5.1.mkv"
if ! skip_if_exists "$out"; then
  echo "  -> $out (5.1 AC-3)"
  $FF -f lavfi -i "testsrc2=size=640x360:rate=24:duration=5" \
      -f lavfi -i "sine=frequency=440:duration=5" \
      -filter_complex "[1:a]pan=5.1|FL=c0|FR=c0|FC=c0|LFE=0.5*c0|BL=0.3*c0|BR=0.3*c0[a51]" \
      -map 0:v -map "[a51]" \
      -c:v libx264 -preset ultrafast -crf 28 -pix_fmt yuv420p \
      -c:a ac3 -b:a 448k \
      -shortest "$out"
fi

# --- 23.976fps variant ---
out="$MEDIA/Movies/32-h264-23.976fps-aac.mkv"
if ! skip_if_exists "$out"; then
  echo "  -> $out (23.976fps)"
  $FF -f lavfi -i "testsrc2=size=640x360:rate=24000/1001:duration=5" \
      -f lavfi -i "sine=frequency=440:duration=5" \
      -c:v libx264 -preset ultrafast -crf 28 -pix_fmt yuv420p -r 24000/1001 \
      -c:a aac -b:a 96k -shortest "$out"
fi

# --- 50fps variant ---
out="$MEDIA/Movies/33-h264-50fps-aac.mkv"
if ! skip_if_exists "$out"; then
  echo "  -> $out (50fps)"
  $FF -f lavfi -i "testsrc2=size=640x360:rate=50:duration=5" \
      -f lavfi -i "sine=frequency=440:duration=5" \
      -c:v libx264 -preset ultrafast -crf 28 -pix_fmt yuv420p -r 50 \
      -c:a aac -b:a 96k -shortest "$out"
fi

# ---------------------------------------------------------------------------
# 3. TV shows: two fake series so Jellyfin's Shows library has real
#    Series/Season/Episode structure to index and group.
# ---------------------------------------------------------------------------

echo "== shows =="

gen_episode() {
  local show="$1" season="$2" ep="$3" vkey="$4" akey="$5"
  local dir="$MEDIA/Shows/${show}/Season ${season}"
  local out="${dir}/${show} S${season}E${ep}.mkv"
  mkdir -p "$dir"
  skip_if_exists "$out" && return
  echo "  -> $out"
  video_args "$vkey"; audio_args "$akey"
  $FF -f lavfi -i "testsrc2=size=640x360:rate=24:duration=5" \
      -f lavfi -i "sine=frequency=440:duration=5" \
      "${VA[@]}" "${AA[@]}" -shortest "$out"
}

gen_episode "Quantum Static" 01 01 h264 aac
gen_episode "Quantum Static" 01 02 h264 ac3
gen_episode "Quantum Static" 02 01 hevc8 aac
gen_episode "Glass Horizon" 01 01 h264 eac3

echo "== done =="
find "$MEDIA/Movies" "$MEDIA/Shows" -type f | sort
