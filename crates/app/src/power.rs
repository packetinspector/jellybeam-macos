//! Display-sleep / screensaver suppression during active video playback.
//!
//! Backed by IOKit's `kIOPMAssertionTypePreventUserIdleDisplaySleep`
//! assertion, chosen over "prevent idle *system* sleep": it blocks both the
//! display and the screensaver (which piggybacks on the display idle timer),
//! and since macOS's system idle sleep only fires once the display has
//! already slept, keeping the display awake transitively keeps the whole
//! machine awake -- without a broader assertion type blocking unrelated
//! power-saving heuristics.
//!
//! The assertion's lifetime is tied 1:1 to `DisplaySleepGuard` via RAII:
//! callers hold one only while video is actively playing (acquired on
//! play/resume, dropped on pause/stop/end-of-stream), so a paused session
//! can never pin the display awake, even via an early-return or panic-unwind
//! drop path.
use std::ffi::c_void;

// Minimal CoreFoundation / IOKit FFI: `jellybeam-app` has no direct
// `core-foundation` dependency (only transitive, e.g. via
// `security-framework`), so `CFStringCreateWithBytes`/`CFRelease` are
// declared directly, matching `gl_video.rs`'s CGL/`objc_msgSend` pattern.

type CfIndex = isize;
type CfStringEncoding = u32;
type CfAllocatorRef = *const c_void;
type CfStringRef = *const c_void;
type CfTypeRef = *const c_void;

/// `kCFStringEncodingUTF8` (`CFString.h`).
const K_CF_STRING_ENCODING_UTF8: CfStringEncoding = 0x0800_0100;

/// `Boolean` (`CFBase.h`) -- `unsigned char` on Darwin, NOT C `_Bool`. Rust's
/// `bool` is ABI-compatible with `_Bool`, not `unsigned char`, and only has
/// two valid bit patterns, so using it for a *returned* `Boolean` would be UB
/// the moment a callee returns anything other than 0/1.
type CfBoolean = u8;

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFStringCreateWithBytes(
        alloc: CfAllocatorRef,
        bytes: *const u8,
        num_bytes: CfIndex,
        encoding: CfStringEncoding,
        is_external_representation: CfBoolean,
    ) -> CfStringRef;

    fn CFRelease(cf: CfTypeRef);
}

/// `IOPMAssertionID` (`IOKit/pwr_mgt/IOPMLib.h`): an opaque `u32` token,
/// not a pointer -- this is what makes releasing it from a different
/// thread than the one that created it sound (see `Send` impl below).
type IoPmAssertionId = u32;
/// `IOReturn` (`IOKit/IOReturn.h`).
type IoReturn = i32;

/// `kIOReturnSuccess` (`IOKit/IOReturn.h`).
const K_IO_RETURN_SUCCESS: IoReturn = 0;
/// `kIOPMAssertionLevelOn` (`IOKit/pwr_mgt/IOPMLibDefs.h`).
const K_IO_PM_ASSERTION_LEVEL_ON: u32 = 255;

#[link(name = "IOKit", kind = "framework")]
unsafe extern "C" {
    fn IOPMAssertionCreateWithName(
        assertion_type: CfStringRef,
        assertion_level: u32,
        assertion_name: CfStringRef,
        assertion_id: *mut IoPmAssertionId,
    ) -> IoReturn;

    fn IOPMAssertionRelease(assertion_id: IoPmAssertionId) -> IoReturn;
}

/// Builds a `CFStringRef` from a Rust `&str`. Caller owns the returned
/// string and must `CFRelease` it (the Create Rule).
fn cfstring_from_str(s: &str) -> CfStringRef {
    let bytes = s.as_bytes();
    unsafe {
        CFStringCreateWithBytes(
            std::ptr::null(),
            bytes.as_ptr(),
            bytes.len() as CfIndex,
            K_CF_STRING_ENCODING_UTF8,
            0,
        )
    }
}

