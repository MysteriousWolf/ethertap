/// EtherTap – VST3 OSC control bridge for Behringer X32 / Midas M32.
///
/// # Architecture
///
/// ```text
///  ┌──────────────────────────────────────────────────────────┐
///  │  Host / DAW                                              │
///  │  ┌───────────────┐     ┌──────────────────────────────┐ │
///  │  │  Audio Thread │     │  GUI Thread (Iced editor)    │ │
///  │  │  process()    │     │  Telemetry + controls        │ │
///  │  └───────┬───────┘     └──────────────┬───────────────┘ │
///  └──────────┼──────────────────────────  │  ───────────────┘
///             │  crossbeam_channel (lock-free)  │
///             └──────────────┬──────────────────┘
///                            ▼
///                ┌───────────────────────┐
///                │  NetworkWorker thread  │
///                │  UDP  →  X32 / M32    │
///                └───────────────────────┘
/// ```
///
/// Real-time safety contract: `process()` must never allocate, block, or lock
/// a contended mutex.  All X32 I/O is delegated via bounded lock-free channels.
use std::sync::{
    atomic::{AtomicBool, AtomicI32, AtomicU8, AtomicU32, AtomicU64, Ordering},
    Arc,
};

use nih_plug::prelude::*;
use parking_lot::Mutex;

mod editor;
mod midi_clock;
mod midi_watcher;
pub mod network;
pub mod osc;
pub mod reconnect;
mod params;

use editor::EditorData;
use network::{NetworkCommand, NetworkStatus, NetworkWorker, now_ms};
use params::{EtherTapParams, SyncMode};

// ─── Timing constants ────────────────────────────────────────────────────────

/// A BPM change must be absent for this many milliseconds before "Sync on
/// Change" fires.  Gives the user time to finish dragging a tempo slider.
const SETTLE_MS: u64 = 500;

/// Minimum BPM delta that restarts the settle timer (OSC sync path).
/// Small enough to catch deliberate automation moves; large enough to ignore
/// floating-point noise from the host.
const BPM_SETTLE_THRESHOLD: f64 = 0.01;

/// Minimum BPM delta that triggers a BpmChanged resync gap in the MIDI clock
/// worker.  Larger than BPM_SETTLE_THRESHOLD so that tiny automation wobbles
/// don't cause audible MIDI-clock gaps — only meaningful tempo steps do.
const BPM_MIDI_THRESHOLD: f64 = 0.5;


// ─── Plugin struct ───────────────────────────────────────────────────────────

pub struct EtherTap {
    params: Arc<EtherTapParams>,

    // ── Lock-free cross-thread channels ──────────────────────────────────
    cmd_tx: crossbeam_channel::Sender<NetworkCommand>,
    status_rx: crossbeam_channel::Receiver<NetworkStatus>,
    /// Sends clock tokens to the MIDI clock worker (Tick or BpmChanged).
    midi_clock_tx: crossbeam_channel::Sender<midi_clock::ClockMsg>,
    /// Sends `Option<String>` to the MIDI worker when the output device changes.
    device_change_tx: crossbeam_channel::Sender<Option<String>>,

    // ── Shared atomics (audio writes, editor reads) ───────────────────────
    conn_status: Arc<AtomicBool>,
    /// Millisecond timestamp of the last TX packet (drives the TX LED).
    tx_activity_ts: Arc<AtomicU64>,
    /// Millisecond timestamp of the last RX packet (drives the RX LED).
    rx_activity_ts: Arc<AtomicU64>,
    /// Millisecond timestamp of the last MIDI clock event sent (drives the CK LED).
    midi_clock_activity_ts: Arc<AtomicU64>,
    /// True when the worker has an open connection to the selected physical output.
    midi_bridge_connected: Arc<AtomicBool>,
    /// True while the MIDI worker is attempting to reconnect to the selected physical output.
    midi_bridge_connecting: Arc<AtomicBool>,
    /// Timing statistics from the MIDI clock worker (updated once per beat).
    midi_clock_stats: Arc<midi_clock::AtomicClockStats>,
    /// Polled hardware delay float stored as u32 bits (f32::from_bits).
    hardware_float: Arc<AtomicU32>,
    /// Current host BPM stored as u32 bits (f32::from_bits).
    host_bpm: Arc<AtomicU32>,
    /// Receiver for MIDI device list changes (from midi_watcher).
    midi_device_rx: Arc<crossbeam_channel::Receiver<Vec<String>>>,
    /// Set by the UI button; swap()-cleared by the audio thread to trigger an
    /// immediate Hard Reset without relying on the unimplemented param setter.
    force_sync_trigger: Arc<AtomicBool>,
    /// Set by the Rate Sync "Force" button — fires an immediate rate-only sync.
    force_rate_trigger: Arc<AtomicBool>,
    /// Bitmask: bit n set ↔ slot (n+1) is BPM-compatible. Written by network worker.
    compatible_slots:  Arc<AtomicU8>,
    /// Bitmask: bit n set ↔ slot (n+1) is occupied. Written by network worker.
    occupied_slots:    Arc<AtomicU8>,
    /// Raw effect type ID for each slot (index = slot-1). i32::MIN = not yet queried.
    slot_types:        Arc<[AtomicI32; 8]>,
    all_slots_mode:    Arc<AtomicBool>,
    scan_targets:      Arc<Mutex<Vec<network::DeviceInfo>>>,
    /// Millisecond timestamp of the last completed TargetsFound scan result.
    scan_completed_ts: Arc<AtomicU64>,
    /// Name and model of the currently connected device, from /info responses.
    connected_device:  Arc<Mutex<(String, String)>>,

    // ── Audio configuration ───────────────────────────────────────────────
    sample_rate: f32,

    // ── MIDI watcher shutdown ─────────────────────────────────────────────
    /// Set to true in Drop to signal the non-macOS polling thread to exit.
    midi_watcher_shutdown: Arc<AtomicBool>,

    // ── Scan generation counter ───────────────────────────────────────────
    /// Incremented by the editor when it clears scan results for a new scan.
    /// Background scan threads discard their results if this changes.
    scan_generation: Arc<AtomicU64>,

