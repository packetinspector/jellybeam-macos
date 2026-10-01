//! Integration tests against a live Jellyfin server.
//!
//! `#[ignore]`d by default (per the crate's test tier contract — these need
//! network + a real server, unlike the unit/fixture tests in `src/`). Run
//! explicitly with:
//!
//!   cargo test -p jellyfin-api --test live_local_server -- --ignored --test-threads=1
//!
//! Targets the local dev server (`dev/docker-compose.yml`, `dev/server.sh up`)
//! at http://localhost:8096 by default with the seeded admin user
//! jellybeam-admin / jellybeam-test (see dev/setup-server.sh). Override with
//! `JELLYBEAM_DEV_SERVER_URL` to target another dev server instance (e.g. the
//! opt-in Jellyfin 12.0 server on :8097 -- see dev/README.md).
//! `--test-threads=1` because several tests share playback-report state
//! (start -> progress -> stop) against the same server session and
//! shouldn't interleave.

use jellyfin_api::models::{DeviceProfile, DirectPlayProfile, DlnaProfileType};
use jellyfin_api::{
    ClientIdentity, ImageKind, ItemQuery, JellyfinClient, PlaybackReport, PlaybackReportKind,
};

fn base_url() -> String {
    std::env::var("JELLYBEAM_DEV_SERVER_URL")
        .unwrap_or_else(|_| "http://localhost:8096".to_string())
}
const ADMIN_USER: &str = "jellybeam-admin";
const ADMIN_PASS: &str = "jellybeam-test";

/// Redact the `ApiKey=...` query param before printing a URL to test
/// output: `image_url`/`stream_url` embed the raw access token, and this
/// suite's own admin token shouldn't end up verbatim in captured stdout/CI
/// logs just because a test wanted to eyeball the URL shape.
fn redact_api_key(url: &str) -> String {
    match url.find("ApiKey=") {
        Some(idx) => {
            let value_start = idx + "ApiKey=".len();
            let value_end = url[value_start..]
                .find('&')
                .map(|i| value_start + i)
                .unwrap_or(url.len());
            format!(
                "{}ApiKey=<redacted>{}",
                &url[..value_start],
                &url[value_end..]
            )
        }
        None => url.to_string(),
    }
}

fn identity() -> ClientIdentity {
    ClientIdentity {
        client: "Jellybeam".to_string(),
        device: "integration-test".to_string(),
        device_id: "jellybeam-integration-test".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
    }
}

fn permissive_device_profile() -> DeviceProfile {
    DeviceProfile {
        direct_play_profiles: vec![DirectPlayProfile {
            container: Some("mp4,mkv,webm,mov".to_string()),
            type_: Some(DlnaProfileType::Video),
            ..Default::default()
        }],
        ..Default::default()
    }
}

async fn authenticated_client() -> JellyfinClient {
    let (client, result) =
        JellyfinClient::authenticate_by_name(&base_url(), identity(), ADMIN_USER, ADMIN_PASS)
            .await
            .expect("authenticate_by_name against local dev server");
    assert!(
        result.access_token.is_some(),
        "server did not return an access token"
    );
    client
}

#[tokio::test]
#[ignore]
async fn authenticate_by_name_succeeds() {
    let client = authenticated_client().await;
    // authenticate_by_name should always populate user_id from the
    // real server's AuthenticationResult.User.Id.
    assert!(
        client.user_id().is_some(),
        "authenticate_by_name should populate user_id"
    );
}

#[tokio::test]
#[ignore]
async fn wrong_password_is_unauthorized() {
    let result = JellyfinClient::authenticate_by_name(
        &base_url(),
        identity(),
        ADMIN_USER,
        "definitely-not-the-password",
    )
    .await;
    let err = match result {
        Err(e) => e,
        Ok(_) => panic!("wrong password must not succeed"),
    };
    assert!(
        matches!(
            err,
            jellyfin_api::ApiError::Unauthorized | jellyfin_api::ApiError::Status { .. }
        ),
        "unexpected error variant: {err:?}"
    );
}

