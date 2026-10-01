//! Hand-written raw FFI bindings to libmpv's client API (`mpv/client.h`),
//! render API (`mpv/render.h`), and OpenGL render backend (`mpv/render_gl.h`),
//! client API version 2.5.0 (mpv v0.41.0, see docs/BUILD.md).
//!
//! ## Why hand-written bindings instead of the `libmpv2` crate or `bindgen`
//!
//! - **We control the surface.** Jellybeam only needs ~25 of the several hundred
//!   symbols libmpv exports (create/init/destroy, option/property/command
//!   plumbing, the event loop, and the OpenGL render-API entry points). A
//!   hand-written, minimal `extern` block is easy to audit line-by-line
//!   against the vendored header (see the doc links on each item below) and
//!   carries zero unused surface.
//! - **No version skew risk.** `libmpv2` (crates.io) pins its own idea of
//!   what the client API looks like and lags upstream releases; since we
//!   vendor and pin libmpv ourselves (docs/BUILD.md: v0.41.0, client API 2.5.0),
//!   declaring exactly what that pinned header exports avoids trusting a
//!   third-party crate's compatibility claims for a brand-new mpv release.
//! - **No `bindgen`/`libclang` build-time dependency.** `bindgen` needs
//!   `libclang` available on every dev/CI machine; a hand-written module has
//!   no such requirement and is easy to review as ordinary Rust.
//! - The tradeoff: if we start needing many more symbols (e.g. full node
//!   introspection, hooks, async command replies), reconsider `bindgen`
//!   against the vendored headers directly (`vendor/prefix/include`) rather
//!   than growing this file by hand indefinitely.
//!
//! Struct layouts below are `#[repr(C)]` and field-for-field transcriptions
//! of the corresponding C structs; see the doc comment above each one for
//! the exact header it mirrors. This module is `unsafe` by nature (it's an
//! FFI boundary) — callers in the rest of this crate are responsible for
//! upholding libmpv's documented invariants (e.g. "the render API must be
//! called with the OpenGL context current").

#![allow(non_camel_case_types)]

use std::ffi::{c_char, c_int, c_void};

// ---------------------------------------------------------------------
// Opaque handles (mpv/client.h, mpv/render.h)
// ---------------------------------------------------------------------

/// Opaque `mpv_handle` (client.h).
#[repr(C)]
pub struct mpv_handle {
    _private: [u8; 0],
}

/// Opaque `mpv_render_context` (render.h).
#[repr(C)]
pub struct mpv_render_context {
    _private: [u8; 0],
}

// ---------------------------------------------------------------------
// mpv_format (client.h)
// ---------------------------------------------------------------------

pub type mpv_format = c_int;
pub const MPV_FORMAT_NONE: mpv_format = 0;
pub const MPV_FORMAT_STRING: mpv_format = 1;
#[allow(dead_code)]
pub const MPV_FORMAT_OSD_STRING: mpv_format = 2;
pub const MPV_FORMAT_FLAG: mpv_format = 3;
pub const MPV_FORMAT_INT64: mpv_format = 4;
pub const MPV_FORMAT_DOUBLE: mpv_format = 5;
pub const MPV_FORMAT_NODE: mpv_format = 6;
pub const MPV_FORMAT_NODE_ARRAY: mpv_format = 7;
pub const MPV_FORMAT_NODE_MAP: mpv_format = 8;
#[allow(dead_code)]
pub const MPV_FORMAT_BYTE_ARRAY: mpv_format = 9;

// ---------------------------------------------------------------------
// mpv_node and friends (client.h)
// ---------------------------------------------------------------------

#[repr(C)]
pub union mpv_node_u {
    pub string: *mut c_char,
    pub flag: c_int,
    pub int64: i64,
    pub double_: f64,
    pub list: *mut mpv_node_list,
    pub ba: *mut mpv_byte_array,
}

/// Mirrors `struct mpv_node` (client.h). Field order (`u` then `format`)
/// matches the header exactly, so `#[repr(C)]` layout matches libmpv's ABI.
#[repr(C)]
pub struct mpv_node {
    pub u: mpv_node_u,
    pub format: mpv_format,
}

impl Default for mpv_node {
    fn default() -> Self {
        // MPV_FORMAT_NONE == 0, so zero-init is a valid empty node (this is
        // explicitly called out as intentional in client.h's mpv_format doc).
        // SAFETY: an all-zero-bytes union is a valid bit pattern for every
        // variant we read through (we always check `format` first).
        unsafe { std::mem::zeroed() }
    }
}

#[repr(C)]
pub struct mpv_node_list {
    pub num: c_int,
    pub values: *mut mpv_node,
    pub keys: *mut *mut c_char,
}

#[repr(C)]
pub struct mpv_byte_array {
    pub data: *mut c_void,
    pub size: usize,
}

// ---------------------------------------------------------------------
// Events (client.h)
// ---------------------------------------------------------------------

pub type mpv_event_id = c_int;
pub const MPV_EVENT_NONE: mpv_event_id = 0;
pub const MPV_EVENT_SHUTDOWN: mpv_event_id = 1;
pub const MPV_EVENT_LOG_MESSAGE: mpv_event_id = 2;
pub const MPV_EVENT_START_FILE: mpv_event_id = 6;
pub const MPV_EVENT_END_FILE: mpv_event_id = 7;
pub const MPV_EVENT_FILE_LOADED: mpv_event_id = 8;
pub const MPV_EVENT_PROPERTY_CHANGE: mpv_event_id = 22;

pub type mpv_end_file_reason = c_int;
pub const MPV_END_FILE_REASON_ERROR: mpv_end_file_reason = 4;

