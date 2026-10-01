//! Live TVHeadend/plugin-channel browse screen
//! (docs/PLUGIN-CHANNELS.md §2.2). A `ViewKind::Channel`
//! view is never mirrored -- its DVR content churns independently of any
//! library scan -- so every listing here is a live
//! `JellyfinClient::live_children` round trip, owned by `root.rs` (which has
//! the client/runtime/`Context<Root>` this needs) and rendered here.
//!
//! Two levels share one [`ChannelBrowseState`], distinguished by `folder_id`:
//! - **Level 1** (`folder_id: None`): the view itself, `ChannelFolderItem`
//!   folders, listed `LiveSort::NameAsc`. A folder row opens level 2.
//! - **Level 2** (`folder_id: Some(id)`): a folder's recordings, listed
//!   `LiveSort::NewestFirst`, rendered as a dense folder listing rather
//!   than a poster grid -- recordings carry no art and a null runtime, so
//!   the only columns worth showing are local date/time (from
//!   `PremiereDate`, server-sent UTC), name, synopsis, and played state.
//!
//! `root.rs::ensure_view_loaded`'s `View::Channel` arm re-queries the current
//! level every time this screen becomes the active nav target, including a
//! plain revisit (see [`ChannelBrowseState::is_for`]), since there is no
//! mirror change event to react to instead. A transient fetch error is
//! swallowed while something is already on screen and surfaced only when
//! the level has nothing loaded at all (spec: "fail soft").
//!
//! [`level_query`], [`should_load_more`] and [`recording_datetime_label`]
//! below are the pure decision points covered by this module's unit tests;
//! the surrounding `uniform_list`/focus/on_click wiring is exercised only
//! by hand and by the E2E suite, the same split `library_list.rs` and
//! `grid.rs` already draw.

use std::cell::Cell;
use std::rc::Rc;

use chrono::{DateTime, Local, Utc};
use gpui::{
    div, prelude::*, px, rgb, rgba, svg, uniform_list, AnyElement, FontWeight, Pixels,
    SharedString, WeakEntity,
};
use jellyfin_api::models::{BaseItemDto, BaseItemKind};
use jellyfin_api::LiveSort;

use crate::cards::{display_title, watched_check_badge};
use crate::focus_grid::{GridFocus, HighlightPolicy};
use crate::grid::GridScroll;
use crate::root::Root;
use crate::theme;
use crate::ui::components::{clamped_line, empty_state, focus_ring};

/// A live listing page's size for both the initial and "load more" fetch.
/// `live_children` returns no `TotalRecordCount`, so "fewer than a full
/// page came back" is what marks a level [`ChannelBrowseState::exhausted`].
pub(crate) const CHANNEL_PAGE_SIZE: u32 = 100;

/// How many rows of runway before the end of what's loaded triggers the
/// next page (`should_load_more`) -- enough headroom that the fetch is
/// already in flight before the user can actually scroll past the last
/// loaded row.
const LOAD_MORE_THRESHOLD: usize = 20;

/// One-line row height (no art, no thumb) -- comfortably fits a 14px title
/// line plus a 12px secondary line vertically centered.
const ROW_HEIGHT: Pixels = px(56.);
const ROW_GAP: Pixels = px(4.);
const ROW_PITCH: Pixels = px(56. + 4.);
const TITLE_LINE_HEIGHT: Pixels = px(20.);
const META_LINE_HEIGHT: Pixels = px(16.);
/// Fixed leading column for a level-2 row's local date/time -- wide enough
/// for the longest realistic label ("Wed 12/31  11:59 PM") without wrapping.
const DATETIME_COLUMN_WIDTH: Pixels = px(132.);

/// Pure: which parent id and [`LiveSort`] a channel browse level fetches,
/// per docs/PLUGIN-CHANNELS.md §2.2 -- the view itself
/// (`folder_id: None`) lists its `ChannelFolderItem` folders `NameAsc`; a
/// folder (`folder_id: Some`) lists its recordings `NewestFirst`.
pub(crate) fn level_query(view_id: &str, folder_id: Option<&str>) -> (String, LiveSort) {
    match folder_id {
        Some(id) => (id.to_string(), LiveSort::NewestFirst),
        None => (view_id.to_string(), LiveSort::NameAsc),
    }
}

