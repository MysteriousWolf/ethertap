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
    sync::Arc,
    sync::atomic::{AtomicI32, AtomicU8, AtomicU32, AtomicU64, Ordering},
    time::{Duration, Instant},
};

use crossbeam_channel::{Receiver, Sender, TryRecvError};
use nice_plug::{nice_log, nice_trace, nice_warn};
use parking_lot::Mutex;
use rosc::{OscMessage, OscPacket, OscType, decoder};

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

/// Full listen window for one scan.  Must outlast the whole probe burst plus
/// the console's reply latency — an X32 answers `/info` in a few ms, but a
/// congested Wi-Fi link or a console still finishing its boot can take far
/// longer, and a scan that stops listening early reports "nothing there".
const SCAN_WINDOW: Duration = Duration::from_millis(1000);
/// Probe retransmits inside one scan window.  Broadcast UDP has no delivery
/// guarantee and a single lost datagram used to mean an empty device list.
const SCAN_PROBE_BURST: usize = 3;
/// Spacing between the retransmits of a burst.
const SCAN_PROBE_SPACING: Duration = Duration::from_millis(200);
/// Consecutive zero-reply scans (with interfaces present) before the worker
/// reports [`ScanHealth::NoReplies`] — one empty scan just means "nothing on
/// the LAN yet", a run of them means the probes are very likely being dropped.
const SCAN_HEALTH_FAILURES: u32 = 3;

/// Discovery cadence floor while disconnected — fast enough that a console
/// powering up alongside the DAW is found within seconds.
const DISCOVERY_INTERVAL_MIN: Duration = Duration::from_secs(5);
/// Discovery cadence ceiling — reached by doubling after fruitless scans so an
/// idle session with no mixer on the network settles to one scan per 30 s.
const DISCOVERY_INTERVAL_MAX: Duration = Duration::from_secs(30);
/// Fruitless discovery passes that keep the floor cadence when the worker is
/// hunting a console it just lost (address rejected, or heartbeats stopped).
///
/// The ordinary ramp assumes "nothing found" means "nothing is out there". A
/// retarget search knows better: the console answered seconds ago, so an empty
/// scan is far more likely to be a dropped probe than an absent device. Four
/// passes ≈ 25 s of floor-cadence searching before the ramp resumes, which
/// covers a DHCP lease change without turning a genuinely powered-down console
/// into a permanent scan every 5 s.
const RETARGET_FAST_PASSES: u32 = 4;

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

// ─── Scan health ─────────────────────────────────────────────────────────────

/// Why the device list looks the way it does.
///
/// A dropped UDP probe is indistinguishable from "no mixer answered" at the
/// socket layer — `send_to` succeeds either way.  On macOS 15+ a host app
/// without the Local Network privacy grant has every LAN datagram discarded
/// silently, which presents exactly like an empty network.  Publishing this
/// state lets the editor tint its scan control instead of leaving the user
/// staring at an empty list with no idea whether to look at the mixer, the
/// cabling, or a permission prompt they dismissed months ago.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ScanHealth {
    /// No scan has completed yet.
    Unknown = 0,
    /// The last scan received at least one reply.
    Ok = 1,
    /// Interfaces were probed but nothing has answered for several scans.
    NoReplies = 2,
    /// No usable IPv4 interface — the machine has no network to scan.
    NoInterfaces = 3,
}

impl ScanHealth {
    pub fn from_u8(v: u8) -> Self {
        match v {
            1 => ScanHealth::Ok,
            2 => ScanHealth::NoReplies,
            3 => ScanHealth::NoInterfaces,
            _ => ScanHealth::Unknown,
        }
    }
}

/// Per-scan counters, logged after every scan and used to derive [`ScanHealth`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScanStats {
    /// Local IPv4 addresses probed from, excluding the loopback fallback.
    /// Doubles as the interface fingerprint used to notice the network coming
    /// up after the plugin loaded.
    pub addrs: Vec<String>,
    /// Probe datagrams handed to the socket layer.
    pub probes: usize,
    /// Decodable replies received, before de-duplication by address.
    pub replies: usize,
}

impl ScanStats {
    /// Usable IPv4 interfaces probed.
    pub fn ifaces(&self) -> usize {
        self.addrs.len()
    }
}

/// The shared sinks every scan writes to, whoever ran it.
///
/// Both the worker's own discovery pass and the editor-triggered background
/// scan thread publish through one of these, so the device list and the health
/// counter mean the same thing regardless of which path produced them.
#[derive(Clone)]
struct ScanPublisher {
    scan_targets: Arc<Mutex<Vec<DeviceInfo>>>,
    scan_health: Arc<AtomicU8>,
    /// Consecutive scans that probed real interfaces and received nothing.
    empty_scans: Arc<AtomicU32>,
}

