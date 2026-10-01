//! Detail overlay/page for a Seerr movie or TV title (spec point 5):
//! poster/backdrop, overview, scores when present, cast row, Similar +
//! Recommended rows, availability/request state line, and the Request /
//! Request 4K / Cancel request / Go to library actions. TV additionally
//! gets a season picker (non-requestable seasons checked+inert) and, when
//! `SeerrRequestOptions` has entries, a profile/root-folder picker inside
//! the request sheet.

use gpui::{
    div, img, linear_color_stop, linear_gradient, prelude::*, px, rgb, rgba, AnyElement, Context,
    FontWeight, ObjectFit, SharedString, WeakEntity,
};

use seerr_api::{SeerrPersonRef, SeerrSeasonStatus};

use crate::discover::{
    self, discover_art, discover_card, inline_error, season_row_state, DiscoverDetailBody,
    DiscoverDetailState, PrimaryAction, RequestSheetState,
};
use crate::image_store::ImageStore;
use crate::nav::DiscoverMediaType;
use crate::root::Root;
use crate::theme;
use crate::ui::components::{
    button, chip_button, dialog_panel, dialog_scrim, form_row, ButtonSize, ButtonVariant,
};

const POSTER_WIDTH: gpui::Pixels = px(220.);
const PROFILE_SIZE: gpui::Pixels = px(72.);
const BACKDROP_HEIGHT: gpui::Pixels = px(240.);

/// A plain backdrop band above the header (spec point 5: "poster/backdrop"),
/// deliberately simpler than `backdrop.rs`'s Home/Detail hero stack -- that
/// helper's blurred layer is built by `ImageStore::get_scrim`, which (like
/// `get`) only resolves `(item_id, kind, tag)` triples, not the plain URLs
/// Discover's cards carry (see `discover::discover_art`'s doc comment for
/// the same constraint). A single sharp image plus a bottom fade into
/// `SURFACE_BASE` -- the same two-stop `linear_gradient` idiom
/// `ui::components::edge_fade_overlays` already uses -- reads as a real
/// backdrop without needing that second fetch path. `None` (no backdrop
/// art at all) renders nothing rather than a placeholder tile: unlike a
/// poster slot, an absent backdrop isn't a loading state worth announcing.
fn backdrop_band(
    body: &DiscoverDetailBody,
    store: &ImageStore,
    root: WeakEntity<Root>,
    cx: &mut gpui::App,
) -> Option<AnyElement> {
    let url = body.card().backdrop_url.clone()?;
    let texture = store.get_remote(&url, root, cx)?;
    Some(
        div()
            .relative()
            .w_full()
            .h(BACKDROP_HEIGHT)
            .rounded(theme::RADIUS_PANEL)
            .overflow_hidden()
            .child(img(texture).object_fit(ObjectFit::Cover).size_full())
            .child(div().absolute().inset_0().bg(linear_gradient(
                180.,
                linear_color_stop(rgba(theme::TRANSPARENT), 0.0),
                linear_color_stop(rgb(theme::SURFACE_BASE), 1.0),
            )))
            .into_any_element(),
    )
}

fn format_runtime(minutes: i32) -> String {
    if minutes <= 0 {
        return String::new();
    }
    let hours = minutes / 60;
    let mins = minutes % 60;
    if hours > 0 {
        format!("{hours}h {mins}m")
    } else {
        format!("{mins}m")
    }
}

fn meta_line(state: &DiscoverDetailState, body: &DiscoverDetailBody) -> String {
    let card = body.card();
    let mut parts = Vec::new();
    if let Some(year) = card.year {
        parts.push(year.to_string());
    }
    if let DiscoverDetailBody::Movie(movie) = body {
        if let Some(minutes) = movie.runtime_minutes {
            let runtime = format_runtime(minutes);
            if !runtime.is_empty() {
                parts.push(runtime);
            }
        }
    }
    let _ = state; // media_type only distinguishes the runtime branch above
    let genres = body
        .genres()
        .iter()
        .map(|g| g.name.clone())
        .collect::<Vec<_>>()
        .join(", ");
    if !genres.is_empty() {
        parts.push(genres);
    }
    parts.join("  \u{2022}  ")
}

