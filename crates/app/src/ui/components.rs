//! Shared component primitives (`docs/DESIGN-GUIDE.md` Part B §8, §12, §13;
//! Part D): buttons, list rows, a status dot, a dialog/sheet scrim+panel
//! shell, a toast shell, section headers, and an icon+message empty state.
//! Each is a plain rendering-only builder fn in this codebase's existing
//! style (`cards.rs`, `player_ui.rs`) -- state stays with the caller.

use gpui::{
    div, linear_color_stop, linear_gradient, prelude::*, px, rgb, rgba, AnyElement, AnyView, App,
    Boundary, Div, ElementId, Font, FontWeight, IntoElement, LineFragment, Pixels, Render,
    ScrollHandle, SharedString, Stateful, TextRun, Window,
};

use crate::theme;

// Shared truncation treatment: gpui 0.2.2's `Styled::truncate()`
// (`overflow_hidden()` + `whitespace_nowrap()` + `text_ellipsis()`) and
// `Styled::line_clamp(n)` do real ellipsis truncation, single- and
// multi-line respectively -- the two helpers below are the one place that
// treatment lives, replacing every card/row's hand-rolled approximation.

/// Single-line truncation: a real "…" (gpui's native `.truncate()`, not a
/// hard mid-word clip) at a fixed reserved `height` so every card/row in a
/// group keeps an identical line height regardless of string length --
/// titles, names, character names, row labels, anywhere §1's "single-line"
/// bucket applies. Callers placing this inside a flex row that should
/// shrink to fit (rather than a fixed pixel width the caller sets
/// afterward via `.w(...)`) must also chain `.flex_1().min_w_0()` on this
/// div (or its wrapper) -- a flex item's default min-width is its content
/// size, which silently defeats `.truncate()`'s own clipping.
pub(crate) fn clamped_line(text: impl Into<SharedString>, height: Pixels) -> Div {
    div().h(height).truncate().child(text.into())
}

/// Cuts `text` down to at most `lines` wrapped lines at `wrap_width`,
/// ending on a real "…" -- the `-webkit-line-clamp: N` behaviour, computed
/// against the *actual* font metrics rather than approximated.
///
/// Not `.line_clamp(n)` + `.text_ellipsis()`: gpui's `text_overflow` layout
/// truncates against `available_width * line_clamp`, a single straight-line
/// budget that's always longer than what N *wrapped* lines actually hold
/// (word wrapping leaves ragged space per line), so the "…" lands on a
/// notional line N+1 that then gets clipped mid-glyph, ellipsis included.
///
/// Instead: `TextSystem::line_wrapper` reports exactly where each wrapped
/// line starts, and `truncate_line` cuts a single line to a width and
/// appends the suffix. If there are fewer than `lines` boundaries the text
/// already fits and is returned untouched; otherwise everything before the
/// last visible line is kept verbatim and only that line is truncated.
pub(crate) fn clamp_text_to_lines(
    text: &str,
    lines: usize,
    font: Font,
    font_size: Pixels,
    wrap_width: Pixels,
    cx: &App,
) -> SharedString {
    if text.is_empty() || lines == 0 || wrap_width <= px(0.) {
        return SharedString::from(text.to_string());
    }
    let mut wrapper = cx.text_system().line_wrapper(font, font_size);
    // Scoped so the `&mut wrapper` borrow the iterator holds ends before
    // `truncate_line` needs its own.
    let boundaries: Vec<Boundary> = {
        let fragments = [LineFragment::text(text)];
        wrapper
            .wrap_line(&fragments, wrap_width)
            .take(lines)
            .collect()
    };
    // `lines` wrapped lines need `lines - 1` boundaries; anything fewer
    // means the string already fits.
    if boundaries.len() < lines {
        return SharedString::from(text.to_string());
    }
    let last_line_start = if lines >= 2 {
        boundaries[lines - 2].ix
    } else {
        0
    };
    // gpui slices on these indices itself, so they are char boundaries --
    // but `split_at` panics if that assumption is ever wrong, and this runs
    // inside a render pass where a panic takes the window with it. Degrade
    // to "leave the text alone" instead; `.line_clamp()` below still bounds
    // the block's height.
    if !text.is_char_boundary(last_line_start) {
        return SharedString::from(text.to_string());
    }
    let (head, tail) = text.split_at(last_line_start);
    // `runs` is gpui's own decoration bookkeeping for a styled line. This
    // block is a single uniform run set by an ancestor, so there is nothing
    // to keep in step and an empty vec is the honest input --
    // `update_runs_after_truncation` iterates it and does nothing.
    let mut runs: Vec<TextRun> = Vec::new();
    let clipped = wrapper.truncate_line(
        SharedString::from(tail.to_string()),
        wrap_width,
        ELLIPSIS,
        &mut runs,
    );
    SharedString::from(format!("{head}{clipped}"))
}

