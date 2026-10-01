//! Typed libmpv wrapper (links vendor/prefix libmpv, see docs/BUILD.md). Owns the mpv
//! handle + render context; exposes commands, property observation, and a render
//! hook the app crate drives from its own GL context: mpv never owns a window.

mod events;
mod node;
mod sys;

use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;

use tokio::sync::mpsc;

#[derive(Debug, thiserror::Error)]
pub enum PlayerError {
    #[error("mpv: {0}")]
    Mpv(String),
    #[error("not loaded")]
    NotLoaded,
}

#[derive(Debug, Clone)]
pub struct LoadRequest {
    pub url: String,
    pub http_headers: Vec<(String, String)>, // auth for direct-stream URLs
    pub start_secs: Option<f64>,             // resume position
    pub external_subs: Vec<String>,          // sidecar subtitle URLs
    /// Preload support: `true` loads with `pause=yes` so a speculative dark
    /// preload can buffer without audibly/visibly starting; promote via
    /// [`Player::set_paused`]`(false)`. `false` matches normal-load
    /// behavior.
    pub start_paused: bool,
    /// Preload support: per-file `demuxer-readahead-secs` override, so a
    /// dark preload caps its read-ahead as bandwidth courtesy; promotion
    /// restores the full target via [`Player::set_readahead_secs`]. `None`
    /// leaves `INIT_OPTIONS`' global value in effect.
    pub readahead_secs: Option<u32>,
    /// Per-file `demuxer-max-bytes` override (raw bytes), the byte-ceiling
    /// sibling of `readahead_secs` -- bounds preload cost regardless of the
    /// source's bitrate. Promotion restores full capacity via
    /// [`Player::set_max_bytes`]; `None` leaves `INIT_OPTIONS`' global value
    /// in effect.
    pub max_bytes: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct Track {
    pub mpv_id: i64,
    pub kind: TrackKind,
    pub title: Option<String>,
    pub lang: Option<String>,
    pub codec: Option<String>,
    pub default: bool,
    pub selected: bool,
    /// mpv `track-list/N/forced`: the container's "forced" flag on a
    /// subtitle track (foreign-dialogue-only subs). `false` for every
    /// non-subtitle track.
    pub forced: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrackKind {
    Video,
    Audio,
    Subtitle,
}

/// Subtitle style overrides (docs/OVERVIEW.md §6), mapped 1:1 onto mpv's
/// `sub-scale`/`sub-pos`/`sub-bold`/`sub-back-color` by
/// [`Player::set_subtitle_style`]. `Copy` so `app`'s settings sheet can hold
/// one in its persisted preferences struct without extra serialization glue.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SubtitleStyle {
    /// mpv `sub-scale`: a multiplier on the subtitle font size, 1.0 = mpv's
    /// own default.
    pub scale: f64,
    /// mpv `sub-pos`: 0-150, percent of screen height from the top (100 =
    /// mpv's own default, roughly bottom-aligned). `None` leaves mpv's
    /// current value untouched rather than forcing the default on every
    /// call.
    pub pos: Option<i64>,
    pub bold: bool,
    /// Background box opacity behind the subtitle text, 0.0 (fully
    /// transparent -- mpv's own default look) to 1.0 (fully opaque black
    /// box). Mapped to `sub-back-color`'s alpha channel; see
    /// [`back_color_hex`].
    pub back_alpha: f64,
}

impl Default for SubtitleStyle {
    fn default() -> Self {
        SubtitleStyle {
            scale: 1.0,
            pos: None,
            bold: false,
            back_alpha: 0.0,
        }
    }
}

/// Builds the `#AARRGGBB` hex string mpv's `sub-back-color` expects (mpv
/// manual, "Subtitles") from a 0.0-1.0 opacity; RGB fixed at black since
/// docs/UX-SPEC.md only calls for an opacity control. Pulled out as a pure
/// function so it's unit-testable without a live mpv context.
fn back_color_hex(alpha: f64) -> String {
    let alpha_byte = (alpha.clamp(0.0, 1.0) * 255.0).round() as u32;
    format!("#{alpha_byte:02X}000000")
}

/// Decoded video frame's format/color pipeline, as reported by mpv's
/// `video-params/*` sub-properties -- see [`Player::video_params`]. Every
/// field is independently `None` until mpv has decoded at least one frame
/// of the current file.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct VideoParams {
    pub width: Option<i64>,
    pub height: Option<i64>,
    /// e.g. `"nv12"`, `"p010"` (10-bit HDR sources typically decode to a
    /// 10-bit-plane format like this).
    pub pixel_format: Option<String>,
    /// e.g. `"bt.709"` (SDR) or `"bt.2020-ncl"` (HDR10/HLG).
    pub colormatrix: Option<String>,
    /// e.g. `"bt.709"` (SDR) or `"bt.2020"` (HDR10/HLG/Dolby Vision).
    pub primaries: Option<String>,
    /// The transfer function/EOTF -- e.g. `"bt.1886"` (SDR) or `"pq"`
    /// (HDR10/Dolby Vision) or `"hlg"` (HLG). The single most direct "is
    /// this actually HDR" signal among these fields.
    pub gamma: Option<String>,
}

/// Network/demuxer read-ahead cache state -- see [`Player::cache_state`].
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct CacheState {
    /// mpv `demuxer-cache-duration`: seconds of already-demuxed media
    /// sitting in the read-ahead buffer, ready to decode without a network
    /// round trip. `None` if mpv hasn't reported one yet (e.g. local file
    /// sources may never populate this the way a network stream does).
    pub demuxer_cache_duration_secs: Option<f64>,
    /// mpv `paused-for-cache`: true while playback is stalled waiting for
    /// more data to buffer (as opposed to a user-requested pause).
    pub paused_for_cache: bool,
    /// mpv `demuxer-cache-state/fw-bytes`: bytes buffered ahead of the
    /// current decode position -- more reliable than
    /// `demuxer_cache_duration_secs`'s duration guess (mpv manual calls that
    /// one "very unreliable"). `None` until mpv populates the
    /// `demuxer-cache-state` node.
    pub fw_bytes: Option<i64>,
    /// mpv `cache-speed` (== `demuxer-cache-state/raw-input-rate`): current
    /// I/O read speed between the cache and the network, bytes/sec measured
    /// over a trailing 1s window. `None` before mpv has measured anything.
    pub cache_speed_bps: Option<i64>,
}

/// Dropped-frame counts, split by mechanism -- see [`Player::frame_drop_stats`].
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct FrameDropStats {
    /// mpv `frame-drop-count`: frames dropped by the video output under the
    /// default `--framedrop=vo` policy (late-for-display, already decoded) --
    /// the counter that moves for most real "dropped frames" reports.
    pub vo: Option<i64>,
    /// mpv `decoder-frame-drop-count`: frames the decoder itself dropped
    /// (only under `--framedrop=decoder`/`decoder+vo`, unset here) or on
    /// damaged packets. Expected to read 0 in normal operation; nonzero
    /// signals something unusual at the demux/decode layer.
    pub decoder: Option<i64>,
}

impl FrameDropStats {
    /// `vo + decoder`, treating a missing side as 0 -- `None` only when both
    /// properties are unavailable.
    pub fn total(&self) -> Option<i64> {
        match (self.vo, self.decoder) {
            (None, None) => None,
            (a, b) => Some(a.unwrap_or(0) + b.unwrap_or(0)),
        }
    }
}

#[derive(Debug, Clone)]
pub enum PlayerEvent {
    /// Fires once mpv has resolved the track list and the file is
    /// considered loaded. For ordinary (seekable) media this is once mpv's
    /// `duration` resolves; for indefinite/live streams (`duration` stays
    /// `MPV_FORMAT_NONE`), it instead fires on `MPV_EVENT_FILE_LOADED` with
    /// `duration_secs: f64::INFINITY`. Callers must check
    /// `duration_secs.is_finite()` before treating it as known-length.
    Loaded {
        duration_secs: f64,
        tracks: Vec<Track>,
    },
    Position {
        secs: f64,
    }, // throttled (~4 Hz)
    PauseChanged {
        paused: bool,
    },
    TracksChanged(Vec<Track>),
    ChapterChanged {
        index: i64,
        title: Option<String>,
    },
    Buffering {
        active: bool,
        percent: f64,
    },
    EndOfFile,
    Error(String),
}

