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
    atomic::{AtomicBool, AtomicU32, Ordering},
    Arc,
};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender};


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
    pub interval_us: u32,  // mean inter-pulse interval
    pub p50_us:      u32,  // 50th-percentile jitter (median)
    pub p95_us:      u32,  // 95th-percentile jitter
    pub p99_us:      u32,  // 99th-percentile jitter
    pub max_us:      u32,  // peak jitter in current window
    pub sample_n:    u32,  // samples currently in window (≤ 256)
}

/// Lock-free version of `ClockStats` for sharing between the RT-priority MIDI
/// worker (writer) and the editor GUI thread (reader).  Replacing
/// `Mutex<ClockStats>` eliminates the mutex acquisition on the RT thread.
#[derive(Default)]
pub struct AtomicClockStats {
    pub interval_us: AtomicU32,
    pub p50_us:      AtomicU32,
    pub p95_us:      AtomicU32,
    pub p99_us:      AtomicU32,
    pub max_us:      AtomicU32,
    pub sample_n:    AtomicU32,
}

impl AtomicClockStats {
    pub fn store(&self, s: &ClockStats) {
        self.interval_us.store(s.interval_us, Ordering::Relaxed);
        self.p50_us     .store(s.p50_us,      Ordering::Relaxed);
        self.p95_us     .store(s.p95_us,      Ordering::Relaxed);
        self.p99_us     .store(s.p99_us,      Ordering::Relaxed);
        self.max_us     .store(s.max_us,      Ordering::Relaxed);
        self.sample_n   .store(s.sample_n,    Ordering::Relaxed);
    }

