use ethertap::network::{NetworkCommand, NetworkStatus, NetworkWorker};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crossbeam_channel;
use parking_lot::Mutex;

fn empty_slot_types() -> Arc<Mutex<[Option<i32>; 8]>> {
    Arc::new(Mutex::new([None; 8]))
}

#[test]
#[ignore]
fn network_worker_connects_to_mock() {
    let (cmd_tx, cmd_rx) = crossbeam_channel::bounded(64);
    let (status_tx, status_rx) = crossbeam_channel::bounded(64);
    let hardware_float = Arc::new(AtomicU32::new(0));

    let worker = NetworkWorker::new(
        cmd_rx,
        status_tx,
        Arc::new(Mutex::new(1)),
        empty_slot_types(),
        hardware_float,
    );

    std::thread::spawn(move || worker.run());

    cmd_tx
        .send(NetworkCommand::UpdateTarget {
            ip: "127.0.0.1".to_string(),
            port: 10023,
        })
        .unwrap();

    let status = status_rx.recv_timeout(Duration::from_secs(5));
    assert!(
        matches!(status, Ok(NetworkStatus::Connected)),
        "Should connect to mock mixer, got {:?}",
        status
    );
}

#[test]
fn exponential_backoff_increases_on_failure() {
    let (cmd_tx, cmd_rx) = crossbeam_channel::bounded(64);
    let (status_tx, status_rx) = crossbeam_channel::bounded(64);
    let hardware_float = Arc::new(AtomicU32::new(0));

    let worker = NetworkWorker::new(
        cmd_rx,
        status_tx,
        Arc::new(Mutex::new(1)),
        empty_slot_types(),
        hardware_float,
    );

    std::thread::spawn(move || worker.run());

    cmd_tx
        .send(NetworkCommand::UpdateTarget {
            ip: "127.0.0.1".to_string(),
            port: 9,
        })
        .unwrap();

    let mut first = status_rx
        .recv_timeout(Duration::from_secs(3))
        .expect("first status should arrive within a few seconds");
    while matches!(first, NetworkStatus::ActivityPulse | NetworkStatus::RxPulse) {
        first = status_rx
            .recv_timeout(Duration::from_secs(3))
            .expect("first status should arrive within a few seconds");
    }
    assert!(
        matches!(first, NetworkStatus::Disconnected),
        "Expected Disconnected after failed heartbeat, got {:?}",
        first
    );

    let start = std::time::Instant::now();
    let mut second = status_rx
        .recv_timeout(Duration::from_secs(8))
        .expect("second status should arrive within backoff window");
    while matches!(
        second,
        NetworkStatus::ActivityPulse | NetworkStatus::RxPulse
    ) {
        second = status_rx
            .recv_timeout(Duration::from_secs(8))
            .expect("second status should arrive within backoff window");
    }
    assert!(
        matches!(second, NetworkStatus::Disconnected),
        "Expected second Disconnected, got {:?}",
        second
    );

    let elapsed = start.elapsed();
    assert!(
        elapsed >= Duration::from_secs(2),
        "Backoff should delay retries; elapsed was {:?}",
        elapsed
    );
}

#[test]
#[ignore]
fn osc_protocol_delay_readback() {
    let (cmd_tx, cmd_rx) = crossbeam_channel::bounded(64);
    let (status_tx, status_rx) = crossbeam_channel::bounded(64);
    let hardware_float = Arc::new(AtomicU32::new(0));

    let slot_types = empty_slot_types();
    slot_types.lock()[0] = Some(10);

    let worker = NetworkWorker::new(
        cmd_rx,
        status_tx,
        Arc::new(Mutex::new(1)),
        slot_types,
        hardware_float.clone(),
    );

    std::thread::spawn(move || worker.run());

    cmd_tx
        .send(NetworkCommand::UpdateTarget {
            ip: "127.0.0.1".to_string(),
            port: 10023,
        })
        .unwrap();

    loop {
        match status_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(NetworkStatus::Connected) => break,
            Ok(_) => continue,
            Err(e) => panic!("Timed out waiting for Connected: {}", e),
        }
    }

    let mut found = false;
    for _ in 0..20 {
        if let Ok(NetworkStatus::DelayReadback(value)) = status_rx.try_recv() {
            let expected = 20.0 / 120.0;
            assert!(
                (value - expected).abs() < 0.001,
                "Expected delay readback ~{}, got {}",
                expected,
                value
            );
            found = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(250));
    }

    assert!(
        found,
        "Should have received a DelayReadback from mock mixer"
    );

    let bits = hardware_float.load(Ordering::Acquire);
    let hw_value = f32::from_bits(bits);
    let expected = 20.0 / 120.0;
    assert!(
        (hw_value - expected).abs() < 0.001,
        "hardware_float should reflect readback value, got {}",
        hw_value
    );
}
