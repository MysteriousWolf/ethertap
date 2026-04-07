/// Background network worker.
///
/// Runs on a dedicated OS thread.  Communicates with the audio thread via
/// lock-free `crossbeam_channel` queues — the audio thread never blocks.
///
/// # Responsibilities
/// * Send OSC packets to the X32/M32 on demand.
/// * Maintain a periodic /info heartbeat for connectivity detection.
/// * Poll the current delay-parameter value every [`TELEMETRY_INTERVAL`]
///   and report it back so the editor can show the hardware BPM.
///
/// # Lifecycle
/// Exits automatically when the audio-thread's `Sender` is dropped.
use std::{
    net::{SocketAddr, UdpSocket},
    sync::atomic::{AtomicU32, Ordering},
    sync::Arc,
    time::{Duration, Instant},
};

use crossbeam_channel::{Receiver, Sender, TryRecvError};
use parking_lot::Mutex;
use rosc::{decoder, OscPacket, OscType};

use crate::osc;

// ─── Constants ───────────────────────────────────────────────────────────────

const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);
/// Retry interval used when the connection is lost (faster than normal heartbeat).
const RECONNECT_INTERVAL: Duration = Duration::from_secs(2);
const TELEMETRY_INTERVAL: Duration = Duration::from_secs(3);
/// Per-recv timeout used for heartbeat / telemetry reads.
const RECV_TIMEOUT: Duration = Duration::from_millis(250);
/// Mute ↔ update dwell for the Hard Reset sequence (spec: 50–100 ms).
const HARD_RESET_DWELL: Duration = Duration::from_millis(75);
/// Worker loop sleep when idle.
const LOOP_SLEEP: Duration = Duration::from_millis(10);

// ─── Device info ─────────────────────────────────────────────────────────────

/// A device that responded to a scan probe, with identifying metadata.
#[derive(Debug, Clone)]
pub struct DeviceInfo {
    pub ip:    String,
    pub port:  u16,
    /// User-set name configured on the console (e.g. "Studio A Desk").
    pub name:  String,
    /// Hardware model string returned by the console (e.g. "X32", "M32").
    pub model: String,
}

impl DeviceInfo {
    /// Human-readable label: "name (model)", "name", "model", or "ip:port".
    pub fn display_name(&self) -> String {
        match (self.name.is_empty(), self.model.is_empty()) {
            (false, false) if self.name != self.model =>
                format!("{} ({})", self.name, self.model),
            (false, _) => self.name.clone(),
            (_, false)  => self.model.clone(),
            _           => format!("{}:{}", self.ip, self.port),
        }
    }
}

// ─── Command / Status types ──────────────────────────────────────────────────

/// Commands from the audio thread (or editor) to the network worker.
#[derive(Debug)]
pub enum NetworkCommand {
    /// Bind a new UDP socket and connect to the given target.
    UpdateTarget { ip: String, port: u16 },
    /// Drop the socket and mark as disconnected.
    Disconnect,
    /// Dispatch the BPM-derived delay time to the given FX slot immediately.
    SyncNow { slot: u8, bpm: f64 },
    /// Batched hard reset: mute all slots → wait → set all → wait → unmute all.
    /// Uses a fixed-size array to avoid heap allocation on the audio thread.
    HardResetBatch { slots: [Option<u8>; 8], bpm: f64 },
    /// Query all 8 FX slots and report which host a Stereo Delay (type 10).
    AuditSlots,
    /// Broadcast an /info probe and collect responding devices.
    ScanTargets,
}

/// Status events produced by the network worker.
#[derive(Debug, Clone)]
pub enum NetworkStatus {
    Connected,
    Disconnected,
    /// An OSC packet was transmitted — blink the TX activity LED for 100 ms.
    ActivityPulse,
    /// An OSC packet was received from the mixer — blink the RX activity LED.
    RxPulse,
    /// Polled delay-time value returned by the mixer.
    DelayReadback(f32),
    /// Scan results: which slots hold a compatible Stereo Delay (DLY) and
    /// which hold any other effect (occupied but incompatible).
    SlotScan { compatible: Vec<u8>, occupied: Vec<u8> },
    /// Devices that responded to the broadcast /info probe.
    TargetsFound(Vec<DeviceInfo>),
    /// Name/model parsed from an /info heartbeat response.
    DeviceIdentified { name: String, model: String },
}

