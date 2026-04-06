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
use params::EtherTapParams;

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
    compatible_slots: Arc<Mutex<Vec<u8>>>,

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
    prev_force_sync: bool,
}

impl Default for EtherTap {
    fn default() -> Self {
        #[cfg(feature = "standalone")]
        mock::start_once();

        let params = Arc::new(EtherTapParams::default());

        let hardware_float = Arc::new(AtomicU32::new(0u32));
        let host_bpm = Arc::new(AtomicU32::new(0u32));
        let force_sync_trigger = Arc::new(AtomicBool::new(false));
        let conn_status = Arc::new(AtomicBool::new(false));
        let tx_activity_ts = Arc::new(AtomicU64::new(0));
        let rx_activity_ts = Arc::new(AtomicU64::new(0));
        let compatible_slots = Arc::new(Mutex::new(Vec::new()));

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
            compatible_slots,
            last_bpm: 0.0,
            bpm_change_ts: 0,
            bpm_is_settling: false,
            hr_pending: false,
            hr_target_beat: 0.0,
            last_pos_beats: 0.0,
            prev_force_sync: false,
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
        main_input_channels: NonZeroU32::new(2),
        main_output_channels: NonZeroU32::new(2),
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
        _layout: &AudioIOLayout,
        _config: &BufferConfig,
        _context: &mut impl InitContext<Self>,
    ) -> bool {
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
                NetworkStatus::CompatibleSlots(slots) => {
                    *self.compatible_slots.lock() = slots;
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

        // ── 4. BPM settle detection ("Sync on Change") ───────────────────
        if self.last_bpm > 0.0 && (bpm - self.last_bpm).abs() > 0.01 {
            // BPM just changed — restart settle timer.
            self.bpm_change_ts = now_ms();
            self.bpm_is_settling = true;
        } else if self.bpm_is_settling {
            let elapsed = now_ms().saturating_sub(self.bpm_change_ts);
            if elapsed >= SETTLE_MS {
                self.bpm_is_settling = false;
                if self.params.sync_on_change.value() && playing {
                    if self.params.hard_reset_auto.value() {
                        // Quantise the Hard Reset to the next beat boundary.
                        let next = pos_beats.ceil();
                        self.hr_target_beat = if next > pos_beats { next } else { next + 1.0 };
                        self.hr_pending = true;
                    } else {
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
        if self.params.sync_continuous.value() && playing {
            if pos_beats.floor() > self.last_pos_beats.floor() {
                self.dispatch(bpm, false); // continuous = plain sync, no Hard Reset
            }
        }
        if playing {
            self.last_pos_beats = pos_beats;
        } else {
            self.last_pos_beats = -1.0; // reset so the first beat after play fires
        }

        // ── 7. Force Sync — dual trigger (param rising edge + UI atomic) ──
        let force_param = self.params.force_sync.value();
        let force_trigger = self.force_sync_trigger.swap(false, Ordering::AcqRel);
        if (force_param && !self.prev_force_sync) || force_trigger {
            // ForceSync is always immediate (no beat quantisation) + Hard Reset.
            self.dispatch(bpm, true);
        }
        self.prev_force_sync = force_param;

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
            compatible_slots: self.compatible_slots.clone(),
            cmd_tx: self.cmd_tx.clone(),
        });
        editor::create(data)
    }
}

// ─── Private helpers ─────────────────────────────────────────────────────────

impl EtherTap {
    /// Dispatch a sync command.  `hard_reset = true` → `HardReset`, else `SyncNow`.
    fn dispatch(&self, bpm: f64, hard_reset: bool) {
        let slot = *self.params.fx_slot.lock();
        if hard_reset {
            let _ = self.cmd_tx.try_send(NetworkCommand::HardReset { slot, bpm });
        } else {
            let _ = self.cmd_tx.try_send(NetworkCommand::SyncNow { slot, bpm });
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
