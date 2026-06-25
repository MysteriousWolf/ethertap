use std::sync::{
    atomic::{AtomicBool, AtomicU32, AtomicU8},
    Arc,
};

use nih_plug::prelude::*;
use nih_plug_iced::{Font, IcedState};
use parking_lot::Mutex;

// ─── UI font ─────────────────────────────────────────────────────────────────

/// Monospace font used for the MIDI clock stats row (and any other
/// fixed-width text).  Swap the `bytes` path to change the font family;
/// the file must be a valid TTF/OTF at compile time.
pub const MONO_FONT: Font = Font::External {
    name: "JetBrains Mono",
    bytes: include_bytes!("../assets/JetBrainsMono-Regular.ttf"),
};

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

// ─── Sync status enum ────────────────────────────────────────────────────────

/// Host-visible summary of the rate-sync state, derived from `conn_status`,
/// `in_sync`, `bpm_is_settling`, `on_change_retry_pending`, and `hr_pending`.
///
/// Precedence (highest first): Offline (not connected) > Synced (matched) >
/// Syncing (settling / retry pending / Hard Reset armed) > Connected (idle).
#[derive(Enum, Debug, PartialEq, Eq, Clone, Copy)]
pub enum SyncStatus {
    /// Not connected to a mixer.
    #[id = "offline"]
    Offline,
    /// Connected, idle, not yet matched to the host BPM.
    #[id = "connected"]
    Connected,
    /// Connected; a sync is in flight (BPM settling, retry pending, or Hard
    /// Reset armed and waiting for its quantised beat boundary).
    #[id = "syncing"]
    Syncing,
    /// Connected and the hardware delay time matches the host BPM.
    #[id = "synced"]
    Synced,
}

// ─── MIDI clock PPQ enum ─────────────────────────────────────────────────────

/// Pulses-per-quarter-note options for MIDI clock output.
#[derive(Enum, Debug, PartialEq, Eq, Clone, Copy)]
pub enum Ppq {
    #[id = "ppq3"]
    P3,
    #[id = "ppq4"]
    P4,
    #[id = "ppq6"]
    P6,
    #[id = "ppq8"]
    P8,
    #[id = "ppq12"]
    P12,
    #[id = "ppq16"]
    P16,
    #[id = "ppq24"]
    P24,
    #[id = "ppq32"]
    P32,
    #[id = "ppq48"]
    P48,
    #[id = "ppq96"]
    P96,
}

impl Ppq {
    pub fn to_u8(self) -> u8 {
        match self {
            Ppq::P3 => 3,
            Ppq::P4 => 4,
            Ppq::P6 => 6,
            Ppq::P8 => 8,
            Ppq::P12 => 12,
            Ppq::P16 => 16,
            Ppq::P24 => 24,
            Ppq::P32 => 32,
            Ppq::P48 => 48,
            Ppq::P96 => 96,
        }
    }

    pub fn from_u8(v: u8) -> Self {
        match v {
            3 => Ppq::P3,
            4 => Ppq::P4,
            6 => Ppq::P6,
            8 => Ppq::P8,
            12 => Ppq::P12,
            16 => Ppq::P16,
            24 => Ppq::P24,
            32 => Ppq::P32,
            48 => Ppq::P48,
            96 => Ppq::P96,
            _ => Ppq::P24, // default
        }
    }
}

impl std::fmt::Display for Ppq {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.to_u8())
    }
}

// ─── Parameter block ─────────────────────────────────────────────────────────

/// All VST3-visible and persisted state for EtherTap.
///
/// Fields marked `#[id]`      are exposed to the host for automation.
/// Fields marked `#[persist]` survive DAW session reloads (non-param data).
/// Fields with `_atom` suffix are worker-thread-facing backing stores, mirrored
/// from their corresponding `#[id]` params at the top of each `process()` call.
#[derive(Params)]
pub struct EtherTapParams {
    // ── Editor window state ──────────────────────────────────────────────
    pub editor_state: Arc<IcedState>,

