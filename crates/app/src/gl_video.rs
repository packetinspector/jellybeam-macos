//! mpv video layer embedding: sibling-below-NSView pattern (docs/OVERVIEW.md §2
//! "Embedding wrinkle"), using `NSOpenGLContext` since mpv's render API is
//! OpenGL-only. The GL context handed to `Player::new` MUST be Core Profile
//! >= 3.2 or VideoToolbox hwdec silently degrades to software decode.
//!
//! The video `NSView` is a permanent sibling of GPUI's own content view,
//! inserted once at startup; only its frame moves, animated between
//! `LayerMode`'s target rects (`video_frame`/`GeometryState`). Visibility is
//! controlled by whether GPUI's chrome paints opaque over it or leaves it
//! transparent -- see `root.rs`'s `render_content`/`render_main`.
//!
//! **Miniplayer video visibility.** GPUI's Metal renderer has no
//! subtractive "paint everywhere except this rect" primitive, so
//! `spawn_geometry_driver` punches the hole one layer down: it maintains a
//! `CAShapeLayer` mask on GPUI's content view (`native_view`), an evenodd
//! path of [full view bounds, current Miniplayer rect], assigned to
//! `native_view.layer().mask`. Wherever the mask is 0, nothing GPUI paints
//! there survives compositing, and the video NSView -- one z-order step
//! further down -- shows through. The mask is hover-gated
//! (`GeometryState::hovering`, `VideoLayer::set_miniplayer_hovering`): it
//! lifts while the pointer is over the Miniplayer rect so
//! `player_ui::render_miniplayer`'s hover scrim/buttons, painted in that
//! same rect on the same content view, aren't masked away along with the
//! Browse UI underneath them.
//!
//! **Two tick loops, deliberately split by thread.** The dedicated GL
//! render thread (`run_render_thread`) owns `Player`/mpv's render API for
//! the app's whole life and must stay on one thread for that
//! (`player::Player::render`'s doc comment) -- but `NSView`/`NSWindow`/
//! `NSOpenGLContext` geometry APIs (`frame`/`bounds`/`setFrame`/
//! `contentView`/`update`) are main-thread-only in AppKit. A GPUI
//! foreground task (`spawn_geometry_driver`, genuinely running on the real
//! main thread per `App::spawn`'s contract) owns all of that instead,
//! publishing just the resulting pixel size to the render thread via an
//! atomic (`RenderSize`).
//!
//! **CGL locking.** Both threads still touch the same `NSOpenGLContext`
//! (render thread: `makeCurrentContext`/`render`/`flushBuffer`; geometry
//! driver: `setFrame`/`update`), with nothing else serializing the two.
//! Apple's docs require `CGLLockContext`/`CGLUnlockContext` around every
//! multi-threaded touch of a shared context; see `CglLock`'s doc comment.

#![allow(deprecated)] // NSOpenGL* is deprecated AppKit API; mpv has no Metal render backend.

use std::ffi::{c_char, c_int, c_void, CString};
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use gpui::{App, Window, WindowBackgroundAppearance};
use objc2::rc::Retained;
use objc2::{define_class, msg_send, MainThreadMarker, MainThreadOnly, Message};
use objc2_app_kit::{
    NSOpenGLContext, NSOpenGLPFAAccelerated, NSOpenGLPFAAlphaSize, NSOpenGLPFAColorSize,
    NSOpenGLPFADepthSize, NSOpenGLPFADoubleBuffer, NSOpenGLPFAOpenGLProfile, NSOpenGLPixelFormat,
    NSOpenGLProfileVersion3_2Core, NSView, NSWindow, NSWindowOrderingMode,
};
use objc2_core_graphics::{CGColor, CGMutablePath};
use objc2_foundation::{NSPoint, NSRect, NSSize};
use objc2_quartz_core::{kCAFillRuleEvenOdd, CAShapeLayer};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};

use player::Player;

