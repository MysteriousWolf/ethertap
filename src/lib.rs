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
    Arc,
    atomic::{AtomicBool, AtomicI32, AtomicU8, AtomicU32, AtomicU64, Ordering},
};

use nih_plug::prelude::*;
use parking_lot::Mutex;

mod editor;
mod midi_clock;
mod midi_hw;
mod midi_watcher;
pub mod network;
pub mod osc;
mod params;
pub mod reconnect;
pub use params::EtherTapParams;

use network::{NetworkCommand, NetworkStatus, NetworkWorker, now_ms};
use params::{SyncMode, SyncStatus};

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
    /// Millisecond timestamp of the last MIDI device-list broadcast (from midi_watcher).
    midi_last_update_ts: Arc<AtomicU64>,
    /// True once the initial MIDI device-list broadcast has landed (from midi_watcher).
    midi_has_update: Arc<AtomicBool>,
    /// Bitmask: bit n set ↔ slot (n+1) is BPM-compatible. Written by network worker.
    compatible_slots: Arc<AtomicU8>,
    /// Bitmask: bit n set ↔ slot (n+1) is occupied. Written by network worker.
    occupied_slots: Arc<AtomicU8>,
    /// Raw effect type ID for each slot (index = slot-1). i32::MIN = not yet queried.
    slot_types: Arc<[AtomicI32; 8]>,
    scan_targets: Arc<Mutex<Vec<network::DeviceInfo>>>,
    /// Millisecond timestamp of the last completed TargetsFound scan result.
    scan_completed_ts: Arc<AtomicU64>,
    /// Latest [`network::ScanHealth`] as a `u8` — the editor tints its scan
    /// control from this so a silently-blocked network is visible without
    /// reading the log.
    scan_health: Arc<AtomicU8>,
    /// Name and model of the currently connected device, from /info responses.
    connected_device: Arc<Mutex<(String, String)>>,

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

    // ── Trigger-param rising-edge detection (for VST automation) ──────────
    // Momentary params self-reset: process() consumes the rising edge, then
    // writes the param back to false via context.set_parameter().  A host
    // automation lane that *holds* true therefore retriggers on every host
    // re-send — intended trigger semantics.
    prev_connect_to_last: bool,
    prev_disconnect_param: bool,
    prev_force_sync_rate: bool,
    prev_force_sync_phase: bool,
    prev_audit_slots: bool,

    // ── Host param shadow (avoids redundant set_parameter calls) ─────────
    /// Last value written to `params.is_connected` from the audio thread.
    last_conn_status: bool,
    /// Last value written to `params.is_matched` from the audio thread.
    last_matched_status: bool,
    /// Last value written to `params.sync_status` from the audio thread.
    last_sync_status: SyncStatus,
    /// Last value written to `params.phase_reset_pending` from the audio thread.
    last_phase_reset_pending: bool,
    /// Last value written to `params.hardware_bpm` from the audio thread.
    /// Compared with an epsilon (> 0.01 BPM) to absorb telemetry jitter.
    last_hardware_bpm: f64,
    /// Last value written to `params.compatible_slot_count` from the audio thread.
    last_compatible_slot_count: u8,
    /// Last value written to `params.midi_bridge_connected` from the audio thread.
    last_midi_bridge_connected_param: bool,
    /// Set by `initialize()`: publish every read-only status parameter once on
    /// the next buffer, regardless of whether it changed. The host restores
    /// these params from the saved session like any other, so the first buffer
    /// has to overwrite whatever it restored rather than trusting a comparison
    /// against our own freshly-reset mirrors.
    force_status_publish: bool,

    // ── OnChange retry ────────────────────────────────────────────────────
    /// True while we're waiting for hardware to confirm the tempo.
    on_change_retry_pending: bool,
    /// The BPM that was sent — compared against hardware readback.
    on_change_retry_bpm: f64,
    /// True when the pending retry should be a Hard Reset (phase+rate).
    on_change_retry_hard_reset: bool,
    /// Timestamp of the last retry dispatch (ms since epoch).
    on_change_last_retry_ms: u64,

    // ── Standalone transport (no DAW host) ───────────────────────────────
    /// BPM set by the standalone transport panel (f32 bits). Used when
    /// transport.tempo is None (dummy audio backend / no host DAW).
    standalone_bpm: Arc<AtomicU32>,
    /// Play/stop state set by the standalone transport panel.
    standalone_playing: Arc<AtomicBool>,
    /// Cumulative beat position in standalone mode (f64 bits). Written by
    /// process(), read by the editor transport display.
    standalone_pos_beats: Arc<AtomicU64>,
    /// One-shot Stop trigger: editor sets, process() swap(false)-consumes
    /// unconditionally each buffer (independent of/before the `playing` gate)
    /// and performs the standalone_playing/standalone_pos_beats reset itself,
    /// serialized with its own accumulation logic — avoids the race a pair of
    /// independent cross-thread store()s would have against the Relaxed
    /// read-modify-write at the position-accumulation site below.
    standalone_stop_trigger: Arc<AtomicBool>,
}

impl Default for EtherTap {
    fn default() -> Self {
        let params = Arc::new(EtherTapParams::default());

        let hardware_float = Arc::new(AtomicU32::new(0u32));
        let host_bpm = Arc::new(AtomicU32::new(0u32));
        let conn_status = Arc::new(AtomicBool::new(false));
        let tx_activity_ts = Arc::new(AtomicU64::new(0));
        let rx_activity_ts = Arc::new(AtomicU64::new(0));
        let midi_clock_activity_ts = Arc::new(AtomicU64::new(0));
        let compatible_slots = Arc::new(AtomicU8::new(0));
        let occupied_slots = Arc::new(AtomicU8::new(0));
        let slot_types: Arc<[AtomicI32; 8]> =
            Arc::new(std::array::from_fn(|_| AtomicI32::new(i32::MIN)));
        let scan_targets = Arc::new(Mutex::new(Vec::<network::DeviceInfo>::new()));
        let scan_completed_ts = Arc::new(AtomicU64::new(0));
        let scan_health = Arc::new(AtomicU8::new(network::ScanHealth::Unknown as u8));
        let connected_device = Arc::new(Mutex::new((String::new(), String::new())));
        let scan_generation = Arc::new(AtomicU64::new(0));

        let standalone_bpm = Arc::new(AtomicU32::new(120.0f32.to_bits()));
        let standalone_playing = Arc::new(AtomicBool::new(true));
        let standalone_pos_beats = Arc::new(AtomicU64::new(0u64));
        let standalone_stop_trigger = Arc::new(AtomicBool::new(false));

        let (cmd_tx, cmd_rx) = crossbeam_channel::bounded::<NetworkCommand>(64);
        let (status_tx, status_rx) = crossbeam_channel::bounded::<NetworkStatus>(64);
        let (midi_clock_tx, midi_clock_rx) =
            crossbeam_channel::bounded::<midi_clock::ClockMsg>(256);
        let (device_change_tx, device_change_rx) = crossbeam_channel::bounded::<Option<String>>(16);
        let midi_bridge_connected = Arc::new(AtomicBool::new(false));
        let midi_bridge_connecting = Arc::new(AtomicBool::new(false));
        let midi_clock_stats = Arc::new(midi_clock::AtomicClockStats::default());

        // Spawn the MIDI device watcher BEFORE any midir::MidiOutput is created
        // (macOS: CoreMIDI notification client must be first).
        let midi_watch = midi_watcher::spawn();
        let midi_watcher_shutdown = midi_watch.shutdown.clone();
        let midi_device_rx = Arc::new(midi_watch.editor_rx);
        let midi_last_update_ts = midi_watch.last_update_ts.clone();
        let midi_has_update = midi_watch.has_update.clone();

        let worker = NetworkWorker::new(
            cmd_rx,
            status_tx,
            params.target_ip.clone(),
            params.target_port.clone(),
            params.fx_slot_atom.clone(),
            network::WorkerShared {
                hardware_float_out: hardware_float.clone(),
                compatible_slots: compatible_slots.clone(),
                occupied_slots: occupied_slots.clone(),
                slot_types: slot_types.clone(),
                scan_targets: scan_targets.clone(),
                connected_device: connected_device.clone(),
                scan_generation: scan_generation.clone(),
                auto_reconnect: params.auto_reconnect_atom.clone(),
                last_device: params.last_device.clone(),
                scan_health: scan_health.clone(),
                last_slot_types: params.last_slot_types.clone(),
            },
        );
        std::thread::Builder::new()
            .name("ethertap-net".into())
            .spawn(move || worker.run())
            .expect("failed to spawn network worker thread");

        let initial_device = params.midi_out_device.lock().clone();
        let initial_ppq = params
            .midi_clock_ppq_atom
            .load(std::sync::atomic::Ordering::Relaxed);
        let midi_worker = midi_clock::MidiClockWorker::new(
            params.midi_clock_enabled_atom.clone(),
            params.midi_auto_connect_atom.clone(),
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
            midi_device_rx,
            midi_last_update_ts,
            midi_has_update,
            midi_watcher_shutdown,
            compatible_slots,
            occupied_slots,
            slot_types,
            scan_targets,
            scan_completed_ts,
            scan_health,
            connected_device,
            scan_generation,
            sample_rate: 44100.0,
            reconnect_sync_pending: false,
            last_conn_status: false,
            last_matched_status: false,
            last_sync_status: SyncStatus::Offline,
            last_phase_reset_pending: false,
            last_hardware_bpm: 0.0,
            last_compatible_slot_count: 0,
            last_midi_bridge_connected_param: false,
            force_status_publish: true,
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
            prev_playing: false,
            prev_connect_to_last: false,
            prev_disconnect_param: false,
            prev_force_sync_rate: false,
            prev_force_sync_phase: false,
            prev_audit_slots: false,
            on_change_retry_pending: false,
            on_change_retry_bpm: 0.0,
            on_change_retry_hard_reset: false,
            on_change_last_retry_ms: 0,
            standalone_tick_samples: 0.0,
            midi_clock_drop_count: Arc::new(AtomicU32::new(0)),
            standalone_bpm,
            standalone_playing,
            standalone_pos_beats,
            standalone_stop_trigger,
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

    // VST3/CLAP hosts see a bus-less layout (EtherTap is a MIDI/OSC control
    // surface — it never reads or writes audio buffers). The standalone CPAL
    // backend, however, refuses to open a device with 0 channels (it filters
    // host configs for `channels == main_output_channels.unwrap_or_default()`
    // and errors with "device does not support 0 audio channels"), forcing a
    // fallback to the dummy backend whose self-paced loop doesn't track wall
    // clock precisely enough for sample-accurate MIDI clock generation. So the
    // standalone binary declares a silent stereo passthrough purely to give
    // CPAL a real device to drive `process()` from a hardware clock.
    #[cfg(feature = "standalone")]
    const AUDIO_IO_LAYOUTS: &'static [AudioIOLayout] = &[AudioIOLayout {
        main_input_channels: Some(new_nonzero_u32(2)),
        main_output_channels: Some(new_nonzero_u32(2)),
        ..AudioIOLayout::const_default()
    }];

