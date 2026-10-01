//! Discover (Seerr/Jellyseerr request management). Built on top of the
//! `seerr-api` crate: `session::{status, connect, open, disconnect}` free
//! functions plus the `SeerrSession` handle's plain `async fn`s.
//!
//! ## Non-negotiable constraints this module (and its siblings under
//! `discover/`) exist to uphold
//!
//! - **Zero startup cost.** Nothing here runs at cold start. [`Root`]
//!   (`root.rs`) calls [`seerr_api::session::status`] -- a local JSON read,
//!   no network -- exactly once per session established
//!   (`main_state_from_bundle`, this app's equivalent of a post-startup
//!   "sidebar metadata" effect), and again only on explicit connect/
//!   disconnect from Settings. No `SeerrSession` is ever opened
//!   (`seerr_api::session::open`, a real network round trip) until the user
//!   actually navigates into Discover or its Settings section.
//! - **No mirror involvement.** Nothing in this module touches
//!   `media_cache`/`Mirror` -- every screen here is a live fetch through a
//!   [`seerr_api::SeerrSession`], exactly like `channel_browse.rs`'s
//!   never-mirrored `ViewKind::Channel` screens (the freshest precedent for
//!   "a live, non-mirrored screen" this app already has -- see that
//!   module's own doc comment).
//! - **Fail open.** A dead/misconfigured Seerr degrades to an inline
//!   message inside the Discover screen only (`DiscoverState::open_error`);
//!   it can never affect Jellyfin browsing/playback/search/startup, since
//!   nothing outside `discover*`/the sidebar's one gating read touches this
//!   module's state at all.
//!
//! ## Module layout
//!
//! This file: state structs (owned by `root.rs::MainState` as
//! `Option<Box<DiscoverState>>`, built lazily on first Discover visit),
//! every pure, unit-testable decision function the spec calls out
//! (availability -> badge, season-picker enablement, sidebar gating,
//! request-button state machine, connect-field validation), and the async
//! control flow itself: an `impl Root` block below (spawning `SeerrSession`
//! calls on the shared tokio runtime, applying their results) that lives in
//! this module rather than in `root.rs`, since Rust allows `impl Root` in
//! any module of the crate and this is the concern that owns the state it
//! operates on. `channel_browse.rs`/`detail.rs` keep the same split: state +
//! pure render + `impl Root` methods, all local to the screen's own module.
//! [`discover_card`] is the one shared rendering helper (poster cell +
//! availability badge) every sub-screen's grid/shelf/rail uses.
//!
//! Sibling files, each one sub-screen's `render()`:
//! `home.rs`, `browse.rs`, `search.rs`, `detail.rs`, `person.rs`,
//! `requests.rs`.

use std::collections::BTreeSet;

use gpui::{
    div, prelude::*, px, rgb, rgba, svg, AnyElement, App, Entity, FontWeight, Pixels, SharedString,
    WeakEntity,
};

use seerr_api::{
    SeerrAuthMethod, SeerrAvailability, SeerrBrowseKind, SeerrCard, SeerrGenre, SeerrHomeRow,
    SeerrMediaType, SeerrMovieDetail, SeerrMyRequest, SeerrPersonCredits, SeerrRequestOptions,
    SeerrSeasonStatus, SeerrSession, SeerrStatus, SeerrTvDetail,
};

use crate::image_store::ImageStore;
use crate::nav::{DiscoverMediaType, DiscoverView, View};
use crate::root::Root;
use crate::text_input::TextInput;
use crate::theme;
use crate::ui::components::clamped_line;

pub(crate) mod browse;
pub(crate) mod detail;
pub(crate) mod home;
pub(crate) mod person;
pub(crate) mod requests;
pub(crate) mod search;

// ---------------------------------------------------------------------
// Pure helpers -- the spec's explicit "pure helpers unit-tested" list.
// ---------------------------------------------------------------------

impl From<DiscoverMediaType> for SeerrMediaType {
    fn from(value: DiscoverMediaType) -> Self {
        match value {
            DiscoverMediaType::Movie => SeerrMediaType::Movie,
            DiscoverMediaType::Tv => SeerrMediaType::Tv,
        }
    }
}

impl From<SeerrMediaType> for DiscoverMediaType {
    fn from(value: SeerrMediaType) -> Self {
        match value {
            SeerrMediaType::Movie => DiscoverMediaType::Movie,
            SeerrMediaType::Tv => DiscoverMediaType::Tv,
        }
    }
}

/// Sidebar gating: "Discover" shows only when Discover is configured for
/// the active account. A one-line wrapper so `root.rs`'s
/// sidebar-row builder reads as intent rather than reaching into
/// `SeerrStatus`'s field directly -- and so this decision has exactly one
/// place a unit test can pin down.
pub(crate) fn sidebar_visible(status: &SeerrStatus) -> bool {
    status.configured
}

/// Corner badge shown on a poster card ("check = available,
/// dot = pending/processing"); `None` means no badge at all
/// (`NotRequested`), reusing `cards.rs`'s "paint nothing" convention rather
/// than an empty box.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AvailabilityBadge {
    /// A filled checkmark -- `cards.rs::watched_check_badge`'s visual
    /// language, reused verbatim (no new colors).
    Available,
    /// A plain accent dot -- pending or actively processing.
    Pending,
}

pub(crate) fn availability_badge(availability: SeerrAvailability) -> Option<AvailabilityBadge> {
    match availability {
        SeerrAvailability::NotRequested => None,
        SeerrAvailability::Available => Some(AvailabilityBadge::Available),
        SeerrAvailability::Pending
        | SeerrAvailability::Processing
        | SeerrAvailability::PartiallyAvailable => Some(AvailabilityBadge::Pending),
    }
}

/// A season row's checkbox state ("season picker where non-requestable
/// seasons are checked+inert"). `selected` is the viewer's
/// own in-progress pick for a still-requestable season; a season the server
/// already reports as unrequestable (already pending/processing/available,
/// or genuinely not requestable for another reason) always renders
/// checked and inert regardless of `selected`, since there is nothing left
/// for the viewer to choose about it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SeasonRowState {
    pub checked: bool,
    pub inert: bool,
}

pub(crate) fn season_row_state(season: &SeerrSeasonStatus, selected: bool) -> SeasonRowState {
    if !season.requestable {
        SeasonRowState {
            checked: true,
            inert: true,
        }
    } else {
        SeasonRowState {
            checked: selected,
            inert: false,
        }
    }
}

/// The detail page's one primary action: "Go to library"
/// wins outright once the item is in the Jellyfin library (regardless of
/// any Seerr-side request state -- the library copy is the whole point of
/// having requested it); otherwise an existing request offers Cancel;
/// otherwise Request when the crate says this account still can; otherwise
/// there is nothing to do here at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PrimaryAction {
    GoToLibrary,
    Cancel {
        request_id: i64,
    },
    Request,
    /// The server has this title but neither has room for a new request
    /// (already fully requested by someone else in a way this account
    /// can't act on, or the server disabled requesting outright) nor an
    /// item to jump to -- the page shows availability only, no button.
    Unavailable,
}

pub(crate) fn primary_action(
    jellyfin_item_id: Option<&str>,
    active_request_id: Option<i64>,
    can_request: bool,
) -> PrimaryAction {
    if jellyfin_item_id.is_some() {
        return PrimaryAction::GoToLibrary;
    }
    if let Some(request_id) = active_request_id {
        return PrimaryAction::Cancel { request_id };
    }
    if can_request {
        return PrimaryAction::Request;
    }
    PrimaryAction::Unavailable
}

/// Field validation before `Root::connect_discover` ever spawns a network
/// probe (Settings -> Discover's Connect button). `Err` carries the exact inline-error
/// sentence the form shows; never a full sentence duplicated at the call
/// site.
pub(crate) fn validate_connect_fields(
    url: &str,
    method: SeerrAuthMethod,
    identity: &str,
    secret: &str,
) -> Result<(), &'static str> {
    if url.trim().is_empty() {
        return Err("Enter a Seerr server address.");
    }
    match method {
        SeerrAuthMethod::ApiKey => {
            if secret.trim().is_empty() {
                return Err("Enter an API key.");
            }
        }
        SeerrAuthMethod::Jellyfin | SeerrAuthMethod::Local => {
            if identity.trim().is_empty() {
                return Err(if method == SeerrAuthMethod::Local {
                    "Enter an email address."
                } else {
                    "Enter a username."
                });
            }
            if secret.trim().is_empty() {
                return Err("Enter a password.");
            }
        }
    }
    Ok(())
}

/// The identity field's label, which changes with `method` ("identity +
/// secret fields").
pub(crate) fn identity_label(method: SeerrAuthMethod) -> &'static str {
    match method {
        SeerrAuthMethod::Jellyfin => "Username",
        SeerrAuthMethod::Local => "Email",
        SeerrAuthMethod::ApiKey => "API Key",
    }
}

pub(crate) fn secret_label(method: SeerrAuthMethod) -> &'static str {
    match method {
        SeerrAuthMethod::Jellyfin | SeerrAuthMethod::Local => "Password",
        SeerrAuthMethod::ApiKey => "API Key",
    }
}

/// Whether the connect form's identity field should show at all -- the API
/// Key method authenticates on the secret field alone (`client.rs`'s
/// `X-Api-Key` header, no separate identity).
pub(crate) fn identity_field_visible(method: SeerrAuthMethod) -> bool {
    !matches!(method, SeerrAuthMethod::ApiKey)
}

