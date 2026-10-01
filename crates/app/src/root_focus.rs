//! Focus/hover navigation: arrow-key focus movement and mouse-hover
//! retargeting across every browse screen, plus Return/click activation
//! (`root.rs`'s former "Focus movement / activation" section, moved here
//! since `focus_grid.rs` itself stays GPUI-free/pure-data -- see its own
//! doc comment -- and these methods need `Context<Root>`/`MainState`.)

use gpui::Context;

use crate::focus_grid::Direction;
use crate::nav::View;
use crate::root::{detail_activate_action, Activate, Root, Screen};
use crate::settings::LibraryViewMode;

impl Root {
    // ---- Focus movement / activation --------------------------------

    pub(crate) fn move_focus(&mut self, dir: Direction, cx: &mut Context<Self>) {
        let mut pending_season_change: Option<usize> = None;
        {
            let Some(state) = self.main_state_mut() else {
                return;
            };
            match state.nav.current.clone() {
                View::Home => {
                    // First arrow key engages the focus ring (see
                    // `HomeState::focus_engaged`) *and* moves -- the ring
                    // simply appears wherever this press lands, rather than
                    // a swallowed reveal-only press.
                    state.home.focus_engaged = true;
                    let lens = state.home.shelf_lens();
                    match dir {
                        Direction::Left => state.home.focus.left(&lens),
                        Direction::Right => state.home.focus.right(&lens),
                        Direction::Up => state.home.focus.up(&lens),
                        Direction::Down => state.home.focus.down(&lens),
                    }
                    // Keep the focused cell fully visible --
                    // both cross-shelf (Up/Down) and within-shelf (Left/
                    // Right) -- see `HomeState::scroll_to_focus`'s doc
                    // comment.
                    state.home.scroll_to_focus();
                }
                View::Library { .. } => {
                    if let Some(lib) = &mut state.library {
                        // Same first-interaction latch as Home above.
                        lib.focus_engaged = true;
                        let n = lib.items.len();
                        match dir {
                            Direction::Left => lib.focus.left(n),
                            Direction::Right => lib.focus.right(n),
                            Direction::Up => lib.focus.up(n),
                            Direction::Down => lib.focus.down(n),
                        }
                        // Item 4: keyboard navigation always takes the
                        // highlight back, regardless of where the mouse was
                        // last resting.
                        lib.highlight.keyboard_select(lib.focus.index);
                        // Each projection tracks its own handle (see
                        // `LibraryState::list_scroll`), so keep-focus-visible
                        // has to address the one actually on screen. In list
                        // mode `focus.row()` is the item index, since
                        // `render_main` pinned `focus.columns` to 1.
                        match lib.view_mode {
                            LibraryViewMode::Grid => lib.scroll.scroll_to_row(lib.focus.row()),
                            LibraryViewMode::List => lib.list_scroll.scroll_to_row(lib.focus.row()),
                        }
                    }
                }
                View::Detail { .. } => {
                    if let Some(detail) = &mut state.detail {
                        pending_season_change = detail.move_focus(dir);
                        // Same "keep focus visible" treatment
                        // as Home/Library -- see `DetailState::
                        // scroll_target_index`'s doc comment.
                        detail.scroll.scroll_to_item(detail.scroll_target_index());
                    }
                }
                View::Channel { .. } => {
                    if let Some(cb) = &mut state.channel_browse {
                        // Same first-interaction latch as Home/Library above.
                        cb.focus_engaged = true;
                        let n = cb.items.len();
                        match dir {
                            Direction::Left => cb.focus.left(n),
                            Direction::Right => cb.focus.right(n),
                            Direction::Up => cb.focus.up(n),
                            Direction::Down => cb.focus.down(n),
                        }
                        cb.highlight.keyboard_select(cb.focus.index);
                        cb.scroll.scroll_to_row(cb.focus.row());
                    }
                }
                // Discover is mouse-first: no `GridFocus`/keyboard-nav wiring
                // for its cards -- see `discover::discover_card`'s doc
                // comment -- arrow keys are simply not bound there.
                View::Discover(_) => {}
            }
        }
        if let Some(ix) = pending_season_change {
            self.select_season(ix, cx);
        }
        cx.notify();
    }

    /// A real mouse-move landed over Library grid cell `index`
    /// (`grid.rs::poster_grid`'s `on_hover`
    /// callback, wired from gpui's `on_mouse_move` -- see
    /// `focus_grid::HighlightPolicy`'s doc comment for why it must be a
    /// real move event, not a bare hover-boolean transition). Mirrors
    /// `focus.index` so `activate_focus`/Return-to-play and everything else
    /// that already reads `lib.focus.index` keeps working unchanged -- the
    /// mouse and keyboard share one canonical focused index, just gated by
    /// `HighlightPolicy` on which input gets to move it.
    pub(crate) fn hover_library_cell(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(state) = self.main_state_mut() else {
            return;
        };
        let Some(lib) = &mut state.library else {
            return;
        };
        // A real hover is an interaction: engage the ring even when the
        // hovered cell is already the (invisibly) focused one, otherwise
        // hovering cell 0 on a fresh launch would highlight nothing.
        let newly_engaged = !lib.focus_engaged;
        lib.focus_engaged = true;
        if lib.highlight.mouse_hover(index) {
            lib.focus.index = index;
            cx.notify();
        } else if newly_engaged {
            cx.notify();
        }
    }

