//! The live per-account Seerr connection ([`SeerrSession`]) and the free
//! functions around it (`status`/`connect`/`open`/`disconnect`). Two
//! deliberate design choices:
//!
//! - **Async, not blocking.** The app runs on gpui's
//!   own async executor, so [`SeerrSession`]'s methods are plain `async fn`s.
//! - **No ambient session state.** There is no shared session object here,
//!   so every entry point below takes `data_dir` and `(server_url,
//!   user_id)` explicitly, and caching a built [`SeerrSession`] across
//!   calls is left to the app.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_util::StreamExt;

use crate::client::{SeerrAuthMethod, SeerrClient};
use crate::error::SeerrError;
use crate::logic::{
    active_request_for, build_season_statuses, card_from_credit_cast, card_from_credit_crew,
    card_from_movie_details, card_from_tv_details, cards_from_mixed_results, cards_from_results,
    decide_request_action, media_type_from_str, movie_can_request, person_ref_from_cast,
    request_status_for_display, trailer_url_from, tv_can_request, ImageContext, RequestAction,
};
use crate::models;
use crate::store::{SeerrConfigEntry, SeerrConfigStore};
use crate::types::*;
use crate::url as seerr_url;

// --- Timeouts ----------------------------------------------------------------

/// Tight timeouts for [`connect`]'s candidate-URL probing -- a bad
/// candidate must fail fast so trying 2-4 candidates still feels instant.
const PROBE_CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
const PROBE_REQUEST_TIMEOUT: Duration = Duration::from_secs(6);

/// Normal timeouts for every other Seerr call, same magnitude as
/// `jellyfin-api`'s own connect/request timeouts.
const NORMAL_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const NORMAL_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// How long a [`SeerrSession`]'s home-snapshot cache stays fresh (docs/14
/// Performance: "60s in-core snapshot cache per account").
const HOME_CACHE_TTL: Duration = Duration::from_secs(60);

/// Cap on in-flight detail fetches for per-request/per-instance lookups --
/// bounds fan-out on a typically-small Seerr instance.
const MAX_CONCURRENT_DETAIL_FETCHES: usize = 3;

/// Errors from the account-level entry points ([`connect`]/[`open`]) that
/// have no equivalent on a plain [`SeerrClient`] call -- "there's nothing
/// saved for this account" and "the URL you typed doesn't parse". Every
/// per-call [`SeerrSession`] method below returns [`SeerrError`] directly
/// instead, since once a session is open every failure is a client-level one.
#[derive(Debug, thiserror::Error)]
pub enum SeerrSessionError {
    /// No saved Seerr connection for this `(server_url, user_id)`.
    #[error("no Seerr server configured for this account")]
    NotConfigured,
    #[error("enter a Seerr server address")]
    InvalidUrl,
    #[error(transparent)]
    Client(#[from] SeerrError),
    #[error("failed to persist seerr config: {0}")]
    Persist(String),
}

#[derive(Default)]
struct GenreCache {
    movie: Option<Vec<SeerrGenre>>,
    tv: Option<Vec<SeerrGenre>>,
}

struct SeerrSessionInner {
    client: SeerrClient,
    /// Seerr's own numeric user id (from the login response) -- distinct
    /// from the Jellyfin `user_id` half of the config store's lookup key
    /// (`SeerrConfigEntry::user_id`). Named distinctly here to keep the two
    /// apart.
    seerr_user_id: i64,
    /// The plain Seerr server URL (no `/api/v1`), used as the base for
    /// `{seerr_url}/imageproxy/tmdb` -- see [`strip_api_v1`].
    public_url: String,
    movie4k_enabled: bool,
    series4k_enabled: bool,
    cache_images: bool,
    application_title: Option<String>,
    home_cache: Mutex<Option<(Instant, SeerrHome)>>,
    genre_cache: Mutex<GenreCache>,
}

/// The active account's live Seerr connection: an auth handle plus the 60s
/// home-snapshot cache and process-lifetime genre cache
/// (kept alive for the session). Cheap to clone (`Arc`-backed),
/// like [`SeerrClient`] itself.
#[derive(Clone)]
pub struct SeerrSession {
    inner: Arc<SeerrSessionInner>,
}

impl SeerrSession {
    fn new(
        client: SeerrClient,
        seerr_user_id: i64,
        api_base_url: &str,
        public: models::PublicSettings,
    ) -> Self {
        SeerrSession {
            inner: Arc::new(SeerrSessionInner {
                client,
                seerr_user_id,
                public_url: strip_api_v1(api_base_url),
                movie4k_enabled: public.movie4k_enabled,
                series4k_enabled: public.series4k_enabled,
                cache_images: public.cache_images,
                application_title: public.application_title,
                home_cache: Mutex::new(None),
                genre_cache: Mutex::new(GenreCache::default()),
            }),
        }
    }

    fn client(&self) -> &SeerrClient {
        &self.inner.client
    }

