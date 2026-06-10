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

#[cfg(unix)]
pub mod clock_sink;
#[cfg(unix)]
pub use clock_sink::{MidiClockSink, SinkStats, SINK_PORT_NAME};
