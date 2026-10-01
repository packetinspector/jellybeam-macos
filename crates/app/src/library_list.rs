//! The Library screen's LIST projection -- Part C §5's Grid/List toggle.
//!
//! Same data, different projection: renders the *identical*
//! `LibraryState::items` slice the poster wall does, after the same sort/
//! filter popovers have already been applied by
//! `root.rs::apply_library_filters`. No second query, no second sort path;
//! switching modes costs one `cx.notify()`.
//!
//! Virtualization discipline is `grid.rs`'s, unchanged: rows go through
//! `uniform_list`, and every render-range call naming a new first row bumps
//! `ImageStore`'s generation and cancels anything more than ~2 screens
//! behind it (docs/DATA.md §3). A list row is ~1/4 the height of a wall row, so
//! the cancel horizon is counted in rows-per-screen here rather than
//! grid.rs's flat 2.

use std::cell::Cell;
use std::rc::Rc;

use gpui::{
    div, prelude::*, px, rgb, rgba, uniform_list, AnyElement, App, FontWeight, Pixels,
    SharedString, WeakEntity,
};
use media_cache::{CardRow, ImageKind};

use crate::cards::{
    art_element, display_title, format_runtime, poster_art_source, watch_indicator, watch_progress,
    watch_progress_bar, PosterArtSource,
};
use crate::grid::{GridScroll, HoverAction, ItemAction};
use crate::image_store::{ImageStore, POSTER_WIDTH};
use crate::root::Root;
use crate::theme;
use crate::ui::components::{clamped_line, focus_ring};
use crate::ui::spec_strip::{spec_strip_condensed, SpecField};

/// The 2:3 thumb's height. 48px is the smallest a poster crop stays
/// *recognisable* at (below it the title block of the artwork itself stops
/// resolving), and it is what sets the row height below.
const THUMB_HEIGHT: Pixels = px(48.);
/// 2:3, exactly as on the wall -- brand §5's poster ratio doesn't change
/// because the cell got small.
const THUMB_WIDTH: Pixels = px(32.);
/// The row's own box: the thumb plus 6px of breathing room top and bottom.
/// 60px keeps two lines of text (14px title + 12px meta) comfortably inside
/// while staying dense enough that a 1500px-tall window shows ~15 rows.
const ROW_HEIGHT: Pixels = px(60.);
/// Separation between rows. Also the exact clearance
/// `ui::components::focus_ring` needs for its 4px outset not to collide with
/// the neighbouring row's own fill.
const ROW_GAP: Pixels = px(4.);
/// What `uniform_list` actually lays out per row: 64px pitch.
const ROW_PITCH: Pixels = px(60. + 4.); // ROW_HEIGHT + ROW_GAP

/// One text line's reserved height at 14px (title) and 12px (meta) -- the
/// same two figures `cards.rs`'s `fixed_title_block` reserves, so a title
/// reads at the same size whichever projection it is rendered in.
const TITLE_LINE_HEIGHT: Pixels = px(20.);
const META_LINE_HEIGHT: Pixels = px(16.);

/// How many rows behind the visible top are still "worth finishing" before
/// their image fetches get cancelled. `grid.rs` uses 2 (of its ~250px-tall
/// wall rows); one screen of 64px list rows is roughly 12, so the same
/// *distance* policy is ~12 rows here -- expressed as a row count rather than
/// re-derived per frame from the viewport, which the render-range callback
/// has no access to anyway.
const CANCEL_HORIZON_ROWS: usize = 12;

/// `metadata`: the year/runtime line under a row's title. Both halves are
/// already on `CardRow` (mirror-projected), so this costs no fetch -- and a
/// row with neither simply shows nothing rather than a placeholder dash
/// (brand §6: no invented values).
fn meta_line(item: &CardRow) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(year) = item.production_year {
        parts.push(year.to_string());
    }
    if let Some(ticks) = item.runtime_ticks.filter(|t| *t > 0) {
        parts.push(format_runtime(ticks));
    }
    parts.join(" · ")
}