    fn user_id(&self) -> i64 {
        self.inner.seerr_user_id
    }

    fn images(&self) -> ImageContext<'_> {
        ImageContext {
            seerr_url: &self.inner.public_url,
            cache_images: self.inner.cache_images,
        }
    }

    fn movie4k_enabled(&self) -> bool {
        self.inner.movie4k_enabled
    }

    fn series4k_enabled(&self) -> bool {
        self.inner.series4k_enabled
    }

    /// The server's configured title (from `/settings/public`), if this
    /// session already fetched it -- used to enrich [`status`]'s otherwise
    /// purely local-file read.
    pub fn app_title(&self) -> Option<String> {
        self.inner.application_title.clone()
    }

    fn cached_home(&self) -> Option<SeerrHome> {
        let guard = self
            .inner
            .home_cache
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        guard.as_ref().and_then(|(at, home)| {
            if at.elapsed() < HOME_CACHE_TTL {
                Some(home.clone())
            } else {
                None
            }
        })
    }

    fn cache_home(&self, home: SeerrHome) {
        *self
            .inner
            .home_cache
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some((Instant::now(), home));
    }

    /// Drops the cached home snapshot after a submit/cancel, so a stale
    /// availability badge doesn't sit for up to [`HOME_CACHE_TTL`].
    fn invalidate_home(&self) {
        *self
            .inner
            .home_cache
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = None;
    }

    fn cached_genres(&self, media_type: SeerrMediaType) -> Option<Vec<SeerrGenre>> {
        let guard = self
            .inner
            .genre_cache
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        match media_type {
            SeerrMediaType::Movie => guard.movie.clone(),
            SeerrMediaType::Tv => guard.tv.clone(),
        }
    }

    fn cache_genres(&self, media_type: SeerrMediaType, genres: Vec<SeerrGenre>) {
        let mut guard = self
            .inner
            .genre_cache
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        match media_type {
            SeerrMediaType::Movie => guard.movie = Some(genres),
            SeerrMediaType::Tv => guard.tv = Some(genres),
        }
    }

    // --- Public async API ----------------------------------------------------

    /// Trending/Movies/TV/Upcoming rows, fetched concurrently; a failed row
    /// is dropped (fail open). Cached 60s.
    pub async fn home(&self) -> SeerrHome {
        if let Some(cached) = self.cached_home() {
            return cached;
        }
        let client = self.client();
        let no_filters = crate::client::BrowseFilters::default();
        let (trending, movies, tv, upcoming_movies, upcoming_tv) = tokio::join!(
            client.discover_trending(1),
            client.discover_movies(1, &no_filters),
            client.discover_tv(1, &no_filters),
            client.discover_movies_upcoming(1),
            client.discover_tv_upcoming(1),
        );
        let images = self.images();
        let mut rows = Vec::new();
        if let Ok(page) = trending {
            rows.push(SeerrHomeRow {
                id: "trending".to_string(),
                title: "Trending".to_string(),
                cards: cards_from_mixed_results(&page.results, images),
            });
        }
        if let Ok(page) = movies {
            rows.push(SeerrHomeRow {
                id: "movies".to_string(),
                title: "Movies".to_string(),
                cards: cards_from_results(&page.results, SeerrMediaType::Movie, images),
            });
        }
        if let Ok(page) = tv {
            rows.push(SeerrHomeRow {
                id: "tv".to_string(),
                title: "TV".to_string(),
                cards: cards_from_results(&page.results, SeerrMediaType::Tv, images),
            });
        }
        if let Ok(page) = upcoming_movies {
            rows.push(SeerrHomeRow {
                id: "upcoming_movies".to_string(),
                title: "Upcoming Movies".to_string(),
                cards: cards_from_results(&page.results, SeerrMediaType::Movie, images),
            });
        }
        if let Ok(page) = upcoming_tv {
            rows.push(SeerrHomeRow {
                id: "upcoming_tv".to_string(),
                title: "Upcoming TV".to_string(),
                cards: cards_from_results(&page.results, SeerrMediaType::Tv, images),
            });
        }
        let home = SeerrHome { rows };
        self.cache_home(home.clone());
        home
    }