impl ScanPublisher {
    /// Merge a scan's devices into the shared list and publish its health.
    ///
    /// `guard` is the scan generation this result belongs to. It is checked
    /// while the device list is locked, so a scan that finished after the
    /// editor started a fresh one cannot resurrect its stale devices.
    /// Returns `None` when the result was discarded for that reason.
    fn record(
        &self,
        devices: &[DeviceInfo],
        stats: &ScanStats,
        guard: Option<(&AtomicU64, u64)>,
    ) -> Option<ScanHealth> {
        {
            let mut list = self.scan_targets.lock();
            if let Some((generation, expected)) = guard
                && generation.load(Ordering::Acquire) != expected
            {
                nice_trace!("[EtherTap] scan: generation changed, discarding stale results");
                return None;
            }
            merge_devices(&mut list, devices.iter().cloned());
        }

        let empty_run = if stats.replies > 0 {
            self.empty_scans.store(0, Ordering::Relaxed);
            0
        } else {
            self.empty_scans.fetch_add(1, Ordering::Relaxed) + 1
        };

        let health = if stats.ifaces() == 0 {
            ScanHealth::NoInterfaces
        } else if stats.replies > 0 {
            ScanHealth::Ok
        } else if empty_run >= SCAN_HEALTH_FAILURES {
            ScanHealth::NoReplies
        } else {
            // One quiet scan is not evidence of anything — hold the previous
            // verdict rather than flapping the editor's colour every 5 s.
            ScanHealth::from_u8(self.scan_health.load(Ordering::Relaxed))
        };
        self.scan_health.store(health as u8, Ordering::Relaxed);

        nice_trace!(
            "[EtherTap] scan: {} iface(s), {} probe(s), {} reply(ies) → {} device(s), health {health:?}",
            stats.ifaces(),
            stats.probes,
            stats.replies,
            devices.len(),
        );
        if health == ScanHealth::NoReplies {
            nice_warn!(
                "[EtherTap] {empty_run} consecutive scans probed {} interface(s) and got no \
                 reply. If a console is powered on and cabled, the probes are most likely \
                 being discarded — on macOS check the host application's Local Network \
                 permission, otherwise check the firewall and that the console shares a subnet.",
                stats.ifaces(),
            );
        }
        Some(health)
    }
}