// ─── Worker ──────────────────────────────────────────────────────────────────

pub struct NetworkWorker {
    cmd_rx: Receiver<NetworkCommand>,
    status_tx: Sender<NetworkStatus>,
    socket: Option<UdpSocket>,
    target: Option<SocketAddr>,
    last_heartbeat: Instant,
    telemetry_timer: Instant,
    /// Shared reference to the active FX slot, updated when the user changes it.
    fx_slot: Arc<Mutex<u8>>,
    /// Shared output for the last polled hardware delay float (f32 bits).
    hardware_float_out: Arc<AtomicU32>,
    /// Set by an explicit `Disconnect` command; prevents automatic reconnect.
    /// Cleared when a new `UpdateTarget` arrives.
    user_disconnected: bool,
    /// Last known connection state — used to pick the heartbeat vs reconnect interval.
    connected: bool,
}

impl NetworkWorker {
    pub fn new(
        cmd_rx: Receiver<NetworkCommand>,
        status_tx: Sender<NetworkStatus>,
        fx_slot: Arc<Mutex<u8>>,
        hardware_float_out: Arc<AtomicU32>,
    ) -> Self {
        let now = Instant::now();
        Self {
            cmd_rx,
            status_tx,
            socket: None,
            target: None,
            last_heartbeat: now,
            telemetry_timer: now,
            fx_slot,
            hardware_float_out,
            user_disconnected: false,
            connected: false,
        }
    }

    /// Main loop — runs until the command channel disconnects (plugin dropped).
    pub fn run(mut self) {
        loop {
            // Drain all pending commands without blocking.
            loop {
                match self.cmd_rx.try_recv() {
                    Ok(cmd) => self.handle(cmd),
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => return,
                }
            }

            // Periodic heartbeat / reconnect.
            // Skipped entirely when the user explicitly disconnected.
            if !self.user_disconnected && self.target.is_some() {
                let interval = if self.connected { HEARTBEAT_INTERVAL } else { RECONNECT_INTERVAL };
                if self.last_heartbeat.elapsed() >= interval {
                    // Socket may have been dropped after a send/recv error — rebind before retrying.
                    if self.socket.is_none() {
                        self.rebind();
                    }
                    self.send_heartbeat();
                    // Advance by exactly one interval so the cadence stays
                    // constant regardless of how long the heartbeat took.
                    self.last_heartbeat = self.last_heartbeat + interval;
                }
            }

            // Periodic hardware telemetry poll — only while connected.
            if self.connected && self.telemetry_timer.elapsed() >= TELEMETRY_INTERVAL {
                self.poll_delay();
                self.telemetry_timer = Instant::now();
            }

            std::thread::sleep(LOOP_SLEEP);
        }
    }

    // ── Command dispatch ──────────────────────────────────────────────────

    fn handle(&mut self, cmd: NetworkCommand) {
        match cmd {
            NetworkCommand::UpdateTarget { ip, port } => {
                match format!("{ip}:{port}").parse::<SocketAddr>() {
                    Ok(addr) => {
                        self.user_disconnected = false;
                        self.target = Some(addr);
                        self.rebind();
                        // Record the reference point *before* the blocking heartbeat
                        // call so the next periodic heartbeat uses a clean baseline.
                        self.last_heartbeat = Instant::now();
                        self.send_heartbeat();
                    }
                    Err(_) => eprintln!("[EtherTap] invalid target: {ip}:{port}"),
                }
            }

            NetworkCommand::Disconnect => {
                self.socket = None;
                self.target = None;
                self.connected = false;
                self.user_disconnected = true;
                let _ = self.status_tx.try_send(NetworkStatus::Disconnected);
            }

            NetworkCommand::SyncNow { slot, bpm } => {
                let value = osc::bpm_to_float(bpm);
                self.send(&osc::set_fx_delay(slot, value));
                self.pulse_tx();
            }

            NetworkCommand::HardResetBatch { slots, bpm } => {
                let value = osc::bpm_to_float(bpm);
                // 1. Mute all slots simultaneously.
                for slot in slots.iter().filter_map(|s| *s) {
                    self.send(&osc::set_fxrtn_mute(slot, true));
                }
                self.pulse_tx();
                std::thread::sleep(HARD_RESET_DWELL);

                // 2. Update delay time on all slots.
                for slot in slots.iter().filter_map(|s| *s) {
                    self.send(&osc::set_fx_delay(slot, value));
                }
                std::thread::sleep(HARD_RESET_DWELL);

                // 3. Unmute all slots simultaneously.
                for slot in slots.iter().filter_map(|s| *s) {
                    self.send(&osc::set_fxrtn_mute(slot, false));
                }
                self.pulse_tx();
            }

            NetworkCommand::AuditSlots => self.audit_slots(),
            NetworkCommand::ScanTargets => self.scan_targets(),
        }
    }