/// The stable key every card list de-duplicates by -- a `SeerrMediaType`
/// discriminant paired with the TMDB id, so a movie and a TV show that
/// happen to share a TMDB id stay distinct cards.
fn seerr_card_key(card: &SeerrCard) -> (u8, i64) {
    let media = match card.media_type {
        SeerrMediaType::Movie => 0u8,
        SeerrMediaType::Tv => 1u8,
    };
    (media, card.tmdb_id)
}

/// Drops every later card whose [`seerr_card_key`] already appeared,
/// keeping the first occurrence and the original order otherwise. This
/// matters because Seerr's popularity-sorted pages can repeat an item
/// when the ordering shifts between page requests, and a person's
/// combined cast-plus-crew credits repeat a title when the same person
/// acted in and directed it. Every Discover list surface (browse,
/// search, person credits, each home shelf) runs its fetched cards
/// through this before they reach a rendered grid/shelf.
pub(crate) fn dedup_cards(cards: Vec<SeerrCard>) -> Vec<SeerrCard> {
    let mut seen = std::collections::HashSet::new();
    cards
        .into_iter()
        .filter(|card| seen.insert(seerr_card_key(card)))
        .collect()
}

/// Appends `page` onto `existing`, skipping any card (in `page`, or
/// repeated within `page` itself) whose key already appears in `existing`
/// -- the browse grid's "load more" apply site, where a freshly fetched
/// page can repeat a card the previous page already showed (see
/// [`dedup_cards`]'s doc comment for why).
pub(crate) fn extend_deduped(existing: &mut Vec<SeerrCard>, page: Vec<SeerrCard>) {
    let mut seen: std::collections::HashSet<_> = existing.iter().map(seerr_card_key).collect();
    existing.extend(
        page.into_iter()
            .filter(|card| seen.insert(seerr_card_key(card))),
    );
}

// ---------------------------------------------------------------------
// Screen state -- owned by `root.rs::MainState::discover`.
// ---------------------------------------------------------------------

#[derive(Default)]
pub(crate) struct DiscoverHomeState {
    pub rows: Vec<SeerrHomeRow>,
    pub loading: bool,
    pub error: Option<String>,
    /// Bumped on every fetch; a response applied only if it still matches
    /// (same shape as `channel_browse::ChannelBrowseState::generation`).
    pub generation: u64,
}

pub(crate) struct DiscoverBrowseState {
    pub kind: SeerrBrowseKind,
    pub cards: Vec<SeerrCard>,
    pub page: i64,
    pub total_pages: i64,
    pub genres: Vec<SeerrGenre>,
    pub genre_id: Option<i64>,
    pub sort_by: Option<String>,
    pub loading: bool,
    pub loading_more: bool,
    pub error: Option<String>,
    pub generation: u64,
    /// `uniform_list`'s scroll handle (`grid.rs`'s own recipe) -- needed so
    /// the grid persists its scroll position across renders and so
    /// `should_load_more`'s range callback has somewhere stable to read
    /// from.
    pub scroll: gpui::UniformListScrollHandle,
}

impl DiscoverBrowseState {
    pub(crate) fn new(kind: SeerrBrowseKind) -> Self {
        DiscoverBrowseState {
            kind,
            cards: Vec::new(),
            page: 0,
            total_pages: 1,
            genres: Vec::new(),
            genre_id: None,
            sort_by: None,
            loading: true,
            loading_more: false,
            error: None,
            generation: 0,
            scroll: gpui::UniformListScrollHandle::new(),
        }
    }

    pub(crate) fn exhausted(&self) -> bool {
        self.page >= self.total_pages && self.page > 0
    }
}

/// Seerr search's own field ("field-atop-results per the
/// app's search recipe"). Uses the house `TextInput` component (same
/// component the Connect screen and Settings' Discover connect form use)
/// rather than the main Search overlay's raw-global-keystroke capture --
/// that overlay is a modal that owns every keystroke while open; Discover
/// Search is an ordinary nav'd page, so it wants an ordinary focusable
/// field with its own `on_change` hook driving the debounce below.
pub(crate) struct DiscoverSearchState {
    pub field: Entity<TextInput>,
    pub cards: Vec<SeerrCard>,
    pub loading: bool,
    pub error: Option<String>,
    /// Bumped on every keystroke; the 300ms debounce timer only actually
    /// fires the fetch if this still matches when it wakes up ("300ms
    /// debounce, generation-guarded").
    pub generation: u64,
    pub searched: bool,
}

impl DiscoverSearchState {
    pub(crate) fn new(cx: &mut gpui::Context<Root>) -> Self {
        let root_weak = cx.entity().downgrade();
        DiscoverSearchState {
            field: cx.new(|cx| {
                TextInput::new(cx, "Search movies and TV...").on_change(move |_window, cx| {
                    // Fires from inside the field's own key-down handler,
                    // which already holds this entity's lease -- `cx.defer`
                    // (same reasoning as `root.rs::new_connect_state`'s
                    // `submit` closure) runs the actual dispatch once that
                    // lease is returned.
                    let root_weak = root_weak.clone();
                    cx.defer(move |cx| {
                        let _ = root_weak.update(cx, |root, cx| root.discover_search_changed(cx));
                    });
                })
            }),
            cards: Vec::new(),
            loading: false,
            error: None,
            generation: 0,
            searched: false,
        }
    }
}

pub(crate) enum DiscoverDetailBody {
    Movie(SeerrMovieDetail),
    Tv(SeerrTvDetail),
}

impl DiscoverDetailBody {
    pub(crate) fn card(&self) -> &SeerrCard {
        match self {
            DiscoverDetailBody::Movie(m) => &m.card,
            DiscoverDetailBody::Tv(t) => &t.card,
        }
    }
    pub(crate) fn genres(&self) -> &[SeerrGenre] {
        match self {
            DiscoverDetailBody::Movie(m) => &m.genres,
            DiscoverDetailBody::Tv(t) => &t.genres,
        }
    }
    pub(crate) fn cast(&self) -> &[seerr_api::SeerrPersonRef] {
        match self {
            DiscoverDetailBody::Movie(m) => &m.cast,
            DiscoverDetailBody::Tv(t) => &t.cast,
        }
    }
    pub(crate) fn similar(&self) -> &[SeerrCard] {
        match self {
            DiscoverDetailBody::Movie(m) => &m.similar,
            DiscoverDetailBody::Tv(t) => &t.similar,
        }
    }
    pub(crate) fn recommendations(&self) -> &[SeerrCard] {
        match self {
            DiscoverDetailBody::Movie(m) => &m.recommendations,
            DiscoverDetailBody::Tv(t) => &t.recommendations,
        }
    }
    pub(crate) fn critics_score(&self) -> Option<i32> {
        match self {
            DiscoverDetailBody::Movie(m) => m.critics_score,
            DiscoverDetailBody::Tv(t) => t.critics_score,
        }
    }
    pub(crate) fn audience_score(&self) -> Option<i32> {
        match self {
            DiscoverDetailBody::Movie(m) => m.audience_score,
            DiscoverDetailBody::Tv(t) => t.audience_score,
        }
    }
    pub(crate) fn active_request(&self) -> Option<&seerr_api::SeerrActiveRequest> {
        match self {
            DiscoverDetailBody::Movie(m) => m.active_request.as_ref(),
            DiscoverDetailBody::Tv(t) => t.active_request.as_ref(),
        }
    }
    pub(crate) fn can_request(&self) -> bool {
        match self {
            DiscoverDetailBody::Movie(m) => m.can_request,
            DiscoverDetailBody::Tv(t) => t.can_request,
        }
    }
    pub(crate) fn can_request_4k(&self) -> bool {
        match self {
            DiscoverDetailBody::Movie(m) => m.can_request_4k,
            DiscoverDetailBody::Tv(t) => t.can_request_4k,
        }
    }
    pub(crate) fn seasons(&self) -> &[SeerrSeasonStatus] {
        match self {
            DiscoverDetailBody::Movie(_) => &[],
            DiscoverDetailBody::Tv(t) => &t.seasons,
        }
    }
}

pub(crate) struct DiscoverDetailState {
    pub media_type: DiscoverMediaType,
    pub tmdb_id: i64,
    pub body: Option<DiscoverDetailBody>,
    pub loading: bool,
    pub error: Option<String>,
    pub generation: u64,
    /// TV only -- seasons the viewer has picked to request that aren't
    /// already unrequestable (`season_row_state`'s `selected` input).
    pub selected_seasons: BTreeSet<i32>,
    pub request_sheet: Option<RequestSheetState>,
}

impl DiscoverDetailState {
    pub(crate) fn new(media_type: DiscoverMediaType, tmdb_id: i64) -> Self {
        DiscoverDetailState {
            media_type,
            tmdb_id,
            body: None,
            loading: true,
            error: None,
            generation: 0,
            selected_seasons: BTreeSet::new(),
            request_sheet: None,
        }
    }

    pub(crate) fn matches(&self, media_type: DiscoverMediaType, tmdb_id: i64) -> bool {
        self.media_type == media_type && self.tmdb_id == tmdb_id
    }
}

