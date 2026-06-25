//! auto_reconnect behavior: opt-in self-connect on startup, identity
//! verification against the persisted (name, model), and rescan-based
//! retargeting when the device moved or an imposter answers.

use std::sync::atomic::{AtomicBool, AtomicU32};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use ethertap::network::NetworkStatus;
use parking_lot::Mutex;

mod common;
use common::*;

/// Worker fixture with controllable persisted state and auto_reconnect atom.
struct AutoFixture {
    cmd_tx: crossbeam_channel::Sender<ethertap::network::NetworkCommand>,
    status_rx: crossbeam_channel::Receiver<NetworkStatus>,
    handle: thread::JoinHandle<()>,
    target_ip: Arc<Mutex<String>>,
    target_port: Arc<Mutex<u16>>,
    last_device: Arc<Mutex<(String, String)>>,
}

fn spawn_auto_worker(
    ip: &str,
    port: u16,
    auto: bool,
    last_device: (String, String),
    scan_port: u16,
) -> AutoFixture {
    let target_ip = Arc::new(Mutex::new(ip.to_string()));
    let target_port = Arc::new(Mutex::new(port));
    let auto_reconnect = Arc::new(AtomicBool::new(auto));
    let last_device = Arc::new(Mutex::new(last_device));

    let (mut worker, cmd_tx, status_rx, _shared) = create_worker_full(
        1,
        [None; 8],
        Arc::new(AtomicU32::new(0)),
        target_ip.clone(),
        target_port.clone(),
        auto_reconnect.clone(),
        last_device.clone(),
    );
    worker.set_scan_port(scan_port);

    let handle = thread::Builder::new()
        .name("ethertap-net-auto-test".into())
        .spawn(move || worker.run())
        .expect("spawn worker");

    AutoFixture {
        cmd_tx,
        status_rx,
        handle,
        target_ip,
        target_port,
        last_device,
    }
}

/// With the atom ON and a persisted target, the worker must connect by itself
/// — no ConnectToLast pulse, no UpdateTarget command — and adopt the device
/// identity into the persisted last_device slot.
#[test]
fn auto_reconnect_connects_on_startup_without_pulse() {
    let mixer = MockMixer::start();
    let fx = spawn_auto_worker(
        "127.0.0.1",
        mixer.port(),
        true,
        (String::new(), String::new()),
        mixer.port(),
    );

    let status = wait_for_status(&fx.status_rx, Duration::from_secs(5));
    assert!(
        matches!(status, Some(NetworkStatus::Connected)),
        "auto_reconnect=ON should self-connect, got {status:?}"
    );
    // Identity adopted (write-through persist).
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while fx.last_device.lock().0.is_empty() && std::time::Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        *fx.last_device.lock(),
        ("Mock Console".to_string(), "X32".to_string()),
        "device identity should be adopted on first connect"
    );

    drop(fx.cmd_tx);
    fx.handle.join().expect("worker thread panicked");
}

/// With the atom OFF, the worker must stay silent: no probe ever reaches the
/// mixer even though a valid persisted target exists.
#[test]
fn no_auto_reconnect_when_disabled() {
    let mixer = MockMixer::start();
    let fx = spawn_auto_worker(
        "127.0.0.1",
        mixer.port(),
        false,
        (String::new(), String::new()),
        mixer.port(),
    );

    // Poll for 1 500 ms: we want to confirm no /info ever arrives, not just that
    // it hasn't arrived yet.  Fixed sleep risks a race where the worker hasn't
    // started yet; polling with early-exit catches any probe that does sneak through.
    let deadline = std::time::Instant::now() + Duration::from_millis(1_500);
    while std::time::Instant::now() < deadline {
        assert_eq!(
            mixer.count_addr("/info"),
            0,
            "auto_reconnect=OFF must not probe the mixer on startup"
        );
        thread::sleep(Duration::from_millis(50));
    }

    drop(fx.cmd_tx);
    fx.handle.join().expect("worker thread panicked");
}

