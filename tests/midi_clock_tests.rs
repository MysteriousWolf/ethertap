//! MIDI clock verification through the mock-suite virtual sink.
//!
//! EtherTap's clock worker connects to the sink's virtual input port
//! ("EtherTap Mock MIDI Sink"); the harness drives playing buffers with
//! advancing song position so the DAW-mode tick scheduler emits 0xF8 bytes.
//! Virtual MIDI ports are CoreMIDI/ALSA features — these tests are unix-only.
//! Standalone builds derive ticks from UI atomics, not the transport these
//! tests drive, so they're VST3-build-only too.
#![cfg(all(unix, not(feature = "standalone")))]

mod common;

use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use common::harness_util::{step_at, E2E_LOCK};
use ethertap::EtherTap;
use mock_suite::MidiClockSink;
use vst_runtime::Harness;

#[test]
fn clock_ticks_reach_virtual_sink_with_zero_drops() {
    let _guard = E2E_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    // Sink must exist before the plugin scans for devices. Unique name:
    // CoreMIDI can surface phantom stale destinations under a reused name,
    // and the worker connects to the first name match.
    let sink_name = format!("EtherTap Test Sink {}", std::process::id());
    let sink = MidiClockSink::start_named(&sink_name).expect("virtual MIDI sink should open");

    let mut harness = Harness::<EtherTap>::new(44_100.0, 256).expect("EtherTap should initialize");
    let params = harness.plugin().ethertap_params();
    let handles = harness.plugin().test_handles();

    // Select the sink as the output device — same path the editor's device
    // picker takes: persist the name, notify the worker.
    *params.midi_out_device.lock() = Some(sink_name.clone());
    handles
        .device_change_tx
        .send(Some(sink_name.clone()))
        .expect("device change channel");

    // The worker connects asynchronously; wait for the bridge before counting.
    let bridge_deadline = Instant::now() + Duration::from_secs(5);
    while !handles.midi_bridge_connected.load(Ordering::Acquire) {
        assert!(
            Instant::now() < bridge_deadline,
            "MIDI worker never connected to the virtual sink port"
        );
        std::thread::sleep(Duration::from_millis(20));
    }

    // midi_clock_enabled defaults true. Drive playing buffers with advancing
    // position at 120 BPM / 24 PPQ: one tick every 91.875 samples → ~2.8
    // ticks per 256-sample buffer. Sleep between steps so the worker thread
    // forwards ticks to the (real) CoreMIDI/ALSA port.
    let deadline = Instant::now() + Duration::from_secs(8);
    let mut pos: i64 = 0;
    while sink.total_clocks() < 48 && Instant::now() < deadline {
        step_at(&mut harness, 120.0, pos);
        pos += 256;
        std::thread::sleep(Duration::from_millis(5));
    }

    assert!(
        sink.total_clocks() >= 48,
        "expected at least 2 beats of clocks at the sink, got {}",
        sink.total_clocks()
    );
    assert_eq!(
        handles.midi_clock_drop_count.load(Ordering::Relaxed),
        0,
        "no clock messages may be dropped on the worker channel"
    );

    // Disable the clock param; tick flow stops (allow in-flight to drain).
    assert!(harness.set_param_normalized("midi_clock_enabled", 0.0));
    step_at(&mut harness, 120.0, pos);
    std::thread::sleep(Duration::from_millis(200));
    let frozen = sink.total_clocks();
    for _ in 0..20 {
        pos += 256;
        step_at(&mut harness, 120.0, pos);
        std::thread::sleep(Duration::from_millis(5));
    }
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(
        sink.total_clocks(),
        frozen,
        "ticks must stop once midi_clock_enabled is false"
    );

    harness.deactivate();
}
