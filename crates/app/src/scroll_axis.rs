//! Per-gesture scroll axis locking, so horizontal ribbon scrolling doesn't
//! get stolen by vertical page scroll ("shelf scroll axis fighting"). Pure
//! data, no GPUI dependency -- unit-testable the same way `focus_grid.rs` is.
//!
//! ## Why this exists at all
//!
//! A Home shelf (`home.rs::render_shelf`) is a horizontally-scrollable div
//! nested inside Home's own vertically-scrollable page. Reading gpui
//! 0.2.2's own `Div::paint_scroll_listener` (`elements/div.rs`): every
//! wheel/trackpad event is dispatched to *every* hitbox under the pointer
//! during the bubble phase, and neither the shelf's nor the page's built-in
//! scroll listener ever calls `stop_propagation`. A real trackpad swipe
//! almost never has a perfectly zero cross-axis component, so a genuinely
//! horizontal swipe over a shelf (dominant `dx`, small residual `dy`) gets
//! consumed *twice*: the shelf correctly scrolls horizontally by `dx`, and
//! the page **also** nudges vertically by the leftover `dy`, every single
//! event -- perceived as the ribbon "fighting" the page for the gesture.
//! Separately, gpui's own scroll listener has a fallback for scroll
//! containers with only one scrollable axis (`overflow.x == Scroll`,
//! `overflow.y != Scroll`): if `delta.x` is exactly zero it reappropriates
//! `delta.y` as the horizontal delta, so a plain vertical mouse-wheel
//! (no horizontal component at all, unlike a trackpad) hovering a shelf
//! scrolls it sideways instead of letting the page scroll.
//!
//! `home.rs::render_shelf` wires an `on_scroll_wheel` handler that: (a)
//! feeds every event's raw `(dx, dy)` through one `AxisLock` per shelf to
//! decide which axis owns the whole gesture, (b) `cx.stop_propagation()`s
//! once locked horizontal (stopping the page's own listener, which runs
//! *after* the shelf's own bubble-phase handlers -- registered later in the
//! render tree, dispatched earlier since bubble iterates children-first),
//! and (c) actively un-does whatever gpui's own default listener just did
//! to the shelf's offset when locked vertical (that listener runs *before*
//! ours on the same element, so by the time we see the event it has already
//! mutated `ScrollHandle`'s offset once).
//!
//! ## Idle reset, not phase-based
//!
//! macOS trackpad gestures do carry a real `TouchPhase` (`Started`/`Moved`/
//! `Ended`), but a plain mouse wheel's events are all `TouchPhase::Moved`
//! with no reliable start/end signal at all. Rather than depend on a signal
//! that's only sometimes present, `AxisLock` uses wall-clock idle detection
//! instead: any event arriving within
//! `GESTURE_IDLE_RESET` of the previous one is part of the same gesture and
//! reuses the locked axis; a longer gap re-evaluates from scratch. The
//! caller supplies `now` rather than this module reading the clock itself,
//! keeping the state machine pure and deterministic to unit-test.

use std::time::{Duration, Instant};

/// ~150ms of no new scroll events ends the current gesture -- long enough that consecutive events within
/// one continuous trackpad swipe (which fire far faster than this) never
/// spuriously reset, short enough that lifting the fingers and starting a
/// distinct new gesture a moment later gets a fresh axis decision rather
/// than inheriting a stale lock.
pub(crate) const GESTURE_IDLE_RESET: Duration = Duration::from_millis(150);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Axis {
    Horizontal,
    Vertical,
}

fn dominant_axis(dx: f32, dy: f32) -> Axis {
    if dx.abs() >= dy.abs() {
        Axis::Horizontal
    } else {
        Axis::Vertical
    }
}

/// One gesture's axis-lock state. Lives per scrollable surface (`home.rs`
/// keeps one per shelf, index-aligned with `shelf_scrolls`) since two
/// different shelves can be mid-gesture independently (unlikely with one
/// physical pointer, but nothing stops two `AxisLock`s from existing).
#[derive(Debug, Clone, Copy)]
pub(crate) struct AxisLock {
    locked: Option<Axis>,
    last_event: Option<Instant>,
}

impl AxisLock {
    pub(crate) fn new() -> Self {
        AxisLock {
            locked: None,
            last_event: None,
        }
    }