    pub fn load(&self) -> ClockStats {
        ClockStats {
            interval_us: self.interval_us.load(Ordering::Relaxed),
            p50_us:      self.p50_us     .load(Ordering::Relaxed),
            p95_us:      self.p95_us     .load(Ordering::Relaxed),
            p99_us:      self.p99_us     .load(Ordering::Relaxed),
            max_us:      self.max_us     .load(Ordering::Relaxed),
            sample_n:    self.sample_n   .load(Ordering::Relaxed),
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
    enabled:          Arc<AtomicBool>,
    clock_rx:         Receiver<ClockMsg>,
    device_change_rx: Receiver<Option<String>>,
    device_watch_rx:  Receiver<Vec<String>>,
    initial_device:   Option<String>,
    bridge_connected:  Arc<AtomicBool>,
    bridge_connecting: Arc<AtomicBool>,
    /// Shared timing statistics — written by this worker, read by the editor.
    pub clock_stats:   Arc<AtomicClockStats>,
    /// Pulses per quarter note — used to gate stats updates at beat boundaries.
    pub midi_ppq:      u8,
}

impl MidiClockWorker {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        enabled:           Arc<AtomicBool>,
        clock_rx:          Receiver<ClockMsg>,
        device_change_rx:  Receiver<Option<String>>,
        device_watch_rx:   Receiver<Vec<String>>,
        initial_device:    Option<String>,
        bridge_connected:  Arc<AtomicBool>,
        bridge_connecting: Arc<AtomicBool>,
        clock_stats:       Arc<AtomicClockStats>,
        midi_ppq:          u8,
    ) -> Self {
        Self { enabled, clock_rx, device_change_rx, device_watch_rx,
               initial_device, bridge_connected, bridge_connecting, clock_stats, midi_ppq }
    }

    pub fn run(self) {
        use midir::MidiOutput;
        let output = match MidiOutput::new("EtherTap") {
            Ok(o) => o,
            Err(e) => { log::error!("[EtherTap] MIDI clock: init failed: {e}"); return; }
        };

        #[cfg(not(target_os = "windows"))]
        run_unix(self, output);

        #[cfg(target_os = "windows")]
        {
            drop(output);
            log::warn!("[EtherTap] MIDI clock: virtual ports unsupported on Windows");
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
    struct MachTimebaseInfo { numer: u32, denom: u32 }

    #[repr(C)]
    struct ThreadTimeConstraintPolicy {
        period:      u32,
        computation: u32,
        constraint:  u32,
        preemptible: i32, // boolean_t
    }

    const THREAD_TIME_CONSTRAINT_POLICY:       u32 = 2;
    const THREAD_TIME_CONSTRAINT_POLICY_COUNT: u32 = 4;

    extern "C" {
        fn mach_timebase_info(info: *mut MachTimebaseInfo) -> i32;
        fn mach_thread_self() -> u32;
        fn thread_policy_set(
            thread:      u32,
            flavor:      u32,
            policy_info: *const u32,
            count:       u32,
        ) -> i32;
    }

    unsafe {
        let mut tb = MachTimebaseInfo { numer: 1, denom: 1 };
        mach_timebase_info(&mut tb);

        let ratio = tb.numer as f64 / tb.denom as f64;
        let policy = ThreadTimeConstraintPolicy {
            period:      (8_000_000.0 / ratio) as u32, //  8 ms
            computation: (  500_000.0 / ratio) as u32, //  0.5 ms
            constraint:  (4_000_000.0 / ratio) as u32, //  4 ms
            preemptible: 1,
        };

        let ret = thread_policy_set(
            mach_thread_self(),
            THREAD_TIME_CONSTRAINT_POLICY,
            &policy as *const _ as *const u32,
            THREAD_TIME_CONSTRAINT_POLICY_COUNT,
        );
        if ret != 0 {
            log::warn!("[EtherTap] MIDI RT thread priority failed (kern={ret})");
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
        p50_us:      pct(50),
        p95_us:      pct(95),
        p99_us:      pct(99),
        max_us:      *dev_slice.last().unwrap_or(&0),
        sample_n:    n as u32,
    }
}

// ─── Non-Windows implementation ───────────────────────────────────────────────

#[cfg(not(target_os = "windows"))]
#[allow(unused_assignments)]
fn run_unix(worker: MidiClockWorker, output: midir::MidiOutput) {
    use midir::os::unix::VirtualOutput;
    use midir::{MidiInputConnection, MidiOutputConnection};

    set_realtime_priority();

    log::info!("[EtherTap] MidiClockWorker starting: initial_device={:?}", worker.initial_device);

    let mut virt_conn: Option<_> = match output.create_virtual("EtherTap MIDI Clock") {
        Ok(c) => Some(c),
        Err(e) => {
            log::error!("[EtherTap] MIDI clock: virtual port failed: {e:?}");
            log::error!("[EtherTap] The port may already exist from a previous run.");
            log::error!("[EtherTap] Try: killall CoreMIDI 2>/dev/null; or reboot.");
            log::error!("[EtherTap] MIDI clock will NOT be emitted until EtherTap is restarted.");
            None
        }
    };

    let (pass_tx, pass_rx) = crossbeam_channel::bounded::<Vec<u8>>(256);
    let pass_drop_count = Arc::new(AtomicU32::new(0));

    let mut current_device: Option<String> = worker.initial_device.clone();
    let mut phys_out: Option<MidiOutputConnection> = None;
    // phys_in kept alive for its Drop impl (stops the CoreMIDI input callback).
    // unused_assignments: the compiler sees some stores as "unused" because the
    // value is never explicitly read — holding it IS the purpose (RAII guard).
    #[allow(unused_assignments)]
    let mut phys_in: Option<MidiInputConnection<()>> = None;

    // ── Resync gap — silence inserted after BPM change ───────────────────────
    let mut gap_expires:      Option<Instant> = None;
    // After the gap (or on TransportStart) hold off until next beat boundary.
    let mut waiting_for_beat: bool            = false;

    // ── Initial physical device connection ────────────────────────────────────
    if let Some(ref name) = current_device {
        log::info!("[EtherTap] run_unix: initial_device = {:?}", name);
        phys_out = try_connect_out(name);
        if phys_out.is_some() {
            phys_in = try_connect_in(name, pass_tx.clone(), pass_drop_count.clone());
        }
    } else {
        log::info!("[EtherTap] run_unix: no initial_device (user must select)");
    }
    worker.bridge_connected.store(phys_out.is_some(), Ordering::Release);
    // If a device is selected but not connected at startup, show connecting state.
    if current_device.is_some() && phys_out.is_none() {
        worker.bridge_connecting.store(true, Ordering::Release);
    }

    let mut known_ports: Vec<String> = Vec::new();

    // ── Inter-pulse timing stats ─ rolling STAT_WINDOW (256) sample ring ──────
    let mut last_send:  Option<Instant>         = None;
    let mut win_us:     [u32; STAT_WINDOW]      = [0u32; STAT_WINDOW];
    let mut win_idx:    usize                   = 0;
    // Total pulses received — used to detect wrap and to gate stat updates.
    let mut win_total:  usize                   = 0;
    // Per-instance tick counter for debug log throttling (replaces static AtomicU64).
    let mut tick_count: u64                     = 0;

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
                            log::debug!("[EtherTap] tick #{} to virtual port, enabled={}", tick_count, worker.enabled.load(Ordering::Relaxed));
                        }
                        if let Some(ref mut vc) = virt_conn {
                            if vc.send(CLOCK_BYTE).is_err() {
                                log::warn!("[EtherTap] virtual port send failed — port may be disconnected");
                            }
                        }
                        if let Some(ref mut out) = phys_out {
                            if out.send(CLOCK_BYTE).is_err() {
                                phys_out  = None;
                                phys_in   = None;
                                worker.bridge_connected.store(false, Ordering::Release);
                            }
                        }
                    }
                }
            }

