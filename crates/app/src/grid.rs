//! Virtualized poster wall (Library view; docs/OVERVIEW.md §5b). Buckets `CardRow`s
//! into fixed-column rows and renders through `uniform_list`, which measures
//! one row and lays the rest out arithmetically rather than running full
//! taffy layout per row.
//!
//! Cancel-on-scroll (docs/DATA.md §3: visible + 1 screen ahead keep priority,
//! offscreen requests are dropped) is wired to `uniform_list`'s render-range
//! callback: a new first row bumps `ImageStore`'s generation and cancels
//! anything more than ~2 rows behind it.

use std::cell::Cell;
use std::rc::Rc;

use gpui::{
    div, prelude::*, px, uniform_list, AnyElement, App, Pixels, UniformListScrollHandle, WeakEntity,
};
use media_cache::CardRow;

use crate::cards::{poster_card, poster_spec_overlay};
use crate::image_store::ImageStore;
use crate::root::Root;
use crate::theme;
use crate::ui::spec_strip::SpecField;

/// A click/dwell callback for one grid cell's `CardRow`.
pub(crate) type ItemAction = Rc<dyn Fn(&CardRow, &mut App)>;
/// A real-mouse-move callback for one grid cell's flat index (not its
/// `CardRow` -- cheaper to thread per-cell than cloning a whole row).
pub(crate) type HoverAction = Rc<dyn Fn(usize, &mut App)>;

/// 2:3 poster cell width, matching `image_store::POSTER_WIDTH`'s fetch
/// bucket closely enough that gpui's downscale-to-fit does the rest
/// (docs/DATA.md §3: fetch at exactly the largest cell size in use).
pub(crate) const CELL_WIDTH: f32 = 160.0;
pub(crate) const CELL_GAP: f32 = 16.0;

/// How many columns fit in `available_width` at `CELL_WIDTH` + gaps.
///
/// The fixed-cell-width answer: `detail.rs`'s season/episode grids lay out
/// against `CELL_WIDTH` directly, so a column count derived from any other
/// pitch would overflow their rows. The Library poster wall uses its own
/// flex-width math instead (`library_columns_for_width`).
pub(crate) fn columns_for_width(available_width: Pixels) -> usize {
    let cell = CELL_WIDTH + CELL_GAP;
    (((f32::from(available_width) - CELL_GAP) / cell).floor() as usize).max(1)
}

// The Library poster wall's own column math.

/// The "5 columns -> 7 columns" target, expressed as a reference rather
/// than a hard count so the wall stays responsive: proportionally more/fewer
/// columns as the window grows or shrinks around this reference width.
pub(crate) const LIBRARY_REFERENCE_COLUMNS: usize = 7;
/// The window width the "7 columns" target above is calibrated against.
pub(crate) const LIBRARY_REFERENCE_WINDOW_WIDTH: f32 = 1500.0;
/// The Library page's horizontal gutters (`render_browse` subtracts the same
/// 48px when it computes `content_width`).
pub(crate) const LIBRARY_PAGE_GUTTERS: f32 = 48.0;
/// Content width left for the wall once the fixed sidebar (§12) and page
/// gutters are taken out: 1212px.
pub(crate) const LIBRARY_REFERENCE_CONTENT_WIDTH: f32 =
    LIBRARY_REFERENCE_WINDOW_WIDTH - crate::gl_video::SIDEBAR_WIDTH as f32 - LIBRARY_PAGE_GUTTERS;
/// One column's pitch implied by the two constants above: ~175px, putting
/// the reference cell within a pixel of `CELL_WIDTH` so `POSTER_WIDTH`'s
/// fetch bucket stays right at every window size this scales across.
pub(crate) const LIBRARY_CELL_PITCH: f32 =
    (LIBRARY_REFERENCE_CONTENT_WIDTH + CELL_GAP) / LIBRARY_REFERENCE_COLUMNS as f32;

/// Bounds on the flex cell width: below `LIBRARY_CELL_MIN_WIDTH` thumbnails
/// stop being readable; past `LIBRARY_CELL_MAX_WIDTH` column count grows instead.
pub(crate) const LIBRARY_CELL_MIN_WIDTH: f32 = 120.0;
pub(crate) const LIBRARY_CELL_MAX_WIDTH: f32 = 220.0;

/// §5's column count: how many `LIBRARY_CELL_PITCH` columns fit in
/// `available_width`. *n* columns need `n*cell + (n-1)*gap`, so the fit test
/// is `(available + gap) / pitch`, not `columns_for_width`'s `(available - gap) / pitch`.
pub(crate) fn library_columns_for_width(available_width: Pixels) -> usize {
    let available = f32::from(available_width);
    // Epsilon so the reference width lands on exactly 7 rather than 6.999.
    let fit = (available + CELL_GAP) / LIBRARY_CELL_PITCH + 1e-3;
    if fit < 1.0 {
        return 1;
    }
    (fit.floor() as usize).max(1)
}