    // ── Network configuration (persisted, not automatable) ───────────────
    #[persist = "target-ip"]
    pub target_ip: Arc<Mutex<String>>,

    #[persist = "target-port"]
    pub target_port: Arc<Mutex<u16>>,

    /// (name, model) of the last connected device, from its `/info` reply.
    /// The network worker verifies this identity on auto-reconnect and
    /// rescans for the device if a different console answers at the
    /// persisted address (e.g. DHCP reassigned the IP).
    #[persist = "last-device"]
    pub last_device: Arc<Mutex<(String, String)>>,

    // ── Auto-reconnect (automatable + worker-facing atom) ────────────────
    /// Opt-in: connect to the last mixer on plugin load and auto-retarget
    /// via identity-verified rescan when the connection drops. Default OFF —
    /// EtherTap never fires network traffic without explicit user consent
    /// ("no surprise automation").
    #[id = "auto_reconnect"]
    pub auto_reconnect: BoolParam,

    pub auto_reconnect_atom: Arc<AtomicBool>,

    // ── FX slot (automatable + worker-facing atom) ───────────────────────
    /// Selected FX slot (1–8).  Exposed as an automatable parameter so a DAW
    /// preset or automation lane can switch the active delay slot.
    #[id = "fx_slot"]
    pub fx_slot: IntParam,

    /// Backing store polled by the network worker (mirrored from `fx_slot`
    /// at the top of each `process()` call — not persisted independently).
    pub fx_slot_atom: Arc<AtomicU8>,

    // ── FX type filter bitmask (worker-facing atom, driven by 7 BoolParams) ─
    /// Hot-path bitmask: bit n set ↔ fx type n is enabled for Auto broadcast.
    /// Bit→type: 0=DLY, 1=3TAP, 2=4TAP, 3=D/RV, 4=D/CR, 5=D/FL, 6=MODD.
    /// Written atomically in `process()` from the 7 `fx_filter_*` params.
    pub fx_type_filter: Arc<AtomicU32>,

    // ── FX type filter params (automatable, drive fx_type_filter atom) ───
    #[id = "fx_filter_dly"]
    pub fx_filter_dly: BoolParam,
    #[id = "fx_filter_3tap"]
    pub fx_filter_3tap: BoolParam,
    #[id = "fx_filter_4tap"]
    pub fx_filter_4tap: BoolParam,
    #[id = "fx_filter_drv"]
    pub fx_filter_drv: BoolParam,
    #[id = "fx_filter_dcr"]
    pub fx_filter_dcr: BoolParam,
    #[id = "fx_filter_dfl"]
    pub fx_filter_dfl: BoolParam,
    #[id = "fx_filter_modd"]
    pub fx_filter_modd: BoolParam,

    // ── MIDI clock enabled (automatable + worker-facing atom) ────────────
    #[id = "midi_clock_enabled"]
    pub midi_clock_enabled: BoolParam,

    pub midi_clock_enabled_atom: Arc<AtomicBool>,

    // ── MIDI clock PPQ (automatable + worker-facing atom) ────────────────
    #[id = "midi_clock_ppq"]
    pub midi_clock_ppq: EnumParam<Ppq>,

    pub midi_clock_ppq_atom: Arc<AtomicU8>,

    // ── MIDI output device (persisted, not automatable — string type) ────
    #[persist = "midi-out-device"]
    pub midi_out_device: Arc<Mutex<Option<String>>>,

    // ── MIDI auto-connect (automatable + worker-facing atom) ─────────────
    #[id = "midi_auto_connect"]
    pub midi_auto_connect: BoolParam,

    pub midi_auto_connect_atom: Arc<AtomicBool>,

    // ── Rate Sync mode ───────────────────────────────────────────────────
    #[id = "rate_sync_mode"]
    pub rate_sync_mode: EnumParam<SyncMode>,

