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
    pub occupied_slots:   Arc<AtomicU8>,
    /// Per-slot type IDs (index = slot-1). i32::MIN = not yet queried.
    pub slot_types:       Arc<[AtomicI32; 8]>,
    pub scan_targets:     Arc<Mutex<Vec<ethertap::network::DeviceInfo>>>,
    pub connected_device: Arc<Mutex<(String, String)>>,
}

impl WorkerShared {
    /// Decode the compatible_slots bitmask into a sorted Vec<u8>.
    pub fn compatible_vec(&self) -> Vec<u8> {
        let mask = self.compatible_slots.load(Ordering::Relaxed);
        (0..8u8).filter(|&b| mask & (1 << b) != 0).map(|b| b + 1).collect()
    }
    /// Decode the occupied_slots bitmask into a sorted Vec<u8>.
    pub fn occupied_vec(&self) -> Vec<u8> {
        let mask = self.occupied_slots.load(Ordering::Relaxed);
        (0..8u8).filter(|&b| mask & (1 << b) != 0).map(|b| b + 1).collect()
    }
    /// Snapshot slot_types as [Option<i32>; 8] (i32::MIN → None).
    pub fn slot_types_snapshot(&self) -> [Option<i32>; 8] {
        std::array::from_fn(|i| {
            let raw = self.slot_types[i].load(Ordering::Relaxed);
            if raw == i32::MIN { None } else { Some(raw) }
        })
    }
}

pub fn create_worker(
    fx_slot: u8,
    slot_types_init: [Option<i32>; 8],
    hardware_float_out: Arc<std::sync::atomic::AtomicU32>,
) -> (NetworkWorker, Sender<NetworkCommand>, Receiver<NetworkStatus>, WorkerShared) {
    let (cmd_tx, cmd_rx) = crossbeam_channel::bounded::<NetworkCommand>(64);
    let (status_tx, status_rx) = crossbeam_channel::bounded::<NetworkStatus>(64);

    let compatible_slots: Arc<AtomicU8> = Arc::new(AtomicU8::new(0));
    let occupied_slots: Arc<AtomicU8>   = Arc::new(AtomicU8::new(0));
    // Encode slot_types_init into atomic array (i32::MIN = None).
    let slot_types_shared: Arc<[AtomicI32; 8]> =
        Arc::new(std::array::from_fn(|i| AtomicI32::new(slot_types_init[i].unwrap_or(i32::MIN))));
    let scan_targets      = Arc::new(Mutex::new(Vec::new()));
    let connected_device  = Arc::new(Mutex::new((String::new(), String::new())));

    let test_shared = WorkerShared {
        compatible_slots:  compatible_slots.clone(),
        occupied_slots:    occupied_slots.clone(),
        slot_types:        slot_types_shared.clone(),
        scan_targets:      scan_targets.clone(),
        connected_device:  connected_device.clone(),
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
) -> (Sender<NetworkCommand>, Receiver<NetworkStatus>, thread::JoinHandle<()>, WorkerShared) {
    let hardware_float = Arc::new(std::sync::atomic::AtomicU32::new(0));
    let slot_types: [Option<i32>; 8] = [
        Some(10), Some(1), Some(10), Some(3),
        None, Some(10), None, Some(2),
    ];
    let (worker, cmd_tx, status_rx, shared) = create_worker(1, slot_types, hardware_float);

    let handle = thread::Builder::new()
        .name("ethertap-net-test".into())
        .spawn(move || worker.run())
        .expect("spawn worker");

    cmd_tx.send(NetworkCommand::UpdateTarget {
        ip: "127.0.0.1".to_string(), port,
    }).expect("send UpdateTarget");

    (cmd_tx, status_rx, handle, shared)
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