/// Fixed left inset for the sidebar (library views).
pub(crate) const SIDEBAR_WIDTH: f64 = 240.0;
/// Below `SIDEBAR_COLLAPSE_BREAKPOINT` window width, the sidebar collapses
/// from the full 240px labeled column to this narrow icon-only rail (icons +
/// tooltips, same ⌘N shortcuts) instead of rendering a partially-clipped
/// full sidebar.
pub(crate) const SIDEBAR_RAIL_WIDTH: f64 = 56.0;
/// DESIGN-GUIDE.md doesn't specify a responsive breakpoint for the sidebar,
/// so the app uses its own: 900px window width.
pub(crate) const SIDEBAR_COLLAPSE_BREAKPOINT: f64 = 900.0;
/// Miniplayer geometry (docs/UX-SPEC.md §3: "bottom-right floating node, 16:9,
/// ~380px wide, draggable to corners"). Shared by the render thread (moves
/// the real NSView), `spawn_geometry_driver` (the same rect, converted into
/// `native_view`'s coordinate space, is the video-visibility CALayer mask's
/// hole), and `player_ui::render_miniplayer` (positions the GPUI-side hover
/// mini-OSD at the same rect, via `miniplayer_rect_px` below).
pub(crate) const MINIPLAYER_WIDTH: f64 = 380.0;
pub(crate) const MINIPLAYER_HEIGHT: f64 = MINIPLAYER_WIDTH * 9.0 / 16.0;
pub(crate) const MINIPLAYER_MARGIN: f64 = 20.0;
/// §7: "RADIUS_MINIPLAYER (12px) rounded corners ON THE VIDEO." Derived from
/// `theme::RADIUS_MINIPLAYER` (not a second independent literal) so the
/// video's own rounded corner and the GPUI-painted chrome around it
/// (hairline border, hover controls) can never silently drift apart;
/// `CGPathAddRoundedRect` wants a `CGFloat` (`f64`), hence the cast.
fn miniplayer_corner_radius() -> f64 {
    f64::from(f32::from(crate::theme::RADIUS_MINIPLAYER))
}
/// docs/UX-SPEC.md §3: "150ms, GPU transform only" -- see this module's
/// doc comment on `GeometryState` for why this crate implements it as a
/// frame lerp instead of a literal GPU transform.
pub(crate) const LAYER_ANIM: Duration = Duration::from_millis(150);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Corner {
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum LayerMode {
    /// Video fills the whole window; sidebar/browse chrome hidden (docs/UX-SPEC.md
    /// §3). Also used while the window itself is in native OS-Fullscreen --
    /// from the video NSView's perspective the target geometry is
    /// identical (full content-view bounds); only `Root`/`main.rs` care
    /// about the OS-Fullscreen bit specifically (native Space, `F`/Esc
    /// semantics).
    FullscreenInWindow,
    Miniplayer(Corner),
}

/// Tracks the mpv render thread's own view of "where should the video
/// NSView's frame be right now", including the in-flight 150ms lerp when
/// `mode` last changed. Owned by the render thread; `VideoLayer::
/// set_layer_mode` (called from GPUI's thread) only ever writes `mode` --
/// the render thread (the only thread that ever touches the live NSView)
/// detects the change itself and captures `anim_from`/`anim_started` at
/// that moment, so a caller can call `set_layer_mode` repeatedly without
/// needing to know the view's current on-screen frame.
struct GeometryState {
    mode: LayerMode,
    /// The video NSView's actual on-screen frame as of the render thread's
    /// last tick, `(x, y, width, height)` in content-view-relative points
    /// (AppKit: y=0 at the bottom). Written every tick by the render
    /// thread; read by `VideoLayer::current_frame` -- primarily for
    /// `JELLYBEAM_E2E`'s Miniplayer/Fullscreen geometry assertions (docs/UX-SPEC.md
    /// §3), which otherwise have no way to observe this sibling NSView's
    /// real frame (it isn't a top-level window `CGWindowListCopyWindowInfo`
    /// could see).
    last_frame: Option<(f64, f64, f64, f64)>,
    /// True while the pointer is over the Miniplayer's floating hover
    /// rect (`VideoLayer::set_miniplayer_hovering`, called from
    /// `player_ui::render_miniplayer`'s `on_hover`). Read by
    /// `spawn_geometry_driver`'s mask-toggle logic -- see this module's
    /// "Miniplayer video visibility" doc section (top of file) for why a
    /// hover-gated mask, not an always-on one, is what's actually correct
    /// here: the mask has to come off while GPUI's own hover scrim/buttons
    /// are painting in that exact same screen rect, or they'd be masked
    /// away right along with the Browse UI underneath them.
    hovering: bool,
}

// ---------------------------------------------------------------------
// Input hit-testing: without this, users can't bring up the OSD with the
// mouse or click the skip pill area after it fades -- only keys work.
//
// AppKit's default `-[NSView hitTest:]` returns `self` for any point
// within its own bounds regardless of what's actually painted there --
// it has no concept of GPUI's transparent/opaque chrome distinction. The
// embedded video view is a full-size (Fullscreen-in-window) or
// corner-rect (Miniplayer) sibling that sits in the same content view as
// GPUI's own `native_view`; depending on exactly how AppKit resolves
// hit-testing/tracking-area dispatch between the two siblings, the video
// view can end up being the one that answers for mouse-down/move/drag
// events instead of GPUI, which then never sees them at all -- no OSD
// show-on-activity, no clicks landing on OSD buttons or the skip pill, no
// scrubber hover. `JellybeamVideoView` closes this off categorically: it
// always returns `nil` from `hitTest:`, so it is *never* eligible to be
// the hit-test target for any mouse event, guaranteeing every one of them
// falls through to GPUI's content view for normal dispatch. The video
// view has no interactive behavior of its own (it only ever displays
// mpv's rendered frames), so giving up all hit-testing is strictly safe.
// ---------------------------------------------------------------------
define_class!(
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "JellybeamVideoView"]
    struct JellybeamVideoView;

    impl JellybeamVideoView {
        #[unsafe(method_id(hitTest:))]
        fn hit_test(&self, _point: NSPoint) -> Option<Retained<NSView>> {
            None
        }
    }
);

/// Owns the embedded video `NSView`, the GL render thread, and the mpv
/// `Player` itself. Created once and kept alive for the app's lifetime.
pub(crate) struct VideoLayer {
    /// `Option` purely so `Drop` can release the `Arc` *while the GL context
    /// is current on this thread*: `Player::drop` calls
    /// `mpv_render_context_free`, an `mpv_render_*` function, and render.h's
    /// "Threading" section requires the creating GL context be current on
    /// the calling thread for ALL of them. `Some` until `Drop`; access via
    /// [`VideoLayer::player`].
    player: Option<Arc<Player>>,
    /// Extra retain on the one shared `NSOpenGLContext` (the render thread
    /// and geometry driver hold the others), kept solely so `Drop` can make
    /// it current for the teardown free above.
    teardown_context: Retained<NSOpenGLContext>,
    stop_flag: Arc<AtomicBool>,
    render_thread: Option<thread::JoinHandle<()>>,
    geometry: Arc<std::sync::Mutex<GeometryState>>,
    /// Set by `clear_to_black` (`Root::stop_playback`), consumed by the
    /// render thread: paints one black frame and stops drawing mpv output
    /// until the next `Player::load`'s frames start arriving again. See
    /// `clear_to_black`'s doc comment for why this exists.
    clear_flag: Arc<AtomicBool>,
}

impl VideoLayer {
    /// The shared mpv player. Present for the layer's whole life; the field
    /// is only `take`n inside `Drop` (see the field's doc comment).
    pub(crate) fn player(&self) -> &Arc<Player> {
        self.player
            .as_ref()
            .expect("VideoLayer.player is Some until Drop")
    }

    /// docs/UX-SPEC.md: stopping playback must show black, not a frozen last frame.
    /// mpv's `stop` command tears down decoding, but the GL front buffer
    /// still holds whatever was last swapped in -- nothing re-draws it, so
    /// without this the video NSView keeps showing that stale frame
    /// indefinitely (visible through Fullscreen-in-window's transparent
    /// chrome cutout, or the Miniplayer rect, until the next item loads).
    /// Picked up by the render thread on its next tick (`run_render_thread`).
    pub(crate) fn clear_to_black(&self) {
        self.clear_flag.store(true, Ordering::Release);
    }
    /// Requests a new target layer mode (Fullscreen-in-window vs. a
    /// Miniplayer corner). The render thread picks this up on its next tick
    /// and animates the NSView's frame toward it over `LAYER_ANIM`.
    pub(crate) fn set_layer_mode(&self, mode: LayerMode) {
        let mut state = crate::gl_video::lock_ignore_poison(&self.geometry);
        state.mode = mode;
        if !matches!(mode, LayerMode::Miniplayer(_)) {
            // Defensive reset: a stale `hovering = true` left over from a
            // previous Miniplayer session must not suppress the video-
            // visibility mask the *next* time Miniplayer is entered, before
            // the new session's first real `on_hover` callback fires.
            state.hovering = false;
        }
    }

    pub(crate) fn layer_mode(&self) -> LayerMode {
        crate::gl_video::lock_ignore_poison(&self.geometry).mode
    }

    /// Called from `player_ui::render_miniplayer`'s `on_hover` --
    /// toggles whether `spawn_geometry_driver`'s video-visibility CALayer
    /// mask is currently lifted. See `GeometryState::hovering`'s doc
    /// comment.
    pub(crate) fn set_miniplayer_hovering(&self, hovering: bool) {
        let mut state = crate::gl_video::lock_ignore_poison(&self.geometry);
        state.hovering = hovering;
    }