/// The persisted target points at a *different* device (imposter) than the
/// persisted identity. The worker must reject it, rescan, find the matching
/// device at its new address, write the new target through to the persisted
/// mutexes, and connect there.
#[test]
fn identity_mismatch_rescans_to_matching_device() {
    let imposter = MockMixer::start_with_identity(0, default_slots(), "Imposter", "X32")
        .expect("bind imposter mock");
    let real = MockMixer::start_with_identity(0, default_slots(), "Real Desk", "X32")
        .expect("bind real mock");

    // Persisted state: last session was connected to "Real Desk", but the
    // persisted address now belongs to the imposter (DHCP moved things).
    let fx = spawn_auto_worker(
        "127.0.0.1",
        imposter.port(),
        true,
        ("Real Desk".to_string(), "X32".to_string()),
        real.port(), // scan probes reach the real device's port
    );

    let connected = wait_for_specific_status(
        &fx.status_rx,
        |s| matches!(s, NetworkStatus::Connected),
        Duration::from_secs(10),
    );
    assert!(connected.is_some(), "should connect after rescan");

    // The worker must end up targeting the real device, write-through persisted.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while *fx.target_port.lock() != real.port() && std::time::Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(
        *fx.target_port.lock(),
        real.port(),
        "persisted target_port must be retargeted to the real device"
    );
    assert_eq!(*fx.target_ip.lock(), "127.0.0.1");
    assert!(
        real.wait_for_addr("/info", Duration::from_secs(2)),
        "real device should be receiving heartbeats after retarget"
    );
    // Identity stays the real device's, not the imposter's.
    assert_eq!(
        *fx.last_device.lock(),
        ("Real Desk".to_string(), "X32".to_string())
    );

    drop(fx.cmd_tx);
    fx.handle.join().expect("worker thread panicked");
}

/// An explicit user connect (ConnectToLast) to a different device must ADOPT
/// the new identity rather than bounce away from the user's choice.
#[test]
fn explicit_connect_adopts_new_identity() {
    let other =
        MockMixer::start_with_identity(0, default_slots(), "Other Desk", "M32").expect("bind mock");

    let fx = spawn_auto_worker(
        "127.0.0.1",
        other.port(),
        true,
        ("Old Desk".to_string(), "X32".to_string()),
        other.port(),
    );
    // Explicit user action: ConnectToLast targets the persisted ip/port.
    fx.cmd_tx
        .send(ethertap::network::NetworkCommand::ConnectToLast)
        .unwrap();

    let connected = wait_for_specific_status(
        &fx.status_rx,
        |s| matches!(s, NetworkStatus::Connected),
        Duration::from_secs(5),
    );
    assert!(connected.is_some(), "explicit connect should succeed");

    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while fx.last_device.lock().0 != "Other Desk" && std::time::Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        *fx.last_device.lock(),
        ("Other Desk".to_string(), "M32".to_string()),
        "explicit connect must adopt the new device identity"
    );

    drop(fx.cmd_tx);
    fx.handle.join().expect("worker thread panicked");
}

// ─── Harness-level (full plugin) ─────────────────────────────────────────────

/// Default build of the plugin: auto_reconnect defaults OFF, so constructing
/// and processing must not probe the persisted target. Flipping the param ON
/// through the host interface must bring the connection up by itself.
#[cfg(not(feature = "standalone"))]
#[test]
fn plugin_auto_reconnect_param_drives_connection() {
    use vst_runtime::Harness;

    let _guard = harness_util::E2E_LOCK.lock().unwrap();
    let mixer = MockMixer::start();

    let mut harness =
        Harness::<ethertap::EtherTap>::new(44_100.0, 256).expect("EtherTap should initialize");
    let params = harness.plugin().ethertap_params();
    *params.target_ip.lock() = "127.0.0.1".to_string();
    *params.target_port.lock() = mixer.port();

    // OFF (default): step for a while, nothing must reach the mixer.
    for _ in 0..20 {
        harness_util::step(&mut harness, 120.0);
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        mixer.count_addr("/info"),
        0,
        "auto_reconnect defaults OFF — no startup probe allowed"
    );

    // ON: the worker picks the atom up and self-connects.
    assert!(harness.set_param_normalized("auto_reconnect", 1.0));
    let connected = harness_util::step_until(&mut harness, 120.0, Duration::from_secs(5), |h| {
        h.param_normalized("is_connected") == Some(1.0)
    });
    assert!(
        connected,
        "enabling auto_reconnect should bring the link up"
    );
}
