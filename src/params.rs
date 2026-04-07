use std::sync::Arc;

use nih_plug::prelude::*;
use nih_plug_iced::IcedState;
use parking_lot::Mutex;

// ─── Sync mode enum ──────────────────────────────────────────────────────────

/// Governs when a sync or phase-reset fires automatically.
#[derive(Enum, Debug, PartialEq, Eq, Clone, Copy)]
pub enum SyncMode {
    /// Never fire automatically — manual trigger only.
    #[id = "manual"]
    Manual,
    /// Fire when the host BPM has settled for ≥ 500 ms.
    #[id = "on_change"]
    OnChange,
    /// Fire on every quarter-note beat boundary while transport is playing.
    #[id = "continuous"]
    Continuous,
}

// ─── Parameter block ─────────────────────────────────────────────────────────

/// All VST3-visible and persisted state for EtherTap.
///
/// Fields marked `#[id]`     are exposed to the host for automation.
/// Fields marked `#[persist]` survive DAW session reloads.
#[derive(Params)]
pub struct EtherTapParams {
    // ── Editor window state ──────────────────────────────────────────────
    #[persist = "editor-state"]
    pub editor_state: Arc<IcedState>,

    // ── Network configuration (persisted, not automatable) ───────────────
    #[persist = "target-ip"]
    pub target_ip: Arc<Mutex<String>>,

    #[persist = "target-port"]
    pub target_port: Arc<Mutex<u16>>,

    #[persist = "fx-slot"]
    pub fx_slot: Arc<Mutex<u8>>,

    // ── Rate Sync mode ───────────────────────────────────────────────────
    /// Controls when a plain delay-time (rate) sync fires.
    /// Default: **OnChange** — sync fires when BPM settles.
    #[id = "rate_sync_mode"]
    pub rate_sync_mode: EnumParam<SyncMode>,

    // ── Phase Sync mode ──────────────────────────────────────────────────
    /// Controls when a Hard Reset (phase) fires.
    /// Default: **Manual** — only via the Force Sync button.
    #[id = "phase_sync_mode"]
    pub phase_sync_mode: EnumParam<SyncMode>,

    // ── Automation triggers (rising edge = fire once) ────────────────────
    /// Connect to the last-used target (host → plugin).
    #[id = "connect_to_last"]
    pub connect_to_last: BoolParam,

    /// Disconnect from the current target (host → plugin).
    #[id = "disconnect"]
    pub disconnect: BoolParam,

    /// Momentary: fire a rate-only sync (delay time, no phase reset).
    #[id = "force_sync_rate"]
    pub force_sync_rate: BoolParam,

    /// Momentary: fire a Hard Reset (phase + rate).
    #[id = "force_sync_phase"]
    pub force_sync_phase: BoolParam,

    /// Momentary: fire a Hard Reset (same as force_sync_phase).
    #[id = "force_sync_both"]
    pub force_sync_both: BoolParam,

    /// Legacy momentary trigger kept for existing automation lanes.
    #[id = "force_sync"]
    pub force_sync: BoolParam,

    // ── Read-only status (plugin → host) ─────────────────────────────────
    /// True while the plugin has an active UDP connection to the mixer.
    /// Updated by the GUI thread; read by the host for automation read-back.
    #[id = "is_connected"]
    pub is_connected: BoolParam,

    /// True when host BPM and hardware delay float are within tolerance.
    /// Updated by the GUI thread; read by the host for automation read-back.
    #[id = "is_matched"]
    pub is_matched: BoolParam,
}

impl Default for EtherTapParams {
    fn default() -> Self {
        Self {
            editor_state: IcedState::from_size(360, 200),
            target_ip: Arc::new(Mutex::new(if cfg!(feature = "standalone") {
                "127.0.0.1".to_owned()
            } else {
                "192.168.1.100".to_owned()
            })),
            target_port: Arc::new(Mutex::new(10023)),
            fx_slot: Arc::new(Mutex::new(1)),
            rate_sync_mode: EnumParam::new("Rate Sync Mode", SyncMode::OnChange),
            phase_sync_mode: EnumParam::new("Phase Sync Mode", SyncMode::Manual),
            connect_to_last: BoolParam::new("Connect to Last", false),
            disconnect: BoolParam::new("Disconnect", false),
            force_sync_rate: BoolParam::new("Force Sync Rate", false),
            force_sync_phase: BoolParam::new("Force Sync Phase", false),
            force_sync_both: BoolParam::new("Force Sync Both", false),
            force_sync: BoolParam::new("Force Sync", false),
            is_connected: BoolParam::new("Is Connected", false),
            is_matched: BoolParam::new("Is Matched", false),
        }
    }
}
