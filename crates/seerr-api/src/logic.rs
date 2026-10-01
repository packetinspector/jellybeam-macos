//! Pure, network-free decision/mapping functions: availability mapping,
//! season requestability, POST-vs-PUT request semantics, and image URL
//! building. Kept separate from [`crate::session`] so each decision can be
//! unit-tested directly against plain inputs, with no server or async
//! runtime involved.
//!
//! Every function here is `pub(crate)`: [`crate::session::SeerrSession`] is
//! the only caller, and it returns the finished [`crate::types`] records the
//! app consumes -- there's no need for the app to reach for these
//! lower-level mapping helpers directly.

use crate::models;
use crate::types::*;

/// `MediaInfo.status` (1..=6) -> [`SeerrAvailability`]. An absent status,
/// `1 = UNKNOWN`, and any value with no bucket here fold into `NotRequested`.
pub(crate) fn availability_from_status(status: Option<i64>) -> SeerrAvailability {
    match status {
        Some(2) => SeerrAvailability::Pending,
        Some(3) => SeerrAvailability::Processing,
        Some(4) => SeerrAvailability::PartiallyAvailable,
        Some(5) => SeerrAvailability::Available,
        _ => SeerrAvailability::NotRequested,
    }
}

fn availability_rank(availability: SeerrAvailability) -> u8 {
    match availability {
        SeerrAvailability::NotRequested => 0,
        SeerrAvailability::Pending => 1,
        SeerrAvailability::Processing => 2,
        SeerrAvailability::PartiallyAvailable => 3,
        SeerrAvailability::Available => 4,
    }
}

/// `MediaRequest.status` (`1 = PENDING`, `2 = APPROVED`, `3 = DECLINED`)
/// -> [`SeerrRequestStatus`], strictly -- `None` for anything else. Contrast
/// [`request_status_for_display`], lossy on purpose for "My Requests".
pub(crate) fn request_status_from_int(status: i64) -> Option<SeerrRequestStatus> {
    match status {
        1 => Some(SeerrRequestStatus::Pending),
        2 => Some(SeerrRequestStatus::Approved),
        3 => Some(SeerrRequestStatus::Declined),
        _ => None,
    }
}

/// `seerr_my_requests`-only mapping: unfiltered `GET /request` can also
/// return `4 = FAILURE`/`5 = COMPLETED`. `COMPLETED` maps to `Approved`
/// (closest analog); `FAILURE`/unrecognized map to `Declined`.
pub(crate) fn request_status_for_display(status: i64) -> SeerrRequestStatus {
    match status {
        1 => SeerrRequestStatus::Pending,
        2 | 5 => SeerrRequestStatus::Approved,
        _ => SeerrRequestStatus::Declined,
    }
}

fn request_status_rank(status: SeerrRequestStatus) -> u8 {
    match status {
        SeerrRequestStatus::Declined => 0,
        SeerrRequestStatus::Pending => 1,
        SeerrRequestStatus::Approved => 2,
    }
}

/// `jellyfinMediaId4k` preferred over `jellyfinMediaId` (docs/14's
/// contract section).
pub(crate) fn jellyfin_item_id_from(info: Option<&models::MediaInfo>) -> Option<String> {
    let info = info?;
    info.jellyfin_media_id4k
        .clone()
        .or_else(|| info.jellyfin_media_id.clone())
}

/// A season is requestable unless already at least PENDING, or already
/// covered by an existing pending/approved request (docs/14's wording).
pub(crate) fn season_requestable(
    availability: SeerrAvailability,
    request_status: Option<SeerrRequestStatus>,
) -> bool {
    let already_available = !matches!(availability, SeerrAvailability::NotRequested);
    let already_requested = matches!(
        request_status,
        Some(SeerrRequestStatus::Pending) | Some(SeerrRequestStatus::Approved)
    );
    !already_available && !already_requested
}

/// The most-advanced status carried by any *SD* request's entry for
/// `season_number` -- `is4k` requests excluded, since `SeerrTvDetail::seasons`
/// only reflects SD availability. Reads `status`, never `status4k`.
fn season_request_status(
    media_info: Option<&models::MediaInfo>,
    season_number: i32,
) -> Option<SeerrRequestStatus> {
    let info = media_info?;
    let mut best: Option<SeerrRequestStatus> = None;
    for request in info.requests.iter().filter(|r| !r.is4k) {
        for season in &request.seasons {
            if season.season_number != Some(season_number) {
                continue;
            }
            let Some(status) = season.status.and_then(request_status_from_int) else {
                continue;
            };
            if best.is_none_or(|current| request_status_rank(status) > request_status_rank(current))
            {
                best = Some(status);
            }
        }
    }
    best
}