    /// `true` exactly when the video-visibility mask should be (and, modulo
    /// `spawn_geometry_driver`'s next ~16ms tick, is) applied: Miniplayer
    /// layer mode and the pointer isn't currently over the hover rect.
    /// Exposed for `JELLYBEAM_E2E`'s structural assertion that the cutout path
    /// is actually wired up (screenshots are TCC-restricted in this
    /// environment, so this state-sampling check -- not a pixel
    /// comparison -- is the verification this fix relies on; see
    /// `e2e.rs::assert_miniplayer_video_visible` and this module's
    /// "Miniplayer video visibility" doc section).
    pub(crate) fn miniplayer_hole_active(&self) -> bool {
        let state = crate::gl_video::lock_ignore_poison(&self.geometry);
        matches!(state.mode, LayerMode::Miniplayer(_)) && !state.hovering
    }

    /// See `GeometryState::last_frame`'s doc comment. `None` only before
    /// the render thread's first tick (effectively never observable by a
    /// caller -- `embed` doesn't return until that thread is already
    /// running).
    pub(crate) fn current_frame(&self) -> Option<(f64, f64, f64, f64)> {
        crate::gl_video::lock_ignore_poison(&self.geometry).last_frame
    }
}

/// Same poisoning-tolerant lock helper as `player::lock_ignore_poison`
/// (that one's private to the `player` crate) -- this `Mutex` only ever
/// guards a plain `Copy` field, so there's no invariant a panic could leave
/// broken.
fn lock_ignore_poison<T>(m: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    match m.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    }
}

/// Raw (but retain-counted-alive) AppKit object pointer, carried across the
/// thread boundary to the dedicated render thread (S1 pattern: `Retained<T>`
/// isn't `Send`, so we transfer ownership of one retain via a raw pointer
/// instead -- see `run_render_thread`'s doc comment for how the count is
/// balanced).
///
/// B1: this used to also carry the video `NSView` and its `NSWindow` to the
/// render thread, which touched their `frame`/`bounds`/`setFrame`/
/// `contentView` every tick -- all AppKit APIs documented (and, per
/// `run_render_thread`'s previous doc comment, confirmed by a real crash for
/// `-update`) as main-thread-only, called here from a thread that is
/// provably NOT the main thread. `NSOpenGLContext::makeCurrentContext`/
/// `flushBuffer`/render aren't main-thread-only (see `run_render_thread`'s
/// doc comment) so `context` alone still needs to cross to the render
/// thread; all view/window geometry now lives on
/// `spawn_geometry_driver`'s real-main-thread task instead (see its doc
/// comment), communicating the render thread's needed pixel size via
/// [`RenderSize`] rather than the render thread reading AppKit geometry
/// itself.
struct GlHandles {
    context: NonNull<c_void>,
}
// SAFETY: what crosses the thread boundary is ownership
// of exactly one ObjC retain on the shared `NSOpenGLContext` (ObjC refcounts
// are atomic), reclaimed by `Retained::from_raw` in `run_render_thread`. The
// *object* is deliberately NOT exclusively owned -- `embed`'s
// `context_for_driver` is a second retain used from the main thread by
// `spawn_geometry_driver` every tick, and `teardown_context` a third used in
// `VideoLayer::drop`. Concurrent use from the threads is made sound by
// `CglLock` serializing every touch of the context from every side (see this
// file's CGL-locking module doc, and the SIGSEGV crash it exists to
// prevent); removing `CglLock` would make this `unsafe impl` unsound.
unsafe impl Send for GlHandles {}

/// B1: the pixel size (post-backing-scale) the render thread should pass to
/// `Player::render` -- written every tick by `spawn_geometry_driver`'s
/// real-main-thread task (the only thread allowed to read
/// `NSView`/`NSWindow` geometry), read by the render thread each of its own
/// ticks instead of touching AppKit itself. Packed into one `AtomicU64`
/// (high 32 bits = width, low 32 bits = height) so a read/write is a single
/// atomic op -- no risk of the render thread observing a torn combination
/// of an old width paired with a new height (or vice versa) from two
/// separate atomics.
struct RenderSize(std::sync::atomic::AtomicU64);

impl RenderSize {
    fn new(width_px: i32, height_px: i32) -> Self {
        Self(std::sync::atomic::AtomicU64::new(Self::pack(
            width_px, height_px,
        )))
    }

    fn pack(width_px: i32, height_px: i32) -> u64 {
        ((width_px as u32 as u64) << 32) | (height_px as u32 as u64)
    }

    fn store(&self, width_px: i32, height_px: i32) {
        self.0
            .store(Self::pack(width_px, height_px), Ordering::Release);
    }

    /// Returns `(width_px, height_px)`.
    fn load(&self) -> (i32, i32) {
        let packed = self.0.load(Ordering::Acquire);
        ((packed >> 32) as u32 as i32, packed as u32 as i32)
    }
}

/// Target NSView frame for `mode`, in `content_bounds`-relative coordinates
/// (AppKit: y=0 is the bottom). `FullscreenInWindow` fills the whole
/// window (docs/UX-SPEC.md §3: sidebar auto-hides); `Miniplayer` is a small rect
/// pinned to one of the four corners.
fn video_frame(content_bounds: NSRect, mode: LayerMode) -> NSRect {
    match mode {
        LayerMode::FullscreenInWindow => NSRect::new(
            NSPoint::new(0.0, 0.0),
            NSSize::new(
                content_bounds.size.width.max(0.0),
                content_bounds.size.height.max(0.0),
            ),
        ),
        LayerMode::Miniplayer(corner) => {
            let w = MINIPLAYER_WIDTH.min(content_bounds.size.width);
            let h = MINIPLAYER_HEIGHT.min(content_bounds.size.height);
            let (x, y) = match corner {
                Corner::BottomRight => (
                    content_bounds.size.width - w - MINIPLAYER_MARGIN,
                    MINIPLAYER_MARGIN,
                ),
                Corner::BottomLeft => (MINIPLAYER_MARGIN, MINIPLAYER_MARGIN),
                Corner::TopRight => (
                    content_bounds.size.width - w - MINIPLAYER_MARGIN,
                    content_bounds.size.height - h - MINIPLAYER_MARGIN,
                ),
                Corner::TopLeft => (
                    MINIPLAYER_MARGIN,
                    content_bounds.size.height - h - MINIPLAYER_MARGIN,
                ),
            };
            NSRect::new(NSPoint::new(x.max(0.0), y.max(0.0)), NSSize::new(w, h))
        }
    }
}

