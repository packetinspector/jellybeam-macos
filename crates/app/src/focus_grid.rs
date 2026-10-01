//! Keyboard-first focus arithmetic (docs/UX-SPEC.md §2: "Arrows: move focus,
//! grid-aware, remembers column"). Pure data, no GPUI dependency, so it's
//! unit-testable; shared by the uniform-column poster grid and Home's
//! heterogeneous shelf rows.

/// Arrow-key direction, shared by every view's focus-movement dispatch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Direction {
    Left,
    Right,
    Up,
    Down,
}

/// Focus over a single uniform-column grid (poster grid, episode strip).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct GridFocus {
    pub index: usize,
    pub columns: usize,
}

impl GridFocus {
    pub(crate) fn new(columns: usize) -> Self {
        GridFocus {
            index: 0,
            columns: columns.max(1),
        }
    }

    pub(crate) fn row(&self) -> usize {
        self.index / self.columns
    }

    /// Clamps `index` after item/column count changes so focus never
    /// points past the end.
    pub(crate) fn clamp(&mut self, item_count: usize) {
        if item_count == 0 {
            self.index = 0;
        } else if self.index >= item_count {
            self.index = item_count - 1;
        }
    }

    pub(crate) fn left(&mut self, item_count: usize) {
        if item_count == 0 {
            return;
        }
        if self.index > 0 {
            self.index -= 1;
        }
    }

    pub(crate) fn right(&mut self, item_count: usize) {
        if item_count == 0 {
            return;
        }
        if self.index + 1 < item_count {
            self.index += 1;
        }
    }

    /// Moves up one row, same column (`index -= columns` preserves
    /// `index % columns`).
    pub(crate) fn up(&mut self, item_count: usize) {
        if item_count == 0 {
            return;
        }
        if self.index >= self.columns {
            self.index -= self.columns;
        }
    }

    /// Moves down one row, same column; clamps to the last item if the
    /// target row is shorter.
    pub(crate) fn down(&mut self, item_count: usize) {
        if item_count == 0 {
            return;
        }
        let target = self.index + self.columns;
        self.index = target.min(item_count - 1);
    }
}

/// Focus over Home's stacked shelves, each a row of a different length.
/// Remembers the *desired* column across row changes (docs/UX-SPEC.md §2),
/// clamped against each row's actual length.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ShelfFocus {
    pub shelf: usize,
    pub column: usize,
    /// Last column deliberately chosen (left/right); preserved across
    /// up/down even when the current row is shorter.
    desired_column: usize,
}

impl ShelfFocus {
    pub(crate) fn new() -> Self {
        ShelfFocus {
            shelf: 0,
            column: 0,
            desired_column: 0,
        }
    }

    /// `lens` returns each shelf's item count in order.
    pub(crate) fn clamp(&mut self, shelf_lens: &[usize]) {
        if shelf_lens.is_empty() {
            self.shelf = 0;
            self.column = 0;
            return;
        }
        if self.shelf >= shelf_lens.len() {
            self.shelf = shelf_lens.len() - 1;
        }
        // Skip empty shelves so focus lands on a real item if one exists.
        if shelf_lens[self.shelf] == 0 {
            if let Some(i) = shelf_lens.iter().position(|&n| n > 0) {
                self.shelf = i;
            }
        }
        let len = shelf_lens[self.shelf];
        self.column = self.desired_column.min(len.saturating_sub(1));
    }

    pub(crate) fn left(&mut self, shelf_lens: &[usize]) {
        if self.column > 0 {
            self.column -= 1;
            self.desired_column = self.column;
        }
        let _ = shelf_lens;
    }

    pub(crate) fn right(&mut self, shelf_lens: &[usize]) {
        if let Some(&len) = shelf_lens.get(self.shelf) {
            if self.column + 1 < len {
                self.column += 1;
                self.desired_column = self.column;
            }
        }
    }

    pub(crate) fn up(&mut self, shelf_lens: &[usize]) {
        if self.shelf > 0 {
            self.shelf -= 1;
            self.clamp(shelf_lens);
        }
    }

    pub(crate) fn down(&mut self, shelf_lens: &[usize]) {
        if self.shelf + 1 < shelf_lens.len() {
            self.shelf += 1;
            self.clamp(shelf_lens);
        }
    }
}

/// Last-input-wins arbiter for which single cell shows the focus ring.
/// `GridFocus`/`ShelfFocus` own the position data; this decides whether
/// the most recent keyboard move or real mouse position wins, so a
/// keyboard move and a mouse hover can never highlight two different
/// cells at once. Generic over a plain `usize` slot id; callers encode
/// `GridFocus`'s `index` or `ShelfFocus`'s `(shelf, column)` themselves.
///
/// Callers must wire `mouse_hover` from a real pointer-move event, never
/// a bare hover-boolean transition -- a hover boolean also flips when the
/// cell moves under a stationary pointer, which would steal the highlight
/// back from a keyboard move that just happened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InputSource {
    Keyboard,
    Mouse,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct HighlightPolicy {
    slot: usize,
    source: InputSource,
}

