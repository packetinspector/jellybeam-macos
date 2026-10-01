//! Search overlay (docs/UX-SPEC.md §1/§2: "⌘F or `/` summons; type-to-filter via
//! `Mirror::search` (<50ms budget); arrows + Return navigate; Esc dismiss").
//!
//! Deliberately has no dedicated `TextInput` entity: the overlay is modal
//! (nothing else can have keyboard focus while it's open), so query text
//! entry rides the same global keystroke observer `root.rs` already uses
//! for Space/Escape/arrows -- one keyboard-handling path for the whole app
//! instead of a focus-management dance just for this field.

use std::time::Instant;

use gpui::{div, prelude::*, px, rgb, rgba, Context, SharedString, WeakEntity};
use media_cache::{CardRow, Mirror};

use crate::root::{Root, Screen};
use crate::theme;
use crate::ui::components::{clamped_line, empty_state_mascot, list_row};

pub(crate) const RESULT_LIMIT: u32 = 20;

pub(crate) struct SearchState {
    pub open: bool,
    pub query: String,
    pub results: Vec<CardRow>,
    pub selected: usize,
    /// Set by `run_query` around each `Mirror::search` call -- surfaced by
    /// `JELLYBEAM_PERF` (budget: keystroke → results < 50ms, docs/OVERVIEW.md §5b).
    pub last_query_micros: Option<u128>,
}

impl SearchState {
    pub(crate) fn new() -> Self {
        SearchState {
            open: false,
            query: String::new(),
            results: Vec::new(),
            selected: 0,
            last_query_micros: None,
        }
    }

    pub(crate) fn open(&mut self) {
        self.open = true;
        self.query.clear();
        self.results.clear();
        self.selected = 0;
    }

    pub(crate) fn close(&mut self) {
        self.open = false;
    }

    pub(crate) fn run_query(&mut self, mirror: &Mirror) {
        let start = Instant::now();
        self.results = if self.query.trim().is_empty() {
            Vec::new()
        } else {
            mirror.search(&self.query, RESULT_LIMIT)
        };
        self.last_query_micros = Some(start.elapsed().as_micros());
        self.selected = 0;
    }

    pub(crate) fn push_char(&mut self, mirror: &Mirror, ch: &str) {
        self.query.push_str(ch);
        self.run_query(mirror);
    }

    pub(crate) fn backspace(&mut self, mirror: &Mirror) {
        self.query.pop();
        self.run_query(mirror);
    }

    pub(crate) fn move_selection(&mut self, delta: i32) {
        if self.results.is_empty() {
            return;
        }
        let len = self.results.len() as i32;
        let next = (self.selected as i32 + delta).clamp(0, len - 1);
        self.selected = next as usize;
    }

    pub(crate) fn selected_item(&self) -> Option<&CardRow> {
        self.results.get(self.selected)
    }
}

