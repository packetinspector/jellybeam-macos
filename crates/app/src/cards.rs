//! Poster/episode cell rendering shared by the Library grid, Home shelves,
//! Detail's season/episode strips, and the Search overlay's result rows
//! (docs/UX-SPEC.md §5: "same cells, same focus rules" everywhere items appear).
//!
//! A cell is stateless: rebuilt every render pass from a `CardRow` plus
//! whatever focus/hover state the caller has. Image loading is fire-and-
//! forget through `ImageStore::get`, which returns `None` until the decode
//! completes and notifies `Root` to re-render.

use gpui::{
    div, img, prelude::*, px, rgb, rgba, svg, AnimationExt, AnyElement, App, FontWeight,
    MouseButton, ObjectFit, Pixels, SharedString, StyledImage, WeakEntity,
};
use media_cache::{CardRow, ImageKind};

use crate::image_store::{ImageStore, POSTER_WIDTH, THUMB_WIDTH};
use crate::root::Root;
use crate::theme;
use crate::ui::components::{clamped_block, clamped_line, focus_ring};

// ---------------------------------------------------------------------
// Card hover/focus recipe (§3): border-only focus rings are banned for
// cards (kept only for non-card controls, e.g. text_input/detail's season
// tab). On hover/focus, scale/shadow/brightness/sibling-dim/title-reveal
// fire together: enter over `focus_enter_animation()` (180ms), exit over
// `focus_exit_animation()` (240ms).
// ---------------------------------------------------------------------

/// gpui's `with_animation` always plays `delta` 0->1 from a fresh mount, with
/// no reverse/interrupt primitive -- so `focus_art_box`/`fixed_title_block`/
/// `apply_row_dim` each mint a *new* element id whenever the driving boolean
/// flips, to get a correctly-directioned enter/exit transition.
fn focus_transition(active: bool) -> gpui::Animation {
    if active {
        theme::focus_enter_animation()
    } else {
        theme::focus_exit_animation()
    }
}

/// §3's per-card growable artwork box: the OUTER `slot` div stays fixed at
/// `width`x`height` so it never shifts sibling cards, while the INNER art
/// box is `absolute` and grows from 100% to `theme::FOCUS_SCALE` (102%,
/// Brand §5's cap), centered over the slot. `theme::FOCUS_BRIGHTNESS` (1.08x)
/// is approximated as a white overlay (gpui `img()` has no brightness
/// filter). Brand §5 bans a shadow on art -- no shadow property here.
///
/// `configure`: applied to the interactive artwork div right after
/// `on_click`, letting a caller attach extra handlers (e.g. `poster_card`'s
/// hover-dwell) without this fn knowing about them; `episode_card` passes
/// the identity closure.
#[allow(clippy::too_many_arguments)]
fn focus_art_box(
    card_id: SharedString,
    focused: bool,
    width: Pixels,
    height: Pixels,
    // Brand §5: every art box uses `theme::RADIUS_ART` today; the param
    // survives for a hypothetical non-art caller needing a different corner.
    radius: Pixels,
    art: AnyElement,
    badges_el: AnyElement,
    on_click: impl Fn(&mut App) + 'static,
    configure: impl FnOnce(gpui::Stateful<gpui::Div>) -> gpui::Stateful<gpui::Div>,
) -> AnyElement {
    let (start_t, end_t): (f32, f32) = if focused { (0.0, 1.0) } else { (1.0, 0.0) };
    let scale_id = SharedString::from(format!("{card_id}-scale-{focused}"));
    let bright_id = SharedString::from(format!("{card_id}-bright-{focused}"));
    let full_bright_alpha = ((theme::FOCUS_BRIGHTNESS - 1.0) * 255.0)
        .round()
        .clamp(0.0, 255.0) as u8;

    let brightness_overlay = div().absolute().inset_0().with_animation(
        bright_id,
        focus_transition(focused),
        move |el, delta| {
            let t = start_t + (end_t - start_t) * delta;
            el.bg(rgba(theme::tint(
                0xffffff,
                (t * full_bright_alpha as f32) as u8,
            )))
        },
    );

    let interactive = div()
        .id(card_id)
        .absolute()
        .rounded(radius)
        .overflow_hidden()
        .bg(rgb(theme::SURFACE_RAISED))
        .cursor_pointer()
        .child(art)
        .child(brightness_overlay)
        .child(badges_el)
        .on_click(move |_event, _window, cx| on_click(cx));
    let interactive = configure(interactive);

    div()
        .relative()
        .w(width)
        .h(height)
        .child(
            interactive.with_animation(scale_id, focus_transition(focused), move |el, delta| {
                let t = start_t + (end_t - start_t) * delta;
                let scale = 1.0 + (theme::FOCUS_SCALE - 1.0) * t;
                let w = width * scale;
                let h = height * scale;
                let off_x = (w - width) * 0.5;
                let off_y = (h - height) * 0.5;
                el.left(-off_x).top(-off_y).w(w).h(h)
            }),
        )
        // `focus_ring`: the app's one focus/current-item treatment, a 2px
        // PISTACCHIO stroke 2px clear of the artwork. Insets are widened by
        // half the scale-lift overshoot on each axis, or a scaled-up card
        // would eat the gap and touch the stroke.
        .when(focused, |d| {
            let lift_x = px(f32::from(width) * (theme::FOCUS_SCALE - 1.0) * 0.5);
            let lift_y = px(f32::from(height) * (theme::FOCUS_SCALE - 1.0) * 0.5);
            let out_x = px(-f32::from(lift_x + theme::FOCUS_RING_OUTSET));
            let out_y = px(-f32::from(lift_y + theme::FOCUS_RING_OUTSET));
            d.child(
                focus_ring(radius)
                    .left(out_x)
                    .right(out_x)
                    .top(out_y)
                    .bottom(out_y),
            )
        })
        .into_any_element()
}

/// §3: siblings in the row drop to `theme::FOCUS_SIBLING_DIM` (0.5), applied
/// to a whole card (art + title). Animated on the same enter/exit clock as
/// `focus_art_box`, keyed off `dim` with the same fresh-id-per-transition
/// trick.
fn apply_row_dim(id: SharedString, dim: bool, content: AnyElement) -> AnyElement {
    let (start, end) = if dim {
        (1.0, theme::FOCUS_SIBLING_DIM)
    } else {
        (theme::FOCUS_SIBLING_DIM, 1.0)
    };
    div()
        .child(content)
        .with_animation(id, focus_transition(dim), move |el, delta| {
            el.opacity(start + (end - start) * delta)
        })
        .into_any_element()
}

/// docs/DESIGN-PLAYER-NAV.md §2.5: which (item_id, tag) pair a 2:3
/// poster-shaped slot should source its art from, per item type.
#[derive(Debug)]
pub(crate) enum PosterArtSource<'a> {
    /// The item's own `Primary` tag -- correct when it genuinely is a poster
    /// (Movie/Series/BoxSet, or a Season with its own poster).
    Own(&'a str),
    /// Falls back to the series' poster (`SeriesPrimaryImageTag`/`SeriesId`):
    /// an Episode's own `Primary` is a 16:9 still that must never crop into
    /// a 2:3 slot (§2.5), and a Season with no poster of its own does the same.
    SeriesFallback(&'a str, &'a str),
    /// Nothing in the chain has art; the flat placeholder tile renders.
    None,
}

