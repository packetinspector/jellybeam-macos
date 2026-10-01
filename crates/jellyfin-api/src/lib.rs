//! Jellyfin REST + WebSocket client. Models are generated (see
//! `codegen/regen.sh`); the client surface below is hand-written.
//!
//! TLS: `reqwest`/`tokio-tungstenite` both use rustls, not native-tls/
//! OpenSSL, so HTTP and WebSocket share one TLS stack with no system
//! OpenSSL dependency. See `Cargo.toml` for the exact feature selection.

// models.rs is generated (see codegen/regen.sh) and isn't hand-tuned for
// clippy's stricter lints; hand-editing it to satisfy them would just be
// undone by the next regeneration.
#[allow(clippy::all)]
pub mod dns;
// Generated Default impls trip clippy::derivable_impls; allowed here (not
// fixed) so the generated file stays untouched across regeneration.
#[allow(clippy::derivable_impls)]
pub mod models;
mod util;
mod ws;

/// Fuzzing-only re-exports. `cargo fuzz` builds with `--cfg fuzzing`;
/// normal builds never see this module, so the private decode surface
/// stays private everywhere else.
#[cfg(fuzzing)]
pub mod fuzzing {
    pub use crate::ws::decode_message;
}

use std::time::Duration;

use models::*;
use util::{describe_error_chain, percent_encode};

/// TCP connect timeout for the REST client: a hung connect to an
/// unreachable/firewalled host must not block forever with no way to retry.
///
/// 10s, not 5s: a VPN route can still be establishing on a cold
/// start (first connect after wake/launch), and 5s was tight enough to fail
/// a connect that would have succeeded a couple seconds later.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Whole-request timeout (connect + send + receive headers/body). Does NOT
/// apply to image/stream URLs -- those are plain strings returned by
/// `image_url`/`stream_url` and fetched by other crates (media-cache,
/// player) with their own timeout/retry policy suited to large transfers.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Build the `reqwest::Client` shared by every REST call this client makes.
/// Panics only if the TLS backend can't be initialized -- the same failure
/// mode `reqwest::Client::new()` (what this replaces) already panics on.
fn build_http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(REQUEST_TIMEOUT)
        .redirect(redirect_policy())
        // The system resolver intermittently stalls ~5s; the shared cache
        // serves every post-first lookup instantly (see `dns`'s docs).
        .dns_resolver(crate::dns::shared_dns_resolver())
        .build()
        .expect("reqwest client with connect/request timeouts should always build")
}

/// Redirect hop limit -- matches reqwest's own default policy, so a
/// deployment behind a redirecting reverse proxy still works.
const MAX_REDIRECT_HOPS: usize = 10;

/// reqwest's default policy strips `Authorization` only on a *cross-host*
/// redirect, ignoring scheme -- a same-host bounce to `http://` would leak
/// the token and silently undo the user's choice of `https://`. Hop count
/// is unchanged; only the downgrade is refused, and loudly.
fn redirect_policy() -> reqwest::redirect::Policy {
    reqwest::redirect::Policy::custom(|attempt| {
        if is_scheme_downgrade(attempt.previous(), attempt.url()) {
            tracing::warn!("refusing an https -> http redirect (would leak the access token)");
            return attempt.error("refusing to follow an https -> http redirect");
        }
        // `previous`'s first entry is the original request URL, not a
        // redirection -- the same off-by-one `Policy::limited` accounts for.
        if attempt.previous().len() > MAX_REDIRECT_HOPS {
            return attempt.error("too many redirects");
        }
        attempt.follow()
    })
}

/// True when the hop we just came from was `https` and the next URL isn't.
/// Split out from [`redirect_policy`] because `reqwest::redirect::Attempt`
/// can't be constructed outside reqwest, so this is the only part of the
/// policy that can be tested directly.
fn is_scheme_downgrade(previous: &[reqwest::Url], next: &reqwest::Url) -> bool {
    previous
        .last()
        .is_some_and(|prev| prev.scheme() == "https" && next.scheme() != "https")
}

/// Response bodies attached to [`ApiError::Status`] are capped at this many
/// bytes: Jellyfin's error responses are usually a short ASP.NET
/// problem-details JSON body worth surfacing for debugging, but must stay
/// bounded so a misbehaving proxy/server can't balloon an error value.
const STATUS_ERROR_BODY_CAP: usize = 2048;

/// Hard ceiling on a *successful* JSON response body. [`STATUS_ERROR_BODY_CAP`]
/// only bounds non-2xx bodies; `Response::json` buffers a 200 body in full
/// before serde sees a byte, so an unbounded response could force an
/// arbitrarily large allocation. 8 MiB is generous even for a large library
/// page.
const JSON_BODY_CAP: usize = 8 * 1024 * 1024;

/// Reads a response body chunk by chunk, refusing to buffer more than `cap`
/// bytes, then deserializes it -- replaces `Response::json()`, which reads
/// the whole body before serde sees any of it. `what` is the endpoint path
/// (log line only); `cap` is a parameter so tests can drive the boundary
/// without pushing 8 MiB across a socket.
async fn read_capped_body(
    mut resp: reqwest::Response,
    cap: usize,
    what: &str,
) -> Result<Vec<u8>, ApiError> {
    let mut body: Vec<u8> = Vec::new();
    loop {
        let chunk = match resp.chunk().await {
            Ok(Some(chunk)) => chunk,
            Ok(None) => return Ok(body),
            Err(e) => return Err(ApiError::Transport(describe_error_chain(&e))),
        };
        if body.len() + chunk.len() > cap {
            tracing::warn!(path = %what, cap, "response body exceeded the size cap; aborting read");
            return Err(ApiError::Decode(format!(
                "response body exceeded {cap} bytes"
            )));
        }
        body.extend_from_slice(&chunk);
    }
}

/// [`read_capped_body`] at [`JSON_BODY_CAP`], plus the deserialize step --
/// the drop-in for every `resp.json().await` in this crate.
async fn read_json_capped<T: serde::de::DeserializeOwned>(
    resp: reqwest::Response,
    what: &str,
) -> Result<T, ApiError> {
    let body = read_capped_body(resp, JSON_BODY_CAP, what).await?;
    serde_json::from_slice(&body).map_err(|e| ApiError::Decode(e.to_string()))
}

#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error("http status {code}{}", if body.is_empty() { String::new() } else { format!(": {body}") })]
    Status { code: u16, body: String },
    #[error("transport: {0}")]
    Transport(String),
    #[error("decode: {0}")]
    Decode(String),
    #[error("unauthorized")]
    Unauthorized,
}

/// Connection identity sent in the `Authorization: MediaBrowser ...` header.
#[derive(Debug, Clone)]
pub struct ClientIdentity {
    pub client: String,    // "Jellybeam"
    pub device: String,    // hostname
    pub device_id: String, // stable per install
    pub version: String,
}

/// A Jellyfin server's version, parsed from `PublicServerInfo.Version`
/// (e.g. `"10.11.0"`, `"12.0.0"`). Ordered lexicographically on
/// `(major, minor, patch)` so `ServerVersion`s can be compared directly;
/// [`Self::at_least`] only looks at `(major, minor)`, which is all any
/// caller has needed so far.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ServerVersion {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
}

impl ServerVersion {
    /// `true` when this version is `>= (major, minor)`, compared
    /// lexicographically on those two components (patch is ignored).
    pub fn at_least(&self, major: u32, minor: u32) -> bool {
        (self.major, self.minor) >= (major, minor)
    }
}

impl std::fmt::Display for ServerVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// Parses `"major.minor[.patch][-suffix]"`. `patch` defaults to `0` when
/// absent (`"12.0"` parses the same as `"12.0.0"`); a trailing non-numeric
/// suffix on any component (e.g. `"10.11.0-rc1"`) is tolerated by taking
/// only that component's leading digits. At least `major.minor` must be
/// present -- anything else is `Err`.
impl std::str::FromStr for ServerVersion {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        // A suffix like "-rc1" only ever trails the *last* numeric
        // component in practice, but splitting it off up front (rather
        // than per-component) keeps the parse simple and matches every
        // real-world version string this needs to handle.
        let leading_digits = |part: &str| -> Result<u32, ()> {
            let digits: String = part.chars().take_while(|c| c.is_ascii_digit()).collect();
            if digits.is_empty() {
                return Err(());
            }
            digits.parse::<u32>().map_err(|_| ())
        };

        let mut parts = s.splitn(3, '.');
        let major = parts.next().ok_or(())?;
        let minor = parts.next().ok_or(())?;
        let major = leading_digits(major)?;
        let minor = leading_digits(minor)?;
        let patch = match parts.next() {
            Some(patch) => leading_digits(patch)?,
            None => 0,
        };
        Ok(ServerVersion {
            major,
            minor,
            patch,
        })
    }
}

/// `GET /System/Info/Public`'s response, trimmed to the fields this client
/// uses. Unauthenticated endpoint -- available pre-login, same as Quick
/// Connect's `/QuickConnect/Enabled`. Unknown fields are ignored so a newer
/// server adding fields doesn't break deserialization.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct PublicServerInfo {
    pub version: Option<String>,
    pub server_name: Option<String>,
    pub id: Option<String>,
    pub startup_wizard_completed: Option<bool>,
}

impl PublicServerInfo {
    /// [`Self::version`] parsed as a [`ServerVersion`], or `None` if it's
    /// absent or doesn't parse (an unrecognized/future version string
    /// should not crash version-gated logic -- callers fail closed via
    /// [`Self::version`] being unusable rather than panicking here).
    pub fn parsed_version(&self) -> Option<ServerVersion> {
        self.version.as_deref()?.parse().ok()
    }
}

/// Escape a value for embedding in an HTTP header's `quoted-string`
/// component (RFC 7230 §3.2.6): backslash-escape `"` and `\`, and strip
/// bare CR/LF. `identity.device` in particular can carry a user-editable
/// macOS device name (e.g. `Ada's "Gaming" Mac`), which without escaping
/// would corrupt the `Authorization` header; an unescaped CR/LF would be a
/// request-splitting vector.
fn escape_header_value(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\r' | '\n' => {}
            _ => out.push(c),
        }
    }
    out
}

