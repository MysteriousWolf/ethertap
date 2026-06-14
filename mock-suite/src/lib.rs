//! EtherTap mock suite — X32/M32 mixer simulator + MIDI clock sink.
//!
//! Three faces:
//! - **Library**: [`MockMixer`] / [`SlotState`] are the fixture used by
//!   EtherTap's integration tests (`tests/common/mod.rs` re-exports them).
//! - **TUI** (`cargo run -p mock-suite`): interactive parity with the retired
//!   Python tool — live MIDI clock stats + mixer slot table + message log.
//! - **Headless** (`cargo run -p mock-suite -- --no-tui …`): scriptable test
//!   mode with `--jsonl` output and `--expect` assertions (exit code 0/1).

pub mod mixer;
pub use mixer::{
    all_dly_slots, all_empty_slots, default_slots, parse_slots_spec, type_name, MockMixer,
    ReceivedMsg, SlotState, DLY, EMPTY,
};

pub mod sink_state;

pub mod loopback_sink;

#[cfg(unix)]
pub mod clock_sink;
#[cfg(unix)]
pub use clock_sink::{MidiClockSink, SINK_PORT_NAME};

/// Snapshot of computed MIDI clock statistics. `None`-like when fewer than 2
/// clocks have arrived (see [`clock_sink::MidiClockSink::stats`] on unix).
///
/// Defined platform-neutrally (no virtual-port types) so the TUI can
/// reference it on Windows too, even though [`MidiClockSink`] itself is
/// unix-only.
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