fn scores_line(body: &DiscoverDetailBody) -> Option<String> {
    let mut parts = Vec::new();
    if let Some(c) = body.critics_score() {
        parts.push(format!("Critics {c}%"));
    }
    if let Some(a) = body.audience_score() {
        parts.push(format!("Audience {a}%"));
    }
    (!parts.is_empty()).then(|| parts.join("   "))
}

/// The one primary-action row (spec point 5). `disabled`-styled per
/// `ui::components::button`'s own convention: a disabled button gets no
/// `on_click` at all rather than one guarded internally, so there is
/// exactly one place ("was the handler even attached?") that decides
/// whether a click can fire.
fn actions_row(
    state: &DiscoverDetailState,
    body: &DiscoverDetailBody,
    root: WeakEntity<Root>,
) -> AnyElement {
    let card = body.card();
    let active_request_id = body.active_request().map(|r| r.request_id);
    let primary = discover::primary_action(
        card.jellyfin_item_id.as_deref(),
        active_request_id,
        body.can_request(),
    );
    let is_tv = matches!(state.media_type, DiscoverMediaType::Tv);
    let seasons_ready = !is_tv || !state.selected_seasons.is_empty();

    match primary {
        PrimaryAction::GoToLibrary => {
            let item_id = card.jellyfin_item_id.clone().unwrap_or_default();
            let root = root.clone();
            button(
                "discover-go-to-library",
                "Go to Library",
                ButtonVariant::Primary,
                ButtonSize::Lg,
                false,
            )
            .on_click(move |_event, _window, cx| {
                let _ = root.update(cx, |root, cx| root.open_detail(item_id.clone(), cx));
            })
            .into_any_element()
        }
        PrimaryAction::Cancel { request_id } => button(
            "discover-cancel-request",
            "Cancel Request",
            ButtonVariant::Secondary,
            ButtonSize::Lg,
            false,
        )
        .on_click(move |_event, _window, cx| {
            let _ = root
                .clone()
                .update(cx, |root, cx| root.cancel_discover_request(request_id, cx));
        })
        .into_any_element(),
        PrimaryAction::Request => {
            let request_btn = {
                let btn = button(
                    "discover-request",
                    "Request",
                    ButtonVariant::Primary,
                    ButtonSize::Lg,
                    !seasons_ready,
                );
                if seasons_ready {
                    let root = root.clone();
                    btn.on_click(move |_event, _window, cx| {
                        let _ = root
                            .clone()
                            .update(cx, |root, cx| root.open_discover_request_sheet(false, cx));
                    })
                    .into_any_element()
                } else {
                    btn.into_any_element()
                }
            };
            let mut row = div().flex().flex_row().gap_2().child(request_btn);
            if body.can_request_4k() {
                let btn4k = {
                    let btn = button(
                        "discover-request-4k",
                        "Request 4K",
                        ButtonVariant::Secondary,
                        ButtonSize::Lg,
                        !seasons_ready,
                    );
                    if seasons_ready {
                        let root = root.clone();
                        btn.on_click(move |_event, _window, cx| {
                            let _ = root
                                .clone()
                                .update(cx, |root, cx| root.open_discover_request_sheet(true, cx));
                        })
                        .into_any_element()
                    } else {
                        btn.into_any_element()
                    }
                };
                row = row.child(btn4k);
            }
            row.into_any_element()
        }
        PrimaryAction::Unavailable => div()
            .text_color(rgba(theme::TEXT_TERTIARY))
            .text_sm()
            .child("Not available to request.")
            .into_any_element(),
    }
}

fn season_chip(season: &SeerrSeasonStatus, selected: bool, root: WeakEntity<Root>) -> AnyElement {
    let row = season_row_state(season, selected);
    let number = season.season_number;
    let chip = chip_button(
        SharedString::from(format!("discover-season-{number}")),
        SharedString::from(season.name.clone()),
        row.checked,
    );
    if row.inert {
        chip.opacity(0.5).into_any_element()
    } else {
        chip.on_click(move |_event, _window, cx| {
            let _ = root
                .clone()
                .update(cx, |root, cx| root.toggle_discover_season(number, cx));
        })
        .into_any_element()
    }
}

