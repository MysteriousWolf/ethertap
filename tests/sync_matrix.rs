//! Comprehensive automation matrix: every host-visible behavior driven
//! through the vst-runtime harness against a MockMixer.
//!
//! Covers: full param sweep + atom mirrors, single-slot vs all-slots
//! dispatch, FX type filtering, hard-reset OSC sequence, rate-OnChange settle
//! dispatch, and telemetry read-back into `is_matched`.
//!
//! The OnChange/telemetry tests are wall-clock bound (SETTLE_MS = 500 ms,
//! telemetry poll = 3 s) — expect a few seconds each.
//!
//! Exercises the VST3 (host-transport) build: standalone builds source BPM
//! and play state from UI atomics instead of the transport these tests drive.
#![cfg(not(feature = "standalone"))]

mod common;

use std::sync::atomic::Ordering;
use std::time::Duration;

use common::harness_util::{connect, drain_auto_sync, step, step_until, wait_for_audit, E2E_LOCK};
use common::{MockMixer, SlotState};
use ethertap::EtherTap;
use vst_runtime::Harness;

#[test]
fn param_sweep_no_panic_and_atoms_mirror() {
    let _guard = E2E_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut harness =
        Harness::<EtherTap>::new(44_100.0, 256).expect("EtherTap should initialize");

    // Sweep every #[id] param across its normalized range; one step each.
    // Proves no value combination panics the RT path.
    let ids: Vec<String> = harness.param_ids().map(String::from).collect();
    for id in &ids {
        for v in [0.0f32, 0.5, 1.0] {
            assert!(harness.set_param_normalized(id, v), "unknown id {id}");
            step(&mut harness, 120.0);
        }
        // Park the param back at default-ish false/0 for trigger params so
        // later sweeps don't compound (triggers self-reset anyway).
        harness.set_param_normalized(id, 0.0);
        step(&mut harness, 120.0);
    }

    // Targeted atom mirrors (worker-facing backing stores).
    let params = harness.plugin().ethertap_params();

    // fx_slot: IntParam 1–8; normalized 1.0 → slot 8.
    harness.set_param_normalized("fx_slot", 1.0);
    step(&mut harness, 120.0);
    assert_eq!(params.fx_slot_atom.load(Ordering::Relaxed), 8);

    // midi_clock_ppq: EnumParam, last variant = P96.
    harness.set_param_normalized("midi_clock_ppq", 1.0);
    step(&mut harness, 120.0);
    assert_eq!(params.midi_clock_ppq_atom.load(Ordering::Relaxed), 96);

    // fx_type_filter bitmask: disable bit 0 (DLY), keep the rest.
    harness.set_param_normalized("fx_filter_dly", 0.0);
    for f in ["fx_filter_3tap", "fx_filter_4tap", "fx_filter_drv",
              "fx_filter_dcr", "fx_filter_dfl", "fx_filter_modd"] {
        harness.set_param_normalized(f, 1.0);
    }
    step(&mut harness, 120.0);
    assert_eq!(params.fx_type_filter.load(Ordering::Relaxed), 0b111_1110);

    // all_slots mirrors to all_slots_atom.
    harness.set_param_normalized("all_slots", 0.0);
    step(&mut harness, 120.0);
    assert!(!params.all_slots_atom.load(Ordering::Relaxed));

    harness.deactivate();
}

#[test]
fn single_slot_mode_targets_selected_slot_only() {
    let _guard = E2E_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    // Slots 1 and 2 both DLY (both BPM-compatible, both would receive in
    // all-slots mode).
    let mut slots = [SlotState::empty(); 8];
    slots[0] = SlotState::dly(100.0);
    slots[1] = SlotState::dly(100.0);
    let mock = MockMixer::start_with_slots(slots);
    let mut harness = connect(&mock);
    wait_for_audit(&mut harness);
    drain_auto_sync(&mut harness, &mock);

    // Single-slot mode targeting slot 2.
    harness.set_param_normalized("all_slots", 0.0);
    harness.set_param_normalized("fx_slot", 1.0 / 7.0); // slot 2 of 1..=8
    step(&mut harness, 120.0);
    let params = harness.plugin().ethertap_params();
    assert_eq!(params.fx_slot_atom.load(Ordering::Relaxed), 2);

    let before = [mock.sync_count(1), mock.sync_count(2)];
    harness.set_param_normalized("force_sync_rate", 1.0);
    let synced = step_until(&mut harness, 120.0, Duration::from_secs(5), |_| {
        mock.sync_count(2) > before[1]
    });
    assert!(synced, "selected slot 2 never received a sync");
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(
        mock.sync_count(1),
        before[0],
        "slot 1 must not receive a sync in single-slot mode"
    );

    harness.deactivate();
}

