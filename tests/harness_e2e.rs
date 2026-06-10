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

mod common;

use std::sync::Mutex;
use std::time::{Duration, Instant};

use common::MockMixer;
use ethertap::EtherTap;
use vst_runtime::Harness;

static E2E_LOCK: Mutex<()> = Mutex::new(());

/// Drive one playing `process()` call at `tempo` BPM, 4/4.
fn step(harness: &mut Harness<EtherTap>, tempo: f64) {
    let mut t = harness.new_transport();
    t.playing = true;
    t.tempo = Some(tempo);
    t.time_sig_numerator = Some(4);
    t.time_sig_denominator = Some(4);
    let mut io = vec![vec![0.0f32; 256]; 2];
    let _ = harness.process(&mut io, t, &[]);
}

/// Step the plugin until `pred` holds or `timeout` elapses. Returns whether
/// the predicate was satisfied. Sleeps between steps so the worker threads
/// get scheduled (their UDP round trips are real).
fn step_until(
    harness: &mut Harness<EtherTap>,
    tempo: f64,
    timeout: Duration,
    mut pred: impl FnMut(&Harness<EtherTap>) -> bool,
) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        step(harness, tempo);
        if pred(harness) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Build a harness pointed at `mock` and connect via the `connect_to_last`
/// trigger param. Panics if the connection doesn't establish.
fn connect(mock: &MockMixer) -> Harness<EtherTap> {
    let mut harness =
        Harness::<EtherTap>::new(44_100.0, 256).expect("EtherTap should initialize");

    let params = harness.plugin().ethertap_params();
    *params.target_ip.lock() = "127.0.0.1".to_string();
    *params.target_port.lock() = mock.port();

    assert!(harness.set_param_normalized("connect_to_last", 1.0));
    let connected = step_until(&mut harness, 120.0, Duration::from_secs(5), |h| {
        h.param_normalized("is_connected") == Some(1.0)
    });
    assert!(connected, "is_connected never became true against MockMixer");
    harness
}

#[test]
fn connect_disconnect_lifecycle_via_trigger_params() {
    let _guard = E2E_LOCK.lock().unwrap();
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
    assert!(disconnected, "is_connected never dropped after disconnect pulse");
    assert_eq!(harness.param_normalized("disconnect"), Some(0.0));

    harness.deactivate();
}

#[test]
fn force_sync_rate_dispatches_osc_to_compatible_slots() {
    let _guard = E2E_LOCK.lock().unwrap();
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
