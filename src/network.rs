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
    /// Primary (best-path) IP — same-subnet preferred, then lowest latency.
    pub ip:    String,
    pub port:  u16,
    /// User-set name configured on the console (e.g. "Studio A Desk").
    pub name:  String,
    /// Hardware model string returned by the console (e.g. "X32", "M32").
    pub model: String,
    /// Probe round-trip time in milliseconds for the primary IP.
    pub latency_ms: Option<f32>,
    /// All IPs this device was seen from.
    /// Each entry is `(ip, latency_ms, direct)` where `direct` means the
    /// device is on the same subnet as the scanning interface (0 router hops).
    /// The primary `ip` is always duplicated here as the first entry.
    pub all_addrs: Vec<(String, Option<f32>, bool)>,
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
    /// Scan results: compatible = BPM-capable slots, occupied = any non-empty slot.
    /// `slot_types` carries the raw type ID for every slot 1–8 (index = slot-1);
    /// `None` means the slot did not respond or could not be parsed.
    SlotScan { compatible: Vec<u8>, occupied: Vec<u8>, slot_types: [Option<i32>; 8] },
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
    /// Raw effect type ID for each slot (index = slot-1).  Used to choose the
    /// correct par/NN address when dispatching or polling delay time.
    slot_types: Arc<Mutex<[Option<i32>; 8]>>,
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
        slot_types: Arc<Mutex<[Option<i32>; 8]>>,
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
            slot_types,
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
                    self.last_heartbeat += interval;
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
                let value   = osc::bpm_to_float(bpm);
                let type_id = self.slot_type_for(slot);
                self.send(&osc::set_fx_delay(slot, type_id, value));
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

                // 2. Update delay time on all slots using the correct par address.
                for slot in slots.iter().filter_map(|s| *s) {
                    let type_id = self.slot_type_for(slot);
                    self.send(&osc::set_fx_delay(slot, type_id, value));
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

    /// Query the current delay time for the active slot and update `hardware_float_out`.
    ///
    /// Uses the effect-specific par address (par/01 or par/02) so the readback
    /// is always the actual delay time, not some other effect parameter.
    fn poll_delay(&mut self) {
        let Some(target) = self.target else { return };

        let slot    = *self.fx_slot.lock();
        let type_id = self.slot_type_for(slot);
        let query   = osc::query_fx_delay(slot, type_id);

        let send_ok = self.socket.as_ref()
            .map(|s| s.send_to(&query, target).is_ok())
            .unwrap_or(false);

        if !send_ok {
            self.socket = None;
            self.connected = false;
            let _ = self.status_tx.try_send(NetworkStatus::Disconnected);
            return;
        }
        self.pulse_tx();

        let mut buf = [0u8; 256];
        if let Some(value) = self.socket.as_ref().and_then(|s| {
            s.recv_from(&mut buf).ok()
                .and_then(|(len, _)| parse_fx_delay_response(&buf[..len]))
        }) {
            self.hardware_float_out.store(value.to_bits(), Ordering::Relaxed);
            let _ = self.status_tx.try_send(NetworkStatus::DelayReadback(value));
            self.pulse_rx();
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
            self.socket = None;
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

    fn audit_slots(&mut self) {
        let (Some(sock), Some(target)) = (&self.socket, &self.target) else {
            return;
        };
        let _ = sock.set_read_timeout(Some(RECV_TIMEOUT));

        let mut compatible  = Vec::new();
        let mut occupied    = Vec::new();
        let mut slot_types  = [None::<i32>; 8];

        for slot in 1u8..=8 {
            if sock.send_to(&osc::query_fx_type(slot), target).is_err() {
                continue;
            }
            let mut buf = [0u8; 256];
            if let Ok((len, _)) = sock.recv_from(&mut buf) {
                if let Some(type_id) = parse_fx_type(&buf[..len]) {
                    slot_types[(slot - 1) as usize] = Some(type_id);
                    // Any response means the slot is occupied.
                    // Empty slots do not respond to /fx/{slot}/type at all.
                    if osc::is_bpm_compatible(type_id, slot) {
                        compatible.push(slot);
                    }
                    occupied.push(slot);
                }
                // No response → slot is empty; slot_types entry stays None.
            }
        }
        // ── Debug: log every slot's effect type ──────────────────────────
        eprintln!("[EtherTap] FX slot audit:");
        for slot in 1u8..=8 {
            match slot_types[(slot - 1) as usize] {
                Some(type_id) => {
                    let short = crate::osc::fx_type_short(type_id, slot);
                    let long  = crate::osc::fx_type_long(type_id, slot);
                    let tag   = if crate::osc::is_bpm_compatible(type_id, slot) {
                        "  [BPM-compatible]"
                    } else {
                        ""
                    };
                    eprintln!("  Slot {slot}: {short}  ({long}){tag}");
                }
                None => eprintln!("  Slot {slot}: no response"),
            }
        }
        eprintln!("  Compatible: {:?}  Occupied: {:?}", compatible, occupied);

        let _ = self.status_tx.try_send(NetworkStatus::SlotScan {
            compatible,
            occupied,
            slot_types,
        });
    }

    // ── Network scan ─────────────────────────────────────────────────────

    /// Probe every local interface in parallel and collect responding devices.
    ///
    /// Each IPv4 interface gets its own socket so the directed subnet broadcast
    /// exits on the correct NIC.  Responses are collected with non-blocking I/O.
    ///
    /// Results carry:
    /// * `latency_ms` — probe round-trip time (useful for comparing paths)
    /// * `all_addrs`  — every `(ip, latency_ms, direct)` triple the device was
    ///   seen from; `direct` means same subnet as the scanning interface.
    ///
    /// Devices are identified by `(name, model)`.  The entry with the best path
    /// (same-subnet first, then lowest latency) becomes the primary; all other
    /// IPs are appended to `all_addrs` so the UI can show them.
    fn scan_targets(&self) {
        use std::{collections::HashMap, net::Ipv4Addr};

        let probe  = osc::heartbeat();
        let window = Duration::from_millis(600);

        // ── One socket per real IPv4 interface ────────────────────────────
        struct Iface { sock: UdpSocket, local: Ipv4Addr, netmask: Ipv4Addr }

        let mut ifaces: Vec<Iface> = Vec::new();

        let raw = if_addrs::get_if_addrs().unwrap_or_default();
        for iface in raw {
            let if_addrs::IfAddr::V4(v4) = iface.addr else { continue };
            if v4.ip.is_loopback() { continue; }

            let Ok(sock) = UdpSocket::bind(format!("{}:0", v4.ip)) else { continue };
            let _ = sock.set_broadcast(true);
            let bcast = v4.broadcast.unwrap_or_else(|| {
                Ipv4Addr::from(u32::from(v4.ip) | !u32::from(v4.netmask))
            });
            let _ = sock.send_to(&probe, SocketAddr::from((bcast, 10023u16)));
            let _ = sock.set_nonblocking(true);
            ifaces.push(Iface { sock, local: v4.ip, netmask: v4.netmask });
        }

        // Loopback socket so a local mock mixer is always discoverable.
        if let Ok(sock) = UdpSocket::bind("127.0.0.1:0") {
            let _ = sock.send_to(&probe, "127.0.0.1:10023".parse::<SocketAddr>().unwrap());
            let _ = sock.set_nonblocking(true);
            ifaces.push(Iface {
                sock,
                local:   Ipv4Addr::LOCALHOST,
                netmask: Ipv4Addr::new(255, 0, 0, 0),
            });
        }

        // ── Collect responses — raw entry per (socket, device) pair ───────
        // Tuple: (ip, latency_ms, name, model, same_subnet)
        type RawEntry = (String, f32, String, String, bool);

        // ip_key → best raw entry (same-subnet wins; ties broken by latency)
        let mut by_ip: HashMap<String, RawEntry> = HashMap::new();
        let mut buf = [0u8; 512];
        let probe_sent_at = Instant::now();
        let start = probe_sent_at;

        while start.elapsed() < window {
            let mut any_recv = false;
            for iface in &ifaces {
                loop {
                    match iface.sock.recv_from(&mut buf) {
                        Ok((len, src)) => {
                            any_recv = true;
                            if decoder::decode_udp(&buf[..len]).is_err() { continue; }
                            let src_v4 = match src.ip() {
                                std::net::IpAddr::V4(v) => v,
                                _ => continue,
                            };

                            let mask        = u32::from(iface.netmask);
                            let same_subnet =
                                (u32::from(iface.local) & mask) == (u32::from(src_v4) & mask);
                            let latency_ms  = probe_sent_at.elapsed().as_micros() as f32 / 1000.0;
                            let ip_key      = src_v4.to_string();
                            let (name, model) = parse_info_strings(&buf[..len]);

                            let entry: RawEntry = (ip_key.clone(), latency_ms, name, model, same_subnet);

                            match by_ip.get(&ip_key) {
                                None => { by_ip.insert(ip_key, entry); }
                                Some((_, _, _, _, prev_same)) => {
                                    // Prefer same-subnet; within that, prefer lower latency.
                                    let better = (!*prev_same && same_subnet)
                                        || (*prev_same == same_subnet && latency_ms < by_ip[&ip_key].1);
                                    if better { by_ip.insert(ip_key, entry); }
                                }
                            }
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                        Err(_) => break,
                    }
                }
            }
            if !any_recv {
                std::thread::sleep(Duration::from_millis(10));
            }
        }

        // ── Merge by (name, model) — best path first ──────────────────────
        // Sort: same-subnet before routed, then ascending latency.
        let mut all: Vec<RawEntry> = by_ip.into_values().collect();
        all.sort_by(|a, b| {
            match (b.4, a.4) { // same_subnet: true sorts before false
                (true,  false) => std::cmp::Ordering::Less,
                (false, true)  => std::cmp::Ordering::Greater,
                _ => a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal),
            }
        });

        let mut result: Vec<DeviceInfo> = Vec::new();

        for (ip, latency_ms, name, model, same_subnet) in all {
            let has_id = !name.is_empty() || !model.is_empty();

            // Try to find an existing entry with the same identity.
            if has_id {
                if let Some(existing) = result.iter_mut()
                    .find(|d| d.name == name && d.model == model)
                {
                    // Same physical device reachable via another IP — append.
                    if !existing.all_addrs.iter().any(|(a, _, _)| *a == ip) {
                        existing.all_addrs.push((ip, Some(latency_ms), same_subnet));
                    }
                    continue;
                }
            }

            // New device.
            result.push(DeviceInfo {
                ip:         ip.clone(),
                port:       10023,
                name,
                model,
                latency_ms: Some(latency_ms),
                all_addrs:  vec![(ip, Some(latency_ms), same_subnet)],
            });
        }

        let _ = self.status_tx.try_send(NetworkStatus::TargetsFound(result));
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

    /// Send `bytes` to the target.  On failure, nulls the socket so that
    /// the next heartbeat cycle triggers a fresh `rebind()` attempt.
    fn send(&mut self, bytes: &[u8]) {
        let Some(target) = self.target else { return };
        let ok = self.socket.as_ref()
            .map(|s| s.send_to(bytes, target).is_ok())
            .unwrap_or(false);
        if !ok {
            self.socket = None;
            self.connected = false;
            let _ = self.status_tx.try_send(NetworkStatus::Disconnected);
        }
    }

    fn pulse_tx(&self) {
        let _ = self.status_tx.try_send(NetworkStatus::ActivityPulse);
    }

    fn pulse_rx(&self) {
        let _ = self.status_tx.try_send(NetworkStatus::RxPulse);
    }

    /// Look up the raw effect type ID for `slot` (1-indexed).
    /// Returns 10 (DLY) as a safe default when the type is not yet known.
    fn slot_type_for(&self, slot: u8) -> i32 {
        let idx = slot.saturating_sub(1) as usize;
        self.slot_types.lock()
            .get(idx)
            .and_then(|t| *t)
            .unwrap_or(10)
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
