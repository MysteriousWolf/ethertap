use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU8, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use parking_lot::Mutex;

use crossbeam_channel::{Receiver, Sender};
use ethertap::network::{NetworkCommand, NetworkStatus, NetworkWorker};
use rosc::{encoder, decoder, OscMessage, OscPacket, OscType};

pub const DLY: i32 = 10;
pub const EMPTY: i32 = -1;

#[derive(Debug, Clone, Copy)]
pub struct SlotState {
    pub type_id: i32,
    pub init_bpm: Option<f64>,
    pub rx_bpm: Option<f64>,
    pub sync_count: u64,
}

impl SlotState {
    pub fn dly(init_bpm: f64) -> Self {
        Self { type_id: DLY, init_bpm: Some(init_bpm), rx_bpm: None, sync_count: 0 }
    }
    pub fn empty() -> Self {
        Self { type_id: EMPTY, init_bpm: None, rx_bpm: None, sync_count: 0 }
    }
    pub fn other(type_id: i32) -> Self {
        Self { type_id, init_bpm: None, rx_bpm: None, sync_count: 0 }
    }
    pub fn delay_float(self) -> f32 {
        self.init_bpm.map_or(0.0, |bpm| {
            let beat_ms = 60_000.0 / bpm;
            (beat_ms / 3000.0).clamp(0.0, 1.0) as f32
        })
    }
}

pub struct MockMixer {
    shutdown: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
    pub port: u16,
    pub received_msgs: Arc<Mutex<Vec<ReceivedMsg>>>,
    pub slots: Arc<Mutex<[SlotState; 8]>>,
}

#[derive(Debug, Clone)]
pub struct ReceivedMsg {
    pub addr: String,
    pub args: Vec<OscType>,
    #[allow(dead_code)]
    pub from: SocketAddr,
    #[allow(dead_code)]
    pub timestamp: Instant,
}

impl ReceivedMsg {
    pub fn is_set_delay(&self) -> Option<(u8, f32)> {
        let parts: Vec<&str> = self.addr.split('/').collect();
        if parts.len() == 5 && parts[1] == "fx" && parts[3] == "par" {
            let slot: u8 = parts[2].parse().ok()?;
            if let Some(OscType::Float(f)) = self.args.first() {
                return Some((slot, *f));
            }
        }
        None
    }
    pub fn is_mute(&self) -> Option<(u8, bool)> {
        if self.addr.starts_with("/fxrtn/") && self.addr.ends_with("/mix/on") {
            let rest = self.addr.strip_prefix("/fxrtn/")?;
            let slot_str = rest.split('/').next()?;
            let slot: u8 = slot_str.parse().ok()?;
            let muted = self.args.first().is_some_and(|a| matches!(a, OscType::Int(0)));
            return Some((slot, muted));
        }
        None
    }
    #[allow(dead_code)]
    pub fn is_heartbeat(&self) -> bool { self.addr == "/info" }
    #[allow(dead_code)]
    pub fn addr_only(&self) -> &str { &self.addr }
}

impl MockMixer {
    pub fn start() -> Self { Self::start_with_slots(default_slots()) }

    pub fn start_with_slots(slots: [SlotState; 8]) -> Self {
        let shutdown = Arc::new(AtomicBool::new(false));
        let received_msgs: Arc<Mutex<Vec<ReceivedMsg>>> = Arc::new(Mutex::new(Vec::new()));
        let slots_shared: Arc<Mutex<[SlotState; 8]>> = Arc::new(Mutex::new(slots));

        let sock = UdpSocket::bind("0.0.0.0:0")
            .expect("MockMixer: bind UDP socket");
        sock.set_read_timeout(Some(Duration::from_millis(100)))
            .expect("set_read_timeout");
        let port = sock.local_addr().expect("local_addr").port();

        let sh = shutdown.clone();
        let rx_msgs = received_msgs.clone();
        let sl = slots_shared.clone();
        let handle = thread::Builder::new()
            .name("mock-mixer".into())
            .spawn(move || run_mock_mixer(sock, sh, rx_msgs, sl))
            .expect("spawn mock mixer");

        Self { shutdown, handle: Some(handle), port, received_msgs, slots: slots_shared }
    }

    pub fn port(&self) -> u16 { self.port }

    pub fn received_addrs(&self) -> Vec<String> {
        self.received_msgs.lock()
            .iter().map(|m| m.addr.clone()).collect()
    }

    pub fn received_filtered(&self, pred: impl Fn(&ReceivedMsg) -> bool) -> Vec<ReceivedMsg> {
        self.received_msgs.lock()
            .iter().filter(|m| pred(m)).cloned().collect()
    }

    pub fn sync_count(&self, slot: u8) -> u64 {
        let idx = (slot as usize).saturating_sub(1);
        self.slots.lock().get(idx).map_or(0, |s| s.sync_count)
    }

    #[allow(dead_code)]
    pub fn rx_bpm(&self, slot: u8) -> Option<f64> {
        let idx = (slot as usize).saturating_sub(1);
        self.slots.lock().get(idx).and_then(|s| s.rx_bpm)
    }

    pub fn received(&self, addr: &str) -> bool {
        self.received_addrs().iter().any(|a| a == addr)
    }

    pub fn wait_for_addr(&self, addr: &str, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if self.received(addr) { return true; }
            thread::sleep(Duration::from_millis(10));
        }
        false
    }

    #[allow(dead_code)]
    pub fn stop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }

    pub fn clear_msgs(&self) { self.received_msgs.lock().clear(); }
}