/// Builds the SD season-status list: merges the title's own status with
/// `mediaInfo`'s (higher-ranked wins), `requestable` folds in any existing
/// SD request.
pub(crate) fn build_season_statuses(
    seasons: &[models::Season],
    media_info: Option<&models::MediaInfo>,
) -> Vec<SeerrSeasonStatus> {
    seasons
        .iter()
        .filter_map(|season| {
            let season_number = season.season_number?;
            let mut availability = availability_from_status(season.status);
            if let Some(info) = media_info {
                if let Some(info_season) = info
                    .seasons
                    .iter()
                    .find(|s| s.season_number == Some(season_number))
                {
                    let info_availability = availability_from_status(info_season.status);
                    if availability_rank(info_availability) > availability_rank(availability) {
                        availability = info_availability;
                    }
                }
            }
            let request_status = season_request_status(media_info, season_number);
            Some(SeerrSeasonStatus {
                season_number,
                name: season
                    .name
                    .clone()
                    .unwrap_or_else(|| format!("Season {season_number}")),
                episode_count: season.episode_count.unwrap_or(0),
                availability,
                requestable: season_requestable(availability, request_status),
            })
        })
        .collect()
}

pub(crate) fn movie_can_request(availability: SeerrAvailability) -> bool {
    availability == SeerrAvailability::NotRequested
}

pub(crate) fn tv_can_request(seasons: &[SeerrSeasonStatus]) -> bool {
    seasons.iter().any(|s| s.requestable)
}

/// The caller's own currently-active (pending or approved) request, if any
/// -- Cancel-request only makes sense for one the caller can cancel.
pub(crate) fn active_request_for(
    media_info: Option<&models::MediaInfo>,
    current_user_id: i64,
) -> Option<SeerrActiveRequest> {
    let info = media_info?;
    info.requests
        .iter()
        .filter(|r| r.requested_by.as_ref().map(|u| u.id) == Some(current_user_id))
        .filter_map(|r| request_status_from_int(r.status).map(|status| (r, status)))
        .filter(|(_, status)| {
            matches!(
                status,
                SeerrRequestStatus::Pending | SeerrRequestStatus::Approved
            )
        })
        .max_by_key(|(_, status)| request_status_rank(*status))
        .map(|(r, status)| SeerrActiveRequest {
            request_id: r.id,
            status,
            is_4k: r.is4k,
            seasons: r.seasons.iter().filter_map(|s| s.season_number).collect(),
        })
}

pub(crate) enum RequestAction {
    Create,
    Update { request_id: i64 },
}

/// POST-vs-PUT: if the caller already has a PENDING request for this item
/// at the same `is_4k` flavor, update it (`PUT`) instead of duplicating.
pub(crate) fn decide_request_action(
    existing_requests: &[models::MediaRequest],
    is_4k: bool,
    current_user_id: i64,
) -> RequestAction {
    existing_requests
        .iter()
        .find(|r| {
            r.is4k == is_4k
                && r.status == 1
                && r.requested_by.as_ref().map(|u| u.id) == Some(current_user_id)
        })
        .map(|r| RequestAction::Update { request_id: r.id })
        .unwrap_or(RequestAction::Create)
}

fn year_from_date_str(date: &str) -> Option<i32> {
    date.get(0..4)?.parse().ok()
}

/// docs/14's "Image URLs": `poster` at `w500`, `backdrop` at
/// `w1920_and_h1080_multi_faces`, base swapped when `cache_images` is set.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ImageContext<'a> {
    pub(crate) seerr_url: &'a str,
    pub(crate) cache_images: bool,
}

fn build_image_url(
    path: Option<&str>,
    size_segment: &str,
    images: ImageContext<'_>,
) -> Option<String> {
    let path = path.filter(|p| !p.is_empty())?;
    let base = if images.cache_images {
        format!("{}/imageproxy/tmdb", images.seerr_url.trim_end_matches('/'))
    } else {
        "https://image.tmdb.org".to_string()
    };
    Some(format!("{base}{size_segment}{path}"))
}

