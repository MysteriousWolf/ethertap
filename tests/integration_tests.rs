use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use ethertap::network::{NetworkCommand, NetworkStatus, NetworkWorker};

mod common;
use common::*;

#[test]
fn network_worker_connects_to_mock() {
    let mixer = MockMixer::start();
    let (cmd_tx, status_rx, handle, _shared) = spawn_worker(mixer.port());

    let status = wait_for_status(&status_rx, Duration::from_secs(5));
    assert!(
        matches!(status, Some(NetworkStatus::Connected)),
        "Should connect to mock mixer, got {:?}",
        status
    );

    drop(cmd_tx);
    handle.join().expect("worker thread panicked");
}

#[test]
fn network_worker_fails_to_connect() {
    let (cmd_tx, status_rx, handle, _shared) = spawn_worker(9999);

    let status = wait_for_status(&status_rx, Duration::from_secs(3));
    assert!(
        matches!(status, Some(NetworkStatus::Disconnected)),
        "Should report Disconnected when no mixer on port, got {:?}",
        status
    );

    drop(cmd_tx);
    handle.join().expect("worker thread panicked");
}

#[test]
fn exponential_backoff_increases_on_failure() {
    let (cmd_tx, status_rx, handle, _shared) = spawn_worker(9999);

    let first = wait_for_status(&status_rx, Duration::from_secs(5));
    assert!(matches!(first, Some(NetworkStatus::Disconnected)));

    let start = Instant::now();
    let second = wait_for_status(&status_rx, Duration::from_secs(8));
    let elapsed = start.elapsed();

    assert!(matches!(second, Some(NetworkStatus::Disconnected)));
    assert!(
        elapsed >= Duration::from_secs(1),
        "Backoff should delay retries; elapsed was {:?}",
        elapsed
    );

    drop(cmd_tx);
    handle.join().expect("worker thread panicked");
}

#[test]
fn mock_receives_heartbeat() {
    let mixer = MockMixer::start();
    let (cmd_tx, _status_rx, handle, _shared) = spawn_worker(mixer.port());

    let found = mixer.wait_for_addr("/info", Duration::from_secs(10));
    assert!(
        found,
        "Mock mixer should receive /info heartbeat from worker"
    );

    drop(cmd_tx);
    handle.join().expect("worker thread panicked");
}

#[test]
fn sync_now_sets_delay_on_mock() {
    let mixer = MockMixer::start_with_slots(all_dly_slots());
    let (cmd_tx, status_rx, handle, _shared) = spawn_worker(mixer.port());

    assert!(wait_for_status(&status_rx, Duration::from_secs(5))
        .is_some_and(|s| matches!(s, NetworkStatus::Connected)));

    mixer.clear_msgs();

    cmd_tx
        .send(NetworkCommand::SyncNow {
            slot: 1,
            bpm: 120.0,
        })
        .unwrap();
    std::thread::sleep(Duration::from_millis(300));

    let set_msgs: Vec<_> = mixer.received_filtered(|m| m.is_set_delay().is_some());
    assert_eq!(
        set_msgs.len(),
        1,
        "Expected exactly 1 delay-set message, got {}",
        set_msgs.len()
    );
    let (slot, value) = set_msgs[0].is_set_delay().unwrap();
    assert_eq!(slot, 1, "Should set slot 1");
    let expected = 20.0_f32 / 120.0;
    assert!(
        (value - expected).abs() < 0.001,
        "Delay value should be ~{expected}, got {value}"
    );

    drop(cmd_tx);
    handle.join().expect("worker thread panicked");
}

