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

use nih_plug::prelude::{GuiContext, ParamSetter};
use nih_plug_iced::{
    button, container, create_iced_editor, executor, text_input,
    widget::{Button, Column, Container, Row, Space, Text, TextInput},
    Alignment, Background, Color, Command, Element, Font, IcedEditor, Length, WindowQueue,
};
use parking_lot::Mutex;

use crate::{
    network::{now_ms, DeviceInfo, NetworkCommand},
    osc,
    params::{EtherTapParams, SyncMode},
};

// ─── Solar Icons Bold font ───────────────────────────────────────────────────

const SOLAR_BOLD: Font = Font::External {
    name: "Solar Icon Set Bold",
    bytes: include_bytes!("../assets/Solar-Icon-Set_Bold.ttf"),
};

// TODO: add assets/Solaar.ttf and replace Font::Default with:
// const LOGO_FONT: Font = Font::External { name: "Solaar", bytes: include_bytes!("../assets/Solaar.ttf") };

// ─── Icon codepoints (Solar Icon Set Bold, PUA) ──────────────────────────────

mod icon {
    pub const LINK: &str        = "\u{ecf2}"; // si-Link — connected
    pub const LINK_BROKEN: &str = "\u{ecf3}"; // si-Link-Broken — disconnected
    pub const RX: &str          = "\u{e94c}"; // si-Download-Minimalistic
    pub const ARROW_RIGHT: &str = "\u{e908}"; // si-Arrow-Right
    pub const ARROW_LEFT: &str  = "\u{e905}"; // si-Arrow-Left
    pub const CHECK: &str       = "\u{ea56}"; // si-Check-Circle
    pub const REFRESH: &str     = "\u{e910}"; // si-Refresh
    pub const BOLT: &str        = "\u{ea50}"; // si-Bolt — force / destructive
    pub const SCAN: &str        = "\u{ec8a}"; // si-Scanner
}

// ─── Theme ───────────────────────────────────────────────────────────────────
//
// Edit ONLY the colour values inside `Theme::dark()` to restyle the entire UI.
// Field names are intentionally semantic (not widget-specific) so this block
// reads as a design-token palette.

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
            selected_bg:     rgb( 28,  52,  90),
            selected_border: rgb( 55, 105, 185),
            selected_text:   rgb(100, 170, 255),
            danger_bg:       rgb( 75,  35,  12),
            danger_border:   rgb(175,  85,  25),
            danger_text:     rgb(225, 175,  50),
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
enum BtnKind { Idle, Active, Force, Disabled }

struct EtherBtn(BtnKind);

impl button::StyleSheet for EtherBtn {
    fn active(&self) -> button::Style {
        match self.0 {
            BtnKind::Idle => button::Style {
                background: Some(Background::Color(THEME.surface)),
                border_radius: 3.0, border_width: 1.0,
                border_color: THEME.surface_border,
                text_color: THEME.muted,
                ..Default::default()
            },
            BtnKind::Active => button::Style {
                background: Some(Background::Color(THEME.selected_bg)),
                border_radius: 3.0, border_width: 1.0,
                border_color: THEME.selected_border,
                text_color: THEME.selected_text,
                ..Default::default()
            },
            BtnKind::Force => button::Style {
                background: Some(Background::Color(THEME.danger_bg)),
                border_radius: 3.0, border_width: 1.0,
                border_color: THEME.danger_border,
                text_color: THEME.danger_text,
                ..Default::default()
            },
            BtnKind::Disabled => button::Style {
                background: Some(Background::Color(THEME.bg)),
                border_radius: 3.0, border_width: 1.0,
                border_color: rgb(38, 38, 50),
                text_color: rgb(45, 45, 58),
                ..Default::default()
            },
        }
    }

    fn hovered(&self) -> button::Style {
        lighten(self.active(), 0.07)
    }

    fn pressed(&self) -> button::Style {
        lighten(self.active(), -0.05)
    }
}

