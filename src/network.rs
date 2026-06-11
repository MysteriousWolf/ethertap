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
    sync::atomic::{AtomicI32, AtomicU32, AtomicU64, AtomicU8, Ordering},
    sync::Arc,
    time::{Duration, Instant},
};

use crossbeam_channel::{Receiver, Sender, TryRecvError};
use parking_lot::Mutex;
use rosc::{decoder, OscMessage, OscPacket, OscType};

use crate::osc;
use crate::reconnect::Backoff;

// ─── Constants ───────────────────────────────────────────────────────────────

const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);
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
    pub ip: String,
    pub port: u16,
    /// User-set name configured on the console (e.g. "Studio A Desk").
    pub name: String,
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
            (false, false) if self.name != self.model => format!("{} ({})", self.name, self.model),
            (false, _) => self.name.clone(),
            (_, false) => self.model.clone(),
            _ => format!("{}:{}", self.ip, self.port),
        }
    }
}

// ─── Command / Status types ──────────────────────────────────────────────────

/// Commands from the audio thread (or editor) to the network worker.
#[derive(Debug)]
pub enum NetworkCommand {
    /// Bind a new UDP socket and connect to the given target.
    UpdateTarget { ip: String, port: u16 },
    /// Connect using the ip/port already stored in the shared params.
    /// Lets the audio thread trigger a connect without allocating or locking.
    ConnectToLast,
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
///
/// All variants are allocation-free so `process()` can drain the channel on
/// the audio thread without risking heap operations.  Data that would require
/// allocation (slot lists, scan results, device identity) is written directly
/// by the network worker to shared `Arc<Mutex<_>>` fields before sending the
/// corresponding sentinel here.
#[derive(Debug, Clone, Copy)]
pub enum NetworkStatus {
    Connected,
    Disconnected,
    /// An OSC packet was transmitted — blink the TX activity LED for 100 ms.
    ActivityPulse,
    /// An OSC packet was received from the mixer — blink the RX activity LED.
    RxPulse,
    /// Polled delay-time value returned by the mixer.
    /// The value is already stored in `hardware_float_out`; this variant
    /// exists solely to trigger a pulse on the RX LED path in `process()`.
    DelayReadback(f32),
    /// Slot audit complete.  Results were written directly to the shared
    /// `compatible_slots`, `occupied_slots`, and `slot_types` mutexes.
    SlotScanDone,
    /// Network scan complete.  Results were merged directly into the shared
    /// `scan_targets` mutex.  The audio thread updates `scan_completed_ts`.
    ScanDone,
}

// ─── Shared state ────────────────────────────────────────────────────────────

/// Arcs written by the network worker and read by the audio thread or editor.
/// Passed as a single unit to [`NetworkWorker::new`] to keep the constructor
/// argument count manageable.
pub struct WorkerShared {
    /// Last polled hardware delay-time float (f32 bits via `f32::from/to_bits`).
    pub hardware_float_out: Arc<AtomicU32>,
    /// Bitmask: bit n set ↔ slot (n+1) is BPM-compatible. Written after AuditSlots.
    pub compatible_slots: Arc<AtomicU8>,
    /// Bitmask: bit n set ↔ slot (n+1) is occupied. Written after AuditSlots.
    pub occupied_slots: Arc<AtomicU8>,
    /// Raw effect type ID per slot (index = slot-1). i32::MIN = not yet queried.
    pub slot_types: Arc<[AtomicI32; 8]>,
    /// Discovered network scan targets — written here, read by editor.
    pub scan_targets: Arc<Mutex<Vec<DeviceInfo>>>,
    /// Name and model of the connected device from `/info` responses.
    pub connected_device: Arc<Mutex<(String, String)>>,
    /// Monotonically-increasing counter; background scan threads discard results
    /// when this changes, preventing stale results from a previous scan.
    pub scan_generation: Arc<AtomicU64>,
}

// ─── Worker ──────────────────────────────────────────────────────────────────

pub struct NetworkWorker {
    cmd_rx: Receiver<NetworkCommand>,
    status_tx: Sender<NetworkStatus>,
    socket: Option<UdpSocket>,
    target: Option<SocketAddr>,
    last_heartbeat: Instant,
    telemetry_timer: Instant,
    /// Persisted target ip/port — read when ConnectToLast is received so the
    /// audio thread never has to lock or clone to trigger a reconnect.
    target_ip: Arc<Mutex<String>>,
    target_port: Arc<Mutex<u16>>,
    /// Shared reference to the active FX slot, updated when the user changes it.
    fx_slot: Arc<AtomicU8>,
    hardware_float_out: Arc<AtomicU32>,
    compatible_slots: Arc<AtomicU8>,
    occupied_slots: Arc<AtomicU8>,
    slot_types: Arc<[AtomicI32; 8]>,
    scan_targets: Arc<Mutex<Vec<DeviceInfo>>>,
    connected_device: Arc<Mutex<(String, String)>>,
    scan_generation: Arc<AtomicU64>,
    /// Set by an explicit `Disconnect` command; prevents automatic reconnect.
    /// Cleared when a new `UpdateTarget` arrives.
    user_disconnected: bool,
    connected: bool,
    backoff: Backoff,
}

/// Encode a slot list (values 1..=8) as a u8 bitmask: bit n = slot (n+1) present.
fn slots_to_bitmask(slots: &[u8]) -> u8 {
    slots
        .iter()
        .fold(0u8, |acc, &s| acc | (1u8 << s.saturating_sub(1)))
}

impl NetworkWorker {
    pub fn new(
        cmd_rx: Receiver<NetworkCommand>,
        status_tx: Sender<NetworkStatus>,
        target_ip: Arc<Mutex<String>>,
        target_port: Arc<Mutex<u16>>,
        fx_slot: Arc<AtomicU8>,
        shared: WorkerShared,
    ) -> Self {
        let now = Instant::now();
        Self {
            cmd_rx,
            status_tx,
            socket: None,
            target: None,
            last_heartbeat: now,
            telemetry_timer: now,
            target_ip,
            target_port,
            fx_slot,
            hardware_float_out: shared.hardware_float_out,
            compatible_slots: shared.compatible_slots,
            occupied_slots: shared.occupied_slots,
            slot_types: shared.slot_types,
            scan_targets: shared.scan_targets,
            connected_device: shared.connected_device,
            scan_generation: shared.scan_generation,
            user_disconnected: false,
            connected: false,
            backoff: Backoff::new(2000, 10000),
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
                let interval = if self.connected {
                    HEARTBEAT_INTERVAL
                } else {
                    Duration::from_millis(self.backoff.next_delay_ms())
                };
                if self.last_heartbeat.elapsed() >= interval {
                    // Socket may have been dropped after a send/recv error — rebind before retrying.
                    if self.socket.is_none() && !self.rebind() {
                        // Bind failed — count as a connection attempt failure so
                        // exponential backoff applies instead of spinning at 10 ms.
                        log::debug!(
                            "[EtherTap] UDP rebind failed — backoff {}ms",
                            self.backoff.next_delay_ms()
                        );
                        self.backoff.record_failure();
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
                self.telemetry_timer += TELEMETRY_INTERVAL;
            }

            std::thread::sleep(LOOP_SLEEP);
        }
    }

    // ── Command dispatch ──────────────────────────────────────────────────

    fn connect(&mut self, ip: String, port: u16) {
        match format!("{ip}:{port}").parse::<SocketAddr>() {
            Ok(addr) => {
                self.user_disconnected = false;
                self.backoff.record_success();
                self.target = Some(addr);
                let _ = self.rebind();
                // Record the reference point *before* the blocking heartbeat
                // call so the next periodic heartbeat uses a clean baseline.
                self.last_heartbeat = Instant::now();
                self.send_heartbeat();
            }
            Err(_) => log::warn!("[EtherTap] invalid target: {ip}:{port}"),
        }
    }

    fn handle(&mut self, cmd: NetworkCommand) {
        match cmd {
            NetworkCommand::UpdateTarget { ip, port } => self.connect(ip, port),

            NetworkCommand::ConnectToLast => {
                let ip = self.target_ip.lock().clone();
                let port = *self.target_port.lock();
                self.connect(ip, port);
            }

            NetworkCommand::Disconnect => {
                self.socket = None;
                self.target = None;
                self.connected = false;
                self.user_disconnected = true;
                // Clear stale hardware BPM so the editor shows no-data instead
                // of a phantom value after the connection drops.
                self.hardware_float_out.store(0u32, Ordering::Release);
                let _ = self.status_tx.try_send(NetworkStatus::Disconnected);
            }

            NetworkCommand::SyncNow { slot, bpm } => {
                let value = osc::bpm_to_float(bpm);
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
            NetworkCommand::ScanTargets => {
                // Run the 600 ms scan on a dedicated thread so the network
                // worker remains responsive to sync commands during the window.
                let scan_targets = self.scan_targets.clone();
                let status_tx = self.status_tx.clone();
                let scan_gen = self.scan_generation.clone();
                // Capture the current generation so the background thread can
                // detect if the editor opened a new scan (and cleared results)
                // before this thread finishes.
                let my_gen = scan_gen.load(Ordering::Acquire);
                std::thread::Builder::new()
                    .name("ethertap-scan".into())
                    .spawn(move || {
                        NetworkWorker::scan_targets_bg(scan_targets, status_tx, scan_gen, my_gen)
                    })
                    .ok(); // best-effort; failure just means no scan result
            }
        }
    }

    // ── Telemetry ─────────────────────────────────────────────────────────

    /// Query the current delay time for the active slot and update `hardware_float_out`.
    ///
    /// Uses the effect-specific par address (par/01 or par/02) so the readback
    /// is always the actual delay time, not some other effect parameter.
    fn poll_delay(&mut self) {
        let Some(target) = self.target else { return };

        let slot = self.fx_slot.load(Ordering::Relaxed);
        let type_id = self.slot_type_for(slot);
        let query = osc::query_fx_delay(slot, type_id);

        let send_ok = self
            .socket
            .as_ref()
            .map(|s| s.send_to(&query, target).is_ok())
            .unwrap_or(false);

        if !send_ok {
            self.socket = None;
            self.connected = false;
            self.hardware_float_out.store(0u32, Ordering::Release);
            let _ = self.status_tx.try_send(NetworkStatus::Disconnected);
            return;
        }
        self.pulse_tx();

        let mut buf = [0u8; 256];
        if let Some(value) = self.socket.as_ref().and_then(|s| {
            // Explicitly set timeout — do not rely on whatever was set by the
            // previous operation (rebind or audit).
            let _ = s.set_read_timeout(Some(RECV_TIMEOUT));
            s.recv_from(&mut buf)
                .ok()
                .and_then(|(len, _)| parse_fx_delay_response(&buf[..len]))
        }) {
            self.hardware_float_out
                .store(value.to_bits(), Ordering::Release);
            let _ = self.status_tx.try_send(NetworkStatus::DelayReadback(value));
            self.pulse_rx();
        }
    }

    // ── Heartbeat ────────────────────────────────────────────────────────

    fn send_heartbeat(&mut self) {
        let Some(target) = self.target else { return };

        // Send the /info probe.  Use .map() so the borrow on self.socket is
        // released before we need to mutate self.connected below.
        let sent = self
            .socket
            .as_ref()
            .map(|s| s.send_to(&osc::heartbeat(), target).is_ok())
            .unwrap_or(false);

        if !sent {
            self.socket = None;
            self.connected = false;
            self.hardware_float_out.store(0u32, Ordering::Release);
            let _ = self.status_tx.try_send(NetworkStatus::Disconnected);
            return;
        }
        self.pulse_tx();

        // Wait briefly for the response (timeout set during rebind).
        let mut buf = [0u8; 512];
        let recv_len = self
            .socket
            .as_ref()
            .and_then(|sock| sock.recv_from(&mut buf).ok().map(|(len, _)| len));

        match recv_len.filter(|&len| {
            decoder::decode_udp(&buf[..len])
                .map(|(_, pkt)| matches!(pkt, OscPacket::Message(ref m) if m.addr == "/info"))
                .unwrap_or(false)
        }) {
            Some(len) => {
                self.connected = true;
                self.backoff.record_success();
                let _ = self.status_tx.try_send(NetworkStatus::Connected);
                self.pulse_rx();
                // Write device identity directly — avoids a String allocation on
                // the audio thread that would otherwise receive DeviceIdentified.
                let (name, model) = parse_info_strings(&buf[..len]);
                if !name.is_empty() || !model.is_empty() {
                    *self.connected_device.lock() = (name, model);
                }
            }
            None => {
                self.connected = false;
                self.backoff.record_failure();
                let _ = self.status_tx.try_send(NetworkStatus::Disconnected);
            }
        }
    }

    // ── Slot audit ───────────────────────────────────────────────────────

    /// Query all 8 FX slots and update the shared slot-list mutexes directly.
    ///
    /// Results are written to `compatible_slots`, `occupied_slots`, and
    /// `slot_types` before sending the allocation-free `SlotScanDone` sentinel.
    /// Urgent sync commands are drained between slot queries so they aren't
    /// delayed for the full duration of the audit (up to 8 × RECV_TIMEOUT).
    fn audit_slots(&mut self) {
        if self.socket.is_none() || self.target.is_none() {
            return;
        }
        if let Some(s) = &self.socket {
            let _ = s.set_read_timeout(Some(RECV_TIMEOUT));
        }

        let mut compatible = Vec::new();
        let mut occupied = Vec::new();
        let mut slot_types = [None::<i32>; 8];
        let mut interrupted = false;

        'audit: for slot in 1u8..=8 {
            // Drain sync/reset commands between queries so they aren't delayed
            // for the full audit window.  AuditSlots and ScanTargets are skipped
            // to avoid re-entrancy; they'll be processed after this audit finishes.
            loop {
                match self.cmd_rx.try_recv() {
                    Ok(
                        cmd @ (NetworkCommand::SyncNow { .. }
                        | NetworkCommand::HardResetBatch { .. }
                        | NetworkCommand::Disconnect
                        | NetworkCommand::UpdateTarget { .. }
                        | NetworkCommand::ConnectToLast),
                    ) => self.handle(cmd),
                    Ok(_) => {} // defer AuditSlots / ScanTargets
                    Err(_) => break,
                }
            }

            // Re-check socket/target after command drain (Disconnect may have cleared them).
            let (Some(sock), Some(target)) = (&self.socket, self.target) else {
                interrupted = true;
                break 'audit;
            };

            if sock.send_to(&osc::query_fx_type(slot), target).is_err() {
                log::warn!("[EtherTap] audit: failed to send slot-{slot} query");
                continue;
            }
            let mut buf = [0u8; 256];
            let Some(sock) = &self.socket else {
                interrupted = true;
                break 'audit;
            };
            if let Ok((len, _)) = sock.recv_from(&mut buf) {
                if let Some(type_id) = parse_fx_type(&buf[..len]) {
                    slot_types[(slot - 1) as usize] = Some(type_id);
                    if osc::is_bpm_compatible(type_id, slot) {
                        compatible.push(slot);
                    }
                    occupied.push(slot);
                }
            }
        }

        if interrupted {
            log::info!("[EtherTap] audit_slots: interrupted (disconnect mid-audit), discarding partial results");
            return;
        }

        log::info!("[EtherTap] FX slot audit:");
        for slot in 1u8..=8 {
            match slot_types[(slot - 1) as usize] {
                Some(type_id) => {
                    let short = crate::osc::fx_type_short(type_id, slot);
                    let long = crate::osc::fx_type_long(type_id, slot);
                    let tag = if crate::osc::is_bpm_compatible(type_id, slot) {
                        "  [BPM-compatible]"
                    } else {
                        ""
                    };
                    log::info!("  Slot {slot}: {short}  ({long}){tag}");
                }
                None => log::info!("  Slot {slot}: no response"),
            }
        }
        log::info!("  Compatible: {:?}  Occupied: {:?}", compatible, occupied);

        // Write atomically — no allocation or lock on the audio thread.
        // slot_types stores each element individually; i32::MIN = not yet queried.
        for (i, t) in slot_types.iter().enumerate() {
            self.slot_types[i].store(t.unwrap_or(i32::MIN), Ordering::Relaxed);
        }
        self.compatible_slots
            .store(slots_to_bitmask(&compatible), Ordering::Release);
        self.occupied_slots
            .store(slots_to_bitmask(&occupied), Ordering::Release);
        let _ = self.status_tx.try_send(NetworkStatus::SlotScanDone);
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
    ///
    /// Runs on a dedicated short-lived thread (spawned from `handle()`) so the
    /// network worker remains fully responsive during the 600 ms window.
    fn scan_targets_bg(
        scan_targets: Arc<parking_lot::Mutex<Vec<DeviceInfo>>>,
        status_tx: Sender<NetworkStatus>,
        scan_generation: Arc<AtomicU64>,
        expected_gen: u64,
    ) {
        use std::{collections::HashMap, net::Ipv4Addr};

        let probe = osc::heartbeat();
        let window = Duration::from_millis(600);

        // ── One socket per real IPv4 interface ────────────────────────────
        struct Iface {
            sock: UdpSocket,
            local: Ipv4Addr,
            netmask: Ipv4Addr,
        }

        let mut ifaces: Vec<Iface> = Vec::new();

        let raw = if_addrs::get_if_addrs().unwrap_or_default();
        for iface in raw {
            let if_addrs::IfAddr::V4(v4) = iface.addr else {
                continue;
            };
            if v4.ip.is_loopback() {
                continue;
            }

            let Ok(sock) = UdpSocket::bind(format!("{}:0", v4.ip)) else {
                continue;
            };
            let _ = sock.set_broadcast(true);
            let bcast = v4
                .broadcast
                .unwrap_or_else(|| Ipv4Addr::from(u32::from(v4.ip) | !u32::from(v4.netmask)));
            let _ = sock.send_to(&probe, SocketAddr::from((bcast, 10023u16)));
            let _ = sock.set_nonblocking(true);
            ifaces.push(Iface {
                sock,
                local: v4.ip,
                netmask: v4.netmask,
            });
        }

        // Loopback socket so a local mock mixer is always discoverable.
        if let Ok(sock) = UdpSocket::bind("127.0.0.1:0") {
            let _ = sock.send_to(
                &probe,
                "127.0.0.1:10023"
                    .parse::<SocketAddr>()
                    .expect("loopback address literal"),
            );
            let _ = sock.set_nonblocking(true);
            ifaces.push(Iface {
                sock,
                local: Ipv4Addr::LOCALHOST,
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
                            if decoder::decode_udp(&buf[..len]).is_err() {
                                continue;
                            }
                            let src_v4 = match src.ip() {
                                std::net::IpAddr::V4(v) => v,
                                _ => continue,
                            };

                            let mask = u32::from(iface.netmask);
                            let same_subnet =
                                (u32::from(iface.local) & mask) == (u32::from(src_v4) & mask);
                            let latency_ms = probe_sent_at.elapsed().as_micros() as f32 / 1000.0;
                            let ip_key = src_v4.to_string();
                            let (name, model) = parse_info_strings(&buf[..len]);

                            let entry: RawEntry =
                                (ip_key.clone(), latency_ms, name, model, same_subnet);

                            match by_ip.get(&ip_key) {
                                None => {
                                    by_ip.insert(ip_key, entry);
                                }
                                Some((_, _, _, _, prev_same)) => {
                                    // Prefer same-subnet; within that, prefer lower latency.
                                    let better = (!*prev_same && same_subnet)
                                        || (*prev_same == same_subnet
                                            && latency_ms < by_ip[&ip_key].1);
                                    if better {
                                        by_ip.insert(ip_key, entry);
                                    }
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
            match (a.4, b.4) {
                // same_subnet: true sorts before false
                (true, false) => std::cmp::Ordering::Less,
                (false, true) => std::cmp::Ordering::Greater,
                _ => a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal),
            }
        });

        let mut result: Vec<DeviceInfo> = Vec::new();

        for (ip, latency_ms, name, model, same_subnet) in all {
            let has_id = !name.is_empty() || !model.is_empty();

            // Try to find an existing entry with the same identity.
            if has_id {
                if let Some(existing) = result
                    .iter_mut()
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
                ip: ip.clone(),
                port: 10023,
                name,
                model,
                latency_ms: Some(latency_ms),
                all_addrs: vec![(ip, Some(latency_ms), same_subnet)],
            });
        }

        // Merge into the shared mutex — the editor reads scan_targets;
        // process() receives ScanDone.
        //
        // Check the scan generation inside the lock so we can't race with the
        // editor's fetch_add + clear: if the generation changed, the editor
        // already cleared the list for a new scan and our results are stale.
        {
            let mut list = scan_targets.lock();
            if scan_generation.load(Ordering::Acquire) != expected_gen {
                log::debug!(
                    "[EtherTap] scan_targets_bg: generation changed, discarding stale results"
                );
                return;
            }
            for dev in result {
                let has_id = !dev.name.is_empty() || !dev.model.is_empty();
                let existing = if has_id {
                    list.iter_mut()
                        .find(|d| d.name == dev.name && d.model == dev.model)
                } else {
                    list.iter_mut().find(|d| d.ip == dev.ip)
                };
                match existing {
                    Some(e) => *e = dev,
                    None => list.push(dev),
                }
            }
        }
        let _ = status_tx.try_send(NetworkStatus::ScanDone);
    }

    // ── UDP helpers ───────────────────────────────────────────────────────

    fn rebind(&mut self) -> bool {
        match UdpSocket::bind("0.0.0.0:0") {
            Ok(sock) => {
                let _ = sock.set_read_timeout(Some(RECV_TIMEOUT));
                self.socket = Some(sock);
                true
            }
            Err(e) => {
                log::warn!("[EtherTap] failed to bind UDP socket: {e}");
                false
            }
        }
    }

    /// Send `bytes` to the target.  On failure, nulls the socket so that
    /// the next heartbeat cycle triggers a fresh `rebind()` attempt.
    fn send(&mut self, bytes: &[u8]) {
        let Some(target) = self.target else { return };
        let ok = self
            .socket
            .as_ref()
            .map(|s| s.send_to(bytes, target).is_ok())
            .unwrap_or(false);
        if !ok {
            self.socket = None;
            self.connected = false;
            self.hardware_float_out.store(0u32, Ordering::Release);
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
        let raw = if idx < 8 {
            self.slot_types[idx].load(Ordering::Relaxed)
        } else {
            i32::MIN
        };
        if raw == i32::MIN {
            log::warn!(
                "[EtherTap] slot_type_for: slot {slot} type unknown (audit pending), \
                 defaulting to DLY — wrong par address if slot holds a different effect"
            );
            10
        } else {
            raw
        }
    }
}

// ─── OSC response parsers ────────────────────────────────────────────────────

fn decode_osc_message(data: &[u8]) -> Option<OscMessage> {
    match decoder::decode_udp(data).ok()? {
        (_, OscPacket::Message(msg)) => Some(msg),
        _ => None,
    }
}

fn parse_fx_type(data: &[u8]) -> Option<i32> {
    let msg = decode_osc_message(data)?;
    match msg.args.first() {
        Some(OscType::Int(id)) => Some(*id),
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
    let strings: Vec<String> = msg
        .args
        .iter()
        .filter_map(|a| {
            if let OscType::String(s) = a {
                Some(s.clone())
            } else {
                None
            }
        })
        .collect();
    match strings.len() {
        0 => (String::new(), String::new()),
        // With a single string we don't know if it's version or name; skip it.
        1 => (String::new(), String::new()),
        // X32 layout: version, name, model[, firmware] — skip version (index 0).
        // With 2 args: (version, name) → return (name, "").
        2 => (strings[1].clone(), String::new()),
        // 3+ args: skip version, take name and model.
        _ => (strings[1].clone(), strings[2].clone()),
    }
}

fn parse_fx_delay_response(data: &[u8]) -> Option<f32> {
    let msg = decode_osc_message(data)?;
    match msg.args.first() {
        Some(OscType::Float(f)) => Some(*f),
        _ => None,
    }
}

// ─── Shared timing utility ───────────────────────────────────────────────────

/// Monotonic milliseconds since first call — used for activity-pulse LED timing.
///
/// Uses `Instant` rather than `SystemTime` so NTP clock adjustments cannot
/// make the timestamps jump backwards and cause LEDs to appear stuck on/off.
pub fn now_ms() -> u64 {
    use std::sync::OnceLock;
    use std::time::Instant;
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    EPOCH.get_or_init(Instant::now).elapsed().as_millis() as u64
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use rosc::{encoder, OscMessage};

    #[test]
    fn display_name_name_and_model() {
        let d = DeviceInfo {
            ip: "192.168.1.100".into(),
            port: 10023,
            name: "Studio Desk".into(),
            model: "X32".into(),
            latency_ms: None,
            all_addrs: vec![],
        };
        assert_eq!(d.display_name(), "Studio Desk (X32)");
    }

    #[test]
    fn display_name_name_only() {
        let d = DeviceInfo {
            ip: "192.168.1.100".into(),
            port: 10023,
            name: "Studio Desk".into(),
            model: "".into(),
            latency_ms: None,
            all_addrs: vec![],
        };
        assert_eq!(d.display_name(), "Studio Desk");
    }

    #[test]
    fn display_name_model_only() {
        let d = DeviceInfo {
            ip: "192.168.1.100".into(),
            port: 10023,
            name: "".into(),
            model: "X32".into(),
            latency_ms: None,
            all_addrs: vec![],
        };
        assert_eq!(d.display_name(), "X32");
    }

    #[test]
    fn display_name_fallback_to_ip() {
        let d = DeviceInfo {
            ip: "192.168.1.100".into(),
            port: 10023,
            name: "".into(),
            model: "".into(),
            latency_ms: None,
            all_addrs: vec![],
        };
        assert_eq!(d.display_name(), "192.168.1.100:10023");
    }

    #[test]
    fn display_name_name_equals_model_uses_name() {
        let d = DeviceInfo {
            ip: "192.168.1.100".into(),
            port: 10023,
            name: "X32".into(),
            model: "X32".into(),
            latency_ms: None,
            all_addrs: vec![],
        };
        assert_eq!(d.display_name(), "X32");
    }

    // ── parse_fx_type ────────────────────────────────────────────────────

    fn make_osc_msg(addr: &str, args: Vec<OscType>) -> Vec<u8> {
        let packet = OscPacket::Message(OscMessage {
            addr: addr.into(),
            args,
        });
        encoder::encode(&packet).expect("encode")
    }

    #[test]
    fn parse_fx_type_dly() {
        let data = make_osc_msg("/fx/1/type", vec![OscType::Int(10)]);
        assert_eq!(parse_fx_type(&data), Some(10));
    }

    #[test]
    fn parse_fx_type_reverb() {
        let data = make_osc_msg("/fx/2/type", vec![OscType::Int(1)]);
        assert_eq!(parse_fx_type(&data), Some(1));
    }

    #[test]
    fn parse_fx_type_empty_slot() {
        // Empty slots do not respond → parse None from non-OSC data
        assert_eq!(parse_fx_type(b""), None);
        assert_eq!(parse_fx_type(b"garbage"), None);
    }

    #[test]
    fn parse_fx_type_float_arg_returns_none() {
        // Wrong argument type
        let data = make_osc_msg("/fx/1/type", vec![OscType::Float(0.5)]);
        assert_eq!(parse_fx_type(&data), None);
    }

    // ── parse_info_strings ───────────────────────────────────────────────

    #[test]
    fn parse_info_strings_full_response() {
        let data = make_osc_msg(
            "/info",
            vec![
                OscType::String("V2.12".into()),
                OscType::String("Studio Desk".into()),
                OscType::String("X32".into()),
                OscType::String("4.06".into()),
            ],
        );
        let (name, model) = parse_info_strings(&data);
        assert_eq!(name, "Studio Desk");
        assert_eq!(model, "X32");
    }

    #[test]
    fn parse_info_strings_no_args() {
        let data = make_osc_msg("/info", vec![]);
        let (name, model) = parse_info_strings(&data);
        assert_eq!(name, "");
        assert_eq!(model, "");
    }

    #[test]
    fn parse_info_strings_single_arg() {
        // Single string is ambiguous (version or name) — return empty to be safe.
        let data = make_osc_msg("/info", vec![OscType::String("V2.12".into())]);
        let (name, model) = parse_info_strings(&data);
        assert_eq!(name, "");
        assert_eq!(model, "");
    }

    #[test]
    fn parse_info_strings_two_args() {
        // X32 layout: (version, name) — skip version, return name.
        let data = make_osc_msg(
            "/info",
            vec![
                OscType::String("V2.12".into()),
                OscType::String("Studio Desk".into()),
            ],
        );
        let (name, model) = parse_info_strings(&data);
        assert_eq!(name, "Studio Desk");
        assert_eq!(model, "");
    }

    #[test]
    fn parse_info_strings_garbage_data() {
        let (name, model) = parse_info_strings(b"\xff\xfe\x00\x01");
        assert_eq!(name, "");
        assert_eq!(model, "");
    }

    // ── parse_fx_delay_response ──────────────────────────────────────────

    #[test]
    fn parse_fx_delay_response_float() {
        let data = make_osc_msg("/fx/1/par/02", vec![OscType::Float(0.1667)]);
        let f = parse_fx_delay_response(&data);
        assert!((f.unwrap() - 0.1667).abs() < 0.001);
    }

    #[test]
    fn parse_fx_delay_response_int_returns_none() {
        let data = make_osc_msg("/fx/1/par/02", vec![OscType::Int(42)]);
        assert_eq!(parse_fx_delay_response(&data), None);
    }

    #[test]
    fn parse_fx_delay_response_empty_returns_none() {
        assert_eq!(parse_fx_delay_response(b""), None);
        assert_eq!(parse_fx_delay_response(b"garbage"), None);
    }

    // ── Backoff fuzz: full cycle ──────────────────────────────────────────

    #[test]
    fn backoff_full_cycle() {
        let mut b = crate::reconnect::Backoff::new(2000, 10000);
        assert_eq!(b.next_delay_ms(), 2000);
        assert!(!b.is_cooling_down());

        b.record_failure();
        assert_eq!(b.next_delay_ms(), 4000);
        assert!(b.is_cooling_down());

        b.record_failure();
        assert_eq!(b.next_delay_ms(), 8000);

        b.record_failure();
        assert_eq!(b.next_delay_ms(), 10000, "should cap at cap_ms");

        b.record_failure();
        assert_eq!(b.next_delay_ms(), 10000, "should stay capped");

        b.record_success();
        assert_eq!(b.next_delay_ms(), 2000, "should reset to base");
        assert!(!b.is_cooling_down());
    }

    #[test]
    fn backoff_reset_clears_everything() {
        let mut b = crate::reconnect::Backoff::new(1000, 5000);
        b.record_failure();
        b.record_failure();
        b.record_failure();
        assert_eq!(b.next_delay_ms(), 5000);
        b.reset();
        assert_eq!(b.next_delay_ms(), 1000);
        assert!(!b.is_cooling_down());
    }

    #[test]
    fn scan_sort_prefers_same_subnet() {
        // RawEntry = (ip, latency_ms, name, model, same_subnet)
        type RawEntry = (String, f32, String, String, bool);
        let mut entries: Vec<RawEntry> = vec![
            ("10.0.0.1".into(), 5.0, "A".into(), "X32".into(), false),
            ("192.168.1.100".into(), 2.0, "B".into(), "X32".into(), true),
            ("172.16.0.1".into(), 1.0, "C".into(), "X32".into(), false),
        ];
        entries.sort_by(|a, b| match (a.4, b.4) {
            (true, false) => std::cmp::Ordering::Less,
            (false, true) => std::cmp::Ordering::Greater,
            _ => a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal),
        });
        assert!(entries[0].4, "first entry should be same-subnet");
        assert_eq!(entries[0].0, "192.168.1.100");
        // non-subnet entries sorted ascending by latency
        assert!(entries[1].1 <= entries[2].1);
    }

    #[test]
    fn backoff_immutable_queries() {
        let b = crate::reconnect::Backoff::new(1000, 10000);
        assert!(!b.is_cooling_down());
        assert_eq!(b.next_delay_ms(), 1000);
        // These don't mutate
        let b2 = crate::reconnect::Backoff::new(500, 1000);
        assert_eq!(b2.next_delay_ms(), 500);
    }
}
