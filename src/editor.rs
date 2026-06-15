/// Iced-based editor for EtherTap.
///
/// # Colour palette
/// All colours live in `THEME` (a `Theme::dark()` constant at the bottom of
/// the "Theme" section).  To produce a different skin, copy `dark()`, rename
/// it, and change the `static THEME` assignment — nothing else needs to move.
///
/// Solar Icon Set Bold (PUA U+E900…) is used for all non-text glyphs.
use std::collections::VecDeque;
use std::sync::{
    atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicU64, AtomicU8, Ordering},
    Arc,
};

use nih_plug::prelude::{BoolParam, GuiContext, ParamSetter};
use nih_plug_iced::{
    button, container, create_iced_editor, executor, pick_list, text_input,
    widget::{tooltip, Button, Column, Container, PickList, Row, Space, Text, TextInput},
    Alignment, Background, Color, Command, Element, Font, IcedEditor, Length, Subscription,
    WindowQueue, WindowSubs,
};
use parking_lot::Mutex;

#[cfg(feature = "standalone")]
use crate::params::SyncStatus;
use crate::{
    network::{now_ms, DeviceInfo, NetworkCommand},
    osc,
    params::{EtherTapParams, Ppq, SyncMode, MONO_FONT},
};

// ─── Solar Icons Bold font ───────────────────────────────────────────────────

const SOLAR_BOLD: Font = Font::External {
    name: "Solar Icon Set Bold",
    bytes: include_bytes!("../assets/Solar-Icon-Set_Bold.ttf"),
};

const LOGO_FONT: Font = Font::External {
    name: "JetBrainsMono Bold",
    bytes: include_bytes!("../assets/JetBrainsMono-Bold.ttf"),
};

// ─── Text helper ─────────────────────────────────────────────────────────────
// Every text element in the UI uses MONO_FONT.  `t!(expr)` expands to
// `t!(expr).font(MONO_FONT)` so callers can still chain `.size()`,
// `.color()`, and `.font(SOLAR_BOLD)` (the last overrides for icon glyphs).

macro_rules! t {
    ($s:expr) => {
        Text::new($s).font(MONO_FONT)
    };
}

// ─── Icon codepoints (Solar Icon Set Bold, PUA) ──────────────────────────────

mod icon {
    pub const LINK: &str = "\u{ecf2}"; // si-Link — connected
    pub const LINK_BROKEN: &str = "\u{ecf3}"; // si-Link-Broken — disconnected
    pub const ARROW_RIGHT: &str = "\u{e908}"; // si-Arrow-Right
    pub const ARROW_LEFT: &str = "\u{e905}"; // si-Arrow-Left
    pub const BOLT: &str = "\u{ea50}"; // si-Bolt — force / destructive
    pub const SCAN: &str = "\u{ec8a}"; // si-Scanner
    pub const CLOCK: &str = "\u{ed1c}"; // si-Clock-Circle — MIDI clock
}

// ─── MIDI out device sentinel ─────────────────────────────────────────────────

/// Displayed in the device PickList when no physical device is selected.
const MIDI_OUT_NONE: &str = "\u{2014} None \u{2014}";

// ─── Theme ───────────────────────────────────────────────────────────────────
//
// Edit ONLY the colour values inside `Theme::dark()` to restyle the entire UI.
// Field names are intentionally semantic (not widget-specific) so this block
// reads as a design-token palette.

struct Theme {
    // ── Window ────────────────────────────────────────────────────────────
    bg: Color, // window background

    // ── Surfaces (idle buttons, text inputs) ─────────────────────────────
    surface: Color, // button / input fill
    // Single uniform 1px hairline border tone used wherever a flat surface
    // needs an edge (idle buttons, inputs, picklists). One fill colour +
    // (at most) this hairline, no bevels.
    surface_border: Color,
    muted: Color, // idle / placeholder text

    // ── Selected state (active radio option, focused input) ───────────────
    selected_bg: Color,
    selected_border: Color,
    selected_text: Color,

    // ── Force action (momentary bolt button) ─────────────────────────────
    danger_bg: Color, // amber fill
    danger_border: Color,

    // ── Body text ─────────────────────────────────────────────────────────
    text: Color,     // main body
    text_dim: Color, // labels, secondary

    // ── Status ────────────────────────────────────────────────────────────
    ok: Color,   // green — connected, synced
    err: Color,  // red   — disconnected, drift
    warn: Color, // amber — TX activity, connecting

    // ── Brand accent ─────────────────────────────────────────────────────
    // Used for: logo "ETHER" glyph, RX active dot, bolt icon, focused input.
    accent: Color,

    // ── Section grouping ──────────────────────────────────────────────────
    section_border: Color, // subtle border around grouped controls
    section_bg: Color,     // background tint for grouped controls
    banner_bg: Color,      // dark banner background
    banner_text: Color,    // primary text on dark banner (ETHER/TAP logo)

    // ── Button state fills ────────────────────────────────────────────────
    enabled_bg: Color, // green fill for Enabled buttons
    error_bg: Color,   // red fill for Error/Disconnect buttons

    // ── Inset surfaces (recessed text inputs, sunken panels) ─────────────
    inset_border: Color, // border for recessed elements
    inset_bg: Color,     // background for recessed elements

    // ── Standalone DAW-shell chrome (test harness only, never shipped) ───
    // dark Asiimov palette — black base, gunmetal panels, orange accents,
    // near-white text. Compiled only with the standalone feature: the shipped
    // VST3 never renders DAW chrome, so the fields don't exist there.
    #[cfg(feature = "standalone")]
    daw_chrome_bg: Color, // transport row / DAW I/O footer background
    #[cfg(feature = "standalone")]
    daw_chrome_panel: Color, // subsection panel backgrounds
    #[cfg(feature = "standalone")]
    daw_chrome_border: Color, // panel border + accent (Asiimov orange)
    #[cfg(feature = "standalone")]
    daw_chrome_text: Color, // primary text on the chrome surface
    #[cfg(feature = "standalone")]
    daw_chrome_text_dim: Color, // secondary/label text on the chrome surface
}

const fn rgb(r: u8, g: u8, b: u8) -> Color {
    Color {
        r: r as f32 / 255.0,
        g: g as f32 / 255.0,
        b: b as f32 / 255.0,
        a: 1.0,
    }
}

impl Theme {
    /// Professional audio engineering palette — warm charcoal surfaces with
    /// amber accent and clean status LEDs, inspired by analog mixing consoles.
    const fn dark() -> Self {
        Self {
            // ── Window ──────────────────────────────────────────────────────
            bg: rgb(10, 10, 10), // near-black neutral

            // ── Surfaces ────────────────────────────────────────────────────
            surface: rgb(22, 22, 22), // button fill — one step above cards
            surface_border: rgb(40, 40, 40), // hairline: idle buttons, inputs, picklists
            muted: rgb(75, 75, 75),   // placeholder / disabled text

            // ── Selected / active ────────────────────────────────────────────
            selected_bg: rgb(55, 88, 145), // muted engineering blue, flattened
            selected_border: rgb(55, 88, 145),
            selected_text: rgb(210, 225, 250), // bright on blue

            // ── Force action ───────────────────────────────────────────────────
            danger_bg: rgb(125, 50, 25), // burnt-amber fill, flattened
            danger_border: rgb(125, 50, 25),

            // ── Body text ────────────────────────────────────────────────────
            text: rgb(230, 228, 224),     // warm off-white
            text_dim: rgb(120, 120, 120), // dimmer but readable

            // ── Status ──────────────────────────────────────────────────────
            ok: rgb(65, 185, 75),
            err: rgb(210, 55, 55),
            warn: rgb(220, 170, 50),

            // ── Brand accent ────────────────────────────────────────────────
            accent: rgb(210, 160, 55),

            // ── Section grouping ────────────────────────────────────────────
            section_border: rgb(34, 34, 34),
            section_bg: rgb(16, 16, 16),
            banner_bg: rgb(22, 22, 22), // flat dark banner, one step up from bg
            banner_text: rgb(230, 228, 224), // primary text on dark banner

            // ── Button fills ───────────────────────────────────────────────
            enabled_bg: rgb(25, 60, 35), // dark green, flattened
            error_bg: rgb(65, 22, 22),   // dark red, flattened

            // ── Inset surfaces ──────────────────────────────────────────────
            inset_border: rgb(34, 34, 34),
            inset_bg: rgb(7, 7, 7),

            // ── Standalone DAW-shell chrome ─────────────────────────────────
            #[cfg(feature = "standalone")]
            daw_chrome_bg: rgb(0, 0, 0), // Asiimov black base
            #[cfg(feature = "standalone")]
            daw_chrome_panel: rgb(18, 18, 22), // Asiimov near-black panel
            #[cfg(feature = "standalone")]
            daw_chrome_border: rgb(251, 116, 45), // Asiimov signature orange (#fb742d)
            #[cfg(feature = "standalone")]
            daw_chrome_text: rgb(246, 246, 246), // Asiimov near-white (#f6f6f6)
            #[cfg(feature = "standalone")]
            daw_chrome_text_dim: rgb(165, 163, 170), // dimmed chrome text (readable on black)
        }
    }
}

static THEME: Theme = Theme::dark();

// ─── Pulse window ────────────────────────────────────────────────────────────

const PULSE_MS: u64 = 100;

// ─── Layout constants ─────────────────────────────────────────────────────────

// Inputs, picklists, modal/section cards, and the outer plugin frame.
const BORDER_RADIUS: f32 = 5.0;
// Buttons — larger, TE rounded-key feel.
const BORDER_RADIUS_BTN: f32 = 8.0;
const SPACING_BTN_BASELINE: u16 = 10;
const SPACING_FX_ROW_GAP: u16 = 12;
const SCAN_MODAL_W: u16 = 290;
const MIDI_MODAL_W: u16 = 240;
/// Vertical gap between titled sections in the main column.
const SECTION_GAP: u16 = 3;
/// Inner padding of every section frame: [top, right, bottom, left].
const SECTION_PAD: [u16; 4] = [2, 6, 4, 6];
/// Title size of every section header (MIXER / EFFECTS / MIDI / SYNC).
const SECTION_TITLE_SIZE: u16 = 9;
/// Uniform inset between `PluginFrame`'s hairline border and its content,
/// applied on all 4 sides in both render paths.
const PLUGIN_FRAME_PAD: u16 = 3;

// ─── Button stylesheet ───────────────────────────────────────────────────────

#[derive(Clone, Copy)]
enum BtnKind {
    Idle,
    Active,
    Force,
    Disabled,
    Enabled,
    Error,
}

struct EtherBtn(BtnKind);

impl button::StyleSheet for EtherBtn {
    fn active(&self) -> button::Style {
        match self.0 {
            // Idle — flat fill, hairline border.
            BtnKind::Idle => button::Style {
                background: Some(Background::Color(THEME.surface)),
                border_radius: BORDER_RADIUS_BTN,
                border_width: 1.0,
                border_color: THEME.surface_border,
                text_color: THEME.muted,
                ..Default::default()
            },
            // Active / selected — blue fill, no border (flat highlight).
            BtnKind::Active => button::Style {
                background: Some(Background::Color(THEME.selected_bg)),
                border_radius: BORDER_RADIUS_BTN,
                border_width: 0.0,
                border_color: THEME.selected_border,
                text_color: THEME.selected_text,
                ..Default::default()
            },
            // Force / momentary action — amber fill, no border.
            BtnKind::Force => button::Style {
                background: Some(Background::Color(THEME.danger_bg)),
                border_radius: BORDER_RADIUS_BTN,
                border_width: 0.0,
                border_color: THEME.danger_border,
                text_color: THEME.accent,
                ..Default::default()
            },
            // Disabled / recessed — flat dark fill, hairline border.
            BtnKind::Disabled => button::Style {
                background: Some(Background::Color(THEME.inset_bg)),
                border_radius: BORDER_RADIUS_BTN,
                border_width: 1.0,
                border_color: THEME.surface_border,
                text_color: THEME.surface_border,
                ..Default::default()
            },
            // Enabled / connected — green fill, no border.
            BtnKind::Enabled => button::Style {
                background: Some(Background::Color(THEME.enabled_bg)),
                border_radius: BORDER_RADIUS_BTN,
                border_width: 0.0,
                border_color: THEME.ok,
                text_color: THEME.ok,
                ..Default::default()
            },
            // Error / disconnected — red fill, no border.
            BtnKind::Error => button::Style {
                background: Some(Background::Color(THEME.error_bg)),
                border_radius: BORDER_RADIUS_BTN,
                border_width: 0.0,
                border_color: THEME.err,
                text_color: THEME.err,
                ..Default::default()
            },
        }
    }

