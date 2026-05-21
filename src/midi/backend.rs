/// Abstract interface for MIDI output backends.
///
/// Implemented by `CoreMidiBackend` (macOS) and `MidirBackend` (other platforms).
pub trait MidiBackend {
    /// Attempt to open a connection to the named MIDI output device.
    fn connect(device_name: &str) -> Option<MidiConnection>;

    /// List all available MIDI output port names.
    fn list_ports() -> Vec<String>;

    /// Send a single MIDI clock tick (0xF8) through the open connection.
    fn send_clock(conn: &mut MidiConnection) -> bool;

    /// Close the connection and release the device.
    fn disconnect(conn: MidiConnection);

    /// Returns true if the connection is still open.
    #[allow(dead_code)]
    fn is_connected(conn: &MidiConnection) -> bool;
}

/// Opaque handle wrapping the platform-specific MIDI connection.
pub struct MidiConnection {
    #[cfg(target_os = "macos")]
    pub(crate) inner: CoreMidiConnection,
    #[cfg(not(target_os = "macos"))]
    pub(crate) inner: Option<midir::MidiOutputConnection>,
}

#[cfg(target_os = "macos")]
pub struct CoreMidiConnection {
    #[allow(dead_code)]
    pub(crate) client: coremidi::Client,
    pub(crate) port: coremidi::OutputPort,
    pub(crate) destination: coremidi::Destination,
}
