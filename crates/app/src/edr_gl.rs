//! The EDR encode pass (crates/app/ARCHITECTURE.md "HDR output"): mpv
//! renders extended-range linear light into an RGBA16F texture, and one
//! full-screen triangle writes it to the float drawable with the sRGB curve
//! continued past 1.0, the encoding the compositor reads an untagged float
//! GL surface in. Exists only while HDR plays; SDR frames go straight to the
//! drawable as before.

use std::ffi::{c_char, c_int, c_uint, CString};

type GLenum = c_uint;
type GLuint = c_uint;
type GLint = c_int;
type GLsizei = c_int;

const GL_TEXTURE_2D: GLenum = 0x0DE1;
const GL_TEXTURE0: GLenum = 0x84C0;
const GL_RGBA16F: GLint = 0x881A;
const GL_RGBA: GLenum = 0x1908;
const GL_FLOAT: GLenum = 0x1406;
const GL_TEXTURE_MIN_FILTER: GLenum = 0x2801;
const GL_TEXTURE_MAG_FILTER: GLenum = 0x2800;
const GL_TEXTURE_WRAP_S: GLenum = 0x2802;
const GL_TEXTURE_WRAP_T: GLenum = 0x2803;
const GL_NEAREST: GLint = 0x2600;
const GL_CLAMP_TO_EDGE: GLint = 0x812F;
const GL_FRAMEBUFFER: GLenum = 0x8D40;
const GL_COLOR_ATTACHMENT0: GLenum = 0x8CE0;
const GL_FRAMEBUFFER_COMPLETE: GLenum = 0x8CD5;
const GL_VERTEX_SHADER: GLenum = 0x8B31;
const GL_FRAGMENT_SHADER: GLenum = 0x8B30;
const GL_COMPILE_STATUS: GLenum = 0x8B81;
const GL_LINK_STATUS: GLenum = 0x8B82;
const GL_BLEND: GLenum = 0x0BE2;
const GL_SCISSOR_TEST: GLenum = 0x0C11;
const GL_DEPTH_TEST: GLenum = 0x0B71;
const GL_TRIANGLES: GLenum = 0x0004;

/// `mpv_opengl_fbo.internal_format` for the intermediate texture.
pub(crate) const INTERMEDIATE_FORMAT: i32 = GL_RGBA16F;

// OpenGL.framework exports every GL 3.2 Core entry point directly (see
// build.rs), the same way `gl_video.rs` already calls `glClear`.
unsafe extern "C" {
    fn glGenTextures(n: GLsizei, textures: *mut GLuint);
    fn glDeleteTextures(n: GLsizei, textures: *const GLuint);
    fn glBindTexture(target: GLenum, texture: GLuint);
    fn glActiveTexture(texture: GLenum);
    fn glTexImage2D(
        target: GLenum,
        level: GLint,
        internal_format: GLint,
        width: GLsizei,
        height: GLsizei,
        border: GLint,
        format: GLenum,
        ty: GLenum,
        data: *const std::ffi::c_void,
    );
    fn glTexParameteri(target: GLenum, pname: GLenum, param: GLint);
    fn glGenFramebuffers(n: GLsizei, ids: *mut GLuint);
    fn glDeleteFramebuffers(n: GLsizei, ids: *const GLuint);
    fn glBindFramebuffer(target: GLenum, fb: GLuint);
    fn glFramebufferTexture2D(
        target: GLenum,
        attachment: GLenum,
        textarget: GLenum,
        texture: GLuint,
        level: GLint,
    );
    fn glCheckFramebufferStatus(target: GLenum) -> GLenum;
    fn glCreateShader(ty: GLenum) -> GLuint;
    fn glShaderSource(
        shader: GLuint,
        count: GLsizei,
        strings: *const *const c_char,
        lengths: *const GLint,
    );
    fn glCompileShader(shader: GLuint);
    fn glGetShaderiv(shader: GLuint, pname: GLenum, out: *mut GLint);
    fn glGetShaderInfoLog(shader: GLuint, max: GLsizei, len: *mut GLsizei, log: *mut c_char);
    fn glDeleteShader(shader: GLuint);
    fn glCreateProgram() -> GLuint;
    fn glAttachShader(program: GLuint, shader: GLuint);
    fn glLinkProgram(program: GLuint);
    fn glGetProgramiv(program: GLuint, pname: GLenum, out: *mut GLint);
    fn glDeleteProgram(program: GLuint);
    fn glUseProgram(program: GLuint);
    fn glGetUniformLocation(program: GLuint, name: *const c_char) -> GLint;
    fn glUniform1i(location: GLint, v: GLint);
    fn glGenVertexArrays(n: GLsizei, arrays: *mut GLuint);
    fn glDeleteVertexArrays(n: GLsizei, arrays: *const GLuint);
    fn glBindVertexArray(array: GLuint);
    fn glDrawArrays(mode: GLenum, first: GLint, count: GLsizei);
    fn glViewport(x: GLint, y: GLint, w: GLsizei, h: GLsizei);
    fn glDisable(cap: GLenum);
}

