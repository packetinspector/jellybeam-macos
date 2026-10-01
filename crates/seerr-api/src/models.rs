//! Hand-written wire DTOs for the ~20 Seerr endpoints this crate calls, not generated from the
//! the Seerr API; every field defaults, so drift degrades gracefully.

use serde::{Deserialize, Serialize};

/// `#/components/schemas/User` (subset); returned by the login endpoints and `/auth/me`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct User {
    #[serde(default)]
    pub id: i64,
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub email: Option<String>,
}

/// `#/components/schemas/PublicSettings` (subset), `GET /settings/public`.
/// `partial_requests_enabled`/`application_title` aren't in the vendored spec but are kept.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PublicSettings {
    #[serde(default)]
    pub movie4k_enabled: bool,
    #[serde(default)]
    pub series4k_enabled: bool,
    #[serde(default)]
    pub cache_images: bool,
    #[serde(default)]
    pub partial_requests_enabled: bool,
    #[serde(default)]
    pub application_title: Option<String>,
}

/// `#/components/schemas/Genre` (subset).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Genre {
    #[serde(default)]
    pub id: i64,
    #[serde(default)]
    pub name: Option<String>,
}

/// `#/components/schemas/Season` (subset): as a title's season list, `status`/`status4k` is
/// availability; nested in [`MediaRequest::seasons`] it's the per-season request status
/// (`status4k` unused there; [`MediaRequest::is4k`] covers 4k).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Season {
    #[serde(default)]
    pub season_number: Option<i32>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub episode_count: Option<i32>,
    #[serde(default)]
    pub status: Option<i64>,
    #[serde(default)]
    pub status4k: Option<i64>,
}

/// `#/components/schemas/User` embedded in `MediaRequest.requestedBy`;
/// `id` drives POST-vs-PUT (see `logic::decide_request_action`), `username` is display-only.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct RequestUser {
    #[serde(default)]
    pub id: i64,
    #[serde(default)]
    pub username: Option<String>,
}

/// `#/components/schemas/MediaRequest` (subset). `status`: `1 = PENDING
/// APPROVAL, 2 = APPROVED, 3 = DECLINED`, mapped 1:1 to `SeerrRequestStatus`
/// (unlike [`MediaInfo::status`]'s six-value
/// scale). `media`/`kind` are populated only on `GET /request`, used by
/// `seerr_my_requests` to look up each request's title/poster.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MediaRequest {
    #[serde(default)]
    pub id: i64,
    #[serde(default)]
    pub status: i64,
    #[serde(default)]
    pub is4k: bool,
    #[serde(default)]
    pub seasons: Vec<Season>,
    #[serde(default)]
    pub requested_by: Option<RequestUser>,
    #[serde(default)]
    pub media: Option<MediaInfo>,
    /// The spec's `type` property (`"movie"`/`"tv"`); renamed since `type` is a Rust keyword.
    #[serde(default, rename = "type")]
    pub kind: Option<String>,
}

/// `#/components/schemas/MediaInfo` (subset). `status`: `1 = UNKNOWN, 2 =
/// PENDING, 3 = PROCESSING, 4 = PARTIALLY_AVAILABLE, 5 = AVAILABLE, 6 =
/// DELETED`; absent, `None`, and `Some(1)` are equivalent -- see `availability_from_status`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MediaInfo {
    #[serde(default)]
    pub tmdb_id: Option<i64>,
    #[serde(default)]
    pub status: Option<i64>,
    #[serde(default)]
    pub seasons: Vec<Season>,
    #[serde(default)]
    pub requests: Vec<MediaRequest>,
    #[serde(default)]
    pub jellyfin_media_id: Option<String>,
    #[serde(default)]
    pub jellyfin_media_id4k: Option<String>,
}

/// One page of results; the shape `/search`, `/discover/*`, and the
/// similar|recommendations endpoints all share.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResultPage<T> {
    #[serde(default)]
    pub page: i64,
    #[serde(default)]
    pub total_pages: i64,
    #[serde(default)]
    pub total_results: i64,
    #[serde(default)]
    pub results: Vec<T>,
}

/// Flattened shape covering `MovieResult`, `TvResult`, and `PersonResult`
/// (`/search`/`/discover/trending` return a mixed `anyOf`); `media_type`
/// distinguishes them, `title` is a movie's name, `name` a TV show's or
/// person's. `PersonResult`'s spec omits `name` (likely a gap) but it's kept here.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MediaResult {
    #[serde(default)]
    pub id: i64,
    #[serde(default)]
    pub media_type: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub overview: Option<String>,
    #[serde(default)]
    pub poster_path: Option<String>,
    #[serde(default)]
    pub profile_path: Option<String>,
    #[serde(default)]
    pub backdrop_path: Option<String>,
    #[serde(default)]
    pub release_date: Option<String>,
    #[serde(default)]
    pub first_air_date: Option<String>,
    #[serde(default)]
    pub media_info: Option<MediaInfo>,
}

/// `#/components/schemas/RelatedVideo` (subset); only `kind == "Trailer"` entries are consumed.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct RelatedVideo {
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    /// The spec's `type` property (`"Trailer"`, `"Teaser"`, ...); renamed since `type` is a Rust
    /// keyword.
    #[serde(default, rename = "type")]
    pub kind: Option<String>,
}

/// `#/components/schemas/CreditCast` (subset).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreditCast {
    #[serde(default)]
    pub id: i64,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub character: Option<String>,
    #[serde(default)]
    pub profile_path: Option<String>,
    #[serde(default)]
    pub media_type: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub overview: Option<String>,
    #[serde(default)]
    pub poster_path: Option<String>,
    #[serde(default)]
    pub backdrop_path: Option<String>,
    #[serde(default)]
    pub release_date: Option<String>,
    #[serde(default)]
    pub first_air_date: Option<String>,
    #[serde(default)]
    pub media_info: Option<MediaInfo>,
}

