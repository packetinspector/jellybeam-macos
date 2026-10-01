//! The "?" keyboard-shortcuts overlay -- a `shift + /` toggled reference
//! card, purely informational (no pause, no layer collapse, no navigation)
//! so it can be summoned over Browse, a playing Miniplayer, or full
//! Fullscreen-in-window/OS-Fullscreen playback without disturbing any of
//! them. Toggled and guarded in `root.rs::handle_global_keystroke` (see that
//! function's own doc comment on the open/close precedence and the
//! text-input guard); this module owns only the shortcut table itself and
//! how it's painted.
//!
//! There is already an accurate, hand-audited shortcuts reference in the
//! Settings sheet (`settings.rs::render_shortcuts_section`) -- this overlay
//! is a second, faster-to-reach presentation of the same facts, grouped by
//! Global/Browse/Player (matching how `root.rs`'s three keystroke handlers
//! partition live bindings) and styled in this app's "spec strip" register
//! rather than Settings' form-row idiom. The two lists are independent and
//! can drift only if `root.rs`'s real bindings change without both being
//! updated by hand.

use gpui::{div, prelude::*, px, rgb, rgba, IntoElement, SharedString, WeakEntity};

use crate::root::Root;
use crate::theme;
use crate::ui::components::{dialog_scrim, keycap_row};

/// One row: the physical key(s) (rendered as `FONT_MONO` uppercase key caps
/// via `keycap_row`) plus what they do, in `FONT_UI` (the app's cascaded
/// default -- see `Root::render`'s doc comment on why body text needs no
/// explicit `font_family` call).
pub(crate) struct ShortcutEntry {
    pub keys: &'static [&'static str],
    pub description: &'static str,
}

pub(crate) struct ShortcutGroup {
    pub title: &'static str,
    pub entries: &'static [ShortcutEntry],
}

/// **Keep this in sync with `root.rs`.** Every row below must correspond to
/// an actual arm in `Root::handle_global_keystroke` or
/// `Root::handle_playback_keystroke` -- there is no compiler check, code
/// generation, or introspection linking this table to those `match` blocks.
/// Re-verify against those two functions by hand before touching either side.
///
/// Three groups, matching where each binding is actually live: **Global**
/// fires unconditionally at the top of `handle_global_keystroke`; **Browse**
/// is that function's final `match`, live whenever the player isn't in
/// Fullscreen-in-window/OS-Fullscreen; **Player** is
/// `handle_playback_keystroke`, live only while Fullscreen owns the
/// keyboard. `Tab` legitimately appears in both Browse and Player: same
/// Miniplayer toggle, opposite direction depending on which state you're
/// already in.
pub(crate) const SHORTCUT_GROUPS: &[ShortcutGroup] = &[
    ShortcutGroup {
        title: "Global",
        entries: &[
            ShortcutEntry {
                keys: &["?"],
                description: "Show / hide this help",
            },
            ShortcutEntry {
                keys: &["⌘F"],
                description: "Search",
            },
            ShortcutEntry {
                keys: &["⌘["],
                description: "Back",
            },
            ShortcutEntry {
                keys: &["⌘]"],
                description: "Forward",
            },
            ShortcutEntry {
                keys: &["⌘1–9"],
                description: "Jump to sidebar item",
            },
            ShortcutEntry {
                keys: &["⌘M"],
                description: "Toggle Miniplayer",
            },
            // Not a `handle_global_keystroke` case at all: this is a
            // `menu.rs` action, bound app-wide via `cx.bind_keys` and also
            // shown in the native Window menu. Listed under Global because
            // that is what it is from the user's side -- it fires whatever
            // has focus, including the About window.
            ShortcutEntry {
                keys: &["⌘W"],
                description: "Close the About window",
            },
        ],
    },
    ShortcutGroup {
        title: "Browse",
        entries: &[
            ShortcutEntry {
                keys: &["/"],
                description: "Search",
            },
            ShortcutEntry {
                keys: &["ESC"],
                description: "Back (stops playback if a Miniplayer is active)",
            },
            ShortcutEntry {
                keys: &["←", "→", "↑", "↓"],
                description: "Move focus",
            },
            ShortcutEntry {
                keys: &["RETURN"],
                description: "Open focused item",
            },
            ShortcutEntry {
                keys: &["[", "]"],
                description: "Previous / next episode (Episode page)",
            },
            ShortcutEntry {
                keys: &["TAB"],
                description: "Restore from Miniplayer",
            },
        ],
    },
    ShortcutGroup {
        title: "Player",
        entries: &[
            ShortcutEntry {
                keys: &["SPACE"],
                description: "Play / pause",
            },
            ShortcutEntry {
                keys: &["RIGHT ⌥"],
                description: "Hold for 2× speed",
            },
            ShortcutEntry {
                keys: &["LEFT ⌥"],
                description: "Hold for 0.5× speed",
            },
            ShortcutEntry {
                // The actual back/forward lengths are a per-user preference
                // (Settings -> Playback -> "Skip length"), which this fixed
                // `&'static str` const table can't interpolate; the
                // Settings sheet's own reference DOES show the live
                // configured values, since it's a plain function.
                keys: &["←", "→"],
                description: "Seek back / forward (configurable in Settings)",
            },
            ShortcutEntry {
                keys: &["⇧←", "⇧→"],
                description: "Seek −60s / +60s",
            },
            ShortcutEntry {
                keys: &["ESC"],
                description: "Dismiss next-up card, then step back a layer",
            },
            ShortcutEntry {
                keys: &["F"],
                description: "Toggle fullscreen",
            },
            ShortcutEntry {
                keys: &["TAB"],
                description: "Collapse to Miniplayer",
            },
            ShortcutEntry {
                keys: &["↑", "↓"],
                description: "Volume up / down",
            },
            ShortcutEntry {
                keys: &["M"],
                description: "Mute",
            },
            ShortcutEntry {
                keys: &["S"],
                description: "Cycle subtitle track",
            },
            ShortcutEntry {
                keys: &["⇧S"],
                description: "Open subtitle track picker",
            },
            ShortcutEntry {
                keys: &["A"],
                description: "Cycle audio track",
            },
            ShortcutEntry {
                keys: &["⇧A"],
                description: "Open audio track picker",
            },
            ShortcutEntry {
                keys: &["I"],
                description: "Toggle info overlay",
            },
            ShortcutEntry {
                keys: &["U"],
                description: "Undo last skip",
            },
            ShortcutEntry {
                keys: &["[", "]"],
                description: "Previous / next episode",
            },
            ShortcutEntry {
                keys: &["RETURN"],
                description: "Play next episode now",
            },
        ],
    },
];

