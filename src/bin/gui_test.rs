/// Standalone GUI runner — opens the EtherTap window without a DAW or a real
/// mixer.
///
/// A mock X32/M32 starts automatically on `127.0.0.1:10023`, reporting
/// 120 BPM and marking every slot as a Stereo Delay (DLY).  The IP field in
/// the GUI will already read `127.0.0.1`.
///
/// Build and run:
///   cargo run --bin ethertap-gui --features standalone
///
/// (Audio I/O is optional; the GUI opens even if no audio device is found.)
fn main() {
    if !nih_plug::nih_export_standalone::<ethertap::EtherTap>() {
        std::process::exit(1);
    }
}