/// Linear interpolation between two frames, `t` clamped to `[0, 1]`. Backs
/// the 150ms Fullscreen-in-window <-> Miniplayer transition.
fn lerp_rect(from: NSRect, to: NSRect, t: f64) -> NSRect {
    let t = t.clamp(0.0, 1.0);
    let lerp = |a: f64, b: f64| a + (b - a) * t;
    NSRect::new(
        NSPoint::new(
            lerp(from.origin.x, to.origin.x),
            lerp(from.origin.y, to.origin.y),
        ),
        NSSize::new(
            lerp(from.size.width, to.size.width),
            lerp(from.size.height, to.size.height),
        ),
    )
}

/// GPUI-side mirror of `video_frame`'s Miniplayer branch, in GPUI's
/// top-left-origin `Pixels` space -- used by `player_ui::render_miniplayer`
/// to position the hover mini-OSD in GPUI chrome exactly where the real
/// NSView (moved by the render thread, AppKit bottom-left-origin space)
/// actually is. Kept in lockstep with `video_frame` by construction (same
/// constants, mirrored y-flip) rather than by sharing code across the
/// AppKit/GPUI coordinate-system boundary. Note this fn is *not* what makes
/// the video visible through that rect -- that's `spawn_geometry_driver`'s
/// CALayer mask (which independently derives the same rect from
/// `video_frame` on the AppKit side, not from this fn); see this module's
/// "Miniplayer video visibility" doc section for why the naive "GPUI paints
/// a transparent cutout here" approach this fn's old doc comment described
/// never actually worked.
pub(crate) fn miniplayer_rect_px(
    viewport: gpui::Size<gpui::Pixels>,
    corner: Corner,
) -> (gpui::Pixels, gpui::Pixels, gpui::Pixels, gpui::Pixels) {
    use gpui::px;
    let w = px(MINIPLAYER_WIDTH.min(f64::from(viewport.width)) as f32);
    let h = px(MINIPLAYER_HEIGHT.min(f64::from(viewport.height)) as f32);
    let margin = px(MINIPLAYER_MARGIN as f32);
    let (left, top) = match corner {
        Corner::BottomRight => (viewport.width - w - margin, viewport.height - h - margin),
        Corner::BottomLeft => (margin, viewport.height - h - margin),
        Corner::TopRight => (viewport.width - w - margin, margin),
        Corner::TopLeft => (margin, margin),
    };
    (left, top, w, h)
}

// dlsym-based get_proc_address, resolved against the process's already-
// loaded OpenGL.framework (see build.rs). Mirrors `player/tests/common/mod.rs`'s
// harness exactly -- that's the pattern `Player::new`'s doc comment points
// real embedders at.
unsafe extern "C" {
    fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
}

/// A plain top-level fn (no captures) so it satisfies `Player::new`'s frozen
/// `fn(&str) -> *mut c_void` parameter type directly.
fn gl_get_proc_address(name: &str) -> *mut c_void {
    let Ok(cname) = CString::new(name) else {
        return std::ptr::null_mut();
    };
    let rtld_default = (-2isize) as *mut c_void; // RTLD_DEFAULT (dlfcn.h)
    unsafe { dlsym(rtld_default, cname.as_ptr()) }
}

// ---------------------------------------------------------------------
// CGL locking (crash fix -- see the SIGSEGV described below)
//
// Apple's own documentation for `NSOpenGLContext`/`CGLContextObj`
// (`OpenGL/CGLCurrent.h`, "Any OpenGL calls made... from more than one
// thread... at the same time must be synchronized using CGLLockContext and
// CGLUnlockContext") requires this whenever more than one thread touches the
// same context, which is exactly this file's B1 shape: the render thread
// (`run_render_thread`, `makeCurrentContext`/`render`/`flushBuffer`) and the
// main-thread geometry driver (`spawn_geometry_driver`, `setFrame`/`update`)
// share one `NSOpenGLContext` with no coordination between them at all.
//
// The crash this fixes: a SIGSEGV inside `glClear` (`GLRResourceList::
// addResource` -> ... -> `mpv_render_context_render` -> `Player::render` ->
// `run_render_thread`) on the render thread, captured live while the main
// thread was concurrently deep in an `NSView`/`NSWindow` display cycle
// (`-[NSWindow displayIfNeeded]` et al on thread 0 at the same moment) --
// i.e. the render thread was drawing into a GL context whose backing
// drawable the main thread was simultaneously resizing/revalidating via
// `setFrame`/`update`. `CGLLockContext`/`CGLUnlockContext` around every
// multi-threaded touch of the shared context (both here and in
// `run_render_thread`) is Apple's documented fix for precisely this.
// ---------------------------------------------------------------------

type CglContextObj = *mut c_void;

unsafe extern "C" {
    fn CGLLockContext(ctx: CglContextObj) -> c_int;
    fn CGLUnlockContext(ctx: CglContextObj) -> c_int;
}

// `objc_msgSend` called directly (not via objc2's `msg_send!` macro) for
// `-[NSOpenGLContext CGLContextObj]` specifically: `msg_send!` verifies the
// declared return type against the selector's *actual* registered
// Objective-C type encoding at runtime (debug builds), and `CGLContextObj`
// is encoded as a pointer to the real `_CGLContextObject` struct
// (`^{_CGLContextObject=...}`), not the generic `^v` a plain `*mut c_void`
// return type encodes as -- confirmed by a real crash the first time this
// ran (`panic_verify`: "expected return to have type code
// '^{_CGLContextObject=...}', but found '^v'"). Both encodings describe an
// ABI-identical single pointer, so going around that check via a raw
// `objc_msgSend` call (the same underlying C function `msg_send!` compiles
// down to) is sound; declaring our own binding just skips the mismatched
// compile-time type description, which is otherwise unavoidable short of
// depending on the `objc2-open-gl` crate purely for one struct definition.
#[link(name = "objc")]
unsafe extern "C" {
    fn objc_msgSend(receiver: *mut c_void, sel: objc2::runtime::Sel) -> *mut c_void;
}

/// Extracts the raw `CGLContextObj` backing an `NSOpenGLContext` via the
/// `-CGLContextObj` selector directly (rather than objc2-app-kit's own
/// generated binding for it, which sits behind the `objc2-open-gl` feature/
/// crate this file otherwise has no need for -- everywhere else here already
/// treats the CGL context as an opaque `*mut c_void`, matching `GlHandles`).
fn cgl_context_obj(context: &NSOpenGLContext) -> CglContextObj {
    let sel = objc2::runtime::Sel::register(c"CGLContextObj");
    let receiver = (context as *const NSOpenGLContext).cast_mut().cast();
    // SAFETY: `CGLContextObj` is a real, documented, zero-argument
    // `NSOpenGLContext` method (AppKit) taking no arguments and returning a
    // `CGLContextObj` -- a plain pointer typedef, ABI-identical to the
    // `*mut c_void` this raw `objc_msgSend` call treats it as (see the doc
    // comment on the `objc_msgSend` binding above for why this goes around
    // `msg_send!`'s stricter, but ABI-irrelevant, encoding check). `context`
    // is a live, non-null `&NSOpenGLContext` for the duration of this call.
    unsafe { objc_msgSend(receiver, sel) }
}

