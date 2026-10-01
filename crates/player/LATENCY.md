# Stream-open latency

What decides `Player::load` -> first frame against a remote Jellyfin
server, the mpv options that keep it short, and how to measure it.

## What determines open-to-first-frame time

On a remote link, round trips cost more than bandwidth. Anything that makes
libavformat issue another blocking read at a new offset, or open another
HTTP connection, adds at least one RTT.

- **Container index location.** mpv cannot report streams or seek until it
  has the index. Matroska `Cues` sit near the front: an open or a far resume
  is one lookup. An MP4 with `moov` at the tail (ffmpeg's default without
  `-movflags +faststart`) makes the `mov` demuxer read the head, then open a
  second HTTP connection with a `Range` request for the tail: about one extra
  RTT, paid equally by fresh opens and resumes. The rest of `moov` then
  streams at the bandwidth cap, so size matters only through transfer time.
  No mpv option avoids this; a server-side `ffmpeg -i in.mp4 -c copy
  -movflags +faststart out.mp4` removes it. MPEG-TS has no index, so a far
  seek is a binary search (`ff_seek_frame_binary`), one read per probe.
- **Stream-info probing.** For formats outside mpv's `auto` whitelist
  (MPEG-TS included), `avformat_find_stream_info()` reads ahead to identify
  streams the headers already describe. Over a slow link this costs more
  than the seek.
- **Resume seeks.** `hr-seek=yes` is global and `start=+X` is always
  frame-exact: the demuxer lands on the preceding keyframe and decodes
  forward. `hr-seek=no` would not change the initial seek anyway;
  `demux_lavf.c` never reads mpv's `SEEK_HR` flag.
- **Demuxer cache and readahead.** With `cache-pause-initial=no` the first
  frame does not wait for the cache. `demuxer-readahead-secs` is the target
  once playing; `demuxer-max-bytes`/`-back-bytes` are seek-cache capacity.
  None of them delays the first frame.
- **Hardware decode.** `hwdec=videotoolbox` initialises on the first
  decodable frame. Landing on a mid-stream keyframe before anything has been
  decoded (a keyframe seek at open) produces transient decode errors that
  take seconds to clear; frame-exact `start=+X` does not. The app keeps one
  long-lived `Player`, so mpv/VideoToolbox setup is paid once per launch.
- **Cold server state.** A file's first open in a session pays for the
  server's file open, page-cache misses and connection setup; a resume
  usually hits a warm file. The app warms the stream head when a Detail page
  opens (`JellyfinClient::warm_stream_head`) and can preload a paused
  stream (`start_paused`, `readahead_secs`, `max_bytes`).

## Load options

`build_loadfile_options` (`src/lib.rs`) builds the per-file options;
`is_network_url` (`http://`/`https://`) gates the network profile, so local
files keep mpv's defaults.

| Option | When | Why |
|---|---|---|
| `pause=no` / `pause=yes` | always | `pause` is global state that survives `loadfile`; without it a file loaded after an EOF-paused one never starts. `yes` is the paused preload. |
| `start=+X` | `start_secs` | Frame-exact resume, identical for every source. |
| `demuxer-lavf-buffersize=1048576` | network | More data per libavformat read before another blocking fetch (default 32 KiB). |
| `stream-buffer-size=1MiB` | network | Same for mpv's stream layer (default 128 KiB); a one-time allocation, separate from the seek cache. |
| `demuxer-lavf-probe-info=nostreams` | network | Skips `avformat_find_stream_info()` unless the headers show no streams; the largest single saving for MPEG-TS. |
| `demuxer-readahead-secs=N` | `readahead_secs` | Caps a preload; `Player::set_readahead_secs` restores it on promotion. |
| `demuxer-max-bytes=N` | `max_bytes` | Byte ceiling for the same preload; `Player::set_max_bytes` restores it. |
| `http-header-fields=...` | headers | Auth for direct-stream URLs. |
| `sub-file=...` | external subs | Sidecar subtitles. |

Global `INIT_OPTIONS` that bear on latency:

| Option | Why |
|---|---|
| `cache=yes`, `demuxer-max-bytes=150MiB`, `demuxer-max-back-bytes=75MiB` | Seek-back and far buffering are cheap once playing. |
| `demuxer-readahead-secs=60` | Margin on a constrained link without slowing the first frame. |
| `cache-pause-initial=no` | Pinned so an mpv default change cannot make the first frame wait for the cache. |
| `stream-lavf-o=reconnect=1,reconnect_streamed=1,reconnect_delay_max=5` | Without it ffmpeg treats a dropped connection as end of file. |
| `hr-seek=yes` | Frame-exact by default; `seek_absolute_fast` overrides per command. |
| `hwdec=videotoolbox` | Needs a Core Profile GL 3.2+ context (see `Player::new`). |

