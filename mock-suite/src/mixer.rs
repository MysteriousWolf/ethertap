//! Mock X32/M32 mixer — a UDP OSC server simulating the protocol surface
//! EtherTap talks to: `/info` heartbeat, `/fx/N/type` queries, `/fx/N/par/NN`
//! get/set (delay time), `/fxrtn/N/mix/on` mutes.
//!
//! Used three ways: as a fixture in `cargo test` (integration tests), behind
//! the `mock-suite` TUI for manual sessions, and headless under the CLI's
//! `--expect`/`--jsonl` test modes.

use std::net::UdpSocket;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use parking_lot::Mutex;
use rosc::{decoder, encoder, OscMessage, OscPacket, OscType};

pub const DLY: i32 = 10;
pub const EMPTY: i32 = -1;

/// Human label for an X32 effect type id (TUI display).
pub fn type_name(type_id: i32) -> &'static str {
    match type_id {
        10 => "DLY",
        11 => "3TAP",
        12 => "4TAP",
        21 => "D/RV",
        24 => "D/CR",
        25 => "D/FL",
        26 => "MODD",
        EMPTY => "---",
        1 => "REV",
        2 => "PIT",
        3 => "CHO",
        _ => "FX?",
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[derive(Debug, Clone, Copy)]
pub struct SlotState {
    pub type_id: i32,
    pub init_bpm: Option<f64>,
    pub rx_bpm: Option<f64>,
    pub sync_count: u64,
    /// ms-since-epoch of the last received sync for this slot (0 = never).
    pub rx_ts_ms: u64,
}

impl SlotState {
    pub fn dly(init_bpm: f64) -> Self {
        Self { type_id: DLY, init_bpm: Some(init_bpm), rx_bpm: None, sync_count: 0, rx_ts_ms: 0 }
    }
    pub fn empty() -> Self {
        Self { type_id: EMPTY, init_bpm: None, rx_bpm: None, sync_count: 0, rx_ts_ms: 0 }
    }
    pub fn other(type_id: i32) -> Self {
        Self { type_id, init_bpm: None, rx_bpm: None, sync_count: 0, rx_ts_ms: 0 }
    }
    pub fn delay_float(self) -> f32 {
        self.init_bpm.map_or(0.0, |bpm| {
            let beat_ms = 60_000.0 / bpm;
            (beat_ms / 3000.0).clamp(0.0, 1.0) as f32
        })
    }
}

#[derive(Debug, Clone)]
pub struct ReceivedMsg {
    pub addr: String,
    pub args: Vec<OscType>,
    /// ms-since-epoch when the message arrived.
    pub ts_ms: u64,
    /// Sender address, `ip:port`.
    pub peer: String,
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
}

/// Cap on the in-memory message log (TUI scroll-back).
const LOG_CAP: usize = 500;

pub struct MockMixer {
    shutdown: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
    pub port: u16,
    pub received_msgs: Arc<Mutex<Vec<ReceivedMsg>>>,
    pub slots: Arc<Mutex<[SlotState; 8]>>,
    /// ms-since-epoch of the most recent OSC message (0 = none yet).
    pub last_msg_ts: Arc<AtomicU64>,
    /// Total messages received (the log is capped; this is not).
    pub total_msgs: Arc<AtomicU64>,
}

impl MockMixer {
    /// Bind an OS-assigned port (test fixtures: no collisions across parallel
    /// test binaries).
    pub fn start() -> Self {
        Self::start_with_slots(default_slots())
    }

    pub fn start_with_slots(slots: [SlotState; 8]) -> Self {
        Self::start_on(0, slots).expect("MockMixer: bind UDP socket")
    }

    /// Bind a specific port (0 = OS-assigned). The real mixer listens on
    /// 10023; the CLI defaults to that.
    pub fn start_on(port: u16, slots: [SlotState; 8]) -> std::io::Result<Self> {
        let shutdown = Arc::new(AtomicBool::new(false));
        let received_msgs: Arc<Mutex<Vec<ReceivedMsg>>> = Arc::new(Mutex::new(Vec::new()));
        let slots_shared: Arc<Mutex<[SlotState; 8]>> = Arc::new(Mutex::new(slots));
        let last_msg_ts = Arc::new(AtomicU64::new(0));
        let total_msgs = Arc::new(AtomicU64::new(0));

        let sock = UdpSocket::bind(("0.0.0.0", port))?;
        sock.set_read_timeout(Some(Duration::from_millis(100)))?;
        let port = sock.local_addr()?.port();

        let sh = shutdown.clone();
        let rx_msgs = received_msgs.clone();
        let sl = slots_shared.clone();
        let ts = last_msg_ts.clone();
        let total = total_msgs.clone();
        let handle = thread::Builder::new()
            .name("mock-mixer".into())
            .spawn(move || run_mock_mixer(sock, sh, rx_msgs, sl, ts, total))?;

        Ok(Self {
            shutdown,
            handle: Some(handle),
            port,
            received_msgs,
            slots: slots_shared,
            last_msg_ts,
            total_msgs,
        })
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn received_addrs(&self) -> Vec<String> {
        self.received_msgs.lock().iter().map(|m| m.addr.clone()).collect()
    }

    pub fn received_filtered(&self, pred: impl Fn(&ReceivedMsg) -> bool) -> Vec<ReceivedMsg> {
        self.received_msgs.lock().iter().filter(|m| pred(m)).cloned().collect()
    }

    /// How many received messages match `addr` exactly.
    pub fn count_addr(&self, addr: &str) -> usize {
        self.received_msgs.lock().iter().filter(|m| m.addr == addr).count()
    }

    pub fn sync_count(&self, slot: u8) -> u64 {
        let idx = (slot as usize).saturating_sub(1);
        self.slots.lock().get(idx).map_or(0, |s| s.sync_count)
    }

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
            if self.received(addr) {
                return true;
            }
            thread::sleep(Duration::from_millis(10));
        }
        false
    }

    pub fn stop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }

    pub fn clear_msgs(&self) {
        self.received_msgs.lock().clear();
    }
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
    last_msg_ts: Arc<AtomicU64>,
    total_msgs: Arc<AtomicU64>,
) {
    let mut buf = [0u8; 4096];
    while !shutdown.load(Ordering::Acquire) {
        match sock.recv_from(&mut buf) {
            Ok((len, peer)) => {
                let data = &buf[..len];
                if let Ok((_, OscPacket::Message(msg))) = decoder::decode_udp(data) {
                    last_msg_ts.store(now_ms(), Ordering::Relaxed);
                    total_msgs.fetch_add(1, Ordering::Relaxed);
                    {
                        let mut log = received_msgs.lock();
                        log.push(ReceivedMsg {
                            addr: msg.addr.clone(),
                            args: msg.args.clone(),
                            ts_ms: now_ms(),
                            peer: peer.to_string(),
                        });
                        // Cap the log so a long TUI session doesn't grow
                        // unboundedly. Tests never approach the cap.
                        let len = log.len();
                        if len > LOG_CAP {
                            log.drain(..len - LOG_CAP);
                        }
                    }
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
            if (1..=8).contains(&slot) {
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
                            state.rx_ts_ms = now_ms();
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

/// Parse a `--slots` spec: comma-separated `dly:BPM`, `other:TYPE`, `empty`.
/// Example: `dly:120,other:1,dly:90,other:3,empty,dly:140,empty,other:2`.
/// Fewer than 8 entries pad with `empty`; more than 8 is an error.
pub fn parse_slots_spec(spec: &str) -> Result<[SlotState; 8], String> {
    let mut slots = [SlotState::empty(); 8];
    let entries: Vec<&str> = spec.split(',').map(str::trim).filter(|s| !s.is_empty()).collect();
    if entries.len() > 8 {
        return Err(format!("--slots: {} entries given, max 8", entries.len()));
    }
    for (i, entry) in entries.iter().enumerate() {
        slots[i] = match entry.split_once(':') {
            None if *entry == "empty" => SlotState::empty(),
            Some(("dly", bpm)) => {
                let bpm: f64 = bpm.parse().map_err(|_| format!("--slots: bad BPM in {entry:?}"))?;
                SlotState::dly(bpm)
            }
            Some(("other", ty)) => {
                let ty: i32 = ty.parse().map_err(|_| format!("--slots: bad type in {entry:?}"))?;
                SlotState::other(ty)
            }
            _ => return Err(format!("--slots: unrecognized entry {entry:?} (want dly:BPM, other:TYPE, or empty)")),
        };
    }
    Ok(slots)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_slots_spec_roundtrip() {
        let slots = parse_slots_spec("dly:120,other:1,empty,dly:90").unwrap();
        assert_eq!(slots[0].type_id, DLY);
        assert_eq!(slots[0].init_bpm, Some(120.0));
        assert_eq!(slots[1].type_id, 1);
        assert_eq!(slots[2].type_id, EMPTY);
        assert_eq!(slots[3].init_bpm, Some(90.0));
        // padding
        assert_eq!(slots[7].type_id, EMPTY);

        assert!(parse_slots_spec("bogus").is_err());
        assert!(parse_slots_spec("dly:abc").is_err());
        assert!(parse_slots_spec(&"empty,".repeat(9)).is_err());
    }

    #[test]
    fn mixer_responds_to_info_and_counts_syncs() {
        let mock = MockMixer::start();
        let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        sock.set_read_timeout(Some(Duration::from_secs(2))).unwrap();

        // /info round trip.
        let info = encode_msg("/info", vec![]);
        sock.send_to(&info, ("127.0.0.1", mock.port())).unwrap();
        let mut buf = [0u8; 1024];
        let (len, _) = sock.recv_from(&mut buf).unwrap();
        let (_, OscPacket::Message(reply)) = decoder::decode_udp(&buf[..len]).unwrap() else {
            panic!("expected OSC message reply");
        };
        assert_eq!(reply.addr, "/info");

        // par set increments sync_count and records BPM.
        let set = encode_msg("/fx/1/par/02", vec![OscType::Float(20.0 / 120.0)]);
        sock.send_to(&set, ("127.0.0.1", mock.port())).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while mock.sync_count(1) == 0 && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(mock.sync_count(1), 1);
        assert!((mock.rx_bpm(1).unwrap() - 120.0).abs() < 0.01);
        assert_eq!(mock.count_addr("/fx/1/par/02"), 1);
        assert!(mock.total_msgs.load(Ordering::Relaxed) >= 2);
    }
}