pub(crate) fn poster_url(path: Option<&str>, images: ImageContext<'_>) -> Option<String> {
    build_image_url(path, "/t/p/w500", images)
}

pub(crate) fn backdrop_url(path: Option<&str>, images: ImageContext<'_>) -> Option<String> {
    build_image_url(path, "/t/p/w1920_and_h1080_multi_faces", images)
}

/// Profile pictures ride the same `w500` prefix as posters -- TMDB/Seerr
/// don't expose a distinct profile-picture size in this app's contract.
pub(crate) fn profile_url(path: Option<&str>, images: ImageContext<'_>) -> Option<String> {
    build_image_url(path, "/t/p/w500", images)
}

pub(crate) fn media_type_from_str(value: Option<&str>) -> Option<SeerrMediaType> {
    match value {
        Some("movie") => Some(SeerrMediaType::Movie),
        Some("tv") => Some(SeerrMediaType::Tv),
        _ => None,
    }
}

/// Per-source fields feeding [`build_card`] -- the one piece each
/// `card_from_*` adapter below actually differs on. Borrowed, not owned:
/// every field is read from the source record and either cloned or mapped
/// once inside `build_card`, so there's no need to clone up front just to
/// populate this struct.
struct CardSource<'a> {
    tmdb_id: i64,
    title: Option<&'a str>,
    fallback_title: Option<&'a str>,
    date: Option<&'a str>,
    fallback_date: Option<&'a str>,
    overview: Option<&'a str>,
    poster_path: Option<&'a str>,
    backdrop_path: Option<&'a str>,
    media_info: Option<&'a models::MediaInfo>,
}

/// Shared builder behind all five `card_from_*` adapters (docs/14's card
/// mapping): title/year fall back from a primary to a secondary source
/// field (movie vs. TV naming), then poster/backdrop URLs and
/// availability/jellyfin-id are derived identically regardless of which
/// vendored DTO the fields came from.
fn build_card(
    media_type: SeerrMediaType,
    source: CardSource<'_>,
    images: ImageContext<'_>,
) -> SeerrCard {
    let title = source
        .title
        .or(source.fallback_title)
        .unwrap_or_default()
        .to_string();
    let year = source
        .date
        .or(source.fallback_date)
        .and_then(year_from_date_str);
    SeerrCard {
        media_type,
        tmdb_id: source.tmdb_id,
        title,
        year,
        overview: source.overview.map(str::to_string),
        poster_url: poster_url(source.poster_path, images),
        backdrop_url: backdrop_url(source.backdrop_path, images),
        availability: availability_from_status(source.media_info.and_then(|m| m.status)),
        jellyfin_item_id: jellyfin_item_id_from(source.media_info),
    }
}

pub(crate) fn card_from_result(
    item: &models::MediaResult,
    media_type: SeerrMediaType,
    images: ImageContext<'_>,
) -> SeerrCard {
    build_card(
        media_type,
        CardSource {
            tmdb_id: item.id,
            title: item.title.as_deref(),
            fallback_title: item.name.as_deref(),
            date: item.release_date.as_deref(),
            fallback_date: item.first_air_date.as_deref(),
            overview: item.overview.as_deref(),
            poster_path: item.poster_path.as_deref(),
            backdrop_path: item.backdrop_path.as_deref(),
            media_info: item.media_info.as_ref(),
        },
        images,
    )
}

/// A list endpoint scoped to one known kind -- `media_type` is supplied by
/// the caller since vendored schemas don't reliably carry their own.
pub(crate) fn cards_from_results(
    items: &[models::MediaResult],
    media_type: SeerrMediaType,
    images: ImageContext<'_>,
) -> Vec<SeerrCard> {
    items
        .iter()
        .map(|item| card_from_result(item, media_type, images))
        .collect()
}

/// A mixed-kind list endpoint: each item's own `mediaType` decides Movie
/// vs TV; a Person hit is dropped -- no Person variant to route it into.
pub(crate) fn cards_from_mixed_results(
    items: &[models::MediaResult],
    images: ImageContext<'_>,
) -> Vec<SeerrCard> {
    items
        .iter()
        .filter_map(|item| {
            let media_type = media_type_from_str(item.media_type.as_deref())?;
            Some(card_from_result(item, media_type, images))
        })
        .collect()
}