/// Renders `items` as a virtualized, focus-aware dense row list.
///
/// `spec_by_id`: `LibraryState::spec_by_id`, the condensed spec fields the
/// browse path already derived once per library in
/// `root.rs::compute_library_metadata`. Reading it here is a `HashMap` hit
/// per *visible* row and nothing else -- no network call, no new sync field,
/// no per-frame DTO parse. On the browse path that map in practice resolves
/// the resolution cell only (the bulk-sync blob carries no `MediaStreams` --
/// see `MediaFacts::condensed_fields`' own honesty note), which is exactly
/// the "resolution/codec readout" worth having in a dense row.
#[allow(clippy::too_many_arguments)]
pub(crate) fn library_list(
    id: &'static str,
    items: Rc<Vec<CardRow>>,
    spec_by_id: Rc<std::collections::HashMap<String, Vec<SpecField>>>,
    focused_index: Option<usize>,
    store: ImageStore,
    root: WeakEntity<Root>,
    scroll: GridScroll,
    on_open: ItemAction,
    on_dwell: ItemAction,
    on_hover: HoverAction,
) -> impl IntoElement {
    let row_count = items.len().max(1);
    let last_seen_row: Rc<Cell<usize>> = scroll.last_seen_row();

    uniform_list(id, row_count, move |range, _window, cx| {
        let start_row = range.start;
        if last_seen_row.get() != start_row {
            last_seen_row.set(start_row);
            let gen = store.bump_generation();
            store.cancel_below_priority(gen.saturating_sub(CANCEL_HORIZON_ROWS as u64));
        }

        range
            .filter_map(|ix| {
                let item = items.get(ix)?.clone();
                let focused = focused_index == Some(ix);
                let spec = spec_by_id.get(&item.id);
                Some(list_row(
                    &item,
                    ix,
                    focused,
                    spec.map(|f| f.as_slice()).unwrap_or_default(),
                    &store,
                    root.clone(),
                    cx,
                    on_open.clone(),
                    on_dwell.clone(),
                    on_hover.clone(),
                ))
            })
            .collect()
    })
    .track_scroll(scroll.handle.clone())
    // Focus-ring clip slack: `uniform_list` clips to its FULL bounds, not
    // padded bounds, so padding here widens the clip box around the cells
    // without moving them -- the mounting container in `root.rs` pulls its
    // own inset back by the same amounts, or the leftmost column and top
    // row lose the ring's outer 4px to the list's clip edge. The top slack
    // is the toolbar's own `py_2` (8px) so the overlap only ever covers
    // toolbar padding, never a button's hitbox.
    .px(theme::FOCUS_RING_CLEARANCE)
    .pt(theme::LIST_TOP_CLIP_SLACK)
    .w_full()
    .h_full()
}