/// One mpv instance for the app's lifetime; items are loaded into it (the video
/// scene node persists across player states per docs/UX-SPEC.md §3).
pub struct Player {
    handle: *mut sys::mpv_handle,
    render_ctx: *mut sys::mpv_render_context,
    /// Set (from mpv's render thread) whenever a new frame is ready.
    /// `needs_render()` consumes it with a swap.
    needs_render: Arc<AtomicBool>,
    /// Raw pointer stashed with `mpv_render_context_set_update_callback`;
    /// balances the extra `Arc::into_raw` refcount bump on `Drop`.
    render_cb_ctx: *const AtomicBool,
    events_rx: Mutex<Option<mpsc::Receiver<PlayerEvent>>>,
    shutdown: Arc<AtomicBool>,
    event_thread: Mutex<Option<thread::JoinHandle<()>>>,
    /// The thread that made the *first* `render()`/`report_swap()` call,
    /// captured lazily. mpv/render.h "Threading" requires every render-API
    /// call for a given context to come from the same thread;
    /// `render`/`report_swap` assert against this in debug builds. Not
    /// checked in `Drop`: teardown legally frees from a different thread.
    render_thread_id: std::sync::OnceLock<std::thread::ThreadId>,
}

// SAFETY: libmpv's client API is documented as "generally fully thread-safe"
// (mpv/client.h "Multithreading": "The client API is generally fully
// thread-safe... everything is serialized through a single lock in the
// playback core."). Every `Player` method except `render`/`report_swap`
// touches only the client API through `self.handle`. `render`/`report_swap`
// touch `self.render_ctx`, which per mpv/render.h "Threading" must be called
// with the *same* OpenGL context current on the calling thread every time —
// that's a caller-side discipline (one render thread, documented on
// `render()` below), not something `Send`/`Sync` can express, and mpv's own
// docs explicitly describe running the render API from a thread distinct
// from normal client-API use as the expected setup. The event thread this
// struct owns only ever calls client-API functions (never render-API ones).
unsafe impl Send for Player {}
unsafe impl Sync for Player {}

/// mpv option name/value pairs set before `mpv_initialize` (client.h: most
/// options are readable/writable as properties after init too, but setting
/// them up front avoids a transient default state).
const INIT_OPTIONS: &[(&CStr, &CStr)] = &[
    // Render-API embedding (render_gl.h): mpv must not own the window; the
    // app drives the render callback from its own GL context.
    (c"vo", c"libmpv"),
    (c"hwdec", c"videotoolbox"),
    (c"keep-open", c"yes"),
    (c"hr-seek", c"yes"),
    (c"input-default-bindings", c"no"),
    (c"input-vo-keyboard", c"no"),
    (c"terminal", c"no"),
    (c"audio-client-name", c"Jellybeam"),
    // Streaming-friendly cache defaults — Jellyfin direct-play/HLS sources
    // are network streams, not instantly-seekable local files. The buffer
    // *capacity* (`demuxer-max-bytes`/`-back-bytes`) stays generous --
    // that's what makes far-forward buffering and seek-back cheap once
    // already playing, and doesn't cost anything until actually filled.
    (c"cache", c"yes"),
    (c"demuxer-max-bytes", c"150MiB"),
    (c"demuxer-max-back-bytes", c"75MiB"),
    // `demuxer-readahead-secs` is the *target* amount of read-ahead the
    // demuxer tries to keep buffered, not a hard gate before the first
    // frame (that's `cache-pause-initial` below). 60s gives a constrained
    // remote link real margin before `paused-for-cache` without slowing the
    // first frame (`tests/latency_bench.rs`).
    (c"demuxer-readahead-secs", c"60"),
    // Explicit rather than relying on mpv's default (`no`): the first frame
    // must never wait for a full cache, whatever a future mpv defaults to.
    (c"cache-pause-initial", c"no"),
    // ffmpeg's http reader treats a dropped or server-closed connection as
    // END OF FILE unless reconnect is enabled -- mpv does NOT turn it on by
    // default. Over a real remote link a mid-stream connection drop then
    // looks like premature EOF (`keep-open=yes` above auto-pauses, and
    // un-pausing re-pauses instantly, still "at EOF"). `reconnect_streamed=1`
    // is the one that matters for already-playing streams;
    // `reconnect_delay_max=5` caps ffmpeg's internal retry backoff so a
    // transient blip recovers in seconds rather than giving up.
    (
        c"stream-lavf-o",
        c"reconnect=1,reconnect_streamed=1,reconnect_delay_max=5",
    ),
    // The app owns all on-screen chrome (docs/OVERVIEW.md §5); mpv's built-in OSD
    // would otherwise fight GPUI's compositor for the same pixels. (There's
    // no separate `--osc` to disable: the on-screen *controller* is a Lua
    // script, and this vendored mpv is built with `-Dlua=disabled` per
    // docs/BUILD.md, so it doesn't exist as a build option at all.)
    (c"osd-level", c"0"),
];

/// The `demuxer-readahead-secs` value `INIT_OPTIONS` sets (kept in sync by
/// hand -- the option table needs a `&CStr` literal). The preload flow caps
/// a dark load below this and restores it on promotion via
/// [`Player::set_readahead_secs`].
pub const DEFAULT_READAHEAD_SECS: u32 = 60;

/// The `demuxer-max-bytes` value `INIT_OPTIONS` sets (`"150MiB"`), in raw
/// bytes -- kept in sync by hand, same contract as `DEFAULT_READAHEAD_SECS`
/// above. The preload flow caps a dark load's per-file `demuxer-max-bytes`
/// below this (see the `app` crate's `playback::PRELOAD_MAX_BYTES`) and
/// restores it on promotion via [`Player::set_max_bytes`].
pub const DEFAULT_MAX_BYTES: u64 = 150 * 1024 * 1024;

/// Properties observed for the `PlayerEvent` stream (see `events.rs`).
const OBSERVED_PROPERTIES: &[(&CStr, sys::mpv_format)] = &[
    (c"pause", sys::MPV_FORMAT_FLAG),
    (c"time-pos", sys::MPV_FORMAT_DOUBLE),
    (c"track-list", sys::MPV_FORMAT_NODE),
    (c"chapter", sys::MPV_FORMAT_INT64),
    (c"duration", sys::MPV_FORMAT_DOUBLE),
    (c"core-idle", sys::MPV_FORMAT_FLAG),
    (c"paused-for-cache", sys::MPV_FORMAT_FLAG),
    (c"eof-reached", sys::MPV_FORMAT_FLAG),
    // Sampling this only at the instant the stall flags flipped showed one
    // stale number per stall instead of the fill actually progressing.
    // Observing the property streams every change (mpv emits it
    // continuously, 0..100, during a rebuffer) -- `events.rs` re-sends
    // `Buffering` per change.
    (c"cache-buffering-state", sys::MPV_FORMAT_DOUBLE),
];

/// The type of the `get_proc_address` callback `Player::new` takes. Named
/// here so the FFI trampoline below has something to transmute to/from.
type GetProcAddressFn = fn(&str) -> *mut c_void;

/// RAII guard that destroys an `mpv_handle` unless defused via
/// `into_inner()`. Used so every fallible step in `Player::new` can use `?`
/// without hand-rolled cleanup at each early return.
struct HandleGuard(*mut sys::mpv_handle);

impl HandleGuard {
    fn into_inner(mut self) -> *mut sys::mpv_handle {
        let p = self.0;
        self.0 = std::ptr::null_mut();
        p
    }
}

impl Drop for HandleGuard {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: `self.0` was returned by `mpv_create` and not yet
            // handed to anyone else (guard still owns it).
            unsafe { sys::mpv_terminate_destroy(self.0) };
        }
    }
}

/// Same idea as `HandleGuard`, for the render context.
struct RenderCtxGuard(*mut sys::mpv_render_context);

impl RenderCtxGuard {
    fn into_inner(mut self) -> *mut sys::mpv_render_context {
        let p = self.0;
        self.0 = std::ptr::null_mut();
        p
    }
}

impl Drop for RenderCtxGuard {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: `self.0` was returned by `mpv_render_context_create`
            // and not yet handed to anyone else.
            unsafe { sys::mpv_render_context_free(self.0) };
        }
    }
}