/// The truncation marker `clamp_text_to_lines` appends. U+2026, matching
/// gpui's own `text_ellipsis()`.
const ELLIPSIS: &str = "…";

/// Multi-line clamp at a fixed reserved height (`lines * line_height`), so
/// every clamped block in a group keeps an identical height regardless of
/// string length -- synopses, descriptions, anywhere §1's "multi-line"
/// bucket applies.
///
/// `font`/`font_size`/`wrap_width` must describe the text as it will
/// actually be set (the caller's own `text_xs()` etc. and the resolved
/// content width), because the clamp is measured, not guessed -- see
/// [`clamp_text_to_lines`]. `.line_clamp()` is still applied on top as a
/// cheap backstop: if a caller ever passes a width that disagrees with the
/// laid-out one, the block still cannot grow past its reserved height.
///
/// Callers set their own `text_color` on the returned `Div` (it cascades to
/// the inner text child like any other GPUI text-style refinement).
pub(crate) fn clamped_block(
    text: &str,
    lines: usize,
    line_height: Pixels,
    font: Font,
    font_size: Pixels,
    wrap_width: Pixels,
    cx: &App,
) -> Div {
    let clamped = clamp_text_to_lines(text, lines, font, font_size, wrap_width, cx);
    div()
        .h(line_height * lines)
        .w_full()
        .line_height(line_height)
        .text_size(font_size)
        .line_clamp(lines)
        .child(clamped)
}

// gpui 0.2.2's `.tooltip()` takes a `Fn(&mut Window, &mut App) -> AnyView`
// builder rather than a plain string, so this is a tiny throwaway `Render`
// view wrapping one label, the minimum needed to satisfy that signature.

struct TextTooltip(SharedString);

impl Render for TextTooltip {
    fn render(&mut self, _window: &mut Window, _cx: &mut gpui::Context<Self>) -> impl IntoElement {
        div()
            .px_2()
            .py_1()
            .rounded_md()
            .bg(rgb(theme::SURFACE_PANEL))
            .border_1()
            .border_color(rgb(theme::SURFACE_HAIRLINE))
            .shadow(theme::shadow_e2())
            .text_size(theme::TEXT_METADATA)
            .text_color(rgba(theme::TEXT_PRIMARY))
            .child(self.0.clone())
    }
}

/// Builds a `.tooltip(...)`-ready closure showing `text` in a small
/// `surface.panel` bubble -- pass to `Stateful<Div>::tooltip`, e.g. the
/// collapsed sidebar rail's icon-only rows (`root.rs::render_main`).
pub(crate) fn tooltip_text(
    text: impl Into<SharedString>,
) -> impl Fn(&mut Window, &mut App) -> AnyView + 'static {
    let text = text.into();
    move |_window: &mut Window, cx: &mut App| cx.new(|_cx| TextTooltip(text.clone())).into()
}

/// **The** focus and current-item treatment for the whole app: a 2px
/// `PISTACCHIO` stroke sitting 2px clear of the element's own edge (CSS
/// `outline: 2px solid; outline-offset: 2px`).
///
/// gpui 0.2.2 has no outline primitive: a real `border` paints *inside* the
/// box and shifts layout on focus, and a spread `BoxShadow` paints outside
/// without shifting layout but is always flush with no way to ask for a
/// gap. So the ring is an absolutely-positioned sibling at
/// `-theme::FOCUS_RING_OUTSET` on all four sides, carrying its own 2px
/// border: gpui paints that on the *inside* of the overlay's box, leaving
/// the gap band genuinely transparent. The caller mounts it as a child of a
/// `.relative()` element and passes that element's own corner radius, grown
/// by the outset to stay concentric. Non-interactive by construction: no
/// id, no listener, no hover style, so it can never swallow a click.
pub(crate) fn focus_ring(radius: Pixels) -> Div {
    let outset = px(-f32::from(theme::FOCUS_RING_OUTSET));
    div()
        .absolute()
        .top(outset)
        .left(outset)
        .right(outset)
        .bottom(outset)
        .rounded(radius + theme::FOCUS_RING_OUTSET)
        // = theme::FOCUS_RING_WIDTH; gpui 0.2.2 has only the fixed-width
        // `border_N()` family, no `.border(Pixels)` -- `theme.rs`'s own
        // `the_focus_ring_is_crisp_...` test pins the two together.
        .border_2()
        .border_color(rgb(theme::ACCENT))
}

