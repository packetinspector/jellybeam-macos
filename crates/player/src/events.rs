//! The dedicated mpv event-loop thread.
//!
//! mpv's own docs (client.h, `mpv_set_wakeup_callback`) describe the pattern
//! used here as the simplest correct way to dispatch events off the calling
//! thread: "spawn a thread that does nothing but call mpv_wait_event() in a
//! loop and dispatches the result". `mpv_wait_event(ctx, -1.0)` already
//! blocks on mpv's internal wakeup mechanism, so no separate
//! `mpv_set_wakeup_callback` registration is needed for this shape; shutdown
//! is driven by `Player::drop` calling `mpv_wakeup()` after setting the
//! `shutdown` flag (see client.h: "If no thread is waiting, the next
//! mpv_wait_event() call will return immediately").

use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TrySendError;

use crate::node::{json_to_tracks, node_to_json};
use crate::sys;
use crate::{PlayerEvent, Track};

/// `PlayerEvent::Position` is coalesced to roughly this rate (~4 Hz).
const POSITION_THROTTLE: Duration = Duration::from_millis(250);

#[derive(Default)]
struct LoadState {
    duration_secs: Option<f64>,
    tracks: Option<Vec<Track>>,
    /// Set once `MPV_EVENT_FILE_LOADED` has been observed for the current
    /// file. For ordinary media this is redundant with `duration_secs`
    /// eventually resolving; for indefinite/live streams (mpv never
    /// resolves `duration` — `MPV_FORMAT_NONE`) this is the signal
    /// `maybe_send_loaded` uses to fire `Loaded` with `duration_secs:
    /// f64::INFINITY` instead of waiting forever.
    file_loaded: bool,
    loaded_sent: bool,
}

#[derive(Default)]
struct BufferState {
    core_idle: bool,
    paused_for_cache: bool,
    /// Last seen user-facing `pause` -- `send_buffering` needs it to tell
    /// "core idle because the user paused" apart from "core idle because
    /// we're starved" (see its doc comment).
    paused: bool,
    /// Last seen `eof-reached` -- kept for the pause-flip diagnostic log
    /// below (an unprompted pause with `eof_reached=true` mid-file is the
    /// premature-EOF-on-connection-drop signature; see `INIT_OPTIONS`'
    /// `stream-lavf-o` reconnect entry in lib.rs) and for `send_buffering`'s
    /// at-EOF exclusion.
    eof_reached: bool,
    /// Last observed `cache-buffering-state` (0-100). Streamed via property
    /// observation so the buffering overlay shows the fill PROGRESSING,
    /// rather than sampling only at stall-flag flips (one stale number for
    /// the whole stall). `None` until mpv first reports it for the current
    /// file.
    buffering_pct: Option<f64>,
}

/// `blocking_send` parks until capacity; `Player::drop` `join()`s this thread
/// from the same (main) thread that drains the receiver, so an unbounded park
/// here deadlocks teardown. Bail out as soon as shutdown is requested.
fn send_event(tx: &mpsc::Sender<PlayerEvent>, shutdown: &AtomicBool, mut ev: PlayerEvent) {
    loop {
        match tx.try_send(ev) {
            Ok(()) => return,
            Err(TrySendError::Closed(_)) => return,
            Err(TrySendError::Full(returned)) => {
                if shutdown.load(Ordering::Acquire) {
                    return; // dropping a late event beats hanging the quit path
                }
                ev = returned;
                std::thread::sleep(Duration::from_millis(2));
            }
        }
    }
}