/// §5's cell width: whatever makes `columns` exactly fill `available_width`
/// (no dead right-hand gutter at any window size), clamped to the band above.
pub(crate) fn library_cell_width(available_width: Pixels, columns: usize) -> Pixels {
    let columns = columns.max(1);
    let gaps = CELL_GAP * (columns - 1) as f32;
    let cell = (f32::from(available_width) - gaps) / columns as f32;
    px(cell.clamp(LIBRARY_CELL_MIN_WIDTH, LIBRARY_CELL_MAX_WIDTH))
}

/// §5's featured-backdrop pick: which item of `len` this library's page
/// draws its atmosphere from. Seeded from the library id and the day, never
/// per-frame randomness, so it can't flicker between renders of the same page.
pub(crate) fn featured_index(view_id: &str, day: u64, len: usize) -> Option<usize> {
    if len == 0 {
        return None;
    }
    // FNV-1a, 64-bit: no dependency, and stable across runs unlike `DefaultHasher`.
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in view_id.as_bytes().iter().chain(day.to_le_bytes().iter()) {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    Some((hash % len as u64) as usize)
}

/// Per-grid persistent state a view keeps around; `poster_grid` itself is stateless.
#[derive(Clone)]
pub(crate) struct GridScroll {
    pub handle: UniformListScrollHandle,
    last_seen_row: Rc<Cell<usize>>,
}

impl GridScroll {
    pub(crate) fn new() -> Self {
        GridScroll {
            handle: UniformListScrollHandle::new(),
            last_seen_row: Rc::new(Cell::new(usize::MAX)),
        }
    }

    pub(crate) fn scroll_to_row(&self, row: usize) {
        self.handle.scroll_to_item(row, gpui::ScrollStrategy::Top);
    }

    /// The cancel-on-scroll bookkeeping cell. Exposed so `library_list.rs`
    /// can run the same policy against its own scroll handle -- the two
    /// projections keep separate handles (to preserve scroll position across
    /// a mode switch), so each needs its own last-seen row too.
    pub(crate) fn last_seen_row(&self) -> Rc<Cell<usize>> {
        self.last_seen_row.clone()
    }
}

/// Renders `items` as a virtualized, focus-aware poster wall.
///
/// `on_hover` fires on real mouse-move only, keyed to the cell's flat index
/// -- not a hover boolean, which `GridScroll::scroll_to_row` (fired on every
/// keyboard move) would misread as a fresh hover when it repositions cells
/// under a stationary pointer (see `focus_grid::HighlightPolicy`'s doc).
#[allow(clippy::too_many_arguments)]
/// `cell_width`: the flex cell width for this window size.
///
/// `focused_spec`: the condensed spec strip, resolved once by the caller
/// per highlighted item rather than per-cell here (the fields come from a
/// `BaseItemDto` blob parse, which must not run per visible cell per frame).
pub(crate) fn poster_grid(
    id: &'static str,
    items: Rc<Vec<CardRow>>,
    columns: usize,
    cell_width: Pixels,
    focused_index: Option<usize>,
    focused_spec: Rc<Vec<SpecField>>,
    store: ImageStore,
    root: WeakEntity<Root>,
    scroll: GridScroll,
    on_open: ItemAction,
    on_dwell: ItemAction,
    on_hover: HoverAction,
) -> impl IntoElement {
    let columns = columns.max(1);
    let row_count = items.len().div_ceil(columns).max(1);
    let cell_w = cell_width;
    let last_seen_row = scroll.last_seen_row.clone();

    uniform_list(id, row_count, move |range, _window, cx| {
        let start_row = range.start;
        if last_seen_row.get() != start_row {
            last_seen_row.set(start_row);
            let gen = store.bump_generation();
            // One row of headroom is still worth finishing; two is not (docs/DATA.md §3).
            store.cancel_below_priority(gen.saturating_sub(2));
        }

        range
            .map(|row_ix| {
                // §3: only cells in the highlighted row dim.
                let row_has_highlight = focused_index.is_some_and(|fi| fi / columns == row_ix);
                let row_items: Vec<AnyElement> = (0..columns)
                    .filter_map(|col| {
                        let ix = row_ix * columns + col;
                        items.get(ix).cloned().map(|item| {
                            let focused = focused_index == Some(ix);
                            let row_dim = row_has_highlight && !focused;
                            let root = root.clone();
                            let store = store.clone();
                            let on_open = on_open.clone();
                            let on_dwell = on_dwell.clone();
                            let on_hover = on_hover.clone();
                            let click_item = item.clone();
                            let dwell_item = item.clone();
                            let spec = focused
                                .then(|| focused_spec.clone())
                                .filter(|fields| !fields.is_empty());
                            let card = poster_card(
                                &item,
                                focused,
                                row_dim,
                                cell_w,
                                &store,
                                root,
                                cx,
                                // Always eager: `range` already restricts rendering to visible rows.
                                true,
                                move |cx| on_open(&click_item, cx),
                                Some(move |cx: &mut App| on_dwell(&dwell_item, cx)),
                                Some(move |cx: &mut App| on_hover(ix, cx)),
                            );
                            // §5: the spec strip is a sibling of the card, not a child --
                            // `poster_card`'s art box is `overflow_hidden` and animates its
                            // scale, neither of which a pin-sharp readout should inherit.
                            match spec {
                                None => card,
                                Some(fields) => div()
                                    .relative()
                                    .child(card)
                                    .child(poster_spec_overlay(cell_w * 1.5, &fields))
                                    .into_any_element(),
                            }
                        })
                    })
                    .collect();
                div()
                    .flex()
                    .flex_row()
                    .gap(px(CELL_GAP))
                    .pb(px(CELL_GAP))
                    .children(row_items)
                    .into_any_element()
            })
            .collect()
    })
    .track_scroll(scroll.handle.clone())
    // Focus-ring clip slack: `uniform_list` clips to its full bounds, not padded
    // bounds, so this padding widens the clip box without moving the cells --
    // `root.rs`'s mounting container pulls its own inset back by the same amount.
    // Top slack matches the toolbar's `py_2` (8px) so it only covers toolbar padding.
    .px(theme::FOCUS_RING_CLEARANCE)
    .pt(theme::LIST_TOP_CLIP_SLACK)
    .w_full()
    .h_full()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pins the headline requirement: seven columns at the reference
    /// window width, where the old fixed-`CELL_WIDTH` math produced six.
    #[test]
    fn library_wall_is_seven_columns_at_the_reference_width() {
        let content = px(LIBRARY_REFERENCE_CONTENT_WIDTH);
        assert_eq!(library_columns_for_width(content), 7);
        assert_eq!(
            columns_for_width(content),
            6,
            "the fixed-cell math is unchanged"
        );
    }

    #[test]
    fn library_columns_scale_with_the_window() {
        // Wider window -> more columns, narrower -> fewer, monotonically.
        let at = |w: f32| library_columns_for_width(px(w));
        assert!(at(1632.) > at(LIBRARY_REFERENCE_CONTENT_WIDTH));
        assert!(at(800.) < at(LIBRARY_REFERENCE_CONTENT_WIDTH));
        let mut previous = 0;
        for w in (200..2400).step_by(50) {
            let columns = at(w as f32);
            assert!(
                columns >= previous,
                "column count must never shrink as width grows"
            );
            previous = columns;
        }
    }

    #[test]
    fn library_columns_never_drops_below_one() {
        assert_eq!(library_columns_for_width(px(0.)), 1);
        assert_eq!(library_columns_for_width(px(-100.)), 1);
        assert_eq!(library_columns_for_width(px(40.)), 1);
    }

    #[test]
    fn library_cells_fill_the_row_exactly() {
        let content = LIBRARY_REFERENCE_CONTENT_WIDTH;
        let columns = library_columns_for_width(px(content));
        let cell = f32::from(library_cell_width(px(content), columns));
        let used = cell * columns as f32 + CELL_GAP * (columns - 1) as f32;
        assert!(
            (used - content).abs() < 1.0,
            "{columns} columns of {cell}px must fill {content}px, used {used}px"
        );
    }

    #[test]
    fn library_cell_width_stays_in_the_readable_band() {
        for w in [0., 120., 640., 1212., 3000., 6000.] {
            let columns = library_columns_for_width(px(w));
            let cell = f32::from(library_cell_width(px(w), columns));
            assert!(
                (LIBRARY_CELL_MIN_WIDTH..=LIBRARY_CELL_MAX_WIDTH).contains(&cell),
                "cell {cell}px at width {w}px is outside the band"
            );
        }
    }

    // ---- §5's featured-backdrop pick --------------------------------

    #[test]
    fn featured_index_is_stable_for_a_library_within_a_day() {
        let a = featured_index("movies-view", 20_000, 76);
        let b = featured_index("movies-view", 20_000, 76);
        assert_eq!(a, b);
        assert!(a.expect("some index") < 76);
    }

    #[test]
    fn featured_index_rotates_across_days_and_differs_across_libraries() {
        let days: std::collections::HashSet<Option<usize>> = (20_000..20_030)
            .map(|day| featured_index("movies-view", day, 76))
            .collect();
        assert!(days.len() > 1, "the pick must rotate day to day");
        let libraries: std::collections::HashSet<Option<usize>> =
            ["movies", "shows", "kids", "boxsets"]
                .into_iter()
                .map(|view| featured_index(view, 20_000, 76))
                .collect();
        assert!(
            libraries.len() > 1,
            "two libraries must not lock to one index"
        );
    }

    #[test]
    fn featured_index_is_none_for_an_empty_candidate_list() {
        assert_eq!(featured_index("movies-view", 20_000, 0), None);
    }

    #[test]
    fn featured_index_is_always_in_range() {
        for len in 1..40usize {
            for day in 20_000..20_010 {
                let ix = featured_index("movies-view", day, len).expect("some index");
                assert!(ix < len);
            }
        }
    }
}
