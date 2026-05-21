/// Standalone GUI runner — opens the EtherTap window without a DAW.
///
/// The default target is `127.0.0.1:10023`.  To test with a mock mixer, run
/// `scripts/mock_mixer.py` in a separate terminal before connecting.
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
