//! Option-key speed-hold monitor (native `NSEvent` `flagsChanged`
//! interception). RIGHT Option (⌥) held -> 2x, LEFT Option held -> 0.5x,
//! release -> back to 1x. Space keeps its normal tap-to-pause behavior.
//!
//! GPUI's `Modifiers` (`gpui::Modifiers`) exposes only one `alt: bool` bit
//! -- both physical Option keys collapse into the same flag before Rust
//! sees them. Only the raw `NSEvent.modifierFlags` on a
//! `NSEventTypeFlagsChanged` event distinguishes which side is down, via
//! long-stable but undocumented device-dependent bits also hardcoded by
//! Chromium (`keyboard_code_conversion_mac.mm`) and Qt's Cocoa plugin:
//!
//! - `0x0000_0020` -- left Option physically held (`NX_DEVICELALTKEYMASK`).
//! - `0x0000_0040` -- right Option physically held (`NX_DEVICERALTKEYMASK`).
//!
//! These are a refinement of the public, device-independent
//! `NSEventModifierFlagOption` (`1 << 19`) bit both keys set identically --
//! the one GPUI's `Modifiers::alt` surfaces, and no more.
//!
//! Uses `addLocalMonitorForEventsMatchingMask(_:handler:)` (local, not
//! global) so speed-hold only reacts while this app's window is frontmost
//! -- see `main.rs`'s window-activation hook for the gap that leaves
//! (Option released after Cmd+Tabbing away). Bridges into `Root` the same
//! way `now_playing.rs` does (see `main.rs::spawn_option_speed_task`): an
//! `RcBlock` handler forwards samples over an unbounded channel to a
//! `cx.spawn`'d task, since the raw NSEvent callback has no `cx` of its own.
//!
//! The handler must return the event unmodified -- a local monitor's
//! return value is what gets dispatched onward, so anything else would
//! swallow the event and break ordinary Option-key use (dead-key input,
//! ⌥-modified menu items) elsewhere in the app.

use std::ptr::NonNull;

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::MainThreadMarker;
use objc2_app_kit::{NSEvent, NSEventMask};

/// `NX_DEVICELALTKEYMASK` -- see module doc.
const NX_DEVICE_LALT_KEY_MASK: u64 = 0x0000_0020;
/// `NX_DEVICERALTKEYMASK` -- see module doc.
const NX_DEVICE_RALT_KEY_MASK: u64 = 0x0000_0040;

/// Public, device-independent Shift/Control/Command bits, as raw `u64` so
/// `decide_speed_rate` stays a plain bit test. Any of these held alongside
/// Option means it's part of another shortcut, not a speed-hold gesture.
const OTHER_MODIFIER_MASK: u64 = (1 << 17) | (1 << 18) | (1 << 20);

/// One `flagsChanged` sample, forwarded to `Root::handle_option_flags_sample`.
#[derive(Debug, Clone, Copy)]
pub(crate) struct OptionFlagsSample {
    pub raw_flags: u64,
}

/// Decides the mpv playback rate for a `flagsChanged` sample. `gate_open`
/// is `Root::should_engage_option_speed_hold` (speed-boost setting,
/// fullscreen ownership, overlays closed). Stateless: safe to call on
/// every sample without tracking prior state.
///
/// - Gate closed, or no Option key down -> `1.0`.
/// - ⌘/⌃/⇧ held alongside Option -> `1.0` (not a speed-hold gesture).
/// - RIGHT Option alone -> `2.0`; LEFT Option alone -> `0.5`.
/// - Both held at once -> RIGHT wins (arbitrary, documented tie-break).
pub(crate) fn decide_speed_rate(raw_flags: u64, gate_open: bool) -> f64 {
    if !gate_open || raw_flags & OTHER_MODIFIER_MASK != 0 {
        return 1.0;
    }
    if raw_flags & NX_DEVICE_RALT_KEY_MASK != 0 {
        2.0
    } else if raw_flags & NX_DEVICE_LALT_KEY_MASK != 0 {
        0.5
    } else {
        1.0
    }
}

/// Holds the opaque local-monitor token
/// `addLocalMonitorForEventsMatchingMask:handler:` returns -- must stay
/// alive for the app's whole life (same contract
/// `now_playing::NowPlaying::_targets` documents for its own AppKit
/// handles), or macOS tears the monitor down and Option-key holds silently
/// stop being observed.
pub(crate) struct OptionSpeedMonitor {
    _monitor: Retained<AnyObject>,
}