// ---------------------------------------------------------------------
// B.8 Buttons
// ---------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ButtonVariant {
    Primary,
    Secondary,
    /// Part B §8's third variant (sidebar rows, tab labels, toolbar
    /// buttons) -- none of the four retrofit targets here happen to
    /// need a plain Ghost button (the Settings rail uses `list_row`
    /// directly, the switcher's footer row uses `popover_row`), but the
    /// Home/Library sidebar-chrome pass (Part D) does. Kept rather than
    /// dropped and re-added identically then.
    #[allow(dead_code)]
    Ghost,
    /// Not in Part B's original three -- a `Ghost`-shaped button whose text
    /// is the `danger` token instead of `text.secondary`, for destructive
    /// actions that don't warrant a full modal confirm (e.g. Settings'
    /// existing "Remove server" row action). Same states/sizing as `Ghost`.
    GhostDanger,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ButtonSize {
    Sm,
    Md,
    Lg,
}

impl ButtonSize {
    fn height(self) -> Pixels {
        match self {
            ButtonSize::Sm => px(28.),
            ButtonSize::Md => px(36.),
            // Brand §5: "padding 11×22" around a 15px Archivo 700 label,
            // i.e. 11 + ~22 line + 11 = 44px. The size this scale already
            // had, now with its brand justification.
            ButtonSize::Lg => px(44.),
        }
    }
    fn text_size(self) -> Pixels {
        match self {
            ButtonSize::Sm => theme::TEXT_METADATA,
            ButtonSize::Md => theme::TEXT_BODY,
            // §5's exact "Archivo 700 15px".
            ButtonSize::Lg => theme::TEXT_BODY,
        }
    }
    /// §5's horizontal padding. The 22px is the brief's own figure for the
    /// full-size button; the two smaller steps scale it down proportionally
    /// rather than falling back to the old 4px-grid values, because a pill
    /// needs noticeably more side inset than a rounded rect to keep its
    /// label off the curve.
    fn h_padding(self) -> Pixels {
        match self {
            ButtonSize::Sm => px(12.),
            ButtonSize::Md => px(16.),
            ButtonSize::Lg => px(22.),
        }
    }
}

/// Brand §5's buttons x Part B §8's size scale. `disabled` = 40% opacity, no
/// pointer cursor, no hover fill -- the caller simply doesn't attach
/// `.on_click` when disabled (this fn can't stop that at the type level).
///
/// Every variant is a **fully rounded 999px pill** (§5: "Buttons — fully
/// rounded, 999px"), which is the single biggest shape change of the brand
/// retrofit: buttons used to be `radius.control` rounded rects
/// indistinguishable from inputs and chips.
pub(crate) fn button(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    variant: ButtonVariant,
    size: ButtonSize,
    disabled: bool,
) -> Stateful<Div> {
    let mut el = div()
        .id(id)
        .flex()
        .items_center()
        .justify_center()
        .h(size.height())
        .px(size.h_padding())
        .rounded(theme::RADIUS_PILL)
        .text_size(size.text_size())
        .when(!disabled, |d| d.cursor_pointer())
        .when(disabled, |d| d.opacity(0.4));

    el = match variant {
        ButtonVariant::Primary => {
            // §5: "Primary: PISTACCHIO fill with a #14100D label, Archivo
            // 700 15px." The accent's one large filled use in the product.
            let el = el
                .bg(rgb(theme::PRIMARY_BUTTON_BG))
                .text_color(rgb(theme::PRIMARY_BUTTON_TEXT))
                .font_weight(FontWeight::BOLD);
            if disabled {
                el
            } else {
                el.hover(|s| s.bg(rgb(theme::PRIMARY_BUTTON_BG_HOVER)))
                    .active(|s| s.bg(rgb(theme::PRIMARY_BUTTON_BG_PRESSED)))
            }
        }
        ButtonVariant::Secondary => {
            // §5: transparent with a 1px HAIRLINE border and PANNA-2 label.
            // The label is Archivo **700**, same weight as Primary's: the
            // two variants are the same box at the same size, only the
            // fill distinguishes them (a 400-weight label inside a
            // hairline pill reads as a link, not a button).
            let el = el
                .bg(rgba(theme::TRANSPARENT))
                .text_color(rgba(theme::TEXT_SECONDARY))
                .font_weight(FontWeight::BOLD)
                .border_1()
                .border_color(rgb(theme::SURFACE_HAIRLINE));
            if disabled {
                el
            } else {
                // Hover raises the border one step up the warm neutral
                // ladder (HAIRLINE -> GRIGIO); a fill on hover would read
                // as the Primary it deliberately isn't.
                el.hover(|s| s.border_color(rgb(theme::GRIGIO)))
            }
        }
        ButtonVariant::Ghost => {
            let el = el.text_color(rgba(theme::TEXT_SECONDARY));
            if disabled {
                el
            } else {
                el.hover(|s| {
                    s.bg(rgb(theme::SURFACE_OVERLAY))
                        .text_color(rgba(theme::TEXT_PRIMARY))
                })
            }
        }
        ButtonVariant::GhostDanger => {
            // No red: §2 allows exactly one hue (the accent), so a
            // destructive action is the loudest warm neutral
            // (`theme::DANGER`) against Ghost's quieter secondary instead;
            // the word "Remove" plus the confirm dialog carry the actual
            // destructiveness. See `theme::DANGER`'s doc comment.
            let el = el.text_color(rgb(theme::DANGER));
            if disabled {
                el
            } else {
                el.hover(|s| s.bg(rgb(theme::SURFACE_OVERLAY)))
            }
        }
    };

    el.child(label.into())
}

