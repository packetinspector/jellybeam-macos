//! Real, richly-typed model of Jellyfin's `DeviceProfile` JSON contract.
//!
//! The capability logic (docs/OVERVIEW.md §2's direct-play matrix) lives
//! here in a local model rather than directly on
//! `jellyfin_api::models::DeviceProfile`, so it can be built and tested
//! independently of the generated type. Field names are taken verbatim
//! from the pinned OpenAPI spec
//! (`crates/jellyfin-api/codegen/jellyfin-openapi-stable.json`, server
//! v12.0.0); they match
//! the real typify-generated `DeviceProfile`'s PascalCase JSON field names,
//! confirmed by inspection of `jellyfin-api/src/models.rs`.
//!
//! [`build_device_profile`] in `lib.rs` builds a [`RawDeviceProfile`] here,
//! then round-trips it through `serde_json::Value` into
//! `jellyfin_api::models::DeviceProfile`. Because both structs serialize to
//! identical JSON key names, this round trip now populates the real struct
//! correctly (see `lib.rs::tests` and `jellyfin-api`'s own model). The tests
//! below assert against [`RawDeviceProfile`] directly (via its own JSON
//! shape) so profile correctness is verified independent of that
//! serialization step.

use serde::{Deserialize, Serialize};

/// Containers mpv/ffmpeg direct-plays without remuxing (docs/OVERVIEW.md §2).
pub(crate) const DIRECT_PLAY_CONTAINERS: &[&str] = &[
    "mkv", "mp4", "m4v", "mov", "avi", "ts", "m2ts", "webm", "flv", "ogv",
];

/// Video codecs VideoToolbox (hardware) or dav1d/ffmpeg (software, AV1 on
/// M1/M2) decode natively on Apple Silicon. ProRes is hardware-decoded on
/// Apple Silicon too (docs/OVERVIEW.md §2) — mpv/VideoToolbox handles it directly,
/// no transcode needed.
pub(crate) const DIRECT_PLAY_VIDEO_CODECS: &[&str] = &[
    "h264",
    "hevc",
    "av1",
    "vp9",
    "vp8",
    "mpeg2video",
    "mpeg4",
    "vc1",
    "prores",
];

/// Audio codecs ffmpeg decodes in software (trivial CPU cost) with
/// multichannel output via CoreAudio. Both "dts" and "dca" are declared:
/// they're the same codec (DTS Coherent Acoustics) under two names, and
/// some server paths report one or the other.
pub(crate) const DIRECT_PLAY_AUDIO_CODECS: &[&str] = &[
    "aac",
    "ac3",
    "eac3",
    "dts",
    "dca",
    "truehd",
    "flac",
    "opus",
    "vorbis",
    "mp3",
    "mp2",
    "pcm_s16le",
    "pcm_s24le",
    "pcm_u8",
];

/// Subtitle formats libass/mpv's bitmap pipeline render GPU-composited while
/// staying embedded in the source container (no server work needed).
pub(crate) const EMBEDDED_SUBTITLE_FORMATS: &[&str] =
    &["ass", "ssa", "srt", "subrip", "pgssub", "dvdsub", "vtt"];

/// Subtitle formats we accept as server-served sidecar files (`sub-file` in
/// mpv) when they can't ride along embedded in the direct-played container.
pub(crate) const EXTERNAL_SUBTITLE_FORMATS: &[&str] = &["srt", "ass", "ssa", "vtt"];