#[test]
fn hard_reset_mutes_sets_unmutes() {
    let mixer = MockMixer::start_with_slots(all_dly_slots());
    let (cmd_tx, status_rx, handle, _shared) = spawn_worker(mixer.port());

    assert!(wait_for_status(&status_rx, Duration::from_secs(5))
        .is_some_and(|s| matches!(s, NetworkStatus::Connected)));

    mixer.clear_msgs();

    let slots: [Option<u8>; 8] = [Some(1), None, Some(3), None, None, None, None, None];
    cmd_tx
        .send(NetworkCommand::HardResetBatch { slots, bpm: 120.0 })
        .unwrap();
    std::thread::sleep(Duration::from_millis(350));

    // Collect the ordered message log once so we can verify both count and sequence.
    let all_msgs: Vec<_> = mixer.received_filtered(|_| true);

    let mute_pos = all_msgs
        .iter()
        .position(|m| m.is_mute().is_some_and(|(_, v)| v));
    let set_pos = all_msgs.iter().position(|m| m.is_set_delay().is_some());
    let umute_pos = all_msgs
        .iter()
        .position(|m| m.is_mute().is_some_and(|(_, v)| !v));

    assert!(mute_pos.is_some(), "Expected at least one mute");
    assert!(set_pos.is_some(), "Expected at least one delay-set");
    assert!(umute_pos.is_some(), "Expected at least one unmute");
    assert!(
        mute_pos.unwrap() < set_pos.unwrap() && set_pos.unwrap() < umute_pos.unwrap(),
        "Message order must be: mute → set_delay → unmute (got mute@{}, set@{}, unmute@{})",
        mute_pos.unwrap(),
        set_pos.unwrap(),
        umute_pos.unwrap()
    );

    let mutes: Vec<_> = all_msgs
        .iter()
        .filter(|m| m.is_mute().is_some_and(|(_, v)| v))
        .collect();
    assert_eq!(
        mutes.len(),
        2,
        "Should receive 2 mute commands, got {}",
        mutes.len()
    );
    for m in &mutes {
        let (slot, _) = m.is_mute().unwrap();
        assert!(
            slot == 1 || slot == 3,
            "Only slots 1 and 3 should be muted, got {slot}"
        );
    }

    let sets: Vec<_> = all_msgs
        .iter()
        .filter(|m| m.is_set_delay().is_some())
        .collect();
    assert_eq!(
        sets.len(),
        2,
        "Should receive 2 delay-set messages, got {}",
        sets.len()
    );

    let unmutes: Vec<_> = all_msgs
        .iter()
        .filter(|m| m.is_mute().is_some_and(|(_, v)| !v))
        .collect();
    assert_eq!(
        unmutes.len(),
        2,
        "Should receive 2 unmute commands, got {}",
        unmutes.len()
    );
    // Every muted slot must have a corresponding unmute — catches bugs where
    // one slot unmutes twice and another stays muted.
    for m in &mutes {
        let (muted_slot, _) = m.is_mute().unwrap();
        assert!(
            unmutes.iter().any(|u| u.is_mute().unwrap().0 == muted_slot),
            "No unmute received for slot {muted_slot}",
        );
    }

    assert_eq!(mixer.sync_count(1), 1, "Slot 1 should have 1 sync");
    assert_eq!(mixer.sync_count(3), 1, "Slot 3 should have 1 sync");

    drop(cmd_tx);
    handle.join().expect("worker thread panicked");
}

#[test]
fn disconnect_and_reconnect() {
    let mixer = MockMixer::start();
    let (cmd_tx, status_rx, handle, _shared) = spawn_worker(mixer.port());

    assert!(wait_for_status(&status_rx, Duration::from_secs(5))
        .is_some_and(|s| matches!(s, NetworkStatus::Connected)));
    drain_all_status(&status_rx);

    cmd_tx.send(NetworkCommand::Disconnect).unwrap();
    assert!(
        wait_for_specific_status(
            &status_rx,
            |s| matches!(s, NetworkStatus::Disconnected),
            Duration::from_secs(5)
        )
        .is_some(),
        "Should disconnect after Disconnect command"
    );

    cmd_tx
        .send(NetworkCommand::UpdateTarget {
            ip: "127.0.0.1".to_string(),
            port: mixer.port(),
        })
        .unwrap();
    assert!(
        wait_for_specific_status(
            &status_rx,
            |s| matches!(s, NetworkStatus::Connected),
            Duration::from_secs(5)
        )
        .is_some(),
        "Should reconnect"
    );

    drop(cmd_tx);
    handle.join().expect("worker thread panicked");
}

#[test]
fn audit_slots_discovers_compatible_slots() {
    let mixer = MockMixer::start();
    let (cmd_tx, status_rx, handle, shared) = spawn_worker(mixer.port());

    assert!(wait_for_status(&status_rx, Duration::from_secs(5))
        .is_some_and(|s| matches!(s, NetworkStatus::Connected)));
    drain_all_status(&status_rx);

    cmd_tx.send(NetworkCommand::AuditSlots).unwrap();

    // SlotScanDone is the allocation-free sentinel; actual data is in shared mutexes.
    let done = wait_for_specific_status(
        &status_rx,
        |s| matches!(s, NetworkStatus::SlotScanDone),
        Duration::from_secs(5),
    );
    assert!(done.is_some(), "Expected SlotScanDone sentinel");

    let compatible = shared.compatible_vec();
    let occupied = shared.occupied_vec();
    let slot_types = shared.slot_types_snapshot();

    assert_eq!(
        compatible,
        vec![1u8, 3],
        "Should find DLY slots 1 and 3 (bus slots) as compatible, got {:?}",
        compatible
    );
    assert!(occupied.contains(&1), "Slot 1 should be occupied");
    assert!(occupied.contains(&2), "Slot 2 should be occupied");
    assert!(!occupied.contains(&5), "Slot 5 should be empty");
    assert!(!occupied.contains(&7), "Slot 7 should be empty");
    assert_eq!(slot_types[0], Some(10));
    assert_eq!(slot_types[1], Some(1));
    assert_eq!(slot_types[4], None);

    drop(cmd_tx);
    handle.join().expect("worker thread panicked");
}

