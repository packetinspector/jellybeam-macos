//! The native macOS menu bar.
//!
//! Jellybeam ran without one: no `Jellybeam` app menu, no `Window` menu, and --
//! because AppKit only wires Cmd+Q to a real `NSMenuItem` -- no working
//! Quit shortcut either. `install` builds both menus once, right after the
//! main window opens, out of gpui's `Menu`/`MenuItem` API (`cx.set_menus`).
//!
//! What is deliberately NOT here: a `Settings…` item. Settings is an in-app
//! sheet reached from the sidebar account popover, and the menu-bar copy
//! (with its `Cmd+,`) was removed at the user's request rather than kept as
//! a second door. The `About Jellybeam` item is the reverse case -- it is the
//! *only* way in, the account popover's old "About" row having been removed
//! as the redundant one.
//!
//! ## Actions, not closures
//!
//! Every item is a `MenuItem::action` over one of the zero-field actions
//! declared below, handled by an app-global `cx.on_action` listener. That
//! indirection buys shortcut glyphs (`cx.set_menus` looks the *action* up
//! in the app keymap, so bindings must be registered before the menus are
//! built -- hence the ordering inside `install`), AppKit enablement
//! (`on_validate_app_menu_command` -> `App::is_action_available`, true for
//! anything with a global listener), and one code path per behavior
//! (`About Jellybeam` calls the same singleton `Root::open_about_window` the
//! rest of the app would).
//!
//! ## Why these particular keys
//!
//! `Cmd+Q` (Quit) is the macOS standard. `Cmd+W` closes the *focused*
//! window -- in practice the About window, since closing the main window
//! would leave Jellybeam running with nothing on screen, so `CloseWindow`
//! deliberately no-ops unless the key window is an `about::AboutView`.
//! There is no Minimize item: `Cmd+M` is already the Miniplayer toggle
//! (`root.rs::handle_global_keystroke`), and a menu item would shadow it
//! with a contradictory meaning. `Window > Zoom` carries no shortcut,
//! matching AppKit's own default.
//!
//! Nothing here collides with `root.rs::handle_global_keystroke`: that
//! handler's `cmd` arm covers `F`, `[`, `]`, `1..9` and `M`, none of which
//! this module binds, and it's gated to the main window's id in `main.rs`
//! so About-window keystrokes never reach it at all.

use gpui::{App, Entity, KeyBinding, Menu, MenuItem};

use crate::about::AboutView;
use crate::root::Root;

gpui::actions!(jellybeam, [ShowAbout, Quit, CloseWindow, ZoomWindow]);

/// Installs the app-global action handlers, their key bindings, and the
/// menu bar itself. Call once, after the main window exists -- `set_menus`
/// needs the bindings to already be in the keymap (see the module doc), and
/// `About Jellybeam` opens a window off `root`.
pub(crate) fn install(root: Entity<Root>, cx: &mut App) {
    // ---- bindings first: `set_menus` reads them to draw the glyphs -----
    cx.bind_keys([
        KeyBinding::new("cmd-w", CloseWindow, None),
        KeyBinding::new("cmd-q", Quit, None),
    ]);

    // ---- handlers ------------------------------------------------------
    cx.on_action(move |_: &ShowAbout, cx| {
        // The singleton opener: a window that is already open is focused,
        // not duplicated.
        root.update(cx, |root, cx| root.open_about_window(cx));
    });
    cx.on_action(|_: &CloseWindow, cx| {
        // Focused-window close, scoped to the About window -- see the
        // module doc for why the main window deliberately ignores this.
        if let Some(active) = cx.active_window() {
            if active.downcast::<AboutView>().is_some() {
                let _ = active.update(cx, |_view, window, _cx| window.remove_window());
            }
        }
    });
    cx.on_action(|_: &ZoomWindow, cx| {
        if let Some(active) = cx.active_window() {
            let _ = active.update(cx, |_view, window, _cx| window.zoom_window());
        }
    });
    cx.on_action(|_: &Quit, cx| {
        // `main.rs`'s `cx.on_app_quit` hook is what actually stops mpv and
        // files the final playback report; this just starts the shutdown.
        cx.quit();
    });

    // ---- the menu bar ---------------------------------------------------
    cx.set_menus(vec![
        Menu {
            name: "Jellybeam".into(),
            items: vec![
                MenuItem::action("About Jellybeam", ShowAbout),
                MenuItem::separator(),
                MenuItem::action("Quit Jellybeam", Quit),
            ],
        },
        Menu {
            name: "Window".into(),
            items: vec![
                MenuItem::action("Close Window", CloseWindow),
                MenuItem::action("Zoom", ZoomWindow),
            ],
        },
    ]);
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{Action, Keystroke};

    /// Every keystroke this module binds must parse, and none of them may
    /// collide with the `cmd`-modified keys `root.rs::
    /// handle_global_keystroke` already claims (`F`, `[`, `]`, `1..9`,
    /// `M`) -- a collision there would fire both the action and the
    /// keystroke handler for one press.
    #[test]
    fn the_bound_keys_parse_and_avoid_the_existing_global_handlers() {
        let claimed_by_root = [
            "f", "[", "]", "m", "1", "2", "3", "4", "5", "6", "7", "8", "9",
        ];
        for source in ["cmd-w", "cmd-q"] {
            let keystroke = Keystroke::parse(source)
                .unwrap_or_else(|err| panic!("{source} does not parse: {err:?}"));
            assert!(
                keystroke.modifiers.platform,
                "{source} lost its Cmd modifier"
            );
            assert!(
                !claimed_by_root.contains(&keystroke.key.as_str()),
                "{source} collides with root.rs::handle_global_keystroke"
            );
        }
    }

    /// The action names are what `cx.set_menus` matches bindings against,
    /// so a rename here silently drops a menu item's shortcut glyph. Pin
    /// the namespace and the five names.
    #[test]
    fn the_actions_keep_their_wire_names() {
        assert_eq!(ShowAbout.name(), "jellybeam::ShowAbout");
        assert_eq!(CloseWindow.name(), "jellybeam::CloseWindow");
        assert_eq!(ZoomWindow.name(), "jellybeam::ZoomWindow");
        assert_eq!(Quit.name(), "jellybeam::Quit");
    }
}