    fn hovered(&self) -> button::Style {
        lighten(self.active(), 0.04)
    }

    fn pressed(&self) -> button::Style {
        lighten(self.active(), -0.03)
    }
}

// ─── Text-input stylesheet ────────────────────────────────────────────────────

struct EtherInput;

impl text_input::StyleSheet for EtherInput {
    fn active(&self) -> text_input::Style {
        text_input::Style {
            background: Background::Color(THEME.inset_bg),
            border_radius: BORDER_RADIUS,
            border_width: 1.0,
            border_color: THEME.inset_border,
        }
    }

    fn focused(&self) -> text_input::Style {
        text_input::Style {
            border_color: THEME.accent,
            ..self.active()
        }
    }

    fn hovered(&self) -> text_input::Style {
        text_input::Style {
            border_color: THEME.muted,
            ..self.active()
        }
    }

    fn placeholder_color(&self) -> Color {
        THEME.text_dim
    }
    fn value_color(&self) -> Color {
        THEME.text
    }
    fn selection_color(&self) -> Color {
        THEME.selected_bg
    }
}

/// Dimmed style for IP/port inputs when a connection is active.
#[derive(Clone, Copy)]
enum EtherInputLocked {
    Editable,
    Locked,
}

impl text_input::StyleSheet for EtherInputLocked {
    fn active(&self) -> text_input::Style {
        match self {
            Self::Editable => EtherInput.active(),
            Self::Locked => text_input::Style {
                background: Background::Color(THEME.inset_bg),
                border_radius: BORDER_RADIUS,
                border_width: 1.0,
                border_color: THEME.inset_border,
            },
        }
    }
    fn focused(&self) -> text_input::Style {
        match self {
            Self::Editable => EtherInput.focused(),
            Self::Locked => self.active(),
        }
    }
    fn hovered(&self) -> text_input::Style {
        match self {
            Self::Editable => EtherInput.hovered(),
            Self::Locked => self.active(),
        }
    }
    fn placeholder_color(&self) -> Color {
        match self {
            Self::Editable => THEME.text_dim,
            Self::Locked => THEME.surface_border,
        }
    }
    fn value_color(&self) -> Color {
        match self {
            Self::Editable => THEME.text,
            Self::Locked => THEME.text_dim,
        }
    }
    fn selection_color(&self) -> Color {
        match self {
            Self::Editable => THEME.selected_bg,
            Self::Locked => THEME.bg,
        }
    }
}

// ─── PickList stylesheet ──────────────────────────────────────────────────────

struct PpqPickStyle;

impl pick_list::StyleSheet for PpqPickStyle {
    fn menu(&self) -> pick_list::Menu {
        pick_list::Menu {
            text_color: THEME.text,
            background: Background::Color(THEME.inset_bg),
            border_width: 1.0,
            border_color: THEME.inset_border,
            selected_text_color: THEME.selected_text,
            selected_background: Background::Color(THEME.selected_bg),
        }
    }

    fn active(&self) -> pick_list::Style {
        pick_list::Style {
            text_color: THEME.text,
            placeholder_color: THEME.muted,
            background: Background::Color(THEME.inset_bg),
            border_radius: BORDER_RADIUS,
            border_width: 1.0,
            border_color: THEME.inset_border,
            icon_size: 0.30,
        }
    }

    fn hovered(&self) -> pick_list::Style {
        pick_list::Style {
            border_color: THEME.accent,
            ..self.active()
        }
    }
}

// ─── Container stylesheets ────────────────────────────────────────────────────

/// Card surface used for the scan popup.
struct ModalCard;
impl container::StyleSheet for ModalCard {
    fn style(&self) -> container::Style {
        container::Style {
            background: Some(Background::Color(THEME.section_bg)),
            border_radius: BORDER_RADIUS,
            border_width: 1.0,
            border_color: THEME.section_border,
            text_color: None,
        }
    }
}

/// Ghost button — no background or border, text only.  Used for close (×).
struct GhostBtn;
impl button::StyleSheet for GhostBtn {
    fn active(&self) -> button::Style {
        button::Style {
            background: None,
            border_radius: 0.0,
            border_width: 0.0,
            border_color: Color::TRANSPARENT,
            text_color: THEME.text_dim,
            ..Default::default()
        }
    }
    fn hovered(&self) -> button::Style {
        button::Style {
            text_color: THEME.text,
            ..self.active()
        }
    }
    fn pressed(&self) -> button::Style {
        self.hovered()
    }
}

/// Tooltip background card.
struct TooltipCard;
impl container::StyleSheet for TooltipCard {
    fn style(&self) -> container::Style {
        container::Style {
            background: Some(Background::Color(THEME.section_bg)),
            border_radius: BORDER_RADIUS,
            border_width: 1.0,
            border_color: THEME.inset_border,
            text_color: Some(THEME.text_dim),
        }
    }
}

/// Section card — wraps a group of related controls in a subtle border and
/// slightly darker background to visually cluster them (mixer-style panel).
struct ModSection;
impl container::StyleSheet for ModSection {
    fn style(&self) -> container::Style {
        container::Style {
            background: Some(Background::Color(THEME.section_bg)),
            border_radius: BORDER_RADIUS,
            border_width: 1.0,
            border_color: THEME.section_border,
            text_color: None,
        }
    }
}

/// Single coherent outer frame around the whole plugin surface (banner +
/// MIXER/EFFECTS/MIDI/SYNC sections).  Used in both modes: VST3 fills the
/// window with it; standalone pins it at the true 360×280 VST3 dimensions.
struct PluginFrame;
impl container::StyleSheet for PluginFrame {
    fn style(&self) -> container::Style {
        container::Style {
            background: Some(Background::Color(THEME.bg)),
            border_radius: BORDER_RADIUS,
            border_width: 1.0,
            border_color: THEME.section_border,
            text_color: None,
        }
    }
}

/// Standalone DAW-shell chrome — transport row + "DAW I/O" footer panel.
/// Deliberately cool-toned against `ModSection`/`BannerBg`'s warm neutrals so
/// the framed 360×280 box reads at a glance as "real plugin content" vs.
/// "test-harness scaffolding we built around it".
#[cfg(feature = "standalone")]
struct DawChrome;
#[cfg(feature = "standalone")]
impl container::StyleSheet for DawChrome {
    fn style(&self) -> container::Style {
        container::Style {
            background: Some(Background::Color(THEME.daw_chrome_bg)),
            border_radius: 0.0,
            border_width: 2.0,
            border_color: THEME.daw_chrome_border,
            text_color: None,
        }
    }
}

/// Gunmetal subsection panel — used for transport row and footer interior
/// sections to visually separate them from the black chrome base.
#[cfg(feature = "standalone")]
struct DawPanel;
#[cfg(feature = "standalone")]
impl container::StyleSheet for DawPanel {
    fn style(&self) -> container::Style {
        container::Style {
            background: Some(Background::Color(THEME.daw_chrome_panel)),
            border_radius: 0.0,
            border_width: 1.0,
            border_color: THEME.daw_chrome_border,
            text_color: None,
        }
    }
}

/// Thin divider line for the standalone dimension-ruler overlay — a colored
/// 1-unit-thick bar (horizontal: width Fill / height 1; vertical: the reverse).
#[cfg(feature = "standalone")]
struct DimLine;
#[cfg(feature = "standalone")]
impl container::StyleSheet for DimLine {
    fn style(&self) -> container::Style {
        container::Style {
            background: Some(Background::Color(THEME.daw_chrome_border)),
            border_radius: 0.0,
            border_width: 0.0,
            border_color: THEME.daw_chrome_border,
            text_color: None,
        }
    }
}

/// Full-window dark backdrop behind the scan popup.
struct ModalBackdrop;
impl container::StyleSheet for ModalBackdrop {
    fn style(&self) -> container::Style {
        container::Style {
            background: Some(Background::Color(THEME.bg)),
            border_radius: 0.0,
            border_width: 0.0,
            border_color: THEME.bg,
            text_color: None,
        }
    }
}

/// Top banner with warm-tinted background for logo + status row.
/// Borderless by design — just a coloured band at the top.
struct BannerBg;
impl container::StyleSheet for BannerBg {
    fn style(&self) -> container::Style {
        container::Style {
            background: Some(Background::Color(THEME.banner_bg)),
            border_radius: 0.0,
            border_width: 0.0,
            border_color: Color::TRANSPARENT,
            text_color: None,
        }
    }
}

// ─── Shared data bundle ───────────────────────────────────────────────────────

pub struct EditorData {
    pub params: Arc<EtherTapParams>,
    pub conn_status: Arc<AtomicBool>,
    pub tx_activity_ts: Arc<AtomicU64>,
    pub rx_activity_ts: Arc<AtomicU64>,
    pub midi_clock_activity_ts: Arc<AtomicU64>,
    pub hardware_float: Arc<AtomicU32>,
    pub host_bpm: Arc<AtomicU32>,
    /// Bitmask: bit n set ↔ slot (n+1) compatible. Written by network worker.
    pub compatible_slots: Arc<AtomicU8>,
    /// Bitmask: bit n set ↔ slot (n+1) occupied. Written by network worker.
    pub occupied_slots: Arc<AtomicU8>,
    /// Raw effect type ID per slot (index = slot-1). i32::MIN = not yet queried.
    pub slot_types: Arc<[AtomicI32; 8]>,
    pub scan_targets: Arc<Mutex<Vec<DeviceInfo>>>,
    /// Millisecond timestamp of the last completed scan (0 = never scanned).
    pub scan_completed_ts: Arc<AtomicU64>,
    /// Name and model parsed from /info heartbeat responses.
    pub connected_device: Arc<Mutex<(String, String)>>,
    /// Incremented each time the editor opens a new scan and clears stale
    /// results.  Background scan threads discard their results if this changed.
    pub scan_generation: Arc<AtomicU64>,
    pub cmd_tx: crossbeam_channel::Sender<NetworkCommand>,
    /// Notifies the MIDI clock worker when the user changes the output device.
    pub device_change_tx: crossbeam_channel::Sender<Option<String>>,
    /// Receives MIDI device hot-plug notifications from midi_watcher.
    pub midi_device_rx: Arc<crossbeam_channel::Receiver<Vec<String>>>,
    /// Millisecond timestamp of the last MIDI device-list broadcast (0 = never).
    pub midi_last_update_ts: Arc<AtomicU64>,
    /// True once the initial MIDI device-list broadcast has landed.
    pub midi_has_update: Arc<AtomicBool>,
    /// True when the worker has an active connection to the selected MIDI output.
    pub midi_bridge_connected: Arc<AtomicBool>,
    /// True while the worker is attempting to reconnect to the selected MIDI output.
    pub midi_bridge_connecting: Arc<AtomicBool>,
    /// Rolling timing statistics from the MIDI clock worker.
    pub midi_clock_stats: Arc<crate::midi_clock::AtomicClockStats>,
    /// Cumulative count of MIDI clock messages dropped on the audio thread.
    /// Written lock-free by process(); drained and logged here on each frame.
    pub midi_clock_drop_count: Arc<AtomicU32>,
    /// BPM set by the standalone transport panel (f32 bits).
    pub standalone_bpm: Arc<AtomicU32>,
    /// Play/stop state for standalone mode.
    pub standalone_playing: Arc<AtomicBool>,
    /// Cumulative beat position in standalone mode (f64 bits), written by process().
    /// Only read by the #[cfg(feature = "standalone")] DAW-shell view.
    #[cfg_attr(not(feature = "standalone"), allow(dead_code))]
    pub standalone_pos_beats: Arc<AtomicU64>,
    /// One-shot Stop trigger: editor sets, process() swap(false)-consumes.
    pub standalone_stop_trigger: Arc<AtomicBool>,
}

// ─── Editor entry point ───────────────────────────────────────────────────────

pub fn create(data: Arc<EditorData>) -> Option<Box<dyn nih_plug::prelude::Editor>> {
    create_iced_editor::<EtherTapEditor>(data.params.editor_state.clone(), data)
}

// ─── Editor struct ────────────────────────────────────────────────────────────