/// Locks a `Mutex`, recovering the guard even if a previous panic poisoned
/// it. `Player`'s mutexes only ever guard plain data moves (`Option::take`),
/// so there's no invariant a panic could have left broken.
fn lock_ignore_poison<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

fn check(ret: c_int) -> Result<(), PlayerError> {
    if ret >= 0 {
        Ok(())
    } else {
        Err(PlayerError::Mpv(mpv_error_string(ret)))
    }
}

fn mpv_error_string(code: c_int) -> String {
    // SAFETY: mpv_error_string always returns a valid, static, non-NULL C
    // string (client.h: unknown codes yield the literal "unknown error").
    let ptr = unsafe { sys::mpv_error_string(code) };
    if ptr.is_null() {
        return format!("mpv error {code}");
    }
    unsafe { CStr::from_ptr(ptr) }
        .to_string_lossy()
        .into_owned()
}

/// Stashes a plain `fn(&str) -> *mut c_void` inside a C `void*` userdata
/// slot so it can round-trip through `mpv_opengl_init_params`.
///
/// SAFETY-adjacent note: `fn` pointers and data pointers are both a single
/// pointer-width value on every platform this crate targets (Apple Silicon
/// macOS); `proc_address_trampoline` below reverses this with a
/// same-size `transmute`. This is the standard way to smuggle a callback
/// through a C API's `void *ctx` parameter.
fn fn_to_ctx(f: GetProcAddressFn) -> *mut c_void {
    // A function-pointer-to-data-pointer cast is a plain, well-defined `as`
    // cast in Rust (unlike the reverse direction in the trampoline below,
    // which needs `transmute`).
    f as *mut c_void
}

/// The `mpv_opengl_init_params::get_proc_address` trampoline: mpv calls this
/// (from whatever thread it likes, per render_gl.h) with the `ctx` we handed
/// it in `Player::new`, and we forward to the real Rust callback.
unsafe extern "C" fn proc_address_trampoline(ctx: *mut c_void, name: *const c_char) -> *mut c_void {
    // Unwinding across an `extern "C"` boundary is undefined behavior. mpv
    // calls this from whatever thread it likes (render_gl.h), so a panic
    // inside the caller-supplied `get_proc_address` (or in the `CStr`
    // conversion below) must be caught here and turned into a null result
    // instead of unwinding into mpv's C frames.
    std::panic::catch_unwind(|| {
        if ctx.is_null() || name.is_null() {
            return std::ptr::null_mut();
        }
        // SAFETY: `ctx` was produced by `fn_to_ctx` from a real
        // `GetProcAddressFn` in `Player::new`, and mpv passes it back
        // unmodified.
        let f: GetProcAddressFn =
            unsafe { std::mem::transmute::<*mut c_void, GetProcAddressFn>(ctx) };
        // SAFETY: `name` is a NUL-terminated C string per render_gl.h.
        let name = unsafe { CStr::from_ptr(name) };
        match name.to_str() {
            Ok(s) => f(s),
            Err(_) => std::ptr::null_mut(),
        }
    })
    .unwrap_or(std::ptr::null_mut())
}

/// The `mpv_render_context_set_update_callback` trampoline: stores "a new
/// frame is ready" into the `AtomicBool` behind `ctx`. Per render.h, this
/// may be called from any thread and must not call back into libmpv.
unsafe extern "C" fn render_update_trampoline(ctx: *mut c_void) {
    // Unwinding across this `extern "C"` boundary is UB (render.h: "not
    // exiting the callback by throwing exceptions"). Nothing below can panic
    // today; the wrapper is what keeps that true if the body ever grows.
    let _ = std::panic::catch_unwind(|| {
        if ctx.is_null() {
            return;
        }
        // SAFETY: `ctx` points at the `AtomicBool` owned by the `Arc` cloned
        // via `Arc::into_raw` in `Player::new`; that Arc is kept alive for
        // exactly as long as `render_ctx` exists (see `Player::drop`), and
        // this callback is never invoked after `mpv_render_context_free`
        // returns.
        let flag = unsafe { &*(ctx as *const AtomicBool) };
        flag.store(true, Ordering::Release);
    });
}

/// Escapes a value for mpv's sub-option-string syntax using the `%n%string`
/// verbatim form (mpv manual, "List Options"/"Escaping"): the value is taken
/// literally for exactly `n` bytes, so embedded commas, colons, or `%` need
/// no further escaping.
fn escape_option_value(s: &str) -> String {
    format!("%{}%{}", s.len(), s)
}

/// True for a URL mpv will fetch over the network (`http://`/`https://`),
/// as opposed to a local file path -- gates the "network profile" options
/// in [`build_loadfile_options`] (buffer sizing that only makes sense when
/// reads have real round-trip cost) so local playback keeps its existing
/// behavior untouched.
fn is_network_url(url: &str) -> bool {
    url.starts_with("http://") || url.starts_with("https://")
}

/// Builds the `options` argument for the `loadfile` command from a
/// `LoadRequest` (resume position, auth headers, external subs, and the
/// remote-latency tuning below). Always returns `Some` (never `None`)
/// because of the unconditional `pause=no` below.
///
/// `pause=no` is required: `keep-open=yes` leaves mpv paused at EOF, and
/// `pause` is global core state that survives `loadfile`, so without it a
/// file loaded after an EOF-paused one would never start. As a per-file
/// option (mpv manual, "loadfile") it is race-free, unlike a separate
/// `set_paused(false)` that could briefly unpause the previous file.
fn build_loadfile_options(req: &LoadRequest) -> Result<Option<CString>, PlayerError> {
    // `start_paused` flips this to `pause=yes` -- a dark preload must open
    // and buffer without starting; everything else about the option's
    // rationale is unchanged from the doc comment above.
    let mut parts = vec![if req.start_paused {
        "pause=yes".to_string()
    } else {
        "pause=no".to_string()
    }];
    let network = is_network_url(&req.url);

    // The preload's capped read-ahead target (see `LoadRequest`'s field
    // doc). Emitted before the network profile so a future reader sees the
    // request-shaped options grouped together; mpv doesn't care about order.
    if let Some(secs) = req.readahead_secs {
        parts.push(format!("demuxer-readahead-secs={secs}"));
    }

    // The byte-ceiling sibling of the read-ahead target above -- see
    // `LoadRequest::max_bytes`'s doc comment for why both are needed (one
    // caps time, the other caps bytes; whichever is reached first wins).
    if let Some(bytes) = req.max_bytes {
        parts.push(format!("demuxer-max-bytes={bytes}"));
    }

    // "+secs" = relative-to-start resume position (mpv `start` option).
    // `start=+X` stays unconditional and frame-exact for every source:
    // `hr-seek=no` doesn't gate it (demux_lavf.c's seek never reads mpv's
    // SEEK_HR flag), and deferring to a post-load keyframe seek measurably
    // regressed MPEG-TS resume (decoder errors landing mid-GOP on a cold
    // hwdec pipeline). See crates/player/LATENCY.md.
    if let Some(start) = req.start_secs {
        parts.push(format!("start=+{start}"));
    }

    if network {
        // Only applied for http(s) sources, since local files have no
        // round-trip cost for these to amortize. `demuxer-lavf-buffersize`/
        // `stream-buffer-size` gate how much data a single read can return
        // before another blocking fetch is issued; at their small defaults,
        // opening a file or resuming into an unindexed container (e.g.
        // MPEG-TS) can need many sequential reads, each paying the link's
        // full round-trip time. 1MiB is a one-time allocation, not a
        // steady-state cost, and doesn't touch `INIT_OPTIONS`'
        // `demuxer-max-bytes`/`-back-bytes` (the seekable-cache budget).
        // See crates/player/LATENCY.md.
        parts.push("demuxer-lavf-buffersize=1048576".to_string()); // 1MiB
        parts.push("stream-buffer-size=1MiB".to_string());

        // `demuxer-lavf-probe-info=nostreams` skips libavformat's
        // `avformat_find_stream_info()` unless the container still looks
        // streamless after its own headers parse. mpv's `auto` default
        // doesn't whitelist MPEG-TS, so a network TS open or resume would
        // otherwise pay a full probe pass it doesn't need (about 3x on
        // `Loaded` for an unindexed-TS resume at ~118ms RTT). Track
        // enumeration is pinned by `tests/load_options.rs`; measurements
        // in crates/player/LATENCY.md.
        parts.push("demuxer-lavf-probe-info=nostreams".to_string());
    }

    if !req.http_headers.is_empty() {
        let joined = req
            .http_headers
            .iter()
            .map(|(k, v)| escape_option_value(&format!("{k}: {v}")))
            .collect::<Vec<_>>()
            .join(",");
        parts.push(format!("http-header-fields={joined}"));
    }

    if !req.external_subs.is_empty() {
        let joined = req
            .external_subs
            .iter()
            .map(|s| escape_option_value(s))
            .collect::<Vec<_>>()
            .join(",");
        parts.push(format!("sub-file={joined}"));
    }

    // Always `Some` now (`pause=no` above guarantees `parts` is non-empty),
    // but the `Option` return type stays -- `load`'s `mpv_command` arg-list
    // construction already branches on it, and a bare "no options at all"
    // shape remains a legitimate thing for `build_loadfile_options` to be
    // able to express in principle.
    CString::new(parts.join(","))
        .map(Some)
        .map_err(|e| PlayerError::Mpv(format!("loadfile options contain a NUL byte: {e}")))
}