/// Merge freshly-scanned devices into `list`, replacing entries that describe
/// the same device.  Identity `(name, model)` wins when the console reported
/// one; otherwise entries are matched by address.
fn merge_devices(list: &mut Vec<DeviceInfo>, devices: impl IntoIterator<Item = DeviceInfo>) {
    for dev in devices {
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
    /// Latest [`ScanHealth`] as a `u8`. Read by the editor to tint its scan
    /// control; never gates behaviour.
    pub scan_health: Arc<AtomicU8>,
    /// Persisted mirror of the slot audit, written by the worker after every
    /// audit so a reloaded session starts with the last known slot map.
    pub last_slot_types: Arc<Mutex<[i32; 8]>>,
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
    /// Shared sinks every scan publishes through, whoever ran it.
    scan: ScanPublisher,
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
    /// Persisted mirror of the slot audit (see [`WorkerShared`]).
    last_slot_types: Arc<Mutex<[i32; 8]>>,
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
    /// When the worker's own discovery last ran.  `None` = never.
    last_discovery: Option<Instant>,
    /// Current background-discovery cadence; doubles on a fruitless scan up to
    /// [`DISCOVERY_INTERVAL_MAX`], snaps back to the floor on a hit.
    discovery_interval: Duration,
    /// IPv4 addresses seen on the last discovery.  A change means the machine's
    /// network came up (or moved), which re-arms discovery immediately instead
    /// of waiting out the ramped interval.
    last_iface_set: Vec<String>,
    /// Set when the heartbeat has failed enough times that the console has
    /// probably moved; consumed by the next discovery pass, which owns the
    /// only scan path so the two can't scan on top of each other.
    retarget_requested: bool,
    /// Fruitless discovery passes still owed the floor cadence because the
    /// worker is actively hunting a console it was talking to moments ago.
    /// See [`RETARGET_FAST_PASSES`]. Zero = ordinary discovery, which ramps.
    retarget_fast_passes: u32,
    /// Whether the fast search above has already been spent without finding
    /// anything. Latches the budget off until the worker actually reaches a
    /// console again — a console that is simply switched off keeps failing its
    /// heartbeat, and each failure run asks for another retarget, which would
    /// otherwise re-arm the budget forever and pin the cadence at the floor.
    retarget_search_spent: bool,
}

/// Encode a slot list (values 1..=8) as a u8 bitmask: bit n = slot (n+1) present.
fn slots_to_bitmask(slots: &[u8]) -> u8 {
    slots
        .iter()
        .fold(0u8, |acc, &s| acc | (1u8 << s.saturating_sub(1)))
}

/// `(ip, latency_ms, name, model, same_subnet, is_loopback)` — one response per source IP.
type RawEntry = (String, f32, String, String, bool, bool);

/// Sort order: loopback > same-subnet > routed; ties broken by ascending latency.
fn entry_cmp(a: &RawEntry, b: &RawEntry) -> std::cmp::Ordering {
    match (a.5, b.5) {
        (true, false) => std::cmp::Ordering::Less,
        (false, true) => std::cmp::Ordering::Greater,
        _ => match (a.4, b.4) {
            (true, false) => std::cmp::Ordering::Less,
            (false, true) => std::cmp::Ordering::Greater,
            _ => a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal),
        },
    }
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
            scan: ScanPublisher {
                scan_targets: shared.scan_targets,
                scan_health: shared.scan_health,
                empty_scans: Arc::new(AtomicU32::new(0)),
            },
            connected_device: shared.connected_device,
            scan_generation: shared.scan_generation,
            user_disconnected: false,
            connected: false,
            backoff: Backoff::new(2000, 10000),
            scan_port: 10023,
            auto_reconnect: shared.auto_reconnect,
            last_device: shared.last_device,
            last_slot_types: shared.last_slot_types,
            target_from_auto: false,
            heartbeat_failures: 0,
            last_auto_attempt: None,
            last_discovery: None,
            discovery_interval: DISCOVERY_INTERVAL_MIN,
            last_iface_set: Vec::new(),
            retarget_requested: false,
            retarget_fast_passes: 0,
            retarget_search_spent: false,
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

            // Background discovery: keeps the device list fresh and retargets a
            // console that moved, with no GUI open and no user action. This is
            // what makes a cold start work — the DAW, the network and the mixer
            // can come up in any order and the worker converges anyway.
            self.maybe_discover();

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
                        nice_trace!(
                            "[EtherTap] UDP rebind failed — backoff {}ms",
                            self.backoff.next_delay_ms()
                        );
                        self.backoff.record_failure();
                    }
                    self.send_heartbeat();
                    // Advance by exactly one interval so the cadence stays
                    // constant regardless of how long the heartbeat took.
                    self.last_heartbeat += interval;
                    // A blocking discovery pass or slot audit can leave the
                    // advanced deadline still in the past, which would refire
                    // the heartbeat on the next 10 ms tick and burn through the
                    // backoff in a burst. Resynchronise instead of catching up.
                    if self.last_heartbeat.elapsed() >= interval {
                        self.last_heartbeat = Instant::now();
                    }
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
                nice_warn!("[EtherTap] invalid target: {ip}:{port}");
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
        // A pending retarget means the persisted address was just rejected —
        // re-dialling it now would only get the same wrong device again. Let
        // the discovery pass find the real one first.
        if self.retarget_requested {
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
        nice_log!("[EtherTap] auto_reconnect: resuming last target {ip}:{port}");
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
                // Run the scan on a dedicated thread so the network worker
                // remains responsive to sync commands during the window.
                let publisher = self.scan.clone();
                let status_tx = self.status_tx.clone();
                let scan_gen = self.scan_generation.clone();
                // Capture the current generation so the background thread can
                // detect if the editor opened a new scan (and cleared results)
                // before this thread finishes.
                let my_gen = scan_gen.load(Ordering::Acquire);
                let scan_port = self.scan_port;
                // Give the probe burst the current target as a unicast hint —
                // a console that ignores broadcast still answers direct.
                let hint = self.target;
                std::thread::Builder::new()
                    .name("ethertap-scan".into())
                    .spawn(move || {
                        NetworkWorker::scan_targets_bg(
                            publisher, status_tx, scan_gen, my_gen, scan_port, hint,
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

        let info_msg = recv_len.and_then(|len| {
            decoder::decode_udp(&buf[..len])
                .ok()
                .and_then(|(_, pkt)| match pkt {
                    OscPacket::Message(m) if m.addr == "/info" => Some(m),
                    _ => None,
                })
        });
        match info_msg {
            Some(msg) => {
                let (name, model) = extract_info_strings(&msg);

                // Identity check for auto-resumed targets: if a *different*
                // device answers at the persisted address (DHCP moved things),
                // reject it and rescan for the persisted identity instead.
                if self.target_from_auto && !name.is_empty() {
                    let expected = self.last_device.lock().clone();
                    let known = !expected.0.is_empty() || !expected.1.is_empty();
                    if known && (expected.0 != name || expected.1 != model) {
                        nice_warn!(
                            "[EtherTap] auto_reconnect: expected {expected:?}, got \
                             ({name:?}, {model:?}) — rescanning for the device"
                        );
                        self.target = None;
                        self.connected = false;
                        // This attempt did not reach our console, so it counts
                        // as a failure — otherwise the throttle stays at its
                        // floor and we re-dial the wrong device every 2 s.
                        self.backoff.record_failure();
                        // Hand the search to the discovery pass, which owns the
                        // only scan path — running one here too would put two
                        // probe bursts on the wire for the same question.
                        self.request_retarget();
                        return;
                    }
                }

                self.connected = true;
                self.heartbeat_failures = 0;
                // A console answered: whatever loss spent the last fast search
                // is over, so the next one gets its full budget again.
                self.retarget_search_spent = false;
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
                // straight failures, ask the discovery pass to find it again.
                if self.heartbeat_failures >= AUTO_RESCAN_FAILURES
                    && self.auto_reconnect.load(Ordering::Relaxed)
                {
                    self.heartbeat_failures = 0;
                    self.request_retarget();
                }
            }
        }
    }

    // ── Background discovery ──────────────────────────────────────────────

    /// Ask the next discovery pass to hunt for the persisted identity, and give
    /// that hunt a budget of floor-cadence passes.
    ///
    /// Both callers are "we were just talking to this console and lost it": a
    /// wrong device answering the persisted address, and a heartbeat that has
    /// stopped landing. Either way the device is far more likely to be present
    /// and missed than absent, so the usual back-off-on-empty-scan reflex is
    /// the wrong one for the next few passes.
    fn request_retarget(&mut self) {
        self.retarget_requested = true;
        // One fast search per loss. Re-arming on every heartbeat-failure run
        // would hold the floor cadence indefinitely against a console that is
        // simply switched off. The latch lifts as soon as any console answers.
        if !self.retarget_search_spent {
            self.retarget_fast_passes = RETARGET_FAST_PASSES;
            self.discovery_interval = DISCOVERY_INTERVAL_MIN;
        }
    }

    /// Decide whether to run a discovery pass this iteration, and run it.
    ///
    /// Discovery is the worker's own job, not the editor's: it runs with the
    /// GUI closed, and it is the only reason a cold start converges when the
    /// DAW, the network interface and the console come up in an arbitrary
    /// order. It is gated on `auto_reconnect` — with the opt-in off, EtherTap
    /// still emits no unrequested traffic.
    fn maybe_discover(&mut self) {
        if self.connected || self.user_disconnected || !self.auto_reconnect.load(Ordering::Relaxed)
        {
            return;
        }
        let due = match self.last_discovery {
            None => true,
            Some(t) => t.elapsed() >= self.discovery_interval || self.retarget_requested,
        };
        if !due {
            return;
        }
        self.discover_and_retarget();
    }

    /// One discovery pass: scan, publish the results for the editor, and adopt
    /// an address if one can be justified.
    ///
    /// Adoption rules, in order:
    /// 1. A device whose `(name, model)` matches the persisted identity — the
    ///    console we were talking to before, wherever DHCP has since put it.
    /// 2. Exactly one device on the network and no persisted identity — a
    ///    first run on a LAN with a single console. Ambiguity never adopts:
    ///    two or more unknown devices leave the choice to the user.
    ///
    /// Blocks the worker for [`SCAN_WINDOW`]. Acceptable: it only runs while
    /// disconnected, when there is nothing to sync or poll.
    fn discover_and_retarget(&mut self) {
        debug_assert!(!self.connected, "discovery must not run while connected");
        self.last_discovery = Some(Instant::now());
        self.retarget_requested = false;

        let last_known = {
            let ip = self.target_ip.lock().clone();
            let port = *self.target_port.lock();
            (!ip.is_empty() && port != 0).then(|| format!("{ip}:{port}").parse().ok())
        }
        .flatten();

        let (devices, stats) = Self::scan_collect(self.scan_port, SCAN_WINDOW, last_known);
        self.scan.record(&devices, &stats, None);

        // An interface set that differs from the last pass means the machine's
        // network just came up (or moved) — keep the fast cadence so a DAW that
        // launched before DHCP finished doesn't sit out a 30 s interval.
        let ifaces_changed = stats.addrs != self.last_iface_set;
        if ifaces_changed {
            nice_log!(
                "[EtherTap] discovery: interfaces changed → {:?}",
                stats.addrs
            );
            self.last_iface_set = stats.addrs;
        }

        let expected = self.last_device.lock().clone();
        let identity_known = !expected.0.is_empty() || !expected.1.is_empty();

        let hit = if identity_known {
            devices
                .iter()
                .find(|d| d.name == expected.0 && d.model == expected.1)
                .cloned()
        } else if devices.len() == 1 {
            devices.first().cloned()
        } else {
            None
        };

        let Some(dev) = hit else {
            // Nothing to adopt — ramp the cadence down so an empty network
            // settles to an occasional probe instead of a scan every 5 s.
            // Two states keep the fast cadence instead: a network that just
            // changed shape, and an active retarget search that still has
            // passes left in its budget.
            let searching = self.retarget_fast_passes > 0;
            self.retarget_fast_passes = self.retarget_fast_passes.saturating_sub(1);
            if searching && self.retarget_fast_passes == 0 {
                // Budget just ran out without a hit: stop treating this loss as
                // fresh, so later heartbeat failures can't re-arm it.
                self.retarget_search_spent = true;
            }
            self.discovery_interval = if ifaces_changed || searching {
                DISCOVERY_INTERVAL_MIN
            } else {
                (self.discovery_interval * 2).min(DISCOVERY_INTERVAL_MAX)
            };
            if identity_known {
                nice_trace!(
                    "[EtherTap] discovery: {expected:?} not found ({} device(s) seen)",
                    devices.len()
                );
            }
            return;
        };

        self.discovery_interval = DISCOVERY_INTERVAL_MIN;
        // The hunt is over — the next fruitless pass is an ordinary one and
        // ramps like any other, and a future loss earns a fresh fast search.
        self.retarget_fast_passes = 0;
        self.retarget_search_spent = false;

        // Already pointed at it and merely waiting on the heartbeat — don't
        // tear down a connection attempt that is still in progress.
        let same_target = self
            .target
            .is_some_and(|t| t.ip().to_string() == dev.ip && t.port() == dev.port);
        if same_target {
            return;
        }

        nice_log!(
            "[EtherTap] discovery: adopting {} at {}:{}",
            dev.display_name(),
            dev.ip,
            dev.port
        );
        *self.target_ip.lock() = dev.ip.clone();
        *self.target_port.lock() = dev.port;
        self.target_from_auto = true;
        self.connect(dev.ip, dev.port);
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
        let _ = self
            .socket
            .as_ref()
            .unwrap()
            .set_read_timeout(Some(RECV_TIMEOUT));

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
                nice_warn!("[EtherTap] audit: failed to send slot-{slot} query");
                continue;
            }
            let mut buf = [0u8; 256];
            let Some(sock) = &self.socket else {
                interrupted = true;
                break 'audit;
            };
            if let Ok((len, _)) = sock.recv_from(&mut buf)
                && let Some(type_id) = parse_fx_type(&buf[..len])
            {
                slot_types[(slot - 1) as usize] = Some(type_id);
                if osc::is_bpm_compatible(type_id, slot) {
                    compatible.push(slot);
                }
                occupied.push(slot);
            }
        }

        if interrupted {
            nice_log!(
                "[EtherTap] audit_slots: interrupted (disconnect mid-audit), discarding partial results"
            );
            return;
        }

        nice_log!("[EtherTap] FX slot audit:");
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
                    nice_log!("  Slot {slot}: {short}  ({long}){tag}");
                }
                None => nice_log!("  Slot {slot}: no response"),
            }
        }
        nice_log!("  Compatible: {:?}  Occupied: {:?}", compatible, occupied);

        // Write atomically — no allocation or lock on the audio thread.
        // slot_types stores each element individually; i32::MIN = not yet queried.
        for (i, t) in slot_types.iter().enumerate() {
            self.slot_types[i].store(t.unwrap_or(i32::MIN), Ordering::Relaxed);
        }
        // Write-through to the persisted mirror so the map survives a reload.
        // Worker-thread only — the audio thread never touches this lock.
        {
            let mut persisted = self.last_slot_types.lock();
            for (i, t) in slot_types.iter().enumerate() {
                persisted[i] = t.unwrap_or(i32::MIN);
            }
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
    /// network worker remains fully responsive during the scan window.
    fn scan_targets_bg(
        publisher: ScanPublisher,
        status_tx: Sender<NetworkStatus>,
        scan_generation: Arc<AtomicU64>,
        expected_gen: u64,
        scan_port: u16,
        unicast_hint: Option<SocketAddr>,
    ) {
        let (devices, stats) = Self::scan_collect(scan_port, SCAN_WINDOW, unicast_hint);
        // Discarded on a generation change — the editor already started a
        // newer scan and cleared the list for it.
        if publisher
            .record(&devices, &stats, Some((&scan_generation, expected_gen)))
            .is_none()
        {
            return;
        }
        let _ = status_tx.try_send(NetworkStatus::ScanDone);
    }

    /// Probe every IPv4 interface (plus loopback) on `scan_port` and collect
    /// the devices that answer within `window`. Pure collection — no shared
    /// state; used by the editor-driven background scan and the worker's own
    /// discovery pass.
    ///
    /// `unicast_hint` is probed directly alongside the broadcasts. A console on
    /// another subnet, or behind a switch that drops directed broadcast, never
    /// sees a broadcast probe but answers a unicast one — which is how a known
    /// address stays discoverable when broadcast discovery finds nothing.
    fn scan_collect(
        scan_port: u16,
        window: Duration,
        unicast_hint: Option<SocketAddr>,
    ) -> (Vec<DeviceInfo>, ScanStats) {
        use std::{collections::HashMap, net::Ipv4Addr};

        let probe = osc::heartbeat();

        // ── One socket per real IPv4 interface ────────────────────────────
        struct Iface {
            sock: UdpSocket,
            local: Ipv4Addr,
            netmask: Ipv4Addr,
            /// Every destination this socket probes, retried once per burst.
            targets: Vec<SocketAddr>,
        }

        let mut ifaces: Vec<Iface> = Vec::new();
        let mut stats = ScanStats::default();

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
            let _ = sock.set_nonblocking(true);
            let directed = v4
                .broadcast
                .unwrap_or_else(|| Ipv4Addr::from(u32::from(v4.ip) | !u32::from(v4.netmask)));
            // Three destinations per interface, because any one of them can be
            // the only one that works: the directed subnet broadcast is the
            // normal path; the limited broadcast survives a wrong netmask and
            // switches that filter directed broadcast; the hint reaches a
            // console that broadcast can't touch at all.
            let mut targets = vec![
                SocketAddr::from((directed, scan_port)),
                SocketAddr::from((Ipv4Addr::BROADCAST, scan_port)),
            ];
            if let Some(hint) = unicast_hint.filter(|h| !h.ip().is_loopback()) {
                targets.push(hint);
            }
            stats.addrs.push(v4.ip.to_string());
            ifaces.push(Iface {
                sock,
                local: v4.ip,
                netmask: v4.netmask,
                targets,
            });
        }

        // Loopback socket so a local mock mixer is always discoverable.
        if let Ok(sock) = UdpSocket::bind("127.0.0.1:0") {
            let _ = sock.set_nonblocking(true);
            let mut targets = vec![SocketAddr::from((Ipv4Addr::LOCALHOST, scan_port))];
            if let Some(hint) = unicast_hint.filter(|h| h.ip().is_loopback())
                && !targets.contains(&hint)
            {
                targets.push(hint);
            }
            ifaces.push(Iface {
                sock,
                local: Ipv4Addr::LOCALHOST,
                netmask: Ipv4Addr::new(255, 0, 0, 0),
                targets,
            });
        }

        // ── Collect responses — raw entry per (socket, device) pair ───────
        // ip_key → best raw entry (loopback wins, then same-subnet; ties broken by latency)
        let mut by_ip: HashMap<String, RawEntry> = HashMap::new();
        let mut buf = [0u8; 512];
        let probe_sent_at = Instant::now();
        let mut bursts_sent = 0usize;

        while probe_sent_at.elapsed() < window {
            // Retransmit on a schedule while still listening. A single probe
            // that gets dropped used to cost the whole scan.
            if bursts_sent < SCAN_PROBE_BURST
                && probe_sent_at.elapsed() >= SCAN_PROBE_SPACING * bursts_sent as u32
            {
                for iface in &ifaces {
                    for target in &iface.targets {
                        if iface.sock.send_to(&probe, target).is_ok() {
                            stats.probes += 1;
                        }
                    }
                }
                bursts_sent += 1;
            }

            let mut any_recv = false;
            for iface in &ifaces {
                while let Ok((len, src)) = iface.sock.recv_from(&mut buf) {
                    any_recv = true;
                    let Ok((_, OscPacket::Message(info_msg))) = decoder::decode_udp(&buf[..len])
                    else {
                        continue;
                    };
                    stats.replies += 1;
                    let src_v4 = match src.ip() {
                        std::net::IpAddr::V4(v) => v,
                        _ => continue,
                    };

                    let mask = u32::from(iface.netmask);
                    let same_subnet = (u32::from(iface.local) & mask) == (u32::from(src_v4) & mask);
                    let is_loopback = src_v4.is_loopback();
                    let latency_ms = probe_sent_at.elapsed().as_micros() as f32 / 1000.0;
                    let ip_key = src_v4.to_string();
                    let (name, model) = extract_info_strings(&info_msg);

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
                        Some((_, prev_latency, _, _, prev_same, prev_loopback)) => {
                            // Prefer loopback, then same-subnet; within that, lower latency.
                            let better = (!*prev_loopback && is_loopback)
                                || (*prev_loopback == is_loopback
                                    && ((!*prev_same && same_subnet)
                                        || (*prev_same == same_subnet
                                            && latency_ms < *prev_latency)));
                            if better {
                                by_ip.insert(ip_key, entry);
                            }
                        }
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
        all.sort_by(entry_cmp);

        let mut result: Vec<DeviceInfo> = Vec::new();

        for (ip, latency_ms, name, model, same_subnet, _is_loopback) in all {
            let has_id = !name.is_empty() || !model.is_empty();

            // Try to find an existing entry with the same identity.
            if has_id
                && let Some(existing) = result
                    .iter_mut()
                    .find(|d| d.name == name && d.model == model)
            {
                // Same physical device reachable via another IP — append.
                if !existing.all_addrs.iter().any(|(a, _, _)| *a == ip) {
                    existing.all_addrs.push((ip, Some(latency_ms), same_subnet));
                }
                continue;
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

        (result, stats)
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
                nice_warn!("[EtherTap] failed to bind UDP socket: {e}");
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
            nice_warn!(
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

/// Extract `(name, model)` from an already-decoded X32 `/info` message.
///
/// X32 format: `/info ,ssss  version  name  model  firmware`
/// Returns empty strings when the expected string args are absent.
fn extract_info_strings(msg: &OscMessage) -> (String, String) {
    let strings: Vec<&str> = msg
        .args
        .iter()
        .filter_map(|a| {
            if let OscType::String(s) = a {
                Some(s.as_str())
            } else {
                None
            }
        })
        .collect();
    match strings.len() {
        // 0 args: no data; 1 arg: ambiguous (could be version) — skip.
        0 | 1 => (String::new(), String::new()),
        // X32 layout: version, name, model[, firmware] — skip version (index 0).
        _ => {
            let name = strings[1].to_owned();
            // With 2 args: (version, name) → return (name, "").
            let model = strings.get(2).map(|s| s.to_string()).unwrap_or_default();
            (name, model)
        }
    }
}

/// Parse the string arguments from an X32 `/info` response (test helper).
///
/// X32 format: `/info ,ssss  version  name  model  firmware`
/// Returns `(name, model)` — empty strings if the args aren't present.
#[cfg(test)]
fn parse_info_strings(data: &[u8]) -> (String, String) {
    let Ok((_, OscPacket::Message(msg))) = decoder::decode_udp(data) else {
        return (String::new(), String::new());
    };
    extract_info_strings(&msg)
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
    use rosc::{OscMessage, encoder};

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

    #[test]
    fn scan_sort_prefers_same_subnet() {
        // Tier order: loopback > same-subnet > other, ties broken by ascending latency.
        let mut entries: Vec<super::RawEntry> = vec![
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
        entries.sort_by(super::entry_cmp);
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
        two.sort_by(super::entry_cmp);
        assert!(
            two[0].5,
            "loopback remains first in a 2-element already-sorted list"
        );
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
                scan_health: Arc::new(AtomicU8::new(ScanHealth::Unknown as u8)),
                last_slot_types: Arc::new(Mutex::new([i32::MIN; 8])),
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

    /// A discovery pass that finds nothing must leave the target alone and back
    /// its cadence off, rather than retargeting to whatever last answered.
    #[test]
    fn discovery_without_devices_does_not_retarget() {
        let mut worker = make_test_worker();
        // Probe a port nothing listens on so the scan is guaranteed empty —
        // without this the developer's own console could answer and the test
        // would depend on what is plugged into the LAN.
        worker.set_scan_port(unused_udp_port());

        // First pass also fingerprints the interfaces, which deliberately holds
        // the fast cadence; the ramp starts once the network looks settled.
        worker.discover_and_retarget();
        assert!(worker.last_discovery.is_some(), "the pass must be recorded");
        let before = worker.discovery_interval;

        worker.discover_and_retarget();

        assert!(
            worker.target.is_none(),
            "an empty scan must not retarget the worker"
        );
        assert!(
            worker.discovery_interval > before,
            "a fruitless scan on an unchanged network must ramp the interval down"
        );
    }

    /// A retarget search is not an ordinary scan: the console answered seconds
    /// ago, so an empty pass means a lost probe far more often than an absent
    /// device. The cadence must hold the floor for the whole budget, then ramp
    /// like anything else so a console that really is powered down doesn't buy
    /// a permanent 5 s scan loop.
    #[test]
    fn retarget_search_holds_the_fast_cadence_then_ramps() {
        let mut worker = make_test_worker();
        worker.set_scan_port(unused_udp_port());

        // Warm-up pass: the first one fingerprints the interfaces, which holds
        // the floor on its own and would mask the behaviour under test.
        worker.discover_and_retarget();

        worker.request_retarget();
        for pass in 1..=RETARGET_FAST_PASSES {
            worker.discover_and_retarget();
            assert_eq!(
                worker.discovery_interval, DISCOVERY_INTERVAL_MIN,
                "pass {pass} of {RETARGET_FAST_PASSES} must still search at the floor cadence"
            );
        }

        worker.discover_and_retarget();
        assert!(
            worker.discovery_interval > DISCOVERY_INTERVAL_MIN,
            "with the search budget spent, the ordinary ramp must resume"
        );

        // A console that is switched off keeps failing its heartbeat, and every
        // failure run asks for another retarget. Those must not re-arm the
        // budget — otherwise the cadence never leaves the floor.
        let ramped = worker.discovery_interval;
        worker.request_retarget();
        assert_eq!(
            worker.retarget_fast_passes, 0,
            "a spent search must not re-arm on a repeat request"
        );
        assert_eq!(
            worker.discovery_interval, ramped,
            "a repeat request must not snap the ramped cadence back to the floor"
        );
        assert!(
            worker.retarget_requested,
            "the pass itself is still requested — only the fast budget is withheld"
        );
    }

    /// The latch is per-loss, not permanent: finding a console again must clear
    /// it, so the *next* time that console goes missing still earns a full fast
    /// search. Without this the budget is a once-per-session affair.
    #[test]
    fn finding_a_console_clears_the_spent_latch() {
        use mock_suite::{MockMixer, default_slots};

        let mock = MockMixer::start_with_identity(0, default_slots(), "Test Desk", "X32")
            .expect("bind mock mixer");
        let mut worker = make_test_worker();
        worker.set_scan_port(mock.port());
        *worker.last_device.lock() = ("Test Desk".to_string(), "X32".to_string());
        // State left behind by an earlier search that came up empty.
        worker.retarget_search_spent = true;
        worker.retarget_fast_passes = 0;

        worker.discover_and_retarget();

        assert!(
            worker.target.is_some(),
            "precondition: the mock must be found and adopted"
        );
        assert!(
            !worker.retarget_search_spent,
            "adopting a device must clear the latch so a later loss searches fast again"
        );

        worker.request_retarget();
        assert_eq!(worker.retarget_fast_passes, RETARGET_FAST_PASSES);
    }

    /// The heartbeat may have been failing for a while by the time the worker
    /// gives up on the address, leaving the cadence ramped. Arming the search
    /// has to undo that — otherwise the first pass of the hunt waits out a 30 s
    /// interval before it even runs.
    #[test]
    fn request_retarget_resets_a_ramped_cadence() {
        let mut worker = make_test_worker();
        worker.discovery_interval = DISCOVERY_INTERVAL_MAX;

        worker.request_retarget();

        assert_eq!(worker.discovery_interval, DISCOVERY_INTERVAL_MIN);
        assert_eq!(worker.retarget_fast_passes, RETARGET_FAST_PASSES);
        assert!(worker.retarget_requested);
    }

    /// The interval doubles per fruitless pass and stops at the ceiling.
    #[test]
    fn discovery_interval_ramps_to_cap() {
        let mut worker = make_test_worker();
        worker.set_scan_port(unused_udp_port());
        // Far more passes than needed to reach the cap from the 5 s floor.
        for _ in 0..8 {
            worker.discovery_interval = (worker.discovery_interval * 2).min(DISCOVERY_INTERVAL_MAX);
        }
        assert_eq!(worker.discovery_interval, DISCOVERY_INTERVAL_MAX);
    }

    /// Discovery is opt-in: with `auto_reconnect` off, the worker emits nothing.
    #[test]
    fn discovery_is_gated_on_auto_reconnect() {
        let mut worker = make_test_worker();
        worker.set_scan_port(unused_udp_port());
        worker.auto_reconnect.store(false, Ordering::Relaxed);
        worker.maybe_discover();
        assert!(
            worker.last_discovery.is_none(),
            "auto_reconnect OFF must not run a discovery pass"
        );
    }

    /// Every scan retransmits its probe — one lost datagram must not cost the
    /// whole window. The loopback socket is always present, so the burst count
    /// holds regardless of what interfaces the test machine has.
    #[test]
    fn scan_probes_are_retransmitted() {
        let (_devices, stats) = NetworkWorker::scan_collect(unused_udp_port(), SCAN_WINDOW, None);
        assert!(
            stats.probes >= SCAN_PROBE_BURST,
            "expected at least {SCAN_PROBE_BURST} probes, sent {}",
            stats.probes
        );
        assert_eq!(stats.replies, 0, "nothing listens on an unused port");
    }

    /// Bind an ephemeral UDP port, then release it — near-certainly unused.
    fn unused_udp_port() -> u16 {
        UdpSocket::bind("127.0.0.1:0")
            .expect("bind ephemeral")
            .local_addr()
            .expect("local_addr")
            .port()
    }

    fn test_publisher() -> ScanPublisher {
        ScanPublisher {
            scan_targets: Arc::new(Mutex::new(Vec::new())),
            scan_health: Arc::new(AtomicU8::new(ScanHealth::Unknown as u8)),
            empty_scans: Arc::new(AtomicU32::new(0)),
        }
    }

    fn device(name: &str, ip: &str) -> DeviceInfo {
        DeviceInfo {
            ip: ip.into(),
            port: 10023,
            name: name.into(),
            model: "X32".into(),
            latency_ms: None,
            all_addrs: vec![],
        }
    }

    /// A reply means the path works, whatever else is wrong.
    #[test]
    fn scan_health_ok_on_reply() {
        let p = test_publisher();
        let stats = ScanStats {
            addrs: vec!["192.168.1.5".into()],
            probes: 3,
            replies: 1,
        };
        assert_eq!(
            p.record(&[device("A", "192.168.1.9")], &stats, None),
            Some(ScanHealth::Ok)
        );
    }

    /// One quiet scan is not evidence — the warning state needs a run of them.
    #[test]
    fn scan_health_escalates_only_after_repeated_silence() {
        let p = test_publisher();
        let stats = ScanStats {
            addrs: vec!["192.168.1.5".into()],
            probes: 3,
            replies: 0,
        };
        for _ in 1..SCAN_HEALTH_FAILURES {
            assert_eq!(
                p.record(&[], &stats, None),
                Some(ScanHealth::Unknown),
                "must hold the previous verdict until the run is long enough"
            );
        }
        assert_eq!(p.record(&[], &stats, None), Some(ScanHealth::NoReplies));

        // A single reply clears it immediately.
        let ok = ScanStats {
            replies: 1,
            ..stats
        };
        assert_eq!(p.record(&[], &ok, None), Some(ScanHealth::Ok));
    }

    /// No interfaces is a different problem from no answers, and says so.
    #[test]
    fn scan_health_reports_missing_interfaces() {
        let p = test_publisher();
        let stats = ScanStats::default();
        assert_eq!(p.record(&[], &stats, None), Some(ScanHealth::NoInterfaces));
    }

    /// A result from a superseded scan must not repopulate the cleared list.
    #[test]
    fn scan_result_from_stale_generation_is_discarded() {
        let p = test_publisher();
        let generation = AtomicU64::new(2);
        let stats = ScanStats {
            addrs: vec!["192.168.1.5".into()],
            probes: 3,
            replies: 1,
        };
        assert_eq!(
            p.record(
                &[device("A", "192.168.1.9")],
                &stats,
                Some((&generation, 1))
            ),
            None
        );
        assert!(p.scan_targets.lock().is_empty());
    }

    /// The same console seen again updates its entry instead of duplicating it,
    /// even when DHCP has since moved it to another address.
    #[test]
    fn merge_devices_dedupes_by_identity() {
        let mut list = vec![device("Desk", "192.168.1.9")];
        merge_devices(&mut list, [device("Desk", "192.168.1.42")]);
        assert_eq!(list.len(), 1, "identity match must replace, not append");
        assert_eq!(list[0].ip, "192.168.1.42", "the newer address wins");

        merge_devices(&mut list, [device("Other", "192.168.1.7")]);
        assert_eq!(list.len(), 2, "a different console is a new entry");
    }

    /// After the persisted address answered with the wrong console, the worker
    /// must not immediately re-dial it — that just gets the same wrong device
    /// again, and starves the discovery pass that could find the right one.
    #[test]
    fn pending_retarget_blocks_redialling_the_rejected_address() {
        let mut worker = make_test_worker();
        worker.auto_reconnect.store(true, Ordering::Relaxed);
        worker.retarget_requested = true;

        worker.maybe_auto_connect();

        assert!(
            worker.last_auto_attempt.is_none(),
            "a pending retarget must suppress the auto-connect attempt"
        );
        assert!(worker.target.is_none());
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
        use rosc::{OscBundle, OscPacket, OscTime, encoder};
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
        use rosc::{OscMessage, OscPacket, OscType, encoder};
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

    /// When `scan_targets_bg` runs but the scan generation has advanced (editor
    /// cleared and restarted a new scan before this thread finished), the stale
    /// results must be discarded and no `ScanDone` sentinel must be sent to the
    /// audio thread. Without this guard, the audio thread would stamp
    /// `scan_completed_ts` for a scan that was already superseded, confusing the
    /// editor's "scan finished" display.
    #[test]
    fn scan_targets_bg_discards_stale_generation() {
        let publisher = test_publisher();
        let scan_targets = publisher.scan_targets.clone();
        let (status_tx, status_rx) = crossbeam_channel::bounded(8);
        let scan_generation = Arc::new(AtomicU64::new(2)); // advanced past expected_gen

        // Call with expected_gen=1 but current generation is 2 → stale.
        NetworkWorker::scan_targets_bg(
            publisher,
            status_tx,
            scan_generation,
            1, // expected_gen — outdated
            unused_udp_port(),
            None,
        );

        assert!(
            scan_targets.lock().is_empty(),
            "stale scan results must not be merged into scan_targets"
        );
        assert!(
            status_rx.try_recv().is_err(),
            "stale scan must not send ScanDone to the audio thread"
        );
    }
}