/// Pure: should the next page be fetched, given `uniform_list`'s visible
/// range end, rows loaded, and the in-flight/end-of-data flags. Never
/// fires mid-fetch, once a level is [`ChannelBrowseState::exhausted`], or
/// before the initial fetch has loaded anything.
pub(crate) fn should_load_more(
    visible_end: usize,
    loaded: usize,
    exhausted: bool,
    loading_more: bool,
) -> bool {
    loaded > 0 && !exhausted && !loading_more && visible_end + LOAD_MORE_THRESHOLD >= loaded
}

/// Pure: a level-2 row's leading column -- `PremiereDate` (server-sent UTC)
/// converted to the viewer's local time. Empty (never an invented value,
/// brand §6) when the DTO carries none.
pub(crate) fn recording_datetime_label(premiere_date: Option<DateTime<Utc>>) -> String {
    match premiere_date {
        Some(d) => d
            .with_timezone(&Local)
            .format("%a %-m/%-d  %-I:%M %p")
            .to_string(),
        None => String::new(),
    }
}

/// `UserData.Played`, defaulting to unplayed for a DTO with no user data at
/// all (never seen live, but the honest default -- same "absence means no"
/// convention every other optional flag in this app uses).
fn is_played(dto: &BaseItemDto) -> bool {
    dto.user_data
        .as_ref()
        .and_then(|u| u.played)
        .unwrap_or(false)
}

/// The resume position to hand `Root::play_item_with_resume_hint` for a
/// level-2 row -- straight off the live DTO's own `UserData`, since this
/// item is never mirrored (see this module's doc comment).
fn resume_ticks_hint(dto: &BaseItemDto) -> Option<i64> {
    dto.user_data
        .as_ref()
        .and_then(|u| u.playback_position_ticks)
        .filter(|t| *t > 0)
}

/// Per-screen state, owned by `MainState::channel_browse`. Rebuilt (fresh,
/// empty `items`) by `root.rs::ensure_view_loaded` whenever the (view,
/// folder) pair actually changes; reused in place -- so a revisit doesn't
/// blank the list before the re-query lands -- when it's the same level.
pub(crate) struct ChannelBrowseState {
    pub view_id: String,
    pub view_name: String,
    /// `None` = level 1 (the view itself); `Some(id)` = level 2 (a folder).
    pub folder_id: Option<String>,
    /// Best-effort: the clicked folder's name, read off the level-1 DTO at
    /// click time. `None` on the rare path that lands on level 2 without
    /// that DTO (a `Nav::forward()` redo) -- `render`'s title falls back
    /// to `view_name` rather than inventing one.
    pub folder_name: Option<String>,
    pub items: Rc<Vec<BaseItemDto>>,
    pub loading: bool,
    pub loading_more: bool,
    /// The last page fetched came back shorter than [`CHANNEL_PAGE_SIZE`] --
    /// there is nothing further to page in for this level.
    pub exhausted: bool,
    /// Set only when `items` is empty and the fetch that would have filled
    /// it failed -- spec's "quiet inline error only when there's nothing to
    /// show". A failed refresh/page-in with existing rows on screen leaves
    /// this `None` and simply keeps what's already there.
    pub error: Option<String>,
    pub focus: GridFocus,
    pub highlight: HighlightPolicy,
    pub focus_engaged: bool,
    pub scroll: GridScroll,
    /// Bumped on every fetch this level starts (initial or load-more); a
    /// response is applied only if it still matches, so a superseded or
    /// navigated-away-from fetch can't clobber a newer one -- same shape as
    /// `Root::connect_generation`/`hover_retarget_epoch`.
    pub generation: u64,
}