/// One row. Brand §5, "Cards, sidebar rows, list rows -- 8px radius,
/// `SURFACE` fill, no border": the fill arrives on hover (and stays for the
/// keyboard-focused row), the radius is `RADIUS_CARD`, and there is no border
/// in any state. The focused row additionally carries the standardized
/// `ui::components::focus_ring` -- a 2px pistachio ring at 2px offset,
/// mounted as an absolutely-positioned, hitbox-free overlay so it can never
/// swallow the row's own click.
///
/// Deliberately NOT `poster_card`'s focus recipe: §3's scale/brightness/
/// sibling-dim animation is a *poster wall* treatment (it exists so one cell
/// separates from a dense field of equally-sized artwork). A list already
/// separates its rows by position, and animating a 60px row's scale would
/// shove its neighbours a pixel on every arrow keypress.
#[allow(clippy::too_many_arguments)]
fn list_row(
    item: &CardRow,
    index: usize,
    focused: bool,
    spec: &[SpecField],
    store: &ImageStore,
    root: WeakEntity<Root>,
    cx: &mut App,
    on_open: ItemAction,
    on_dwell: ItemAction,
    on_hover: HoverAction,
) -> AnyElement {
    // §2.5's poster fallback chain, unchanged and for the same reason: an
    // Episode's own 16:9 still must never be cropped into a 2:3 slot, however
    // small that slot is. Fetched at `POSTER_WIDTH` -- the *same* bucket the
    // wall uses, so switching modes paints from the already-warmed disk/
    // texture cache instead of issuing a second set of requests at a second
    // width (`image_warm.rs` warms exactly this key).
    let (art_item_id, art_tag, art_blurhash) = match poster_art_source(item) {
        PosterArtSource::Own(tag) => (item.id.as_str(), Some(tag), item.blurhash.as_deref()),
        PosterArtSource::SeriesFallback(series_id, tag) => (series_id, Some(tag), None),
        PosterArtSource::None => (item.id.as_str(), None, item.blurhash.as_deref()),
    };
    let art = art_element(
        art_item_id,
        art_tag,
        art_blurhash,
        ImageKind::Primary,
        POSTER_WIDTH,
        store,
        root,
        cx,
        // Always eager: `uniform_list`'s range already restricts this to the
        // visible rows (see `library_list`'s cancel-on-scroll wiring).
        true,
    );

    let progress = watch_progress(item);
    let thumb = div()
        .relative()
        .flex_shrink_0()
        .w(THUMB_WIDTH)
        .h(THUMB_HEIGHT)
        // Brand §5: "Poster art -- 2px radius, no shadow."
        .rounded(theme::RADIUS_ART)
        .overflow_hidden()
        .child(art)
        .children(progress.map(|p| {
            watch_progress_bar(p)
                .absolute()
                .bottom_0()
                .left_0()
                .right_0()
        }));

    // §12: every title in the app passes through `display_title` (strips a
    // wrapping quote pair the server stored around the name -- a presentation
    // unwrap, never a rename; the mirrored value is untouched).
    let title = display_title(&item.name);
    let text_block = div()
        .flex()
        .flex_col()
        .flex_1()
        // Without this a flex item's min-width is its content size, which
        // silently defeats `clamped_line`'s truncation (see that fn's doc).
        .min_w_0()
        .child(
            clamped_line(title, TITLE_LINE_HEIGHT)
                .w_full()
                .text_sm()
                .font_weight(FontWeight::MEDIUM)
                .text_color(rgba(theme::TEXT_PRIMARY)),
        )
        .child(
            clamped_line(meta_line(item), META_LINE_HEIGHT)
                .w_full()
                .text_xs()
                .text_color(rgba(theme::TEXT_TERTIARY)),
        );

    // §5's spec readout, on the rows where it means something: a Movie is one
    // file with one resolution, so "1080P" is a fact about the row. A Series
    // or BoxSet row is a container of many files, and printing one member's
    // resolution against the whole thing would be a claim the data doesn't
    // support. Uses the shared `spec_strip_condensed` (Martian Mono at
    // `TEXT_SPEC`, `GRIGIO` baseline, brighter only where §5's data-driven
    // classifier says so), never a hand-set colour.
    let spec_cell = (item.item_type == "Movie" && !spec.is_empty())
        .then(|| div().flex_shrink_0().child(spec_strip_condensed(spec)));

    let badge = watch_indicator(item, progress).map(|badge| div().flex_shrink_0().child(badge));

    let open_item = item.clone();
    let dwell_item = item.clone();
    let dwell_key = item.id.clone();
    let store_for_hover = store.clone();

    let row = div()
        .id(SharedString::from(format!("library-row-{}", item.id)))
        .relative()
        .h(ROW_HEIGHT)
        // The pitch wrapper below is a plain block, so the row would already
        // fill it -- stated explicitly so a future wrapper change can't
        // silently shrink every row to its content width.
        .w_full()
        .flex()
        .flex_row()
        .items_center()
        .gap(theme::SPACE_SNUG)
        .px(theme::SPACE_SNUG)
        // Brand §5: 8px radius, SURFACE fill, no border -- in every state.
        .rounded(theme::RADIUS_CARD)
        .cursor_pointer()
        .when(focused, |d| d.bg(rgb(theme::SURFACE_RAISED)))
        .hover(|s| s.bg(rgb(theme::SURFACE_RAISED)))
        .child(thumb)
        .child(text_block)
        .children(spec_cell)
        .children(badge)
        .on_click(move |_event, _window, cx| on_open(&open_item, cx))
        // A REAL mouse-move, never a hover boolean -- `uniform_list`
        // repositions rows under a stationary pointer on every
        // keyboard-driven scroll, which a hover boolean would misread as a
        // fresh hover (see `focus_grid::HighlightPolicy`'s doc comment).
        .on_mouse_move(move |_event, _window, cx| on_hover(index, cx))
        .on_hover(move |hovering, _window, cx| {
            if *hovering {
                let epoch = store_for_hover.dwell_enter(&dwell_key);
                let store = store_for_hover.clone();
                let key = dwell_key.clone();
                let on_dwell = on_dwell.clone();
                let item = dwell_item.clone();
                cx.spawn(async move |cx| {
                    cx.background_executor()
                        .timer(std::time::Duration::from_millis(350))
                        .await;
                    if store.dwell_is_current(&key, epoch) {
                        let _ = cx.update(|cx| on_dwell(&item, cx));
                    }
                })
                .detach();
            } else {
                store_for_hover.dwell_exit(&dwell_key);
            }
        })
        .when(focused, |d| d.child(focus_ring(theme::RADIUS_CARD)));

    div().h(ROW_PITCH).pb(ROW_GAP).child(row).into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(item_type: &str) -> CardRow {
        CardRow {
            id: "id-1".into(),
            item_type: item_type.into(),
            name: "A Title".into(),
            primary_tag: None,
            blurhash: None,
            played: false,
            position_ticks: 0,
            runtime_ticks: None,
            unplayed_count: None,
            production_year: None,
            index_number: None,
            premiere_date: None,
            parent_index_number: None,
            series_id: None,
            series_primary_tag: None,
            parent_backdrop_item_id: None,
            parent_backdrop_tag: None,
            series_name: None,
            last_played_date: None,
            overview: None,
            is_virtual: false,
            library_id: None,
        }
    }

    /// The row pitch is what `uniform_list` multiplies by the item count to
    /// size its scroll extent, so it must stay exactly row + gap -- an
    /// off-by-a-pixel here shows up as drift over thousands of rows.
    #[test]
    fn row_pitch_is_row_height_plus_gap() {
        assert_eq!(
            f32::from(ROW_PITCH),
            f32::from(ROW_HEIGHT) + f32::from(ROW_GAP)
        );
    }

    /// The gap has to clear `focus_ring`'s outset, or the focused row's ring
    /// overlaps the row above/below it.
    #[test]
    fn the_row_gap_clears_the_focus_ring_outset() {
        assert!(f32::from(ROW_GAP) >= f32::from(theme::FOCUS_RING_OUTSET));
    }

    #[test]
    fn meta_line_joins_year_and_runtime_and_omits_what_is_missing() {
        let mut item = row("Movie");
        assert_eq!(meta_line(&item), "");

        item.production_year = Some(1999);
        assert_eq!(meta_line(&item), "1999");

        item.runtime_ticks = Some(90 * 60 * 10_000_000);
        assert_eq!(meta_line(&item), "1999 · 1h 30m");

        item.production_year = None;
        assert_eq!(meta_line(&item), "1h 30m");
    }

    /// A zero runtime is the mirror's "unknown", not a real zero-length file
    /// -- printing "0m" would be inventing a fact (brand §6).
    #[test]
    fn meta_line_drops_a_zero_runtime() {
        let mut item = row("Movie");
        item.production_year = Some(2004);
        item.runtime_ticks = Some(0);
        assert_eq!(meta_line(&item), "2004");
    }
}