/// Sidebar rows, search results, server list, settings rows.
///
/// Brand §5: 8px radius, `SURFACE` fill, no border; the selected nav row is
/// a fill only, no left accent bar -- §2 spends the accent on actions and
/// state the eye should chase, and a fill answers "which row am I on"
/// perfectly well. The fill is `surface.overlay` rather than
/// `surface.raised` since this row is used both on the sidebar (already
/// `surface.raised`) and inside Settings' `surface.panel` sheet.
pub(crate) fn list_row(
    id: impl Into<ElementId>,
    active: bool,
    content: impl IntoElement,
) -> Stateful<Div> {
    div()
        .id(id)
        .flex()
        .items_center()
        .justify_between()
        .rounded(theme::RADIUS_CARD)
        .px(theme::SPACE_DEFAULT)
        .py(theme::SPACE_SNUG)
        .when(active, |d| d.bg(rgb(theme::SURFACE_OVERLAY)))
        .hover(|s| s.bg(rgb(theme::SURFACE_OVERLAY)))
        .child(content)
}

/// Brand §5's status mark: a 6px `PISTACCHIO` dot plus Martian Mono 10px
/// label. This is the dot half; [`status_label`] below is the matching mono
/// treatment for the text. Inactive is the same 6px circle as an outline in
/// `text.tertiary` rather than a second colour -- §2 leaves no second hue
/// for "not connected", and filled-vs-hollow needs no legend.
pub(crate) fn status_dot(active: bool) -> impl IntoElement {
    let base = div().size(px(6.)).flex_shrink_0().rounded_full();
    if active {
        base.bg(rgb(theme::ACCENT))
    } else {
        base.border_1().border_color(rgba(theme::TEXT_TERTIARY))
    }
}

/// Brand §3/§5's status label: Martian Mono 10px in a warm neutral, and
/// factual/uppercase by convention at every call site (`CONNECTED`,
/// `OFFLINE`). Pairs with [`status_dot`].
///
/// `emphasis` picks the tone: `true` for the state the viewer is meant to
/// read first (an error line), `false` for a resting one -- never a second
/// hue, per §2. See `theme::DANGER`/`theme::WARNING`'s doc comments.
pub(crate) fn status_label(text: impl Into<SharedString>, emphasis: bool) -> impl IntoElement {
    div()
        .font_family(theme::FONT_MONO)
        .text_size(theme::TEXT_SPEC)
        .text_color(rgb(if emphasis {
            theme::DANGER
        } else {
            theme::WARNING
        }))
        .child(text.into())
}

/// Page-level section header (Section role, 20px/Semibold) -- shelf row
/// titles, settings/popover section sub-groupings. Currently unconsumed;
/// kept ready rather than deleted and re-added verbatim later (see
/// `settings.rs::render`'s doc comment for why its own header uses the
/// *Title* role instead).
#[allow(dead_code)]
pub(crate) fn section_title(text: impl Into<SharedString>) -> impl IntoElement {
    div()
        .text_xl()
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(rgba(theme::TEXT_PRIMARY))
        .child(text.into())
}