impl ChannelBrowseState {
    pub(crate) fn new(
        view_id: String,
        view_name: String,
        folder_id: Option<String>,
        folder_name: Option<String>,
    ) -> Self {
        ChannelBrowseState {
            view_id,
            view_name,
            folder_id,
            folder_name,
            items: Rc::new(Vec::new()),
            loading: true,
            loading_more: false,
            exhausted: false,
            error: None,
            focus: GridFocus::new(1),
            highlight: HighlightPolicy::new(),
            focus_engaged: false,
            scroll: GridScroll::new(),
            generation: 0,
        }
    }

    /// Whether this state is already displaying exactly `(view_id,
    /// folder_id)` -- `root.rs::ensure_view_loaded`'s "reuse vs. rebuild"
    /// check.
    pub(crate) fn is_for(&self, view_id: &str, folder_id: Option<&str>) -> bool {
        self.view_id == view_id && self.folder_id.as_deref() == folder_id
    }

    /// Level 1 iff there is no open folder.
    pub(crate) fn is_folder_level(&self) -> bool {
        self.folder_id.is_none()
    }
}

/// What activating the currently-focused row should do -- level 1 opens a
/// nested folder, level 2 plays the recording. `None` for an empty/
/// out-of-range focus (nothing to activate) or a level-1 row that somehow
/// isn't a `ChannelFolderItem` (fails safe by not opening a non-folder as if
/// it were one; the folder-icon-vs-not distinction the server itself draws).
pub(crate) enum ChannelActivate {
    OpenFolder(String),
    Play {
        item_id: String,
        item_name: String,
        resume_ticks_hint: Option<i64>,
    },
}

pub(crate) fn channel_activate_action(state: &ChannelBrowseState) -> Option<ChannelActivate> {
    let item = state.items.get(state.focus.index)?;
    let id = item.id?.to_string();
    if state.is_folder_level() {
        return (item.type_ == Some(BaseItemKind::ChannelFolderItem))
            .then_some(ChannelActivate::OpenFolder(id));
    }
    Some(ChannelActivate::Play {
        item_id: id,
        item_name: item.name.clone().unwrap_or_default(),
        resume_ticks_hint: resume_ticks_hint(item),
    })
}

/// The screen's title -- the open folder's own name when known, the view's
/// otherwise (level 1, or the rare unnamed-folder fallback above).
fn page_title(state: &ChannelBrowseState) -> String {
    state
        .folder_name
        .clone()
        .unwrap_or_else(|| state.view_name.clone())
}

pub(crate) fn render(state: &ChannelBrowseState, root: WeakEntity<Root>) -> impl IntoElement {
    let show_empty = state.items.is_empty() && !state.loading;
    let empty_message = state
        .error
        .clone()
        .unwrap_or_else(|| "No recordings here yet.".to_string());

    div()
        .id("channel-browse")
        .relative()
        .size_full()
        .flex()
        .flex_col()
        .bg(rgb(theme::SURFACE_BASE))
        .child(
            div()
                .pt(theme::SPACE_LOOSE)
                .px(theme::SPACE_SECTION)
                .text_size(theme::TEXT_TITLE)
                .font_weight(FontWeight::BOLD)
                .text_color(rgba(theme::TEXT_PRIMARY))
                .child(SharedString::from(page_title(state))),
        )
        .when(show_empty, |d| {
            d.child(
                div()
                    .flex_1()
                    .min_h_0()
                    .px_5()
                    .child(empty_state("icons/search.svg", empty_message)),
            )
        })
        .when(!state.items.is_empty(), |d| {
            d.child(
                div()
                    .flex_1()
                    .min_h_0()
                    .px(px(20.) - theme::FOCUS_RING_CLEARANCE)
                    .mt(-theme::LIST_TOP_CLIP_SLACK)
                    .pb_5()
                    .child(channel_list(state, root.clone())),
            )
        })
}

