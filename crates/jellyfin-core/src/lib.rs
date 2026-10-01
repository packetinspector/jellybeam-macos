//! Session lifecycle, DeviceProfile, playback decision + reporting state machine,
//! WebSocket supervision (reconnect/backoff + reconciliation triggers).
//!
//! `EventBus::spawn` returns `(broadcast::Receiver<BusEvent>, EventBusHandle)`
//! rather than just the receiver: drop the returned [`EventBusHandle`] (or
//! call [`EventBusHandle::shutdown`]) to stop the reconnect supervisor, or it
//! runs, reconnecting indefinitely, until process exit.
//!
//! `decide_playback`/`decide_playback_with_source` take `item_id: &str`
//! because a plugin/channel `MediaSourceInfo.id` is not the item id
//! (docs/PLUGIN-CHANNELS.md §2.3) — see `stream_url`'s
//! doc comment in `jellyfin_api`.

mod backoff;
mod device_profile;
mod event_bus;
mod playback;
mod reporting;

pub use event_bus::{EventBus, EventBusHandle};
pub use reporting::ReportingSession;

use jellyfin_api::models::{DeviceProfile, MediaSourceInfo, PlaybackInfoResponse};
#[cfg(test)]
use jellyfin_api::models::{MediaStream, MediaStreamType};
use jellyfin_api::{JellyfinClient, ServerEvent};
use tokio::sync::broadcast;

/// Builds Jellybeam's DeviceProfile: declares near-everything (mpv/ffmpeg) so the
/// server only transcodes for bandwidth or broken files. Correctness-critical:
/// contract tests assert an expected decision per corpus file.
pub fn build_device_profile(max_streaming_bitrate: Option<u32>) -> DeviceProfile {
    let raw = device_profile::build(max_streaming_bitrate);
    match serde_json::to_value(raw) {
        Ok(value) => match serde_json::from_value(value) {
            Ok(profile) => profile,
            Err(err) => {
                // Should never fail (both sides serialize identical PascalCase
                // JSON keys); fall back loudly rather than silently declaring
                // nothing, which the server would read as "transcode everything".
                tracing::error!(
                    ?err,
                    "RawDeviceProfile -> DeviceProfile round trip failed after \
                     serializing successfully; falling back to an empty default \
                     DeviceProfile (server will transcode nearly everything)"
                );
                DeviceProfile::default()
            }
        },
        Err(err) => {
            tracing::error!(
                ?err,
                "failed to serialize RawDeviceProfile to JSON; falling back to \
                 an empty default DeviceProfile (server will transcode nearly \
                 everything)"
            );
            DeviceProfile::default()
        }
    }
}

/// Outcome of PlaybackInfo negotiation, normalized for the player + UI (the UI
/// must always be able to show WHY a transcode happened — never silent).
#[derive(Debug, Clone)]
pub enum PlaybackDecision {
    DirectPlay {
        source: MediaSourceInfo,
        url: String,
    },
    Transcode {
        source: MediaSourceInfo,
        hls_url: String,
        reasons: Vec<String>,
    },
}

/// Picks the best `MediaSource` (see `playback::choose`: DirectPlay/DirectStream
/// beats a TranscodingUrl fallback, first-listed source wins ties) and asks the
/// client to build the playback URL for it. Delegates to
/// [`decide_playback_with_source`] with no preferred source.
///
/// `item_id` is threaded through separately from each `MediaSourceInfo.id`
/// because plugin/channel sources use a distinct, non-item id
/// (docs/PLUGIN-CHANNELS.md §2.3).
pub fn decide_playback(
    client: &JellyfinClient,
    item_id: &str,
    info: &PlaybackInfoResponse,
) -> Result<PlaybackDecision, CoreError> {
    decide_playback_with_source(client, item_id, info, None)
}