#[tokio::test]
#[ignore]
async fn get_user_views_returns_configured_libraries() {
    let client = authenticated_client().await;
    let views = client.get_user_views().await.expect("get_user_views");
    // dev/setup-server.sh seeds at least one library; don't hard-code the
    // exact name/count (that's the corpus generator's business), just
    // assert the call round-trips real BaseItemDto rows without erroring.
    for view in &views {
        assert!(view.id.is_some(), "view missing Id: {view:?}");
    }
    println!(
        "views: {:?}",
        views
            .iter()
            .filter_map(|v| v.name.clone())
            .collect::<Vec<_>>()
    );
}

#[tokio::test]
#[ignore]
async fn get_items_and_playback_info_round_trip() {
    let client = authenticated_client().await;
    let views = client.get_user_views().await.expect("get_user_views");

    let mut found_item = None;
    for view in &views {
        let Some(parent_id) = view.id.map(|id| id.to_string()) else {
            continue;
        };
        let result = client
            .get_items(&ItemQuery {
                parent_id: Some(parent_id),
                include_item_types: vec!["Movie".to_string(), "Episode".to_string()],
                recursive: true,
                sort_by: None,
                sort_order: None,
                fields: vec![],
                start_index: 0,
                limit: 5,
                ids: vec![],
                is_missing: None,
                min_date_last_saved: None,
                ..ItemQuery::new()
            })
            .await
            .expect("get_items");
        if let Some(item) = result.items.into_iter().next() {
            found_item = Some(item);
            break;
        }
    }

    let Some(item) = found_item else {
        eprintln!("no playable item found in any library; skipping PlaybackInfo assertion");
        return;
    };
    let item_id = item.id.expect("item has an Id").to_string();

    let profile = permissive_device_profile();
    let info = client
        .get_playback_info(&item_id, &profile, Some(0))
        .await
        .expect("get_playback_info");
    assert!(
        !info.media_sources.is_empty(),
        "expected at least one MediaSource"
    );

    let source = &info.media_sources[0];
    let stream_url = client.stream_url(&item_id, source);
    assert!(
        stream_url.starts_with(&base_url()),
        "stream_url should be rooted at the server: {stream_url}"
    );
    println!("stream_url: {}", redact_api_key(&stream_url));

    if let Some(tag) = item.image_tags.get("Primary") {
        let image_url = client.image_url(&item_id, ImageKind::Primary, tag, 300);
        assert!(image_url.starts_with(&base_url()));
        println!("image_url: {}", redact_api_key(&image_url));
    }
}