/// §2.5's per-type poster fallback chain: an Episode's own 16:9 `Primary`
/// still must never be cropped into a 2:3 slot, so it always defers to its
/// series' poster. A Season uses its own poster if it has one, else the same
/// series fallback; everything else uses its own `Primary` tag unchanged.
///
/// `pub(crate)`: also the source of truth for what the background poster
/// warmer (`image_warm.rs`) prefetches -- it must resolve the same
/// `(item_id, tag)` a rendered cell will ask for.
pub(crate) fn poster_art_source(item: &CardRow) -> PosterArtSource<'_> {
    if item.item_type != "Episode" {
        if let Some(tag) = item.primary_tag.as_deref() {
            return PosterArtSource::Own(tag);
        }
    }
    match (
        item.series_id.as_deref(),
        item.series_primary_tag.as_deref(),
    ) {
        (Some(id), Some(tag)) => PosterArtSource::SeriesFallback(id, tag),
        _ => PosterArtSource::None,
    }
}

/// Which (item_id, kind, tag) a 16:9 rail cell (`episode_card`, and the
/// Episode Detail page's sibling rail) should source its art from. Fixes
/// "blank dark tiles": an episode with no own `Primary` still used to fall
/// straight to a plain gray placeholder with no fallback chain. Now falls
/// back to the nearest ancestor (season, else series) backdrop -- already
/// native 16:9, so still no stretch/crop of a poster-shaped image.
///
/// `pub(crate)`: also reused by `detail.rs::prefetch_adjacent_episode_hero`
/// to decide what to prefetch for the prev/next sibling episode.
#[derive(Debug)]
pub(crate) enum RailArtSource<'a> {
    /// The episode's own `Primary` still -- already the correct 16:9
    /// aspect, safe to `Cover`-fit with no crop.
    Own(&'a str),
    /// Nearest ancestor (season, else series) backdrop -- also native
    /// 16:9, so still no stretch/crop, just not this exact episode's frame.
    ParentBackdrop(&'a str, &'a str),
    /// Nothing in the chain has art at all; the caller paints
    /// `placeholder_with_title` instead of a blank tile.
    None,
}

pub(crate) fn rail_art_source(item: &CardRow) -> RailArtSource<'_> {
    if let Some(tag) = item.primary_tag.as_deref() {
        return RailArtSource::Own(tag);
    }
    match (
        item.parent_backdrop_item_id.as_deref(),
        item.parent_backdrop_tag.as_deref(),
    ) {
        (Some(id), Some(tag)) => RailArtSource::ParentBackdrop(id, tag),
        _ => RailArtSource::None,
    }
}

/// The "no-art-at-all" placeholder: the item's name centered over the same
/// flat tile `art_element` uses, so a missing-art episode reads as that
/// specific episode rather than an unexplained blank rectangle.
fn placeholder_with_title(name: &str) -> AnyElement {
    let id = SharedString::from(format!("art-placeholder-{name}"));
    div()
        .size_full()
        .flex()
        .items_center()
        .justify_center()
        .bg(rgb(theme::SURFACE_PANEL))
        .p_2()
        .child(
            div()
                .text_color(rgba(theme::TEXT_TERTIARY))
                .text_xs()
                .text_center()
                .child(SharedString::from(name.to_string())),
        )
        .with_animation(id, theme::skeleton_animation(), |el, delta| {
            el.opacity(theme::skeleton_opacity(delta))
        })
        .into_any_element()
}

/// Renders the poster/thumb `img()` element for `item`: decoded texture if
/// ready, else the blurhash placeholder, else a flat fallback tile.
///
/// `eager`: when `false`, only reads the decoded-texture cache and never
/// kicks off a new fetch -- used by `poster_card` for Home shelf cells
/// outside the visible+1-screen window, since Home shelves render
/// unvirtualized and would otherwise fetch every poster on first paint.
#[allow(clippy::too_many_arguments)]
pub(crate) fn art_element(
    item_id: &str,
    tag: Option<&str>,
    blurhash: Option<&str>,
    kind: ImageKind,
    width: u32,
    store: &ImageStore,
    root: WeakEntity<Root>,
    cx: &mut App,
    eager: bool,
) -> AnyElement {
    if let Some(tag) = tag {
        let texture = if eager {
            store.get(item_id, kind, tag, width, root, cx)
        } else {
            store.get_cached(item_id, kind, tag, width)
        };
        if let Some(texture) = texture {
            let just_arrived = store.take_fresh_arrival(item_id, tag);
            let image = img(texture).object_fit(ObjectFit::Cover).size_full();
            return if just_arrived {
                image
                    .with_animation(
                        SharedString::from(format!("fade-{item_id}-{tag}")),
                        gpui::Animation::new(std::time::Duration::from_millis(150)),
                        |img, delta| img.opacity(delta),
                    )
                    .into_any_element()
            } else {
                image.into_any_element()
            };
        }
    }
    if let Some(hash) = blurhash {
        if let Some(placeholder) = store.blurhash(hash) {
            return img(placeholder)
                .object_fit(ObjectFit::Cover)
                .size_full()
                .into_any_element();
        }
    }
    // Part B §13: breathes rather than sit flat -- reads as "loading", not
    // "broken".
    art_placeholder()
}

/// The loading-state placeholder for a card's art, painted when `art_element`
/// has neither a decoded texture nor a blurhash yet.
///
/// Brand §5: flat `SURFACE` fill; only the breathing-opacity pulse
/// (`theme::skeleton_animation`) carries the "loading" read -- no gradient,
/// no shimmer.
fn art_placeholder() -> AnyElement {
    div()
        .size_full()
        .bg(rgb(theme::SURFACE_RAISED))
        .with_animation(
            "art-placeholder-pulse",
            theme::skeleton_animation(),
            |el, delta| el.opacity(theme::skeleton_opacity(delta)),
        )
        .into_any_element()
}

/// §2's filled numeric unwatched badge (series/season/box-set cards): amber
/// fill, near-black text -- never a naked dot.
///
/// Deliberately unpositioned: `badges()` pins it to the artwork's top-right
/// inset, while `library_list.rs`'s row places it inline at the row's right
/// edge -- both read this same helper so the two projections can't disagree.
fn unwatched_count_badge(count: i64) -> AnyElement {
    div()
        .min_w(px(18.))
        .h(px(18.))
        .px(px(4.))
        .rounded_full()
        .bg(rgb(theme::UNWATCHED_BADGE_BG))
        .flex()
        .items_center()
        .justify_center()
        .child(
            div()
                .text_size(theme::TEXT_CAPTION)
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(rgb(theme::UNWATCHED_BADGE_TEXT))
                .child(SharedString::from(count.to_string())),
        )
        .into_any_element()
}

/// The single-item WATCHED indicator (episode/movie cards): a small filled
/// circular checkmark badge, top-right -- a badge rather than a tick, so it
/// never reads as a progress bar. Backed by `theme::ART_SCRIM` (same
/// scrim `episode_hover_play_button` uses) with a `theme::TEXT_PRIMARY`
/// check; unpositioned for the same reason `unwatched_count_badge` is.
///
/// `pub(crate)`: also reused verbatim by `channel_browse.rs`'s recording
/// rows (docs/PLUGIN-CHANNELS.md §2.2), read directly
/// off the live DTO's `UserData.Played`.
pub(crate) fn watched_check_badge() -> AnyElement {
    div()
        .w(px(20.))
        .h(px(20.))
        .rounded_full()
        .bg(rgba(theme::ART_SCRIM))
        .flex()
        .items_center()
        .justify_center()
        .child(
            svg()
                .path("icons/check.svg")
                .w(px(12.))
                .h(px(12.))
                .text_color(rgba(theme::TEXT_PRIMARY)),
        )
        .into_any_element()
}