/// A group header -- `FONT_MONO`, uppercase, the same "spec strip" register
/// brand §3 reserves for "spec strips, keyboard shortcuts, status labels".
/// Distinct from `ui/components.rs::dense_label` (used by Settings' own
/// Navigation/Playback headers), which is plain `FONT_UI` body text -- this
/// overlay leans further into the mono register throughout, per this
/// feature's own brand direction.
fn group_header(title: &'static str) -> impl IntoElement {
    div()
        .font_family(theme::FONT_MONO)
        .text_size(theme::TEXT_SPEC)
        .text_color(rgba(theme::TEXT_QUATERNARY))
        .child(title.to_uppercase())
}

/// One shortcut row: description on the left (flexible, truncates before it
/// pushes the key caps off the panel), key caps trailing-aligned on the
/// right -- same row grammar `ui/components.rs::form_row` uses.
fn shortcut_row(entry: &ShortcutEntry) -> impl IntoElement {
    div()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .gap_4()
        .min_h(px(30.))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .text_size(theme::TEXT_METADATA)
                .text_color(rgba(theme::TEXT_SECONDARY))
                .child(entry.description),
        )
        .child(keycap_row(
            entry.keys.iter().map(|k| SharedString::from(*k)),
        ))
}

/// The overlay itself: `dialog_scrim` (same dimmed-backdrop + click-to-
/// dismiss + scroll-containment component the Settings sheet uses) wrapping
/// a centered panel. Per this feature's own brand direction (distinct from
/// Settings' `SURFACE_PANEL`/no-border `dialog_panel`), the panel is built
/// by hand on `SURFACE_OVERLAY` with a `HAIRLINE` border rather than reusing
/// `dialog_panel` outright.
pub(crate) fn render(viewport_height: gpui::Pixels, root: WeakEntity<Root>) -> impl IntoElement {
    let dismiss_root = root.clone();
    let close_root = root;

    let body = div()
        .id("shortcuts-body")
        .max_h((viewport_height * 0.7).min(px(520.)))
        .overflow_y_scroll()
        .flex()
        .flex_col()
        .gap_1()
        .children(SHORTCUT_GROUPS.iter().map(|group| {
            div()
                .flex()
                .flex_col()
                .gap_1()
                .child(group_header(group.title))
                .children(group.entries.iter().map(shortcut_row))
        }));

    let panel_content = div()
        .flex()
        .flex_col()
        .gap_3()
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .justify_between()
                .child(
                    div()
                        .text_size(theme::TEXT_TITLE)
                        .text_color(rgba(theme::TEXT_PRIMARY))
                        .child("Keyboard Shortcuts"),
                )
                .child(
                    div()
                        .id("shortcuts-close")
                        .cursor_pointer()
                        .px_1()
                        .text_color(rgba(theme::TEXT_TERTIARY))
                        .on_click(move |_event, _window, cx| {
                            let _ =
                                close_root.update(cx, |root, cx| root.close_shortcuts_overlay(cx));
                        })
                        .child("✕"),
                ),
        )
        .child(body);

    // `dialog_panel` provides the shared width/rounding/click-containment
    // shell but paints `SURFACE_PANEL` with no border; this feature calls
    // for `SURFACE_OVERLAY` + a `HAIRLINE` border specifically, so the
    // panel chrome is applied here instead of delegating to it.
    let panel = div()
        .id("shortcuts-panel")
        .w(px(440.))
        .p_4()
        .rounded_xl()
        .bg(rgb(theme::SURFACE_OVERLAY))
        .border_1()
        .border_color(rgb(theme::HAIRLINE))
        .shadow(theme::shadow_e3())
        .on_click(|_event, _window, cx| {
            cx.stop_propagation();
        })
        .child(panel_content);

    dialog_scrim(
        "shortcuts-overlay",
        Some(move |cx: &mut gpui::App| {
            let _ = dismiss_root.update(cx, |root, cx| root.close_shortcuts_overlay(cx));
        }),
        panel,
    )
}