struct EtherTapEditor {
    data: Arc<EditorData>,
    context: Arc<dyn GuiContext>,
    ip_state: text_input::State,
    port_state: text_input::State,
    // Rate Sync — 3 radio buttons + force (bolt-only)
    btn_rate_manual: button::State,
    btn_rate_change: button::State,
    btn_rate_cont: button::State,
    btn_rate_force: button::State,
    // Phase Sync — 3 radio buttons + force (bolt-only)
    btn_phase_manual: button::State,
    btn_phase_change: button::State,
    btn_phase_cont: button::State,
    btn_phase_force: button::State,
    // FX row controls
    btn_auto: button::State,
    btn_query: button::State,
    slot_states: [button::State; 8],
    /// Per-effect-type toggles (bit order: Delay/3Tap/4Tap/D+Rev/D+Cho/D+Fln/Mod).
    btn_fx_type: [button::State; 7],
    // Output clock section — toggle button + PPQ pick list + MIDI out device
    btn_clock_toggle: button::State,
    btn_midi_auto_connect: button::State,
    btn_auto_reconnect: button::State,
    pick_ppq: pick_list::State<Ppq>,
    /// Available MIDI output port names — first entry is always the sentinel.
    midi_out_ports: Vec<String>,
    // MIDI device picker modal
    show_midi_picker: bool,
    btn_midi_picker: button::State,
    midi_picker_states: Vec<button::State>,
    // Network scan — btn_scan doubles as the modal close button
    btn_scan: button::State,
    btn_connect: button::State,
    scan_result_states: Vec<button::State>,
    show_scan_results: bool,
    /// ms-since-epoch when the last ScanTargets command was dispatched.
    last_scan_trigger_ms: u64,

    // ── Standalone DAW frame ─────────────────────────────────────────────
    // These fields are only read in #[cfg(feature = "standalone")] view code.
    #[cfg_attr(not(feature = "standalone"), allow(dead_code))]
    standalone_bpm: Arc<AtomicU32>,
    #[cfg_attr(not(feature = "standalone"), allow(dead_code))]
    standalone_playing: Arc<AtomicBool>,
    #[cfg_attr(not(feature = "standalone"), allow(dead_code))]
    standalone_stop_trigger: Arc<AtomicBool>,
    #[cfg_attr(not(feature = "standalone"), allow(dead_code))]
    tap_times: VecDeque<std::time::Instant>,
    /// DAW-chrome widget state, grouped so `view()` can hand the whole bundle
    /// to `daw_shell()` as ONE disjoint `&mut` field borrow alongside the
    /// plugin content's (or a modal's) own field borrows — old-iced
    /// stateful-widget borrow rules forbid a `&mut self` helper method here.
    daw: DawChromeState,
}

/// Widget state for the standalone DAW shell (transport row + param footer).
/// Exists in both builds (update() arms touch `bpm_input_value` regardless of
/// feature); the button states are only read by standalone view code.
#[cfg_attr(not(feature = "standalone"), allow(dead_code))]
struct DawChromeState {
    btn_play_stop: button::State,
    btn_stop: button::State,
    btn_tap: button::State,
    bpm_input: text_input::State,
    bpm_input_value: String,
    // Footer param-mode chips (rate/phase sync mode + force triggers).
    btn_daw_rate_manual: button::State,
    btn_daw_rate_change: button::State,
    btn_daw_rate_cont: button::State,
    btn_daw_phase_manual: button::State,
    btn_daw_phase_change: button::State,
    btn_daw_phase_cont: button::State,
    btn_daw_force_rate: button::State,
    btn_daw_force_phase: button::State,
    /// One state per trigger chip in the PARAMETERS IN footer, indexed in
    /// table order — resized in `daw_shell()` so adding a chip to the table
    /// there is the only change needed for a new param.
    btn_daw_triggers: Vec<button::State>,
}

// ─── Messages ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
enum Message {
    IpEdited(String),
    PortEdited(String),
    SlotSelected(u8),
    SetRateSyncMode(SyncMode),
    SetPhaseSyncMode(SyncMode),
    ForceRateSync,
    ForcePhaseSync,
    QuerySlots,
    ToggleAutoSlots,
    ToggleAutoReconnect,
    /// Flip one bit in the fx_type_filter bitmask (bit = 0..6).
    ToggleFxType(u8),
    /// Toggle MIDI clock output on/off.
    ToggleMidiClock,
    /// Toggle the persisted `midi_auto_connect` param.
    ToggleMidiAutoConnect,
    /// Set MIDI clock pulses per quarter note.
    SetClockPpq(Ppq),
    /// Open/close the MIDI device picker modal dialog.
    ToggleMidiPicker,
    /// Select a MIDI output device from the picker modal.
    SelectMidiDevice(String),
    ScanTargets,
    /// Fired every render frame via `WindowSubs::on_frame`; gated to 5 s.
    OnFrame,
    SelectTarget(String, u16),
    Connect,
    Disconnect,
    // Standalone transport controls (constructed only in --features standalone view code)
    #[cfg_attr(not(feature = "standalone"), allow(dead_code))]
    SetStandaloneBpm(String),
    #[cfg_attr(not(feature = "standalone"), allow(dead_code))]
    ToggleStandalonePlay,
    #[cfg_attr(not(feature = "standalone"), allow(dead_code))]
    StopStandalone,
    #[cfg_attr(not(feature = "standalone"), allow(dead_code))]
    TapTempo,
}

// ─── IcedEditor impl ─────────────────────────────────────────────────────────

impl IcedEditor for EtherTapEditor {
    type Executor = executor::Default;
    type Message = Message;
    type InitializationFlags = Arc<EditorData>;

    fn new(data: Arc<EditorData>, context: Arc<dyn GuiContext>) -> (Self, Command<Message>) {
        let init_bpm = f32::from_bits(data.standalone_bpm.load(Ordering::Relaxed));
        let bpm_input_value = format!("{:.1}", init_bpm);
        let standalone_bpm = data.standalone_bpm.clone();
        let standalone_playing = data.standalone_playing.clone();
        let standalone_stop_trigger = data.standalone_stop_trigger.clone();
        (
            Self {
                data,
                context,
                ip_state: Default::default(),
                port_state: Default::default(),
                btn_rate_manual: Default::default(),
                btn_rate_change: Default::default(),
                btn_rate_cont: Default::default(),
                btn_rate_force: Default::default(),
                btn_phase_manual: Default::default(),
                btn_phase_change: Default::default(),
                btn_phase_cont: Default::default(),
                btn_phase_force: Default::default(),
                btn_auto: Default::default(),
                btn_query: Default::default(),
                btn_fx_type: Default::default(),
                btn_clock_toggle: Default::default(),
                btn_midi_auto_connect: Default::default(),
                btn_auto_reconnect: Default::default(),
                pick_ppq: Default::default(),
                midi_out_ports: vec![MIDI_OUT_NONE.to_string()],
                show_midi_picker: false,
                btn_midi_picker: Default::default(),
                midi_picker_states: Vec::new(),
                slot_states: Default::default(),
                btn_scan: Default::default(),
                btn_connect: Default::default(),
                scan_result_states: Vec::new(),
                show_scan_results: false,
                last_scan_trigger_ms: 0,
                standalone_bpm,
                standalone_playing,
                standalone_stop_trigger,
                tap_times: VecDeque::new(),
                daw: DawChromeState {
                    btn_play_stop: Default::default(),
                    btn_stop: Default::default(),
                    btn_tap: Default::default(),
                    bpm_input: Default::default(),
                    bpm_input_value,
                    btn_daw_rate_manual: Default::default(),
                    btn_daw_rate_change: Default::default(),
                    btn_daw_rate_cont: Default::default(),
                    btn_daw_phase_manual: Default::default(),
                    btn_daw_phase_change: Default::default(),
                    btn_daw_phase_cont: Default::default(),
                    btn_daw_force_rate: Default::default(),
                    btn_daw_force_phase: Default::default(),
                    btn_daw_triggers: Vec::new(),
                },
            },
            Command::none(),
        )
    }

    fn context(&self) -> &dyn GuiContext {
        self.context.as_ref()
    }

    /// Hook `on_frame` so we can gate a periodic rescan to every 5 s.
    fn subscription(&self, window_subs: &mut WindowSubs<Message>) -> Subscription<Message> {
        // on_frame fires every render frame; the actual scan is rate-limited
        // to once every 5 s inside the OnFrame handler.
        window_subs.on_frame = Some(Message::OnFrame);
        Subscription::none()
    }

