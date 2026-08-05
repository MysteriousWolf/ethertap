/// Background MIDI clock + bridge worker.
///
/// # BPM accuracy — why we use the burst approach
///
/// MIDI receivers determine tempo by **counting how many 0xF8 bytes arrive per
/// beat window**, not by measuring the inter-pulse interval.  Sending exactly
/// 24 pulses per quarter note gives the correct BPM regardless of when within
/// the beat the burst arrives.
///
/// Using `std::thread::sleep` to space pulses *appears* cleaner but breaks BPM
/// accuracy: the OS scheduler on macOS/Linux overshoots sleeps by 0.5–2 ms.
/// At 120 BPM (24 PPQ, 20.8 ms interval) a 1 ms overshoot makes the receiver
/// report ~114 BPM — 5 BPM low.  More importantly, if the worker sleeps between
/// every tick it consumes the channel slower than the audio thread fills it;
/// the bounded channel saturates within minutes and `try_send` starts silently
/// dropping ticks, killing the clock entirely.
///
/// # Phase sync — resync gap on BPM change
///
/// When the DAW BPM changes by more than 0.5 BPM the worker inserts a silence
/// gap (no 0xF8 bytes) sized to 1.5 beats, floored at 1 500 ms and capped at
/// 3 000 ms.  Receivers detect the missing pulses, reset their tempo-averaging
/// filter, and snap to the new BPM immediately on the first burst after the
/// gap.  Resumption is held until the next beat-boundary tick so the receiver's
/// pulse counter is aligned with the DAW click track.
///
/// When transport starts from stopped (`TransportStart`) there is no silence
/// gap — we simply hold off until the next beat boundary so the first 0xF8
/// always lands on a quarter-note edge that the DAW metronome is also clicking.
///
/// No MIDI Start / Stop / Continue messages are sent — those travel back into
/// the DAW through the virtual port and would inadvertently stop/start playback.
///
/// # Bridge / passthrough
///
/// When a physical MIDI output device is selected the worker also opens the
/// matching MIDI input and forwards every non-clock byte through the virtual
/// port, making it a transparent proxy.
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU32, Ordering},
};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender};
use nice_plug::{nice_error, nice_log, nice_trace, nice_warn};

// ─── Constants ────────────────────────────────────────────────────────────────

/// Minimum silence inserted after `BpmChanged`.  1 500 ms ≈ 3× the former
/// 500 ms — gives sluggish receivers (some Behringer units need >800 ms) a
/// clean window to detect the gap and reset phase.
const MIN_RESYNC_GAP_MS: u64 = 1_500;

/// Maximum silence cap.  At very slow BPM (≤13) the 1.5-beat formula would
/// exceed 4 500 ms.  Receivers that implement a "no clock = stopped" timeout
/// (typically 2–5 s) would misinterpret such a long gap as a transport stop
/// and emit a spurious MIDI Start on resumption.  3 000 ms is safely below
/// common timeouts while still being long enough for all known receivers.
const MAX_RESYNC_GAP_MS: u64 = 3_000;

/// Gap duration expressed in beats.  Must be > 1.0 to guarantee at least one
/// *full* silent beat regardless of where within the current beat the gap
/// starts.  1.5 beats is the smallest value that satisfies this and is long
/// enough for receivers that count multiple missing pulses before resetting.
const BEATS_IN_GAP: f64 = 1.5;

const CLOCK_BYTE: &[u8] = &[0xF8];

/// Log one debug line every N ticks (4 beats @ 24-PPQ = 96 ticks).
const DEBUG_LOG_INTERVAL_TICKS: u64 = 96;

/// Rolling window size for timing statistics.  256 pulses ≈ 10.7 beats @ 120 BPM.
/// Gives a meaningful p99 (≥100 samples needed) within ~5 s of starting playback.
const STAT_WINDOW: usize = 256;

// ─── Timing stats ─────────────────────────────────────────────────────────────

/// Jitter statistics computed from a rolling 256-pulse window.
/// All `_us` fields are in microseconds (absolute deviation from mean interval).
///
/// Interpretation:
/// - `p50_us` — half of all pulses deviate by less than this  (median jitter)
/// - `p95_us` — 5 % of pulses deviate by more than this
/// - `p99_us` — 1 % of pulses deviate by more than this  (worst 1 %)
/// - `max_us` — single worst pulse in the current window
#[derive(Default, Clone, Copy, Debug)]
pub struct ClockStats {
    pub interval_us: u32, // mean inter-pulse interval
    pub p50_us: u32,      // 50th-percentile jitter (median)
    pub p95_us: u32,      // 95th-percentile jitter
    pub p99_us: u32,      // 99th-percentile jitter
    pub max_us: u32,      // peak jitter in current window
    pub sample_n: u32,    // samples currently in window (≤ 256)
}

/// Lock-free version of `ClockStats` for sharing between the RT-priority MIDI
/// worker (writer) and the editor GUI thread (reader).  Replacing
/// `Mutex<ClockStats>` eliminates the mutex acquisition on the RT thread.
#[derive(Default)]
pub struct AtomicClockStats {
    pub interval_us: AtomicU32,
    pub p50_us: AtomicU32,
    pub p95_us: AtomicU32,
    pub p99_us: AtomicU32,
    pub max_us: AtomicU32,
    pub sample_n: AtomicU32,
}

impl AtomicClockStats {
    pub fn store(&self, s: &ClockStats) {
        self.interval_us.store(s.interval_us, Ordering::Relaxed);
        self.p50_us.store(s.p50_us, Ordering::Relaxed);
        self.p95_us.store(s.p95_us, Ordering::Relaxed);
        self.p99_us.store(s.p99_us, Ordering::Relaxed);
        self.max_us.store(s.max_us, Ordering::Relaxed);
        self.sample_n.store(s.sample_n, Ordering::Relaxed);
    }

    pub fn load(&self) -> ClockStats {
        ClockStats {
            interval_us: self.interval_us.load(Ordering::Relaxed),
            p50_us: self.p50_us.load(Ordering::Relaxed),
            p95_us: self.p95_us.load(Ordering::Relaxed),
            p99_us: self.p99_us.load(Ordering::Relaxed),
            max_us: self.max_us.load(Ordering::Relaxed),
            sample_n: self.sample_n.load(Ordering::Relaxed),
        }
    }
}

// ─── Clock message ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy)]
pub enum ClockMsg {
    /// A single 0xF8 clock pulse.
    /// `on_beat` is true when this tick falls exactly on a quarter-note boundary
    /// (tick_index % ppq == 0); used to phase-align resumption after a gap.
    Tick { on_beat: bool },