/// Builds the `MediaBrowser` auth header value:
/// `MediaBrowser Client="Jellybeam", Device="...", DeviceId="...",
/// Version="...", Token="..."` (the `Token` clause is omitted pre-auth).
///
/// `pub(crate)` so `ws.rs` can build the same header value for the
/// WebSocket handshake (Jellyfin 12 no longer accepts the token as a
/// `?api_key=` query param there either).
pub(crate) fn auth_header(identity: &ClientIdentity, token: Option<&str>) -> String {
    let mut header = format!(
        r#"MediaBrowser Client="{}", Device="{}", DeviceId="{}", Version="{}""#,
        escape_header_value(&identity.client),
        escape_header_value(&identity.device),
        escape_header_value(&identity.device_id),
        escape_header_value(&identity.version)
    );
    if let Some(token) = token {
        header.push_str(&format!(r#", Token="{}""#, escape_header_value(token)));
    }
    header
}

struct Inner {
    base_url: String, // no trailing slash
    identity: ClientIdentity,
    token: String,
    http: reqwest::Client,
    /// The authenticated user's id, when known. Populated automatically by
    /// [`JellyfinClient::authenticate_by_name`] from `AuthenticationResult`'s
    /// `User.Id`. A session resumed via [`JellyfinClient::from_token`]
    /// (Keychain restore) has no `AuthenticationResult` to read this from,
    /// so it starts `None` there -- [`JellyfinClient::with_user_id`] sets it
    /// explicitly for that path.
    user_id: Option<String>,
}

/// One authenticated server connection. Cheap to clone (inner Arc).
#[derive(Clone)]
pub struct JellyfinClient {
    inner: std::sync::Arc<Inner>,
}

#[derive(Default)]
pub struct ItemQuery {
    pub parent_id: Option<String>,
    pub include_item_types: Vec<String>,
    pub recursive: bool,
    pub sort_by: Option<String>,
    /// Sort direction for `sort_by`: `"Ascending"` or `"Descending"`, sent
    /// as the `sortOrder` query param. `None` omits the param (server
    /// default).
    pub sort_order: Option<String>,
    pub fields: Vec<String>,
    pub start_index: u32,
    pub limit: u32,
    pub ids: Vec<String>,
    /// `isMissing` query param: server-side filter on missing/unaired
    /// virtual episode placeholders (`LocationType == Virtual`, no backing
    /// file). `None` omits the param entirely (not an explicit `false`).
    /// Note: a `Series`/`Season`-scoped recursive query restricted to
    /// `include_item_types = [Episode]` still gets the server's own default
    /// missing/unaired filter applied regardless of this param -- callers
    /// wanting both missing and present items must leave item types
    /// unrestricted instead.
    pub is_missing: Option<bool>,
    /// `minDateLastSaved` query param (an ISO 8601 UTC timestamp string):
    /// server-side filter for every item whose
    /// `DateLastSaved` is at or after this instant -- everything ADDED **or
    /// UPDATED** since then. `None` omits the param.
    ///
    /// **Sent TWICE, as both `minDateLastSaved`    /// `minDateLastSavedForUser`, with the same value.** Workaround for a
    /// real server bug (verified against Jellyfin 10.10.7): sending
    /// `minDateLastSaved` alone on a `recursive=true` query returns HTTP 500
    /// (`SqliteItemRepository`'s where-clause builder binds
    /// `@MinDateLastSaved` but the predicate names
    /// `@MinDateLastSavedForUser`, leaving it unbound unless both are sent).
    /// Both predicates are the same `DateLastSaved >= @param` server-side,
    /// so sending one instant for both is a no-op on servers where the bug
    /// is fixed.
    ///
    /// The *response* carries no `DateLastSaved` back (absent from the
    /// pinned `BaseItemDto`, and omitted by 10.10.7 even when named in
    /// `fields`) -- this is a filter-only value.
    pub min_date_last_saved: Option<String>,
    /// `enableImages` query param. `None` omits it (server default: `true`
    /// -- every item carries `ImageTags`/`BackdropImageTags`/
    /// `ImageBlurHashes`). `Some(false)` strips all of that from the
    /// response, for callers (e.g. an id-only reconciliation sweep) that
    /// only need `Id` and want a page to cost bytes, not kilobytes, per item.
    pub enable_images: Option<bool>,
    /// `enableUserData` query param. `None` omits it (server default:
    /// `true`). `Some(false)` omits `UserData` from the response.
    ///
    /// A response fetched with `enableUserData=false` must never be written
    /// into the mirror -- the missing `UserData` reads as "unplayed,
    /// position 0" to `rows::extract_columns`. Safe only for queries used as
    /// an id set (e.g. the reconcile sweep), which re-fetches full DTOs for
    /// any id it actually stores.
    pub enable_user_data: Option<bool>,
    /// `filters` query param (`IsPlayed`, `IsResumable`, ...), comma-joined;
    /// empty omits it.
    pub filters: Vec<String>,
}

impl ItemQuery {
    /// Equivalent to `ItemQuery::default()`, for `..ItemQuery::new()`
    /// struct-update syntax. Adding a pub field here breaks any caller using
    /// an exhaustive struct literal instead of `..`; prefer this.
    pub fn new() -> Self {
        Self::default()
    }
}

/// Sort for [`JellyfinClient::live_children`] -- unlike [`ItemQuery::sort_by`]/
/// `sort_order`, this has no mirror-side `media_cache::Sort` counterpart to
/// mirror 1:1, since a live channel/folder browse
/// (docs/PLUGIN-CHANNELS.md §2.2) never touches the
/// mirror. `ServerOrder` sends no `sortBy`/`sortOrder` at all -- the only
/// order that is ever correct for DVR content, whatever the plugin returns
/// right now; `NameAsc` and `NewestFirst` are the two additional orderings
/// the spec calls for (`sortBy=SortName&sortOrder=Ascending`/// `sortBy=PremiereDate&sortOrder=Descending` respectively).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LiveSort {
    /// No `sortBy`/`sortOrder` sent -- whatever order the server/plugin
    /// returns.
    ServerOrder,
    /// `sortBy=SortName&sortOrder=Ascending`.
    NameAsc,
    /// `sortBy=PremiereDate&sortOrder=Descending`.
    NewestFirst,
}

/// `GET /Shows/NextUp` query options -- the Settings panel's "Next Up"
/// section (cutoff + rewatching). Both knobs are omitted from the request
/// entirely when left at their default, same "send nothing rather than the
/// server's own default" convention [`ItemQuery::enable_images`] documents.
#[derive(Debug, Clone, Default)]
pub struct NextUpOptions {
    /// `nextUpDateCutoff` query param: an RFC3339 UTC instant. The server
    /// only returns series with unwatched content added on/after this date,
    /// which is how the Settings panel's "Off/14/30/90/365 days" preset
    /// ladder is implemented -- `None` ("Off") omits the param, matching
    /// today's unfiltered behavior exactly. Caller (media-cache's
    /// `sync::refresh_next_up`) is responsible for turning an N-days preset
    /// into "now minus N days" and formatting it; this struct just carries
    /// the already-formatted string, same as `ItemQuery::min_date_last_saved`.
    pub date_cutoff: Option<String>,
    /// `enableRewatching` query param. Server default is `false` (a fully
    /// watched series never resurfaces in Next Up); `true` sends
    /// `enableRewatching=true` so a rewatched-from-the-start series' next
    /// episode counts too.
    pub enable_rewatching: bool,
}

async fn check_status(resp: reqwest::Response) -> Result<reqwest::Response, ApiError> {
    if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
        return Err(ApiError::Unauthorized);
    }
    if !resp.status().is_success() {
        let code = resp.status().as_u16();
        let body = capped_body_text(resp, STATUS_ERROR_BODY_CAP).await;
        return Err(ApiError::Status { code, body });
    }
    Ok(resp)
}

/// Read at most `cap` bytes of diagnostic text while preserving the HTTP status on read failure.
async fn capped_body_text(mut resp: reqwest::Response, cap: usize) -> String {
    // SECURITY.md: stop reading at the cap so error responses cannot exhaust memory.
    let mut body = Vec::new();
    let truncated = loop {
        match resp.chunk().await {
            Ok(Some(chunk)) => {
                let remaining = cap.saturating_sub(body.len());
                body.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
                if chunk.len() > remaining {
                    break true;
                }
            }
            Ok(None) => break false,
            Err(_) => return "<failed to read response body>".to_string(),
        }
    };
    let mut text = String::from_utf8_lossy(&body).into_owned();
    let mut end = text.len().min(cap);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
    if truncated {
        text.push_str("... [truncated]");
    }
    text
}

/// Extra (non-default) `Fields` [`JellyfinClient::live_children`] requests --
/// per docs/PLUGIN-CHANNELS.md §2.2.
const LIVE_CHILDREN_FIELDS: [&str; 8] = [
    "Overview",
    "OriginalTitle",
    "SeriesName",
    "DateCreated",
    "PremiereDate",
    "ImageBlurHashes",
    "ParentId",
    "SeriesPrimaryImageTag",
];

impl JellyfinClient {
    pub async fn authenticate_by_name(
        base_url: &str,
        identity: ClientIdentity,
        username: &str,
        password: &str,
    ) -> Result<(Self, AuthenticationResult), ApiError> {
        let base_url = base_url.trim_end_matches('/').to_string();
        let http = build_http_client();
        let url = format!("{base_url}/Users/AuthenticateByName");

        let body = AuthenticateUserByName {
            username: Some(username.to_string()),
            pw: Some(password.to_string()),
        };

        let resp = http
            .post(&url)
            .header("Authorization", auth_header(&identity, None))
            .json(&body)
            .send()
            .await
            .map_err(|e| ApiError::Transport(describe_error_chain(&e)))?;
        let resp = check_status(resp).await?;
        let result: AuthenticationResult =
            read_json_capped(resp, "/Users/AuthenticateByName").await?;

        let token = result.access_token.clone().ok_or_else(|| {
            ApiError::Decode("AuthenticateByName response missing AccessToken".to_string())
        })?;

        let user_id = result
            .user
            .as_ref()
            .and_then(|u| u.id)
            .map(|id| id.to_string());

        let client = Self {
            inner: std::sync::Arc::new(Inner {
                base_url,
                identity,
                token,
                http,
                user_id,
            }),
        };
        Ok((client, result))
    }

    /// `POST /QuickConnect/Initiate` -- starts a Quick Connect handshake
    /// (docs/OVERVIEW.md §6: Initiate -> poll
    /// Connect -> AuthenticateWithQuickConnect). No prior authentication
    /// required (same pre-auth `MediaBrowser` header shape as
    /// [`Self::authenticate_by_name`], carrying only `identity`, no
    /// `Token=` clause). The returned [`QuickConnectResult`] carries the
    /// user-facing `code` (show it prominently) and the `secret` (kept
    /// client-side, used to poll and to complete authentication -- never
    /// shown to the user).
    pub async fn quick_connect_initiate(
        base_url: &str,
        identity: &ClientIdentity,
    ) -> Result<QuickConnectResult, ApiError> {
        let base_url = base_url.trim_end_matches('/');
        let http = build_http_client();
        let url = format!("{base_url}/QuickConnect/Initiate");
        let resp = http
            .post(&url)
            .header("Authorization", auth_header(identity, None))
            .send()
            .await
            .map_err(|e| ApiError::Transport(describe_error_chain(&e)))?;
        let resp = check_status(resp).await?;
        read_json_capped(resp, "/QuickConnect/Initiate").await
    }

    /// `GET /QuickConnect/Connect?secret=...` -- one poll of a Quick Connect
    /// request's state. Callers drive the 2s poll loop (a UI/timer
    /// concern, out of scope for this crate); `result.authenticated ==
    /// Some(true)` means [`Self::authenticate_with_quick_connect`] can now
    /// be called with the same secret. A 404 means the secret expired/was
    /// never valid -- surfaced as `ApiError::Status` like any other
    /// non-success response (no special-casing, unlike
    /// [`Self::get_media_segments`]'s 404-as-empty: an unknown secret here
    /// is a real error the poll loop should stop on, not a normal "nothing
    /// yet" state).
    pub async fn quick_connect_poll(
        base_url: &str,
        identity: &ClientIdentity,
        secret: &str,
    ) -> Result<QuickConnectResult, ApiError> {
        let base_url = base_url.trim_end_matches('/');
        let http = build_http_client();
        let url = format!("{base_url}/QuickConnect/Connect");
        let resp = http
            .get(&url)
            .header("Authorization", auth_header(identity, None))
            .query(&[("secret", secret)])
            .send()
            .await
            .map_err(|e| ApiError::Transport(describe_error_chain(&e)))?;
        let resp = check_status(resp).await?;
        read_json_capped(resp, "/QuickConnect/Connect").await
    }

    /// `GET /QuickConnect/Enabled` -- whether the server has Quick Connect
    /// turned on at all. Worth checking before showing the "Use Quick
    /// Connect" toggle's flow, since Initiate 401s outright when disabled.
    pub async fn quick_connect_enabled(
        base_url: &str,
        identity: &ClientIdentity,
    ) -> Result<bool, ApiError> {
        let base_url = base_url.trim_end_matches('/');
        let http = build_http_client();
        let url = format!("{base_url}/QuickConnect/Enabled");
        let resp = http
            .get(&url)
            .header("Authorization", auth_header(identity, None))
            .send()
            .await
            .map_err(|e| ApiError::Transport(describe_error_chain(&e)))?;
        let resp = check_status(resp).await?;
        read_json_capped(resp, "/QuickConnect/Enabled").await
    }

    /// `GET /System/Info/Public` -- the server's version and identity,
    /// unauthenticated (same pre-auth shape as
    /// [`Self::quick_connect_enabled`]). Used to seed/refresh the server
    /// version gate (see [`ServerVersion::at_least`]).
    pub async fn public_system_info(
        base_url: &str,
        identity: &ClientIdentity,
    ) -> Result<PublicServerInfo, ApiError> {
        let base_url = base_url.trim_end_matches('/');
        let http = build_http_client();
        let url = format!("{base_url}/System/Info/Public");
        let resp = http
            .get(&url)
            .header("Authorization", auth_header(identity, None))
            .send()
            .await
            .map_err(|e| ApiError::Transport(describe_error_chain(&e)))?;
        let resp = check_status(resp).await?;
        read_json_capped(resp, "/System/Info/Public").await
    }

    /// [`Self::public_system_info`] against this client's own
    /// `base_url`/`identity` -- for refreshing the server version gate on an
    /// already-connected client (sign-in, session restore, account switch,
    /// websocket reconnect, reauthorization). Deliberately does not require
    /// (or send) the auth token: `/System/Info/Public` is unauthenticated,
    /// and a call here must still succeed if the current token has expired.
    pub async fn refresh_public_system_info(&self) -> Result<PublicServerInfo, ApiError> {
        Self::public_system_info(&self.inner.base_url, &self.inner.identity).await
    }

    /// `POST /Users/AuthenticateWithQuickConnect` -- completes the
    /// handshake once [`Self::quick_connect_poll`] reports `authenticated:
    /// true`, exchanging the secret for a real access token. Mirrors
    /// [`Self::authenticate_by_name`]'s shape exactly (constructs a fresh
    /// client from the response rather than requiring an existing one).
    pub async fn authenticate_with_quick_connect(
        base_url: &str,
        identity: ClientIdentity,
        secret: &str,
    ) -> Result<(Self, AuthenticationResult), ApiError> {
        let base_url = base_url.trim_end_matches('/').to_string();
        let http = build_http_client();
        let url = format!("{base_url}/Users/AuthenticateWithQuickConnect");

        let secret_value: QuickConnectDtoSecret = secret
            .to_string()
            .try_into()
            .map_err(|_| ApiError::Decode("empty Quick Connect secret".to_string()))?;
        let body = QuickConnectDto {
            secret: secret_value,
        };

        let resp = http
            .post(&url)
            .header("Authorization", auth_header(&identity, None))
            .json(&body)
            .send()
            .await
            .map_err(|e| ApiError::Transport(describe_error_chain(&e)))?;
        let resp = check_status(resp).await?;
        let result: AuthenticationResult =
            read_json_capped(resp, "/Users/AuthenticateWithQuickConnect").await?;

        let token = result.access_token.clone().ok_or_else(|| {
            ApiError::Decode(
                "AuthenticateWithQuickConnect response missing AccessToken".to_string(),
            )
        })?;
        let user_id = result
            .user
            .as_ref()
            .and_then(|u| u.id)
            .map(|id| id.to_string());

        let client = Self {
            inner: std::sync::Arc::new(Inner {
                base_url,
                identity,
                token,
                http,
                user_id,
            }),
        };
        Ok((client, result))
    }

