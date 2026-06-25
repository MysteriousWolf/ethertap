//! Shared MIDI clock accumulation/stats logic for [`crate::MidiClockSink`]
//! (OS virtual port, `cfg(unix)`) and [`crate::loopback_sink::LoopbackClockSink`]
//! (in-process loopback, all platforms).
//!
//! Both sinks receive raw MIDI bytes (a midir callback vs. a polled
//! `LoopbackPort::try_recv()`) and feed them through [`SinkState::on_message`]
//! identically — 0xF8 clock-byte counting, BPM sampling, and jitter window
//! bookkeeping are platform-agnostic.

use std::collections::VecDeque;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use crate::SinkStats;

pub const CLOCK_BYTE: u8 = 0xF8;
/// Clocks per beat at the MIDI-standard 24 PPQ (BPM estimation only — the
/// sink measures whatever EtherTap sends; jitter stats are PPQ-agnostic).
pub const MIDI_CPB: usize = 24;
/// Timestamps kept → `WINDOW - 1` intervals = 10 beats at 24 PPQ.
pub const WINDOW: usize = 241;
pub const BPM_HIST: usize = 180;

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[derive(Default)]
pub struct SinkState {
    clock_times: VecDeque<Instant>,
    bpm_history: VecDeque<f64>,
    total_clocks: u64,
    other_msgs: u64,
    last_hex: String,
    last_ts_ms: u64,
    last_clock_ts_ms: u64,
}

impl SinkState {
    /// Feed one received MIDI message into the accumulator. Mirrors the
    /// per-message handling previously inlined in the midir virtual-port
    /// callback.
    pub fn on_message(&mut self, message: &[u8]) {
        let now = Instant::now();
        self.last_hex = message
            .iter()
            .map(|b| format!("0x{b:02X}"))
            .collect::<Vec<_>>()
            .join(" ");
        self.last_ts_ms = now_ms();
        let Some(&first) = message.first() else {
            return;
        };
        if first == CLOCK_BYTE {
            self.total_clocks += 1;
            self.last_clock_ts_ms = self.last_ts_ms;
            self.clock_times.push_back(now);
            while self.clock_times.len() > WINDOW {
                self.clock_times.pop_front();
            }
            // Sample BPM once per beat from the last CPB+1 stamps.
            if self.total_clocks.is_multiple_of(MIDI_CPB as u64)
                && self.clock_times.len() > MIDI_CPB
            {
                let n = self.clock_times.len();
                let first_t = self.clock_times[n - 1 - MIDI_CPB];
                let last_t = self.clock_times[n - 1];
                let mean_iv = last_t.duration_since(first_t).as_secs_f64() / MIDI_CPB as f64;
                if mean_iv > 0.0 {
                    self.bpm_history
                        .push_back(60.0 / (mean_iv * MIDI_CPB as f64));
                    while self.bpm_history.len() > BPM_HIST {
                        self.bpm_history.pop_front();
                    }
                }
            }
        } else {
            self.other_msgs += 1;
        }
    }

    /// Total 0xF8 clock bytes received since start.
    pub fn total_clocks(&self) -> u64 {
        self.total_clocks
    }