pub type mpv_log_level = c_int;
pub const MPV_LOG_LEVEL_ERROR: mpv_log_level = 20;

#[repr(C)]
pub struct mpv_event_property {
    pub name: *const c_char,
    pub format: mpv_format,
    pub data: *mut c_void,
}

#[repr(C)]
pub struct mpv_event_log_message {
    pub prefix: *const c_char,
    pub level: *const c_char,
    pub text: *const c_char,
    pub log_level: mpv_log_level,
}

#[repr(C)]
pub struct mpv_event_end_file {
    pub reason: mpv_end_file_reason,
    pub error: c_int,
    pub playlist_entry_id: i64,
    pub playlist_insert_id: i64,
    pub playlist_insert_num_entries: c_int,
}

#[repr(C)]
pub struct mpv_event {
    pub event_id: mpv_event_id,
    pub error: c_int,
    pub reply_userdata: u64,
    pub data: *mut c_void,
}

// ---------------------------------------------------------------------
// Render API (render.h)
// ---------------------------------------------------------------------

pub type mpv_render_param_type = c_int;
pub const MPV_RENDER_PARAM_INVALID: mpv_render_param_type = 0;
pub const MPV_RENDER_PARAM_API_TYPE: mpv_render_param_type = 1;
pub const MPV_RENDER_PARAM_OPENGL_INIT_PARAMS: mpv_render_param_type = 2;
pub const MPV_RENDER_PARAM_OPENGL_FBO: mpv_render_param_type = 3;
pub const MPV_RENDER_PARAM_FLIP_Y: mpv_render_param_type = 4;

/// `MPV_RENDER_API_TYPE_OPENGL` (render.h `#define`).
pub const MPV_RENDER_API_TYPE_OPENGL: &std::ffi::CStr = c"opengl";

#[repr(C)]
pub struct mpv_render_param {
    pub type_: mpv_render_param_type,
    pub data: *mut c_void,
}

/// Mirrors `mpv_opengl_init_params` (render_gl.h).
#[repr(C)]
pub struct mpv_opengl_init_params {
    pub get_proc_address:
        unsafe extern "C" fn(ctx: *mut c_void, name: *const c_char) -> *mut c_void,
    pub get_proc_address_ctx: *mut c_void,
}

/// Mirrors `mpv_opengl_fbo` (render_gl.h).
#[repr(C)]
pub struct mpv_opengl_fbo {
    pub fbo: c_int,
    pub w: c_int,
    pub h: c_int,
    pub internal_format: c_int,
}

// ---------------------------------------------------------------------
// Functions
// ---------------------------------------------------------------------

unsafe extern "C" {
    // -- client.h --
    pub fn mpv_error_string(error: c_int) -> *const c_char;
    pub fn mpv_free(data: *mut c_void);
    pub fn mpv_create() -> *mut mpv_handle;
    pub fn mpv_initialize(ctx: *mut mpv_handle) -> c_int;
    pub fn mpv_terminate_destroy(ctx: *mut mpv_handle);

    pub fn mpv_set_option_string(
        ctx: *mut mpv_handle,
        name: *const c_char,
        data: *const c_char,
    ) -> c_int;

    pub fn mpv_command(ctx: *mut mpv_handle, args: *const *const c_char) -> c_int;

    pub fn mpv_set_property(
        ctx: *mut mpv_handle,
        name: *const c_char,
        format: mpv_format,
        data: *mut c_void,
    ) -> c_int;
    pub fn mpv_set_property_string(
        ctx: *mut mpv_handle,
        name: *const c_char,
        data: *const c_char,
    ) -> c_int;
    pub fn mpv_get_property(
        ctx: *mut mpv_handle,
        name: *const c_char,
        format: mpv_format,
        data: *mut c_void,
    ) -> c_int;
    pub fn mpv_get_property_string(ctx: *mut mpv_handle, name: *const c_char) -> *mut c_char;
    /// Currently unused: `track-list` is read exclusively through the
    /// property-observation stream (see `events.rs`), whose event-owned
    /// `mpv_node`s must *not* be freed by us. Kept declared (it's the
    /// documented counterpart to `mpv_get_property(..., MPV_FORMAT_NODE,
    /// ...)`) for the day something needs a one-off manual node fetch.
    #[allow(dead_code)]
    pub fn mpv_free_node_contents(node: *mut mpv_node);

    pub fn mpv_observe_property(
        ctx: *mut mpv_handle,
        reply_userdata: u64,
        name: *const c_char,
        format: mpv_format,
    ) -> c_int;

    pub fn mpv_request_log_messages(ctx: *mut mpv_handle, min_level: *const c_char) -> c_int;

    pub fn mpv_wait_event(ctx: *mut mpv_handle, timeout: f64) -> *mut mpv_event;
    pub fn mpv_wakeup(ctx: *mut mpv_handle);

    // -- render.h --
    pub fn mpv_render_context_create(
        res: *mut *mut mpv_render_context,
        mpv: *mut mpv_handle,
        params: *mut mpv_render_param,
    ) -> c_int;
    pub fn mpv_render_context_set_update_callback(
        ctx: *mut mpv_render_context,
        callback: unsafe extern "C" fn(*mut c_void),
        callback_ctx: *mut c_void,
    );
    pub fn mpv_render_context_render(
        ctx: *mut mpv_render_context,
        params: *mut mpv_render_param,
    ) -> c_int;
    pub fn mpv_render_context_report_swap(ctx: *mut mpv_render_context);
    pub fn mpv_render_context_free(ctx: *mut mpv_render_context);
}