/// Like [`decide_playback`], but when `preferred_media_source_id` names a
/// `MediaSourceInfo.Id` present in `info.media_sources`, the decision matrix
/// is scoped to just that source — callers resuming against a
/// `MediaSourceId` already negotiated with the server need that exact source
/// honored rather than a possibly different "best" one.
///
/// Falls back to considering every source, as [`decide_playback`] does, when
/// `preferred_media_source_id` is `None` or matches no source.
pub fn decide_playback_with_source(
    client: &JellyfinClient,
    item_id: &str,
    info: &PlaybackInfoResponse,
    preferred_media_source_id: Option<&str>,
) -> Result<PlaybackDecision, CoreError> {
    let facts: Vec<playback::SourceFacts> = info
        .media_sources
        .iter()
        .map(playback::extract_facts)
        .collect();

    let preferred_index = preferred_media_source_id
        .and_then(|id| facts.iter().position(|f| f.id.as_deref() == Some(id)));

    let (scoped_facts, index_offset): (&[playback::SourceFacts], usize) = match preferred_index {
        Some(idx) => (std::slice::from_ref(&facts[idx]), idx),
        None => (facts.as_slice(), 0),
    };

    let choice = playback::choose(scoped_facts)?;
    let real_index = index_offset + choice.index();
    let source = info
        .media_sources
        .get(real_index)
        .cloned()
        .ok_or(CoreError::NoPlayableSource)?;
    let url = match choice {
        // docs/PLUGIN-CHANNELS.md §2.3: `stream_url` prefers
        // `TranscodingUrl` when set, but a DirectPlay choice must never resolve to
        // it (a codec-blind source can have both set). Clear `transcoding_url` so
        // `stream_url` falls through to the static stream form instead.
        playback::Choice::DirectPlay { .. } if source.transcoding_url.is_some() => {
            let direct_source = MediaSourceInfo {
                transcoding_url: None,
                ..source.clone()
            };
            client.stream_url(item_id, &direct_source)
        }
        _ => client.stream_url(item_id, &source),
    };
    Ok(match choice {
        playback::Choice::DirectPlay { .. } => PlaybackDecision::DirectPlay { source, url },
        playback::Choice::Transcode { reasons, .. } => PlaybackDecision::Transcode {
            source,
            hls_url: url,
            reasons,
        },
    })
}

#[derive(Debug, Clone)]
pub struct ReportContext {
    pub item_id: String,
    pub media_source_id: String,
    pub play_session_id: String,
}

/// Connection-state + forwarded server events emitted by [`EventBus::spawn`].
/// media-cache listens for `NeedsReconcile` to trigger its resync pass.
#[derive(Debug, Clone)]
pub enum BusEvent {
    Server(ServerEvent),
    Connected,
    Disconnected,
    /// Emitted after every (re)connect — reconciliation trigger.
    NeedsReconcile,
}

/// Receives the next [`BusEvent`], mapping a lagged consumer to
/// [`BusEvent::NeedsReconcile`] instead of ending the stream.
///
/// All consumers of an `EventBus` receiver must use this (or an equivalent
/// match on `Err(Lagged)`/`Err(Closed)`) instead of calling `.recv()`
/// directly: the naive `while let Ok(event) = rx.recv().await` pattern reads
/// `Err(Lagged)` as channel-closed and silently stops consuming. Returns
/// `None` only on a genuine `Closed`.
pub async fn recv_bus(rx: &mut broadcast::Receiver<BusEvent>) -> Option<BusEvent> {
    match rx.recv().await {
        Ok(event) => Some(event),
        Err(broadcast::error::RecvError::Lagged(skipped)) => {
            tracing::warn!(
                skipped,
                "event bus consumer lagged behind the broadcast channel; \
                 treating as NeedsReconcile instead of ending the stream"
            );
            Some(BusEvent::NeedsReconcile)
        }
        Err(broadcast::error::RecvError::Closed) => None,
    }
}

