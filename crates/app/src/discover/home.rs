//! Discover home: the top action row (Search / My Requests / Movies / TV)
//! plus `SeerrHome`'s shelves (Trending, Movies, TV, Upcoming Movies,
//! Upcoming TV), styled with this app's own `home.rs::render_shelf`
//! visual language.

use gpui::{div, prelude::*, rgb, rgba, Context, FontWeight, SharedString, WeakEntity};

use crate::discover::{discover_card, inline_error, DiscoverState};
use crate::image_store::ImageStore;
use crate::nav::DiscoverMediaType;
use crate::root::Root;
use crate::theme;

fn top_action_row(root: WeakEntity<Root>) -> impl IntoElement {
    let search_root = root.clone();
    let requests_root = root.clone();
    let movies_root = root.clone();
    let tv_root = root;

    div()
        .flex()
        .flex_row()
        .gap_2()
        .child(super::action_chip(
            SharedString::from("discover-action-search"),
            "icons/search.svg",
            "Search",
            move |cx| {
                let _ = search_root.update(cx, |root, cx| root.open_discover_search(cx));
            },
        ))
        .child(super::action_chip(
            SharedString::from("discover-action-requests"),
            "icons/list.svg",
            "My Requests",
            move |cx| {
                let _ = requests_root.update(cx, |root, cx| root.open_discover_my_requests(cx));
            },
        ))
        .child(super::action_chip(
            SharedString::from("discover-action-movies"),
            "icons/film.svg",
            "Movies",
            move |cx| {
                let _ = movies_root.update(cx, |root, cx| {
                    root.open_discover_browse(DiscoverMediaType::Movie, cx)
                });
            },
        ))
        .child(super::action_chip(
            SharedString::from("discover-action-tv"),
            "icons/tv.svg",
            "TV",
            move |cx| {
                let _ = tv_root.update(cx, |root, cx| {
                    root.open_discover_browse(DiscoverMediaType::Tv, cx)
                });
            },
        ))
}

fn shelf(
    row: &seerr_api::SeerrHomeRow,
    store: &ImageStore,
    root: WeakEntity<Root>,
    cx: &mut Context<Root>,
) -> impl IntoElement {
    let cards = row
        .cards
        .iter()
        .map(|card| {
            let media_type: DiscoverMediaType = card.media_type.into();
            let tmdb_id = card.tmdb_id;
            let open_root = root.clone();
            discover_card(card, store, root.clone(), cx, move |cx| {
                let _ = open_root.update(cx, |root, cx| {
                    root.open_discover_detail(media_type, tmdb_id, cx)
                });
            })
        })
        .collect::<Vec<_>>();

    div()
        .flex()
        .flex_col()
        .gap_2()
        .child(
            div()
                .text_color(rgba(theme::TEXT_PRIMARY))
                .text_xl()
                .font_weight(FontWeight::SEMIBOLD)
                .child(SharedString::from(row.title.clone())),
        )
        .child(
            div()
                .id(SharedString::from(format!("discover-shelf-{}", row.id)))
                .overflow_x_scroll()
                .flex()
                .flex_row()
                .gap(super::CARD_GAP)
                .pb_2()
                .children(cards),
        )
}

pub(crate) fn render(
    state: &DiscoverState,
    store: &ImageStore,
    root: WeakEntity<Root>,
    cx: &mut Context<Root>,
) -> impl IntoElement {
    let body: gpui::AnyElement = if let Some(error) = &state.open_error {
        inline_error(error.clone()).into_any_element()
    } else if state.opening || (state.home.loading && state.home.rows.is_empty()) {
        inline_error("Loading Discover...").into_any_element()
    } else if let Some(error) = &state.home.error {
        inline_error(error.clone()).into_any_element()
    } else if state.home.rows.is_empty() {
        inline_error("Nothing to show yet.").into_any_element()
    } else {
        div()
            .flex()
            .flex_col()
            .gap(theme::SPACE_COMFORTABLE)
            .children(
                state
                    .home
                    .rows
                    .iter()
                    .map(|row| shelf(row, store, root.clone(), cx)),
            )
            .into_any_element()
    };

    div()
        .id("discover-home")
        .size_full()
        .overflow_y_scroll()
        .flex()
        .flex_col()
        .gap(theme::SPACE_LOOSE)
        .bg(rgb(theme::SURFACE_BASE))
        .p(theme::SPACE_SECTION)
        .child(
            div()
                .text_size(theme::TEXT_TITLE)
                .font_weight(FontWeight::BOLD)
                .text_color(rgba(theme::TEXT_PRIMARY))
                .child("Discover"),
        )
        .child(top_action_row(root))
        .child(body)
}