    pub async fn browse(
        &self,
        kind: SeerrBrowseKind,
        page: i64,
        filters: SeerrBrowseFilters,
    ) -> Result<SeerrPage, SeerrError> {
        let client = self.client();
        let images = self.images();
        let seerr_filters: crate::client::BrowseFilters = filters.into();

        let (cards, page_no, total_pages, total_results) = match kind {
            SeerrBrowseKind::Trending => {
                let result = client.discover_trending(page).await?;
                (
                    cards_from_mixed_results(&result.results, images),
                    result.page,
                    result.total_pages,
                    result.total_results,
                )
            }
            SeerrBrowseKind::Movies => {
                let result = client.discover_movies(page, &seerr_filters).await?;
                (
                    cards_from_results(&result.results, SeerrMediaType::Movie, images),
                    result.page,
                    result.total_pages,
                    result.total_results,
                )
            }
            SeerrBrowseKind::Tv => {
                let result = client.discover_tv(page, &seerr_filters).await?;
                (
                    cards_from_results(&result.results, SeerrMediaType::Tv, images),
                    result.page,
                    result.total_pages,
                    result.total_results,
                )
            }
            SeerrBrowseKind::UpcomingMovies => {
                let result = client.discover_movies_upcoming(page).await?;
                (
                    cards_from_results(&result.results, SeerrMediaType::Movie, images),
                    result.page,
                    result.total_pages,
                    result.total_results,
                )
            }
            SeerrBrowseKind::UpcomingTv => {
                let result = client.discover_tv_upcoming(page).await?;
                (
                    cards_from_results(&result.results, SeerrMediaType::Tv, images),
                    result.page,
                    result.total_pages,
                    result.total_results,
                )
            }
        };

        Ok(SeerrPage {
            cards,
            page: page_no,
            total_pages,
            total_results,
        })
    }

    /// Process-lifetime cache -- genre lists never change within one run.
    pub async fn genres(&self, media_type: SeerrMediaType) -> Result<Vec<SeerrGenre>, SeerrError> {
        if let Some(cached) = self.cached_genres(media_type) {
            return Ok(cached);
        }
        let client = self.client();
        let raw = if matches!(media_type, SeerrMediaType::Movie) {
            client.genres_movie().await
        } else {
            client.genres_tv().await
        }?;
        let genres: Vec<SeerrGenre> = raw
            .into_iter()
            .map(|g| SeerrGenre {
                id: g.id,
                name: g.name.unwrap_or_default(),
            })
            .collect();
        self.cache_genres(media_type, genres.clone());
        Ok(genres)
    }

    pub async fn search(&self, query: &str, page: i64) -> Result<SeerrPage, SeerrError> {
        let result = self.client().search(query, page).await?;
        let images = self.images();
        Ok(SeerrPage {
            cards: cards_from_mixed_results(&result.results, images),
            page: result.page,
            total_pages: result.total_pages,
            total_results: result.total_results,
        })
    }

    /// Detail + similar + recommendations + ratings fetched concurrently;
    /// only the primary fetch is load-bearing, the rest fail open. Ratings
    /// 404s map to `None`, never an error.
    pub async fn movie(&self, tmdb_id: i64) -> Result<SeerrMovieDetail, SeerrError> {
        let client = self.client();
        let (details, similar, recommendations, ratings) = tokio::join!(
            client.movie_details(tmdb_id),
            client.movie_similar(tmdb_id, 1),
            client.movie_recommendations(tmdb_id, 1),
            client.movie_ratings(tmdb_id),
        );
        let details = details?;
        let images = self.images();
        let card = card_from_movie_details(&details, images);
        let (critics_score, audience_score) = ratings
            .ok()
            .flatten()
            .map(|r| {
                (
                    r.critics_score.and_then(|v| i32::try_from(v).ok()),
                    r.audience_score.and_then(|v| i32::try_from(v).ok()),
                )
            })
            .unwrap_or((None, None));
        let can_request = movie_can_request(card.availability);

        Ok(SeerrMovieDetail {
            runtime_minutes: details.runtime.map(|r| r.round() as i32),
            genres: details
                .genres
                .iter()
                .map(|g| SeerrGenre {
                    id: g.id,
                    name: g.name.clone().unwrap_or_default(),
                })
                .collect(),
            cast: details
                .credits
                .as_ref()
                .map(|c| {
                    c.cast
                        .iter()
                        .map(|c| person_ref_from_cast(c, images))
                        .collect()
                })
                .unwrap_or_default(),
            similar: similar
                .map(|p| cards_from_results(&p.results, SeerrMediaType::Movie, images))
                .unwrap_or_default(),
            recommendations: recommendations
                .map(|p| cards_from_results(&p.results, SeerrMediaType::Movie, images))
                .unwrap_or_default(),
            trailer_url: trailer_url_from(&details.related_videos),
            critics_score,
            audience_score,
            active_request: active_request_for(details.media_info.as_ref(), self.user_id()),
            can_request,
            can_request_4k: can_request && self.movie4k_enabled(),
            card,
        })
    }

