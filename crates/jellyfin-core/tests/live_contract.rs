//! Per-corpus-file contract tests against a real, running Jellyfin server
//! (not a fixture/mock): the device profile
//! ([`jellyfin_core::build_device_profile`]) is the correctness-critical
//! artifact, so every corpus file asserts its expected playback decision.
//!
//! Every test here is `#[ignore]`-gated because it requires a live server —
//! CI / `cargo test` runs skip these by default. To run them for real:
//!
//! ```text
//! cargo test -p jellyfin-core --test live_contract -- --ignored --nocapture
//! ```
//!
//! Requires:
//! - A Jellyfin server reachable at `http://localhost:8096` by default;
//!   override with `JELLYBEAM_DEV_SERVER_URL` to target another dev server
//!   instance (e.g. the opt-in Jellyfin 12.0 server on :8097 -- see
//!   dev/README.md).
//! - The `jellybeam-admin` / `jellybeam-test` account.
//! - The `dev/media` corpus ingested as a library (specifically the four
//!   named `Movies/*` files below — see each test's doc comment).
//!
//! Each test authenticates fresh (tokens observed to expire quickly against
//! the dev server — reusing one across a slow test run intermittently 401s),
//! resolves its target corpus file's item id by matching the real filename
//! (see [`find_item_id_by_filename`] for why `Name` alone isn't reliable),
//! builds Jellybeam's real [`jellyfin_core::build_device_profile`], POSTs
//! `PlaybackInfo` against the live server, and asserts the server's actual
//! decision — not a recorded fixture, the real thing, so this catches drift
//! between docs/OVERVIEW.md §2's direct-play matrix and what the server actually
//! decides for each codec/container/subtitle combination.

use jellyfin_api::models::{
    DeviceProfile, DirectPlayProfile, DlnaProfileType, EncodingContext, MediaStreamProtocol,
    MediaStreamType, SubtitleDeliveryMethod, TranscodeSeekInfo, TranscodingProfile,
};
use jellyfin_api::{ClientIdentity, ItemQuery, JellyfinClient};
use jellyfin_core::PlaybackDecision;

fn base_url() -> String {
    std::env::var("JELLYBEAM_DEV_SERVER_URL")
        .unwrap_or_else(|_| "http://localhost:8096".to_string())
}
const USERNAME: &str = "jellybeam-admin";
const PASSWORD: &str = "jellybeam-test";

fn identity() -> ClientIdentity {
    ClientIdentity {
        client: "Jellybeam-LiveContractTest".to_string(),
        device: "live-contract-test".to_string(),
        device_id: "jellybeam-w21-live-contract-test-device".to_string(),
        version: "0.1.0".to_string(),
    }
}

/// Authenticate fresh against the live dev server. Panics with a message
/// pointing at the likely cause (server not running / corpus not seeded)
/// rather than an opaque `ApiError` if it fails — these tests are meant to
/// be run interactively by someone who can act on that.
async fn authenticated_client() -> JellyfinClient {
    let base_url = base_url();
    let (client, _auth) =
        JellyfinClient::authenticate_by_name(&base_url, identity(), USERNAME, PASSWORD)
            .await
            .unwrap_or_else(|err| {
                panic!(
                    "failed to authenticate against {base_url} as '{USERNAME}': {err:?}. \
                     Is the dev Jellyfin server running with the jellybeam-admin/jellybeam-test \
                     account seeded?"
                )
            });
    client
}

/// Resolve a corpus file's item id by matching the basename of its `Path`
/// against `filename`. `Name` alone isn't reliable here — Jellyfin's own
/// metadata parsing shortens/rewrites titles for some of the corpus's
/// codec-tag filenames (e.g. `07-hevc8-ac3.mkv` shows up with `Name:
/// "07-hevc8"`), so this lists every Movie recursively with `Fields=Path`
/// and matches on the real filename instead.
async fn find_item_id_by_filename(client: &JellyfinClient, filename: &str) -> String {
    let result = client
        .get_items(&ItemQuery {
            parent_id: None,
            include_item_types: vec!["Movie".to_string()],
            recursive: true,
            sort_by: None,
            sort_order: None,
            fields: vec!["Path".to_string()],
            start_index: 0,
            limit: 500,
            ids: Vec::new(),
            is_missing: None,
            min_date_last_saved: None,
            ..ItemQuery::new()
        })
        .await
        .expect("list items from the live server (GET /Items)");

    let found = result.items.into_iter().find(|item| {
        item.path
            .as_deref()
            .and_then(|p| p.rsplit('/').next())
            .is_some_and(|basename| basename == filename)
    });

    let item = found.unwrap_or_else(|| {
        panic!(
            "corpus file '{filename}' not found in the live server's library -- \
             is dev/media ingested as a library on this server?"
        )
    });

    item.id
        .unwrap_or_else(|| panic!("item for '{filename}' has no Id"))
        .to_string()
}