/// HLS/TS video codecs the transcoder falls back to (VideoToolbox can
/// hardware-encode both on Apple Silicon).
const TRANSCODE_VIDEO_CODECS: &str = "h264,hevc";
/// HLS/TS audio codecs the transcoder falls back to.
const TRANSCODE_AUDIO_CODECS: &str = "aac,ac3";
/// 5.1 baseline for the transcoded fallback stream; direct play (the common
/// path) is unaffected and passes through the source's real channel layout.
const TRANSCODE_MAX_AUDIO_CHANNELS: &str = "6";
/// Jellyfin's conventional HLS segment length (seconds).
const HLS_SEGMENT_LENGTH_SECS: i32 = 6;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum DlnaProfileType {
    Audio,
    Video,
    Photo,
    Subtitle,
    Lyric,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum MediaStreamProtocol {
    #[serde(rename = "http")]
    Http,
    #[serde(rename = "hls")]
    Hls,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum TranscodeSeekInfo {
    Auto,
    Bytes,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum EncodingContext {
    Streaming,
    Static,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum CodecType {
    Video,
    VideoAudio,
    Audio,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum SubtitleDeliveryMethod {
    Encode,
    Embed,
    External,
    Hls,
    Drop,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum ProfileConditionType {
    Equals,
    NotEquals,
    LessThanEqual,
    GreaterThanEqual,
    EqualsAny,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum ProfileConditionValue {
    AudioChannels,
    AudioBitrate,
    AudioProfile,
    Width,
    Height,
    Has64BitOffsets,
    PacketLength,
    VideoBitDepth,
    VideoBitrate,
    VideoFramerate,
    VideoLevel,
    VideoProfile,
    VideoTimestamp,
    IsAnamorphic,
    RefFrames,
    NumAudioStreams,
    NumVideoStreams,
    IsSecondaryAudio,
    VideoCodecTag,
    IsAvc,
    IsInterlaced,
    AudioSampleRate,
    AudioBitDepth,
    VideoRangeType,
    NumStreams,
    VideoRotation,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub(crate) struct ProfileCondition {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) condition: Option<ProfileConditionType>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) property: Option<ProfileConditionValue>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) value: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) is_required: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub(crate) struct DirectPlayProfile {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) container: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) audio_codec: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) video_codec: Option<String>,
    #[serde(rename = "Type", skip_serializing_if = "Option::is_none")]
    pub(crate) kind: Option<DlnaProfileType>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub(crate) struct TranscodingProfile {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) container: Option<String>,
    #[serde(rename = "Type", skip_serializing_if = "Option::is_none")]
    pub(crate) kind: Option<DlnaProfileType>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) video_codec: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) audio_codec: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) protocol: Option<MediaStreamProtocol>,
    pub(crate) estimate_content_length: bool,
    pub(crate) enable_mpegts_m2_ts_mode: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) transcode_seek_info: Option<TranscodeSeekInfo>,
    pub(crate) copy_timestamps: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) context: Option<EncodingContext>,
    pub(crate) enable_subtitles_in_manifest: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) max_audio_channels: Option<String>,
    pub(crate) min_segments: i32,
    pub(crate) segment_length: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) break_on_non_key_frames: Option<bool>,
    #[serde(default)]
    pub(crate) conditions: Vec<ProfileCondition>,
    pub(crate) enable_audio_vbr_encoding: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub(crate) struct ContainerProfile {
    #[serde(rename = "Type", skip_serializing_if = "Option::is_none")]
    pub(crate) kind: Option<DlnaProfileType>,
    #[serde(default)]
    pub(crate) conditions: Vec<ProfileCondition>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) container: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) sub_container: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub(crate) struct CodecProfile {
    #[serde(rename = "Type", skip_serializing_if = "Option::is_none")]
    pub(crate) kind: Option<CodecType>,
    #[serde(default)]
    pub(crate) conditions: Vec<ProfileCondition>,
    #[serde(default)]
    pub(crate) apply_conditions: Vec<ProfileCondition>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) codec: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) container: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) sub_container: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub(crate) struct SubtitleProfile {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) format: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) method: Option<SubtitleDeliveryMethod>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) didl_mode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) language: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) container: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "PascalCase", default)]
pub(crate) struct RawDeviceProfile {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) max_streaming_bitrate: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) max_static_bitrate: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) music_streaming_transcoding_bitrate: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) max_static_music_bitrate: Option<i32>,
    pub(crate) direct_play_profiles: Vec<DirectPlayProfile>,
    pub(crate) transcoding_profiles: Vec<TranscodingProfile>,
    pub(crate) container_profiles: Vec<ContainerProfile>,
    pub(crate) codec_profiles: Vec<CodecProfile>,
    pub(crate) subtitle_profiles: Vec<SubtitleProfile>,
}

/// "No cap" must be an explicit huge number, never an omitted field: a
/// `DeviceProfile` leaving `MaxStreamingBitrate` out makes Jellyfin fall
/// back to a server-side default cap (~8 Mbps observed), silently denying
/// direct play/stream for a legitimately-direct-playable source and
/// violating the "Direct Play by default" product rule. 1 Gbps is far
/// above any real file's bitrate, so it never constrains a source.
pub(crate) const UNCAPPED_STREAMING_BITRATE: u32 = 1_000_000_000;