/// Dense sub-grouping label (Settings section labels, popover section
/// splits) -- `text.tertiary` + `text.metadata` (13px), the smaller of
/// Part B §13's two header tiers.
pub(crate) fn dense_label(text: impl Into<SharedString>) -> impl IntoElement {
    div()
        .text_size(theme::TEXT_METADATA)
        .text_color(rgba(theme::TEXT_TERTIARY))
        .mt_3()
        .mb_1()
        .child(text.into())
}

// Covers the settings form grid's rows (`settings.rs::render` covers the
// sidebar+content shell). Before this, bitrate/skip-segment/subtitle preset
// rows stacked their label *above* a button row while the Server section
// used label-left/actions-right (`list_row`) -- two row grammars on the
// same sheet reading as inconsistent. `form_row` + `chip_button` give every
// non-`list_row` settings row the same label-left/control-right grammar at
// the same row height as `button()`'s `Sm` size.

/// One row of the settings form grid: label flush-left, control cluster
/// flush-right, both edges aligned down their own column across every row
/// in a section (flex `justify_between`). The control edge is intentionally
/// trailing-aligned only, per Part B §13's "controls right-aligned"
/// phrasing, not a second fixed column, since different controls are
/// legitimately different widths. `min_h` matches `list_row`'s own
/// effective height so rows read as the same rhythm down the same sheet.
pub(crate) fn form_row(
    label: impl Into<SharedString>,
    control: impl IntoElement,
) -> impl IntoElement {
    div()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .gap_4()
        .min_h(px(44.))
        .w_full()
        .child(
            // §0: the label is the FLEXIBLE column — it shrinks/truncates so
            // the control cluster never gets pushed past the panel edge.
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_size(theme::TEXT_METADATA)
                .text_color(rgba(theme::TEXT_SECONDARY))
                .child(label.into()),
        )
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .flex_shrink_0()
                .gap_2()
                .child(control),
        )
}

/// `form_row` plus a muted help/description line underneath, for a row
/// whose control needs a sentence of explanation. Previously callers
/// rendered the description as a plain sibling `div` after `form_row` with
/// no shared layout contract -- easy to nest *inside* `form_row`'s label
/// column by mistake, which inherits `.truncate()` and hard-clips the
/// sentence mid-word instead of wrapping it. Here the description spans the
/// row's own full width below it (not the label column's, which can be
/// squeezed arbitrarily narrow), with `min_w_0()` and no truncation
/// anywhere in its chain so it wraps instead of overflowing.
pub(crate) fn form_row_desc(
    label: impl Into<SharedString>,
    control: impl IntoElement,
    description: impl Into<SharedString>,
) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .w_full()
        .min_w_0()
        .gap_1()
        .child(form_row(label, control))
        .child(
            div()
                .min_w_0()
                .text_size(theme::TEXT_METADATA)
                .text_color(rgba(theme::TEXT_QUATERNARY))
                .child(description.into()),
        )
}

/// A pill-shaped chip for a segmented preset choice or a single on/off
/// toggle (Settings' bitrate cap / skip-segment action / subtitle preset /
/// bold toggle) -- sized exactly like `button()`'s `Sm` variant (28px
/// height, `text.metadata`, `space.tight` padding) so these chips sit on
/// the same baseline as the Switch/Remove `button()` calls in the same
/// sheet, unlike the old bespoke `px_3().py_1().text_sm()` shape this
/// replaces.
///
/// Visual pass 2 §2 ("the accent is overused"): `active` used to drive an
/// `accent`-filled vs. `surface.panel` resting state, the same amber fill
/// as the Play/Resume button -- with five skip-segment rows plus bitrate/
/// delay/subtitle presets all selectable at once, a single Playback pane
/// could show seven amber fills simultaneously, and amber stopped reading
/// as "press this." Selected uses a neutral raised surface
/// (`theme::CONTROL_SELECTED_FILL`, PANNA 10%) plus a 1px hairline ring;
/// unselected is fully transparent at 60% text, no fill, no accent.
///
/// Brand retrofit: the ring used to be a *top-edge-only* border
/// (`border_t_1`), approximating the inset top highlight GPUI 0.2.2 has no
/// real primitive for. §5 makes every button a 999px pill, and a single
/// curved top arc on a pill reads as a rendering artifact rather than as a
/// lit edge -- so it is a full 1px border now, in the same tone. The border
/// is painted at 1px in *both* states (transparent when unselected) rather
/// than only in the selected branch, so toggling selection never shifts the
/// chip's size by the ring's own width -- `theme::TRANSPARENT`'s documented
/// "always-present, only visible in one branch" use case.
pub(crate) fn chip_button(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    active: bool,
) -> Stateful<Div> {
    div()
        .id(id)
        .flex()
        .items_center()
        .justify_center()
        .h(ButtonSize::Sm.height())
        .px(ButtonSize::Sm.h_padding())
        // Brand §5: a segmented preset chip is a button, so it wears the
        // same 999px pill every other button in the app does.
        .rounded(theme::RADIUS_PILL)
        .cursor_pointer()
        .text_size(ButtonSize::Sm.text_size())
        .border_1()
        .border_color(if active {
            rgba(theme::CONTROL_SELECTED_TOP_HIGHLIGHT)
        } else {
            rgba(theme::TRANSPARENT)
        })
        .text_color(if active {
            rgba(theme::TEXT_PRIMARY)
        } else {
            rgba(theme::TEXT_CONTROL_UNSELECTED)
        })
        .bg(if active {
            rgba(theme::CONTROL_SELECTED_FILL)
        } else {
            rgba(theme::TRANSPARENT)
        })
        .hover(move |s| {
            if active {
                s.bg(rgba(theme::CONTROL_SELECTED_FILL_HOVER))
            } else {
                s.bg(rgb(theme::SURFACE_OVERLAY))
            }
        })
        .child(label.into())
}

