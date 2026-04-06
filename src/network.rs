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
const TELEMETRY_INTERVAL: Duration = Duration::from_secs(3);
/// Per-recv timeout used for heartbeat / telemetry reads.
const RECV_TIMEOUT: Duration = Duration::from_millis(250);
/// Mute ↔ update dwell for the Hard Reset sequence (spec: 50–100 ms).
const HARD_RESET_DWELL: Duration = Duration::from_millis(75);
/// Worker loop sleep when idle.
const LOOP_SLEEP: Duration = Duration::from_millis(10);

// ─── Command / Status types ──────────────────────────────────────────────────

/// Commands from the audio thread (or editor) to the network worker.
#[derive(Debug)]
pub enum NetworkCommand {
    /// Bind a new UDP socket and connect to the given target.
    UpdateTarget { ip: String, port: u16 },
    /// Dispatch the BPM-derived delay time to the given FX slot immediately.
    SyncNow { slot: u8, bpm: f64 },
    /// Mute → update BPM → unmute to realign hardware phase.
    HardReset { slot: u8, bpm: f64 },
    /// Query all 8 FX slots and report which host a Stereo Delay (type 10).
    AuditSlots,
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
    /// FX slot indices (1-based) confirmed to host a Stereo Delay after audit.
    CompatibleSlots(Vec<u8>),
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

            // Periodic heartbeat.
            if self.last_heartbeat.elapsed() >= HEARTBEAT_INTERVAL {
                self.send_heartbeat();
                self.last_heartbeat = Instant::now();
            }

            // Periodic hardware telemetry poll.
            if self.telemetry_timer.elapsed() >= TELEMETRY_INTERVAL {
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
                        self.target = Some(addr);
                        self.rebind();
                    }
                    Err(_) => eprintln!("[EtherTap] invalid target: {ip}:{port}"),
                }
            }

            NetworkCommand::SyncNow { slot, bpm } => {
                let value = osc::bpm_to_float(bpm);
                self.send(&osc::set_fx_delay(slot, value));
                self.pulse_tx();
            }

            NetworkCommand::HardReset { slot, bpm } => {
                // 1. Mute FX return.
                self.send(&osc::set_fxrtn_mute(slot, true));
                self.pulse_tx();
                std::thread::sleep(HARD_RESET_DWELL);

                // 2. Update delay time.
                let value = osc::bpm_to_float(bpm);
                self.send(&osc::set_fx_delay(slot, value));
                std::thread::sleep(HARD_RESET_DWELL);

                // 3. Unmute.
                self.send(&osc::set_fxrtn_mute(slot, false));
                self.pulse_tx();
            }

            NetworkCommand::AuditSlots => self.audit_slots(),
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

        // Short receive window for the response.
        let _ = sock.set_read_timeout(Some(RECV_TIMEOUT));
        let mut buf = [0u8; 256];
        if let Ok((len, _)) = sock.recv_from(&mut buf) {
            if let Some(value) = parse_fx_delay_response(&buf[..len]) {
                self.hardware_float_out.store(value.to_bits(), Ordering::Relaxed);
                let _ = self.status_tx.try_send(NetworkStatus::DelayReadback(value));
                self.pulse_rx();
            }
        }
        // Restore normal timeout.
        let _ = sock.set_read_timeout(Some(HEARTBEAT_INTERVAL));
    }

    // ── Heartbeat ────────────────────────────────────────────────────────

    fn send_heartbeat(&self) {
        self.send(&osc::heartbeat());

        let Some(sock) = &self.socket else { return };
        let mut buf = [0u8; 512];
        // Short window — we don't want to stall the heartbeat check.
        let _ = sock.set_read_timeout(Some(RECV_TIMEOUT));
        match sock.recv_from(&mut buf) {
            Ok((len, _)) => {
                if decoder::decode_udp(&buf[..len]).is_ok() {
                    let _ = self.status_tx.try_send(NetworkStatus::Connected);
                    self.pulse_rx();
                }
            }
            Err(_) => {
                let _ = self.status_tx.try_send(NetworkStatus::Disconnected);
            }
        }
        let _ = sock.set_read_timeout(Some(HEARTBEAT_INTERVAL));
    }

    // ── Slot audit ───────────────────────────────────────────────────────

    fn audit_slots(&self) {
        let (Some(sock), Some(target)) = (&self.socket, &self.target) else {
            return;
        };
        let _ = sock.set_read_timeout(Some(RECV_TIMEOUT));

        let mut compatible = Vec::new();
        for slot in 1u8..=8 {
            if sock.send_to(&osc::query_fx_type(slot), target).is_err() {
                continue;
            }
            let mut buf = [0u8; 256];
            if let Ok((len, _)) = sock.recv_from(&mut buf) {
                if let Some(type_id) = parse_fx_type(&buf[..len]) {
                    if type_id == osc::DLY_TYPE_ID {
                        compatible.push(slot);
                    }
                }
            }
        }

        let _ = sock.set_read_timeout(Some(HEARTBEAT_INTERVAL));
        let _ = self.status_tx.try_send(NetworkStatus::CompatibleSlots(compatible));
    }

    // ── UDP helpers ───────────────────────────────────────────────────────

    fn rebind(&mut self) {
        match UdpSocket::bind("0.0.0.0:0") {
            Ok(sock) => {
                let _ = sock.set_read_timeout(Some(HEARTBEAT_INTERVAL));
                self.socket = Some(sock);
                self.send_heartbeat();
                self.last_heartbeat = Instant::now();
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