    // ── Phase Sync mode ──────────────────────────────────────────────────
    #[id = "phase_sync_mode"]
    pub phase_sync_mode: EnumParam<SyncMode>,

    // ── Automation triggers (rising edge = fire once) ────────────────────
    #[id = "connect_to_last"]
    pub connect_to_last: BoolParam,

    #[id = "disconnect"]
    pub disconnect: BoolParam,

    /// Momentary: fire a rate-only sync (delay time, no phase reset).
    #[id = "force_sync_rate"]
    pub force_sync_rate: BoolParam,

    /// Momentary: fire a Hard Reset (phase + rate).
    #[id = "force_sync_phase"]
    pub force_sync_phase: BoolParam,

    /// Momentary: re-query the mixer's FX slots (effect type + BPM audit).
    #[id = "audit_slots"]
    pub audit_slots: BoolParam,

    /// Dispatch target: all compatible slots (true) vs. the selected
    /// `fx_slot` only (false).  Worker reads the mirrored atom.
    #[id = "all_slots"]
    pub all_slots: BoolParam,

    /// Backing store read by `dispatch()` (mirrored from `all_slots` at the
    /// top of each `process()` call).
    pub all_slots_atom: Arc<AtomicBool>,

    // ── Read-only status (plugin → host) ─────────────────────────────────
    #[id = "is_connected"]
    pub is_connected: BoolParam,

    #[id = "is_matched"]
    pub is_matched: BoolParam,

    /// Summary sync state: Offline / Connected / Syncing / Synced. See
    /// [`SyncStatus`] for the precedence rules.
    #[id = "sync_status"]
    pub sync_status: EnumParam<SyncStatus>,

    /// Mirrors `hr_pending` — a quantised Hard Reset is armed and waiting
    /// for its target beat boundary.
    #[id = "phase_reset_pending"]
    pub phase_reset_pending: BoolParam,

    /// Hardware delay time converted to BPM (`20.0 / hardware_float`).
    /// `0.0` means no telemetry has been received yet.
    #[id = "hardware_bpm"]
    pub hardware_bpm: FloatParam,

    /// Number of FX slots (0–8) reported BPM-compatible by the last slot
    /// audit, i.e. `compatible_slots.count_ones()`.
    #[id = "compatible_slot_count"]
    pub compatible_slot_count: IntParam,

    /// Mirrors the MIDI clock worker's open-connection flag for the
    /// selected physical output.
    #[id = "midi_bridge_connected"]
    pub midi_bridge_connected: BoolParam,
}