/// Regression test for the known server bug (docs/OVERVIEW.md §4): a
/// PlaybackReport with a non-integer VolumeLevel 400s. This drives the real
/// three-endpoint lifecycle (Start -> Progress -> Stopped) against the live
/// server and asserts none of them error — the strongest possible check
/// that our `u8`-typed VolumeLevel field actually satisfies the server's
/// int32 model binder, not just that it "looks like an int" in a unit test.
#[tokio::test]
#[ignore]
async fn playback_report_lifecycle_does_not_400() {
    let client = authenticated_client().await;
    let views = client.get_user_views().await.expect("get_user_views");

    let mut found_item = None;
    for view in &views {
        let Some(parent_id) = view.id.map(|id| id.to_string()) else {
            continue;
        };
        let result = client
            .get_items(&ItemQuery {
                parent_id: Some(parent_id),
                include_item_types: vec!["Movie".to_string(), "Episode".to_string()],
                recursive: true,
                sort_by: None,
                sort_order: None,
                fields: vec![],
                start_index: 0,
                limit: 1,
                ids: vec![],
                is_missing: None,
                min_date_last_saved: None,
                ..ItemQuery::new()
            })
            .await
            .expect("get_items");
        if let Some(item) = result.items.into_iter().next() {
            found_item = Some(item);
            break;
        }
    }

    let Some(item) = found_item else {
        eprintln!("no playable item found in any library; skipping playback-report lifecycle");
        return;
    };
    let item_id = item.id.expect("item has an Id").to_string();

    let profile = permissive_device_profile();
    let info = client
        .get_playback_info(&item_id, &profile, Some(0))
        .await
        .expect("get_playback_info");
    let Some(source) = info.media_sources.into_iter().next() else {
        eprintln!("no MediaSource for item; skipping playback-report lifecycle");
        return;
    };
    let media_source_id = source.id.clone().unwrap_or_else(|| item_id.clone());
    let play_session_id = info
        .play_session_id
        .unwrap_or_else(|| "test-session".to_string());

    let base = PlaybackReport {
        kind: PlaybackReportKind::Start,
        item_id: item_id.clone(),
        media_source_id: media_source_id.clone(),
        position_ticks: 0,
        is_paused: false,
        volume_level: 100,
        audio_stream_index: None,
        subtitle_stream_index: None,
        play_session_id: play_session_id.clone(),
    };
    client
        .report_playback(base.clone())
        .await
        .expect("Start report must not 400");

    client
        .report_playback(PlaybackReport {
            kind: PlaybackReportKind::Progress,
            position_ticks: 1_000_000,
            volume_level: 37, // deliberately not a "round" value like 0/50/100
            is_paused: true,
            ..base.clone()
        })
        .await
        .expect("Progress report must not 400");

    client
        .report_playback(PlaybackReport {
            kind: PlaybackReportKind::Stopped,
            position_ticks: 2_000_000,
            ..base
        })
        .await
        .expect("Stopped report must not 400");
}

/// `get_media_segments` against the real dev server. The corpus this
/// dev server is seeded from (`dev/setup-server.sh`) almost certainly has no
/// MediaSegments data (nothing generates skip-intro/credits markers at
/// seed time) and may even predate the MediaSegments feature entirely (see
/// `codegen/regen.sh`'s pin-policy comment: pinned spec 12.0.0 is newer
/// than the pinned server image 10.10.7) -- so this deliberately does not
/// assert on the *contents* of the result. What it asserts is the thing
/// that actually regresses if `get_media_segments` is wrong: the call
/// completes without erroring (whether the server answers with a real,
/// empty `MediaSegmentDtoQueryResult` or 404s because it doesn't know the
/// route, both fold to `Ok(vec![])` -- see the doc comment on
/// `get_media_segments`).
#[tokio::test]
#[ignore]
async fn get_media_segments_does_not_error_against_live_server() {
    let client = authenticated_client().await;
    let views = client.get_user_views().await.expect("get_user_views");

    let mut found_item = None;
    for view in &views {
        let Some(parent_id) = view.id.map(|id| id.to_string()) else {
            continue;
        };
        let result = client
            .get_items(&ItemQuery {
                parent_id: Some(parent_id),
                include_item_types: vec!["Movie".to_string(), "Episode".to_string()],
                recursive: true,
                sort_by: None,
                sort_order: None,
                fields: vec![],
                start_index: 0,
                limit: 1,
                ids: vec![],
                is_missing: None,
                min_date_last_saved: None,
                ..ItemQuery::new()
            })
            .await
            .expect("get_items");
        if let Some(item) = result.items.into_iter().next() {
            found_item = Some(item);
            break;
        }
    }

    let Some(item) = found_item else {
        eprintln!("no playable item found in any library; skipping get_media_segments check");
        return;
    };
    let item_id = item.id.expect("item has an Id").to_string();

    let segments = client
        .get_media_segments(&item_id, &[])
        .await
        .expect("get_media_segments must not error, even with no data / no server support");
    println!(
        "segments for {item_id}: {} (empty is expected on this corpus)",
        segments.len()
    );

    let filtered = client
        .get_media_segments(&item_id, &["Intro", "Outro"])
        .await
        .expect("get_media_segments with includeSegmentTypes must not error");
    println!(
        "filtered (Intro/Outro) segments for {item_id}: {}",
        filtered.len()
    );
}