    /// Resume a session from a stored token (Keychain).
    ///
    /// A session resumed this way has no `AuthenticationResult` to read a
    /// user id from, so [`Self::user_id`] returns `None` until the caller
    /// supplies one via [`Self::with_user_id`]. The Keychain-restore path
    /// typically has the user id available from whatever it stored
    /// alongside the token and should pass it through that way.
    pub fn from_token(base_url: &str, identity: ClientIdentity, token: &str) -> Self {
        let base_url = base_url.trim_end_matches('/').to_string();
        Self {
            inner: std::sync::Arc::new(Inner {
                base_url,
                identity,
                token: token.to_string(),
                http: build_http_client(),
                user_id: None,
            }),
        }
    }

    /// Builder that attaches a known user id to a client (typically right
    /// after [`Self::from_token`], for the Keychain-restore path where the
    /// user id was stored alongside the token but there's no
    /// `AuthenticationResult` to derive it from automatically). Returns a
    /// new client sharing the same connection; does not mutate `self` in
    /// place since `JellyfinClient`'s `Inner` is behind an `Arc`.
    pub fn with_user_id(self, user_id: &str) -> Self {
        let inner = Inner {
            base_url: self.inner.base_url.clone(),
            identity: self.inner.identity.clone(),
            token: self.inner.token.clone(),
            http: self.inner.http.clone(),
            user_id: Some(user_id.to_string()),
        };
        Self {
            inner: std::sync::Arc::new(inner),
        }
    }

    /// The authenticated user's id, if known. Always `Some` after
    /// [`Self::authenticate_by_name`]; `None` after [`Self::from_token`]
    /// unless [`Self::with_user_id`] was also called.
    pub fn user_id(&self) -> Option<&str> {
        self.inner.user_id.as_deref()
    }

    fn auth_header(&self) -> String {
        auth_header(&self.inner.identity, Some(&self.inner.token))
    }

    async fn get<T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        query: &[(&str, String)],
    ) -> Result<T, ApiError> {
        let url = format!("{}{}", self.inner.base_url, path);
        let resp = self
            .inner
            .http
            .get(&url)
            .header("Authorization", self.auth_header())
            .query(query)
            .send()
            .await
            .map_err(|e| ApiError::Transport(describe_error_chain(&e)))?;
        let resp = check_status(resp).await?;
        read_json_capped(resp, path).await
    }

    pub async fn get_user_views(&self) -> Result<Vec<BaseItemDto>, ApiError> {
        let result: ItemsResult = self.get("/UserViews", &[]).await?;
        Ok(result.items)
    }

    pub async fn get_items(&self, q: &ItemQuery) -> Result<ItemsResult, ApiError> {
        let mut query: Vec<(&str, String)> = Vec::new();
        if let Some(parent_id) = &q.parent_id {
            query.push(("parentId", parent_id.clone()));
        }
        if !q.include_item_types.is_empty() {
            query.push(("includeItemTypes", q.include_item_types.join(",")));
        }
        query.push(("recursive", q.recursive.to_string()));
        if let Some(sort_by) = &q.sort_by {
            query.push(("sortBy", sort_by.clone()));
        }
        if let Some(sort_order) = &q.sort_order {
            query.push(("sortOrder", sort_order.clone()));
        }
        if !q.fields.is_empty() {
            query.push(("fields", q.fields.join(",")));
        }
        query.push(("startIndex", q.start_index.to_string()));
        query.push(("limit", q.limit.to_string()));
        if !q.ids.is_empty() {
            query.push(("ids", q.ids.join(",")));
        }
        if let Some(is_missing) = q.is_missing {
            query.push(("isMissing", is_missing.to_string()));
        }
        if let Some(min_date_last_saved) = &q.min_date_last_saved {
            query.push(("minDateLastSaved", min_date_last_saved.clone()));
            // Not a typo and not a second filter the caller asked for: see
            // `ItemQuery::min_date_last_saved`'s doc comment. 10.10.x's
            // where-clause builder leaves `@MinDateLastSavedForUser`
            // unbound (HTTP 500) unless this is sent too, and both
            // predicates are the same `DateLastSaved >= @param` server-side,
            // so sending one instant for both is a no-op semantically.
            query.push(("minDateLastSavedForUser", min_date_last_saved.clone()));
        }
        // Payload-shrinking knobs, only ever sent when a caller explicitly
        // asks (see their doc comments): omitted entirely when `None`, so
        // every existing query's bytes on the wire are unchanged.
        if let Some(enable_images) = q.enable_images {
            query.push(("enableImages", enable_images.to_string()));
        }
        if let Some(enable_user_data) = q.enable_user_data {
            query.push(("enableUserData", enable_user_data.to_string()));
        }
        if !q.filters.is_empty() {
            query.push(("filters", q.filters.join(",")));
        }
        self.get("/Items", &query).await
    }

    /// docs/PLUGIN-CHANNELS.md §2.2: a live,
    /// non-recursive, server-sorted children listing -- the browse
    /// primitive for a Jellyfin plugin channel (e.g. a TVHeadend recordings
    /// library): its content is never mirrored (fact 2/3 -- no usable
    /// parent linkage, no WS/`LibraryChanged` coverage), so a channel view
    /// and each `ChannelFolderItem` inside it are re-queried live, every
    /// time the grid becomes visible, via this method instead of
    /// `media_cache::Mirror::children`.
    ///
    /// `GET /Items?parentId=&recursive=false&startIndex=&limit=&fields=...`
    /// with `sortBy`/`sortOrder` per `sort` (see [`LiveSort`]) -- the same
    /// field list [`Self::get_items`] callers use for a full sync
    /// (`Overview,OriginalTitle,SeriesName,DateCreated,PremiereDate,
    /// ImageBlurHashes,ParentId,SeriesPrimaryImageTag`). Returns the raw
    /// item DTOs; ordinary paging works via `start_index`/`limit`.
    pub async fn live_children(
        &self,
        parent_id: &str,
        start_index: u32,
        limit: u32,
        sort: LiveSort,
    ) -> Result<Vec<BaseItemDto>, ApiError> {
        let (sort_by, sort_order) = match sort {
            LiveSort::ServerOrder => (None, None),
            LiveSort::NameAsc => (Some("SortName".to_string()), Some("Ascending".to_string())),
            LiveSort::NewestFirst => (
                Some("PremiereDate".to_string()),
                Some("Descending".to_string()),
            ),
        };
        let query = ItemQuery {
            parent_id: Some(parent_id.to_string()),
            recursive: false,
            start_index,
            limit,
            fields: LIVE_CHILDREN_FIELDS.iter().map(|s| s.to_string()).collect(),
            sort_by,
            sort_order,
            ..ItemQuery::new()
        };
        let result = self.get_items(&query).await?;
        Ok(result.items)
    }

    pub async fn get_resume_items(&self) -> Result<ItemsResult, ApiError> {
        self.get("/UserItems/Resume", &[]).await
    }

    /// `fields` matters here more than it looks: NextUp's items get
    /// UPSERTED over the mirror's rows by `media-cache`'s `refresh_next_up`,
    /// and the server's field-less NextUp response omits `Overview` (and
    /// the other opt-in `ItemFields`). A bare request therefore silently
    /// strips the currently-watched episode's synopsis out of the mirror --
    /// the Detail rail's *selected* card would be the only one with no
    /// description, because it is exactly the episode NextUp returns.
    /// Callers that persist the result must pass their full field set.
    pub async fn get_next_up(
        &self,
        fields: &[String],
        options: &NextUpOptions,
    ) -> Result<ItemsResult, ApiError> {
        let mut query: Vec<(&str, String)> = Vec::new();
        if !fields.is_empty() {
            query.push(("fields", fields.join(",")));
        }
        if let Some(cutoff) = &options.date_cutoff {
            query.push(("nextUpDateCutoff", cutoff.clone()));
        }
        if options.enable_rewatching {
            query.push(("enableRewatching", "true".to_string()));
        }
        self.get("/Shows/NextUp", &query).await
    }

    /// `GET
    /// /Items/{itemId}/Similar`, per the pinned OpenAPI spec
    /// (`codegen/jellyfin-openapi-stable.json`, `GetSimilarItems` --
    /// `itemId` path param, `userId`/`limit`/`fields` query params, response
    /// `BaseItemDtoQueryResult` -- the same shape `ItemsResult` already
    /// aliases for `/Items`/`/UserViews`/NextUp). `userId` is sent whenever
    /// this client knows one (`JellyfinClient::user_id`) so recommendations
    /// are personalized for that user; a session resumed from Keychain with no `AuthenticationResult`
    /// (see `Inner::user_id`'s doc comment) simply omits it, which the spec
    /// allows.
    pub async fn get_similar(
        &self,
        item_id: &str,
        limit: u32,
    ) -> Result<Vec<BaseItemDto>, ApiError> {
        let path = format!("/Items/{}/Similar", percent_encode(item_id));
        let mut query: Vec<(&str, String)> = vec![("limit", limit.to_string())];
        if let Some(user_id) = &self.inner.user_id {
            query.push(("userId", user_id.clone()));
        }
        let result: ItemsResult = self.get(&path, &query).await?;
        Ok(result.items)
    }

    pub async fn get_playback_info(
        &self,
        item_id: &str,
        profile: &DeviceProfile,
        start_ticks: Option<i64>,
    ) -> Result<PlaybackInfoResponse, ApiError> {
        let url = format!(
            "{}/Items/{}/PlaybackInfo",
            self.inner.base_url,
            percent_encode(item_id)
        );
        let body = PlaybackInfoDto {
            start_time_ticks: start_ticks,
            device_profile: Some(profile.clone()),
            ..Default::default()
        };
        let resp = self
            .inner
            .http
            .post(&url)
            .header("Authorization", self.auth_header())
            .json(&body)
            .send()
            .await
            .map_err(|e| ApiError::Transport(describe_error_chain(&e)))?;
        let resp = check_status(resp).await?;
        read_json_capped(resp, "/Items/{itemId}/PlaybackInfo").await
    }

    /// Fetches skip-intro/credits markers for an item: `GET
    /// /MediaSegments/{itemId}[?includeSegmentTypes=...]`. `include_types`
    /// (e.g. `["Intro", "Outro"]`) is sent as a comma-joined
    /// `includeSegmentTypes` query param when non-empty; an empty slice
    /// omits the param entirely, which per the spec means "no filter" (all
    /// segment types).
    ///
    /// MediaSegments is a newer Jellyfin feature (not present on
    /// every server this client may talk to -- e.g. the pinned dev-server
    /// image predates it, see `codegen/regen.sh`'s pin-policy comment for
    /// why the generated models can be ahead of the live server). A server
    /// without the feature responds `404 Not Found` to this endpoint
    /// entirely (there's no per-item "no segments" vs. "don't know what
    /// MediaSegments is" distinction in the API), so this treats 404 as "no
    /// segments" (`Ok(vec![])`) rather than an error -- callers (skip-intro
    /// UI) can then treat an empty result uniformly as "nothing to skip",
    /// whether that's because the item genuinely has no segments or because
    /// the server doesn't support the feature at all. Any other non-success
    /// status still surfaces as `ApiError::Status`/`ApiError::Unauthorized`
    /// as usual.
    pub async fn get_media_segments(
        &self,
        item_id: &str,
        include_types: &[&str],
    ) -> Result<Vec<MediaSegmentDto>, ApiError> {
        let url = format!(
            "{}/MediaSegments/{}",
            self.inner.base_url,
            percent_encode(item_id)
        );
        let mut query: Vec<(&str, String)> = Vec::new();
        if !include_types.is_empty() {
            query.push(("includeSegmentTypes", include_types.join(",")));
        }
        let resp = self
            .inner
            .http
            .get(&url)
            .header("Authorization", self.auth_header())
            .query(&query)
            .send()
            .await
            .map_err(|e| ApiError::Transport(describe_error_chain(&e)))?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(Vec::new());
        }
        let resp = check_status(resp).await?;
        let result: MediaSegmentDtoQueryResult =
            read_json_capped(resp, "/MediaSegments/{itemId}").await?;
        Ok(result.items)
    }

    pub async fn report_playback(&self, report: PlaybackReport) -> Result<(), ApiError> {
        let path = match report.kind {
            PlaybackReportKind::Start => "/Sessions/Playing",
            PlaybackReportKind::Progress => "/Sessions/Playing/Progress",
            PlaybackReportKind::Stopped => "/Sessions/Playing/Stopped",
        };
        let url = format!("{}{}", self.inner.base_url, path);
        let request = self
            .inner
            .http
            .post(&url)
            .header("Authorization", self.auth_header());

        let resp = match report.kind {
            PlaybackReportKind::Stopped => request.json(&stop_body(&report)).send().await,
            PlaybackReportKind::Start | PlaybackReportKind::Progress => {
                request.json(&start_progress_body(&report)).send().await
            }
        }
        .map_err(|e| ApiError::Transport(describe_error_chain(&e)))?;

        check_status(resp).await?;
        Ok(())
    }

    /// Builds a direct-fetch image URL: `/Items/{id}/Images/{kind}?tag=...&ApiKey=...`.
    ///
    /// `ApiKey` is the only query-param form Jellyfin 12 accepts (`api_key`
    /// was legacy authorization, removed by default in 12.0); these URLs
    /// are fetched by clients that take a URL, not a header.
    ///
    /// **Carries the raw access token in the URL's `ApiKey` query param** --
    /// that's simply how Jellyfin's image endpoint
    /// authenticates a plain `<img src=...>`/URLSession fetch, no
    /// `Authorization` header involved. Treat the returned string like a
    /// credential: don't log it at more than debug level, don't persist it
    /// beyond an in-memory cache key, and don't include it in crash
    /// reports/telemetry.
    pub fn image_url(&self, item_id: &str, kind: ImageKind, tag: &str, max_width: u32) -> String {
        let kind_str = match kind {
            ImageKind::Primary => "Primary",
            ImageKind::Backdrop => "Backdrop",
            ImageKind::Thumb => "Thumb",
        };
        // Without `quality`, Jellyfin
        // re-encodes resized images at near-lossless JPEG quality -- measured
        // 95-145KB per 320px poster and 600-780KB per 1280px backdrop on a
        // real library, ~5-8x larger than needed. Over a slow remote link
        // that is the difference between a poster wall filling in under a
        // second and taking tens of seconds. `format=Webp` + per-kind
        // quality (90 for sharp grid art; 80 for backdrops, which
        // render dimmed and/or blurred behind scrims) measured 22-46KB per
        // poster and 75-165KB per backdrop on the same library. A server too
        // old to honor `format`/`quality` simply ignores them and returns
        // JPEG -- the decode path sniffs bytes, not the URL.
        let quality = match kind {
            ImageKind::Backdrop => 80,
            ImageKind::Primary | ImageKind::Thumb => 90,
        };
        format!(
            "{}/Items/{}/Images/{}?tag={}&maxWidth={}&quality={}&format=Webp&ApiKey={}",
            self.inner.base_url,
            percent_encode(item_id),
            kind_str,
            percent_encode(tag),
            max_width,
            quality,
            percent_encode(&self.inner.token),
        )
    }

    /// Builds a trickplay tile-sheet image URL:
    /// `/Videos/{itemId}/Trickplay/{width}/{tileIndex}.jpg?mediaSourceId=...&ApiKey=...`
    /// (`mediaSourceId` only when `media_source_id` is `Some`), per the
    /// `GetTrickplayTileImage` operation in the pinned OpenAPI spec
    /// (`codegen/jellyfin-openapi-stable.json`,
    /// `/Videos/{itemId}/Trickplay/{width}/{index}.jpg`). `width` must be
    /// one of the widths advertised in the item's
    /// `BaseItemDto::trickplay` manifest (`TrickplayInfoDto::width`);
    /// `tile_index` addresses one tile within that width's sprite sheet.
    ///
    /// **Carries the raw access token in the URL's `ApiKey` query param**
    /// (same as [`Self::image_url`], for the same reason: this is fetched
    /// directly, no `Authorization` header involved) -- treat the returned
    /// string like a credential: don't log it above debug level, don't
    /// persist it beyond an in-memory cache key, and don't include it in
    /// crash reports/telemetry.
    pub fn trickplay_tile_url(
        &self,
        item_id: &str,
        width: u32,
        tile_index: u32,
        media_source_id: Option<&str>,
    ) -> String {
        let mut url = format!(
            "{}/Videos/{}/Trickplay/{}/{}.jpg?",
            self.inner.base_url,
            percent_encode(item_id),
            width,
            tile_index,
        );
        if let Some(media_source_id) = media_source_id {
            url.push_str("mediaSourceId=");
            url.push_str(&percent_encode(media_source_id));
            url.push('&');
        }
        url.push_str("ApiKey=");
        url.push_str(&percent_encode(&self.inner.token));
        url
    }

    /// Static direct-play stream: `/Videos/{itemId}/stream?static=true&mediaSourceId=...`.
    /// If the source carries a server-issued `TranscodingUrl` (HLS), that URL is
    /// used instead (resolved against `base_url` if it's relative) — the server's
    /// PlaybackInfo response is what decided transcode vs. direct play, and that
    /// decision (including any HLS query params) must be passed through as-is.
    ///
    /// docs/PLUGIN-CHANNELS.md §2.3, fact 4: `item_id`
    /// and `source.id` are **not interchangeable**. For an ordinary library
    /// item `MediaSourceInfo.id == item id` and the distinction never shows.
    /// For a Jellyfin plugin channel recording (e.g. a TVHeadend tuner
    /// item), the server issues `MediaSources[0].Id` as a short
    /// plugin-scoped id that names a *source*, not an *item* -- building the
    /// `/Videos/{id}/stream` path from it names a non-existent item and the
    /// server answers HTTP 400. The path segment must always be the real
    /// item id; `mediaSourceId` is the only place `source.id` belongs
    /// (`GET /Videos/{itemId}/stream?static=true&mediaSourceId=<short id>`
    /// answers 206 and plays; `GET /Videos/{short id}/stream?...` answers
    /// 400).
    ///
    /// **Carries the raw access token in the URL's `ApiKey` query param**,
    /// same as [`Self::image_url`] and for the same
    /// reason (the player fetches this URL directly, no `Authorization`
    /// header involved) -- treat the returned string as a credential.
    pub fn stream_url(&self, item_id: &str, source: &MediaSourceInfo) -> String {
        if let Some(transcoding_url) = &source.transcoding_url {
            if transcoding_url.starts_with("http://") || transcoding_url.starts_with("https://") {
                return transcoding_url.clone();
            }
            let path = transcoding_url.strip_prefix('/').unwrap_or(transcoding_url);
            return format!("{}/{}", self.inner.base_url, path);
        }

        let media_source_id = match &source.id {
            Some(id) => id.clone(),
            None => {
                // MediaSourceInfo.id is Option in the
                // generated model but should always be set by a
                // well-behaved server; silently building a URL with an
                // empty mediaSourceId produces a request that will just
                // 404/400 with no clue why. Surface it instead.
                tracing::warn!(
                    "stream_url: MediaSourceInfo.id is missing; built URL will have an empty mediaSourceId"
                );
                String::new()
            }
        };
        format!(
            "{}/Videos/{}/stream?static=true&mediaSourceId={}&ApiKey={}",
            self.inner.base_url,
            percent_encode(item_id),
            percent_encode(&media_source_id),
            percent_encode(&self.inner.token),
        )
    }

    /// This client's server base URL (scheme://host[:port], no trailing
    /// slash) -- e.g. as a cache key for per-server measured state.
    pub fn base_url(&self) -> &str {
        &self.inner.base_url
    }

    /// Measure real download throughput from this server in bits/sec via
    /// Jellyfin's own `/Playback/BitrateTest` endpoint: the server streams `test_bytes`
    /// of random data, and wall-clock over received bytes gives the
    /// effective rate. TCP slow-start means small sizes under-measure on
    /// high-RTT links -- callers should use a few MB and treat the result
    /// as a floor, applying their own safety factor.
    pub async fn measure_bitrate(&self, test_bytes: u64) -> Result<u64, ApiError> {
        let url = format!(
            "{}/Playback/BitrateTest?size={}",
            self.inner.base_url, test_bytes
        );
        let t = std::time::Instant::now();
        let resp = self
            .inner
            .http
            .get(&url)
            .header("Authorization", self.auth_header())
            .send()
            .await
            .map_err(|e| ApiError::Transport(describe_error_chain(&e)))?;
        let mut resp = check_status(resp).await?;
        let mut received: u64 = 0;
        while let Some(chunk) = resp
            .chunk()
            .await
            .map_err(|e| ApiError::Transport(e.to_string()))?
        {
            received += chunk.len() as u64;
        }
        let secs = t.elapsed().as_secs_f64();
        if received == 0 || secs <= 0.0 {
            return Err(ApiError::Decode(
                "bitrate test returned no data".to_string(),
            ));
        }
        Ok(((received as f64 * 8.0) / secs) as u64)
    }

    /// Warm the play path for an item the user is *looking at* but hasn't
    /// played yet -- a ranged GET of the first `max_bytes` of the item's
    /// static stream. Two effects measured to dominate cold-start latency
    /// (LATENCY.md): the server reads the file head into OS page cache,    /// this client's pooled HTTP connection is established/kept hot.
    /// Fire-and-forget: callers spawn it and drop the result; any error is
    /// logged at debug (warming is an optimization, never a failure mode).
    pub async fn warm_stream_head(&self, item_id: &str, max_bytes: u64) -> Result<(), ApiError> {
        let url = format!(
            "{}/Videos/{}/stream?static=true&ApiKey={}",
            self.inner.base_url,
            percent_encode(item_id),
            percent_encode(&self.inner.token),
        );
        let resp = self
            .inner
            .http
            .get(&url)
            .header("Range", format!("bytes=0-{}", max_bytes.saturating_sub(1)))
            .send()
            .await
            .map_err(|e| ApiError::Transport(e.to_string()))?;
        // Drain (up to the ranged size) so the server actually reads the
        // file head rather than aborting on our disconnect. Capped rather
        // than `.bytes()`: the Range header is a *request*; a
        // hostile/misbehaving server can ignore it and stream the whole
        // file, which an unbounded drain would buffer. `max_bytes` + slack
        // keeps honest servers' ranged replies intact while bounding the
        // malicious case.
        let cap = usize::try_from(max_bytes.saturating_add(64 * 1024)).unwrap_or(usize::MAX);
        let _ = read_capped_body(resp, cap, "stream-warm head").await?;
        Ok(())
    }

    /// Connect the WebSocket; events arrive on the returned channel until drop.
    /// Reconnection is the CALLER's job (jellyfin-core session) — this is one connection.
    pub async fn connect_ws(&self) -> Result<tokio::sync::mpsc::Receiver<ServerEvent>, ApiError> {
        ws::connect(
            &self.inner.base_url,
            &self.inner.identity,
            &self.inner.token,
        )
        .await
    }
}

