//! Shared test harness for `player`'s headless integration tests: render
//! to an offscreen FBO, no window.
//!
//! Provides:
//! - A pbuffer-less offscreen CGL context (`GlContext`): no window, no
//!   display attach, just an OpenGL context made current so mpv's render API and our own readback
//!   GL calls have somewhere to run.
//! - `get_proc_address`, resolved via `dlsym`/`RTLD_DEFAULT` against
//!   `OpenGL.framework` (already linked into the test binary — see
//!   `build.rs`), matching `Player::new`'s `fn(&str) -> *mut c_void`
//!   signature exactly (a plain fn item, no captures).
//! - A small FBO/texture/`glReadPixels` helper for the rendered-frame pixel
//!   test.
//! - `media()` for locating `dev/media` corpus files, and `collect_events`
//!   for bridging `Player::events()`'s tokio receiver onto a plain
//!   `std::sync::mpsc::Receiver` so tests can use `recv_timeout`.

#![allow(dead_code)] // not every test file uses every helper here.

use std::ffi::{c_char, c_void, CString};
use std::os::raw::{c_int, c_uint};
use std::path::PathBuf;
use std::sync::mpsc as std_mpsc;
use std::time::Duration;

use player::{Player, PlayerEvent};

// ---------------------------------------------------------------------
// dlsym-based get_proc_address. mpv never owns a window; the app hands it
// its own GL context's resolver the same way.
// ---------------------------------------------------------------------

unsafe extern "C" {
    fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
}

/// A plain top-level fn (no captured state) so it satisfies `Player::new`'s
/// `fn(&str) -> *mut c_void` parameter type directly.
pub fn gl_get_proc_address(name: &str) -> *mut c_void {
    let Ok(cname) = CString::new(name) else {
        return std::ptr::null_mut();
    };
    // RTLD_DEFAULT == -2 on Darwin (dlfcn.h). Search the whole process image
    // — OpenGL.framework is already linked into this test binary (build.rs
    // links it whenever CARGO_CFG_TARGET_OS == "macos").
    let rtld_default = (-2isize) as *mut c_void;
    unsafe { dlsym(rtld_default, cname.as_ptr()) }
}

// ---------------------------------------------------------------------
// CGL: pbuffer-less offscreen context
// ---------------------------------------------------------------------

type CglError = c_int;
type CglPixelFormatObj = *mut c_void;
type CglContextObj = *mut c_void;
type CglPixelFormatAttribute = c_int;

const K_CGL_PFA_ACCELERATED: CglPixelFormatAttribute = 73;
/// `kCGLPFAOpenGLProfile` (OpenGL/CGLTypes.h): selects which GL profile
/// `CGLChoosePixelFormat` should hand back, paired with one of the
/// `kCGLOGLPVersion_*` values below.
const K_CGL_PFA_OPENGL_PROFILE: CglPixelFormatAttribute = 99;
/// `kCGLOGLPVersion_3_2_Core` (OpenGL/CGLTypes.h): a Core Profile, GL >= 3.2
/// context. Without this, `CGLChoosePixelFormat` on macOS defaults to a
/// legacy/compatibility-profile context, under which mpv's VideoToolbox
/// hwdec interop silently falls back to software decoding instead of
/// erroring — see the Core Profile contract note on `player::Player::new`.
const K_CGL_OGLP_VERSION_3_2_CORE: CglPixelFormatAttribute = 0x3200;

unsafe extern "C" {
    fn CGLChoosePixelFormat(
        attribs: *const CglPixelFormatAttribute,
        pix: *mut CglPixelFormatObj,
        npix: *mut c_int,
    ) -> CglError;
    fn CGLCreateContext(
        pix: CglPixelFormatObj,
        share: CglContextObj,
        ctx: *mut CglContextObj,
    ) -> CglError;
    fn CGLSetCurrentContext(ctx: CglContextObj) -> CglError;
    fn CGLDestroyContext(ctx: CglContextObj) -> CglError;
    fn CGLDestroyPixelFormat(pix: CglPixelFormatObj) -> CglError;
}

fn choose_pixel_format(accelerated: bool) -> Result<CglPixelFormatObj, String> {
    // Core Profile >= 3.2 in every case (accelerated or not): mpv's
    // hwdec=videotoolbox path needs it (see the constant doc comments
    // above), and a headless test harness that silently exercised the
    // software-decode path instead of hardware would defeat the point of
    // the hwdec-current test coverage this enables.
    let mut attribs: Vec<CglPixelFormatAttribute> = if accelerated {
        vec![K_CGL_PFA_ACCELERATED]
    } else {
        vec![]
    };
    attribs.push(K_CGL_PFA_OPENGL_PROFILE);
    attribs.push(K_CGL_OGLP_VERSION_3_2_CORE);
    attribs.push(0);
    let mut pix: CglPixelFormatObj = std::ptr::null_mut();
    let mut npix: c_int = 0;
    // SAFETY: `attribs` is a valid, 0-terminated CGL attribute list;
    // `pix`/`npix` are correctly-typed out-params.
    let err = unsafe { CGLChoosePixelFormat(attribs.as_ptr(), &mut pix, &mut npix) };
    if err != 0 || pix.is_null() || npix == 0 {
        return Err(format!(
            "CGLChoosePixelFormat failed (error={err}, npix={npix}, accelerated={accelerated})"
        ));
    }
    Ok(pix)
}