/// Build Jellybeam's real device profile. `max_streaming_bitrate` is `Some` only
/// for bandwidth-constrained remote streaming (docs/OVERVIEW.md §2/§6); when `None`,
/// mpv is trusted to direct-play everything in the matrix and the wire
/// carries [`UNCAPPED_STREAMING_BITRATE`] (see its doc comment for why
/// omitting the field is NOT equivalent).
pub(crate) fn build(max_streaming_bitrate: Option<u32>) -> RawDeviceProfile {
    RawDeviceProfile {
        name: Some("Jellybeam".to_string()),
        // Server caps bitrate for content it *streams* to us; direct play is
        // a raw byte copy mpv reads at link speed, hence "uncapped" -- but
        // stated explicitly, never omitted.
        max_streaming_bitrate: Some(clamp_to_i32(
            max_streaming_bitrate.unwrap_or(UNCAPPED_STREAMING_BITRATE),
        )),
        // No artificial static (= direct play) bitrate ceiling: mpv/ffmpeg
        // decode is hardware-accelerated regardless of source bitrate.
        max_static_bitrate: None,
        // Music playback is deferred to v2 (docs/OVERVIEW.md §6) — no music profile.
        music_streaming_transcoding_bitrate: None,
        max_static_music_bitrate: None,
        direct_play_profiles: vec![direct_play_profile()],
        transcoding_profiles: vec![transcoding_profile()],
        // No extra container-level gate: DirectPlayProfile above is the only
        // real container constraint we have.
        container_profiles: Vec::new(),
        codec_profiles: codec_profiles(max_streaming_bitrate),
        subtitle_profiles: subtitle_profiles(),
    }
}

fn clamp_to_i32(bitrate: u32) -> i32 {
    bitrate.min(i32::MAX as u32) as i32
}

fn direct_play_profile() -> DirectPlayProfile {
    DirectPlayProfile {
        container: Some(DIRECT_PLAY_CONTAINERS.join(",")),
        audio_codec: Some(DIRECT_PLAY_AUDIO_CODECS.join(",")),
        video_codec: Some(DIRECT_PLAY_VIDEO_CODECS.join(",")),
        kind: Some(DlnaProfileType::Video),
    }
}

fn transcoding_profile() -> TranscodingProfile {
    TranscodingProfile {
        container: Some("ts".to_string()),
        kind: Some(DlnaProfileType::Video),
        video_codec: Some(TRANSCODE_VIDEO_CODECS.to_string()),
        audio_codec: Some(TRANSCODE_AUDIO_CODECS.to_string()),
        protocol: Some(MediaStreamProtocol::Hls),
        estimate_content_length: false,
        // Classic Apple HLS mpegts segments — matches Container: "ts".
        enable_mpegts_m2_ts_mode: true,
        transcode_seek_info: Some(TranscodeSeekInfo::Auto),
        copy_timestamps: false,
        context: Some(EncodingContext::Streaming),
        // WebVTT sidecar-in-manifest for HLS instead of burning subs in.
        enable_subtitles_in_manifest: true,
        max_audio_channels: Some(TRANSCODE_MAX_AUDIO_CHANNELS.to_string()),
        min_segments: 1,
        segment_length: HLS_SEGMENT_LENGTH_SECS,
        break_on_non_key_frames: Some(true),
        conditions: Vec::new(),
        enable_audio_vbr_encoding: true,
    }
}

/// Only a real constraint gets a CodecProfile: no interlaced-only limits, no
/// fabricated profile/level ceilings mpv doesn't actually have. The single
/// exception is bitrate, and only when the caller told us we're
/// bandwidth-constrained (`max_streaming_bitrate: Some`) — otherwise this is
/// empty, matching "declare near-everything" from docs/OVERVIEW.md §2.
fn codec_profiles(max_streaming_bitrate: Option<u32>) -> Vec<CodecProfile> {
    match max_streaming_bitrate {
        Some(bitrate) => vec![CodecProfile {
            kind: Some(CodecType::Video),
            codec: None,     // applies to all video codecs
            container: None, // applies to all containers
            sub_container: None,
            apply_conditions: Vec::new(),
            conditions: vec![ProfileCondition {
                condition: Some(ProfileConditionType::LessThanEqual),
                property: Some(ProfileConditionValue::VideoBitrate),
                value: Some(bitrate.to_string()),
                is_required: Some(true),
            }],
        }],
        None => Vec::new(),
    }
}

fn subtitle_profiles() -> Vec<SubtitleProfile> {
    EMBEDDED_SUBTITLE_FORMATS
        .iter()
        .map(|format| subtitle_profile(format, SubtitleDeliveryMethod::Embed))
        .chain(
            EXTERNAL_SUBTITLE_FORMATS
                .iter()
                .map(|format| subtitle_profile(format, SubtitleDeliveryMethod::External)),
        )
        .collect()
}