    // ── BPM settle state machine ──────────────────────────────────────────
    last_bpm: f64,
    bpm_change_ts: u64,
    bpm_is_settling: bool,
    /// BPM captured when the settle timer was (re-)armed.  Compared at settle
    /// completion to detect slow BPM drift that would slip under the 0.01 BPM
    /// change threshold and cause a sync at the wrong tempo.
    bpm_at_settle_start: f64,

    // ── Quantised auto Hard Reset ─────────────────────────────────────────
    /// Waiting to fire Hard Reset at `hr_target_beat`.
    hr_pending: bool,
    hr_target_beat: f64,

    // ── Continuous sync beat tracking ─────────────────────────────────────
    last_pos_beats: f64,
    /// Integer beat index (pos_beats.floor() as i64) from the previous buffer.
    /// Used for beat-crossing detection without floating-point epsilon hacks.
    last_beat_idx: i64,
    /// Bar number from the previous buffer; -1 when transport is stopped.
    /// Used to detect bar crossings and loop repeats (PS1).
    last_bar_number: i32,
    /// Time signature from the previous buffer (numerator, denominator).
    /// Cached to detect mid-song time-sig changes (PS3).
    last_time_sig: (i32, i32),

    // ── MIDI clock LED pulse counter ──────────────────────────────────────
    /// Counts outgoing 0xF8 pulses; resets at 24 so the LED blinks once/beat.
    midi_clock_pulse_count: u8,
    /// Last BPM at which a stable clock was running; used to detect changes
    /// large enough to warrant a resync gap (> 0.5 BPM).
    last_clock_bpm: f64,
    last_bpm_changed_at: Option<f64>,
    /// Whether transport was playing in the previous process() call.
    /// Used to detect the not-playing → playing edge for TransportStart.
    prev_playing: bool,
    /// Fractional-sample accumulator for standalone MIDI clock (no DAW transport).
    /// Stored as f64 to preserve sub-sample remainder and avoid truncation drift.
    standalone_tick_samples: f64,
    /// Cumulative count of MIDI clock messages dropped due to a stalled worker.
    /// Written in process() (atomic increment), drained by the editor on each frame.
    midi_clock_drop_count: Arc<AtomicU32>,

    // ── Reconnect auto-sync ───────────────────────────────────────────────
    /// Set when connection is established; cleared once SlotScan arrives and
    /// we dispatch the current BPM to all newly-detected compatible slots.
    reconnect_sync_pending: bool,

    // ── Force-sync rising-edge detection (for VST automation) ─────────────
    prev_force_sync:       bool,
    prev_connect_to_last:  bool,
    prev_disconnect_param: bool,
    prev_force_sync_rate:  bool,
    prev_force_sync_phase: bool,
    prev_force_sync_both:  bool,

    // ── Host param shadow (avoids redundant set_parameter calls) ─────────
    /// Last value written to `params.is_connected` from the audio thread.
    last_conn_status: bool,
    /// Last value written to `params.is_matched` from the audio thread.
    last_matched_status: bool,

    // ── OnChange retry ────────────────────────────────────────────────────
    /// True while we're waiting for hardware to confirm the tempo.
    on_change_retry_pending: bool,
    /// The BPM that was sent — compared against hardware readback.
    on_change_retry_bpm: f64,
    /// True when the pending retry should be a Hard Reset (phase+rate).
    on_change_retry_hard_reset: bool,
    /// Timestamp of the last retry dispatch (ms since epoch).
    on_change_last_retry_ms: u64,
}

