//! Discover's Movies/TV browse grids (spec point 3): the app's grid recipe
//! (a `uniform_list` bucketed into fixed-column rows, same shape
//! `channel_browse.rs`'s live listing already uses for its own "fetch next
//! page on approach" trigger), sort + genre filter chips.

use std::cell::Cell;
use std::rc::Rc;

use gpui::{
    div, prelude::*, rgba, uniform_list, AnyElement, Context, FontWeight, SharedString, WeakEntity,
};

use seerr_api::SeerrGenre;

use crate::channel_browse::should_load_more;
use crate::discover::{discover_card, inline_error, DiscoverBrowseState};
use crate::image_store::ImageStore;
use crate::nav::DiscoverMediaType;
use crate::root::Root;
use crate::theme;
use crate::ui::components::chip_button;

/// Sort presets offered per media kind -- `None` means "let the server pick
/// its own default" (popularity), matching Seerr's own discover default.
/// The "Newest" preset's TMDB sort key differs by kind (movies sort by
/// `release_date`, TV by `first_air_date`), hence the split rather than one
/// shared list.
fn sort_presets(kind: DiscoverMediaType) -> Vec<(&'static str, Option<&'static str>)> {
    match kind {
        DiscoverMediaType::Movie => vec![
            ("Popularity", None),
            ("Newest", Some("release_date.desc")),
            ("Top Rated", Some("vote_average.desc")),
        ],
        DiscoverMediaType::Tv => vec![
            ("Popularity", None),
            ("Newest", Some("first_air_date.desc")),
            ("Top Rated", Some("vote_average.desc")),
        ],
    }
}

fn toolbar(
    state: &DiscoverBrowseState,
    kind: DiscoverMediaType,
    root: WeakEntity<Root>,
) -> impl IntoElement {
    let sort_chips = sort_presets(kind)
        .into_iter()
        .map(|(label, sort_by)| {
            let active = state.sort_by.as_deref() == sort_by;
            let root = root.clone();
            let sort_by = sort_by.map(str::to_string);
            chip_button(
                SharedString::from(format!("discover-sort-{label}")),
                label,
                active,
            )
            .on_click(move |_event, _window, cx| {
                let sort_by = sort_by.clone();
                let _ = root.update(cx, |root, cx| root.set_discover_sort(sort_by, cx));
            })
        })
        .collect::<Vec<_>>();

    let genre_all = {
        let active = state.genre_id.is_none();
        let root = root.clone();
        chip_button("discover-genre-all", "All Genres", active).on_click(
            move |_event, _window, cx| {
                let _ = root.update(cx, |root, cx| root.set_discover_genre(None, cx));
            },
        )
    };
    let genre_chips = state
        .genres
        .iter()
        .map(|genre: &SeerrGenre| {
            let active = state.genre_id == Some(genre.id);
            let root = root.clone();
            let id = genre.id;
            chip_button(
                SharedString::from(format!("discover-genre-{id}")),
                genre.name.clone(),
                active,
            )
            .on_click(move |_event, _window, cx| {
                let _ = root.update(cx, |root, cx| root.set_discover_genre(Some(id), cx));
            })
        })
        .collect::<Vec<_>>();

    div()
        .flex()
        .flex_col()
        .gap_2()
        .child(
            div()
                .flex()
                .flex_row()
                .flex_wrap()
                .gap_2()
                .children(sort_chips),
        )
        .child(
            div()
                .id("discover-genre-row")
                .overflow_x_scroll()
                .flex()
                .flex_row()
                .gap_2()
                .child(genre_all)
                .children(genre_chips),
        )
}

fn grid(
    state: &DiscoverBrowseState,
    columns: usize,
    store: &ImageStore,
    root: WeakEntity<Root>,
) -> AnyElement {
    let columns = columns.max(1);
    let cards = Rc::new(state.cards.clone());
    let row_count = cards.len().div_ceil(columns).max(1);
    let loading_more = state.loading_more;
    let exhausted = state.exhausted();
    let last_seen_end: Rc<Cell<usize>> = Rc::new(Cell::new(usize::MAX));
    let load_more_root = root.clone();
    let loaded_rows = cards.len().div_ceil(columns);

    // `uniform_list`'s row closure is `'static` (gpui calls it on later
    // render passes, well past this fn's own return) -- `store` arrives
    // here as a borrow, so an owned clone (cheap: `ImageStore` is
    // `Rc`-backed) has to cross into it, exactly like `root`/`cards` below.
    let store = store.clone();

    let list = uniform_list("discover-browse-grid", row_count, {
        let cards = cards.clone();
        let root = root.clone();
        let store = store.clone();
        move |range, _window, cx| {
            let end = range.end;
            if last_seen_end.get() != end {
                last_seen_end.set(end);
                if should_load_more(end, loaded_rows, exhausted, loading_more) {
                    let _ = load_more_root
                        .clone()
                        .update(cx, |root, cx| root.load_more_discover_browse(cx));
                }
            }
            range
                .map(|row_ix| {
                    let row_cards = cards
                        .iter()
                        .skip(row_ix * columns)
                        .take(columns)
                        .cloned()
                        .collect::<Vec<_>>();
                    let cells = row_cards
                        .iter()
                        .map(|card| {
                            let media_type: DiscoverMediaType = card.media_type.into();
                            let tmdb_id = card.tmdb_id;
                            let open_root = root.clone();
                            discover_card(card, &store, root.clone(), cx, move |cx| {
                                let _ = open_root.update(cx, |root, cx| {
                                    root.open_discover_detail(media_type, tmdb_id, cx)
                                });
                            })
                        })
                        .collect::<Vec<_>>();
                    div()
                        .flex()
                        .flex_row()
                        .gap(super::CARD_GAP)
                        .pb(super::CARD_GAP)
                        .children(cells)
                        .into_any_element()
                })
                .collect()
        }
    });

    list.track_scroll(state.scroll.clone())
        .w_full()
        .h_full()
        .into_any_element()
}

pub(crate) fn render(
    state: &DiscoverBrowseState,
    kind: DiscoverMediaType,
    columns: usize,
    store: &ImageStore,
    root: WeakEntity<Root>,
    _cx: &mut Context<Root>,
) -> impl IntoElement {
    let title = match kind {
        DiscoverMediaType::Movie => "Movies",
        DiscoverMediaType::Tv => "TV",
    };

    let body: AnyElement = if let Some(error) = &state.error {
        if state.cards.is_empty() {
            inline_error(error.clone()).into_any_element()
        } else {
            grid(state, columns, store, root.clone())
        }
    } else if state.loading && state.cards.is_empty() {
        inline_error("Loading...").into_any_element()
    } else if state.cards.is_empty() {
        inline_error("Nothing to show yet.").into_any_element()
    } else {
        grid(state, columns, store, root.clone())
    };

    div()
        .id("discover-browse")
        .size_full()
        .flex()
        .flex_col()
        .gap(theme::SPACE_DEFAULT)
        .bg(gpui::rgb(theme::SURFACE_BASE))
        .p(theme::SPACE_SECTION)
        .child(
            div()
                .text_size(theme::TEXT_TITLE)
                .font_weight(FontWeight::BOLD)
                .text_color(rgba(theme::TEXT_PRIMARY))
                .child(title),
        )
        .child(toolbar(state, kind, root))
        .child(div().flex_1().min_h_0().child(body))
}