/// `kIOPMAssertionTypePreventUserIdleDisplaySleep` (`IOPMLib.h`).
const ASSERTION_TYPE: &str = "PreventUserIdleDisplaySleep";
const ASSERTION_REASON: &str = "Jellybeam video playback";

/// RAII handle for an active `PreventUserIdleDisplaySleep` power assertion.
/// While held, the display, screensaver, and system idle sleep will not
/// activate due to user idle time; dropping releases the assertion
/// immediately. Construct on playback start/resume, drop on pause/stop/end.
pub(crate) struct DisplaySleepGuard {
    assertion_id: IoPmAssertionId,
}

impl DisplaySleepGuard {
    /// Creates a new display-sleep assertion. Returns `None` (after logging
    /// to stderr) if IOKit refuses for any reason; treated as non-fatal
    /// everywhere -- playback continues without the sleep-suppression
    /// guarantee. Never panics.
    pub(crate) fn new() -> Option<Self> {
        let assertion_type = cfstring_from_str(ASSERTION_TYPE);
        if assertion_type.is_null() {
            eprintln!("DisplaySleepGuard::new: failed to create assertion-type CFString");
            return None;
        }
        let assertion_name = cfstring_from_str(ASSERTION_REASON);
        if assertion_name.is_null() {
            eprintln!("DisplaySleepGuard::new: failed to create assertion-name CFString");
            unsafe { CFRelease(assertion_type) };
            return None;
        }

        let mut assertion_id: IoPmAssertionId = 0;
        let result = unsafe {
            IOPMAssertionCreateWithName(
                assertion_type,
                K_IO_PM_ASSERTION_LEVEL_ON,
                assertion_name,
                &mut assertion_id,
            )
        };

        // Both CFStrings are only needed for the duration of the call
        // above (the Create Rule: we created them, we release them).
        unsafe {
            CFRelease(assertion_name);
            CFRelease(assertion_type);
        }

        if result != K_IO_RETURN_SUCCESS {
            eprintln!(
                "DisplaySleepGuard::new: IOPMAssertionCreateWithName failed (IOReturn={result})"
            );
            return None;
        }

        Some(Self { assertion_id })
    }
}

impl Drop for DisplaySleepGuard {
    fn drop(&mut self) {
        let result = unsafe { IOPMAssertionRelease(self.assertion_id) };
        if result != K_IO_RETURN_SUCCESS {
            eprintln!("DisplaySleepGuard::drop: IOPMAssertionRelease failed (IOReturn={result})");
        }
    }
}

// SAFETY: `assertion_id` is a plain `u32` token, not a pointer or anything
// tied to the creating thread -- IOKit's power assertion API is documented
// as safe to release from any thread/process. No interior mutability or
// shared state beyond that single `Copy` u32, so cross-thread drop is sound.
unsafe impl Send for DisplaySleepGuard {}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pins that create/drop never panics, even if IOKit denies the
    /// assertion (e.g. no window server session in CI) -- doesn't require `Some`.
    #[test]
    fn create_and_drop_does_not_panic() {
        let guard = DisplaySleepGuard::new();
        drop(guard);
    }

    /// Pins that two guards don't interfere -- IOKit assertions are
    /// reference-counted per-ID, not a single global toggle.
    #[test]
    fn two_independent_guards_do_not_panic() {
        let first = DisplaySleepGuard::new();
        let second = DisplaySleepGuard::new();
        drop(first);
        drop(second);
    }

    /// Pins that a dropped guard leaves no dangling state that crashes
    /// later, by exercising further unrelated work in the process.
    #[test]
    fn no_crash_after_drop_when_process_continues() {
        {
            let _guard = DisplaySleepGuard::new();
        }
        let after = DisplaySleepGuard::new();
        drop(after);
    }

    /// Must be `Send`: playback lives on a background task/thread (see
    /// `playback.rs`), so the guard has to cross an await/thread boundary.
    #[test]
    fn is_send() {
        fn assert_send<T: Send>() {}
        assert_send::<DisplaySleepGuard>();
    }
}