impl Player {
    /// `get_proc_address` comes from the app's GL context; mpv renders into
    /// that context and never creates a window of its own.
    ///
    /// The caller must have an OpenGL context "current" on the calling
    /// thread before calling this (mpv/render.h: the render API resolves and
    /// calls GL functions during `mpv_render_context_create` itself, not
    /// just during `render()`), and must keep using *that same* context
    /// (made current on whichever thread calls `render()`/`report_swap()`)
    /// for the lifetime of this `Player`.
    ///
    /// **Contract:** that GL context must be a Core Profile context, OpenGL
    /// version >= 3.2. This crate requests `hwdec=videotoolbox` (see
    /// `INIT_OPTIONS`), and mpv's VideoToolbox interop path silently
    /// degrades to software decoding — no error, just worse performance and
    /// higher power draw — if the context it's handed isn't a Core Profile
    /// context of at least that version. Verify with `hwdec_current()` after
    /// loading a hardware-decodable file if this ever needs re-checking.
    pub fn new(get_proc_address: fn(&str) -> *mut std::ffi::c_void) -> Result<Self, PlayerError> {
        let handle = unsafe { sys::mpv_create() };
        if handle.is_null() {
            return Err(PlayerError::Mpv("mpv_create returned NULL".to_string()));
        }
        let handle_guard = HandleGuard(handle);

        for (name, value) in INIT_OPTIONS {
            check(unsafe { sys::mpv_set_option_string(handle, name.as_ptr(), value.as_ptr()) })?;
        }

        check(unsafe { sys::mpv_initialize(handle) })?;

        // Only fatal/error-level messages become PlayerEvent::Error; see
        // events.rs.
        check(unsafe { sys::mpv_request_log_messages(handle, c"error".as_ptr()) })?;

        for (name, format) in OBSERVED_PROPERTIES {
            check(unsafe { sys::mpv_observe_property(handle, 0, name.as_ptr(), *format) })?;
        }

        // -- Render API (OpenGL) --
        let ctx_ptr = fn_to_ctx(get_proc_address);
        let mut gl_init_params = sys::mpv_opengl_init_params {
            get_proc_address: proc_address_trampoline,
            get_proc_address_ctx: ctx_ptr,
        };
        let mut render_params = [
            sys::mpv_render_param {
                type_: sys::MPV_RENDER_PARAM_API_TYPE,
                data: sys::MPV_RENDER_API_TYPE_OPENGL.as_ptr() as *mut c_void,
            },
            sys::mpv_render_param {
                type_: sys::MPV_RENDER_PARAM_OPENGL_INIT_PARAMS,
                data: (&mut gl_init_params) as *mut sys::mpv_opengl_init_params as *mut c_void,
            },
            sys::mpv_render_param {
                type_: sys::MPV_RENDER_PARAM_INVALID,
                data: std::ptr::null_mut(),
            },
        ];
        let mut render_ctx: *mut sys::mpv_render_context = std::ptr::null_mut();
        check(unsafe {
            sys::mpv_render_context_create(&mut render_ctx, handle, render_params.as_mut_ptr())
        })?;
        let render_ctx_guard = RenderCtxGuard(render_ctx);

        let needs_render = Arc::new(AtomicBool::new(false));
        // SAFETY-relevant: this clone's pointer is reclaimed in `Drop` via
        // `Arc::from_raw`, after `mpv_render_context_free` (so the callback
        // can no longer read through it).
        let render_cb_ctx = Arc::into_raw(Arc::clone(&needs_render));
        unsafe {
            sys::mpv_render_context_set_update_callback(
                render_ctx,
                render_update_trampoline,
                render_cb_ctx as *mut c_void,
            );
        }

        let (tx, rx) = mpsc::channel::<PlayerEvent>(256);
        let shutdown = Arc::new(AtomicBool::new(false));

        let thread_handle = {
            let handle_addr = handle as usize;
            let shutdown = Arc::clone(&shutdown);
            thread::Builder::new()
                .name("player-mpv-events".to_string())
                .spawn(move || {
                    // Reconstruct the pointer on the new thread: raw
                    // pointers aren't `Send`, but `usize` is, and `Player`
                    // guarantees `handle` outlives this thread (see `Drop`).
                    let handle = handle_addr as *mut sys::mpv_handle;
                    events::run_event_loop(handle, tx, shutdown);
                })
                .map_err(|e| PlayerError::Mpv(format!("failed to spawn mpv event thread: {e}")))?
        };

        Ok(Player {
            handle: handle_guard.into_inner(),
            render_ctx: render_ctx_guard.into_inner(),
            needs_render,
            render_cb_ctx,
            events_rx: Mutex::new(Some(rx)),
            shutdown,
            event_thread: Mutex::new(Some(thread_handle)),
            render_thread_id: std::sync::OnceLock::new(),
        })
    }

    pub fn events(&self) -> tokio::sync::mpsc::Receiver<PlayerEvent> {
        let mut slot = lock_ignore_poison(&self.events_rx);
        if let Some(rx) = slot.take() {
            return rx;
        }
        // events() is documented/expected to be called once; the
        // signature returns an owned `Receiver` rather than `Option`, so a
        // second call can't signal "already taken" through the type. Hand
        // back a receiver whose sender was dropped immediately instead of
        // panicking — it just reads as "channel closed, no more events".
        let (_tx, rx) = mpsc::channel(1);
        rx
    }

    pub fn load(&self, req: LoadRequest) -> Result<(), PlayerError> {
        let options = build_loadfile_options(&req)?;
        let url = CString::new(req.url)
            .map_err(|e| PlayerError::Mpv(format!("url contains a NUL byte: {e}")))?;

        // mpv 0.38+'s `loadfile` command signature is
        // `loadfile <url> [<flags> [<index> [<options>]]]` (client.h /
        // input.rst "loadfile"): <options> can only be passed positionally
        // after <index>, so when we have options to set, a placeholder
        // <index> of "-1" (mpv's own documented default/no-op value) must
        // be inserted before it, or mpv either rejects the command or
        // misinterprets the options string as the index.
        let mut args: Vec<*const c_char> =
            vec![c"loadfile".as_ptr(), url.as_ptr(), c"replace".as_ptr()];
        if let Some(options) = &options {
            args.push(c"-1".as_ptr());
            args.push(options.as_ptr());
        }
        args.push(std::ptr::null());

        check(unsafe { sys::mpv_command(self.handle, args.as_ptr()) })
    }

    /// Current hardware-decode backend in use, per mpv's `hwdec-current`
    /// property (e.g. `"videotoolbox"`), or `None` if nothing has loaded yet
    /// or decoding fell back to software. See the Core Profile contract note
    /// on `Player::new` — a non-Core-Profile GL context is the most common
    /// reason this reports `None`/software when hardware decode was
    /// requested.
    pub fn hwdec_current(&self) -> Option<String> {
        let value = self.get_string(c"hwdec-current")?;
        if value.is_empty() || value == "no" {
            None
        } else {
            Some(value)
        }
    }