    /// Feed one wheel/scroll event's raw `(dx, dy)` at time `now`. Returns
    /// the axis this whole gesture is locked to -- the first event of a new
    /// gesture (either the very first ever, or one arriving
    /// `GESTURE_IDLE_RESET` or more after the previous event) picks the
    /// dominant axis by `|dx|` vs `|dy|` and locks it; every subsequent
    /// event within the idle window reuses that same lock regardless of
    /// its own (possibly noisy) per-event delta ratio.
    pub(crate) fn on_event(&mut self, dx: f32, dy: f32, now: Instant) -> Axis {
        let is_new_gesture = match self.last_event {
            None => true,
            Some(prev) => now.saturating_duration_since(prev) >= GESTURE_IDLE_RESET,
        };
        self.last_event = Some(now);
        if is_new_gesture {
            let axis = dominant_axis(dx, dy);
            self.locked = Some(axis);
            axis
        } else {
            // `locked` is always `Some` once `last_event` is `Some` (set
            // together on the previous call), so this default is never
            // actually reached -- kept as a safe fallback rather than an
            // `.unwrap()` so a future refactor can't turn a logic slip into
            // a panic on the render thread.
            self.locked.unwrap_or_else(|| dominant_axis(dx, dy))
        }
    }
}

impl Default for AxisLock {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_event_locks_to_its_own_dominant_axis() {
        let mut lock = AxisLock::new();
        let t0 = Instant::now();
        assert_eq!(lock.on_event(10.0, 1.0, t0), Axis::Horizontal);

        let mut lock2 = AxisLock::new();
        assert_eq!(lock2.on_event(1.0, 10.0, t0), Axis::Vertical);
    }

    #[test]
    fn subsequent_events_within_the_gesture_keep_the_lock_even_if_noisy() {
        let mut lock = AxisLock::new();
        let t0 = Instant::now();
        // Gesture starts clearly horizontal.
        assert_eq!(lock.on_event(20.0, 2.0, t0), Axis::Horizontal);
        // A later event within this same gesture happens to report a
        // vertical-dominant delta (real trackpad noise) -- the lock must
        // NOT flip mid-gesture.
        let t1 = t0 + Duration::from_millis(16);
        assert_eq!(lock.on_event(1.0, 5.0, t1), Axis::Horizontal);
        let t2 = t1 + Duration::from_millis(16);
        assert_eq!(lock.on_event(0.5, 8.0, t2), Axis::Horizontal);
    }

    #[test]
    fn idle_gap_resets_and_re_evaluates() {
        let mut lock = AxisLock::new();
        let t0 = Instant::now();
        assert_eq!(lock.on_event(20.0, 1.0, t0), Axis::Horizontal);

        // A new gesture, well past the idle window, dominant the other way.
        let t1 = t0 + GESTURE_IDLE_RESET + Duration::from_millis(1);
        assert_eq!(lock.on_event(1.0, 20.0, t1), Axis::Vertical);
    }

    #[test]
    fn exactly_at_the_idle_threshold_counts_as_a_new_gesture() {
        let mut lock = AxisLock::new();
        let t0 = Instant::now();
        lock.on_event(20.0, 1.0, t0);
        let t1 = t0 + GESTURE_IDLE_RESET;
        assert_eq!(lock.on_event(1.0, 20.0, t1), Axis::Vertical);
    }

    #[test]
    fn a_gesture_can_run_much_longer_than_the_idle_window_without_resetting() {
        let mut lock = AxisLock::new();
        let t0 = Instant::now();
        assert_eq!(lock.on_event(20.0, 1.0, t0), Axis::Horizontal);
        // 30 consecutive events, 16ms apart (~60Hz), well under the idle
        // reset threshold between any two consecutive ones, spanning almost
        // half a second in total -- the lock must hold the whole time.
        let mut t = t0;
        for _ in 0..30 {
            t += Duration::from_millis(16);
            assert_eq!(lock.on_event(1.0, 6.0, t), Axis::Horizontal);
        }
    }

    #[test]
    fn tie_prefers_horizontal() {
        let mut lock = AxisLock::new();
        assert_eq!(lock.on_event(5.0, 5.0, Instant::now()), Axis::Horizontal);
    }
}