impl Default for EtherTap {
    fn default() -> Self {
        let params = Arc::new(EtherTapParams::default());

        let hardware_float = Arc::new(AtomicU32::new(0u32));
        let host_bpm = Arc::new(AtomicU32::new(0u32));
        let force_sync_trigger = Arc::new(AtomicBool::new(false));
        let force_rate_trigger = Arc::new(AtomicBool::new(false));
        let conn_status = Arc::new(AtomicBool::new(false));
        let tx_activity_ts = Arc::new(AtomicU64::new(0));
        let rx_activity_ts = Arc::new(AtomicU64::new(0));
        let midi_clock_activity_ts = Arc::new(AtomicU64::new(0));
        let compatible_slots = Arc::new(AtomicU8::new(0));
        let occupied_slots   = Arc::new(AtomicU8::new(0));
        let slot_types: Arc<[AtomicI32; 8]> =
            Arc::new(std::array::from_fn(|_| AtomicI32::new(i32::MIN)));
        let all_slots_mode   = Arc::new(AtomicBool::new(true));
        let scan_targets      = Arc::new(Mutex::new(Vec::<network::DeviceInfo>::new()));
        let scan_completed_ts = Arc::new(AtomicU64::new(0));
        let connected_device  = Arc::new(Mutex::new((String::new(), String::new())));
        let scan_generation   = Arc::new(AtomicU64::new(0));

        let (cmd_tx, cmd_rx) = crossbeam_channel::bounded::<NetworkCommand>(64);
        let (status_tx, status_rx) = crossbeam_channel::bounded::<NetworkStatus>(64);
        let (midi_clock_tx, midi_clock_rx) =
            crossbeam_channel::bounded::<midi_clock::ClockMsg>(256);
        let (device_change_tx, device_change_rx) =
            crossbeam_channel::bounded::<Option<String>>(16);
        let midi_bridge_connected  = Arc::new(AtomicBool::new(false));
        let midi_bridge_connecting = Arc::new(AtomicBool::new(false));
        let midi_clock_stats       = Arc::new(midi_clock::AtomicClockStats::default());

        // Spawn the MIDI device watcher BEFORE any midir::MidiOutput is created
        // (macOS: CoreMIDI notification client must be first).
        let midi_watch = midi_watcher::spawn();
        let midi_watcher_shutdown = midi_watch.shutdown.clone();
        let midi_device_rx = Arc::new(midi_watch.editor_rx);

        let worker = NetworkWorker::new(
            cmd_rx,
            status_tx,
            params.target_ip.clone(),
            params.target_port.clone(),
            params.fx_slot.clone(),
            network::WorkerShared {
                hardware_float_out: hardware_float.clone(),
                compatible_slots:   compatible_slots.clone(),
                occupied_slots:     occupied_slots.clone(),
                slot_types:         slot_types.clone(),
                scan_targets:       scan_targets.clone(),
                connected_device:   connected_device.clone(),
                scan_generation:    scan_generation.clone(),
            },
        );
        std::thread::Builder::new()
            .name("ethertap-net".into())
            .spawn(move || worker.run())
            .expect("failed to spawn network worker thread");

        let initial_device = params.midi_out_device.lock().clone();
        let initial_ppq = params.midi_clock_ppq.load(std::sync::atomic::Ordering::Relaxed);
        let midi_worker = midi_clock::MidiClockWorker::new(
            params.midi_clock_enabled.clone(),
            midi_clock_rx,
            device_change_rx,
            midi_watch.worker_rx,
            initial_device,
            midi_bridge_connected.clone(),
            midi_bridge_connecting.clone(),
            midi_clock_stats.clone(),
            initial_ppq,
        );
        std::thread::Builder::new()
            .name("ethertap-midi-clk".into())
            .spawn(move || midi_worker.run())
            .expect("failed to spawn MIDI clock worker thread");

        Self {
            params,
            cmd_tx,
            status_rx,
            midi_clock_tx,
            device_change_tx,
            conn_status,
            tx_activity_ts,
            rx_activity_ts,
            midi_clock_activity_ts,
            midi_bridge_connected,
            midi_bridge_connecting,
            midi_clock_stats,
            hardware_float,
            host_bpm,
            force_sync_trigger,
            force_rate_trigger,
            midi_device_rx,
            midi_watcher_shutdown,
            compatible_slots,
            occupied_slots,
            slot_types,
            all_slots_mode,
            scan_targets,
            scan_completed_ts,
            connected_device,
            scan_generation,
            sample_rate: 44100.0,
            reconnect_sync_pending: false,
            last_conn_status: false,
            last_matched_status: false,
            last_bpm: 0.0,
            bpm_change_ts: 0,
            bpm_is_settling: false,
            bpm_at_settle_start: 0.0,
            hr_pending: false,
            hr_target_beat: 0.0,
            last_pos_beats: 0.0,
            last_beat_idx: -1,
            last_bar_number: -1,
            last_time_sig: (4, 4),
            midi_clock_pulse_count: 0,
            last_clock_bpm: 0.0,
            last_bpm_changed_at: None,
            prev_playing:   false,
            prev_force_sync:       false,
            prev_connect_to_last:  false,
            prev_disconnect_param: false,
            prev_force_sync_rate:  false,
            prev_force_sync_phase: false,
            prev_force_sync_both:  false,
            on_change_retry_pending:    false,
            on_change_retry_bpm:        0.0,
            on_change_retry_hard_reset: false,
            on_change_last_retry_ms:    0,
            standalone_tick_samples:    0.0,
            midi_clock_drop_count:      Arc::new(AtomicU32::new(0)),
        }
    }
}

// ─── Plugin impl ─────────────────────────────────────────────────────────────

impl Plugin for EtherTap {
    const NAME: &'static str = "EtherTap";
    const VENDOR: &'static str = "EtherTap Project";
    const URL: &'static str = "";
    const EMAIL: &'static str = "";
    const VERSION: &'static str = env!("CARGO_PKG_VERSION");