    /// Same concurrency/fail-open shape as [`Self::movie`]. `seasons`
    /// reflects SD availability only; `can_request_4k` reuses the SD
    /// requestability signal, gated by the server's own `series4kEnabled`
    /// toggle -- the spec exposes exactly one (SD) season list.
    pub async fn tv(&self, tmdb_id: i64) -> Result<SeerrTvDetail, SeerrError> {
        let client = self.client();
        let (details, similar, recommendations, ratings) = tokio::join!(
            client.tv_details(tmdb_id),
            client.tv_similar(tmdb_id, 1),
            client.tv_recommendations(tmdb_id, 1),
            client.tv_ratings(tmdb_id),
        );
        let details = details?;
        let images = self.images();
        let card = card_from_tv_details(&details, images);
        let seasons = build_season_statuses(&details.seasons, details.media_info.as_ref());
        let can_request = tv_can_request(&seasons);
        let (critics_score, audience_score) = ratings
            .ok()
            .flatten()
            .map(|r| {
                (
                    r.critics_score.and_then(|v| i32::try_from(v).ok()),
                    r.audience_score.and_then(|v| i32::try_from(v).ok()),
                )
            })
            .unwrap_or((None, None));

        Ok(SeerrTvDetail {
            genres: details
                .genres
                .iter()
                .map(|g| SeerrGenre {
                    id: g.id,
                    name: g.name.clone().unwrap_or_default(),
                })
                .collect(),
            cast: details
                .credits
                .as_ref()
                .map(|c| {
                    c.cast
                        .iter()
                        .map(|c| person_ref_from_cast(c, images))
                        .collect()
                })
                .unwrap_or_default(),
            similar: similar
                .map(|p| cards_from_results(&p.results, SeerrMediaType::Tv, images))
                .unwrap_or_default(),
            recommendations: recommendations
                .map(|p| cards_from_results(&p.results, SeerrMediaType::Tv, images))
                .unwrap_or_default(),
            trailer_url: trailer_url_from(&details.related_videos),
            critics_score,
            audience_score,
            active_request: active_request_for(details.media_info.as_ref(), self.user_id()),
            can_request,
            can_request_4k: can_request && self.series4k_enabled(),
            seasons,
            card,
        })
    }

    /// Cast + crew, each capped at 25 (docs/14's contract section).
    pub async fn person(&self, person_id: i64) -> Result<SeerrPersonCredits, SeerrError> {
        let client = self.client();
        let (details, credits) = tokio::join!(
            client.person_details(person_id),
            client.person_combined_credits(person_id),
        );
        let details = details?;
        let credits = credits.unwrap_or_default();
        let images = self.images();

        let mut combined = Vec::new();
        for credit in credits.cast.iter().take(25) {
            if let Some(media_type) = media_type_from_str(credit.media_type.as_deref()) {
                combined.push(card_from_credit_cast(credit, media_type, images));
            }
        }
        for credit in credits.crew.iter().take(25) {
            if let Some(media_type) = media_type_from_str(credit.media_type.as_deref()) {
                combined.push(card_from_credit_crew(credit, media_type, images));
            }
        }

        Ok(SeerrPersonCredits {
            name: details.name.unwrap_or_default(),
            profile_url: crate::logic::profile_url(details.profile_path.as_deref(), images),
            credits: combined,
        })
    }

    /// Radarr/Sonarr instances at the requested 4K flavor; empty `servers`
    /// means a plain Request button.
    pub async fn request_options(
        &self,
        media_type: SeerrMediaType,
        is_4k: bool,
    ) -> Result<SeerrRequestOptions, SeerrError> {
        let client = self.client().clone();
        let instances: Vec<models::ServarrInstance> =
            if matches!(media_type, SeerrMediaType::Movie) {
                client.service_radarr_list().await
            } else {
                client.service_sonarr_list().await
            }?
            .into_iter()
            .filter(|i| i.is4k == is_4k)
            .collect();

        // Fetch every instance's profiles/root folders concurrently;
        // `join_all` preserves `instances`' order so the `zip` below pairs
        // each instance with its own detail fetch.
        let details: Vec<models::ServiceDetail> =
            futures_util::future::join_all(instances.iter().map(|instance| {
                let client = client.clone();
                let id = instance.id;
                async move {
                    if matches!(media_type, SeerrMediaType::Movie) {
                        client.service_radarr_detail(id).await
                    } else {
                        client.service_sonarr_detail(id).await
                    }
                    .unwrap_or_default()
                }
            }))
            .await;

        let mut servers = Vec::new();
        for (instance, detail) in instances.into_iter().zip(details) {
            let active_profile_id = detail.server.as_ref().and_then(|s| s.active_profile_id);
            let active_directory = detail
                .server
                .as_ref()
                .and_then(|s| s.active_directory.clone());

            servers.push(SeerrServiceServer {
                server_id: instance.id,
                name: instance.name.unwrap_or_default(),
                is_4k: instance.is4k,
                is_default: instance.is_default,
                profiles: detail
                    .profiles
                    .into_iter()
                    .map(|p| SeerrProfile {
                        is_default: active_profile_id == Some(p.id),
                        id: p.id,
                        name: p.name.unwrap_or_default(),
                    })
                    .collect(),
                root_folders: detail
                    .root_folders
                    .into_iter()
                    .map(|f| {
                        let path = f.path.unwrap_or_default();
                        SeerrRootFolder {
                            is_default: active_directory.as_deref() == Some(path.as_str()),
                            id: f.id,
                            path,
                        }
                    })
                    .collect(),
            });
        }
        Ok(SeerrRequestOptions { servers })
    }

