use coremidi::{Client, Destinations, EventBuffer, Protocol};

use super::backend::{CoreMidiConnection, MidiBackend, MidiConnection};

/// CoreMIDI backend for macOS — uses native CoreMIDI framework for
/// MIDI output with hot-plug notification support.
pub struct CoreMidiBackend;

impl MidiBackend for CoreMidiBackend {
    fn connect(device_name: &str) -> Option<MidiConnection> {
        let client = match Client::new("EtherTap") {
            Ok(c) => c,
            Err(e) => {
                log::warn!("Failed to create CoreMIDI client: {}", e);
                return None;
            }
        };

        let port = match client.output_port("EtherTap-Out") {
            Ok(p) => p,
            Err(e) => {
                log::warn!("Failed to create CoreMIDI output port: {}", e);
                return None;
            }
        };

        let destination = Destinations
            .into_iter()
            .find(|d| d.display_name().as_deref() == Some(device_name));

        let destination = match destination {
            Some(d) => d,
            None => {
                log::warn!("CoreMIDI destination '{}' not found", device_name);
                return None;
            }
        };

        Some(MidiConnection {
            inner: CoreMidiConnection {
                client,
                port,
                destination,
            },
        })
    }

    fn list_ports() -> Vec<String> {
        let mut ports: Vec<String> = Destinations
            .into_iter()
            .filter_map(|d| d.display_name())
            .collect();
        ports.sort_unstable();
        ports
    }

    fn send_clock(conn: &mut MidiConnection) -> bool {
        let packet = EventBuffer::new(Protocol::Midi10).with_packet(0, &[0xF8]);
        conn.inner
            .port
            .send(&conn.inner.destination, &packet)
            .is_ok()
    }

    fn disconnect(_conn: MidiConnection) {
        // Connection closes when MidiConnection is dropped.
    }

    fn is_connected(_conn: &MidiConnection) -> bool {
        // CoreMIDI connections are implicit; if the struct exists,
        // the port is valid.  Higher-level logic (send failures,
        // device disappearance) drives reconnection.
        true
    }
}