/// The Request/Request 4K sheet ("request dialog -> sheet/overlay"). Built
/// when the viewer opens it; dropped on close or on
/// a successful submit (the refreshed detail fetch carries the new
/// `active_request` state instead).
pub(crate) struct RequestSheetState {
    pub is_4k: bool,
    pub options: Option<SeerrRequestOptions>,
    pub options_loading: bool,
    pub selected_server_id: Option<i64>,
    pub selected_profile_id: Option<i64>,
    pub selected_root_folder: Option<String>,
    pub submitting: bool,
    pub error: Option<String>,
}

impl RequestSheetState {
    pub(crate) fn new(is_4k: bool) -> Self {
        RequestSheetState {
            is_4k,
            options: None,
            options_loading: true,
            selected_server_id: None,
            selected_profile_id: None,
            selected_root_folder: None,
            submitting: false,
            error: None,
        }
    }
}

pub(crate) struct DiscoverPersonState {
    pub person_id: i64,
    pub credits: Option<SeerrPersonCredits>,
    pub loading: bool,
    pub error: Option<String>,
    pub generation: u64,
}

impl DiscoverPersonState {
    pub(crate) fn new(person_id: i64) -> Self {
        DiscoverPersonState {
            person_id,
            credits: None,
            loading: true,
            error: None,
            generation: 0,
        }
    }
}

#[derive(Default)]
pub(crate) struct DiscoverMyRequestsState {
    pub requests: Vec<SeerrMyRequest>,
    pub loading: bool,
    pub error: Option<String>,
    pub generation: u64,
}

/// The whole Discover section's live state -- built lazily
/// (`Root::open_discover*`) the first time the viewer opens any Discover
/// screen this session, and torn down along with the rest of `MainState` on
/// every session swap (`session.rs::teardown_main_state`/
/// `main_state_from_bundle`), so a stale `SeerrSession` for the previous
/// account can never leak into a freshly-opened one.
pub(crate) struct DiscoverState {
    /// `None` until `Root::sync_discover_session` finishes opening it (or
    /// forever, if Discover isn't configured / the server is unreachable --
    /// see `open_error`, the fail-open surface this feature promises).
    pub session: Option<SeerrSession>,
    pub opening: bool,
    pub open_error: Option<String>,
    pub open_generation: u64,
    pub home: DiscoverHomeState,
    pub browse: Option<DiscoverBrowseState>,
    pub search: DiscoverSearchState,
    pub detail: Option<DiscoverDetailState>,
    pub person: Option<DiscoverPersonState>,
    pub my_requests: DiscoverMyRequestsState,
}

impl DiscoverState {
    pub(crate) fn new(cx: &mut gpui::Context<Root>) -> Self {
        DiscoverState {
            session: None,
            opening: false,
            open_error: None,
            open_generation: 0,
            home: DiscoverHomeState::default(),
            browse: None,
            search: DiscoverSearchState::new(cx),
            detail: None,
            person: None,
            my_requests: DiscoverMyRequestsState::default(),
        }
    }
}

/// Settings -> Discover's connect form -- always constructed
/// (`root.rs::main_state_from_bundle`, same as `ConnectState`'s own
/// `TextInput`s for the Connect screen), independent of `DiscoverState`
/// itself: the viewer may open Settings and connect Discover for the very
/// first time before ever visiting the Discover screen, at which point
/// `MainState::discover` is still `None`. Building three empty text fields
/// is not the "Seerr code" the zero-startup-cost rule guards against (no
/// network, no `seerr_api` call happens until Connect is clicked) -- the
/// same class of cost as the Connect screen's own fields, which this app
/// already pays unconditionally at every launch.
pub(crate) struct DiscoverConnectFormState {
    pub url: Entity<TextInput>,
    pub identity: Entity<TextInput>,
    pub secret: Entity<TextInput>,
    pub method: SeerrAuthMethod,
    pub connecting: bool,
    pub error: Option<String>,
}

impl DiscoverConnectFormState {
    pub(crate) fn new(cx: &mut gpui::Context<Root>) -> Self {
        DiscoverConnectFormState {
            url: cx.new(|cx| TextInput::new(cx, "seerr.example.com")),
            identity: cx.new(|cx| TextInput::new(cx, "username")),
            secret: cx.new(|cx| TextInput::new(cx, "password").password()),
            method: SeerrAuthMethod::Jellyfin,
            connecting: false,
            error: None,
        }
    }
}

// ---------------------------------------------------------------------
// Shared rendering: the poster card every Discover grid/shelf/rail uses.
// ---------------------------------------------------------------------

/// 2:3 poster geometry, matching `cards.rs::poster_card`'s own `width *
/// 1.5` aspect so a Discover card sits at the same proportions as every
/// Jellyfin poster card elsewhere in the app.
pub(crate) const CARD_WIDTH: Pixels = px(160.);
pub(crate) const CARD_GAP: Pixels = px(16.);

/// Resolves a `SeerrCard`'s poster art through `ImageStore`'s generic
/// remote-URL path (`ImageStore::get_remote`) -- the crate already builds
/// absolute TMDB/imageproxy URLs, so there is no `(item_id, kind, tag)`
/// triple to resolve here, only a URL that may or may not be present.
fn discover_art(
    poster_url: Option<&str>,
    store: &ImageStore,
    root: WeakEntity<Root>,
    cx: &mut App,
) -> AnyElement {
    if let Some(url) = poster_url {
        if let Some(texture) = store.get_remote(url, root, cx) {
            return gpui::img(texture)
                .object_fit(gpui::ObjectFit::Cover)
                .size_full()
                .into_any_element();
        }
    }
    // No poster yet (still fetching, or the crate has none for this title):
    // a flat placeholder tile, same brand-neutral fill `cards.rs::
    // art_placeholder` uses for the equivalent Jellyfin case. No blurhash
    // is available for Discover art (Seerr/TMDB doesn't hand one out), so
    // this is the honest resting state rather than an approximation of it.
    div()
        .size_full()
        .bg(rgb(theme::SURFACE_RAISED))
        .into_any_element()
}

fn availability_badge_element(badge: AvailabilityBadge) -> AnyElement {
    match badge {
        AvailabilityBadge::Available => crate::cards::watched_check_badge(),
        AvailabilityBadge::Pending => div()
            .w(px(10.))
            .h(px(10.))
            .rounded_full()
            .bg(rgb(theme::ACCENT))
            .into_any_element(),
    }
}

/// One poster cell -- shared by Home shelves, Browse grids, Search results,
/// My Requests, and the Similar/Recommended rails on Detail. Deliberately
/// simpler than `cards.rs::poster_card`'s full hover/focus choreography
/// (scale/brightness/sibling-dim animation): that machinery is tightly
/// coupled to `CardRow`'s mirror-sourced fields and this is a new,
/// independent card family with no keyboard `GridFocus` wiring of its own
/// (Discover is mouse-first, per this feature's own scope) -- a plain hover
/// affordance is honest about that rather than a partial port of a system
/// this doesn't fully participate in. Colors/radii/spacing are all named
/// tokens, per this app's "no raw literals outside theme.rs" rule.
#[allow(clippy::too_many_arguments)]
pub(crate) fn discover_card(
    card: &SeerrCard,
    store: &ImageStore,
    root: WeakEntity<Root>,
    cx: &mut App,
    on_click: impl Fn(&mut App) + 'static,
) -> AnyElement {
    let height = CARD_WIDTH * 1.5;
    let art = discover_art(card.poster_url.as_deref(), store, root, cx);
    let badge = availability_badge(card.availability).map(availability_badge_element);
    let card_id = SharedString::from(format!(
        "discover-card-{:?}-{}",
        card.media_type, card.tmdb_id
    ));

    let art_box = div()
        .id(card_id)
        .relative()
        .w(CARD_WIDTH)
        .h(height)
        .rounded(theme::RADIUS_ART)
        .overflow_hidden()
        .bg(rgb(theme::SURFACE_RAISED))
        .cursor_pointer()
        .child(art)
        .children(badge.map(|b| div().absolute().top_1().right_1().child(b)))
        .hover(|s| s.opacity(0.85))
        .on_click(move |_event, _window, cx| on_click(cx));

    let year = card.year.map(|y| y.to_string()).unwrap_or_default();

    div()
        .flex()
        .flex_col()
        .gap_1()
        .w(CARD_WIDTH)
        .flex_shrink_0()
        .child(art_box)
        .child(
            clamped_line(card.title.clone(), px(20.))
                .w(CARD_WIDTH)
                .text_sm()
                .font_weight(FontWeight::MEDIUM)
                .text_color(rgba(theme::TEXT_SECONDARY)),
        )
        .child(
            clamped_line(year, px(16.))
                .w(CARD_WIDTH)
                .text_xs()
                .text_color(rgba(theme::TEXT_TERTIARY)),
        )
        .into_any_element()
}

/// A top action chip ("Search, My Requests, Movies, TV
/// chips") -- `chip_button`'s shape, always resting/unselected (these are
/// navigation links, not a segmented choice).
pub(crate) fn action_chip(
    id: SharedString,
    icon_path: &'static str,
    label: &'static str,
    on_click: impl Fn(&mut App) + 'static,
) -> AnyElement {
    div()
        .id(id)
        .flex()
        .items_center()
        .gap_2()
        .h(px(36.))
        .px(theme::SPACE_DEFAULT)
        .rounded(theme::RADIUS_PILL)
        .cursor_pointer()
        .bg(rgba(theme::TRANSPARENT))
        .border_1()
        .border_color(rgb(theme::SURFACE_HAIRLINE))
        .hover(|s| s.bg(rgb(theme::SURFACE_OVERLAY)))
        .child(
            svg()
                .path(icon_path)
                .w(px(14.))
                .h(px(14.))
                .text_color(rgba(theme::TEXT_SECONDARY)),
        )
        .child(
            div()
                .text_size(theme::TEXT_BODY)
                .text_color(rgba(theme::TEXT_PRIMARY))
                .child(label),
        )
        .on_click(move |_event, _window, cx| on_click(cx))
        .into_any_element()
}