/// Watch-state badge (top-right: unplayed COUNT, or a watched checkmark) +
/// watched-progress bar (bottom edge), from mirror user data (docs/UX-SPEC.md §5).
/// Progress uses its own `PROGRESS`/`PROGRESS_TRACK` hue, not the accent.
fn badges(item: &CardRow) -> AnyElement {
    let progress = watch_progress(item);

    div()
        .absolute()
        .inset_0()
        .children(
            watch_indicator(item, progress)
                .map(|badge| div().absolute().top_1().right_1().child(badge)),
        )
        .children(progress.map(|p| {
            watch_progress_bar(p)
                .absolute()
                .bottom_0()
                .left_0()
                .right_0()
        }))
        .into_any_element()
}

/// How far into `item` the user got, 0..=1, `None` with no resume position.
/// Shared with `library_list.rs` so both projections derive "partially
/// watched" from one expression, not two that could drift.
pub(crate) fn watch_progress(item: &CardRow) -> Option<f32> {
    item.runtime_ticks
        .filter(|_| item.position_ticks > 0)
        .map(|runtime| (item.position_ticks as f64 / runtime as f64).clamp(0.0, 1.0) as f32)
}

/// §2: progress uses its own `PROGRESS`/`PROGRESS_TRACK` hue, not the accent.
/// Unpositioned -- the caller pins it flush to whatever edge it owns.
pub(crate) fn watch_progress_bar(fraction: f32) -> gpui::Div {
    div()
        .h(theme::PROGRESS_HEIGHT)
        .bg(rgba(theme::PROGRESS_TRACK))
        .child(
            div()
                .h_full()
                .w(gpui::relative(fraction))
                .bg(rgb(theme::PROGRESS)),
        )
}

/// §2: series/season/box-set cells get a filled COUNT badge
/// (`CardRow::unplayed_count`). Everything else gets the watched check badge
/// when `played` and not in progress -- a `Some` `progress` already excludes
/// a resumable Continue Watching item, so watched/resumable stay mutually
/// exclusive with no extra carve-out.
///
/// `None` means nothing to say about watch state -- the caller paints
/// nothing rather than an empty box.
pub(crate) fn watch_indicator(item: &CardRow, progress: Option<f32>) -> Option<AnyElement> {
    let is_countable = matches!(item.item_type.as_str(), "Series" | "Season" | "BoxSet");
    if is_countable {
        return match item.unplayed_count {
            Some(n) if n > 0 => Some(unwatched_count_badge(n)),
            _ => None,
        };
    }
    (item.played && progress.is_none()).then(watched_check_badge)
}

// ---------------------------------------------------------------------
// Fixed-height card title blocks.
// ---------------------------------------------------------------------

/// One text line's reserved height at `text_sm()`/14px -- generous enough
/// not to clip descenders at that size.
const TITLE_LINE_HEIGHT: Pixels = px(20.);
/// One text line's reserved height at `text_xs()`/12px, for the metadata
/// line underneath.
const META_LINE_HEIGHT: Pixels = px(16.);
/// §4's "2 lines' worth", fixed regardless of either line's actual content
/// -- title line + metadata line, no gap between (both are already
/// generously leaded for their size).
const TITLE_BLOCK_HEIGHT: Pixels = px(36.); // TITLE_LINE_HEIGHT + META_LINE_HEIGHT

/// §12: trims a matching wrapping quote pair (straight or curly, single or
/// double) at **display time only** -- the stored value is never mutated,
/// per the "show server-configured names verbatim" rule. An unmatched quote
/// (`'Salem's Lot`, `Aria "`) passes through untouched; nested pairs unwrap
/// one layer per iteration.
pub(crate) fn display_title(name: &str) -> String {
    /// (open, close) for every pair kind this trims. A straight quote is
    /// its own closer; curly quotes are directional.
    const PAIRS: [(char, char); 4] = [
        ('"', '"'),
        ('\u{201c}', '\u{201d}'),
        ('\'', '\''),
        ('\u{2018}', '\u{2019}'),
    ];
    let mut out = name.trim();
    loop {
        let mut chars = out.chars();
        let (Some(first), Some(last)) = (chars.next(), chars.next_back()) else {
            // Zero or one character left -- a lone quote is unmatched.
            break;
        };
        if PAIRS
            .iter()
            .any(|(open, close)| first == *open && last == *close)
        {
            let inner = &out[first.len_utf8()..out.len() - last.len_utf8()];
            if inner.trim().is_empty() {
                break;
            }
            out = inner.trim();
        } else {
            break;
        }
    }
    out.to_string()
}

/// §4: every card in a row gets an IDENTICAL title-block height regardless
/// of content -- a one-line `clamped_line` title with a real ellipsis, plus
/// a metadata line that reserves its fixed height even when `metadata` is
/// `None`.
///
/// §3's title/metadata "reveal" is a `TEXT_PRIMARY`/`TEXT_SECONDARY` (title)
/// and full/dim (metadata) color jump on `focused`, left un-animated: gpui
/// has no `Hsla` lerp to animate a color swap.
fn fixed_title_block(
    width: Pixels,
    title: String,
    metadata: Option<String>,
    focused: bool,
) -> AnyElement {
    // §12: the one place every poster/episode card title passes through.
    let title = display_title(&title);
    let title_color = if focused {
        theme::TEXT_PRIMARY
    } else {
        theme::TEXT_SECONDARY
    };
    let meta_color = if focused {
        theme::TEXT_SECONDARY
    } else {
        theme::TEXT_TERTIARY
    };

    div()
        .w(width)
        .h(TITLE_BLOCK_HEIGHT)
        .flex()
        .flex_col()
        .child(
            clamped_line(title, TITLE_LINE_HEIGHT)
                .w_full()
                // Part B §1: Card title role (14px/Medium), gpui's `text_sm()`.
                .text_sm()
                .font_weight(FontWeight::MEDIUM)
                .text_color(rgba(title_color)),
        )
        .child(
            clamped_line(metadata.unwrap_or_default(), META_LINE_HEIGHT)
                .w_full()
                .text_xs()
                .text_color(rgba(meta_color)),
        )
        .into_any_element()
}