// ─── Text-input stylesheet ────────────────────────────────────────────────────

struct EtherInput;

impl text_input::StyleSheet for EtherInput {
    fn active(&self) -> text_input::Style {
        text_input::Style {
            background: Background::Color(THEME.bg),
            border_radius: 3.0,
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
            background: Background::Color(rgb(20, 20, 28)),
            border_radius: 3.0,
            border_width: 1.0,
            border_color: rgb(38, 38, 50),
        }
    }
    fn focused(&self)      -> text_input::Style { self.active() }
    fn hovered(&self)      -> text_input::Style { self.active() }
    fn placeholder_color(&self) -> Color { rgb(45, 45, 58) }
    fn value_color(&self)       -> Color { THEME.text_dim }
    fn selection_color(&self)   -> Color { THEME.bg }
}

// ─── Container stylesheets ────────────────────────────────────────────────────

/// Card surface used for the scan popup.
struct ModalCard;
impl container::StyleSheet for ModalCard {
    fn style(&self) -> container::Style {
        container::Style {
            background: Some(Background::Color(THEME.surface)),
            border_radius: 6.0,
            border_width: 1.0,
            border_color: THEME.surface_border,
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

// ─── Shared data bundle ───────────────────────────────────────────────────────

pub struct EditorData {
    pub params:             Arc<EtherTapParams>,
    pub conn_status:        Arc<AtomicBool>,
    pub tx_activity_ts:     Arc<AtomicU64>,
    pub rx_activity_ts:     Arc<AtomicU64>,
    pub hardware_float:     Arc<AtomicU32>,
    pub host_bpm:           Arc<AtomicU32>,
    pub force_sync_trigger: Arc<AtomicBool>,
    pub force_rate_trigger: Arc<AtomicBool>,
    pub compatible_slots:   Arc<Mutex<Vec<u8>>>,
    pub occupied_slots:     Arc<Mutex<Vec<u8>>>,
    pub all_slots_mode:     Arc<AtomicBool>,
    pub scan_targets:       Arc<Mutex<Vec<DeviceInfo>>>,
    /// Name and model parsed from /info heartbeat responses.
    pub connected_device:   Arc<Mutex<(String, String)>>,
    pub cmd_tx:             crossbeam_channel::Sender<NetworkCommand>,
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
    // Rate Sync radio group + force
    btn_rate_manual: button::State,
    btn_rate_change: button::State,
    btn_rate_cont:   button::State,
    btn_rate_force:  button::State,
    // Phase Sync radio group + force
    btn_phase_manual: button::State,
    btn_phase_change: button::State,
    btn_phase_cont:   button::State,
    btn_phase_force:  button::State,
    // FX row controls
    btn_all:     button::State,
    btn_query:   button::State,
    slot_states: [button::State; 8],
    // Network scan — btn_scan doubles as the modal close button
    btn_scan:           button::State,
    btn_connect:        button::State,
    scan_result_states: [button::State; 8],
    show_scan_results:  bool,
    // Track last values written to read-only DAW params to avoid spamming setter
    last_param_connected: bool,
    last_param_matched:   bool,
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
    ToggleAllSlots,
    ScanTargets,
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
                btn_all:          Default::default(),
                btn_query:        Default::default(),
                slot_states:      Default::default(),
                btn_scan:           Default::default(),
                btn_connect:        Default::default(),
                scan_result_states: Default::default(),
                show_scan_results:  false,
                last_param_connected: false,
                last_param_matched:   false,
            },
            Command::none(),
        )
    }