    /// BPM changed significantly (> 0.5 BPM).
    /// Worker inserts a 1 500 ms silence, then waits for the next beat-boundary
    /// tick before resuming, giving receivers a clean phase-locked restart.
    BpmChanged { new_bpm: f64 },

    /// Transport moved from stopped → playing.
    /// No silence gap is inserted; the worker simply holds off until the next
    /// beat-boundary tick so the first 0xF8 is phase-aligned with the DAW click.
    TransportStart,

    /// Transport stopped.  Gates all pending ticks so that 0xF8 pulses queued
    /// in the channel before the stop do not reach the output after transport
    /// halts.  The next `TransportStart` re-arms phase alignment.
    Stop,
}

// ─── Worker struct ────────────────────────────────────────────────────────────

pub struct MidiClockWorker {
    enabled: Arc<AtomicBool>,
    /// When true, auto-pick + connect the first available physical MIDI
    /// device whenever a port scan finds none currently selected. Mirrors
    /// the mixer's `connect_to_last` reconnect posture. Default OFF.
    auto_connect: Arc<AtomicBool>,
    clock_rx: Receiver<ClockMsg>,
    device_change_rx: Receiver<Option<String>>,
    device_watch_rx: Receiver<Vec<String>>,
    initial_device: Option<String>,
    bridge_connected: Arc<AtomicBool>,
    bridge_connecting: Arc<AtomicBool>,
    /// Shared timing statistics — written by this worker, read by the editor.
    pub clock_stats: Arc<AtomicClockStats>,
    /// Pulses per quarter note — used to gate stats updates at beat boundaries.
    pub midi_ppq: u8,
    /// GUI-visible device selection — write-through target for auto-connect
    /// picks, mirroring the network worker's `last_device` pattern (see
    /// CLAUDE.md). Read by the editor's Select button / device picker.
    midi_out_device: Arc<parking_lot::Mutex<Option<String>>>,
}

impl MidiClockWorker {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        enabled: Arc<AtomicBool>,
        auto_connect: Arc<AtomicBool>,
        clock_rx: Receiver<ClockMsg>,
        device_change_rx: Receiver<Option<String>>,
        device_watch_rx: Receiver<Vec<String>>,
        initial_device: Option<String>,
        bridge_connected: Arc<AtomicBool>,
        bridge_connecting: Arc<AtomicBool>,
        clock_stats: Arc<AtomicClockStats>,
        midi_ppq: u8,
        midi_out_device: Arc<parking_lot::Mutex<Option<String>>>,
    ) -> Self {
        Self {
            enabled,
            auto_connect,
            clock_rx,
            device_change_rx,
            device_watch_rx,
            initial_device,
            bridge_connected,
            bridge_connecting,
            clock_stats,
            midi_ppq,
            midi_out_device,
        }
    }

    pub fn run(self) {
        use midir::MidiOutput;
        // A MidiOutput handle is only needed for the macOS/Linux "publish our
        // own virtual port" feature below. Its absence (e.g. no ALSA seq
        // device) must not prevent the platform-independent phys_out /
        // loopback bridge from starting.
        let output = match MidiOutput::new("EtherTap") {
            Ok(o) => Some(o),
            Err(e) => {
                nice_warn!("[EtherTap] MIDI clock: virtual port host init failed: {e}");
                None
            }
        };

        run_worker(self, output);
    }
}

// ─── Physical output: hardware midir port or in-process loopback ─────────────

/// A connected `phys_out` device — either a real midir hardware port or a
/// registered [`midi_loopback`] port. Loopback ports are interchangeable with
/// hardware ports from the worker's point of view (see
/// `docs/spec/cross-platform-midi-clock.md`, Approach C).
enum PhysOutput {
    Hardware(midir::MidiOutputConnection),
    Loopback(Sender<Vec<u8>>),
}

impl PhysOutput {
    /// Send a MIDI message. Mirrors `midir::MidiOutputConnection::send`'s
    /// error-as-disconnect semantics: a loopback send failure (port full or
    /// its receiver dropped) is treated the same as a hardware send failure.
    fn send(&mut self, message: &[u8]) -> Result<(), ()> {
        match self {
            PhysOutput::Hardware(conn) => conn.send(message).map_err(|_| ()),
            PhysOutput::Loopback(tx) => tx.try_send(message.to_vec()).map_err(|_| ()),
        }
    }
}

// ─── macOS real-time thread priority ─────────────────────────────────────────
//
// Calls thread_policy_set(THREAD_TIME_CONSTRAINT_POLICY) so the OS scheduler
// treats the MIDI clock worker as a soft real-time thread, preventing the
// "two pulses bunched together" stutters that happen under normal scheduling
// when a context switch lands between consecutive 0xF8 sends.
//
// Period 8 ms / computation 0.5 ms / constraint 4 ms / preemptible true.

#[cfg(target_os = "macos")]
fn set_realtime_priority() {
    #[repr(C)]
    struct MachTimebaseInfo {
        numer: u32,
        denom: u32,
    }

    #[repr(C)]
    struct ThreadTimeConstraintPolicy {
        period: u32,
        computation: u32,
        constraint: u32,
        preemptible: i32, // boolean_t
    }

    const THREAD_TIME_CONSTRAINT_POLICY: u32 = 2;
    const THREAD_TIME_CONSTRAINT_POLICY_COUNT: u32 = 4;

    unsafe extern "C" {
        fn mach_timebase_info(info: *mut MachTimebaseInfo) -> i32;
        fn mach_thread_self() -> u32;
        fn thread_policy_set(thread: u32, flavor: u32, policy_info: *const u32, count: u32) -> i32;
    }

    unsafe {
        let mut tb = MachTimebaseInfo { numer: 1, denom: 1 };
        mach_timebase_info(&mut tb);

        let ratio = tb.numer as f64 / tb.denom as f64;
        let policy = ThreadTimeConstraintPolicy {
            period: (8_000_000.0 / ratio) as u32,     //  8 ms
            computation: (500_000.0 / ratio) as u32,  //  0.5 ms
            constraint: (4_000_000.0 / ratio) as u32, //  4 ms
            preemptible: 1,
        };

        let ret = thread_policy_set(
            mach_thread_self(),
            THREAD_TIME_CONSTRAINT_POLICY,
            &policy as *const _ as *const u32,
            THREAD_TIME_CONSTRAINT_POLICY_COUNT,
        );
        if ret != 0 {
            nice_warn!("[EtherTap] MIDI RT thread priority failed (kern={ret})");
        }
    }
}