    /// Movie: single request. TV: per-season, only seasons the caller
    /// marked `requestable`. Updates (`PUT`) an existing PENDING request
    /// instead of duplicating.
    pub async fn submit_request(&self, input: SeerrRequestInput) -> Result<(), SeerrError> {
        let client = self.client();
        let media_type_str: &'static str = match input.media_type {
            SeerrMediaType::Movie => "movie",
            SeerrMediaType::Tv => "tv",
        };
        let seasons = if input.seasons.is_empty() {
            None
        } else {
            Some(input.seasons.clone())
        };

        let existing_requests = match input.media_type {
            SeerrMediaType::Movie => client
                .movie_details(input.tmdb_id)
                .await
                .ok()
                .and_then(|d| d.media_info)
                .map(|m| m.requests)
                .unwrap_or_default(),
            SeerrMediaType::Tv => client
                .tv_details(input.tmdb_id)
                .await
                .ok()
                .and_then(|d| d.media_info)
                .map(|m| m.requests)
                .unwrap_or_default(),
        };
        let action = decide_request_action(&existing_requests, input.is_4k, self.user_id());

        match action {
            RequestAction::Update { request_id } => {
                client
                    .request_update(
                        request_id,
                        crate::models::RequestUpdateBody {
                            media_type: media_type_str,
                            seasons,
                            is4k: input.is_4k,
                            server_id: input.server_id,
                            profile_id: input.profile_id,
                            root_folder: input.root_folder,
                        },
                    )
                    .await?;
            }
            RequestAction::Create => {
                client
                    .request_create(crate::models::RequestCreateBody {
                        media_type: media_type_str,
                        media_id: input.tmdb_id,
                        seasons,
                        is4k: input.is_4k,
                        server_id: input.server_id,
                        profile_id: input.profile_id,
                        root_folder: input.root_folder,
                    })
                    .await?;
            }
        }
        // The submitted item's availability badge just changed; a stale
        // 60s-cached home snapshot would keep showing its old state.
        self.invalidate_home();
        Ok(())
    }

    pub async fn cancel_request(&self, request_id: i64) -> Result<(), SeerrError> {
        self.client().request_delete(request_id).await?;
        self.invalidate_home();
        Ok(())
    }

    /// The account's own requests, scoped by `requestedBy=<seerr_user_id>` --
    /// `GET /request` is otherwise unscoped for `ADMIN`/`MANAGE_REQUESTS`
    /// accounts and would return every user's requests. `seerr_user_id` is
    /// captured at login for every auth method (JELLYFIN/LOCAL from the
    /// login response, API_KEY from a `GET /auth/me` -- see
    /// `SeerrClient::login`), so it's always available here. One request
    /// this app can't resolve a title for is omitted, same fail-open as
    /// [`Self::home`].
    pub async fn my_requests(&self) -> Result<Vec<SeerrMyRequest>, SeerrError> {
        let client = self.client().clone();
        let page = client.my_requests(self.user_id(), 50, 0).await?;
        let images = self.images();

        // Each request's title/poster needs its own detail fetch, up to 50,
        // bounded via `StreamExt::buffered` (preserves server order).
        let resolved: Vec<Option<SeerrMyRequest>> =
            futures_util::stream::iter(page.results.into_iter().map(|request| {
                let client = client.clone();
                async move {
                    let tmdb_id = request.media.as_ref().and_then(|m| m.tmdb_id);
                    let card = match (tmdb_id, request.kind.as_deref()) {
                        (Some(id), Some("movie")) => client
                            .movie_details(id)
                            .await
                            .ok()
                            .map(|d| card_from_movie_details(&d, images)),
                        (Some(id), Some("tv")) => client
                            .tv_details(id)
                            .await
                            .ok()
                            .map(|d| card_from_tv_details(&d, images)),
                        _ => None,
                    };
                    card.map(|card| SeerrMyRequest {
                        card,
                        status: request_status_for_display(request.status),
                        is_4k: request.is4k,
                        seasons: request
                            .seasons
                            .iter()
                            .filter_map(|s| s.season_number)
                            .collect(),
                        requested_by: request.requested_by.and_then(|u| u.username),
                    })
                }
            }))
            .buffered(MAX_CONCURRENT_DETAIL_FETCHES)
            .collect()
            .await;

        // Fail-open per item: a request this app couldn't resolve a
        // title for is dropped rather than failing the whole call.
        Ok(resolved.into_iter().flatten().collect())
    }
}

/// Strip a normalized API base's trailing `/api/v1` back off, for
/// `{seerr_url}/imageproxy/tmdb`. A no-op if the suffix is absent.
fn strip_api_v1(base: &str) -> String {
    base.strip_suffix("/api/v1").unwrap_or(base).to_string()
}

