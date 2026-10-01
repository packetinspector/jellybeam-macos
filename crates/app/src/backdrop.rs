//! Visual-pass §1's shared backdrop scrim, used by Home's hero
//! (`home.rs::render_hero`) and every Detail backdrop (movie/series/episode,
//! `detail.rs`). Bottom to top, per the owner's exact spec:
//!
//!   (a) image, object-fit cover
//!   (b) blur(20px) + brightness(0.45) BELOW the fold -- sharp only in the
//!       top ~60% of the backdrop's own height
//!   (c) `linear-gradient(to top, base-bg 0%, base-bg 45%, transparent 100%)`
//!   (d) `linear-gradient(to right, base-bg 0%, rgba(base,0.7) 25%, transparent 55%)`
//!       -- pulled back from 35%/65% in visual pass §6 (the left two-thirds
//!       of the hero was reading as nearly solid black)
//!
//! Content sits on top of all four (the caller adds it as a further child
//! after `layer()`).
//!
//! **GPUI reality**, two approximations, both intentional:
//!
//! 1. No CSS `filter: blur()/brightness()` exists for a `div()`/`img()` in
//!    this GPUI version. (b) is instead produced on the CPU ahead of paint
//!    (`image_store.rs`'s `decode_bytes_scrim`: downscale, Gaussian blur,
//!    flat RGB multiply for the darken step), cached in the same decoded-LRU
//!    the sharp texture lives in, and painted here as a second `img()`
//!    layer positioned to cover only the bottom region.
//! 2. `gpui::linear_gradient` takes exactly two color stops, not the CSS
//!    three-stop gradients (c)/(d) call for. Each is instead built from two
//!    stacked sub-layers -- a flat block for the "still opaque" segment plus
//!    a 2-stop fade for the remaining segment -- reproducing the same
//!    piecewise-linear curve as two GPUI-native layers. This composition
//!    also happens to cover the sharp/blurred seam from approximation 1:
//!    (c)'s flat segment starts at 45% height-from-bottom, inside where the
//!    blur layer begins at 40%, so the seam sits under fully-opaque
//!    `base-bg`, never visible.

use gpui::{
    div, img, linear_color_stop, linear_gradient, prelude::*, relative, rgb, rgba, AnyElement,
    ObjectFit, RenderImage, Rgba, StyledImage,
};
use std::sync::Arc;

use crate::theme;

/// §1: "sharp only in the top ~60vh" -- expressed as a fraction of the
/// backdrop's own height (both call sites are themselves already
/// container-relative elements: Home's hero is `h(relative(0.42))` of the
/// page, Detail's backdrop is the full page). The blurred layer covers the
/// remaining bottom `1.0 - SHARP_TOP_FRACTION`.
const SHARP_TOP_FRACTION: f32 = 0.6;

/// (c)'s flat-opaque segment, exactly as spec'd: `base-bg 0%, base-bg 45%`.
const VERTICAL_FLAT_FRACTION: f32 = 0.45;

/// (d)'s two breakpoints. Visual pass §6: the left two-thirds of the hero
/// was reading as nearly solid black, so this was pulled back from
/// opaque-until-35%/transparent-by-65% to opaque-until-25%/transparent-by-55%
/// -- same 30%-wide fade band, just shifted left so it clears sooner.
const HORIZONTAL_OPAQUE_FRACTION: f32 = 0.25;
const HORIZONTAL_TRANSPARENT_FRACTION: f32 = 0.55;
/// (d)'s middle stop: `rgba(base, 0.7)`.
const HORIZONTAL_MID_ALPHA: u8 = 0xb3; // 0.7 * 255, rounded