fn channel_list(state: &ChannelBrowseState, root: WeakEntity<Root>) -> impl IntoElement {
    let items = state.items.clone();
    let is_folder_level = state.is_folder_level();
    let focused_index = state.focus_engaged.then(|| state.highlight.slot());
    let row_count = items.len().max(1);
    let exhausted = state.exhausted;
    let loading_more = state.loading_more;
    let last_seen_end: Rc<Cell<usize>> = Rc::new(Cell::new(usize::MAX));
    let load_more_root = root.clone();

    uniform_list(
        "channel-browse-list",
        row_count,
        move |range, _window, cx| {
            let end = range.end;
            if last_seen_end.get() != end {
                last_seen_end.set(end);
                if should_load_more(end, items.len(), exhausted, loading_more) {
                    let _ = load_more_root
                        .clone()
                        .update(cx, |root, cx| root.load_more_channel_items(cx));
                }
            }

            range
                .filter_map(|ix| {
                    let item = items.get(ix)?;
                    let focused = focused_index == Some(ix);
                    Some(if is_folder_level {
                        folder_row(item, ix, focused, root.clone())
                    } else {
                        recording_row(item, ix, focused, root.clone())
                    })
                })
                .collect()
        },
    )
    .track_scroll(state.scroll.handle.clone())
    .px(theme::FOCUS_RING_CLEARANCE)
    .pt(theme::LIST_TOP_CLIP_SLACK)
    .w_full()
    .h_full()
}

/// Level 1: a `ChannelFolderItem` row -- name plus a trailing chevron (opens
/// level 2, never a detail page). No art (per spec, these are date-grouping
/// folders, not media with posters) and no metadata line -- there is
/// nothing else server-side to show for one.
fn folder_row(
    item: &BaseItemDto,
    index: usize,
    focused: bool,
    root: WeakEntity<Root>,
) -> AnyElement {
    let id = item.id.map(|u| u.to_string()).unwrap_or_default();
    let name = display_title(&item.name.clone().unwrap_or_default());
    let open_id = id.clone();
    let click_root = root.clone();
    let hover_root = root;

    let row = div()
        .id(SharedString::from(format!("channel-folder-{id}")))
        .relative()
        .h(ROW_HEIGHT)
        .w_full()
        .flex()
        .flex_row()
        .items_center()
        .gap(theme::SPACE_SNUG)
        .px(theme::SPACE_SNUG)
        .rounded(theme::RADIUS_CARD)
        .cursor_pointer()
        .when(focused, |d| d.bg(rgb(theme::SURFACE_RAISED)))
        .hover(|s| s.bg(rgb(theme::SURFACE_RAISED)))
        .child(
            div().flex_1().min_w_0().child(
                clamped_line(name, TITLE_LINE_HEIGHT)
                    .w_full()
                    .text_sm()
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(rgba(theme::TEXT_PRIMARY)),
            ),
        )
        .child(
            svg()
                .path("icons/chevron-right.svg")
                .w(px(16.))
                .h(px(16.))
                .flex_shrink_0()
                .text_color(rgba(theme::TEXT_TERTIARY)),
        )
        .on_click(move |_event, _window, cx| {
            let _ = click_root.update(cx, |root, cx| root.open_channel_folder(open_id.clone(), cx));
        })
        .on_mouse_move(move |_event, _window, cx| {
            let _ = hover_root.update(cx, |root, cx| root.hover_channel_cell(index, cx));
        })
        .when(focused, |d| d.child(focus_ring(theme::RADIUS_CARD)));

    div().h(ROW_PITCH).pb(ROW_GAP).child(row).into_any_element()
}