/// A headless CGL context with no attached drawable/pbuffer — we only ever
/// render into our own FBOs, never `CGLFlushDrawable`.
pub struct GlContext {
    ctx: CglContextObj,
    pix: CglPixelFormatObj,
}

impl GlContext {
    /// Creates the context and makes it current on the calling thread.
    /// Tries an accelerated (real GPU) pixel format first, falling back to
    /// "any renderer" if that's unavailable in this environment.
    pub fn new_current() -> Result<Self, String> {
        let pix = choose_pixel_format(true).or_else(|_| choose_pixel_format(false))?;

        let mut ctx: CglContextObj = std::ptr::null_mut();
        // SAFETY: `pix` was just produced by a successful CGLChoosePixelFormat.
        let err = unsafe { CGLCreateContext(pix, std::ptr::null_mut(), &mut ctx) };
        if err != 0 || ctx.is_null() {
            unsafe { CGLDestroyPixelFormat(pix) };
            return Err(format!("CGLCreateContext failed (error={err})"));
        }

        // SAFETY: `ctx` was just created successfully.
        let err = unsafe { CGLSetCurrentContext(ctx) };
        if err != 0 {
            unsafe {
                CGLDestroyContext(ctx);
                CGLDestroyPixelFormat(pix);
            }
            return Err(format!("CGLSetCurrentContext failed (error={err})"));
        }

        Ok(GlContext { ctx, pix })
    }
}

impl Drop for GlContext {
    fn drop(&mut self) {
        unsafe {
            CGLSetCurrentContext(std::ptr::null_mut());
            CGLDestroyContext(self.ctx);
            CGLDestroyPixelFormat(self.pix);
        }
    }
}

// ---------------------------------------------------------------------
// Minimal GL bindings for the harness's own FBO setup / pixel readback.
// (`Player::render` calls into libmpv's own GL usage; this is just what the
// *test* needs to build a target FBO and read it back.)
// ---------------------------------------------------------------------

type GLenum = c_uint;
type GLuint = c_uint;
type GLint = c_int;
type GLsizei = c_int;

const GL_TEXTURE_2D: GLenum = 0x0DE1;
const GL_RGBA8: GLint = 0x8058;
const GL_RGBA: GLenum = 0x1908;
const GL_UNSIGNED_BYTE: GLenum = 0x1401;
const GL_FRAMEBUFFER: GLenum = 0x8D40;
const GL_COLOR_ATTACHMENT0: GLenum = 0x8CE0;
const GL_FRAMEBUFFER_COMPLETE: GLenum = 0x8CD5;

unsafe extern "C" {
    fn glGenTextures(n: GLsizei, textures: *mut GLuint);
    fn glBindTexture(target: GLenum, texture: GLuint);
    fn glTexImage2D(
        target: GLenum,
        level: GLint,
        internalformat: GLint,
        width: GLsizei,
        height: GLsizei,
        border: GLint,
        format: GLenum,
        type_: GLenum,
        pixels: *const c_void,
    );
    fn glTexParameteri(target: GLenum, pname: GLenum, param: GLint);
    fn glGenFramebuffers(n: GLsizei, framebuffers: *mut GLuint);
    fn glBindFramebuffer(target: GLenum, framebuffer: GLuint);
    fn glFramebufferTexture2D(
        target: GLenum,
        attachment: GLenum,
        textarget: GLenum,
        texture: GLuint,
        level: GLint,
    );
    fn glCheckFramebufferStatus(target: GLenum) -> GLenum;
    fn glViewport(x: GLint, y: GLint, width: GLsizei, height: GLsizei);
    fn glClearColor(r: f32, g: f32, b: f32, a: f32);
    fn glClear(mask: GLuint);
    fn glReadPixels(
        x: GLint,
        y: GLint,
        width: GLsizei,
        height: GLsizei,
        format: GLenum,
        type_: GLenum,
        pixels: *mut c_void,
    );
    fn glDeleteFramebuffers(n: GLsizei, framebuffers: *const GLuint);
    fn glDeleteTextures(n: GLsizei, textures: *const GLuint);
    fn glGetError() -> GLenum;
}

const GL_TEXTURE_MIN_FILTER: GLenum = 0x2801;
const GL_TEXTURE_MAG_FILTER: GLenum = 0x2800;
const GL_NEAREST: GLint = 0x2600;
const GL_COLOR_BUFFER_BIT: GLuint = 0x4000;

/// An offscreen render target: a texture-backed FBO, RGBA8, `width`x`height`.
pub struct Fbo {
    pub fbo: GLuint,
    pub tex: GLuint,
    pub width: i32,
    pub height: i32,
}