/// RAII `CGLLockContext`/`CGLUnlockContext` guard -- see the module-level
/// doc comment above for why every multi-threaded touch of the shared
/// `NSOpenGLContext` in this file must be wrapped in one of these. Both
/// threads' `NSOpenGLContext` retains point at the same underlying ObjC
/// object, so `CGLContextObj()` called on either side yields the same
/// `CglContextObj` and the lock genuinely serializes the two threads.
struct CglLock(CglContextObj);

impl CglLock {
    fn acquire(ctx: CglContextObj) -> Self {
        // SAFETY: `ctx` was produced by `cgl_context_obj` from a live
        // `NSOpenGLContext` that outlives every lock/unlock call site in
        // this file (both threads hold their own retain on it for the
        // app's whole life -- see `embed`'s doc comment on `context_for_
        // driver`).
        unsafe { CGLLockContext(ctx) };
        CglLock(ctx)
    }
}

impl Drop for CglLock {
    fn drop(&mut self) {
        // SAFETY: balances the `CGLLockContext` call in `acquire` on the
        // same context, same thread (CGL locks are not meant to cross
        // threads -- this guard never does).
        unsafe {
            CGLUnlockContext(self.0);
        }
    }
}

/// `GL_COLOR_BUFFER_BIT` (`gl.h`) -- used only by `clear_to_black`'s render-
/// thread-side handling below to paint one plain black frame.
const GL_COLOR_BUFFER_BIT: u32 = 0x4000;

unsafe extern "C" {
    fn glClearColor(r: f32, g: f32, b: f32, a: f32);
    fn glClear(mask: u32);
}

impl VideoLayer {
    /// Must be called once, on the main thread, right after the GPUI window
    /// is created. Creates the sibling NSView + NSOpenGLContext, inserts it
    /// below GPUI's own content view (S1 mechanism), hands the GL context
    /// off to a dedicated render thread that owns `Player` and mpv's render
    /// API for the rest of the app's life, and spawns the main-thread
    /// geometry driver (B1, `spawn_geometry_driver`) that animates the video
    /// NSView's frame every tick. Blocks (briefly) waiting for the render
    /// thread to report `Player::new`'s result.
    ///
    /// `cx` is needed (not just `window`) because the geometry driver is a
    /// GPUI foreground task (`App::spawn` -- "Spawns the future... on the
    /// main thread", per its doc comment) rather than a raw AppKit
    /// mechanism (`NSTimer`/a libdispatch timer source): piggybacking on
    /// GPUI's own main-thread task scheduling avoids introducing a second,
    /// independent main-thread scheduling mechanism into an app that
    /// already has one.
    pub(crate) fn embed(window: &mut Window, cx: &mut App) -> Result<Self, String> {
        let mtm = MainThreadMarker::new().ok_or("VideoLayer::embed must run on the main thread")?;

        window.set_background_appearance(WindowBackgroundAppearance::Transparent);

        let raw = window
            .window_handle()
            .map_err(|e| format!("no window handle: {e}"))?
            .as_raw();
        let ns_view_ptr = match raw {
            RawWindowHandle::AppKit(handle) => handle.ns_view,
            other => return Err(format!("expected an AppKit window handle, got {other:?}")),
        };

        // SAFETY: `ns_view_ptr` points at GPUI's own content NSView
        // ("native_view"), guaranteed valid for as long as the window is
        // open.
        let native_view: &NSView = unsafe { ns_view_ptr.cast().as_ref() };
        // An owned retain, handed to `spawn_geometry_driver` so it can
        // maintain the Miniplayer video-visibility CALayer mask on this same
        // view every tick (see this module's doc comment) -- everything
        // else in this function keeps using the borrowed `native_view`
        // above, unaffected by this extra retain.
        let native_view_owned: Retained<NSView> = native_view.retain();
        let content_view: Retained<NSView> = unsafe { native_view.superview() }
            .ok_or("gpui's native_view should be a subview of the window's real contentView")?;
        let ns_window: Retained<NSWindow> = content_view
            .window()
            .ok_or("content view has no window yet")?;

        let frame = video_frame(content_view.bounds(), LayerMode::FullscreenInWindow);
        // `JellybeamVideoView`, not a plain `NSView` -- see its doc comment
        // above (input-transparent to hit-testing). No overridden
        // initializer, so the inherited `NSView` `initWithFrame:` is
        // invoked directly on the subclass's allocation via `msg_send!`
        // (the usual objc2 idiom for reusing a superclass's designated
        // initializer unchanged); `into_super()` then gives back the same
        // `Retained<NSView>` type this function already used everywhere
        // else below -- the object's *runtime* class (and therefore its
        // overridden `hitTest:`) is unaffected by that static Rust type.
        let video_view: Retained<NSView> = unsafe {
            let typed: Retained<JellybeamVideoView> =
                msg_send![mtm.alloc::<JellybeamVideoView>(), initWithFrame: frame];
            typed.into_super()
        };
        // Deliberately NOT an autoresizing mask: geometry is now
        // state-driven (Fullscreen-in-window vs. Miniplayer corner, see
        // `LayerMode`/`GeometryState`) and animated by the render thread's
        // tick loop below, not left to AppKit's autoresize machinery --
        // Miniplayer's corner rect must NOT track the window's full size.
        //
        // Deliberately NOT setWantsLayer(true): `NSOpenGLContext::setView`
        // targets a classic backing-store view. The layer-hosting
        // equivalent (`CAOpenGLLayer`) needs an Objective-C subclass
        // overriding `drawInCGLContext:`, which objc2's safe class-wrapper
        // macros don't make easy to author from Rust.

        #[rustfmt::skip]
        let mut attribs: Vec<u32> = vec![
            NSOpenGLPFAOpenGLProfile, NSOpenGLProfileVersion3_2Core,
            NSOpenGLPFAAccelerated,
            NSOpenGLPFADoubleBuffer,
            NSOpenGLPFAColorSize, 24,
            NSOpenGLPFAAlphaSize, 8,
            NSOpenGLPFADepthSize, 24,
            0,
        ];
        let attribs_ptr = NonNull::new(attribs.as_mut_ptr()).expect("attribs is non-empty");
        // SAFETY: `attribs_ptr` is a valid, 0-terminated
        // NSOpenGLPixelFormatAttribute array, alive for this call's duration.
        let pixel_format: Retained<NSOpenGLPixelFormat> =
            unsafe { NSOpenGLPixelFormat::initWithAttributes(mtm.alloc(), attribs_ptr) }.ok_or(
                "no matching NSOpenGLPixelFormat (need an accelerated Core Profile >= 3.2 context)",
            )?;

        let context: Retained<NSOpenGLContext> =
            NSOpenGLContext::initWithFormat_shareContext(mtm.alloc(), &pixel_format, None)
                .ok_or("failed to create NSOpenGLContext")?;
        context.setView(Some(&video_view), mtm);

        // Insert as a SIBLING of GPUI's native_view, positioned BELOW it, so
        // GPUI's chrome (opaque only where it explicitly paints) composites
        // on top of the video (S1 mechanism).
        content_view.addSubview_positioned_relativeTo(
            &video_view,
            NSWindowOrderingMode::Below,
            Some(native_view),
        );

        let backing_scale = ns_window.backingScaleFactor();

        // B1: a second retain on `context` for the main-thread geometry
        // driver spawned below -- the render thread gets its own retain via
        // the usual raw-pointer transfer (`GlHandles`, `Retained` isn't
        // `Send`) since it's a genuinely different OS thread, but the
        // geometry driver runs as a GPUI foreground task on this same main
        // OS thread, so it can just capture a `Retained` directly, no
        // marshaling needed. Both retains independently manage the same
        // underlying NSOpenGLContext's refcount (atomic at the ObjC runtime
        // level); each thread calls its own distinct set of methods on it
        // (this one only ever calls `update`, main-thread-only -- see
        // `spawn_geometry_driver`'s doc comment; the render thread only ever
        // calls `makeCurrentContext`/`flushBuffer`, confirmed safe off-main
        // -- see `run_render_thread`'s doc comment) -- but "distinct
        // methods" alone doesn't make concurrent calls safe (see the
        // SIGSEGV this fixed, described below); the actual safety
        // invariant is `CglLock` serializing every touch from either thread
        // (see its doc comment above).
        let context_for_driver = context.clone();
        // Third retain, held by `VideoLayer` itself so `Drop` can
        // make the context current for `mpv_render_context_free`.
        let teardown_context = context.clone();

        let initial_w = (frame.size.width * backing_scale).max(1.0) as i32;
        let initial_h = (frame.size.height * backing_scale).max(1.0) as i32;
        let render_size = Arc::new(RenderSize::new(initial_w, initial_h));

        let handles = GlHandles {
            context: NonNull::new(Retained::into_raw(context))
                .expect("Retained::into_raw never returns null")
                .cast(),
        };

        let stop_flag = Arc::new(AtomicBool::new(false));
        let clear_flag = Arc::new(AtomicBool::new(false));
        let geometry = Arc::new(std::sync::Mutex::new(GeometryState {
            mode: LayerMode::FullscreenInWindow,
            last_frame: None,
            hovering: false,
        }));
        let (player_tx, player_rx) = std::sync::mpsc::channel::<Result<Arc<Player>, String>>();
        let thread_stop = stop_flag.clone();
        let thread_render_size = render_size.clone();
        let thread_clear_flag = clear_flag.clone();
        let render_thread = thread::Builder::new()
            .name("jellybeam-mpv-render".to_string())
            .spawn(move || {
                run_render_thread(
                    handles,
                    thread_stop,
                    thread_render_size,
                    thread_clear_flag,
                    player_tx,
                )
            })
            .map_err(|e| format!("failed to spawn render thread: {e}"))?;

        let player = player_rx
            .recv_timeout(Duration::from_secs(10))
            .map_err(|_| {
                "render thread did not report a Player::new result in time".to_string()
            })??;

        spawn_geometry_driver(
            video_view,
            ns_window,
            context_for_driver,
            native_view_owned,
            content_view,
            backing_scale,
            stop_flag.clone(),
            geometry.clone(),
            render_size,
            cx,
        );

        window.activate_window();

        Ok(VideoLayer {
            player: Some(player),
            teardown_context,
            stop_flag,
            render_thread: Some(render_thread),
            geometry,
            clear_flag,
        })
    }
}