#[cfg(not(target_os = "macos"))]
fn set_realtime_priority() {}

// ─── Stat helpers ─────────────────────────────────────────────────────────────

/// Compute timing stats from `n` interval samples (µs) in `win`.
/// Only the first `n` entries are read — safe before the ring wraps.
fn compute_stats(win: &[u32; STAT_WINDOW], n: usize) -> ClockStats {
    if n < 2 {
        return ClockStats::default();
    }

    let slice = &win[..n];

    // Mean interval.
    let sum: u64 = slice.iter().map(|&x| x as u64).sum();
    let mean = (sum / n as u64) as u32;

    // Absolute deviations from mean, sorted ascending.
    let mut devs = [0u32; STAT_WINDOW];
    for (i, &x) in slice.iter().enumerate() {
        devs[i] = x.abs_diff(mean);
    }
    let dev_slice = &mut devs[..n];
    dev_slice.sort_unstable();

    let pct = |p: usize| -> u32 {
        // Index of the p-th percentile (0-based, round up).
        let idx = (n * p).div_ceil(100).saturating_sub(1).min(n - 1);
        dev_slice[idx]
    };

    ClockStats {
        interval_us: mean,
        p50_us: pct(50),
        p95_us: pct(95),
        p99_us: pct(99),
        max_us: *dev_slice.last().unwrap_or(&0),
        sample_n: n as u32,
    }
}

// ─── Worker implementation ─────────────────────────────────────────────────