#[test]
fn fx_type_filter_excludes_disabled_type() {
    let _guard = E2E_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    // Slot 1 = DLY (type 10), slot 2 = MODD (type 26) — both BPM-compatible.
    let mut slots = [SlotState::empty(); 8];
    slots[0] = SlotState::dly(100.0);
    slots[1] = SlotState::other(26);
    let mock = MockMixer::start_with_slots(slots);
    let mut harness = connect(&mock);
    wait_for_audit(&mut harness);
    drain_auto_sync(&mut harness, &mock);

    // Disable MODD in the type filter; force a rate sync; only DLY advances.
    harness.set_param_normalized("fx_filter_modd", 0.0);
    step(&mut harness, 120.0);
    let before = [mock.sync_count(1), mock.sync_count(2)];

    harness.set_param_normalized("force_sync_rate", 1.0);
    let synced = step_until(&mut harness, 120.0, Duration::from_secs(5), |_| {
        mock.sync_count(1) > before[0]
    });
    assert!(synced, "DLY slot never received a sync");
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(
        mock.sync_count(2),
        before[1],
        "filtered-out MODD slot must not receive a sync"
    );

    // Re-enable MODD; now both advance.
    harness.set_param_normalized("fx_filter_modd", 1.0);
    step(&mut harness, 120.0);
    let before2 = mock.sync_count(2);
    harness.set_param_normalized("force_sync_rate", 1.0);
    let synced2 = step_until(&mut harness, 120.0, Duration::from_secs(5), |_| {
        mock.sync_count(2) > before2
    });
    assert!(synced2, "re-enabled MODD slot never received a sync");

    harness.deactivate();
}

#[test]
fn force_sync_phase_sends_hard_reset_sequence() {
    let _guard = E2E_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut slots = [SlotState::empty(); 8];
    slots[0] = SlotState::dly(100.0);
    let mock = MockMixer::start_with_slots(slots);
    let mut harness = connect(&mock);
    wait_for_audit(&mut harness);
    mock.clear_msgs();

    harness.set_param_normalized("force_sync_phase", 1.0);
    let reset = step_until(&mut harness, 120.0, Duration::from_secs(5), |_| {
        // Unmute (mix/on 1) is the tail of the hard-reset sequence.
        !mock.received_filtered(|m| m.is_mute() == Some((1, false))).is_empty()
    });
    assert!(reset, "hard reset never completed at the mock");

    // Sequence: mute → delay-time set → unmute, in order. A racing reconnect
    // auto-sync may have landed an unrelated delay-time set *before* the hard
    // reset, so look for a set strictly between the mute and the unmute.
    let msgs = mock.received_filtered(|m| {
        m.is_mute().map(|(s, _)| s == 1).unwrap_or(false)
            || m.is_set_delay().map(|(s, _)| s == 1).unwrap_or(false)
    });
    let mute_idx = msgs.iter().position(|m| m.is_mute() == Some((1, true)));
    let unmute_idx = msgs.iter().position(|m| m.is_mute() == Some((1, false)));
    match (mute_idx, unmute_idx) {
        (Some(m), Some(u)) => {
            assert!(m < u, "unmute arrived before mute: mute {m}, unmute {u}");
            let set_between = msgs[m..u]
                .iter()
                .any(|msg| msg.is_set_delay().is_some_and(|(_, v)| v > 0.0));
            assert!(set_between, "no delay-time set between mute ({m}) and unmute ({u})");
        }
        other => panic!("hard reset incomplete: (mute, unmute) = {other:?}"),
    }

    harness.deactivate();
}

#[test]
fn rate_on_change_dispatches_after_settle() {
    let _guard = E2E_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut slots = [SlotState::empty(); 8];
    slots[0] = SlotState::dly(120.0);
    let mock = MockMixer::start_with_slots(slots);
    let mut harness = connect(&mock);
    wait_for_audit(&mut harness);

    // rate_sync_mode defaults to OnChange. Let any reconnect auto-sync drain,
    // then snapshot.
    let _ = step_until(&mut harness, 120.0, Duration::from_secs(2), |_| {
        mock.sync_count(1) > 0
    });
    std::thread::sleep(Duration::from_millis(300));
    let before = mock.sync_count(1);

    // Change tempo 120 → 100 and keep stepping past SETTLE_MS (500 ms wall
    // clock): the settle handler must dispatch the new BPM with no force pulse.
    let dispatched = step_until(&mut harness, 100.0, Duration::from_secs(4), |_| {
        mock.sync_count(1) > before
    });
    assert!(dispatched, "OnChange settle never dispatched after BPM change");
    let rx = mock.rx_bpm(1).expect("slot 1 should have a received BPM");
    assert!((rx - 100.0).abs() < 0.5, "settled dispatch should carry 100 BPM, got {rx}");

    harness.deactivate();
}

#[test]
fn telemetry_readback_drives_is_matched() {
    let _guard = E2E_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut slots = [SlotState::empty(); 8];
    slots[0] = SlotState::dly(120.0);
    let mock = MockMixer::start_with_slots(slots);
    let mut harness = connect(&mock);
    wait_for_audit(&mut harness);

    // Force a sync at the host tempo so the hardware state matches, then wait
    // for the 3 s telemetry poll to read it back into hardware_float →
    // is_matched flips true.
    harness.set_param_normalized("force_sync_rate", 1.0);
    let matched = step_until(&mut harness, 120.0, Duration::from_secs(8), |h| {
        h.param_normalized("is_matched") == Some(1.0)
    });
    assert!(matched, "is_matched never became true after telemetry read-back");

    // Host tempo moves to 100 while the hardware still holds 120 → mismatch.
    // (Keep rate mode Manual so nothing auto-corrects the drift.)
    harness.set_param_normalized("rate_sync_mode", 0.0); // Manual
    step(&mut harness, 120.0);
    let unmatched = step_until(&mut harness, 100.0, Duration::from_secs(8), |h| {
        h.param_normalized("is_matched") == Some(0.0)
    });
    assert!(unmatched, "is_matched never dropped after host/hardware drift");

    harness.deactivate();
}