impl Drop for VideoLayer {
    fn drop(&mut self) {
        self.stop_flag.store(true, Ordering::Release);
        if let Some(handle) = self.render_thread.take() {
            let _ = handle.join();
        }
        // `Player::drop` runs `mpv_render_context_free`, an
        // `mpv_render_*` function -- render.h "Threading" requires the same
        // GL context the render context was created with be current on the
        // calling thread for ALL of them, and the render thread cleared it
        // on its way out just above. Re-establish it here (under the same
        // `CglLock` every other touch of the shared context takes) so the
        // free -- and mpv's glDelete* teardown behind it -- runs legally
        // instead of against no context (GPU-object leak + documented UB).
        if let Some(player) = self.player.take() {
            let cgl = cgl_context_obj(&self.teardown_context);
            let lock = CglLock::acquire(cgl);
            self.teardown_context.makeCurrentContext();
            // playback.rs's diagnostic tasks hold `Arc<Player>` clones and
            // are `abort()`ed before teardown, but `JoinHandle::abort` is
            // asynchronous -- wait briefly so the final release (and thus
            // the free) happens on THIS thread with the context current,
            // not on a Tokio worker with none. If a clone still lingers
            // after the grace period, proceeding merely reproduces the old
            // behavior for that rare case; debug builds assert loudly.
            for _ in 0..100 {
                if Arc::strong_count(&player) == 1 {
                    break;
                }
                thread::sleep(Duration::from_millis(5));
            }
            debug_assert_eq!(
                Arc::strong_count(&player),
                1,
                "stray Arc<Player> clone would run mpv_render_context_free \
                 on a thread with no GL context current"
            );
            drop(player);
            NSOpenGLContext::clearCurrentContext();
            drop(lock);
        }
    }
}