/// Discover's own inline fail-open message (spec: "degrades to an inline
/// message inside the Discover screen only") -- reuses `empty_state`'s
/// exact Brand §5 recipe (Archivo, factual, one line) rather than a second
/// bespoke error treatment.
pub(crate) fn inline_error(message: impl Into<SharedString>) -> impl IntoElement {
    crate::ui::components::empty_state("icons/search.svg", message)
}

/// Top-level dispatch -- `root.rs`'s `render_browse` calls this once for
/// whichever `DiscoverView` `nav.current` names, exactly the same shape as
/// its `View::Channel`/`View::Detail` arms dispatching into
/// `channel_browse::render`/`detail::render`.
pub(crate) fn render(
    state: &DiscoverState,
    view: crate::nav::DiscoverView,
    content_width: Pixels,
    store: &ImageStore,
    root: WeakEntity<Root>,
    cx: &mut gpui::Context<Root>,
) -> AnyElement {
    use crate::nav::DiscoverView;
    match view {
        DiscoverView::Home => home::render(state, store, root, cx).into_any_element(),
        DiscoverView::BrowseMovies | DiscoverView::BrowseTv => match &state.browse {
            Some(browse) => {
                let media_type = match view {
                    DiscoverView::BrowseMovies => DiscoverMediaType::Movie,
                    _ => DiscoverMediaType::Tv,
                };
                let columns = crate::grid::columns_for_width(content_width);
                browse::render(browse, media_type, columns, store, root, cx).into_any_element()
            }
            None => inline_error("Loading...").into_any_element(),
        },
        DiscoverView::Search => search::render(&state.search, store, root, cx).into_any_element(),
        DiscoverView::MyRequests => {
            requests::render(&state.my_requests, store, root, cx).into_any_element()
        }
        DiscoverView::Detail { .. } => match &state.detail {
            Some(d) => detail::render(d, store, root, cx).into_any_element(),
            None => inline_error("Loading...").into_any_element(),
        },
        DiscoverView::Person { .. } => match &state.person {
            Some(p) => person::render(p, store, root, cx).into_any_element(),
            None => inline_error("Loading...").into_any_element(),
        },
    }
}

// ---- Discover async control flow (moved from root.rs; see module doc
// comment above for why this flow lives here) --------------------------

impl Root {
    // ---- Discover (Seerr) -------------------------------------------------
    //
    // Every fetch here goes straight through a
    // live `seerr_api::SeerrSession` (never `Mirror`), matching
    // `channel_browse.rs`'s own "live, non-mirrored screen" shape: a plain
    // `self.runtime.spawn(async move {...})` for the actual network call,
    // bridged back to the GPUI thread via a oneshot channel + `cx.spawn`,
    // guarded by a per-sub-screen generation counter so a superseded or
    // navigated-away-from fetch can never clobber a newer one's result.

    /// Opens (or reuses) this account's `SeerrSession` -- the one real
    /// network round trip Discover ever needs outside an explicit user
    /// action, and only once the viewer has actually navigated into
    /// Discover (`ensure_view_loaded`'s `View::Discover` arm, the sole
    /// caller). Fails open into `DiscoverState::open_error` when Discover
    /// isn't configured for this account or the server can't be reached --
    /// this never touches Jellyfin browsing/playback/search in any way.
    pub(crate) fn sync_discover_session(&mut self, cx: &mut Context<Self>) {
        let Some(state) = self.main_state_mut() else {
            return;
        };
        let Some(ds) = &mut state.discover else {
            return;
        };
        if ds.session.is_some() || ds.opening {
            return;
        }
        if !state.seerr_status.configured {
            ds.open_error = Some(
                "Discover isn't set up yet. Open Settings -> Discover to connect.".to_string(),
            );
            return;
        }
        ds.opening = true;
        ds.open_error = None;
        ds.open_generation = ds.open_generation.wrapping_add(1);
        let generation = ds.open_generation;
        let base_url = state.base_url.clone();
        let user_id = state.client.user_id().unwrap_or_default().to_string();
        let data_dir = crate::paths::state_root();
        self.bridge(
            cx,
            async move { seerr_api::open(&data_dir, &base_url, &user_id).await },
            move |root, result, cx| root.apply_discover_session(generation, result, cx),
        );
        cx.notify();
    }

    fn apply_discover_session(
        &mut self,
        generation: u64,
        result: Result<seerr_api::SeerrSession, seerr_api::SeerrSessionError>,
        cx: &mut Context<Self>,
    ) {
        let still_on_discover = {
            let Some(state) = self.main_state_mut() else {
                return;
            };
            let Some(ds) = &mut state.discover else {
                return;
            };
            if ds.open_generation != generation {
                return;
            }
            ds.opening = false;
            match result {
                Ok(session) => ds.session = Some(session),
                Err(e) => ds.open_error = Some(e.to_string()),
            }
            matches!(state.nav.current, View::Discover(_))
        };
        cx.notify();
        // The session the current sub-screen was waiting on just resolved
        // (either way) -- re-dispatch so a freshly-opened session actually
        // kicks off its fetch instead of sitting idle until the next nav
        // event. A no-op if the viewer has since left Discover entirely.
        if still_on_discover {
            self.ensure_view_loaded(cx);
        }
    }

    /// The one place `ensure_view_loaded`'s `View::Discover` arm dispatches
    /// to -- picks which sub-screen's own fetch (if any) this nav target
    /// needs, deduping exactly like `View::Library`/`View::Detail` above
    /// (a plain revisit of the same sub-screen reuses what's already
    /// loaded; a genuine change rebuilds that sub-screen's state fresh and
    /// fetches).
    pub(crate) fn dispatch_discover_view(&mut self, view: DiscoverView, cx: &mut Context<Self>) {
        match view {
            DiscoverView::Home => self.spawn_discover_home(cx),
            DiscoverView::BrowseMovies | DiscoverView::BrowseTv => {
                let kind = match view {
                    DiscoverView::BrowseMovies => seerr_api::SeerrBrowseKind::Movies,
                    _ => seerr_api::SeerrBrowseKind::Tv,
                };
                let Some(state) = self.main_state_mut() else {
                    return;
                };
                let Some(ds) = &mut state.discover else {
                    return;
                };
                let same = ds.browse.as_ref().map(|b| b.kind) == Some(kind);
                if same {
                    return;
                }
                ds.browse = Some(DiscoverBrowseState::new(kind));
                self.spawn_discover_browse(kind, 1, false, cx);
            }
            DiscoverView::Search => {}
            DiscoverView::MyRequests => self.spawn_discover_my_requests(cx),
            DiscoverView::Detail {
                media_type,
                tmdb_id,
            } => {
                let Some(state) = self.main_state_mut() else {
                    return;
                };
                let Some(ds) = &mut state.discover else {
                    return;
                };
                let same = ds
                    .detail
                    .as_ref()
                    .is_some_and(|d| d.matches(media_type, tmdb_id));
                if same {
                    return;
                }
                ds.detail = Some(DiscoverDetailState::new(media_type, tmdb_id));
                self.spawn_discover_detail(media_type, tmdb_id, cx);
            }
            DiscoverView::Person { person_id } => {
                let Some(state) = self.main_state_mut() else {
                    return;
                };
                let Some(ds) = &mut state.discover else {
                    return;
                };
                let same = ds.person.as_ref().is_some_and(|p| p.person_id == person_id);
                if same {
                    return;
                }
                ds.person = Some(DiscoverPersonState::new(person_id));
                self.spawn_discover_person(person_id, cx);
            }
        }
    }

    pub(crate) fn open_discover(&mut self, cx: &mut Context<Self>) {
        self.collapse_fullscreen_player_for_nav(cx);
        let Some(state) = self.main_state_mut() else {
            return;
        };
        state.nav.go(View::Discover(DiscoverView::Home));
        self.ensure_view_loaded(cx);
        cx.notify();
    }

    pub(crate) fn open_discover_browse(
        &mut self,
        media_type: DiscoverMediaType,
        cx: &mut Context<Self>,
    ) {
        self.collapse_fullscreen_player_for_nav(cx);
        let Some(state) = self.main_state_mut() else {
            return;
        };
        let view = match media_type {
            DiscoverMediaType::Movie => DiscoverView::BrowseMovies,
            DiscoverMediaType::Tv => DiscoverView::BrowseTv,
        };
        state.nav.go(View::Discover(view));
        self.ensure_view_loaded(cx);
        cx.notify();
    }

    pub(crate) fn open_discover_search(&mut self, cx: &mut Context<Self>) {
        self.collapse_fullscreen_player_for_nav(cx);
        let Some(state) = self.main_state_mut() else {
            return;
        };
        state.nav.go(View::Discover(DiscoverView::Search));
        self.ensure_view_loaded(cx);
        cx.notify();
    }