/// Runs until `shutdown` is observed true (woken via `mpv_wakeup`) or mpv
/// itself reports `MPV_EVENT_SHUTDOWN`.
///
/// # Safety / lifetime contract
/// `handle` must stay valid for as long as this function runs. `Player`
/// upholds this by setting `shutdown`, calling `mpv_wakeup`, and `join`ing
/// this thread *before* calling `mpv_terminate_destroy` in its `Drop` impl.
pub(crate) fn run_event_loop(
    handle: *mut sys::mpv_handle,
    tx: mpsc::Sender<PlayerEvent>,
    shutdown: Arc<AtomicBool>,
) {
    let mut load_state = LoadState::default();
    let mut buffer_state = BufferState::default();
    let mut last_position_sent: Option<Instant> = None;

    loop {
        if shutdown.load(Ordering::Acquire) {
            return;
        }

        // SAFETY: `handle` is valid per the contract above. mpv guarantees
        // the returned pointer is never NULL and stays valid until the next
        // `mpv_wait_event` call (or handle destruction) — we finish reading
        // everything reachable from `event` before looping back here.
        let event = unsafe { &*sys::mpv_wait_event(handle, -1.0) };

        if shutdown.load(Ordering::Acquire) {
            return;
        }

        match event.event_id {
            sys::MPV_EVENT_NONE => {}
            sys::MPV_EVENT_SHUTDOWN => return,
            sys::MPV_EVENT_START_FILE => {
                // Reset per-file state; `Loaded` fires again once the new
                // file's duration + track-list are both known. The stale
                // buffering fill and EOF flag must not leak across files
                // either -- a new load starting while the previous file's
                // last-seen pct was 100 would otherwise show "Buffering...
                // 100%" through the whole open (the reported symptom's
                // second half); flag properties re-emit on change, so only
                // the value-carrying/latched ones need manual reset.
                load_state = LoadState::default();
                buffer_state.buffering_pct = None;
                buffer_state.eof_reached = false;
            }
            sys::MPV_EVENT_END_FILE => handle_end_file(event, &tx, &shutdown),
            sys::MPV_EVENT_LOG_MESSAGE => handle_log_message(event, &tx, &shutdown),
            sys::MPV_EVENT_FILE_LOADED => {
                handle_file_loaded(handle, &tx, &mut load_state, &shutdown)
            }
            sys::MPV_EVENT_PROPERTY_CHANGE => {
                if event.data.is_null() {
                    continue;
                }
                // SAFETY: MPV_EVENT_PROPERTY_CHANGE's data is
                // `mpv_event_property*` (client.h).
                let prop = unsafe { &*(event.data as *const sys::mpv_event_property) };
                // SAFETY: `prop` borrows from `event`, which client.h
                // documents as valid until the next `mpv_wait_event` call;
                // the `cstr_in` borrow (and every use of `name` below) stays
                // within this loop iteration.
                let name = unsafe { cstr_in(prop, prop.name) };
                handle_property_change(
                    handle,
                    name,
                    prop,
                    &tx,
                    &mut load_state,
                    &mut buffer_state,
                    &mut last_position_sent,
                    &shutdown,
                );
            }
            _ => {}
        }
    }
}

fn handle_end_file(event: &sys::mpv_event, tx: &mpsc::Sender<PlayerEvent>, shutdown: &AtomicBool) {
    if event.data.is_null() {
        return;
    }
    // SAFETY: MPV_EVENT_END_FILE's data is `mpv_event_end_file*` (client.h).
    let end_file = unsafe { &*(event.data as *const sys::mpv_event_end_file) };
    if end_file.reason == sys::MPV_END_FILE_REASON_ERROR {
        send_event(
            tx,
            shutdown,
            PlayerEvent::Error(mpv_error_string_owned(end_file.error)),
        );
    }
}

