//! Home screen: Continue Watching / Next Up / Latest-per-library shelves
//! (docs/UX-SPEC.md §1/§5). Shelves render as plain flex rows, not through
//! `uniform_list` like the Library poster wall in `grid.rs`, so each shelf
//! tracks its own horizontal `ScrollHandle` and only lets cells within
//! `EAGER_COLUMN_MARGIN` columns of the viewport (or the focused column)
//! issue an image fetch -- see `render_shelf`. This windows within-shelf
//! only; a shelf below the fold still eagerly loads its own first screen.

use std::cell::RefCell;
use std::collections::HashSet;
use std::rc::Rc;
use std::time::Instant;

use gpui::svg;
use gpui::{
    div, linear_color_stop, linear_gradient, point, prelude::*, px, relative, rgb, rgba,
    AnimationExt, AnyElement, App, Context, FontWeight, Pixels, ScrollHandle, SharedString,
    WeakEntity,
};
use media_cache::{CardRow, ImageKind, Mirror};

use crate::cards::{poster_card, resume_card};
use crate::detail::backdrop_source;
use crate::focus_grid::ShelfFocus;
use crate::grid::{CELL_GAP, CELL_WIDTH};
use crate::image_store::{ImageStore, BACKDROP_WIDTH};
use crate::root::Root;
use crate::scroll_axis::{Axis, AxisLock};
use crate::theme;
use crate::ui::components::{button, ButtonSize, ButtonVariant};

/// "1 screen ahead" headroom on each side of a shelf's viewport, in columns
/// (docs/DATA.md §3's visible-range policy, applied per-shelf per the module doc).
const EAGER_COLUMN_MARGIN: i32 = 4;

pub(crate) struct Shelf {
    pub title: String,
    pub items: Vec<CardRow>,
}

pub(crate) struct HomeState {
    pub shelves: Vec<Shelf>,
    pub focus: ShelfFocus,
    /// Whether the user has interacted with Home's focus (arrow key or card
    /// hover) this run. Until set, `render` paints no focus ring or sibling
    /// dimming, so a freshly launched Home shows no unexplained highlight.
    /// Set by `Root::move_focus`/`hover_home_cell`; `refresh` never resets it.
    pub focus_engaged: bool,
    /// Counts how many times `refresh` actually replaced `shelves` with new
    /// content, not how many times it was called -- `refresh` skips the
    /// replace when the re-queried shelves are identical to what's on screen.
    rebuild_count: u64,
    /// One horizontal `ScrollHandle` per shelf, index-aligned with
    /// `shelves`, so `render_shelf` can read each shelf's actual scroll
    /// offset for the eager-load window (module doc comment).
    shelf_scrolls: Vec<ScrollHandle>,
    /// One `AxisLock` per shelf, index-aligned with `shelf_scrolls`; see
    /// `scroll_axis.rs`'s module doc comment for why each shelf needs its
    /// own gesture-lock state.
    shelf_axis_locks: Vec<Rc<RefCell<AxisLock>>>,
    /// Tracks the outer vertical shelf list (`render`'s `#home` container).
    /// `scroll_to_focus` uses this for cross-shelf (Up/Down) auto-scroll,
    /// and each per-shelf handle in `shelf_scrolls` for within-shelf scroll.
    outer_scroll: ScrollHandle,
}

impl HomeState {
    /// `hidden_libraries`: library ids to exclude -- skips their "Latest in
    /// X" shelf entirely and drops their items from Continue Watching/Next
    /// Up too, since those two shelves are cross-library.
    ///
    /// `hide_watched_latest`: forwarded to `Mirror::latest`'s own
    /// `hide_watched` param; not applied to `resume()`/`next_up()`, which
    /// are already exclusively unwatched/in-progress items by construction.
    pub(crate) fn load(
        mirror: &Mirror,
        views: &[media_cache::ViewSummary],
        hidden_libraries: &HashSet<String>,
        hide_watched_latest: bool,
    ) -> Self {
        let mut shelves = Vec::new();

        let continue_watching = filter_hidden_libraries(mirror.resume(30), hidden_libraries);
        // Captured before ownership moves into the shelf; feeds the dedup
        // pass below.
        let resume_ids: Vec<String> = continue_watching.iter().map(|c| c.id.clone()).collect();
        if !continue_watching.is_empty() {
            shelves.push(Shelf {
                title: "Continue Watching".to_string(),
                items: continue_watching,
            });
        }

        // Deduplicated at display time only -- the mirror queries themselves
        // stay untouched since an item can legitimately belong to both sets;
        // showing it twice on one screen is the bug being fixed.
        let next_up = dedup_against(
            &resume_ids,
            filter_hidden_libraries(mirror.next_up(30), hidden_libraries),
        );
        if !next_up.is_empty() {
            shelves.push(Shelf {
                title: "Next Up".to_string(),
                items: next_up,
            });
        }

        for view in views_for_latest_shelves(views, hidden_libraries) {
            let latest = mirror.latest(&view.id, 30, hide_watched_latest);
            if !latest.is_empty() {
                shelves.push(Shelf {
                    title: format!("Latest in {}", view.name),
                    items: latest,
                });
            }
        }

        let mut focus = ShelfFocus::new();
        let lens: Vec<usize> = shelves.iter().map(|s| s.items.len()).collect();
        focus.clamp(&lens);

        let shelf_scrolls = (0..shelves.len()).map(|_| ScrollHandle::new()).collect();
        let shelf_axis_locks = (0..shelves.len())
            .map(|_| Rc::new(RefCell::new(AxisLock::new())))
            .collect();

        HomeState {
            shelves,
            focus,
            focus_engaged: false,
            rebuild_count: 0,
            shelf_scrolls,
            shelf_axis_locks,
            outer_scroll: ScrollHandle::new(),
        }
    }

    pub(crate) fn shelf_lens(&self) -> Vec<usize> {
        self.shelves.iter().map(|s| s.items.len()).collect()
    }

    pub(crate) fn shelf_rebuild_count(&self) -> u64 {
        self.rebuild_count
    }

    /// Index-aligned with `shelves`; falls back to a fresh, unscrolled
    /// handle if the two are momentarily out of sync, defaulting the eager
    /// window to "near the start" rather than panicking.
    fn shelf_scroll(&self, ix: usize) -> ScrollHandle {
        self.shelf_scrolls.get(ix).cloned().unwrap_or_default()
    }

