//! Conversion from libmpv's tagged-union `mpv_node` tree (used for the
//! `track-list` property, among others) into `serde_json::Value`, plus the
//! `Vec<Track>` extraction on top of that. Kept separate from `sys` (raw FFI)
//! and `lib` (public API + player state machine) since it's a pure,
//! independently-testable transformation.

use std::ffi::CStr;

use serde_json::{Map, Number, Value};

use crate::sys;
use crate::{Track, TrackKind};

/// Recursion depth cap for `node_to_json`. mpv's own node-shaped properties
/// (`track-list` and friends) are always shallow — a handful of levels at
/// most — but nothing in the client API documents a bound on how deeply a
/// `mpv_node` tree could nest, and this function recurses once per level.
/// Capping recursion turns a hypothetical pathological/future-mpv-version
/// tree into a bounded `Value::Null` at the cutoff instead of a stack
/// overflow.
const MAX_NODE_DEPTH: u32 = 64;

/// Recursively converts a `*const mpv_node` into a `serde_json::Value`.
///
/// # Safety
/// `node` must point to a valid, initialized `mpv_node` (as returned by
/// `mpv_get_property(..., MPV_FORMAT_NODE, ...)` or found in the `data` field
/// of a `MPV_FORMAT_NODE` property-change event). This does not take
/// ownership or free anything — the caller decides whether/how to release
/// the source node (see call sites in `lib.rs`).
pub unsafe fn node_to_json(node: &sys::mpv_node) -> Value {
    unsafe { node_to_json_at_depth(node, 0) }
}

/// Depth-tracking implementation behind `node_to_json`; see `MAX_NODE_DEPTH`.
///
/// # Safety
/// Same contract as `node_to_json`.
unsafe fn node_to_json_at_depth(node: &sys::mpv_node, depth: u32) -> Value {
    if depth >= MAX_NODE_DEPTH {
        return Value::Null;
    }
    match node.format {
        sys::MPV_FORMAT_NONE => Value::Null,
        sys::MPV_FORMAT_STRING => {
            // SAFETY: format tag guarantees `u.string` is the active union
            // member; libmpv guarantees non-NULL for MPV_FORMAT_STRING.
            let s = unsafe { node.u.string };
            if s.is_null() {
                Value::Null
            } else {
                let cs = unsafe { CStr::from_ptr(s) };
                Value::String(cs.to_string_lossy().into_owned())
            }
        }
        sys::MPV_FORMAT_FLAG => {
            let flag = unsafe { node.u.flag };
            Value::Bool(flag != 0)
        }
        sys::MPV_FORMAT_INT64 => {
            let v = unsafe { node.u.int64 };
            Value::Number(Number::from(v))
        }
        sys::MPV_FORMAT_DOUBLE => {
            let v = unsafe { node.u.double_ };
            Number::from_f64(v)
                .map(Value::Number)
                .unwrap_or(Value::Null)
        }
        sys::MPV_FORMAT_NODE_ARRAY => {
            let list_ptr = unsafe { node.u.list };
            if list_ptr.is_null() {
                return Value::Array(Vec::new());
            }
            let list = unsafe { &*list_ptr };
            let len = list.num.max(0) as usize;
            let mut out = Vec::with_capacity(len);
            for i in 0..len {
                if list.values.is_null() {
                    break;
                }
                let child = unsafe { &*list.values.add(i) };
                out.push(unsafe { node_to_json_at_depth(child, depth + 1) });
            }
            Value::Array(out)
        }
        sys::MPV_FORMAT_NODE_MAP => {
            let list_ptr = unsafe { node.u.list };
            if list_ptr.is_null() {
                return Value::Object(Map::new());
            }
            let list = unsafe { &*list_ptr };
            let len = list.num.max(0) as usize;
            let mut out = Map::with_capacity(len);
            for i in 0..len {
                if list.values.is_null() || list.keys.is_null() {
                    break;
                }
                let key_ptr = unsafe { *list.keys.add(i) };
                let key = if key_ptr.is_null() {
                    String::new()
                } else {
                    unsafe { CStr::from_ptr(key_ptr) }
                        .to_string_lossy()
                        .into_owned()
                };
                let child = unsafe { &*list.values.add(i) };
                out.insert(key, unsafe { node_to_json_at_depth(child, depth + 1) });
            }
            Value::Object(out)
        }
        // MPV_FORMAT_BYTE_ARRAY and anything unknown: not needed for the
        // properties this crate observes; represent as null rather than
        // guessing at a layout.
        _ => Value::Null,
    }
}