            // ── Passthrough: forward non-clock bytes from physical input ───
            recv(pass_rx) -> msg => {
                let drops = pass_drop_count.swap(0, Ordering::Relaxed);
                if drops > 0 {
                    log::warn!("[EtherTap] MIDI passthrough: {drops} byte(s) dropped (pass channel full)");
                }
                if let Ok(bytes) = msg {
                    if let Some(ref mut vc) = virt_conn {
                        let _ = vc.send(&bytes);
                    }
                }
            }

// ── Device selection changed by editor ────────────────────────────
            recv(worker.device_change_rx) -> dev => {
                let Ok(new_device) = dev else { break };
                log::info!("[EtherTap] device_change_rx: {:?}", new_device.as_deref());
                phys_in  = None;
                phys_out = None;
                current_device = new_device;
                backoff.reset();
                if let Some(ref name) = current_device {
                    worker.bridge_connecting.store(true, Ordering::Release);
                    phys_out = try_connect_out(name);
                    if phys_out.is_some() {
                        phys_in = try_connect_in(name, pass_tx.clone(), pass_drop_count.clone());
                        if phys_in.is_none() {
                            log::warn!("[EtherTap] MIDI passthrough unavailable: \
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
                        &current_device,
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
                    &current_device,
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
#[cfg(not(target_os = "windows"))]
#[allow(clippy::too_many_arguments)]
fn handle_port_scan(
    ports_now: &[String],
    known_ports: &mut Vec<String>,
    current_device: &Option<String>,
    phys_out: &mut Option<midir::MidiOutputConnection>,
    phys_in: &mut Option<midir::MidiInputConnection<()>>,
    backoff: &mut crate::reconnect::Backoff,
    pass_tx: &crossbeam_channel::Sender<Vec<u8>>,
    pass_drop_count: &Arc<AtomicU32>,
    worker: &MidiClockWorker,
) {
    if backoff.is_cooling_down() {
        log::debug!("[EtherTap] handle_port_scan: backoff cooling down, skipping");
        return;
    }

    known_ports.clear();
    known_ports.extend_from_slice(ports_now);

    if let Some(ref name) = current_device {
        let present = known_ports.iter().any(|p| p == name);
        log::debug!("[EtherTap] handle_port_scan: device={:?}, present={}, phys_out.is_some()={}",
            name, present, phys_out.is_some());

        if present && phys_out.is_none() {
            log::info!("[EtherTap] handle_port_scan: attempting to connect to {:?}", name);
            worker.bridge_connecting.store(true, Ordering::Release);
            *phys_out = try_connect_out(name);
            if phys_out.is_some() {
                backoff.record_success();
                *phys_in = try_connect_in(name, pass_tx.clone(), pass_drop_count.clone());
                if phys_in.is_none() {
                    log::warn!("[EtherTap] MIDI passthrough unavailable: \
                                no input port matching '{name}'");
                }
            } else {
                backoff.record_failure();
            }
            worker.bridge_connecting.store(false, Ordering::Release);
            worker.bridge_connected.store(phys_out.is_some(), Ordering::Release);
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

#[cfg(not(target_os = "windows"))]
fn try_connect_out(device_name: &str) -> Option<midir::MidiOutputConnection> {
    let out = match midir::MidiOutput::new("EtherTap-PhysOut") {
        Ok(o) => o,
        Err(e) => {
            log::warn!("[EtherTap] try_connect_out: MidiOutput::new failed: {e}");
            return None;
        }
    };
    let port = match out.ports().into_iter().find(|p| {
        out.port_name(p).map(|n| n == device_name).unwrap_or(false)
    }) {
        Some(p) => p,
        None => {
            log::warn!("[EtherTap] try_connect_out: port '{device_name}' not found");
            return None;
        }
    };
    match out.connect(&port, "EtherTap-PhysOut") {
        Ok(c) => {
            log::info!("[EtherTap] try_connect_out: connected to '{device_name}'");
            Some(c)
        }
        Err(e) => {
            log::warn!("[EtherTap] try_connect_out: connect to '{device_name}' failed: {e}");
            None
        }
    }
}

/// Open a MIDI input on `device_name` and forward every non-clock byte to
/// `pass_tx`.  0xF8 bytes are dropped — EtherTap is the clock master.
/// `drop_count` is incremented when the pass channel is full (silent drop).
#[cfg(not(target_os = "windows"))]
fn try_connect_in(
    device_name: &str,
    pass_tx: Sender<Vec<u8>>,
    drop_count: Arc<AtomicU32>,
) -> Option<midir::MidiInputConnection<()>> {
    use midir::MidiInput;
    let inp = match MidiInput::new("EtherTap-PhysIn") {
        Ok(i) => i,
        Err(e) => {
            log::warn!("[EtherTap] try_connect_in: MidiInput::new failed: {e}");
            return None;
        }
    };
    let port = match inp.ports().into_iter().find(|p| {
        inp.port_name(p).map(|n| n == device_name).unwrap_or(false)
    }) {
        Some(p) => p,
        None => {
            log::warn!("[EtherTap] try_connect_in: port '{device_name}' not found");
            return None;
        }
    };
    match inp.connect(&port, "EtherTap-PhysIn", move |_ts, msg, _| {
        if msg.first().copied() != Some(0xF8) {
            if pass_tx.try_send(msg.to_vec()).is_err() {
                drop_count.fetch_add(1, Ordering::Relaxed);
            }
        }
    }, ()) {
        Ok(c) => {
            log::info!("[EtherTap] try_connect_in: connected input to '{device_name}'");
            Some(c)
        }
        Err(e) => {
            log::warn!("[EtherTap] try_connect_in: connect to '{device_name}' failed: {e}");
            None
        }
    }
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
        let mut win = [20833u32; STAT_WINDOW];
        win[0] = 21333;
        let stats = compute_stats(&win, 64);
        assert!(stats.p50_us <= 500);
        assert!(stats.max_us >= 200);
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

#[test]
    fn backoff_initial_state() {
        let b = crate::reconnect::Backoff::new(1000, 10000);
        assert!(!b.is_cooling_down());
        assert_eq!(b.next_delay_ms(), 1000);
    }

    #[test]
    fn backoff_exponential_growth() {
        let mut b = crate::reconnect::Backoff::new(1000, 10000);
        assert_eq!(b.next_delay_ms(), 1000);
        b.record_failure();
        assert_eq!(b.next_delay_ms(), 2000);
        b.record_failure();
        assert_eq!(b.next_delay_ms(), 4000);
        b.record_failure();
        assert_eq!(b.next_delay_ms(), 8000);
        b.record_failure();
        assert_eq!(b.next_delay_ms(), 10000, "capped at max");
    }

    #[test]
    fn backoff_reset_on_success() {
        let mut b = crate::reconnect::Backoff::new(1000, 10000);
        b.record_failure();
        b.record_failure();
        assert!(b.is_cooling_down());
        b.record_success();
        assert!(!b.is_cooling_down());
        assert_eq!(b.next_delay_ms(), 1000, "back to base after success");
    }

    #[test]
    fn backoff_cooling_down() {
        let mut b = crate::reconnect::Backoff::new(1000, 10000);
        b.record_failure();
        assert!(b.is_cooling_down());
        std::hint::black_box(&b);
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
}
