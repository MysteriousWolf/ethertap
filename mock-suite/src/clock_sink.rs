//! MIDI clock sink — a virtual MIDI input port ("EtherTap Mock MIDI Sink")
//! that counts 0xF8 clock bytes and computes interval/jitter statistics, so a
//! human (TUI) or a script (headless mode) can verify EtherTap's MIDI clock
//! output without hardware.
//!
//! Virtual ports are a CoreMIDI/ALSA feature — unavailable on Windows, hence
//! the `unix` gate (matches `midir::os::unix::VirtualInput`).

#![cfg(unix)]

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use midir::os::unix::VirtualInput;
use midir::{Ignore, MidiInput, MidiInputConnection};
use parking_lot::Mutex;

pub const SINK_PORT_NAME: &str = "EtherTap Mock MIDI Sink";

const CLOCK_BYTE: u8 = 0xF8;
/// Clocks per beat at the MIDI-standard 24 PPQ (BPM estimation only — the
/// sink measures whatever EtherTap sends; jitter stats are PPQ-agnostic).
const MIDI_CPB: usize = 24;
/// Timestamps kept → `WINDOW - 1` intervals = 10 beats at 24 PPQ.
const WINDOW: usize = 241;
const BPM_HIST: usize = 180;

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[derive(Default)]
struct SinkState {
    clock_times: VecDeque<Instant>,
    bpm_history: VecDeque<f64>,
    total_clocks: u64,
    other_msgs: u64,
    last_hex: String,
    last_ts_ms: u64,
    last_clock_ts_ms: u64,
}

/// Snapshot of computed statistics. `None`-like when fewer than 2 clocks have
/// arrived (see [`MidiClockSink::stats`]).
#[derive(Debug, Clone, Default)]
pub struct SinkStats {
    pub bpm: f64,
    pub bpm_history: Vec<f64>,
    pub total_clocks: u64,
    pub other_msgs: u64,
    pub sample_count: usize,
    pub mean_us: f64,
    pub std_us: f64,
    pub p50_us: f64,
    pub p75_us: f64,
    pub p95_us: f64,
    pub p99_us: f64,
    pub max_us: f64,
    pub last_hex: String,
    pub last_ts_ms: u64,
    pub last_clock_ts_ms: u64,
}

pub struct MidiClockSink {
    state: Arc<Mutex<SinkState>>,
    conn: Option<MidiInputConnection<()>>,
    port_name: String,
}

impl MidiClockSink {
    /// Open the virtual input port under the canonical name and start
    /// counting.
    pub fn start() -> Result<Self, String> {
        Self::start_named(SINK_PORT_NAME)
    }

    /// Open under a caller-chosen name. Tests use a per-process unique name:
    /// CoreMIDI can surface phantom (stale, unowned) virtual destinations
    /// under a previously used name, and EtherTap's worker connects to the
    /// *first* name match — a unique name guarantees exactly one live match.
    pub fn start_named(port_name: &str) -> Result<Self, String> {
        let mut input = MidiInput::new(port_name).map_err(|e| e.to_string())?;
        // Timing bytes (0xF8) are exactly what we want — ignore nothing but
        // active sense.
        input.ignore(Ignore::ActiveSense);

        let state = Arc::new(Mutex::new(SinkState::default()));
        let cb_state = state.clone();
        let conn = input
            .create_virtual(
                port_name,
                move |_timestamp_us, message, _| {
                    let now = Instant::now();
                    let mut s = cb_state.lock();
                    s.last_hex = message
                        .iter()
                        .map(|b| format!("0x{b:02X}"))
                        .collect::<Vec<_>>()
                        .join(" ");
                    s.last_ts_ms = now_ms();
                    let Some(&first) = message.first() else { return };
                    if first == CLOCK_BYTE {
                        s.total_clocks += 1;
                        s.last_clock_ts_ms = s.last_ts_ms;
                        s.clock_times.push_back(now);
                        while s.clock_times.len() > WINDOW {
                            s.clock_times.pop_front();
                        }
                        // Sample BPM once per beat from the last CPB+1 stamps.
                        if s.total_clocks % MIDI_CPB as u64 == 0
                            && s.clock_times.len() > MIDI_CPB
                        {
                            let n = s.clock_times.len();
                            let first_t = s.clock_times[n - 1 - MIDI_CPB];
                            let last_t = s.clock_times[n - 1];
                            let mean_iv = last_t.duration_since(first_t).as_secs_f64()
                                / MIDI_CPB as f64;
                            if mean_iv > 0.0 {
                                s.bpm_history.push_back(60.0 / (mean_iv * MIDI_CPB as f64));
                                while s.bpm_history.len() > BPM_HIST {
                                    s.bpm_history.pop_front();
                                }
                            }
                        }
                    } else {
                        s.other_msgs += 1;
                    }
                },
                (),
            )
            .map_err(|e| e.to_string())?;

        Ok(Self { state, conn: Some(conn), port_name: port_name.to_string() })
    }