#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    #[error(transparent)]
    Api(#[from] jellyfin_api::ApiError),
    #[error("no playable media source")]
    NoPlayableSource,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pins that the round trip lands real data in the generated
    /// `DeviceProfile`; the exhaustive matrix itself is asserted against
    /// `RawDeviceProfile` in `device_profile::tests`.
    #[test]
    fn build_device_profile_populates_the_real_generated_model() {
        let profile = build_device_profile(Some(4_000_000));
        assert_eq!(profile.max_streaming_bitrate, Some(4_000_000));
        assert!(!profile.direct_play_profiles.is_empty());
        assert!(!profile.transcoding_profiles.is_empty());
        assert!(!profile.subtitle_profiles.is_empty());
        assert!(
            !profile.codec_profiles.is_empty(),
            "bitrate cap should produce a CodecProfile"
        );

        // Proves the capability matrix's content survives the round trip,
        // not just that some placeholder shell comes out non-null.
        let direct_play_video_codecs: Vec<&str> = profile
            .direct_play_profiles
            .iter()
            .filter_map(|p| p.video_codec.as_deref())
            .flat_map(|codecs| codecs.split(','))
            .collect();
        assert!(
            direct_play_video_codecs.contains(&"prores"),
            "missing 'prores' post-round-trip: {direct_play_video_codecs:?}"
        );
        assert!(
            direct_play_video_codecs.contains(&"hevc"),
            "missing 'hevc' post-round-trip: {direct_play_video_codecs:?}"
        );

        let subtitle_formats: Vec<&str> = profile
            .subtitle_profiles
            .iter()
            .filter_map(|p| p.format.as_deref())
            .collect();
        assert!(
            subtitle_formats.contains(&"ass"),
            "missing 'ass' subtitle format post-round-trip: {subtitle_formats:?}"
        );

        let transcode_video_codecs: Vec<&str> = profile
            .transcoding_profiles
            .iter()
            .filter_map(|p| p.video_codec.as_deref())
            .flat_map(|codecs| codecs.split(','))
            .collect();
        assert!(
            transcode_video_codecs.contains(&"hevc"),
            "missing 'hevc' transcode fallback post-round-trip: {transcode_video_codecs:?}"
        );

        let unbounded = build_device_profile(None);
        // "uncapped" must survive as the explicit unlimited value, not
        // degrade to an omitted field, which the server defaults to ~8 Mbps.
        assert_eq!(
            unbounded.max_streaming_bitrate,
            Some(device_profile::UNCAPPED_STREAMING_BITRATE as i32)
        );
        assert!(unbounded.codec_profiles.is_empty());
        assert!(
            !unbounded.direct_play_profiles.is_empty()
                && !unbounded.transcoding_profiles.is_empty()
                && !unbounded.subtitle_profiles.is_empty(),
            "the unbounded profile must round-trip the same non-empty matrix"
        );
    }

    // The decision matrix itself is fixture-tested in `playback::tests`;
    // these prove the public function is wired end to end, including the
    // real (synchronous, local, no network) `JellyfinClient::stream_url` call.

    fn client() -> JellyfinClient {
        JellyfinClient::from_token(
            "http://localhost:8096",
            jellyfin_api::ClientIdentity {
                client: "Jellybeam".into(),
                device: "test".into(),
                device_id: "test-device".into(),
                version: "0.1.0".into(),
            },
            "test-token",
        )
    }

    /// A source with real, incompatible codec facts (not codec-blind) that
    /// genuinely can't direct play — needed because a source with no
    /// `MediaStreams` now counts as codec-blind and would otherwise direct
    /// play (docs/PLUGIN-CHANNELS.md §2.3).
    fn incompatible_codec_streams() -> Vec<MediaStream> {
        vec![
            MediaStream {
                type_: Some(MediaStreamType::Video),
                codec: Some("wmv3".into()),
                ..Default::default()
            },
            MediaStream {
                type_: Some(MediaStreamType::Audio),
                codec: Some("wmav2".into()),
                ..Default::default()
            },
        ]
    }

    #[test]
    fn decide_playback_end_to_end_direct_play() {
        let info = PlaybackInfoResponse {
            media_sources: vec![MediaSourceInfo {
                id: Some("src-1".into()),
                supports_direct_play: Some(true),
                ..Default::default()
            }],
            ..Default::default()
        };
        match decide_playback(&client(), "src-1", &info).expect("test assertion") {
            PlaybackDecision::DirectPlay { url, .. } => {
                assert!(url.contains("/Videos/src-1/stream"), "{url}");
            }
            other => panic!("expected DirectPlay, got {other:?}"),
        }
    }

    /// Pins that a source whose `MediaSourceInfo.id` differs from the item
    /// id still builds a stream URL rooted at the item id
    /// (docs/PLUGIN-CHANNELS.md §2.3, §3).
    #[test]
    fn decide_playback_end_to_end_uses_item_id_when_source_id_differs() {
        let info = PlaybackInfoResponse {
            media_sources: vec![MediaSourceInfo {
                id: Some("ab12cd34".into()),
                supports_direct_play: Some(true),
                ..Default::default()
            }],
            ..Default::default()
        };
        match decide_playback(&client(), "item-real-1", &info).expect("test assertion") {
            PlaybackDecision::DirectPlay { url, .. } => {
                assert!(
                    url.contains("/Videos/item-real-1/stream"),
                    "path segment should be the item id, not the source id: {url}"
                );
                assert!(
                    url.contains("mediaSourceId=ab12cd34"),
                    "mediaSourceId query param should still be the source id: {url}"
                );
            }
            other => panic!("expected DirectPlay, got {other:?}"),
        }
    }

    #[test]
    fn decide_playback_end_to_end_transcode() {
        let info = PlaybackInfoResponse {
            media_sources: vec![MediaSourceInfo {
                id: Some("src-1".into()),
                container: Some("wmv".into()),
                transcoding_url: Some("/videos/src-1/master.m3u8".into()),
                media_streams: incompatible_codec_streams(),
                ..Default::default()
            }],
            ..Default::default()
        };
        match decide_playback(&client(), "src-1", &info).expect("test assertion") {
            PlaybackDecision::Transcode {
                hls_url, reasons, ..
            } => {
                assert!(hls_url.ends_with("/videos/src-1/master.m3u8"), "{hls_url}");
                assert!(!reasons.is_empty());
            }
            other => panic!("expected Transcode, got {other:?}"),
        }
    }

    #[test]
    fn decide_playback_end_to_end_no_playable_source() {
        let info = PlaybackInfoResponse::default();
        assert!(matches!(
            decide_playback(&client(), "item-1", &info).expect_err("test assertion"),
            CoreError::NoPlayableSource
        ));
    }

    // --- decide_playback_with_source ---

    #[test]
    fn decide_playback_with_source_scopes_the_decision_to_the_preferred_source() {
        // "a" is directly playable, "b" is transcode-only; without a
        // preference `choose` picks "a", so requesting "b" pins that scoping
        // actually restricts the matrix rather than being ignored.
        let info = PlaybackInfoResponse {
            media_sources: vec![
                MediaSourceInfo {
                    id: Some("a".into()),
                    supports_direct_play: Some(true),
                    ..Default::default()
                },
                MediaSourceInfo {
                    id: Some("b".into()),
                    container: Some("wmv".into()),
                    transcoding_url: Some("/videos/b/master.m3u8".into()),
                    media_streams: incompatible_codec_streams(),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };

        match decide_playback_with_source(&client(), "item-1", &info, Some("b"))
            .expect("test assertion")
        {
            PlaybackDecision::Transcode { source, .. } => {
                assert_eq!(source.id.as_deref(), Some("b"));
            }
            other => panic!("expected Transcode for the preferred source 'b', got {other:?}"),
        }

        // Without a preference, the default (best-source) behavior still picks "a".
        match decide_playback_with_source(&client(), "item-1", &info, None).expect("test assertion")
        {
            PlaybackDecision::DirectPlay { source, .. } => {
                assert_eq!(source.id.as_deref(), Some("a"));
            }
            other => panic!("expected DirectPlay for the default choice, got {other:?}"),
        }
    }

    #[test]
    fn decide_playback_with_source_falls_back_when_preferred_id_not_found() {
        let info = PlaybackInfoResponse {
            media_sources: vec![MediaSourceInfo {
                id: Some("a".into()),
                supports_direct_play: Some(true),
                ..Default::default()
            }],
            ..Default::default()
        };

        match decide_playback_with_source(&client(), "item-1", &info, Some("does-not-exist"))
            .expect("test assertion")
        {
            PlaybackDecision::DirectPlay { source, .. } => {
                assert_eq!(source.id.as_deref(), Some("a"));
            }
            other => panic!("expected fallback to the default choice, got {other:?}"),
        }
    }

    #[test]
    fn decide_playback_delegates_to_decide_playback_with_source_none() {
        let info = PlaybackInfoResponse {
            media_sources: vec![MediaSourceInfo {
                id: Some("src-1".into()),
                supports_direct_play: Some(true),
                ..Default::default()
            }],
            ..Default::default()
        };
        let via_default = match decide_playback(&client(), "src-1", &info).expect("test assertion")
        {
            PlaybackDecision::DirectPlay { source, .. } => source.id,
            other => panic!("expected DirectPlay, got {other:?}"),
        };
        let via_explicit_none = match decide_playback_with_source(&client(), "src-1", &info, None)
            .expect("test assertion")
        {
            PlaybackDecision::DirectPlay { source, .. } => source.id,
            other => panic!("expected DirectPlay, got {other:?}"),
        };
        assert_eq!(via_default, via_explicit_none);
    }

    // --- recv_bus ---

    #[tokio::test]
    async fn recv_bus_passes_through_ordinary_events() {
        let (tx, mut rx) = broadcast::channel(8);
        tx.send(BusEvent::Connected).expect("send");
        match recv_bus(&mut rx).await {
            Some(BusEvent::Connected) => {}
            other => panic!("expected Connected, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn recv_bus_maps_lagged_to_needs_reconcile_instead_of_ending_the_stream() {
        // Capacity 2, send 5: the receiver, subscribed before any sends, is guaranteed to lag.
        let (tx, mut rx) = broadcast::channel(2);
        for _ in 0..5 {
            let _ = tx.send(BusEvent::Disconnected);
        }
        match recv_bus(&mut rx).await {
            Some(BusEvent::NeedsReconcile) => {}
            other => panic!("expected NeedsReconcile on lag, got {other:?}"),
        }
        // The stream is not over; a bare `while let Ok(e) = rx.recv().await`
        // would have silently ended here.
        match recv_bus(&mut rx).await {
            Some(BusEvent::Disconnected) => {}
            other => panic!("expected the stream to continue after lag, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn recv_bus_returns_none_on_closed() {
        let (tx, mut rx) = broadcast::channel::<BusEvent>(8);
        drop(tx);
        assert!(recv_bus(&mut rx).await.is_none());
    }
}
