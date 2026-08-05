/// Standalone GUI runner — opens the EtherTap window without a DAW.
///
/// The default target is `127.0.0.1:10023`.  To test with a mock mixer, run
/// `cargo run -p mock-suite` in a separate terminal before connecting (press
/// `m` to toggle the mixer panel).
///
/// Build and run:
///   cargo run --bin ethertap-gui --features standalone
///
/// (Audio I/O is optional; the GUI opens even if no audio device is found.)
fn main() {
    if !nice_plug::nice_export_standalone::<ethertap::EtherTap>() {
        eprintln!("[EtherTap] standalone init failed");
        std::process::exit(1);
    }
}