    /// Same index-aligned/fresh-fallback shape as `shelf_scroll` above.
    fn shelf_axis_lock(&self, ix: usize) -> Rc<RefCell<AxisLock>> {
        self.shelf_axis_locks
            .get(ix)
            .cloned()
            .unwrap_or_else(|| Rc::new(RefCell::new(AxisLock::new())))
    }

    /// Auto-scrolls both axes to keep the focused cell visible: the outer
    /// page to the focused shelf, that shelf's own scroll to the focused
    /// column. Called from `detail.rs::move_focus` after each arrow-key move,
    /// not from `render`, so it doesn't fight the viewer's free-scrolling.
    pub(crate) fn scroll_to_focus(&self) {
        self.outer_scroll.scroll_to_item(self.focus.shelf);
        self.shelf_scroll(self.focus.shelf)
            .scroll_to_item(self.focus.column);
    }

    /// Re-runs the shelf queries in place (docs/UX-SPEC.md §5: "update live on
    /// WebSocket events"), preserving focus via `ShelfFocus::clamp`, and
    /// returns whether `shelves` actually changed.
    ///
    /// Diffs the freshly-queried shelves against the current ones first and
    /// only swaps when something genuinely changed, since not every
    /// `MirrorChange` touches something a Home shelf shows.
    ///
    /// While `mirror.is_syncing()`, merges into the current shelves via
    /// `merge_shelves_preserving_order` instead of replacing wholesale, so
    /// tiles already on screen don't reorder on every debounced sync tick;
    /// content (badges, art) still updates live, and new items append at
    /// the end. Once sync completes, the next call does one final ordered
    /// replace.
    pub(crate) fn refresh(
        &mut self,
        mirror: &Mirror,
        views: &[media_cache::ViewSummary],
        hidden_libraries: &HashSet<String>,
        hide_watched_latest: bool,
        syncing: bool,
    ) -> bool {
        let fresh = Self::load(mirror, views, hidden_libraries, hide_watched_latest);
        let next_shelves = if syncing {
            merge_shelves_preserving_order(&self.shelves, &fresh.shelves)
        } else {
            fresh.shelves
        };
        if shelves_equal(&self.shelves, &next_shelves) {
            return false;
        }
        self.rebuild_count += 1;
        let focus = self.focus;
        self.shelves = next_shelves;
        self.focus = focus;
        self.focus.clamp(&self.shelf_lens());
        // Keep `shelf_scrolls` index-aligned with `shelves`; existing
        // handles are kept as-is (shelf order is stable in practice) so a
        // refresh preserves each shelf's scroll position.
        self.shelf_scrolls
            .resize_with(self.shelves.len(), ScrollHandle::new);
        self.shelf_axis_locks.resize_with(self.shelves.len(), || {
            Rc::new(RefCell::new(AxisLock::new()))
        });
        true
    }

    pub(crate) fn focused_item(&self) -> Option<CardRow> {
        self.shelves
            .get(self.focus.shelf)
            .and_then(|s| s.items.get(self.focus.column))
            .cloned()
    }
}

/// Merges a freshly-queried shelf's items into the current ones, preserving
/// the position of every item still present in `fresh` (content like badges
/// and art updates in place) rather than adopting the fresh sort order.
/// Items no longer in `fresh` are dropped; genuinely new items append at the
/// end in `fresh`'s relative order.
fn merge_items_preserving_order(current: &[CardRow], fresh: &[CardRow]) -> Vec<CardRow> {
    let fresh_by_id: std::collections::HashMap<&str, &CardRow> =
        fresh.iter().map(|r| (r.id.as_str(), r)).collect();
    let existing_ids: std::collections::HashSet<&str> =
        current.iter().map(|r| r.id.as_str()).collect();

    let mut merged: Vec<CardRow> = current
        .iter()
        .filter_map(|r| fresh_by_id.get(r.id.as_str()).map(|f| (*f).clone()))
        .collect();
    merged.extend(
        fresh
            .iter()
            .filter(|r| !existing_ids.contains(r.id.as_str()))
            .cloned(),
    );
    merged
}

/// The shelf-list-level counterpart of `merge_items_preserving_order`:
/// existing shelves (matched by title) keep their position and get their
/// items merged the same order-preserving way; a shelf that's brand new
/// this refresh appends at the end instead of following `HomeState::load`'s
/// normal ordering, so a visible shelf never jumps mid-sync.
fn merge_shelves_preserving_order(current: &[Shelf], fresh: &[Shelf]) -> Vec<Shelf> {
    let fresh_by_title: std::collections::HashMap<&str, &Shelf> =
        fresh.iter().map(|s| (s.title.as_str(), s)).collect();
    let existing_titles: std::collections::HashSet<&str> =
        current.iter().map(|s| s.title.as_str()).collect();

    let mut merged: Vec<Shelf> = current
        .iter()
        .filter_map(|s| {
            fresh_by_title.get(s.title.as_str()).map(|f| Shelf {
                title: f.title.clone(),
                items: merge_items_preserving_order(&s.items, &f.items),
            })
        })
        .collect();
    merged.extend(
        fresh
            .iter()
            .filter(|s| !existing_titles.contains(s.title.as_str()))
            .map(|s| Shelf {
                title: s.title.clone(),
                items: s.items.clone(),
            }),
    );
    merged
}

/// Compares two shelf lists on exactly the fields `render`/`poster_card`
/// paint (order, id, title, art, watched/progress badges), not full
/// `CardRow` equality -- a field like `production_year` never reaches the
/// screen, so a change confined to it must not trigger a rebuild.
fn shelves_equal(a: &[Shelf], b: &[Shelf]) -> bool {
    a.len() == b.len()
        && a.iter().zip(b).all(|(sa, sb)| {
            sa.title == sb.title
                && sa.items.len() == sb.items.len()
                && sa
                    .items
                    .iter()
                    .zip(&sb.items)
                    .all(|(ia, ib)| card_row_rendered_state_equal(ia, ib))
        })
}

