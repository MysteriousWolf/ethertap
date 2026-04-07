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
mod network;
mod osc;
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

    // ── Shared atomics (audio writes, editor reads) ───────────────────────
    conn_status: Arc<AtomicBool>,
    /// Millisecond timestamp of the last TX packet (drives the TX LED).
    tx_activity_ts: Arc<AtomicU64>,
    /// Millisecond timestamp of the last RX packet (drives the RX LED).
    rx_activity_ts: Arc<AtomicU64>,
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
    all_slots_mode:    Arc<AtomicBool>,
    scan_targets:      Arc<Mutex<Vec<network::DeviceInfo>>>,
    /// Name and model of the currently connected device, from /info responses.
    connected_device:  Arc<Mutex<(String, String)>>,

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

    // ── Force-sync rising-edge detection (for VST automation) ─────────────
    prev_force_sync:       bool,
    prev_connect_to_last:  bool,
    prev_disconnect_param: bool,
    prev_force_sync_rate:  bool,
    prev_force_sync_phase: bool,
    prev_force_sync_both:  bool,
}

impl Default for EtherTap {
    fn default() -> Self {
        #[cfg(feature = "standalone")]
        mock::start_once();

        let params = Arc::new(EtherTapParams::default());

        let hardware_float = Arc::new(AtomicU32::new(0u32));
        let host_bpm = Arc::new(AtomicU32::new(0u32));
        let force_sync_trigger = Arc::new(AtomicBool::new(false));
        let force_rate_trigger = Arc::new(AtomicBool::new(false));
        let conn_status = Arc::new(AtomicBool::new(false));
        let tx_activity_ts = Arc::new(AtomicU64::new(0));
        let rx_activity_ts = Arc::new(AtomicU64::new(0));
        let compatible_slots = Arc::new(Mutex::new(Vec::new()));
        let occupied_slots   = Arc::new(Mutex::new(Vec::<u8>::new()));
        let all_slots_mode   = Arc::new(AtomicBool::new(true));
        let scan_targets     = Arc::new(Mutex::new(Vec::<network::DeviceInfo>::new()));
        let connected_device = Arc::new(Mutex::new((String::new(), String::new())));

        let (cmd_tx, cmd_rx) = crossbeam_channel::bounded::<NetworkCommand>(64);
        let (status_tx, status_rx) = crossbeam_channel::bounded::<NetworkStatus>(64);

        let worker = NetworkWorker::new(
            cmd_rx,
            status_tx,
            params.fx_slot.clone(),
            hardware_float.clone(),
        );
        std::thread::Builder::new()
            .name("ethertap-net".into())
            .spawn(move || worker.run())
            .expect("failed to spawn network worker thread");

        Self {
            params,
            cmd_tx,
            status_rx,
            conn_status,
            tx_activity_ts,
            rx_activity_ts,
            hardware_float,
            host_bpm,
            force_sync_trigger,
            force_rate_trigger,
            compatible_slots,
            occupied_slots,
            all_slots_mode,
            scan_targets,
            connected_device,
            last_bpm: 0.0,
            bpm_change_ts: 0,
            bpm_is_settling: false,
            hr_pending: false,
            hr_target_beat: 0.0,
            last_pos_beats: 0.0,
            prev_force_sync:       false,
            prev_connect_to_last:  false,
            prev_disconnect_param: false,
            prev_force_sync_rate:  false,
            prev_force_sync_phase: false,
            prev_force_sync_both:  false,
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
        _config: &BufferConfig,
        _context: &mut impl InitContext<Self>,
    ) -> bool {
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
        _buffer: &mut Buffer,
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
                NetworkStatus::SlotScan { compatible, occupied } => {
                    *self.compatible_slots.lock() = compatible;
                    *self.occupied_slots.lock()   = occupied;
                }
                NetworkStatus::TargetsFound(targets) => {
                    *self.scan_targets.lock() = targets;
                }
                NetworkStatus::DeviceIdentified { name, model } => {
                    *self.connected_device.lock() = (name, model);
                }
            }
        }

        // ── 2. Sample transport ───────────────────────────────────────────
        let transport = context.transport();
        let bpm = transport.tempo.unwrap_or(120.0);
        let pos_beats = transport.pos_beats().unwrap_or(0.0);
        let playing = transport.playing;

        // ── 3. Publish host BPM for the editor ───────────────────────────
        self.host_bpm.store((bpm as f32).to_bits(), Ordering::Relaxed);

        // ── 4. BPM settle detection ("On Change" modes) ──────────────────
        if self.last_bpm > 0.0 && (bpm - self.last_bpm).abs() > 0.01 {
            // BPM just changed — restart settle timer.
            self.bpm_change_ts = now_ms();
            self.bpm_is_settling = true;
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
                    } else if rate_mode == SyncMode::OnChange {
                        self.dispatch(bpm, false);
                    }
                }
            }
        }

        // ── 5. Quantised Hard Reset gate ──────────────────────────────────
        if self.hr_pending && playing && pos_beats >= self.hr_target_beat {
            self.dispatch(bpm, true);
            self.hr_pending = false;
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
        }

        // ── 7. Force triggers — param automation edges + UI atomics ─────────

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
            hardware_float: self.hardware_float.clone(),
            host_bpm: self.host_bpm.clone(),
            force_sync_trigger: self.force_sync_trigger.clone(),
            force_rate_trigger: self.force_rate_trigger.clone(),
            compatible_slots:  self.compatible_slots.clone(),
            occupied_slots:    self.occupied_slots.clone(),
            all_slots_mode:    self.all_slots_mode.clone(),
            scan_targets:      self.scan_targets.clone(),
            connected_device:  self.connected_device.clone(),
            cmd_tx: self.cmd_tx.clone(),
        });
        editor::create(data)
    }
}

// ─── Private helpers ─────────────────────────────────────────────────────────

impl EtherTap {
    /// Dispatch a sync command.  `hard_reset = true` → `HardReset`, else `SyncNow`.
    ///
    /// When "all slots" mode is active every compatible slot receives the
    /// command; falls back to the single selected slot when none are known yet.
    fn dispatch(&self, bpm: f64, hard_reset: bool) {
        let slots: Vec<u8> = if self.all_slots_mode.load(Ordering::Relaxed) {
            let cs = self.compatible_slots.lock();
            if cs.is_empty() {
                vec![*self.params.fx_slot.lock()]
            } else {
                cs.clone()
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
