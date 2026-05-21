#[cfg(not(target_os = "macos"))]
pub struct MidirBackend;

#[cfg(not(target_os = "macos"))]
impl super::backend::MidiBackend for MidirBackend {
    fn connect(device_name: &str) -> Option<super::backend::MidiConnection> {
        let out = match midir::MidiOutput::new("EtherTap-Out") {
            Ok(o) => o,
            Err(e) => {
                log::warn!("Failed to create MIDI output: {}", e);
                return None;
            }
        };
        let port = out
            .ports()
            .into_iter()
            .find(|p| out.port_name(p).map(|n| n == device_name).unwrap_or(false));
        let port = match port {
            Some(p) => p,
            None => {
                log::warn!("MIDI output port '{}' not found", device_name);
                return None;
            }
        };
        match out.connect(&port, "EtherTap-Out") {
            Ok(conn) => Some(super::backend::MidiConnection { inner: Some(conn) }),
            Err(e) => {
                log::warn!("Failed to connect to MIDI port '{}': {}", device_name, e);
                None
            }
        }
    }

    fn list_ports() -> Vec<String> {
        match midir::MidiOutput::new("EtherTap-Scan") {
            Ok(out) => {
                let mut ports: Vec<String> = out
                    .ports()
                    .iter()
                    .filter_map(|p| out.port_name(p).ok())
                    .collect();
                ports.sort_unstable();
                ports
            }
            Err(e) => {
                log::warn!("Failed to create MIDI output for port scan: {}", e);
                Vec::new()
            }
        }
    }

    fn send_clock(conn: &mut super::backend::MidiConnection) -> bool {
        match conn.inner {
            Some(ref mut c) => c.send(&[0xF8]).is_ok(),
            None => false,
        }
    }

    fn disconnect(conn: super::backend::MidiConnection) {
        drop(conn);
    }

    fn is_connected(conn: &super::backend::MidiConnection) -> bool {
        conn.inner.is_some()
    }
}