fn card_row_rendered_state_equal(a: &CardRow, b: &CardRow) -> bool {
    a.id == b.id
        && a.name == b.name
        && a.primary_tag == b.primary_tag
        && a.blurhash == b.blurhash
        && a.played == b.played
        && a.position_ticks == b.position_ticks
        && a.runtime_ticks == b.runtime_ticks
        // `poster_card`'s artwork fallback chain branches on `item_type`
        // and falls back to `series_id`/`series_primary_tag`, so all three
        // affect what a shelf cell paints.
        && a.item_type == b.item_type
        && a.series_id == b.series_id
        && a.series_primary_tag == b.series_primary_tag
        // Continue Watching/Next Up render through `resume_card`, which
        // also reads these fields for its art fallback and text block, so
        // they affect what those two shelves paint too.
        && a.parent_backdrop_item_id == b.parent_backdrop_item_id
        && a.parent_backdrop_tag == b.parent_backdrop_tag
        && a.index_number == b.index_number
        && a.parent_index_number == b.parent_index_number
        && a.series_name == b.series_name
        && a.is_virtual == b.is_virtual
        && a.premiere_date == b.premiere_date
}

/// The hero item: `shelves[0].items[0]` when that shelf is Continue
/// Watching, reusing `HomeState::load`'s existing query (no new fetch); no
/// hero when nothing is in progress.
pub(crate) fn hero_candidate(state: &HomeState) -> Option<&CardRow> {
    let first = state.shelves.first()?;
    if first.title != "Continue Watching" {
        return None;
    }
    first.items.first()
}

/// Icon + label resume button, built bespoke rather than through
/// `ui::components::button` (whose `label` only accepts text, no icon slot)
/// but reusing that fn's Primary/`Lg` sizing so it matches visually.
///
/// Brand §5 Primary button: `PISTACCHIO` fill, `NOTTE` label. A fixed accent
/// over arbitrary hero artwork used to clash with the backdrop, so the hero
/// now carries a `NOTTE` scrim (`backdrop.rs`) instead of a quieter fill.
fn hero_resume_button(item_id: String, item_name: String, root: WeakEntity<Root>) -> AnyElement {
    div()
        .id("hero-resume")
        .h(px(44.))
        .px(px(22.))
        .rounded(theme::RADIUS_PILL)
        .flex()
        .items_center()
        .justify_center()
        .gap(theme::SPACE_COMPACT)
        .bg(rgb(theme::PRIMARY_BUTTON_BG))
        .cursor_pointer()
        .hover(|s| s.bg(rgb(theme::PRIMARY_BUTTON_BG_HOVER)))
        .active(|s| s.bg(rgb(theme::PRIMARY_BUTTON_BG_PRESSED)))
        .child(
            svg()
                .path("icons/play.svg")
                .w(px(15.))
                .h(px(15.))
                .text_color(rgb(theme::PRIMARY_BUTTON_TEXT)),
        )
        .child(
            div()
                .text_size(theme::TEXT_BODY)
                .font_weight(gpui::FontWeight::BOLD)
                .text_color(rgb(theme::PRIMARY_BUTTON_TEXT))
                .child("Resume"),
        )
        .on_click(move |_e, _w, cx| {
            let _ = root.update(cx, |root, cx| {
                root.play_item(item_id.clone(), item_name.clone(), cx)
            });
        })
        .into_any_element()
}

// Brand §2 bans a second accent hue, gradients, and translucency, which
// ruled out painting the hero's ambient-sampled colour as a page wash.
// `ImageStore::ambient_color`/`cards::poster_ambient_source` stay in place
// for a future non-tinting consumer (e.g. scrim depth) even though nothing
// here paints with them now.

/// Eyebrow/metadata derivation, pulled out of `render_hero` as a pure fn so
/// it's unit-testable without a live `ImageStore`/`Mirror`. `series_name`
/// comes from the full DTO (a `CardRow` alone has no series display name),
/// so it's threaded in as a plain `Option<&str>` rather than the DTO itself.
fn hero_eyebrow_and_meta(item: &CardRow, series_name: Option<&str>) -> (Option<String>, String) {
    let is_episode = item.parent_index_number.is_some() && item.index_number.is_some();
    if !is_episode {
        let meta = item
            .production_year
            .map(|y| y.to_string())
            .unwrap_or_default();
        return (None, meta);
    }
    let eyebrow = series_name.map(str::to_string);
    let meta = match (item.parent_index_number, item.index_number) {
        (Some(s), Some(e)) => {
            let mut parts = vec![format!("S{s} E{e}")];
            if let Some(rt) = item.runtime_ticks {
                parts.push(crate::cards::format_runtime(rt));
            }
            if let Some(y) = item.production_year {
                parts.push(y.to_string());
            }
            parts.join(" · ")
        }
        _ => String::new(),
    };
    (eyebrow, meta)
}