/// The primitive `media-cache::sync::delta_sync` is built on, exercised
/// against the real pinned dev server (10.10.7) because that is the ONLY
/// place its failure mode shows up: a `recursive=true` query carrying
/// `minDateLastSaved` *without* the `minDateLastSavedForUser` companion
/// returns HTTP 500 ("Must add values for the following parameters:
/// @MinDateLastSavedForUser" -- see `ItemQuery::min_date_last_saved`). A
/// mock server happily answers either shape, so only this test can catch a
/// regression that would silently reduce delta sync to a no-op-with-errors
/// in the field.
///
/// Asserts the filter is genuinely applied, not just accepted: a far-future
/// cursor must return nothing, while a far-past one must return items.
#[tokio::test]
#[ignore]
async fn min_date_last_saved_filters_without_erroring_against_live_server() {
    let client = authenticated_client().await;

    let past = client
        .get_items(&ItemQuery {
            recursive: true,
            limit: 5,
            min_date_last_saved: Some("2000-01-01T00:00:00Z".to_string()),
            ..ItemQuery::new()
        })
        .await
        .expect("minDateLastSaved query must not 500 (needs the ForUser companion param)");
    assert!(
        !past.items.is_empty(),
        "a far-past minDateLastSaved should return the whole corpus, got none"
    );

    let future = client
        .get_items(&ItemQuery {
            recursive: true,
            limit: 5,
            min_date_last_saved: Some("2099-01-01T00:00:00Z".to_string()),
            ..ItemQuery::new()
        })
        .await
        .expect("minDateLastSaved query must not 500");
    assert!(
        future.items.is_empty(),
        "a far-future minDateLastSaved should return nothing, got {} items -- \
         the filter is being ignored server-side",
        future.items.len()
    );
}

#[tokio::test]
#[ignore]
async fn websocket_connects_and_can_receive_or_time_out() {
    let client = authenticated_client().await;
    let mut rx = client.connect_ws().await.expect("connect_ws");

    // No library scan is happening, so we don't expect an event necessarily
    // — the assertion here is that the connection is live (recv doesn't
    // immediately error/close) within a short window, not that a specific
    // event arrives.
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(3), rx.recv()).await;
    match outcome {
        Ok(Some(event)) => println!("received event: {event:?}"),
        Ok(None) => panic!("websocket channel closed immediately"),
        Err(_timeout) => {
            println!("no event within 3s (expected if server is idle) — connection stayed open")
        }
    }
}

// --- Quick Connect ---------------------------------------------------------
//
// `Initiate` -> poll `Connect` -> `AuthenticateWithQuickConnect` is
// exercised end to end by the app crate's `JELLYBEAM_E2E` walk
// (`crates/app/src/e2e.rs`), which approves the request headlessly via the
// admin REST API's `/QuickConnect/Authorize?code=` (no second physical
// client available in CI). What's worth asserting here, at the jellyfin-api
// layer, is the piece that doesn't need a second party at all: Initiate
// against the real server returns a real, non-empty user-facing code.

#[tokio::test]
#[ignore]
async fn quick_connect_initiate_returns_a_code_against_live_dev_server() {
    let result = JellyfinClient::quick_connect_initiate(&base_url(), &identity())
        .await
        .expect("Initiate against the live dev server (is Quick Connect enabled? see dev/setup-server.sh)");
    let code = result
        .code
        .expect("live Initiate response should include a Code");
    assert!(
        !code.is_empty(),
        "live Quick Connect code should be non-empty"
    );
    println!("live Quick Connect code: {code}");
}
