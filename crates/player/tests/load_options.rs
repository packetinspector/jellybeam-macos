//! Regression coverage for `Player::load`'s per-request options: resume
//! position (`start_secs`), auth headers (`http_headers`), and external
//! subtitle sidecars (`external_subs`).
//!
//! mpv 0.38+ `loadfile` takes `<url> [<flags> [<index> [<options>]]]`, so
//! `load` inserts a `-1` index before `<options>`; these tests pin that all
//! three fields actually reach mpv through that argv shape.

#![cfg(target_os = "macos")]

mod common;

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use player::{LoadRequest, Player, PlayerEvent, TrackKind};

fn simple_load(path: String) -> LoadRequest {
    LoadRequest {
        url: path,
        http_headers: Vec::new(),
        start_secs: None,
        external_subs: Vec::new(),
        start_paused: false,
        readahead_secs: None,
        max_bytes: None,
    }
}

// ---------------------------------------------------------------------
// (a) start_secs resume
// ---------------------------------------------------------------------

#[test]
fn start_secs_resumes_near_the_requested_position() {
    let _gl = common::GlContext::new_current().expect("headless CGL context");
    let player = Player::new(common::gl_get_proc_address).expect("Player::new");
    let events = common::collect_events(&player);

    let req = LoadRequest {
        start_secs: Some(2.0),
        ..simple_load(common::media("Movies/01-h264-aac.mkv"))
    };
    player.load(req).expect("load() should be accepted");

    let first_position = common::wait_for(&events, Duration::from_secs(20), |ev| {
        if let PlayerEvent::Position { secs } = ev {
            Some(*secs)
        } else {
            None
        }
    })
    .expect("expected a Position event within 20s");

    assert!(
        first_position >= 1.9,
        "expected the first reported position to be at/near the requested \
         start_secs=2.0 resume point, got {first_position}"
    );
}

// ---------------------------------------------------------------------
// (b) http_headers reach the server
// ---------------------------------------------------------------------

/// A minimal single-purpose HTTP/1.1 file server: accepts connections on a
/// background thread for the remainder of the test process, serves the
/// exact bytes of `file_path` (honoring `Range` requests — mpv's
/// ffmpeg-based http demuxer issues them to probe/seek an mkv's Cues), and
/// records every `Authorization` header value it sees so the test can
/// assert mpv actually sent the header `LoadRequest::http_headers` was
/// supposed to attach.
fn spawn_http_file_server(file_path: String) -> (String, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind 127.0.0.1:0");
    let addr = listener.local_addr().expect("local_addr");
    let auth_seen = Arc::new(Mutex::new(Vec::new()));
    let auth_seen_bg = Arc::clone(&auth_seen);

    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let file_path = file_path.clone();
            let auth_seen = Arc::clone(&auth_seen_bg);
            std::thread::spawn(move || serve_one(stream, &file_path, &auth_seen));
        }
    });

    (format!("http://{addr}/video"), auth_seen)
}

fn serve_one(stream: TcpStream, file_path: &str, auth_seen: &Arc<Mutex<Vec<String>>>) {
    let Ok(cloned) = stream.try_clone() else {
        return;
    };
    let mut reader = BufReader::new(cloned);

    let mut request_line = String::new();
    if reader.read_line(&mut request_line).unwrap_or(0) == 0 {
        return;
    }

    let mut range: Option<(u64, Option<u64>)> = None;
    loop {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        if name.eq_ignore_ascii_case("authorization") {
            if let Ok(mut seen) = auth_seen.lock() {
                seen.push(value.to_string());
            }
        } else if name.eq_ignore_ascii_case("range") {
            range = parse_range(value);
        }
    }

    let Ok(data) = std::fs::read(file_path) else {
        return;
    };
    let total_len = data.len() as u64;

    let (header, body): (String, &[u8]) = match range {
        Some((start, end)) if start < total_len => {
            let end = end.unwrap_or(total_len - 1).min(total_len - 1);
            let start = start.min(end);
            let chunk = &data[start as usize..=end as usize];
            (
                format!(
                    "HTTP/1.1 206 Partial Content\r\n\
                     Content-Range: bytes {start}-{end}/{total_len}\r\n\
                     Content-Length: {}\r\n\
                     Accept-Ranges: bytes\r\n\
                     Content-Type: video/x-matroska\r\n\
                     Connection: close\r\n\r\n",
                    chunk.len()
                ),
                chunk,
            )
        }
        _ => (
            format!(
                "HTTP/1.1 200 OK\r\n\
                 Content-Length: {total_len}\r\n\
                 Accept-Ranges: bytes\r\n\
                 Content-Type: video/x-matroska\r\n\
                 Connection: close\r\n\r\n"
            ),
            data.as_slice(),
        ),
    };

    let mut stream = stream;
    let _ = stream.write_all(header.as_bytes());
    let _ = stream.write_all(body);
}