    /// Same "real mouse-move re-takes the highlight" treatment as
    /// `hover_library_cell`, for a channel browse row (`channel_browse.rs`'s
    /// `on_mouse_move` wiring).
    pub(crate) fn hover_channel_cell(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(state) = self.main_state_mut() else {
            return;
        };
        let Some(cb) = &mut state.channel_browse else {
            return;
        };
        let newly_engaged = !cb.focus_engaged;
        cb.focus_engaged = true;
        if cb.highlight.mouse_hover(index) {
            cb.focus.index = index;
            cx.notify();
        } else if newly_engaged {
            cx.notify();
        }
    }

    /// Mouse-move over a Home shelf card focuses it, so hovering Continue
    /// Watching / Next Up doesn't leave "no indicator" -- exactly like
    /// `hover_library_cell` does for the Library grid. Fired
    /// only from real `on_mouse_move` events (see `poster_card`'s
    /// `on_mouse_move` doc comment), so keyboard-driven scrolling under a
    /// stationary pointer can't fight it; dedup below keeps notify traffic
    /// to actual changes.
    pub(crate) fn hover_home_cell(&mut self, shelf: usize, column: usize, cx: &mut Context<Self>) {
        let Some(state) = self.main_state_mut() else {
            return;
        };
        // Same first-hover engagement rule as `hover_library_cell`.
        let newly_engaged = !state.home.focus_engaged;
        state.home.focus_engaged = true;
        let focus = &mut state.home.focus;
        if focus.shelf == shelf && focus.column == column {
            if newly_engaged {
                cx.notify();
            }
            return;
        }
        focus.shelf = shelf;
        focus.column = column;
        cx.notify();
    }

    pub(crate) fn activate_focus(&mut self, cx: &mut Context<Self>) {
        let action = {
            let Some(state) = self.main_state() else {
                return;
            };
            // Return with a not-yet-engaged (invisible) focus must not
            // silently open whatever cell 0 happens to be -- the user has
            // no highlight telling them what Return would target. Instead
            // the press engages the ring (revealing the target) and the
            // NEXT Return activates it. Detail pages are unaffected: their
            // activation target (the Play button / episode strip) is always
            // visibly evident.
            match &state.nav.current {
                View::Home if !state.home.focus_engaged => None,
                View::Home => state
                    .home
                    .focused_item()
                    .map(|c| Activate::OpenDetail(c.id)),
                View::Library { .. }
                    if state.library.as_ref().is_some_and(|l| !l.focus_engaged) =>
                {
                    None
                }
                View::Library { .. } => state
                    .library
                    .as_ref()
                    .and_then(|l| l.items.get(l.focus.index))
                    .map(|c| Activate::OpenDetail(c.id.clone())),
                View::Detail { .. } => state.detail.as_ref().and_then(detail_activate_action),
                View::Channel { .. }
                    if state
                        .channel_browse
                        .as_ref()
                        .is_some_and(|c| !c.focus_engaged) =>
                {
                    None
                }
                View::Channel { .. } => state
                    .channel_browse
                    .as_ref()
                    .and_then(crate::channel_browse::channel_activate_action)
                    .map(|a| match a {
                        crate::channel_browse::ChannelActivate::OpenFolder(id) => {
                            Activate::OpenChannelFolder(id)
                        }
                        crate::channel_browse::ChannelActivate::Play {
                            item_id,
                            item_name,
                            resume_ticks_hint,
                        } => Activate::PlayRecording(item_id, item_name, resume_ticks_hint),
                    }),
                // Discover is mouse-first (see `move_focus`'s matching
                // arm) -- there is no keyboard-engaged focus target for
                // Return to activate.
                View::Discover(_) => None,
            }
        };
        match action {
            Some(Activate::OpenDetail(id)) => self.open_detail(id, cx),
            Some(Activate::PlayItem(id, name)) => self.play_item(id, name, cx),
            Some(Activate::SwitchEpisode(id)) => self.open_episode_in_place(id, cx),
            Some(Activate::OpenChannelFolder(id)) => self.open_channel_folder(id, cx),
            Some(Activate::PlayRecording(id, name, hint)) => {
                self.play_item_with_resume_hint(id, name, hint, cx)
            }
            None => {
                // The un-engaged Home/Library/Channel cases above: this
                // Return's job is to reveal the ring; the next one activates.
                if let Screen::Main(state) = &mut self.screen {
                    match &state.nav.current {
                        View::Home if !state.home.focus_engaged => {
                            state.home.focus_engaged = true;
                            cx.notify();
                        }
                        View::Library { .. } => {
                            if let Some(lib) = &mut state.library {
                                if !lib.focus_engaged {
                                    lib.focus_engaged = true;
                                    cx.notify();
                                }
                            }
                        }
                        View::Channel { .. } => {
                            if let Some(cb) = &mut state.channel_browse {
                                if !cb.focus_engaged {
                                    cb.focus_engaged = true;
                                    cx.notify();
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
    }
}