    // -- Property getters shared by `hwdec_current` and the info-overlay
    // -- getters below. Every mpv numeric
    // property this crate reads is requested through `MPV_FORMAT_DOUBLE`/
    // `_INT64`/`_FLAG` regardless of its internal storage type: per
    // client.h's `mpv_get_property` doc, "the underlying value will be
    // converted", so this is exactly as safe as requesting a property's
    // "native" format and simpler than tracking each one's actual type.

    /// SAFETY-relevant: `mpv_get_property_string` returns either NULL or an
    /// mpv-allocated NUL-terminated string, which is freed here per
    /// client.h's contract for that function (mirrors
    /// `events::read_chapter_title`).
    fn get_string(&self, name: &CStr) -> Option<String> {
        let raw = unsafe { sys::mpv_get_property_string(self.handle, name.as_ptr()) };
        if raw.is_null() {
            return None;
        }
        let value = unsafe { CStr::from_ptr(raw) }
            .to_string_lossy()
            .into_owned();
        unsafe { sys::mpv_free(raw as *mut c_void) };
        Some(value)
    }

    /// SAFETY: `MPV_FORMAT_DOUBLE` writes one `f64` into `value` (client.h);
    /// no mpv-owned allocation to free.
    fn get_double(&self, name: &CStr) -> Option<f64> {
        let mut value: f64 = 0.0;
        let ret = unsafe {
            sys::mpv_get_property(
                self.handle,
                name.as_ptr(),
                sys::MPV_FORMAT_DOUBLE,
                &mut value as *mut f64 as *mut c_void,
            )
        };
        (ret >= 0).then_some(value)
    }

    /// SAFETY: `MPV_FORMAT_INT64` writes one `i64` into `value` (client.h);
    /// no mpv-owned allocation to free.
    fn get_int64(&self, name: &CStr) -> Option<i64> {
        let mut value: i64 = 0;
        let ret = unsafe {
            sys::mpv_get_property(
                self.handle,
                name.as_ptr(),
                sys::MPV_FORMAT_INT64,
                &mut value as *mut i64 as *mut c_void,
            )
        };
        (ret >= 0).then_some(value)
    }

    /// SAFETY: `MPV_FORMAT_FLAG` writes one `c_int` into `value` (client.h);
    /// no mpv-owned allocation to free.
    fn get_flag(&self, name: &CStr) -> Option<bool> {
        let mut value: c_int = 0;
        let ret = unsafe {
            sys::mpv_get_property(
                self.handle,
                name.as_ptr(),
                sys::MPV_FORMAT_FLAG,
                &mut value as *mut c_int as *mut c_void,
            )
        };
        (ret >= 0).then_some(value != 0)
    }

    /// SAFETY: `mpv_set_property(..., MPV_FORMAT_FLAG, ...)` reads
    /// `sizeof(c_int)` bytes from `value`'s address per client.h's contract
    /// for that format; `value` is declared `c_int` (MPV_FORMAT_FLAG's
    /// underlying storage, not `bool`) so the format tag matches the
    /// local's actual type/width.
    fn set_property_flag(&self, name: &CStr, flag: bool) -> Result<(), PlayerError> {
        let mut value: c_int = if flag { 1 } else { 0 };
        check(unsafe {
            sys::mpv_set_property(
                self.handle,
                name.as_ptr(),
                sys::MPV_FORMAT_FLAG,
                &mut value as *mut c_int as *mut c_void,
            )
        })
    }

    /// SAFETY: `mpv_set_property(..., MPV_FORMAT_INT64, ...)` reads
    /// `sizeof(i64)` bytes from `value`'s address per client.h's contract
    /// for that format; `value` is declared `i64` so the format tag matches
    /// the local's actual type/width.
    fn set_property_int64(&self, name: &CStr, value: i64) -> Result<(), PlayerError> {
        let mut value = value;
        check(unsafe {
            sys::mpv_set_property(
                self.handle,
                name.as_ptr(),
                sys::MPV_FORMAT_INT64,
                &mut value as *mut i64 as *mut c_void,
            )
        })
    }

    /// SAFETY: `mpv_set_property(..., MPV_FORMAT_DOUBLE, ...)` reads
    /// `sizeof(f64)` bytes from `value`'s address per client.h's contract
    /// for that format; `value` is declared `f64` so the format tag matches
    /// the local's actual type/width.
    fn set_property_double(&self, name: &CStr, value: f64) -> Result<(), PlayerError> {
        let mut value = value;
        check(unsafe {
            sys::mpv_set_property(
                self.handle,
                name.as_ptr(),
                sys::MPV_FORMAT_DOUBLE,
                &mut value as *mut f64 as *mut c_void,
            )
        })
    }

    /// Pulls a node-shaped property (e.g. `demuxer-cache-state`) and
    /// converts it via `node::node_to_json` -- same conversion the
    /// `track-list` property-change event uses (`events.rs`), just reached
    /// through a one-shot `mpv_get_property` pull instead of an observed
    /// event, since `Player::cache_state` is polled on demand rather than
    /// subscribed to (a change-event stream for a property that's
    /// meaningfully different on every packet would be far noisier than
    /// this crate's existing `OBSERVED_PROPERTIES` list wants to be).
    ///
    /// SAFETY-relevant: `mpv_get_property(..., MPV_FORMAT_NODE, ...)` fills
    /// `node` with mpv-owned allocations (per client.h, "the underlying
    /// data must be freed with `mpv_free_node_contents`"); freed
    /// immediately after conversion, mirroring `get_string`'s
    /// fetch-then-free shape for the plain-string getters above.
    fn get_node_json(&self, name: &CStr) -> Option<serde_json::Value> {
        let mut node = sys::mpv_node::default();
        let ret = unsafe {
            sys::mpv_get_property(
                self.handle,
                name.as_ptr(),
                sys::MPV_FORMAT_NODE,
                &mut node as *mut sys::mpv_node as *mut c_void,
            )
        };
        if ret < 0 {
            return None;
        }
        let value = unsafe { node::node_to_json(&node) };
        unsafe { sys::mpv_free_node_contents(&mut node as *mut sys::mpv_node) };
        Some(value)
    }

    /// mpv `path`: the URL/path the currently-open file was loaded with, or
    /// `None` when nothing is loaded. The preload promote path uses this as
    /// a cheap sanity check that the dark-preloaded file is still what mpv
    /// actually holds before trusting the warm state.
    pub fn current_path(&self) -> Option<String> {
        self.get_string(c"path")
    }

    /// mpv `mpv-version`, e.g. `"mpv 0.38.0"` -- a build-info property, not
    /// playback state, so unlike most getters in this section it resolves
    /// right after `Player::new`, before anything is ever loaded. Read by
    /// the About window's spec strip (`app`'s `about.rs`) to show which mpv
    /// build the app actually linked, rather than a version number this
    /// crate would otherwise have to track by hand and risk drifting from
    /// whatever `libmpv` the binary was actually built/run against.
    pub fn mpv_version(&self) -> Option<String> {
        self.get_string(c"mpv-version")
    }

    /// mpv `video-bitrate`, bits/sec, calculated from packet sizes (mpv
    /// manual: "may be totally incorrect for certain file formats"; shown
    /// as a measured/approximate figure, not the stream's nominal bitrate).
    /// `None` before mpv has measured anything yet (e.g. right after load).
    pub fn video_bitrate(&self) -> Option<f64> {
        self.get_double(c"video-bitrate").filter(|v| *v > 0.0)
    }

    /// mpv `audio-bitrate` -- see `video_bitrate`'s doc comment.
    pub fn audio_bitrate(&self) -> Option<f64> {
        self.get_double(c"audio-bitrate").filter(|v| *v > 0.0)
    }

    /// mpv `time-pos`: current playback position in seconds. A polled
    /// one-shot read of the same property `OBSERVED_PROPERTIES` streams as
    /// `PlayerEvent::Position` -- for a caller that isn't already holding
    /// onto the latest `Position` event (e.g. the periodic playback
    /// diagnostics log in `app`'s `playback.rs`, which runs on its own
    /// timer rather than reacting to the event stream), this is cheaper
    /// than plumbing the last-seen position through as extra state.
    pub fn position_secs(&self) -> Option<f64> {
        self.get_double(c"time-pos")
    }