/// A full-screen triangle from `gl_VertexID`; Core Profile still needs a
/// bound VAO but no vertex buffer.
const VERTEX_SRC: &str = "#version 150
out vec2 uv;
void main() {
    vec2 p = vec2(float((gl_VertexID << 1) & 2), float(gl_VertexID & 2));
    uv = p;
    gl_Position = vec4(p * 2.0 - 1.0, 0.0, 1.0);
}
";

/// The sRGB transfer, mirrored for negatives and continued above 1.0 as
/// CoreGraphics' extended-range colour spaces define it.
const FRAGMENT_SRC: &str = "#version 150
uniform sampler2D linear_tex;
in vec2 uv;
out vec4 frag;
void main() {
    vec3 l = texture(linear_tex, uv).rgb;
    vec3 a = abs(l);
    vec3 e = mix(a * 12.92, 1.055 * pow(a, vec3(1.0 / 2.4)) - 0.055,
                 step(vec3(0.0031308), a));
    frag = vec4(sign(l) * e, 1.0);
}
";

/// The intermediate target and encode program. Every method needs the
/// owning GL context current on the calling thread (the render thread).
pub(crate) struct EdrPass {
    program: GLuint,
    vao: GLuint,
    sampler_loc: GLint,
    target: Option<Target>,
}

struct Target {
    fbo: GLuint,
    tex: GLuint,
    width: i32,
    height: i32,
}

fn compile(ty: GLenum, src: &str) -> Result<GLuint, String> {
    let csrc = CString::new(src).map_err(|e| e.to_string())?;
    // SAFETY: plain GL calls on the current context with a live,
    // NUL-terminated source string; the info-log buffer is sized by `len`.
    unsafe {
        let shader = glCreateShader(ty);
        let ptr = csrc.as_ptr();
        glShaderSource(shader, 1, &ptr, std::ptr::null());
        glCompileShader(shader);
        let mut ok: GLint = 0;
        glGetShaderiv(shader, GL_COMPILE_STATUS, &mut ok);
        if ok == 0 {
            let mut log = vec![0 as c_char; 1024];
            let mut len: GLsizei = 0;
            glGetShaderInfoLog(shader, log.len() as GLsizei, &mut len, log.as_mut_ptr());
            glDeleteShader(shader);
            let bytes: Vec<u8> = log[..len.max(0) as usize]
                .iter()
                .map(|&c| c as u8)
                .collect();
            return Err(format!(
                "EDR shader compile failed: {}",
                String::from_utf8_lossy(&bytes)
            ));
        }
        Ok(shader)
    }
}

impl EdrPass {
    pub(crate) fn new() -> Result<Self, String> {
        let vs = compile(GL_VERTEX_SHADER, VERTEX_SRC)?;
        let fs = match compile(GL_FRAGMENT_SHADER, FRAGMENT_SRC) {
            Ok(fs) => fs,
            Err(e) => {
                // SAFETY: `vs` is a live shader name on the current context.
                unsafe { glDeleteShader(vs) };
                return Err(e);
            }
        };
        // SAFETY: plain GL object creation on the current context; the
        // shaders are detached by deletion once linked.
        unsafe {
            let program = glCreateProgram();
            glAttachShader(program, vs);
            glAttachShader(program, fs);
            glLinkProgram(program);
            glDeleteShader(vs);
            glDeleteShader(fs);
            let mut ok: GLint = 0;
            glGetProgramiv(program, GL_LINK_STATUS, &mut ok);
            if ok == 0 {
                glDeleteProgram(program);
                return Err("EDR program link failed".to_string());
            }
            let sampler_loc = glGetUniformLocation(program, c"linear_tex".as_ptr());
            let mut vao: GLuint = 0;
            glGenVertexArrays(1, &mut vao);
            Ok(EdrPass {
                program,
                vao,
                sampler_loc,
                target: None,
            })
        }
    }

