//! The About window's motion vocabulary (see `about.rs`'s module doc for
//! the full brief): an entrance curve, its duration,
//! and a clock helper that turns elapsed time into curve progress.
//!
//! Generalizes `theme.rs::ease_emphasized`'s Newton-iteration Bezier solver
//! to arbitrary control points, one solver for arbitrary curves. Not folded
//! into `theme.rs` itself: these curves are scoped to the About window's
//! own brief, not the brand-wide tokens of `docs/DESIGN-GUIDE.md` Part A.
//!
//! Each animated value is a pure function of elapsed time from `about.rs`'s
//! own clock (`AboutView::opened_at`), not a `gpui::Animation` -- that type
//! keys its start time on the element's tree position, so a sibling
//! appearing or disappearing (a staggered entrance) resets it.

use std::time::Duration;

/// Solves `y` for a given `x` on the cubic Bezier through `(0,0)`,
/// `(x1,y1)`, `(x2,y2)`, `(1,1)` -- the same curve CSS `cubic-bezier()`
/// uses. `y1`/`y2` may fall outside `[0,1]` (an overshoot curve swings `y`
/// past 1.0), but `x1`/`x2` must stay within `[0,1]` for `x(t)` to remain
/// monotone.
///
/// Newton's method on the parametric form, mirroring
/// `theme::ease_emphasized`'s derivation; see
/// `matches_theme_ease_emphasized` for the cross-check.
pub(crate) fn cubic_bezier(x1: f32, y1: f32, x2: f32, y2: f32) -> impl Fn(f32) -> f32 {
    move |x: f32| {
        if x <= 0.0 {
            return 0.0;
        }
        if x >= 1.0 {
            return 1.0;
        }
        let sample_x = |t: f32| {
            let mt = 1.0 - t;
            3.0 * mt * mt * t * x1 + 3.0 * mt * t * t * x2 + t * t * t
        };
        let sample_y = |t: f32| {
            let mt = 1.0 - t;
            3.0 * mt * mt * t * y1 + 3.0 * mt * t * t * y2 + t * t * t
        };
        let dsample_x = |t: f32| {
            let mt = 1.0 - t;
            3.0 * mt * mt * x1 + 6.0 * mt * t * (x2 - x1) + 3.0 * t * t * (1.0 - x2)
        };
        let mut t = x;
        for _ in 0..8 {
            let dx = sample_x(t) - x;
            let d = dsample_x(t);
            if d.abs() < 1e-6 {
                break;
            }
            t = (t - dx / d).clamp(0.0, 1.0);
        }
        sample_y(t)
    }
}

/// The About window's entrance curve, `cubic-bezier(0.2, 0.8, 0.2, 1)`:
/// panel fade/rise/scale-in and the staggered content reveal that follows
/// it.
pub(crate) fn entrance_ease() -> impl Fn(f32) -> f32 {
    cubic_bezier(0.2, 0.8, 0.2, 1.0)
}
/// Panel entrance / one staggered content element's reveal, `entrance_ease`.
pub(crate) const ENTRANCE_MS: u64 = 500;
/// Linear `0..=1` progress of a one-shot of `duration_ms`, `elapsed` after
/// it started -- the raw `x` to feed one of the eases above. Clamped at
/// both ends: `elapsed` before the event returns `0.0`, long after returns `1.0`.
pub(crate) fn progress(elapsed: Duration, duration_ms: u64) -> f32 {
    if duration_ms == 0 {
        return 1.0;
    }
    (elapsed.as_secs_f32() * 1000.0 / duration_ms as f32).clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every curve is pinned at `(0,0)` and `(1,1)` regardless of the
    /// interior control points.
    #[test]
    fn endpoints_are_pinned_at_zero_and_one() {
        for (x1, y1, x2, y2) in [
            (0.2, 0.8, 0.2, 1.0),
            (0.3, 1.4, 0.5, 1.0),
            (0.4, 0.0, 0.7, 1.0),
        ] {
            let f = cubic_bezier(x1, y1, x2, y2);
            assert!((f(0.0) - 0.0).abs() < 1e-4);
            assert!((f(1.0) - 1.0).abs() < 1e-4);
        }
    }

    /// `entrance_ease` is monotone increasing and never overshoots past 1.0.
    #[test]
    fn entrance_ease_is_monotone_and_does_not_overshoot() {
        let f = entrance_ease();
        let mid = f(0.5);
        assert!(
            mid > 0.5 && mid < 1.0,
            "expected a mid-curve value in (0.5,1), got {mid}"
        );
        let mut prev = 0.0;
        for i in 1..=20 {
            let x = i as f32 / 20.0;
            let y = f(x);
            assert!(
                y >= prev - 1e-4,
                "entrance_ease must be monotone, dipped at x={x}"
            );
            assert!(
                y <= 1.0 + 1e-4,
                "entrance_ease must not overshoot 1.0, got {y} at x={x}"
            );
            prev = y;
        }
    }

    /// The generalized solver must reproduce `theme::ease_emphasized`'s
    /// own hand-rolled output for the same curve.
    #[test]
    fn matches_theme_ease_emphasized() {
        let generalized = cubic_bezier(0.2, 0.0, 0.0, 1.0);
        let original = crate::theme::ease_emphasized();
        for i in 0..=10 {
            let x = i as f32 / 10.0;
            let (a, b) = (generalized(x), original(x));
            assert!(
                (a - b).abs() < 1e-3,
                "at x={x}: generalized={a} original={b}"
            );
        }
    }

    /// Guards against a copy-paste swap between duration constants.
    #[test]
    fn durations_match_spec() {
        assert_eq!(ENTRANCE_MS, 500);
    }

    /// `progress` is pinned at both ends, linear between, and never panics
    /// on an out-of-range `elapsed`.
    #[test]
    fn progress_clamps_at_both_ends_and_is_linear_between() {
        assert_eq!(progress(Duration::ZERO, 500), 0.0);
        assert!((progress(Duration::from_millis(250), 500) - 0.5).abs() < 1e-4);
        assert_eq!(progress(Duration::from_millis(500), 500), 1.0);
        assert_eq!(progress(Duration::from_secs(60), 500), 1.0);
        assert_eq!(progress(Duration::from_millis(10), 0), 1.0);
    }
}