pub(crate) fn card_from_movie_details(
    details: &models::MovieDetails,
    images: ImageContext<'_>,
) -> SeerrCard {
    build_card(
        SeerrMediaType::Movie,
        CardSource {
            tmdb_id: details.id,
            title: details.title.as_deref(),
            fallback_title: None,
            date: details.release_date.as_deref(),
            fallback_date: None,
            overview: details.overview.as_deref(),
            poster_path: details.poster_path.as_deref(),
            backdrop_path: details.backdrop_path.as_deref(),
            media_info: details.media_info.as_ref(),
        },
        images,
    )
}

pub(crate) fn card_from_tv_details(
    details: &models::TvDetails,
    images: ImageContext<'_>,
) -> SeerrCard {
    build_card(
        SeerrMediaType::Tv,
        CardSource {
            tmdb_id: details.id,
            title: details.name.as_deref(),
            fallback_title: None,
            date: details.first_air_date.as_deref(),
            fallback_date: None,
            overview: details.overview.as_deref(),
            poster_path: details.poster_path.as_deref(),
            backdrop_path: details.backdrop_path.as_deref(),
            media_info: details.media_info.as_ref(),
        },
        images,
    )
}

pub(crate) fn card_from_credit_cast(
    credit: &models::CreditCast,
    media_type: SeerrMediaType,
    images: ImageContext<'_>,
) -> SeerrCard {
    build_card(
        media_type,
        CardSource {
            tmdb_id: credit.id,
            title: credit.title.as_deref(),
            fallback_title: credit.name.as_deref(),
            date: credit.release_date.as_deref(),
            fallback_date: credit.first_air_date.as_deref(),
            overview: credit.overview.as_deref(),
            poster_path: credit.poster_path.as_deref(),
            backdrop_path: credit.backdrop_path.as_deref(),
            media_info: credit.media_info.as_ref(),
        },
        images,
    )
}

pub(crate) fn card_from_credit_crew(
    credit: &models::CreditCrew,
    media_type: SeerrMediaType,
    images: ImageContext<'_>,
) -> SeerrCard {
    build_card(
        media_type,
        CardSource {
            tmdb_id: credit.id,
            title: credit.title.as_deref(),
            fallback_title: credit.name.as_deref(),
            date: credit.release_date.as_deref(),
            fallback_date: credit.first_air_date.as_deref(),
            overview: credit.overview.as_deref(),
            poster_path: credit.poster_path.as_deref(),
            backdrop_path: credit.backdrop_path.as_deref(),
            media_info: credit.media_info.as_ref(),
        },
        images,
    )
}

pub(crate) fn person_ref_from_cast(
    credit: &models::CreditCast,
    images: ImageContext<'_>,
) -> SeerrPersonRef {
    SeerrPersonRef {
        person_id: credit.id,
        name: credit.name.clone().unwrap_or_default(),
        role: credit.character.clone(),
        profile_url: profile_url(credit.profile_path.as_deref(), images),
    }
}