/// Level 2: one recording -- fixed leading local date/time
/// (`recording_datetime_label`), name, a one-line synopsis, and a watched
/// checkmark when `UserData.Played` (`watched_check_badge`, the same glyph
/// every other watched item in this app uses -- no runtime/progress bar:
/// spec's "null runtime" makes one meaningless here). Activating the row
/// plays the recording directly (spec allows "folder-listing -> play").
fn recording_row(
    item: &BaseItemDto,
    index: usize,
    focused: bool,
    root: WeakEntity<Root>,
) -> AnyElement {
    let id = item.id.map(|u| u.to_string()).unwrap_or_default();
    let name = display_title(&item.name.clone().unwrap_or_default());
    let overview = item.overview.clone().unwrap_or_default();
    let datetime = recording_datetime_label(item.premiere_date);
    let played = is_played(item);
    let hint = resume_ticks_hint(item);

    let play_id = id.clone();
    let play_name = item.name.clone().unwrap_or_default();
    let click_root = root.clone();
    let hover_root = root;

    let text_block = div()
        .flex()
        .flex_col()
        .flex_1()
        .min_w_0()
        .child(
            clamped_line(name, TITLE_LINE_HEIGHT)
                .w_full()
                .text_sm()
                .font_weight(FontWeight::MEDIUM)
                .text_color(rgba(theme::TEXT_PRIMARY)),
        )
        .child(
            clamped_line(overview, META_LINE_HEIGHT)
                .w_full()
                .text_xs()
                .text_color(rgba(theme::TEXT_SYNOPSIS)),
        );

    let badge = played.then(|| div().flex_shrink_0().child(watched_check_badge()));

    let row = div()
        .id(SharedString::from(format!("channel-recording-{id}")))
        .relative()
        .h(ROW_HEIGHT)
        .w_full()
        .flex()
        .flex_row()
        .items_center()
        .gap(theme::SPACE_SNUG)
        .px(theme::SPACE_SNUG)
        .rounded(theme::RADIUS_CARD)
        .cursor_pointer()
        .when(focused, |d| d.bg(rgb(theme::SURFACE_RAISED)))
        .hover(|s| s.bg(rgb(theme::SURFACE_RAISED)))
        .child(
            div()
                .flex_shrink_0()
                .w(DATETIME_COLUMN_WIDTH)
                .font_family(theme::FONT_MONO)
                .text_size(theme::TEXT_SPEC)
                .text_color(rgba(theme::TEXT_TERTIARY))
                .child(SharedString::from(datetime)),
        )
        .child(text_block)
        .children(badge)
        .on_click(move |_event, _window, cx| {
            let _ = click_root.update(cx, |root, cx| {
                root.play_item_with_resume_hint(play_id.clone(), play_name.clone(), hint, cx)
            });
        })
        .on_mouse_move(move |_event, _window, cx| {
            let _ = hover_root.update(cx, |root, cx| root.hover_channel_cell(index, cx));
        })
        .when(focused, |d| d.child(focus_ring(theme::RADIUS_CARD)));

    div().h(ROW_PITCH).pb(ROW_GAP).child(row).into_any_element()
}

// ---- Channel browse async control flow ---------------------------------

impl Root {
    /// Fetches one page of the currently-open channel browse level
    /// (`state.channel_browse`), live -- `start_index: 0` is the initial/
    /// re-query fetch (replaces `items` wholesale on success, per spec §4's
    /// "a plain re-list is correct and sufficient" -- no cache-busting, no
    /// merge-by-id); any other `start_index` is a "load more" page
    /// (`load_more_channel_items`), appended on success. Generation-guarded,
    /// see `ChannelBrowseState::generation`.
    pub(crate) fn spawn_channel_page(&mut self, start_index: u32, cx: &mut Context<Self>) {
        let Some(state) = self.main_state_mut() else {
            return;
        };
        let Some(cb) = &mut state.channel_browse else {
            return;
        };
        cb.generation += 1;
        let generation = cb.generation;
        let view_id = cb.view_id.clone();
        let folder_id = cb.folder_id.clone();
        if start_index == 0 {
            cb.loading = true;
        } else {
            cb.loading_more = true;
        }
        let client = state.client.clone();
        let (parent_id, sort) = crate::channel_browse::level_query(&view_id, folder_id.as_deref());
        let limit = crate::channel_browse::CHANNEL_PAGE_SIZE;
        self.bridge(
            cx,
            async move {
                client
                    .live_children(&parent_id, start_index, limit, sort)
                    .await
            },
            move |root, result, cx| {
                root.apply_channel_fetch(view_id, folder_id, generation, start_index, result, cx)
            },
        );
        cx.notify();
    }

