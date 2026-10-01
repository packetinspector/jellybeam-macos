//! Seerr search (spec point 4): field-atop-results, matching the main
//! `search.rs` overlay's own layout language (field on top, results below)
//! without touching that mirror-backed overlay at all -- this is Discover's
//! own screen, backed by `SeerrSession::search` instead of `Mirror::search`.

use gpui::{div, prelude::*, px, rgb, rgba, AnyElement, Context, FontWeight, WeakEntity};

use crate::discover::{discover_card, inline_error, DiscoverSearchState};
use crate::image_store::ImageStore;
use crate::nav::DiscoverMediaType;
use crate::root::Root;
use crate::theme;

pub(crate) fn render(
    state: &DiscoverSearchState,
    store: &ImageStore,
    root: WeakEntity<Root>,
    cx: &mut Context<Root>,
) -> impl IntoElement {
    let results: AnyElement = if state.loading {
        inline_error("Searching...").into_any_element()
    } else if let Some(error) = &state.error {
        inline_error(error.clone()).into_any_element()
    } else if state.searched && state.cards.is_empty() {
        // DESIGN-GUIDE.md §A.7's search-no-results pose -- `inline_error`
        // stays plain text for the loading/error branches above.
        crate::ui::components::empty_state_mascot(
            "brand/jellybeam/jb_mascot_searching.png",
            "No results.",
        )
        .into_any_element()
    } else {
        let cards = state
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
            .id("discover-search-results")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .flex()
            .flex_row()
            .flex_wrap()
            .gap(super::CARD_GAP)
            .children(cards)
            .into_any_element()
    };

    div()
        .id("discover-search")
        .size_full()
        .flex()
        .flex_col()
        .gap(theme::SPACE_DEFAULT)
        .bg(rgb(theme::SURFACE_BASE))
        .p(theme::SPACE_SECTION)
        .child(
            div()
                .text_size(theme::TEXT_TITLE)
                .font_weight(FontWeight::BOLD)
                .text_color(rgba(theme::TEXT_PRIMARY))
                .child("Search"),
        )
        .child(
            div()
                .w(px(480.))
                .px_4()
                .py_3()
                .rounded_lg()
                .bg(rgb(theme::SURFACE_RAISED))
                .border_1()
                .border_color(rgb(theme::SURFACE_HAIRLINE))
                .child(state.field.clone()),
        )
        .child(results)
}
