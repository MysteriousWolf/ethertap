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

    // 7. All steps should return ProcessStatus::Normal.
    assert!(
        result.statuses.iter().all(|&s| s == ProcessStatus::Normal),
        "all steps should return ProcessStatus::Normal"
    );

    // 8. process() mirrors BoolParam → atom each buffer. After running 4 steps,
    //     the atom must reflect the BoolParam's default (true), overwriting our
    //     earlier store(false). Verifies the mirror path actually executes.
    assert!(
        params.midi_clock_enabled_atom.load(Ordering::Relaxed),
        "atom must reflect BoolParam default (true) after process() runs"
    );

    // Cleanly deactivate the plugin (Drop handles thread cleanup).
    harness.deactivate();
}

/// Coverage guard: the host-visible param id set. Fails when a param is
/// added or renamed without updating this list — the reminder to also wire
/// it into the standalone DAW shell's PARAMETERS IN/OUT footer (editor.rs
/// `daw_shell`) and any host-facing docs.
#[test]
fn param_id_set_is_accounted_for() {
    let harness = Harness::<EtherTap>::new(44_100.0, 256).expect("EtherTap should initialize");

    let mut actual: Vec<String> = harness.param_ids().map(str::to_owned).collect();
    actual.sort();

    let mut expected = vec![
        "fx_slot",
        "fx_filter_dly",
        "fx_filter_3tap",
        "fx_filter_4tap",
        "fx_filter_drv",
        "fx_filter_dcr",
        "fx_filter_dfl",
        "fx_filter_modd",
        "midi_clock_enabled",
        "midi_clock_ppq",
        "midi_auto_connect",
        "auto_reconnect",
        "rate_sync_mode",
        "phase_sync_mode",
        "connect_to_last",
        "disconnect",
        "force_sync_rate",
        "force_sync_phase",
        "audit_slots",
        "all_slots",
        "is_connected",
        "is_matched",
        "sync_status",
        "phase_reset_pending",
        "hardware_bpm",
        "compatible_slot_count",
        "midi_bridge_connected",
    ];
    expected.sort_unstable();

    assert_eq!(
        actual, expected,
        "host param set changed — update the DAW shell footer (editor.rs \
         daw_shell) and this list together"
    );
}
