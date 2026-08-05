//! Hardware MIDI port connection helpers.
//!
//! Requires real hardware MIDI devices and is excluded from coverage
//! measurement (see `scripts/coverage.sh` IGNORE_REGEX). The loopback path
//! in `midi_clock::try_connect_out` is kept in that module; only the hardware
//! fallback lives here so it does not inflate the missed-line count.
use std::sync::{
    Arc,
    atomic::{AtomicU32, Ordering},
};

use crossbeam_channel::Sender;
use nice_plug::{nice_log, nice_warn};

/// Try to open `device_name` as a midir hardware MIDI output port.
/// Returns `None` if no matching port is found or the connection fails.
pub(crate) fn try_hw_out(device_name: &str) -> Option<midir::MidiOutputConnection> {
    let out = match midir::MidiOutput::new("EtherTap-PhysOut") {
        Ok(o) => o,
        Err(e) => {
            nice_warn!("[EtherTap] try_connect_out: MidiOutput::new failed: {e}");
            return None;
        }
    };
    let port = match out
        .ports()
        .into_iter()
        .find(|p| out.port_name(p).map(|n| n == device_name).unwrap_or(false))
    {
        Some(p) => p,
        None => {
            nice_warn!("[EtherTap] try_connect_out: port '{device_name}' not found");
            return None;
        }
    };
    match out.connect(&port, "EtherTap-PhysOut") {
        Ok(c) => {
            nice_log!("[EtherTap] try_connect_out: connected to '{device_name}'");
            Some(c)
        }
        Err(e) => {
            nice_warn!("[EtherTap] try_connect_out: connect to '{device_name}' failed: {e}");
            None
        }
    }
}

/// Try to open `device_name` as a midir hardware MIDI input port, forwarding
/// non-0xF8 bytes to `pass_tx`.
/// Returns `None` if no matching port is found or the connection fails.
pub(crate) fn try_hw_in(
    device_name: &str,
    pass_tx: Sender<Vec<u8>>,
    drop_count: Arc<AtomicU32>,
) -> Option<midir::MidiInputConnection<()>> {
    use midir::MidiInput;
    let inp = match MidiInput::new("EtherTap-PhysIn") {
        Ok(i) => i,
        Err(e) => {
            nice_warn!("[EtherTap] try_connect_in: MidiInput::new failed: {e}");
            return None;
        }
    };
    let port = match inp
        .ports()
        .into_iter()
        .find(|p| inp.port_name(p).map(|n| n == device_name).unwrap_or(false))
    {
        Some(p) => p,
        None => {
            nice_warn!("[EtherTap] try_connect_in: port '{device_name}' not found");
            return None;
        }
    };
    match inp.connect(
        &port,
        "EtherTap-PhysIn",
        move |_ts, msg, _| {
            if msg.first().copied() != Some(0xF8) && pass_tx.try_send(msg.to_vec()).is_err() {
                drop_count.fetch_add(1, Ordering::Relaxed);
            }
        },
        (),
    ) {
        Ok(c) => {
            nice_log!("[EtherTap] try_connect_in: connected input to '{device_name}'");
            Some(c)
        }
        Err(e) => {
            nice_warn!("[EtherTap] try_connect_in: connect to '{device_name}' failed: {e}");
            None
        }
    }
}
