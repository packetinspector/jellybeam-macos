//! Plain, app-facing Seerr Discover records/enums -- the types
//! [`crate::session::SeerrSession`] returns directly.
//!
//! `SeerrAuthMethod` isn't duplicated here: [`crate::client::SeerrAuthMethod`]
//! already derives what both the client and the app-facing surface need,
//! so it serves both roles directly; see its doc comment.

/// A Seerr title's kind -- movies and TV use different endpoints/season semantics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SeerrMediaType {
    Movie,
    Tv,
}

/// `MediaInfo.status` (1..=5); absent/unrecognized/`DELETED` folds into `NotRequested`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SeerrAvailability {
    NotRequested,
    Pending,
    Processing,
    PartiallyAvailable,
    Available,
}

/// Local-file-only read: whether Discover is configured. `app_title` fills
/// in only when the caller passes an already-open [`crate::session::SeerrSession`]
/// to [`crate::session::status`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeerrStatus {
    pub configured: bool,
    pub seerr_url: Option<String>,
    pub method: Option<crate::client::SeerrAuthMethod>,
    pub identity: Option<String>,
    pub app_title: Option<String>,
}

/// One browse/shelf tile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeerrCard {
    pub media_type: SeerrMediaType,
    pub tmdb_id: i64,
    pub title: String,
    pub year: Option<i32>,
    pub overview: Option<String>,
    pub poster_url: Option<String>,
    pub backdrop_url: Option<String>,
    pub availability: SeerrAvailability,
    /// Set when already in the Jellyfin library; the detail screen's
    /// primary action becomes "Go to library" instead of a request action.
    pub jellyfin_item_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeerrHomeRow {
    pub id: String,
    pub title: String,
    pub cards: Vec<SeerrCard>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeerrHome {
    pub rows: Vec<SeerrHomeRow>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeerrPage {
    pub cards: Vec<SeerrCard>,
    pub page: i64,
    pub total_pages: i64,
    pub total_results: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SeerrBrowseKind {
    Trending,
    Movies,
    Tv,
    UpcomingMovies,
    UpcomingTv,
}

/// Raw query-param passthrough, mirroring [`crate::client::BrowseFilters`]
/// one field at a time; every field is optional, unvalidated. Kept as its
/// own type (rather than reusing `client::BrowseFilters` directly) so
/// `SeerrSession::browse`'s public signature matches the spec's
/// `SeerrBrowseFilters` name and doesn't leak the wire-layer type.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SeerrBrowseFilters {
    pub sort_by: Option<String>,
    pub genre_id: Option<i64>,
    pub min_vote: Option<f64>,
    pub network_id: Option<i64>,
    pub status: Option<String>,
}

impl From<SeerrBrowseFilters> for crate::client::BrowseFilters {
    fn from(filters: SeerrBrowseFilters) -> Self {
        crate::client::BrowseFilters {
            sort_by: filters.sort_by,
            genre_id: filters.genre_id,
            min_vote: filters.min_vote,
            network_id: filters.network_id,
            status: filters.status,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeerrGenre {
    pub id: i64,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeerrPersonRef {
    pub person_id: i64,
    pub name: String,
    pub role: Option<String>,
    pub profile_url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeerrPersonCredits {
    pub name: String,
    pub profile_url: Option<String>,
    pub credits: Vec<SeerrCard>,
}

/// `MediaRequest.status` (1=pending, 2=approved, 3=declined), a direct 1:1.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SeerrRequestStatus {
    Pending,
    Approved,
    Declined,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeerrActiveRequest {
    pub request_id: i64,
    pub status: SeerrRequestStatus,
    pub is_4k: bool,
    /// TV only -- always empty for a movie request.
    pub seasons: Vec<i32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeerrSeasonStatus {
    pub season_number: i32,
    pub name: String,
    pub episode_count: i32,
    pub availability: SeerrAvailability,
    pub requestable: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SeerrMovieDetail {
    pub card: SeerrCard,
    pub runtime_minutes: Option<i32>,
    pub genres: Vec<SeerrGenre>,
    pub cast: Vec<SeerrPersonRef>,
    pub similar: Vec<SeerrCard>,
    pub recommendations: Vec<SeerrCard>,
    pub trailer_url: Option<String>,
    pub critics_score: Option<i32>,
    pub audience_score: Option<i32>,
    pub active_request: Option<SeerrActiveRequest>,
    pub can_request: bool,
    pub can_request_4k: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SeerrTvDetail {
    pub card: SeerrCard,
    pub genres: Vec<SeerrGenre>,
    pub cast: Vec<SeerrPersonRef>,
    pub similar: Vec<SeerrCard>,
    pub recommendations: Vec<SeerrCard>,
    pub trailer_url: Option<String>,
    pub critics_score: Option<i32>,
    pub audience_score: Option<i32>,
    pub active_request: Option<SeerrActiveRequest>,
    pub can_request: bool,
    pub can_request_4k: bool,
    pub seasons: Vec<SeerrSeasonStatus>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeerrProfile {
    pub id: i64,
    pub name: String,
    pub is_default: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeerrRootFolder {
    pub id: i64,
    pub path: String,
    pub is_default: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeerrServiceServer {
    pub server_id: i64,
    pub name: String,
    pub is_4k: bool,
    pub is_default: bool,
    pub profiles: Vec<SeerrProfile>,
    pub root_folders: Vec<SeerrRootFolder>,
}

/// Empty `servers` means a plain Request button (no profile/root-folder pickers).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeerrRequestOptions {
    pub servers: Vec<SeerrServiceServer>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeerrRequestInput {
    pub media_type: SeerrMediaType,
    pub tmdb_id: i64,
    pub is_4k: bool,
    /// TV only -- ignored (must be empty) for a movie request.
    pub seasons: Vec<i32>,
    pub server_id: Option<i64>,
    pub profile_id: Option<i64>,
    pub root_folder: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeerrMyRequest {
    pub card: SeerrCard,
    pub status: SeerrRequestStatus,
    pub is_4k: bool,
    pub seasons: Vec<i32>,
    pub requested_by: Option<String>,
}
