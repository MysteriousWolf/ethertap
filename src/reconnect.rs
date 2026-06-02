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
        (self.base_ms << self.attempts).min(self.cap_ms)
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
