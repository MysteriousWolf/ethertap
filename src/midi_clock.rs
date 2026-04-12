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
/// When the DAW BPM changes by more than 0.5 BPM the worker inserts a 1 500 ms
/// silence (no 0xF8 bytes).  Receivers detect the missing pulses, reset their
/// tempo-averaging filter, and snap to the new BPM immediately on the first
/// burst after the gap.  Resumption is held until the next beat-boundary tick
/// so the receiver's pulse counter is aligned with the DAW click track.
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
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender};
use parking_lot::Mutex;

// ─── Constants ────────────────────────────────────────────────────────────────

/// Silence inserted after `BpmChanged` to force receivers to reset their
/// tempo-averaging filter, followed by beat-aligned resumption.
/// 1 500 ms ≈ 3× the former 500 ms — gives sluggish receivers (some Behringer
/// units need >800 ms) a clean window to detect the gap and reset phase.
const RESYNC_GAP_MS: u64 = 1_500;

const CLOCK_BYTE: &[u8] = &[0xF8];

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
    BpmChanged,

    /// Transport moved from stopped → playing.
    /// No silence gap is inserted; the worker simply holds off until the next
    /// beat-boundary tick so the first 0xF8 is phase-aligned with the DAW click.
    TransportStart,
}

// ─── Worker struct ────────────────────────────────────────────────────────────

pub struct MidiClockWorker {
    enabled:          Arc<Mutex<bool>>,
    clock_rx:         Receiver<ClockMsg>,
    device_change_rx: Receiver<Option<String>>,
    initial_device:   Option<String>,
    bridge_connected: Arc<AtomicBool>,
    /// Shared timing statistics — written by this worker, read by the editor.
    pub clock_stats:  Arc<Mutex<ClockStats>>,
}

impl MidiClockWorker {
    pub fn new(
        enabled:          Arc<Mutex<bool>>,
        clock_rx:         Receiver<ClockMsg>,
        device_change_rx: Receiver<Option<String>>,
        initial_device:   Option<String>,
        bridge_connected: Arc<AtomicBool>,
        clock_stats:      Arc<Mutex<ClockStats>>,
    ) -> Self {
        Self { enabled, clock_rx, device_change_rx, initial_device, bridge_connected,
               clock_stats }
    }

    pub fn run(self) {
        use midir::MidiOutput;
        let output = match MidiOutput::new("EtherTap") {
            Ok(o) => o,
            Err(e) => { eprintln!("[EtherTap] MIDI clock: init failed: {e}"); return; }
        };

        #[cfg(not(target_os = "windows"))]
        run_unix(self, output);

        #[cfg(target_os = "windows")]
        {
            drop(output);
            eprintln!("[EtherTap] MIDI clock: virtual ports unsupported on Windows");
        }
    }
}