pub(crate) fn render(
    state: &SearchState,
    root: WeakEntity<Root>,
    _cx: &mut Context<Root>,
) -> impl IntoElement {
    let query_display = if state.query.is_empty() {
        SharedString::from("Search movies and shows...")
    } else {
        SharedString::from(state.query.clone())
    };
    let query_color = if state.query.is_empty() {
        rgba(theme::TEXT_QUATERNARY)
    } else {
        rgba(theme::TEXT_PRIMARY)
    };

    let rows = state
        .results
        .iter()
        .enumerate()
        .map(|(ix, item)| {
            let selected = ix == state.selected;
            let item_id = item.id.clone();
            let root = root.clone();
            let year = item
                .production_year
                .map(|y| format!("  ({y})"))
                .unwrap_or_default();
            list_row(
                SharedString::from(format!("result-{}", item.id)),
                selected,
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .gap_3()
                    .w_full()
                    .text_color(if selected {
                        rgba(theme::TEXT_PRIMARY)
                    } else {
                        rgba(theme::TEXT_SECONDARY)
                    })
                    .child(
                        // A long title must truncate with
                        // a real ellipsis instead of overflowing past the
                        // item-type label at the row's trailing edge.
                        // Strip wrapping quotes at display time,
                        // same as every other title render site.
                        clamped_line(
                            format!("{}{}", crate::cards::display_title(&item.name), year),
                            px(20.),
                        )
                        .flex_1()
                        .min_w_0(),
                    )
                    .child(
                        div()
                            .flex_shrink_0()
                            .text_color(rgba(theme::TEXT_TERTIARY))
                            .text_sm()
                            .child(SharedString::from(item.item_type.clone())),
                    ),
            )
            .cursor_pointer()
            .on_click(move |_event, _window, cx| {
                let _ = root.update(cx, |root, cx| root.search_open_item(item_id.clone(), cx));
            })
        })
        .collect::<Vec<_>>();

    // DESIGN-GUIDE.md §A.7's search-no-results pose above the factual
    // empty line -- replaces the previous bare "No matches" text-only line.
    let empty_hint = (!state.query.is_empty() && state.results.is_empty()).then(|| {
        empty_state_mascot(
            "brand/jellybeam/jb_mascot_searching.png",
            "No items match that search.",
        )
    });

    div()
        .id("search-overlay")
        .absolute()
        .inset_0()
        .flex()
        .items_start()
        .justify_center()
        .pt(px(120.))
        // Part C §8: aligned to the same scrim value Settings' dialog scrim
        // uses (55% black, down from 67%) -- Search is functionally a modal
        // too.
        .bg(rgba(theme::SCRIM))
        .child(
            div()
                .w(px(640.))
                .max_h(px(520.))
                .flex()
                .flex_col()
                // Part C §8/B.12: `radius.sheet` (12px, was `rounded_lg`/8px),
                // `surface.panel` (one level up from `surface.raised`), and
                // `shadow E3` standing in for the panel's edge entirely --
                // no accent border (B.12: "shadow alone defines the
                // edge"), same recipe as `ui::components::dialog_panel`.
                .rounded_xl()
                .bg(rgb(theme::SURFACE_PANEL))
                .shadow(theme::shadow_e3())
                .child(
                    div()
                        .px_4()
                        .py_3()
                        .border_b_1()
                        .border_color(rgb(theme::SURFACE_OVERLAY))
                        .text_lg()
                        .text_color(query_color)
                        .child(query_display),
                )
                .child(
                    div()
                        .id("search-results")
                        .flex()
                        .flex_col()
                        .p_2()
                        .overflow_y_scroll()
                        .children(rows)
                        .children(empty_hint),
                ),
        )
}

// ---- Search overlay control flow (moved from root.rs) ------------------

impl Root {
    pub(crate) fn search_open_item(&mut self, item_id: String, cx: &mut Context<Self>) {
        if let Screen::Main(state) = &mut self.screen {
            state.search.close();
        }
        self.open_detail(item_id, cx);
    }

    pub(crate) fn open_search(&mut self, cx: &mut Context<Self>) {
        self.collapse_fullscreen_player_for_nav(cx);
        let Some(state) = self.main_state_mut() else {
            return;
        };
        if !state.search.open {
            state.search.open();
            cx.notify();
        }
    }

    fn close_search(&mut self, cx: &mut Context<Self>) {
        let Some(state) = self.main_state_mut() else {
            return;
        };
        state.search.close();
        cx.notify();
    }

    fn search_move(&mut self, delta: i32, cx: &mut Context<Self>) {
        let Some(state) = self.main_state_mut() else {
            return;
        };
        state.search.move_selection(delta);
        cx.notify();
    }

    fn search_type_char(&mut self, ch: &str, cx: &mut Context<Self>) {
        let Some(state) = self.main_state_mut() else {
            return;
        };
        let mirror = state.mirror.clone();
        state.search.push_char(&mirror, ch);
        cx.notify();
    }

    fn search_backspace(&mut self, cx: &mut Context<Self>) {
        let Some(state) = self.main_state_mut() else {
            return;
        };
        let mirror = state.mirror.clone();
        state.search.backspace(&mirror);
        cx.notify();
    }

    fn search_activate(&mut self, cx: &mut Context<Self>) {
        let selected = if let Screen::Main(state) = &self.screen {
            state.search.selected_item().cloned()
        } else {
            None
        };
        if let Some(item) = selected {
            self.search_open_item(item.id, cx);
        }
    }
    pub(crate) fn handle_search_keystroke(
        &mut self,
        key: &str,
        key_char: Option<&str>,
        other_mods: bool,
        cx: &mut Context<Self>,
    ) {
        match key {
            "escape" => self.close_search(cx),
            "up" => self.search_move(-1, cx),
            "down" => self.search_move(1, cx),
            "enter" | "return" => self.search_activate(cx),
            "backspace" => self.search_backspace(cx),
            _ => {
                if !other_mods {
                    if let Some(ch) = key_char {
                        if !ch.is_empty() && ch.chars().all(|c| !c.is_control()) {
                            self.search_type_char(ch, cx);
                        }
                    }
                }
            }
        }
    }
}