fn subtitle_profile(format: &str, method: SubtitleDeliveryMethod) -> SubtitleProfile {
    SubtitleProfile {
        format: Some(format.to_string()),
        method: Some(method),
        didl_mode: None,
        language: None,
        container: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn json(bitrate: Option<u32>) -> serde_json::Value {
        serde_json::to_value(build(bitrate)).expect("profile always serializes")
    }

    #[test]
    fn declares_one_direct_play_profile_covering_the_matrix() {
        let v = json(None);
        let dpp = &v["DirectPlayProfiles"][0];
        assert_eq!(dpp["Type"], "Video");
        for c in DIRECT_PLAY_CONTAINERS {
            assert!(
                dpp["Container"]
                    .as_str()
                    .expect("test assertion")
                    .split(',')
                    .any(|x| x == *c),
                "missing container {c}"
            );
        }
        for c in DIRECT_PLAY_VIDEO_CODECS {
            assert!(
                dpp["VideoCodec"]
                    .as_str()
                    .expect("test assertion")
                    .split(',')
                    .any(|x| x == *c),
                "missing video codec {c}"
            );
        }
        for c in DIRECT_PLAY_AUDIO_CODECS {
            assert!(
                dpp["AudioCodec"]
                    .as_str()
                    .expect("test assertion")
                    .split(',')
                    .any(|x| x == *c),
                "missing audio codec {c}"
            );
        }
    }

    #[test]
    fn direct_play_never_declares_interlaced_or_bitrate_limits_when_unbounded() {
        let v = json(None);
        assert_eq!(
            v["CodecProfiles"].as_array().expect("test assertion").len(),
            0
        );
        assert!(v["MaxStaticBitrate"].is_null());
    }

    /// An OMITTED `MaxStreamingBitrate` is
    /// not "no cap" -- Jellyfin substitutes a server-side default (~8 Mbps)
    /// and silently denies direct play/stream to anything above it. The
    /// uncapped profile must carry the explicit huge value on the wire.
    #[test]
    fn uncapped_profile_sends_an_explicit_unlimited_streaming_bitrate() {
        let v = json(None);
        assert_eq!(
            v["MaxStreamingBitrate"]
                .as_i64()
                .expect("must be present, never omitted"),
            i64::from(UNCAPPED_STREAMING_BITRATE as i32),
        );
        // A real cap still passes through verbatim.
        let capped = json(Some(8_000_000));
        assert_eq!(capped["MaxStreamingBitrate"].as_i64(), Some(8_000_000));
    }

    #[test]
    fn bitrate_cap_only_applied_when_bandwidth_constrained() {
        let v = json(Some(8_000_000));
        assert_eq!(v["MaxStreamingBitrate"], 8_000_000);
        // Static (direct play) bitrate stays uncapped even when streaming is.
        assert!(v["MaxStaticBitrate"].is_null());
        let cp = &v["CodecProfiles"][0];
        assert_eq!(cp["Type"], "Video");
        let cond = &cp["Conditions"][0];
        assert_eq!(cond["Condition"], "LessThanEqual");
        assert_eq!(cond["Property"], "VideoBitrate");
        assert_eq!(cond["Value"], "8000000");
        assert_eq!(cond["IsRequired"], true);
    }

    #[test]
    fn hls_transcoding_fallback_shape() {
        let v = json(None);
        let tp = &v["TranscodingProfiles"][0];
        assert_eq!(tp["Container"], "ts");
        assert_eq!(tp["Protocol"], "hls");
        assert_eq!(tp["Type"], "Video");
        assert_eq!(tp["VideoCodec"], "h264,hevc");
        assert_eq!(tp["AudioCodec"], "aac,ac3");
        assert_eq!(tp["Context"], "Streaming");
        assert_eq!(
            v["TranscodingProfiles"]
                .as_array()
                .expect("test assertion")
                .len(),
            1
        );
    }

    #[test]
    fn subtitle_profiles_match_embed_vs_external_split() {
        let v = json(None);
        let profiles = v["SubtitleProfiles"].as_array().expect("test assertion");
        assert_eq!(
            profiles.len(),
            EMBEDDED_SUBTITLE_FORMATS.len() + EXTERNAL_SUBTITLE_FORMATS.len()
        );

        let embed: Vec<&str> = profiles
            .iter()
            .filter(|p| p["Method"] == "Embed")
            .map(|p| p["Format"].as_str().expect("test assertion"))
            .collect();
        let external: Vec<&str> = profiles
            .iter()
            .filter(|p| p["Method"] == "External")
            .map(|p| p["Format"].as_str().expect("test assertion"))
            .collect();

        for f in EMBEDDED_SUBTITLE_FORMATS {
            assert!(embed.contains(f), "missing embed subtitle format {f}");
        }
        for f in EXTERNAL_SUBTITLE_FORMATS {
            assert!(external.contains(f), "missing external subtitle format {f}");
        }
        // PGS/VobSub are bitmap formats — never sent as an external sidecar.
        assert!(!external.contains(&"pgssub"));
        assert!(!external.contains(&"dvdsub"));
    }

    #[test]
    fn no_interlaced_or_hdcp_style_fake_constraints_leak_in() {
        // Regression guard: nothing in this module should ever reference an
        // "interlaced" condition — mpv has no such limitation on Apple
        // Silicon, so declaring one would cause needless server transcodes.
        let v = json(Some(5_000_000));
        let dump = v.to_string();
        assert!(!dump.contains("IsInterlaced"));
    }
}