fn season_picker(
    state: &DiscoverDetailState,
    seasons: &[SeerrSeasonStatus],
    root: WeakEntity<Root>,
) -> AnyElement {
    let chips = seasons
        .iter()
        .map(|season| {
            let selected = state.selected_seasons.contains(&season.season_number);
            season_chip(season, selected, root.clone())
        })
        .collect::<Vec<_>>();
    div()
        .flex()
        .flex_col()
        .gap_1()
        .child(
            div()
                .text_size(theme::TEXT_METADATA)
                .text_color(rgba(theme::TEXT_TERTIARY))
                .child("Seasons"),
        )
        .child(div().flex().flex_row().flex_wrap().gap_2().children(chips))
        .into_any_element()
}

fn cast_person(
    person: &SeerrPersonRef,
    store: &ImageStore,
    root: WeakEntity<Root>,
    cx: &mut gpui::App,
) -> AnyElement {
    let art = discover_art(person.profile_url.as_deref(), store, root.clone(), cx);
    let person_id = person.person_id;
    let open_root = root.clone();
    div()
        .id(SharedString::from(format!("discover-cast-{person_id}")))
        .flex()
        .flex_col()
        .items_center()
        .gap_1()
        .w(px(84.))
        .flex_shrink_0()
        .cursor_pointer()
        .child(
            div()
                .w(PROFILE_SIZE)
                .h(PROFILE_SIZE)
                .rounded_full()
                .overflow_hidden()
                .bg(rgb(theme::SURFACE_RAISED))
                .child(art),
        )
        .child(
            div()
                .text_size(theme::TEXT_METADATA)
                .text_color(rgba(theme::TEXT_SECONDARY))
                .text_center()
                .w_full()
                .truncate()
                .child(SharedString::from(person.name.clone())),
        )
        .on_click(move |_event, _window, cx| {
            let _ = open_root.update(cx, |root, cx| root.open_discover_person(person_id, cx));
        })
        .into_any_element()
}

fn rail(
    title: &'static str,
    cards: &[seerr_api::SeerrCard],
    store: &ImageStore,
    root: WeakEntity<Root>,
    cx: &mut Context<Root>,
) -> Option<AnyElement> {
    if cards.is_empty() {
        return None;
    }
    let items = cards
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
    Some(
        div()
            .flex()
            .flex_col()
            .gap_2()
            .child(
                div()
                    .text_xl()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(rgba(theme::TEXT_PRIMARY))
                    .child(title),
            )
            .child(
                div()
                    .id(SharedString::from(format!("discover-rail-{title}")))
                    .overflow_x_scroll()
                    .flex()
                    .flex_row()
                    .gap(super::CARD_GAP)
                    .pb_2()
                    .children(items),
            )
            .into_any_element(),
    )
}