    fn update(&mut self, _window: &mut WindowQueue, msg: Message) -> Command<Message> {
        match msg {
            Message::IpEdited(s) => {
                // Only update when disconnected — editing while connected is ignored.
                if !self.data.conn_status.load(Ordering::Acquire) {
                    *self.data.params.target_ip.lock() = s;
                }
            }
            Message::PortEdited(s) => {
                if !self.data.conn_status.load(Ordering::Acquire) {
                    if let Ok(port) = s.parse::<u16>() {
                        *self.data.params.target_port.lock() = port;
                    }
                    // invalid input: rejected; params unchanged, TextInput reverts on next frame
                }
            }
            Message::SlotSelected(slot) => {
                let setter = ParamSetter::new(self.context.as_ref());
                setter.begin_set_parameter(&self.data.params.fx_slot);
                setter.set_parameter(&self.data.params.fx_slot, slot as i32);
                setter.end_set_parameter(&self.data.params.fx_slot);
            }
            Message::SetRateSyncMode(mode) => {
                let setter = ParamSetter::new(self.context.as_ref());
                setter.begin_set_parameter(&self.data.params.rate_sync_mode);
                setter.set_parameter(&self.data.params.rate_sync_mode, mode);
                setter.end_set_parameter(&self.data.params.rate_sync_mode);
            }
            Message::SetPhaseSyncMode(mode) => {
                let setter = ParamSetter::new(self.context.as_ref());
                setter.begin_set_parameter(&self.data.params.phase_sync_mode);
                setter.set_parameter(&self.data.params.phase_sync_mode, mode);
                setter.end_set_parameter(&self.data.params.phase_sync_mode);
            }
            Message::ForceRateSync => {
                pulse_param(self.context.as_ref(), &self.data.params.force_sync_rate);
            }
            Message::ForcePhaseSync => {
                pulse_param(self.context.as_ref(), &self.data.params.force_sync_phase);
            }
            Message::QuerySlots => {
                pulse_param(self.context.as_ref(), &self.data.params.audit_slots);
            }
            Message::ToggleAutoSlots => {
                let setter = ParamSetter::new(self.context.as_ref());
                let next = !self.data.params.all_slots.value();
                setter.begin_set_parameter(&self.data.params.all_slots);
                setter.set_parameter(&self.data.params.all_slots, next);
                setter.end_set_parameter(&self.data.params.all_slots);
            }
            Message::ToggleFxType(bit) => {
                let setter = ParamSetter::new(self.context.as_ref());
                toggle_fx_filter_param(&setter, &self.data.params, bit);
            }
            Message::ToggleMidiClock => {
                let setter = ParamSetter::new(self.context.as_ref());
                let next = !self.data.params.midi_clock_enabled.value();
                setter.begin_set_parameter(&self.data.params.midi_clock_enabled);
                setter.set_parameter(&self.data.params.midi_clock_enabled, next);
                setter.end_set_parameter(&self.data.params.midi_clock_enabled);
            }
            Message::ToggleMidiAutoConnect => {
                let setter = ParamSetter::new(self.context.as_ref());
                let next = !self.data.params.midi_auto_connect.value();
                setter.begin_set_parameter(&self.data.params.midi_auto_connect);
                setter.set_parameter(&self.data.params.midi_auto_connect, next);
                setter.end_set_parameter(&self.data.params.midi_auto_connect);
            }
            Message::ToggleAutoReconnect => {
                let setter = ParamSetter::new(self.context.as_ref());
                let next = !self.data.params.auto_reconnect.value();
                setter.begin_set_parameter(&self.data.params.auto_reconnect);
                setter.set_parameter(&self.data.params.auto_reconnect, next);
                setter.end_set_parameter(&self.data.params.auto_reconnect);
            }
            Message::SetClockPpq(ppq) => {
                let setter = ParamSetter::new(self.context.as_ref());
                setter.begin_set_parameter(&self.data.params.midi_clock_ppq);
                setter.set_parameter(&self.data.params.midi_clock_ppq, ppq);
                setter.end_set_parameter(&self.data.params.midi_clock_ppq);
            }
            Message::ToggleMidiPicker => {
                self.show_midi_picker = !self.show_midi_picker;
                // Keep button-state vector in sync with port count (grow or shrink).
                if self.show_midi_picker {
                    self.midi_picker_states
                        .resize_with(self.midi_out_ports.len(), Default::default);
                    self.midi_picker_states.truncate(self.midi_out_ports.len());
                }
            }
            Message::SelectMidiDevice(name) => {
                let device = if name == MIDI_OUT_NONE {
                    None
                } else {
                    Some(name)
                };
                *self.data.params.midi_out_device.lock() = device.clone();
                let _ = self.data.device_change_tx.try_send(device);
                self.show_midi_picker = false;
            }
            Message::ScanTargets => {
                self.show_scan_results = !self.show_scan_results;
                if self.show_scan_results {
                    // Increment the generation BEFORE clearing so any background
                    // scan thread that finishes after the clear will see the
                    // changed generation and discard its (now-stale) results.
                    self.data.scan_generation.fetch_add(1, Ordering::Release);
                    // Clear stale entries from a previous session so the panel
                    // starts fresh; the first scan result arrives within ~600 ms.
                    self.data.scan_targets.lock().clear();
                    if self
                        .data
                        .cmd_tx
                        .try_send(NetworkCommand::ScanTargets)
                        .is_err()
                    {
                        log::warn!("[EtherTap] editor: ScanTargets dropped (worker channel full)");
                    }
                    self.last_scan_trigger_ms = now_ms();
                }
            }
            Message::OnFrame => {
                // Rate-limit: only dispatch a rescan every 5 s while the panel is open.
                if self.show_scan_results {
                    let elapsed = now_ms().saturating_sub(self.last_scan_trigger_ms);
                    if elapsed >= 5_000 {
                        // Don't clear — merge so known devices stay visible.
                        let _ = self.data.cmd_tx.try_send(NetworkCommand::ScanTargets);
                        self.last_scan_trigger_ms = now_ms();
                    }
                }
                // Drain MIDI device notifications from the watcher channel.
                // On macOS these arrive via CoreMIDI callback (no polling);
                // on other platforms the watcher polls internally at 2 s.
                while let Ok(ports) = self.data.midi_device_rx.try_recv() {
                    let mut list = vec![MIDI_OUT_NONE.to_string()];
                    list.extend(
                        ports
                            .iter()
                            .filter(|n| n.as_str() != "EtherTap MIDI Clock")
                            .cloned(),
                    );
                    self.midi_out_ports = list;
                }
                // Drain MIDI clock drop counter written by the audio thread.
                let drops = self.data.midi_clock_drop_count.swap(0, Ordering::Relaxed);
                if drops > 0 {
                    log::warn!(
                        "[EtherTap] {drops} MIDI clock message(s) dropped (worker stalled?)"
                    );
                }
            }
            Message::SelectTarget(ip, port) => {
                *self.data.params.target_ip.lock() = ip.clone();
                *self.data.params.target_port.lock() = port;
                let _ = self
                    .data
                    .cmd_tx
                    .try_send(NetworkCommand::UpdateTarget { ip, port });
                self.show_scan_results = false;
            }
            Message::Connect => {
                // Pulse the connect_to_last trigger param: process() detects the
                // rising edge, sends ConnectToLast (worker reads the persisted
                // ip/port mutexes itself) + AuditSlots, and sets all_slots true —
                // so a host recording automation sees the gesture too.
                pulse_param(self.context.as_ref(), &self.data.params.connect_to_last);
            }
            Message::Disconnect => {
                pulse_param(self.context.as_ref(), &self.data.params.disconnect);
                // Keep connected_device so the header shows the last known name.
            }
            Message::SetStandaloneBpm(s) => {
                if let Ok(v) = s.parse::<f32>() {
                    let clamped = v.clamp(20.0, 300.0);
                    self.standalone_bpm
                        .store(clamped.to_bits(), Ordering::Relaxed);
                    self.daw.bpm_input_value = format!("{:.1}", clamped);
                } else {
                    self.daw.bpm_input_value = s;
                }
            }
            Message::ToggleStandalonePlay => {
                let was = self.standalone_playing.load(Ordering::Relaxed);
                self.standalone_playing.store(!was, Ordering::Relaxed);
            }
            Message::StopStandalone => {
                // One-shot trigger only — process() performs the
                // standalone_playing/standalone_pos_beats reset itself,
                // serialized with its own accumulation logic. Do NOT store
                // those atomics here: a pair of independent cross-thread
                // store()s would race process()'s Relaxed read-modify-write
                // of standalone_pos_beats and risk a clobbered reset.
                self.standalone_stop_trigger.store(true, Ordering::Release);
            }
            Message::TapTempo => {
                let now = std::time::Instant::now();
                const MAX_GAP: std::time::Duration = std::time::Duration::from_secs(2);
                if let Some(&last) = self.tap_times.back() {
                    if now.duration_since(last) > MAX_GAP {
                        self.tap_times.clear();
                    }
                }
                self.tap_times.push_back(now);
                if self.tap_times.len() > 8 {
                    self.tap_times.pop_front();
                }
                if self.tap_times.len() >= 2 {
                    debug_assert!(self.tap_times.len() >= 2);
                    let first = self.tap_times[0];
                    let last = *self.tap_times.back().unwrap();
                    let secs = last.duration_since(first).as_secs_f32()
                        / (self.tap_times.len() - 1) as f32;
                    if secs > 0.0 {
                        let bpm = (60.0 / secs).clamp(20.0, 300.0);
                        self.standalone_bpm.store(bpm.to_bits(), Ordering::Relaxed);
                        self.daw.bpm_input_value = format!("{:.1}", bpm);
                    }
                }
            }
        }
        Command::none()
    }

    fn view(&mut self) -> Element<'_, Message> {
        // ── Read shared state ─────────────────────────────────────────────
        let connected = self.data.conn_status.load(Ordering::Acquire);
        let now = now_ms();
        let tx_on = {
            let ts = self.data.tx_activity_ts.load(Ordering::Relaxed);
            ts > 0 && now.saturating_sub(ts) < PULSE_MS
        };
        let rx_on = {
            let ts = self.data.rx_activity_ts.load(Ordering::Relaxed);
            ts > 0 && now.saturating_sub(ts) < PULSE_MS
        };

        let host_bpm_f = f32::from_bits(self.data.host_bpm.load(Ordering::Acquire));
        let host_float = osc::bpm_to_float(host_bpm_f as f64);
        let hw_float = f32::from_bits(self.data.hardware_float.load(Ordering::Acquire));
        let hw_bpm = osc::float_to_bpm(hw_float);
        let has_hw = hw_float > 0.0001;
        let in_sync = has_hw && (host_float - hw_float).abs() < 0.001;

        let rate_mode = self.data.params.rate_sync_mode.value();
        let phase_mode = self.data.params.phase_sync_mode.value();
        let cur_slot = self.data.params.fx_slot.value() as u8;
        let compat_mask = self.data.compatible_slots.load(Ordering::Acquire);
        let occup_mask = self.data.occupied_slots.load(Ordering::Acquire);
        // Snapshot slot_types from atomics (i32::MIN = not yet queried → None).
        let slot_types: [Option<i32>; 8] = std::array::from_fn(|i| {
            let raw = self.data.slot_types[i].load(Ordering::Relaxed);
            if raw == i32::MIN {
                None
            } else {
                Some(raw)
            }
        });
        let all_mode = self.data.params.all_slots.value();
        let post_audit = compat_mask != 0 || occup_mask != 0;

        // ── Scan popup modal ──────────────────────────────────────────────
        //
        // When open, we return a completely different view (full-window
        // dark card) so the main layout height never changes.
        if self.show_scan_results {
            let scan_targets_snap = self.data.scan_targets.lock().clone();
            // Keep button-state vec in sync with the actual number of discovered
            // devices (may grow as rescans arrive).
            self.scan_result_states
                .resize_with(scan_targets_snap.len(), Default::default);
            self.scan_result_states.truncate(scan_targets_snap.len());
            let completed_ts = self.data.scan_completed_ts.load(Ordering::Relaxed);
            let scanning_now = now_ms().saturating_sub(self.last_scan_trigger_ms) < 1500;

            // ── Title row ────────────────────────────────────────────────────
            let status_str = if completed_ts == 0 {
                "scanning\u{2026}".to_string()
            } else {
                let age_s = now_ms().saturating_sub(completed_ts) as f32 / 1000.0;
                if scanning_now {
                    format!("{:.1}s ago \u{2022} rescanning\u{2026}", age_s)
                } else {
                    format!("{:.1}s ago", age_s)
                }
            };
            let status_color = if scanning_now {
                THEME.warn
            } else {
                THEME.text_dim
            };

            let mut card_col = Column::new()
                .push(
                    Row::new()
                        .push(t!("DISCOVERED DEVICES").size(11).color(THEME.text))
                        .push(Space::with_width(Length::Units(10)))
                        .push(t!(&status_str).size(9).color(status_color))
                        .push(Space::with_width(Length::Fill))
                        .push(
                            Button::new(
                                &mut self.btn_scan,
                                t!("\u{00d7}").size(16).color(THEME.text_dim),
                            )
                            .on_press(Message::ScanTargets)
                            .style(GhostBtn)
                            .padding([0, 4]),
                        )
                        .align_items(Alignment::Center),
                )
                .push(Space::with_height(Length::Units(6)))
                .spacing(4);

            if scan_targets_snap.is_empty() && completed_ts == 0 {
                card_col = card_col.push(
                    t!("Waiting for responses\u{2026}")
                        .size(11)
                        .color(THEME.text_dim),
                );
            } else {
                for (state, dev) in self
                    .scan_result_states
                    .iter_mut()
                    .zip(scan_targets_snap.iter())
                {
                    let name_line = dev.display_name();

                    // Primary (preferred) address — brighter than alt routes.
                    let lat_str = dev
                        .latency_ms
                        .map_or("\u{2014}".into(), |ms| format!("{:.1} ms", ms));
                    let direct = dev.all_addrs.first().map(|(_, _, d)| *d).unwrap_or(false);
                    let path_str = if direct { "direct" } else { "routed" };
                    let addr_line = format!("{}  {}  {}", dev.ip, lat_str, path_str);

                    let mut entry = Column::new()
                        .push(t!(name_line).size(11).color(THEME.text))
                        .push(t!(addr_line).size(9).color(THEME.muted))
                        .spacing(2);

                    // Alt IPs — dimmer than the preferred route.
                    for (alt_ip, alt_lat, alt_direct) in dev.all_addrs.iter().skip(1) {
                        let alt_lat_str =
                            alt_lat.map_or("\u{2014}".into(), |ms| format!("{:.1} ms", ms));
                        let alt_path = if *alt_direct { "direct" } else { "routed" };
                        let alt_line = format!("{} (alt)  {}  {}", alt_ip, alt_lat_str, alt_path);
                        entry = entry.push(t!(alt_line).size(9).color(THEME.text_dim));
                    }

                    card_col = card_col.push(
                        Button::new(state, entry)
                            .on_press(Message::SelectTarget(dev.ip.clone(), dev.port))
                            .style(EtherBtn(BtnKind::Idle))
                            .padding([5, 8])
                            .width(Length::Fill),
                    );
                }
            }

            let card = Container::new(card_col)
                .padding(12)
                .style(ModalCard)
                .width(Length::Units(SCAN_MODAL_W));

            // Backdrop fills the plugin frame only — in standalone the DAW
            // chrome (transport/footer) stays visible and interactive.
            let backdrop = Container::new(card)
                .width(Length::Fill)
                .height(Length::Fill)
                .center_x()
                .center_y()
                .style(ModalBackdrop);

            #[cfg(feature = "standalone")]
            return daw_shell(
                backdrop.into(),
                &mut self.daw,
                &self.data,
                connected,
                in_sync,
            );
            #[cfg(not(feature = "standalone"))]
            return Container::new(backdrop)
                .width(Length::Fill)
                .height(Length::Fill)
                .style(PluginFrame)
                .into();
        }

