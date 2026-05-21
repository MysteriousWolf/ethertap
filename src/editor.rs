/// Iced-based editor for EtherTap.
///
/// # Colour palette
/// All colours live in `THEME` (a `Theme::dark()` constant at the bottom of
/// the "Theme" section).  To produce a different skin, copy `dark()`, rename
/// it, and change the `static THEME` assignment — nothing else needs to move.
///
/// Solar Icon Set Bold (PUA U+E900…) is used for all non-text glyphs.
use std::sync::{
    atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
    Arc,
};

use midir::MidiOutput;

use nih_plug::prelude::{GuiContext, ParamSetter};
use nih_plug_iced::{
    button, container, create_iced_editor, executor, pick_list, text_input,
    widget::{tooltip, Button, Column, Container, PickList, Row, Space, Text, TextInput},
    Alignment, Background, Color, Command, Element, Font, IcedEditor,
    Length, Subscription, WindowQueue, WindowSubs,
};
use parking_lot::Mutex;

use crate::{
    network::{now_ms, DeviceInfo, NetworkCommand},
    osc,
    params::{EtherTapParams, SyncMode, MONO_FONT},
};

// ─── Solar Icons Bold font ───────────────────────────────────────────────────

const SOLAR_BOLD: Font = Font::External {
    name: "Solar Icon Set Bold",
    bytes: include_bytes!("../assets/Solar-Icon-Set_Bold.ttf"),
};

// ─── Text helper ─────────────────────────────────────────────────────────────
// Every text element in the UI uses MONO_FONT.  `t!(expr)` expands to
// `t!(expr).font(MONO_FONT)` so callers can still chain `.size()`,
// `.color()`, and `.font(SOLAR_BOLD)` (the last overrides for icon glyphs).

macro_rules! t {
    ($s:expr) => { Text::new($s).font(MONO_FONT) };
}

// TODO: add assets/Solaar.ttf and replace Font::Default with:
// const LOGO_FONT: Font = Font::External { name: "Solaar", bytes: include_bytes!("../assets/Solaar.ttf") };

// ─── Icon codepoints (Solar Icon Set Bold, PUA) ──────────────────────────────

mod icon {
    pub const LINK: &str        = "\u{ecf2}"; // si-Link — connected
    pub const LINK_BROKEN: &str = "\u{ecf3}"; // si-Link-Broken — disconnected
    pub const ARROW_RIGHT: &str = "\u{e908}"; // si-Arrow-Right
    pub const ARROW_LEFT: &str  = "\u{e905}"; // si-Arrow-Left
    pub const BOLT: &str        = "\u{ea50}"; // si-Bolt — force / destructive
    pub const SCAN: &str        = "\u{ec8a}"; // si-Scanner
    pub const CLOCK: &str       = "\u{ed1c}"; // si-Clock-Circle — MIDI clock
}

// ─── MIDI out device sentinel ─────────────────────────────────────────────────

/// Displayed in the device PickList when no physical device is selected.
const MIDI_OUT_NONE: &str = "\u{2014} None \u{2014}";

// ─── Theme ───────────────────────────────────────────────────────────────────
//
// Edit ONLY the colour values inside `Theme::dark()` to restyle the entire UI.
// Field names are intentionally semantic (not widget-specific) so this block
// reads as a design-token palette.

#[allow(dead_code)]
struct Theme {
    // ── Window ────────────────────────────────────────────────────────────
    bg:              Color, // window background

    // ── Surfaces (idle buttons, text inputs) ─────────────────────────────
    surface:         Color, // button / input fill
    surface_border:  Color, // idle border
    muted:           Color, // idle / placeholder text

    // ── Selected state (active radio option, focused input) ───────────────
    selected_bg:     Color,
    selected_border: Color,
    selected_text:   Color,

    // ── Destructive / force action ────────────────────────────────────────
    danger_bg:       Color,
    danger_border:   Color,
    danger_text:     Color,

    // ── Body text ─────────────────────────────────────────────────────────
    text:            Color,
    text_dim:        Color, // labels, secondary info

    // ── Status ────────────────────────────────────────────────────────────
    ok:              Color, // sync match, connected
    err:             Color, // sync drift, disconnected
    warn:            Color, // TX activity

    // ── Accent ────────────────────────────────────────────────────────────
    // Used for: logo "ETHER" glyph, RX activity LED, focused-input border.
    // Change this one value to shift the whole brand colour.
    accent:          Color,
}

const fn rgb(r: u8, g: u8, b: u8) -> Color {
    Color { r: r as f32 / 255.0, g: g as f32 / 255.0, b: b as f32 / 255.0, a: 1.0 }
}

impl Theme {
    const fn dark() -> Self {
        Self {
            bg:              rgb( 15,  15,  21),
            surface:         rgb( 32,  32,  42),
            surface_border:  rgb( 55,  55,  70),
            muted:           rgb(115, 115, 130),
            selected_bg:     rgb( 45,  90, 170),
            selected_border: rgb( 45,  90, 170),
            selected_text:   rgb(200, 225, 255),
            danger_bg:       rgb(130,  50,  15),
            danger_border:   rgb(130,  50,  15),
            danger_text:     rgb(255, 210,  90),
            text:            rgb(210, 210, 222),
            text_dim:        rgb( 95,  95, 110),
            ok:              rgb( 70, 190,  80),
            err:             rgb(215,  65,  65),
            warn:            rgb(225, 175,  50),
            accent:          rgb( 50, 180, 170), // teal — brand colour
        }
    }
}

static THEME: Theme = Theme::dark();

// ─── Pulse window ────────────────────────────────────────────────────────────

const PULSE_MS: u64 = 100;

// ─── Button stylesheet ───────────────────────────────────────────────────────

#[derive(Clone, Copy)]
enum BtnKind { Idle, Active, Force, Disabled, Enabled, Error }

struct EtherBtn(BtnKind);