/// Runs on the dedicated render thread for the app's whole lifetime.
/// Reconstructs the GL context from the raw pointer handed off by `embed`
/// (S1 pattern: `Retained<T>` isn't `Send`), makes it current on THIS
/// thread once and keeps it current for every
/// `Player::new`/`render`/`report_swap` call -- `player::Player::render`'s
/// doc comment requires the same context stay current on the same thread
/// for the `Player`'s whole life.
///
/// B1: this thread used to also own the video `NSView`/its `NSWindow` and
/// read/wrote their geometry (`frame`/`bounds`/`setFrame`/`contentView`)
/// every tick -- all of which are main-thread-only AppKit APIs, called here
/// from a thread that is provably not the main thread (the same crash class
/// `-update`'s fix below already existed for, just not yet applied to these
/// calls). It no longer touches AppKit at all: `spawn_geometry_driver`'s
/// real-main-thread task now owns all NSView/NSWindow geometry (including
/// calling `-update` directly, since it genuinely runs on the main thread)
/// and hands this thread only the pixel size it needs via [`RenderSize`].
///
/// Calling `NSOpenGLContext::makeCurrentContext`/`flushBuffer` off the real
/// AppKit main thread is exactly the shape Apple's own legacy
/// CVDisplayLink-driven `NSOpenGLView` sample code uses (a dedicated thread
/// owns the context and drives rendering itself), and it's the same
/// contract mpv's own render-thread model expects. `-update` was the one
/// documented exception (crashed for real, `EXC_BREAKPOINT`/SIGTRAP inside
/// AppKit, once called off-thread) -- it's no longer called from this
/// thread at all now. But "off-main is fine" and "concurrent with the main
/// thread is fine" are different claims: this thread's `makeCurrentContext`/
/// `render`/`flushBuffer` sequence and the main-thread geometry driver's
/// `setFrame`/`update` calls both mutate state on the *same* shared
/// `NSOpenGLContext`, and nothing serialized them against each other --
/// which is exactly what crashed for real a second time: a SIGSEGV inside
/// `glClear`, this thread, captured mid-render while the main thread was
/// concurrently deep in an AppKit display cycle. Every touch of `context`
/// below is now wrapped in
/// [`CglLock`] (see its doc comment) to fix that for good.
fn run_render_thread(
    handles: GlHandles,
    stop: Arc<AtomicBool>,
    render_size: Arc<RenderSize>,
    clear_flag: Arc<AtomicBool>,
    player_tx: std::sync::mpsc::Sender<Result<Arc<Player>, String>>,
) {
    // SAFETY: this pointer was produced by `Retained::into_raw` on the main
    // thread just before this thread was spawned, carrying ownership of
    // exactly one retain. `Retained::from_raw` reclaims that retain without
    // incrementing again (unlike `Retained::retain`), so this doesn't leak
    // and doesn't double-free.
    let context: Retained<NSOpenGLContext> =
        unsafe { Retained::from_raw(handles.context.as_ptr().cast()) }.expect("non-null");
    let cgl_ctx = cgl_context_obj(&context);

    {
        let _lock = CglLock::acquire(cgl_ctx);
        context.makeCurrentContext();
    }

    let player = match Player::new(gl_get_proc_address) {
        Ok(p) => Arc::new(p),
        Err(e) => {
            let _ = player_tx.send(Err(format!("Player::new failed: {e}")));
            return;
        }
    };
    let _ = player_tx.send(Ok(player.clone()));
    tracing::info!("mpv render context created; video render thread running");

    let frame_budget = Duration::from_millis(16); // ~60Hz poll, matches S1's render thread.

    while !stop.load(Ordering::Acquire) {
        let tick_start = std::time::Instant::now();

        // docs/UX-SPEC.md: `VideoLayer::clear_to_black` (stop-playback) request --
        // painted with priority over a stale `needs_render` (there
        // shouldn't be one right after `Player::stop`, but if mpv still had
        // a frame queued, black must win).
        if clear_flag.swap(false, Ordering::AcqRel) {
            let _lock = CglLock::acquire(cgl_ctx);
            unsafe {
                glClearColor(0.0, 0.0, 0.0, 1.0);
                glClear(GL_COLOR_BUFFER_BIT);
            }
            context.flushBuffer();
        } else if player.needs_render() {
            let (w, h) = render_size.load();
            let _lock = CglLock::acquire(cgl_ctx);
            match player.render(0, w, h) {
                Ok(()) => {
                    context.flushBuffer();
                    player.report_swap();
                }
                Err(e) => tracing::warn!(error = %e, "player.render failed"),
            }
        }

        let elapsed = tick_start.elapsed();
        if elapsed < frame_budget {
            thread::sleep(frame_budget - elapsed);
        }
    }

    {
        let _lock = CglLock::acquire(cgl_ctx);
        NSOpenGLContext::clearCurrentContext();
    }
    tracing::info!("video render thread stopped");
}

