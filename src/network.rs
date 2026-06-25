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
/// Consecutive heartbeat failures before an auto_reconnect rescan kicks in.
const AUTO_RESCAN_FAILURES: u32 = 3;
/// Listen window for the synchronous identity rescan.
const RESCAN_WINDOW: Duration = Duration::from_millis(600);

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
    /// Mirrored from the `auto_reconnect` host param each `process()` call.
    /// ON: the worker self-connects to the persisted target on startup and
    /// retargets via identity-verified rescan when the device moved.
    pub auto_reconnect: Arc<std::sync::atomic::AtomicBool>,
    /// Persisted (name, model) of the last connected device. Empty until the
    /// first successful connect; verified on auto-reconnect.
    pub last_device: Arc<Mutex<(String, String)>>,
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
    /// UDP port scan probes are sent to. Always 10023 (the real mixer port)
    /// in production; tests override it so a MockMixer on an OS-assigned port
    /// is discoverable without colliding across parallel test binaries.
    scan_port: u16,
    /// Mirror of the `auto_reconnect` host param (see [`WorkerShared`]).
    auto_reconnect: Arc<std::sync::atomic::AtomicBool>,
    /// Persisted (name, model) of the last connected device.
    last_device: Arc<Mutex<(String, String)>>,
    /// True when the current target was established by the run-loop
    /// self-connect (auto-resume) rather than an explicit user command.
    /// Auto-resumed targets get identity verification; explicit ones adopt
    /// whatever identity answers — the user chose that device.
    target_from_auto: bool,
    /// Consecutive heartbeat failures since the last success; at
    /// [`AUTO_RESCAN_FAILURES`] with auto_reconnect ON, the worker rescans
    /// for the persisted identity in case the device's address moved.
    heartbeat_failures: u32,
    /// Throttle for run-loop self-connect attempts.
    last_auto_attempt: Option<Instant>,
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
            scan_port: 10023,
            auto_reconnect: shared.auto_reconnect,
            last_device: shared.last_device,
            target_from_auto: false,
            heartbeat_failures: 0,
            last_auto_attempt: None,
        }
    }

    /// Override the port scan probes target. Test hook — production code
    /// never calls this; the real mixer always listens on 10023.
    #[doc(hidden)]
    pub fn set_scan_port(&mut self, port: u16) {
        self.scan_port = port;
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

            // Auto-resume: with the auto_reconnect param ON, no target, and a
            // persisted address available, connect without any user pulse.
            // This is the authoritative auto-connect path — `initialize()`
            // sends nothing, so hosts that restore param state after init
            // still converge here once `process()` mirrors the atom.
            self.maybe_auto_connect();

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
            Err(_) => {
                // Count as a failure so the auto-resume throttle backs off
                // instead of retrying an unparseable address at loop cadence.
                self.backoff.record_failure();
                log::warn!("[EtherTap] invalid target: {ip}:{port}");
            }
        }
    }

    /// Run-loop self-connect: auto_reconnect ON, no explicit disconnect, no
    /// current target, persisted address available, and the throttle expired.
    fn maybe_auto_connect(&mut self) {
        if self.target.is_some()
            || self.user_disconnected
            || !self.auto_reconnect.load(Ordering::Relaxed)
        {
            return;
        }
        // Throttle attempts by the shared backoff so a dead persisted target
        // doesn't get hammered at loop cadence.
        let delay = Duration::from_millis(self.backoff.next_delay_ms());
        if self.last_auto_attempt.is_some_and(|t| t.elapsed() < delay) {
            return;
        }
        let ip = self.target_ip.lock().clone();
        let port = *self.target_port.lock();
        if ip.is_empty() || port == 0 {
            return;
        }
        self.last_auto_attempt = Some(Instant::now());
        self.target_from_auto = true;
        log::info!("[EtherTap] auto_reconnect: resuming last target {ip}:{port}");
        self.connect(ip, port);
    }

    fn handle(&mut self, cmd: NetworkCommand) {
        match cmd {
            NetworkCommand::UpdateTarget { ip, port } => {
                self.target_from_auto = false;
                self.connect(ip, port);
            }

            NetworkCommand::ConnectToLast => {
                let ip = self.target_ip.lock().clone();
                let port = *self.target_port.lock();
                self.target_from_auto = false;
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
                let scan_port = self.scan_port;
                std::thread::Builder::new()
                    .name("ethertap-scan".into())
                    .spawn(move || {
                        NetworkWorker::scan_targets_bg(
                            scan_targets,
                            status_tx,
                            scan_gen,
                            my_gen,
                            scan_port,
                        )
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
                let (name, model) = parse_info_strings(&buf[..len]);

                // Identity check for auto-resumed targets: if a *different*
                // device answers at the persisted address (DHCP moved things),
                // reject it and rescan for the persisted identity instead.
                if self.target_from_auto && !name.is_empty() {
                    let expected = self.last_device.lock().clone();
                    let known = !expected.0.is_empty() || !expected.1.is_empty();
                    if known && (expected.0 != name || expected.1 != model) {
                        log::warn!(
                            "[EtherTap] auto_reconnect: expected {expected:?}, got \
                             ({name:?}, {model:?}) — rescanning for the device"
                        );
                        self.target = None;
                        self.connected = false;
                        self.rescan_for_last_device();
                        return;
                    }
                }

                self.connected = true;
                self.heartbeat_failures = 0;
                self.backoff.record_success();
                let _ = self.status_tx.try_send(NetworkStatus::Connected);
                self.pulse_rx();
                // Write device identity directly — avoids a String allocation on
                // the audio thread that would otherwise receive DeviceIdentified.
                if !name.is_empty() || !model.is_empty() {
                    *self.connected_device.lock() = (name.clone(), model.clone());
                    // Write-through persist: explicit connects adopt whatever
                    // device the user pointed at; auto-resumes only reach here
                    // with a matching (or previously unknown) identity.
                    *self.last_device.lock() = (name, model);
                }
            }
            None => {
                self.connected = false;
                self.heartbeat_failures = self.heartbeat_failures.saturating_add(1);
                self.backoff.record_failure();
                let _ = self.status_tx.try_send(NetworkStatus::Disconnected);

                // The device may have moved address entirely — after a few
                // straight failures, try to find it again by identity.
                if self.heartbeat_failures >= AUTO_RESCAN_FAILURES
                    && self.auto_reconnect.load(Ordering::Relaxed)
                {
                    self.heartbeat_failures = 0;
                    self.rescan_for_last_device();
                }
            }
        }
    }

    /// Synchronous LAN rescan for the persisted device identity. On a hit,
    /// write the new address through to the persisted target and connect to
    /// it (as an auto-resumed target, so identity stays verified).
    ///
    /// Blocks the worker for [`RESCAN_WINDOW`] — acceptable: it only runs
    /// while disconnected, when there is nothing to sync or poll.
    fn rescan_for_last_device(&mut self) {
        debug_assert!(!self.connected, "rescan must not run while connected");
        let expected = self.last_device.lock().clone();
        if expected.0.is_empty() && expected.1.is_empty() {
            return;
        }
        let devices = Self::scan_collect(self.scan_port, RESCAN_WINDOW);
        let hit = devices
            .into_iter()
            .find(|d| d.name == expected.0 && d.model == expected.1);
        match hit {
            Some(dev) => {
                log::info!(
                    "[EtherTap] auto_reconnect: found {:?} at {}:{}",
                    dev.display_name(),
                    dev.ip,
                    dev.port
                );
                *self.target_ip.lock() = dev.ip.clone();
                *self.target_port.lock() = dev.port;
                self.target_from_auto = true;
                self.connect(dev.ip, dev.port);
            }
            None => {
                self.backoff.record_failure();
                log::debug!("[EtherTap] auto_reconnect: device {expected:?} not found in rescan");
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
        scan_port: u16,
    ) {
        let result = Self::scan_collect(scan_port, Duration::from_millis(600));

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

    /// Probe every IPv4 interface (plus loopback) on `scan_port` and collect
    /// the devices that answer within `window`. Pure collection — no shared
    /// state; used by the editor-driven background scan and the synchronous
    /// auto-reconnect rescan.
    fn scan_collect(scan_port: u16, window: Duration) -> Vec<DeviceInfo> {
        use std::{collections::HashMap, net::Ipv4Addr};

        let probe = osc::heartbeat();

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
            let _ = sock.send_to(&probe, SocketAddr::from((bcast, scan_port)));
            let _ = sock.set_nonblocking(true);
            ifaces.push(Iface {
                sock,
                local: v4.ip,
                netmask: v4.netmask,
            });
        }

        // Loopback socket so a local mock mixer is always discoverable.
        if let Ok(sock) = UdpSocket::bind("127.0.0.1:0") {
            let _ = sock.send_to(&probe, SocketAddr::from((Ipv4Addr::LOCALHOST, scan_port)));
            let _ = sock.set_nonblocking(true);
            ifaces.push(Iface {
                sock,
                local: Ipv4Addr::LOCALHOST,
                netmask: Ipv4Addr::new(255, 0, 0, 0),
            });
        }

        // ── Collect responses — raw entry per (socket, device) pair ───────
        // Tuple: (ip, latency_ms, name, model, same_subnet, is_loopback)
        type RawEntry = (String, f32, String, String, bool, bool);

        // ip_key → best raw entry (loopback wins, then same-subnet; ties broken by latency)
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
                            let is_loopback = src_v4.is_loopback();
                            let latency_ms = probe_sent_at.elapsed().as_micros() as f32 / 1000.0;
                            let ip_key = src_v4.to_string();
                            let (name, model) = parse_info_strings(&buf[..len]);

                            let entry: RawEntry = (
                                ip_key.clone(),
                                latency_ms,
                                name,
                                model,
                                same_subnet,
                                is_loopback,
                            );

                            match by_ip.get(&ip_key) {
                                None => {
                                    by_ip.insert(ip_key, entry);
                                }
                                Some((_, _, _, _, prev_same, prev_loopback)) => {
                                    // Prefer loopback, then same-subnet; within that, lower latency.
                                    let better = (!*prev_loopback && is_loopback)
                                        || (*prev_loopback == is_loopback
                                            && ((!*prev_same && same_subnet)
                                                || (*prev_same == same_subnet
                                                    && latency_ms < by_ip[&ip_key].1)));
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
        // Sort: loopback first, then same-subnet before routed, then ascending latency.
        let mut all: Vec<RawEntry> = by_ip.into_values().collect();
        all.sort_by(|a, b| {
            match (a.5, b.5) {
                // is_loopback: true sorts before false
                (true, false) => std::cmp::Ordering::Less,
                (false, true) => std::cmp::Ordering::Greater,
                _ => match (a.4, b.4) {
                    // same_subnet: true sorts before false
                    (true, false) => std::cmp::Ordering::Less,
                    (false, true) => std::cmp::Ordering::Greater,
                    _ => a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal),
                },
            }
        });

        let mut result: Vec<DeviceInfo> = Vec::new();

        for (ip, latency_ms, name, model, same_subnet, _is_loopback) in all {
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
                port: scan_port,
                name,
                model,
                latency_ms: Some(latency_ms),
                all_addrs: vec![(ip, Some(latency_ms), same_subnet)],
            });
        }

        result
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

    #[test]
    fn parse_fx_delay_response_malformed_packets_return_none() {
        // rosc is lenient with null type-tag area; ethertap must still return None
        // because the packet carries no float arg.
        let truncated_type_tag = &[
            0x2f, 0x66, 0x78, 0x00, // "/fx\0"
            0x00, 0x00, 0x00, 0x00, // null type tag area
            0x2c, 0x66, 0x00, // trailing ",f\0" (ignored by rosc)
        ];
        assert_eq!(parse_fx_delay_response(truncated_type_tag), None);

        let garbage_after_header = &[
            0x2f, 0x69, 0x6e, 0x66, 0x6f, 0x00, 0x00, 0x00, // "/info\0\0\0"
            0x00, 0x00, 0x00, 0x00, // null type tag area
            0xff, 0xff, 0xff, 0xff, // garbage
        ];
        assert_eq!(parse_fx_delay_response(garbage_after_header), None);
    }

    #[test]
    fn parse_fx_type_unknown_address_still_extracts_int() {
        // parse_fx_type does not validate the address — it extracts the first Int
        // arg from any valid OSC message. This is the current design: the network
        // worker uses request/response pairing and never passes a stray-address
        // packet to parse_fx_type. Document that behavior here.
        let data = make_osc_msg("/wrong/address", vec![OscType::Int(10)]);
        assert_eq!(parse_fx_type(&data), Some(10));

        // No-arg packet: returns None regardless of address.
        let data_no_args = make_osc_msg("/wrong/address", vec![]);
        assert_eq!(parse_fx_type(&data_no_args), None);
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
        // Mirrors production sort: (ip, latency_ms, name, model, same_subnet, is_loopback)
        // Tier order: loopback > same-subnet > other, ties broken by ascending latency.
        type RawEntry = (String, f32, String, String, bool, bool);
        let mut entries: Vec<RawEntry> = vec![
            (
                "10.0.0.1".into(),
                5.0,
                "A".into(),
                "X32".into(),
                false,
                false,
            ),
            (
                "192.168.1.100".into(),
                2.0,
                "B".into(),
                "X32".into(),
                true,
                false,
            ),
            (
                "172.16.0.1".into(),
                1.0,
                "C".into(),
                "X32".into(),
                false,
                false,
            ),
            (
                "127.0.0.1".into(),
                0.5,
                "D".into(),
                "X32".into(),
                false,
                true,
            ),
        ];
        let sort_fn = |a: &RawEntry, b: &RawEntry| match (a.5, b.5) {
            (true, false) => std::cmp::Ordering::Less,
            (false, true) => std::cmp::Ordering::Greater,
            _ => match (a.4, b.4) {
                (true, false) => std::cmp::Ordering::Less,
                (false, true) => std::cmp::Ordering::Greater,
                _ => a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal),
            },
        };
        entries.sort_by(sort_fn);
        // Loopback sorts first regardless of latency or subnet.
        assert!(entries[0].5, "loopback must be first");
        assert_eq!(entries[0].0, "127.0.0.1");
        // Same-subnet sorts before routed.
        assert!(entries[1].4, "same-subnet must precede routed");
        assert_eq!(entries[1].0, "192.168.1.100");
        // Remaining entries sorted ascending by latency.
        assert!(
            entries[2].1 <= entries[3].1,
            "routed entries sorted by latency"
        );

        // A 2-element sort with loopback FIRST makes Rust call cmp(data[1], data[0])
        // = cmp(non-loopback, loopback), exercising the (false, true) arm.
        let mut two = [
            (
                "127.0.0.1".into(),
                0.5f32,
                "".into(),
                "".into(),
                false,
                true,
            ), // loopback at [0]
            (
                "192.168.1.1".into(),
                1.0f32,
                "".into(),
                "".into(),
                false,
                false,
            ), // non-loopback at [1]
        ];
        two.sort_by(sort_fn);
        assert!(
            two[0].5,
            "loopback remains first in a 2-element already-sorted list"
        );
    }

    #[test]
    fn backoff_immutable_queries() {
        // Prove is_cooling_down() and next_delay_ms() don't consume or mutate state.
        let mut b = crate::reconnect::Backoff::new(1000, 10000);
        b.record_failure();

        let cooling1 = b.is_cooling_down();
        let delay1 = b.next_delay_ms();
        let cooling2 = b.is_cooling_down();
        let delay2 = b.next_delay_ms();

        assert_eq!(cooling1, cooling2, "is_cooling_down() must be idempotent");
        assert_eq!(delay1, delay2, "next_delay_ms() must be idempotent");
    }

    fn make_test_worker() -> NetworkWorker {
        let (_cmd_tx, cmd_rx) = crossbeam_channel::bounded(8);
        let (status_tx, _status_rx) = crossbeam_channel::bounded(8);
        NetworkWorker::new(
            cmd_rx,
            status_tx,
            Arc::new(Mutex::new("192.168.1.100".to_string())),
            Arc::new(Mutex::new(10023u16)),
            Arc::new(AtomicU8::new(1)),
            WorkerShared {
                hardware_float_out: Arc::new(AtomicU32::new(0)),
                compatible_slots: Arc::new(AtomicU8::new(0)),
                occupied_slots: Arc::new(AtomicU8::new(0)),
                slot_types: Arc::new(std::array::from_fn(|_| AtomicI32::new(i32::MIN))),
                scan_targets: Arc::new(Mutex::new(Vec::new())),
                connected_device: Arc::new(Mutex::new((String::new(), String::new()))),
                scan_generation: Arc::new(AtomicU64::new(0)),
                auto_reconnect: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                last_device: Arc::new(Mutex::new((String::new(), String::new()))),
            },
        )
    }

    /// Passing an unparseable IP string to connect() must record a backoff failure
    /// instead of panicking. Covers the Err branch of SocketAddr::parse.
    #[test]
    fn connect_invalid_address_records_backoff_failure() {
        let mut worker = make_test_worker();
        assert!(!worker.backoff.is_cooling_down());
        // "256.256.256.256" is an invalid IPv4 address — parse() returns Err.
        worker.connect("256.256.256.256".to_string(), 10023);
        assert!(
            worker.backoff.is_cooling_down(),
            "invalid address must trigger backoff record_failure"
        );
    }

    /// maybe_auto_connect skips a second attempt within the backoff delay window.
    /// Uses an unparseable address so connect() takes the Err branch (doesn't set
    /// self.target), ensuring the SECOND call is throttled by last_auto_attempt
    /// (line 347-348) rather than the earlier `target.is_some()` guard.
    #[test]
    fn maybe_auto_connect_throttled_by_backoff() {
        let mut worker = make_test_worker();
        worker.auto_reconnect.store(true, Ordering::Relaxed);
        // Invalid address: parse() returns Err → connect() doesn't set self.target.
        *worker.target_ip.lock() = "256.256.256.256".to_string();
        *worker.target_port.lock() = 10023;
        // First call: last_auto_attempt is None → proceeds past throttle → connect fails.
        worker.maybe_auto_connect();
        assert!(
            worker.last_auto_attempt.is_some(),
            "last_auto_attempt must be set after first call"
        );
        assert!(
            worker.target.is_none(),
            "target stays None when address is invalid"
        );
        // Capture backoff delay after first call (connect() recorded one failure).
        let delay_after_first = worker.backoff.next_delay_ms();
        assert!(
            delay_after_first > 2000,
            "first failed connect must have incremented backoff delay"
        );

        // Immediately call again: elapsed ≈ 0ms < delay_after_first → throttle.
        // The throttled path must NOT call connect() again, so backoff must not grow.
        worker.maybe_auto_connect();
        assert_eq!(
            worker.backoff.next_delay_ms(),
            delay_after_first,
            "throttled second call must not increment backoff (connect() must be skipped)"
        );
    }

    /// rescan_for_last_device returns immediately when last_device identity is empty.
    /// Covers the early-return guard at line 585-586.
    #[test]
    fn rescan_for_last_device_returns_early_for_empty_identity() {
        let mut worker = make_test_worker();
        // last_device defaults to ("", "") in make_test_worker — rescan must return early.
        worker.rescan_for_last_device();
        // If it didn't return early it would block for RESCAN_WINDOW scanning the LAN.
        // Verify no connection was attempted: backoff must remain idle (no failures recorded).
        assert!(
            !worker.backoff.is_cooling_down(),
            "rescan with empty identity must not attempt connection (backoff must stay idle)"
        );
        assert!(
            worker.target.is_none(),
            "rescan with empty identity must not retarget the worker"
        );
    }

    /// maybe_auto_connect returns early when the target IP is empty.
    #[test]
    fn maybe_auto_connect_skips_empty_ip() {
        let mut worker = make_test_worker();
        worker.auto_reconnect.store(true, Ordering::Relaxed);
        *worker.target_ip.lock() = String::new(); // empty → early return
        worker.maybe_auto_connect();
        // No connection attempt must have been recorded.
        assert!(
            worker.last_auto_attempt.is_none(),
            "empty IP must not record an auto-connect attempt"
        );
        assert!(
            !worker.backoff.is_cooling_down(),
            "empty IP early-return must not trigger a backoff failure"
        );
    }

    /// A valid OSC bundle must not be decoded as a Message.
    /// Exercises the `_ => None` arm in `decode_osc_message`.
    #[test]
    fn decode_osc_bundle_returns_none() {
        use rosc::{encoder, OscBundle, OscPacket, OscTime};
        let bundle_bytes = encoder::encode(&OscPacket::Bundle(OscBundle {
            timetag: OscTime {
                seconds: 0,
                fractional: 1,
            },
            content: vec![],
        }))
        .unwrap_or_default();
        assert!(
            decode_osc_message(&bundle_bytes).is_none(),
            "OSC bundle must not decode as a Message"
        );
    }

    /// An OSC message whose args are all non-String values must yield empty
    /// name and model strings. Exercises the `else { None }` arm of the
    /// filter_map inside `parse_info_strings`.
    #[test]
    fn parse_info_strings_non_string_arg_filtered_out() {
        use rosc::{encoder, OscMessage, OscPacket, OscType};
        let msg_bytes = encoder::encode(&OscPacket::Message(OscMessage {
            addr: "/info".to_string(),
            args: vec![OscType::Int(42), OscType::Int(99)],
        }))
        .unwrap();
        let (name, model) = parse_info_strings(&msg_bytes);
        assert!(name.is_empty(), "int args should produce empty name");
        assert!(model.is_empty(), "int args should produce empty model");
    }

    /// Slot index above 8 (out-of-bounds) must return the DLY fallback (10).
    /// Exercises the `i32::MIN` path in `slot_type_for` (idx >= 8).
    #[test]
    fn slot_type_for_above_8_returns_default() {
        let w = make_test_worker();
        // slot 9 → idx = 8 → out of the [AtomicI32; 8] bounds → raw = i32::MIN
        // → defaults to DLY type (10).
        let ty = w.slot_type_for(9);
        assert_eq!(
            ty, 10,
            "slot 9 (out of range) should default to DLY type 10"
        );
    }
}