fn handle_log_message(
    event: &sys::mpv_event,
    tx: &mpsc::Sender<PlayerEvent>,
    shutdown: &AtomicBool,
) {
    if event.data.is_null() {
        return;
    }
    // SAFETY: MPV_EVENT_LOG_MESSAGE's data is `mpv_event_log_message*`
    // (client.h).
    let log = unsafe { &*(event.data as *const sys::mpv_event_log_message) };
    if log.log_level <= sys::MPV_LOG_LEVEL_ERROR {
        // SAFETY: `prefix`/`text` borrow from `log`, which borrows from
        // `event`, valid until the next `mpv_wait_event` call (client.h);
        // both are only used within this function call.
        let prefix = unsafe { cstr_in(log, log.prefix) };
        let text = unsafe { cstr_in(log, log.text) }.trim_end();
        send_event(
            tx,
            shutdown,
            PlayerEvent::Error(format!("{prefix}: {text}")),
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn handle_property_change(
    handle: *mut sys::mpv_handle,
    name: &str,
    prop: &sys::mpv_event_property,
    tx: &mpsc::Sender<PlayerEvent>,
    load_state: &mut LoadState,
    buffer_state: &mut BufferState,
    last_position_sent: &mut Option<Instant>,
    shutdown: &AtomicBool,
) {
    match name {
        "pause" => {
            if let Some(paused) = read_flag(prop) {
                // A pause=true arriving with eof_reached=true that the user
                // didn't ask for is `keep-open` reacting to (possibly
                // premature) end-of-stream -- lets a stall be diagnosed
                // from jellybeam.log alone.
                tracing::info!(
                    paused,
                    eof_reached = buffer_state.eof_reached,
                    paused_for_cache = buffer_state.paused_for_cache,
                    core_idle = buffer_state.core_idle,
                    "mpv pause flip"
                );
                buffer_state.paused = paused;
                send_event(tx, shutdown, PlayerEvent::PauseChanged { paused });
                // A pause flip changes `send_buffering`'s verdict (core-idle
                // during a user pause is NOT buffering) -- re-derive so the
                // overlay clears/appears in the same beat as the pause.
                send_buffering(handle, buffer_state, tx, shutdown);
            }
        }
        "time-pos" => {
            if let Some(secs) = read_double(prop) {
                let now = Instant::now();
                let should_send = match *last_position_sent {
                    Some(t) => now.duration_since(t) >= POSITION_THROTTLE,
                    None => true,
                };
                if should_send {
                    *last_position_sent = Some(now);
                    send_event(tx, shutdown, PlayerEvent::Position { secs });
                }
            }
        }
        "duration" => {
            if let Some(secs) = read_double(prop) {
                load_state.duration_secs = Some(secs);
                maybe_send_loaded(load_state, tx, shutdown);
            }
        }
        "track-list" => {
            if prop.format == sys::MPV_FORMAT_NODE && !prop.data.is_null() {
                // SAFETY: format tag says `data` is `mpv_node*`, owned by
                // this event (valid until the next `mpv_wait_event` call —
                // we finish converting it to owned data below well before
                // that).
                let node = unsafe { &*(prop.data as *const sys::mpv_node) };
                let json = unsafe { node_to_json(node) };
                let tracks = json_to_tracks(&json);
                load_state.tracks = Some(tracks.clone());
                send_event(tx, shutdown, PlayerEvent::TracksChanged(tracks));
                maybe_send_loaded(load_state, tx, shutdown);
            }
        }
        "chapter" => {
            if let Some(index) = read_int64(prop) {
                let title = read_chapter_title(handle, index);
                send_event(tx, shutdown, PlayerEvent::ChapterChanged { index, title });
            }
        }
        "core-idle" => {
            if let Some(v) = read_flag(prop) {
                buffer_state.core_idle = v;
                send_buffering(handle, buffer_state, tx, shutdown);
            }
        }
        "paused-for-cache" => {
            if let Some(v) = read_flag(prop) {
                buffer_state.paused_for_cache = v;
                send_buffering(handle, buffer_state, tx, shutdown);
            }
        }
        "eof-reached" if read_flag(prop) == Some(true) => {
            buffer_state.eof_reached = true;
            send_event(tx, shutdown, PlayerEvent::EndOfFile);
        }
        "eof-reached" => {
            buffer_state.eof_reached = false;
        }
        "cache-buffering-state" => {
            if let Some(pct) = read_double(prop) {
                buffer_state.buffering_pct = Some(pct);
                // Re-send on every fill change so the overlay's number
                // actually progresses 0..100 during a rebuffer (see
                // `BufferState::buffering_pct`).
                send_buffering(handle, buffer_state, tx, shutdown);
            }
        }
        _ => {}
    }
}

/// Handles `MPV_EVENT_FILE_LOADED`: marks the current file loaded, and — if
/// no "duration" property-change notification has reached us yet — actively
/// queries mpv's current `duration` value rather than trusting only the
/// (separately-dispatched, ordering-not-guaranteed) property-observation
/// stream. mpv typically resolves `duration` before `FILE_LOADED`, but the
/// *notification* for it is a separate queued event not guaranteed to
/// arrive first, so relying only on "have we processed a duration
/// property-change event" would misclassify ordinary files as
/// indefinite/live purely due to event-ordering luck. Querying directly
/// sidesteps the race; only a genuinely indefinite/live stream falls
/// through to `maybe_send_loaded`'s `f64::INFINITY` fallback.
fn handle_file_loaded(
    handle: *mut sys::mpv_handle,
    tx: &mpsc::Sender<PlayerEvent>,
    load_state: &mut LoadState,
    shutdown: &AtomicBool,
) {
    load_state.file_loaded = true;
    if load_state.duration_secs.is_none() {
        load_state.duration_secs = read_duration_property(handle);
    }
    maybe_send_loaded(load_state, tx, shutdown);
}

/// Actively reads mpv's current `duration` property value (as opposed to
/// waiting for a property-change notification for it) — see the doc
/// comment on `handle_file_loaded`, the only caller.
fn read_duration_property(handle: *mut sys::mpv_handle) -> Option<f64> {
    let mut value: f64 = 0.0;
    // SAFETY: `handle` valid per this module's contract; `value` is a
    // correctly-typed out-param for MPV_FORMAT_DOUBLE (mirrors
    // `read_buffering_percent` above).
    let ret = unsafe {
        sys::mpv_get_property(
            handle,
            c"duration".as_ptr(),
            sys::MPV_FORMAT_DOUBLE,
            &mut value as *mut f64 as *mut c_void,
        )
    };
    if ret >= 0 {
        Some(value)
    } else {
        None
    }
}

/// Fires `PlayerEvent::Loaded` once per file, as soon as enough state is
/// known:
/// - track-list is always required;
/// - `duration_secs` is used if mpv has resolved it;
/// - otherwise (indefinite/live stream), it waits for `MPV_EVENT_FILE_LOADED`
///   (`load_state.file_loaded`) and then fires with `duration_secs:
///   f64::INFINITY` — see the `PlayerEvent::Loaded` doc comment for the
///   semantics callers should expect from that value.
fn maybe_send_loaded(
    load_state: &mut LoadState,
    tx: &mpsc::Sender<PlayerEvent>,
    shutdown: &AtomicBool,
) {
    if load_state.loaded_sent {
        return;
    }
    let Some(tracks) = load_state.tracks.clone() else {
        return;
    };
    let duration_secs = match load_state.duration_secs {
        Some(secs) => secs,
        None if load_state.file_loaded => f64::INFINITY,
        None => return,
    };
    load_state.loaded_sent = true;
    send_event(
        tx,
        shutdown,
        PlayerEvent::Loaded {
            duration_secs,
            tracks,
        },
    );
}

/// `active` reflects mpv's `paused-for-cache` OR `core-idle` -- but
/// `core-idle` is true during ANY halt, including a plain user pause and
/// `keep-open`'s at-EOF pause, so it only counts as *buffering* when
/// neither of those explains the idle (otherwise every pause/EOF would
/// wear a bogus "Buffering... 100%" overlay). `percent` reads mpv's
/// `cache-buffering-state` when available, falling back to a coarse 0/100
/// derived from `active` rather than fabricating a number mpv never gave us.
fn send_buffering(
    handle: *mut sys::mpv_handle,
    state: &BufferState,
    tx: &mpsc::Sender<PlayerEvent>,
    shutdown: &AtomicBool,
) {
    let active = state.paused_for_cache || (state.core_idle && !state.paused && !state.eof_reached);
    // Prefer the OBSERVED fill level (streamed per change, so the overlay
    // progresses); the one-shot read remains only as a fallback for the
    // window before the first property-change event of a file arrives.
    let percent = state
        .buffering_pct
        .or_else(|| read_buffering_percent(handle))
        .unwrap_or(if active { 0.0 } else { 100.0 });
    send_event(tx, shutdown, PlayerEvent::Buffering { active, percent });
}

fn read_buffering_percent(handle: *mut sys::mpv_handle) -> Option<f64> {
    let mut value: f64 = 0.0;
    // SAFETY: `handle` valid per this module's contract; `value` is a
    // correctly-typed out-param for MPV_FORMAT_DOUBLE.
    let ret = unsafe {
        sys::mpv_get_property(
            handle,
            c"cache-buffering-state".as_ptr(),
            sys::MPV_FORMAT_DOUBLE,
            &mut value as *mut f64 as *mut c_void,
        )
    };
    if ret >= 0 {
        Some(value)
    } else {
        None
    }
}

fn read_chapter_title(handle: *mut sys::mpv_handle, index: i64) -> Option<String> {
    if index < 0 {
        return None;
    }
    let prop_name = CString::new(format!("chapter-list/{index}/title")).ok()?;
    // SAFETY: `handle` valid per this module's contract; `mpv_get_property_string`
    // returns either NULL or an mpv-allocated NUL-terminated string, which we
    // free below per client.h's contract for that function.
    let raw = unsafe { sys::mpv_get_property_string(handle, prop_name.as_ptr()) };
    if raw.is_null() {
        return None;
    }
    let title = unsafe { CStr::from_ptr(raw) }
        .to_string_lossy()
        .into_owned();
    unsafe { sys::mpv_free(raw as *mut c_void) };
    Some(title)
}

fn read_flag(prop: &sys::mpv_event_property) -> Option<bool> {
    if prop.format != sys::MPV_FORMAT_FLAG || prop.data.is_null() {
        return None;
    }
    // SAFETY: format tag confirms `data` points at a C `int` (client.h
    // MPV_FORMAT_FLAG).
    Some(unsafe { *(prop.data as *const c_int) } != 0)
}

fn read_double(prop: &sys::mpv_event_property) -> Option<f64> {
    if prop.format != sys::MPV_FORMAT_DOUBLE || prop.data.is_null() {
        return None;
    }
    // SAFETY: format tag confirms `data` points at an `f64`.
    Some(unsafe { *(prop.data as *const f64) })
}

fn read_int64(prop: &sys::mpv_event_property) -> Option<i64> {
    if prop.format != sys::MPV_FORMAT_INT64 || prop.data.is_null() {
        return None;
    }
    // SAFETY: format tag confirms `data` points at an `i64`.
    Some(unsafe { *(prop.data as *const i64) })
}

/// # Safety
/// `ptr` must be NULL or point at a NUL-terminated C string that stays valid
/// for reads for all of `'a`. Callers in this module bound `'a` to the
/// borrow of the owning `mpv_event`/`mpv_event_property`, which client.h
/// documents as valid until the next `mpv_wait_event` call.
unsafe fn cstr_to_str<'a>(ptr: *const c_char) -> &'a str {
    if ptr.is_null() {
        return "";
    }
    // SAFETY: forwarded from this fn's own caller-upheld contract above.
    unsafe { CStr::from_ptr(ptr) }.to_str().unwrap_or("")
}

/// Owner-witness form of [`cstr_to_str`]: binds the returned `&str`'s
/// lifetime to the borrow of `_owner` (the mpv event struct `ptr` was read
/// out of) instead of letting the caller pick `'a` freely.
///
/// # Safety
/// Same contract as [`cstr_to_str`]: `ptr` must be NULL or a NUL-terminated
/// C string valid for reads for as long as `_owner` is borrowed.
unsafe fn cstr_in<T>(_owner: &T, ptr: *const c_char) -> &str {
    // SAFETY: forwarded from this fn's own caller-upheld contract above.
    unsafe { cstr_to_str(ptr) }
}

fn mpv_error_string_owned(code: c_int) -> String {
    // SAFETY: mpv_error_string always returns a valid, static, non-NULL C
    // string (client.h).
    let ptr = unsafe { sys::mpv_error_string(code) };
    // SAFETY: per client.h, `mpv_error_string`'s returned pointer is
    // `'static` (a compiled-in string table), so the plain form is fine
    // here — there's no owning event to bind the lifetime to.
    unsafe { cstr_to_str(ptr) }.to_string()
}

#[cfg(test)]
mod tests {
    //! Unit tests for the `Loaded`/indefinite-stream state machine, driving
    //! `maybe_send_loaded` directly — the pure internal seam between mpv
    //! event plumbing and `PlayerEvent` delivery.
    //!
    //! `handle_file_loaded` itself does a live `mpv_get_property` call, so
    //! it can't be called with a fake handle here without segfaulting.
    //! These tests instead simulate exactly what it does to `LoadState` —
    //! set `file_loaded = true`, and set `duration_secs` if the (simulated)
    //! active query resolved one.
    use super::*;
    use crate::TrackKind;

    fn fake_video_track() -> Track {
        Track {
            mpv_id: 0,
            kind: TrackKind::Video,
            title: None,
            lang: None,
            codec: None,
            default: false,
            selected: false,
            forced: false,
        }
    }

    #[test]
    fn indefinite_stream_fires_loaded_with_infinite_duration_once_file_loaded() {
        let (tx, mut rx) = mpsc::channel(8);
        let shutdown = AtomicBool::new(false);
        // track-list resolves, but duration never does (as for a live
        // stream, where mpv reports MPV_FORMAT_NONE) — nothing should fire
        // yet.
        let mut load_state = LoadState {
            tracks: Some(vec![fake_video_track()]),
            ..LoadState::default()
        };
        maybe_send_loaded(&mut load_state, &tx, &shutdown);
        assert!(
            rx.try_recv().is_err(),
            "must not fire Loaded before MPV_EVENT_FILE_LOADED when duration is unknown"
        );

        // mpv reports the file loaded; the (simulated) active duration
        // query still comes back empty (genuinely indefinite/live stream),
        // so Loaded should fire now with an infinite duration.
        load_state.file_loaded = true;
        maybe_send_loaded(&mut load_state, &tx, &shutdown);

        match rx.try_recv() {
            Ok(PlayerEvent::Loaded {
                duration_secs,
                tracks,
            }) => {
                assert!(
                    duration_secs.is_infinite() && duration_secs.is_sign_positive(),
                    "expected +INFINITY, got {duration_secs}"
                );
                assert_eq!(tracks.len(), 1);
            }
            other => panic!("expected PlayerEvent::Loaded, got {other:?}"),
        }

        // A second MPV_EVENT_FILE_LOADED (e.g. a spurious duplicate) must
        // not re-fire Loaded.
        maybe_send_loaded(&mut load_state, &tx, &shutdown);
        assert!(
            rx.try_recv().is_err(),
            "Loaded must only fire once per file"
        );
    }

    #[test]
    fn finite_duration_fires_without_waiting_for_file_loaded() {
        let (tx, mut rx) = mpsc::channel(8);
        let shutdown = AtomicBool::new(false);
        // Ordinary (non-live) file: duration resolves via the "duration"
        // property observer before/without any MPV_EVENT_FILE_LOADED
        // bookkeeping happening in this test — Loaded must still fire as
        // soon as both duration and tracks are known.
        let mut load_state = LoadState {
            tracks: Some(vec![fake_video_track()]),
            duration_secs: Some(42.5),
            ..LoadState::default()
        };
        maybe_send_loaded(&mut load_state, &tx, &shutdown);

        match rx.try_recv() {
            Ok(PlayerEvent::Loaded { duration_secs, .. }) => {
                assert_eq!(duration_secs, 42.5);
            }
            other => panic!("expected PlayerEvent::Loaded, got {other:?}"),
        }
    }

    #[test]
    fn finite_file_loaded_before_duration_notification_still_reports_finite_duration() {
        // Regression coverage for the FILE_LOADED-vs-duration-notification
        // race `handle_file_loaded`'s doc comment describes: this must NOT
        // be misclassified as indefinite/live just because the
        // notification-driven `duration_secs` field hadn't been set yet.
        let (tx, mut rx) = mpsc::channel(8);
        let shutdown = AtomicBool::new(false);
        let mut load_state = LoadState {
            tracks: Some(vec![fake_video_track()]),
            ..LoadState::default()
        };

        // Simulates `handle_file_loaded`'s active `mpv_get_property`
        // query resolving a real, finite duration.
        load_state.file_loaded = true;
        load_state.duration_secs = Some(123.4);
        maybe_send_loaded(&mut load_state, &tx, &shutdown);

        match rx.try_recv() {
            Ok(PlayerEvent::Loaded { duration_secs, .. }) => {
                assert_eq!(
                    duration_secs, 123.4,
                    "a finite duration resolved at FILE_LOADED time must not be \
                     overridden by the indefinite-stream INFINITY fallback"
                );
            }
            other => panic!("expected PlayerEvent::Loaded, got {other:?}"),
        }
    }

    #[test]
    fn no_tracks_yet_never_fires_even_after_file_loaded() {
        let (tx, mut rx) = mpsc::channel(8);
        let shutdown = AtomicBool::new(false);
        let mut load_state = LoadState {
            file_loaded: true,
            ..LoadState::default()
        };

        // track-list hasn't arrived at all yet; FILE_LOADED alone must not
        // be enough.
        maybe_send_loaded(&mut load_state, &tx, &shutdown);
        assert!(
            rx.try_recv().is_err(),
            "must not fire Loaded before track-list is known"
        );
    }

    /// Runs `send_event` on a helper thread and returns whether it completed
    /// within `budget` -- a real `blocking_send`-style hang would never
    /// signal `done`, so this is how the tests below prove "returns promptly"
    /// without risking a wedged test process.
    fn completes_within(budget: Duration, body: impl FnOnce() + Send + 'static) -> bool {
        let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
        std::thread::spawn(move || {
            body();
            let _ = done_tx.send(());
        });
        done_rx.recv_timeout(budget).is_ok()
    }

    /// Pins: `send_event` (try_send + shutdown-aware backoff) returns
    /// promptly when the channel is full and shutdown was requested,
    /// instead of parking the event thread forever and deadlocking
    /// `Player::drop`'s `join()`.
    #[test]
    fn send_event_returns_promptly_when_full_and_shutdown_set() {
        // Capacity 1, receiver kept alive so the channel is Full (not Closed).
        let (tx, _rx) = mpsc::channel::<PlayerEvent>(1);
        assert!(
            tx.try_send(PlayerEvent::Position { secs: 1.0 }).is_ok(),
            "prime the single slot so the next send would block"
        );

        let completed = completes_within(Duration::from_secs(5), move || {
            let shutdown = AtomicBool::new(true);
            send_event(&tx, &shutdown, PlayerEvent::Position { secs: 2.0 });
        });
        assert!(
            completed,
            "send_event hung on a full channel with shutdown set (the GEL-304 deadlock)"
        );
    }

    /// A dropped receiver (`TrySendError::Closed`) must also return
    /// immediately -- even without shutdown -- rather than spin.
    #[test]
    fn send_event_returns_promptly_when_receiver_dropped() {
        let (tx, rx) = mpsc::channel::<PlayerEvent>(1);
        drop(rx);

        let completed = completes_within(Duration::from_secs(5), move || {
            let shutdown = AtomicBool::new(false);
            send_event(&tx, &shutdown, PlayerEvent::Position { secs: 3.0 });
        });
        assert!(
            completed,
            "send_event must return once the receiver is gone (channel Closed)"
        );
    }

    /// Guards the other half of the fix: on a full channel with shutdown NOT
    /// set, `send_event` must still back-pressure (park), not silently drop
    /// the event -- it only bails once shutdown is requested. Here it stays
    /// blocked until a slot is drained, then delivers.
    #[test]
    fn send_event_backpressures_until_drained_when_not_shutting_down() {
        let (tx, mut rx) = mpsc::channel::<PlayerEvent>(1);
        assert!(tx.try_send(PlayerEvent::Position { secs: 1.0 }).is_ok());

        let shutdown = Arc::new(AtomicBool::new(false));
        let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
        let tx_bg = tx.clone();
        let sd = shutdown.clone();
        std::thread::spawn(move || {
            send_event(&tx_bg, &sd, PlayerEvent::Position { secs: 2.0 });
            let _ = done_tx.send(());
        });

        // Still blocked while the channel is full and shutdown is unset.
        assert!(
            done_rx.recv_timeout(Duration::from_millis(200)).is_err(),
            "send_event must back-pressure (not drop) while full and not shutting down"
        );

        // Drain one slot; the parked send now completes.
        assert!(matches!(rx.try_recv(), Ok(PlayerEvent::Position { .. })));
        assert!(
            done_rx.recv_timeout(Duration::from_secs(5)).is_ok(),
            "send_event should deliver once capacity frees up"
        );
    }
}