    /// mpv `container-fps`: the frame rate the container declares (as
    /// opposed to `estimated-vf-fps`, which this crate doesn't expose --
    /// the container-declared value is what the info overlay's "FPS" row
    /// wants, since it matches the file's metadata).
    pub fn container_fps(&self) -> Option<f64> {
        self.get_double(c"container-fps").filter(|v| *v > 0.0)
    }

    /// Dropped-frame counts, split by mechanism (mpv `frame-drop-count` /
    /// `decoder-frame-drop-count`) -- see [`FrameDropStats`]'s doc comment.
    /// This used to read `vo-drop-frame-count`, which does not exist as an
    /// mpv property at all, so the info overlay's "Dropped frames" row was
    /// under-counting by however many frames the VO had dropped.
    pub fn frame_drop_stats(&self) -> FrameDropStats {
        FrameDropStats {
            vo: self.get_int64(c"frame-drop-count"),
            decoder: self.get_int64(c"decoder-frame-drop-count"),
        }
    }

    /// Current decoded video frame's format/color pipeline (mpv's
    /// `video-params` sub-properties) -- the HDR pipeline info the info
    /// overlay shows (colormatrix/primaries/gamma distinguish e.g. HDR10's
    /// `bt.2020`/`pq` from SDR's `bt.709`/`bt.1886`). All fields
    /// individually `None` until mpv has decoded at least one frame.
    pub fn video_params(&self) -> VideoParams {
        VideoParams {
            width: self.get_int64(c"video-params/w"),
            height: self.get_int64(c"video-params/h"),
            pixel_format: self.get_string(c"video-params/pixelformat"),
            colormatrix: self.get_string(c"video-params/colormatrix"),
            primaries: self.get_string(c"video-params/primaries"),
            gamma: self.get_string(c"video-params/gamma"),
        }
    }

    /// Network/demuxer read-ahead cache state (mpv `demuxer-cache-
    /// duration` + `paused-for-cache` + `demuxer-cache-state/fw-bytes` +
    /// `cache-speed`) -- the info overlay's "cache depth"/"Buffer"/
    /// "Network" rows and the OSD scrubber's buffered-ahead fill.
    pub fn cache_state(&self) -> CacheState {
        let fw_bytes = self
            .get_node_json(c"demuxer-cache-state")
            .and_then(|v| v.get("fw-bytes").and_then(serde_json::Value::as_i64));
        CacheState {
            demuxer_cache_duration_secs: self.get_double(c"demuxer-cache-duration"),
            paused_for_cache: self.get_flag(c"paused-for-cache").unwrap_or(false),
            fw_bytes,
            cache_speed_bps: self.get_int64(c"cache-speed"),
        }
    }

    pub fn stop(&self) -> Result<(), PlayerError> {
        let args = [c"stop".as_ptr(), std::ptr::null()];
        check(unsafe { sys::mpv_command(self.handle, args.as_ptr()) })
    }

    pub fn set_paused(&self, paused: bool) -> Result<(), PlayerError> {
        self.set_property_flag(c"pause", paused)
    }

    /// Preload promotion: raises/lowers the live `demuxer-readahead-secs`
    /// target at runtime -- a per-file `readahead_secs` cap from
    /// [`LoadRequest`] stays in effect for the whole file otherwise, and a
    /// promoted preload should buffer as deep as a normal play
    /// (`INIT_OPTIONS`' 60s).
    pub fn set_readahead_secs(&self, secs: u32) -> Result<(), PlayerError> {
        self.set_property_int64(c"demuxer-readahead-secs", i64::from(secs))
    }

    /// Preload promotion: raises the live `demuxer-max-bytes` ceiling back
    /// to normal, the byte-cap sibling of `set_readahead_secs` above -- a
    /// promoted preload should buffer as deep as a normal play rather than
    /// staying capped at the dark preload's ceiling and stuttering. `bytes`
    /// is clamped to `i64::MAX` purely so the cast can't overflow.
    pub fn set_max_bytes(&self, bytes: u64) -> Result<(), PlayerError> {
        let value: i64 = bytes.min(i64::MAX as u64) as i64;
        self.set_property_int64(c"demuxer-max-bytes", value)
    }

    /// hr-seek (frame-accurate).
    pub fn seek_absolute(&self, secs: f64) -> Result<(), PlayerError> {
        let target = CString::new(format!("{secs}"))
            .map_err(|e| PlayerError::Mpv(format!("bad seek target: {e}")))?;
        let args = [
            c"seek".as_ptr(),
            target.as_ptr(),
            // "absolute+exact": frame-accurate regardless of playback state;
            // reinforces the global `hr-seek=yes` init option.
            c"absolute+exact".as_ptr(),
            std::ptr::null(),
        ];
        check(unsafe { sys::mpv_command(self.handle, args.as_ptr()) })
    }

    /// Fast/inexact seek: jumps to the nearest keyframe at or before `secs`
    /// (mpv `seek <t> absolute+keyframes`) instead of decoding forward from
    /// that keyframe to land exactly on `secs` the way `seek_absolute`'s
    /// `hr-seek` does. Much cheaper for a long jump -- the global
    /// `hr-seek=yes` init option makes every seek frame-exact by default,
    /// which is fine for a nearby jump but means seeking far into a file
    /// (a scrubber drag, or an arrow-key jump) pays for potentially minutes
    /// of decode-and-discard just to reach the target frame. Use this for
    /// interactive, "close enough" seeks (scrubber drags, arrow-key jumps);
    /// keep `seek_absolute` wherever landing on the exact frame matters
    /// (e.g. resume restore).
    pub fn seek_absolute_fast(&self, secs: f64) -> Result<(), PlayerError> {
        let target = CString::new(format!("{secs}"))
            .map_err(|e| PlayerError::Mpv(format!("bad seek target: {e}")))?;
        let args = [
            c"seek".as_ptr(),
            target.as_ptr(),
            // "absolute+keyframes": jump to the nearest keyframe, no
            // decode-forward-to-target pass -- overrides the global
            // `hr-seek=yes` default for this one command (mpv manual,
            // "seek" command: the flags argument takes precedence).
            c"absolute+keyframes".as_ptr(),
            std::ptr::null(),
        ];
        check(unsafe { sys::mpv_command(self.handle, args.as_ptr()) })
    }

    pub fn seek_relative(&self, delta_secs: f64) -> Result<(), PlayerError> {
        let target = CString::new(format!("{delta_secs}"))
            .map_err(|e| PlayerError::Mpv(format!("bad seek delta: {e}")))?;
        let args = [
            c"seek".as_ptr(),
            target.as_ptr(),
            c"relative".as_ptr(),
            std::ptr::null(),
        ];
        check(unsafe { sys::mpv_command(self.handle, args.as_ptr()) })
    }

    pub fn set_track(&self, kind: TrackKind, mpv_id: Option<i64>) -> Result<(), PlayerError> {
        let prop = match kind {
            TrackKind::Video => c"vid",
            TrackKind::Audio => c"aid",
            TrackKind::Subtitle => c"sid",
        };
        let owned;
        let value_ptr = match mpv_id {
            Some(id) => {
                owned = CString::new(id.to_string())
                    .map_err(|e| PlayerError::Mpv(format!("bad track id: {e}")))?;
                owned.as_ptr()
            }
            None => c"no".as_ptr(),
        };
        check(unsafe { sys::mpv_set_property_string(self.handle, prop.as_ptr(), value_ptr) })
    }

    pub fn set_volume(&self, percent: u8) -> Result<(), PlayerError> {
        self.set_property_double(c"volume", f64::from(percent))
    }

    /// mpv `speed`: a multiplier on the normal playback rate (`1.0` is
    /// normal, mpv's default). Used for hold-to-speed-up.
    pub fn set_speed(&self, rate: f64) -> Result<(), PlayerError> {
        self.set_property_double(c"speed", rate)
    }