fn parse_range(value: &str) -> Option<(u64, Option<u64>)> {
    let spec = value.strip_prefix("bytes=")?;
    let (start, end) = spec.split_once('-')?;
    let start: u64 = start.trim().parse().ok()?;
    let end = if end.trim().is_empty() {
        None
    } else {
        end.trim().parse().ok()
    };
    Some((start, end))
}

#[test]
fn http_headers_are_sent_to_the_server() {
    let _gl = common::GlContext::new_current().expect("headless CGL context");
    let player = Player::new(common::gl_get_proc_address).expect("Player::new");
    let events = common::collect_events(&player);

    let (url, auth_seen) = spawn_http_file_server(common::media("Movies/01-h264-aac.mkv"));

    let req = LoadRequest {
        url,
        http_headers: vec![("Authorization".to_string(), "Bearer test-token".to_string())],
        start_secs: None,
        external_subs: Vec::new(),
        start_paused: false,
        readahead_secs: None,
        max_bytes: None,
    };
    player.load(req).expect("load() should be accepted");

    common::wait_for(&events, Duration::from_secs(20), |ev| {
        matches!(ev, PlayerEvent::Loaded { .. }).then_some(())
    })
    .expect("expected a Loaded event within 20s");

    let seen = auth_seen.lock().expect("auth_seen mutex");
    assert!(
        seen.iter().any(|v| v == "Bearer test-token"),
        "expected the server to receive an 'Authorization: Bearer test-token' \
         header, saw {seen:?}"
    );
}

// ---------------------------------------------------------------------
// (a.1) start_secs resume over a real (loopback) HTTP connection
//
// `start_secs_resumes_near_the_requested_position` above already covers
// this for a local file path; this is the same contract over http(s) --
// `start=+X` is frame-exact regardless of transport (a keyframe-landing
// network resume is slower; see `build_loadfile_options` and LATENCY.md),
// so this is intentionally identical in shape to the local-file test.
// ---------------------------------------------------------------------

#[test]
fn network_resume_is_frame_exact_same_as_local() {
    let _gl = common::GlContext::new_current().expect("headless CGL context");
    let player = Player::new(common::gl_get_proc_address).expect("Player::new");
    let events = common::collect_events(&player);

    let (url, _auth_seen) = spawn_http_file_server(common::media("Movies/01-h264-aac.mkv"));
    let req = LoadRequest {
        start_secs: Some(2.0),
        ..simple_load(url)
    };
    player.load(req).expect("load() should be accepted");

    let first_position = common::wait_for(&events, Duration::from_secs(20), |ev| {
        if let PlayerEvent::Position { secs } = ev {
            Some(*secs)
        } else {
            None
        }
    })
    .expect("expected a Position event within 20s");

    assert!(
        first_position >= 1.9,
        "expected the first reported position to be at/near the requested \
         start_secs=2.0 resume point over http, got {first_position}"
    );
}

// ---------------------------------------------------------------------
// (c) external_subs sidecar
// ---------------------------------------------------------------------