impl Default for EtherTapParams {
    fn default() -> Self {
        Self {
            #[cfg(not(feature = "standalone"))]
            editor_state: IcedState::from_size(360, 280),
            #[cfg(feature = "standalone")]
            editor_state: IcedState::from_size(500, 620),
            target_ip: Arc::new(Mutex::new(if cfg!(feature = "standalone") {
                "127.0.0.1".to_owned()
            } else {
                "192.168.1.100".to_owned()
            })),
            target_port: Arc::new(Mutex::new(10023)),
            last_device: Arc::new(Mutex::new((String::new(), String::new()))),
            auto_reconnect: BoolParam::new("Auto Reconnect", false),
            auto_reconnect_atom: Arc::new(AtomicBool::new(false)),
            fx_slot: IntParam::new("FX Slot", 1, IntRange::Linear { min: 1, max: 8 }),
            fx_slot_atom: Arc::new(AtomicU8::new(1)),
            fx_type_filter: Arc::new(AtomicU32::new(0x7F)),
            fx_filter_dly: BoolParam::new("FX Filter: Delay", true),
            fx_filter_3tap: BoolParam::new("FX Filter: 3 Tap", true),
            fx_filter_4tap: BoolParam::new("FX Filter: 4 Tap", true),
            fx_filter_drv: BoolParam::new("FX Filter: Delay+Reverb", true),
            fx_filter_dcr: BoolParam::new("FX Filter: Delay+Chorus", true),
            fx_filter_dfl: BoolParam::new("FX Filter: Delay+Flanger", true),
            fx_filter_modd: BoolParam::new("FX Filter: Mod Delay", true),
            midi_clock_enabled: BoolParam::new("MIDI Clock Enabled", true),
            midi_clock_enabled_atom: Arc::new(AtomicBool::new(true)),
            midi_clock_ppq: EnumParam::new("MIDI Clock PPQ", Ppq::P24),
            midi_clock_ppq_atom: Arc::new(AtomicU8::new(24)),
            midi_out_device: Arc::new(Mutex::new(None)),
            midi_auto_connect: BoolParam::new("MIDI Auto-Connect", false),
            midi_auto_connect_atom: Arc::new(AtomicBool::new(false)),
            rate_sync_mode: EnumParam::new("Rate Sync Mode", SyncMode::OnChange),
            phase_sync_mode: EnumParam::new("Phase Sync Mode", SyncMode::Manual),
            connect_to_last: BoolParam::new("Connect to Last", false),
            disconnect: BoolParam::new("Disconnect", false),
            force_sync_rate: BoolParam::new("Force Sync Rate", false),
            force_sync_phase: BoolParam::new("Force Sync Phase", false),
            audit_slots: BoolParam::new("Audit Slots", false),
            all_slots: BoolParam::new("All Slots", true),
            all_slots_atom: Arc::new(AtomicBool::new(true)),
            is_connected: BoolParam::new("Is Connected", false),
            is_matched: BoolParam::new("Is Matched", false),
            sync_status: EnumParam::new("Sync Status", SyncStatus::Offline),
            phase_reset_pending: BoolParam::new("Phase Reset Pending", false),
            hardware_bpm: FloatParam::new(
                "Hardware BPM",
                0.0,
                FloatRange::Linear {
                    min: 0.0,
                    max: 999.0,
                },
            ),
            compatible_slot_count: IntParam::new(
                "Compatible Slot Count",
                0,
                IntRange::Linear { min: 0, max: 8 },
            ),
            midi_bridge_connected: BoolParam::new("MIDI Bridge Connected", false),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ppq_to_u8_covers_all_variants() {
        let cases = [
            (Ppq::P3, 3u8),
            (Ppq::P4, 4),
            (Ppq::P6, 6),
            (Ppq::P8, 8),
            (Ppq::P12, 12),
            (Ppq::P16, 16),
            (Ppq::P24, 24),
            (Ppq::P32, 32),
            (Ppq::P48, 48),
            (Ppq::P96, 96),
        ];
        for (variant, expected) in cases {
            assert_eq!(variant.to_u8(), expected);
        }
    }

    #[test]
    fn ppq_from_u8_covers_all_variants() {
        let known: &[(u8, Ppq)] = &[
            (3, Ppq::P3),
            (4, Ppq::P4),
            (6, Ppq::P6),
            (8, Ppq::P8),
            (12, Ppq::P12),
            (16, Ppq::P16),
            (24, Ppq::P24),
            (32, Ppq::P32),
            (48, Ppq::P48),
            (96, Ppq::P96),
        ];
        for &(v, expected) in known {
            assert_eq!(Ppq::from_u8(v), expected);
        }
        // Unknown value defaults to P24.
        assert_eq!(Ppq::from_u8(0), Ppq::P24);
        assert_eq!(Ppq::from_u8(255), Ppq::P24);
    }

    #[test]
    fn ppq_roundtrip_to_u8_from_u8() {
        for &v in &[3u8, 4, 6, 8, 12, 16, 24, 32, 48, 96] {
            assert_eq!(Ppq::from_u8(v).to_u8(), v);
        }
    }

    #[test]
    fn ppq_display_matches_to_u8() {
        for v in [3u8, 4, 6, 8, 12, 16, 24, 32, 48, 96] {
            let p = Ppq::from_u8(v);
            assert_eq!(format!("{p}"), format!("{v}"));
        }
    }
}