        // ── MIDI device picker modal ──────────────────────────────────────
        //
        // Overlaid full-window card listing every available output, similar to
        // the network scan popup.
        if self.show_midi_picker {
            // Re-sync button states if port list grew since last open.
            if self.midi_out_ports.len() > self.midi_picker_states.len() {
                self.midi_picker_states
                    .resize_with(self.midi_out_ports.len(), Default::default);
            }

            // ── Status row ──────────────────────────────────────────────────
            //
            // Mirrors the mixer scan modal's status-string style: "Xs ago" +
            // a state-dependent color. macOS uses CoreMIDI notifications
            // (event-driven, no polling); other platforms poll on a fixed
            // interval, so show a countdown to the next scan.
            let last_update = self.data.midi_last_update_ts.load(Ordering::Relaxed);
            let (midi_status_str, midi_status_color) =
                if !self.data.midi_has_update.load(Ordering::Relaxed) {
                    ("waiting for devices\u{2026}".to_string(), THEME.warn)
                } else {
                    let age_s = now_ms().saturating_sub(last_update) as f32 / 1000.0;
                    if cfg!(target_os = "macos") {
                        (
                            format!("updated {:.1}s ago \u{2022} live (event-driven)", age_s),
                            THEME.text_dim,
                        )
                    } else {
                        let next_in =
                            (crate::midi_watcher::POLL_INTERVAL_SECS as f32 - age_s).max(0.0);
                        (
                            format!(
                                "updated {:.1}s ago \u{2022} next scan in {:.1}s",
                                age_s, next_in
                            ),
                            THEME.text_dim,
                        )
                    }
                };

            let mut picker_col = Column::new()
                .push(
                    Row::new()
                        .push(t!("MIDI OUTPUT").size(11).color(THEME.text))
                        .push(Space::with_width(Length::Units(10)))
                        .push(t!(&midi_status_str).size(9).color(midi_status_color))
                        .push(Space::with_width(Length::Fill))
                        .push(
                            Button::new(
                                &mut self.btn_midi_picker,
                                t!("\u{00d7}").size(16).color(THEME.text_dim),
                            )
                            .on_press(Message::ToggleMidiPicker)
                            .style(GhostBtn)
                            .padding([0, 4]),
                        )
                        .align_items(Alignment::Center),
                )
                .push(Space::with_height(Length::Units(6)))
                .spacing(4);

            let port_snapshot = self.midi_out_ports.clone();
            for (state, port_name) in self.midi_picker_states.iter_mut().zip(port_snapshot.iter()) {
                let btn_name = if *port_name == MIDI_OUT_NONE {
                    "None".to_string()
                } else {
                    port_name.clone()
                };
                picker_col = picker_col.push(
                    Button::new(state, t!(&btn_name).size(11).color(THEME.text))
                        .on_press(Message::SelectMidiDevice(port_name.clone()))
                        .style(EtherBtn(BtnKind::Idle))
                        .padding([5, 8])
                        .width(Length::Fill),
                );
            }

            let card = Container::new(picker_col)
                .padding(12)
                .style(ModalCard)
                .width(Length::Units(MIDI_MODAL_W));

            // Same frame-scoped backdrop treatment as the scan modal above.
            let backdrop = Container::new(card)
                .width(Length::Fill)
                .height(Length::Fill)
                .center_x()
                .center_y()
                .style(ModalBackdrop);

            #[cfg(feature = "standalone")]
            return daw_shell(
                backdrop.into(),
                &mut self.daw,
                &self.data,
                connected,
                in_sync,
            );
            #[cfg(not(feature = "standalone"))]
            return Container::new(backdrop)
                .width(Length::Fill)
                .height(Length::Fill)
                .style(PluginFrame)
                .into();
        }

        // ── Logo + device info + status header ────────────────────────────
        //
        // Layout:  ETHERTAP  [fill]  [icon] device-name  [fill]  TX TX  RX RX
        let conn_color = if connected { THEME.ok } else { THEME.err };
        let ck_on = {
            let ts = self.data.midi_clock_activity_ts.load(Ordering::Relaxed);
            ts > 0 && now.saturating_sub(ts) < PULSE_MS
        };
        let tx_color = if tx_on { THEME.warn } else { THEME.text_dim };
        let rx_color = if rx_on { THEME.accent } else { THEME.text_dim };
        let ck_color = if ck_on { THEME.ok } else { THEME.text_dim };

        let target_ip = self.data.params.target_ip.lock().clone();
        let target_port = *self.data.params.target_port.lock();
        let device_label = {
            let (name, model) = self.data.connected_device.lock().clone();
            if !name.is_empty() || !model.is_empty() {
                let dev = DeviceInfo {
                    ip: target_ip.clone(),
                    port: target_port,
                    name,
                    model,
                    latency_ms: None,
                    all_addrs: vec![],
                };
                dev.display_name()
            } else if connected {
                format!("{}:{}", target_ip, target_port)
            } else {
                "Disconnected".to_string()
            }
        };

        // LOGO_FONT (JetBrainsMono Bold) gives the logo a chunky console-inspired
        // weight that contrasts against the regular MONO_FONT body text.
        // The entire row sits inside a warm-tinted banner container.
        // Connection indicator uses a colored dot (●/○) matching TX/RX/CK style.
        let header = Container::new(
            Row::new()
                .push(t!("ETHER").size(28).font(LOGO_FONT).color(THEME.accent))
                .push(t!("TAP").size(28).font(LOGO_FONT).color(THEME.banner_text))
                .push(Space::with_width(Length::Fill))
                .push(
                    t!(if connected { "●" } else { "○" })
                        .size(10)
                        .color(conn_color),
                )
                .push(Space::with_width(Length::Units(4)))
                .push(t!(&device_label).size(11).color(if connected {
                    THEME.text
                } else {
                    THEME.muted
                }))
                .push(Space::with_width(Length::Fill))
                .push(t!(if tx_on { "●" } else { "○" }).size(8).color(tx_color))
                .push(Space::with_width(Length::Units(2)))
                .push(t!("TX").size(10).color(tx_color))
                .push(Space::with_width(Length::Units(8)))
                .push(t!(if rx_on { "●" } else { "○" }).size(8).color(rx_color))
                .push(Space::with_width(Length::Units(2)))
                .push(t!("RX").size(10).color(rx_color))
                .push(Space::with_width(Length::Units(8)))
                .push(t!(if ck_on { "●" } else { "○" }).size(8).color(ck_color))
                .push(Space::with_width(Length::Units(2)))
                .push(t!("CK").size(10).color(ck_color))
                .align_items(Alignment::Center),
        )
        .padding([4, 10]) // slim vertical, horizontal inset for centering
        .style(BannerBg);

        // ── Network config + scan + connect ──────────────────────────────
        let input_style = if connected {
            EtherInputLocked::Locked
        } else {
            EtherInputLocked::Editable
        };
        // Single source of truth: read display values from params each frame.
        let ip_val = self.data.params.target_ip.lock().clone();
        let port_val = self.data.params.target_port.lock().to_string();
        let ip_input: Element<'_, Message> =
            TextInput::new(&mut self.ip_state, "IP address", &ip_val, Message::IpEdited)
                .size(11)
                .font(MONO_FONT)
                .padding(4)
                .width(Length::FillPortion(3))
                .style(input_style)
                .into();
        let port_input: Element<'_, Message> =
            TextInput::new(&mut self.port_state, "Port", &port_val, Message::PortEdited)
                .size(11)
                .font(MONO_FONT)
                .padding(4)
                .width(Length::FillPortion(2))
                .style(input_style)
                .into();

        let scan_btn = {
            let icon_color = if connected {
                THEME.surface_border
            } else {
                THEME.text_dim
            };
            let inner = Row::new()
                .push(t!(icon::SCAN).size(11).font(SOLAR_BOLD).color(icon_color))
                .push(Space::with_width(Length::Units(4)))
                .push(t!("Scan").size(10).color(icon_color))
                .align_items(Alignment::Center);
            let btn = Button::new(&mut self.btn_scan, inner)
                .style(EtherBtn(if connected {
                    BtnKind::Disabled
                } else {
                    BtnKind::Idle
                }))
                .padding([4, 8]);
            if connected {
                btn
            } else {
                btn.on_press(Message::ScanTargets)
            }
        };

        // Content-sized so it adapts to text length; BtnKind variants handle
        // the text color automatically via the stylesheet.
        let conn_btn = if connected {
            Button::new(&mut self.btn_connect, t!("Disconnect").size(10))
                .on_press(Message::Disconnect)
                .style(EtherBtn(BtnKind::Enabled))
                .padding([4, 6])
        } else {
            Button::new(&mut self.btn_connect, t!("Connect").size(10))
                .on_press(Message::Connect)
                .style(EtherBtn(BtnKind::Error))
                .padding([4, 6])
        };

        // Persisted `auto_reconnect` toggle — same visual pattern as the MIDI
        // auto-connect toggle. ON: reconnect to the last mixer at load and
        // retarget by device identity when the address moves.
        let auto_reconnect_on = self.data.params.auto_reconnect.value();
        let auto_reconnect_btn = Button::new(
            &mut self.btn_auto_reconnect,
            Row::new()
                .push(t!(if auto_reconnect_on { "●" } else { "○" }).size(9).color(
                    if auto_reconnect_on {
                        THEME.ok
                    } else {
                        THEME.muted
                    },
                ))
                .push(Space::with_width(Length::Units(4)))
                .push(t!("Auto").size(10))
                .align_items(Alignment::Center),
        )
        .on_press(Message::ToggleAutoReconnect)
        .style(EtherBtn(if auto_reconnect_on {
            BtnKind::Enabled
        } else {
            BtnKind::Idle
        }))
        .padding([4, 8]);

        let net_row = Row::new()
            .push(ip_input)
            .push(t!("  :  ").size(11).color(THEME.text_dim))
            .push(port_input)
            .push(Space::with_width(Length::Units(8)))
            .push(scan_btn)
            .push(Space::with_width(Length::Units(4)))
            .push(conn_btn)
            .push(Space::with_width(Length::Units(4)))
            .push(auto_reconnect_btn)
            .align_items(Alignment::Center);

        // ── Slot selector ─────────────────────────────────────────────────
        //
        // Each slot column: [button, gap(2), type label].
        // "All" and "Query" reserve a matching spacer below so button text
        // baselines align across the row regardless of label presence.

        let slot_cols = self.slot_states.iter_mut().zip(1u8..=8u8).fold(
            Row::new()
                .spacing(2)
                .width(Length::Fill)
                .align_items(Alignment::Center),
            |row, (state, slot)| {
                let is_compat = !post_audit || compat_mask & (1 << (slot - 1)) != 0;
                let is_sel = !all_mode && slot == cur_slot && is_compat;
                let is_all_sel = all_mode && compat_mask & (1 << (slot - 1)) != 0;

                let kind = if !is_compat {
                    BtnKind::Disabled
                } else if is_sel || is_all_sel {
                    BtnKind::Active
                } else {
                    BtnKind::Idle
                };
                let text_color = match kind {
                    BtnKind::Active => THEME.selected_text,
                    BtnKind::Disabled => THEME.surface_border,
                    _ => THEME.muted,
                };
                let btn = Button::new(
                    state,
                    Container::new(t!(slot.to_string()).size(11).color(text_color)),
                )
                .style(EtherBtn(kind))
                .padding([4, 8]);
                let btn = if is_compat && !all_mode {
                    btn.on_press(Message::SlotSelected(slot))
                } else {
                    btn
                };

                // Resolve the short type label (e.g. "DLY", "GEQ2", "···").
                let (label_text, label_color) = if !post_audit {
                    ("\u{00b7}\u{00b7}\u{00b7}", THEME.surface_border)
                } else {
                    let type_id = slot_types[(slot - 1) as usize];
                    let name = type_id.map_or("···", |t| crate::osc::fx_type_short(t, slot));
                    let color = if compat_mask & (1 << (slot - 1)) != 0 {
                        THEME.ok
                    } else if occup_mask & (1 << (slot - 1)) != 0 {
                        THEME.warn
                    } else {
                        THEME.text_dim
                    };
                    (name, color)
                };

                let slot_col = Column::new()
                    .push(btn)
                    .push(Space::with_height(Length::Units(2)))
                    .push(t!(label_text).size(8).color(label_color))
                    .align_items(Alignment::Center);

                // Tooltip on hover: full effect name at the same size as the UI font.
                let long_name: &'static str = if post_audit {
                    slot_types[(slot - 1) as usize]
                        .map(|t| crate::osc::fx_type_long(t, slot))
                        .unwrap_or("")
                } else {
                    ""
                };

                let slot_elem: Element<'_, Message> = if !long_name.is_empty() {
                    tooltip::Tooltip::new(slot_col, long_name, tooltip::Position::Bottom)
                        .size(11)
                        .gap(2)
                        .padding(4)
                        .style(TooltipCard)
                        .into()
                } else {
                    slot_col.into()
                };

                let slot_elem: Element<'_, Message> = Container::new(slot_elem)
                    .width(Length::FillPortion(1))
                    .center_x()
                    .into();

