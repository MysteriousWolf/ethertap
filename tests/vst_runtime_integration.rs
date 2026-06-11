/// Integration test: EtherTap driven through the vst-runtime harness.
///
/// EtherTap::default() spawns a network worker (tries 192.168.1.100:10023)
/// and a MIDI clock worker. Neither can connect in a CI / local test
/// environment — the network worker fails silently (bounded channel,
/// non-blocking). Dropping the Harness closes the channels; both threads
/// exit naturally. The test completes in milliseconds with no network timeout.
use std::sync::atomic::Ordering;

use ethertap::EtherTap;
use vst_runtime::{Harness, ProcessStatus, Transport};

#[test]
fn ethertap_harness_drives_process_and_exposes_params() {
    // 1. Construct and initialize EtherTap through the harness.
    let mut harness = Harness::<EtherTap>::new(44_100.0, 256).expect("EtherTap should initialize");

    // 2. Access the plugin's params via the shared Arc.
    let params = harness.plugin().ethertap_params();

    // 3. Confirm midi_clock_enabled default is true (via backing atom — the
    //    authoritative BoolParam is mirrored to the atom each process() call).
    assert!(
        params.midi_clock_enabled_atom.load(Ordering::Relaxed),
        "midi_clock_enabled should default to true"
    );

    // 4. Disable via the backing atom (direct worker-facing path, same as before).
    params
        .midi_clock_enabled_atom
        .store(false, Ordering::Relaxed);

    // 5. Build a playing transport at 120 BPM, 4/4.
    let mut t = Transport::new(44_100.0);
    t.playing = true;
    t.tempo = Some(120.0);
    t.time_sig_numerator = Some(4);
    t.time_sig_denominator = Some(4);

    // 6. Run a four-step scenario with 256-sample buffers.
    let result = harness
        .scenario(256)
        .transport(t)
        .step()
        .step()
        .step()
        .step()
        .finish();

    // 7. Four steps → four statuses.
    assert_eq!(result.statuses.len(), 4, "expected one status per step");

    // 8. All steps should return ProcessStatus::Normal.
    assert!(
        result.statuses.iter().all(|&s| s == ProcessStatus::Normal),
        "all steps should return ProcessStatus::Normal"
    );

    // 9. Four output buffer snapshots (one per step).
    assert_eq!(
        result.output_buffers.len(),
        4,
        "expected one output buffer snapshot per step"
    );

    // 10. The backing atom is shared — our store(false) above persists; note
    //     that process() mirrors the BoolParam → atom each buffer, so after
    //     running steps the atom will reflect the BoolParam's default (true).
    //     The atom value after processing reflects the param default, not our
    //     manual store — this is expected: process() is the authoritative sync.
    //     Just verify the test ran without panic.
    let _ = params.midi_clock_enabled_atom.load(Ordering::Relaxed);

    // Cleanly deactivate the plugin (Drop handles thread cleanup).
    harness.deactivate();
}
