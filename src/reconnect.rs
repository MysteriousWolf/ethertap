use std::time::{Duration, Instant};

/// Exponential backoff for reconnection attempts.
///
/// Doubles the delay on each failure (base << attempts), capped at cap_ms.
/// Resets to base on success.
pub struct Backoff {
    base_ms: u64,
    cap_ms: u64,
    attempts: u32,
    skip_until: Option<Instant>,
}

impl Backoff {
    pub fn new(base_ms: u64, cap_ms: u64) -> Self {
        Self {
            base_ms,
            cap_ms,
            attempts: 0,
            skip_until: None,
        }
    }

    pub fn next_delay_ms(&self) -> u64 {
        // Guard shift amount first (prevents panic for attempts ≥ 64), then use
        // saturating_mul so large base × 2^attempts wraps to u64::MAX and is
        // immediately capped — no overflow in either debug or release builds.
        if self.attempts >= 64 {
            return self.cap_ms;
        }
        self.base_ms
            .saturating_mul(1u64 << self.attempts)
            .min(self.cap_ms)
    }

    pub fn record_failure(&mut self) {
        self.attempts = self.attempts.saturating_add(1);
        self.skip_until =
            Some(Instant::now() + Duration::from_millis(self.next_delay_ms()));
    }

    pub fn record_success(&mut self) {
        self.attempts = 0;
        self.skip_until = None;
    }

    pub fn is_cooling_down(&self) -> bool {
        self.skip_until
            .is_some_and(|until| Instant::now() < until)
    }

    pub fn reset(&mut self) {
        self.attempts = 0;
        self.skip_until = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_no_overflow_after_many_failures() {
        let mut b = Backoff::new(1000, 10000);
        for _ in 0..100 {
            b.record_failure();
        }
        assert_eq!(b.next_delay_ms(), 10000, "must stay capped, not overflow to 0");
    }
}