impl HighlightPolicy {
    pub(crate) fn new() -> Self {
        HighlightPolicy {
            slot: 0,
            source: InputSource::Keyboard,
        }
    }

    pub(crate) fn slot(&self) -> usize {
        self.slot
    }

    /// Unused by any call site today; kept for symmetry with `slot()` and
    /// for a future mouse- vs. keyboard-driven visual treatment.
    #[allow(dead_code)]
    pub(crate) fn source(&self) -> InputSource {
        self.source
    }

    /// Arrow-key navigation landed on `slot`; always takes over regardless
    /// of the mouse's last reported position.
    pub(crate) fn keyboard_select(&mut self, slot: usize) {
        self.slot = slot;
        self.source = InputSource::Keyboard;
    }

    /// A real mouse-move landed over `slot`; always takes over. Returns
    /// whether anything changed, so a caller can skip a redundant
    /// `cx.notify()` when the mouse is merely still resting on the cell.
    pub(crate) fn mouse_hover(&mut self, slot: usize) -> bool {
        if self.source == InputSource::Mouse && self.slot == slot {
            return false;
        }
        self.slot = slot;
        self.source = InputSource::Mouse;
        true
    }
}

impl Default for HighlightPolicy {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grid_up_down_remembers_column() {
        let mut f = GridFocus::new(5);
        f.index = 7; // row 1, col 2
        f.up(20);
        assert_eq!(f.index, 2); // row 0, col 2 preserved
        f.down(20);
        assert_eq!(f.index, 7);
    }

    #[test]
    fn grid_left_right_clamp_at_edges() {
        let mut f = GridFocus::new(3);
        f.left(10);
        assert_eq!(f.index, 0);
        f.index = 9;
        f.right(10);
        assert_eq!(f.index, 9);
    }

    #[test]
    fn grid_down_clamps_into_short_last_row() {
        let mut f = GridFocus::new(5);
        f.index = 3; // row 0 col 3
        f.down(7); // only 2 items in row 1 (indices 5,6)
        assert_eq!(f.index, 6);
    }

    #[test]
    fn shelf_focus_remembers_desired_column_across_shorter_rows() {
        let mut f = ShelfFocus::new();
        let lens = [10, 2, 10];
        f.right(&lens);
        f.right(&lens);
        f.right(&lens);
        assert_eq!(f.column, 3);
        f.down(&lens); // shelf 1 only has 2 items
        assert_eq!(f.shelf, 1);
        assert_eq!(f.column, 1); // clamped
        f.down(&lens); // shelf 2 has 10 -- desired column (3) restored
        assert_eq!(f.shelf, 2);
        assert_eq!(f.column, 3);
    }

    #[test]
    fn shelf_focus_clamp_skips_empty_shelves() {
        let mut f = ShelfFocus::new();
        let lens = [0, 0, 5];
        f.clamp(&lens);
        assert_eq!(f.shelf, 2);
    }

    // ---- HighlightPolicy ----

    #[test]
    fn highlight_policy_starts_on_slot_zero_keyboard_owned() {
        let p = HighlightPolicy::new();
        assert_eq!(p.slot(), 0);
        assert_eq!(p.source(), InputSource::Keyboard);
    }

    #[test]
    fn keyboard_select_always_overrides_mouse() {
        let mut p = HighlightPolicy::new();
        p.mouse_hover(5);
        assert_eq!(p.slot(), 5);
        assert_eq!(p.source(), InputSource::Mouse);

        p.keyboard_select(2);
        assert_eq!(p.slot(), 2);
        assert_eq!(p.source(), InputSource::Keyboard);
    }

    #[test]
    fn mouse_hover_overrides_keyboard() {
        let mut p = HighlightPolicy::new();
        p.keyboard_select(3);
        assert!(p.mouse_hover(7));
        assert_eq!(p.slot(), 7);
        assert_eq!(p.source(), InputSource::Mouse);
    }

    /// Pins: re-hovering the slot the mouse already owns is a no-op
    /// (returns `false`); keyboard selection always wins regardless.
    #[test]
    fn same_slot_mouse_hover_is_a_no_op() {
        let mut p = HighlightPolicy::new();
        p.mouse_hover(4);
        assert!(
            !p.mouse_hover(4),
            "re-hovering the same slot should be a no-op"
        );
        assert_eq!(p.slot(), 4);
        assert_eq!(p.source(), InputSource::Mouse);
    }

    #[test]
    fn one_highlight_at_a_time_across_a_mixed_input_sequence() {
        let mut p = HighlightPolicy::new();
        p.keyboard_select(0);
        p.keyboard_select(1);
        assert_eq!((p.slot(), p.source()), (1, InputSource::Keyboard));

        p.mouse_hover(9);
        assert_eq!((p.slot(), p.source()), (9, InputSource::Mouse));

        // Keyboard always wins immediately over a prior mouse hover.
        p.keyboard_select(2);
        assert_eq!((p.slot(), p.source()), (2, InputSource::Keyboard));

        p.mouse_hover(6);
        assert_eq!((p.slot(), p.source()), (6, InputSource::Mouse));
    }
}