    /// Applies `opts` to mpv's subtitle-rendering properties
    /// (docs/OVERVIEW.md §6). Idempotent: safe to call on init and again on
    /// every settings change.
    /// mpv accepts `sub-scale`/`sub-pos`/`sub-bold`/`sub-back-color`
    /// regardless of whether a subtitle track is currently selected, so
    /// this never needs to check track state first.
    pub fn set_subtitle_style(&self, opts: &SubtitleStyle) -> Result<(), PlayerError> {
        self.set_property_double(c"sub-scale", opts.scale)?;

        if let Some(pos) = opts.pos {
            // sub-pos is documented (mpv manual, "Subtitles") as 0-150,
            // percent of screen height from the top -- clamp defensively so
            // a future UI bug (e.g. an off-by-one preset) can't hand mpv an
            // out-of-range value.
            self.set_property_int64(c"sub-pos", pos.clamp(0, 150))?;
        }

        self.set_property_flag(c"sub-bold", opts.bold)?;

        let color = CString::new(back_color_hex(opts.back_alpha))
            .map_err(|e| PlayerError::Mpv(format!("bad sub-back-color: {e}")))?;
        check(unsafe {
            sys::mpv_set_property_string(self.handle, c"sub-back-color".as_ptr(), color.as_ptr())
        })?;

        // Always-on legibility, not a user preference: a thin stroke and
        // soft shadow keep subtitles readable over busy frames and OSD
        // chrome. Units are mpv's style-scaled pixels (mpv manual,
        // "Subtitles"); larger values read as heavy comic-caption outlines.
        self.set_property_double(c"sub-border-size", 2.0)?;
        check(unsafe {
            sys::mpv_set_property_string(
                self.handle,
                c"sub-border-color".as_ptr(),
                c"#FF000000".as_ptr(),
            )
        })?;
        self.set_property_double(c"sub-shadow-offset", 1.5)?;
        check(unsafe {
            sys::mpv_set_property_string(
                self.handle,
                c"sub-shadow-color".as_ptr(),
                c"#AA000000".as_ptr(),
            )
        })
    }

    // Render integration (called from the app's display-link/draw path):

    /// True if mpv wants a new frame drawn.
    pub fn needs_render(&self) -> bool {
        self.needs_render.swap(false, Ordering::AcqRel)
    }

    /// Render into the currently bound FBO at the given pixel size.
    ///
    /// Must be called with the same OpenGL context current as when this
    /// `Player` was constructed (see `new`'s doc comment). Per mpv/render.h
    /// "Threading", every `mpv_render_*` call for this context (including
    /// `report_swap`) must come from the *same* thread each time — the first
    /// call to `render`/`report_swap` fixes which thread that is, and
    /// subsequent calls from any other thread trip a `debug_assert_eq!`
    /// below.
    pub fn render(&self, fbo: i32, width: i32, height: i32) -> Result<(), PlayerError> {
        let this_thread = std::thread::current().id();
        let render_thread = *self.render_thread_id.get_or_init(|| this_thread);
        debug_assert_eq!(
            this_thread, render_thread,
            "Player::render called from a different thread than the first \
             render()/report_swap() call; mpv/render.h requires a single \
             render thread with the OpenGL context current"
        );
        if self.render_ctx.is_null() {
            return Err(PlayerError::NotLoaded);
        }
        let mut fbo_desc = sys::mpv_opengl_fbo {
            fbo,
            w: width,
            h: height,
            internal_format: 0,
        };
        // Flip only for the default framebuffer (fbo == 0), whose coordinate
        // system is flipped relative to mpv's internal convention — see
        // render.h's MPV_RENDER_PARAM_FLIP_Y doc. App-owned FBOs render
        // right-side-up already.
        let mut flip_y: c_int = if fbo == 0 { 1 } else { 0 };
        let mut params = [
            sys::mpv_render_param {
                type_: sys::MPV_RENDER_PARAM_OPENGL_FBO,
                data: &mut fbo_desc as *mut sys::mpv_opengl_fbo as *mut c_void,
            },
            sys::mpv_render_param {
                type_: sys::MPV_RENDER_PARAM_FLIP_Y,
                data: &mut flip_y as *mut c_int as *mut c_void,
            },
            sys::mpv_render_param {
                type_: sys::MPV_RENDER_PARAM_INVALID,
                data: std::ptr::null_mut(),
            },
        ];
        check(unsafe { sys::mpv_render_context_render(self.render_ctx, params.as_mut_ptr()) })
    }