    #[cfg(not(feature = "standalone"))]
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

            // Automated test hook: ETHERTAP_TEST_PORT=<port> pre-sets the
            // target to 127.0.0.1:<port> and triggers an immediate connect,
            // enabling headless integration tests without GUI interaction.
            if let Ok(port_str) = std::env::var("ETHERTAP_TEST_PORT")
                && let Ok(port) = port_str.parse::<u16>()
            {
                *self.params.target_ip.lock() = "127.0.0.1".to_string();
                *self.params.target_port.lock() = port;
                let _ = self.cmd_tx.try_send(NetworkCommand::ConnectToLast);
            }
        }
        #[cfg(not(feature = "standalone"))]
        let _ = layout;

        self.adopt_restored_state();

        // No network traffic at load: connecting is either explicit (user
        // pulses connect_to_last / the editor Connect button) or opt-in via
        // the auto_reconnect param, whose atom the network worker polls and
        // self-connects on. AuditSlots fires from process() on every connect
        // transition.
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

        // Mirror automatable params → worker-facing atomics (once per buffer).
        self.params
            .fx_slot_atom
            .store(self.params.fx_slot.value() as u8, Ordering::Relaxed);
        self.params
            .midi_clock_enabled_atom
            .store(self.params.midi_clock_enabled.value(), Ordering::Relaxed);
        self.params
            .midi_auto_connect_atom
            .store(self.params.midi_auto_connect.value(), Ordering::Relaxed);
        self.params
            .auto_reconnect_atom
            .store(self.params.auto_reconnect.value(), Ordering::Relaxed);
        self.params.midi_clock_ppq_atom.store(
            self.params.midi_clock_ppq.value().to_u8(),
            Ordering::Relaxed,
        );
        self.params
            .all_slots_atom
            .store(self.params.all_slots.value(), Ordering::Relaxed);
        {
            let p = &self.params;
            let bits: u32 = (p.fx_filter_dly.value() as u32)
                | ((p.fx_filter_3tap.value() as u32) << 1)
                | ((p.fx_filter_4tap.value() as u32) << 2)
                | ((p.fx_filter_drv.value() as u32) << 3)
                | ((p.fx_filter_dcr.value() as u32) << 4)
                | ((p.fx_filter_dfl.value() as u32) << 5)
                | ((p.fx_filter_modd.value() as u32) << 6);
            p.fx_type_filter.store(bits, Ordering::Relaxed);
        }

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
                    if self.reconnect_sync_pending && self.last_bpm > 0.0 && !self.bpm_is_settling {
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
        let pos_beats_raw = transport.pos_beats(); // None when host doesn't report position
        let pos_beats = pos_beats_raw.unwrap_or(0.0);

        // In standalone builds always use the UI-driven atomics for BPM and playing.
        // pos_beats_raw.is_none() cannot be used as a sentinel: Transport::pos_beats()
        // computes from pos_samples + tempo when both are present, and the dummy backend
        // sets both, so pos_beats_raw is never None even without a real DAW.
        // #[cfg(feature = "standalone")] is exact — only the standalone binary enables it,
        // never the exported VST3.
        #[cfg(feature = "standalone")]
        let (bpm, mut playing) = {
            let raw = f32::from_bits(self.standalone_bpm.load(Ordering::Relaxed)) as f64;
            let b = if raw.is_finite() && raw > 0.0 {
                raw
            } else {
                120.0
            };
            let p = self.standalone_playing.load(Ordering::Relaxed);
            (b, p)
        };
        #[cfg(not(feature = "standalone"))]
        let (bpm, playing) = (
            transport.tempo.unwrap_or(120.0).max(10.0),
            transport.playing,
        );

        // Stop trigger: one-shot, consumed unconditionally every buffer —
        // independent of and *before* the `playing` gate below — so a Stop
        // pressed while already paused still zeroes the position. process()
        // performs both the playing and position resets itself, serialized
        // with its own Relaxed read-modify-write accumulation of
        // standalone_pos_beats, eliminating the race a pair of independent
        // editor-thread store()s would otherwise have against it.
        #[cfg(feature = "standalone")]
        if self.standalone_stop_trigger.swap(false, Ordering::AcqRel) {
            playing = false;
            self.standalone_playing.store(false, Ordering::Relaxed);
            self.standalone_pos_beats
                .store(0.0f64.to_bits(), Ordering::Relaxed);
        }

        // Bar / time-sig metadata for PS1-PS3 phase sync improvements.
        let bar_number = transport.bar_number().unwrap_or(-1);
        let ts_num = transport.time_sig_numerator.unwrap_or(4);
        let ts_den = transport.time_sig_denominator.unwrap_or(4);
        // Quarter-note length of one bar; e.g. 4/4 → 4.0 qn, 3/4 → 3.0 qn, 7/8 → 3.5 qn.
        let bar_len_qn = ts_num as f64 * 4.0 / ts_den as f64;
        // Start of the next bar in quarter notes (used for PS2 HR quantisation).
        let next_bar_beat =
            transport.bar_start_pos_beats().unwrap_or(pos_beats.floor()) + bar_len_qn;
        // Active loop range, if the host has one enabled (used for PS4).
        let loop_range = transport.loop_range_beats();
        // Capture prev_playing now — before any updates — so step 7 can detect
        // the stopped→playing edge correctly.  self.prev_playing is written at
        // the very end of process() so it always reflects the previous call.
        let was_playing = self.prev_playing;

        // ── 3. Publish host BPM for the editor ───────────────────────────
        self.host_bpm
            .store((bpm as f32).to_bits(), Ordering::Release);

        // ── 3b. Update read-only host params from audio thread ───────────
        // This keeps is_connected / is_matched current even when the GUI is
        // closed; context.set_parameter() updates the internal atomic and, for
        // VST3, schedules a host notification via the GUI event loop.
        let connected = self.conn_status.load(Ordering::Acquire);
        let force_status = std::mem::take(&mut self.force_status_publish);
        let hw_float = f32::from_bits(self.hardware_float.load(Ordering::Acquire));
        let in_sync =
            connected && hw_float > 0.0001 && (osc::bpm_to_float(bpm) - hw_float).abs() < 0.001;
        // Publishing the value and reacting to a transition are separate jobs:
        // the forced first-buffer publish overwrites whatever the host restored,
        // but nothing has actually changed, so the connect/disconnect side
        // effects below must stay keyed on a real transition.
        let conn_changed = connected != self.last_conn_status;
        if force_status || conn_changed {
            context.set_parameter(&self.params.is_connected, connected);
        }
        if conn_changed {
            if connected {
                // Just (re)connected: scan slots and arm the auto-sync.
                // This mirrors the manual "Query → All" flow in the editor.
                // all_slots is a host param now — write it through the same
                // set_parameter path as is_connected so the host stays in sync;
                // the atom mirror picks it up at the top of the next buffer,
                // well before the SlotScanDone-gated dispatch fires.
                context.set_parameter(&self.params.all_slots, true);
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
        if force_status || in_sync != self.last_matched_status {
            context.set_parameter(&self.params.is_matched, in_sync);
            self.last_matched_status = in_sync;
        }

        // sync_status: Offline (not connected) > Synced (matched) > Syncing
        // (settling / retry pending / Hard Reset armed) > Connected (idle).
        let sync_status = if !connected {
            SyncStatus::Offline
        } else if in_sync {
            SyncStatus::Synced
        } else if self.bpm_is_settling || self.on_change_retry_pending || self.hr_pending {
            SyncStatus::Syncing
        } else {
            SyncStatus::Connected
        };
        if force_status || sync_status != self.last_sync_status {
            context.set_parameter(&self.params.sync_status, sync_status);
            self.last_sync_status = sync_status;
        }

        // phase_reset_pending mirrors hr_pending (quantised Hard Reset armed).
        if force_status || self.hr_pending != self.last_phase_reset_pending {
            context.set_parameter(&self.params.phase_reset_pending, self.hr_pending);
            self.last_phase_reset_pending = self.hr_pending;
        }

        // hardware_bpm = 20.0 / hardware_float when telemetry present, else 0.0.
        // Epsilon-guarded (> 0.01 BPM) to absorb telemetry float jitter.
        let hardware_bpm = if hw_float > 0.0001 {
            osc::float_to_bpm(hw_float)
        } else {
            0.0
        };
        if force_status || (hardware_bpm - self.last_hardware_bpm).abs() > 0.01 {
            context.set_parameter(&self.params.hardware_bpm, hardware_bpm as f32);
            self.last_hardware_bpm = hardware_bpm;
        }

        // compatible_slot_count = popcount of the compatible_slots bitmask.
        let compatible_slot_count =
            self.compatible_slots.load(Ordering::Relaxed).count_ones() as i32;
        if force_status || compatible_slot_count as u8 != self.last_compatible_slot_count {
            context.set_parameter(&self.params.compatible_slot_count, compatible_slot_count);
            self.last_compatible_slot_count = compatible_slot_count as u8;
        }

        // midi_bridge_connected mirrors the MIDI worker's open-connection flag.
        let midi_bridge_connected = self.midi_bridge_connected.load(Ordering::Relaxed);
        if force_status || midi_bridge_connected != self.last_midi_bridge_connected_param {
            context.set_parameter(&self.params.midi_bridge_connected, midi_bridge_connected);
            self.last_midi_bridge_connected_param = midi_bridge_connected;
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
            // Save the pre-seek position before resetting last_pos_beats — the
            // loop-repeat check below needs the original value to test whether
            // the transport was near the loop end before jumping back.
            let pre_seek_pos_beats = self.last_pos_beats;
            self.last_pos_beats = pos_beats - 1.0;
            self.last_beat_idx = pos_beats.floor() as i64 - 1;
            self.last_bar_number = bar_number - 1;
            self.bpm_is_settling = false;
            self.on_change_retry_pending = false;

            let is_loop_repeat = loop_range
                .map(|(start, end)| {
                    pos_beats >= start - 0.5
                        && pos_beats <= start + 0.5
                        && pre_seek_pos_beats >= end - 0.5
                })
                .unwrap_or(false);

            self.hr_pending = true;
            self.hr_target_beat = if is_loop_repeat {
                next_bar_beat
            } else {
                pos_beats.ceil()
            };
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
                            self.on_change_retry_pending = true;
                            self.on_change_retry_bpm = bpm;
                            self.on_change_retry_hard_reset = true;
                            self.on_change_last_retry_ms = now;
                        } else if rate_mode == SyncMode::OnChange {
                            self.dispatch(bpm, false);
                            self.on_change_retry_pending = true;
                            self.on_change_retry_bpm = bpm;
                            self.on_change_retry_hard_reset = false;
                            self.on_change_last_retry_ms = now;
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
        if playing
            && bar_number >= 0
            && bar_number != self.last_bar_number
            && bar_number != self.last_bar_number + 1
        {
            // Non-sequential bar jump — re-arm a bar-boundary Hard Reset.
            if !self.hr_pending {
                self.hr_pending = true;
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
                self.hr_pending = true;
                self.hr_target_beat = next_bar_beat;
            }
        }
        // Read PPQ once per buffer — drives LED blink cadence and tick generation.
        let midi_ppq = self.params.midi_clock_ppq_atom.load(Ordering::Relaxed);

        if playing {
            self.last_pos_beats = pos_beats;
            self.last_beat_idx = beat_idx;
            self.last_bar_number = bar_number;
            self.last_time_sig = cur_time_sig;
        } else {
            self.last_pos_beats = -1.0; // reset so the first beat after play fires
            self.last_beat_idx = -1;
            self.last_bar_number = -1;
            // Prime the counter so the first tick after resumption fires the LED
            // immediately, regardless of the PPQ setting.
            self.midi_clock_pulse_count = midi_ppq.saturating_sub(1);
        }

        // ── 7. MIDI clock output via CoreMIDI virtual source ─────────────────
        if self.params.midi_clock_enabled_atom.load(Ordering::Relaxed) {
            // Standalone builds always take the tick-accumulator path below —
            // pos_beats_raw is forced to None so transport-derived beat_start
            // (computed by the dummy backend from pos_samples + fixed tempo,
            // see the #[cfg] block above) never drives MIDI clock timing.
            #[cfg(not(feature = "standalone"))]
            let pos_beats_raw = transport.pos_beats(); // None = no DAW transport
            #[cfg(feature = "standalone")]
            let pos_beats_raw: Option<f64> = None;

            if let Some(beat_start) = pos_beats_raw {
                // DAW mode: follow transport position and play state.
                if !playing {
                    self.last_clock_bpm = 0.0;
                    if was_playing {
                        // Gate any ticks that were already queued for this
                        // buffer before we learned transport stopped.
                        if self
                            .midi_clock_tx
                            .try_send(midi_clock::ClockMsg::Stop)
                            .is_err()
                        {
                            self.midi_clock_drop_count.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                }
                if playing && !was_playing {
                    if self
                        .midi_clock_tx
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
                            Some(prev) if (bpm - prev).abs() > BPM_MIDI_THRESHOLD => {
                                if self
                                    .midi_clock_tx
                                    .try_send(midi_clock::ClockMsg::BpmChanged { new_bpm: bpm })
                                    .is_ok()
                                {
                                    self.last_bpm_changed_at = Some(bpm);
                                } else {
                                    self.midi_clock_drop_count.fetch_add(1, Ordering::Relaxed);
                                    self.last_bpm_changed_at = Some(bpm);
                                }
                            }
                            _ => {} // Some(_): BPM unchanged; None: impossible (line 893 always seeds Some on play-start)
                        }
                        self.last_clock_bpm = bpm;

                        let samples_per_beat = transport.sample_rate as f64 * 60.0 / bpm;
                        let beats_per_sample = 1.0 / samples_per_beat;
                        let beat_end = beat_start + buf_len as f64 * beats_per_sample;

                        let clock_start = (beat_start * ppq).ceil() as i64;
                        let clock_end = (beat_end * ppq).ceil() as i64;

                        let mut tick_drops = 0u32;
                        for k in clock_start..clock_end {
                            let on_beat = k % ppq as i64 == 0;
                            if self
                                .midi_clock_tx
                                .try_send(midi_clock::ClockMsg::Tick { on_beat })
                                .is_err()
                            {
                                tick_drops += 1;
                            }
                            self.midi_clock_pulse_count += 1;
                            if self.midi_clock_pulse_count >= midi_ppq {
                                self.midi_clock_pulse_count = 0;
                                self.midi_clock_activity_ts.store(now, Ordering::Relaxed);
                            }
                        }
                        if tick_drops > 0 {
                            self.midi_clock_drop_count
                                .fetch_add(tick_drops, Ordering::Relaxed);
                        }
                    }
                }
            } else {
                // Standalone (no DAW transport): derive tick interval from sample count
                // to avoid Instant::now() syscalls on the audio thread. `playing` is
                // already sourced from standalone_playing in standalone builds (see
                // the #[cfg] block above) and from transport.playing otherwise.
                let buf_len = buffer.samples();
                if playing && buf_len > 0 {
                    let beats_this_buf =
                        buf_len as f64 / (transport.sample_rate as f64 * 60.0 / bpm);
                    let raw_pos = f64::from_bits(self.standalone_pos_beats.load(Ordering::Relaxed));
                    let prev_pos = if raw_pos.is_finite() { raw_pos } else { 0.0 };
                    self.standalone_pos_beats
                        .store((prev_pos + beats_this_buf).to_bits(), Ordering::Relaxed);

                    let ppq = midi_ppq as f64;
                    let tick_interval_f = transport.sample_rate as f64 * 60.0 / bpm / ppq;
                    if tick_interval_f.is_normal() {
                        // On BPM change, rescale the accumulator to preserve phase.
                        // Resetting to 0 would cause the next tick to fire immediately
                        // regardless of where in the beat we were.
                        if (bpm - self.last_clock_bpm).abs() > BPM_SETTLE_THRESHOLD
                            && self.last_clock_bpm > 0.0
                        {
                            let old_interval =
                                transport.sample_rate as f64 * 60.0 / self.last_clock_bpm / ppq;
                            if old_interval.is_normal() {
                                self.standalone_tick_samples =
                                    self.standalone_tick_samples * tick_interval_f / old_interval;
                            }
                        }
                        self.last_clock_bpm = bpm;
                        self.standalone_tick_samples += buf_len as f64;
                        while self.standalone_tick_samples >= tick_interval_f {
                            let on_beat = self.midi_clock_pulse_count == 0;
                            let _ = self
                                .midi_clock_tx
                                .try_send(midi_clock::ClockMsg::Tick { on_beat });
                            self.standalone_tick_samples -= tick_interval_f;
                            self.midi_clock_pulse_count += 1;
                            if self.midi_clock_pulse_count >= midi_ppq {
                                self.midi_clock_pulse_count = 0;
                                self.midi_clock_activity_ts.store(now, Ordering::Relaxed);
                            }
                        }
                    }
                }
            }
        }

        // ── 8. Momentary trigger params — rising edge fires, then self-reset ─
        // Both the editor (via ParamSetter) and host automation lanes drive
        // these.  After consuming a rising edge, process() writes the param
        // back to false through context.set_parameter() — same proven
        // audio-thread path as is_connected/is_matched.  A host lane holding
        // true retriggers on each host re-send (intended trigger semantics).

        // Connection control: send ConnectToLast so the network worker reads
        // ip/port itself — no String allocation on the audio thread.
        let connect_param = self.params.connect_to_last.value();
        if connect_param {
            if !self.prev_connect_to_last {
                let _ = self.cmd_tx.try_send(NetworkCommand::ConnectToLast);
                let _ = self.cmd_tx.try_send(NetworkCommand::AuditSlots);
                context.set_parameter(&self.params.all_slots, true);
            }
            context.set_parameter(&self.params.connect_to_last, false);
        }
        self.prev_connect_to_last = connect_param;

        let disconnect_param = self.params.disconnect.value();
        if disconnect_param {
            if !self.prev_disconnect_param {
                let _ = self.cmd_tx.try_send(NetworkCommand::Disconnect);
            }
            context.set_parameter(&self.params.disconnect, false);
        }
        self.prev_disconnect_param = disconnect_param;

        let audit_param = self.params.audit_slots.value();
        if audit_param {
            if !self.prev_audit_slots {
                let _ = self.cmd_tx.try_send(NetworkCommand::AuditSlots);
            }
            context.set_parameter(&self.params.audit_slots, false);
        }
        self.prev_audit_slots = audit_param;

        // Rate-only sync (delay time, no phase reset).
        let force_rate_param = self.params.force_sync_rate.value();
        if force_rate_param {
            if !self.prev_force_sync_rate {
                self.dispatch(bpm, false);
            }
            context.set_parameter(&self.params.force_sync_rate, false);
        }
        self.prev_force_sync_rate = force_rate_param;

        // Phase (hard reset) sync.
        let force_phase_param = self.params.force_sync_phase.value();
        if force_phase_param {
            if !self.prev_force_sync_phase {
                self.dispatch(bpm, true);
            }
            context.set_parameter(&self.params.force_sync_phase, false);
        }
        self.prev_force_sync_phase = force_phase_param;

        self.last_bpm = bpm;
        self.prev_playing = playing;

        ProcessStatus::Normal
    }

    fn editor(&mut self, _async_executor: AsyncExecutor<Self>) -> Option<Box<dyn Editor>> {
        editor::create_editor(self)
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
        _ => None,
    }
}

impl EtherTap {
    /// Reconcile the plugin's own state with the parameter values the host has
    /// just restored. Called from `initialize()`, before the first buffer.
    ///
    /// A VST3 host restores every `#[id]` parameter from the saved session,
    /// including the momentary triggers and the read-only status readouts. Both
    /// groups need handling: a restored trigger must not look like a fresh
    /// press, and a restored status must not be believed.
    fn adopt_restored_state(&mut self) {
        // Mirror auto_reconnect into the worker-facing atom right here, not
        // only from process(). The worker's discovery and self-connect both
        // read that atom, and a plugin sitting on a bypassed or disabled track
        // may never get a process() call to mirror it — which used to mean
        // auto-reconnect silently did nothing for the whole session.
        self.params
            .auto_reconnect_atom
            .store(self.params.auto_reconnect.value(), Ordering::Relaxed);

        // Seed the momentary-trigger edge detectors from the restored values.
        // A session stored with one of them true would otherwise read as a
        // rising edge on the first buffer and fire a command nobody asked for.
        // A restored `disconnect` was the worst of them: it latched the worker
        // into user-disconnected, which blocks auto-reconnect and background
        // discovery until the user presses Connect by hand.
        self.prev_connect_to_last = self.params.connect_to_last.value();
        self.prev_disconnect_param = self.params.disconnect.value();
        self.prev_force_sync_rate = self.params.force_sync_rate.value();
        self.prev_force_sync_phase = self.params.force_sync_phase.value();
        self.prev_audit_slots = self.params.audit_slots.value();

        // Read-only status params are host-visible, so the host restores them
        // too. Believing a restored "connected" would leave the UI and any
        // automation lane lying about the mixer until the first real status
        // arrives, so force a full status republish on the first buffer.
        //
        // The republish reads `conn_status` / `hardware_float`, which the
        // network worker owns — they are never host-restored, so they already
        // hold the truth and must not be cleared here. `initialize()` runs on
        // every host activation, not only at load: clearing them would make a
        // still-connected plugin report offline whenever its track is
        // deactivated and re-enabled, and the offline→online edge on the next
        // heartbeat would re-fire the eight-query connect-time slot audit.
        // Seeding the edge detector from the live value keeps the publish
        // forced without inventing a transition. At genuine load time the
        // worker has not connected yet, so this reads false as before.
        self.last_conn_status = self.conn_status.load(Ordering::Acquire);
        self.last_matched_status = false;
        self.last_sync_status = SyncStatus::Offline;
        self.last_phase_reset_pending = false;
        self.last_hardware_bpm = 0.0;
        self.force_status_publish = true;

        // Restore the last slot audit so the editor can draw the console's slot
        // map straight away rather than eight blanks. The audit that runs on
        // the next connect overwrites all of it.
        let restored = *self.params.last_slot_types.lock();
        let mut compatible = 0u8;
        let mut occupied = 0u8;
        for (i, &type_id) in restored.iter().enumerate() {
            self.slot_types[i].store(type_id, Ordering::Relaxed);
            if type_id == i32::MIN {
                continue;
            }
            let slot = i as u8 + 1;
            occupied |= 1 << i;
            if osc::is_bpm_compatible(type_id, slot) {
                compatible |= 1 << i;
            }
        }
        self.compatible_slots.store(compatible, Ordering::Release);
        self.occupied_slots.store(occupied, Ordering::Release);
        self.last_compatible_slot_count = compatible.count_ones() as u8;
    }

    /// Dispatch a sync command.  `hard_reset = true` → `HardReset`, else `SyncNow`.
    ///
    /// When "all slots" mode is active, every compatible slot whose effect type
    /// is enabled in `params.fx_type_filter` receives the command; falls back to
    /// the single selected slot when no compatible slots are known yet.
    fn dispatch(&self, bpm: f64, hard_reset: bool) {
        // Build slot list into a fixed-size stack array — no heap allocation.
        let mut slots = [None::<u8>; 8];
        let mut n = 0usize;

        if self.params.all_slots_atom.load(Ordering::Acquire) {
            // All four reads are lock-free atomic loads — no mutex on the audio thread.
            let fallback_slot = self.params.fx_slot_atom.load(Ordering::Relaxed);
            let filter = self.params.fx_type_filter.load(Ordering::Relaxed);
            // Decode compatible_slots bitmask: bit n → slot (n+1).
            let compat_mask = self.compatible_slots.load(Ordering::Relaxed);
            if compat_mask == 0 {
                slots[0] = Some(fallback_slot);
                n = 1;
            } else {
                for bit in 0..8u8 {
                    if compat_mask & (1 << bit) == 0 {
                        continue;
                    }
                    let slot = bit + 1;
                    let raw_type = self.slot_types[bit as usize].load(Ordering::Relaxed);
                    let include = if raw_type == i32::MIN {
                        true // not yet audited: include
                    } else {
                        match fx_type_to_bit(raw_type) {
                            Some(b) => (filter >> b) & 1 == 1,
                            None => true, // unknown type: include
                        }
                    };
                    if include {
                        slots[n] = Some(slot);
                        n += 1;
                    }
                }
            }
        } else {
            slots[0] = Some(self.params.fx_slot_atom.load(Ordering::Relaxed));
            n = 1;
        }

        if hard_reset {
            let _ = self
                .cmd_tx
                .try_send(NetworkCommand::HardResetBatch { slots, bpm });
        } else {
            for slot in slots[..n].iter().filter_map(|s| *s) {
                let _ = self.cmd_tx.try_send(NetworkCommand::SyncNow { slot, bpm });
            }
        }
    }

    /// Return a clone of the plugin's `Arc<EtherTapParams>` so integration
    /// tests can read and write param state via the shared Arc without
    /// reaching into private fields.
    pub fn ethertap_params(&self) -> Arc<EtherTapParams> {
        self.params.clone()
    }

    /// Shared-state handles for harness-driven integration tests (vst-runtime):
    /// the same Arcs the workers and editor observe, so a test can assert on
    /// connection status, telemetry read-back, slot audits, and MIDI clock
    /// health without GUI or private-field access.
    #[doc(hidden)]
    pub fn test_handles(&self) -> TestHandles {
        TestHandles {
            conn_status: self.conn_status.clone(),
            hardware_float: self.hardware_float.clone(),
            compatible_slots: self.compatible_slots.clone(),
            occupied_slots: self.occupied_slots.clone(),
            slot_types: self.slot_types.clone(),
            connected_device: self.connected_device.clone(),
            midi_clock_stats: self.midi_clock_stats.clone(),
            midi_clock_drop_count: self.midi_clock_drop_count.clone(),
            device_change_tx: self.device_change_tx.clone(),
            midi_bridge_connected: self.midi_bridge_connected.clone(),
        }
    }
}

/// Observation surface returned by [`EtherTap::test_handles`].  Test-only by
/// convention (`#[doc(hidden)]` accessor); fields are the live shared Arcs,
/// not snapshots.
#[doc(hidden)]
pub struct TestHandles {
    pub conn_status: Arc<AtomicBool>,
    /// Hardware delay float as `f32` bits (see `f32::from_bits`).
    pub hardware_float: Arc<AtomicU32>,
    pub compatible_slots: Arc<AtomicU8>,
    pub occupied_slots: Arc<AtomicU8>,
    pub slot_types: Arc<[AtomicI32; 8]>,
    pub connected_device: Arc<Mutex<(String, String)>>,
    pub midi_clock_stats: Arc<midi_clock::AtomicClockStats>,
    pub midi_clock_drop_count: Arc<AtomicU32>,
    /// Notifies the MIDI clock worker of an output-device change (same channel
    /// the editor's device picker uses).
    pub device_change_tx: crossbeam_channel::Sender<Option<String>>,
    /// True while the worker holds an open connection to the selected output.
    pub midi_bridge_connected: Arc<AtomicBool>,
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
        fn transport(&self) -> &Transport {
            &self.transport
        }
        fn next_event(&mut self) -> Option<PluginNoteEvent<EtherTap>> {
            None
        }
        fn send_event(&mut self, _event: PluginNoteEvent<EtherTap>) {}
        fn set_latency_samples(&self, _samples: u32) {}
        fn set_current_voice_capacity(&self, _capacity: u32) {}
    }

    fn make_buffer() -> Buffer<'static> {
        Buffer::default()
    }

    fn make_aux() -> AuxiliaryBuffers<'static> {
        AuxiliaryBuffers {
            inputs: &mut [],
            outputs: &mut [],
        }
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

    // ── Standalone Stop trigger ──────────────────────────────────────────
    //
    // Encodes WHY the trigger-atomic idiom is mandatory: a naive pair of
    // independent editor-thread store()s on standalone_playing /
    // standalone_pos_beats races process()'s Relaxed read-modify-write
    // accumulation of standalone_pos_beats (see the standalone position
    // block above). This test asserts the *outcome* the race would
    // jeopardize — both atomics land at their Stop values after a single
    // process() call consumes the one-shot trigger — proving process()
    // performs the reset itself rather than relying on cross-thread stores.
    #[cfg(feature = "standalone")]
    #[test]
    fn standalone_stop_trigger_zeroes_position_and_clears_playing() {
        let mut plugin = EtherTap::default();
        let mut ctx = MockProcessContext::new(120.0, true);
        let mut buffer = make_buffer();
        let mut aux = make_aux();

        // Simulate: transport was running and had accumulated position.
        plugin.standalone_playing.store(true, Ordering::Relaxed);
        plugin
            .standalone_pos_beats
            .store(42.5f64.to_bits(), Ordering::Relaxed);

        // Editor thread sets the one-shot trigger (mirrors Message::StopStandalone).
        plugin
            .standalone_stop_trigger
            .store(true, Ordering::Release);

        plugin.process(&mut buffer, &mut aux, &mut ctx);

        assert!(
            !plugin.standalone_playing.load(Ordering::Relaxed),
            "Stop must leave standalone_playing == false",
        );
        let pos = f64::from_bits(plugin.standalone_pos_beats.load(Ordering::Relaxed));
        assert_eq!(pos, 0.0, "Stop must zero standalone_pos_beats, got {pos}");

        // Trigger is one-shot — a second process() call must not re-fire it
        // (e.g. re-zeroing a position the user has since moved forward).
        plugin
            .standalone_pos_beats
            .store(7.0f64.to_bits(), Ordering::Relaxed);
        plugin.process(&mut buffer, &mut aux, &mut ctx);
        let pos_after = f64::from_bits(plugin.standalone_pos_beats.load(Ordering::Relaxed));
        assert_eq!(
            pos_after, 7.0,
            "consumed trigger must not fire again, got {pos_after}"
        );
    }

    /// Stop pressed while already paused must still zero the position — the
    /// trigger is consumed unconditionally each buffer, independent of and
    /// before the `playing` gate (gating on `playing` would silently swallow
    /// a Stop-while-paused).
    #[cfg(feature = "standalone")]
    #[test]
    fn standalone_stop_trigger_resets_position_while_paused() {
        let mut plugin = EtherTap::default();
        let mut ctx = MockProcessContext::new(120.0, false);
        let mut buffer = make_buffer();
        let mut aux = make_aux();

        plugin.standalone_playing.store(false, Ordering::Relaxed);
        plugin
            .standalone_pos_beats
            .store(13.0f64.to_bits(), Ordering::Relaxed);
        plugin
            .standalone_stop_trigger
            .store(true, Ordering::Release);

        plugin.process(&mut buffer, &mut aux, &mut ctx);

        assert!(!plugin.standalone_playing.load(Ordering::Relaxed));
        let pos = f64::from_bits(plugin.standalone_pos_beats.load(Ordering::Relaxed));
        assert_eq!(
            pos, 0.0,
            "Stop-while-paused must still zero position, got {pos}"
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
        assert_eq!(plugin.params.sync_status.value(), SyncStatus::Offline);
        assert!(!plugin.params.phase_reset_pending.value());
        assert_eq!(plugin.params.hardware_bpm.value(), 0.0);
        assert_eq!(plugin.params.compatible_slot_count.value(), 0);
        assert!(!plugin.params.midi_bridge_connected.value());

        plugin.process(&mut buffer, &mut aux, &mut ctx);
        assert!(!plugin.params.is_connected.value());
        assert!(!plugin.params.is_matched.value());
        assert_eq!(plugin.params.sync_status.value(), SyncStatus::Offline);

        // Connect + matching hardware telemetry: is_connected, is_matched,
        // and sync_status (Offline -> Synced) all flip together.
        plugin.conn_status.store(true, Ordering::Release);
        let hw_val = (20.0_f64 / 120.0_f64) as f32;
        plugin
            .hardware_float
            .store(hw_val.to_bits(), Ordering::Release);
        plugin.process(&mut buffer, &mut aux, &mut ctx);
        assert!(plugin.params.is_connected.value());
        assert!(plugin.params.is_matched.value());
        assert_eq!(plugin.params.sync_status.value(), SyncStatus::Synced);

        // hardware_bpm = 20.0 / hardware_float = 120.0 in this case.
        let hw_bpm = plugin.params.hardware_bpm.value();
        assert!((hw_bpm - 120.0).abs() < 0.01, "hardware_bpm = {hw_bpm}");

        // compatible_slot_count mirrors popcount of compatible_slots bitmask.
        plugin
            .compatible_slots
            .store(0b0000_1011, Ordering::Relaxed); // 3 bits set
        plugin.process(&mut buffer, &mut aux, &mut ctx);
        assert_eq!(plugin.params.compatible_slot_count.value(), 3);

        // midi_bridge_connected mirrors the MIDI worker's atomic.
        plugin.midi_bridge_connected.store(true, Ordering::Relaxed);
        plugin.process(&mut buffer, &mut aux, &mut ctx);
        assert!(plugin.params.midi_bridge_connected.value());

        // phase_reset_pending mirrors hr_pending.
        plugin.hr_pending = true;
        plugin.process(&mut buffer, &mut aux, &mut ctx);
        assert!(plugin.params.phase_reset_pending.value());

        // Disconnect: is_connected, is_matched, sync_status, hardware_bpm
        // all reflect the offline state. hr_pending is cleared by the
        // disconnect handler, so phase_reset_pending follows it down.
        plugin.conn_status.store(false, Ordering::Release);
        plugin
            .hardware_float
            .store(0.0f32.to_bits(), Ordering::Release);
        plugin.process(&mut buffer, &mut aux, &mut ctx);
        assert!(!plugin.params.is_connected.value());
        assert!(!plugin.params.is_matched.value());
        assert_eq!(plugin.params.sync_status.value(), SyncStatus::Offline);
        assert_eq!(plugin.params.hardware_bpm.value(), 0.0);
        assert!(!plugin.params.phase_reset_pending.value());
    }

    /// `sync_status` precedence: Offline (not connected) > Synced (matched) >
    /// Syncing (settling / retry pending / Hard Reset armed) > Connected (idle).
    #[test]
    fn sync_status_precedence_transitions() {
        let mut plugin = EtherTap::default();
        let mut ctx = MockProcessContext::new(120.0, false);
        let mut buffer = make_buffer();
        let mut aux = make_aux();

        // Not connected -> Offline regardless of other flags.
        assert_eq!(plugin.params.sync_status.value(), SyncStatus::Offline);

        // Connected, no telemetry (not matched), no syncing flags -> Connected.
        plugin.conn_status.store(true, Ordering::Release);
        plugin.process(&mut buffer, &mut aux, &mut ctx);
        assert_eq!(plugin.params.sync_status.value(), SyncStatus::Connected);

        // Connected + bpm_is_settling -> Syncing (even though not matched).
        plugin.bpm_is_settling = true;
        plugin.process(&mut buffer, &mut aux, &mut ctx);
        assert_eq!(plugin.params.sync_status.value(), SyncStatus::Syncing);
        plugin.bpm_is_settling = false;

        // Back to Connected once the settling flag clears.
        plugin.process(&mut buffer, &mut aux, &mut ctx);
        assert_eq!(plugin.params.sync_status.value(), SyncStatus::Connected);

        // Connected + on_change_retry_pending -> Syncing.
        plugin.on_change_retry_pending = true;
        plugin.process(&mut buffer, &mut aux, &mut ctx);
        assert_eq!(plugin.params.sync_status.value(), SyncStatus::Syncing);
        plugin.on_change_retry_pending = false;
        plugin.process(&mut buffer, &mut aux, &mut ctx);
        assert_eq!(plugin.params.sync_status.value(), SyncStatus::Connected);

        // Connected + hr_pending -> Syncing.
        plugin.hr_pending = true;
        plugin.process(&mut buffer, &mut aux, &mut ctx);
        assert_eq!(plugin.params.sync_status.value(), SyncStatus::Syncing);

        // Matched (Synced) takes precedence over Syncing flags.
        let hw_val = (20.0_f64 / 120.0_f64) as f32;
        plugin
            .hardware_float
            .store(hw_val.to_bits(), Ordering::Release);
        plugin.process(&mut buffer, &mut aux, &mut ctx);
        assert_eq!(plugin.params.sync_status.value(), SyncStatus::Synced);

        // Disconnect: Offline takes precedence over everything else.
        plugin.conn_status.store(false, Ordering::Release);
        plugin.process(&mut buffer, &mut aux, &mut ctx);
        assert_eq!(plugin.params.sync_status.value(), SyncStatus::Offline);
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

        // last_device identity (name, model) — adopted on connect, verified
        // by the auto-reconnect path; must survive session reloads.
        *params.last_device.lock() = ("Studio Desk".to_owned(), "X32".to_owned());
        let json = serde_json::to_string(&*params.last_device.lock()).unwrap();
        assert_eq!(
            serde_json::from_str::<(String, String)>(&json).unwrap(),
            ("Studio Desk".to_owned(), "X32".to_owned())
        );

        // auto_reconnect must default OFF — no surprise automation.
        assert!(!params.auto_reconnect.value());
        assert!(
            !params
                .auto_reconnect_atom
                .load(std::sync::atomic::Ordering::Relaxed)
        );
    }

    // ── Restored session state ──────────────────────────────────────────────

    /// Write a host parameter by `#[id]`, the way a host restoring a session
    /// does — straight into the parameter, with no plugin code in between.
    fn set_param(plugin: &EtherTap, id: &str, normalized: f32) {
        let map = plugin.params.param_map();
        let (_, ptr, _) = map
            .iter()
            .find(|(name, _, _)| name == id)
            .unwrap_or_else(|| panic!("no parameter with id {id}"));
        // SAFETY: `ptr` points into `plugin.params`, which outlives this call.
        unsafe {
            ptr.set_normalized_value(normalized);
        }
    }

    /// A session saved while `disconnect` happened to be true must not fire a
    /// Disconnect on load. That command latches the worker into
    /// user-disconnected, which kills auto-reconnect and background discovery
    /// for the whole session — the exact "it never connects" symptom.
    #[test]
    fn restored_disconnect_trigger_does_not_fire_on_load() {
        let mut plugin = EtherTap::default();
        let (test_cmd_tx, test_cmd_rx) = crossbeam_channel::bounded(8);
        plugin.cmd_tx = test_cmd_tx;

        // Host restores the saved parameter set, then initialize() runs.
        set_param(&plugin, "disconnect", 1.0);
        assert!(plugin.params.disconnect.value(), "precondition");
        plugin.adopt_restored_state();

        let mut ctx = MockProcessContext::new(120.0, false);
        let mut buffer = make_buffer();
        let mut aux = make_aux();
        plugin.process(&mut buffer, &mut aux, &mut ctx);

        assert!(
            !test_cmd_rx
                .try_iter()
                .any(|c| matches!(c, NetworkCommand::Disconnect)),
            "a restored trigger value must not be read as a rising edge"
        );
    }

    /// Same hazard on the connect side: a restored `connect_to_last` must not
    /// put traffic on the network before the user has asked for any.
    #[test]
    fn restored_connect_trigger_does_not_fire_on_load() {
        let mut plugin = EtherTap::default();
        let (test_cmd_tx, test_cmd_rx) = crossbeam_channel::bounded(8);
        plugin.cmd_tx = test_cmd_tx;

        set_param(&plugin, "connect_to_last", 1.0);
        plugin.adopt_restored_state();

        let mut ctx = MockProcessContext::new(120.0, false);
        let mut buffer = make_buffer();
        let mut aux = make_aux();
        plugin.process(&mut buffer, &mut aux, &mut ctx);

        assert!(
            !test_cmd_rx
                .try_iter()
                .any(|c| matches!(c, NetworkCommand::ConnectToLast)),
            "a restored trigger value must not be read as a rising edge"
        );
    }

    /// After a genuine press the trigger must still work — the seeding fixes
    /// the load-time edge without disarming the control.
    #[test]
    fn trigger_still_fires_on_a_real_press_after_load() {
        let mut plugin = EtherTap::default();
        let (test_cmd_tx, test_cmd_rx) = crossbeam_channel::bounded(8);
        plugin.cmd_tx = test_cmd_tx;

        set_param(&plugin, "disconnect", 1.0);
        plugin.adopt_restored_state();

        let mut ctx = MockProcessContext::new(120.0, false);
        let mut buffer = make_buffer();
        let mut aux = make_aux();
        // First buffer consumes the restored value and self-resets the param;
        // the edge detector still holds the pre-reset `true`, so it takes one
        // more buffer to observe the reset and re-arm.
        plugin.process(&mut buffer, &mut aux, &mut ctx);
        plugin.process(&mut buffer, &mut aux, &mut ctx);
        let _ = test_cmd_rx.try_iter().count();

        // Now a real press.
        set_param(&plugin, "disconnect", 1.0);
        plugin.process(&mut buffer, &mut aux, &mut ctx);
        assert!(
            test_cmd_rx
                .try_iter()
                .any(|c| matches!(c, NetworkCommand::Disconnect)),
            "a genuine rising edge must still dispatch Disconnect"
        );
    }

    /// The worker polls an atom for auto_reconnect. initialize() must mirror
    /// the restored parameter into it, because a suspended or bypassed track
    /// may never call process() to do it.
    #[test]
    fn restored_auto_reconnect_reaches_the_worker_atom() {
        let mut plugin = EtherTap::default();
        set_param(&plugin, "auto_reconnect", 1.0);
        assert!(
            !plugin.params.auto_reconnect_atom.load(Ordering::Relaxed),
            "precondition: the atom starts at the default, not the restored value"
        );

        plugin.adopt_restored_state();

        assert!(
            plugin.params.auto_reconnect_atom.load(Ordering::Relaxed),
            "the worker-facing atom must reflect the restored parameter"
        );
    }

    /// A session saved while connected restores `is_connected = true`. On load
    /// the plugin is offline, and the first buffer must say so rather than
    /// waiting for a transition that will never come.
    #[test]
    fn restored_connected_status_is_overwritten_on_first_buffer() {
        let mut plugin = EtherTap::default();
        set_param(&plugin, "is_connected", 1.0);
        assert!(plugin.params.is_connected.value(), "precondition");

        plugin.adopt_restored_state();
        let mut ctx = MockProcessContext::new(120.0, false);
        let mut buffer = make_buffer();
        let mut aux = make_aux();
        plugin.process(&mut buffer, &mut aux, &mut ctx);

        assert!(
            !plugin.params.is_connected.value(),
            "a restored connected status must be corrected on the first buffer"
        );
        assert_eq!(plugin.params.sync_status.value(), SyncStatus::Offline);
    }

    /// `initialize()` runs on every host activation, not only at load — a DAW
    /// calls it again whenever the track is deactivated and re-enabled. The
    /// worker keeps its socket across that, so the plugin must not announce
    /// itself offline, and must not re-fire the connect-time slot audit for a
    /// connection it never lost.
    #[test]
    fn host_reactivation_keeps_a_live_connection() {
        let mut plugin = EtherTap::default();
        let (test_cmd_tx, test_cmd_rx) = crossbeam_channel::bounded(16);
        plugin.cmd_tx = test_cmd_tx;

        let mut ctx = MockProcessContext::new(120.0, false);
        let mut buffer = make_buffer();
        let mut aux = make_aux();

        // Load, then the worker connects and the first buffer picks it up.
        plugin.adopt_restored_state();
        plugin.process(&mut buffer, &mut aux, &mut ctx);
        plugin.conn_status.store(true, Ordering::Release);
        plugin.process(&mut buffer, &mut aux, &mut ctx);
        assert!(plugin.params.is_connected.value(), "precondition: online");
        assert!(
            test_cmd_rx
                .try_iter()
                .any(|c| matches!(c, NetworkCommand::AuditSlots)),
            "precondition: the genuine connect fires the slot audit"
        );

        // Host deactivates and re-enables the track: initialize() runs again.
        plugin.adopt_restored_state();
        plugin.process(&mut buffer, &mut aux, &mut ctx);

        assert!(
            plugin.params.is_connected.value(),
            "reactivation must not report a still-live connection as offline"
        );
        assert!(
            !test_cmd_rx
                .try_iter()
                .any(|c| matches!(c, NetworkCommand::AuditSlots)),
            "no connect transition happened, so no second slot audit"
        );
    }

    /// The slot audit is expensive (eight round trips) and only runs on
    /// connect. Restoring the last one lets the editor show the console's slot
    /// map on load instead of eight blanks.
    #[test]
    fn restored_slot_audit_repopulates_the_slot_map() {
        let mut plugin = EtherTap::default();
        // Slot 1 = DLY (BPM-compatible), slot 2 = a reverb (not), rest silent.
        *plugin.params.last_slot_types.lock() = [
            10,
            1,
            i32::MIN,
            i32::MIN,
            i32::MIN,
            i32::MIN,
            i32::MIN,
            i32::MIN,
        ];

        plugin.adopt_restored_state();

        assert_eq!(
            plugin.slot_types[0].load(Ordering::Relaxed),
            10,
            "restored slot types must reach the shared array"
        );
        assert_eq!(
            plugin.occupied_slots.load(Ordering::Acquire),
            0b0000_0011,
            "both answering slots count as occupied"
        );
        assert_eq!(
            plugin.compatible_slots.load(Ordering::Acquire),
            0b0000_0001,
            "only the delay is BPM-compatible"
        );
    }

    /// A session that never saw a console must not claim slots exist.
    #[test]
    fn empty_restored_slot_audit_leaves_the_map_blank() {
        let mut plugin = EtherTap::default();
        plugin.adopt_restored_state();
        assert_eq!(plugin.occupied_slots.load(Ordering::Acquire), 0);
        assert_eq!(plugin.compatible_slots.load(Ordering::Acquire), 0);
    }

    // ── NetworkStatus::ScanDone ─────────────────────────────────────────────

    /// Sending NetworkStatus::ScanDone through the status channel must update
    /// scan_completed_ts so the editor can show when the last LAN scan finished.
    #[test]
    fn scan_done_status_updates_scan_completed_ts() {
        let mut plugin = EtherTap::default();
        // Inject a fresh status channel that carries a ScanDone message.
        let (status_tx, status_rx) = crossbeam_channel::bounded(8);
        status_tx.send(NetworkStatus::ScanDone).unwrap();
        plugin.status_rx = status_rx;

        // Pre-set a sentinel so the assertion is unambiguous: if process() does
        // NOT update scan_completed_ts, the sentinel survives and the check fails.
        // Using u64::MAX avoids a trivially-true `after >= 0` comparison.
        plugin.scan_completed_ts.store(u64::MAX, Ordering::Relaxed);

        let mut ctx = MockProcessContext::new(120.0, false);
        let mut buffer = make_buffer();
        let mut aux = make_aux();
        plugin.process(&mut buffer, &mut aux, &mut ctx);
        let after = plugin.scan_completed_ts.load(Ordering::Relaxed);
        assert!(
            after != u64::MAX,
            "ScanDone must overwrite scan_completed_ts with now_ms() (sentinel still present)"
        );
    }

    /// Sending NetworkStatus::SlotScanDone when reconnect_sync_pending is set
    /// and BPM has settled must trigger an immediate dispatch (reconnect sync).
    #[test]
    fn slot_scan_done_with_reconnect_pending_dispatches() {
        let mut plugin = EtherTap::default();
        let (status_tx, status_rx) = crossbeam_channel::bounded(8);
        status_tx.send(NetworkStatus::SlotScanDone).unwrap();
        plugin.status_rx = status_rx;

        // Wire a test cmd channel so we can observe what dispatch() sends.
        let (test_cmd_tx, test_cmd_rx) = crossbeam_channel::bounded(8);
        plugin.cmd_tx = test_cmd_tx;

        // Pre-conditions: BPM settled, pending reconnect sync.
        // Keep conn_status false so the "just connected" transition path doesn't
        // re-arm reconnect_sync_pending during the same process() call.
        plugin.reconnect_sync_pending = true;
        plugin.last_bpm = 120.0;
        plugin.bpm_is_settling = false;
        // slot 1 compatible, fallback slot = 1, single-slot mode (compat_mask=0 path).
        plugin.compatible_slots.store(0, Ordering::Relaxed);
        plugin.params.fx_slot_atom.store(1, Ordering::Relaxed);

        let mut ctx = MockProcessContext::new(120.0, false);
        let mut buffer = make_buffer();
        let mut aux = make_aux();
        plugin.process(&mut buffer, &mut aux, &mut ctx);

        // reconnect_sync_pending must be cleared after dispatch.
        assert!(
            !plugin.reconnect_sync_pending,
            "SlotScanDone must clear reconnect_sync_pending"
        );
        // The dispatch must have sent a SyncNow command to the network worker.
        let cmd = test_cmd_rx
            .try_recv()
            .expect("SlotScanDone dispatch must send at least one NetworkCommand");
        assert!(
            matches!(cmd, NetworkCommand::SyncNow { .. }),
            "SlotScanDone dispatch must send SyncNow, got: {cmd:?}"
        );
    }

    // ── dispatch() filter edge cases ───────────────────────────────────────

    /// When all_slots is enabled and a compatible slot has not yet been audited
    /// (slot_types = i32::MIN), the slot must be included in the dispatch.
    #[test]
    fn dispatch_all_slots_unaudited_slot_is_included() {
        let mut plugin = EtherTap::default();
        let (test_tx, test_rx) = crossbeam_channel::bounded(8);
        plugin.cmd_tx = test_tx;

        plugin.params.all_slots_atom.store(true, Ordering::Relaxed);
        // slot 1 is compatible (bit 0 set), all filter bits enabled.
        plugin.compatible_slots.store(0x01, Ordering::Relaxed);
        plugin.params.fx_type_filter.store(0x7F, Ordering::Relaxed);
        // slot_types[0] defaults to i32::MIN ("not yet audited") → include path.
        plugin.dispatch(120.0, false);

        let cmd = test_rx
            .try_recv()
            .expect("unaudited compatible slot must produce a SyncNow command");
        assert!(
            matches!(cmd, NetworkCommand::SyncNow { slot: 1, .. }),
            "dispatch must target slot 1 for the unaudited-include path, got: {cmd:?}"
        );
        assert!(
            test_rx.try_recv().is_err(),
            "only one compatible slot (slot 1) — no extra commands expected"
        );
    }

    /// When all_slots is enabled and a slot has a non-BPM-compatible type (no
    /// bit in fx_type_to_bit), the slot is included ("unknown type: include").
    #[test]
    fn dispatch_all_slots_unknown_type_is_included() {
        let mut plugin = EtherTap::default();
        let (test_tx, test_rx) = crossbeam_channel::bounded(8);
        plugin.cmd_tx = test_tx;

        plugin.params.all_slots_atom.store(true, Ordering::Relaxed);
        plugin.compatible_slots.store(0x01, Ordering::Relaxed);
        plugin.params.fx_type_filter.store(0x7F, Ordering::Relaxed);
        // Type 1 (AMBI reverb) has no fx_type_to_bit entry → None → include.
        plugin.slot_types[0].store(1, Ordering::Relaxed);
        plugin.dispatch(120.0, false);

        let cmd = test_rx
            .try_recv()
            .expect("unknown-type compatible slot must produce a SyncNow command");
        assert!(
            matches!(cmd, NetworkCommand::SyncNow { slot: 1, .. }),
            "dispatch must include unknown type (AMBI reverb) in all-slots mode, got: {cmd:?}"
        );
    }

    /// `SlotScanDone` fires while `bpm_is_settling=true` — reconnect auto-sync must
    /// NOT dispatch because the current BPM is still mid-transition. Dispatching at
    /// this point would set the mixer to a stale (wrong) tempo. The pending flag must
    /// stay set so the settle handler picks it up once the BPM stabilises.
    #[test]
    fn slot_scan_done_does_not_dispatch_while_settling() {
        let mut plugin = EtherTap::default();
        let (status_tx, status_rx) = crossbeam_channel::bounded(8);
        status_tx.send(NetworkStatus::SlotScanDone).unwrap();
        plugin.status_rx = status_rx;

        let (test_cmd_tx, test_cmd_rx) = crossbeam_channel::bounded(8);
        plugin.cmd_tx = test_cmd_tx;

        plugin.reconnect_sync_pending = true;
        plugin.last_bpm = 120.0;
        plugin.bpm_is_settling = true; // ← BPM still changing: stale dispatch would be wrong
        plugin.last_conn_status = true; // skip just-connected transition

        let mut ctx = MockProcessContext::new(120.0, false);
        let mut buffer = make_buffer();
        let mut aux = make_aux();
        plugin.process(&mut buffer, &mut aux, &mut ctx);

        assert!(
            plugin.reconnect_sync_pending,
            "reconnect_sync_pending must stay set while BPM is still settling"
        );
        assert!(
            test_cmd_rx.try_recv().is_err(),
            "SlotScanDone must not dispatch while BPM is settling (stale-BPM guard)"
        );
    }

    /// `SlotScanDone` fires before any BPM has been seen (`last_bpm == 0.0`) — the
    /// reconnect auto-sync must NOT dispatch because we have nothing valid to send.
    /// The pending flag stays set and the settle or next-process path will dispatch
    /// once `last_bpm` is populated.
    #[test]
    fn slot_scan_done_does_not_dispatch_when_no_bpm_known() {
        let mut plugin = EtherTap::default();
        let (status_tx, status_rx) = crossbeam_channel::bounded(8);
        status_tx.send(NetworkStatus::SlotScanDone).unwrap();
        plugin.status_rx = status_rx;

        let (test_cmd_tx, test_cmd_rx) = crossbeam_channel::bounded(8);
        plugin.cmd_tx = test_cmd_tx;

        plugin.reconnect_sync_pending = true;
        plugin.last_bpm = 0.0; // ← no BPM seen yet
        plugin.bpm_is_settling = false;
        plugin.last_conn_status = true;

        let mut ctx = MockProcessContext::new(120.0, false);
        let mut buffer = make_buffer();
        let mut aux = make_aux();
        plugin.process(&mut buffer, &mut aux, &mut ctx);

        assert!(
            plugin.reconnect_sync_pending,
            "reconnect_sync_pending must stay set when no BPM is known"
        );
        assert!(
            test_cmd_rx.try_recv().is_err(),
            "SlotScanDone must not dispatch when last_bpm is zero (no BPM known)"
        );
    }

    #[test]
    fn on_change_retry_dispatches_when_hw_mismatched() {
        let mut plugin = EtherTap::default();
        let (_, fresh_rx) = crossbeam_channel::bounded(8);
        plugin.status_rx = fresh_rx;

        // Wire a test cmd channel so we can observe what dispatch() sends.
        let (test_cmd_tx, test_cmd_rx) = crossbeam_channel::bounded(8);
        plugin.cmd_tx = test_cmd_tx;

        plugin.conn_status.store(true, Ordering::Relaxed);
        // Set last_conn_status = true to skip the "just-connected" transition
        // (which would enqueue an AuditSlots command before our SyncNow).
        plugin.last_conn_status = true;
        plugin.on_change_retry_pending = true;
        plugin.on_change_retry_bpm = 100.0;
        plugin.on_change_retry_hard_reset = false;
        // Fallback slot = 1 (single-slot mode with no compatible slots → fallback).
        plugin.params.fx_slot_atom.store(1, Ordering::Relaxed);
        // now_ms() is Instant-based elapsed time (not wall clock). Initialize
        // the epoch via a first call, then sleep 2.1s so the next process()
        // call sees now_ms() >= 2100 → now - 0 >= 2000ms → retry fires.
        let _ = now_ms();
        std::thread::sleep(std::time::Duration::from_millis(2100));
        plugin.on_change_last_retry_ms = 0;
        // hardware_float = 0 (no readback yet) so is_matched stays false, retry
        // condition passes (hw float != expected for 100 BPM).
        plugin.hardware_float.store(0u32, Ordering::Relaxed);
        plugin.hr_pending = false;

        let mut ctx = MockProcessContext::new(100.0, true);
        let mut buffer = make_buffer();
        let mut aux = make_aux();
        plugin.process(&mut buffer, &mut aux, &mut ctx);

        assert!(
            plugin.on_change_last_retry_ms > 0,
            "retry fired: on_change_last_retry_ms must be updated"
        );
        // The retry must have dispatched a SyncNow to the network worker —
        // checking the timing state alone cannot distinguish "retry path ran"
        // from "retry branch taken but dispatch() silently dropped the command."
        let cmd = test_cmd_rx
            .try_recv()
            .expect("on_change retry must dispatch a SyncNow command");
        assert!(
            matches!(cmd, NetworkCommand::SyncNow { slot: 1, bpm } if (bpm - 100.0).abs() < 0.01),
            "retry dispatch must carry the retry BPM (100.0), got: {cmd:?}"
        );
    }
}