/// One 2:3 poster cell (Library grid, Home shelves, Search results).
///
/// `eager`: whether this cell may kick off a new image fetch (`true`) or
/// only paint from cache (`false`). Callers that virtualize to exactly the
/// visible set (Library grid) always pass `true`; callers rendering beyond
/// what's on screen (Home shelves) pass `false` outside their visible-range
/// heuristic.
#[allow(clippy::too_many_arguments)]
pub(crate) fn poster_card(
    item: &CardRow,
    focused: bool,
    // §3: true when a different cell in this row/shelf owns the highlight
    // (drives `apply_row_dim`); always `false` when `focused` is `true`.
    row_dim: bool,
    width: Pixels,
    store: &ImageStore,
    root: WeakEntity<Root>,
    cx: &mut App,
    eager: bool,
    on_click: impl Fn(&mut App) + 'static,
    on_hover_dwell: Option<impl Fn(&mut App) + 'static + Clone>,
    // Fired on real `on_mouse_move` events, deliberately not `on_hover`
    // state (see `focus_grid::HighlightPolicy`). Callers wire this to
    // `HighlightPolicy::mouse_hover`; `None` for callers not yet wired.
    on_mouse_move: Option<impl Fn(&mut App) + 'static>,
) -> AnyElement {
    let height = width * 1.5;
    // §2.5: a series-fallback tile has no blurhash -- the DTO only carries
    // a blurhash for its own Primary image, not an ancestor's.
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
        root.clone(),
        cx,
        eager,
    );

    let dwell_key = item.id.clone();
    let store_for_hover = store.clone();
    let hover_action = on_hover_dwell.clone();

    let card_id = SharedString::from(format!("card-{}", item.id));
    let art_box = focus_art_box(
        card_id,
        focused,
        width,
        height,
        theme::RADIUS_ART,
        art,
        badges(item),
        on_click,
        move |cell| {
            let mut cell = cell;
            if let Some(on_move) = on_mouse_move {
                cell = cell.on_mouse_move(move |_event, _window, cx| on_move(cx));
            }
            if let Some(on_dwell) = hover_action {
                // Armed from a real `on_mouse_move` event, never gpui's
                // hover *state* -- `on_hover(true)` can fire with the mouse
                // never moving (cursor resting at launch, a card sliding
                // under a stationary cursor), which drove unwanted preload
                // fetches. `dwell_is_armed` caps it to one timer per hover;
                // hover-exit disarms so a pointer that leaves before 350ms
                // never fires.
                let store_for_move = store_for_hover.clone();
                let move_key = dwell_key.clone();
                cell = cell.on_mouse_move(move |_event, _window, cx| {
                    if store_for_move.dwell_is_armed(&move_key) {
                        return;
                    }
                    let epoch = store_for_move.dwell_enter(&move_key);
                    let store = store_for_move.clone();
                    let key = move_key.clone();
                    let on_dwell = on_dwell.clone();
                    cx.spawn(async move |cx| {
                        cx.background_executor()
                            .timer(std::time::Duration::from_millis(350))
                            .await;
                        if store.dwell_is_current(&key, epoch) {
                            let _ = cx.update(|cx| on_dwell(cx));
                        }
                    })
                    .detach();
                });
                cell = cell.on_hover(move |hovering, _window, _cx| {
                    if !*hovering {
                        store_for_hover.dwell_exit(&dwell_key);
                    }
                });
            }
            cell
        },
    );

    let title_block = fixed_title_block(
        width,
        item.name.clone(),
        item.production_year.map(|y| y.to_string()),
        focused,
    );

    let dim_id = SharedString::from(format!("card-{}-dim-{}", item.id, row_dim));
    apply_row_dim(
        dim_id,
        row_dim,
        div()
            .flex()
            .flex_col()
            // Without this, a rail's flex child shrinks to fit
            // instead of overflowing into the scroll area (the edge-fade
            // assumes real overflow); a no-op for exact-width grid cells.
            .flex_shrink_0()
            .gap_1()
            .child(art_box)
            .child(title_block)
            .into_any_element(),
    )
}

/// §5's condensed spec strip over a hovered/focused poster's lower edge:
/// `RESOLUTION │ HDR │ AUDIO`, in the same plate language as `ui::spec_strip`.
///
/// `art_height` bottom-aligns the strip to the poster box, not the whole
/// cell (which also carries the title block). Painted over a black ramp,
/// not a flat fill, so mono at 60% never sits on bare artwork (§4's
/// local-scrim rule at card scale).
pub(crate) fn poster_spec_overlay(
    art_height: Pixels,
    fields: &[crate::ui::spec_strip::SpecField],
) -> AnyElement {
    const STRIP_HEIGHT: Pixels = px(46.);
    div()
        .absolute()
        .left_0()
        .right_0()
        .top(art_height - STRIP_HEIGHT)
        .h(STRIP_HEIGHT)
        .flex()
        .flex_col()
        .justify_end()
        .overflow_hidden()
        .rounded_bl(theme::RADIUS_ART)
        .rounded_br(theme::RADIUS_ART)
        .bg(gpui::linear_gradient(
            180.,
            gpui::linear_color_stop(rgba(theme::TRANSPARENT), 0.0),
            gpui::linear_color_stop(rgba(theme::tint(theme::NOTTE, 0xe0)), 1.0),
        ))
        .child(
            // `spec_strip_condensed`: a wall cell is too narrow for §5's
            // pill chrome, and §6 forbids dropping a value to make one fit.
            crate::ui::spec_strip::spec_strip_condensed(fields)
                .px_1()
                .pb_1()
                .overflow_hidden(),
        )
        .into_any_element()
}

/// `ticks`'s `RunTimeTicks` (100ns units) as `"1h 23m"`/`"42m"` -- shared by
/// `detail.rs`'s header metadata line and `episode_card`'s runtime line
/// below (§2.3: "Missing: runtime -- add next to/under the title").
pub(crate) fn format_runtime(ticks: i64) -> String {
    let total_minutes = ticks / 10_000_000 / 60;
    let h = total_minutes / 60;
    let m = total_minutes % 60;
    if h > 0 {
        format!("{h}h {m}m")
    } else {
        format!("{m}m")
    }
}

/// The reason text shown for a virtual item (`CardRow::is_virtual`, a server
/// placeholder episode with no `MediaSources`): "Airs <date>" if its
/// `PremiereDate` is still in the future, else "Missing". `premiere_date` is
/// the RFC3339 string the mirror already carries, parsed here rather than
/// threading a second pre-parsed field through. Callers gate on `is_virtual`
/// themselves -- this only decides which reason to show.
pub(crate) fn virtual_status_label(premiere_date: Option<&str>) -> String {
    let airs_in_future = premiere_date
        .and_then(|d| chrono::DateTime::parse_from_rfc3339(d).ok())
        .filter(|d| d.with_timezone(&chrono::Utc) > chrono::Utc::now());
    match airs_in_future {
        // Day + month, no year -- matches `details_block`'s "Added <date>"
        // convention minus the year (that one keeps it for old library adds).
        Some(d) => format!("Airs {}", d.format("%b %-d")),
        None => "Missing".to_string(),
    }
}

// ---------------------------------------------------------------------
// Episode card 2-line synopsis.
// ---------------------------------------------------------------------

/// One synopsis line's reserved height at `text_xs()`/12px -- matches
/// `META_LINE_HEIGHT`'s own sizing logic for the same font size.
const SYNOPSIS_LINE_HEIGHT: Pixels = px(16.);
/// The synopsis' own type size -- gpui's `text_xs()`, named here because
/// the measured clamp (`ui::components::clamped_block`) needs the real
/// value, not just the style method.
const SYNOPSIS_TEXT_SIZE: Pixels = px(12.);
/// How many lines of description an episode card shows. The CSS reference
/// is `-webkit-line-clamp: 2`. `clamped_block` reserves
/// `SYNOPSIS_LINE_HEIGHT * SYNOPSIS_LINES` whatever the text does.
const SYNOPSIS_LINES: usize = 2;
/// Every episode card's title-plus-description stack is exactly this tall:
/// a one-line clamped title, a one-line runtime/status line
/// (`TITLE_BLOCK_HEIGHT` covers both), then a two-line clamped description.
/// A fixed budget per zone keeps a row of cards aligned regardless of title
/// length.
const EPISODE_TEXT_BLOCK_HEIGHT: Pixels = px(68.); // TITLE_BLOCK + SYNOPSIS_BLOCK

#[cfg(test)]
mod episode_card_height_tests {
    use super::*;