#[derive(Debug, Clone, Copy)]
pub enum ImageKind {
    Primary,
    Backdrop,
    Thumb,
}

/// Start/progress/stopped payloads unified; each variant maps to one of the three endpoints.
/// NOTE: volume_level MUST serialize as integer (server 400s otherwise — regression test).
#[derive(Debug, Clone)]
pub struct PlaybackReport {
    pub kind: PlaybackReportKind,
    pub item_id: String,
    pub media_source_id: String,
    pub position_ticks: i64,
    pub is_paused: bool,
    pub volume_level: u8,
    pub audio_stream_index: Option<i32>,
    pub subtitle_stream_index: Option<i32>,
    pub play_session_id: String,
}

#[derive(Debug, Clone, Copy)]
pub enum PlaybackReportKind {
    Start,
    Progress,
    Stopped,
}

/// Decoded WebSocket events Jellybeam cares about; everything else is Ignored(name).
#[derive(Debug, Clone)]
pub enum ServerEvent {
    LibraryChanged {
        added: Vec<String>,
        updated: Vec<String>,
        removed: Vec<String>,
    },
    UserDataChanged {
        item_userdata: Vec<(String, UserItemDataDto)>,
    },
    ForceKeepAlive,
    Ignored(String),
}

/// Request body for `POST /Sessions/Playing` and `POST /Sessions/Playing/Progress`.
///
/// Hand-written (not the generated `PlaybackStartInfo`/`PlaybackProgressInfo`
/// models) so the wire shape sent to the server is fully under our control —
/// in particular `VolumeLevel` is `u8`, which `serde_json` always serializes
/// as a JSON integer. The known sharp edge (docs/OVERVIEW.md §4, jellyfin-desktop's
/// tracker): the server 400s a progress report if `VolumeLevel` arrives as a
/// JSON float. See the `playback_report_serialization` tests below.
#[derive(Debug, serde::Serialize)]
struct StartProgressBody<'a> {
    #[serde(rename = "ItemId")]
    item_id: &'a str,
    #[serde(rename = "MediaSourceId")]
    media_source_id: &'a str,
    #[serde(rename = "PositionTicks")]
    position_ticks: i64,
    #[serde(rename = "IsPaused")]
    is_paused: bool,
    #[serde(rename = "IsMuted")]
    is_muted: bool,
    #[serde(rename = "VolumeLevel")]
    volume_level: u8,
    #[serde(rename = "AudioStreamIndex", skip_serializing_if = "Option::is_none")]
    audio_stream_index: Option<i32>,
    #[serde(
        rename = "SubtitleStreamIndex",
        skip_serializing_if = "Option::is_none"
    )]
    subtitle_stream_index: Option<i32>,
    #[serde(rename = "PlaySessionId")]
    play_session_id: &'a str,
    #[serde(rename = "CanSeek")]
    can_seek: bool,
}

/// Request body for `POST /Sessions/Playing/Stopped`.
#[derive(Debug, serde::Serialize)]
struct StopBody<'a> {
    #[serde(rename = "ItemId")]
    item_id: &'a str,
    #[serde(rename = "MediaSourceId")]
    media_source_id: &'a str,
    #[serde(rename = "PositionTicks")]
    position_ticks: i64,
    #[serde(rename = "PlaySessionId")]
    play_session_id: &'a str,
}

fn start_progress_body(report: &PlaybackReport) -> StartProgressBody<'_> {
    StartProgressBody {
        item_id: &report.item_id,
        media_source_id: &report.media_source_id,
        position_ticks: report.position_ticks,
        is_paused: report.is_paused,
        is_muted: false,
        volume_level: report.volume_level,
        audio_stream_index: report.audio_stream_index,
        subtitle_stream_index: report.subtitle_stream_index,
        play_session_id: &report.play_session_id,
        can_seek: true,
    }
}

