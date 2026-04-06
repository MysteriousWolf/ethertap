use std::sync::Arc;

use nih_plug::prelude::*;
use nih_plug_iced::IcedState;
use parking_lot::Mutex;

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

    // ── Sync-mode controls ───────────────────────────────────────────────

    /// Fire a sync whenever the host BPM has been stable for ≥ 500 ms.
    #[id = "sync_on_change"]
    pub sync_on_change: BoolParam,

    /// Fire a plain (no Hard Reset) sync on every quarter-note beat boundary
    /// while the host transport is playing.
    #[id = "sync_continuous"]
    pub sync_continuous: BoolParam,

    // ── Hard-reset mode ──────────────────────────────────────────────────

    /// `false` = **Manual Only** — Hard Reset only via `force_sync`.
    /// `true`  = **Auto + Manual** — Hard Reset also fires when BPM settles
    ///           (quantised to the next beat boundary).
    #[id = "hard_reset_auto"]
    pub hard_reset_auto: BoolParam,

    // ── Momentary trigger ────────────────────────────────────────────────

    /// VST3 automation-visible momentary trigger.  The audio thread fires an
    /// immediate Hard Reset on the rising edge (false → true).
    /// The UI uses [`force_sync_trigger`] instead to avoid the missing
    /// `ProcessContext::set_parameter` limitation in this nih-plug version.
    #[id = "force_sync"]
    pub force_sync: BoolParam,
}

impl Default for EtherTapParams {
    fn default() -> Self {
        Self {
            editor_state: IcedState::from_size(440, 380),
            target_ip: Arc::new(Mutex::new(if cfg!(feature = "standalone") {
                "127.0.0.1".to_owned()
            } else {
                "192.168.1.100".to_owned()
            })),
            target_port: Arc::new(Mutex::new(10023)),
            fx_slot: Arc::new(Mutex::new(1)),
            sync_on_change: BoolParam::new("Sync on Change", false),
            sync_continuous: BoolParam::new("Sync Continuous", false),
            hard_reset_auto: BoolParam::new("Hard Reset Auto", false),
            force_sync: BoolParam::new("Force Sync", false),
        }
    }
}
