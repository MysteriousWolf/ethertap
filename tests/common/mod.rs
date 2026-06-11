// Shared by multiple test binaries — each compiles this module separately and
// uses a different subset, so per-binary dead-code/unused-import warnings are
// expected noise.
#![allow(dead_code)]
#![allow(unused_imports)]

// The mock mixer fixture lives in the `mock-suite` workspace crate (also the
// `mock-suite` CLI/TUI); this module re-exports it and keeps only the
// ethertap-specific NetworkWorker test glue.
pub use mock_suite::{
    all_dly_slots, all_empty_slots, default_slots, MockMixer, ReceivedMsg, SlotState, DLY, EMPTY,
};

use std::sync::atomic::{AtomicI32, AtomicU8, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender};
use ethertap::network::{NetworkCommand, NetworkStatus, NetworkWorker};
use parking_lot::Mutex;

/// Test-visible slice of the Arcs written by `NetworkWorker`.
/// Returned by `create_worker` so tests can inspect slot/scan data after
/// receiving the `SlotScanDone` sentinel.
pub struct WorkerShared {
    /// Bitmask: bit n set ↔ slot (n+1) compatible. Written by network worker after audit.
    pub compatible_slots: Arc<AtomicU8>,
    /// Bitmask: bit n set ↔ slot (n+1) occupied. Written by network worker after audit.
    pub occupied_slots: Arc<AtomicU8>,
    /// Per-slot type IDs (index = slot-1). i32::MIN = not yet queried.
    pub slot_types: Arc<[AtomicI32; 8]>,
    pub scan_targets: Arc<Mutex<Vec<ethertap::network::DeviceInfo>>>,
    pub connected_device: Arc<Mutex<(String, String)>>,
}

impl WorkerShared {
    /// Decode the compatible_slots bitmask into a sorted Vec<u8>.
    pub fn compatible_vec(&self) -> Vec<u8> {
        let mask = self.compatible_slots.load(Ordering::Relaxed);
        (0..8u8)
            .filter(|&b| mask & (1 << b) != 0)
            .map(|b| b + 1)
            .collect()
    }
    /// Decode the occupied_slots bitmask into a sorted Vec<u8>.
    pub fn occupied_vec(&self) -> Vec<u8> {
        let mask = self.occupied_slots.load(Ordering::Relaxed);
        (0..8u8)
            .filter(|&b| mask & (1 << b) != 0)
            .map(|b| b + 1)
            .collect()
    }
    /// Snapshot slot_types as [Option<i32>; 8] (i32::MIN → None).
    pub fn slot_types_snapshot(&self) -> [Option<i32>; 8] {
        std::array::from_fn(|i| {
            let raw = self.slot_types[i].load(Ordering::Relaxed);
            if raw == i32::MIN {
                None
            } else {
                Some(raw)
            }
        })
    }
}

pub fn create_worker(
    fx_slot: u8,
    slot_types_init: [Option<i32>; 8],
    hardware_float_out: Arc<std::sync::atomic::AtomicU32>,
) -> (
    NetworkWorker,
    Sender<NetworkCommand>,
    Receiver<NetworkStatus>,
    WorkerShared,
) {
    let (cmd_tx, cmd_rx) = crossbeam_channel::bounded::<NetworkCommand>(64);
    let (status_tx, status_rx) = crossbeam_channel::bounded::<NetworkStatus>(64);

    let compatible_slots: Arc<AtomicU8> = Arc::new(AtomicU8::new(0));
    let occupied_slots: Arc<AtomicU8> = Arc::new(AtomicU8::new(0));
    // Encode slot_types_init into atomic array (i32::MIN = None).
    let slot_types_shared: Arc<[AtomicI32; 8]> = Arc::new(std::array::from_fn(|i| {
        AtomicI32::new(slot_types_init[i].unwrap_or(i32::MIN))
    }));
    let scan_targets = Arc::new(Mutex::new(Vec::new()));
    let connected_device = Arc::new(Mutex::new((String::new(), String::new())));

    let test_shared = WorkerShared {
        compatible_slots: compatible_slots.clone(),
        occupied_slots: occupied_slots.clone(),
        slot_types: slot_types_shared.clone(),
        scan_targets: scan_targets.clone(),
        connected_device: connected_device.clone(),
    };

    let worker = NetworkWorker::new(
        cmd_rx,
        status_tx,
        Arc::new(Mutex::new(String::new())),
        Arc::new(Mutex::new(0u16)),
        Arc::new(AtomicU8::new(fx_slot)),
        ethertap::network::WorkerShared {
            hardware_float_out,
            compatible_slots,
            occupied_slots,
            slot_types: slot_types_shared,
            scan_targets,
            connected_device,
            scan_generation: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        },
    );

    (worker, cmd_tx, status_rx, test_shared)
}

