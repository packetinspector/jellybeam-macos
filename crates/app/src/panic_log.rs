//! Crash-survivability: turns a stray panic inside a keystroke/player-event/
//! mirror-change handler from "the whole app aborts silently, with nothing
//! but a macOS crash reporter `.ips` file to go on" into "the app logs what
//! panicked, where, and with what backtrace, then keeps running".
//!
//! This matters because a bare SIGABRT inside gpui's `platform::mac::
//! window::handle_key_event` carries no Rust-level panic message anywhere:
//! `panic_cannot_unwind` aborts before a panic hook's default stderr-only
//! output would reach a log (Finder-launched sessions have no attached
//! terminal). [`install_panic_hook`] and [`catch_and_log`] make such a
//! crash leave an actual trail instead.

use std::io::Write;
use std::path::PathBuf;

/// `~/Library/Logs/Jellybeam` -- shared by [`panic_log_path`] (this module's
/// `panic.log`) and `main.rs::init_tracing`'s `jellybeam.log`, so both land in
/// the one place a user (or support instructions) would think to look.
pub(crate) fn log_dir() -> PathBuf {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default();
    home.join("Library/Logs/Jellybeam")
}

/// `~/Library/Logs/Jellybeam/panic.log` -- every panic this process ever sees
/// (caught by [`catch_and_log`] or not) is appended here, in addition to
/// stderr.
pub(crate) fn panic_log_path() -> PathBuf {
    log_dir().join("panic.log")
}

/// Appends `text` (expected to already end in its own newline) to
/// [`panic_log_path`], creating the directory if needed, and also prints it
/// to stderr. Best-effort: if the log file can't be created/opened, this
/// silently falls back to stderr-only rather than panicking again from
/// inside panic-handling code.
pub(crate) fn append_panic_log(text: &str) {
    let path = panic_log_path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let _ = f.write_all(text.as_bytes());
    }
    eprint!("{text}");
}

/// Extracts a human-readable message from a panic payload -- covers both
/// `&'static str` (a string-literal message) and `String` (a formatted
/// one); anything else falls back to a fixed placeholder rather than
/// silently losing the fact that a panic happened.
pub(crate) fn panic_payload_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "<non-string panic payload>".to_string()
    }
}

/// A rough, dependency-free wall-clock timestamp -- good enough to order
/// log lines without pulling in a date/time formatting crate.
fn unix_timestamp_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Installed once, first thing in `main()` -- before the tokio runtime,
/// before `Application::new()`, before anything else that could itself
/// panic. Every panic that reaches this hook -- caught by a
/// [`catch_and_log`] further up the stack or not -- gets its message,
/// source location, and a full backtrace
/// (`std::backtrace::Backtrace::force_capture`) appended to `panic.log`
/// before Rust's normal post-hook behavior continues (unwind, or an
/// immediate abort if unwinding isn't possible at that point -- e.g. across
/// AppKit's `extern "C"` event-handling boundary; see [`catch_and_log`]'s
/// doc comment). Deliberately does *not* call `std::process::abort()` or
/// otherwise change what happens after it returns -- only *installing* a
/// hook can never make a panic more or less fatal than it already was, it
/// only changes what gets recorded on the way. That's "keep the default
/// abort behavior after logging": this hook adds a durable, readable record
/// of *why*, it doesn't add or remove any abort.
pub(crate) fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        let backtrace = std::backtrace::Backtrace::force_capture();
        let location = info
            .location()
            .map(|l| l.to_string())
            .unwrap_or_else(|| "<unknown location>".to_string());
        let message = panic_payload_message(info.payload());
        let text = format!(
            "[{ts}] PANIC at {location}: {message}\nbacktrace:\n{backtrace}\n",
            ts = unix_timestamp_secs(),
        );
        append_panic_log(&text);
    }));
}

/// Runs `f`, and if it panics, logs `context` + the panic payload message
/// to `panic.log` (the hook installed by [`install_panic_hook`] has
/// already run by this point and logged the full backtrace -- this adds
/// the "which handler, and that it was recovered" context the bare hook
/// output doesn't have) and returns `None` instead of letting the panic
/// keep unwinding. Returns `Some(f()'s result)` on the non-panicking path.
///
/// Every call site wrapping a GPUI/AppKit-driven callback with this exists
/// because of one specific hazard: a panic that reaches AppKit's `extern
/// "C"` event-handling boundary (`handle_key_event` and friends, in gpui's
/// `platform/mac/window.rs`) cannot unwind across it -- Rust detects that
/// statically and aborts the whole process immediately
/// (`panic_nounwind`/`panic_cannot_unwind`) rather than continuing to
/// unwind toward whatever `catch_unwind` *would* eventually have caught
/// it, because by definition there is no such catch between an `extern
/// "C"` frame and its caller. This is the same SIGABRT hazard described
/// in this module's doc comment. Catching *here*, right at the handler-dispatch entry point
/// (`observe_keystrokes`'s closure body, each player-event/mirror-change
/// callback) -- i.e. before control returns into gpui's own dispatch code,
/// let alone AppKit's -- keeps a bug in one handler from taking the whole
/// app down: this one keystroke/event is dropped, but the app (and the
/// user's still-live playback session, if any) keeps running.
pub(crate) fn catch_and_log<F, R>(context: &str, f: F) -> Option<R>
where
    F: FnOnce() -> R + std::panic::UnwindSafe,
{
    match std::panic::catch_unwind(f) {
        Ok(value) => Some(value),
        Err(payload) => {
            let message = panic_payload_message(payload.as_ref());
            append_panic_log(&format!(
                "[{ts}] CAUGHT panic in {context}: {message} -- handler dropped, app continuing\n",
                ts = unix_timestamp_secs(),
            ));
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn panic_payload_message_handles_str_and_string_and_other() {
        let str_payload: Box<dyn std::any::Any + Send> = Box::new("boom");
        assert_eq!(panic_payload_message(str_payload.as_ref()), "boom");

        let string_payload: Box<dyn std::any::Any + Send> = Box::new("boom".to_string());
        assert_eq!(panic_payload_message(string_payload.as_ref()), "boom");

        let other_payload: Box<dyn std::any::Any + Send> = Box::new(42_i32);
        assert_eq!(
            panic_payload_message(other_payload.as_ref()),
            "<non-string panic payload>"
        );
    }

    #[test]
    fn catch_and_log_swallows_a_panic_and_returns_none() {
        // Panicking inside a `#[test]` fn prints to stderr via the default
        // (or, here, our installed) hook -- expected noise for this test,
        // not a failure signal; the assertion is on `catch_and_log`'s
        // return value; also asserts `panic_log.rs` itself doesn't
        // introduce a new double-panic -- the bug class this module
        // exists to prevent -- when the payload is a plain `&str`.
        let result: Option<()> = catch_and_log("test handler", || panic!("boom"));
        assert!(result.is_none());
    }

    #[test]
    fn catch_and_log_returns_the_value_when_f_does_not_panic() {
        let result = catch_and_log("test handler", || 1 + 1);
        assert_eq!(result, Some(2));
    }
}