    /// Must be called from the same thread as `render()` — see its doc
    /// comment and mpv/render.h "Threading".
    pub fn report_swap(&self) {
        if let Some(&render_thread) = self.render_thread_id.get() {
            debug_assert_eq!(
                std::thread::current().id(),
                render_thread,
                "Player::report_swap called from a different thread than \
                 render(); mpv/render.h requires a single render thread with \
                 the OpenGL context current"
            );
        }
        if self.render_ctx.is_null() {
            return;
        }
        unsafe { sys::mpv_render_context_report_swap(self.render_ctx) };
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        // 1. Ask the event thread to stop, and wake it out of a blocking
        //    `mpv_wait_event` call (see events.rs).
        self.shutdown.store(true, Ordering::Release);
        if !self.handle.is_null() {
            unsafe { sys::mpv_wakeup(self.handle) };
        }
        if let Some(handle) = lock_ignore_poison(&self.event_thread).take() {
            let _ = handle.join();
        }

        // 2. Tear down the render context. This guarantees the update
        //    callback can never fire again, so it's now safe to reclaim the
        //    `Arc` we lent it.
        if !self.render_ctx.is_null() {
            unsafe { sys::mpv_render_context_free(self.render_ctx) };
        }
        if !self.render_cb_ctx.is_null() {
            // SAFETY: balances the `Arc::into_raw` in `Player::new`; nothing
            // can dereference `render_cb_ctx` after the free above.
            drop(unsafe { Arc::from_raw(self.render_cb_ctx) });
        }

        // 3. Destroy the core. Safe now — the event thread (the only other
        //    user of `handle`) has already joined.
        if !self.handle.is_null() {
            unsafe { sys::mpv_terminate_destroy(self.handle) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Compile-time verification of the `Send`/`Sync` claim documented above
    /// `unsafe impl Send for Player` / `unsafe impl Sync for Player`: this
    /// only compiles if `Player` really does implement both, so a future
    /// change that (accidentally) breaks the invariant those `unsafe impl`s
    /// rely on fails the build here rather than silently compiling.
    fn assert_send_sync<T: Send + Sync>() {}

    #[test]
    fn player_is_send_and_sync() {
        assert_send_sync::<Player>();
    }

    // --- FrameDropStats::total ------------------------------------------

    #[test]
    fn frame_drop_stats_total_sums_both_sides() {
        let stats = FrameDropStats {
            vo: Some(4),
            decoder: Some(2),
        };
        assert_eq!(stats.total(), Some(6));
    }

    #[test]
    fn frame_drop_stats_total_treats_one_missing_side_as_zero() {
        let stats = FrameDropStats {
            vo: Some(4),
            decoder: None,
        };
        assert_eq!(stats.total(), Some(4));
    }

    #[test]
    fn frame_drop_stats_total_is_none_when_both_sides_are_unavailable() {
        assert_eq!(FrameDropStats::default().total(), None);
    }

    #[test]
    fn escape_option_value_uses_verbatim_length_prefix() {
        assert_eq!(escape_option_value("plain"), "%5%plain");
        assert_eq!(escape_option_value(""), "%0%");
        // Commas/colons inside the value must not need their own escaping —
        // the %n% form is exact-byte-length, not delimiter-based.
        assert_eq!(escape_option_value("a,b:c"), "%5%a,b:c");
    }

    #[test]
    fn build_loadfile_options_is_just_pause_no_for_a_bare_request() {
        // `pause=no` is unconditional (see `build_loadfile_options`'s doc
        // comment), so even a bare request still produces `Some(options)`.
        // A local (non-http) URL isolates it from the network profile (see
        // `build_loadfile_options_adds_a_network_profile_only_for_http_urls`).
        let req = LoadRequest {
            url: "/local/path/video.mkv".to_string(),
            http_headers: Vec::new(),
            start_secs: None,
            external_subs: Vec::new(),
            start_paused: false,
            readahead_secs: None,
            max_bytes: None,
        };
        let options = build_loadfile_options(&req)
            .expect("no NUL bytes")
            .expect("pause=no makes every request non-empty");
        assert_eq!(options.to_str().expect("ASCII-only test input"), "pause=no");
    }

    #[test]
    fn build_loadfile_options_combines_start_headers_and_subs() {
        let req = LoadRequest {
            url: "/local/path/video.mkv".to_string(),
            http_headers: vec![("Authorization".to_string(), "Bearer tok".to_string())],
            start_secs: Some(12.5),
            external_subs: vec!["/tmp/one.srt".to_string(), "/tmp/two.srt".to_string()],
            start_paused: false,
            readahead_secs: None,
            max_bytes: None,
        };
        let options = build_loadfile_options(&req)
            .expect("no NUL bytes")
            .expect("non-empty request should produce Some(options)");
        let options = options.to_str().expect("ASCII-only test input");

        assert!(options.contains("start=+12.5"), "{options}");
        assert!(
            options.contains("http-header-fields=%25%Authorization: Bearer tok"),
            "{options}"
        );
        assert!(
            options.contains("sub-file=%12%/tmp/one.srt,%12%/tmp/two.srt"),
            "{options}"
        );
    }

    #[test]
    fn build_loadfile_options_passes_through_start_secs_for_local_and_network() {
        // Regression guard: `start=+X` flows through unconditionally for
        // every source, with no `hr-seek=no` sibling (see
        // `build_loadfile_options`'s doc comment for why).
        for url in ["/local/path/video.mkv", "https://example.invalid/video"] {
            let req = LoadRequest {
                url: url.to_string(),
                http_headers: Vec::new(),
                start_secs: Some(2.0),
                external_subs: Vec::new(),
                start_paused: false,
                readahead_secs: None,
                max_bytes: None,
            };
            let options = build_loadfile_options(&req)
                .expect("no NUL bytes")
                .expect("non-empty");
            let options = options.to_str().expect("ASCII-only test input");
            assert!(options.contains("start=+2"), "{url}: {options}");
            assert!(
                !options.contains("hr-seek"),
                "hr-seek=no was found not to affect the `start` seek -- see \
                 build_loadfile_options's doc comment; it must not be emitted, {url}: {options}"
            );
        }
    }

    #[test]
    fn build_loadfile_options_adds_a_network_profile_only_for_http_urls() {
        let network = LoadRequest {
            url: "https://example.invalid/video".to_string(),
            http_headers: Vec::new(),
            start_secs: None,
            external_subs: Vec::new(),
            start_paused: false,
            readahead_secs: None,
            max_bytes: None,
        };
        let options = build_loadfile_options(&network)
            .expect("no NUL bytes")
            .expect("non-empty");
        let options = options.to_str().expect("ASCII-only test input");
        assert!(options.contains("demuxer-lavf-buffersize="), "{options}");
        assert!(options.contains("stream-buffer-size="), "{options}");
        assert!(
            options.contains("demuxer-lavf-probe-info=nostreams"),
            "{options}"
        );

        let local = LoadRequest {
            url: "/local/path/video.mkv".to_string(),
            ..network
        };
        let options = build_loadfile_options(&local)
            .expect("no NUL bytes")
            .expect("non-empty");
        assert_eq!(
            options.to_str().expect("ASCII-only test input"),
            "pause=no",
            "a local path must not get the network profile's buffer overrides"
        );
    }

    #[test]
    fn build_loadfile_options_start_paused_emits_pause_yes() {
        let req = LoadRequest {
            url: "/local/path/video.mkv".to_string(),
            http_headers: Vec::new(),
            start_secs: None,
            external_subs: Vec::new(),
            start_paused: true,
            readahead_secs: None,
            max_bytes: None,
        };
        let options = build_loadfile_options(&req)
            .expect("no NUL bytes")
            .expect("non-empty");
        assert_eq!(
            options.to_str().expect("ASCII-only test input"),
            "pause=yes",
            "start_paused must flip the per-file pause override, nothing else"
        );
    }

    #[test]
    fn build_loadfile_options_readahead_cap_emits_for_any_source() {
        // The cap is a caller decision (dark preload), not a network-profile
        // member -- it must appear for local paths too, so tests against
        // corpus files exercise the same option surface.
        for url in ["/local/path/video.mkv", "http://example.invalid/video"] {
            let req = LoadRequest {
                url: url.to_string(),
                http_headers: Vec::new(),
                start_secs: None,
                external_subs: Vec::new(),
                start_paused: false,
                readahead_secs: Some(15),
                max_bytes: None,
            };
            let options = build_loadfile_options(&req)
                .expect("no NUL bytes")
                .expect("non-empty");
            let options = options.to_str().expect("ASCII-only test input");
            assert!(
                options.contains("demuxer-readahead-secs=15"),
                "{url}: {options}"
            );
        }
    }

    /// The byte-ceiling sibling of the read-ahead cap above -- same "must
    /// appear for local paths too" reasoning.
    #[test]
    fn build_loadfile_options_max_bytes_cap_emits_for_any_source() {
        for url in ["/local/path/video.mkv", "http://example.invalid/video"] {
            let req = LoadRequest {
                url: url.to_string(),
                http_headers: Vec::new(),
                start_secs: None,
                external_subs: Vec::new(),
                start_paused: true,
                readahead_secs: Some(15),
                max_bytes: Some(6 * 1024 * 1024),
            };
            let options = build_loadfile_options(&req)
                .expect("no NUL bytes")
                .expect("non-empty");
            let options = options.to_str().expect("ASCII-only test input");
            assert!(
                options.contains("demuxer-max-bytes=6291456"),
                "{url}: {options}"
            );
        }
    }

    /// The byte cap must appear on a preload-shaped
    /// request and be entirely absent from a normal-playback-shaped one --
    /// a normal load must keep relying on `INIT_OPTIONS`' global 150MiB
    /// (unbounded per-file), never the preload's ~6MB ceiling.
    #[test]
    fn build_loadfile_options_max_bytes_cap_absent_for_a_normal_request() {
        let preload = LoadRequest {
            url: "/local/path/video.mkv".to_string(),
            http_headers: Vec::new(),
            start_secs: None,
            external_subs: Vec::new(),
            start_paused: true,
            readahead_secs: Some(15),
            max_bytes: Some(6 * 1024 * 1024),
        };
        let options = build_loadfile_options(&preload)
            .expect("no NUL bytes")
            .expect("non-empty");
        assert!(
            options
                .to_str()
                .expect("ASCII-only test input")
                .contains("demuxer-max-bytes="),
            "a preload request must cap demuxer-max-bytes"
        );

        let normal = LoadRequest {
            start_paused: false,
            readahead_secs: None,
            max_bytes: None,
            ..preload
        };
        let options = build_loadfile_options(&normal)
            .expect("no NUL bytes")
            .expect("non-empty");
        assert!(
            !options
                .to_str()
                .expect("ASCII-only test input")
                .contains("demuxer-max-bytes"),
            "a normal (non-preload) load must not cap demuxer-max-bytes"
        );
    }

    #[test]
    fn is_network_url_recognizes_http_and_https_only() {
        assert!(is_network_url("http://example.invalid/x"));
        assert!(is_network_url("https://example.invalid/x"));
        assert!(!is_network_url("/local/path/video.mkv"));
        assert!(!is_network_url("file:///local/path/video.mkv"));
    }

    // --- subtitle style overrides -------------------------------------------

    #[test]
    fn back_color_hex_maps_alpha_to_the_aarrggbb_alpha_byte() {
        assert_eq!(back_color_hex(0.0), "#00000000");
        assert_eq!(back_color_hex(1.0), "#FF000000");
        // 50% should round to 0x80 (128/255 ~= 0.502) -- exact midpoint
        // rounding, not truncation, so a UI showing "50%" doesn't silently
        // apply e.g. 49%.
        assert_eq!(back_color_hex(0.5), "#80000000");
    }

    #[test]
    fn back_color_hex_clamps_out_of_range_alpha() {
        assert_eq!(back_color_hex(-1.0), "#00000000");
        assert_eq!(back_color_hex(2.0), "#FF000000");
    }

    #[test]
    fn subtitle_style_default_leaves_pos_unset() {
        // Default must not force a specific sub-pos on every player init --
        // `set_subtitle_style` treats `pos: None` as "leave mpv's current
        // value alone" (see its doc comment); a `Some` default here would
        // silently override any future default-position change on mpv's
        // side.
        assert_eq!(SubtitleStyle::default().pos, None);
        assert_eq!(SubtitleStyle::default().scale, 1.0);
        assert!(!SubtitleStyle::default().bold);
    }
}
