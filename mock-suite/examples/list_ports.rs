//! Open the mock clock sink, then enumerate MIDI output ports as midir sees
//! them — diagnoses port-name mismatches between the sink's virtual input and
//! the name EtherTap's worker tries to connect to.

fn main() {
    let _sink = mock_suite::MidiClockSink::start().expect("open sink");
    std::thread::sleep(std::time::Duration::from_millis(300));
    let out = midir::MidiOutput::new("list-ports").expect("midi output");
    for p in out.ports() {
        println!("out port: {:?}", out.port_name(&p));
    }
}