    /// The virtual port's name (what shows up in device pickers / port scans).
    pub fn port_name(&self) -> &str {
        &self.port_name
    }

    /// Close the port and stop counting. Idempotent.
    pub fn stop(&mut self) {
        if let Some(conn) = self.conn.take() {
            conn.close();
        }
    }

    pub fn is_running(&self) -> bool {
        self.conn.is_some()
    }

    /// Total 0xF8 clock bytes received since start.
    pub fn total_clocks(&self) -> u64 {
        self.state.lock().total_clocks
    }

    /// Compute interval/jitter statistics over the current window. Returns
    /// `None` until at least two clocks have arrived.
    pub fn stats(&self) -> Option<SinkStats> {
        let s = self.state.lock();
        if s.clock_times.len() < 2 {
            return None;
        }
        let times: Vec<Instant> = s.clock_times.iter().copied().collect();
        let intervals: Vec<f64> = times
            .windows(2)
            .map(|w| w[1].duration_since(w[0]).as_secs_f64())
            .collect();
        let mean_iv = intervals.iter().sum::<f64>() / intervals.len() as f64;
        let bpm = if mean_iv > 0.0 { 60.0 / (mean_iv * MIDI_CPB as f64) } else { 0.0 };
        let var = intervals.iter().map(|iv| (iv - mean_iv).powi(2)).sum::<f64>()
            / (intervals.len().max(2) - 1) as f64;

        let mut abs_jitter_us: Vec<f64> =
            intervals.iter().map(|iv| (iv - mean_iv).abs() * 1e6).collect();
        abs_jitter_us.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let pct = |p: f64| -> f64 {
            let idx = (p / 100.0) * (abs_jitter_us.len() - 1) as f64;
            let lo = idx.floor() as usize;
            let hi = (lo + 1).min(abs_jitter_us.len() - 1);
            abs_jitter_us[lo] + (abs_jitter_us[hi] - abs_jitter_us[lo]) * (idx - lo as f64)
        };

        Some(SinkStats {
            bpm,
            bpm_history: s.bpm_history.iter().copied().collect(),
            total_clocks: s.total_clocks,
            other_msgs: s.other_msgs,
            sample_count: intervals.len(),
            mean_us: mean_iv * 1e6,
            std_us: var.sqrt() * 1e6,
            p50_us: pct(50.0),
            p75_us: pct(75.0),
            p95_us: pct(95.0),
            p99_us: pct(99.0),
            max_us: abs_jitter_us.last().copied().unwrap_or(0.0),
            last_hex: s.last_hex.clone(),
            last_ts_ms: s.last_ts_ms,
            last_clock_ts_ms: s.last_clock_ts_ms,
        })
    }
}

impl Drop for MidiClockSink {
    fn drop(&mut self) {
        self.stop();
    }
}