/// A deliberately narrow negative-control [`DeviceProfile`]: only accepts a
/// codec/container combination (`wmv`/`wmv3`/`wmav2`) that none of the
/// corpus files use, so the server MUST refuse direct play/direct stream
/// for anything in the corpus and fall back to its `TranscodingProfile`.
fn wmv_only_negative_control_profile() -> DeviceProfile {
    DeviceProfile {
        name: Some("jellybeam-negative-control-wmv-only".to_string()),
        direct_play_profiles: vec![DirectPlayProfile {
            container: Some("wmv".to_string()),
            audio_codec: Some("wmav2".to_string()),
            video_codec: Some("wmv3".to_string()),
            type_: Some(DlnaProfileType::Video),
        }],
        transcoding_profiles: vec![TranscodingProfile {
            container: Some("ts".to_string()),
            type_: Some(DlnaProfileType::Video),
            video_codec: Some("h264,hevc".to_string()),
            audio_codec: Some("aac,ac3".to_string()),
            protocol: Some(MediaStreamProtocol::Hls),
            estimate_content_length: false,
            enable_mpegts_m2_ts_mode: true,
            transcode_seek_info: Some(TranscodeSeekInfo::Auto),
            copy_timestamps: false,
            context: Some(EncodingContext::Streaming),
            enable_subtitles_in_manifest: true,
            max_audio_channels: Some("6".to_string()),
            min_segments: 1,
            segment_length: 6,
            break_on_non_key_frames: true,
            conditions: Vec::new(),
            enable_audio_vbr_encoding: true,
        }],
        ..Default::default()
    }
}

/// `26-hevc10-hdr10-ac3.mkv`: HEVC 10-bit + HDR10, AC3 audio, mkv container
/// — every one of those is in Jellybeam's real direct-play matrix
/// (`device_profile::DIRECT_PLAY_VIDEO_CODECS`/`_AUDIO_CODECS`/
/// `_CONTAINERS`). The server must agree: `SupportsDirectPlay == true`.
#[ignore = "requires a live Jellyfin server at localhost:8096 with the dev corpus"]
#[tokio::test]
async fn hevc10_hdr10_ac3_mkv_supports_direct_play() {
    let client = authenticated_client().await;
    let item_id = find_item_id_by_filename(&client, "26-hevc10-hdr10-ac3.mkv").await;
    let profile = jellyfin_core::build_device_profile(None);

    let info = client
        .get_playback_info(&item_id, &profile, None)
        .await
        .expect("PlaybackInfo request to the live server");

    assert!(
        info.media_sources
            .iter()
            .any(|s| s.supports_direct_play == Some(true)),
        "expected SupportsDirectPlay == true for 26-hevc10-hdr10-ac3.mkv, got: {:#?}",
        info.media_sources
    );
}

/// `16-av1-aac.mkv`: AV1 video + AAC audio in mkv — both in the direct-play
/// matrix. Asserted via [`jellyfin_core::decide_playback`] (the real public
/// decision function, not just the raw flag) so this also exercises the
/// full decision path end to end.
#[ignore = "requires a live Jellyfin server at localhost:8096 with the dev corpus"]
#[tokio::test]
async fn av1_aac_mkv_direct_plays() {
    let client = authenticated_client().await;
    let item_id = find_item_id_by_filename(&client, "16-av1-aac.mkv").await;
    let profile = jellyfin_core::build_device_profile(None);

    let info = client
        .get_playback_info(&item_id, &profile, None)
        .await
        .expect("PlaybackInfo request to the live server");

    match jellyfin_core::decide_playback(&client, &item_id, &info) {
        Ok(PlaybackDecision::DirectPlay { .. }) => {}
        other => panic!("expected DirectPlay for 16-av1-aac.mkv, got {other:?}"),
    }
}

/// `03-h264-eac3.ts`: H264 + E-AC3 in an MPEG-TS container — the "classic"
/// broadcast-style corpus file, and the one whose real server response
/// (captured while writing this test, against a live dev server) confirmed
/// the negative-control profile's expected shape below.
#[ignore = "requires a live Jellyfin server at localhost:8096 with the dev corpus"]
#[tokio::test]
async fn h264_eac3_ts_direct_plays() {
    let client = authenticated_client().await;
    let item_id = find_item_id_by_filename(&client, "03-h264-eac3.ts").await;
    let profile = jellyfin_core::build_device_profile(None);

    let info = client
        .get_playback_info(&item_id, &profile, None)
        .await
        .expect("PlaybackInfo request to the live server");

    match jellyfin_core::decide_playback(&client, &item_id, &info) {
        Ok(PlaybackDecision::DirectPlay { .. }) => {}
        other => panic!("expected DirectPlay for 03-h264-eac3.ts, got {other:?}"),
    }
}