    pub(crate) fn open_discover_my_requests(&mut self, cx: &mut Context<Self>) {
        self.collapse_fullscreen_player_for_nav(cx);
        let Some(state) = self.main_state_mut() else {
            return;
        };
        state.nav.go(View::Discover(DiscoverView::MyRequests));
        self.ensure_view_loaded(cx);
        cx.notify();
    }

    pub(crate) fn open_discover_detail(
        &mut self,
        media_type: DiscoverMediaType,
        tmdb_id: i64,
        cx: &mut Context<Self>,
    ) {
        self.collapse_fullscreen_player_for_nav(cx);
        let Some(state) = self.main_state_mut() else {
            return;
        };
        state.nav.go(View::Discover(DiscoverView::Detail {
            media_type,
            tmdb_id,
        }));
        self.ensure_view_loaded(cx);
        cx.notify();
    }

    pub(crate) fn open_discover_person(&mut self, person_id: i64, cx: &mut Context<Self>) {
        self.collapse_fullscreen_player_for_nav(cx);
        let Some(state) = self.main_state_mut() else {
            return;
        };
        state
            .nav
            .go(View::Discover(DiscoverView::Person { person_id }));
        self.ensure_view_loaded(cx);
        cx.notify();
    }

    // --- Home shelves ----------------------------------------------------

    fn spawn_discover_home(&mut self, cx: &mut Context<Self>) {
        let Some(ds) = self.discover_mut() else {
            return;
        };
        let Some(session) = ds.session.clone() else {
            return;
        };
        if ds.home.loading {
            return;
        }
        ds.home.loading = true;
        ds.home.generation = ds.home.generation.wrapping_add(1);
        let generation = ds.home.generation;
        self.bridge(
            cx,
            async move { session.home().await },
            move |root, home, cx| root.apply_discover_home(generation, home, cx),
        );
        cx.notify();
    }

    fn apply_discover_home(
        &mut self,
        generation: u64,
        home: seerr_api::SeerrHome,
        cx: &mut Context<Self>,
    ) {
        let Some(ds) = self.discover_mut() else {
            return;
        };
        if ds.home.generation != generation {
            return;
        }
        ds.home.loading = false;
        // `SeerrSession::home` is already fail-open per row (a row whose
        // fetch failed is simply omitted) -- there is no error case to
        // surface here separately.
        // De-duplicated within each shelf only, per `dedup_cards`
        // -- different shelves legitimately repeat a title.
        ds.home.rows = home
            .rows
            .into_iter()
            .map(|mut row| {
                row.cards = dedup_cards(row.cards);
                row
            })
            .collect();
        cx.notify();
    }

    // --- Browse grids ------------------------------------------------------

    /// `dispatch_discover_view` (fresh page 1), `set_discover_sort`/
    /// `set_discover_genre` (also page 1, filters changed), and
    /// `load_more_discover_browse` (the next page) all funnel through here
    /// so pagination and a filter/sort change share one fetch path.
    fn spawn_discover_browse(
        &mut self,
        kind: seerr_api::SeerrBrowseKind,
        page: i64,
        append: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(ds) = self.discover_mut() else {
            return;
        };
        let Some(session) = ds.session.clone() else {
            return;
        };
        let Some(browse) = &mut ds.browse else {
            return;
        };
        if browse.kind != kind {
            return; // stale call from before the viewer switched kind
        }
        browse.generation = browse.generation.wrapping_add(1);
        let generation = browse.generation;
        if append {
            browse.loading_more = true;
        } else {
            browse.loading = true;
        }
        browse.error = None;
        let filters = seerr_api::SeerrBrowseFilters {
            sort_by: browse.sort_by.clone(),
            genre_id: browse.genre_id,
            min_vote: None,
            network_id: None,
            status: None,
        };
        self.bridge(
            cx,
            async move { session.browse(kind, page, filters).await },
            move |root, result, cx| {
                root.apply_discover_browse(kind, generation, append, result, cx)
            },
        );
        self.spawn_discover_genres_if_needed(kind, cx);
        cx.notify();
    }

    fn apply_discover_browse(
        &mut self,
        kind: seerr_api::SeerrBrowseKind,
        generation: u64,
        append: bool,
        result: Result<seerr_api::SeerrPage, seerr_api::SeerrError>,
        cx: &mut Context<Self>,
    ) {
        let Some(ds) = self.discover_mut() else {
            return;
        };
        let Some(browse) = &mut ds.browse else {
            return;
        };
        if browse.kind != kind || browse.generation != generation {
            return;
        }
        browse.loading = false;
        browse.loading_more = false;
        match result {
            Ok(page) => {
                browse.error = None;
                // `dedup_cards`/`extend_deduped`: Seerr's
                // popularity-sorted pages can repeat an item when the
                // ordering shifts between requests. Paging still advances
                // by the server's own page number (below), not by the
                // local card count, so a dropped duplicate never shifts
                // "next page" math.
                if append {
                    extend_deduped(&mut browse.cards, page.cards);
                } else {
                    browse.cards = dedup_cards(page.cards);
                }
                browse.page = page.page;
                browse.total_pages = page.total_pages.max(1);
            }
            Err(e) => {
                if browse.cards.is_empty() {
                    browse.error = Some(e.to_string());
                }
            }
        }
        cx.notify();
    }

    /// `channel_browse.rs`'s `should_load_more`, wired the same way from
    /// `browse`'s `uniform_list` range callback -- re-checks its
    /// own guards inline, so a stray/duplicate call is always a safe no-op.
    pub(crate) fn load_more_discover_browse(&mut self, cx: &mut Context<Self>) {
        let Some(ds) = self.discover_mut() else {
            return;
        };
        let Some(browse) = &ds.browse else {
            return;
        };
        if browse.loading || browse.loading_more || browse.exhausted() {
            return;
        }
        let kind = browse.kind;
        let next_page = browse.page + 1;
        self.spawn_discover_browse(kind, next_page, true, cx);
    }

    fn spawn_discover_genres_if_needed(
        &mut self,
        kind: seerr_api::SeerrBrowseKind,
        cx: &mut Context<Self>,
    ) {
        let Some(ds) = self.discover_mut() else {
            return;
        };
        let Some(session) = ds.session.clone() else {
            return;
        };
        let Some(browse) = &ds.browse else {
            return;
        };
        if browse.kind != kind || !browse.genres.is_empty() {
            return;
        }
        let media_type = match kind {
            seerr_api::SeerrBrowseKind::Movies => seerr_api::SeerrMediaType::Movie,
            _ => seerr_api::SeerrMediaType::Tv,
        };
        self.bridge(
            cx,
            async move { session.genres(media_type).await },
            move |root, result, cx| {
                if let Ok(genres) = result {
                    root.apply_discover_genres(kind, genres, cx);
                }
            },
        );
    }

    fn apply_discover_genres(
        &mut self,
        kind: seerr_api::SeerrBrowseKind,
        genres: Vec<seerr_api::SeerrGenre>,
        cx: &mut Context<Self>,
    ) {
        let Some(ds) = self.discover_mut() else {
            return;
        };
        let Some(browse) = &mut ds.browse else {
            return;
        };
        if browse.kind != kind {
            return;
        }
        browse.genres = genres;
        cx.notify();
    }

    pub(crate) fn set_discover_sort(&mut self, sort_by: Option<String>, cx: &mut Context<Self>) {
        let Some(ds) = self.discover_mut() else {
            return;
        };
        let Some(browse) = &mut ds.browse else {
            return;
        };
        browse.sort_by = sort_by;
        browse.cards.clear();
        browse.page = 0;
        browse.total_pages = 1;
        let kind = browse.kind;
        self.spawn_discover_browse(kind, 1, false, cx);
    }

    pub(crate) fn set_discover_genre(&mut self, genre_id: Option<i64>, cx: &mut Context<Self>) {
        let Some(ds) = self.discover_mut() else {
            return;
        };
        let Some(browse) = &mut ds.browse else {
            return;
        };
        browse.genre_id = genre_id;
        browse.cards.clear();
        browse.page = 0;
        browse.total_pages = 1;
        let kind = browse.kind;
        self.spawn_discover_browse(kind, 1, false, cx);
    }

    // --- Search --------------------------------------------------------