impl OptionSpeedMonitor {
    /// Registers the `flagsChanged` local monitor. Must run on the main
    /// thread, once, any time after the window has opened (same
    /// requirement, and same reasoning, as `NowPlaying::register`). Returns
    /// `None` if not on the main thread or if AppKit refuses the
    /// registration -- never fatal to playback, just means Option-key
    /// speed-hold is unavailable for this run (Space/pause and everything
    /// else are completely unaffected).
    pub(crate) fn register(
        tx: tokio::sync::mpsc::UnboundedSender<OptionFlagsSample>,
    ) -> Option<Self> {
        MainThreadMarker::new()?;
        let handler = block2::RcBlock::new(move |event: NonNull<NSEvent>| -> *mut NSEvent {
            crate::panic_log::catch_and_log(
                "NSEvent flagsChanged local monitor",
                std::panic::AssertUnwindSafe(|| {
                    // SAFETY: AppKit guarantees a valid, live `NSEvent` for
                    // the duration of this call.
                    let raw_flags = unsafe { event.as_ref() }.modifierFlags().0 as u64;
                    let _ = tx.send(OptionFlagsSample { raw_flags });
                }),
            );
            // Pass-through, always -- see this module's doc comment on why
            // returning anything else here would break ordinary Option-key
            // use (dead keys, ⌥-modified menu items) everywhere in the app.
            event.as_ptr()
        });
        let monitor = unsafe {
            NSEvent::addLocalMonitorForEventsMatchingMask_handler(
                NSEventMask::FlagsChanged,
                &handler,
            )
        }?;
        // `addLocalMonitorForEventsMatchingMask:handler:` keeps its own
        // internal copy of the block for as long as the monitor is
        // registered -- same "forget our copy, AppKit's own retain is what
        // keeps it alive" contract `NowPlaying::register`'s
        // `register_simple!` macro already relies on for `addTargetWithHandler`.
        std::mem::forget(handler);
        Some(Self { _monitor: monitor })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GENERIC_OPTION: u64 = 1 << 19; // NSEventModifierFlagOption
    const CMD: u64 = 1 << 20;
    const CTRL: u64 = 1 << 18;
    const SHIFT: u64 = 1 << 17;

    #[test]
    fn no_option_key_is_1x_regardless_of_gate() {
        assert_eq!(decide_speed_rate(0, true), 1.0);
        assert_eq!(decide_speed_rate(0, false), 1.0);
    }

    #[test]
    fn right_option_alone_is_2x_when_gate_open() {
        let flags = NX_DEVICE_RALT_KEY_MASK | GENERIC_OPTION;
        assert_eq!(decide_speed_rate(flags, true), 2.0);
    }

    #[test]
    fn left_option_alone_is_half_x_when_gate_open() {
        let flags = NX_DEVICE_LALT_KEY_MASK | GENERIC_OPTION;
        assert_eq!(decide_speed_rate(flags, true), 0.5);
    }

    #[test]
    fn gate_closed_suppresses_either_side() {
        let right = NX_DEVICE_RALT_KEY_MASK | GENERIC_OPTION;
        let left = NX_DEVICE_LALT_KEY_MASK | GENERIC_OPTION;
        assert_eq!(decide_speed_rate(right, false), 1.0);
        assert_eq!(decide_speed_rate(left, false), 1.0);
    }

    #[test]
    fn both_sides_held_at_once_right_wins() {
        let flags = NX_DEVICE_LALT_KEY_MASK | NX_DEVICE_RALT_KEY_MASK | GENERIC_OPTION;
        assert_eq!(decide_speed_rate(flags, true), 2.0);
    }

    #[test]
    fn a_joined_command_modifier_cancels_the_boost() {
        let flags = NX_DEVICE_RALT_KEY_MASK | GENERIC_OPTION | CMD;
        assert_eq!(decide_speed_rate(flags, true), 1.0);
    }

    #[test]
    fn a_joined_control_modifier_cancels_the_boost() {
        let flags = NX_DEVICE_LALT_KEY_MASK | GENERIC_OPTION | CTRL;
        assert_eq!(decide_speed_rate(flags, true), 1.0);
    }

    #[test]
    fn a_joined_shift_modifier_cancels_the_boost() {
        let flags = NX_DEVICE_RALT_KEY_MASK | GENERIC_OPTION | SHIFT;
        assert_eq!(decide_speed_rate(flags, true), 1.0);
    }

    #[test]
    fn unrelated_bits_like_capslock_do_not_affect_the_decision() {
        const CAPS_LOCK: u64 = 1 << 16;
        let flags = NX_DEVICE_RALT_KEY_MASK | GENERIC_OPTION | CAPS_LOCK;
        assert_eq!(decide_speed_rate(flags, true), 2.0);
    }
}