impl Drop for MockMixer {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

fn run_mock_mixer(
    sock: UdpSocket,
    shutdown: Arc<AtomicBool>,
    received_msgs: Arc<Mutex<Vec<ReceivedMsg>>>,
    slots: Arc<Mutex<[SlotState; 8]>>,
) {
    let mut buf = [0u8; 4096];
    while !shutdown.load(Ordering::Acquire) {
        match sock.recv_from(&mut buf) {
            Ok((len, peer)) => {
                let data = &buf[..len];
                if let Ok((_, OscPacket::Message(msg))) = decoder::decode_udp(data) {
                    received_msgs.lock().push(ReceivedMsg {
                        addr: msg.addr.clone(),
                        args: msg.args.clone(),
                        from: peer,
                        timestamp: Instant::now(),
                    });
                    if let Some(response) = handle_request(&msg, &slots) {
                        let _ = sock.send_to(&response, peer);
                    }
                }
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(_) => break,
        }
    }
}

fn handle_request(msg: &OscMessage, slots: &Arc<Mutex<[SlotState; 8]>>) -> Option<Vec<u8>> {
    if msg.addr == "/info" {
        return Some(encode_msg("/info", vec![
            OscType::String("V2.12".to_string()),
            OscType::String("Mock Console".to_string()),
            OscType::String("X32".to_string()),
            OscType::String("4.06".to_string()),
        ]));
    }

    let parts: Vec<&str> = msg.addr.split('/').collect();

    if parts.len() == 4 && parts[1] == "fx" && parts[3] == "type" {
        if let Ok(slot) = parts[2].parse::<usize>() {
            if slot >= 1 && slot <= 8 {
                let type_id = slots.lock().get(slot - 1).map_or(0, |s| s.type_id);
                // Empty slots (type == -1) don't respond, matching real X32 behavior.
                if type_id != EMPTY {
                    return Some(encode_msg(&msg.addr, vec![OscType::Int(type_id)]));
                }
            }
        }
        return None;
    }

    if parts.len() == 5 && parts[1] == "fx" && parts[3] == "par" {
        if let Ok(slot) = parts[2].parse::<usize>() {
            let slot_idx = slot - 1;
            if slot_idx < 8 {
                let mut slots_guard = slots.lock();
                if msg.args.is_empty() {
                    if let Some(state) = slots_guard.get(slot_idx) {
                        let f = state.rx_bpm.map_or_else(
                            || state.delay_float(),
                            |bpm| ((60_000.0 / bpm) / 3000.0).clamp(0.0, 1.0) as f32,
                        );
                        return Some(encode_msg(&msg.addr, vec![OscType::Float(f)]));
                    }
                } else if let Some(OscType::Float(value)) = msg.args.first() {
                    if *value > 0.0 {
                        if let Some(state) = slots_guard.get_mut(slot_idx) {
                            state.rx_bpm = Some(20.0 / *value as f64);
                            state.sync_count += 1;
                        }
                    }
                    return None;
                }
            }
        }
        return None;
    }

    None
}

fn encode_msg(addr: &str, args: Vec<OscType>) -> Vec<u8> {
    let packet = OscPacket::Message(OscMessage { addr: addr.to_string(), args });
    encoder::encode(&packet).expect("OSC encode")
}

pub fn default_slots() -> [SlotState; 8] {
    [
        SlotState::dly(120.0),
        SlotState::other(1),
        SlotState::dly(90.0),
        SlotState::other(3),
        SlotState::empty(),
        SlotState::dly(140.0),
        SlotState::empty(),
        SlotState::other(2),
    ]
}

pub fn all_dly_slots() -> [SlotState; 8] {
    [SlotState::dly(120.0); 8]
}

pub fn all_empty_slots() -> [SlotState; 8] {
    [SlotState::empty(); 8]
}

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
    #[allow(dead_code)]
    pub scan_targets:     std::sync::Arc<Mutex<Vec<ethertap::network::DeviceInfo>>>,
    #[allow(dead_code)]
    pub connected_device: std::sync::Arc<Mutex<(String, String)>>,
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
    hardware_float_out: std::sync::Arc<std::sync::atomic::AtomicU32>,
) -> (NetworkWorker, Sender<NetworkCommand>, Receiver<NetworkStatus>, WorkerShared) {
    let (cmd_tx, cmd_rx) = crossbeam_channel::bounded::<NetworkCommand>(64);
    let (status_tx, status_rx) = crossbeam_channel::bounded::<NetworkStatus>(64);

    let compatible_slots: Arc<AtomicU8> = Arc::new(AtomicU8::new(0));
    let occupied_slots: Arc<AtomicU8>   = Arc::new(AtomicU8::new(0));
    // Encode slot_types_init into atomic array (i32::MIN = None).
    let slot_types_shared: Arc<[AtomicI32; 8]> =
        Arc::new(std::array::from_fn(|i| AtomicI32::new(slot_types_init[i].unwrap_or(i32::MIN))));
    let scan_targets      = std::sync::Arc::new(Mutex::new(Vec::new()));
    let connected_device  = std::sync::Arc::new(Mutex::new((String::new(), String::new())));

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
        std::sync::Arc::new(Mutex::new(String::new())),
        std::sync::Arc::new(Mutex::new(0u16)),
        std::sync::Arc::new(AtomicU8::new(fx_slot)),
        ethertap::network::WorkerShared {
            hardware_float_out,
            compatible_slots,
            occupied_slots,
            slot_types: slot_types_shared,
            scan_targets,
            connected_device,
            scan_generation: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
        },
    );

    (worker, cmd_tx, status_rx, test_shared)
}


pub fn spawn_worker(
    port: u16,
) -> (Sender<NetworkCommand>, Receiver<NetworkStatus>, thread::JoinHandle<()>, WorkerShared) {
    let hardware_float = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
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
    while let Ok(_) = status_rx.try_recv() {}
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