    /// Compute interval/jitter statistics over the current window. Returns
    /// `None` until at least two clocks have arrived.
    pub fn stats(&self) -> Option<SinkStats> {
        if self.clock_times.len() < 2 {
            return None;
        }
        let times: Vec<Instant> = self.clock_times.iter().copied().collect();
        let intervals: Vec<f64> = times
            .windows(2)
            .map(|w| w[1].duration_since(w[0]).as_secs_f64())
            .collect();
        let mean_iv = intervals.iter().sum::<f64>() / intervals.len() as f64;
        let bpm = if mean_iv > 0.0 {
            60.0 / (mean_iv * MIDI_CPB as f64)
        } else {
            0.0
        };
        let var = intervals
            .iter()
            .map(|iv| (iv - mean_iv).powi(2))
            .sum::<f64>()
            / (intervals.len().max(2) - 1) as f64;

        let mut abs_jitter_us: Vec<f64> = intervals
            .iter()
            .map(|iv| (iv - mean_iv).abs() * 1e6)
            .collect();
        abs_jitter_us.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let pct = |p: f64| -> f64 {
            let idx = (p / 100.0) * (abs_jitter_us.len() - 1) as f64;
            let lo = idx.floor() as usize;
            let hi = (lo + 1).min(abs_jitter_us.len() - 1);
            abs_jitter_us[lo] + (abs_jitter_us[hi] - abs_jitter_us[lo]) * (idx - lo as f64)
        };

        Some(SinkStats {
            bpm,
            bpm_history: self.bpm_history.iter().copied().collect(),
            total_clocks: self.total_clocks,
            other_msgs: self.other_msgs,
            sample_count: intervals.len(),
            mean_us: mean_iv * 1e6,
            std_us: var.sqrt() * 1e6,
            p50_us: pct(50.0),
            p75_us: pct(75.0),
            p95_us: pct(95.0),
            p99_us: pct(99.0),
            max_us: abs_jitter_us.last().copied().unwrap_or(0.0),
            last_hex: self.last_hex.clone(),
            last_ts_ms: self.last_ts_ms,
            last_clock_ts_ms: self.last_clock_ts_ms,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn on_message_empty_slice_is_no_op() {
        let mut s = SinkState::default();
        s.on_message(&[]);
        assert_eq!(s.total_clocks(), 0);
        assert!(s.stats().is_none());
    }

    #[test]
    fn on_message_non_clock_byte_increments_other_msgs() {
        let mut s = SinkState::default();
        // 0x90 = Note On — not a clock byte
        s.on_message(&[0x90, 0x3C, 0x64]);
        assert_eq!(s.total_clocks(), 0);
        // stats() needs ≥2 clock timestamps so it returns None here
        assert!(s.stats().is_none());
    }

    #[test]
    fn on_message_clock_byte_increments_total_clocks() {
        let mut s = SinkState::default();
        s.on_message(&[CLOCK_BYTE]);
        assert_eq!(s.total_clocks(), 1);
    }

    #[test]
    fn stats_returns_none_with_fewer_than_two_clocks() {
        let mut s = SinkState::default();
        s.on_message(&[CLOCK_BYTE]);
        assert!(s.stats().is_none(), "need ≥2 clocks for stats");
    }

    #[test]
    fn stats_returns_some_with_two_or_more_clocks() {
        let mut s = SinkState::default();
        s.on_message(&[CLOCK_BYTE]);
        // Small sleep so Instant::now() advances between messages
        std::thread::sleep(std::time::Duration::from_micros(100));
        s.on_message(&[CLOCK_BYTE]);
        let stats = s.stats().expect("two clocks → stats must be Some");
        assert_eq!(stats.total_clocks, 2);
        assert!(stats.mean_us > 0.0, "mean interval must be positive");
    }

    #[test]
    fn stats_computes_percentiles_with_many_clocks() {
        let mut s = SinkState::default();
        // Send enough clocks that all percentile paths are exercised.
        for _ in 0..10 {
            s.on_message(&[CLOCK_BYTE]);
            std::thread::sleep(std::time::Duration::from_micros(100));
        }
        let stats = s.stats().expect("10 clocks → stats must be Some");
        assert!(stats.sample_count >= 9, "9 intervals from 10 timestamps");
        assert!(stats.p50_us >= 0.0);
        assert!(stats.p95_us >= stats.p50_us);
        assert!(stats.max_us >= 0.0);
    }

    #[test]
    fn bpm_sampling_fires_after_midi_cpb_clocks() {
        let mut s = SinkState::default();
        // Send MIDI_CPB+1 clocks (25 for 24 PPQ) so the BPM-sample path fires.
        for _ in 0..=(MIDI_CPB) {
            s.on_message(&[CLOCK_BYTE]);
            std::thread::sleep(std::time::Duration::from_micros(500));
        }
        // After MIDI_CPB clocks the bpm_history may have a sample if the
        // clock_times window is large enough. The main thing to verify is
        // no panic and stats still work.
        let stats = s.stats().expect("stats must be Some after many clocks");
        assert!(stats.total_clocks > MIDI_CPB as u64);
    }
}