// ─── macOS real-time thread priority ─────────────────────────────────────────
//
// Calls thread_policy_set(THREAD_TIME_CONSTRAINT_POLICY) so the OS scheduler
// treats the MIDI clock worker as a soft real-time thread.  This eliminates the
// "two pulses bunched together" stutters caused by scheduler pre-emption between
// consecutive sends.
//
// Period 8 ms covers tempos up to ~310 BPM at 24 PPQ (shortest inter-tick gap).
// Computation 0.5 ms: time to send one 0xF8 byte via CoreMIDI.
// Constraint 4 ms: deadline — the kernel must schedule us within 4 ms of need.
// Preemptible 1: another RT thread may still pre-empt us between ticks (safe).

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
            eprintln!("[EtherTap] MIDI RT thread priority failed (kern={ret})");
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
        let idx = ((n * p + 99) / 100).saturating_sub(1).min(n - 1);
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
fn run_unix(worker: MidiClockWorker, output: midir::MidiOutput) {
    use midir::os::unix::VirtualOutput;
    use midir::{MidiInputConnection, MidiOutputConnection};

    set_realtime_priority();

    let mut virt_conn = match output.create_virtual("EtherTap MIDI Clock") {
        Ok(c) => c,
        Err(e) => { eprintln!("[EtherTap] MIDI clock: virtual port failed: {e:?}"); return; }
    };

    let (pass_tx, pass_rx) = crossbeam_channel::bounded::<Vec<u8>>(256);

    let mut current_device: Option<String> = worker.initial_device;
    let mut phys_out: Option<MidiOutputConnection> = None;
    // phys_in kept alive for its Drop impl (stops the CoreMIDI input callback).
    #[allow(unused_assignments)]
    let mut phys_in: Option<MidiInputConnection<()>> = None;

    // ── Resync gap — silence inserted after BPM change ───────────────────────
    let mut gap_expires:      Option<Instant> = None;
    // After the gap (or on TransportStart) hold off until next beat boundary.
    let mut waiting_for_beat: bool            = false;

    // ── Initial physical device connection ────────────────────────────────────
    if let Some(ref name) = current_device.clone() {
        phys_out = try_connect_out(name);
        if phys_out.is_some() {
            phys_in = try_connect_in(name, pass_tx.clone());
        }
    }
    worker.bridge_connected.store(phys_out.is_some(), Ordering::Relaxed);

    let mut last_reconnect = Instant::now();

    // ── Inter-pulse timing stats — rolling STAT_WINDOW (256) sample ring ──────
    let mut last_send:  Option<Instant>         = None;
    let mut win_us:     [u32; STAT_WINDOW]      = [0u32; STAT_WINDOW];
    let mut win_idx:    usize                   = 0;
    // Total pulses received — used to detect wrap and to gate stat updates.
    let mut win_total:  usize                   = 0;

    loop {
        crossbeam_channel::select! {

            // ── Clock messages from the audio thread ──────────────────────────
            recv(worker.clock_rx) -> msg => {
                let Ok(msg) = msg else { break };

                match msg {
                    // ── BPM changed: insert silence gap ──────────────────────
                    ClockMsg::BpmChanged => {
                        gap_expires = Some(Instant::now()
                            + Duration::from_millis(RESYNC_GAP_MS));
                        // Discard timing history — the gap will corrupt intervals.
                        last_send = None;
                    }

                    // ── Transport started: phase-align without gap ────────────
                    ClockMsg::TransportStart => {
                        waiting_for_beat = true;
                        // Reset timing history so stats start fresh each play.
                        last_send  = None;
                        win_total  = 0;
                        win_idx    = 0;
                        *worker.clock_stats.lock() = ClockStats::default();
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

                        if !*worker.enabled.lock() {
                            continue;
                        }

                        // ── Timing stats: rolling 256-pulse window ────────────
                        if let Some(prev) = last_send.take() {
                            let us = prev.elapsed().as_micros()
                                         .min(u32::MAX as u128) as u32;
                            win_us[win_idx] = us;
                            win_idx   = (win_idx + 1) % STAT_WINDOW;
                            win_total = win_total.saturating_add(1);

                            // Update stats every 24 pulses (one beat) once we have
                            // at least 48 samples (2 beats — enough for p50/p95).
                            // After 256 samples the full window is valid.
                            if win_total % 24 == 0 && win_total >= 48 {
                                let n = win_total.min(STAT_WINDOW);
                                let stats = compute_stats(&win_us, n);
                                *worker.clock_stats.lock() = stats;
                            }
                        }
                        last_send = Some(Instant::now());

                        let _ = virt_conn.send(CLOCK_BYTE);
                        if let Some(ref mut out) = phys_out {
                            if out.send(CLOCK_BYTE).is_err() {
                                phys_out  = None;
                                phys_in   = None;
                                worker.bridge_connected.store(false, Ordering::Relaxed);
                            }
                        }
                    }
                }
            }

            // ── Device selection changed by editor ────────────────────────────
            recv(worker.device_change_rx) -> dev => {
                let Ok(new_device) = dev else { break };
                phys_in  = None;
                phys_out = None;
                current_device = new_device;
                if let Some(ref name) = current_device.clone() {
                    phys_out = try_connect_out(name);
                    if phys_out.is_some() {
                        phys_in = try_connect_in(name, pass_tx.clone());
                    }
                }
                worker.bridge_connected.store(phys_out.is_some(), Ordering::Relaxed);
                last_reconnect = Instant::now();
            }

            // ── MIDI passthrough from physical input ──────────────────────────
            recv(pass_rx) -> msg => {
                if let Ok(bytes) = msg {
                    let _ = virt_conn.send(&bytes);
                }
            }

            // ── Reconnect heartbeat ───────────────────────────────────────────
            default(Duration::from_millis(500)) => {
                if phys_out.is_none() {
                    if let Some(ref name) = current_device.clone() {
                        if last_reconnect.elapsed() >= Duration::from_secs(1) {
                            last_reconnect = Instant::now();
                            phys_out = try_connect_out(name);
                            if phys_out.is_some() {
                                phys_in = try_connect_in(name, pass_tx.clone());
                            }
                            worker.bridge_connected.store(phys_out.is_some(), Ordering::Relaxed);
                        }
                    }
                }
            }
        }
    }
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

#[cfg(not(target_os = "windows"))]
fn try_connect_out(device_name: &str) -> Option<midir::MidiOutputConnection> {
    let out  = midir::MidiOutput::new("EtherTap-PhysOut").ok()?;
    let port = out.ports().into_iter().find(|p| {
        out.port_name(p).map(|n| n == device_name).unwrap_or(false)
    })?;
    out.connect(&port, "EtherTap-PhysOut").ok()
}

/// Open a MIDI input on `device_name` and forward every non-clock byte to
/// `pass_tx`.  0xF8 bytes are dropped — EtherTap is the clock master.
#[cfg(not(target_os = "windows"))]
fn try_connect_in(
    device_name: &str,
    pass_tx: Sender<Vec<u8>>,
) -> Option<midir::MidiInputConnection<()>> {
    use midir::MidiInput;
    let inp  = MidiInput::new("EtherTap-PhysIn").ok()?;
    let port = inp.ports().into_iter().find(|p| {
        inp.port_name(p).map(|n| n == device_name).unwrap_or(false)
    })?;
    inp.connect(&port, "EtherTap-PhysIn", move |_ts, msg, _| {
        if msg.first().copied() != Some(0xF8) {
            let _ = pass_tx.try_send(msg.to_vec());
        }
    }, ()).ok()
}