    /// `DiscoverSearchState::new`'s `on_change` hook lands here on
    /// every keystroke (deferred, see that constructor's own comment).
    /// Bumps the generation immediately (so a same-tick keystroke can never
    /// race the timer below) and, for a non-empty query, arms a 300ms
    /// debounce that only actually fires
    /// `fire_discover_search` if this is still the newest keystroke by the
    /// time it wakes up.
    pub(crate) fn discover_search_changed(&mut self, cx: &mut Context<Self>) {
        let Some(ds) = self.discover_mut() else {
            return;
        };
        let query = ds.search.field.read(cx).content.clone();
        ds.search.generation = ds.search.generation.wrapping_add(1);
        let generation = ds.search.generation;
        if query.trim().is_empty() {
            ds.search.cards.clear();
            ds.search.loading = false;
            ds.search.error = None;
            ds.search.searched = false;
            cx.notify();
            return;
        }
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(300))
                .await;
            let _ = this.update(cx, |root, cx| {
                root.fire_discover_search(generation, query.clone(), cx)
            });
        })
        .detach();
    }

    fn fire_discover_search(&mut self, generation: u64, query: String, cx: &mut Context<Self>) {
        let Some(ds) = self.discover_mut() else {
            return;
        };
        if ds.search.generation != generation {
            return; // superseded by a newer keystroke
        }
        let Some(session) = ds.session.clone() else {
            return;
        };
        ds.search.loading = true;
        ds.search.error = None;
        self.bridge(
            cx,
            async move { session.search(&query, 1).await },
            move |root, result, cx| root.apply_discover_search(generation, result, cx),
        );
        cx.notify();
    }

    fn apply_discover_search(
        &mut self,
        generation: u64,
        result: Result<seerr_api::SeerrPage, seerr_api::SeerrError>,
        cx: &mut Context<Self>,
    ) {
        let Some(ds) = self.discover_mut() else {
            return;
        };
        if ds.search.generation != generation {
            return;
        }
        ds.search.loading = false;
        ds.search.searched = true;
        match result {
            Ok(page) => {
                ds.search.cards = dedup_cards(page.cards);
                ds.search.error = None;
            }
            Err(e) => {
                ds.search.cards.clear();
                ds.search.error = Some(e.to_string());
            }
        }
        cx.notify();
    }

    // --- My Requests -----------------------------------------------------

    fn spawn_discover_my_requests(&mut self, cx: &mut Context<Self>) {
        let Some(ds) = self.discover_mut() else {
            return;
        };
        let Some(session) = ds.session.clone() else {
            return;
        };
        if ds.my_requests.loading {
            return;
        }
        ds.my_requests.loading = true;
        ds.my_requests.generation = ds.my_requests.generation.wrapping_add(1);
        let generation = ds.my_requests.generation;
        self.bridge(
            cx,
            async move { session.my_requests().await },
            move |root, result, cx| root.apply_discover_my_requests(generation, result, cx),
        );
        cx.notify();
    }

    fn apply_discover_my_requests(
        &mut self,
        generation: u64,
        result: Result<Vec<seerr_api::SeerrMyRequest>, seerr_api::SeerrError>,
        cx: &mut Context<Self>,
    ) {
        let Some(ds) = self.discover_mut() else {
            return;
        };
        if ds.my_requests.generation != generation {
            return;
        }
        ds.my_requests.loading = false;
        match result {
            Ok(requests) => {
                ds.my_requests.requests = requests;
                ds.my_requests.error = None;
            }
            Err(e) => ds.my_requests.error = Some(e.to_string()),
        }
        cx.notify();
    }

    // --- Detail: fetch, seasons, request sheet --------------------------

    fn spawn_discover_detail(
        &mut self,
        media_type: DiscoverMediaType,
        tmdb_id: i64,
        cx: &mut Context<Self>,
    ) {
        let Some(ds) = self.discover_mut() else {
            return;
        };
        let Some(session) = ds.session.clone() else {
            return;
        };
        let Some(detail) = &mut ds.detail else {
            return;
        };
        if !detail.matches(media_type, tmdb_id) {
            return;
        }
        detail.generation = detail.generation.wrapping_add(1);
        let generation = detail.generation;
        detail.loading = true;
        self.bridge(
            cx,
            async move {
                match media_type {
                    DiscoverMediaType::Movie => {
                        session.movie(tmdb_id).await.map(DiscoverDetailBody::Movie)
                    }
                    DiscoverMediaType::Tv => session.tv(tmdb_id).await.map(DiscoverDetailBody::Tv),
                }
            },
            move |root, result, cx| {
                root.apply_discover_detail(media_type, tmdb_id, generation, result, cx)
            },
        );
        cx.notify();
    }

    fn apply_discover_detail(
        &mut self,
        media_type: DiscoverMediaType,
        tmdb_id: i64,
        generation: u64,
        result: Result<DiscoverDetailBody, seerr_api::SeerrError>,
        cx: &mut Context<Self>,
    ) {
        let Some(ds) = self.discover_mut() else {
            return;
        };
        let Some(detail) = &mut ds.detail else {
            return;
        };
        if !detail.matches(media_type, tmdb_id) || detail.generation != generation {
            return;
        }
        detail.loading = false;
        match result {
            Ok(body) => {
                detail.error = None;
                detail.body = Some(body);
            }
            Err(e) => detail.error = Some(e.to_string()),
        }
        cx.notify();
    }

    pub(crate) fn toggle_discover_season(&mut self, season_number: i32, cx: &mut Context<Self>) {
        let Some(ds) = self.discover_mut() else {
            return;
        };
        let Some(detail) = &mut ds.detail else {
            return;
        };
        if !detail.selected_seasons.remove(&season_number) {
            detail.selected_seasons.insert(season_number);
        }
        cx.notify();
    }

    pub(crate) fn open_discover_request_sheet(&mut self, is_4k: bool, cx: &mut Context<Self>) {
        {
            let Some(state) = self.main_state_mut() else {
                return;
            };
            let Some(ds) = &mut state.discover else {
                return;
            };
            let Some(detail) = &mut ds.detail else {
                return;
            };
            // Defensive re-check: the button that opens this is already
            // gated on the same condition (`detail::actions_row`),
            // but a sheet with nothing to submit for must never open.
            if matches!(detail.media_type, DiscoverMediaType::Tv)
                && detail.selected_seasons.is_empty()
            {
                return;
            }
            detail.request_sheet = Some(RequestSheetState::new(is_4k));
        }
        cx.notify();
        self.spawn_discover_request_options(is_4k, cx);
    }

    fn spawn_discover_request_options(&mut self, is_4k: bool, cx: &mut Context<Self>) {
        let Some(ds) = self.discover_mut() else {
            return;
        };
        let Some(session) = ds.session.clone() else {
            return;
        };
        let Some(detail) = &ds.detail else {
            return;
        };
        let media_type: seerr_api::SeerrMediaType = detail.media_type.into();
        self.bridge(
            cx,
            async move { session.request_options(media_type, is_4k).await },
            move |root, result, cx| root.apply_discover_request_options(result, cx),
        );
    }

    fn apply_discover_request_options(
        &mut self,
        result: Result<seerr_api::SeerrRequestOptions, seerr_api::SeerrError>,
        cx: &mut Context<Self>,
    ) {
        let Some(ds) = self.discover_mut() else {
            return;
        };
        let Some(detail) = &mut ds.detail else {
            return;
        };
        let Some(sheet) = &mut detail.request_sheet else {
            return;
        };
        sheet.options_loading = false;
        match result {
            Ok(options) => {
                if let Some(server) = options
                    .servers
                    .iter()
                    .find(|s| s.is_default)
                    .or_else(|| options.servers.first())
                {
                    sheet.selected_server_id = Some(server.server_id);
                    sheet.selected_profile_id = server
                        .profiles
                        .iter()
                        .find(|p| p.is_default)
                        .or_else(|| server.profiles.first())
                        .map(|p| p.id);
                    sheet.selected_root_folder = server
                        .root_folders
                        .iter()
                        .find(|f| f.is_default)
                        .or_else(|| server.root_folders.first())
                        .map(|f| f.path.clone());
                }
                sheet.options = Some(options);
            }
            Err(e) => sheet.error = Some(e.to_string()),
        }
        cx.notify();
    }

    pub(crate) fn close_discover_request_sheet(&mut self, cx: &mut Context<Self>) {
        let Some(ds) = self.discover_mut() else {
            return;
        };
        let Some(detail) = &mut ds.detail else {
            return;
        };
        detail.request_sheet = None;
        cx.notify();
    }

    pub(crate) fn set_discover_request_server(&mut self, server_id: i64, cx: &mut Context<Self>) {
        let Some(ds) = self.discover_mut() else {
            return;
        };
        let Some(detail) = &mut ds.detail else {
            return;
        };
        let Some(sheet) = &mut detail.request_sheet else {
            return;
        };
        sheet.selected_server_id = Some(server_id);
        if let Some(server) = sheet
            .options
            .as_ref()
            .and_then(|o| o.servers.iter().find(|s| s.server_id == server_id))
        {
            sheet.selected_profile_id = server
                .profiles
                .iter()
                .find(|p| p.is_default)
                .or_else(|| server.profiles.first())
                .map(|p| p.id);
            sheet.selected_root_folder = server
                .root_folders
                .iter()
                .find(|f| f.is_default)
                .or_else(|| server.root_folders.first())
                .map(|f| f.path.clone());
        }
        cx.notify();
    }

    pub(crate) fn set_discover_request_profile(&mut self, profile_id: i64, cx: &mut Context<Self>) {
        let Some(ds) = self.discover_mut() else {
            return;
        };
        let Some(detail) = &mut ds.detail else {
            return;
        };
        let Some(sheet) = &mut detail.request_sheet else {
            return;
        };
        sheet.selected_profile_id = Some(profile_id);
        cx.notify();
    }

    pub(crate) fn set_discover_request_root_folder(
        &mut self,
        path: String,
        cx: &mut Context<Self>,
    ) {
        let Some(ds) = self.discover_mut() else {
            return;
        };
        let Some(detail) = &mut ds.detail else {
            return;
        };
        let Some(sheet) = &mut detail.request_sheet else {
            return;
        };
        sheet.selected_root_folder = Some(path);
        cx.notify();
    }

    /// In-flight submission disables the sheet's own Confirm button
    /// (`sheet.submitting`, read by `detail::render_request_sheet`
    /// -- spec's "in-flight request submission disables the button").
    pub(crate) fn submit_discover_request(&mut self, cx: &mut Context<Self>) {
        let Some(ds) = self.discover_mut() else {
            return;
        };
        let Some(session) = ds.session.clone() else {
            return;
        };
        let Some(detail) = &mut ds.detail else {
            return;
        };
        let media_type = detail.media_type;
        let tmdb_id = detail.tmdb_id;
        let seasons: Vec<i32> = if matches!(media_type, DiscoverMediaType::Tv) {
            detail.selected_seasons.iter().copied().collect()
        } else {
            Vec::new()
        };
        let Some(sheet) = &mut detail.request_sheet else {
            return;
        };
        if sheet.submitting {
            return;
        }
        sheet.submitting = true;
        sheet.error = None;
        let input = seerr_api::SeerrRequestInput {
            media_type: media_type.into(),
            tmdb_id,
            is_4k: sheet.is_4k,
            seasons,
            server_id: sheet.selected_server_id,
            profile_id: sheet.selected_profile_id,
            root_folder: sheet.selected_root_folder.clone(),
        };
        self.bridge(
            cx,
            async move { session.submit_request(input).await },
            move |root, result, cx| {
                root.apply_discover_request_submit(media_type, tmdb_id, result, cx)
            },
        );
        cx.notify();
    }

    fn apply_discover_request_submit(
        &mut self,
        media_type: DiscoverMediaType,
        tmdb_id: i64,
        result: Result<(), seerr_api::SeerrError>,
        cx: &mut Context<Self>,
    ) {
        let refetch = {
            let Some(state) = self.main_state_mut() else {
                return;
            };
            let Some(ds) = &mut state.discover else {
                return;
            };
            let Some(detail) = &mut ds.detail else {
                return;
            };
            if !detail.matches(media_type, tmdb_id) {
                return;
            }
            match result {
                Ok(()) => {
                    detail.request_sheet = None;
                    detail.selected_seasons.clear();
                    true
                }
                Err(e) => {
                    if let Some(sheet) = &mut detail.request_sheet {
                        sheet.submitting = false;
                        sheet.error = Some(e.to_string());
                    }
                    false
                }
            }
        };
        cx.notify();
        if refetch {
            // The submitted item's availability/active-request state just
            // changed server-side -- refresh in place. `render`'s dispatch
            // keeps showing the stale body (never `None`) while this is in
            // flight, so there is no loading flash.
            self.spawn_discover_detail(media_type, tmdb_id, cx);
        }
    }

    pub(crate) fn cancel_discover_request(&mut self, request_id: i64, cx: &mut Context<Self>) {
        let Some(ds) = self.discover_mut() else {
            return;
        };
        let Some(session) = ds.session.clone() else {
            return;
        };
        let Some(detail) = &ds.detail else {
            return;
        };
        let media_type = detail.media_type;
        let tmdb_id = detail.tmdb_id;
        self.bridge(
            cx,
            async move { session.cancel_request(request_id).await },
            move |root, result, cx| {
                if result.is_ok() {
                    root.spawn_discover_detail(media_type, tmdb_id, cx);
                }
            },
        );
        cx.notify();
    }

    // --- Person ------------------------------------------------------------

    fn spawn_discover_person(&mut self, person_id: i64, cx: &mut Context<Self>) {
        let Some(ds) = self.discover_mut() else {
            return;
        };
        let Some(session) = ds.session.clone() else {
            return;
        };
        let Some(person) = &mut ds.person else {
            return;
        };
        if person.person_id != person_id {
            return;
        }
        person.generation = person.generation.wrapping_add(1);
        let generation = person.generation;
        person.loading = true;
        self.bridge(
            cx,
            async move { session.person(person_id).await },
            move |root, result, cx| root.apply_discover_person(person_id, generation, result, cx),
        );
        cx.notify();
    }

    fn apply_discover_person(
        &mut self,
        person_id: i64,
        generation: u64,
        result: Result<seerr_api::SeerrPersonCredits, seerr_api::SeerrError>,
        cx: &mut Context<Self>,
    ) {
        let Some(ds) = self.discover_mut() else {
            return;
        };
        let Some(person) = &mut ds.person else {
            return;
        };
        if person.person_id != person_id || person.generation != generation {
            return;
        }
        person.loading = false;
        match result {
            Ok(mut credits) => {
                // A person's combined cast-plus-crew credits repeat a title
                // when the same person acted in and directed it.
                credits.credits = dedup_cards(credits.credits);
                person.error = None;
                person.credits = Some(credits);
            }
            Err(e) => person.error = Some(e.to_string()),
        }
        cx.notify();
    }

    // --- Settings -> Discover: connect/disconnect ---------------------

    pub(crate) fn set_discover_connect_method(
        &mut self,
        method: seerr_api::SeerrAuthMethod,
        cx: &mut Context<Self>,
    ) {
        let Some(state) = self.main_state_mut() else {
            return;
        };
        state.discover_connect.method = method;
        state.discover_connect.error = None;
        cx.notify();
    }

    pub(crate) fn connect_discover(&mut self, cx: &mut Context<Self>) {
        // Block-scoped so each `&mut self.screen` borrow ends before the
        // next one starts -- this fn touches `self.screen` at three
        // separate points (read the form, write a validation error and
        // bail, or write `connecting` and proceed), and NLL can only prove
        // that safe when each read/write is its own short-lived borrow
        // rather than one borrow held across all three.
        let (url, identity, secret, method) = {
            let Some(state) = self.main_state_mut() else {
                return;
            };
            if state.discover_connect.connecting {
                return;
            }
            (
                state.discover_connect.url.read(cx).content.clone(),
                state.discover_connect.identity.read(cx).content.clone(),
                state.discover_connect.secret.read(cx).content.clone(),
                state.discover_connect.method,
            )
        };
        if let Err(msg) = validate_connect_fields(&url, method, &identity, &secret) {
            let Some(state) = self.main_state_mut() else {
                return;
            };
            state.discover_connect.error = Some(msg.to_string());
            cx.notify();
            return;
        }
        let (base_url, user_id) = {
            let Some(state) = self.main_state_mut() else {
                return;
            };
            state.discover_connect.connecting = true;
            state.discover_connect.error = None;
            (
                state.base_url.clone(),
                state.client.user_id().unwrap_or_default().to_string(),
            )
        };
        let data_dir = crate::paths::state_root();
        // Captured now (the account that's live while this connect is in
        // flight) so `apply_discover_connect` can tell a same-account
        // completion from a stale one -- see that fn's doc comment.
        let spawned_identity = (base_url.clone(), user_id.clone());
        self.bridge(
            cx,
            async move {
                seerr_api::connect(
                    &data_dir,
                    seerr_api::ConnectArgs {
                        server_url: &base_url,
                        user_id: &user_id,
                        url: &url,
                        method,
                        identity: &identity,
                        secret: &secret,
                    },
                )
                .await
            },
            move |root, result, cx| root.apply_discover_connect(spawned_identity, result, cx),
        );
        cx.notify();
    }

    /// `(base_url, user_id)` identity of the account a Discover connect
    /// belongs to vs. the account that's live right now -- pulled out of
    /// [`apply_discover_connect`] as a tiny pure fn so the mismatch branch
    /// (account switched mid-flight) is unit-testable without building a
    /// full `MainState`.
    pub(crate) fn discover_connect_identity_still_current(
        spawned: &(String, String),
        current: &(String, String),
    ) -> bool {
        spawned == current
    }

    /// Applies a finished [`connect_discover`] call to the CURRENT
    /// `MainState` -- but only if `spawned_identity` (the `(base_url,
    /// user_id)` captured when the connect was kicked off) still matches
    /// that state's account. Switching Jellyfin accounts rebuilds
    /// `MainState` wholesale (`main_state_from_bundle`), so a mismatch here
    /// means the connect's result belongs to an account this app has since
    /// navigated away from; applying it would hand the new account the old
    /// account's live Seerr session. On a mismatch the result is dropped
    /// entirely -- `discover_connect` is left untouched, since that form
    /// state belongs to the new account, not the stale one, and its
    /// `connecting` flag is already `false` on the freshly built
    /// `MainState` (`DiscoverConnectFormState::new`'s default), so there is
    /// nothing to reset.
    fn apply_discover_connect(
        &mut self,
        spawned_identity: (String, String),
        result: Result<
            (seerr_api::SeerrSession, seerr_api::SeerrStatus),
            seerr_api::SeerrSessionError,
        >,
        cx: &mut Context<Self>,
    ) {
        let Some(state) = self.main_state_mut() else {
            return;
        };
        let current_identity = (
            state.base_url.clone(),
            state.client.user_id().unwrap_or_default().to_string(),
        );
        if !Self::discover_connect_identity_still_current(&spawned_identity, &current_identity) {
            tracing::info!(
                "dropping a Discover connect result: the account switched while it was in flight"
            );
            return;
        }

        match result {
            Ok((session, status)) => {
                // Built before touching `self.screen` at all: `DiscoverState
                // ::new` only needs `cx` (a separate handle, not derived
                // from `self.screen`), so there is no borrow to juggle here.
                let mut discover_state = DiscoverState::new(cx);
                discover_state.session = Some(session);
                let Some(state) = self.main_state_mut() else {
                    return;
                };
                state.discover_connect.connecting = false;
                state.discover_connect.error = None;
                // Sidebar gating "re-evaluated ... after
                // connect/disconnect in settings" -- every render already
                // reads this field directly, so updating it here is the
                // whole of that wiring.
                state.seerr_status = status;
                // "Writes only on successful connect" -- and
                // the already-open session is handed straight to a fresh
                // `DiscoverState` so opening Discover right after connecting
                // doesn't pay for a second network round trip.
                state.discover = Some(Box::new(discover_state));
            }
            Err(e) => {
                let Some(state) = self.main_state_mut() else {
                    return;
                };
                state.discover_connect.connecting = false;
                state.discover_connect.error = Some(e.to_string());
            }
        }
        cx.notify();
    }

    pub(crate) fn disconnect_discover(&mut self, cx: &mut Context<Self>) {
        let Some(state) = self.main_state_mut() else {
            return;
        };
        let base_url = state.base_url.clone();
        let user_id = state.client.user_id().unwrap_or_default().to_string();
        let data_dir = crate::paths::state_root();
        if let Err(e) = seerr_api::disconnect(&data_dir, &base_url, &user_id) {
            tracing::warn!(error = %e, "failed to remove the Discover connection");
        }
        state.seerr_status = seerr_api::SeerrStatus {
            configured: false,
            seerr_url: None,
            method: None,
            identity: None,
            app_title: None,
        };
        // Spec: "clear/rebuild the live SeerrSession handle at every
        // session-swap point (sign-in, restore, account switch, sign-out,
        // remove-active)" -- Discover has no separate sign-out flow of its
        // own, so a disconnect is the equivalent event: drop the live
        // session outright rather than leaving a stale handle for an
        // account this app no longer has credentials for.
        state.discover = None;
        if matches!(state.nav.current, View::Discover(_)) {
            state.nav.go(View::Home);
        }
        cx.notify();
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    // --- availability_badge -----------------------------------------------

    #[test]
    fn not_requested_shows_no_badge() {
        assert_eq!(availability_badge(SeerrAvailability::NotRequested), None);
    }

    #[test]
    fn available_shows_the_checkmark_badge() {
        assert_eq!(
            availability_badge(SeerrAvailability::Available),
            Some(AvailabilityBadge::Available)
        );
    }

    #[test]
    fn pending_and_processing_and_partial_all_show_the_pending_dot() {
        for availability in [
            SeerrAvailability::Pending,
            SeerrAvailability::Processing,
            SeerrAvailability::PartiallyAvailable,
        ] {
            assert_eq!(
                availability_badge(availability),
                Some(AvailabilityBadge::Pending),
                "{availability:?} must show the pending dot"
            );
        }
    }

    // --- season_row_state ---------------------------------------------------

    fn season(requestable: bool) -> SeerrSeasonStatus {
        SeerrSeasonStatus {
            season_number: 1,
            name: "Season 1".to_string(),
            episode_count: 10,
            availability: SeerrAvailability::NotRequested,
            requestable,
        }
    }

    #[test]
    fn an_unrequestable_season_is_always_checked_and_inert() {
        let state = season_row_state(&season(false), false);
        assert!(state.checked);
        assert!(state.inert);
        // Even if the caller somehow passed `selected: true`, the row must
        // still read as inert -- there is nothing to toggle either way.
        let state = season_row_state(&season(false), true);
        assert!(state.checked);
        assert!(state.inert);
    }

    #[test]
    fn a_requestable_season_reflects_the_viewers_own_selection() {
        let unselected = season_row_state(&season(true), false);
        assert!(!unselected.checked);
        assert!(!unselected.inert);

        let selected = season_row_state(&season(true), true);
        assert!(selected.checked);
        assert!(!selected.inert);
    }

    // --- primary_action -------------------------------------------------

    #[test]
    fn go_to_library_wins_over_every_other_state() {
        assert_eq!(
            primary_action(Some("jf-item-1"), Some(42), true),
            PrimaryAction::GoToLibrary
        );
    }

    #[test]
    fn an_active_request_offers_cancel() {
        assert_eq!(
            primary_action(None, Some(7), true),
            PrimaryAction::Cancel { request_id: 7 }
        );
    }

    #[test]
    fn no_item_no_request_but_requestable_offers_request() {
        assert_eq!(primary_action(None, None, true), PrimaryAction::Request);
    }

    #[test]
    fn no_item_no_request_not_requestable_is_unavailable() {
        assert_eq!(
            primary_action(None, None, false),
            PrimaryAction::Unavailable
        );
    }

    // --- validate_connect_fields ------------------------------------------

    #[test]
    fn empty_url_is_rejected_regardless_of_method() {
        assert!(validate_connect_fields("", SeerrAuthMethod::ApiKey, "", "key").is_err());
    }

    #[test]
    fn api_key_method_only_needs_the_secret() {
        assert!(
            validate_connect_fields("http://seerr.test", SeerrAuthMethod::ApiKey, "", "").is_err()
        );
        assert!(validate_connect_fields(
            "http://seerr.test",
            SeerrAuthMethod::ApiKey,
            "",
            "abc123"
        )
        .is_ok());
    }

    #[test]
    fn jellyfin_and_local_methods_need_identity_and_secret() {
        assert!(
            validate_connect_fields("http://seerr.test", SeerrAuthMethod::Jellyfin, "", "pw")
                .is_err()
        );
        assert!(validate_connect_fields(
            "http://seerr.test",
            SeerrAuthMethod::Jellyfin,
            "alice",
            ""
        )
        .is_err());
        assert!(validate_connect_fields(
            "http://seerr.test",
            SeerrAuthMethod::Local,
            "alice@example.com",
            "pw"
        )
        .is_ok());
    }

    // --- sidebar_visible -----------------------------------------------

    #[test]
    fn sidebar_gating_follows_status_configured() {
        let mut status = SeerrStatus {
            configured: false,
            seerr_url: None,
            method: None,
            identity: None,
            app_title: None,
        };
        assert!(!sidebar_visible(&status));
        status.configured = true;
        assert!(sidebar_visible(&status));
    }

    // --- DiscoverBrowseState::exhausted -------------------------------

    #[test]
    fn browse_state_is_not_exhausted_before_the_first_page_lands() {
        let state = DiscoverBrowseState::new(SeerrBrowseKind::Movies);
        assert!(!state.exhausted());
    }

    #[test]
    fn browse_state_is_exhausted_once_the_last_page_is_reached() {
        let mut state = DiscoverBrowseState::new(SeerrBrowseKind::Movies);
        state.page = 3;
        state.total_pages = 3;
        assert!(state.exhausted());
        state.page = 2;
        assert!(!state.exhausted());
    }

    // --- media type round trip -------------------------------------------

    #[test]
    fn discover_media_type_round_trips_through_seerr_media_type() {
        for dt in [DiscoverMediaType::Movie, DiscoverMediaType::Tv] {
            let seerr: SeerrMediaType = dt.into();
            let back: DiscoverMediaType = seerr.into();
            assert_eq!(dt, back);
        }
    }

    // --- dedup_cards / extend_deduped --------------------------------------

    fn card(media_type: SeerrMediaType, tmdb_id: i64, title: &str) -> SeerrCard {
        SeerrCard {
            media_type,
            tmdb_id,
            title: title.to_string(),
            year: None,
            overview: None,
            poster_url: None,
            backdrop_url: None,
            availability: SeerrAvailability::NotRequested,
            jellyfin_item_id: None,
        }
    }

    #[test]
    fn a_card_repeated_later_in_the_list_is_dropped_keeping_the_first_and_the_order() {
        let cards = vec![
            card(SeerrMediaType::Movie, 1, "first"),
            card(SeerrMediaType::Movie, 2, "t2"),
            card(SeerrMediaType::Movie, 1, "repeat"),
            card(SeerrMediaType::Movie, 3, "t3"),
        ];
        let titles: Vec<String> = dedup_cards(cards).into_iter().map(|c| c.title).collect();
        assert_eq!(titles, vec!["first", "t2", "t3"]);
    }

    #[test]
    fn a_movie_and_a_tv_show_sharing_a_tmdb_id_are_distinct_cards() {
        let cards = vec![
            card(SeerrMediaType::Movie, 7, "movie"),
            card(SeerrMediaType::Tv, 7, "tv"),
        ];
        assert_eq!(dedup_cards(cards).len(), 2);
    }

    #[test]
    fn an_already_distinct_list_is_returned_unchanged() {
        let cards = vec![
            card(SeerrMediaType::Movie, 1, "one"),
            card(SeerrMediaType::Tv, 2, "two"),
        ];
        assert_eq!(dedup_cards(cards.clone()), cards);
    }

    #[test]
    fn extend_deduped_drops_a_page_2_card_that_page_1_already_showed() {
        // Seerr's popularity-sorted browse shifts between requests, so page 2
        // can carry an item page 1 already showed.
        let mut existing = vec![
            card(SeerrMediaType::Movie, 1, "One"),
            card(SeerrMediaType::Movie, 2, "Two"),
        ];
        let page2 = vec![
            card(SeerrMediaType::Movie, 2, "Two again"),
            card(SeerrMediaType::Movie, 3, "Three"),
        ];
        extend_deduped(&mut existing, page2);
        let titles: Vec<&str> = existing.iter().map(|c| c.title.as_str()).collect();
        assert_eq!(titles, vec!["One", "Two", "Three"]);
    }
}