    // ── Telemetry ─────────────────────────────────────────────────────────

    /// Send a `/fx/{slot}/par/02` query and report the returned float.
    fn poll_delay(&self) {
        let Some(sock) = &self.socket else { return };
        let Some(target) = &self.target else { return };

        let slot = *self.fx_slot.lock();
        let query = osc::query_fx_delay(slot);

        if sock.send_to(&query, target).is_err() {
            let _ = self.status_tx.try_send(NetworkStatus::Disconnected);
            return;
        }
        self.pulse_tx();

        let mut buf = [0u8; 256];
        if let Ok((len, _)) = sock.recv_from(&mut buf) {
            if let Some(value) = parse_fx_delay_response(&buf[..len]) {
                self.hardware_float_out
                    .store(value.to_bits(), Ordering::Relaxed);
                let _ = self.status_tx.try_send(NetworkStatus::DelayReadback(value));
                self.pulse_rx();
            }
        }
    }

    // ── Heartbeat ────────────────────────────────────────────────────────

    fn send_heartbeat(&mut self) {
        let Some(target) = self.target else { return };

        // Send the /info probe.  Use .map() so the borrow on self.socket is
        // released before we need to mutate self.connected below.
        let sent = self.socket.as_ref()
            .map(|s| s.send_to(&osc::heartbeat(), target).is_ok())
            .unwrap_or(false);

        if !sent {
            self.connected = false;
            let _ = self.status_tx.try_send(NetworkStatus::Disconnected);
            return;
        }
        self.pulse_tx();

        // Wait briefly for the response.
        let mut buf = [0u8; 512];
        let recv_len = self.socket.as_ref().and_then(|sock| {
            let _ = sock.set_read_timeout(Some(RECV_TIMEOUT));
            sock.recv_from(&mut buf).ok().map(|(len, _)| len)
        });

        match recv_len.filter(|&len| decoder::decode_udp(&buf[..len]).is_ok()) {
            Some(len) => {
                self.connected = true;
                let _ = self.status_tx.try_send(NetworkStatus::Connected);
                self.pulse_rx();
                let (name, model) = parse_info_strings(&buf[..len]);
                if !name.is_empty() || !model.is_empty() {
                    let _ = self.status_tx.try_send(
                        NetworkStatus::DeviceIdentified { name, model });
                }
            }
            None => {
                self.connected = false;
                let _ = self.status_tx.try_send(NetworkStatus::Disconnected);
            }
        }
    }

    // ── Slot audit ───────────────────────────────────────────────────────

    fn audit_slots(&self) {
        let (Some(sock), Some(target)) = (&self.socket, &self.target) else {
            return;
        };
        let _ = sock.set_read_timeout(Some(RECV_TIMEOUT));

        let mut compatible = Vec::new();
        let mut occupied   = Vec::new();
        for slot in 1u8..=8 {
            if sock.send_to(&osc::query_fx_type(slot), target).is_err() {
                continue;
            }
            let mut buf = [0u8; 256];
            if let Ok((len, _)) = sock.recv_from(&mut buf) {
                if let Some(type_id) = parse_fx_type(&buf[..len]) {
                    if type_id == osc::DLY_TYPE_ID {
                        compatible.push(slot);
                        occupied.push(slot);
                    } else if type_id != 0 {
                        occupied.push(slot); // occupied but not a DLY
                    }
                    // type_id == 0  →  slot is empty, nothing pushed
                }
            }
        }
        let _ = self
            .status_tx
            .try_send(NetworkStatus::SlotScan { compatible, occupied });
    }

    // ── Network scan ─────────────────────────────────────────────────────