    fn context(&self) -> &dyn GuiContext { self.context.as_ref() }

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
            Message::ToggleAllSlots => {
                let prev = self.data.all_slots_mode.load(Ordering::Relaxed);
                self.data.all_slots_mode.store(!prev, Ordering::Relaxed);
            }
            Message::ScanTargets => {
                self.show_scan_results = !self.show_scan_results;
                if self.show_scan_results {
                    self.data.scan_targets.lock().clear();
                    let _ = self.data.cmd_tx.try_send(NetworkCommand::ScanTargets);
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
        let all_mode   = self.data.all_slots_mode.load(Ordering::Relaxed);
        let post_audit = !compatible.is_empty() || !occupied.is_empty();

        // ── Scan popup modal ──────────────────────────────────────────────
        //
        // When open, we return a completely different view (full-window
        // dark card) so the main layout height never changes.
        if self.show_scan_results {
            let scan_targets_snap = self.data.scan_targets.lock().clone();

            let mut card_col = Column::new()
                .push(
                    Row::new()
                        .push(Text::new("DISCOVERED DEVICES").size(9).color(THEME.text_dim))
                        .push(Space::with_width(Length::Fill))
                        .push(
                            Button::new(
                                &mut self.btn_scan,
                                Text::new("\u{00d7}").size(13).color(THEME.muted),
                            )
                            .on_press(Message::ScanTargets)
                            .style(EtherBtn(BtnKind::Idle))
                            .padding([2, 7]),
                        )
                        .align_items(Alignment::Center),
                )
                .push(Space::with_height(Length::Units(8)))
                .spacing(4);

            if scan_targets_snap.is_empty() {
                card_col = card_col.push(
                    Text::new("  Scanning\u{2026}").size(11).color(THEME.muted),
                );
            } else {
                for (state, dev) in self.scan_result_states.iter_mut()
                    .zip(scan_targets_snap.iter())
                {
                    let name_line = dev.display_name();
                    let addr_line = format!("{}:{}", dev.ip, dev.port);
                    let entry = Column::new()
                        .push(Text::new(name_line).size(11).color(THEME.text))
                        .push(Text::new(addr_line).size(9).color(THEME.text_dim))
                        .spacing(1);
                    card_col = card_col.push(
                        Button::new(state, entry)
                            .on_press(Message::SelectTarget(dev.ip.clone(), dev.port))
                            .style(EtherBtn(BtnKind::Idle))
                            .padding([5, 10])
                            .width(Length::Fill),
                    );
                }
            }

            let card = Container::new(card_col)
                .padding(14)
                .style(ModalCard)
                .width(Length::Units(270));

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
        let tx_color = if tx_on { THEME.warn   } else { THEME.text_dim };
        let rx_color = if rx_on { THEME.accent } else { THEME.text_dim };

        let target_ip   = self.data.params.target_ip.lock().clone();
        let target_port = *self.data.params.target_port.lock();
        let device_label = {
            let (name, model) = self.data.connected_device.lock().clone();
            if !name.is_empty() || !model.is_empty() {
                let dev = DeviceInfo { ip: target_ip.clone(), port: target_port, name, model };
                dev.display_name()
            } else if connected {
                format!("{}:{}", target_ip, target_port)
            } else {
                "Disconnected".to_string()
            }
        };

        // Update read-only DAW params whenever connected/matched state changes.
        if connected != self.last_param_connected {
            let setter = ParamSetter::new(self.context.as_ref());
            setter.begin_set_parameter(&self.data.params.is_connected);
            setter.set_parameter(&self.data.params.is_connected, connected);
            setter.end_set_parameter(&self.data.params.is_connected);
            self.last_param_connected = connected;
        }
        if in_sync != self.last_param_matched {
            let setter = ParamSetter::new(self.context.as_ref());
            setter.begin_set_parameter(&self.data.params.is_matched);
            setter.set_parameter(&self.data.params.is_matched, in_sync);
            setter.end_set_parameter(&self.data.params.is_matched);
            self.last_param_matched = in_sync;
        }

        let header = Row::new()
            .push(Text::new("ETHER").size(20).font(Font::Default).color(THEME.accent))
            .push(Text::new("TAP")  .size(20).font(Font::Default).color(THEME.text))
            .push(Space::with_width(Length::Fill))
            .push(Text::new(conn_icon).size(11).font(SOLAR_BOLD).color(conn_color))
            .push(Space::with_width(Length::Units(5)))
            .push(Text::new(&device_label).size(11).color(
                if connected { THEME.text_dim } else { THEME.muted }
            ))
            .push(Space::with_width(Length::Fill))
            .push(Text::new("TX").size(10).color(tx_color))
            .push(Space::with_width(Length::Units(8)))
            .push(Text::new("RX").size(10).color(rx_color))
            .align_items(Alignment::Center);

        // ── Network config + scan + connect ──────────────────────────────
        let (ip_input, port_input): (Element<'_, Message>, Element<'_, Message>) = if connected {
            (
                TextInput::new(&mut self.ip_state, "IP address", &self.ip_buf, Message::IpEdited)
                    .size(11).padding(4).width(Length::FillPortion(3))
                    .style(EtherInputLocked).into(),
                TextInput::new(&mut self.port_state, "Port", &self.port_buf, Message::PortEdited)
                    .size(11).padding(4).width(Length::FillPortion(1))
                    .style(EtherInputLocked).into(),
            )
        } else {
            (
                TextInput::new(&mut self.ip_state, "IP address", &self.ip_buf, Message::IpEdited)
                    .size(11).padding(4).width(Length::FillPortion(3))
                    .style(EtherInput).into(),
                TextInput::new(&mut self.port_state, "Port", &self.port_buf, Message::PortEdited)
                    .size(11).padding(4).width(Length::FillPortion(1))
                    .style(EtherInput).into(),
            )
        };

        let scan_btn = {
            let inner = Row::new()
                .push(Text::new(icon::SCAN).size(11).font(SOLAR_BOLD)
                    .color(if connected { rgb(45, 45, 58) } else { THEME.text_dim }))
                .push(Space::with_width(Length::Units(4)))
                .push(Text::new("Scan").size(10)
                    .color(if connected { rgb(45, 45, 58) } else { THEME.text_dim }))
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
                Container::new(Text::new("Disconnect").size(10).color(THEME.danger_text))
                    .width(Length::Fill).center_x(),
            )
            .on_press(Message::Disconnect)
            .style(EtherBtn(BtnKind::Force))
            .padding([4, 6])
            .width(Length::Units(74))
        } else {
            Button::new(
                &mut self.btn_connect,
                Container::new(Text::new("Connect").size(10).color(THEME.selected_text))
                    .width(Length::Fill).center_x(),
            )
            .on_press(Message::Connect)
            .style(EtherBtn(BtnKind::Active))
            .padding([4, 6])
            .width(Length::Units(74))
        };

        let net_row = Row::new()
            .push(Text::new("Target  ").size(11).color(THEME.text_dim))
            .push(ip_input)
            .push(Text::new("  :  ").size(11).color(THEME.text_dim))
            .push(port_input)
            .push(Space::with_width(Length::Units(6)))
            .push(scan_btn)
            .push(Space::with_width(Length::Units(4)))
            .push(conn_btn)
            .align_items(Alignment::Center);

        // ── Slot selector ─────────────────────────────────────────────────
        //
        // Each slot is a Column: [button, gap(2), indicator dot].
        // "All" and "Query" use the same structure so their button text
        // top-aligns with the slot button text when the row uses Alignment::Start.
        let dot_placeholder = Length::Units(11); // reserves space for dot row

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
                    Container::new(Text::new(slot.to_string()).size(11).color(text_color))
                        .center_x(),
                )
                .style(EtherBtn(kind))
                .padding([4, 7]);
                let btn = if is_compat && !all_mode {
                    btn.on_press(Message::SlotSelected(slot))
                } else {
                    btn
                };

                let dot_color = if !post_audit {
                    THEME.surface_border
                } else if compatible.contains(&slot) {
                    THEME.ok
                } else if occupied.contains(&slot) {
                    THEME.warn
                } else {
                    THEME.text_dim
                };

                let slot_col = Column::new()
                    .push(btn)
                    .push(Space::with_height(Length::Units(2)))
                    .push(Text::new("\u{2022}").size(9).color(dot_color))
                    .align_items(Alignment::Center);

                row.push(slot_col)
            },
        );

        // "All" and "Query" in identical Column structure — dot row is a blank
        // spacer so button text sits at the same y-offset as slot button text.
        let all_col = Column::new()
            .push(
                Button::new(
                    &mut self.btn_all,
                    Container::new(Text::new("All").size(11).color(
                        if all_mode { THEME.selected_text } else { THEME.muted },
                    )).center_x(),
                )
                .on_press(Message::ToggleAllSlots)
                .style(EtherBtn(if all_mode { BtnKind::Active } else { BtnKind::Idle }))
                .padding([4, 10]),
            )
            .push(Space::with_height(Length::Units(2)))
            .push(Space::with_height(dot_placeholder))
            .align_items(Alignment::Center);

        let query_col = Column::new()
            .push(
                Button::new(
                    &mut self.btn_query,
                    Row::new()
                        .push(Text::new(icon::SCAN).size(11).font(SOLAR_BOLD).color(THEME.text_dim))
                        .push(Space::with_width(Length::Units(4)))
                        .push(Text::new("Query").size(10).color(THEME.text_dim))
                        .align_items(Alignment::Center),
                )
                .on_press(Message::QuerySlots)
                .style(EtherBtn(BtnKind::Idle))
                .padding([4, 10]),
            )
            .push(Space::with_height(Length::Units(2)))
            .push(Space::with_height(dot_placeholder))
            .align_items(Alignment::Center);

        // "FX (DLY)" label — padded by 5 px at the top so its text baseline
        // aligns with the button text (buttons have 5 px top padding).
        let label_col = Column::new()
            .push(Space::with_height(Length::Units(5)))
            .push(Text::new("FX (DLY)").size(11).color(THEME.text_dim))
            .align_items(Alignment::Start);

        let fx_row = Row::new()
            .push(label_col)
            .push(Space::with_width(Length::Units(8)))
            .push(slot_cols)
            .push(Space::with_width(Length::Fill))
            .push(all_col)
            .push(Space::with_width(Length::Units(6)))
            .push(query_col)
            .align_items(Alignment::Start);

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
                .push(Text::new(icon::RX).size(13).font(SOLAR_BOLD).color(THEME.text_dim))
                .push(Space::with_width(Length::Units(4)))
                .push(Text::new("NO DATA").size(11).color(THEME.text_dim))
                .align_items(Alignment::Center).into()
        } else if in_sync {
            Row::new()
                .push(Text::new(icon::CHECK).size(13).font(SOLAR_BOLD).color(THEME.ok))
                .push(Space::with_width(Length::Units(4)))
                .push(Text::new("MATCH").size(11).color(THEME.ok))
                .align_items(Alignment::Center).into()
        } else {
            Row::new()
                .push(Text::new(icon::REFRESH).size(13).font(SOLAR_BOLD).color(THEME.err))
                .push(Space::with_width(Length::Units(4)))
                .push(Text::new("DRIFT").size(11).color(THEME.err))
                .align_items(Alignment::Center).into()
        };

        let telem_row = Row::new()
            .push(Text::new("Host ").size(11).color(THEME.text_dim))
            .push(Text::new(host_bpm_str).size(11).color(THEME.text))
            .push(Space::with_width(Length::Units(4)))
            .push(Text::new(icon::ARROW_RIGHT).size(13).font(SOLAR_BOLD).color(THEME.text_dim))
            .push(Space::with_width(Length::Units(4)))
            .push(Text::new(host_float_str).size(11).color(THEME.text))
            .push(Space::with_width(Length::Fill))
            .push(Text::new("Mixer ").size(11).color(THEME.text_dim))
            .push(Text::new(hw_bpm_str).size(11).color(THEME.text))
            .push(Space::with_width(Length::Units(4)))
            .push(Text::new(icon::ARROW_LEFT).size(13).font(SOLAR_BOLD).color(THEME.text_dim))
            .push(Space::with_width(Length::Units(4)))
            .push(Text::new(hw_float_str).size(11).color(THEME.text))
            .push(Space::with_width(Length::Units(10)))
            .push(sync_badge)
            .align_items(Alignment::Center);

        // ── Rate Sync ─────────────────────────────────────────────────────
        let rate_row = Row::new()
            .push(radio_btn(&mut self.btn_rate_manual, "Manual",
                rate_mode == SyncMode::Manual, Message::SetRateSyncMode(SyncMode::Manual)))
            .push(Space::with_width(Length::Units(6)))
            .push(radio_btn(&mut self.btn_rate_change, "On Change",
                rate_mode == SyncMode::OnChange, Message::SetRateSyncMode(SyncMode::OnChange)))
            .push(Space::with_width(Length::Units(6)))
            .push(radio_btn(&mut self.btn_rate_cont, "Continuous",
                rate_mode == SyncMode::Continuous, Message::SetRateSyncMode(SyncMode::Continuous)))
            .push(Space::with_width(Length::Fill))
            .push(force_btn(&mut self.btn_rate_force, "FORCE SYNC", Message::ForceRateSync))
            .align_items(Alignment::Center);

        // ── Phase Sync ────────────────────────────────────────────────────
        let phase_row = Row::new()
            .push(radio_btn(&mut self.btn_phase_manual, "Manual",
                phase_mode == SyncMode::Manual, Message::SetPhaseSyncMode(SyncMode::Manual)))
            .push(Space::with_width(Length::Units(6)))
            .push(radio_btn(&mut self.btn_phase_change, "On Change",
                phase_mode == SyncMode::OnChange, Message::SetPhaseSyncMode(SyncMode::OnChange)))
            .push(Space::with_width(Length::Units(6)))
            .push(radio_btn(&mut self.btn_phase_cont, "Continuous",
                phase_mode == SyncMode::Continuous, Message::SetPhaseSyncMode(SyncMode::Continuous)))
            .push(Space::with_width(Length::Fill))
            .push(force_btn(&mut self.btn_phase_force, "FORCE SYNC", Message::ForcePhaseSync))
            .align_items(Alignment::Center);

        // ── Assembly ──────────────────────────────────────────────────────
        let content = Column::new()
            .push(header)
            .push(Space::with_height(6.into()))
            .push(telem_row)
            .push(Space::with_height(6.into()))
            .push(net_row)
            .push(Space::with_height(6.into()))
            .push(fx_row)
            .push(Space::with_height(6.into()))
            .push(Text::new("RATE SYNC MODE").size(9).color(THEME.text_dim))
            .push(Space::with_height(4.into()))
            .push(rate_row)
            .push(Space::with_height(6.into()))
            .push(Text::new("PHASE SYNC MODE").size(9).color(THEME.text_dim))
            .push(Space::with_height(4.into()))
            .push(phase_row)
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

fn radio_btn<'a>(
    state: &'a mut button::State,
    label: &str,
    selected: bool,
    msg: Message,
) -> Button<'a, Message> {
    Button::new(
        state,
        Container::new(
            Text::new(label).size(11)
                .color(if selected { THEME.selected_text } else { THEME.muted }),
        ).center_x(),
    )
    .on_press(msg)
    .style(EtherBtn(if selected { BtnKind::Active } else { BtnKind::Idle }))
    .padding([4, 10])
}

fn force_btn<'a>(
    state: &'a mut button::State,
    label: &'static str,
    msg: Message,
) -> Button<'a, Message> {
    Button::new(
        state,
        Row::new()
            .push(Text::new(icon::BOLT).size(13).font(SOLAR_BOLD).color(THEME.danger_text))
            .push(Space::with_width(Length::Units(5)))
            .push(Text::new(label).size(11).color(THEME.danger_text))
            .align_items(Alignment::Center),
    )
    .on_press(msg)
    .style(EtherBtn(BtnKind::Force))
    .padding([4, 12])
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
