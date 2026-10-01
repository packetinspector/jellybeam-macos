//! Shared capped-exponential-with-jitter backoff, used by both
//! [`crate::event_bus::EventBus`]'s reconnect loop and
//! [`crate::reporting::ReportingSession`]'s report retries.

use std::time::Duration;

/// Doubles from `base` up to `cap`, ±25% jitter on every value returned so a
/// fleet of clients reconnecting after a server restart doesn't thunder-herd.
#[derive(Debug, Clone)]
pub(crate) struct Backoff {
    base: Duration,
    cap: Duration,
    attempt: u32,
}

impl Backoff {
    pub(crate) fn new(base: Duration, cap: Duration) -> Self {
        Self {
            base,
            cap,
            attempt: 0,
        }
    }

    /// Next delay, advancing internal state. Jitter uses `rand::thread_rng`;
    /// callers that need determinism should assert on bounds, not exact
    /// values (see tests below and in `event_bus.rs`).
    pub(crate) fn next_delay(&mut self) -> Duration {
        let unjittered = self.unjittered_delay_for(self.attempt);
        self.attempt = self.attempt.saturating_add(1);
        jitter(unjittered, self.cap)
    }

    fn unjittered_delay_for(&self, attempt: u32) -> Duration {
        let shift = attempt.min(20); // avoid overflow on 1u64 << shift
        let scaled = self.base.saturating_mul(1u32 << shift);
        scaled.min(self.cap)
    }

    /// Bounds a call to `next_delay()` would currently produce, without
    /// advancing state — used by tests to assert without flakiness.
    #[cfg(test)]
    pub(crate) fn current_bounds(&self) -> (Duration, Duration) {
        let base = self.unjittered_delay_for(self.attempt);
        jitter_bounds(base, self.cap)
    }

    pub(crate) fn reset(&mut self) {
        self.attempt = 0;
    }
}

fn jitter(delay: Duration, cap: Duration) -> Duration {
    use rand::Rng;
    let factor = rand::thread_rng().gen_range(0.75..=1.25);
    let jittered = delay.mul_f64(factor);
    jittered.min(cap)
}

#[cfg(test)]
fn jitter_bounds(delay: Duration, cap: Duration) -> (Duration, Duration) {
    (delay.mul_f64(0.75).min(cap), delay.mul_f64(1.25).min(cap))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starts_at_base_and_doubles_up_to_cap() {
        let mut b = Backoff::new(Duration::from_secs(1), Duration::from_secs(60));
        let (lo, hi) = b.current_bounds();
        assert!(lo <= Duration::from_secs(1) && hi >= Duration::from_millis(750));

        for _ in 0..10 {
            b.next_delay();
        }
        let (lo, hi) = b.current_bounds();
        assert!(lo <= Duration::from_secs(60));
        assert!(hi <= Duration::from_secs(60));
        // Should have saturated at the cap well before 10 doublings (1*2^10=1024s).
        assert!(lo >= Duration::from_secs(44)); // 60 * 0.75-ish lower bound
    }

    #[test]
    fn reset_returns_to_base() {
        let mut b = Backoff::new(Duration::from_millis(100), Duration::from_secs(10));
        for _ in 0..5 {
            b.next_delay();
        }
        b.reset();
        let (lo, hi) = b.current_bounds();
        assert!(lo <= Duration::from_millis(100));
        assert!(hi <= Duration::from_millis(130));
    }

    #[test]
    fn never_exceeds_cap() {
        let mut b = Backoff::new(Duration::from_secs(30), Duration::from_secs(60));
        for _ in 0..5 {
            let d = b.next_delay();
            assert!(d <= Duration::from_secs(60));
        }
    }
}