    /// Pins: the three zones' budgets sum to the reserved block height, or
    /// cards stop lining up.
    #[test]
    fn the_episode_text_block_budget_is_the_sum_of_its_zones() {
        assert_eq!(TITLE_BLOCK_HEIGHT, TITLE_LINE_HEIGHT + META_LINE_HEIGHT);
        let synopsis_block = SYNOPSIS_LINE_HEIGHT * SYNOPSIS_LINES as f32;
        assert_eq!(
            EPISODE_TEXT_BLOCK_HEIGHT,
            TITLE_BLOCK_HEIGHT + synopsis_block
        );
    }

    /// Pins: `detail.rs::EPISODE_ROW_HEIGHT` duplicates this figure as a
    /// literal; if they drift, that spacer silently mis-sizes.
    #[test]
    fn the_detail_row_height_estimate_still_matches_these_budgets() {
        assert_eq!(f32::from(EPISODE_TEXT_BLOCK_HEIGHT), 68.0);
    }
}

/// §6's episode card title block: a one-line title clamp, a one-line runtime
/// row, and a fixed-height, measured two-line synopsis clamp
/// (`ui::components::clamped_block` -- a real `LineWrapper` measurement, not
/// gpui's `.line_clamp()`/`.text_ellipsis()`, which hard-cuts mid-word; see
/// `clamp_text_to_lines` for why). The synopsis block always renders, even
/// with no overview, so every card in a row reserves the same height.
///
/// Colors are fixed regardless of focus/watched state -- title `PANNA`,
/// runtime and description `GRIGIO`, always; watched is the checkmark badge
/// only, current episode is the 2px ring only. That's why this deliberately
/// doesn't share `fixed_title_block`, which keeps the poster cells'
/// focus-driven color jump.
fn episode_title_block(
    width: Pixels,
    title: String,
    metadata: Option<String>,
    overview: Option<String>,
    cx: &App,
) -> AnyElement {
    let title = display_title(&title);
    div()
        .w(width)
        .h(EPISODE_TEXT_BLOCK_HEIGHT)
        .flex()
        .flex_col()
        .child(
            clamped_line(title, TITLE_LINE_HEIGHT)
                .w_full()
                // Part B §1: Card title role (14px/Medium), gpui's `text_sm()`.
                .text_sm()
                .font_weight(FontWeight::MEDIUM)
                .text_color(rgba(theme::TEXT_PRIMARY)),
        )
        .child(
            clamped_line(metadata.unwrap_or_default(), META_LINE_HEIGHT)
                .w_full()
                .text_xs()
                .text_color(rgb(theme::GRIGIO)),
        )
        .child(
            clamped_block(
                overview.as_deref().unwrap_or_default(),
                SYNOPSIS_LINES,
                SYNOPSIS_LINE_HEIGHT,
                gpui::font(theme::FONT_UI),
                SYNOPSIS_TEXT_SIZE,
                width,
                cx,
            )
            .text_color(rgb(theme::GRIGIO)),
        )
        .into_any_element()
}

/// The one-click-play affordance on an `episode_card`'s thumbnail: a small
/// circular button, bottom-left, hidden until the card is hovered
/// (`group_hover` keyed to `group_name`, set via `episode_card`'s
/// `focus_art_box` `configure` hook), calling `on_play` directly rather than
/// navigating. `theme::ART_SCRIM` backs the glyph for legibility over any
/// thumbnail; `theme::ICON_HOVER_FILL` brightens it on hover, matching
/// `player_ui.rs`'s small icon buttons. Stops propagation on mouse down/up
/// so the click never also reaches the card's own navigate-`on_click`.
fn episode_hover_play_button(
    card_id: SharedString,
    group_name: SharedString,
    on_play: impl Fn(&mut App) + 'static,
) -> AnyElement {
    div()
        .id(SharedString::from(format!("{card_id}-play")))
        .absolute()
        .left(px(6.))
        .bottom(px(6.))
        .w(px(26.))
        .h(px(26.))
        .flex()
        .items_center()
        .justify_center()
        .rounded_full()
        .bg(rgba(theme::ART_SCRIM))
        .opacity(0.0)
        .group_hover(group_name, |s| s.opacity(1.0))
        .hover(|s| s.bg(rgba(theme::ICON_HOVER_FILL)))
        .cursor_pointer()
        .child(
            svg()
                .path("icons/play.svg")
                .w(px(11.))
                .h(px(11.))
                .text_color(rgba(theme::TEXT_PRIMARY)),
        )
        .on_mouse_down(MouseButton::Left, |_event, _window, cx| {
            cx.stop_propagation()
        })
        .on_mouse_up(MouseButton::Left, |_event, _window, cx| {
            cx.stop_propagation()
        })
        .on_click(move |_event, _window, cx| on_play(cx))
        .into_any_element()
}

/// A 16:9 episode cell (Detail page's season → episode list, docs/UX-SPEC.md §5).
/// `item.index_number` drives the `"E{n} · {name}"` title prefix;
/// `item.runtime_ticks` adds a runtime line (§2.3); `item.overview` a 2-line
/// synopsis clamp (§6).
///
/// `on_click` navigates to the episode's Detail page (never plays directly),
/// except on the Episode Detail page's own sibling rail, which swaps in
/// place instead. `on_play` is the explicit one-click-play affordance: a
/// hover-revealed play-glyph overlay (`episode_hover_play_button`).
#[allow(clippy::too_many_arguments)]
pub(crate) fn episode_card(
    item: &CardRow,
    focused: bool,
    width: Pixels,
    store: &ImageStore,
    root: WeakEntity<Root>,
    cx: &mut App,
    on_click: impl Fn(&mut App) + 'static,
    on_play: impl Fn(&mut App) + 'static,
) -> AnyElement {
    let height = width * 9.0 / 16.0;
    // Only the `None` branch skips `art_element` for the named placeholder;
    // `Own`/`ParentBackdrop` both still go through it unchanged.
    let art = match rail_art_source(item) {
        RailArtSource::Own(tag) => art_element(
            &item.id,
            Some(tag),
            item.blurhash.as_deref(),
            ImageKind::Primary,
            THUMB_WIDTH,
            store,
            root,
            cx,
            // Always eager: a short, single-season list the user opened
            // deliberately (unlike `poster_card`, not lazy-windowed).
            true,
        ),
        RailArtSource::ParentBackdrop(parent_id, tag) => art_element(
            parent_id,
            Some(tag),
            None,
            ImageKind::Backdrop,
            THUMB_WIDTH,
            store,
            root,
            cx,
            true,
        ),
        RailArtSource::None => placeholder_with_title(&item.name),
    };
    // A virtual episode has nothing playable behind it -- dim the artwork
    // so it reads as unavailable at a glance.
    let art = if item.is_virtual {
        div().size_full().opacity(0.4).child(art).into_any_element()
    } else {
        art
    };

    // §6: "E{n} · Title". §12's quote trim must run on the bare name here --
    // once the prefix is prepended, the quotes no longer wrap the string.
    let episode_name = display_title(&item.name);
    let title = if let Some(n) = item.index_number {
        format!("E{n} · {episode_name}")
    } else {
        episode_name
    };
    // A virtual episode has no runtime -- the metadata line instead shows
    // why it's not playable ("Airs <date>"/"Missing").
    let runtime = if item.is_virtual {
        Some(virtual_status_label(item.premiere_date.as_deref()))
    } else {
        item.runtime_ticks.map(format_runtime)
    };

    let card_id = SharedString::from(format!("ep-{}", item.id));
    // Per-card group name scopes `group_hover` to exactly this card's own
    // hover area, not any other card's.
    let hover_group = SharedString::from(format!("{card_id}-hover"));
    // No one-click-play affordance on a virtual episode -- nothing to play.
    // `on_click` (navigate to Detail) stays wired regardless.
    let play_button = (!item.is_virtual)
        .then(|| episode_hover_play_button(card_id.clone(), hover_group.clone(), on_play));
    let art_box = focus_art_box(
        card_id,
        focused,
        width,
        height,
        // Brand §5: 2px radius for any aspect ratio, matching the 2:3 poster.
        theme::RADIUS_ART,
        art,
        badges(item),
        on_click,
        move |cell| {
            let cell = cell.group(hover_group);
            match play_button {
                Some(play_button) => cell.child(play_button),
                None => cell,
            }
        },
    );

    let title_block = episode_title_block(width, title, runtime, item.overview.clone(), cx);

    // No `apply_row_dim`/focus-driven colors on an episode card: the
    // sibling-dim recipe made a whole non-highlighted row read as disabled.
    // Every card is typographically identical; watched state is the
    // checkmark badge only, current episode is the 2px ring only.
    div()
        .relative()
        .flex()
        .flex_col()
        .gap_1()
        .child(art_box)
        .child(title_block)
        .into_any_element()
}

