//! Shared anchored popover primitive (`docs/DESIGN-GUIDE.md` Part B §9, Part
//! D), replacing each caller's own hand-built full-viewport click-catcher
//! plus manually-centered fixed child, which never actually anchored to
//! what opened it.
//!
//! `popover_trigger` wraps the trigger and popover in one `div().relative()`
//! container so `anchored()`'s default position (no `.position()` call)
//! resolves against that container's own on-screen box via GPUI's layout
//! pass, with no manual viewport math.
//!
//! `click_away_catcher` is a separate full-window `deferred()` element at a
//! lower paint priority than the popover panel, so it can be mounted from
//! wherever a full-size container is available while the panel, mounted
//! deep at the trigger, still paints on top -- `deferred()` priority is a
//! frame-wide ordering, not tied to tree location.

use gpui::{
    anchored, deferred, div, prelude::*, px, rgb, rgba, AnimationExt, App, Corner, Deferred, Div,
    ElementId, IntoElement, Pixels, Point, Stateful,
};

use crate::theme;

/// `anchor`/`offset` follow docs/DESIGN-GUIDE.md Part B §9's per-instance
/// anchor table. `panel` is a closure, not a pre-built element, so a closed
/// popover with a long list never builds rows nobody sees.
pub(crate) fn popover_trigger<P: IntoElement>(
    id: impl Into<ElementId>,
    trigger: impl IntoElement,
    open: bool,
    anchor: Corner,
    offset: Point<Pixels>,
    panel: impl FnOnce() -> P,
) -> impl IntoElement {
    div().id(id).relative().child(trigger).when(open, |d| {
        d.child(
            deferred(
                anchored()
                    .anchor(anchor)
                    .offset(offset)
                    .snap_to_window_with_margin(theme::popover_window_margin())
                    .child(panel()),
            )
            .with_priority(1),
        )
    })
}

/// Full-viewport click-away catcher, no scrim (Part B §9 distinguishes
/// popovers from modal dialogs, see `dialog_scrim` in `ui/components.rs`).
/// Priority 0 so a `popover_trigger` panel (priority 1) painted this same
/// frame lands on top of it.
pub(crate) fn click_away_catcher(
    id: impl Into<ElementId>,
    on_dismiss: impl Fn(&mut App) + 'static,
) -> Deferred {
    deferred(
        div()
            .id(id)
            .absolute()
            .inset_0()
            .on_click(move |_event, _window, cx| on_dismiss(cx)),
    )
    .with_priority(0)
}

/// Part B §9's `popover_panel` anatomy, shared by every popover instance.
/// Stops its own click from bubbling to a `click_away_catcher` mounted
/// behind it.
pub(crate) fn popover_panel(
    id: impl Into<ElementId>,
    max_h: Pixels,
    content: impl IntoElement,
) -> impl IntoElement {
    let id = id.into();
    // Part B §6's `motion.reveal` (200ms ease_out_quint) entrance fade,
    // applied here once for every consumer. Reusing the same element id on
    // reopen means the fade only restarts on a fresh `open` transition.
    div()
        .id(id.clone())
        .min_w(px(200.))
        .max_w(px(360.))
        .max_h(max_h)
        .p_1()
        .rounded_lg()
        .bg(rgb(theme::SURFACE_PANEL))
        .border_1()
        .border_color(rgb(theme::SURFACE_HAIRLINE))
        .shadow(theme::shadow_e2())
        .overflow_y_scroll()
        .on_click(|_event, _window, cx| {
            cx.stop_propagation();
        })
        .child(content)
        .with_animation(id, theme::reveal_animation(), |el, delta| el.opacity(delta))
}

/// One selectable row inside a popover panel (Part B §9's item-row spec).
/// `highlighted` is the keyboard-nav cursor, styled identically to hover so
/// keyboard and mouse selection read the same way.
pub(crate) fn popover_row(
    id: impl Into<ElementId>,
    selected_or_highlighted: bool,
    content: impl IntoElement,
) -> Stateful<Div> {
    div()
        .id(id)
        .flex()
        .items_center()
        .justify_between()
        .gap_2()
        .rounded_md()
        .px_3()
        .py_2()
        .cursor_pointer()
        .when(selected_or_highlighted, |d| {
            d.bg(rgb(theme::SURFACE_OVERLAY))
        })
        .hover(|s| s.bg(rgb(theme::SURFACE_OVERLAY)))
        .child(content)
}

/// Section label for grouping rows inside a popover panel (Part B §9/§10).
/// Unused for now; kept as scaffolding for a future Subtitles/Forced-SDH
/// split once `player::Track` carries that classification.
#[allow(dead_code)]
pub(crate) fn popover_section_label(text: impl Into<gpui::SharedString>) -> impl IntoElement {
    div()
        .px_3()
        .pt_2()
        .pb_1()
        .text_size(theme::TEXT_CAPTION)
        .text_color(rgba(theme::TEXT_TERTIARY))
        .child(text.into())
}

/// Up/Down arrow-key index math for a popover's keyboard-nav cursor.
/// Clamps rather than wraps, matching `focus_grid.rs`'s convention. `None`
/// in with `delta >= 0` lands on index 0, not row 1.
pub(crate) fn move_highlight(current: Option<usize>, delta: i32, len: usize) -> Option<usize> {
    if len == 0 {
        return None;
    }
    let next = match current {
        None if delta >= 0 => 0,
        None => 0,
        Some(ix) => (ix as i32 + delta).clamp(0, len as i32 - 1) as usize,
    };
    Some(next)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn move_highlight_from_none_lands_on_first_row() {
        assert_eq!(move_highlight(None, 1, 5), Some(0));
        assert_eq!(move_highlight(None, -1, 5), Some(0));
    }

    #[test]
    fn move_highlight_clamps_at_both_ends() {
        assert_eq!(move_highlight(Some(0), -1, 5), Some(0));
        assert_eq!(move_highlight(Some(4), 1, 5), Some(4));
    }

    #[test]
    fn move_highlight_steps_by_delta() {
        assert_eq!(move_highlight(Some(2), 1, 5), Some(3));
        assert_eq!(move_highlight(Some(2), -1, 5), Some(1));
    }

    #[test]
    fn move_highlight_empty_list_is_none() {
        assert_eq!(move_highlight(Some(0), 1, 0), None);
    }
}
