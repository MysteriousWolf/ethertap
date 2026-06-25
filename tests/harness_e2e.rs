//! End-to-end automation tests: EtherTap driven through the vst-runtime
//! harness, talking to a real `MockMixer` over UDP/OSC.
//!
//! Flow under test: host-style param write → `process()` rising edge →
//! network worker command → OSC at the mock → status read-back into the
//! `is_connected`/`is_matched` params via `context.set_parameter`.
//!
//! Each `EtherTap::default()` spawns real network + MIDI worker threads (and,
//! on macOS, a CoreMIDI watcher). Tests serialize on `E2E_LOCK` so multiple
//! plugin instances never construct concurrently.
//!
//! Exercises the VST3 (host-transport) build: standalone builds source BPM
//! and play state from UI atomics instead of the transport these tests drive.
#![cfg(not(feature = "standalone"))]

mod common;

use std::time::Duration;

use common::MockMixer;
use common::harness_util::{E2E_LOCK, connect, step_until};

#[test]
fn connect_disconnect_lifecycle_via_trigger_params() {
    let _guard = E2E_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mock = MockMixer::start();
    let mut harness = connect(&mock);

    // The worker heartbeats /info at the mock.
    assert!(
        mock.wait_for_addr("/info", Duration::from_secs(2)),
        "mock never received /info heartbeat"
    );

    // Trigger params self-reset after consumption.
    assert_eq!(harness.param_normalized("connect_to_last"), Some(0.0));

    // Connect armed all-slots mode (process() writes the param on the
    // connect transition).
    assert_eq!(harness.param_normalized("all_slots"), Some(1.0));

    // Disconnect via the trigger param; is_connected drops.
    assert!(harness.set_param_normalized("disconnect", 1.0));
    let disconnected = step_until(&mut harness, 120.0, Duration::from_secs(5), |h| {
        h.param_normalized("is_connected") == Some(0.0)
    });
    assert!(
        disconnected,
        "is_connected never dropped after disconnect pulse"
    );
    assert_eq!(harness.param_normalized("disconnect"), Some(0.0));

    harness.deactivate();
}

#[test]
fn force_sync_rate_dispatches_osc_to_compatible_slots() {
    let _guard = E2E_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mock = MockMixer::start();
    let mut harness = connect(&mock);
    let handles = harness.plugin().test_handles();

    // Connect fired AuditSlots; wait for the audit to land. default_slots has
    // DLY in slots 1, 3, 6 — but `osc::is_bpm_compatible` restricts BPM sync
    // to slots 1–4 (X32 constraint), so only 1 and 3 are dispatch targets.
    let audited = step_until(&mut harness, 120.0, Duration::from_secs(5), |_| {
        handles
            .compatible_slots
            .load(std::sync::atomic::Ordering::Relaxed)
            != 0
    });
    assert!(audited, "slot audit never completed");

    // The reconnect auto-sync may already have fired; snapshot counts, then
    // pulse force_sync_rate and assert exactly the compatible slots advance.
    let before: Vec<u64> = (1..=8).map(|s| mock.sync_count(s)).collect();

    assert!(harness.set_param_normalized("force_sync_rate", 1.0));
    let synced = step_until(&mut harness, 120.0, Duration::from_secs(5), |_| {
        mock.sync_count(1) > before[0]
    });
    assert!(synced, "force_sync_rate never produced a sync at the mock");
    // Param self-reset.
    assert_eq!(harness.param_normalized("force_sync_rate"), Some(0.0));

    // All-slots mode: every compatible DLY slot (1, 3) advanced; incompatible
    // slots (non-delay types, empties, and slots above 4) did not.
    // Allow the remaining sends to drain.
    std::thread::sleep(Duration::from_millis(300));
    for &slot in &[1u8, 3] {
        assert!(
            mock.sync_count(slot) > before[(slot - 1) as usize],
            "compatible slot {slot} did not receive a sync"
        );
    }
    for &slot in &[2u8, 4, 5, 6, 7, 8] {
        assert_eq!(
            mock.sync_count(slot),
            before[(slot - 1) as usize],
            "incompatible slot {slot} unexpectedly received a sync"
        );
    }

    // The dispatched delay float encodes the host tempo (120 BPM → 20/120).
    assert!(
        (mock.rx_bpm(1).expect("slot 1 should have a received BPM") - 120.0).abs() < 0.5,
        "dispatched BPM should match host tempo"
    );

    harness.deactivate();
}