/// The hero backdrop + title/metadata/CTA stack. Backdrop art reuses
/// `detail.rs::backdrop_source`'s fallback chain against a full
/// `BaseItemDto` fetched once via `Mirror::item` (a local blob-parse, not a
/// network round trip), since the browse `CardRow` carries no backdrop tag.
///
/// Also returns the backdrop's ambient color (`None` until it decodes) for
/// callers that want it; `render` currently discards it.
fn render_hero(
    item: &CardRow,
    mirror: &Mirror,
    store: &ImageStore,
    root: WeakEntity<Root>,
    cx: &mut App,
) -> (AnyElement, Option<u32>) {
    let dto = mirror.item(&item.id);
    let backdrop = dto.as_ref().and_then(backdrop_source);
    // Sharp + processed backdrop variants, composited by `backdrop::layer`
    // (see its doc comment for the full stack).
    let sharp_tex = backdrop.clone().and_then(|(id, tag)| {
        store.get(
            &id,
            ImageKind::Backdrop,
            &tag,
            BACKDROP_WIDTH,
            root.clone(),
            cx,
        )
    });
    let blurred_tex = backdrop.clone().and_then(|(id, tag)| {
        store.get_scrim(
            &id,
            ImageKind::Backdrop,
            &tag,
            BACKDROP_WIDTH,
            root.clone(),
            cx,
        )
    });
    let art = crate::backdrop::layer(sharp_tex, blurred_tex);

    // Pure peek at the already-decoded backdrop's average color; `store.get`
    // above already kicked off/resolved the fetch, so no extra fetch here.
    let ambient = backdrop
        .as_ref()
        .and_then(|(id, tag)| store.ambient_color(id, ImageKind::Backdrop, tag, BACKDROP_WIDTH));

    // §9: series name as eyebrow, episode title as the Display title, then
    // "S1 E3 · 24m · 2019" metadata (movies: no eyebrow) -- never repeat the
    // title between the two.
    let series_name = dto.as_ref().and_then(|d| d.series_name.clone());
    let (eyebrow, meta) = hero_eyebrow_and_meta(item, series_name.as_deref());

    let resume_root = root.clone();
    let detail_root = root;
    let resume_id = item.id.clone();
    let resume_name = item.name.clone();
    let detail_id = item.id.clone();

    let hero = div()
        .relative()
        .w_full()
        .h(relative(0.42))
        .min_h(px(320.))
        // §9: full-bleed to the top and both edges of the content area, no
        // gutter or top rounding.
        .overflow_hidden()
        .child(art)
        // §6: 120px band, transparent at top fading to `SURFACE_BASE` at
        // the hero's bottom edge, for a soft dissolve into the page below.
        .child(
            div()
                .absolute()
                .bottom_0()
                .left_0()
                .right_0()
                .h(px(120.))
                .bg(linear_gradient(
                    180.,
                    linear_color_stop(rgba(theme::TRANSPARENT), 0.0),
                    linear_color_stop(rgb(theme::SURFACE_BASE), 1.0),
                )),
        )
        .child(
            div()
                .absolute()
                .left(theme::SPACE_PAGE)
                .right(theme::SPACE_PAGE)
                .bottom(theme::SPACE_LOOSE)
                .flex()
                .flex_col()
                .gap_2()
                .children(eyebrow.map(|e| {
                    div()
                        .text_size(theme::TEXT_METADATA)
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(rgba(theme::TEXT_TERTIARY))
                        .child(SharedString::from(e.to_uppercase()))
                }))
                .child(
                    div()
                        .text_size(theme::TEXT_DISPLAY)
                        .line_height(theme::TEXT_DISPLAY_LINE_HEIGHT)
                        .font_weight(FontWeight::BOLD)
                        .text_color(rgba(theme::TEXT_PRIMARY))
                        // §12: see `cards::display_title`.
                        .child(SharedString::from(crate::cards::display_title(&item.name))),
                )
                .children((!meta.is_empty()).then(|| {
                    theme::apply_tabular_nums(
                        div()
                            .text_size(theme::TEXT_METADATA)
                            .text_color(rgba(theme::TEXT_TERTIARY))
                            .child(SharedString::from(meta)),
                    )
                }))
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .gap_2()
                        .mt_2()
                        .child(hero_resume_button(resume_id, resume_name, resume_root))
                        .child(
                            button(
                                "hero-more-info",
                                "More Info",
                                ButtonVariant::Secondary,
                                ButtonSize::Lg,
                                false,
                            )
                            .on_click(move |_e, _w, cx| {
                                let _ = detail_root
                                    .update(cx, |root, cx| root.open_detail(detail_id.clone(), cx));
                            }),
                        ),
                ),
        )
        .into_any_element();

    (hero, ambient)
}

/// A slim pulsing "Syncing your library…" line while `Mirror::is_syncing()`
/// and no shelf rebuild has landed yet. Non-blocking -- shelves still render
/// whatever's already there -- and disappears once sync completes or any
/// shelf has content.
fn render_sync_indicator(mirror: &Mirror, rebuild_count: u64) -> Option<AnyElement> {
    if rebuild_count != 0 || !mirror.is_syncing() {
        return None;
    }
    Some(
        div()
            .flex()
            .justify_end()
            .child(
                div()
                    .text_size(theme::TEXT_METADATA)
                    .text_color(rgba(theme::TEXT_TERTIARY))
                    .child("Syncing your library…")
                    .with_animation(
                        "home-sync-pulse",
                        theme::skeleton_animation(),
                        |el, delta| el.opacity(theme::skeleton_opacity(delta)),
                    ),
            )
            .into_any_element(),
    )
}

pub(crate) fn render(
    state: &HomeState,
    store: &ImageStore,
    mirror: &Mirror,
    // Content pane width; resume shelves size cards to "five visible, sixth
    // bleeding off the edge" from this -- see `resume_card_width`.
    content_width: gpui::Pixels,
    root: WeakEntity<Root>,
    cx: &mut Context<Root>,
) -> impl IntoElement {
    // The shelves column is inset by `SPACE_SECTION` on both sides (see the
    // padded wrapper at the bottom of this fn).
    let shelf_width = (content_width - theme::SPACE_SECTION * 2.0).max(px(0.));
    let hero_item = hero_candidate(state).cloned();
    let (hero, ambient) = match &hero_item {
        Some(item) => {
            let (hero, ambient) = render_hero(item, mirror, store, root.clone(), cx);
            (Some(hero), ambient)
        }
        None => (None, None),
    };
    // Brand §2 removed the ambient wash this color used to paint;
    // `render_hero` still returns it rather than changing its signature.
    let _ = ambient;
    let sync_indicator = render_sync_indicator(mirror, state.shelf_rebuild_count());

    let rows = state
        .shelves
        .iter()
        .enumerate()
        .map(|(shelf_ix, shelf)| {
            render_shelf(
                shelf,
                shelf_ix,
                shelf_width,
                state.focus,
                state.focus_engaged,
                store,
                root.clone(),
                state.shelf_scroll(shelf_ix),
                state.shelf_axis_lock(shelf_ix),
                cx,
            )
        })
        .collect::<Vec<_>>();

    // §9: the hero is full-bleed, so the page's own padding (previously on
    // this whole column) now wraps only the shelves section below it.
    div()
        .id("home")
        .size_full()
        .relative()
        .flex()
        .flex_col()
        .overflow_y_scroll()
        .track_scroll(&state.outer_scroll)
        .bg(rgb(theme::SURFACE_BASE))
        .children(hero)
        .child(
            div()
                .flex()
                .flex_col()
                .gap_5()
                .py_5()
                .px(theme::SPACE_SECTION)
                .children(sync_indicator)
                .children(rows),
        )
}

/// Drops every row of `items` whose `library_id` names a hidden library. A
/// row with no `library_id` is kept (fail open), so an unresolved-library
/// edge case never silently hides a Continue Watching/Next Up item.
fn filter_hidden_libraries(items: Vec<CardRow>, hidden: &HashSet<String>) -> Vec<CardRow> {
    if hidden.is_empty() {
        return items;
    }
    items
        .into_iter()
        .filter(|item| {
            item.library_id
                .as_deref()
                .is_none_or(|lib| !hidden.contains(lib))
        })
        .collect()
}

/// docs/PLUGIN-CHANNELS.md §2.1: the views a "Latest in
/// X" shelf may be built for -- excludes hidden libraries same as
/// `filter_hidden_libraries`, plus every `Channel` view, whose content is
/// never mirrored.
fn views_for_latest_shelves<'a>(
    views: &'a [media_cache::ViewSummary],
    hidden: &HashSet<String>,
) -> Vec<&'a media_cache::ViewSummary> {
    views
        .iter()
        .filter(|v| v.kind != media_cache::ViewKind::Channel && !hidden.contains(&v.id))
        .collect()
}