fn render_body(
    state: &DiscoverDetailState,
    body: &DiscoverDetailBody,
    store: &ImageStore,
    root: WeakEntity<Root>,
    cx: &mut Context<Root>,
) -> AnyElement {
    let card = body.card();
    let backdrop = backdrop_band(body, store, root.clone(), cx);
    let poster = discover_art(card.poster_url.as_deref(), store, root.clone(), cx);
    let scores = scores_line(body);
    let is_tv = matches!(state.media_type, DiscoverMediaType::Tv);
    let seasons = body.seasons().to_vec();

    let cast = body
        .cast()
        .iter()
        .map(|p| cast_person(p, store, root.clone(), cx))
        .collect::<Vec<_>>();

    let header = div()
        .flex()
        .flex_row()
        .gap(theme::SPACE_LOOSE)
        .child(
            div()
                .w(POSTER_WIDTH)
                .h(POSTER_WIDTH * 1.5)
                .flex_shrink_0()
                .rounded(theme::RADIUS_ART)
                .overflow_hidden()
                .bg(rgb(theme::SURFACE_RAISED))
                .child(poster),
        )
        .child(
            div()
                .flex()
                .flex_col()
                .gap_2()
                .flex_1()
                .min_w_0()
                .child(
                    div()
                        .text_size(theme::TEXT_DISPLAY)
                        .line_height(theme::TEXT_DISPLAY_LINE_HEIGHT)
                        .font_weight(FontWeight::BOLD)
                        .text_color(rgba(theme::TEXT_PRIMARY))
                        .child(card.title.clone()),
                )
                .child(
                    div()
                        .text_size(theme::TEXT_BODY)
                        .text_color(rgba(theme::TEXT_SECONDARY))
                        .child(meta_line(state, body)),
                )
                .children(scores.map(|s| {
                    div()
                        .text_size(theme::TEXT_METADATA)
                        .text_color(rgba(theme::TEXT_TERTIARY))
                        .child(s)
                }))
                .children(card.overview.clone().map(|overview| {
                    div()
                        .max_w(px(720.))
                        .text_size(theme::TEXT_BODY)
                        .text_color(rgba(theme::TEXT_SECONDARY))
                        .child(overview)
                }))
                .when(is_tv, |d| {
                    d.child(season_picker(state, &seasons, root.clone()))
                })
                .child(actions_row(state, body, root.clone())),
        );

    let cast_row = (!cast.is_empty()).then(|| {
        div()
            .flex()
            .flex_col()
            .gap_2()
            .child(
                div()
                    .text_xl()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(rgba(theme::TEXT_PRIMARY))
                    .child("Cast"),
            )
            .child(
                div()
                    .id("discover-cast-row")
                    .overflow_x_scroll()
                    .flex()
                    .flex_row()
                    .gap(theme::SPACE_DEFAULT)
                    .pb_2()
                    .children(cast),
            )
    });

    let similar_row = rail("Similar", body.similar(), store, root.clone(), cx);
    let recommended_row = rail(
        "Recommended",
        body.recommendations(),
        store,
        root.clone(),
        cx,
    );

    div()
        .flex()
        .flex_col()
        .gap(theme::SPACE_LOOSE)
        .children(backdrop)
        .child(header)
        .children(cast_row)
        .children(similar_row)
        .children(recommended_row)
        .into_any_element()
}

fn option_chip_row<T: Copy + PartialEq + 'static>(
    id_prefix: &'static str,
    options: &[(T, String)],
    selected: Option<T>,
    on_pick: impl Fn(T, &mut gpui::App) + 'static,
) -> AnyElement {
    let on_pick = std::rc::Rc::new(on_pick);
    let chips = options
        .iter()
        .enumerate()
        .map(|(ix, (value, label))| {
            let active = selected == Some(*value);
            let value = *value;
            let on_pick = on_pick.clone();
            chip_button(
                SharedString::from(format!("{id_prefix}-{ix}")),
                SharedString::from(label.clone()),
                active,
            )
            .on_click(move |_event, _window, cx| on_pick(value, cx))
        })
        .collect::<Vec<_>>();
    div()
        .flex()
        .flex_row()
        .flex_wrap()
        .gap_2()
        .children(chips)
        .into_any_element()
}

