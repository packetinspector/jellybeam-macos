//! Playback decision: pick the best `MediaSource` from a `PlaybackInfoResponse`
//! honoring the server's SupportsDirectPlay / SupportsDirectStream /
//! TranscodingUrl verdict, and synthesize human-readable transcode reasons.
//!
//! Rather than hard-depend directly on the generated `MediaSourceInfo`
//! struct's fields, [`extract_facts`] round-trips each source through
//! `serde_json` into [`SourceFacts`]. The generated type carries
//! `SupportsDirectPlay`/`SupportsDirectStream`/`TranscodingUrl`/`Container`
//! etc. (PascalCase JSON names — confirmed by inspection of
//! `jellyfin-api/src/models.rs`), so this reads real data end to end; the
//! round-trip costs nothing and stays robust to future field churn in the
//! generated type. The decision matrix itself ([`choose`]) is
//! fixture-tested directly against [`SourceFacts`], independent of the
//! extraction step.

use jellyfin_api::models::MediaSourceInfo;
#[cfg(test)]
use jellyfin_api::models::{MediaStream, MediaStreamType};
use serde::Deserialize;

use crate::CoreError;

#[derive(Debug, Clone, Deserialize, Default, PartialEq)]
#[serde(rename_all = "PascalCase", default)]
pub(crate) struct SourceFacts {
    pub id: Option<String>,
    pub container: Option<String>,
    pub path: Option<String>,
    pub protocol: Option<String>,
    pub supports_direct_play: bool,
    pub supports_direct_stream: bool,
    pub supports_transcoding: bool,
    pub transcoding_url: Option<String>,
    /// docs/PLUGIN-CHANNELS.md §2.3: enough of each
    /// `MediaStream` to tell whether the server had codec facts to judge
    /// direct-playability with -- see [`is_codec_blind`].
    pub media_streams: Vec<StreamFacts>,
}

/// Just enough of a `MediaStream` to tell whether the server actually had
/// codec facts to judge direct-playability with -- see [`is_codec_blind`].
#[derive(Debug, Clone, Deserialize, Default, PartialEq)]
#[serde(rename_all = "PascalCase", default)]
pub(crate) struct StreamFacts {
    pub codec: Option<String>,
    #[serde(rename = "Type")]
    pub stream_type: Option<String>,
}

/// Best-effort extraction — never panics even if a future server response
/// is missing fields the schema says are optional.
pub(crate) fn extract_facts(source: &MediaSourceInfo) -> SourceFacts {
    serde_json::to_value(source)
        .ok()
        .and_then(|v| serde_json::from_value(v).ok())
        .unwrap_or_default()
}