async fn build_session(entry: &SeerrConfigEntry) -> Result<SeerrSession, SeerrSessionError> {
    let (client, user) = SeerrClient::login(
        &entry.seerr_url,
        entry.method,
        &entry.identity,
        &entry.secret,
        NORMAL_CONNECT_TIMEOUT,
        NORMAL_REQUEST_TIMEOUT,
    )
    .await?;
    // Public settings are a UI-gating nicety: a fetch failure degrades to
    // "every 4K/caching toggle off" rather than failing the connection.
    let public = client.public_settings().await.unwrap_or_default();
    Ok(SeerrSession::new(client, user.id, &entry.seerr_url, public))
}

// --- Account-level entry points ------------------------------------------------

/// Local-file-only read (zero network): whether Discover is configured for
/// `(server_url, user_id)` (docs/14's "zero startup cost" rule). Pass an
/// already-open `live` session (if the caller has one cached) to fill in
/// `app_title`; without one, `app_title` is always `None` here since it's
/// only known after a live `/settings/public` fetch.
pub fn status(
    data_dir: &Path,
    server_url: &str,
    user_id: &str,
    live: Option<&SeerrSession>,
) -> SeerrStatus {
    let not_configured = SeerrStatus {
        configured: false,
        seerr_url: None,
        method: None,
        identity: None,
        app_title: None,
    };
    let config = SeerrConfigStore::load(data_dir);
    let Some(entry) = config.find(server_url, user_id) else {
        return not_configured;
    };
    SeerrStatus {
        configured: true,
        seerr_url: Some(entry.seerr_url.clone()),
        method: Some(entry.method),
        identity: Some(entry.identity.clone()),
        app_title: live.and_then(SeerrSession::app_title),
    }
}

/// [`connect`]'s arguments beyond `data_dir`: bundled into one struct
/// because five of the six are `&str` (`server_url`/`user_id`/`url`/
/// `identity`/`secret`) -- a bare positional list that size is a
/// transposition hazard (e.g. passing `identity` where `secret` is
/// expected compiles silently, since both are just `&str`). Field names
/// match the old parameter names.
pub struct ConnectArgs<'a> {
    pub server_url: &'a str,
    pub user_id: &'a str,
    pub url: &'a str,
    pub method: SeerrAuthMethod,
    pub identity: &'a str,
    pub secret: &'a str,
}

/// Expands `url` into candidates, tries a real login on each (tight
/// timeouts) until one succeeds, then saves the connection and caches
/// `/settings/public` flags. Nothing is saved on failure.
pub async fn connect(
    data_dir: &Path,
    args: ConnectArgs<'_>,
) -> Result<(SeerrSession, SeerrStatus), SeerrSessionError> {
    let ConnectArgs {
        server_url,
        user_id,
        url,
        method,
        identity,
        secret,
    } = args;
    let candidates = seerr_url::candidate_urls(url);
    if candidates.is_empty() {
        return Err(SeerrSessionError::InvalidUrl);
    }

    let mut last_error: Option<SeerrError> = None;
    let mut connected: Option<(SeerrClient, models::User, String)> = None;
    for candidate in &candidates {
        let attempt = SeerrClient::login(
            candidate,
            method,
            identity,
            secret,
            PROBE_CONNECT_TIMEOUT,
            PROBE_REQUEST_TIMEOUT,
        )
        .await;
        match attempt {
            Ok((client, user)) => {
                connected = Some((client, user, seerr_url::normalize_api_base(candidate)));
                break;
            }
            Err(e) => last_error = Some(e),
        }
    }
    let (client, user, resolved_seerr_url) = match connected {
        Some(v) => v,
        None => {
            return Err(last_error
                .map(SeerrSessionError::Client)
                .unwrap_or(SeerrSessionError::InvalidUrl));
        }
    };

    let public = client.public_settings().await.unwrap_or_default();
    let app_title = public.application_title.clone();

    let entry = SeerrConfigEntry {
        server_url: server_url.to_string(),
        user_id: user_id.to_string(),
        seerr_url: resolved_seerr_url.clone(),
        method,
        identity: identity.to_string(),
        secret: secret.to_string(),
    };
    let mut config = SeerrConfigStore::load(data_dir);
    config.upsert(entry.clone());
    config
        .save(data_dir)
        .map_err(|e| SeerrSessionError::Persist(e.to_string()))?;

    let session = SeerrSession::new(client, user.id, &resolved_seerr_url, public);

    Ok((
        session,
        SeerrStatus {
            configured: true,
            seerr_url: Some(entry.seerr_url),
            method: Some(method),
            identity: Some(entry.identity),
            app_title,
        },
    ))
}