                row.push(slot_elem)
            },
        );

        // Slot buttons row — the 8 numbered slots now use FillPortion for
        // equal-width distribution across the available space.

        // Query button prepended to the slot row (line 1).
        // Wrapped in a Column with a bottom spacer so its button baseline aligns
        // with the slot columns (which have a sub-label below each button).
        let query_col = Column::new()
            .push(
                Button::new(
                    &mut self.btn_query,
                    Row::new()
                        .push(
                            t!(icon::SCAN)
                                .size(11)
                                .font(SOLAR_BOLD)
                                .color(THEME.text_dim),
                        )
                        .push(Space::with_width(Length::Units(4)))
                        .push(t!("Scan").size(10).color(THEME.text_dim))
                        .align_items(Alignment::Center),
                )
                .on_press(Message::QuerySlots)
                .style(EtherBtn(BtnKind::Idle))
                .padding([4, 8]),
            )
            .push(Space::with_height(Length::Units(SPACING_BTN_BASELINE)))
            .align_items(Alignment::Center);
        let fx_line1 = Row::new()
            .push(query_col)
            .push(Space::with_width(Length::Units(SPACING_FX_ROW_GAP)))
            .push(slot_cols)
            .align_items(Alignment::Start);

        // ── FX type filter toggles (line 2, with All button prepended) ────
        //
        // The All button uses BtnKind::Enabled (green) when active.
        let filter_on: [bool; 7] = [
            self.data.params.fx_filter_dly.value(),
            self.data.params.fx_filter_3tap.value(),
            self.data.params.fx_filter_4tap.value(),
            self.data.params.fx_filter_drv.value(),
            self.data.params.fx_filter_dcr.value(),
            self.data.params.fx_filter_dfl.value(),
            self.data.params.fx_filter_modd.value(),
        ];
        const TYPE_BITS: &[(&str, u8, &str)] = &[
            ("Delay", 0, "Stereo Delay"),
            (
                "3 Tap",
                1,
                "3-Tap Delay — three echoes, delay time at par/01",
            ),
            (
                "4 Tap",
                2,
                "4-Tap Delay — four echoes, delay time at par/01",
            ),
            ("D+Rev", 3, "Delay + Reverb"),
            ("D+Cho", 4, "Delay + Chorus"),
            ("D+Fln", 5, "Delay + Flanger"),
            (
                "Mod",
                6,
                "Modulated Delay — chorused delay, delay time at par/02",
            ),
        ];
        let mut fx_type_row = Row::new()
            .width(Length::Fill)
            .align_items(Alignment::Center);
        for (state, &(name, bit, tip)) in self.btn_fx_type.iter_mut().zip(TYPE_BITS.iter()) {
            let on = filter_on[bit as usize];
            let btn = Button::new(
                state,
                t!(name)
                    .size(9)
                    .color(if on { THEME.selected_text } else { THEME.muted }),
            )
            .on_press(Message::ToggleFxType(bit))
            .style(EtherBtn(if on { BtnKind::Active } else { BtnKind::Idle }))
            .padding([4, 6]);
            let elem: Element<'_, Message> = Container::new(
                tooltip::Tooltip::new(btn, tip, tooltip::Position::Bottom)
                    .size(11)
                    .gap(2)
                    .padding(4)
                    .style(TooltipCard),
            )
            .width(Length::FillPortion(1))
            .center_x()
            .into();
            fx_type_row = fx_type_row.push(elem);
        }
        let fx_line2 = Row::new()
            .push(
                Button::new(
                    &mut self.btn_auto,
                    t!("All")
                        .size(11)
                        .color(if all_mode { THEME.ok } else { THEME.muted }),
                )
                .on_press(Message::ToggleAutoSlots)
                .style(EtherBtn(if all_mode {
                    BtnKind::Enabled
                } else {
                    BtnKind::Idle
                }))
                .padding([4, 8]),
            )
            .push(Space::with_width(Length::Units(SPACING_FX_ROW_GAP)))
            .push(fx_type_row)
            .align_items(Alignment::Center);

        // ── Telemetry (host + mixer on one line) ──────────────────────────
        let host_bpm_str = if host_bpm_f > 0.0 {
            format!("{host_bpm_f:>7.2} BPM")
        } else {
            "     --- BPM".into()
        };
        let host_float_str = if host_bpm_f > 0.0 {
            format!("{host_float:.4}")
        } else {
            "------".into()
        };
        let hw_bpm_str = if has_hw {
            format!("{:>7.2} BPM", hw_bpm)
        } else {
            "     --- BPM".into()
        };
        let hw_float_str = if has_hw {
            format!("{hw_float:.4}")
        } else {
            "------".into()
        };

        let sync_badge: Element<'_, Message> = if !has_hw {
            Row::new()
                .push(t!("○").size(10).color(THEME.text_dim))
                .push(Space::with_width(Length::Units(4)))
                .push(t!("NONE").size(11).color(THEME.text_dim))
                .align_items(Alignment::Center)
                .into()
        } else if in_sync {
            Row::new()
                .push(t!("●").size(10).color(THEME.ok))
                .push(Space::with_width(Length::Units(4)))
                .push(t!("MATCH").size(11).color(THEME.ok))
                .align_items(Alignment::Center)
                .into()
        } else {
            Row::new()
                .push(t!("●").size(10).color(THEME.err))
                .push(Space::with_width(Length::Units(4)))
                .push(t!("DRIFT").size(11).color(THEME.err))
                .align_items(Alignment::Center)
                .into()
        };

        let telem_row = Row::new()
            .push(t!("Host ").size(11).color(THEME.text_dim))
            .push(t!(host_bpm_str).size(11).color(THEME.text))
            .push(Space::with_width(Length::Units(4)))
            .push(
                t!(icon::ARROW_RIGHT)
                    .size(13)
                    .font(SOLAR_BOLD)
                    .color(THEME.text_dim),
            )
            .push(Space::with_width(Length::Units(4)))
            .push(t!(host_float_str).size(11).color(THEME.text))
            .push(Space::with_width(Length::Fill))
            .push(t!("Mixer ").size(11).color(THEME.text_dim))
            .push(t!(hw_bpm_str).size(11).color(THEME.text))
            .push(Space::with_width(Length::Units(4)))
            .push(
                t!(icon::ARROW_LEFT)
                    .size(13)
                    .font(SOLAR_BOLD)
                    .color(THEME.text_dim),
            )
            .push(Space::with_width(Length::Units(4)))
            .push(t!(hw_float_str).size(11).color(THEME.text))
            .push(Space::with_width(Length::Units(10)))
            .push(sync_badge)
            .align_items(Alignment::Center);

        // ── MIDI section (2 rows: device+auto+PPQ / enable+stats) ─────────
        let clock_on = self.data.params.midi_clock_enabled.value();
        let clock_ppq = self.data.params.midi_clock_ppq.value();

        const PPQ_OPTIONS: &[Ppq] = &[
            Ppq::P3,
            Ppq::P4,
            Ppq::P6,
            Ppq::P8,
            Ppq::P12,
            Ppq::P16,
            Ppq::P24,
            Ppq::P32,
            Ppq::P48,
            Ppq::P96,
        ];

        // ── Row 1: device picker (Fill) + auto-connect + PPQ ──────────────
        // ── Row 2: clock enable toggle + jitter stats (right-aligned) ─────
        let current_out_device = self.data.params.midi_out_device.lock().clone();
        let bridge_conn = self.data.midi_bridge_connected.load(Ordering::Acquire);
        let bridge_connecting = self.data.midi_bridge_connecting.load(Ordering::Acquire);
        let device_selected = current_out_device.is_some();

        // MIDI picker button style depends on selection + connection state.
        let midi_picker_kind = if !device_selected {
            BtnKind::Disabled
        } else if bridge_connecting && !bridge_conn {
            BtnKind::Active
        } else if bridge_conn {
            BtnKind::Enabled
        } else {
            BtnKind::Idle
        };

        // No separate status ladder: the device picker button already encodes
        // connection state (SCAN/LINK_BROKEN/LINK icon + BtnKind color) and
        // `midi_clk_btn` text encodes Enable/Enabled/Connecting/Active.

        // Persisted `midi_auto_connect` toggle, placed right of the picker.
        let auto_connect_on = self.data.params.midi_auto_connect.value();
        let midi_auto_connect_btn: Element<'_, Message> = Button::new(
            &mut self.btn_midi_auto_connect,
            Row::new()
                .push(t!(if auto_connect_on { "●" } else { "○" }).size(9).color(
                    if auto_connect_on {
                        THEME.ok
                    } else {
                        THEME.muted
                    },
                ))
                .push(Space::with_width(Length::Units(4)))
                .push(t!("Auto").size(10))
                .align_items(Alignment::Center),
        )
        .on_press(Message::ToggleMidiAutoConnect)
        .style(EtherBtn(if auto_connect_on {
            BtnKind::Enabled
        } else {
            BtnKind::Idle
        }))
        .padding([4, 8])
        .into();

        // MIDI clock output toggle — state-aware text and color.
        let (clk_text, clk_style) = if !clock_on {
            ("Enable", BtnKind::Idle)
        } else if bridge_conn {
            ("Active", BtnKind::Enabled)
        } else if bridge_connecting {
            ("Connecting...", BtnKind::Active)
        } else {
            ("Enabled", BtnKind::Idle)
        };
        let midi_clk_btn: Element<'_, Message> = Button::new(
            &mut self.btn_clock_toggle,
            Row::new()
                .push(t!(icon::CLOCK).size(11).font(SOLAR_BOLD))
                .push(Space::with_width(Length::Units(4)))
                .push(t!(clk_text).size(10))
                .align_items(Alignment::Center),
        )
        .on_press(Message::ToggleMidiClock)
        .style(EtherBtn(clk_style))
        .padding([4, 8])
        .into();

        // Device selector on the left, PPQ on the right.
        // When no device is selected, show a "Select" button with SCAN icon
        // plus a device count hint.
        let (midi_icon, midi_icon_color, selected_display) = if device_selected {
            let icon = if bridge_conn {
                icon::LINK
            } else {
                icon::LINK_BROKEN
            };
            let color = if bridge_conn { THEME.ok } else { THEME.muted };
            (
                icon,
                color,
                current_out_device.unwrap_or_else(|| {
                    log::error!("[EtherTap] device_selected=true but current_out_device is None");
                    String::new()
                }),
            )
        } else {
            let count = self.midi_out_ports.len().saturating_sub(1);
            let label = if count == 0 {
                "Select".to_string()
            } else {
                format!("Select  ({})", count)
            };
            (icon::SCAN, THEME.text_dim, label)
        };

        let clock_row = Row::new()
            .push(
                Button::new(
                    &mut self.btn_midi_picker,
                    Row::new()
                        .push(
                            t!(midi_icon)
                                .size(11)
                                .font(SOLAR_BOLD)
                                .color(midi_icon_color),
                        )
                        .push(Space::with_width(Length::Units(4)))
                        .push(t!(&selected_display).size(10).color(midi_icon_color))
                        .align_items(Alignment::Center),
                )
                .on_press(Message::ToggleMidiPicker)
                .style(EtherBtn(midi_picker_kind))
                .padding([4, 8])
                .width(Length::Fill),
            )
            .push(Space::with_width(Length::Units(8)))
            .push(midi_auto_connect_btn)
            .push(Space::with_width(Length::Units(8)))
            .push(t!("PPQ").size(9).color(THEME.text_dim))
            .push(Space::with_width(Length::Units(4)))
            .push(
                PickList::new(
                    &mut self.pick_ppq,
                    PPQ_OPTIONS,
                    Some(clock_ppq),
                    Message::SetClockPpq,
                )
                .text_size(10)
                .font(MONO_FONT)
                .padding([4, 6])
                .width(Length::Units(48))
                .style(PpqPickStyle),
            )
            .spacing(0)
            .align_items(Alignment::Center);

        // ── MIDI clock timing stats (jitter percentiles) ──────────────────
        //
        // Always rendered — shows placeholder dashes until 48 samples (2 beats)
        // are collected.  A single monospace string guarantees pixel-perfect
        // column alignment regardless of how the numbers change width.
        //
        // Format (size 8, MONO_FONT, right-aligned):
        //   "avg  20.8ms  p50±    0µs  p95±  450µs  p99± 1234µs  max± 5678µs"
        //   "avg   --.-ms  p50±   --µs  p95±   --µs  p99±   --µs  max±   --µs"
        //
        // Field widths (fixed, guaranteed by {:5.1} / {:5}):
        //   avg value  = 5 chars + "ms"
        //   jitter val = 5 chars + "µs"
        let clock_stats_row: Element<'_, Message> = {
            let stats = self.data.midi_clock_stats.load();
            let has_data = clock_on && stats.sample_n >= 48;

            // Colour for the p99 / max values.
            let p99_color = if !has_data {
                THEME.text_dim
            } else if stats.p99_us > 5_000 {
                THEME.err
            } else if stats.p99_us > 2_000 {
                THEME.warn
            } else {
                THEME.ok
            };
            let max_color = if !has_data {
                THEME.text_dim
            } else if stats.max_us > 10_000 {
                THEME.err
            } else if stats.max_us > 5_000 {
                THEME.warn
            } else {
                THEME.ok
            };

            // Pre-format every field to a fixed character count so that the
            // monospace Row never shifts even as values change magnitude.
            // avg: {:5.1} → "  8.3" … "125.0"  (5 chars)
            // jitter: {:5} → "    0" … "99999"  (5 chars)
            let avg_str = if has_data {
                format!("{:5.1}", stats.interval_us as f32 / 1_000.0)
            } else {
                " --.-".to_string()
            };
            let p50_str = if has_data {
                format!("{:5}", stats.p50_us)
            } else {
                "   --".to_string()
            };
            let p95_str = if has_data {
                format!("{:5}", stats.p95_us)
            } else {
                "   --".to_string()
            };
            let p99_str = if has_data {
                format!("{:5}", stats.p99_us)
            } else {
                "   --".to_string()
            };
            let max_str = if has_data {
                format!("{:5}", stats.max_us)
            } else {
                "   --".to_string()
            };

            // Split into dim labels + variably-coloured values so p99/max
            // can turn yellow/red while keeping a single monospace typeface.
            // Every string literal here has a fixed char count; values are
            // pre-formatted above to the same width, so columns are stable.
            Row::new()
                .push(midi_clk_btn)
                .push(Space::with_width(Length::Fill))
                .push(t!("avg ").size(9).color(THEME.text_dim))
                .push(t!(avg_str).size(9).color(THEME.text_dim))
                .push(t!("ms  p50\u{b1}").size(9).color(THEME.text_dim))
                .push(
                    t!(p50_str)
                        .size(9)
                        .color(if has_data { THEME.ok } else { THEME.text_dim }),
                )
                .push(t!("\u{b5}s  p95\u{b1}").size(9).color(THEME.text_dim))
                .push(
                    t!(p95_str)
                        .size(9)
                        .color(if has_data { THEME.ok } else { THEME.text_dim }),
                )
                .push(t!("\u{b5}s  p99\u{b1}").size(9).color(THEME.text_dim))
                .push(t!(p99_str).size(9).color(p99_color))
                .push(t!("\u{b5}s  max\u{b1}").size(9).color(THEME.text_dim))
                .push(t!(max_str).size(9).color(max_color))
                .push(t!("\u{b5}s").size(9).color(THEME.text_dim))
                .align_items(Alignment::Center)
                .into()
        };

        // ── Rate Sync row ─────────────────────────────────────────────────
        let rate_row = Row::new()
            .push(t!("RATE").size(9).color(THEME.text_dim))
            .push(Space::with_width(Length::Units(4)))
            .push(sync_btn(
                &mut self.btn_rate_manual,
                "Man",
                rate_mode == SyncMode::Manual,
                Message::SetRateSyncMode(SyncMode::Manual),
            ))
            .push(Space::with_width(Length::Units(4)))
            .push(sync_btn(
                &mut self.btn_rate_change,
                "BPM",
                rate_mode == SyncMode::OnChange,
                Message::SetRateSyncMode(SyncMode::OnChange),
            ))
            .push(Space::with_width(Length::Units(4)))
            .push(sync_btn(
                &mut self.btn_rate_cont,
                "Cont",
                rate_mode == SyncMode::Continuous,
                Message::SetRateSyncMode(SyncMode::Continuous),
            ))
            .push(Space::with_width(Length::Units(4)))
            .push(force_icon_btn(
                &mut self.btn_rate_force,
                Message::ForceRateSync,
            ))
            .push(Space::with_width(Length::Fill))
            .push(t!("PHASE").size(9).color(THEME.text_dim))
            .push(Space::with_width(Length::Units(4)))
            .push(sync_btn(
                &mut self.btn_phase_manual,
                "Man",
                phase_mode == SyncMode::Manual,
                Message::SetPhaseSyncMode(SyncMode::Manual),
            ))
            .push(Space::with_width(Length::Units(4)))
            .push(sync_btn(
                &mut self.btn_phase_change,
                "BPM",
                phase_mode == SyncMode::OnChange,
                Message::SetPhaseSyncMode(SyncMode::OnChange),
            ))
            .push(Space::with_width(Length::Units(4)))
            .push(sync_btn(
                &mut self.btn_phase_cont,
                "Cont",
                phase_mode == SyncMode::Continuous,
                Message::SetPhaseSyncMode(SyncMode::Continuous),
            ))
            .push(Space::with_width(Length::Units(4)))
            .push(force_icon_btn(
                &mut self.btn_phase_force,
                Message::ForcePhaseSync,
            ))
            .align_items(Alignment::Center);

        // ── Assembly ──────────────────────────────────────────────────────
        //
        // The banner is edge-to-edge (outside the content padding).  Each
        // section frame uses a titled-border pattern via `section()`: the
        // title text sits right on the top border line (zero top padding,
        // no background patch) with the content below it.
        let banner = header; // edge-to-edge, outside content padding
        let content = Column::new()
            .push(section(
                "MIXER",
                Column::new()
                    .push(net_row)
                    .push(Space::with_height(4.into()))
                    .push(telem_row)
                    .into(),
                Length::FillPortion(2),
            ))
            .push(Space::with_height(Length::Units(SECTION_GAP)))
            .push(section(
                "EFFECTS",
                Column::new()
                    .push(fx_line1)
                    .push(Space::with_height(3.into()))
                    .push(fx_line2)
                    .into(),
                Length::FillPortion(2),
            ))
            .push(Space::with_height(Length::Units(SECTION_GAP)))
            .push(section(
                "MIDI",
                Column::new()
                    .push(clock_row)
                    .push(Space::with_height(3.into()))
                    .push(clock_stats_row)
                    .into(),
                Length::FillPortion(2),
            ))
            .push(Space::with_height(Length::Units(SECTION_GAP)))
            .push(section("SYNC", rate_row.into(), Length::FillPortion(1)))
            .padding([0, 5, 4, 5])
            .spacing(0)
            .height(Length::Fill);

        // One coherent plugin surface: banner (edge-to-edge) + all four
        // sections inside a single outer frame.  Identical column in both
        // modes; each cfg block below wraps it in a `PluginFrame` container
        // at its mode's dimensions. `content` is the only `Length::Fill`
        // child, so it absorbs all space left over after the banner.
        let plugin_column = Column::new()
            .push(banner)
            .push(Space::with_height(Length::Units(4)))
            .push(content)
            .height(Length::Fill);

        // ── Standalone DAW frame (compiled only with --features standalone) ──
        // The shell is a free function over `&mut self.daw` (one field borrow)
        // so the modal early-returns above can reuse it around their backdrop.
        #[cfg(feature = "standalone")]
        let result = daw_shell(
            plugin_column.into(),
            &mut self.daw,
            &self.data,
            connected,
            in_sync,
        );

        #[cfg(not(feature = "standalone"))]
        let result = Container::new(plugin_column)
            .width(Length::Fill)
            .height(Length::Fill)
            .padding(PLUGIN_FRAME_PAD)
            .style(PluginFrame)
            .into();

        result
    }

    fn background_color(&self) -> Color {
        THEME.bg
    }
}