#[allow(unused_assignments)]
fn run_worker(worker: MidiClockWorker, output: Option<midir::MidiOutput>) {
    use midir::MidiInputConnection;

    set_realtime_priority();

    nice_log!(
        "[EtherTap] MidiClockWorker starting: initial_device={:?}",
        worker.initial_device
    );

    #[cfg(unix)]
    let mut virt_conn: Option<midir::MidiOutputConnection> = output.and_then(|output| {
        use midir::os::unix::VirtualOutput;
        match output.create_virtual("EtherTap MIDI Clock") {
            Ok(c) => Some(c),
            Err(e) => {
                nice_error!("[EtherTap] MIDI clock: virtual port failed: {e:?}");
                nice_error!("[EtherTap] The port may already exist from a previous run.");
                nice_error!("[EtherTap] Try: killall CoreMIDI 2>/dev/null; or reboot.");
                nice_error!(
                    "[EtherTap] MIDI clock will NOT be emitted until EtherTap is restarted."
                );
                None
            }
        }
    });
    #[cfg(not(unix))]
    {
        drop(output);
    }

    let (pass_tx, pass_rx) = crossbeam_channel::bounded::<Vec<u8>>(256);
    let pass_drop_count = Arc::new(AtomicU32::new(0));

    let mut current_device: Option<String> = worker.initial_device.clone();
    let mut phys_out: Option<PhysOutput> = None;
    // phys_in kept alive for its Drop impl (stops the CoreMIDI input callback).
    // unused_assignments: the compiler sees some stores as "unused" because the
    // value is never explicitly read — holding it IS the purpose (RAII guard).
    #[allow(unused_assignments)]
    let mut phys_in: Option<MidiInputConnection<()>> = None;

    // ── Resync gap — silence inserted after BPM change ───────────────────────
    let mut gap_expires: Option<Instant> = None;
    // After the gap (or on TransportStart) hold off until next beat boundary.
    let mut waiting_for_beat: bool = false;

    // ── Initial physical device connection ────────────────────────────────────
    if let Some(name) = current_device.as_deref() {
        nice_log!("[EtherTap] run_worker: initial_device = {:?}", name);
        phys_out = try_connect_out(name);
        if phys_out.is_some() {
            phys_in = try_connect_in(name, pass_tx.clone(), pass_drop_count.clone());
        }
    } else {
        nice_log!("[EtherTap] run_worker: no initial_device (user must select)");
    }
    worker
        .bridge_connected
        .store(phys_out.is_some(), Ordering::Release);
    // If a device is selected but not connected at startup, show connecting state.
    if current_device.is_some() && phys_out.is_none() {
        worker.bridge_connecting.store(true, Ordering::Release);
    }

    let mut known_ports: Vec<String> = Vec::new();

    // ── Inter-pulse timing stats ─ rolling STAT_WINDOW (256) sample ring ──────
    let mut last_send: Option<Instant> = None;
    let mut win_us: [u32; STAT_WINDOW] = [0u32; STAT_WINDOW];
    let mut win_idx: usize = 0;
    // Total pulses received — used to detect wrap and to gate stat updates.
    let mut win_total: usize = 0;
    // Per-instance tick counter for debug log throttling (replaces static AtomicU64).
    let mut tick_count: u64 = 0;

    // Periodic port scan timer — fires every 1 s regardless of clock activity.
    let port_scan_timer = crossbeam_channel::tick(Duration::from_secs(1));

    // ── Reconnect backoff state ───────────────────────────────────────────────
    let mut backoff = crate::reconnect::Backoff::new(1000, 10000);

    loop {
        crossbeam_channel::select! {

                    // ── Clock messages from the audio thread ──────────────────────────
                    recv(worker.clock_rx) -> msg => {
                        let Ok(msg) = msg else { break };

                        match msg {
                            // ── BPM changed: insert silence gap ──────────────────────
                            ClockMsg::BpmChanged { new_bpm } => {
                                let beat_ms = 60_000.0 / new_bpm.max(1.0);
                                let gap_ms = (BEATS_IN_GAP * beat_ms)
                                    .max(MIN_RESYNC_GAP_MS as f64)
                                    .min(MAX_RESYNC_GAP_MS as f64) as u64;
                                gap_expires = Some(Instant::now()
                                    + Duration::from_millis(gap_ms));
                                // Discard timing history — the gap will corrupt intervals.
                                last_send = None;
                            }

                            // ── Transport started: phase-align without gap ────────────
                            ClockMsg::TransportStart => {
                                // Cancel any in-progress BPM-change gap — the user
                                // explicitly restarted transport, so phase-align now
                                // rather than waiting for the gap to drain.
                                gap_expires      = None;
                                waiting_for_beat = true;
                                // Reset timing history so stats start fresh each play.
                                last_send  = None;
                                win_total  = 0;
                                win_idx    = 0;
                                worker.clock_stats.store(&ClockStats::default());
                            }

                            // ── Transport stopped: gate pending ticks ─────────────────
                            ClockMsg::Stop => {
                                // Any ticks already queued before this Stop was sent will
                                // be consumed and silenced by the waiting_for_beat gate.
                                // The next TransportStart re-arms with a beat-boundary
                                // realignment so resumption is always in-phase.
                                gap_expires      = None;
                                waiting_for_beat = true;
                                last_send        = None;
                            }

                            // ── Clock tick (forwarded immediately — no sleep) ─────────
                            ClockMsg::Tick { on_beat } => {
                                // Drop ticks while a resync gap is active.
                                if let Some(exp) = gap_expires {
                                    if Instant::now() < exp {
                                        continue; // still in gap
                                    }
                                    // Gap over — wait for the next beat boundary.
                                    gap_expires      = None;
                                    waiting_for_beat = true;
                                    last_send        = None; // discard stale timestamp
                                }

                                // Hold until the next quarter-note beat to phase-align.
                                if waiting_for_beat {
                                    if !on_beat {
                                        continue;
                                    }
                                    waiting_for_beat = false;
                                }

                                if !worker.enabled.load(Ordering::Relaxed) {
                                    continue;
                                }

                                // ── Timing stats: rolling 256-pulse window ────────────
                                if let Some(prev) = last_send.take() {
                                    let us = prev.elapsed().as_micros()
                                                 .min(u32::MAX as u128) as u32;
                                    win_us[win_idx] = us;
                                    win_idx   = (win_idx + 1) % STAT_WINDOW;
                                    win_total = win_total.saturating_add(1);

                                    // Update stats every PPQ ticks (one beat).
                                    // Gate on ≥2*PPQ samples so p50/p95 are meaningful.
                                    let ppq_usize = worker.midi_ppq as usize;
                                    if win_total.is_multiple_of(ppq_usize) && win_total >= ppq_usize * 2 {
                                        let n = win_total.min(STAT_WINDOW);
                                        let stats = compute_stats(&win_us, n);
                                        worker.clock_stats.store(&stats);
                                    }
                                }
                                last_send = Some(Instant::now());
                                tick_count = tick_count.wrapping_add(1);
                                if tick_count.is_multiple_of(DEBUG_LOG_INTERVAL_TICKS) {
                                    nice_trace!("[EtherTap] tick #{} to virtual port, enabled={}", tick_count, worker.enabled.load(Ordering::Relaxed));
                                }
                                #[cfg(unix)]
                                if let Some(ref mut vc) = virt_conn
                                    && vc.send(CLOCK_BYTE).is_err()
                                {
                                    nice_warn!("[EtherTap] virtual port send failed — port may be disconnected");
                                }
                                if let Some(ref mut out) = phys_out
                                    && out.send(CLOCK_BYTE).is_err()
                                {
                                    phys_out  = None;
                                    phys_in   = None;
                                    worker.bridge_connected.store(false, Ordering::Release);
                                }
                            }
                        }
                    }

                    // ── Passthrough: forward non-clock bytes from physical input ───
                    recv(pass_rx) -> msg => {
                        let drops = pass_drop_count.swap(0, Ordering::Relaxed);
                        if drops > 0 {
                            nice_warn!("[EtherTap] MIDI passthrough: {drops} byte(s) dropped (pass channel full)");
                        }
                        if let Ok(bytes) = msg {
                            #[cfg(unix)]
                            if let Some(ref mut vc) = virt_conn {
                                let _ = vc.send(&bytes);
                            }
                            #[cfg(not(unix))]
                            let _ = bytes;
                        }
                    }

        // ── Device selection changed by editor ────────────────────────────
                    recv(worker.device_change_rx) -> dev => {
                        let Ok(new_device) = dev else { break };
                        nice_log!("[EtherTap] device_change_rx: {:?}", new_device.as_deref());
                        phys_in  = None;
                        phys_out = None;
                        current_device = new_device;
                        backoff.reset();
                        if let Some(name) = current_device.as_deref() {
                            worker.bridge_connecting.store(true, Ordering::Release);
                            phys_out = try_connect_out(name);
                            if phys_out.is_some() {
                                phys_in = try_connect_in(name, pass_tx.clone(), pass_drop_count.clone());
                                if phys_in.is_none() {
                                    nice_warn!("[EtherTap] MIDI passthrough unavailable: \
                                                no input port matching '{name}'");
                                }
                            }
                            worker.bridge_connecting.store(false, Ordering::Release);
                            worker.bridge_connected.store(phys_out.is_some(), Ordering::Release);
                        } else {
                            worker.bridge_connected.store(false, Ordering::Release);
                        }
                    }

                    // ── Device watcher notification (macOS: native CoreMIDI callback;
                    //     non-macOS: polling fallback).  When device topology changes,
                    //     immediately try to reconnect or mark as disconnected.
                    recv(worker.device_watch_rx) -> msg => {
                        if let Ok(ports_now) = msg {
                            handle_port_scan(
                                &ports_now,
                                &mut known_ports,
                                &mut current_device,
                                &mut phys_out,
                                &mut phys_in,
                                &mut backoff,
                                &pass_tx,
                                &pass_drop_count,
                                &worker,
                            );
                        }
                    }

                    // ── Periodic port scan (safety net when notifications are delayed;
                    //     primary mechanism on non-macOS where polling is the only option).
                    recv(port_scan_timer) -> _ => {
                        // On macOS the notification callback is the primary trigger; the
                        // timer is a 30 s recovery fallback.  On non-macOS it runs every 1 s.
                        let ports_now: Vec<String> =
                            if let Ok(out) = midir::MidiOutput::new("EtherTap-Scan") {
                                out.ports()
                                    .iter()
                                    .filter_map(|p| out.port_name(p).ok())
                                    .collect()
                            } else {
                                Vec::new()
                            };
                        handle_port_scan(
                            &ports_now,
                            &mut known_ports,
                            &mut current_device,
                            &mut phys_out,
                            &mut phys_in,
                            &mut backoff,
                            &pass_tx,
                            &pass_drop_count,
                            &worker,
                        );
                    }
                }
    }
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

/// Shared port-scan logic used by both the notification callback (macOS) and
/// the periodic timer (all platforms).
#[allow(clippy::too_many_arguments)]
fn handle_port_scan(
    ports_now: &[String],
    known_ports: &mut Vec<String>,
    current_device: &mut Option<String>,
    phys_out: &mut Option<PhysOutput>,
    phys_in: &mut Option<midir::MidiInputConnection<()>>,
    backoff: &mut crate::reconnect::Backoff,
    pass_tx: &crossbeam_channel::Sender<Vec<u8>>,
    pass_drop_count: &Arc<AtomicU32>,
    worker: &MidiClockWorker,
) {
    if backoff.is_cooling_down() {
        nice_trace!("[EtherTap] handle_port_scan: backoff cooling down, skipping");
        return;
    }

    known_ports.clear();
    known_ports.extend_from_slice(ports_now);
    // Registered loopback ports are invisible to midir's hardware port
    // enumeration (the source of `ports_now`), so union them in here — a
    // connected loopback port must never be reported as "disappeared" by
    // the presence check below.
    for name in midi_loopback::registered_names() {
        if !known_ports.contains(&name) {
            known_ports.push(name);
        }
    }

    // ── Auto-connect guard: device present, none selected ─────────────────
    // Mirrors `connect_to_last`'s reconnect posture (params.rs `connect_to_last`).
    // Only auto-picks when the toggle is explicitly ON — default OFF means
    // zero behavior change ("no surprise automation", CLAUDE.md).
    if worker.auto_connect.load(Ordering::Relaxed)
        && current_device.is_none()
        && !known_ports.is_empty()
    {
        let picked = known_ports[0].clone();
        nice_log!(
            "[EtherTap] handle_port_scan: auto_connect ON, no device selected — \
                    auto-picking '{picked}'"
        );
        *current_device = Some(picked.clone());
        *worker.midi_out_device.lock() = Some(picked);
        backoff.reset();
    }

    if let Some(name) = current_device.as_deref() {
        let present = known_ports.iter().any(|p| p == name);
        nice_trace!(
            "[EtherTap] handle_port_scan: device={:?}, present={}, phys_out.is_some()={}",
            name,
            present,
            phys_out.is_some()
        );

        if present && phys_out.is_none() {
            nice_log!(
                "[EtherTap] handle_port_scan: attempting to connect to {:?}",
                name
            );
            worker.bridge_connecting.store(true, Ordering::Release);
            *phys_out = try_connect_out(name);
            if phys_out.is_some() {
                backoff.record_success();
                *phys_in = try_connect_in(name, pass_tx.clone(), pass_drop_count.clone());
                if phys_in.is_none() {
                    nice_warn!(
                        "[EtherTap] MIDI passthrough unavailable: \
                                no input port matching '{name}'"
                    );
                }
            } else {
                backoff.record_failure();
            }
            worker.bridge_connecting.store(false, Ordering::Release);
            worker
                .bridge_connected
                .store(phys_out.is_some(), Ordering::Release);
        } else if !present && phys_out.is_some() {
            *phys_out = None;
            *phys_in = None;
            // Device disappeared — show as disconnected, not "connecting".
            // bridge_connecting will become true only once the next reconnect
            // attempt actually starts (in the `present && phys_out.is_none()` branch).
            worker.bridge_connecting.store(false, Ordering::Release);
            worker.bridge_connected.store(false, Ordering::Release);
        } else if present && phys_out.is_some() {
            worker.bridge_connecting.store(false, Ordering::Release);
        }
    } else {
        worker.bridge_connecting.store(false, Ordering::Release);
    }
}

/// Try to connect `phys_out` to `device_name`. First consults the
/// [`midi_loopback`] registry — a registered loopback port under this name is
/// interchangeable with a hardware port (see
/// `docs/spec/cross-platform-midi-clock.md`, Approach C). Falls back to midir
/// hardware port enumeration via [`crate::midi_hw::try_hw_out`] if no loopback
/// port is registered.
fn try_connect_out(device_name: &str) -> Option<PhysOutput> {
    if let Ok(tx) = midi_loopback::connect(device_name) {
        nice_log!("[EtherTap] try_connect_out: connected to loopback port '{device_name}'");
        return Some(PhysOutput::Loopback(tx));
    }
    crate::midi_hw::try_hw_out(device_name).map(PhysOutput::Hardware)
}

/// Open a MIDI input on `device_name` and forward every non-clock byte to
/// `pass_tx`.  0xF8 bytes are dropped — EtherTap is the clock master.
/// `drop_count` is incremented when the pass channel is full (silent drop).
///
/// If `device_name` is registered in the [`midi_loopback`] registry, input
/// passthrough is unavailable for it (loopback ports are output-only sinks
/// from the worker's point of view) — this returns `None`, which the caller
/// already logs as "passthrough unavailable".
fn try_connect_in(
    device_name: &str,
    pass_tx: Sender<Vec<u8>>,
    drop_count: Arc<AtomicU32>,
) -> Option<midir::MidiInputConnection<()>> {
    if midi_loopback::connect(device_name).is_ok() {
        nice_log!(
            "[EtherTap] try_connect_in: '{device_name}' is a loopback port — no input passthrough"
        );
        return None;
    }
    crate::midi_hw::try_hw_in(device_name, pass_tx, drop_count)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compute_stats_empty_window() {
        let win = [0u32; STAT_WINDOW];
        let stats = compute_stats(&win, 0);
        assert_eq!(stats.sample_n, 0);
        assert_eq!(stats.interval_us, 0);
    }

    #[test]
    fn compute_stats_single_sample() {
        let mut win = [0u32; STAT_WINDOW];
        win[0] = 20833;
        let stats = compute_stats(&win, 1);
        assert_eq!(stats.sample_n, 0, "n=1 < 2 → early return, no stats");
    }

    #[test]
    fn compute_stats_mixed_jitter() {
        // 63 samples at 20833 µs, 1 outlier at 21333 µs.
        // Mean = (63*20833 + 21333) / 64 = 20840 µs (integer truncation).
        // Deviation of majority: abs(20833 - 20840) = 7 µs → p50 = 7.
        // Deviation of outlier:  abs(21333 - 20840) = 493 µs → max = 493.
        let mut win = [20833u32; STAT_WINDOW];
        win[0] = 21333;
        let stats = compute_stats(&win, 64);
        assert_eq!(stats.p50_us, 7, "p50 should be 7 µs, got {}", stats.p50_us);
        assert_eq!(
            stats.max_us, 493,
            "max should be 493 µs, got {}",
            stats.max_us
        );
    }

    #[test]
    fn compute_stats_identical_intervals() {
        let mut win = [0u32; STAT_WINDOW];
        for slot in win.iter_mut() {
            *slot = 20833;
        }
        let stats = compute_stats(&win, 64);
        assert_eq!(stats.p50_us, 0);
        assert_eq!(stats.p95_us, 0);
        assert_eq!(stats.p99_us, 0);
        assert_eq!(stats.max_us, 0);
        assert_eq!(stats.interval_us, 20833);
    }

    /// Builds a `MidiClockWorker` with dummy channels/atomics for exercising
    /// `handle_port_scan` directly — no real MIDI I/O involved.
    fn make_test_worker(auto_connect: bool) -> MidiClockWorker {
        let (_clock_tx, clock_rx) = crossbeam_channel::bounded(1);
        let (_dc_tx, device_change_rx) = crossbeam_channel::bounded(1);
        let (_dw_tx, device_watch_rx) = crossbeam_channel::bounded(1);
        MidiClockWorker::new(
            Arc::new(AtomicBool::new(true)),
            Arc::new(AtomicBool::new(auto_connect)),
            clock_rx,
            device_change_rx,
            device_watch_rx,
            None,
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicClockStats::default()),
            24,
            Arc::new(parking_lot::Mutex::new(None)),
        )
    }

    /// ON + no device selected + a device is present → auto-pick the first
    /// available device (mirrors `connect_to_last`'s "device present, none
    /// selected" guard). The actual `try_connect_out` will fail in CI (no real
    /// port named "Fake Device 1"), but the auto-pick of `current_device`
    /// itself is the deterministic, testable signal that the guard fired.
    #[test]
    fn handle_port_scan_auto_connect_on_picks_first_device_when_none_selected() {
        let worker = make_test_worker(true);
        let mut known_ports: Vec<String> = Vec::new();
        let mut current_device: Option<String> = None;
        let mut phys_out = None;
        let mut phys_in = None;
        let mut backoff = crate::reconnect::Backoff::new(1000, 10000);
        let (pass_tx, _pass_rx) = crossbeam_channel::bounded::<Vec<u8>>(16);
        let pass_drop_count = Arc::new(AtomicU32::new(0));

        handle_port_scan(
            &["Fake Device 1".to_string(), "Fake Device 2".to_string()],
            &mut known_ports,
            &mut current_device,
            &mut phys_out,
            &mut phys_in,
            &mut backoff,
            &pass_tx,
            &pass_drop_count,
            &worker,
        );

        assert_eq!(
            current_device,
            Some("Fake Device 1".to_string()),
            "auto_connect ON + no device selected must auto-pick the first available device"
        );
        assert_eq!(
            worker.midi_out_device.lock().as_deref(),
            Some("Fake Device 1"),
            "auto-pick must write through to midi_out_device so the GUI Select button reflects it"
        );
    }

    /// OFF (default) → zero behavior change: no auto-pick, `current_device`
    /// stays `None`, connection remains fully manual.
    #[test]
    fn handle_port_scan_auto_connect_off_is_a_no_op() {
        let worker = make_test_worker(false);
        let mut known_ports: Vec<String> = Vec::new();
        let mut current_device: Option<String> = None;
        let mut phys_out = None;
        let mut phys_in = None;
        let mut backoff = crate::reconnect::Backoff::new(1000, 10000);
        let (pass_tx, _pass_rx) = crossbeam_channel::bounded::<Vec<u8>>(16);
        let pass_drop_count = Arc::new(AtomicU32::new(0));

        handle_port_scan(
            &["Fake Device 1".to_string(), "Fake Device 2".to_string()],
            &mut known_ports,
            &mut current_device,
            &mut phys_out,
            &mut phys_in,
            &mut backoff,
            &pass_tx,
            &pass_drop_count,
            &worker,
        );

        assert_eq!(
            current_device, None,
            "auto_connect OFF must leave device selection untouched — fully manual, no surprise automation"
        );
    }

    #[test]
    fn clock_stats_default() {
        let stats = ClockStats::default();
        assert_eq!(stats.interval_us, 0);
        assert_eq!(stats.p50_us, 0);
        assert_eq!(stats.p95_us, 0);
        assert_eq!(stats.p99_us, 0);
        assert_eq!(stats.max_us, 0);
        assert_eq!(stats.sample_n, 0);
    }

    /// `try_connect_out` consults the `midi_loopback` registry by name before
    /// falling back to midir hardware enumeration — a registered loopback
    /// port is interchangeable with a hardware port.
    #[test]
    fn try_connect_out_finds_registered_loopback_port() {
        let name = format!(
            "EtherTap Test Loopback Out {}-{:?}",
            std::process::id(),
            std::thread::current().id()
        );
        let port = midi_loopback::register(&name, midi_loopback::DEFAULT_CAPACITY)
            .expect("register should succeed");

        let mut out = try_connect_out(&name).expect("should connect to registered loopback port");
        out.send(&[0xF8]).expect("send to loopback should succeed");

        let received = port
            .try_recv()
            .expect("loopback port should receive the sent message");
        assert_eq!(received, vec![0xF8]);
    }

    /// A connected loopback `phys_out` must survive a periodic-scan
    /// `handle_port_scan` call even when the scan's `ports_now` (built from
    /// real midir hardware enumeration) does not list the loopback name —
    /// `handle_port_scan` must union in `midi_loopback::registered_names()`
    /// for its presence check, otherwise `!present && phys_out.is_some()`
    /// disconnects the loopback ~1s after every successful connection.
    #[test]
    fn handle_port_scan_does_not_disconnect_present_loopback_port() {
        let name = format!(
            "EtherTap Test Loopback Scan {}-{:?}",
            std::process::id(),
            std::thread::current().id()
        );
        let _port = midi_loopback::register(&name, midi_loopback::DEFAULT_CAPACITY)
            .expect("register should succeed");

        let worker = make_test_worker(false);
        let mut known_ports: Vec<String> = Vec::new();
        let mut current_device: Option<String> = Some(name.clone());
        let mut phys_out = try_connect_out(&name);
        assert!(
            phys_out.is_some(),
            "should connect to registered loopback port"
        );
        let mut phys_in = None;
        let mut backoff = crate::reconnect::Backoff::new(1000, 10000);
        let (pass_tx, _pass_rx) = crossbeam_channel::bounded::<Vec<u8>>(16);
        let pass_drop_count = Arc::new(AtomicU32::new(0));

        // Simulate a midir hardware scan tick that does NOT include the
        // loopback-registered name (loopback ports are invisible to midir).
        handle_port_scan(
            &[],
            &mut known_ports,
            &mut current_device,
            &mut phys_out,
            &mut phys_in,
            &mut backoff,
            &pass_tx,
            &pass_drop_count,
            &worker,
        );

        assert!(
            phys_out.is_some(),
            "a connected loopback port must not be reported as disappeared by the scan timer"
        );
    }

    /// `try_connect_in` recognizes a registered loopback port by name and
    /// returns `None` (no input passthrough for loopback-backed devices)
    /// rather than falling through to a hardware port search.
    #[test]
    fn try_connect_in_returns_none_for_registered_loopback_port() {
        let name = format!(
            "EtherTap Test Loopback In {}-{:?}",
            std::process::id(),
            std::thread::current().id()
        );
        let _port = midi_loopback::register(&name, midi_loopback::DEFAULT_CAPACITY)
            .expect("register should succeed");

        let (pass_tx, _pass_rx) = crossbeam_channel::bounded::<Vec<u8>>(16);
        let drop_count = Arc::new(AtomicU32::new(0));

        assert!(
            try_connect_in(&name, pass_tx, drop_count).is_none(),
            "loopback-backed devices have no input passthrough"
        );
    }

    #[test]
    fn atomic_clock_stats_store_and_load_roundtrip() {
        let atomic = AtomicClockStats::default();
        let written = ClockStats {
            interval_us: 20833,
            p50_us: 7,
            p95_us: 42,
            p99_us: 100,
            max_us: 493,
            sample_n: 64,
        };
        atomic.store(&written);
        let loaded = atomic.load();
        assert_eq!(loaded.interval_us, 20833);
        assert_eq!(loaded.p50_us, 7);
        assert_eq!(loaded.p95_us, 42);
        assert_eq!(loaded.p99_us, 100);
        assert_eq!(loaded.max_us, 493);
        assert_eq!(loaded.sample_n, 64);
    }

    /// When the selected device is present in the port list but phys_out is None
    /// (not yet connected), handle_port_scan must attempt to connect and, on
    /// success, record a backoff success and call try_connect_in for passthrough.
    /// Uses a loopback port so try_connect_out succeeds deterministically.
    #[test]
    fn handle_port_scan_connects_when_device_present_unconnected() {
        let name = format!(
            "EtherTap Test Connect {}-{:?}",
            std::process::id(),
            std::thread::current().id()
        );
        let _port = midi_loopback::register(&name, midi_loopback::DEFAULT_CAPACITY)
            .expect("register loopback for connect test");

        let worker = make_test_worker(false);
        let mut known_ports: Vec<String> = Vec::new();
        let mut current_device: Option<String> = Some(name.clone());
        let mut phys_out: Option<PhysOutput> = None; // not yet connected
        let mut phys_in = None;
        let mut backoff = crate::reconnect::Backoff::new(1000, 10000);
        let (pass_tx, _pass_rx) = crossbeam_channel::bounded::<Vec<u8>>(16);
        let pass_drop_count = Arc::new(AtomicU32::new(0));

        handle_port_scan(
            &[], // loopback port present via registered_names() union
            &mut known_ports,
            &mut current_device,
            &mut phys_out,
            &mut phys_in,
            &mut backoff,
            &pass_tx,
            &pass_drop_count,
            &worker,
        );

        assert!(
            phys_out.is_some(),
            "loopback port should be connected after scan"
        );
        assert!(
            worker
                .bridge_connected
                .load(std::sync::atomic::Ordering::Acquire),
            "bridge_connected must be true after successful connect"
        );
        assert!(
            !backoff.is_cooling_down(),
            "backoff must reset (record_success) after a successful connect"
        );
    }

    /// When the backoff is cooling down, `handle_port_scan` must return early
    /// without mutating any state — otherwise it would hammer a just-failed
    /// device at full scan-timer rate instead of respecting the backoff delay.
    #[test]
    fn handle_port_scan_skips_when_backoff_cooling_down() {
        let worker = make_test_worker(false);
        let mut known_ports: Vec<String> = Vec::new();
        let mut current_device: Option<String> = Some("FakeDevice".to_string());
        let mut phys_out: Option<PhysOutput> = None;
        let mut phys_in = None;
        let mut backoff = crate::reconnect::Backoff::new(100, 10000);
        backoff.record_failure(); // puts it in cooling-down state
        assert!(backoff.is_cooling_down());

        let (pass_tx, _pass_rx) = crossbeam_channel::bounded::<Vec<u8>>(16);
        let pass_drop_count = Arc::new(AtomicU32::new(0));

        handle_port_scan(
            &["FakeDevice".to_string()],
            &mut known_ports,
            &mut current_device,
            &mut phys_out,
            &mut phys_in,
            &mut backoff,
            &pass_tx,
            &pass_drop_count,
            &worker,
        );

        // Still None — the early-return prevented the connect attempt.
        assert!(
            phys_out.is_none(),
            "backoff cooling must prevent connect attempt"
        );
    }

    /// When a connected device disappears from the port list, `handle_port_scan`
    /// must clear `phys_out` / `phys_in` and update the bridge-connected atom.
    #[test]
    fn handle_port_scan_device_disappears_disconnects() {
        let name = format!(
            "EtherTap Test Disappear {}-{:?}",
            std::process::id(),
            std::thread::current().id()
        );
        let _port = midi_loopback::register(&name, midi_loopback::DEFAULT_CAPACITY)
            .expect("register loopback for disappear test");

        let worker = make_test_worker(false);
        let mut known_ports: Vec<String> = Vec::new();
        let mut current_device: Option<String> = Some(name.clone());
        // Connect to the loopback port so phys_out is Some.
        let mut phys_out = try_connect_out(&name);
        assert!(phys_out.is_some());
        let mut phys_in = None;
        let mut backoff = crate::reconnect::Backoff::new(1000, 10000);
        let (pass_tx, _pass_rx) = crossbeam_channel::bounded::<Vec<u8>>(16);
        let pass_drop_count = Arc::new(AtomicU32::new(0));

        // Drop the registered port so it disappears from loopback_names().
        drop(_port);

        // Simulate a scan where the device is no longer listed anywhere.
        handle_port_scan(
            &[], // no hardware ports
            &mut known_ports,
            &mut current_device,
            &mut phys_out,
            &mut phys_in,
            &mut backoff,
            &pass_tx,
            &pass_drop_count,
            &worker,
        );

        assert!(
            phys_out.is_none(),
            "disappeared device must disconnect phys_out"
        );
        assert!(
            !worker
                .bridge_connected
                .load(std::sync::atomic::Ordering::Acquire)
        );
    }

    /// When a selected device is present AND already connected (`phys_out.is_some()`),
    /// `handle_port_scan` must set `bridge_connecting = false` and leave the
    /// connection intact — no reconnect attempt, no state mutation.
    #[test]
    fn handle_port_scan_device_present_and_already_connected() {
        let name = format!(
            "EtherTap Test AlreadyConn {}-{:?}",
            std::process::id(),
            std::thread::current().id()
        );
        let _port = midi_loopback::register(&name, midi_loopback::DEFAULT_CAPACITY)
            .expect("register loopback");

        let worker = make_test_worker(false);
        worker
            .bridge_connecting
            .store(true, std::sync::atomic::Ordering::Release);

        let mut known_ports: Vec<String> = Vec::new();
        let mut current_device: Option<String> = Some(name.clone());
        let mut phys_out = try_connect_out(&name);
        assert!(phys_out.is_some());
        let mut phys_in = None;
        let mut backoff = crate::reconnect::Backoff::new(1000, 10000);
        let (pass_tx, _pass_rx) = crossbeam_channel::bounded::<Vec<u8>>(16);
        let pass_drop_count = Arc::new(AtomicU32::new(0));

        handle_port_scan(
            &[], // loopback port is still registered, so it IS present via loopback union
            &mut known_ports,
            &mut current_device,
            &mut phys_out,
            &mut phys_in,
            &mut backoff,
            &pass_tx,
            &pass_drop_count,
            &worker,
        );

        assert!(phys_out.is_some(), "connected port must remain connected");
        assert!(
            !worker
                .bridge_connecting
                .load(std::sync::atomic::Ordering::Acquire),
            "bridge_connecting must be cleared when already connected"
        );
    }

    /// When no device is selected at all, `handle_port_scan` must set
    /// `bridge_connecting = false` regardless of the port list — there is
    /// nothing to connect to.
    #[test]
    fn handle_port_scan_no_device_selected_clears_connecting() {
        let worker = make_test_worker(false);
        worker
            .bridge_connecting
            .store(true, std::sync::atomic::Ordering::Release);

        let mut known_ports: Vec<String> = Vec::new();
        let mut current_device: Option<String> = None;
        let mut phys_out: Option<PhysOutput> = None;
        let mut phys_in = None;
        let mut backoff = crate::reconnect::Backoff::new(1000, 10000);
        let (pass_tx, _pass_rx) = crossbeam_channel::bounded::<Vec<u8>>(16);
        let pass_drop_count = Arc::new(AtomicU32::new(0));

        handle_port_scan(
            &["SomeDevice".to_string()],
            &mut known_ports,
            &mut current_device,
            &mut phys_out,
            &mut phys_in,
            &mut backoff,
            &pass_tx,
            &pass_drop_count,
            &worker,
        );

        assert!(
            !worker
                .bridge_connecting
                .load(std::sync::atomic::Ordering::Acquire),
            "bridge_connecting must be cleared when no device is selected"
        );
    }

    /// When initial_device is nonexistent at startup, bridge_connecting=true.
    /// Covers run_worker lines 396-398 (enter the if-let branch, log, try_connect_out)
    /// and line 410 (connecting=true when phys_out=None but device is Some).
    #[test]
    fn run_worker_nonexistent_initial_device_sets_bridge_connecting() {
        let (clock_tx, clock_rx) = crossbeam_channel::bounded(1);
        let (_dc_tx, device_change_rx) = crossbeam_channel::bounded(1);
        let (_dw_tx, device_watch_rx) = crossbeam_channel::bounded(1);
        let bridge_connected = Arc::new(AtomicBool::new(false));
        let bridge_connecting = Arc::new(AtomicBool::new(false));

        let worker = MidiClockWorker::new(
            Arc::new(AtomicBool::new(true)),
            Arc::new(AtomicBool::new(false)),
            clock_rx,
            device_change_rx,
            device_watch_rx,
            Some("EtherTap Test NonExistent Port 9999999".to_string()),
            bridge_connected.clone(),
            bridge_connecting.clone(),
            Arc::new(AtomicClockStats::default()),
            24,
            Arc::new(parking_lot::Mutex::new(None)),
        );

        let bc = bridge_connected.clone();
        let bconn = bridge_connecting.clone();
        let handle = std::thread::spawn(move || worker.run());

        // Poll until the worker sets bridge_connecting (set synchronously before the
        // main loop, so this should be near-instant on any machine). 5 s is a generous
        // bound for a slow CI runner.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            if bconn.load(std::sync::atomic::Ordering::Acquire) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }

        assert!(
            bconn.load(std::sync::atomic::Ordering::Acquire),
            "bridge_connecting must be set when device is selected but port not found"
        );
        assert!(
            !bc.load(std::sync::atomic::Ordering::Acquire),
            "bridge_connected must remain false for a nonexistent port"
        );

        drop(clock_tx);
        let _ = handle.join();
    }

    /// When initial_device IS connectable at startup, bridge_connected=true.
    /// Covers run_worker line 400 (phys_in = try_connect_in) — the `if phys_out.is_some()` body.
    #[test]
    fn run_worker_existing_initial_device_sets_bridge_connected() {
        let port_name = format!(
            "EtherTap Test InitDev {} {:?}",
            std::process::id(),
            std::thread::current().id()
        );
        let _port = midi_loopback::register(&port_name, midi_loopback::DEFAULT_CAPACITY)
            .expect("register loopback port");

        let (clock_tx, clock_rx) = crossbeam_channel::bounded(1);
        let (_dc_tx, device_change_rx) = crossbeam_channel::bounded(1);
        let (_dw_tx, device_watch_rx) = crossbeam_channel::bounded(1);
        let bridge_connected = Arc::new(AtomicBool::new(false));
        let bridge_connecting = Arc::new(AtomicBool::new(false));

        let worker = MidiClockWorker::new(
            Arc::new(AtomicBool::new(true)),
            Arc::new(AtomicBool::new(false)),
            clock_rx,
            device_change_rx,
            device_watch_rx,
            Some(port_name),
            bridge_connected.clone(),
            bridge_connecting.clone(),
            Arc::new(AtomicClockStats::default()),
            24,
            Arc::new(parking_lot::Mutex::new(None)),
        );

        let bc = bridge_connected.clone();
        let handle = std::thread::spawn(move || worker.run());

        // Poll until bridge_connected flips — it is set synchronously before the
        // main loop. 5 s covers the worst-case slow CI runner.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            if bc.load(std::sync::atomic::Ordering::Acquire) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }

        assert!(
            bc.load(std::sync::atomic::Ordering::Acquire),
            "bridge_connected must be true when initial device is connectable"
        );

        drop(clock_tx);
        let _ = handle.join();
    }
}