    const AUDIO_IO_LAYOUTS: &'static [AudioIOLayout] = &[AudioIOLayout {
        main_input_channels: None,
        main_output_channels: None,
        ..AudioIOLayout::const_default()
    }];

    const MIDI_INPUT: MidiConfig = MidiConfig::None;
    const MIDI_OUTPUT: MidiConfig = MidiConfig::None;
    const SAMPLE_ACCURATE_AUTOMATION: bool = true;

    type SysExMessage = ();
    type BackgroundTask = ();

    fn params(&self) -> Arc<dyn Params> {
        self.params.clone()
    }

    fn initialize(
        &mut self,
        layout: &AudioIOLayout,
        config: &BufferConfig,
        _context: &mut impl InitContext<Self>,
    ) -> bool {
        self.sample_rate = config.sample_rate;
        #[cfg(feature = "standalone")]
        {
            let ins = layout.main_input_channels.map_or(0, |n| n.get());
            let outs = layout.main_output_channels.map_or(0, |n| n.get());
            nih_log!("EtherTap standalone — audio I/O: {ins} in / {outs} out");
        }
        #[cfg(not(feature = "standalone"))]
        let _ = layout;

        let ip = self.params.target_ip.lock().clone();
        let port = *self.params.target_port.lock();
        let _ = self.cmd_tx.try_send(NetworkCommand::UpdateTarget { ip, port });
        let _ = self.cmd_tx.try_send(NetworkCommand::AuditSlots);
        true
    }

    fn process(
        &mut self,
        buffer: &mut Buffer,
        _aux: &mut AuxiliaryBuffers,
        context: &mut impl ProcessContext<Self>,
    ) -> ProcessStatus {
        // Cache the current timestamp once — avoids repeated syscalls throughout
        // this function.  A single call is accurate enough for all timing checks.
        let now = now_ms();

        // ── 1. Drain network status (lock-free, allocation-free) ─────────
        // All NetworkStatus variants are Copy/allocation-free.  Payload data
        // (slot lists, scan results, device identity) was written directly to
        // shared mutexes by the network worker before sending the sentinel.
        while let Ok(status) = self.status_rx.try_recv() {
            match status {
                NetworkStatus::Connected => self.conn_status.store(true, Ordering::Release),
                NetworkStatus::Disconnected => self.conn_status.store(false, Ordering::Release),
                NetworkStatus::ActivityPulse => {
                    self.tx_activity_ts.store(now, Ordering::Relaxed);
                }
                NetworkStatus::RxPulse => {
                    self.rx_activity_ts.store(now, Ordering::Relaxed);
                }
                // hardware_float is written directly by the worker; this variant
                // exists only to keep the RX LED active after telemetry polls.
                NetworkStatus::DelayReadback(_) => {}
                NetworkStatus::SlotScanDone => {
                    // Slot data already written to compatible_slots/occupied_slots/
                    // slot_types by the worker.  Fire the reconnect auto-sync only
                    // when BPM has settled — dispatching during a BPM transition
                    // sends a stale pre-settle BPM.  The settle handler clears
                    // reconnect_sync_pending when it takes over.
                    if self.reconnect_sync_pending
                        && self.last_bpm > 0.0
                        && !self.bpm_is_settling
                    {
                        self.reconnect_sync_pending = false;
                        self.dispatch(self.last_bpm, false);
                    }
                }
                NetworkStatus::ScanDone => {
                    // Scan results already merged into scan_targets by the worker.
                    self.scan_completed_ts.store(now, Ordering::Relaxed);
                }
            }
        }

        // ── 2. Sample transport ───────────────────────────────────────────
        let transport = context.transport();
        let bpm = transport.tempo.unwrap_or(120.0).max(10.0);
        let pos_beats_raw = transport.pos_beats(); // None when host doesn't report position
        let pos_beats = pos_beats_raw.unwrap_or(0.0);
        let playing = transport.playing;

        // Bar / time-sig metadata for PS1-PS3 phase sync improvements.
        let bar_number    = transport.bar_number().unwrap_or(-1);
        let ts_num        = transport.time_sig_numerator.unwrap_or(4);
        let ts_den        = transport.time_sig_denominator.unwrap_or(4);
        // Quarter-note length of one bar; e.g. 4/4 → 4.0 qn, 3/4 → 3.0 qn, 7/8 → 3.5 qn.
        let bar_len_qn    = ts_num as f64 * 4.0 / ts_den as f64;
        // Start of the next bar in quarter notes (used for PS2 HR quantisation).
        let next_bar_beat = transport.bar_start_pos_beats().unwrap_or(pos_beats.floor())
                            + bar_len_qn;
        // Active loop range, if the host has one enabled (used for PS4).
        let loop_range    = transport.loop_range_beats();
        // Capture prev_playing now — before any updates — so step 7 can detect
        // the stopped→playing edge correctly.  self.prev_playing is written at
        // the very end of process() so it always reflects the previous call.
        let was_playing = self.prev_playing;

        // ── 3. Publish host BPM for the editor ───────────────────────────
        self.host_bpm.store((bpm as f32).to_bits(), Ordering::Release);

        // ── 3b. Update read-only host params from audio thread ───────────
        // This keeps is_connected / is_matched current even when the GUI is
        // closed; context.set_parameter() updates the internal atomic and, for
        // VST3, schedules a host notification via the GUI event loop.
        let connected = self.conn_status.load(Ordering::Acquire);
        let hw_float  = f32::from_bits(self.hardware_float.load(Ordering::Acquire));
        let in_sync   = connected
            && hw_float > 0.0001
            && (osc::bpm_to_float(bpm) - hw_float).abs() < 0.001;
        if connected != self.last_conn_status {
            context.set_parameter(&self.params.is_connected, connected);
            if connected {
                // Just (re)connected: scan slots and arm the auto-sync.
                // This mirrors the manual "Query → All" flow in the editor.
                self.all_slots_mode.store(true, Ordering::Release);
                self.reconnect_sync_pending = true;
                let _ = self.cmd_tx.try_send(NetworkCommand::AuditSlots);
            } else {
                // Disconnected: clear any pending sync state so stale retries
                // don't fire immediately on the next reconnect.
                self.on_change_retry_pending = false;
                self.hr_pending = false;
                self.hr_target_beat = 0.0;
                self.bpm_is_settling = false;
                self.bpm_change_ts = 0;
                self.bpm_at_settle_start = 0.0;
            }
            self.last_conn_status = connected;
        }
        if in_sync != self.last_matched_status {
            context.set_parameter(&self.params.is_matched, in_sync);
            self.last_matched_status = in_sync;
        }

        // ── 3c. Backward seek / loop detection ───────────────────────────
        // If the host jumps position backward, the beat-crossing check and
        // settle timer become stale.  Reset them and re-arm a Hard Reset:
        // a loop-back is exactly when we want phase sync, not cancel it.
        //
        // PS4: distinguish loop repeats from scrubs using loop_range_beats().
        // Loop repeat → land at loop start, snap HR to next bar for tighter
        // musical alignment.  Scrub → snap HR to next beat only.
        if playing && pos_beats < self.last_pos_beats - 0.5 {
            self.last_pos_beats          = pos_beats - 1.0;
            self.last_beat_idx           = pos_beats.floor() as i64 - 1;
            self.last_bar_number         = bar_number - 1;
            self.bpm_is_settling         = false;
            self.on_change_retry_pending = false;

            let is_loop_repeat = loop_range
                .map(|(start, end)| pos_beats >= start - 0.5 && pos_beats <= start + 0.5
                                 && self.last_pos_beats + 1.0 >= end - 0.5)
                .unwrap_or(false);

            self.hr_pending     = true;
            self.hr_target_beat = if is_loop_repeat { next_bar_beat } else { pos_beats.ceil() };
        }

        // ── 4. BPM settle detection ("On Change" modes) ──────────────────
        if self.last_bpm > 0.0 && (bpm - self.last_bpm).abs() > BPM_SETTLE_THRESHOLD {
            // BPM just changed — restart settle timer and cancel any retry.
            self.bpm_change_ts = now;
            self.bpm_is_settling = true;
            self.bpm_at_settle_start = bpm;
            self.on_change_retry_pending = false;
            self.hr_pending = false;
            self.hr_target_beat = 0.0;
        } else if self.bpm_is_settling {
            let elapsed = now.saturating_sub(self.bpm_change_ts);
            if elapsed >= SETTLE_MS {
                // Guard: if BPM drifted past 0.01 since settle started (slow
                // automation), re-arm rather than dispatching at the wrong tempo.
                if (bpm - self.bpm_at_settle_start).abs() > BPM_SETTLE_THRESHOLD {
                    self.bpm_change_ts = now;
                    self.bpm_at_settle_start = bpm;
                    // keep bpm_is_settling = true, wait another SETTLE_MS
                } else {
                    self.bpm_is_settling = false;
                    // Settle supersedes any deferred reconnect sync — the correct
                    // settled BPM will be dispatched below (On Change mode) or the
                    // reconnect sync should not fire at all (Manual mode).
                    self.reconnect_sync_pending = false;
                    if playing {
                        let phase_mode = self.params.phase_sync_mode.value();
                        let rate_mode = self.params.rate_sync_mode.value();
                        if phase_mode == SyncMode::OnChange {
                            // PS2: Quantise the Hard Reset to the next bar boundary
                            // (musically stronger than the next beat).  next_bar_beat
                            // is always strictly after pos_beats because bar_start +
                            // bar_len_qn > pos_beats (we're inside the current bar).
                            self.hr_target_beat = next_bar_beat;
                            self.hr_pending = true;
                            // Arm retry (hard reset will be dispatched at hr_target_beat).
                            self.on_change_retry_pending    = true;
                            self.on_change_retry_bpm        = bpm;
                            self.on_change_retry_hard_reset = true;
                            self.on_change_last_retry_ms    = now;
                        } else if rate_mode == SyncMode::OnChange {
                            self.dispatch(bpm, false);
                            self.on_change_retry_pending    = true;
                            self.on_change_retry_bpm        = bpm;
                            self.on_change_retry_hard_reset = false;
                            self.on_change_last_retry_ms    = now;
                        }
                    }
                }
            }
        }

        // ── 5. Quantised Hard Reset gate ──────────────────────────────────
        if self.hr_pending && playing && pos_beats >= self.hr_target_beat {
            self.dispatch(bpm, true);
            self.hr_pending = false;
            // Reset the retry timer so we wait a full 2 s after the hard reset
            // before checking whether the hardware has caught up.
            self.on_change_last_retry_ms = now;
        }

        // ── 5b. OnChange retry — resend every 2 s until hardware confirms ─
        // Only retries when connected; stops automatically once in_sync.
        if self.on_change_retry_pending && self.conn_status.load(Ordering::Acquire) {
            let hw_float = f32::from_bits(self.hardware_float.load(Ordering::Acquire));
            let target_float = osc::bpm_to_float(self.on_change_retry_bpm);
            let matched = hw_float > 0.0001 && (target_float - hw_float).abs() < 0.001;
            if matched {
                self.on_change_retry_pending = false;
            } else if now.saturating_sub(self.on_change_last_retry_ms) >= 2_000 {
                // Skip a retry if there is already a quantised Hard Reset queued —
                // it will fire at the next beat boundary and acts as the retry.
                if !self.hr_pending {
                    self.dispatch(self.on_change_retry_bpm, self.on_change_retry_hard_reset);
                }
                self.on_change_last_retry_ms = now;
            }
        }

        // ── 6. Continuous sync (fires on every beat crossing) ────────────
        let beat_idx = pos_beats.floor() as i64;
        if playing && beat_idx > self.last_beat_idx {
            let phase_mode = self.params.phase_sync_mode.value();
            let rate_mode = self.params.rate_sync_mode.value();
            if phase_mode == SyncMode::Continuous {
                self.dispatch(bpm, true); // continuous phase reset
            } else if rate_mode == SyncMode::Continuous {
                self.dispatch(bpm, false); // continuous rate sync only
            }
        }

        // ── 6b. PS1: Bar-crossing detection ──────────────────────────────
        // bar_number advances by 1 each bar; any other change means a jump
        // (loop repeat, song-position rewind) that wasn't caught above because
        // pos_beats didn't drop by more than 0.5 (e.g., loop from bar 5 → bar 1).
        if playing && bar_number >= 0 && bar_number != self.last_bar_number
            && bar_number != self.last_bar_number + 1
        {
            // Non-sequential bar jump — re-arm a bar-boundary Hard Reset.
            if !self.hr_pending {
                self.hr_pending     = true;
                self.hr_target_beat = next_bar_beat;
            }
        }

        // ── 6c. PS3: Time signature change detection ──────────────────────
        // A time-sig change invalidates bar-length math everywhere.  Reset bar
        // tracking so the next bar-crossing comparison starts fresh.
        let cur_time_sig = (ts_num, ts_den);
        if cur_time_sig != self.last_time_sig && self.last_time_sig != (4, 4) {
            // Re-arm a bar-boundary Hard Reset so the mixer re-aligns.
            if !self.hr_pending {
                self.hr_pending     = true;
                self.hr_target_beat = next_bar_beat;
            }
        }
        // Read PPQ once per buffer — drives LED blink cadence and tick generation.
        let midi_ppq = self.params.midi_clock_ppq.load(Ordering::Relaxed);

        if playing {
            self.last_pos_beats  = pos_beats;
            self.last_beat_idx   = beat_idx;
            self.last_bar_number = bar_number;
            self.last_time_sig   = cur_time_sig;
        } else {
            self.last_pos_beats  = -1.0; // reset so the first beat after play fires
            self.last_beat_idx   = -1;
            self.last_bar_number = -1;
            // Prime the counter so the first tick after resumption fires the LED
            // immediately, regardless of the PPQ setting.
            self.midi_clock_pulse_count = midi_ppq.saturating_sub(1);
        }

        // ── 7. MIDI clock output via CoreMIDI virtual source ─────────────────
        if self.params.midi_clock_enabled.load(Ordering::Relaxed) {
            let pos_beats_raw = transport.pos_beats(); // None = no DAW transport

            if let Some(beat_start) = pos_beats_raw {
                // DAW mode: follow transport position and play state.
                if !playing {
                    self.last_clock_bpm = 0.0;
                    if was_playing {
                        // Gate any ticks that were already queued for this
                        // buffer before we learned transport stopped.
                        if self.midi_clock_tx
                            .try_send(midi_clock::ClockMsg::Stop)
                            .is_err()
                        {
                            self.midi_clock_drop_count.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                }
                if playing && !was_playing {
                    if self.midi_clock_tx
                        .try_send(midi_clock::ClockMsg::TransportStart)
                        .is_err()
                    {
                        self.midi_clock_drop_count.fetch_add(1, Ordering::Relaxed);
                    }
                    // Prevent a spurious BpmChanged gap on the transition frame:
                    // if BPM was changed while stopped the BpmChanged check below
                    // would fire and immediately override the TransportStart
                    // (which promises no gap).  Seeding last_bpm_changed_at here
                    // means the threshold delta is measured from the current BPM.
                    self.last_bpm_changed_at = Some(bpm);
                }
                if playing {
                    let buf_len = buffer.samples();
                    if buf_len > 0 {
                        let ppq = midi_ppq as f64;
                        match self.last_bpm_changed_at {
                            None => {
                                self.last_bpm_changed_at = Some(bpm);
                            }
                            Some(prev) if (bpm - prev).abs() > BPM_MIDI_THRESHOLD => {
                                if self.midi_clock_tx
                                    .try_send(midi_clock::ClockMsg::BpmChanged { new_bpm: bpm })
                                    .is_ok()
                                {
                                    self.last_bpm_changed_at = Some(bpm);
                                } else {
                                    self.midi_clock_drop_count.fetch_add(1, Ordering::Relaxed);
                                    self.last_bpm_changed_at = Some(bpm);
                                }
                            }
                            Some(_) => {}
                        }
                        self.last_clock_bpm = bpm;

                        let samples_per_beat = transport.sample_rate as f64 * 60.0 / bpm;
                        let beats_per_sample = 1.0 / samples_per_beat;
                        let beat_end =
                            beat_start + buf_len as f64 * beats_per_sample;

                        let clock_start = (beat_start * ppq).ceil() as i64;
                        let clock_end   = (beat_end   * ppq).ceil() as i64;

                        let mut tick_drops = 0u32;
                        for k in clock_start..clock_end {
                            let on_beat = k % ppq as i64 == 0;
                            if self.midi_clock_tx
                                .try_send(midi_clock::ClockMsg::Tick { on_beat })
                                .is_err()
                            {
                                tick_drops += 1;
                            }
                            self.midi_clock_pulse_count += 1;
                            if self.midi_clock_pulse_count >= midi_ppq {
                                self.midi_clock_pulse_count = 0;
                                self.midi_clock_activity_ts
                                    .store(now, Ordering::Relaxed);
                            }
                        }
                        if tick_drops > 0 {
                            self.midi_clock_drop_count.fetch_add(tick_drops, Ordering::Relaxed);
                        }
                    }
                }
            } else {
                // Standalone (no DAW transport): derive tick interval from sample count
                // to avoid Instant::now() syscalls on the audio thread.
                let buf_len = buffer.samples();
                if buf_len > 0 {
                    let ppq = midi_ppq as f64;
                    let tick_interval_f = transport.sample_rate as f64 * 60.0 / bpm / ppq;
                    if tick_interval_f.is_normal() {
                        // On BPM change, rescale the accumulator to preserve phase.
                        // Resetting to 0 would cause the next tick to fire immediately
                        // regardless of where in the beat we were.
                        if (bpm - self.last_clock_bpm).abs() > BPM_SETTLE_THRESHOLD
                            && self.last_clock_bpm > 0.0
                        {
                            let old_interval = transport.sample_rate as f64
                                * 60.0 / self.last_clock_bpm / ppq;
                            if old_interval.is_normal() {
                                self.standalone_tick_samples =
                                    self.standalone_tick_samples * tick_interval_f / old_interval;
                            }
                        }
                        self.last_clock_bpm = bpm;
                        self.standalone_tick_samples += buf_len as f64;
                        while self.standalone_tick_samples >= tick_interval_f {
                            let on_beat = self.midi_clock_pulse_count == 0;
                            let _ = self.midi_clock_tx
                                .try_send(midi_clock::ClockMsg::Tick { on_beat });
                            self.standalone_tick_samples -= tick_interval_f;
                            self.midi_clock_pulse_count += 1;
                            if self.midi_clock_pulse_count >= midi_ppq {
                                self.midi_clock_pulse_count = 0;
                                self.midi_clock_activity_ts
                                    .store(now, Ordering::Relaxed);
                            }
                        }
                    }
                }
            }
        }

        // ── 8. Force triggers — param automation edges + UI atomics ─────────

        // Connection control via automation: send ConnectToLast so the network
        // worker reads ip/port itself — no String allocation on the audio thread.
        let connect_param = self.params.connect_to_last.value();
        if connect_param && !self.prev_connect_to_last {
            let _ = self.cmd_tx.try_send(NetworkCommand::ConnectToLast);
            let _ = self.cmd_tx.try_send(NetworkCommand::AuditSlots);
            self.all_slots_mode.store(true, Ordering::Release);
        }
        self.prev_connect_to_last = connect_param;

        let disconnect_param = self.params.disconnect.value();
        if disconnect_param && !self.prev_disconnect_param {
            let _ = self.cmd_tx.try_send(NetworkCommand::Disconnect);
        }
        self.prev_disconnect_param = disconnect_param;

        // Rate-only sync: new automation param + legacy UI atomic.
        let force_rate_param = self.params.force_sync_rate.value();
        let force_rate_trigger = self.force_rate_trigger.swap(false, Ordering::AcqRel);
        if (force_rate_param && !self.prev_force_sync_rate) || force_rate_trigger {
            self.dispatch(bpm, false);
        }
        self.prev_force_sync_rate = force_rate_param;

        // Phase (hard reset) sync: new automation params + legacy params + UI atomic.
        let force_phase_param = self.params.force_sync_phase.value();
        let force_both_param  = self.params.force_sync_both.value();
        let force_legacy_param = self.params.force_sync.value();
        let force_trigger = self.force_sync_trigger.swap(false, Ordering::AcqRel);
        if (force_phase_param && !self.prev_force_sync_phase)
            || (force_both_param  && !self.prev_force_sync_both)
            || (force_legacy_param && !self.prev_force_sync)
            || force_trigger
        {
            self.dispatch(bpm, true);
        }
        self.prev_force_sync_phase = force_phase_param;
        self.prev_force_sync_both  = force_both_param;
        self.prev_force_sync       = force_legacy_param;

        self.last_bpm = bpm;
        self.prev_playing = playing;

        ProcessStatus::Normal
    }

    fn editor(&mut self, _async_executor: AsyncExecutor<Self>) -> Option<Box<dyn Editor>> {
        let data = Arc::new(EditorData {
            params: self.params.clone(),
            conn_status: self.conn_status.clone(),
            tx_activity_ts: self.tx_activity_ts.clone(),
            rx_activity_ts: self.rx_activity_ts.clone(),
            midi_clock_activity_ts: self.midi_clock_activity_ts.clone(),
            hardware_float: self.hardware_float.clone(),
            host_bpm: self.host_bpm.clone(),
            force_sync_trigger: self.force_sync_trigger.clone(),
            force_rate_trigger: self.force_rate_trigger.clone(),
            midi_device_rx:    self.midi_device_rx.clone(),
            compatible_slots:  self.compatible_slots.clone(),
            occupied_slots:    self.occupied_slots.clone(),
            slot_types:        self.slot_types.clone(),
            all_slots_mode:    self.all_slots_mode.clone(),
            scan_targets:      self.scan_targets.clone(),
            scan_completed_ts: self.scan_completed_ts.clone(),
            connected_device:  self.connected_device.clone(),
            scan_generation:   self.scan_generation.clone(),
            cmd_tx: self.cmd_tx.clone(),
            device_change_tx: self.device_change_tx.clone(),
            midi_bridge_connected: self.midi_bridge_connected.clone(),
            midi_bridge_connecting: self.midi_bridge_connecting.clone(),
            midi_clock_stats: self.midi_clock_stats.clone(),
            midi_clock_drop_count: self.midi_clock_drop_count.clone(),
        });
        editor::create(data)
    }
}

impl Drop for EtherTap {
    fn drop(&mut self) {
        self.midi_watcher_shutdown.store(true, Ordering::Release);
    }
}

// ─── Private helpers ─────────────────────────────────────────────────────────

/// Map an X32 effect type ID to its bit position in the `fx_type_filter` bitmask.
/// Returns `None` for types that are not filterable (non-delay effects).
fn fx_type_to_bit(type_id: i32) -> Option<u8> {
    match type_id {
        10 => Some(0), // DLY
        11 => Some(1), // 3TAP
        12 => Some(2), // 4TAP
        21 => Some(3), // D/RV
        24 => Some(4), // D/CR
        25 => Some(5), // D/FL
        26 => Some(6), // MODD
        _  => None,
    }
}

impl EtherTap {
    /// Dispatch a sync command.  `hard_reset = true` → `HardReset`, else `SyncNow`.
    ///
    /// When "all slots" mode is active, every compatible slot whose effect type
    /// is enabled in `params.fx_type_filter` receives the command; falls back to
    /// the single selected slot when no compatible slots are known yet.
    fn dispatch(&self, bpm: f64, hard_reset: bool) {
        // Build slot list into a fixed-size stack array — no heap allocation.
        let mut slots = [None::<u8>; 8];
        let mut n = 0usize;

        if self.all_slots_mode.load(Ordering::Acquire) {
            // All four reads are lock-free atomic loads — no mutex on the audio thread.
            let fallback_slot = self.params.fx_slot.load(Ordering::Relaxed);
            let filter        = self.params.fx_type_filter.load(Ordering::Relaxed);
            // Decode compatible_slots bitmask: bit n → slot (n+1).
            let compat_mask   = self.compatible_slots.load(Ordering::Relaxed);
            if compat_mask == 0 {
                slots[0] = Some(fallback_slot);
                n = 1;
            } else {
                for bit in 0..8u8 {
                    if n >= slots.len() { break; }
                    if compat_mask & (1 << bit) == 0 { continue; }
                    let slot = bit + 1;
                    let raw_type = self.slot_types[bit as usize].load(Ordering::Relaxed);
                    let include = if raw_type == i32::MIN {
                        true // not yet audited: include
                    } else {
                        match fx_type_to_bit(raw_type) {
                            Some(b) => (filter >> b) & 1 == 1,
                            None    => true, // unknown type: include
                        }
                    };
                    if include {
                        slots[n] = Some(slot);
                        n += 1;
                    }
                }
            }
        } else {
            slots[0] = Some(self.params.fx_slot.load(Ordering::Relaxed));
            n = 1;
        }

        if hard_reset {
            let _ = self.cmd_tx.try_send(NetworkCommand::HardResetBatch { slots, bpm });
        } else {
            for slot in slots[..n].iter().filter_map(|s| *s) {
                let _ = self.cmd_tx.try_send(NetworkCommand::SyncNow { slot, bpm });
            }
        }
    }
}

// ─── VST3 export ─────────────────────────────────────────────────────────────

impl Vst3Plugin for EtherTap {
    const VST3_CLASS_ID: [u8; 16] = *b"EtherTapOSCBridg";
    const VST3_SUBCATEGORIES: &'static [Vst3SubCategory] =
        &[Vst3SubCategory::Fx, Vst3SubCategory::Tools];
}

nih_export_vst3!(EtherTap);

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::sync::atomic::Ordering;

    use super::*;

    // ── fx_type_to_bit tests ──────────────────────────────────────────────

    #[test]
    fn fx_type_to_bit_all_known_types() {
        let cases = [
            (10, Some(0), "DLY"),
            (11, Some(1), "3TAP"),
            (12, Some(2), "4TAP"),
            (21, Some(3), "D/RV"),
            (24, Some(4), "D/CR"),
            (25, Some(5), "D/FL"),
            (26, Some(6), "MODD"),
        ];
        for (type_id, expected_bit, name) in cases {
            assert_eq!(fx_type_to_bit(type_id), expected_bit, "{name} ({type_id})");
        }
    }

    #[test]
    fn fx_type_to_bit_non_delay_returns_none() {
        for id in [0, 1, 3, 99, -1] {
            assert_eq!(fx_type_to_bit(id), None, "type_id={id}");
        }
    }

    #[test]
    fn fx_type_to_bit_covers_all_bpm_compatible() {
        for type_id in [10, 11, 12, 21, 24, 25, 26] {
            assert!(fx_type_to_bit(type_id).is_some(),
                "{type_id} is BPM-compatible but no bit assigned");
        }
    }

    // ── DAW mock (MockProcessContext) tests ─────────────────────────────────

    struct MockProcessContext {
        transport: Transport,
    }

    impl MockProcessContext {
        fn new(bpm: f64, playing: bool) -> Self {
            let mut transport = Transport::new(44100.0);
            transport.tempo = Some(bpm);
            transport.playing = playing;
            Self { transport }
        }
    }

    impl ProcessContext<EtherTap> for MockProcessContext {
        fn plugin_api(&self) -> nih_plug::context::PluginApi {
            nih_plug::context::PluginApi::Vst3
        }
        fn execute_background(&self, _task: ()) {}
        fn execute_gui(&self, _task: ()) {}
        fn transport(&self) -> &Transport { &self.transport }
        fn next_event(&mut self) -> Option<PluginNoteEvent<EtherTap>> { None }
        fn send_event(&mut self, _event: PluginNoteEvent<EtherTap>) {}
        fn set_latency_samples(&self, _samples: u32) {}
        fn set_current_voice_capacity(&self, _capacity: u32) {}
    }

    fn make_buffer() -> Buffer<'static> { Buffer::default() }

    fn make_aux() -> AuxiliaryBuffers<'static> {
        AuxiliaryBuffers { inputs: &mut [], outputs: &mut [] }
    }

    #[test]
    fn process_empty_buffer_does_not_panic() {
        let mut plugin = EtherTap::default();
        let mut ctx = MockProcessContext::new(120.0, false);
        let mut buffer = make_buffer();
        let mut aux = make_aux();
        let status = plugin.process(&mut buffer, &mut aux, &mut ctx);
        assert_eq!(status, ProcessStatus::Normal);
    }

    #[test]
    fn process_publishes_host_bpm() {
        let mut plugin = EtherTap::default();
        let mut ctx = MockProcessContext::new(120.0, false);
        let mut buffer = make_buffer();
        let mut aux = make_aux();
        plugin.process(&mut buffer, &mut aux, &mut ctx);
        let published = f32::from_bits(plugin.host_bpm.load(Ordering::Acquire));
        assert!(
            (published - 120.0).abs() < 0.01,
            "expected host_bpm ≈ 120, got {published}",
        );
    }

    #[test]
    fn read_only_params_update_on_process() {
        let mut plugin = EtherTap::default();
        let mut ctx = MockProcessContext::new(120.0, false);
        let mut buffer = make_buffer();
        let mut aux = make_aux();

        assert!(!plugin.params.is_connected.value());
        assert!(!plugin.params.is_matched.value());

        plugin.process(&mut buffer, &mut aux, &mut ctx);
        assert!(!plugin.params.is_connected.value());
        assert!(!plugin.params.is_matched.value());

        plugin.conn_status.store(true, Ordering::Release);
        let hw_val = (20.0_f64 / 120.0_f64) as f32;
        plugin.hardware_float.store(hw_val.to_bits(), Ordering::Release);
        plugin.process(&mut buffer, &mut aux, &mut ctx);
        assert!(plugin.params.is_connected.value());
        assert!(plugin.params.is_matched.value());

        plugin.conn_status.store(false, Ordering::Release);
        plugin.process(&mut buffer, &mut aux, &mut ctx);
        assert!(!plugin.params.is_connected.value());
        assert!(!plugin.params.is_matched.value());
    }

    // ── Persistence round-trip ─────────────────────────────────────────────

    #[test]
    fn persist_fields_roundtrip() {
        let params = EtherTapParams::default();

        *params.target_ip.lock() = "10.0.0.50".to_owned();
        let json = serde_json::to_string(&*params.target_ip.lock()).unwrap();
        assert_eq!(serde_json::from_str::<String>(&json).unwrap(), "10.0.0.50");

        *params.target_port.lock() = 10024u16;
        let json = serde_json::to_string(&*params.target_port.lock()).unwrap();
        assert_eq!(serde_json::from_str::<u16>(&json).unwrap(), 10024);

        params.fx_slot.store(4u8, Ordering::Relaxed);
        let json = serde_json::to_string(&params.fx_slot.load(Ordering::Relaxed)).unwrap();
        assert_eq!(serde_json::from_str::<u8>(&json).unwrap(), 4);

        params.fx_type_filter.store(0b000_0011u32, Ordering::Relaxed);
        let json = serde_json::to_string(&params.fx_type_filter.load(Ordering::Relaxed)).unwrap();
        assert_eq!(serde_json::from_str::<u32>(&json).unwrap(), 0b000_0011u32);

        params.midi_clock_enabled.store(false, Ordering::Relaxed);
        let json = serde_json::to_string(&params.midi_clock_enabled.load(Ordering::Relaxed)).unwrap();
        assert_eq!(serde_json::from_str::<bool>(&json).unwrap(), false);

        params.midi_clock_ppq.store(48u8, Ordering::Relaxed);
        let json = serde_json::to_string(&params.midi_clock_ppq.load(Ordering::Relaxed)).unwrap();
        assert_eq!(serde_json::from_str::<u8>(&json).unwrap(), 48);

        let device = Some("Midi Through Port-0".to_owned());
        *params.midi_out_device.lock() = device;
        let json = serde_json::to_string(&*params.midi_out_device.lock()).unwrap();
        assert_eq!(
            serde_json::from_str::<Option<String>>(&json).unwrap(),
            Some("Midi Through Port-0".to_owned())
        );

        *params.midi_out_device.lock() = None;
        let json = serde_json::to_string(&*params.midi_out_device.lock()).unwrap();
        assert_eq!(serde_json::from_str::<Option<String>>(&json).unwrap(), None);
    }
}