impl button::StyleSheet for EtherBtn {
    fn active(&self) -> button::Style {
        match self.0 {
            BtnKind::Idle => button::Style {
                background: Some(Background::Color(rgb(22, 22, 30))),
                border_radius: 5.0, border_width: 1.0,
                border_color: THEME.surface_border,
                text_color: THEME.muted,
                ..Default::default()
            },
            BtnKind::Active => button::Style {
                background: Some(Background::Color(THEME.selected_bg)),
                border_radius: 5.0, border_width: 0.0,
                border_color: THEME.selected_bg,
                text_color: THEME.selected_text,
                ..Default::default()
            },
            BtnKind::Force => button::Style {
                background: Some(Background::Color(THEME.danger_bg)),
                border_radius: 5.0, border_width: 0.0,
                border_color: THEME.danger_bg,
                text_color: THEME.danger_text,
                ..Default::default()
            },
            BtnKind::Disabled => button::Style {
                background: Some(Background::Color(THEME.bg)),
                border_radius: 5.0, border_width: 1.0,
                border_color: rgb(22, 22, 30),
                text_color: THEME.surface_border,
                ..Default::default()
            },
            BtnKind::Enabled => button::Style {
                background: Some(Background::Color(rgb(25, 70, 35))),
                border_radius: 5.0, border_width: 0.0,
                border_color: THEME.ok,
                text_color: THEME.ok,
                ..Default::default()
            },
            BtnKind::Error => button::Style {
                background: Some(Background::Color(rgb(75, 20, 20))),
                border_radius: 5.0, border_width: 0.0,
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
            background: Background::Color(THEME.bg),
            border_radius: 5.0,
            border_width: 1.0,
            border_color: THEME.surface_border,
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

    fn placeholder_color(&self) -> Color { THEME.text_dim }
    fn value_color(&self)       -> Color { THEME.text }
    fn selection_color(&self)   -> Color { THEME.selected_bg }
}

/// Dimmed style for IP/port inputs when a connection is active.
struct EtherInputLocked;

impl text_input::StyleSheet for EtherInputLocked {
    fn active(&self) -> text_input::Style {
        text_input::Style {
            background: Background::Color(THEME.bg),
            border_radius: 3.0,
            border_width: 1.0,
            border_color: THEME.surface,
        }
    }
    fn focused(&self)      -> text_input::Style { self.active() }
    fn hovered(&self)      -> text_input::Style { self.active() }
    fn placeholder_color(&self) -> Color { THEME.surface_border }
    fn value_color(&self)       -> Color { THEME.text_dim }
    fn selection_color(&self)   -> Color { THEME.bg }
}

// ─── PickList stylesheet ──────────────────────────────────────────────────────

struct PpqPickStyle;

impl pick_list::StyleSheet for PpqPickStyle {
    fn menu(&self) -> pick_list::Menu {
        pick_list::Menu {
            text_color:          THEME.text,
            background:          Background::Color(THEME.surface),
            border_width:        1.0,
            border_color:        THEME.surface_border,
            selected_text_color: THEME.selected_text,
            selected_background: Background::Color(THEME.selected_bg),
        }
    }

    fn active(&self) -> pick_list::Style {
        pick_list::Style {
            text_color:        THEME.text,
            placeholder_color: THEME.muted,
            background:        Background::Color(THEME.surface),
            border_radius:     3.0,
            border_width:      1.0,
            border_color:      THEME.surface_border,
            icon_size:         0.55,
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
            background: Some(Background::Color(THEME.surface)),
            border_radius: 3.0,
            border_width: 1.0,
            border_color: THEME.surface_border,
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
        button::Style { text_color: THEME.text, ..self.active() }
    }
    fn pressed(&self) -> button::Style { self.hovered() }
}

/// Tooltip background card.
struct TooltipCard;
impl container::StyleSheet for TooltipCard {
    fn style(&self) -> container::Style {
        container::Style {
            background: Some(Background::Color(THEME.surface)),
            border_radius: 3.0,
            border_width: 1.0,
            border_color: THEME.surface_border,
            text_color: Some(THEME.text_dim),
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

// ─── Shared data bundle ───────────────────────────────────────────────────────

pub struct EditorData {
    pub params:             Arc<EtherTapParams>,
    pub conn_status:        Arc<AtomicBool>,
    pub tx_activity_ts:          Arc<AtomicU64>,
    pub rx_activity_ts:          Arc<AtomicU64>,
    pub midi_clock_activity_ts:  Arc<AtomicU64>,
    pub hardware_float:     Arc<AtomicU32>,
    pub host_bpm:           Arc<AtomicU32>,
    pub force_sync_trigger: Arc<AtomicBool>,
    pub force_rate_trigger: Arc<AtomicBool>,
    pub compatible_slots:   Arc<Mutex<Vec<u8>>>,
    pub occupied_slots:     Arc<Mutex<Vec<u8>>>,
    /// Raw effect type ID for each slot (index = slot-1). Updated after AuditSlots.
    pub slot_types:         Arc<Mutex<[Option<i32>; 8]>>,
    pub all_slots_mode:     Arc<AtomicBool>,
    pub scan_targets:       Arc<Mutex<Vec<DeviceInfo>>>,
    /// Millisecond timestamp of the last completed scan (0 = never scanned).
    pub scan_completed_ts:  Arc<AtomicU64>,
    /// Name and model parsed from /info heartbeat responses.
    pub connected_device:   Arc<Mutex<(String, String)>>,
    pub cmd_tx:             crossbeam_channel::Sender<NetworkCommand>,
    /// Notifies the MIDI clock worker when the user changes the output device.
    pub device_change_tx:   crossbeam_channel::Sender<Option<String>>,
    /// True when the worker has an active connection to the selected MIDI output.
    pub midi_bridge_connected: Arc<AtomicBool>,
    /// Rolling timing statistics from the MIDI clock worker.
    pub midi_clock_stats: Arc<Mutex<crate::midi_clock::ClockStats>>,
}

// ─── Editor entry point ───────────────────────────────────────────────────────

pub fn create(data: Arc<EditorData>) -> Option<Box<dyn nih_plug::prelude::Editor>> {
    create_iced_editor::<EtherTapEditor>(data.params.editor_state.clone(), data)
}

// ─── Editor struct ────────────────────────────────────────────────────────────

struct EtherTapEditor {
    data:       Arc<EditorData>,
    context:    Arc<dyn GuiContext>,
    ip_buf:     String,
    port_buf:   String,
    ip_state:   text_input::State,
    port_state: text_input::State,
    // Rate Sync — 3 radio buttons + force (bolt-only)
    btn_rate_manual: button::State,
    btn_rate_change: button::State,
    btn_rate_cont:   button::State,
    btn_rate_force:  button::State,
    // Phase Sync — 3 radio buttons + force (bolt-only)
    btn_phase_manual: button::State,
    btn_phase_change: button::State,
    btn_phase_cont:   button::State,
    btn_phase_force:  button::State,
    // FX row controls
    btn_auto:    button::State,
    btn_query:   button::State,
    slot_states: [button::State; 8],
    /// Per-effect-type toggles (bit order: Delay/3Tap/4Tap/D+Rev/D+Cho/D+Fln/Mod).
    btn_fx_type: [button::State; 7],
    // Output clock section — toggle button + PPQ pick list + MIDI out device
    btn_clock_toggle:    button::State,
    pick_ppq:            pick_list::State<u8>,
    pick_midi_out:       pick_list::State<String>,
    /// Available MIDI output port names — first entry is always the sentinel.
    midi_out_ports:      Vec<String>,
    /// ms-since-epoch of last port enumeration.
    last_port_enum_ms:   u64,
    // Network scan — btn_scan doubles as the modal close button
    btn_scan:              button::State,
    btn_connect:           button::State,
    scan_result_states:    [button::State; 8],
    show_scan_results:     bool,
    /// ms-since-epoch when the last ScanTargets command was dispatched.
    last_scan_trigger_ms:  u64,
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
    /// Flip one bit in the fx_type_filter bitmask (bit = 0..6).
    ToggleFxType(u8),
    /// Toggle MIDI clock output on/off.
    ToggleMidiClock,
    /// Set MIDI clock pulses per quarter note.
    SetClockPpq(u8),
    /// Select a MIDI output device for clock injection + passthrough bridging.
    /// The sentinel value MIDI_OUT_NONE means "no device selected".
    SetMidiOutDevice(String),
    ScanTargets,
    /// Fired every render frame via `WindowSubs::on_frame`; gated to 5 s.
    OnFrame,
    SelectTarget(String, u16),
    Connect,
    Disconnect,
}

// ─── IcedEditor impl ─────────────────────────────────────────────────────────

impl IcedEditor for EtherTapEditor {
    type Executor = executor::Default;
    type Message  = Message;
    type InitializationFlags = Arc<EditorData>;

    fn new(data: Arc<EditorData>, context: Arc<dyn GuiContext>) -> (Self, Command<Message>) {
        let ip_buf   = data.params.target_ip.lock().clone();
        let port_buf = data.params.target_port.lock().to_string();
        (
            Self {
                data, context,
                ip_buf, port_buf,
                ip_state:   Default::default(),
                port_state: Default::default(),
                btn_rate_manual:  Default::default(),
                btn_rate_change:  Default::default(),
                btn_rate_cont:    Default::default(),
                btn_rate_force:   Default::default(),
                btn_phase_manual: Default::default(),
                btn_phase_change: Default::default(),
                btn_phase_cont:   Default::default(),
                btn_phase_force:  Default::default(),
                btn_auto:         Default::default(),
                btn_query:        Default::default(),
                btn_fx_type:      Default::default(),
                btn_clock_toggle:  Default::default(),
                pick_ppq:          Default::default(),
                pick_midi_out:     Default::default(),
                midi_out_ports:    vec![MIDI_OUT_NONE.to_string()],
                last_port_enum_ms: 0,
                slot_states:       Default::default(),
                btn_scan:              Default::default(),
                btn_connect:           Default::default(),
                scan_result_states:    Default::default(),
                show_scan_results:     false,
                last_scan_trigger_ms:  0,
            },
            Command::none(),
        )
    }

    fn context(&self) -> &dyn GuiContext { self.context.as_ref() }

    /// Hook `on_frame` so we can gate a periodic rescan to every 5 s.
    fn subscription(
        &self,
        window_subs: &mut WindowSubs<Message>,
    ) -> Subscription<Message> {
        // on_frame fires every render frame; the actual scan is rate-limited
        // to once every 5 s inside the OnFrame handler.
        window_subs.on_frame = Some(Message::OnFrame);
        Subscription::none()
    }

    fn update(&mut self, _window: &mut WindowQueue, msg: Message) -> Command<Message> {
        match msg {
            Message::IpEdited(s) => {
                // Only update the buffer when disconnected — editing while connected is ignored.
                if !self.data.conn_status.load(Ordering::Relaxed) {
                    self.ip_buf = s.clone();
                    *self.data.params.target_ip.lock() = s;
                }
            }
            Message::PortEdited(s) => {
                if !self.data.conn_status.load(Ordering::Relaxed) {
                    self.port_buf = s.clone();
                    if let Ok(port) = s.parse::<u16>() {
                        *self.data.params.target_port.lock() = port;
                    }
                }
            }
            Message::SlotSelected(slot) => {
                *self.data.params.fx_slot.lock() = slot;
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
            Message::ForceRateSync  => {
                self.data.force_rate_trigger.store(true, Ordering::Release);
            }
            Message::ForcePhaseSync => {
                self.data.force_sync_trigger.store(true, Ordering::Release);
            }
            Message::QuerySlots => {
                let _ = self.data.cmd_tx.try_send(NetworkCommand::AuditSlots);
            }
            Message::ToggleAutoSlots => {
                let prev = self.data.all_slots_mode.load(Ordering::Relaxed);
                self.data.all_slots_mode.store(!prev, Ordering::Relaxed);
            }
            Message::ToggleFxType(bit) => {
                let mut filter = self.data.params.fx_type_filter.lock();
                *filter ^= 1_u32 << bit;
            }
            Message::ToggleMidiClock => {
                let mut enabled = self.data.params.midi_clock_enabled.lock();
                *enabled = !*enabled;
            }
            Message::SetClockPpq(ppq) => {
                *self.data.params.midi_clock_ppq.lock() = ppq;
            }
            Message::SetMidiOutDevice(name) => {
                let device = if name == MIDI_OUT_NONE { None } else { Some(name) };
                *self.data.params.midi_out_device.lock() = device.clone();
                let _ = self.data.device_change_tx.try_send(device);
            }
            Message::ScanTargets => {
                self.show_scan_results = !self.show_scan_results;
                if self.show_scan_results {
                    // Clear stale entries from a previous session so the panel
                    // starts fresh; the first scan result arrives within ~600 ms.
                    self.data.scan_targets.lock().clear();
                    let _ = self.data.cmd_tx.try_send(NetworkCommand::ScanTargets);
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
                // Refresh MIDI output port list every 2 s.
                if now_ms().saturating_sub(self.last_port_enum_ms) >= 2_000 {
                    self.last_port_enum_ms = now_ms();
                    let mut ports = vec![MIDI_OUT_NONE.to_string()];
                    if let Ok(out) = MidiOutput::new("EtherTap-Enum") {
                        for p in out.ports() {
                            if let Ok(name) = out.port_name(&p) {
                                if name != "EtherTap MIDI Clock" {
                                    ports.push(name);
                                }
                            }
                        }
                    }
                    self.midi_out_ports = ports;
                }
            }
            Message::SelectTarget(ip, port) => {
                self.ip_buf   = ip.clone();
                self.port_buf = port.to_string();
                *self.data.params.target_ip.lock()   = ip.clone();
                *self.data.params.target_port.lock() = port;
                let _ = self.data.cmd_tx.try_send(NetworkCommand::UpdateTarget { ip, port });
                self.show_scan_results = false;
            }
            Message::Connect => {
                let ip   = self.data.params.target_ip.lock().clone();
                let port = *self.data.params.target_port.lock();
                let _ = self.data.cmd_tx.try_send(NetworkCommand::UpdateTarget { ip, port });
                let _ = self.data.cmd_tx.try_send(NetworkCommand::AuditSlots);
                self.data.all_slots_mode.store(true, Ordering::Relaxed);
            }
            Message::Disconnect => {
                let _ = self.data.cmd_tx.try_send(NetworkCommand::Disconnect);
                // Keep connected_device so the header shows the last known name.
            }
        }
        Command::none()
    }

    fn view(&mut self) -> Element<'_, Message> {
        // ── Read shared state ─────────────────────────────────────────────
        let connected = self.data.conn_status.load(Ordering::Relaxed);
        let now         = now_ms();
        let tx_on = { let ts = self.data.tx_activity_ts.load(Ordering::Relaxed);
                      ts > 0 && now.saturating_sub(ts) < PULSE_MS };
        let rx_on = { let ts = self.data.rx_activity_ts.load(Ordering::Relaxed);
                      ts > 0 && now.saturating_sub(ts) < PULSE_MS };

        let host_bpm_f = f32::from_bits(self.data.host_bpm.load(Ordering::Relaxed));
        let host_float = osc::bpm_to_float(host_bpm_f as f64);
        let hw_float   = f32::from_bits(self.data.hardware_float.load(Ordering::Relaxed));
        let hw_bpm     = osc::float_to_bpm(hw_float);
        let has_hw     = hw_float > 0.0001;
        let in_sync    = has_hw && (host_float - hw_float).abs() < 0.001;

        let rate_mode  = self.data.params.rate_sync_mode.value();
        let phase_mode = self.data.params.phase_sync_mode.value();
        let cur_slot   = *self.data.params.fx_slot.lock();
        let compatible = self.data.compatible_slots.lock().clone();
        let occupied   = self.data.occupied_slots.lock().clone();
        let slot_types = *self.data.slot_types.lock();
        let all_mode   = self.data.all_slots_mode.load(Ordering::Relaxed);
        let post_audit = !compatible.is_empty() || !occupied.is_empty();

        // ── Scan popup modal ──────────────────────────────────────────────
        //
        // When open, we return a completely different view (full-window
        // dark card) so the main layout height never changes.
        if self.show_scan_results {
            let scan_targets_snap = self.data.scan_targets.lock().clone();
            let completed_ts = self.data.scan_completed_ts.load(Ordering::Relaxed);
            let scanning_now  = now_ms().saturating_sub(self.last_scan_trigger_ms) < 1500;

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
            let status_color = if scanning_now { THEME.warn } else { THEME.text_dim };

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
                    t!("Waiting for responses\u{2026}").size(11).color(THEME.text_dim),
                );
            } else {
                for (state, dev) in self.scan_result_states.iter_mut()
                    .zip(scan_targets_snap.iter())
                {
                    let name_line = dev.display_name();

                    // Primary (preferred) address — brighter than alt routes.
                    let lat_str = dev.latency_ms.map_or("\u{2014}".into(),
                        |ms| format!("{:.1} ms", ms));
                    let direct = dev.all_addrs.first().map(|(_, _, d)| *d).unwrap_or(false);
                    let path_str = if direct { "direct" } else { "routed" };
                    let addr_line = format!("{}  {}  {}", dev.ip, lat_str, path_str);

                    let mut entry = Column::new()
                        .push(t!(name_line).size(11).color(THEME.text))
                        .push(t!(addr_line).size(9).color(THEME.muted))
                        .spacing(2);

                    // Alt IPs — dimmer than the preferred route.
                    for (alt_ip, alt_lat, alt_direct) in dev.all_addrs.iter().skip(1) {
                        let alt_lat_str = alt_lat.map_or("\u{2014}".into(),
                            |ms| format!("{:.1} ms", ms));
                        let alt_path = if *alt_direct { "direct" } else { "routed" };
                        let alt_line = format!("{} (alt)  {}  {}", alt_ip, alt_lat_str, alt_path);
                        entry = entry.push(
                            t!(alt_line).size(9).color(THEME.text_dim),
                        );
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
                .width(Length::Units(290));

            return Container::new(card)
                .width(Length::Fill)
                .height(Length::Fill)
                .center_x()
                .center_y()
                .style(ModalBackdrop)
                .into();
        }

        // ── Logo + device info + status header ────────────────────────────
        //
        // Layout:  ETHERTAP  [fill]  [icon] device-name  [fill]  TX TX  RX RX
        let (conn_icon, conn_color) = if connected {
            (icon::LINK,        THEME.ok)
        } else {
            (icon::LINK_BROKEN, THEME.err)
        };
        let ck_on = { let ts = self.data.midi_clock_activity_ts.load(Ordering::Relaxed);
                      ts > 0 && now.saturating_sub(ts) < PULSE_MS };
        let tx_color = if tx_on { THEME.warn   } else { THEME.text_dim };
        let rx_color = if rx_on { THEME.accent } else { THEME.text_dim };
        let ck_color = if ck_on { THEME.ok     } else { THEME.text_dim };

        let target_ip   = self.data.params.target_ip.lock().clone();
        let target_port = *self.data.params.target_port.lock();
        let device_label = {
            let (name, model) = self.data.connected_device.lock().clone();
            if !name.is_empty() || !model.is_empty() {
                let dev = DeviceInfo {
                    ip: target_ip.clone(), port: target_port, name, model,
                    latency_ms: None, all_addrs: vec![],
                };
                dev.display_name()
            } else if connected {
                format!("{}:{}", target_ip, target_port)
            } else {
                "Disconnected".to_string()
            }
        };

        let header = Row::new()
            .push(t!("ETHER").size(20).font(Font::Default).color(THEME.accent))
            .push(t!("TAP")  .size(20).font(Font::Default).color(THEME.text))
            .push(Space::with_width(Length::Fill))
            .push(t!(conn_icon).size(11).font(SOLAR_BOLD).color(conn_color))
            .push(Space::with_width(Length::Units(5)))
            .push(t!(&device_label).size(11).color(
                if connected { THEME.text_dim } else { THEME.muted }
            ))
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
            .align_items(Alignment::Center);

        // ── Network config + scan + connect ──────────────────────────────
        let (ip_input, port_input): (Element<'_, Message>, Element<'_, Message>) = if connected {
            (
                TextInput::new(&mut self.ip_state, "IP address", &self.ip_buf, Message::IpEdited)
                    .size(11).font(MONO_FONT).padding(4).width(Length::FillPortion(3))
                    .style(EtherInputLocked).into(),
                TextInput::new(&mut self.port_state, "Port", &self.port_buf, Message::PortEdited)
                    .size(11).font(MONO_FONT).padding(4).width(Length::FillPortion(1))
                    .style(EtherInputLocked).into(),
            )
        } else {
            (
                TextInput::new(&mut self.ip_state, "IP address", &self.ip_buf, Message::IpEdited)
                    .size(11).font(MONO_FONT).padding(4).width(Length::FillPortion(3))
                    .style(EtherInput).into(),
                TextInput::new(&mut self.port_state, "Port", &self.port_buf, Message::PortEdited)
                    .size(11).font(MONO_FONT).padding(4).width(Length::FillPortion(1))
                    .style(EtherInput).into(),
            )
        };

        let scan_btn = {
            let icon_color = if connected { THEME.surface_border } else { THEME.text_dim };
            let inner = Row::new()
                .push(t!(icon::SCAN).size(11).font(SOLAR_BOLD).color(icon_color))
                .push(Space::with_width(Length::Units(4)))
                .push(t!("Scan").size(10).color(icon_color))
                .align_items(Alignment::Center);
            let btn = Button::new(&mut self.btn_scan, inner)
                .style(EtherBtn(if connected { BtnKind::Disabled } else { BtnKind::Idle }))
                .padding([4, 8]);
            if connected { btn } else { btn.on_press(Message::ScanTargets) }
        };

        // Fixed width so the button doesn't resize between "Connect"/"Disconnect".
        // Container with Fill + center_x centers the text within that fixed width.
        let conn_btn = if connected {
            Button::new(
                &mut self.btn_connect,
                Container::new(t!("Disconnect").size(10).color(THEME.ok))
                    .width(Length::Fill).center_x(),
            )
            .on_press(Message::Disconnect)
            .style(EtherBtn(BtnKind::Enabled))
            .padding([4, 6])
            .width(Length::Units(74))
        } else {
            Button::new(
                &mut self.btn_connect,
                Container::new(t!("Connect").size(10).color(THEME.err))
                    .width(Length::Fill).center_x(),
            )
            .on_press(Message::Connect)
            .style(EtherBtn(BtnKind::Error))
            .padding([4, 6])
            .width(Length::Units(74))
        };

        let net_row = Row::new()
            .push(t!("Target  ").size(11).color(THEME.text_dim))
            .push(ip_input)
            .push(t!("  :  ").size(11).color(THEME.text_dim))
            .push(port_input)
            .push(Space::with_width(Length::Units(6)))
            .push(scan_btn)
            .push(Space::with_width(Length::Units(4)))
            .push(conn_btn)
            .align_items(Alignment::Center);

        // ── Slot selector ─────────────────────────────────────────────────
        //
        // Each slot column: [button, gap(2), type label].
        // "All" and "Query" reserve a matching spacer below so button text
        // baselines align across the row regardless of label presence.

        let slot_cols = self.slot_states.iter_mut().zip(1u8..=8u8).fold(
            Row::new().spacing(4).align_items(Alignment::Start),
            |row, (state, slot)| {
                let is_compat  = !post_audit || compatible.contains(&slot);
                let is_sel     = !all_mode && slot == cur_slot && is_compat;
                let is_all_sel = all_mode && compatible.contains(&slot);

                let kind = if !is_compat {
                    BtnKind::Disabled
                } else if is_sel || is_all_sel {
                    BtnKind::Active
                } else {
                    BtnKind::Idle
                };
                let text_color = match kind {
                    BtnKind::Active   => THEME.selected_text,
                    BtnKind::Disabled => rgb(45, 45, 58),
                    _                 => THEME.muted,
                };
                let btn = Button::new(
                    state,
                    Container::new(t!(slot.to_string()).size(11).color(text_color))
                        .center_x(),
                )
                .style(EtherBtn(kind))
                .padding([4, 7]);
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
                    let color = if compatible.contains(&slot) {
                        THEME.ok
                    } else if occupied.contains(&slot) {
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
                } else { "" };

                let slot_elem: Element<'_, Message> = if !long_name.is_empty() {
                    tooltip::Tooltip::new(
                        slot_col,
                        long_name,
                        tooltip::Position::Bottom,
                    )
                    .size(11)
                    .gap(2)
                    .padding(4)
                    .style(TooltipCard)
                    .into()
                } else {
                    slot_col.into()
                };

                row.push(slot_elem)
            },
        );

        // "Auto" button (was "All") + "Query" button.
        // Matching Column wrapper (with blank spacer below) keeps button baselines
        // aligned with the numbered slot buttons.
        let auto_col = Column::new()
            .push(
                Button::new(
                    &mut self.btn_auto,
                    Container::new(t!("Auto").size(11).color(
                        if all_mode { THEME.selected_text } else { THEME.muted },
                    )).center_x(),
                )
                .on_press(Message::ToggleAutoSlots)
                .style(EtherBtn(if all_mode { BtnKind::Active } else { BtnKind::Idle }))
                .padding([4, 8]),
            )
            .push(Space::with_height(Length::Units(2)))
            .push(Space::with_height(Length::Units(11)))
            .align_items(Alignment::Center);

        let query_col = Column::new()
            .push(
                Button::new(
                    &mut self.btn_query,
                    Row::new()
                        .push(t!(icon::SCAN).size(11).font(SOLAR_BOLD).color(THEME.text_dim))
                        .push(Space::with_width(Length::Units(4)))
                        .push(t!("Query").size(10).color(THEME.text_dim))
                        .align_items(Alignment::Center),
                )
                .on_press(Message::QuerySlots)
                .style(EtherBtn(BtnKind::Idle))
                .padding([4, 8]),
            )
            .push(Space::with_height(Length::Units(2)))
            .push(Space::with_height(Length::Units(11)))
            .align_items(Alignment::Center);

        // "FX SLOTS" label — padded by 5 px at the top so its text baseline
        // aligns with the button text (buttons have 5 px top padding).
        let label_col = Column::new()
            .push(Space::with_height(Length::Units(5)))
            .push(t!("FX SLOTS").size(11).color(THEME.text_dim))
            .align_items(Alignment::Start);

        let fx_row = Row::new()
            .push(label_col)
            .push(Space::with_width(Length::Units(8)))
            .push(slot_cols)
            .push(Space::with_width(Length::Fill))
            .push(auto_col)
            .push(Space::with_width(Length::Units(6)))
            .push(query_col)
            .align_items(Alignment::Start);

        // ── FX type filter row (always visible below the slot row) ────────
        //
        // Seven toggle buttons; each enables/disables a delay type in Auto mode.
        let filter = *self.data.params.fx_type_filter.lock();
        const TYPE_BITS: &[(&str, u8, &str)] = &[
            ("Delay", 0, "Stereo Delay"),
            ("3 Tap", 1, "3-Tap Delay — three echoes, delay time at par/01"),
            ("4 Tap", 2, "4-Tap Delay — four echoes, delay time at par/01"),
            ("D+Rev", 3, "Delay + Reverb"),
            ("D+Cho", 4, "Delay + Chorus"),
            ("D+Fln", 5, "Delay + Flanger"),
            ("Mod",   6, "Modulated Delay — chorused delay, delay time at par/02"),
        ];
        let mut filter_row = Row::new()
            .push(t!("AUTO").size(9).color(THEME.text_dim))
            .push(Space::with_width(Length::Units(6)))
            .spacing(4)
            .align_items(Alignment::Center);
        for (state, &(name, bit, tip)) in self.btn_fx_type.iter_mut().zip(TYPE_BITS.iter()) {
            let on = (filter >> bit) & 1 == 1;
            let btn = Button::new(
                state,
                t!(name).size(10)
                    .color(if on { THEME.selected_text } else { THEME.muted }),
            )
            .on_press(Message::ToggleFxType(bit))
            .style(EtherBtn(if on { BtnKind::Active } else { BtnKind::Idle }))
            .padding([4, 8]);
            filter_row = filter_row.push(
                tooltip::Tooltip::new(btn, tip, tooltip::Position::Bottom)
                    .size(11).gap(2).padding(4).style(TooltipCard),
            );
        }
        let fx_filter_row: Element<'_, Message> = filter_row.into();

        // ── Telemetry (host + mixer on one line) ──────────────────────────
        let host_bpm_str   = if host_bpm_f > 0.0 { format!("{host_bpm_f:>7.2} BPM") }
                             else { "     --- BPM".into() };
        let host_float_str = if host_bpm_f > 0.0 { format!("{host_float:.4}") }
                             else { "------".into() };
        let hw_bpm_str     = if has_hw { format!("{:>7.2} BPM", hw_bpm) }
                             else { "     --- BPM".into() };
        let hw_float_str   = if has_hw { format!("{hw_float:.4}") }
                             else { "------".into() };

        let sync_badge: Element<'_, Message> = if !has_hw {
            Row::new()
                .push(t!("○").size(10).color(THEME.text_dim))
                .push(Space::with_width(Length::Units(4)))
                .push(t!("NO DATA").size(11).color(THEME.text_dim))
                .align_items(Alignment::Center).into()
        } else if in_sync {
            Row::new()
                .push(t!("●").size(10).color(THEME.ok))
                .push(Space::with_width(Length::Units(4)))
                .push(t!("MATCH").size(11).color(THEME.ok))
                .align_items(Alignment::Center).into()
        } else {
            Row::new()
                .push(t!("●").size(10).color(THEME.err))
                .push(Space::with_width(Length::Units(4)))
                .push(t!("DRIFT").size(11).color(THEME.err))
                .align_items(Alignment::Center).into()
        };

        let telem_row = Row::new()
            .push(t!("Host ").size(11).color(THEME.text_dim))
            .push(t!(host_bpm_str).size(11).color(THEME.text))
            .push(Space::with_width(Length::Units(4)))
            .push(t!(icon::ARROW_RIGHT).size(13).font(SOLAR_BOLD).color(THEME.text_dim))
            .push(Space::with_width(Length::Units(4)))
            .push(t!(host_float_str).size(11).color(THEME.text))
            .push(Space::with_width(Length::Fill))
            .push(t!("Mixer ").size(11).color(THEME.text_dim))
            .push(t!(hw_bpm_str).size(11).color(THEME.text))
            .push(Space::with_width(Length::Units(4)))
            .push(t!(icon::ARROW_LEFT).size(13).font(SOLAR_BOLD).color(THEME.text_dim))
            .push(Space::with_width(Length::Units(4)))
            .push(t!(hw_float_str).size(11).color(THEME.text))
            .push(Space::with_width(Length::Units(10)))
            .push(sync_badge)
            .align_items(Alignment::Center);

        // ── Output clock section (PPQ + MIDI OUT device + toggle — one row) ─
        let clock_on  = *self.data.params.midi_clock_enabled.lock();
        let clock_ppq = *self.data.params.midi_clock_ppq.lock();
        let clk_color = if clock_on { THEME.ok } else { THEME.muted };

        const PPQ_OPTIONS: &[u8] = &[3, 4, 6, 8, 12, 16, 24, 32, 48, 96];

        // ── MIDI bridge device + clock enable — single row ────────────────
        // Layout: OUTPUT  PPQ [ppq]  OUT [device=Fill]  [status]  [MIDI CLK]
        let current_out_device = self.data.params.midi_out_device.lock().clone();
        let bridge_conn = self.data.midi_bridge_connected.load(Ordering::Relaxed);
        let device_selected = current_out_device.is_some();

        // Status indicator: check icon (green) when connected, refresh icon
        // (yellow) while reconnecting.  Hidden when no device is selected.
        let bridge_status: Element<'_, Message> = if device_selected {
            let (dot, color) = if bridge_conn {
                ("●", THEME.ok)
            } else {
                ("●", THEME.warn)
            };
            t!(dot).size(10).color(color).into()
        } else {
            Space::with_width(Length::Units(11)).into()
        };

        let selected_display = current_out_device
            .unwrap_or_else(|| MIDI_OUT_NONE.to_string());

        let clock_row = Row::new()
            .push(t!("OUTPUT").size(9).color(THEME.text_dim))
            .push(Space::with_width(Length::Units(5)))
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
                .width(Length::Units(52))
                .style(PpqPickStyle),
            )
            .push(Space::with_width(Length::Units(8)))
            .push(t!("OUT").size(9).color(THEME.text_dim))
            .push(Space::with_width(Length::Units(4)))
            .push(
                PickList::new(
                    &mut self.pick_midi_out,
                    self.midi_out_ports.as_slice(),
                    Some(selected_display),
                    Message::SetMidiOutDevice,
                )
                .text_size(10)
                .font(MONO_FONT)
                .padding([4, 6])
                .width(Length::Fill)
                .style(PpqPickStyle),
            )
            .push(Space::with_width(Length::Units(4)))
            .push(bridge_status)
            .push(Space::with_width(Length::Units(6)))
            .push(
                Button::new(
                    &mut self.btn_clock_toggle,
                    Row::new()
                        .push(t!(icon::CLOCK).size(11).font(SOLAR_BOLD).color(clk_color))
                        .push(Space::with_width(Length::Units(4)))
                        .push(t!("MIDI CLK").size(10).color(clk_color))
                        .align_items(Alignment::Center),
                )
                .on_press(Message::ToggleMidiClock)
                .style(EtherBtn(if clock_on { BtnKind::Enabled } else { BtnKind::Idle }))
                .padding([4, 8]),
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
            let stats   = *self.data.midi_clock_stats.lock();
            let has_data = clock_on && stats.sample_n >= 48;

            // Colour for the p99 / max values.
            let p99_color = if !has_data             { THEME.text_dim }
                else if stats.p99_us > 5_000         { THEME.err      }
                else if stats.p99_us > 2_000         { THEME.warn     }
                else                                 { THEME.ok       };
            let max_color = if !has_data             { THEME.text_dim }
                else if stats.max_us > 10_000        { THEME.err      }
                else if stats.max_us > 5_000         { THEME.warn     }
                else                                 { THEME.ok       };

            // Pre-format every field to a fixed character count so that the
            // monospace Row never shifts even as values change magnitude.
            // avg: {:5.1} → "  8.3" … "125.0"  (5 chars)
            // jitter: {:5} → "    0" … "99999"  (5 chars)
            let avg_str = if has_data {
                format!("{:5.1}", stats.interval_us as f32 / 1_000.0)
            } else { " --.-".to_string() };
            let p50_str = if has_data { format!("{:5}", stats.p50_us) }
                          else        { "   --".to_string()           };
            let p95_str = if has_data { format!("{:5}", stats.p95_us) }
                          else        { "   --".to_string()           };
            let p99_str = if has_data { format!("{:5}", stats.p99_us) }
                          else        { "   --".to_string()           };
            let max_str = if has_data { format!("{:5}", stats.max_us) }
                          else        { "   --".to_string()           };

            // Split into dim labels + variably-coloured values so p99/max
            // can turn yellow/red while keeping a single monospace typeface.
            // Every string literal here has a fixed char count; values are
            // pre-formatted above to the same width, so columns are stable.
            Row::new()
                .push(Space::with_width(Length::Fill))
                .push(t!("avg ").size(8).color(THEME.text_dim))
                .push(t!(avg_str).size(8).color(THEME.text_dim))
                .push(t!("ms  p50\u{b1}").size(8).color(THEME.text_dim))
                .push(t!(p50_str).size(8).font(MONO_FONT)
                    .color(if has_data { THEME.ok } else { THEME.text_dim }))
                .push(t!("\u{b5}s  p95\u{b1}").size(8).color(THEME.text_dim))
                .push(t!(p95_str).size(8).font(MONO_FONT)
                    .color(if has_data { THEME.ok } else { THEME.text_dim }))
                .push(t!("\u{b5}s  p99\u{b1}").size(8).color(THEME.text_dim))
                .push(t!(p99_str).size(8).color(p99_color))
                .push(t!("\u{b5}s  max\u{b1}").size(8).color(THEME.text_dim))
                .push(t!(max_str).size(8).color(max_color))
                .push(t!("\u{b5}s").size(8).color(THEME.text_dim))
                .align_items(Alignment::Center)
                .into()
        };

        // ── Rate Sync row ─────────────────────────────────────────────────
        let rate_row = Row::new()
            .push(t!("RATE").size(9).color(THEME.text_dim))
            .push(Space::with_width(Length::Units(5)))
            .push(sync_btn(&mut self.btn_rate_manual,  "Man",  rate_mode == SyncMode::Manual,
                Message::SetRateSyncMode(SyncMode::Manual)))
            .push(Space::with_width(Length::Units(3)))
            .push(sync_btn(&mut self.btn_rate_change,  "BPM",  rate_mode == SyncMode::OnChange,
                Message::SetRateSyncMode(SyncMode::OnChange)))
            .push(Space::with_width(Length::Units(3)))
            .push(sync_btn(&mut self.btn_rate_cont,    "Cont", rate_mode == SyncMode::Continuous,
                Message::SetRateSyncMode(SyncMode::Continuous)))
            .push(Space::with_width(Length::Units(5)))
            .push(force_icon_btn(&mut self.btn_rate_force, Message::ForceRateSync))
            .push(Space::with_width(Length::Fill))
            .push(t!("PHASE").size(9).color(THEME.text_dim))
            .push(Space::with_width(Length::Units(5)))
            .push(sync_btn(&mut self.btn_phase_manual, "Man",  phase_mode == SyncMode::Manual,
                Message::SetPhaseSyncMode(SyncMode::Manual)))
            .push(Space::with_width(Length::Units(3)))
            .push(sync_btn(&mut self.btn_phase_change, "BPM",  phase_mode == SyncMode::OnChange,
                Message::SetPhaseSyncMode(SyncMode::OnChange)))
            .push(Space::with_width(Length::Units(3)))
            .push(sync_btn(&mut self.btn_phase_cont,   "Cont", phase_mode == SyncMode::Continuous,
                Message::SetPhaseSyncMode(SyncMode::Continuous)))
            .push(Space::with_width(Length::Units(5)))
            .push(force_icon_btn(&mut self.btn_phase_force, Message::ForcePhaseSync))
            .align_items(Alignment::Center);

        // ── Assembly ──────────────────────────────────────────────────────
        //
        // Equal Length::Fill gaps between every section so the layout scales
        // uniformly with the window height.
        let content = Column::new()
            .push(header)
            .push(Space::with_height(Length::Fill))
            .push(telem_row)
            .push(Space::with_height(Length::Fill))
            .push(net_row)
            .push(Space::with_height(Length::Fill))
            .push(fx_row)
            .push(Space::with_height(4.into()))
            .push(fx_filter_row)
            .push(Space::with_height(Length::Fill))
            .push(clock_row)
            .push(Space::with_height(3.into()))
            .push(clock_stats_row)
            .push(Space::with_height(Length::Fill))
            .push(rate_row)
            .padding(10u16)
            .spacing(0);

        Container::new(content)
            .width(Length::Fill)
            .height(Length::Fill)
            .into()
    }

    fn background_color(&self) -> Color { THEME.bg }
}

// ─── View helpers ─────────────────────────────────────────────────────────────

/// Compact radio-style sync mode button (Man / BPM / Cont).
fn sync_btn<'a>(
    state: &'a mut button::State,
    label: &'static str,
    selected: bool,
    msg: Message,
) -> Button<'a, Message> {
    Button::new(
        state,
        Container::new(
            t!(label).size(10)
                .color(if selected { THEME.selected_text } else { THEME.muted }),
        ).center_x(),
    )
    .on_press(msg)
    .style(EtherBtn(if selected { BtnKind::Active } else { BtnKind::Idle }))
    .padding([4, 8])
}

/// Bolt-only force-sync button (no text label).
fn force_icon_btn(state: &mut button::State, msg: Message) -> Button<'_, Message> {
    Button::new(
        state,
        t!(icon::BOLT).size(11).font(SOLAR_BOLD).color(THEME.danger_text),
    )
    .on_press(msg)
    .style(EtherBtn(BtnKind::Force))
    .padding([4, 8])
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