pub(crate) fn trailer_url_from(videos: &[models::RelatedVideo]) -> Option<String> {
    videos
        .iter()
        .find(|v| v.kind.as_deref() == Some("Trailer"))
        .and_then(|v| v.url.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn availability_from_status_maps_every_known_value() {
        assert_eq!(
            availability_from_status(Some(2)),
            SeerrAvailability::Pending
        );
        assert_eq!(
            availability_from_status(Some(3)),
            SeerrAvailability::Processing
        );
        assert_eq!(
            availability_from_status(Some(4)),
            SeerrAvailability::PartiallyAvailable
        );
        assert_eq!(
            availability_from_status(Some(5)),
            SeerrAvailability::Available
        );
    }

    #[test]
    fn availability_from_status_folds_unknown_and_absent_into_not_requested() {
        assert_eq!(
            availability_from_status(None),
            SeerrAvailability::NotRequested,
            "absent mediaInfo/status"
        );
        assert_eq!(
            availability_from_status(Some(1)),
            SeerrAvailability::NotRequested,
            "explicit UNKNOWN"
        );
        assert_eq!(
            availability_from_status(Some(6)),
            SeerrAvailability::NotRequested,
            "DELETED"
        );
        assert_eq!(
            availability_from_status(Some(99)),
            SeerrAvailability::NotRequested,
            "a future/unrecognized value"
        );
    }

    #[test]
    fn season_requestable_matrix() {
        // Not yet available, no request at all -> requestable.
        assert!(season_requestable(SeerrAvailability::NotRequested, None));

        // Already at least pending-availability -> never requestable.
        for availability in [
            SeerrAvailability::Pending,
            SeerrAvailability::Processing,
            SeerrAvailability::PartiallyAvailable,
            SeerrAvailability::Available,
        ] {
            assert!(
                !season_requestable(availability, None),
                "{availability:?} must not be requestable"
            );
        }

        // Already covered by a pending/approved request -> not requestable.
        assert!(!season_requestable(
            SeerrAvailability::NotRequested,
            Some(SeerrRequestStatus::Pending)
        ));
        assert!(!season_requestable(
            SeerrAvailability::NotRequested,
            Some(SeerrRequestStatus::Approved)
        ));

        // A declined request does not block re-requesting.
        assert!(season_requestable(
            SeerrAvailability::NotRequested,
            Some(SeerrRequestStatus::Declined)
        ));
    }

    fn season(number: i32, status: Option<i64>) -> models::Season {
        models::Season {
            season_number: Some(number),
            name: None,
            episode_count: Some(10),
            status,
            status4k: None,
        }
    }

    fn media_request(is4k: bool, seasons: Vec<models::Season>) -> models::MediaRequest {
        models::MediaRequest {
            id: 1,
            status: 0,
            is4k,
            seasons,
            requested_by: None,
            media: None,
            kind: None,
        }
    }

    #[test]
    fn build_season_statuses_marks_a_pending_request_season_not_requestable() {
        let seasons = vec![season(1, None), season(2, None)];
        let media_info = models::MediaInfo {
            requests: vec![media_request(
                false,
                vec![models::Season {
                    status: Some(1), // PENDING (request-level, per MediaRequest.status semantics)
                    ..season(1, None)
                }],
            )],
            ..Default::default()
        };
        let result = build_season_statuses(&seasons, Some(&media_info));
        assert_eq!(result.len(), 2);
        assert!(!result[0].requestable, "season 1 has a pending request");
        assert!(result[1].requestable, "season 2 has no request at all");
    }

    #[test]
    fn build_season_statuses_ignores_4k_requests_for_the_sd_list() {
        let seasons = vec![season(1, None)];
        let media_info = models::MediaInfo {
            requests: vec![media_request(
                true, // a 4K request
                vec![models::Season {
                    status: Some(1),
                    ..season(1, None)
                }],
            )],
            ..Default::default()
        };
        let result = build_season_statuses(&seasons, Some(&media_info));
        assert!(
            result[0].requestable,
            "a 4K request must not block SD requestability"
        );
    }

    #[test]
    fn build_season_statuses_merges_the_higher_ranked_availability() {
        // mediaInfo.seasons AVAILABLE must win over the title's own NotRequested.
        let seasons = vec![season(1, None)];
        let media_info = models::MediaInfo {
            seasons: vec![season(1, Some(5))],
            ..Default::default()
        };
        let result = build_season_statuses(&seasons, Some(&media_info));
        assert_eq!(result[0].availability, SeerrAvailability::Available);
        assert!(!result[0].requestable);
    }

    fn requested_by(id: i64) -> models::RequestUser {
        models::RequestUser { id, username: None }
    }

    #[test]
    fn decide_request_action_creates_when_no_existing_request() {
        let action = decide_request_action(&[], false, 42);
        assert!(matches!(action, RequestAction::Create));
    }

    #[test]
    fn decide_request_action_updates_the_callers_own_pending_request() {
        let existing = vec![models::MediaRequest {
            id: 7,
            status: 1, // pending
            is4k: false,
            seasons: Vec::new(),
            requested_by: Some(requested_by(42)),
            media: None,
            kind: None,
        }];
        let action = decide_request_action(&existing, false, 42);
        assert!(matches!(action, RequestAction::Update { request_id: 7 }));
    }

    #[test]
    fn decide_request_action_ignores_another_users_pending_request() {
        let existing = vec![models::MediaRequest {
            id: 7,
            status: 1,
            is4k: false,
            seasons: Vec::new(),
            requested_by: Some(requested_by(999)),
            media: None,
            kind: None,
        }];
        let action = decide_request_action(&existing, false, 42);
        assert!(matches!(action, RequestAction::Create));
    }

    #[test]
    fn decide_request_action_ignores_a_pending_request_at_the_other_4k_flavor() {
        let existing = vec![models::MediaRequest {
            id: 7,
            status: 1,
            is4k: true, // caller is submitting an SD (is4k: false) request
            seasons: Vec::new(),
            requested_by: Some(requested_by(42)),
            media: None,
            kind: None,
        }];
        let action = decide_request_action(&existing, false, 42);
        assert!(matches!(action, RequestAction::Create));
    }

    #[test]
    fn decide_request_action_ignores_a_declined_request() {
        let existing = vec![models::MediaRequest {
            id: 7,
            status: 3, // declined
            is4k: false,
            seasons: Vec::new(),
            requested_by: Some(requested_by(42)),
            media: None,
            kind: None,
        }];
        let action = decide_request_action(&existing, false, 42);
        assert!(
            matches!(action, RequestAction::Create),
            "a declined request must not be updated -- a fresh request should be created"
        );
    }

    #[test]
    fn request_status_from_int_is_strict() {
        assert_eq!(
            request_status_from_int(1),
            Some(SeerrRequestStatus::Pending)
        );
        assert_eq!(
            request_status_from_int(2),
            Some(SeerrRequestStatus::Approved)
        );
        assert_eq!(
            request_status_from_int(3),
            Some(SeerrRequestStatus::Declined)
        );
        assert_eq!(request_status_from_int(4), None, "FAILURE has no slot");
        assert_eq!(request_status_from_int(5), None, "COMPLETED has no slot");
        assert_eq!(request_status_from_int(0), None);
    }

    #[test]
    fn request_status_for_display_folds_completed_and_failure() {
        assert_eq!(
            request_status_for_display(5),
            SeerrRequestStatus::Approved,
            "COMPLETED displays as Approved"
        );
        assert_eq!(
            request_status_for_display(4),
            SeerrRequestStatus::Declined,
            "FAILURE displays as Declined"
        );
        assert_eq!(request_status_for_display(1), SeerrRequestStatus::Pending);
        assert_eq!(request_status_for_display(2), SeerrRequestStatus::Approved);
        assert_eq!(request_status_for_display(3), SeerrRequestStatus::Declined);
    }

    #[test]
    fn jellyfin_item_id_from_prefers_the_4k_id() {
        let info = models::MediaInfo {
            jellyfin_media_id: Some("sd-id".to_string()),
            jellyfin_media_id4k: Some("4k-id".to_string()),
            ..Default::default()
        };
        assert_eq!(
            jellyfin_item_id_from(Some(&info)),
            Some("4k-id".to_string())
        );
    }

    #[test]
    fn jellyfin_item_id_from_falls_back_to_sd_id() {
        let info = models::MediaInfo {
            jellyfin_media_id: Some("sd-id".to_string()),
            ..Default::default()
        };
        assert_eq!(
            jellyfin_item_id_from(Some(&info)),
            Some("sd-id".to_string())
        );
    }

    #[test]
    fn jellyfin_item_id_from_is_none_without_mediainfo() {
        assert_eq!(jellyfin_item_id_from(None), None);
    }

    #[test]
    fn build_image_url_uses_tmdb_by_default() {
        let images = ImageContext {
            seerr_url: "https://seerr.test",
            cache_images: false,
        };
        assert_eq!(
            poster_url(Some("/abc.jpg"), images),
            Some("https://image.tmdb.org/t/p/w500/abc.jpg".to_string())
        );
        assert_eq!(
            backdrop_url(Some("/abc.jpg"), images),
            Some("https://image.tmdb.org/t/p/w1920_and_h1080_multi_faces/abc.jpg".to_string())
        );
    }

    #[test]
    fn build_image_url_swaps_base_when_cache_images_is_set() {
        let images = ImageContext {
            seerr_url: "https://seerr.test",
            cache_images: true,
        };
        assert_eq!(
            poster_url(Some("/abc.jpg"), images),
            Some("https://seerr.test/imageproxy/tmdb/t/p/w500/abc.jpg".to_string())
        );
    }

    #[test]
    fn build_image_url_is_none_without_a_path() {
        let images = ImageContext {
            seerr_url: "https://seerr.test",
            cache_images: false,
        };
        assert_eq!(poster_url(None, images), None);
        assert_eq!(poster_url(Some(""), images), None);
    }
}