/// Display-time dedup: drops every row of `items` whose id already appears
/// in `exclude_ids`, so an item never appears in two shelves on screen.
fn dedup_against(exclude_ids: &[String], items: Vec<CardRow>) -> Vec<CardRow> {
    if exclude_ids.is_empty() {
        return items;
    }
    let seen: std::collections::HashSet<&str> = exclude_ids.iter().map(String::as_str).collect();
    items
        .into_iter()
        .filter(|item| !seen.contains(item.id.as_str()))
        .collect()
}

/// Continue Watching/Next Up are the only shelves using `resume_card`'s
/// mixed 16:9-still/2:3-poster treatment; every other shelf keeps
/// `poster_card`. Matched on title since `HomeState::load` is the only
/// place that names these two stable, human-facing strings.
fn is_resume_shelf(title: &str) -> bool {
    title == "Continue Watching" || title == "Next Up"
}

/// Five whole card pitches plus half of a sixth across the shelf's width:
/// target five visible, with the sixth bleeding off the right edge.
const RESUME_CARDS_ACROSS: f32 = 5.5;
/// The widest and narrowest an episode card in these two shelves may get,
/// so a very wide window doesn't turn five cards into five billboards and a
/// very narrow one doesn't shrink the still past legibility.
const RESUME_CARD_MIN_WIDTH: f32 = 180.0;
const RESUME_CARD_MAX_WIDTH: f32 = 320.0;

/// The 16:9 episode-card width that puts `RESUME_CARDS_ACROSS` cards across
/// `shelf_width`; *n* cards need `n*w + (n-1)*gap`, hence `+ CELL_GAP`.
fn resume_card_width(shelf_width: Pixels) -> Pixels {
    let width = (f32::from(shelf_width) + CELL_GAP) / RESUME_CARDS_ACROSS - CELL_GAP;
    px(width.clamp(RESUME_CARD_MIN_WIDTH, RESUME_CARD_MAX_WIDTH))
}

/// The fixed row height these two shelves render every card at, derived
/// from `resume_card_width` so width drives height (not the reverse) --
/// the count that should fit across the shelf is the invariant, not a
/// factor of the poster cell. Movie cards in these rows scale to the same
/// shared height at their own 2:3 ratio.
fn resume_row_height(shelf_width: Pixels) -> Pixels {
    resume_card_width(shelf_width) * 9.0 / 16.0
}

/// The column range (inclusive lo, exclusive hi) near `scroll`'s current
/// viewport, expanded by `EAGER_COLUMN_MARGIN` on each side. Before first
/// paint, `bounds()`/`offset()` read as zero, resolving to a conservative
/// `[0, EAGER_COLUMN_MARGIN + 1)` rather than nothing.
///
/// `pitch`: the caller's estimate of one column's width + gap. Continue
/// Watching/Next Up mix 16:9 and 2:3 card widths in one row, so
/// `render_shelf` passes the wider episode-card pitch -- an overestimate
/// only ever widens this window, never narrows it, so worst case is a few
/// extra eager fetches, never a skipped one.
fn eager_column_range(scroll: &ScrollHandle, pitch: f32) -> (i32, i32) {
    if pitch <= 0.0 {
        return (0, i32::MAX); // degenerate layout constants; never filter
    }
    let offset_x = f32::from(scroll.offset().x);
    let viewport_width = f32::from(scroll.bounds().size.width);

    let first_visible = (-offset_x / pitch).floor().max(0.0) as i32;
    let visible_columns = (viewport_width / pitch).ceil() as i32;
    let lo = (first_visible - EAGER_COLUMN_MARGIN).max(0);
    let hi = first_visible + visible_columns.max(1) + EAGER_COLUMN_MARGIN;
    (lo, hi)
}