/// `#/components/schemas/CreditCrew` (subset).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreditCrew {
    #[serde(default)]
    pub id: i64,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub job: Option<String>,
    #[serde(default)]
    pub department: Option<String>,
    #[serde(default)]
    pub profile_path: Option<String>,
    #[serde(default)]
    pub media_type: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub overview: Option<String>,
    #[serde(default)]
    pub poster_path: Option<String>,
    #[serde(default)]
    pub backdrop_path: Option<String>,
    #[serde(default)]
    pub release_date: Option<String>,
    #[serde(default)]
    pub first_air_date: Option<String>,
    #[serde(default)]
    pub media_info: Option<MediaInfo>,
}

/// `MovieDetails.credits` / `TvDetails.credits` (subset).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Credits {
    #[serde(default)]
    pub cast: Vec<CreditCast>,
    #[serde(default)]
    pub crew: Vec<CreditCrew>,
}

/// `#/components/schemas/MovieDetails` (subset), `GET /movie/{id}`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MovieDetails {
    #[serde(default)]
    pub id: i64,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub overview: Option<String>,
    #[serde(default)]
    pub poster_path: Option<String>,
    #[serde(default)]
    pub backdrop_path: Option<String>,
    #[serde(default)]
    pub release_date: Option<String>,
    #[serde(default)]
    pub runtime: Option<f64>,
    #[serde(default)]
    pub genres: Vec<Genre>,
    #[serde(default)]
    pub related_videos: Vec<RelatedVideo>,
    #[serde(default)]
    pub credits: Option<Credits>,
    #[serde(default)]
    pub media_info: Option<MediaInfo>,
}

/// `#/components/schemas/TvDetails` (subset), `GET /tv/{id}`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TvDetails {
    #[serde(default)]
    pub id: i64,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub overview: Option<String>,
    #[serde(default)]
    pub poster_path: Option<String>,
    #[serde(default)]
    pub backdrop_path: Option<String>,
    #[serde(default)]
    pub first_air_date: Option<String>,
    #[serde(default)]
    pub episode_run_time: Vec<i32>,
    #[serde(default)]
    pub genres: Vec<Genre>,
    #[serde(default)]
    pub seasons: Vec<Season>,
    #[serde(default)]
    pub related_videos: Vec<RelatedVideo>,
    #[serde(default)]
    pub credits: Option<Credits>,
    #[serde(default)]
    pub media_info: Option<MediaInfo>,
}

/// `#/components/schemas/PersonDetails` (subset), `GET /person/{id}`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PersonDetails {
    #[serde(default)]
    pub id: i64,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub profile_path: Option<String>,
    #[serde(default)]
    pub biography: Option<String>,
}

/// `GET /person/{id}/combined_credits` response shape.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct CombinedCredits {
    #[serde(default)]
    pub cast: Vec<CreditCast>,
    #[serde(default)]
    pub crew: Vec<CreditCrew>,
}

/// `GET /movie|tv/{id}/ratings` response (TV never populates `audience_score`);
/// a 404 means "no score," returned as `Ok(None)`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Ratings {
    #[serde(default)]
    pub critics_score: Option<i64>,
    #[serde(default)]
    pub audience_score: Option<i64>,
}

/// `RadarrSettings`/`SonarrSettings` (subset); one shape covers both `GET /service/radarr` and `GET
/// /service/sonarr`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServarrInstance {
    #[serde(default)]
    pub id: i64,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub is4k: bool,
    #[serde(default)]
    pub is_default: bool,
    #[serde(default)]
    pub active_profile_id: Option<i64>,
    #[serde(default)]
    pub active_directory: Option<String>,
}

/// `#/components/schemas/ServiceProfile` (subset).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ServiceProfile {
    #[serde(default)]
    pub id: i64,
    #[serde(default)]
    pub name: Option<String>,
}

/// `#/components/schemas/RootFolder` (subset).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RootFolder {
    #[serde(default)]
    pub id: i64,
    #[serde(default)]
    pub path: Option<String>,
}

/// `GET /service/radarr|sonarr/{id}` response; `server.active_profile_id`/`active_directory` mark
/// the current default profile/root folder.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServiceDetail {
    #[serde(default)]
    pub server: Option<ServarrInstance>,
    #[serde(default)]
    pub profiles: Vec<ServiceProfile>,
    #[serde(default)]
    pub root_folders: Vec<RootFolder>,
}

/// `#/components/schemas/PageInfo` (subset), embedded in `GET /request`'s response.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct PageInfo {
    #[serde(default)]
    pub page: i64,
    #[serde(default)]
    pub pages: i64,
    #[serde(default)]
    pub results: i64,
}

/// `GET /request` response shape (`seerr_my_requests`).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RequestsPage {
    #[serde(default)]
    pub page_info: PageInfo,
    #[serde(default)]
    pub results: Vec<MediaRequest>,
}

/// `POST /request` request body (subset of the spec's writable fields).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RequestCreateBody {
    pub media_type: &'static str,
    pub media_id: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seasons: Option<Vec<i32>>,
    pub is4k: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub root_folder: Option<String>,
}

/// `PUT /request/{id}` request body (subset of the spec's writable fields).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RequestUpdateBody {
    pub media_type: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seasons: Option<Vec<i32>>,
    pub is4k: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub root_folder: Option<String>,
}