// ─── View helpers ─────────────────────────────────────────────────────────────

/// Pulse a momentary trigger BoolParam: set true via ParamSetter so the host
/// records the gesture; process() consumes the rising edge and self-resets the
/// param to false through context.set_parameter().
/// Titled section frame — the one visual pattern every main-column group
/// (MIXER / EFFECTS / MIDI / SYNC) uses: dim uppercase title sitting on the
/// frame's top edge, content inside a `ModSection`-styled container with the
/// shared `SECTION_PAD` inset.
fn section<'a>(
    title: &'static str,
    content: Element<'a, Message>,
    height: Length,
) -> Element<'a, Message> {
    Column::new()
        .push(t!(title).size(SECTION_TITLE_SIZE).color(THEME.text_dim))
        .push(
            Container::new(
                Container::new(content)
                    .padding(SECTION_PAD)
                    .width(Length::Fill),
            )
            .height(Length::Fill)
            .center_y()
            .style(ModSection),
        )
        .height(height)
        .into()
}

fn pulse_param(context: &dyn GuiContext, param: &BoolParam) {
    let setter = ParamSetter::new(context);
    setter.begin_set_parameter(param);
    setter.set_parameter(param, true);
    setter.end_set_parameter(param);
}

/// Build the standalone DAW shell — transport row, dimension rulers, param
/// footer — around `inner`, which gets framed at the true VST3 dimensions
/// (360×280, `PluginFrame`).  `inner` is either the shared plugin surface
/// column or a modal backdrop replacing it: modals cover only the plugin
/// frame; the DAW chrome stays visible and interactive around them.
///
/// Free function over `&mut DawChromeState` (a single field borrow) rather
/// than a `&mut self` method — old-iced stateful-widget borrow rules require
/// the caller's other field borrows (modal button states, content states) to
/// stay disjoint from the chrome's.
#[cfg(feature = "standalone")]
fn daw_shell<'a>(
    inner: Element<'a, Message>,
    daw: &'a mut DawChromeState,
    data: &'a EditorData,
    connected: bool,
    in_sync: bool,
) -> Element<'a, Message> {
    let sa_playing = data.standalone_playing.load(Ordering::Relaxed);
    let sa_pos = f64::from_bits(data.standalone_pos_beats.load(Ordering::Relaxed));
    let rate_mode = data.params.rate_sync_mode.value();
    let phase_mode = data.params.phase_sync_mode.value();

    let transport_row = Container::new(
        Row::new()
            .push(
                Button::new(
                    &mut daw.btn_play_stop,
                    t!(if sa_playing { "\u{2016}" } else { "\u{25b6}" })
                        .size(11)
                        .color(if sa_playing { THEME.ok } else { THEME.muted }),
                )
                .on_press(Message::ToggleStandalonePlay)
                .style(EtherBtn(BtnKind::Idle))
                .padding([3, 7]),
            )
            .push(Space::with_width(Length::Units(4)))
            .push(
                Button::new(
                    &mut daw.btn_stop,
                    t!("\u{25a0}").size(11).color(THEME.muted),
                )
                .on_press(Message::StopStandalone)
                .style(EtherBtn(BtnKind::Idle))
                .padding([3, 7]),
            )
            .push(Space::with_width(Length::Units(8)))
            .push(t!("BPM").size(9).color(THEME.daw_chrome_text_dim))
            .push(Space::with_width(Length::Units(4)))
            .push(
                TextInput::new(
                    &mut daw.bpm_input,
                    "120.0",
                    &daw.bpm_input_value,
                    Message::SetStandaloneBpm,
                )
                .size(11)
                .font(MONO_FONT)
                .padding([3, 5])
                .style(EtherInput)
                .width(Length::Units(52)),
            )
            .push(Space::with_width(Length::Units(6)))
            .push(
                Button::new(&mut daw.btn_tap, t!("Tap").size(10).color(THEME.accent))
                    .on_press(Message::TapTempo)
                    .style(EtherBtn(BtnKind::Idle))
                    .padding([3, 7]),
            )
            .push(Space::with_width(Length::Fill))
            .push(
                t!(format!("\u{25ce} {:.2}", sa_pos))
                    .size(9)
                    .font(MONO_FONT)
                    .color(THEME.daw_chrome_text_dim),
            )
            .align_items(Alignment::Center),
    )
    .padding([4, 10])
    .width(Length::Fill)
    .style(DawPanel);

    // RATE / PHASE sync mode chips: interactive (sync_btn/force_icon_btn).
    let rate_chip: Element<'_, Message> = Row::new()
        .push(
            t!("rate_sync_mode")
                .size(9)
                .color(THEME.daw_chrome_text_dim),
        )
        .push(Space::with_width(Length::Units(4)))
        .push(sync_btn(
            &mut daw.btn_daw_rate_manual,
            "Man",
            rate_mode == SyncMode::Manual,
            Message::SetRateSyncMode(SyncMode::Manual),
        ))
        .push(sync_btn(
            &mut daw.btn_daw_rate_change,
            "BPM",
            rate_mode == SyncMode::OnChange,
            Message::SetRateSyncMode(SyncMode::OnChange),
        ))
        .push(sync_btn(
            &mut daw.btn_daw_rate_cont,
            "Con",
            rate_mode == SyncMode::Continuous,
            Message::SetRateSyncMode(SyncMode::Continuous),
        ))
        .push(force_icon_btn(
            &mut daw.btn_daw_force_rate,
            Message::ForceRateSync,
        ))
        .spacing(2)
        .align_items(Alignment::Center)
        .into();

    let phase_chip: Element<'_, Message> = Row::new()
        .push(
            t!("phase_sync_mode")
                .size(9)
                .color(THEME.daw_chrome_text_dim),
        )
        .push(Space::with_width(Length::Units(4)))
        .push(sync_btn(
            &mut daw.btn_daw_phase_manual,
            "Man",
            phase_mode == SyncMode::Manual,
            Message::SetPhaseSyncMode(SyncMode::Manual),
        ))
        .push(sync_btn(
            &mut daw.btn_daw_phase_change,
            "BPM",
            phase_mode == SyncMode::OnChange,
            Message::SetPhaseSyncMode(SyncMode::OnChange),
        ))
        .push(sync_btn(
            &mut daw.btn_daw_phase_cont,
            "Con",
            phase_mode == SyncMode::Continuous,
            Message::SetPhaseSyncMode(SyncMode::Continuous),
        ))
        .push(force_icon_btn(
            &mut daw.btn_daw_force_phase,
            Message::ForcePhaseSync,
        ))
        .spacing(2)
        .align_items(Alignment::Center)
        .into();

    // PARAMETERS IN: automatable params the DAW can write to the plugin.
    // Compound mode selectors (rate/phase + force) get one row each;
    // momentary trigger / toggle chips follow at 4 per row, built from this
    // table — adding a param here is the only change a new chip needs
    // (button states resize to match).
    let params_in_compound = vec![rate_chip, phase_chip];
    let all_slots_on = data.params.all_slots.value();
    let auto_reconnect_on = data.params.auto_reconnect.value();
    let trigger_specs: [(&'static str, Message, bool); 5] = [
        ("connect_to_last", Message::Connect, false),
        ("disconnect", Message::Disconnect, false),
        ("audit_slots", Message::QuerySlots, false),
        ("all_slots", Message::ToggleAutoSlots, !all_slots_on),
        (
            "auto_reconnect",
            Message::ToggleAutoReconnect,
            !auto_reconnect_on,
        ),
    ];
    daw.btn_daw_triggers
        .resize_with(trigger_specs.len(), Default::default);
    let params_in_triggers: Vec<Element<'_, Message>> = daw
        .btn_daw_triggers
        .iter_mut()
        .zip(trigger_specs)
        .map(|(state, (label, msg, dimmed))| daw_trigger_chip(state, label, msg, dimmed))
        .collect();

    // PARAMETERS OUT: read-only status the plugin writes back to the DAW.
    let sync_status = data.params.sync_status.value();
    let hardware_bpm = data.params.hardware_bpm.value();
    let compatible_slot_count = data.params.compatible_slot_count.value();
    let phase_reset_pending = data.params.phase_reset_pending.value();
    let midi_bridge_connected = data.params.midi_bridge_connected.value();
    let params_out: Vec<Element<'_, Message>> = vec![
        daw_indicator_chip("is_connected", connected),
        daw_indicator_chip("is_matched", in_sync),
        daw_value_chip("sync_status", sync_status_label(sync_status)),
        daw_indicator_chip("phase_reset_pending", phase_reset_pending),
        daw_value_chip("hardware_bpm", format!("{:.2}", hardware_bpm)),
        daw_value_chip(
            "compatible_slot_count",
            format!("{}", compatible_slot_count),
        ),
        daw_indicator_chip("midi_bridge_connected", midi_bridge_connected),
    ];

    let footer = Container::new(
        Column::new()
            .push(
                t!("\u{25B6} PARAMETERS IN")
                    .size(9)
                    .color(THEME.daw_chrome_border),
            )
            .push(Space::with_height(Length::Units(4)))
            .push(wrap_rows(params_in_compound, 1))
            .push(Space::with_height(Length::Units(3)))
            .push(wrap_rows(params_in_triggers, 4))
            .push(Space::with_height(Length::Units(6)))
            .push(
                t!("\u{25B6} PARAMETERS OUT")
                    .size(9)
                    .color(THEME.daw_chrome_border),
            )
            .push(Space::with_height(Length::Units(4)))
            .push(wrap_rows(params_out, 4))
            .padding([5, 6])
            .spacing(2)
            .width(Length::Fill),
    )
    .width(Length::Fill)
    .style(DawPanel);

    // The framed 360×280 box contains *exactly* what a real VST3 host
    // renders, so the frame's border is the one true seam between "what the
    // plugin draws" and "DAW chrome we built around it for standalone test".
    let framed_plugin = Container::new(inner)
        .width(Length::Units(360))
        .height(Length::Units(280))
        .padding(PLUGIN_FRAME_PAD)
        .style(PluginFrame);

    // Dimension-ruler overlay: rulers sized to the frame's exact edges
    // (360 / 280) so they double as a visual cross-check that the box really
    // does render at true VST3 dimensions.  Blank spacers mirror the ruler
    // gutters on the opposite edges so the framed box sits dead-center.
    const RULER_GUTTER: u16 = 40;
    let ruler_row_h = || {
        Row::new()
            .push(Space::new(Length::Units(RULER_GUTTER), Length::Units(1)))
            .push(Space::with_width(Length::Units(4)))
            .push(dim_ruler_h(360, "360px".to_string()))
            .push(Space::with_width(Length::Units(4)))
            .push(Space::new(Length::Units(RULER_GUTTER), Length::Units(1)))
    };

    let dimensioned_frame = Column::new()
        .push(ruler_row_h())
        .push(Space::with_height(Length::Units(4)))
        .push(
            Row::new()
                .push(dim_ruler_v(280, "280px".to_string()))
                .push(Space::with_width(Length::Units(4)))
                .push(framed_plugin)
                .push(Space::with_width(Length::Units(4)))
                .push(dim_ruler_v(280, "280px".to_string()))
                .align_items(Alignment::Center),
        )
        .push(Space::with_height(Length::Units(4)))
        .push(ruler_row_h());

    Container::new(
        Column::new()
            .push(transport_row)
            .push(
                Container::new(dimensioned_frame)
                    .width(Length::Fill)
                    .height(Length::Fill)
                    .center_x()
                    .center_y(),
            )
            .push(footer)
            .height(Length::Fill),
    )
    .width(Length::Fill)
    .height(Length::Fill)
    .style(DawChrome)
    .into()
}