/// A real on/off switch, sitting beside a *separate* segmented delay
/// control (`settings.rs::render_autoplay_section`) instead of a
/// `chip_button` doubling as the on/off flip next to delay presets -- the
/// two used to both render as active, reading as two active segments in
/// one control. 36x20 track, 16px knob, `theme::TOGGLE_TRACK_OFF` (cream
/// 10%) or `_ON` (solid accent green) with a solid cream knob sliding to
/// the track's right edge when on.
/// Caller attaches `.on_click(...)` -- presentation only, no state of its own.
pub(crate) fn toggle_switch(id: impl Into<ElementId>, on: bool) -> Stateful<Div> {
    let track_w = px(36.);
    let track_h = px(20.);
    let knob = px(16.);
    let pad = px(2.);
    div()
        .id(id)
        .relative()
        .flex_shrink_0()
        .w(track_w)
        .h(track_h)
        .rounded_full()
        .cursor_pointer()
        .bg(if on {
            rgba(theme::TOGGLE_TRACK_ON)
        } else {
            rgba(theme::TOGGLE_TRACK_OFF)
        })
        .child(
            div()
                .absolute()
                .top(pad)
                .left(if on { track_w - knob - pad } else { pad })
                .w(knob)
                .h(knob)
                .rounded_full()
                .bg(rgb(theme::TOGGLE_KNOB)),
        )
}

/// A single keyboard-shortcut key/chord chip (Settings' Shortcuts section).
/// Brand §3 puts keyboard shortcuts in Martian Mono explicitly, inside a
/// small rounded/bordered chip: same visual vocabulary as the sidebar's
/// plain-text `⌘N` hint but boxed, since a scannable *list* of shortcuts
/// needs each key to read as a distinct token. Purely presentational -- no
/// `id`, no click handler; this reference is static content, not a
/// rebinding UI.
pub(crate) fn keycap(label: impl Into<SharedString>) -> impl IntoElement {
    div()
        .font_family(theme::FONT_MONO)
        .text_size(theme::TEXT_SPEC)
        .text_color(rgba(theme::TEXT_SECONDARY))
        .px_1p5()
        .py_0p5()
        .rounded(px(4.))
        .bg(rgb(theme::SURFACE_RAISED))
        .border_1()
        .border_color(rgb(theme::SURFACE_HAIRLINE))
        .child(label.into())
}

/// A row of one or more [`keycap`] chips -- most shortcuts are a single key,
/// but a few are a chord (`⇧` + `S`) or have two independent bindings for the
/// same action (`⌘M` and `Tab` both toggle the miniplayer). Small gap, right
/// edge is where the caller (`form_row`'s control slot) trailing-aligns it.
pub(crate) fn keycap_row(labels: impl IntoIterator<Item = SharedString>) -> impl IntoElement {
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap_1()
        .children(labels.into_iter().map(keycap))
}

/// Part B §13's toast shell: `surface.panel` bg, `radius.panel`, `shadow
/// E2`. Fade in/out is the caller's job (`with_animation`, Part B §6's
/// `motion.fade`) since that needs a stable per-toast element id the caller
/// already has (the toast's own generation/timestamp).
pub(crate) fn toast_shell(content: impl IntoElement) -> Div {
    div()
        .px_4()
        .py_2()
        .rounded_lg()
        .bg(rgb(theme::SURFACE_PANEL))
        .shadow(theme::shadow_e2())
        .text_color(rgba(theme::TEXT_PRIMARY))
        .child(content)
}