/// The full (a)+(b)+(c)+(d) stack, absolutely filling its (already
/// `.relative()`) parent. `sharp`/`blurred` are `None` while their fetch is
/// still in flight -- `sharp` falls back to a flat `surface.raised` tile
/// (matches every other art-loading placeholder in this app); `blurred`
/// simply isn't painted yet (the sharp layer -- or its placeholder --
/// already covers the whole area underneath it, so there's no gap).
pub(crate) fn layer(
    sharp: Option<Arc<RenderImage>>,
    blurred: Option<Arc<RenderImage>>,
) -> AnyElement {
    let sharp_layer: AnyElement = match sharp {
        Some(tex) => img(tex)
            .object_fit(ObjectFit::Cover)
            .size_full()
            .into_any_element(),
        None => div()
            .size_full()
            .bg(rgb(theme::SURFACE_RAISED))
            .into_any_element(),
    };

    div()
        .absolute()
        .inset_0()
        // (a) sharp, full-bleed.
        .child(div().absolute().inset_0().child(sharp_layer))
        // (b) blurred+darkened, bottom region only.
        .children(blurred.map(|tex| {
            div()
                .absolute()
                .top(relative(SHARP_TOP_FRACTION))
                .bottom_0()
                .left_0()
                .right_0()
                .overflow_hidden()
                .child(img(tex).object_fit(ObjectFit::Cover).size_full())
        }))
        // (c) vertical scrim.
        .child(vertical_scrim())
        // (d) horizontal scrim, painted last -- "content sits on (d)".
        .child(horizontal_scrim())
        .into_any_element()
}

fn base_bg() -> Rgba {
    rgb(theme::SURFACE_BASE)
}

/// (c): `linear-gradient(to top, base-bg 0%, base-bg 45%, transparent 100%)`,
/// as two stacked layers -- see this module's doc comment, approximation 2.
fn vertical_scrim() -> AnyElement {
    div()
        .absolute()
        .inset_0()
        .child(
            // 0%..45% (measuring from the bottom): flat, fully opaque.
            div()
                .absolute()
                .bottom_0()
                .left_0()
                .right_0()
                .h(relative(VERTICAL_FLAT_FRACTION))
                .bg(base_bg()),
        )
        .child(
            // 45%..100%: fades from opaque (at its own bottom edge, which is
            // exactly the flat block's top edge -- continuous, no seam) to
            // fully transparent at the container's top.
            div()
                .absolute()
                .top_0()
                .bottom(relative(VERTICAL_FLAT_FRACTION))
                .left_0()
                .right_0()
                .bg(linear_gradient(
                    0.,
                    linear_color_stop(base_bg(), 0.0),
                    linear_color_stop(rgba(theme::TRANSPARENT), 1.0),
                )),
        )
        .into_any_element()
}

/// (d): `linear-gradient(to right, base-bg 0%, rgba(base,0.7) 35%, transparent 65%)`,
/// as two stacked layers over the left `HORIZONTAL_TRANSPARENT_FRACTION` of
/// the container's width (nothing painted beyond that -- the spec's own
/// "transparent 65%" already means full transparency for the remaining
/// width, so there's nothing to paint there).
fn horizontal_scrim() -> AnyElement {
    div()
        .absolute()
        .inset_0()
        .flex()
        .flex_row()
        .child(
            // 0%..25%: opaque base-bg fading toward the 70%-alpha midpoint.
            div()
                .h_full()
                .w(relative(HORIZONTAL_OPAQUE_FRACTION))
                .bg(linear_gradient(
                    90.,
                    linear_color_stop(base_bg(), 0.0),
                    linear_color_stop(
                        rgba(theme::tint(theme::SURFACE_BASE, HORIZONTAL_MID_ALPHA)),
                        1.0,
                    ),
                )),
        )
        .child(
            // 25%..55%: the 70%-alpha midpoint fading to fully transparent.
            div()
                .h_full()
                .w(relative(
                    HORIZONTAL_TRANSPARENT_FRACTION - HORIZONTAL_OPAQUE_FRACTION,
                ))
                .bg(linear_gradient(
                    90.,
                    linear_color_stop(
                        rgba(theme::tint(theme::SURFACE_BASE, HORIZONTAL_MID_ALPHA)),
                        0.0,
                    ),
                    linear_color_stop(rgba(theme::TRANSPARENT), 1.0),
                )),
        )
        .into_any_element()
}