    /// The FBO mpv renders into, (re)allocated at `width`x`height`.
    pub(crate) fn target_fbo(&mut self, width: i32, height: i32) -> Result<i32, String> {
        if let Some(t) = &self.target {
            if t.width == width && t.height == height {
                return Ok(t.fbo as i32);
            }
        }
        self.release_target();
        // SAFETY: plain GL object creation on the current context; the
        // texture is allocated with no initial data.
        unsafe {
            let mut tex: GLuint = 0;
            glGenTextures(1, &mut tex);
            glBindTexture(GL_TEXTURE_2D, tex);
            glTexImage2D(
                GL_TEXTURE_2D,
                0,
                GL_RGBA16F,
                width.max(1),
                height.max(1),
                0,
                GL_RGBA,
                GL_FLOAT,
                std::ptr::null(),
            );
            glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
            glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
            glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_WRAP_S, GL_CLAMP_TO_EDGE);
            glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_WRAP_T, GL_CLAMP_TO_EDGE);
            glBindTexture(GL_TEXTURE_2D, 0);
            let mut fbo: GLuint = 0;
            glGenFramebuffers(1, &mut fbo);
            glBindFramebuffer(GL_FRAMEBUFFER, fbo);
            glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D, tex, 0);
            let status = glCheckFramebufferStatus(GL_FRAMEBUFFER);
            glBindFramebuffer(GL_FRAMEBUFFER, 0);
            self.target = Some(Target {
                fbo,
                tex,
                width,
                height,
            });
            if status != GL_FRAMEBUFFER_COMPLETE {
                self.release_target();
                return Err(format!("EDR framebuffer incomplete: {status:#x}"));
            }
            Ok(fbo as i32)
        }
    }

    /// Encodes the intermediate texture into framebuffer `dst` (0 is the
    /// drawable). mpv rendered into it with the drawable's flip, so the copy
    /// is position-for-position.
    pub(crate) fn encode_into(&self, dst: u32, width: i32, height: i32) {
        let Some(t) = &self.target else {
            return;
        };
        // SAFETY: plain GL state and draw calls on the current context with
        // objects this pass owns; mpv resets its own state on every render.
        unsafe {
            glBindFramebuffer(GL_FRAMEBUFFER, dst);
            glViewport(0, 0, width, height);
            glDisable(GL_BLEND);
            glDisable(GL_SCISSOR_TEST);
            glDisable(GL_DEPTH_TEST);
            glUseProgram(self.program);
            glActiveTexture(GL_TEXTURE0);
            glBindTexture(GL_TEXTURE_2D, t.tex);
            glUniform1i(self.sampler_loc, 0);
            glBindVertexArray(self.vao);
            glDrawArrays(GL_TRIANGLES, 0, 3);
            glBindVertexArray(0);
            glBindTexture(GL_TEXTURE_2D, 0);
            glUseProgram(0);
        }
    }

    /// Frees the intermediate texture; the program stays for the next item.
    pub(crate) fn release_target(&mut self) {
        if let Some(t) = self.target.take() {
            // SAFETY: names created by `target_fbo` on this context.
            unsafe {
                glDeleteFramebuffers(1, &t.fbo);
                glDeleteTextures(1, &t.tex);
            }
        }
    }
}