    /// Applies a `spawn_channel_page` result once it lands, ignoring it if
    /// the level it was fetched for is no longer the current one (a newer
    /// fetch for the same level superseded it, or navigation moved away
    /// from this exact (view, folder) entirely before the round trip
    /// returned).
    fn apply_channel_fetch(
        &mut self,
        view_id: String,
        folder_id: Option<String>,
        generation: u64,
        start_index: u32,
        result: Result<Vec<jellyfin_api::models::BaseItemDto>, jellyfin_api::ApiError>,
        cx: &mut Context<Self>,
    ) {
        let Some(state) = self.main_state_mut() else {
            return;
        };
        let Some(cb) = &mut state.channel_browse else {
            return;
        };
        if cb.generation != generation || !cb.is_for(&view_id, folder_id.as_deref()) {
            return;
        }
        cb.loading = false;
        cb.loading_more = false;
        match result {
            Ok(items) => {
                cb.error = None;
                let returned = items.len();
                if start_index == 0 {
                    cb.items = Rc::new(items);
                } else {
                    let mut merged = (*cb.items).clone();
                    merged.extend(items);
                    cb.items = Rc::new(merged);
                }
                cb.exhausted = (returned as u32) < crate::channel_browse::CHANNEL_PAGE_SIZE;
                cb.focus.clamp(cb.items.len());
            }
            Err(e) => {
                // Fail soft (spec §2.2): see `ChannelBrowseState::error`.
                if cb.items.is_empty() {
                    cb.error = Some(e.to_string());
                }
            }
        }
        cx.notify();
    }

    /// `channel_browse.rs`'s scroll-triggered paging (`should_load_more`).
    /// Re-checks the same guards inline, so a stray or duplicate call (this
    /// is wired from a render-time callback, not an edge-triggered one) is
    /// always safe to no-op.
    pub(crate) fn load_more_channel_items(&mut self, cx: &mut Context<Self>) {
        let Some(state) = self.main_state() else {
            return;
        };
        let Some(cb) = &state.channel_browse else {
            return;
        };
        if cb.exhausted || cb.loading_more || cb.loading {
            return;
        }
        let start_index = cb.items.len() as u32;
        self.spawn_channel_page(start_index, cx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- level_query -------------------------------------------------

    #[test]
    fn level_query_for_the_view_itself_is_name_asc() {
        let (parent, sort) = level_query("view-1", None);
        assert_eq!(parent, "view-1");
        assert_eq!(sort, LiveSort::NameAsc);
    }

    #[test]
    fn level_query_for_a_folder_is_newest_first() {
        let (parent, sort) = level_query("view-1", Some("folder-1"));
        assert_eq!(parent, "folder-1");
        assert_eq!(sort, LiveSort::NewestFirst);
    }

    // ---- should_load_more ---------------------------------------------

    #[test]
    fn should_load_more_fires_near_the_end_of_what_is_loaded() {
        assert!(should_load_more(80, 100, false, false));
        assert!(
            !should_load_more(50, 100, false, false),
            "not near the end yet"
        );
    }

    #[test]
    fn should_load_more_never_fires_while_a_page_is_already_in_flight() {
        assert!(!should_load_more(95, 100, false, true));
    }

    #[test]
    fn should_load_more_never_fires_once_a_level_is_exhausted() {
        assert!(!should_load_more(95, 100, true, false));
    }

    #[test]
    fn should_load_more_never_fires_before_the_initial_fetch_has_loaded_anything() {
        assert!(!should_load_more(0, 0, false, false));
    }

    // ---- recording_datetime_label ---------------------------------------

    #[test]
    fn recording_datetime_label_is_empty_for_a_missing_premiere_date() {
        assert_eq!(recording_datetime_label(None), "");
    }

    #[test]
    fn recording_datetime_label_converts_utc_to_local() {
        // Pins: conversion routes through `Local`, and distinct instants
        // yield distinct labels (exact wall-clock text is timezone-dependent).
        let d1: DateTime<Utc> = "2026-09-09T12:00:00Z"
            .parse()
            .expect("valid RFC3339 fixture");
        let d2: DateTime<Utc> = "2026-09-10T12:00:00Z"
            .parse()
            .expect("valid RFC3339 fixture");
        let label1 = recording_datetime_label(Some(d1));
        let label2 = recording_datetime_label(Some(d2));
        assert!(!label1.is_empty());
        assert_ne!(label1, label2, "two different instants must not collide");
    }
}