#[allow(clippy::too_many_arguments)]
fn render_shelf(
    shelf: &Shelf,
    shelf_ix: usize,
    // The width this shelf lays out across; feeds `resume_card_width`'s
    // "five visible, sixth bleeding off the edge" sizing.
    shelf_width: Pixels,
    focus: ShelfFocus,
    // When false, `focused`/`row_dim` below are forced off so a
    // never-touched Home paints no ring or sibling dimming; focus position
    // still feeds `eager` and Return's target.
    focus_engaged: bool,
    store: &ImageStore,
    root: WeakEntity<Root>,
    scroll: ScrollHandle,
    axis_lock: Rc<RefCell<AxisLock>>,
    cx: &mut Context<Root>,
) -> impl IntoElement {
    let title = shelf.title.clone();
    let cell_w = px(CELL_WIDTH);
    let is_resume = is_resume_shelf(&shelf.title);
    // The row's fixed height and the widest card in it (an Episode's 16:9
    // width), used to size `resume_card` and as the pitch estimate for
    // `eager_column_range`.
    let row_height = resume_row_height(shelf_width);
    let episode_width = row_height * 16.0 / 9.0;
    let pitch = if is_resume {
        f32::from(episode_width) + CELL_GAP
    } else {
        CELL_WIDTH + CELL_GAP
    };
    let (lo, hi) = eager_column_range(&scroll, pitch);
    // Second clone of the handle for the edge fades: `scroll` itself moves
    // into the wheel-handler closure below.
    let scroll_for_fades = scroll.clone();
    let cards = shelf
        .items
        .iter()
        .enumerate()
        .map(|(col, item)| {
            let focused = focus_engaged && focus.shelf == shelf_ix && focus.column == col;
            // Active row: every column dims except the focused one.
            // Shelves compute this off keyboard `ShelfFocus` only, unlike
            // the Library grid's mouse-driven `HighlightPolicy`.
            let row_dim = focus_engaged && focus.shelf == shelf_ix && !focused;
            // Only cells within this shelf's scrolled-into-view viewport,
            // or the focused column (so arrow-key nav past the eager window
            // still loads what it lands on), may issue a new image fetch.
            let eager = focused || {
                let col = col as i32;
                col >= lo && col < hi
            };
            let item_id = item.id.clone();
            let dwell_id = item.id.clone();
            let root_click = root.clone();
            let root_dwell = root.clone();
            if is_resume {
                return resume_card(
                    item,
                    focused,
                    row_dim,
                    row_height,
                    store,
                    root.clone(),
                    cx,
                    eager,
                    move |cx| {
                        let _ =
                            root_click.update(cx, |root, cx| root.open_detail(item_id.clone(), cx));
                    },
                    Some(move |cx: &mut App| {
                        let _ = root_dwell
                            .update(cx, |root, cx| root.prefetch_detail(dwell_id.clone(), cx));
                    }),
                    // Hover focuses the card (scale/shadow/sibling-dim),
                    // same as Library -- see `Root::hover_home_cell`.
                    Some({
                        let root_hover = root.clone();
                        move |cx: &mut App| {
                            let _ = root_hover
                                .update(cx, |root, cx| root.hover_home_cell(shelf_ix, col, cx));
                        }
                    }),
                );
            }
            poster_card(
                item,
                focused,
                row_dim,
                cell_w,
                store,
                root.clone(),
                cx,
                eager,
                move |cx| {
                    let _ = root_click.update(cx, |root, cx| root.open_detail(item_id.clone(), cx));
                },
                Some(move |cx: &mut App| {
                    let _ = root_dwell
                        .update(cx, |root, cx| root.prefetch_detail(dwell_id.clone(), cx));
                }),
                // Hover focuses the card, same treatment as Library -- see
                // `Root::hover_home_cell`.
                Some({
                    let root_hover = root.clone();
                    move |cx: &mut App| {
                        let _ = root_hover
                            .update(cx, |root, cx| root.hover_home_cell(shelf_ix, col, cx));
                    }
                }),
            )
        })
        .collect::<Vec<_>>();

    div()
        .flex()
        .flex_col()
        .gap_2()
        .child(
            // Section heading role: 20px/Semibold reads as a distinct
            // register from body text.
            div()
                .text_color(rgba(theme::TEXT_PRIMARY))
                .text_xl()
                .font_weight(FontWeight::SEMIBOLD)
                .child(SharedString::from(title)),
        )
        .child(
            // Same per-edge fade treatment the season/cast/similar rails
            // got in §8 (shared helper in ui::components).
            crate::ui::components::edge_faded_strip(
                div()
                    .id(SharedString::from(format!("shelf-{shelf_ix}")))
                    .flex()
                    .flex_row()
                    .gap_4()
                    .overflow_x_scroll()
                    .track_scroll(&scroll)
                    // See `scroll_axis.rs`'s module doc comment for the
                    // full mechanism behind the two branches below.
                    .on_scroll_wheel(move |event, window, cx| {
                        let delta = event.delta.pixel_delta(window.line_height());
                        let axis = axis_lock.borrow_mut().on_event(
                            f32::from(delta.x),
                            f32::from(delta.y),
                            Instant::now(),
                        );
                        match axis {
                            Axis::Horizontal => {
                                // gpui's built-in listener already applied
                                // `delta.x` to `scroll`; stop propagation so
                                // the page's vertical listener doesn't also
                                // nudge from the gesture's residual delta.y.
                                cx.stop_propagation();
                            }
                            Axis::Vertical => {
                                // gpui falls back to treating `delta.y` as
                                // horizontal scroll when `delta.x` is exactly
                                // zero (plain wheel, not trackpad); undone
                                // here, left unconsumed so it scrolls the
                                // page instead.
                                let applied_dx = if delta.x != px(0.) { delta.x } else { delta.y };
                                if applied_dx != px(0.) {
                                    let current = scroll.offset();
                                    scroll.set_offset(point(current.x - applied_dx, current.y));
                                }
                            }
                        }
                    })
                    .children(cards),
                &scroll_for_fades,
            ),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- §9: hero_eyebrow_and_meta ("never repeat the title") -----------

    #[test]
    fn movie_hero_has_no_eyebrow_and_meta_is_just_the_year() {
        let mut item = card("m1");
        item.production_year = Some(2019);
        let (eyebrow, meta) = hero_eyebrow_and_meta(&item, Some("Some Series"));
        assert_eq!(eyebrow, None, "a movie must never get a series eyebrow");
        assert_eq!(meta, "2019");
    }

    #[test]
    fn episode_hero_uses_series_name_as_eyebrow_not_the_episode_title() {
        let mut item = card("e1");
        item.name = "The One Where".to_string();
        item.parent_index_number = Some(1);
        item.index_number = Some(3);
        item.runtime_ticks = Some(24 * 60 * 10_000_000);
        item.production_year = Some(2019);
        let (eyebrow, meta) = hero_eyebrow_and_meta(&item, Some("Friends"));
        assert_eq!(eyebrow.as_deref(), Some("Friends"));
        assert_eq!(meta, "S1 E3 · 24m · 2019");
        assert!(
            !meta.contains("The One Where"),
            "the episode title must appear as the Display title only, never \
             duplicated into the metadata line: {meta}"
        );
    }

    #[test]
    fn episode_hero_degrades_gracefully_without_a_known_series_name() {
        let mut item = card("e2");
        item.parent_index_number = Some(2);
        item.index_number = Some(4);
        item.runtime_ticks = None;
        item.production_year = None;
        let (eyebrow, meta) = hero_eyebrow_and_meta(&item, None);
        assert_eq!(eyebrow, None);
        assert_eq!(meta, "S2 E4");
    }

    fn card(id: &str) -> CardRow {
        CardRow {
            id: id.to_string(),
            item_type: "Movie".to_string(),
            name: format!("Movie {id}"),
            primary_tag: Some("tag-1".to_string()),
            blurhash: Some("hash-1".to_string()),
            played: false,
            position_ticks: 0,
            runtime_ticks: Some(100),
            unplayed_count: None,
            production_year: Some(2020),
            index_number: None,
            parent_index_number: None,
            series_id: None,
            series_primary_tag: None,
            parent_backdrop_item_id: None,
            parent_backdrop_tag: None,
            last_played_date: None,
            overview: None,
            premiere_date: None,
            is_virtual: false,
            series_name: None,
            library_id: None,
        }
    }

    fn shelf(title: &str, items: Vec<CardRow>) -> Shelf {
        Shelf {
            title: title.to_string(),
            items,
        }
    }

    // ---- Order-freeze during initial sync ---------------------------------

    #[test]
    fn merge_items_preserving_order_keeps_existing_positions() {
        // Existing tiles keep their order; the merge must not adopt fresh order.
        let current = vec![card("1"), card("2")];
        let fresh = vec![card("3"), card("1"), card("2")];
        let merged = merge_items_preserving_order(&current, &fresh);
        assert_eq!(
            merged.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(),
            vec!["1", "2", "3"],
            "existing tiles keep their position; the new item appends at \
             the end instead of jumping to the front"
        );
    }

    #[test]
    fn merge_items_preserving_order_updates_content_in_place() {
        // Content updates even though position is frozen.
        let current = vec![card("1")];
        let mut updated = card("1");
        updated.played = true;
        let fresh = vec![updated];
        let merged = merge_items_preserving_order(&current, &fresh);
        assert_eq!(merged.len(), 1);
        assert!(
            merged[0].played,
            "content must still update live even while position is frozen"
        );
    }

    #[test]
    fn merge_items_preserving_order_drops_items_fresh_no_longer_has() {
        let current = vec![card("1"), card("2")];
        let fresh = vec![card("2")];
        let merged = merge_items_preserving_order(&current, &fresh);
        assert_eq!(
            merged.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(),
            vec!["2"]
        );
    }

    #[test]
    fn merge_shelves_preserving_order_keeps_shelf_order_and_appends_new_shelves() {
        let current = vec![shelf("Continue Watching", vec![card("1")])];
        // A brand-new shelf mid-sync must append at the end, not sort in.
        let fresh = vec![
            shelf("Continue Watching", vec![card("1")]),
            shelf("Next Up", vec![card("2")]),
        ];
        let merged = merge_shelves_preserving_order(&current, &fresh);
        assert_eq!(
            merged.iter().map(|s| s.title.as_str()).collect::<Vec<_>>(),
            vec!["Continue Watching", "Next Up"]
        );
    }

    /// Repeated re-querying during a sync burst must not move tiles already
    /// on screen, only append new ones.
    #[test]
    fn merge_shelves_preserving_order_is_stable_across_repeated_merges() {
        let mut state_shelves = vec![shelf("Latest in Movies", vec![card("1"), card("2")])];
        // Three sync ticks, each adding one newer item ahead of the rest.
        for new_id in ["3", "4", "5"] {
            let mut fresh_items: Vec<CardRow> = vec![card(new_id)];
            fresh_items.extend(state_shelves[0].items.iter().map(|c| card(c.id.as_str())));
            let fresh = vec![shelf("Latest in Movies", fresh_items)];
            state_shelves = merge_shelves_preserving_order(&state_shelves, &fresh);
        }
        assert_eq!(
            state_shelves[0]
                .items
                .iter()
                .map(|c| c.id.as_str())
                .collect::<Vec<_>>(),
            vec!["1", "2", "3", "4", "5"],
            "original tiles ('1', '2') must stay first, in their original \
             order, through every sync tick -- new items only ever append"
        );
    }

    /// Identical shelf content, even freshly re-queried into new
    /// allocations, must compare equal.
    #[test]
    fn identical_shelves_compare_equal() {
        let a = vec![shelf("Continue Watching", vec![card("1"), card("2")])];
        let b = vec![shelf("Continue Watching", vec![card("1"), card("2")])];
        assert!(shelves_equal(&a, &b));
    }

    #[test]
    fn reordered_items_are_not_equal() {
        let a = vec![shelf("Latest", vec![card("1"), card("2")])];
        let b = vec![shelf("Latest", vec![card("2"), card("1")])];
        assert!(
            !shelves_equal(&a, &b),
            "a real reorder (e.g. a new item landing in date_created order) \
             must still be treated as a change"
        );
    }

    #[test]
    fn a_new_shelf_is_not_equal() {
        let a = vec![shelf("Continue Watching", vec![card("1")])];
        let b = vec![
            shelf("Continue Watching", vec![card("1")]),
            shelf("Latest in Movies", vec![card("2")]),
        ];
        assert!(!shelves_equal(&a, &b));
    }

    #[test]
    fn an_irrelevant_field_change_does_not_count_as_a_visible_difference() {
        // `production_year` never reaches the screen; must not trigger a rebuild.
        let mut changed = card("1");
        changed.production_year = Some(1999);
        let a = vec![shelf("Latest", vec![card("1")])];
        let b = vec![shelf("Latest", vec![changed])];
        assert!(shelves_equal(&a, &b));
    }

    /// Unlike `production_year`, `item_type` changes what a poster cell
    /// paints (`poster_art_source` branches on it), so it must count as a
    /// visible difference.
    #[test]
    fn an_item_type_change_counts_as_a_visible_difference() {
        let mut changed = card("1");
        changed.item_type = "Episode".to_string();
        let a = vec![shelf("Latest", vec![card("1")])];
        let b = vec![shelf("Latest", vec![changed])];
        assert!(!shelves_equal(&a, &b));
    }

    /// A series-poster-fallback tag change must also count --
    /// `poster_art_source` reads it.
    #[test]
    fn a_series_primary_tag_change_counts_as_a_visible_difference() {
        let mut episode = card("1");
        episode.item_type = "Episode".to_string();
        episode.series_id = Some("series-1".to_string());
        let mut with_poster = episode.clone();
        with_poster.series_primary_tag = Some("series-poster".to_string());
        let a = vec![shelf("Continue Watching", vec![episode])];
        let b = vec![shelf("Continue Watching", vec![with_poster])];
        assert!(!shelves_equal(&a, &b));
    }

    /// `resume_card` paints `series_name` on its second line, so a change to
    /// it must count as a visible difference too.
    #[test]
    fn a_series_name_change_counts_as_a_visible_difference_on_a_resume_shelf() {
        let mut episode = card("1");
        episode.item_type = "Episode".to_string();
        episode.series_name = Some("The Wire".to_string());
        let mut renamed = episode.clone();
        renamed.series_name = Some("The Wire (US)".to_string());
        let a = vec![shelf("Continue Watching", vec![episode])];
        let b = vec![shelf("Continue Watching", vec![renamed])];
        assert!(!shelves_equal(&a, &b));
    }

    /// `resume_card`'s "S{s} E{e}" segment reads `index_number`/
    /// `parent_index_number` directly.
    #[test]
    fn an_episode_number_change_counts_as_a_visible_difference_on_a_resume_shelf() {
        let mut episode = card("1");
        episode.item_type = "Episode".to_string();
        episode.parent_index_number = Some(1);
        episode.index_number = Some(3);
        let mut renumbered = episode.clone();
        renumbered.index_number = Some(4);
        let a = vec![shelf("Next Up", vec![episode])];
        let b = vec![shelf("Next Up", vec![renumbered])];
        assert!(!shelves_equal(&a, &b));
    }

    #[test]
    fn a_watch_progress_change_counts_as_a_visible_difference() {
        // Progress bars/unwatched dots render from `played`/
        // `position_ticks`; a change must trigger a rebuild.
        let mut watched = card("1");
        watched.played = true;
        watched.position_ticks = 5000;
        let a = vec![shelf("Continue Watching", vec![card("1")])];
        let b = vec![shelf("Continue Watching", vec![watched])];
        assert!(!shelves_equal(&a, &b));
    }

    #[test]
    fn an_image_tag_change_counts_as_a_visible_difference() {
        let mut new_art = card("1");
        new_art.primary_tag = Some("tag-2".to_string());
        let a = vec![shelf("Latest", vec![card("1")])];
        let b = vec![shelf("Latest", vec![new_art])];
        assert!(!shelves_equal(&a, &b));
    }

    // ---- Continue Watching / Next Up ---------------------------------------

    #[test]
    fn next_up_drops_items_already_in_continue_watching() {
        let resume_ids = vec!["1".to_string(), "2".to_string()];
        let next_up = vec![card("2"), card("3"), card("1"), card("4")];
        let ids: Vec<String> = dedup_against(&resume_ids, next_up)
            .into_iter()
            .map(|c| c.id)
            .collect();
        assert_eq!(ids, vec!["3".to_string(), "4".to_string()]);
    }

    #[test]
    fn dedup_is_a_no_op_when_continue_watching_is_empty() {
        let next_up = vec![card("1"), card("2")];
        assert_eq!(dedup_against(&[], next_up).len(), 2);
    }

    #[test]
    fn a_fully_overlapping_next_up_becomes_empty_rather_than_duplicated() {
        let resume_ids = vec!["1".to_string(), "2".to_string()];
        assert!(dedup_against(&resume_ids, vec![card("1"), card("2")]).is_empty());
    }

    // ---- Feature 2: per-library Home visibility --------------------------

    #[test]
    fn filter_hidden_libraries_is_a_no_op_when_nothing_is_hidden() {
        let mut a = card("1");
        a.library_id = Some("movies".to_string());
        let items = vec![a];
        assert_eq!(
            filter_hidden_libraries(items, &HashSet::new()).len(),
            1,
            "an empty hidden set must never drop anything"
        );
    }

    #[test]
    fn filter_hidden_libraries_drops_items_from_a_hidden_library() {
        let mut visible = card("1");
        visible.library_id = Some("movies".to_string());
        let mut hidden = card("2");
        hidden.library_id = Some("kids".to_string());
        let hidden_set: HashSet<String> = ["kids".to_string()].into_iter().collect();
        let kept = filter_hidden_libraries(vec![visible, hidden], &hidden_set);
        assert_eq!(
            kept.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(),
            vec!["1"],
            "the item stamped with the hidden library id must be dropped, \
             the other kept"
        );
    }

    /// An item with no resolved `library_id` must fail open (kept), not be
    /// treated as belonging to a hidden library.
    #[test]
    fn filter_hidden_libraries_keeps_items_with_no_resolved_library() {
        let unresolved = card("1"); // library_id: None, from the `card()` helper
        let hidden_set: HashSet<String> = ["kids".to_string()].into_iter().collect();
        let kept = filter_hidden_libraries(vec![unresolved], &hidden_set);
        assert_eq!(kept.len(), 1);
    }

    // ---- docs/PLUGIN-CHANNELS.md §2.1: Channel views
    // ---- never feed a "Latest in X" shelf ---------------------------------

    use crate::test_support::view_summary as view;

    /// A `Channel` view's id must be absent from Home's "Latest in X"
    /// view-iteration, even when it isn't in `hidden_libraries` -- excluded
    /// by kind, not by visibility settings.
    #[test]
    fn views_for_latest_shelves_excludes_channel_views() {
        let views = vec![
            view("movies", "Movies", media_cache::ViewKind::Library),
            view("recordings", "Recordings", media_cache::ViewKind::Channel),
        ];
        let eligible = views_for_latest_shelves(&views, &HashSet::new());
        let ids: Vec<&str> = eligible.iter().map(|v| v.id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["movies"],
            "a Channel view's id must never enter the Latest-shelf view-iteration: {ids:?}"
        );
    }

    #[test]
    fn views_for_latest_shelves_still_excludes_hidden_libraries() {
        let views = vec![
            view("movies", "Movies", media_cache::ViewKind::Library),
            view("kids", "Kids", media_cache::ViewKind::Library),
        ];
        let hidden: HashSet<String> = ["kids".to_string()].into_iter().collect();
        let ids: Vec<&str> = views_for_latest_shelves(&views, &hidden)
            .iter()
            .map(|v| v.id.as_str())
            .collect();
        assert_eq!(ids, vec!["movies"]);
    }

    /// §9: "target five visible, sixth bleeding off the right edge."
    #[test]
    fn resume_cards_show_five_and_a_bleeding_sixth_at_the_reference_width() {
        // Reference window (1500px) minus the fixed sidebar, the page
        // gutters and the shelves column's own SPACE_SECTION padding.
        let shelf_width =
            px(crate::grid::LIBRARY_REFERENCE_CONTENT_WIDTH) - theme::SPACE_SECTION * 2.0;
        let pitch = f32::from(resume_card_width(shelf_width)) + CELL_GAP;
        let visible = (f32::from(shelf_width) + CELL_GAP) / pitch;
        assert!(
            (5.4..=5.6).contains(&visible),
            "expected ~5.5 cards across, got {visible}"
        );
    }

    #[test]
    fn resume_cards_are_smaller_than_the_pre_vp3_hero_tiles() {
        let shelf_width =
            px(crate::grid::LIBRARY_REFERENCE_CONTENT_WIDTH) - theme::SPACE_SECTION * 2.0;
        // Old formula: a 16:9 card as wide as `CELL_WIDTH * 1.5 * 0.8` was tall.
        let previous_width = px(CELL_WIDTH) * 1.5 * 0.8 * 16.0 / 9.0;
        assert!(resume_card_width(shelf_width) < previous_width);
    }

    #[test]
    fn resume_cards_keep_their_16_by_9_aspect() {
        for width in [400., 900., 1148., 2400.] {
            let w = f32::from(resume_card_width(px(width)));
            let h = f32::from(resume_row_height(px(width)));
            assert!(
                (w / h - 16.0 / 9.0).abs() < 0.01,
                "aspect drifted at {width}px"
            );
        }
    }
}