impl Drop for EdrPass {
    fn drop(&mut self) {
        self.release_target();
        // SAFETY: names created by `new` on this context, which the render
        // thread keeps current until after this pass is dropped.
        unsafe {
            glDeleteVertexArrays(1, &self.vao);
            glDeleteProgram(self.program);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::c_void;

    type CglObj = *mut c_void;
    unsafe extern "C" {
        fn CGLChoosePixelFormat(attribs: *const c_int, pix: *mut CglObj, npix: *mut c_int)
            -> c_int;
        fn CGLCreateContext(pix: CglObj, share: CglObj, ctx: *mut CglObj) -> c_int;
        fn CGLSetCurrentContext(ctx: CglObj) -> c_int;
        fn CGLDestroyContext(ctx: CglObj) -> c_int;
        fn CGLDestroyPixelFormat(pix: CglObj) -> c_int;
        fn glClearColor(r: f32, g: f32, b: f32, a: f32);
        fn glClear(mask: c_uint);
        fn glScissor(x: GLint, y: GLint, w: GLsizei, h: GLsizei);
        fn glEnable(cap: GLenum);
        fn glReadPixels(
            x: GLint,
            y: GLint,
            w: GLsizei,
            h: GLsizei,
            format: GLenum,
            ty: GLenum,
            data: *mut c_void,
        );
    }

    /// Offscreen 3.2 Core context, current on this test's thread.
    struct Ctx(CglObj, CglObj);
    impl Ctx {
        fn new() -> Ctx {
            // kCGLPFAOpenGLProfile, kCGLOGLPVersion_3_2_Core, terminator.
            let attribs = [99, 0x3200, 0];
            let (mut pix, mut n, mut ctx) = (std::ptr::null_mut(), 0, std::ptr::null_mut());
            // SAFETY: valid attribute list and out-pointers.
            unsafe {
                assert_eq!(CGLChoosePixelFormat(attribs.as_ptr(), &mut pix, &mut n), 0);
                assert_eq!(CGLCreateContext(pix, std::ptr::null_mut(), &mut ctx), 0);
                assert_eq!(CGLSetCurrentContext(ctx), 0);
            }
            Ctx(ctx, pix)
        }
    }
    impl Drop for Ctx {
        fn drop(&mut self) {
            // SAFETY: objects created in `new`.
            unsafe {
                CGLSetCurrentContext(std::ptr::null_mut());
                CGLDestroyContext(self.0);
                CGLDestroyPixelFormat(self.1);
            }
        }
    }

    fn srgb_encode(l: f32) -> f32 {
        let a = l.abs();
        let e = if a < 0.003_130_8 {
            a * 12.92
        } else {
            1.055 * a.powf(1.0 / 2.4) - 0.055
        };
        e.copysign(l)
    }

    #[test]
    fn encode_pass_extends_srgb_above_one_and_keeps_orientation() {
        let _ctx = Ctx::new();
        let mut pass = EdrPass::new().expect("EDR pass builds on a 3.2 Core context");
        let (w, h) = (4, 4);

        // Destination: a second float target standing in for the drawable.
        let mut dst = EdrPass::new().expect("second pass builds");
        let dst_fbo = dst.target_fbo(w, h).expect("destination target") as u32;

        let src_fbo = pass.target_fbo(w, h).expect("source target") as u32;
        // SAFETY: GL calls on the current test context.
        unsafe {
            glBindFramebuffer(GL_FRAMEBUFFER, src_fbo);
            glViewport(0, 0, w, h);
            glClearColor(0.0, 0.0, 0.0, 1.0);
            glClear(0x4000);
            // Bottom half: 4x reference white in red, 0.18 in green.
            glEnable(GL_SCISSOR_TEST);
            glScissor(0, 0, w, h / 2);
            glClearColor(4.0, 0.18, 0.0, 1.0);
            glClear(0x4000);
            glDisable(GL_SCISSOR_TEST);
        }
        pass.encode_into(dst_fbo, w, h);

        let mut px = vec![0f32; (w * h * 4) as usize];
        // SAFETY: `px` holds w*h RGBA floats.
        unsafe {
            glBindFramebuffer(GL_FRAMEBUFFER, dst_fbo);
            glReadPixels(0, 0, w, h, GL_RGBA, GL_FLOAT, px.as_mut_ptr().cast());
            glBindFramebuffer(GL_FRAMEBUFFER, 0);
        }
        let bottom = &px[0..4];
        let top = &px[((h - 1) * w * 4) as usize..((h - 1) * w * 4 + 4) as usize];
        let close = |a: f32, b: f32| (a - b).abs() < 0.01;
        assert!(close(bottom[0], srgb_encode(4.0)), "{bottom:?}");
        assert!(
            bottom[0] > 1.0,
            "values above SDR white survive: {bottom:?}"
        );
        assert!(close(bottom[1], srgb_encode(0.18)), "{bottom:?}");
        assert!(close(bottom[3], 1.0));
        assert!(close(top[0], 0.0) && close(top[1], 0.0), "{top:?}");

        pass.release_target();
        assert!(pass.target.is_none());
    }
}
