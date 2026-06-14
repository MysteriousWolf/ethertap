//! Loopback diagnostic: open the sink, connect a MidiOutput to it directly,
//! send 0xF8 bytes, report what the sink counted.
//!
//! Virtual MIDI ports are a CoreMIDI/ALSA feature — unix-only, matching
//! `mock_suite::clock_sink`.

#[cfg(not(unix))]
fn main() {
    eprintln!("sink_loopback: unix-only (virtual MIDI ports)");
}

#[cfg(unix)]
fn main() {
    use std::time::Duration;

    let sink = mock_suite::MidiClockSink::start().expect("open sink");
    std::thread::sleep(Duration::from_millis(300));

    let out = midir::MidiOutput::new("loopback-test").expect("midi output");
    let matching: Vec<_> = out
        .ports()
        .into_iter()
        .filter(|p| {
            out.port_name(p)
                .map(|n| n == mock_suite::SINK_PORT_NAME)
                .unwrap_or(false)
        })
        .collect();
    println!("matching ports: {}", matching.len());

    for (i, port) in matching.iter().enumerate() {
        let out = midir::MidiOutput::new("loopback-test").expect("midi output");
        let before = sink.total_clocks();
        let mut conn = out.connect(port, "loopback").expect("connect");
        for _ in 0..24 {
            conn.send(&[0xF8]).expect("send");
            std::thread::sleep(Duration::from_millis(2));
        }
        std::thread::sleep(Duration::from_millis(300));
        println!(
            "port {i}: sink counted {} (delta {})",
            sink.total_clocks(),
            sink.total_clocks() - before
        );
        conn.close();
    }
}