/// Extracts `Track`s from a `track-list` property value already converted to
/// JSON (mpv's node-array-of-node-maps shape, one object per track — see the
/// "track-list" entry in the mpv manual's property list).
pub fn json_to_tracks(value: &Value) -> Vec<Track> {
    let Some(array) = value.as_array() else {
        return Vec::new();
    };
    let mut out = Vec::with_capacity(array.len());
    for item in array {
        let kind = match item.get("type").and_then(Value::as_str) {
            Some("video") => TrackKind::Video,
            Some("audio") => TrackKind::Audio,
            Some("sub") => TrackKind::Subtitle,
            // mpv also reports "unknown"/attachment-derived pseudo-tracks;
            // Track only models the three kinds the public API names.
            _ => continue,
        };
        let mpv_id = item.get("id").and_then(Value::as_i64).unwrap_or(-1);
        let title = item.get("title").and_then(Value::as_str).map(str::to_owned);
        let lang = item.get("lang").and_then(Value::as_str).map(str::to_owned);
        let codec = item.get("codec").and_then(Value::as_str).map(str::to_owned);
        let default = item
            .get("default")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let selected = item
            .get("selected")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        // See `Track::forced`'s doc comment -- mpv only ever sets this on
        // subtitle entries, but reading it unconditionally (rather than
        // gating on `kind == TrackKind::Subtitle`) costs nothing and needs
        // no extra branch: a video/audio entry simply never has the key.
        let forced = item.get("forced").and_then(Value::as_bool).unwrap_or(false);
        out.push(Track {
            mpv_id,
            kind,
            title,
            lang,
            codec,
            default,
            selected,
            forced,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_to_tracks_filters_and_maps_fields() {
        let value = serde_json::json!([
            {
                "id": 1, "type": "video", "codec": "hevc", "default": true,
                "selected": true
            },
            {
                "id": 2, "type": "audio", "lang": "eng", "codec": "aac",
                "title": "Commentary", "selected": false
            },
            {
                "id": 3, "type": "sub", "lang": "eng", "codec": "ass"
            },
            {
                "id": 4, "type": "attachment"
            },
            {
                "id": 5, "type": "sub", "lang": "jpn", "codec": "ass",
                "forced": true
            }
        ]);

        let tracks = json_to_tracks(&value);
        assert_eq!(
            tracks.len(),
            4,
            "attachment pseudo-track must be filtered out"
        );

        assert_eq!(tracks[0].kind, TrackKind::Video);
        assert_eq!(tracks[0].mpv_id, 1);
        assert!(tracks[0].default);
        assert!(tracks[0].selected);
        assert_eq!(tracks[0].codec.as_deref(), Some("hevc"));

        assert_eq!(tracks[1].kind, TrackKind::Audio);
        assert_eq!(tracks[1].lang.as_deref(), Some("eng"));
        assert_eq!(tracks[1].title.as_deref(), Some("Commentary"));
        assert!(!tracks[1].selected);

        assert_eq!(tracks[2].kind, TrackKind::Subtitle);
        assert_eq!(tracks[2].codec.as_deref(), Some("ass"));
        assert!(!tracks[2].default);
        assert!(!tracks[2].forced, "no \"forced\" key defaults to false");

        assert_eq!(tracks[3].kind, TrackKind::Subtitle);
        assert_eq!(tracks[3].lang.as_deref(), Some("jpn"));
        assert!(tracks[3].forced);
    }

    #[test]
    fn json_to_tracks_on_non_array_is_empty() {
        assert!(json_to_tracks(&Value::Null).is_empty());
    }

    /// Builds a chain of `depth` nested single-element `MPV_FORMAT_NODE_ARRAY`
    /// nodes, terminated by an `MPV_FORMAT_NONE` leaf. Leaks the boxed
    /// `mpv_node`/`mpv_node_list` allocations (fine for a test).
    fn build_nested_array(depth: u32) -> sys::mpv_node {
        if depth == 0 {
            return sys::mpv_node::default();
        }
        let child = build_nested_array(depth - 1);
        let values: *mut sys::mpv_node = Box::into_raw(Box::new(child));
        let list = Box::into_raw(Box::new(sys::mpv_node_list {
            num: 1,
            values,
            keys: std::ptr::null_mut(),
        }));
        sys::mpv_node {
            u: sys::mpv_node_u { list },
            format: sys::MPV_FORMAT_NODE_ARRAY,
        }
    }

    #[test]
    fn node_to_json_caps_recursion_depth() {
        // Deeper than MAX_NODE_DEPTH (64); if the cap weren't applied this
        // would still succeed (100 is nowhere near a real stack overflow),
        // but it lets us assert the cutoff actually happens at 64 rather
        // than recursing all the way down.
        let node = build_nested_array(100);
        let value = unsafe { node_to_json(&node) };

        let mut cur = &value;
        let mut depth = 0u32;
        while let Some(arr) = cur.as_array() {
            let Some(first) = arr.first() else { break };
            cur = first;
            depth += 1;
        }

        assert_eq!(
            depth, MAX_NODE_DEPTH,
            "expected exactly {MAX_NODE_DEPTH} levels of nested arrays before hitting the cap"
        );
        assert_eq!(
            *cur,
            Value::Null,
            "node past the depth cap must be represented as Null"
        );
    }
}