Measured and left out: `demuxer-lavf-probesize`/`-analyzeduration` (no gain
alone or on top of `nostreams`), 4 MiB buffers (same as 1 MiB),
`demuxer-lavf-o=fflags=+fastseek` (no effect), a deferred
`absolute-percent+keyframes` resume seek (the cold-hwdec errors above;
seconds slower), and `start=+0` on a fresh open (same as omitting it).

## Measurements

Via the benchmark relay at ~118 ms RTT and 16 Mbit/s, median of three runs:

| Case | Loaded |
|---|---|
| 240 s MPEG-TS, resume +200 s | ~130-150 ms (~410-460 ms without `nostreams`) |
| 240 s MP4, `moov` at tail, fresh or resume | ~260-270 ms |
| 240 s MP4, faststart, fresh or resume | ~130-140 ms |
| 2400 s MP4, `moov` at tail (~1.4 MB `moov`) | ~260-345 ms |
| 2400 s MP4, faststart | ~135-215 ms |

Without the relay, tail and faststart MP4 open equally fast: the gap is
round trips, not parsing. On a remote link over a VPN (~118 ms RTT, ~2 MB/s),
cold opens of real moov-at-tail MP4 episodes without a preload took 13-16 s
click to first frame, nearly all in mpv's open phase; larger real `moov`
atoms, TLS on the second connection and RTT jitter add to the one-RTT floor.

## Benchmark: `tests/latency_bench.rs`

All tests are `#[ignore]`d. They need the dev Jellyfin server
(`dev/setup-server.sh`, `localhost:8096`, seeded test account) and, for the
proxied ones, a TCP relay on `127.0.0.1:8097` forwarding to `:8096` with a
fixed per-direction delay and a per-connection bandwidth cap. The relay must
add the delay once per request (after an idle gap), not per read chunk, or
large responses are charged fake round trips. No relay ships with the tests.

| Tests | RTT | Bandwidth |
|---|---|---|
| `bench_fresh_load_*`, `bench_resume_200s_*`, `bench_fresh_vs_resume_proxied_*` | ~50 ms | 40 Mbit/s |
| `bench_moov_tail_*`, `bench_faststart_*` (proxied) | ~118 ms | 16 Mbit/s |
| `bench_click_to_first_frame_*` (also routes `PlaybackInfo` through the relay) | ~118 ms | 12 Mbit/s |

```
cargo test -p player --test latency_bench -- --ignored --nocapture --test-threads=1 [name]
```
Tests named `*_direct`, the unproxied fresh/resume ones, and the transcode
test skip the relay. `bench_real_server_cold_vs_promote` targets a real
server via `JELLYBEAM_BENCH_URL` + `JELLYBEAM_BENCH_TOKEN` (optional
`JELLYBEAM_BENCH_USER_ID`, `JELLYBEAM_BENCH_MATCH_MKV`/`_MP4`). The long
fixtures `92`-`98` are not checked in; the module docs give the `ffmpeg`
recipes. Put them in `dev/media/Movies/` and `POST /Library/Refresh`; a test
whose fixture is missing skips with a message.

Reading the output (compare rows within one run; absolute numbers depend
on the relay and on how warm the server is):

- `Loaded=` / `first-Position=`: from just before `Player::load` to the first
  `PlayerEvent::Loaded` / `Position`. Each run builds a new `Player`, so it
  includes hwdec setup the app pays once per launch.
- `COLD ... playback_info= loaded= first_frame= first_position=`: the whole
  click pipeline, starting before `PlaybackInfo`.
- `WARM ... head_start=Nms click->frame= click->pos_advance=`: from
  unpausing a preload opened N ms before the click.
- `first-segment=`: time to a server-side HLS transcode's first segment; no
  mpv involved.

## Tests that pin this behaviour

- `src/lib.rs` unit tests `build_loadfile_options_*` (network profile only
  for `http(s)`, `start=+X` with no `hr-seek`, preload caps only when
  requested) and `is_network_url_recognizes_http_and_https_only`.
- `tests/load_options.rs`, over a loopback HTTP server:
  `http_headers_are_sent_to_the_server`,
  `network_resume_is_frame_exact_same_as_local`, and
  `network_track_enumeration_unaffected_by_probe_info_nostreams` (ASS
  subtitles and a second audio track survive `nostreams`).
- `tests/playback.rs`: `load_after_eof_starts_playing_not_paused`,
  `seek_absolute_fast_converges_near_target`.
- `tests/hwdec.rs`: `hwdec_current_reports_videotoolbox_during_hevc_playback`.
- `tests/latency_bench.rs`: the measurements above (manual).
