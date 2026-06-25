//! MIDI clock sink — a virtual MIDI input port ("EtherTap Mock MIDI Sink")
//! that counts 0xF8 clock bytes and computes interval/jitter statistics, so a
//! human (TUI) or a script (headless mode) can verify EtherTap's MIDI clock
//! output without hardware.
//!
//! Virtual ports are a CoreMIDI/ALSA feature — unavailable on Windows, hence
//! the `unix` gate (matches `midir::os::unix::VirtualInput`).

#![cfg(unix)]

use std::sync::Arc;

use midir::os::unix::VirtualInput;
use midir::{Ignore, MidiInput, MidiInputConnection};
use parking_lot::Mutex;

use crate::SinkStats;
use crate::sink_state::SinkState;

pub const SINK_PORT_NAME: &str = "EtherTap Mock MIDI Sink";

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
                    cb_state.lock().on_message(message);
                },
                (),
            )
            .map_err(|e| e.to_string())?;

        Ok(Self {
            state,
            conn: Some(conn),
            port_name: port_name.to_string(),
        })
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
        self.state.lock().total_clocks()
    }

    /// Compute interval/jitter statistics over the current window. Returns
    /// `None` until at least two clocks have arrived.
    pub fn stats(&self) -> Option<SinkStats> {
        self.state.lock().stats()
    }
}

impl Drop for MidiClockSink {
    fn drop(&mut self) {
        self.stop();
    }
}