/// B1 fix: drives all video `NSView`/`NSWindow`/`NSOpenGLContext` geometry
/// touches (`frame`/`bounds`/`setFrame`/`contentView`/`update`) from a GPUI
/// foreground task -- `App::spawn`'s doc comment: "Spawns the future
/// returned by the given function on the main thread" -- instead of from
/// `run_render_thread`'s dedicated (non-main) render thread, which is what
/// this whole fix is about (see `run_render_thread`'s and `GlHandles`' doc
/// comments for the crash this replaces). Every ~16ms (matching the render
/// thread's own tick cadence): detects a `LayerMode` change requested via
/// `VideoLayer::set_layer_mode`, (re)computes the lerp toward the target
/// rect, calls `view.setFrame` if it actually moved, publishes the result to
/// `geometry.last_frame` (read by `VideoLayer::current_frame`, primarily
/// `JELLYBEAM_E2E`'s geometry assertions), and publishes the current
/// backing-scaled pixel size to `render_size` for the render thread to pick
/// up on its own next tick. Runs until `stop` is set (`VideoLayer::drop`);
/// intentionally detached (like every other app-lifetime GPUI task in this
/// codebase, e.g. `main.rs`'s `spawn_player_events_task`) rather than held
/// onto, since nothing needs to await its completion.
#[allow(clippy::too_many_arguments)] // plain data parameters, no natural grouping.
fn spawn_geometry_driver(
    view: Retained<NSView>,
    window: Retained<NSWindow>,
    context: Retained<NSOpenGLContext>,
    // GPUI's own content view + its superview, for the Miniplayer
    // video-visibility CALayer mask -- see this module's doc comment.
    native_view: Retained<NSView>,
    content_view: Retained<NSView>,
    backing_scale: f64,
    stop: Arc<AtomicBool>,
    geometry: Arc<std::sync::Mutex<GeometryState>>,
    render_size: Arc<RenderSize>,
    cx: &mut App,
) {
    const GEOMETRY_TICK: Duration = Duration::from_millis(16);

    let cgl_ctx = cgl_context_obj(&context);

    cx.spawn(async move |cx| {
        // Geometry animation bookkeeping: `current_mode`/`anim_from`/
        // `anim_started` are all local to this task -- `geometry.mode` is
        // the only thing another task/thread (`VideoLayer::set_layer_mode`)
        // ever writes, so a change is detected here by simple comparison
        // each tick, capturing the view's actual on-screen frame at that
        // instant as the animation's start point (see `GeometryState`'s doc
        // comment for why this is safer than trying to snapshot the frame
        // from the calling thread).
        let mut current_mode = LayerMode::FullscreenInWindow;
        let mut anim_from = view.frame();
        let mut anim_started = std::time::Instant::now();
        let mut last_size = view.bounds().size;
        // The Miniplayer video-visibility mask (see this module's doc
        // comment). `mask_layer` is created lazily and reused every tick
        // (only its `.path` changes, tracking the animation/drag); `applied`
        // mirrors whether it's currently assigned to `native_view.layer()`
        // so `setMask` is only called on an actual on/off transition, not
        // every tick.
        let mut mask_layer: Option<Retained<CAShapeLayer>> = None;
        let mut mask_applied = false;
        while !stop.load(Ordering::Acquire) {
            let tick_start = std::time::Instant::now();

            // This task is only ever polled on the real main thread
            // (`App::spawn`'s contract), so this is always `Some` in
            // practice -- checked (not `new_unchecked`) anyway, matching
            // this file's general preference for a real, verified
            // `MainThreadMarker` over an asserted one wherever the cost is
            // negligible (see the removed `main_thread_gl_update`'s old doc
            // comment, which used to make the same tradeoff for the same
            // reason).
            let Some(mtm) = MainThreadMarker::new() else {
                cx.background_executor().timer(GEOMETRY_TICK).await;
                continue;
            };

            let desired_mode = lock_ignore_poison(&geometry).mode;
            if desired_mode != current_mode {
                anim_from = view.frame();
                anim_started = tick_start;
                current_mode = desired_mode;
            }
            let content_bounds = window
                .contentView()
                .map(|v| v.bounds())
                .unwrap_or_else(|| view.bounds());
            let target = video_frame(content_bounds, current_mode);
            let progress =
                anim_started.elapsed().as_secs_f64() / LAYER_ANIM.as_secs_f64().max(1e-6);
            let new_frame = if progress >= 1.0 {
                target
            } else {
                lerp_rect(anim_from, target, progress)
            };
            let prev_bounds = view.bounds();
            if (new_frame.origin.x - view.frame().origin.x).abs() > 0.01
                || (new_frame.origin.y - view.frame().origin.y).abs() > 0.01
                || (new_frame.size.width - prev_bounds.size.width).abs() > 0.01
                || (new_frame.size.height - prev_bounds.size.height).abs() > 0.01
            {
                // `setFrame` resizes/repositions the drawable backing the
                // shared `NSOpenGLContext` -- must not race the render
                // thread's `render`/`flushBuffer` (see `CglLock`'s doc
                // comment; this is the exact class of touch that crashed
                // for real, with a SIGSEGV).
                let _lock = CglLock::acquire(cgl_ctx);
                view.setFrame(new_frame);
            }
            let want_hole = {
                let mut g = lock_ignore_poison(&geometry);
                g.last_frame = Some((
                    new_frame.origin.x,
                    new_frame.origin.y,
                    new_frame.size.width,
                    new_frame.size.height,
                ));
                matches!(current_mode, LayerMode::Miniplayer(_)) && !g.hovering
            };

            // Miniplayer video-visibility mask -- see this module's doc
            // comment ("Miniplayer video visibility") for why this exists
            // and why it's hover-gated. `new_frame` is exactly the video
            // NSView's own current on-screen rect (already computed above,
            // mid-animation/drag included), converted from `content_view`'s
            // coordinate space into `native_view`'s own -- `convertRect:
            // toView:` is used specifically so this doesn't have to reason
            // about either view's `isFlipped`/layer `geometryFlipped`
            // state by hand.
            if want_hole {
                let hole = content_view.convertRect_toView(new_frame, Some(&native_view));
                let outer = native_view.bounds();
                let shape = mask_layer.get_or_insert_with(|| {
                    let shape = CAShapeLayer::new();
                    shape.setFillColor(Some(&CGColor::new_generic_gray(0.0, 1.0)));
                    // SAFETY: `kCAFillRuleEvenOdd` is a valid, immutable,
                    // process-lifetime `NSString` constant exported by the
                    // QuartzCore framework this crate links against.
                    shape.setFillRule(unsafe { kCAFillRuleEvenOdd });
                    shape
                });
                let path = CGMutablePath::new();
                // SAFETY: `path` was just created and is uniquely owned
                // here; a null transform pointer is CoreGraphics's
                // documented way to add a rect/rounded-rect untransformed.
                unsafe {
                    CGMutablePath::add_rect(Some(&path), std::ptr::null(), outer);
                    // §7: "RADIUS_MINIPLAYER (12px) rounded corners ON THE
                    // VIDEO" -- the cutout hole this evenodd path punches
                    // becomes a rounded rect (`CGPathAddRoundedRect`, was a
                    // plain `add_rect`) instead of a sharp-cornered one, so
                    // the video itself reads as rounded (not just whatever
                    // GPUI chrome happens to be painted around it) -- see
                    // this module's "Miniplayer video visibility" doc
                    // section above for the CGL/mask discipline this
                    // preserves unchanged (same evenodd two-subpath shape,
                    // same hover-gated apply/lift, only the hole's own
                    // geometry changed).
                    CGMutablePath::add_rounded_rect(
                        Some(&path),
                        std::ptr::null(),
                        hole,
                        miniplayer_corner_radius(),
                        miniplayer_corner_radius(),
                    );
                }
                shape.setPath(Some(&path));
                if !mask_applied {
                    if let Some(layer) = native_view.layer() {
                        // SAFETY: `shape` is a live `CAShapeLayer` (a
                        // `CALayer` subclass) owned by this same task;
                        // assigning it as another layer's mask is a normal,
                        // main-thread-only CoreAnimation operation (this
                        // whole task only ever runs on the main thread, see
                        // the `mtm` check above).
                        unsafe { layer.setMask(Some(shape)) };
                    }
                    mask_applied = true;
                }
            } else if mask_applied {
                if let Some(layer) = native_view.layer() {
                    // SAFETY: see above.
                    unsafe { layer.setMask(None) };
                }
                mask_applied = false;
            }

            let bounds = view.bounds();
            let w = (bounds.size.width * backing_scale).max(1.0) as i32;
            let h = (bounds.size.height * backing_scale).max(1.0) as i32;
            render_size.store(w, h);

            if (bounds.size.width - last_size.width).abs() > 0.5
                || (bounds.size.height - last_size.height).abs() > 0.5
            {
                last_size = bounds.size;
                // Tells the GL context its backing store changed size
                // (frame changes now come from the geometry animation
                // above, not AppKit's autoresizing mask -- see `embed`'s
                // doc comment on why the mask was dropped). Called
                // directly now -- no `dispatch_async_f` bounce needed --
                // because this task genuinely runs on the main thread
                // (`mtm` above proves it), unlike the old render-thread
                // call site this replaces. Locked for the same reason
                // `setFrame` above is: `update` re-validates the shared
                // context against the (possibly just-resized) drawable, and
                // must not race the render thread's use of that same
                // context.
                let _lock = CglLock::acquire(cgl_ctx);
                context.update(mtm);
            }

            let elapsed = tick_start.elapsed();
            if elapsed < GEOMETRY_TICK {
                cx.background_executor()
                    .timer(GEOMETRY_TICK - elapsed)
                    .await;
            }
        }
    })
    .detach();
}