/// Brand §5's empty/error state: Archivo, factual, one line, no
/// illustration and no jokes. e.g. `No items in this library.` /
/// `Server unreachable — check the Jellyfin URL in Settings.` The 32px
/// centred icon Part B §13 called for is gone -- an icon above one factual
/// sentence is exactly the decoration §6 rules out. `icon_path` is kept in
/// the signature (ignored) so call sites don't each need editing.
pub(crate) fn empty_state(
    _icon_path: &'static str,
    message: impl Into<SharedString>,
) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .py_6()
        .child(
            div()
                .text_size(theme::TEXT_BODY)
                .text_color(rgba(theme::TEXT_TERTIARY))
                .child(message.into()),
        )
}

/// The same factual empty state, with a tier-2 UI-state mascot above it --
/// DESIGN-GUIDE.md §A.7, only for the specific screens it names (never
/// over posters, backdrops or video). `asset` is one `jb_mascot_*` pose, at
/// 96px, aspect kept, no tint.
pub(crate) fn empty_state_mascot(
    asset: &'static str,
    message: impl Into<SharedString>,
) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .py_6()
        .gap_3()
        .child(crate::root::mascot_image(asset, px(96.)))
        .child(
            div()
                .text_size(theme::TEXT_BODY)
                .text_color(rgba(theme::TEXT_TERTIARY))
                .child(message.into()),
        )
}

// ---------------------------------------------------------------------
// B.12 Dialog / sheet shell
// ---------------------------------------------------------------------

/// Part B §12's scrim: `theme::SCRIM` (NOTTE at 55%), full-viewport, centers its
/// child. `on_dismiss`: `None` for a destructive confirm dialog (must
/// explicitly Cancel/Confirm, no click-away close); `Some` for a
/// non-destructive sheet like Settings.
pub(crate) fn dialog_scrim(
    id: impl Into<ElementId>,
    on_dismiss: Option<impl Fn(&mut gpui::App) + 'static>,
    panel: impl IntoElement,
) -> Stateful<Div> {
    let mut el = div()
        .id(id)
        .absolute()
        .inset_0()
        .flex()
        .items_center()
        .justify_center()
        .bg(rgba(theme::SCRIM))
        // GPUI delivers scroll-wheel events to every element under the
        // pointer in reverse paint order, not just the strict ancestor
        // chain -- without this, wheel input over the dialog also scrolls
        // whatever scrollable sits beneath it. The panel's own scroll
        // containers paint above this scrim and handle their scroll first.
        .on_scroll_wheel(|_event, _window, cx| cx.stop_propagation());
    if let Some(on_dismiss) = on_dismiss {
        el = el.on_click(move |_event, _window, cx| on_dismiss(cx));
    }
    el.child(panel)
}

/// Part B §12's panel: `surface.panel` bg, `radius.sheet` (12px), `shadow
/// E3`, no border (shadow alone defines the edge). Stops its own click from
/// bubbling to the scrim behind it.
pub(crate) fn dialog_panel(
    id: impl Into<ElementId>,
    width: Pixels,
    content: impl IntoElement,
) -> Stateful<Div> {
    div()
        .id(id)
        .w(width)
        .flex()
        .flex_col()
        .rounded_xl()
        // Clip children to the panel: without this, a form row whose
        // control cluster refuses to shrink paints straight past the
        // panel's right edge onto the transparent window area beyond.
        .overflow_hidden()
        .bg(rgb(theme::SURFACE_PANEL))
        .shadow(theme::shadow_e3())
        .on_click(|_event, _window, cx| {
            cx.stop_propagation();
        })
        .child(content)
}

/// The fade band's width at either edge of a scrolling strip.
pub(crate) const STRIP_EDGE_FADE: f32 = 48.0;

/// The `#detail-scroll` page background the strips sit on. The backdrop's
/// own (c) vertical scrim is already at or near fully-opaque `SURFACE_BASE`
/// by the time the page reaches the season tabs / cast / similar rails
/// (`backdrop.rs`: opaque from 45% of the viewport height upward from the
/// bottom), so fading into `SURFACE_BASE` is fading into what's actually
/// painted there rather than into an invented color.
pub(crate) fn strip_fade_color() -> u32 {
    theme::SURFACE_BASE
}

