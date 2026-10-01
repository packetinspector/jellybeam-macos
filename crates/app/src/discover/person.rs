//! Person page (spec point 7): header + credits grid.

use gpui::{div, prelude::*, px, rgb, rgba, AnyElement, Context, FontWeight, WeakEntity};

use crate::discover::{discover_art, discover_card, inline_error, DiscoverPersonState};
use crate::image_store::ImageStore;
use crate::nav::DiscoverMediaType;
use crate::root::Root;
use crate::theme;

const PROFILE_WIDTH: gpui::Pixels = px(120.);

pub(crate) fn render(
    state: &DiscoverPersonState,
    store: &ImageStore,
    root: WeakEntity<Root>,
    cx: &mut Context<Root>,
) -> impl IntoElement {
    let body: AnyElement = if state.loading {
        inline_error("Loading...").into_any_element()
    } else if let Some(error) = &state.error {
        inline_error(error.clone()).into_any_element()
    } else if let Some(credits) = &state.credits {
        let profile = discover_art(credits.profile_url.as_deref(), store, root.clone(), cx);
        let cards = credits
            .credits
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
            .gap(theme::SPACE_LOOSE)
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(theme::SPACE_DEFAULT)
                    .child(
                        div()
                            .w(PROFILE_WIDTH)
                            .h(PROFILE_WIDTH * 1.5)
                            .flex_shrink_0()
                            .rounded(theme::RADIUS_ART)
                            .overflow_hidden()
                            .bg(rgb(theme::SURFACE_RAISED))
                            .child(profile),
                    )
                    .child(
                        div()
                            .text_size(theme::TEXT_TITLE)
                            .font_weight(FontWeight::BOLD)
                            .text_color(rgba(theme::TEXT_PRIMARY))
                            .child(credits.name.clone()),
                    ),
            )
            .child(
                div()
                    .id("discover-person-credits")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .flex()
                    .flex_row()
                    .flex_wrap()
                    .gap(super::CARD_GAP)
                    .children(cards),
            )
            .into_any_element()
    } else {
        inline_error("Nothing to show.").into_any_element()
    };

    div()
        .id("discover-person")
        .size_full()
        .overflow_y_scroll()
        .bg(rgb(theme::SURFACE_BASE))
        .p(theme::SPACE_SECTION)
        .child(body)
}
