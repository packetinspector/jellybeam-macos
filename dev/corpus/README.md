# Synthetic media corpus

`dev/corpus/gen-corpus.sh` generates a small, deterministic library of synthetic
media into `dev/media/` for exercising Jellybeam (and Jellyfin itself) against
a realistic spread of containers, codecs, and metadata edge cases — without
needing any real video files or host-installed tools.

Everything runs through the `linuxserver/ffmpeg` image (which bundles
jellyfin-ffmpeg with `libx264`, `libx265`, `libsvtav1`/`libaom`,
`libvpx-vp9`, `aac`/`ac3`/`eac3`/`flac`/`libopus`, and `libass`). The host
machine never needs ffmpeg installed.

## Usage

```
dev/corpus/gen-corpus.sh          # generate anything missing (idempotent)
dev/corpus/gen-corpus.sh --force  # wipe dev/media/ and regenerate everything
```

or via `dev/server.sh corpus`.

All clips are tiny: 5-9 seconds, 640x360, `testsrc2` video pattern + `sine`
tone audio. This is a format/metadata compatibility corpus, not a visual
quality one.

## Layout

```
dev/media/
  Movies/    33 files, each a distinct "movie"
  Shows/     2 fake series, 4 episodes total
```

Jellyfin derives an item's title from its filename. Its built-in local
filename parser recognizes common scene-release tokens (`aac`, `ac3`,
`5.1`, ...) and strips them — and, we found empirically, once it hits a
leading numeric segment like `01-` it stops parsing there and drops
everything after it as release info. So two files like `h264-aac.mkv` and
`h264-ac3.mp4` end up both titled just `h264`, and `01-h264-aac.mkv` ends
up titled just `01`. Either way every file in the corpus gets a distinct
title because of the leading zero-padded index, even though the *readable*
part of the title is often short. Titles aren't meant to be pretty here —
just distinct and stable. If you need to eyeball what a file actually is,
read the filename in the Jellyfin path field, not the parsed title.

The dev server's `Movies`/`Shows` libraries also have remote metadata and
image fetchers disabled (see `dev/setup-server.sh`) specifically so
Jellyfin doesn't try to identify these codec-named files against TheMovieDB
and rename them to unrelated real movies (yes, this actually happens —
`av1-aac.mkv` got matched to a real film called "AV-1" the first time
around).

## Base matrix (25 files, `Movies/01-*.*` .. `Movies/25-*.*`)

Every `{h264, hevc-8bit, hevc-10bit, av1, vp9}` x
`{aac, ac3, eac3, flac, opus}` pair appears **exactly once** (full 5x5
video x audio coverage — not the full 3-way cross product with containers,
which would be 75 files). Container is cycled `mkv, mp4, ts, mkv, mp4, ...`
across the 25 combos so all three containers get exercised repeatedly
without inflating the file count.

| # | Video | Audio | Container |
|---|-------|-------|-----------|
| 01-05 | h264 | aac, ac3, eac3, flac, opus | mkv, mp4, ts, mkv, mp4 |
| 06-10 | hevc-8bit | aac, ac3, eac3, flac, opus | ts, mkv, mp4, ts, mkv |
| 11-15 | hevc-10bit | aac, ac3, eac3, flac, opus | mp4, ts, mkv, mp4, ts |
| 16-20 | av1 (libsvtav1) | aac, ac3, eac3, flac, opus | mkv, mp4, ts, mkv, mp4 |
| 21-25 | vp9 (libvpx-vp9) | aac, ac3, eac3, flac, opus | ts, mkv, mp4, ts, mkv |

## Special files (26-33)

| File | What it tests |
|---|---|
| `26-hevc10-hdr10-ac3.mkv` | HEVC 10-bit with static HDR10 metadata: BT.2020 primaries, SMPTE ST 2084 (PQ) transfer, mastering-display + MaxCLL/MaxFALL SEI. See note below on how this was actually produced. |
| `27-h264-ass-subs-aac.mkv` | Embedded, styled ASS subtitle track (custom font/color/bold via override tags). |
| `28-h264-srt-sidecar-aac.mkv` + `.srt` | Plain video with an external `.srt` sidecar sharing the same basename, for Jellyfin's external-subtitle auto-pairing. |
| `29-h264-chapters-aac.mkv` | Three embedded chapters via an ffmetadata chapter file. |
| `30-h264-dual-audio.mkv` | Two audio tracks (AAC "English" + AC-3 "Spanish", distinct tones so they're audibly different). |
| `31-h264-ac3-5.1.mkv` | 5.1 surround AC-3 (verified via ffprobe: 6 channels). |
| `32-h264-23.976fps-aac.mkv` | 23.976 fps (`24000/1001`) variant. |
| `33-h264-50fps-aac.mkv` | 50 fps variant. |
| `34-hevc10-hdr10-nits-ladder-aac.mkv` | HDR10 luminance ladder: six neutral bars, left to right 100, 203, 400, 1000, 2000 and 4000 nits (PQ). With EDR working the 203-nit bar matches the app's white UI and the bars to its right are brighter, up to the display's headroom; tone-mapped to SDR they converge on white. The info overlay's "HDR output" row states which. |

### HDR10 metadata gotcha

ffmpeg's generic `-color_primaries`/`-color_trc`/`-colorspace` output
options did **not** reliably reach libx265's VUI parameters in the
`linuxserver/ffmpeg` build used here (only matrix coefficients came
through in testing; primaries/transfer stayed `unknown` per `ffprobe`).
`-x265-params colorprim=...:transfer=...` alone didn't fix it either.
What worked: set the mastering-display/MaxCLL SEI via `-x265-params`, then
force the VUI colour flags explicitly with the `hevc_metadata` bitstream
filter (`-bsf:v hevc_metadata=colour_primaries=9:transfer_characteristics=16:matrix_coefficients=9`).
Verified with `ffprobe -show_streams`: `color_primaries=bt2020`,
`color_transfer=smpte2084`, `color_space=bt2020nc`, plus
`Mastering display metadata` and `Content light level metadata` side data
on frames.

## Shows (4 episodes across 2 series)

```
Shows/Quantum Static/Season 01/Quantum Static S01E01.mkv   (h264/aac)
Shows/Quantum Static/Season 01/Quantum Static S01E02.mkv   (h264/ac3)
Shows/Quantum Static/Season 02/Quantum Static S02E01.mkv   (hevc-8bit/aac)
Shows/Glass Horizon/Season 01/Glass Horizon S01E01.mkv     (h264/eac3)
```

Enough to exercise series/season grouping (one show with two seasons, a
second show with one) without a large fixture set.

## What's deliberately NOT generated: DTS, TrueHD, PGS

DTS and Dolby TrueHD have no free/open-source encoders — `ffmpeg`'s
`dca`/`truehd` entries in `linuxserver/ffmpeg -encoders` are decode-only
(no `E` flag), and there is no legally-encumbrance-free way to produce
valid bitstreams for either in a container image. PGS (`.sup`) subtitles
are a Blu-ray bitmap-subtitle format produced by authoring tools, not
something `ffmpeg` can synthesize from scratch (there's no PGS encoder;
`libzvbi`/similar teletext tools don't apply here either).

These will need to come from real-world open test-media suites later
(e.g. jellyfin-media-tests-style fixtures, or clips pulled from public
domain Blu-ray test discs) rather than being synthesized. Everything else
in this corpus (h264/hevc/av1/vp9 video; aac/ac3/eac3/flac/opus audio;
ASS/SRT subs; chapters; HDR10 metadata) has a working open encoder path
and is generated here.