/// A "gradient fade mask at the overflowing edge", built the only way this
/// gpui version allows (no `mask-image`): a 2-stop gradient band painted
/// over the strip's own edge, transparent on the content side and the page
/// color on the outside.
///
/// `ScrollHandle::max_offset()`/`offset()` report the *previous* frame's
/// layout, the only overflow signal available here. Before a strip has
/// ever been laid out the fade is shown unconditionally -- an unnecessary
/// fade for one frame is cheaper than a strip silently running off the
/// edge with no fade at all.
pub(crate) fn edge_fade_overlays(handle: &ScrollHandle) -> Vec<AnyElement> {
    let laid_out = handle.bounds().size.width > px(0.);
    let max_x = handle.max_offset().width;
    let scrolled = -handle.offset().x;
    let overflows = !laid_out || max_x > px(0.5);
    let mut fades: Vec<AnyElement> = Vec::new();
    if scrolled > px(0.5) {
        fades.push(
            div()
                .absolute()
                .top_0()
                .bottom_0()
                .left_0()
                .w(px(STRIP_EDGE_FADE))
                .bg(linear_gradient(
                    90.,
                    linear_color_stop(rgb(strip_fade_color()), 0.0),
                    linear_color_stop(rgba(theme::TRANSPARENT), 1.0),
                ))
                .into_any_element(),
        );
    }
    if overflows && (!laid_out || scrolled < max_x - px(0.5)) {
        fades.push(
            div()
                .absolute()
                .top_0()
                .bottom_0()
                .right_0()
                .w(px(STRIP_EDGE_FADE))
                .bg(linear_gradient(
                    90.,
                    linear_color_stop(rgba(theme::TRANSPARENT), 0.0),
                    linear_color_stop(rgb(strip_fade_color()), 1.0),
                ))
                .into_any_element(),
        );
    }
    fades
}

/// Wraps a horizontally scrolling strip in the relative container its edge
/// fades need. The fade bands carry no listeners, so they don't intercept
/// clicks on the tabs/cards underneath (gpui only hit-tests elements that
/// registered handlers).
/// Takes the scroll container as a concrete `Stateful<Div>` (not
/// `impl IntoElement`) because this wrapper also owns the focus-ring
/// clip-slack: `theme::FOCUS_RING_CLEARANCE` of padding must go INSIDE the
/// overflow container (that's what actually widens its clip box so a
/// focused card's ring isn't cut off) with the equal-and-opposite negative
/// margin OUT here on the wrapper, keeping every card exactly where it was
/// on screen. The fade overlays anchor to this wrapper's (now wider) box,
/// so the clearance zone at each end sits under the most opaque part of the
/// fade whenever content is actually scrolled past it -- a scrolled-off
/// card can't leak a bare sliver through the slack.
pub(crate) fn edge_faded_strip(strip: Stateful<Div>, handle: &ScrollHandle) -> Div {
    div()
        .relative()
        .flex()
        .flex_col()
        .mx(-theme::FOCUS_RING_CLEARANCE)
        .my(-theme::FOCUS_RING_CLEARANCE)
        .child(
            strip
                .px(theme::FOCUS_RING_CLEARANCE)
                .py(theme::FOCUS_RING_CLEARANCE),
        )
        .children(edge_fade_overlays(handle))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `ButtonSize`'s three heights must stay in the order Part B §8
    /// specifies (sm < md < lg) -- a cheap regression guard against a future
    /// edit accidentally flattening/reversing the scale.
    #[test]
    fn button_sizes_strictly_increase() {
        assert!(ButtonSize::Sm.height() < ButtonSize::Md.height());
        assert!(ButtonSize::Md.height() < ButtonSize::Lg.height());
        assert!(ButtonSize::Sm.h_padding() < ButtonSize::Md.h_padding());
        assert!(ButtonSize::Md.h_padding() < ButtonSize::Lg.h_padding());
    }

    /// Brand §5's Primary button, pinned: "PISTACCHIO fill with a #14100D
    /// label, Archivo 700 15px, padding 11×22". Height 44 = 11 + line + 11.
    #[test]
    fn the_primary_button_matches_the_brand_spec() {
        assert_eq!(theme::PRIMARY_BUTTON_BG, theme::PISTACCHIO);
        assert_eq!(theme::PRIMARY_BUTTON_TEXT, theme::NOTTE);
        assert_eq!(ButtonSize::Lg.text_size(), px(15.));
        assert_eq!(ButtonSize::Lg.h_padding(), px(22.));
        assert_eq!(ButtonSize::Lg.height(), px(44.));
    }
}