fn stop_body(report: &PlaybackReport) -> StopBody<'_> {
    StopBody {
        item_id: &report.item_id,
        media_source_id: &report.media_source_id,
        position_ticks: report.position_ticks,
        play_session_id: &report.play_session_id,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_identity() -> ClientIdentity {
        ClientIdentity {
            client: "Jellybeam".to_string(),
            device: "Test Mac".to_string(),
            device_id: "device-123".to_string(),
            version: "0.1.0".to_string(),
        }
    }

    #[test]
    fn auth_header_without_token() {
        let h = auth_header(&sample_identity(), None);
        assert_eq!(
            h,
            r#"MediaBrowser Client="Jellybeam", Device="Test Mac", DeviceId="device-123", Version="0.1.0""#
        );
    }

    #[test]
    fn auth_header_with_token() {
        let h = auth_header(&sample_identity(), Some("tok"));
        assert_eq!(
            h,
            r#"MediaBrowser Client="Jellybeam", Device="Test Mac", DeviceId="device-123", Version="0.1.0", Token="tok""#
        );
    }

    /// N2 regression test: a device name containing a quote and a
    /// backslash (e.g. a real macOS device name like `Ada's "Gaming" Mac`)
    /// must not be able to prematurely close the quoted `Device="..."`
    /// value or otherwise corrupt the header.
    #[test]
    fn auth_header_escapes_embedded_quotes_and_backslashes() {
        let identity = ClientIdentity {
            client: "Jellybeam".to_string(),
            device: r#"Ada's "Gaming" Mac\Test"#.to_string(),
            device_id: "device-123".to_string(),
            version: "0.1.0".to_string(),
        };
        let h = auth_header(&identity, Some(r#"tok"en"#));
        assert_eq!(
            h,
            r#"MediaBrowser Client="Jellybeam", Device="Ada's \"Gaming\" Mac\\Test", DeviceId="device-123", Version="0.1.0", Token="tok\"en""#
        );
    }

    /// N2 regression test: bare CR/LF in a value (never legitimate here)
    /// must be stripped, not passed through into the header value where it
    /// could be used to inject additional header lines.
    #[test]
    fn auth_header_strips_embedded_crlf() {
        let identity = ClientIdentity {
            client: "Jellybeam".to_string(),
            device: "evil\r\nX-Injected: true".to_string(),
            device_id: "device-123".to_string(),
            version: "0.1.0".to_string(),
        };
        let h = auth_header(&identity, None);
        assert!(!h.contains('\r'));
        assert!(!h.contains('\n'));
        assert_eq!(
            h,
            r#"MediaBrowser Client="Jellybeam", Device="evilX-Injected: true", DeviceId="device-123", Version="0.1.0""#
        );
    }

    fn sample_report(kind: PlaybackReportKind) -> PlaybackReport {
        PlaybackReport {
            kind,
            item_id: "item-1".to_string(),
            media_source_id: "source-1".to_string(),
            position_ticks: 123_456_789,
            is_paused: true,
            volume_level: 42,
            audio_stream_index: Some(1),
            subtitle_stream_index: None,
            play_session_id: "session-1".to_string(),
        }
    }

    /// Regression test for the known server bug (docs/OVERVIEW.md §4): a
    /// PlaybackReport with volume_level=42 must serialize VolumeLevel as
    /// the exact JSON integer `42`, never `42.0`. Asserts the *exact* JSON,
    /// not just that it round-trips, because serde_json would happily
    /// serialize an f64-typed field holding 42.0 in a way that's easy to
    /// eyeball as "fine" but that the server's int32 model binder 400s on.
    #[test]
    fn playback_report_serializes_volume_level_as_integer() {
        let report = sample_report(PlaybackReportKind::Progress);
        let value = serde_json::to_value(start_progress_body(&report)).expect("serializes");
        assert_eq!(value["VolumeLevel"], serde_json::json!(42));
        assert!(
            value["VolumeLevel"].is_u64(),
            "VolumeLevel must be a JSON integer, got {value}"
        );

        let raw = serde_json::to_string(&value).expect("serializes");
        assert!(raw.contains(r#""VolumeLevel":42"#), "raw JSON: {raw}");
        assert!(!raw.contains("42.0"), "raw JSON: {raw}");
    }

    #[test]
    fn playback_report_start_progress_body_exact_json() {
        let report = sample_report(PlaybackReportKind::Start);
        let value = serde_json::to_value(start_progress_body(&report)).expect("serializes");
        assert_eq!(
            value,
            serde_json::json!({
                "ItemId": "item-1",
                "MediaSourceId": "source-1",
                "PositionTicks": 123_456_789,
                "IsPaused": true,
                "IsMuted": false,
                "VolumeLevel": 42,
                "AudioStreamIndex": 1,
                "PlaySessionId": "session-1",
                "CanSeek": true,
            })
        );
    }

    #[test]
    fn playback_report_stop_body_exact_json() {
        let report = sample_report(PlaybackReportKind::Stopped);
        let value = serde_json::to_value(stop_body(&report)).expect("serializes");
        assert_eq!(
            value,
            serde_json::json!({
                "ItemId": "item-1",
                "MediaSourceId": "source-1",
                "PositionTicks": 123_456_789,
                "PlaySessionId": "session-1",
            })
        );
    }

    #[test]
    fn image_url_builds_expected_query() {
        let client = JellyfinClient::from_token(
            "http://localhost:8096",
            sample_identity(),
            "tok en/with?special",
        );
        let url = client.image_url("item 1", ImageKind::Primary, "tag&1", 300);
        assert_eq!(
            url,
            "http://localhost:8096/Items/item%201/Images/Primary?tag=tag%261&maxWidth=300&quality=90&format=Webp&ApiKey=tok%20en%2Fwith%3Fspecial"
        );
    }

    #[test]
    fn image_url_kinds() {
        let client = JellyfinClient::from_token("http://localhost:8096", sample_identity(), "t");
        assert!(client
            .image_url("i", ImageKind::Backdrop, "tag", 100)
            .contains("/Images/Backdrop?"));
        assert!(client
            .image_url("i", ImageKind::Thumb, "tag", 100)
            .contains("/Images/Thumb?"));
    }

    /// Backdrops render dimmed and/or
    /// blurred, so they ship at a lower re-encode quality than sharp grid
    /// art. Measured 600-780KB -> 75-165KB per 1280px backdrop.
    #[test]
    fn image_url_quality_is_lower_for_backdrops_than_sharp_art() {
        let client = JellyfinClient::from_token("http://localhost:8096", sample_identity(), "t");
        assert!(client
            .image_url("i", ImageKind::Backdrop, "tag", 1280)
            .contains("quality=80"));
        assert!(client
            .image_url("i", ImageKind::Primary, "tag", 320)
            .contains("quality=90"));
        assert!(client
            .image_url("i", ImageKind::Thumb, "tag", 400)
            .contains("quality=90"));
    }

    // --- trickplay_tile_url ---------------------------------------

    #[test]
    fn trickplay_tile_url_without_media_source_id() {
        let client = JellyfinClient::from_token(
            "http://localhost:8096",
            sample_identity(),
            "tok en/with?special",
        );
        let url = client.trickplay_tile_url("item 1", 320, 7, None);
        assert_eq!(
            url,
            "http://localhost:8096/Videos/item%201/Trickplay/320/7.jpg?ApiKey=tok%20en%2Fwith%3Fspecial"
        );
    }

    #[test]
    fn trickplay_tile_url_with_media_source_id() {
        let client = JellyfinClient::from_token("http://localhost:8096", sample_identity(), "tok");
        let url = client.trickplay_tile_url("item-1", 320, 7, Some("source 1&x"));
        assert_eq!(
            url,
            "http://localhost:8096/Videos/item-1/Trickplay/320/7.jpg?mediaSourceId=source%201%26x&ApiKey=tok"
        );
    }

    #[test]
    fn stream_url_direct_play_uses_static_videos_endpoint() {
        let client = JellyfinClient::from_token("http://localhost:8096", sample_identity(), "tok");
        let source = MediaSourceInfo {
            id: Some("src-1".to_string()),
            ..Default::default()
        };
        let url = client.stream_url("src-1", &source);
        assert_eq!(
            url,
            "http://localhost:8096/Videos/src-1/stream?static=true&mediaSourceId=src-1&ApiKey=tok"
        );
    }

    /// docs/PLUGIN-CHANNELS.md §2.3, §3: a Jellyfin
    /// plugin channel recording (e.g. a TVHeadend tuner item) has
    /// `MediaSources[0].Id` set to a short plugin-issued id that is NOT the
    /// item id. The stream path segment must be the item id (or the server
    /// 400s -- observed on device); only the `mediaSourceId` query param
    /// uses `source.id`.
    #[test]
    fn stream_url_uses_item_id_for_path_and_source_id_for_query_when_they_differ() {
        let client = JellyfinClient::from_token("http://localhost:8096", sample_identity(), "tok");
        let source = MediaSourceInfo {
            id: Some("ab12cd34".to_string()),
            ..Default::default()
        };
        let url = client.stream_url("item-1", &source);
        assert_eq!(
            url,
            "http://localhost:8096/Videos/item-1/stream?static=true&mediaSourceId=ab12cd34&ApiKey=tok"
        );
    }

    /// N4 regression test: a missing `MediaSourceInfo.id` (server bug, or a
    /// caller passing a source it built itself) must not panic or make
    /// `stream_url` fallible -- it's `-> String`, infallible by design (see
    /// the doc comment on `stream_url`) -- so the best available behavior
    /// is a (still-broken, server will 400/404 it) URL with an empty
    /// `mediaSourceId`, plus a `tracing::warn!` for whoever's watching
    /// logs. This test only asserts the non-panicking, URL-still-built
    /// half; the warn itself isn't asserted (would need a test-only
    /// tracing subscriber dependency this crate doesn't otherwise need).
    #[test]
    fn stream_url_with_missing_media_source_id_still_builds_a_url() {
        let client = JellyfinClient::from_token("http://localhost:8096", sample_identity(), "tok");
        let source = MediaSourceInfo {
            id: None,
            ..Default::default()
        };
        let url = client.stream_url("item-1", &source);
        assert_eq!(
            url,
            "http://localhost:8096/Videos/item-1/stream?static=true&mediaSourceId=&ApiKey=tok"
        );
    }

    // --- N6: ApiError::Status body capture -----------------------------

    #[test]
    fn api_error_status_display_includes_body_when_present() {
        let err = ApiError::Status {
            code: 404,
            body: "item not found".to_string(),
        };
        assert_eq!(err.to_string(), "http status 404: item not found");
    }

    #[test]
    fn api_error_status_display_omits_colon_when_body_empty() {
        let err = ApiError::Status {
            code: 500,
            body: String::new(),
        };
        assert_eq!(err.to_string(), "http status 500");
    }

    #[tokio::test]
    async fn capped_body_text_truncates_at_the_byte_cap() {
        // Build a body well over the cap, verify the captured text is
        // bounded and marked as truncated rather than growing unbounded.
        let big_body = "x".repeat(STATUS_ERROR_BODY_CAP * 2);
        let server = httpbin_style_server(big_body.clone()).await;
        let resp = reqwest::get(&server)
            .await
            .expect("request the mock server");
        let text = capped_body_text(resp, STATUS_ERROR_BODY_CAP).await;
        assert!(
            text.len() <= STATUS_ERROR_BODY_CAP + "... [truncated]".len(),
            "captured body should stay near the {STATUS_ERROR_BODY_CAP}-byte cap, got {} bytes",
            text.len()
        );
        assert!(
            text.ends_with("... [truncated]"),
            "truncated body should be marked as such: {text}"
        );
    }

    #[tokio::test]
    async fn capped_body_text_returns_short_body_unmodified() {
        let server = httpbin_style_server("short error body".to_string()).await;
        let resp = reqwest::get(&server)
            .await
            .expect("request the mock server");
        let text = capped_body_text(resp, STATUS_ERROR_BODY_CAP).await;
        assert_eq!(text, "short error body");
    }

    #[tokio::test]
    async fn capped_body_text_stops_before_the_server_finishes_the_body() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let address = listener.local_addr().expect("address");
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept");
            let _ = stream.read(&mut [0; 4096]).await;
            stream
                .write_all(b"HTTP/1.1 500 Error\r\nContent-Length: 1000000\r\n\r\n")
                .await
                .expect("headers");
            stream.write_all(b"abcde").await.expect("body prefix");
            std::future::pending::<()>().await;
        });
        let response = reqwest::get(format!("http://{address}"))
            .await
            .expect("get");
        let result =
            tokio::time::timeout(Duration::from_secs(1), capped_body_text(response, 4)).await;
        server.abort();
        assert_eq!(
            result.expect("must not wait for the rest of the body"),
            "abcd... [truncated]"
        );
    }

    #[tokio::test]
    async fn capped_body_text_preserves_utf8_at_the_boundary() {
        let server = httpbin_style_server("abéz".to_string()).await;
        let response = reqwest::get(server).await.expect("get");
        assert_eq!(capped_body_text(response, 3).await, "ab... [truncated]");
    }

    /// Minimal one-shot HTTP server (raw TCP, no framework) that returns
    /// `body` for the first request it receives on `GET /`, then stops.
    /// Returns the URL to hit. Local-only (127.0.0.1, ephemeral port); does
    /// not touch the dev Jellyfin server.
    async fn httpbin_style_server(body: String) -> String {
        use tokio::io::AsyncWriteExt;
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind local listener");
        let addr = listener.local_addr().expect("local_addr");
        tokio::spawn(async move {
            if let Ok((mut stream, _)) = listener.accept().await {
                // Drain the request (best-effort; we don't need to parse it).
                let mut discard = [0u8; 1024];
                let _ = tokio::io::AsyncReadExt::read(&mut stream, &mut discard).await;
                let response = format!(
                    "HTTP/1.1 500 Internal Server Error\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(response.as_bytes()).await;
                let _ = stream.shutdown().await;
            }
        });
        format!("http://{addr}/")
    }

    #[test]
    fn stream_url_transcode_passes_through_absolute_url() {
        let client = JellyfinClient::from_token("http://localhost:8096", sample_identity(), "tok");
        let source = MediaSourceInfo {
            transcoding_url: Some("https://other-host/hls/master.m3u8?a=1".to_string()),
            ..Default::default()
        };
        assert_eq!(
            client.stream_url("item-1", &source),
            "https://other-host/hls/master.m3u8?a=1"
        );
    }

    #[test]
    fn stream_url_transcode_resolves_relative_url_against_base() {
        let client = JellyfinClient::from_token("http://localhost:8096", sample_identity(), "tok");
        let source = MediaSourceInfo {
            transcoding_url: Some("/videos/1/master.m3u8?DeviceId=x".to_string()),
            ..Default::default()
        };
        assert_eq!(
            client.stream_url("item-1", &source),
            "http://localhost:8096/videos/1/master.m3u8?DeviceId=x"
        );
    }

    #[test]
    fn from_token_trims_trailing_slash() {
        let client = JellyfinClient::from_token("http://localhost:8096/", sample_identity(), "tok");
        assert_eq!(client.inner.base_url, "http://localhost:8096");
    }

    // --- Model deserialization from recorded fixtures -----------------

    #[test]
    fn deserializes_base_item_dto_fixture() {
        let raw = include_str!("../tests/fixtures/base_item_dto.json");
        let item: BaseItemDto = serde_json::from_str(raw).expect("deserializes");
        assert_eq!(
            item.id,
            Some(uuid::Uuid::parse_str("e2f5a5f1-1a0b-4b3a-9c2e-000000000001").expect("uuid"))
        );
        assert_eq!(item.name.as_deref(), Some("Sample Movie"));
    }

    #[test]
    fn deserializes_base_item_dto_with_unknown_fields() {
        // Hard requirement: unknown fields from server drift must never
        // fail deserialization (no deny_unknown_fields anywhere).
        let raw = include_str!("../tests/fixtures/base_item_dto_with_unknown_fields.json");
        let item: BaseItemDto =
            serde_json::from_str(raw).expect("deserializes despite unknown fields");
        assert_eq!(item.name.as_deref(), Some("Sample Movie"));
    }

    /// B1 regression test: an unrecognized `Type` value (e.g. a BaseItemKind
    /// the server added after this crate's spec was pinned) must not fail
    /// deserialization of the item -- it should fall back to
    /// `BaseItemKind::Unrecognized` via the enum's `#[serde(other)]` arm
    /// (see `codegen/postprocess_enums.py`), not bubble up as an error.
    #[test]
    fn base_item_dto_with_unknown_type_deserializes_to_unrecognized() {
        let raw = include_str!("../tests/fixtures/base_item_dto_unknown_type.json");
        let item: BaseItemDto =
            serde_json::from_str(raw).expect("deserializes despite unknown Type");
        assert_eq!(item.name.as_deref(), Some("Item From The Future"));
        assert_eq!(item.type_, Some(BaseItemKind::Unrecognized));
    }

    /// Regression test: a full `ItemsResult` (`/Items`, `/UserViews`,
    /// `/UserItems/Resume`, `/Shows/NextUp` response shape) must deserialize
    /// completely even when one item among several has drifted -- an
    /// unrecognized BaseItemKind AND an unrecognized nested MediaStreamType
    /// -- rather than failing the whole page over one bad item. Without the
    /// #[serde(other)] fallback, one drifted item anywhere in the page
    /// would fail the entire `get_items`/`get_user_views`/etc. call.
    #[test]
    fn items_result_with_one_drifted_item_deserializes_fully() {
        let raw = include_str!("../tests/fixtures/items_result_with_drifted_item.json");
        let result: ItemsResult = serde_json::from_str(raw).expect("deserializes despite drift");
        assert_eq!(result.total_record_count, Some(3));
        assert_eq!(
            result.items.len(),
            3,
            "all three items must be present, drifted one included"
        );

        assert_eq!(result.items[0].type_, Some(BaseItemKind::Movie));
        assert_eq!(result.items[2].type_, Some(BaseItemKind::Episode));

        let drifted = &result.items[1];
        assert_eq!(
            drifted.name.as_deref(),
            Some("Drifted Item From A Newer Server")
        );
        assert_eq!(drifted.type_, Some(BaseItemKind::Unrecognized));
        let stream = drifted
            .media_streams
            .first()
            .expect("drifted item keeps its MediaStreams");
        assert_eq!(stream.type_, Some(models::MediaStreamType::Unrecognized));
    }

    #[test]
    fn deserializes_playback_info_response_fixture() {
        let raw = include_str!("../tests/fixtures/playback_info_response.json");
        let resp: PlaybackInfoResponse = serde_json::from_str(raw).expect("deserializes");
        assert_eq!(resp.media_sources.len(), 1);
        assert_eq!(resp.media_sources[0].media_streams.len(), 2);
        assert!(resp.play_session_id.is_some());
    }

    #[test]
    fn deserializes_authentication_result_fixture() {
        let raw = include_str!("../tests/fixtures/authentication_result.json");
        let result: AuthenticationResult = serde_json::from_str(raw).expect("deserializes");
        assert!(result.access_token.is_some());
        assert!(result.user.is_some());
    }

    // --- mock-server helpers ---------------------------------------

    /// One-shot local HTTP server that captures the raw request (request
    /// line + headers, read up to the end of headers) it receives    /// responds `200 OK` with `body` as `application/json`. Returns the
    /// base URL to hit and a receiver that yields the captured request text
    /// once a connection lands. Local-only (127.0.0.1, ephemeral port);
    /// does not touch the dev Jellyfin server. Modeled on
    /// `httpbin_style_server` above, extended to capture the request so
    /// tests can assert on query-string shape (e.g. `sortOrder`).
    async fn capturing_json_server(
        body: String,
    ) -> (String, tokio::sync::oneshot::Receiver<String>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind local listener");
        let addr = listener.local_addr().expect("local_addr");
        let (tx, rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            if let Ok((mut stream, _)) = listener.accept().await {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                loop {
                    let n = stream.read(&mut chunk).await.unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                        break;
                    }
                }
                let request_text = String::from_utf8_lossy(&buf).to_string();
                let _ = tx.send(request_text);
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(response.as_bytes()).await;
                let _ = stream.shutdown().await;
            }
        });
        (format!("http://{addr}"), rx)
    }

    /// One-shot local HTTP server that captures the raw request and responds
    /// with an arbitrary `status_line` (e.g. `"404 Not Found"`) and `body`.
    /// Generalizes `capturing_json_server` (always `200 OK`) for tests that
    /// need to drive a specific non-2xx status through the real HTTP path.
    /// Local-only (127.0.0.1, ephemeral port); does not touch the dev
    /// Jellyfin server.
    async fn capturing_status_server(
        status_line: &str,
        body: String,
    ) -> (String, tokio::sync::oneshot::Receiver<String>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind local listener");
        let addr = listener.local_addr().expect("local_addr");
        let (tx, rx) = tokio::sync::oneshot::channel();
        let status_line = status_line.to_string();
        tokio::spawn(async move {
            if let Ok((mut stream, _)) = listener.accept().await {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                loop {
                    let n = stream.read(&mut chunk).await.unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                        break;
                    }
                }
                let request_text = String::from_utf8_lossy(&buf).to_string();
                let _ = tx.send(request_text);
                let response = format!(
                    "HTTP/1.1 {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    status_line,
                    body.len(),
                    body
                );
                let _ = stream.write_all(response.as_bytes()).await;
                let _ = stream.shutdown().await;
            }
        });
        (format!("http://{addr}"), rx)
    }

    // --- MediaSegments (skip-intro/credits) ------------------------

    /// Fixture-based deserialization: a recorded `/MediaSegments/{itemId}`
    /// response with one Intro and one Outro segment, each carrying a
    /// tick-based [start, end) range, must deserialize completely --
    /// including the enum `Type` field resolving to the real
    /// `MediaSegmentType` variants (not falling back to `Unrecognized`,
    /// which would indicate the enum's wire values drifted from the spec).
    #[test]
    fn deserializes_media_segments_result_fixture() {
        let raw = include_str!("../tests/fixtures/media_segments_result.json");
        let result: MediaSegmentDtoQueryResult = serde_json::from_str(raw).expect("deserializes");
        assert_eq!(result.total_record_count, Some(2));
        assert_eq!(result.items.len(), 2);

        let intro = &result.items[0];
        assert_eq!(intro.type_, Some(models::MediaSegmentType::Intro));
        assert_eq!(intro.start_ticks, Some(0));
        assert_eq!(intro.end_ticks, Some(900_000_000));

        let outro = &result.items[1];
        assert_eq!(outro.type_, Some(models::MediaSegmentType::Outro));
        assert_eq!(outro.start_ticks, Some(68_400_000_000));
        assert_eq!(outro.end_ticks, Some(72_000_000_000));
    }

    #[tokio::test]
    async fn get_media_segments_builds_expected_path_and_query() {
        let raw = include_str!("../tests/fixtures/media_segments_result.json");
        let (base_url, rx) = capturing_json_server(raw.to_string()).await;
        let client = JellyfinClient::from_token(&base_url, sample_identity(), "tok");

        let segments = client
            .get_media_segments("item 1", &["Intro", "Outro"])
            .await
            .expect("get_media_segments against mock server");
        assert_eq!(segments.len(), 2);

        let request = rx.await.expect("mock server captured a request");
        let request_line = request.lines().next().unwrap_or_default();
        assert!(
            request_line.starts_with("GET /MediaSegments/item%201?"),
            "request line missing percent-encoded item id path: {request_line}"
        );
        assert!(
            request_line.contains("includeSegmentTypes=Intro%2COutro"),
            "request line missing includeSegmentTypes: {request_line}"
        );
    }

    #[tokio::test]
    async fn get_media_segments_omits_query_when_include_types_empty() {
        let raw = include_str!("../tests/fixtures/media_segments_result.json");
        let (base_url, rx) = capturing_json_server(raw.to_string()).await;
        let client = JellyfinClient::from_token(&base_url, sample_identity(), "tok");

        client
            .get_media_segments("item-1", &[])
            .await
            .expect("get_media_segments against mock server");

        let request = rx.await.expect("mock server captured a request");
        let request_line = request.lines().next().unwrap_or_default();
        assert!(
            request_line.starts_with("GET /MediaSegments/item-1"),
            "unexpected request line: {request_line}"
        );
        assert!(
            !request_line.contains("includeSegmentTypes"),
            "request line should omit includeSegmentTypes when include_types is empty: {request_line}"
        );
    }

    /// A server without the MediaSegments feature (e.g. the pinned dev
    /// server image, older than the pinned spec -- see `codegen/regen.sh`)
    /// responds 404 to this endpoint entirely. `get_media_segments` must
    /// treat that as "no segments" (`Ok(vec![])`), not surface it as an
    /// `ApiError`, so skip-intro UI can treat both "genuinely no segments"
    /// and "server doesn't support the feature" the same way.
    #[tokio::test]
    async fn get_media_segments_returns_empty_on_404() {
        let (base_url, _rx) =
            capturing_status_server("404 Not Found", r#"{"title":"Not Found"}"#.to_string()).await;
        let client = JellyfinClient::from_token(&base_url, sample_identity(), "tok");

        let segments = client
            .get_media_segments("item-1", &[])
            .await
            .expect("404 must be translated to Ok(vec![]), not an error");
        assert!(segments.is_empty());
    }

    /// A non-404 failure (e.g. 500, or an auth failure) must still surface
    /// as a real error -- only 404 gets the "feature not supported"
    /// treatment.
    #[tokio::test]
    async fn get_media_segments_surfaces_non_404_errors() {
        let (base_url, _rx) =
            capturing_status_server("500 Internal Server Error", "boom".to_string()).await;
        let client = JellyfinClient::from_token(&base_url, sample_identity(), "tok");

        let err = client
            .get_media_segments("item-1", &[])
            .await
            .expect_err("500 must surface as an error, not Ok(vec![])");
        assert!(
            matches!(err, ApiError::Status { code: 500, .. }),
            "unexpected error variant: {err:?}"
        );
    }

    // --- ItemQuery.sort_order -> `sortOrder` query param -----------

    #[tokio::test]
    async fn get_items_sends_sort_order_when_set() {
        let (base_url, rx) = capturing_json_server("{}".to_string()).await;
        let client = JellyfinClient::from_token(&base_url, sample_identity(), "tok");
        let query = ItemQuery {
            sort_by: Some("SortName".to_string()),
            sort_order: Some("Descending".to_string()),
            ..ItemQuery::new()
        };
        client
            .get_items(&query)
            .await
            .expect("get_items against mock server");
        let request = rx.await.expect("mock server captured a request");
        let request_line = request.lines().next().unwrap_or_default();
        assert!(
            request_line.contains("sortBy=SortName"),
            "request line missing sortBy: {request_line}"
        );
        assert!(
            request_line.contains("sortOrder=Descending"),
            "request line missing sortOrder: {request_line}"
        );
    }

    #[tokio::test]
    async fn get_items_omits_sort_order_when_unset() {
        let (base_url, rx) = capturing_json_server("{}".to_string()).await;
        let client = JellyfinClient::from_token(&base_url, sample_identity(), "tok");
        let query = ItemQuery {
            sort_by: Some("SortName".to_string()),
            ..ItemQuery::new()
        };
        client
            .get_items(&query)
            .await
            .expect("get_items against mock server");
        let request = rx.await.expect("mock server captured a request");
        let request_line = request.lines().next().unwrap_or_default();
        assert!(
            !request_line.contains("sortOrder"),
            "request line should omit sortOrder when None: {request_line}"
        );
    }

    // --- docs/PLUGIN-CHANNELS.md §2.2/§3: live_children

    /// Live-browse contract, `LiveSort::ServerOrder`: the request must carry
    /// `parentId`, `recursive=false`, and no `sortBy`/`sortOrder` at all --
    /// "whatever the plugin returns right now" is the only order that is
    /// ever correct for DVR content.
    #[tokio::test]
    async fn live_children_server_order_sends_non_recursive_query_with_no_sort() {
        let (base_url, rx) = capturing_json_server("{}".to_string()).await;
        let client = JellyfinClient::from_token(&base_url, sample_identity(), "tok");

        client
            .live_children("channel-1", 0, 50, LiveSort::ServerOrder)
            .await
            .expect("live_children against mock server");

        let request = rx.await.expect("mock server captured a request");
        let request_line = request.lines().next().unwrap_or_default();
        assert!(
            request_line.contains("parentId=channel-1"),
            "request line missing parentId: {request_line}"
        );
        assert!(
            request_line.contains("recursive=false"),
            "request line must be non-recursive: {request_line}"
        );
        assert!(
            !request_line.contains("sortBy"),
            "ServerOrder must send no sortBy: {request_line}"
        );
        assert!(
            !request_line.contains("sortOrder"),
            "ServerOrder must send no sortOrder: {request_line}"
        );
    }

    /// `LiveSort::NameAsc` must send `sortBy=SortName&sortOrder=Ascending`.
    #[tokio::test]
    async fn live_children_name_asc_sends_sort_name_ascending() {
        let (base_url, rx) = capturing_json_server("{}".to_string()).await;
        let client = JellyfinClient::from_token(&base_url, sample_identity(), "tok");

        client
            .live_children("channel-1", 0, 50, LiveSort::NameAsc)
            .await
            .expect("live_children against mock server");

        let request = rx.await.expect("mock server captured a request");
        let request_line = request.lines().next().unwrap_or_default();
        assert!(
            request_line.contains("recursive=false"),
            "request line must be non-recursive: {request_line}"
        );
        assert!(
            request_line.contains("sortBy=SortName"),
            "NameAsc must sort by SortName: {request_line}"
        );
        assert!(
            request_line.contains("sortOrder=Ascending"),
            "NameAsc must sort ascending: {request_line}"
        );
    }

    /// `LiveSort::NewestFirst` must send
    /// `sortBy=PremiereDate&sortOrder=Descending`.
    #[tokio::test]
    async fn live_children_newest_first_sends_sort_premiere_date_descending() {
        let (base_url, rx) = capturing_json_server("{}".to_string()).await;
        let client = JellyfinClient::from_token(&base_url, sample_identity(), "tok");

        client
            .live_children("folder-1", 0, 50, LiveSort::NewestFirst)
            .await
            .expect("live_children against mock server");

        let request = rx.await.expect("mock server captured a request");
        let request_line = request.lines().next().unwrap_or_default();
        assert!(
            request_line.contains("recursive=false"),
            "request line must be non-recursive: {request_line}"
        );
        assert!(
            request_line.contains("sortBy=PremiereDate"),
            "NewestFirst must sort by PremiereDate: {request_line}"
        );
        assert!(
            request_line.contains("sortOrder=Descending"),
            "NewestFirst must sort descending: {request_line}"
        );
    }

    /// The response's items must come straight through, in server order.
    #[tokio::test]
    async fn live_children_maps_response_items() {
        let body = serde_json::json!({
            "Items": [
                { "Id": "11111111-1111-1111-1111-111111111111", "Name": "Day 1", "Type": "ChannelFolderItem" },
                { "Id": "22222222-2222-2222-2222-222222222222", "Name": "Day 2", "Type": "ChannelFolderItem" },
            ],
            "TotalRecordCount": 2
        })
        .to_string();
        let (base_url, _rx) = capturing_json_server(body).await;
        let client = JellyfinClient::from_token(&base_url, sample_identity(), "tok");

        let items = client
            .live_children("channel-1", 0, 50, LiveSort::ServerOrder)
            .await
            .expect("live_children against mock server");

        assert_eq!(items.len(), 2);
        assert_eq!(items[0].name.as_deref(), Some("Day 1"));
        assert_eq!(items[1].name.as_deref(), Some("Day 2"));
    }

    // --- media-cache reconciliation fix: ItemQuery.is_missing -> `isMissing`

    #[tokio::test]
    async fn get_items_sends_is_missing_when_set() {
        let (base_url, rx) = capturing_json_server("{}".to_string()).await;
        let client = JellyfinClient::from_token(&base_url, sample_identity(), "tok");
        let query = ItemQuery {
            is_missing: Some(true),
            ..ItemQuery::new()
        };
        client
            .get_items(&query)
            .await
            .expect("get_items against mock server");
        let request = rx.await.expect("mock server captured a request");
        let request_line = request.lines().next().unwrap_or_default();
        assert!(
            request_line.contains("isMissing=true"),
            "request line missing isMissing: {request_line}"
        );
    }

    #[tokio::test]
    async fn get_items_omits_is_missing_when_unset() {
        let (base_url, rx) = capturing_json_server("{}".to_string()).await;
        let client = JellyfinClient::from_token(&base_url, sample_identity(), "tok");
        let query = ItemQuery::new();
        client
            .get_items(&query)
            .await
            .expect("get_items against mock server");
        let request = rx.await.expect("mock server captured a request");
        let request_line = request.lines().next().unwrap_or_default();
        assert!(
            !request_line.contains("isMissing"),
            "request line should omit isMissing when None: {request_line}"
        );
    }

    // --- Reconciliation ID sweep: ItemQuery.enable_images / enable_user_data

    /// The sweep's enumeration pages are only cheap if these actually reach
    /// the wire -- a silently-dropped `enableImages=false` would put every
    /// item's image tags and blurhashes back into a 1,000-item page.
    #[tokio::test]
    async fn get_items_sends_enable_images_and_user_data_when_set() {
        let (base_url, rx) = capturing_json_server("{}".to_string()).await;
        let client = JellyfinClient::from_token(&base_url, sample_identity(), "tok");
        let query = ItemQuery {
            enable_images: Some(false),
            enable_user_data: Some(false),
            ..ItemQuery::new()
        };
        client
            .get_items(&query)
            .await
            .expect("get_items against mock server");
        let request = rx.await.expect("mock server captured a request");
        let request_line = request.lines().next().unwrap_or_default();
        assert!(
            request_line.contains("enableImages=false"),
            "request line missing enableImages: {request_line}"
        );
        assert!(
            request_line.contains("enableUserData=false"),
            "request line missing enableUserData: {request_line}"
        );
    }

    /// Pins: `filters` reaches the wire comma-joined (the watched-state catch-up's id-set queries depend on it).
    #[tokio::test]
    async fn get_items_sends_filters_comma_joined() {
        let (base_url, rx) = capturing_json_server("{}".to_string()).await;
        let client = JellyfinClient::from_token(&base_url, sample_identity(), "tok");
        let query = ItemQuery {
            filters: vec!["IsPlayed".to_string(), "IsResumable".to_string()],
            ..ItemQuery::new()
        };
        let _ = client.get_items(&query).await;
        let request = rx.await.expect("mock server captured a request");
        let request_line = request.lines().next().unwrap_or_default();
        assert!(
            request_line.contains("filters=IsPlayed%2CIsResumable"),
            "request line missing filters: {request_line}"
        );
    }

    /// Every other query in the app leaves these `None`, and must keep
    /// sending exactly the bytes it always did.
    #[tokio::test]
    async fn get_items_omits_enable_images_and_user_data_when_unset() {
        let (base_url, rx) = capturing_json_server("{}".to_string()).await;
        let client = JellyfinClient::from_token(&base_url, sample_identity(), "tok");
        client
            .get_items(&ItemQuery::new())
            .await
            .expect("get_items against mock server");
        let request = rx.await.expect("mock server captured a request");
        let request_line = request.lines().next().unwrap_or_default();
        assert!(
            !request_line.contains("enableImages"),
            "request line should omit enableImages when None: {request_line}"
        );
        assert!(
            !request_line.contains("enableUserData"),
            "request line should omit enableUserData when None: {request_line}"
        );
    }

    // --- Incremental delta sync: ItemQuery.min_date_last_saved ------------

    /// The param must go out under BOTH names -- see
    /// `ItemQuery::min_date_last_saved`'s doc comment for the 10.10.x
    /// unbound-`@MinDateLastSavedForUser` HTTP 500 this works around. If a
    /// future spec re-pin or refactor drops the companion param, this fails
    /// here rather than as an intermittent 500 in the field.
    #[tokio::test]
    async fn get_items_sends_both_min_date_last_saved_params_when_set() {
        let (base_url, rx) = capturing_json_server("{}".to_string()).await;
        let client = JellyfinClient::from_token(&base_url, sample_identity(), "tok");
        let query = ItemQuery {
            min_date_last_saved: Some("2026-08-17T07:45:12Z".to_string()),
            ..ItemQuery::new()
        };
        client
            .get_items(&query)
            .await
            .expect("get_items against mock server");
        let request = rx.await.expect("mock server captured a request");
        let request_line = request.lines().next().unwrap_or_default();
        assert!(
            request_line.contains("minDateLastSaved=2026-08-17T07%3A45%3A12Z"),
            "request line missing minDateLastSaved: {request_line}"
        );
        assert!(
            request_line.contains("minDateLastSavedForUser=2026-08-17T07%3A45%3A12Z"),
            "request line missing the minDateLastSavedForUser companion \
             (10.10.x returns HTTP 500 without it): {request_line}"
        );
    }

    #[tokio::test]
    async fn get_items_omits_min_date_last_saved_when_unset() {
        let (base_url, rx) = capturing_json_server("{}".to_string()).await;
        let client = JellyfinClient::from_token(&base_url, sample_identity(), "tok");
        let query = ItemQuery::new();
        client
            .get_items(&query)
            .await
            .expect("get_items against mock server");
        let request = rx.await.expect("mock server captured a request");
        let request_line = request.lines().next().unwrap_or_default();
        assert!(
            !request_line.contains("minDateLastSaved"),
            "request line should omit minDateLastSaved when None: {request_line}"
        );
    }

    // --- get_similar --------------------------------------

    #[tokio::test]
    async fn get_similar_deserializes_items_from_the_fixture() {
        let raw = include_str!("../tests/fixtures/similar_items_result.json");
        let (base_url, rx) = capturing_json_server(raw.to_string()).await;
        let client = JellyfinClient::from_token(&base_url, sample_identity(), "tok");

        let items = client
            .get_similar("item-1", 12)
            .await
            .expect("get_similar against mock server");
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].name.as_deref(), Some("Similar Movie One"));
        assert_eq!(items[1].name.as_deref(), Some("Similar Movie Two"));

        let _ = rx.await;
    }

    #[tokio::test]
    async fn get_similar_builds_expected_path_and_limit_query() {
        let (base_url, rx) = capturing_json_server("{}".to_string()).await;
        let client = JellyfinClient::from_token(&base_url, sample_identity(), "tok");

        client
            .get_similar("item 1", 8)
            .await
            .expect("get_similar against mock server");

        let request = rx.await.expect("mock server captured a request");
        let request_line = request.lines().next().unwrap_or_default();
        assert!(
            request_line.starts_with("GET /Items/item%201/Similar?"),
            "request line missing percent-encoded item id path: {request_line}"
        );
        assert!(
            request_line.contains("limit=8"),
            "request line missing limit: {request_line}"
        );
        assert!(
            !request_line.contains("userId"),
            "request line should omit userId when the client doesn't know one: {request_line}"
        );
    }

    #[tokio::test]
    async fn get_similar_sends_user_id_when_known() {
        let (base_url, rx) = capturing_json_server("{}".to_string()).await;
        let client =
            JellyfinClient::from_token(&base_url, sample_identity(), "tok").with_user_id("user-9");

        client
            .get_similar("item-1", 8)
            .await
            .expect("get_similar against mock server");

        let request = rx.await.expect("mock server captured a request");
        let request_line = request.lines().next().unwrap_or_default();
        assert!(
            request_line.contains("userId=user-9"),
            "request line missing userId: {request_line}"
        );
    }

    #[tokio::test]
    async fn get_similar_surfaces_non_2xx_errors() {
        let (base_url, _rx) =
            capturing_status_server("500 Internal Server Error", "boom".to_string()).await;
        let client = JellyfinClient::from_token(&base_url, sample_identity(), "tok");

        let err = client
            .get_similar("item-1", 8)
            .await
            .expect_err("500 must surface as an error");
        assert!(
            matches!(err, ApiError::Status { code: 500, .. }),
            "unexpected error variant: {err:?}"
        );
    }

    #[test]
    fn item_query_new_matches_default() {
        let a = ItemQuery::new();
        let b = ItemQuery::default();
        assert_eq!(a.parent_id, b.parent_id);
        assert_eq!(a.recursive, b.recursive);
        assert_eq!(a.sort_order, None);
        assert_eq!(a.start_index, 0);
        assert_eq!(a.limit, 0);
    }

    // --- JellyfinClient::user_id -------------------------------------

    #[tokio::test]
    async fn authenticate_by_name_captures_user_id_from_fixture() {
        // Fixture-based: serve the exact recorded AuthenticateByName response
        // (`tests/fixtures/authentication_result.json`, also used by
        // `deserializes_authentication_result_fixture` above) from a local
        // mock server and drive it through the real `authenticate_by_name`
        // HTTP path, rather than only unit-testing the extraction logic.
        let raw = include_str!("../tests/fixtures/authentication_result.json");
        let (base_url, _rx) = capturing_json_server(raw.to_string()).await;
        let (client, result) = JellyfinClient::authenticate_by_name(
            &base_url,
            sample_identity(),
            "jellybeam-admin",
            "jellybeam-test",
        )
        .await
        .expect("authenticate_by_name against mock server");

        let expected_user_id = result
            .user
            .as_ref()
            .and_then(|u| u.id)
            .map(|id| id.to_string())
            .expect("fixture has a User.Id");
        assert_eq!(client.user_id(), Some(expected_user_id.as_str()));
        assert_eq!(
            client.user_id(),
            Some("e2f5a5f1-1a0b-4b3a-9c2e-0000000000aa")
        );
    }

    #[test]
    fn from_token_has_no_user_id_until_with_user_id_is_called() {
        let client = JellyfinClient::from_token("http://localhost:8096", sample_identity(), "tok");
        assert_eq!(client.user_id(), None);

        let client = client.with_user_id("user-123");
        assert_eq!(client.user_id(), Some("user-123"));
    }

    // --- Quick Connect -------------------------------------------------------

    #[tokio::test]
    async fn quick_connect_initiate_returns_code_and_secret_from_fixture() {
        let raw = include_str!("../tests/fixtures/quick_connect_result.json");
        let (base_url, rx) = capturing_json_server(raw.to_string()).await;
        let result = JellyfinClient::quick_connect_initiate(&base_url, &sample_identity())
            .await
            .expect("quick_connect_initiate against mock server");
        assert_eq!(result.code.as_deref(), Some("123456"));
        assert_eq!(result.secret.as_deref(), Some("the-quick-connect-secret"));
        assert_eq!(result.authenticated, Some(false));

        let request = rx.await.expect("captured request");
        assert!(
            request.starts_with("POST /QuickConnect/Initiate"),
            "{request}"
        );
        assert!(
            request
                .to_lowercase()
                .contains("authorization: mediabrowser"),
            "{request}"
        );
        assert!(
            !request.contains("Token="),
            "pre-auth request must not carry a Token clause: {request}"
        );
    }

    #[tokio::test]
    async fn quick_connect_poll_sends_secret_as_query_param() {
        let mut authenticated_raw: serde_json::Value =
            serde_json::from_str(include_str!("../tests/fixtures/quick_connect_result.json"))
                .expect("valid json");
        authenticated_raw["Authenticated"] = serde_json::Value::Bool(true);
        let (base_url, rx) = capturing_json_server(authenticated_raw.to_string()).await;

        let result = JellyfinClient::quick_connect_poll(
            &base_url,
            &sample_identity(),
            "the-quick-connect-secret",
        )
        .await
        .expect("quick_connect_poll against mock server");
        assert_eq!(result.authenticated, Some(true));

        let request = rx.await.expect("captured request");
        assert!(
            request.starts_with("GET /QuickConnect/Connect"),
            "{request}"
        );
        assert!(
            request.contains("secret=the-quick-connect-secret"),
            "{request}"
        );
    }

    #[tokio::test]
    async fn quick_connect_enabled_deserializes_bare_bool() {
        let (base_url, _rx) = capturing_json_server("true".to_string()).await;
        let enabled = JellyfinClient::quick_connect_enabled(&base_url, &sample_identity())
            .await
            .expect("quick_connect_enabled against mock server");
        assert!(enabled);
    }

    #[test]
    fn server_version_parse_table() {
        let ok_cases: &[(&str, ServerVersion)] = &[
            (
                "12.0.0",
                ServerVersion {
                    major: 12,
                    minor: 0,
                    patch: 0,
                },
            ),
            (
                "10.11.10",
                ServerVersion {
                    major: 10,
                    minor: 11,
                    patch: 10,
                },
            ),
            (
                "12.0",
                ServerVersion {
                    major: 12,
                    minor: 0,
                    patch: 0,
                },
            ),
            (
                "10.11.0-rc1",
                ServerVersion {
                    major: 10,
                    minor: 11,
                    patch: 0,
                },
            ),
            (
                "10.11-beta",
                ServerVersion {
                    major: 10,
                    minor: 11,
                    patch: 0,
                },
            ),
        ];
        for (input, expected) in ok_cases {
            assert_eq!(
                input
                    .parse::<ServerVersion>()
                    .unwrap_or_else(|_| panic!("expected {input:?} to parse as a ServerVersion")),
                *expected,
                "parsing {input:?}"
            );
        }

        let err_cases = ["", "12", "not-a-version", "12.", ".0", "v12.0.0"];
        for input in err_cases {
            assert!(
                input.parse::<ServerVersion>().is_err(),
                "expected {input:?} to fail to parse"
            );
        }
    }

    #[test]
    fn server_version_at_least() {
        assert!("12.0.0"
            .parse::<ServerVersion>()
            .expect("parses")
            .at_least(12, 0));
        assert!(!"10.11.0"
            .parse::<ServerVersion>()
            .expect("parses")
            .at_least(12, 0));
        assert!("12.1.0"
            .parse::<ServerVersion>()
            .expect("parses")
            .at_least(12, 0));
    }

    #[test]
    fn server_version_display_round_trips() {
        let v = ServerVersion {
            major: 12,
            minor: 0,
            patch: 0,
        };
        assert_eq!(v.to_string(), "12.0.0");
    }

    #[tokio::test]
    async fn public_system_info_deserializes_and_ignores_unknown_fields() {
        let body = serde_json::json!({
            "Version": "12.0.0",
            "ServerName": "example-server",
            "Id": "00000000-0000-0000-0000-000000000000",
            "StartupWizardCompleted": true,
            "OperatingSystem": "Linux",
            "LocalAddress": "http://192.0.2.1:8096"
        })
        .to_string();
        let (base_url, rx) = capturing_json_server(body).await;

        let info = JellyfinClient::public_system_info(&base_url, &sample_identity())
            .await
            .expect("public_system_info against mock server");
        assert_eq!(info.version.as_deref(), Some("12.0.0"));
        assert_eq!(info.server_name.as_deref(), Some("example-server"));
        assert_eq!(info.startup_wizard_completed, Some(true));
        assert_eq!(
            info.parsed_version(),
            Some(ServerVersion {
                major: 12,
                minor: 0,
                patch: 0
            })
        );

        let request = rx.await.expect("mock server captured a request");
        let request_line = request.lines().next().unwrap_or_default();
        assert!(
            request_line.starts_with("GET /System/Info/Public"),
            "{request_line}"
        );
    }

    #[tokio::test]
    async fn quick_connect_poll_surfaces_404_as_unknown_secret_error() {
        // Unlike get_media_segments's 404-as-empty-vec special case, an
        // unknown/expired Quick Connect secret is a real error the poll
        // loop should stop on -- verify it does NOT get swallowed.
        let (base_url, _rx) =
            capturing_status_server("404 Not Found", r#"{"title":"unknown secret"}"#.to_string())
                .await;
        let result =
            JellyfinClient::quick_connect_poll(&base_url, &sample_identity(), "expired-secret")
                .await;
        assert!(
            matches!(result, Err(ApiError::Status { code: 404, .. })),
            "{result:?}"
        );
    }

    #[tokio::test]
    async fn authenticate_with_quick_connect_posts_secret_and_captures_token() {
        let raw = include_str!("../tests/fixtures/authentication_result.json");
        let (base_url, rx) = capturing_json_server(raw.to_string()).await;
        let (client, result) = JellyfinClient::authenticate_with_quick_connect(
            &base_url,
            sample_identity(),
            "the-quick-connect-secret",
        )
        .await
        .expect("authenticate_with_quick_connect against mock server");

        assert!(result.access_token.is_some());
        assert_eq!(
            client.user_id(),
            Some("e2f5a5f1-1a0b-4b3a-9c2e-0000000000aa")
        );

        let request = rx.await.expect("captured request");
        assert!(
            request.starts_with("POST /Users/AuthenticateWithQuickConnect"),
            "{request}"
        );
    }

    fn url(s: &str) -> reqwest::Url {
        reqwest::Url::parse(s).expect("test url")
    }

    /// The case reqwest's own policy misses: it strips `Authorization` only
    /// on a *cross-host* redirect, so a same-host bounce from `https://` to
    /// `http://` keeps the access token and puts it on the wire in the
    /// clear, silently undoing the user's choice of `https://`.
    #[test]
    fn same_host_https_to_http_redirect_is_a_scheme_downgrade() {
        assert!(is_scheme_downgrade(
            &[url("https://media.example.com/Items")],
            &url("http://media.example.com/Items")
        ));
    }

    /// Only the scheme is judged here -- a cross-host downgrade is refused
    /// for the same reason, and reqwest's own header stripping is not
    /// relied on to cover it.
    #[test]
    fn cross_host_https_to_http_redirect_is_a_scheme_downgrade() {
        assert!(is_scheme_downgrade(
            &[url("https://media.example.com/Items")],
            &url("http://192.0.2.50:8096/Items")
        ));
    }

    /// Everything legitimate still passes: upgrades, same-scheme hops,    /// a plain-`http://` server (the common LAN setup) redirecting within
    /// `http://` -- there is nothing to downgrade there, and refusing it
    /// would break real deployments behind a redirecting reverse proxy.
    #[test]
    fn non_downgrading_redirects_are_allowed() {
        assert!(!is_scheme_downgrade(
            &[url("http://media.example.com/Items")],
            &url("https://media.example.com/Items")
        ));
        assert!(!is_scheme_downgrade(
            &[url("https://media.example.com/Items")],
            &url("https://media.example.com/Items2")
        ));
        assert!(!is_scheme_downgrade(
            &[url("http://192.0.2.50:8096/Items")],
            &url("http://192.0.2.50:8096/Items2")
        ));
        // First hop: nothing has been visited yet.
        assert!(!is_scheme_downgrade(&[], &url("http://example.com/")));
    }

    /// One-shot local HTTP server that answers with `status_line` plus a
    /// `Location` header -- for asserting the real client's redirect
    /// behavior end to end, not just the policy predicate.
    async fn redirecting_server(location: String) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind local listener");
        let addr = listener.local_addr().expect("local_addr");
        tokio::spawn(async move {
            if let Ok((mut stream, _)) = listener.accept().await {
                let mut discard = [0u8; 1024];
                let _ = stream.read(&mut discard).await;
                let response = format!(
                    "HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                );
                let _ = stream.write_all(response.as_bytes()).await;
                let _ = stream.shutdown().await;
            }
        });
        format!("http://{addr}")
    }

    /// The hop limit is unchanged from reqwest's default (10) and ordinary
    /// same-scheme redirects are still followed -- this fix must not turn
    /// into "redirects are off", which would break any server fronted by a
    /// redirecting reverse proxy.
    #[tokio::test]
    async fn an_ordinary_same_scheme_redirect_is_still_followed() {
        let raw = include_str!("../tests/fixtures/similar_items_result.json");
        let (target, _rx) = capturing_json_server(raw.to_string()).await;
        let base_url = redirecting_server(format!("{target}/UserViews")).await;
        let client = JellyfinClient::from_token(&base_url, sample_identity(), "tok");
        let views = client
            .get_user_views()
            .await
            .expect("the redirect to the same scheme is followed");
        assert!(!views.is_empty());
    }

    /// A body under the cap is read and decoded exactly as before.
    #[tokio::test]
    async fn read_capped_body_returns_a_body_that_fits() {
        let (base_url, _rx) = capturing_json_server("[1,2,3]".to_string()).await;
        let resp = reqwest::get(&base_url).await.expect("request");
        let body = read_capped_body(resp, 1024, "/test")
            .await
            .expect("under the cap");
        assert_eq!(body, b"[1,2,3]");
    }

    /// Before this, `Response::json()` buffered the entire body into memory
    /// before serde saw a byte, so a hostile or broken server could answer
    /// any list endpoint with an arbitrarily large body and force a matching
    /// allocation. The read now stops at the ceiling with a clean
    /// `ApiError::Decode`.
    #[tokio::test]
    async fn read_capped_body_refuses_a_body_over_the_cap() {
        let (base_url, _rx) = capturing_json_server("x".repeat(64 * 1024)).await;
        let resp = reqwest::get(&base_url).await.expect("request");
        let err = read_capped_body(resp, 1024, "/test")
            .await
            .expect_err("over the cap");
        assert!(
            matches!(&err, ApiError::Decode(msg) if msg.contains("exceeded")),
            "{err:?}"
        );
    }

    /// Pins the inclusive edge: the cap check is `body.len() + chunk.len() > cap`,
    /// so a body of exactly `cap` bytes is accepted (not `>= cap`).
    #[tokio::test]
    async fn read_capped_body_accepts_a_body_exactly_at_the_cap() {
        const CAP: usize = 4096;
        let (base_url, _rx) = capturing_json_server("x".repeat(CAP)).await;
        let resp = reqwest::get(&base_url).await.expect("request");
        let body = read_capped_body(resp, CAP, "/test")
            .await
            .expect("a body exactly at the cap is accepted");
        assert_eq!(body.len(), CAP);
    }

    /// The tight complement to the exactly-at-cap case above.
    #[tokio::test]
    async fn read_capped_body_refuses_a_body_one_byte_over_the_cap() {
        const CAP: usize = 4096;
        let (base_url, _rx) = capturing_json_server("x".repeat(CAP + 1)).await;
        let resp = reqwest::get(&base_url).await.expect("request");
        let err = read_capped_body(resp, CAP, "/test")
            .await
            .expect_err("one byte over the cap is refused");
        assert!(
            matches!(&err, ApiError::Decode(msg) if msg.contains("exceeded")),
            "{err:?}"
        );
    }
}