fn render_request_sheet(
    state: &DiscoverDetailState,
    sheet: &RequestSheetState,
    root: WeakEntity<Root>,
) -> AnyElement {
    let title = if sheet.is_4k { "Request 4K" } else { "Request" };
    let _ = state;

    let body: AnyElement = if sheet.options_loading {
        inline_error("Loading request options...").into_any_element()
    } else {
        let mut col = div().flex().flex_col().gap(theme::SPACE_DEFAULT);
        if let Some(options) = &sheet.options {
            if !options.servers.is_empty() {
                let server_options = options
                    .servers
                    .iter()
                    .map(|s| (s.server_id, s.name.clone()))
                    .collect::<Vec<_>>();
                let root_for_server = root.clone();
                col = col.child(form_row(
                    "Server",
                    option_chip_row(
                        "discover-server",
                        &server_options,
                        sheet.selected_server_id,
                        move |id, cx| {
                            let _ = root_for_server
                                .clone()
                                .update(cx, |root, cx| root.set_discover_request_server(id, cx));
                        },
                    ),
                ));

                if let Some(server) = options
                    .servers
                    .iter()
                    .find(|s| Some(s.server_id) == sheet.selected_server_id)
                {
                    if !server.profiles.is_empty() {
                        let profile_options = server
                            .profiles
                            .iter()
                            .map(|p| (p.id, p.name.clone()))
                            .collect::<Vec<_>>();
                        let root_for_profile = root.clone();
                        col = col.child(form_row(
                            "Quality Profile",
                            option_chip_row(
                                "discover-profile",
                                &profile_options,
                                sheet.selected_profile_id,
                                move |id, cx| {
                                    let _ = root_for_profile.clone().update(cx, |root, cx| {
                                        root.set_discover_request_profile(id, cx)
                                    });
                                },
                            ),
                        ));
                    }
                    if !server.root_folders.is_empty() {
                        let folder_options = server
                            .root_folders
                            .iter()
                            .enumerate()
                            .map(|(ix, f)| (ix, f.path.clone()))
                            .collect::<Vec<_>>();
                        let selected_ix = sheet.selected_root_folder.as_ref().and_then(|path| {
                            server.root_folders.iter().position(|f| &f.path == path)
                        });
                        let root_for_folder = root.clone();
                        let paths = server
                            .root_folders
                            .iter()
                            .map(|f| f.path.clone())
                            .collect::<Vec<_>>();
                        col = col.child(form_row(
                            "Folder",
                            option_chip_row(
                                "discover-folder",
                                &folder_options,
                                selected_ix,
                                move |ix, cx| {
                                    if let Some(path) = paths.get(ix).cloned() {
                                        let _ = root_for_folder.clone().update(cx, |root, cx| {
                                            root.set_discover_request_root_folder(path, cx)
                                        });
                                    }
                                },
                            ),
                        ));
                    }
                }
            }
        }
        col.into_any_element()
    };

    let submit_disabled = sheet.submitting || sheet.options_loading;
    let submit_btn = {
        let btn = button(
            "discover-request-confirm",
            if sheet.submitting {
                "Requesting..."
            } else {
                "Confirm Request"
            },
            ButtonVariant::Primary,
            ButtonSize::Md,
            submit_disabled,
        );
        if submit_disabled {
            btn.into_any_element()
        } else {
            let root = root.clone();
            btn.on_click(move |_event, _window, cx| {
                let _ = root
                    .clone()
                    .update(cx, |root, cx| root.submit_discover_request(cx));
            })
            .into_any_element()
        }
    };
    let cancel_root = root.clone();
    let cancel_btn = button(
        "discover-request-cancel-sheet",
        "Cancel",
        ButtonVariant::Secondary,
        ButtonSize::Md,
        false,
    )
    .on_click(move |_event, _window, cx| {
        let _ = cancel_root
            .clone()
            .update(cx, |root, cx| root.close_discover_request_sheet(cx));
    });

    let dismiss_root = root.clone();
    dialog_scrim(
        "discover-request-scrim",
        Some(move |cx: &mut gpui::App| {
            let _ = dismiss_root
                .clone()
                .update(cx, |root, cx| root.close_discover_request_sheet(cx));
        }),
        dialog_panel(
            "discover-request-panel",
            px(420.),
            div()
                .flex()
                .flex_col()
                .gap(theme::SPACE_DEFAULT)
                .p(theme::SPACE_DEFAULT)
                .child(
                    div()
                        .text_lg()
                        .font_weight(FontWeight::BOLD)
                        .text_color(rgba(theme::TEXT_PRIMARY))
                        .child(title),
                )
                .children(
                    sheet
                        .error
                        .clone()
                        .map(|e| div().text_color(rgb(theme::DANGER)).text_sm().child(e)),
                )
                .child(body)
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .justify_end()
                        .gap_2()
                        .child(cancel_btn)
                        .child(submit_btn),
                ),
        ),
    )
    .into_any_element()
}

pub(crate) fn render(
    state: &DiscoverDetailState,
    store: &ImageStore,
    root: WeakEntity<Root>,
    cx: &mut Context<Root>,
) -> impl IntoElement {
    let page: AnyElement = match &state.body {
        Some(body) => render_body(state, body, store, root.clone(), cx),
        None if state.loading => inline_error("Loading...").into_any_element(),
        None => inline_error(
            state
                .error
                .clone()
                .unwrap_or_else(|| "Not found.".to_string()),
        )
        .into_any_element(),
    };

    let sheet = state
        .request_sheet
        .as_ref()
        .map(|sheet| render_request_sheet(state, sheet, root.clone()));

    div()
        .id("discover-detail")
        .relative()
        .size_full()
        .overflow_y_scroll()
        .bg(rgb(theme::SURFACE_BASE))
        .p(theme::SPACE_SECTION)
        .child(page)
        .children(sheet)
}
