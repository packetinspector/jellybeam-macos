//! My Requests (spec point 6): a grid of the account's own requests with
//! status badges; activating one opens the Seerr detail page for that
//! title.

use gpui::{div, prelude::*, rgb, rgba, AnyElement, Context, FontWeight, SharedString, WeakEntity};

use seerr_api::{SeerrMyRequest, SeerrRequestStatus};

use crate::discover::{discover_card, inline_error, DiscoverMyRequestsState};
use crate::image_store::ImageStore;
use crate::nav::DiscoverMediaType;
use crate::root::Root;
use crate::theme;

fn status_text(status: SeerrRequestStatus) -> &'static str {
    match status {
        SeerrRequestStatus::Pending => "PENDING",
        SeerrRequestStatus::Approved => "APPROVED",
        SeerrRequestStatus::Declined => "DECLINED",
    }
}

fn request_cell(
    request: &SeerrMyRequest,
    store: &ImageStore,
    root: WeakEntity<Root>,
    cx: &mut gpui::App,
) -> AnyElement {
    let media_type: DiscoverMediaType = request.card.media_type.into();
    let tmdb_id = request.card.tmdb_id;
    let open_root = root.clone();
    let card = discover_card(&request.card, store, root, cx, move |cx| {
        let _ = open_root.update(cx, |root, cx| {
            root.open_discover_detail(media_type, tmdb_id, cx)
        });
    });
    div()
        .flex()
        .flex_col()
        .gap_1()
        .w(super::CARD_WIDTH)
        .flex_shrink_0()
        .child(card)
        .child(
            div()
                .font_family(theme::FONT_MONO)
                .text_size(theme::TEXT_CAPTION)
                .text_color(if matches!(request.status, SeerrRequestStatus::Declined) {
                    rgba(theme::TEXT_TERTIARY)
                } else {
                    rgba(theme::TEXT_SECONDARY)
                })
                .child(SharedString::from(status_text(request.status))),
        )
        .into_any_element()
}

pub(crate) fn render(
    state: &DiscoverMyRequestsState,
    store: &ImageStore,
    root: WeakEntity<Root>,
    cx: &mut Context<Root>,
) -> impl IntoElement {
    let body: AnyElement = if state.loading {
        inline_error("Loading your requests...").into_any_element()
    } else if let Some(error) = &state.error {
        inline_error(error.clone()).into_any_element()
    } else if state.requests.is_empty() {
        inline_error("No requests yet.").into_any_element()
    } else {
        let cells = state
            .requests
            .iter()
            .map(|request| request_cell(request, store, root.clone(), cx))
            .collect::<Vec<_>>();
        div()
            .id("discover-my-requests-grid")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .flex()
            .flex_row()
            .flex_wrap()
            .gap(super::CARD_GAP)
            .children(cells)
            .into_any_element()
    };

    div()
        .id("discover-my-requests")
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
                .child("My Requests"),
        )
        .child(body)
}