// ---------------------------------------------------------------------
// Continue Watching / Next Up card redesign (§3): unlike other Home
// shelves, an Episode row's `poster_art_source` falls back to the series
// poster (§2.5), so different episodes of one show rendered as identical
// tiles. `resume_card` instead shows an Episode's own 16:9 still
// (`rail_art_source`) with a 3-line title block (title / series name at 60%
// opacity / "S{s} E{e} · {remaining} left"); a Movie keeps its 2:3 poster
// but the same 3-line block, with an empty series line and "{remaining}
// left" on the third.
// ---------------------------------------------------------------------

/// §3's "S{s} E{e}" segment, matching `home.rs::hero_eyebrow_and_meta`'s
/// format. A missing number is omitted entirely, never rendered as "S? E?".
/// `None` (not `""`) when neither number is known, so `resume_meta_line` can
/// tell "nothing to show" from "show an empty string".
fn season_episode_label(item: &CardRow) -> Option<String> {
    match (item.parent_index_number, item.index_number) {
        (Some(s), Some(e)) => Some(format!("S{s} E{e}")),
        (None, Some(e)) => Some(format!("E{e}")),
        (Some(s), None) => Some(format!("S{s}")),
        (None, None) => None,
    }
}

/// §3's "remaining time" third line: `runtime_ticks - position_ticks`,
/// formatted like `format_runtime` plus " left". A Next Up item
/// (`position_ticks == 0`) just gets its full runtime, no special case.
/// `None` when there's no runtime to compute from.
fn remaining_label(item: &CardRow) -> Option<String> {
    let runtime = item.runtime_ticks?;
    if runtime <= 0 {
        return None;
    }
    let remaining = (runtime - item.position_ticks).max(0);
    Some(format!("{} left", format_runtime(remaining)))
}

/// §3's full third line: `season_episode_label` + `remaining_label` (or
/// `virtual_status_label` for a virtual item), joined with " · " when both
/// are present, else whichever one is; `None` when neither applies.
fn resume_meta_line(item: &CardRow) -> Option<String> {
    let se = season_episode_label(item);
    let time_part = if item.is_virtual {
        Some(virtual_status_label(item.premiere_date.as_deref()))
    } else {
        remaining_label(item)
    };
    match (se, time_part) {
        (Some(se), Some(t)) => Some(format!("{se} · {t}")),
        (Some(se), None) => Some(se),
        (None, Some(t)) => Some(t),
        (None, None) => None,
    }
}

/// §3's fixed 3-line title block height (title + series name + S/E·remaining),
/// reserved regardless of content -- a Movie has no series name but still
/// reserves the same height, so every card in the row bottom-aligns.
const RESUME_TITLE_BLOCK_HEIGHT: Pixels = px(52.); // TITLE_LINE_HEIGHT + META_LINE_HEIGHT + META_LINE_HEIGHT (20 + 16 + 16)

fn resume_title_block(
    width: Pixels,
    title: String,
    series_name: Option<String>,
    meta: Option<String>,
    focused: bool,
) -> AnyElement {
    // §12: same display-time quote trim as `fixed_title_block`, for both
    // lines this block renders.
    let title = display_title(&title);
    let series_name = series_name.map(|s| display_title(&s));
    let title_color = if focused {
        theme::TEXT_PRIMARY
    } else {
        theme::TEXT_SECONDARY
    };
    let meta_color = if focused {
        theme::TEXT_SECONDARY
    } else {
        theme::TEXT_TERTIARY
    };

    div()
        .w(width)
        .h(RESUME_TITLE_BLOCK_HEIGHT)
        .flex()
        .flex_col()
        .child(
            clamped_line(title, TITLE_LINE_HEIGHT)
                .w_full()
                .text_sm()
                .font_weight(FontWeight::MEDIUM)
                .text_color(rgba(title_color)),
        )
        .child(
            clamped_line(series_name.unwrap_or_default(), META_LINE_HEIGHT)
                .w_full()
                .text_xs()
                // `theme::TEXT_SYNOPSIS`'s alpha (0x99/255) is exactly 60%,
                // reused rather than adding a new token.
                .text_color(rgba(theme::TEXT_SYNOPSIS)),
        )
        .child(
            clamped_line(meta.unwrap_or_default(), META_LINE_HEIGHT)
                .w_full()
                .text_xs()
                .text_color(rgba(meta_color)),
        )
        .into_any_element()
}