/// `27-h264-ass-subs-aac.mkv`: H264 + AAC direct-plays, AND its embedded
/// `.ass` subtitle stream must be deliverable without transcoding (Embed or
/// External `SubtitleDeliveryMethod` — never `Encode`, which means the
/// server would burn the subtitles into a transcoded video stream, or
/// `Drop`, which means they're not delivered at all). This is the one test
/// that actually exercises the B1 fix's `EMBEDDED_SUBTITLE_FORMATS`
/// declaration end to end against a real server decision, not just the
/// profile's own JSON shape.
#[ignore = "requires a live Jellyfin server at localhost:8096 with the dev corpus"]
#[tokio::test]
async fn h264_ass_subs_aac_mkv_direct_plays_with_deliverable_subtitles() {
    let client = authenticated_client().await;
    let item_id = find_item_id_by_filename(&client, "27-h264-ass-subs-aac.mkv").await;
    let profile = jellyfin_core::build_device_profile(None);

    let info = client
        .get_playback_info(&item_id, &profile, None)
        .await
        .expect("PlaybackInfo request to the live server");

    let source = match jellyfin_core::decide_playback(&client, &item_id, &info) {
        Ok(PlaybackDecision::DirectPlay { source, .. }) => source,
        other => panic!("expected DirectPlay for 27-h264-ass-subs-aac.mkv, got {other:?}"),
    };

    let ass_stream = source
        .media_streams
        .iter()
        .find(|s| s.type_ == Some(MediaStreamType::Subtitle) && s.codec.as_deref() == Some("ass"))
        .unwrap_or_else(|| {
            panic!(
                "expected an 'ass' subtitle MediaStream on 27-h264-ass-subs-aac.mkv, \
                 got streams: {:#?}",
                source.media_streams
            )
        });

    match ass_stream.delivery_method {
        Some(SubtitleDeliveryMethod::Embed) | Some(SubtitleDeliveryMethod::External) => {}
        other => panic!(
            "expected the ass subtitle stream to be deliverable without transcoding \
             (Embed or External), got DeliveryMethod: {other:?}"
        ),
    }
}

/// Negative control: a deliberately narrow profile (`wmv`/`wmv3`/`wmav2`
/// only — nothing in the corpus matches) must force the server to refuse
/// direct play/stream and fall back to transcoding, with a non-empty set of
/// reasons explaining why (via [`jellyfin_core::decide_playback`]'s
/// synthesized `TranscodeDecision::Transcode.reasons` — see
/// `playback.rs::synthesize_reasons`'s module docs for why those are
/// synthesized client-side rather than read off a `TranscodeReasons` field:
/// the pinned OpenAPI spec doesn't expose one on `MediaSourceInfo`, only on
/// `SessionInfoDto.TranscodingInfo`).
///
/// Proves the corpus-file tests above are actually discriminating (the
/// server can and does say "no" to a profile that deserves a "no"), not
/// just trivially true because the server accepts anything.
#[ignore = "requires a live Jellyfin server at localhost:8096 with the dev corpus"]
#[tokio::test]
async fn wmv_only_negative_control_forces_transcode_with_reasons() {
    let client = authenticated_client().await;
    // Any corpus file works here since none of them are wmv/wmv3/wmav2;
    // 03-h264-eac3.ts is small and simple.
    let item_id = find_item_id_by_filename(&client, "03-h264-eac3.ts").await;
    let profile = wmv_only_negative_control_profile();

    let info = client
        .get_playback_info(&item_id, &profile, None)
        .await
        .expect("PlaybackInfo request to the live server");

    assert!(
        info.media_sources
            .iter()
            .all(|s| s.supports_direct_play != Some(true)),
        "expected SupportsDirectPlay == false under the wmv-only negative-control \
         profile, got: {:#?}",
        info.media_sources
    );

    match jellyfin_core::decide_playback(&client, &item_id, &info) {
        Ok(PlaybackDecision::Transcode { reasons, .. }) => {
            assert!(
                !reasons.is_empty(),
                "expected non-empty TranscodeReasons under the negative-control profile"
            );
        }
        other => panic!(
            "expected Transcode (with reasons) under the wmv-only negative-control \
             profile, got {other:?}"
        ),
    }
}
