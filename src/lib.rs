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
    atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
    Arc,
};

use nih_plug::prelude::*;
use parking_lot::Mutex;

mod editor;
mod midi_clock;
pub mod network;
pub mod osc;
mod params;
#[cfg(feature = "standalone")]
mod mock;

use editor::EditorData;
use network::{NetworkCommand, NetworkStatus, NetworkWorker, now_ms};
use params::{EtherTapParams, SyncMode};

// ─── Settling constant ───────────────────────────────────────────────────────

/// A BPM change must be absent for this many milliseconds before "Sync on
/// Change" fires.  Gives the user time to finish dragging a tempo slider.
const SETTLE_MS: u64 = 500;

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
    /// Timing statistics from the MIDI clock worker (updated once per beat).
    midi_clock_stats: Arc<Mutex<midi_clock::ClockStats>>,
    /// Polled hardware delay float stored as u32 bits (f32::from_bits).
    hardware_float: Arc<AtomicU32>,
    /// Current host BPM stored as u32 bits (f32::from_bits).
    host_bpm: Arc<AtomicU32>,
    /// Set by the UI button; swap()-cleared by the audio thread to trigger an
    /// immediate Hard Reset without relying on the unimplemented param setter.
    force_sync_trigger: Arc<AtomicBool>,
    /// Set by the Rate Sync "Force" button — fires an immediate rate-only sync.
    force_rate_trigger: Arc<AtomicBool>,
    compatible_slots:  Arc<Mutex<Vec<u8>>>,
    occupied_slots:    Arc<Mutex<Vec<u8>>>,
    /// Raw effect type ID for each slot (index = slot-1, None = not yet queried).
    slot_types:        Arc<Mutex<[Option<i32>; 8]>>,
    all_slots_mode:    Arc<AtomicBool>,
    scan_targets:      Arc<Mutex<Vec<network::DeviceInfo>>>,
    /// Millisecond timestamp of the last completed TargetsFound scan result.
    scan_completed_ts: Arc<AtomicU64>,
    /// Name and model of the currently connected device, from /info responses.
    connected_device:  Arc<Mutex<(String, String)>>,

    // ── Audio configuration ───────────────────────────────────────────────
    sample_rate: f32,

    // ── BPM settle state machine ──────────────────────────────────────────
    last_bpm: f64,
    bpm_change_ts: u64,
    bpm_is_settling: bool,

    // ── Quantised auto Hard Reset ─────────────────────────────────────────
    /// Waiting to fire Hard Reset at `hr_target_beat`.
    hr_pending: bool,
    hr_target_beat: f64,

    // ── Continuous sync beat tracking ─────────────────────────────────────
    last_pos_beats: f64,

    // ── MIDI clock LED pulse counter ──────────────────────────────────────
    /// Counts outgoing 0xF8 pulses; resets at 24 so the LED blinks once/beat.
    midi_clock_pulse_count: u8,
    /// Last BPM at which a stable clock was running; used to detect changes
    /// large enough to warrant a resync gap (> 0.5 BPM).
    last_clock_bpm: f64,
    /// Whether transport was playing in the previous process() call.
    /// Used to detect the not-playing → playing edge for TransportStart.
    prev_playing: bool,

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
        let compatible_slots = Arc::new(Mutex::new(Vec::new()));
        let occupied_slots   = Arc::new(Mutex::new(Vec::<u8>::new()));
        let slot_types       = Arc::new(Mutex::new([None::<i32>; 8]));
        let all_slots_mode   = Arc::new(AtomicBool::new(true));
        let scan_targets      = Arc::new(Mutex::new(Vec::<network::DeviceInfo>::new()));
        let scan_completed_ts = Arc::new(AtomicU64::new(0));
        let connected_device  = Arc::new(Mutex::new((String::new(), String::new())));

        let (cmd_tx, cmd_rx) = crossbeam_channel::bounded::<NetworkCommand>(64);
        let (status_tx, status_rx) = crossbeam_channel::bounded::<NetworkStatus>(64);
        let (midi_clock_tx, midi_clock_rx) =
            crossbeam_channel::bounded::<midi_clock::ClockMsg>(256);
        let (device_change_tx, device_change_rx) =
            crossbeam_channel::bounded::<Option<String>>(16);
        let midi_bridge_connected = Arc::new(AtomicBool::new(false));
        let midi_clock_stats      = Arc::new(Mutex::new(midi_clock::ClockStats::default()));

        let worker = NetworkWorker::new(
            cmd_rx,
            status_tx,
            params.fx_slot.clone(),
            slot_types.clone(),
            hardware_float.clone(),
        );
        std::thread::Builder::new()
            .name("ethertap-net".into())
            .spawn(move || worker.run())
            .expect("failed to spawn network worker thread");

        let initial_device = params.midi_out_device.lock().clone();
        let midi_worker = midi_clock::MidiClockWorker::new(
            params.midi_clock_enabled.clone(),
            midi_clock_rx,
            device_change_rx,
            initial_device,
            midi_bridge_connected.clone(),
            midi_clock_stats.clone(),
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
            midi_clock_stats,
            hardware_float,
            host_bpm,
            force_sync_trigger,
            force_rate_trigger,
            compatible_slots,
            occupied_slots,
            slot_types,
            all_slots_mode,
            scan_targets,
            scan_completed_ts,
            connected_device,
            sample_rate: 44100.0,
            reconnect_sync_pending: false,
            last_conn_status: false,
            last_matched_status: false,
            last_bpm: 0.0,
            bpm_change_ts: 0,
            bpm_is_settling: false,
            hr_pending: false,
            hr_target_beat: 0.0,
            last_pos_beats: 0.0,
            midi_clock_pulse_count: 0,
            last_clock_bpm: 0.0,
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
        // ── 1. Drain network status (lock-free) ───────────────────────────
        while let Ok(status) = self.status_rx.try_recv() {
            match status {
                NetworkStatus::Connected => self.conn_status.store(true, Ordering::Relaxed),
                NetworkStatus::Disconnected => self.conn_status.store(false, Ordering::Relaxed),
                NetworkStatus::ActivityPulse => {
                    self.tx_activity_ts.store(now_ms(), Ordering::Relaxed);
                }
                NetworkStatus::RxPulse => {
                    self.rx_activity_ts.store(now_ms(), Ordering::Relaxed);
                }
                NetworkStatus::DelayReadback(f) => {
                    // hardware_float_out is written directly by the worker via Arc;
                    // the status message is used here only for the rx LED.
                    let _ = f; // value already in hardware_float via Arc<AtomicU32>
                }
                NetworkStatus::SlotScan { compatible, occupied, slot_types } => {
                    *self.compatible_slots.lock() = compatible;
                    *self.occupied_slots.lock()   = occupied;
                    *self.slot_types.lock()       = slot_types;
                    // Auto-sync on reconnect: dispatch current BPM once the
                    // freshly-scanned compatible slots are in place.
                    if self.reconnect_sync_pending && self.last_bpm > 0.0 {
                        self.reconnect_sync_pending = false;
                        self.dispatch(self.last_bpm, false);
                    }
                }
                NetworkStatus::TargetsFound(new_targets) => {
                    // Merge into the existing list so the scan panel
                    // updates in-place without blanking on each rescan.
                    let mut list = self.scan_targets.lock();
                    for dev in new_targets {
                        let has_id = !dev.name.is_empty() || !dev.model.is_empty();
                        let existing = if has_id {
                            list.iter_mut().find(|d| d.name == dev.name && d.model == dev.model)
                        } else {
                            list.iter_mut().find(|d| d.ip == dev.ip)
                        };
                        match existing {
                            Some(e) => *e = dev, // refresh latency / addrs
                            None    => list.push(dev),
                        }
                    }
                    self.scan_completed_ts.store(now_ms(), Ordering::Relaxed);
                }
                NetworkStatus::DeviceIdentified { name, model } => {
                    *self.connected_device.lock() = (name, model);
                }
            }
        }

        // ── 2. Sample transport ───────────────────────────────────────────
        let transport = context.transport();
        let bpm = transport.tempo.unwrap_or(120.0);
        let pos_beats_raw = transport.pos_beats(); // None when host doesn't report position
        let pos_beats = pos_beats_raw.unwrap_or(0.0);
        let playing = transport.playing;

        // ── 3. Publish host BPM for the editor ───────────────────────────
        self.host_bpm.store((bpm as f32).to_bits(), Ordering::Relaxed);

        // ── 3b. Update read-only host params from audio thread ───────────
        // This keeps is_connected / is_matched current even when the GUI is
        // closed; context.set_parameter() updates the internal atomic and, for
        // VST3, schedules a host notification via the GUI event loop.
        let connected = self.conn_status.load(Ordering::Relaxed);
        let hw_float  = f32::from_bits(self.hardware_float.load(Ordering::Relaxed));
        let in_sync   = connected
            && hw_float > 0.0001
            && (osc::bpm_to_float(bpm) - hw_float).abs() < 0.001;
        if connected != self.last_conn_status {
            context.set_parameter(&self.params.is_connected, connected);
            if connected {
                // Just (re)connected: scan slots and arm the auto-sync.
                // This mirrors the manual "Query → All" flow in the editor.
                self.all_slots_mode.store(true, Ordering::Relaxed);
                self.reconnect_sync_pending = true;
                let _ = self.cmd_tx.try_send(NetworkCommand::AuditSlots);
            }
            self.last_conn_status = connected;
        }
        if in_sync != self.last_matched_status {
            context.set_parameter(&self.params.is_matched, in_sync);
            self.last_matched_status = in_sync;
        }

        // ── 4. BPM settle detection ("On Change" modes) ──────────────────
        if self.last_bpm > 0.0 && (bpm - self.last_bpm).abs() > 0.01 {
            // BPM just changed — restart settle timer and cancel any retry.
            self.bpm_change_ts = now_ms();
            self.bpm_is_settling = true;
            self.on_change_retry_pending = false;
        } else if self.bpm_is_settling {
            let elapsed = now_ms().saturating_sub(self.bpm_change_ts);
            if elapsed >= SETTLE_MS {
                self.bpm_is_settling = false;
                if playing {
                    let phase_mode = self.params.phase_sync_mode.value();
                    let rate_mode = self.params.rate_sync_mode.value();
                    if phase_mode == SyncMode::OnChange {
                        // Quantise the Hard Reset to the next beat boundary.
                        let next = pos_beats.ceil();
                        self.hr_target_beat = if next > pos_beats { next } else { next + 1.0 };
                        self.hr_pending = true;
                        // Arm retry (hard reset will be dispatched at hr_target_beat).
                        self.on_change_retry_pending    = true;
                        self.on_change_retry_bpm        = bpm;
                        self.on_change_retry_hard_reset = true;
                        self.on_change_last_retry_ms    = now_ms();
                    } else if rate_mode == SyncMode::OnChange {
                        self.dispatch(bpm, false);
                        self.on_change_retry_pending    = true;
                        self.on_change_retry_bpm        = bpm;
                        self.on_change_retry_hard_reset = false;
                        self.on_change_last_retry_ms    = now_ms();
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
            self.on_change_last_retry_ms = now_ms();
        }

        // ── 5b. OnChange retry — resend every 2 s until hardware confirms ─
        // Only retries when connected; stops automatically once in_sync.
        if self.on_change_retry_pending && self.conn_status.load(Ordering::Relaxed) {
            let hw_float = f32::from_bits(self.hardware_float.load(Ordering::Relaxed));
            let target_float = osc::bpm_to_float(self.on_change_retry_bpm);
            let matched = hw_float > 0.0001 && (target_float - hw_float).abs() < 0.001;
            if matched {
                self.on_change_retry_pending = false;
            } else if now_ms().saturating_sub(self.on_change_last_retry_ms) >= 2_000 {
                // Skip a retry if there is already a quantised Hard Reset queued —
                // it will fire at the next beat boundary and acts as the retry.
                if !self.hr_pending {
                    self.dispatch(self.on_change_retry_bpm, self.on_change_retry_hard_reset);
                }
                self.on_change_last_retry_ms = now_ms();
            }
        }

        // ── 6. Continuous sync (fires on every beat crossing) ────────────
        if playing && pos_beats.floor() > self.last_pos_beats.floor() {
            let phase_mode = self.params.phase_sync_mode.value();
            let rate_mode = self.params.rate_sync_mode.value();
            if phase_mode == SyncMode::Continuous {
                self.dispatch(bpm, true); // continuous phase reset
            } else if rate_mode == SyncMode::Continuous {
                self.dispatch(bpm, false); // continuous rate sync only
            }
        }
        if playing {
            self.last_pos_beats = pos_beats;
        } else {
            self.last_pos_beats = -1.0; // reset so the first beat after play fires
            // Reset the LED counter so the first beat after playback restarts
            // pulses the LED immediately rather than waiting for the counter to
            // roll over from wherever it stopped.
            self.midi_clock_pulse_count = 23; // next increment wraps to 0 → fires
        }
        self.prev_playing = playing;

        // ── 7. MIDI clock output via CoreMIDI virtual source ─────────────────
        // Design notes:
        //
        // • Transport start/stop → worker sends Stop/SPP/Start or Stop/SPP/Continue
        //   so receivers are always phase-locked to the DAW position.
        //
        // • BPM change (>0.5 BPM) → worker sends Stop, waits 150 ms, then
        //   SPP+Continue.  Receivers reset their averaging filter and snap to the
        //   new tempo on the very first pulses after the gap.
        //
        // Guard on pos_beats_raw — if the host does not report beat position we
        // skip rather than emitting clocks from an assumed position-0.
        if *self.params.midi_clock_enabled.lock() {
            // ── Clock pulses while playing ────────────────────────────────────
            // Reset BPM tracking when stopped so BpmChanged doesn't fire on
            // the first buffer after transport restarts.
            if !playing {
                self.last_clock_bpm = 0.0;
            }

            // On the first playing buffer after a stop, signal the worker to
            // phase-align the clock to the next beat boundary before resuming.
            if playing && !self.prev_playing {
                let _ = self.midi_clock_tx
                    .try_send(midi_clock::ClockMsg::TransportStart);
            }

            if playing {
                if let Some(beat_start) = pos_beats_raw {
                    let buf_len = buffer.samples();
                    if buf_len > 0 {
                        let ppq = *self.params.midi_clock_ppq.lock() as f64;

                        // Detect a meaningful BPM change and signal a resync gap.
                        if self.last_clock_bpm > 0.0
                            && (bpm - self.last_clock_bpm).abs() > 0.5
                        {
                            let _ = self.midi_clock_tx
                                .try_send(midi_clock::ClockMsg::BpmChanged);
                        }
                        self.last_clock_bpm = bpm;

                        let samples_per_beat = self.sample_rate as f64 * 60.0 / bpm;
                        let beats_per_sample = 1.0 / samples_per_beat;
                        let beat_end =
                            beat_start + buf_len as f64 * beats_per_sample;

                        let clock_start = (beat_start * ppq).ceil() as i64;
                        let clock_end   = (beat_end   * ppq).ceil() as i64;

                        for k in clock_start..clock_end {
                            let on_beat = k % ppq as i64 == 0;
                            let _ = self.midi_clock_tx
                                .try_send(midi_clock::ClockMsg::Tick { on_beat });
                            // LED: pulse once per beat (every 24 ticks at standard PPQ).
                            self.midi_clock_pulse_count =
                                self.midi_clock_pulse_count.wrapping_add(1);
                            if self.midi_clock_pulse_count.is_multiple_of(24) {
                                self.midi_clock_activity_ts
                                    .store(now_ms(), Ordering::Relaxed);
                            }
                        }
                    }
                }
            }
        }
        // ── 8. Force triggers — param automation edges + UI atomics ─────────

        // Connection control via automation.
        let connect_param = self.params.connect_to_last.value();
        if connect_param && !self.prev_connect_to_last {
            let ip   = self.params.target_ip.lock().clone();
            let port = *self.params.target_port.lock();
            let _ = self.cmd_tx.try_send(NetworkCommand::UpdateTarget { ip, port });
            let _ = self.cmd_tx.try_send(NetworkCommand::AuditSlots);
            self.all_slots_mode.store(true, Ordering::Relaxed);
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
            compatible_slots:  self.compatible_slots.clone(),
            occupied_slots:    self.occupied_slots.clone(),
            slot_types:        self.slot_types.clone(),
            all_slots_mode:    self.all_slots_mode.clone(),
            scan_targets:      self.scan_targets.clone(),
            scan_completed_ts: self.scan_completed_ts.clone(),
            connected_device:  self.connected_device.clone(),
            cmd_tx: self.cmd_tx.clone(),
            device_change_tx: self.device_change_tx.clone(),
            midi_bridge_connected: self.midi_bridge_connected.clone(),
            midi_clock_stats: self.midi_clock_stats.clone(),
        });
        editor::create(data)
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
        let slots: Vec<u8> = if self.all_slots_mode.load(Ordering::Relaxed) {
            let cs = self.compatible_slots.lock();
            if cs.is_empty() {
                vec![*self.params.fx_slot.lock()]
            } else {
                let filter = *self.params.fx_type_filter.lock();
                let types  = *self.slot_types.lock();
                cs.iter().filter(|&&slot| {
                    match types[(slot - 1) as usize] {
                        Some(type_id) => match fx_type_to_bit(type_id) {
                            Some(bit) => (filter >> bit) & 1 == 1,
                            None      => true, // unknown type: include
                        },
                        None => true, // type not yet audited: include
                    }
                }).copied().collect()
            }
        } else {
            vec![*self.params.fx_slot.lock()]
        };

        if hard_reset {
            // Pack into a fixed-size array — no heap allocation on the audio thread.
            let mut arr = [None::<u8>; 8];
            for (dst, &src) in arr.iter_mut().zip(slots.iter()) {
                *dst = Some(src);
            }
            let _ = self.cmd_tx.try_send(NetworkCommand::HardResetBatch { slots: arr, bpm });
        } else {
            for slot in slots {
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