impl Fbo {
    pub fn new(width: i32, height: i32) -> Result<Self, String> {
        unsafe {
            let mut tex: GLuint = 0;
            glGenTextures(1, &mut tex);
            glBindTexture(GL_TEXTURE_2D, tex);
            glTexImage2D(
                GL_TEXTURE_2D,
                0,
                GL_RGBA8,
                width,
                height,
                0,
                GL_RGBA,
                GL_UNSIGNED_BYTE,
                std::ptr::null(),
            );
            glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
            glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);

            let mut fbo: GLuint = 0;
            glGenFramebuffers(1, &mut fbo);
            glBindFramebuffer(GL_FRAMEBUFFER, fbo);
            glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D, tex, 0);

            let status = glCheckFramebufferStatus(GL_FRAMEBUFFER);
            if status != GL_FRAMEBUFFER_COMPLETE {
                glDeleteFramebuffers(1, &fbo);
                glDeleteTextures(1, &tex);
                return Err(format!("FBO incomplete: status=0x{status:x}"));
            }

            // A visible, non-video clear color so a test can distinguish
            // "mpv never rendered into this at all" from "mpv rendered
            // uniform content" if that ever comes up.
            glViewport(0, 0, width, height);
            glClearColor(0.2, 0.4, 0.6, 1.0);
            glClear(GL_COLOR_BUFFER_BIT);

            let _ = glGetError(); // drain/ignore; not asserted on here.

            Ok(Fbo {
                fbo,
                tex,
                width,
                height,
            })
        }
    }

    /// Reads back RGBA8 pixels (`width * height * 4` bytes).
    pub fn read_pixels(&self) -> Vec<u8> {
        let mut buf = vec![0u8; (self.width * self.height * 4) as usize];
        unsafe {
            glBindFramebuffer(GL_FRAMEBUFFER, self.fbo);
            glReadPixels(
                0,
                0,
                self.width,
                self.height,
                GL_RGBA,
                GL_UNSIGNED_BYTE,
                buf.as_mut_ptr() as *mut c_void,
            );
        }
        buf
    }
}

impl Drop for Fbo {
    fn drop(&mut self) {
        unsafe {
            glDeleteFramebuffers(1, &self.fbo);
            glDeleteTextures(1, &self.tex);
        }
    }
}

/// True if the RGBA8 buffer isn't a single uniform color — i.e. something
/// was actually rasterized into it (as opposed to a flat clear color).
pub fn has_pixel_variation(rgba: &[u8]) -> bool {
    let Some(first) = rgba.chunks_exact(4).next() else {
        return false;
    };
    rgba.chunks_exact(4).any(|px| px != first)
}

// ---------------------------------------------------------------------
// Corpus + event helpers
// ---------------------------------------------------------------------

/// Absolute path to a file under `dev/media/` (repo root, two levels above
/// `crates/player`).
pub fn media(rel: &str) -> String {
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.pop(); // crates
    path.pop(); // repo root
    path.push("dev");
    path.push("media");
    path.push(rel);
    path.to_string_lossy().into_owned()
}

/// Bridges `Player::events()`'s tokio `Receiver` onto a `std::sync::mpsc`
/// receiver on a plain background thread, so tests can use
/// `recv_timeout`/`try_recv` without pulling in a tokio runtime.
pub fn collect_events(player: &Player) -> std_mpsc::Receiver<PlayerEvent> {
    let mut rx = player.events();
    let (tx, out_rx) = std_mpsc::channel();
    std::thread::spawn(move || {
        while let Some(event) = rx.blocking_recv() {
            if tx.send(event).is_err() {
                break;
            }
        }
    });
    out_rx
}

/// Fails the current test immediately if `event` is a `PlayerEvent::Error`.
///
/// `wait_for` below calls this on every event it sees before consulting the
/// caller's predicate, so playback tests get "no unexpected mpv error
/// arrived" coverage for free just by using `wait_for` — an mpv-reported
/// error (log-message-derived or an `MPV_END_FILE_REASON_ERROR`) during what
/// should be normal playback now surfaces as a loud, specific test failure
/// instead of `wait_for` silently discarding it as "doesn't match the
/// predicate" and the test either passing for the wrong reason or timing
/// out with a confusing "no event" message.
pub fn assert_not_player_error(event: &PlayerEvent) {
    if let PlayerEvent::Error(msg) = event {
        panic!("unexpected PlayerEvent::Error during normal playback: {msg}");
    }
}

/// Polls `rx` until `pred` matches an event or `timeout` elapses, returning
/// the matching event (subsequent events are dropped, which is fine — tests
/// use this for "eventually observe X").
///
/// Every event observed is passed through `assert_not_player_error` first
/// (see its doc comment) — an mpv error while waiting fails the test
/// immediately rather than being silently filtered out.
pub fn wait_for<T>(
    rx: &std_mpsc::Receiver<PlayerEvent>,
    timeout: Duration,
    mut pred: impl FnMut(&PlayerEvent) -> Option<T>,
) -> Option<T> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            return None;
        }
        match rx.recv_timeout(remaining) {
            Ok(event) => {
                assert_not_player_error(&event);
                if let Some(v) = pred(&event) {
                    return Some(v);
                }
            }
            Err(_) => return None,
        }
    }
}