pub fn spawn_worker(
    port: u16,
) -> (
    Sender<NetworkCommand>,
    Receiver<NetworkStatus>,
    thread::JoinHandle<()>,
    WorkerShared,
) {
    let hardware_float = Arc::new(std::sync::atomic::AtomicU32::new(0));
    let slot_types: [Option<i32>; 8] = [
        Some(10),
        Some(1),
        Some(10),
        Some(3),
        None,
        Some(10),
        None,
        Some(2),
    ];
    let (worker, cmd_tx, status_rx, shared) = create_worker(1, slot_types, hardware_float);

    let handle = thread::Builder::new()
        .name("ethertap-net-test".into())
        .spawn(move || worker.run())
        .expect("spawn worker");

    cmd_tx
        .send(NetworkCommand::UpdateTarget {
            ip: "127.0.0.1".to_string(),
            port,
        })
        .expect("send UpdateTarget");

    (cmd_tx, status_rx, handle, shared)
}

// ─── vst-runtime harness helpers (shared by harness_e2e / sync_matrix /
// midi_clock_tests) ──────────────────────────────────────────────────────────

pub mod harness_util {
    use std::time::{Duration, Instant};

    use ethertap::EtherTap;
    use vst_runtime::Harness;

    use super::MockMixer;

    /// Serializes harness-based tests *within one test binary*: each
    /// `EtherTap::default()` spawns real network + MIDI worker threads (and,
    /// on macOS, a CoreMIDI watcher) — never construct two concurrently.
    pub static E2E_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Drive one playing `process()` call at `tempo` BPM, 4/4, with the song
    /// position set from `pos_samples` (beat/bar fields derived).
    pub fn step_at(harness: &mut Harness<EtherTap>, tempo: f64, pos_samples: i64) {
        let mut t = harness.new_transport();
        t.playing = true;
        t.tempo = Some(tempo);
        t.time_sig_numerator = Some(4);
        t.time_sig_denominator = Some(4);
        let pos_beats = pos_samples as f64 / 44_100.0 / 60.0 * tempo;
        let bar_number = (pos_beats / 4.0).floor() as i32;
        t.set_song_position(
            Some(pos_samples),
            Some(pos_beats),
            Some(bar_number as f64 * 4.0),
            Some(bar_number),
            None,
        );
        let mut io = vec![vec![0.0f32; 256]; 2];
        let _ = harness.process(&mut io, t, &[]);
    }

    /// Drive one playing `process()` call at `tempo` BPM, 4/4, position zero.
    pub fn step(harness: &mut Harness<EtherTap>, tempo: f64) {
        step_at(harness, tempo, 0);
    }

    /// Step the plugin until `pred` holds or `timeout` elapses. Sleeps between
    /// steps so the worker threads get scheduled (their UDP round trips are
    /// real).
    pub fn step_until(
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

    /// Build a harness pointed at `mock` and connect via the
    /// `connect_to_last` trigger param. Panics if the connection doesn't
    /// establish.
    pub fn connect(mock: &MockMixer) -> Harness<EtherTap> {
        let mut harness =
            Harness::<EtherTap>::new(44_100.0, 256).expect("EtherTap should initialize");

        let params = harness.plugin().ethertap_params();
        *params.target_ip.lock() = "127.0.0.1".to_string();
        *params.target_port.lock() = mock.port();

        assert!(harness.set_param_normalized("connect_to_last", 1.0));
        let connected = step_until(&mut harness, 120.0, Duration::from_secs(5), |h| {
            h.param_normalized("is_connected") == Some(1.0)
        });
        assert!(
            connected,
            "is_connected never became true against MockMixer"
        );
        harness
    }

    /// Wait for the post-connect slot audit to land (compatible_slots != 0).
    pub fn wait_for_audit(harness: &mut Harness<EtherTap>) {
        let handles = harness.plugin().test_handles();
        let audited = step_until(harness, 120.0, Duration::from_secs(5), |_| {
            handles
                .compatible_slots
                .load(std::sync::atomic::Ordering::Relaxed)
                != 0
        });
        assert!(audited, "slot audit never completed");
    }

    /// Let the reconnect auto-sync fire and drain: on connect the plugin arms
    /// `reconnect_sync_pending` and dispatches once to all compatible slots
    /// after the slot scan lands. Tests that assert on per-slot sync counts
    /// must snapshot *after* this, or the auto-sync races their baselines.
    pub fn drain_auto_sync(harness: &mut Harness<EtherTap>, mock: &MockMixer) {
        let _ = step_until(harness, 120.0, Duration::from_secs(2), |_| {
            (1..=8u8).any(|s| mock.sync_count(s) > 0)
        });
        std::thread::sleep(Duration::from_millis(300));
    }
}

pub fn wait_for_status(
    status_rx: &Receiver<NetworkStatus>,
    timeout: Duration,
) -> Option<NetworkStatus> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        match status_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(NetworkStatus::ActivityPulse | NetworkStatus::RxPulse) => continue,
            Ok(other) => return Some(other),
            Err(_) => continue,
        }
    }
    None
}

pub fn drain_all_status(status_rx: &Receiver<NetworkStatus>) {
    while status_rx.try_recv().is_ok() {}
}

pub fn wait_for_specific_status(
    status_rx: &Receiver<NetworkStatus>,
    pred: impl Fn(&NetworkStatus) -> bool,
    timeout: Duration,
) -> Option<NetworkStatus> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        match status_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(status) => {
                if pred(&status) {
                    return Some(status);
                }
            }
            Err(_) => continue,
        }
    }
    None
}