/// Builds a fresh [`SeerrSession`] from the saved config for
/// `(server_url, user_id)` -- a network round trip (the exception to
/// [`status`]'s zero-network contract). `Err(NotConfigured)` when there's
/// no saved connection.
pub async fn open(
    data_dir: &Path,
    server_url: &str,
    user_id: &str,
) -> Result<SeerrSession, SeerrSessionError> {
    let entry = SeerrConfigStore::load(data_dir)
        .find(server_url, user_id)
        .cloned()
        .ok_or(SeerrSessionError::NotConfigured)?;
    build_session(&entry).await
}

/// Removes the saved Seerr connection for `(server_url, user_id)`. The
/// caller is responsible for dropping any live [`SeerrSession`] it was
/// holding for this account -- this crate keeps no session cache of its own.
pub fn disconnect(data_dir: &Path, server_url: &str, user_id: &str) -> std::io::Result<()> {
    let mut config = SeerrConfigStore::load(data_dir);
    config.remove(server_url, user_id);
    config.save(data_dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal one-shot-per-response mock server: answers with the next queued `(status, body)` pair.
    async fn scripted_json_server(responses: Vec<(u16, String)>) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind local listener");
        let addr = listener.local_addr().expect("local_addr");

        tokio::spawn(async move {
            for (status, body) in responses {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };

                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                let header_end = loop {
                    let n = stream.read(&mut chunk).await.unwrap_or(0);
                    if n == 0 {
                        break None;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        break Some(pos + 4);
                    }
                };
                let Some(header_end) = header_end else {
                    continue;
                };

                let head = String::from_utf8_lossy(&buf[..header_end]).to_string();
                let mut content_length = 0usize;
                for line in head.split("\r\n").skip(1) {
                    if let Some((name, value)) = line.split_once(':') {
                        if name.trim().eq_ignore_ascii_case("content-length") {
                            content_length = value.trim().parse().unwrap_or(0);
                        }
                    }
                }
                let mut body_read = buf.len() - header_end;
                while body_read < content_length {
                    let n = stream.read(&mut chunk).await.unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    body_read += n;
                }

                let status_line = match status {
                    200 => "200 OK",
                    201 => "201 Created",
                    _ => "500 Internal Server Error",
                };
                let response = format!(
                    "HTTP/1.1 {status_line}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(response.as_bytes()).await;
                let _ = stream.shutdown().await;
            }
        });

        format!("http://{addr}")
    }

    fn sample_entry(server_url: &str, user_id: &str, seerr_url: &str) -> SeerrConfigEntry {
        SeerrConfigEntry {
            server_url: server_url.to_string(),
            user_id: user_id.to_string(),
            seerr_url: seerr_url.to_string(),
            method: SeerrAuthMethod::Jellyfin,
            identity: "alice".to_string(),
            secret: "hunter2".to_string(),
        }
    }

    // --- status(): local-file-only, zero network --------------------------------

    #[test]
    fn status_with_nothing_saved_is_not_configured() {
        let dir = tempfile::tempdir().expect("tempdir");
        let status = status(dir.path(), "http://jf.test", "user-1", None);
        assert!(!status.configured);
        assert!(status.seerr_url.is_none());
    }

    #[test]
    fn status_reads_the_saved_config_without_a_live_session() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut config = SeerrConfigStore::default();
        config.upsert(sample_entry(
            "http://jf.test",
            "user-1",
            "http://seerr.test/api/v1",
        ));
        config.save(dir.path()).expect("save seerr config");

        let status = status(dir.path(), "http://jf.test", "user-1", None);
        assert!(status.configured);
        assert_eq!(
            status.seerr_url.as_deref(),
            Some("http://seerr.test/api/v1")
        );
        assert_eq!(status.method, Some(SeerrAuthMethod::Jellyfin));
        assert_eq!(status.identity.as_deref(), Some("alice"));
        assert!(
            status.app_title.is_none(),
            "no live session was passed, so app_title has nothing to draw from"
        );
    }

    #[test]
    fn status_keys_on_the_requested_account_not_just_any_saved_entry() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut config = SeerrConfigStore::default();
        config.upsert(sample_entry(
            "http://jf-b.test",
            "user-b",
            "http://seerr-b.test/api/v1",
        ));
        config.save(dir.path()).expect("save seerr config");

        assert!(!status(dir.path(), "http://jf-a.test", "user-a", None).configured);
    }

    // --- Home-cache invalidation on submit/cancel -------------------------------

    #[tokio::test]
    async fn submit_request_invalidates_the_home_cache_on_success() {
        // API_KEY auth: `GET /auth/me`, `GET /movie/{id}` (no mediaInfo), `POST /request`.
        let base = scripted_json_server(vec![
            (
                200,
                r#"{"id":1,"username":"jellybeam-test-user"}"#.to_string(),
            ),
            (200, r#"{"id":42,"title":"Sample Movie"}"#.to_string()),
            (201, r#"{"id":7,"status":1,"is4k":false}"#.to_string()),
        ])
        .await;

        let (client, user) = SeerrClient::login(
            &base,
            SeerrAuthMethod::ApiKey,
            "",
            "test-key",
            Duration::from_secs(2),
            Duration::from_secs(6),
        )
        .await
        .expect("login against the mock server should succeed");
        assert_eq!(user.id, 1);

        let session = SeerrSession::new(client, user.id, &base, models::PublicSettings::default());
        session.cache_home(SeerrHome {
            rows: vec![SeerrHomeRow {
                id: "trending".to_string(),
                title: "Trending".to_string(),
                cards: Vec::new(),
            }],
        });
        assert!(
            session.cached_home().is_some(),
            "precondition: a home snapshot is cached before the submit"
        );

        session
            .submit_request(SeerrRequestInput {
                media_type: SeerrMediaType::Movie,
                tmdb_id: 42,
                is_4k: false,
                seasons: Vec::new(),
                server_id: None,
                profile_id: None,
                root_folder: None,
            })
            .await
            .expect("submit against the mock server should succeed");

        assert!(
            session.cached_home().is_none(),
            "a successful submit must invalidate the now-stale cached home snapshot"
        );
    }

    #[tokio::test]
    async fn cancel_request_invalidates_the_home_cache_on_success() {
        let base = scripted_json_server(vec![
            (
                200,
                r#"{"id":1,"username":"jellybeam-test-user"}"#.to_string(),
            ),
            (200, r#"{"id":7,"status":1,"is4k":false}"#.to_string()), // DELETE /request/7
        ])
        .await;

        let (client, user) = SeerrClient::login(
            &base,
            SeerrAuthMethod::ApiKey,
            "",
            "test-key",
            Duration::from_secs(2),
            Duration::from_secs(6),
        )
        .await
        .expect("login against the mock server should succeed");

        let session = SeerrSession::new(client, user.id, &base, models::PublicSettings::default());
        session.cache_home(SeerrHome { rows: Vec::new() });

        session
            .cancel_request(7)
            .await
            .expect("cancel against the mock server should succeed");

        assert!(
            session.cached_home().is_none(),
            "a successful cancel must invalidate the now-stale cached home snapshot"
        );
    }

    // --- connect()/open()/disconnect(): store integration -----------------------

    #[tokio::test]
    async fn connect_saves_the_config_and_returns_a_live_session() {
        let dir = tempfile::tempdir().expect("tempdir");
        let base = scripted_json_server(vec![
            (
                200,
                r#"{"id":1,"username":"jellybeam-test-user"}"#.to_string(),
            ), // /auth/me
            (
                200,
                r#"{"movie4kEnabled":false,"series4kEnabled":false,"cacheImages":false}"#
                    .to_string(),
            ), // /settings/public
        ])
        .await;
        // Strip the scheme so `candidate_urls` treats it as a bare host and
        // tries `base` (http, default port) first -- matching the mock
        // server's own scheme/port.
        let host = base.trim_start_matches("http://");

        let (session, status) = connect(
            dir.path(),
            ConnectArgs {
                server_url: "http://jf.test",
                user_id: "user-1",
                url: host,
                method: SeerrAuthMethod::ApiKey,
                identity: "",
                secret: "test-key",
            },
        )
        .await
        .expect("connect against the mock server should succeed");

        assert!(status.configured);
        assert_eq!(session.app_title(), None);

        let saved = SeerrConfigStore::load(dir.path());
        let entry = saved
            .find("http://jf.test", "user-1")
            .expect("connect should have persisted an entry");
        assert_eq!(entry.method, SeerrAuthMethod::ApiKey);
    }

    #[tokio::test]
    async fn open_fails_with_not_configured_when_nothing_is_saved() {
        let dir = tempfile::tempdir().expect("tempdir");
        // `SeerrSession` deliberately has no `Debug` impl (it holds a
        // `SeerrClient`, and `client.rs`'s own doc comment notes that type
        // skips `Debug` so a cookie/API key can never print) -- `matches!`
        // avoids needing `Result::expect_err`'s `T: Debug` bound.
        assert!(matches!(
            open(dir.path(), "http://jf.test", "user-1").await,
            Err(SeerrSessionError::NotConfigured)
        ));
    }

    #[test]
    fn disconnect_removes_only_the_named_account() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut config = SeerrConfigStore::default();
        config.upsert(sample_entry(
            "http://jf-a.test",
            "user-a",
            "http://seerr-a.test/api/v1",
        ));
        config.upsert(sample_entry(
            "http://jf-b.test",
            "user-b",
            "http://seerr-b.test/api/v1",
        ));
        config.save(dir.path()).expect("save");

        disconnect(dir.path(), "http://jf-a.test", "user-a").expect("disconnect");

        let reloaded = SeerrConfigStore::load(dir.path());
        assert!(reloaded.find("http://jf-a.test", "user-a").is_none());
        assert!(reloaded.find("http://jf-b.test", "user-b").is_some());
    }
}