/// docs/PLUGIN-CHANNELS.md §2.3: true when `facts`
/// carries no Video or Audio `MediaStream` with a non-empty `Codec` -- i.e.
/// the server had nothing to judge direct-playability with. (A Jellyfin
/// plugin channel, e.g. a TVHeadend recordings backend, reports
/// `MediaStreams` with `Codec: null` for every stream; a source with zero
/// streams at all counts as blind too.) Subtitle/other stream types never
/// count, in either direction. Stream-type comparison is case-insensitive.
pub(crate) fn is_codec_blind(facts: &SourceFacts) -> bool {
    !facts.media_streams.iter().any(|stream| {
        let is_video_or_audio = stream
            .stream_type
            .as_deref()
            .is_some_and(|t| t.eq_ignore_ascii_case("Video") || t.eq_ignore_ascii_case("Audio"));
        let has_codec = stream.codec.as_deref().is_some_and(|c| !c.is_empty());
        is_video_or_audio && has_codec
    })
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Choice {
    DirectPlay { index: usize },
    Transcode { index: usize, reasons: Vec<String> },
}

impl Choice {
    pub(crate) fn index(&self) -> usize {
        match self {
            Choice::DirectPlay { index } | Choice::Transcode { index, .. } => *index,
        }
    }
}

/// Pure decision matrix, independently testable with fixtures:
/// - Any source with SupportsDirectPlay or SupportsDirectStream wins,
///   preferred in source-list order (the server already orders sources by
///   preference).
/// - Otherwise, among the sources the server would otherwise route to
///   transcoding (those with a `TranscodingUrl`), the first one that is
///   [`is_codec_blind`] is played directly instead
///   (docs/PLUGIN-CHANNELS.md §2.3). A "no direct play"
///   verdict is only real evidence of incompatibility when the server
///   actually had codec facts to judge the source with; a Jellyfin plugin
///   channel (e.g. a TVHeadend recordings backend) hands back
///   `MediaStreams` with every `Codec` null, and the server answers
///   `SupportsDirectPlay: false` plus a `TranscodingUrl` purely because our
///   device profile declares codecs at all, not because it detected an
///   incompatible one. The static stream endpoint serves those bytes as-is,
///   and Direct Play is this app's default (transcoding is strictly
///   opt-in) -- so a codec-blind verdict must not force a transcode. (A
///   source with no `TranscodingUrl` either has nothing routing it
///   anywhere and correctly falls through to `NoPlayableSource` below.)
/// - Otherwise, the first source with a TranscodingUrl is used.
/// - Otherwise, `NoPlayableSource`.
///
/// Reviewed and retained un-scoped to a particular plugin: the rule is
/// deliberately keyed on server-reported facts (no codecs on any stream)
/// rather than on channel/plugin identity, which isn't reliably present on
/// a source at all (docs/PLUGIN-CHANNELS.md §2.1). And
/// under this app's opt-in-only transcoding default, a codec-blind source
/// without this rule isn't "transcoded instead" -- it's unplayable -- so
/// the rule strictly widens what plays; the residual risk of a genuinely
/// incompatible codec-blind source is accepted.
pub(crate) fn choose(sources: &[SourceFacts]) -> Result<Choice, CoreError> {
    if sources.is_empty() {
        return Err(CoreError::NoPlayableSource);
    }

    if let Some((index, _)) = sources
        .iter()
        .enumerate()
        .find(|(_, f)| f.supports_direct_play || f.supports_direct_stream)
    {
        return Ok(Choice::DirectPlay { index });
    }

    if let Some((index, facts)) = sources
        .iter()
        .enumerate()
        .find(|(_, f)| f.transcoding_url.is_some() && is_codec_blind(f))
    {
        tracing::info!(
            media_source_id = facts.id.as_deref().unwrap_or("<unknown>"),
            "server judged direct play unsupported with no codec facts to judge by; \
             playing directly via the static stream endpoint instead of transcoding"
        );
        return Ok(Choice::DirectPlay { index });
    }

    if let Some((index, facts)) = sources
        .iter()
        .enumerate()
        .find(|(_, f)| f.transcoding_url.is_some())
    {
        return Ok(Choice::Transcode {
            index,
            reasons: synthesize_reasons(facts),
        });
    }

    Err(CoreError::NoPlayableSource)
}

/// The pinned OpenAPI spec (v12.0.0) doesn't expose a structured
/// `TranscodeReasons` list on `MediaSourceInfo` (that enum only appears on
/// `SessionInfoDto.TranscodingInfo`, which comes from `/Sessions`, not
/// `PlaybackInfo`). So we synthesize a readable reason from what the server
/// did tell us via its support flags, which is always available and never
/// leaves the UI silent about why a transcode happened.
fn synthesize_reasons(facts: &SourceFacts) -> Vec<String> {
    let mut reasons = Vec::new();
    if !facts.supports_direct_play {
        reasons.push("server: direct play not supported for this source".to_string());
    }
    if !facts.supports_direct_stream {
        reasons.push("server: direct stream not supported for this source".to_string());
    }
    if let Some(container) = &facts.container {
        reasons.push(format!("container '{container}' requires transcoding"));
    }
    if reasons.is_empty() {
        // Flags looked fine but the server still handed us a TranscodingUrl
        // instead — trust it and say so rather than going silent.
        reasons.push("server selected transcoding for this source".to_string());
    }
    reasons
}

#[cfg(test)]
mod tests {
    use super::*;

    fn direct_play(id: &str) -> SourceFacts {
        SourceFacts {
            id: Some(id.to_string()),
            supports_direct_play: true,
            ..Default::default()
        }
    }

    fn direct_stream(id: &str) -> SourceFacts {
        SourceFacts {
            id: Some(id.to_string()),
            supports_direct_stream: true,
            ..Default::default()
        }
    }

    /// A source the server genuinely judged incompatible: it had real codec
    /// facts (unlike [`codec_blind`]) and still answered no direct play.
    fn transcode_only(id: &str, container: &str) -> SourceFacts {
        SourceFacts {
            id: Some(id.to_string()),
            container: Some(container.to_string()),
            transcoding_url: Some(format!("/videos/{id}/master.m3u8")),
            supports_transcoding: true,
            media_streams: vec![
                stream("Video", Some("wmv3")),
                stream("Audio", Some("wmav2")),
            ],
            ..Default::default()
        }
    }

    fn unplayable(id: &str) -> SourceFacts {
        SourceFacts {
            id: Some(id.to_string()),
            ..Default::default()
        }
    }

    fn stream(stream_type: &str, codec: Option<&str>) -> StreamFacts {
        StreamFacts {
            stream_type: Some(stream_type.to_string()),
            codec: codec.map(str::to_string),
        }
    }

    /// A server-side plugin channel source with no codec facts at all
    /// (`Codec: null` on every stream) that the server nonetheless marked
    /// as not directly playable, with a `TranscodingUrl` set.
    fn codec_blind(id: &str) -> SourceFacts {
        SourceFacts {
            id: Some(id.to_string()),
            transcoding_url: Some(format!("/videos/{id}/master.m3u8")),
            supports_transcoding: true,
            media_streams: vec![stream("Video", None), stream("Audio", None)],
            ..Default::default()
        }
    }

    #[test]
    fn picks_direct_play_source() {
        let sources = vec![direct_play("a")];
        assert_eq!(
            choose(&sources).expect("test assertion"),
            Choice::DirectPlay { index: 0 }
        );
    }

    #[test]
    fn picks_direct_stream_source() {
        let sources = vec![direct_stream("a")];
        assert_eq!(
            choose(&sources).expect("test assertion"),
            Choice::DirectPlay { index: 0 }
        );
    }

    #[test]
    fn direct_play_preferred_over_transcode_when_both_present() {
        let sources = vec![transcode_only("a", "wmv"), direct_play("b")];
        assert_eq!(
            choose(&sources).expect("test assertion"),
            Choice::DirectPlay { index: 1 }
        );
    }

    #[test]
    fn falls_back_to_transcode_with_reasons() {
        let sources = vec![transcode_only("a", "wmv")];
        match choose(&sources).expect("test assertion") {
            Choice::Transcode { index, reasons } => {
                assert_eq!(index, 0);
                assert!(reasons.iter().any(|r| r.contains("direct play")));
                assert!(reasons.iter().any(|r| r.contains("wmv")));
            }
            other => panic!("expected Transcode, got {other:?}"),
        }
    }

    #[test]
    fn no_playable_source_when_list_empty() {
        assert!(matches!(
            choose(&[]).expect_err("test assertion"),
            CoreError::NoPlayableSource
        ));
    }

    #[test]
    fn no_playable_source_when_nothing_supports_anything() {
        let sources = vec![unplayable("a"), unplayable("b")];
        assert!(matches!(
            choose(&sources).expect_err("test assertion"),
            CoreError::NoPlayableSource
        ));
    }

    // --- docs/PLUGIN-CHANNELS.md §2.3: codec-blind rule

    #[test]
    fn picks_codec_blind_source_over_transcoding_when_server_had_no_codec_facts() {
        let sources = vec![codec_blind("a")];
        assert_eq!(
            choose(&sources).expect("test assertion"),
            Choice::DirectPlay { index: 0 }
        );
    }

    #[test]
    fn does_not_widen_rule_to_a_source_with_real_codec_facts() {
        // Direct play false + transcoding url, but the server *did* have
        // codec facts to judge with -- this must still transcode.
        let sources = vec![transcode_only("a", "wmv")];
        match choose(&sources).expect("test assertion") {
            Choice::Transcode { index, .. } => assert_eq!(index, 0),
            other => panic!("expected Transcode, got {other:?}"),
        }
    }

    #[test]
    fn a_real_direct_play_source_wins_over_a_later_codec_blind_one() {
        let sources = vec![codec_blind("a"), direct_play("b")];
        assert_eq!(
            choose(&sources).expect("test assertion"),
            Choice::DirectPlay { index: 1 }
        );
    }

    #[test]
    fn zero_media_streams_counts_as_codec_blind() {
        let facts = SourceFacts {
            id: Some("a".to_string()),
            transcoding_url: Some("/videos/a/master.m3u8".to_string()),
            supports_transcoding: true,
            media_streams: Vec::new(),
            ..Default::default()
        };
        assert!(is_codec_blind(&facts));
        assert_eq!(
            choose(&[facts]).expect("test assertion"),
            Choice::DirectPlay { index: 0 }
        );
    }

    #[test]
    fn subtitle_only_codec_info_still_counts_as_blind() {
        let facts = SourceFacts {
            id: Some("a".to_string()),
            transcoding_url: Some("/videos/a/master.m3u8".to_string()),
            supports_transcoding: true,
            media_streams: vec![
                stream("Video", None),
                stream("Audio", None),
                stream("Subtitle", Some("subrip")),
            ],
            ..Default::default()
        };
        assert!(is_codec_blind(&facts));
        assert_eq!(
            choose(&[facts]).expect("test assertion"),
            Choice::DirectPlay { index: 0 }
        );
    }

    #[test]
    fn a_video_stream_with_a_real_codec_is_not_blind() {
        let facts = SourceFacts {
            id: Some("a".to_string()),
            media_streams: vec![stream("Video", Some("h264")), stream("Audio", None)],
            ..Default::default()
        };
        assert!(!is_codec_blind(&facts));
    }

    #[test]
    fn extract_facts_round_trip_carries_media_streams_and_codecs() {
        let source = MediaSourceInfo {
            id: Some("abc".into()),
            supports_direct_play: Some(false),
            supports_transcoding: Some(true),
            transcoding_url: Some("/videos/abc/master.m3u8".into()),
            media_streams: vec![
                MediaStream {
                    type_: Some(MediaStreamType::Video),
                    codec: Some("h264".into()),
                    ..Default::default()
                },
                MediaStream {
                    type_: Some(MediaStreamType::Audio),
                    codec: Some("aac".into()),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let facts = extract_facts(&source);
        assert_eq!(facts.media_streams.len(), 2);
        assert_eq!(facts.media_streams[0].stream_type.as_deref(), Some("Video"));
        assert_eq!(facts.media_streams[0].codec.as_deref(), Some("h264"));
        assert_eq!(facts.media_streams[1].stream_type.as_deref(), Some("Audio"));
        assert_eq!(facts.media_streams[1].codec.as_deref(), Some("aac"));
        assert!(!is_codec_blind(&facts));
        // This source has real codec facts, so despite SupportsDirectPlay
        // being false, the codec-blind rule must not fire for it.
        match choose(&[facts]).expect("test assertion") {
            Choice::Transcode { .. } => {}
            other => panic!("expected Transcode, got {other:?}"),
        }
    }

    #[test]
    fn extract_facts_defaults_missing_fields_safely() {
        let source = MediaSourceInfo {
            id: Some("abc".into()),
            ..Default::default()
        };
        let facts = extract_facts(&source);
        assert_eq!(facts.id.as_deref(), Some("abc"));
        assert!(!facts.supports_direct_play);
        assert!(!facts.supports_direct_stream);
        assert!(facts.transcoding_url.is_none());
    }

    #[test]
    fn extract_facts_reads_real_capability_fields_from_media_source_info() {
        let source = MediaSourceInfo {
            id: Some("abc".into()),
            container: Some("mkv".into()),
            supports_direct_play: Some(true),
            supports_direct_stream: Some(false),
            supports_transcoding: Some(true),
            ..Default::default()
        };
        let facts = extract_facts(&source);
        assert!(facts.supports_direct_play);
        assert!(!facts.supports_direct_stream);
        assert_eq!(facts.container.as_deref(), Some("mkv"));
        assert_eq!(
            choose(&[facts]).expect("test assertion"),
            Choice::DirectPlay { index: 0 }
        );
    }

    #[test]
    fn extract_facts_from_raw_json_matches_spec_field_names() {
        // Simulates the exact wire JSON shape (PascalCase, per the OpenAPI
        // spec) via direct SourceFacts deserialization, independent of the
        // generated MediaSourceInfo struct.
        let json = serde_json::json!({
            "Id": "abc",
            "Container": "mkv",
            "SupportsDirectPlay": true,
            "SupportsDirectStream": false,
            "SupportsTranscoding": true,
        });
        let facts: SourceFacts = serde_json::from_value(json).expect("test assertion");
        assert!(facts.supports_direct_play);
        assert_eq!(
            choose(&[facts]).expect("test assertion"),
            Choice::DirectPlay { index: 0 }
        );
    }
}