/// Toggle one FX type filter BoolParam via ParamSetter (bit 0–6).
fn toggle_fx_filter_param(setter: &ParamSetter, params: &EtherTapParams, bit: u8) {
    macro_rules! toggle {
        ($field:ident) => {{
            let next = !params.$field.value();
            setter.begin_set_parameter(&params.$field);
            setter.set_parameter(&params.$field, next);
            setter.end_set_parameter(&params.$field);
        }};
    }
    match bit {
        0 => toggle!(fx_filter_dly),
        1 => toggle!(fx_filter_3tap),
        2 => toggle!(fx_filter_4tap),
        3 => toggle!(fx_filter_drv),
        4 => toggle!(fx_filter_dcr),
        5 => toggle!(fx_filter_dfl),
        6 => toggle!(fx_filter_modd),
        _ => {}
    }
}

/// Compact radio-style sync mode button (Man / BPM / Cont).
fn sync_btn<'a>(
    state: &'a mut button::State,
    label: &'static str,
    selected: bool,
    msg: Message,
) -> Button<'a, Message> {
    Button::new(
        state,
        Container::new(t!(label).size(10).color(if selected {
            THEME.selected_text
        } else {
            THEME.muted
        })),
    )
    .on_press(msg)
    .style(EtherBtn(if selected {
        BtnKind::Active
    } else {
        BtnKind::Idle
    }))
    .padding([4, 8])
}

/// Bolt-only force-sync button (no text label).
fn force_icon_btn(state: &mut button::State, msg: Message) -> Button<'_, Message> {
    Button::new(
        state,
        t!(icon::BOLT).size(11).font(SOLAR_BOLD).color(THEME.accent),
    )
    .on_press(msg)
    .style(EtherBtn(BtnKind::Force))
    .padding([4, 8])
}

/// Momentary trigger button for the DAW parameters-in footer.
/// `dimmed` = legacy/secondary params rendered in dim chrome text.
#[cfg(feature = "standalone")]
fn daw_trigger_chip<'a>(
    state: &'a mut button::State,
    label: &'static str,
    msg: Message,
    dimmed: bool,
) -> Element<'a, Message> {
    let color = if dimmed {
        THEME.daw_chrome_text_dim
    } else {
        THEME.daw_chrome_text
    };
    Button::new(state, t!(label).size(9).color(color))
        .on_press(msg)
        .style(EtherBtn(BtnKind::Idle))
        .padding([3, 6])
        .into()
}

/// Read-only indicator LED for the DAW parameters-out footer.
#[cfg(feature = "standalone")]
fn daw_indicator_chip<'a>(label: &'static str, on: bool) -> Element<'a, Message> {
    let (dot, color) = if on {
        ("●", THEME.ok)
    } else {
        ("○", THEME.err)
    };
    Row::new()
        .push(t!(dot).size(9).color(color))
        .push(Space::with_width(Length::Units(3)))
        .push(t!(label).size(9).color(THEME.daw_chrome_text_dim))
        .align_items(Alignment::Center)
        .into()
}

/// Read-only label=value chip for the DAW parameters-out footer — for params
/// whose host-visible state isn't a simple on/off LED (`sync_status`,
/// `hardware_bpm`, `compatible_slot_count`).
#[cfg(feature = "standalone")]
fn daw_value_chip<'a>(label: &'static str, value: String) -> Element<'a, Message> {
    Row::new()
        .push(t!(label).size(9).color(THEME.daw_chrome_text_dim))
        .push(Space::with_width(Length::Units(3)))
        .push(t!(value).size(9).color(THEME.accent))
        .align_items(Alignment::Center)
        .into()
}

/// Maps `SyncStatus` to its DAW-shell footer label.
#[cfg(feature = "standalone")]
fn sync_status_label(status: SyncStatus) -> String {
    match status {
        SyncStatus::Offline => "offline".to_string(),
        SyncStatus::Connected => "connected".to_string(),
        SyncStatus::Syncing => "syncing".to_string(),
        SyncStatus::Synced => "synced".to_string(),
    }
}

/// Footer-scoped wrap/grid: chunks `items` into a `Column` of `Row`s, `per_row`
/// items each (last row may be shorter). Count-driven so the standalone footer
/// self-adjusts as params are added/removed — no hand-stacked layout to edit.
/// Scoped to this footer (non-goal: no general-purpose layout component).
#[cfg(feature = "standalone")]
fn wrap_rows<'a>(items: Vec<Element<'a, Message>>, per_row: usize) -> Column<'a, Message> {
    let mut rows = Column::new().spacing(4);
    let mut chunk = Row::new().spacing(8);
    let mut count = 0;

    for item in items {
        chunk = chunk.push(item);
        count += 1;
        if count == per_row {
            rows = rows.push(chunk);
            chunk = Row::new().spacing(8);
            count = 0;
        }
    }
    if count > 0 {
        rows = rows.push(chunk);
    }

    rows
}

/// Horizontal dimension ruler — `─── 360px ───`, spanning exactly `width`
/// units so it lines up with the framed VST box's edge. Drawn in the DAW
/// shell above the box to surface the host-rendered plugin's true pixel
/// dimensions ("more data on the actual VST" — not guessable from the
/// screenshot alone).
#[cfg(feature = "standalone")]
fn dim_ruler_h<'a>(width: u16, label: String) -> Element<'a, Message> {
    let line = || {
        Container::new(Space::new(Length::Fill, Length::Units(1)))
            .width(Length::Fill)
            .style(DimLine)
    };
    Row::new()
        .push(line())
        .push(Space::with_width(Length::Units(4)))
        .push(t!(label).size(8).color(THEME.daw_chrome_border))
        .push(Space::with_width(Length::Units(4)))
        .push(line())
        .width(Length::Units(width))
        .align_items(Alignment::Center)
        .into()
}

/// Vertical counterpart to [`dim_ruler_h`] — spans `height` units alongside
/// the framed box. Label stays upright (no text rotation in this iced API).
#[cfg(feature = "standalone")]
fn dim_ruler_v<'a>(height: u16, label: String) -> Element<'a, Message> {
    let line = || {
        Container::new(Space::new(Length::Units(1), Length::Fill))
            .height(Length::Fill)
            .style(DimLine)
    };
    Column::new()
        .push(line())
        .push(Space::with_height(Length::Units(4)))
        .push(t!(label).size(8).color(THEME.daw_chrome_border))
        .push(Space::with_height(Length::Units(4)))
        .push(line())
        .width(Length::Units(40))
        .height(Length::Units(height))
        .align_items(Alignment::Center)
        .into()
}

// ─── Style utility ────────────────────────────────────────────────────────────

fn lighten(style: button::Style, delta: f32) -> button::Style {
    button::Style {
        background: style.background.map(|b| {
            let Background::Color(c) = b;
            Background::Color(Color {
                r: (c.r + delta).clamp(0.0, 1.0),
                g: (c.g + delta).clamp(0.0, 1.0),
                b: (c.b + delta).clamp(0.0, 1.0),
                a: c.a,
            })
        }),
        ..style
    }
}