    /// Broadcast `/info` and collect all responding device addresses.
    fn scan_targets(&self) {
        let Ok(sock) = UdpSocket::bind("0.0.0.0:0") else { return };
        let _ = sock.set_broadcast(true);

        let probe = osc::heartbeat();
        // Broadcast to the network — may not loop back to localhost on macOS.
        if let Ok(bcast) = "255.255.255.255:10023".parse::<SocketAddr>() {
            let _ = sock.send_to(&probe, bcast);
        }
        // Also unicast to loopback so a local mock mixer is always reachable.
        if let Ok(loopback) = "127.0.0.1:10023".parse::<SocketAddr>() {
            let _ = sock.send_to(&probe, loopback);
        }

        let mut found: Vec<DeviceInfo> = Vec::new();
        let mut buf = [0u8; 512];
        let start = Instant::now();
        let window = Duration::from_millis(600);

        loop {
            if start.elapsed() >= window { break; }
            let remaining = window - start.elapsed();
            let _ = sock.set_read_timeout(Some(remaining.min(RECV_TIMEOUT)));
            match sock.recv_from(&mut buf) {
                Ok((len, src)) => {
                    if decoder::decode_udp(&buf[..len]).is_ok() {
                        let ip = src.ip().to_string();
                        if found.iter().any(|d| d.ip == ip) { continue; }
                        let (name, model) = parse_info_strings(&buf[..len]);
                        found.push(DeviceInfo { ip, port: 10023, name, model });
                    }
                }
                Err(_) => {}  // timeout — keep looping until window expires
            }
        }

        let _ = self.status_tx.try_send(NetworkStatus::TargetsFound(found));
    }

    // ── UDP helpers ───────────────────────────────────────────────────────

    fn rebind(&mut self) {
        match UdpSocket::bind("0.0.0.0:0") {
            Ok(sock) => {
                let _ = sock.set_read_timeout(Some(RECV_TIMEOUT));
                self.socket = Some(sock);
            }
            Err(e) => eprintln!("[EtherTap] failed to bind UDP socket: {e}"),
        }
    }

    fn send(&self, bytes: &[u8]) {
        let (Some(sock), Some(target)) = (&self.socket, &self.target) else {
            return;
        };
        if sock.send_to(bytes, target).is_err() {
            let _ = self.status_tx.try_send(NetworkStatus::Disconnected);
        }
    }

    fn pulse_tx(&self) {
        let _ = self.status_tx.try_send(NetworkStatus::ActivityPulse);
    }

    fn pulse_rx(&self) {
        let _ = self.status_tx.try_send(NetworkStatus::RxPulse);
    }
}

// ─── OSC response parsers ────────────────────────────────────────────────────

fn parse_fx_type(data: &[u8]) -> Option<i32> {
    let (_, packet) = decoder::decode_udp(data).ok()?;
    match packet {
        OscPacket::Message(msg) => match msg.args.first() {
            Some(OscType::Int(id)) => Some(*id),
            _ => None,
        },
        _ => None,
    }
}

/// Parse the string arguments from an X32 `/info` response.
///
/// X32 format: `/info ,ssss  version  name  model  firmware`
/// Returns `(name, model)` — empty strings if the args aren't present.
fn parse_info_strings(data: &[u8]) -> (String, String) {
    let Ok((_, OscPacket::Message(msg))) = decoder::decode_udp(data) else {
        return (String::new(), String::new());
    };
    let strings: Vec<String> = msg.args.iter().filter_map(|a| {
        if let OscType::String(s) = a { Some(s.clone()) } else { None }
    }).collect();
    match strings.len() {
        0 => (String::new(), String::new()),
        1 => (strings[0].clone(), String::new()),
        2 => (strings[0].clone(), strings[1].clone()),
        // 3+ args: X32 layout is version, name, model[, firmware]
        _ => (strings[1].clone(), strings[2].clone()),
    }
}

fn parse_fx_delay_response(data: &[u8]) -> Option<f32> {
    let (_, packet) = decoder::decode_udp(data).ok()?;
    match packet {
        OscPacket::Message(msg) => match msg.args.first() {
            Some(OscType::Float(f)) => Some(*f),
            _ => None,
        },
        _ => None,
    }
}

// ─── Shared timing utility ───────────────────────────────────────────────────

/// Milliseconds since UNIX_EPOCH — used for the 100 ms activity-pulse LEDs.
pub fn now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