#[test]
fn telemetry_poll_updates_hardware_float() {
    let mixer = MockMixer::start_with_slots(all_dly_slots());
    let hardware_float = Arc::new(AtomicU32::new(0));
    let slot_types: [Option<i32>; 8] = [Some(10); 8];

    let (cmd_tx, cmd_rx) = crossbeam_channel::bounded(64);
    let (status_tx, status_rx) = crossbeam_channel::bounded(64);

    let worker = NetworkWorker::new(
        cmd_rx,
        status_tx,
        Arc::new(parking_lot::Mutex::new(String::new())),
        Arc::new(parking_lot::Mutex::new(0u16)),
        Arc::new(std::sync::atomic::AtomicU8::new(1u8)),
        ethertap::network::WorkerShared {
            hardware_float_out: hardware_float.clone(),
            compatible_slots: Arc::new(std::sync::atomic::AtomicU8::new(0)),
            occupied_slots: Arc::new(std::sync::atomic::AtomicU8::new(0)),
            slot_types: Arc::new(std::array::from_fn(|i| {
                std::sync::atomic::AtomicI32::new(slot_types[i].unwrap_or(i32::MIN))
            })),
            scan_targets: Arc::new(parking_lot::Mutex::new(Vec::new())),
            connected_device: Arc::new(parking_lot::Mutex::new((String::new(), String::new()))),
            scan_generation: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        },
    );
    let handle = std::thread::Builder::new()
        .name("ethertap-net-test".into())
        .spawn(move || worker.run())
        .expect("spawn");

    cmd_tx
        .send(NetworkCommand::UpdateTarget {
            ip: "127.0.0.1".to_string(),
            port: mixer.port(),
        })
        .unwrap();

    loop {
        match status_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(NetworkStatus::Connected) => break,
            Ok(_) => continue,
            Err(e) => panic!("Timed out waiting for Connected: {}", e),
        }
    }

    let deadline = Instant::now() + Duration::from_secs(10);
    let mut found = false;
    while Instant::now() < deadline {
        if let Ok(NetworkStatus::DelayReadback(value)) = status_rx.try_recv() {
            let expected = 20.0 / 120.0;
            assert!(
                (value - expected as f32).abs() < 0.001,
                "Expected delay readback ~{}, got {}",
                expected,
                value
            );
            found = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    assert!(
        found,
        "Should have received a DelayReadback from mock mixer"
    );

    let bits = hardware_float.load(Ordering::Acquire);
    let hw_value = f32::from_bits(bits);
    let expected = 20.0 / 120.0;
    assert!(
        (hw_value - expected as f32).abs() < 0.001,
        "hardware_float should reflect readback value, got {}",
        hw_value
    );

    drop(cmd_tx);
    handle.join().expect("worker thread panicked");
}

#[test]
fn audit_slots_empty_mixer() {
    let mixer = MockMixer::start_with_slots(all_empty_slots());
    let (cmd_tx, status_rx, handle, shared) = spawn_worker(mixer.port());

    assert!(wait_for_status(&status_rx, Duration::from_secs(5))
        .is_some_and(|s| matches!(s, NetworkStatus::Connected)));
    drain_all_status(&status_rx);

    cmd_tx.send(NetworkCommand::AuditSlots).unwrap();

    let done = wait_for_specific_status(
        &status_rx,
        |s| matches!(s, NetworkStatus::SlotScanDone),
        Duration::from_secs(5),
    );
    assert!(done.is_some(), "Expected SlotScanDone sentinel");

    let compatible = shared.compatible_vec();
    let occupied = shared.occupied_vec();
    let slot_types = shared.slot_types_snapshot();

    assert!(
        compatible.is_empty(),
        "No compatible slots expected, got {:?}",
        compatible
    );
    assert!(
        occupied.is_empty(),
        "No occupied slots expected, got {:?}",
        occupied
    );
    assert!(
        slot_types.iter().all(|t| t.is_none()),
        "All slot types should be None for empty mixer"
    );

    drop(cmd_tx);
    handle.join().expect("worker thread panicked");
}

/// A LAN scan with an overridden probe port must discover a mock mixer bound
/// to an OS-assigned port — this is what lets reconnect tests rediscover a
/// device whose IP/port moved without colliding on the fixed real port 10023.
#[test]
fn scan_discovers_mock_on_custom_scan_port() {
    let mixer = MockMixer::start_with_identity(0, default_slots(), "Scan Target", "X32")
        .expect("bind mock mixer");

    let hardware_float = Arc::new(AtomicU32::new(0));
    let (mut worker, cmd_tx, status_rx, shared) = create_worker(1, [None; 8], hardware_float);
    worker.set_scan_port(mixer.port());

    let handle = std::thread::Builder::new()
        .name("ethertap-net-scan-test".into())
        .spawn(move || worker.run())
        .expect("spawn worker");

    cmd_tx.send(NetworkCommand::ScanTargets).unwrap();
    let done = wait_for_specific_status(
        &status_rx,
        |s| matches!(s, NetworkStatus::ScanDone),
        Duration::from_secs(5),
    );
    assert!(done.is_some(), "Expected ScanDone after ScanTargets");

    let targets = shared.scan_targets.lock().clone();
    let found = targets
        .iter()
        .find(|d| d.name == "Scan Target" && d.model == "X32");
    let found = found.unwrap_or_else(|| panic!("mock not discovered; targets: {targets:?}"));
    assert_eq!(
        found.port,
        mixer.port(),
        "DeviceInfo must carry the scan port"
    );

    drop(cmd_tx);
    handle.join().expect("worker thread panicked");
}