/// One Continue Watching/Next Up cell (`home.rs` only; other shelves keep
/// `poster_card`). `row_height` is fixed by the caller; a Movie renders at
/// its usual 2:3 size, an Episode at `row_height`'s 16:9-equivalent width --
/// constant row height, differing card widths. `resume_title_block`
/// delivers the common baseline underneath.
#[allow(clippy::too_many_arguments)]
pub(crate) fn resume_card(
    item: &CardRow,
    focused: bool,
    // §3: same `row_dim` contract as `poster_card`.
    row_dim: bool,
    row_height: Pixels,
    store: &ImageStore,
    root: WeakEntity<Root>,
    cx: &mut App,
    eager: bool,
    on_click: impl Fn(&mut App) + 'static,
    on_hover_dwell: Option<impl Fn(&mut App) + 'static + Clone>,
    on_mouse_move: Option<impl Fn(&mut App) + 'static>,
) -> AnyElement {
    let is_episode = item.item_type == "Episode";
    let width = if is_episode {
        row_height * 16.0 / 9.0
    } else {
        row_height / 1.5
    };

    // §3: an Episode uses its own 16:9 still via `rail_art_source` (same
    // fallback chain as `episode_card`'s rail cells); a Movie keeps the
    // ordinary `poster_art_source` 2:3 chain.
    let art = if is_episode {
        match rail_art_source(item) {
            RailArtSource::Own(tag) => art_element(
                &item.id,
                Some(tag),
                item.blurhash.as_deref(),
                ImageKind::Primary,
                THUMB_WIDTH,
                store,
                root.clone(),
                cx,
                eager,
            ),
            RailArtSource::ParentBackdrop(parent_id, tag) => art_element(
                parent_id,
                Some(tag),
                None,
                ImageKind::Backdrop,
                THUMB_WIDTH,
                store,
                root.clone(),
                cx,
                eager,
            ),
            RailArtSource::None => placeholder_with_title(&item.name),
        }
    } else {
        let (art_item_id, art_tag, art_blurhash) = match poster_art_source(item) {
            PosterArtSource::Own(tag) => (item.id.as_str(), Some(tag), item.blurhash.as_deref()),
            PosterArtSource::SeriesFallback(series_id, tag) => (series_id, Some(tag), None),
            PosterArtSource::None => (item.id.as_str(), None, item.blurhash.as_deref()),
        };
        art_element(
            art_item_id,
            art_tag,
            art_blurhash,
            ImageKind::Primary,
            POSTER_WIDTH,
            store,
            root.clone(),
            cx,
            eager,
        )
    };
    // Same as `episode_card`: a virtual item has nothing playable behind
    // it -- dim its artwork.
    let art = if item.is_virtual {
        div().size_full().opacity(0.4).child(art).into_any_element()
    } else {
        art
    };

    // Brand §5: all artwork is 2px radius now, so both card shapes agree.
    let radius = theme::RADIUS_ART;

    let dwell_key = item.id.clone();
    let store_for_hover = store.clone();
    let hover_action = on_hover_dwell.clone();

    let card_id = SharedString::from(format!("resume-{}", item.id));
    let art_box = focus_art_box(
        card_id,
        focused,
        width,
        row_height,
        radius,
        art,
        badges(item),
        on_click,
        move |cell| {
            let mut cell = cell;
            if let Some(on_move) = on_mouse_move {
                cell = cell.on_mouse_move(move |_event, _window, cx| on_move(cx));
            }
            if let Some(on_dwell) = hover_action {
                // Armed from a real `on_mouse_move` event, never gpui's
                // hover *state* -- `on_hover(true)` can fire with the mouse
                // never moving (cursor resting at launch, a card sliding
                // under a stationary cursor), which drove unwanted preload
                // fetches. `dwell_is_armed` caps it to one timer per hover;
                // hover-exit disarms so a pointer that leaves before 350ms
                // never fires.
                let store_for_move = store_for_hover.clone();
                let move_key = dwell_key.clone();
                cell = cell.on_mouse_move(move |_event, _window, cx| {
                    if store_for_move.dwell_is_armed(&move_key) {
                        return;
                    }
                    let epoch = store_for_move.dwell_enter(&move_key);
                    let store = store_for_move.clone();
                    let key = move_key.clone();
                    let on_dwell = on_dwell.clone();
                    cx.spawn(async move |cx| {
                        cx.background_executor()
                            .timer(std::time::Duration::from_millis(350))
                            .await;
                        if store.dwell_is_current(&key, epoch) {
                            let _ = cx.update(|cx| on_dwell(cx));
                        }
                    })
                    .detach();
                });
                cell = cell.on_hover(move |hovering, _window, _cx| {
                    if !*hovering {
                        store_for_hover.dwell_exit(&dwell_key);
                    }
                });
            }
            cell
        },
    );

    // §3's line 2: only an Episode has a series name (`CardRow::series_name`);
    // a Movie's line stays empty but still reserves its height.
    let series_name = is_episode.then(|| item.series_name.clone()).flatten();
    let title_block = resume_title_block(
        width,
        item.name.clone(),
        series_name,
        resume_meta_line(item),
        focused,
    );

    let dim_id = SharedString::from(format!("resume-{}-dim-{}", item.id, row_dim));
    apply_row_dim(
        dim_id,
        row_dim,
        div()
            .flex()
            .flex_col()
            // Without this, a rail's flex child shrinks to fit
            // instead of overflowing into the scroll area (the edge-fade
            // assumes real overflow); a no-op for exact-width grid cells.
            .flex_shrink_0()
            .gap_1()
            .child(art_box)
            .child(title_block)
            .into_any_element(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- Display-time wrapping-quote trim -----------------------------

    #[test]
    fn matching_wrapping_quote_pairs_are_stripped() {
        assert_eq!(display_title("\"Wuthering Heights\""), "Wuthering Heights");
        assert_eq!(
            display_title("\u{201c}Wuthering Heights\u{201d}"),
            "Wuthering Heights"
        );
        assert_eq!(display_title("'Round Midnight'"), "Round Midnight");
        assert_eq!(display_title("\u{2018}Heat\u{2019}"), "Heat");
        // Nested pairs unwrap one layer at a time.
        assert_eq!(display_title("\"\u{201c}Nested\u{201d}\""), "Nested");
    }

    #[test]
    fn unmatched_quotes_are_left_alone() {
        assert_eq!(display_title("'Salem's Lot"), "'Salem's Lot");
        assert_eq!(display_title("\"Unclosed"), "\"Unclosed");
        assert_eq!(display_title("Unopened\""), "Unopened\"");
        // Curly open with a straight close is not a pair.
        assert_eq!(
            display_title("\u{201c}Mismatched\""),
            "\u{201c}Mismatched\""
        );
        assert_eq!(display_title("\""), "\"");
        assert_eq!(display_title("\"\""), "\"\"");
    }

    #[test]
    fn interior_quotes_survive_untouched() {
        assert_eq!(
            display_title("The \"Big\" Bang Theory"),
            "The \"Big\" Bang Theory"
        );
        assert_eq!(display_title("Wuthering Heights"), "Wuthering Heights");
        assert_eq!(display_title(""), "");
    }

    fn base_row(item_type: &str) -> CardRow {
        CardRow {
            id: "item-1".to_string(),
            item_type: item_type.to_string(),
            name: "Item".to_string(),
            primary_tag: None,
            blurhash: None,
            played: false,
            position_ticks: 0,
            runtime_ticks: None,
            unplayed_count: None,
            production_year: None,
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

    /// Pins: an Episode's own 16:9 `Primary` never fills a 2:3 poster slot,
    /// even when present -- falls back to the series poster instead.
    #[test]
    fn episode_never_uses_its_own_primary_for_a_poster_slot() {
        let mut ep = base_row("Episode");
        ep.primary_tag = Some("episode-still".to_string());
        ep.series_id = Some("series-1".to_string());
        ep.series_primary_tag = Some("series-poster".to_string());

        match poster_art_source(&ep) {
            PosterArtSource::SeriesFallback(id, tag) => {
                assert_eq!(id, "series-1");
                assert_eq!(tag, "series-poster");
            }
            other => panic!("expected SeriesFallback, got a variant that isn't it: {other:?}"),
        }
    }

    #[test]
    fn episode_with_no_series_poster_falls_through_to_placeholder() {
        let mut ep = base_row("Episode");
        ep.primary_tag = Some("episode-still".to_string());
        ep.series_id = Some("series-1".to_string());
        // series_primary_tag left None -- nothing in the chain has art.
        assert!(matches!(poster_art_source(&ep), PosterArtSource::None));
    }

    #[test]
    fn season_with_its_own_poster_uses_it() {
        let mut season = base_row("Season");
        season.primary_tag = Some("season-poster".to_string());
        season.series_id = Some("series-1".to_string());
        season.series_primary_tag = Some("series-poster".to_string());

        match poster_art_source(&season) {
            PosterArtSource::Own(tag) => assert_eq!(tag, "season-poster"),
            other => panic!("expected Own, got: {other:?}"),
        }
    }

    #[test]
    fn season_without_its_own_poster_falls_back_to_series_poster() {
        let mut season = base_row("Season");
        season.series_id = Some("series-1".to_string());
        season.series_primary_tag = Some("series-poster".to_string());

        match poster_art_source(&season) {
            PosterArtSource::SeriesFallback(id, tag) => {
                assert_eq!(id, "series-1");
                assert_eq!(tag, "series-poster");
            }
            other => panic!("expected SeriesFallback, got: {other:?}"),
        }
    }

    #[test]
    fn movie_always_uses_its_own_primary() {
        let mut movie = base_row("Movie");
        movie.primary_tag = Some("movie-poster".to_string());
        match poster_art_source(&movie) {
            PosterArtSource::Own(tag) => assert_eq!(tag, "movie-poster"),
            other => panic!("expected Own, got: {other:?}"),
        }
    }

    #[test]
    fn movie_with_no_art_at_all_is_none() {
        let movie = base_row("Movie");
        assert!(matches!(poster_art_source(&movie), PosterArtSource::None));
    }

    /// Pins: an episode with its own `Primary` still uses it directly.
    #[test]
    fn rail_art_uses_episodes_own_primary_when_present() {
        let mut ep = base_row("Episode");
        ep.primary_tag = Some("episode-still".to_string());
        ep.parent_backdrop_item_id = Some("series-1".to_string());
        ep.parent_backdrop_tag = Some("series-backdrop".to_string());
        match rail_art_source(&ep) {
            RailArtSource::Own(tag) => assert_eq!(tag, "episode-still"),
            other => panic!("expected Own, got: {other:?}"),
        }
    }

    /// Pins: an episode with no `Primary` still falls back to the nearest
    /// ancestor backdrop, not straight to nothing.
    #[test]
    fn rail_art_falls_back_to_parent_backdrop_when_episode_has_no_primary() {
        let mut ep = base_row("Episode");
        ep.primary_tag = None;
        ep.parent_backdrop_item_id = Some("series-1".to_string());
        ep.parent_backdrop_tag = Some("series-backdrop".to_string());
        match rail_art_source(&ep) {
            RailArtSource::ParentBackdrop(id, tag) => {
                assert_eq!(id, "series-1");
                assert_eq!(tag, "series-backdrop");
            }
            other => panic!("expected ParentBackdrop, got: {other:?}"),
        }
    }

    /// Pins: nothing anywhere in the chain has art -> `None`, never a
    /// silently reused tag.
    #[test]
    fn rail_art_is_none_when_nothing_in_the_chain_has_art() {
        let ep = base_row("Episode");
        assert!(matches!(rail_art_source(&ep), RailArtSource::None));
    }

    #[test]
    fn format_runtime_hours_and_minutes() {
        assert_eq!(format_runtime(3_600_000_000), "6m");
        assert_eq!(format_runtime(45 * 60 * 10_000_000), "45m");
        assert_eq!(format_runtime((90 * 60) as i64 * 10_000_000), "1h 30m");
    }

    // ---- Virtual episode status label ---------------------------------

    #[test]
    fn virtual_status_label_is_airs_date_for_a_future_premiere() {
        let future = (chrono::Utc::now() + chrono::Duration::days(30)).to_rfc3339();
        let label = virtual_status_label(Some(&future));
        assert!(
            label.starts_with("Airs "),
            "expected an \"Airs <date>\" label, got: {label}"
        );
    }

    #[test]
    fn virtual_status_label_is_missing_for_a_past_premiere() {
        let past = (chrono::Utc::now() - chrono::Duration::days(30)).to_rfc3339();
        assert_eq!(virtual_status_label(Some(&past)), "Missing");
    }

    #[test]
    fn virtual_status_label_is_missing_with_no_premiere_date_at_all() {
        assert_eq!(virtual_status_label(None), "Missing");
    }

    #[test]
    fn virtual_status_label_is_missing_for_an_unparseable_date() {
        assert_eq!(virtual_status_label(Some("not-a-date")), "Missing");
    }

    // ---- Continue Watching / Next Up card redesign -------

    #[test]
    fn season_episode_label_formats_both_numbers() {
        let mut ep = base_row("Episode");
        ep.parent_index_number = Some(3);
        ep.index_number = Some(9);
        assert_eq!(season_episode_label(&ep).as_deref(), Some("S3 E9"));
    }

    /// Pins: a missing season/episode number is dropped, never "S? E?".
    #[test]
    fn season_episode_label_drops_a_missing_season_number() {
        let mut ep = base_row("Episode");
        ep.index_number = Some(9);
        assert_eq!(season_episode_label(&ep).as_deref(), Some("E9"));
    }

    #[test]
    fn season_episode_label_drops_a_missing_episode_number() {
        let mut ep = base_row("Episode");
        ep.parent_index_number = Some(3);
        assert_eq!(season_episode_label(&ep).as_deref(), Some("S3"));
    }

    #[test]
    fn season_episode_label_is_none_for_a_movie() {
        let movie = base_row("Movie");
        assert_eq!(season_episode_label(&movie), None);
    }

    #[test]
    fn remaining_label_subtracts_position_from_runtime() {
        let mut ep = base_row("Episode");
        ep.runtime_ticks = Some(30 * 60 * 10_000_000); // 30m
        ep.position_ticks = 18 * 60 * 10_000_000; // 18m watched
        assert_eq!(remaining_label(&ep).as_deref(), Some("12m left"));
    }

    /// Pins: an unstarted (Next Up) item's "remaining" is just its full
    /// runtime.
    #[test]
    fn remaining_label_is_full_runtime_when_unstarted() {
        let mut ep = base_row("Episode");
        ep.runtime_ticks = Some(42 * 60 * 10_000_000);
        ep.position_ticks = 0;
        assert_eq!(remaining_label(&ep).as_deref(), Some("42m left"));
    }

    #[test]
    fn remaining_label_is_none_with_no_runtime_data() {
        let ep = base_row("Episode");
        assert_eq!(remaining_label(&ep), None);
    }

    #[test]
    fn resume_meta_line_joins_season_episode_and_remaining() {
        let mut ep = base_row("Episode");
        ep.parent_index_number = Some(3);
        ep.index_number = Some(9);
        ep.runtime_ticks = Some(30 * 60 * 10_000_000);
        ep.position_ticks = 18 * 60 * 10_000_000;
        assert_eq!(resume_meta_line(&ep).as_deref(), Some("S3 E9 · 12m left"));
    }

    /// Pins: a Movie (no season/episode numbers) degrades to just the
    /// remaining time.
    #[test]
    fn resume_meta_line_is_just_remaining_time_for_a_movie() {
        let mut movie = base_row("Movie");
        movie.runtime_ticks = Some(120 * 60 * 10_000_000);
        movie.position_ticks = 90 * 60 * 10_000_000;
        assert_eq!(resume_meta_line(&movie).as_deref(), Some("30m left"));
    }

    /// Pins: a virtual episode shows its status label, not a remaining time.
    #[test]
    fn resume_meta_line_uses_virtual_status_label_for_a_virtual_episode() {
        let mut ep = base_row("Episode");
        ep.parent_index_number = Some(2);
        ep.index_number = Some(4);
        ep.is_virtual = true;
        ep.premiere_date = None;
        assert_eq!(resume_meta_line(&ep).as_deref(), Some("S2 E4 · Missing"));
    }

    #[test]
    fn resume_meta_line_is_none_with_nothing_to_show() {
        let movie = base_row("Movie");
        assert_eq!(resume_meta_line(&movie), None);
    }
}