#[test]
fn external_subs_sidecar_adds_a_subtitle_track() {
    let _gl = common::GlContext::new_current().expect("headless CGL context");

    // Baseline: the container itself (h264 video + aac audio, per the
    // corpus filename) carries no subtitle track of its own — this is what
    // makes a subtitle track showing up after attaching `external_subs`
    // meaningful evidence that it came from the sidecar, not something
    // already embedded.
    {
        let player = Player::new(common::gl_get_proc_address).expect("Player::new");
        let events = common::collect_events(&player);
        player
            .load(simple_load(common::media(
                "Movies/28-h264-srt-sidecar-aac.mkv",
            )))
            .expect("load() should be accepted");
        let tracks = common::wait_for(&events, Duration::from_secs(20), |ev| {
            if let PlayerEvent::Loaded { tracks, .. } = ev {
                Some(tracks.clone())
            } else {
                None
            }
        })
        .expect("expected a Loaded event within 20s");
        assert!(
            !tracks.iter().any(|t| t.kind == TrackKind::Subtitle),
            "expected no subtitle track without external_subs, got {tracks:?}"
        );
    }

    // With the sidecar attached via `external_subs`, a subtitle track must
    // appear — and its codec ("subrip", what an SRT sidecar decodes to)
    // corroborates it's the external file and not something the container
    // secretly already had.
    {
        let player = Player::new(common::gl_get_proc_address).expect("Player::new");
        let events = common::collect_events(&player);
        let req = LoadRequest {
            url: common::media("Movies/28-h264-srt-sidecar-aac.mkv"),
            http_headers: Vec::new(),
            start_secs: None,
            external_subs: vec![common::media("Movies/28-h264-srt-sidecar-aac.srt")],
            start_paused: false,
            readahead_secs: None,
            max_bytes: None,
        };
        player.load(req).expect("load() should be accepted");

        // mpv attaches `sub-file` external tracks as a step alongside (not
        // strictly before) the main demuxer's own track enumeration, so the
        // subtitle track isn't guaranteed to already be present in the
        // track-list bundled with the `Loaded` event — it may only show up
        // in a `TracksChanged` shortly after. Accept either.
        let tracks = common::wait_for(&events, Duration::from_secs(20), |ev| {
            let tracks = match ev {
                PlayerEvent::Loaded { tracks, .. } => tracks,
                PlayerEvent::TracksChanged(tracks) => tracks,
                _ => return None,
            };
            tracks
                .iter()
                .any(|t| t.kind == TrackKind::Subtitle)
                .then(|| tracks.clone())
        })
        .expect(
            "expected a subtitle track to appear (via Loaded or a later \
             TracksChanged) within 20s",
        );

        let sub = tracks
            .iter()
            .find(|t| t.kind == TrackKind::Subtitle)
            .unwrap_or_else(|| {
                panic!("expected a subtitle track from the external_subs sidecar, got {tracks:?}")
            });
        assert_eq!(
            sub.codec.as_deref(),
            Some("subrip"),
            "expected the sidecar .srt to show up as a subrip-coded subtitle track, got {sub:?}"
        );
    }
}

// ---------------------------------------------------------------------
// (d) Track enumeration over http(s) still works with the network
// profile's `demuxer-lavf-probe-info=nostreams` (see this crate's
// LATENCY.md, "Load options", and `build_loadfile_options`).
//
// `nostreams` tells libavformat's demuxer to skip `avformat_find_stream_info`
// UNLESS the file appears to have no streams after just opening it (mpv
// manual: "The auto choice... tries to skip this for a few known-safe
// whitelisted formats... nostreams only calls it if the file seems to
// contain no streams after opening"). `tracks.rs`'s existing coverage for
// ASS subtitles and dual audio only exercises local file paths, which never
// go through `is_network_url`'s network-profile gate at all -- this test
// repeats both checks over a real loopback HTTP connection (same
// `spawn_http_file_server` helper as (b)/(a.1) above) specifically to prove
// `nostreams` doesn't silently drop a track that a fuller probe would have
// found for these container shapes.
// ---------------------------------------------------------------------

#[test]
fn network_track_enumeration_unaffected_by_probe_info_nostreams() {
    let _gl = common::GlContext::new_current().expect("headless CGL context");

    {
        let player = Player::new(common::gl_get_proc_address).expect("Player::new");
        let events = common::collect_events(&player);
        let (url, _auth_seen) =
            spawn_http_file_server(common::media("Movies/27-h264-ass-subs-aac.mkv"));
        player
            .load(simple_load(url))
            .expect("load() should be accepted");
        let tracks = common::wait_for(&events, Duration::from_secs(20), |ev| {
            if let PlayerEvent::Loaded { tracks, .. } = ev {
                Some(tracks.clone())
            } else {
                None
            }
        })
        .expect("expected a Loaded event within 20s");
        let sub = tracks
            .iter()
            .find(|t| t.kind == TrackKind::Subtitle)
            .unwrap_or_else(|| {
                panic!("expected an ASS subtitle track over http (nostreams), got {tracks:?}")
            });
        assert_eq!(
            sub.codec.as_deref(),
            Some("ass"),
            "expected an ASS-coded subtitle track over http (nostreams), got {sub:?}"
        );
    }

    {
        let player = Player::new(common::gl_get_proc_address).expect("Player::new");
        let events = common::collect_events(&player);
        let (url, _auth_seen) =
            spawn_http_file_server(common::media("Movies/30-h264-dual-audio.mkv"));
        player
            .load(simple_load(url))
            .expect("load() should be accepted");
        let tracks = common::wait_for(&events, Duration::from_secs(20), |ev| {
            if let PlayerEvent::Loaded { tracks, .. } = ev {
                Some(tracks.clone())
            } else {
                None
            }
        })
        .expect("expected a Loaded event within 20s");
        let audio_tracks: Vec<_> = tracks
            .iter()
            .filter(|t| t.kind == TrackKind::Audio)
            .collect();
        assert!(
            audio_tracks.len() >= 2,
            "expected at least 2 audio tracks over http (nostreams) for a dual-audio file, \
             got {audio_tracks:?}"
        );
    }
}
