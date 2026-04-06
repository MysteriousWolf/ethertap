/// Iced-based editor for EtherTap — Phase 2.
///
/// Uses the iced version bundled with nih_plug_iced (iced-baseview, stateful-
/// widget pattern: every interactive widget stores a `State` field here).
///
/// Layout (440 × 380):
/// ┌──────────────────────────────────────────────┐
/// │  ETHERTAP             ● Connected  ⚡TX  ·RX  │
/// │  Target: [192.168.1.100      ] : [10023]      │
/// │  FX:  [1] [2] [3] …                          │
/// ├── TELEMETRY ─────────────────────────────────┤
/// │  Host:   120.00 BPM  →  0.1667               │
/// │  Mixer:  120.10 BPM  ←  0.1666   ● MATCH     │
/// ├── SYNC MODE ─────────────────────────────────┤
/// │  [Sync on Change]  [Sync Continuous]          │
/// ├── RESET MODE ────────────────────────────────┤
/// │  [Manual Only]  [Auto + Manual]  [FORCE SYNC] │
/// ├── ───────────────────────────────────────────┤
/// │  [Audit Slots]             hint text          │
/// └──────────────────────────────────────────────┘
use std::sync::{
    atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
    Arc,
};

use nih_plug::prelude::{GuiContext, ParamSetter};
use nih_plug_iced::{
    button, create_iced_editor, executor, text_input,
    widget::{Button, Column, Container, Row, Space, Text, TextInput},
    Alignment, Background, Color, Command, Element, IcedEditor, Length, WindowQueue,
};
use parking_lot::Mutex;

use crate::{
    network::{now_ms, NetworkCommand},
    osc,
    params::EtherTapParams,
};

// ─── Pulse window ────────────────────────────────────────────────────────────

const PULSE_MS: u64 = 100;

// ─── Button stylesheet ───────────────────────────────────────────────────────

#[derive(Clone, Copy)]
enum BtnKind {
    Idle,
    Active,
    Force, // orange/red — destructive momentary
}

struct EtherBtn(BtnKind);

impl button::StyleSheet for EtherBtn {
    fn active(&self) -> button::Style {
        match self.0 {
            BtnKind::Idle => button::Style {
                background: Some(Background::Color(Color::from_rgb8(32, 32, 42))),
                border_radius: 3.0,
                border_width: 1.0,
                border_color: Color::from_rgb8(55, 55, 70),
                text_color: Color::from_rgb8(120, 120, 135),
                ..Default::default()
            },
            BtnKind::Active => button::Style {
                background: Some(Background::Color(Color::from_rgb8(28, 52, 90))),
                border_radius: 3.0,
                border_width: 1.0,
                border_color: Color::from_rgb8(55, 105, 185),
                text_color: Color::from_rgb8(100, 170, 255),
                ..Default::default()
            },
            BtnKind::Force => button::Style {
                background: Some(Background::Color(Color::from_rgb8(80, 38, 15))),
                border_radius: 3.0,
                border_width: 1.0,
                border_color: Color::from_rgb8(180, 90, 30),
                text_color: Color::from_rgb8(240, 150, 60),
                ..Default::default()
            },
        }
    }

    fn hovered(&self) -> button::Style {
        let a = self.active();
        button::Style {
            background: a.background.map(|b| {
                let Background::Color(c) = b;
                Background::Color(Color {
                    r: (c.r + 0.06).min(1.0),
                    g: (c.g + 0.06).min(1.0),
                    b: (c.b + 0.06).min(1.0),
                    a: c.a,
                })
            }),
            ..a
        }
    }
}

// ─── Shared data bundle ───────────────────────────────────────────────────────

pub struct EditorData {
    pub params: Arc<EtherTapParams>,
    pub conn_status: Arc<AtomicBool>,
    /// Timestamp of last TX packet (ms since epoch).
    pub tx_activity_ts: Arc<AtomicU64>,
    /// Timestamp of last RX packet (ms since epoch).
    pub rx_activity_ts: Arc<AtomicU64>,
    /// Last polled hardware delay float, stored as f32 bits in AtomicU32.
    pub hardware_float: Arc<AtomicU32>,
    /// Current host BPM, stored as f32 bits in AtomicU32.
    pub host_bpm: Arc<AtomicU32>,
    /// Atomic trigger: the UI sets this; the audio thread swap()-clears it.
    pub force_sync_trigger: Arc<AtomicBool>,
    pub compatible_slots: Arc<Mutex<Vec<u8>>>,
    pub cmd_tx: crossbeam_channel::Sender<NetworkCommand>,
}

// ─── Editor entry point ───────────────────────────────────────────────────────

pub fn create(data: Arc<EditorData>) -> Option<Box<dyn nih_plug::prelude::Editor>> {
    create_iced_editor::<EtherTapEditor>(data.params.editor_state.clone(), data)
}

// ─── Editor struct ────────────────────────────────────────────────────────────

struct EtherTapEditor {
    data: Arc<EditorData>,
    context: Arc<dyn GuiContext>,
    ip_buf: String,
    port_buf: String,
    // Stateful widgets (required by this iced version)
    ip_state: text_input::State,
    port_state: text_input::State,
    btn_sync_chg: button::State,
    btn_sync_cont: button::State,
    btn_hr_manual: button::State,
    btn_hr_auto: button::State,
    btn_force: button::State,
    btn_audit: button::State,
    slot_states: [button::State; 8],
}

// ─── Messages ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
enum Message {
    IpEdited(String),
    PortEdited(String),
    SlotSelected(u8),
    ToggleSyncOnChange,
    ToggleSyncContinuous,
    SetHardResetAuto(bool),
    ForceSync,
    AuditSlots,
}

// ─── IcedEditor impl ─────────────────────────────────────────────────────────

impl IcedEditor for EtherTapEditor {
    type Executor = executor::Default;
    type Message = Message;
    type InitializationFlags = Arc<EditorData>;

    fn new(data: Arc<EditorData>, context: Arc<dyn GuiContext>) -> (Self, Command<Message>) {
        let ip_buf = data.params.target_ip.lock().clone();
        let port_buf = data.params.target_port.lock().to_string();
        (
            Self {
                data,
                context,
                ip_buf,
                port_buf,
                ip_state: Default::default(),
                port_state: Default::default(),
                btn_sync_chg: Default::default(),
                btn_sync_cont: Default::default(),
                btn_hr_manual: Default::default(),
                btn_hr_auto: Default::default(),
                btn_force: Default::default(),
                btn_audit: Default::default(),
                slot_states: Default::default(),
            },
            Command::none(),
        )
    }

    fn context(&self) -> &dyn GuiContext {
        self.context.as_ref()
    }

    fn update(&mut self, _window: &mut WindowQueue, msg: Message) -> Command<Message> {
        match msg {
            Message::IpEdited(s) => {
                self.ip_buf = s.clone();
                *self.data.params.target_ip.lock() = s.clone();
                let port = *self.data.params.target_port.lock();
                let _ = self.data.cmd_tx.try_send(NetworkCommand::UpdateTarget { ip: s, port });
            }
            Message::PortEdited(s) => {
                self.port_buf = s.clone();
                if let Ok(port) = s.parse::<u16>() {
                    *self.data.params.target_port.lock() = port;
                    let ip = self.data.params.target_ip.lock().clone();
                    let _ = self.data.cmd_tx.try_send(NetworkCommand::UpdateTarget { ip, port });
                }
            }
            Message::SlotSelected(slot) => {
                *self.data.params.fx_slot.lock() = slot;
            }
            Message::ToggleSyncOnChange => {
                let setter = ParamSetter::new(self.context.as_ref());
                let v = self.data.params.sync_on_change.value();
                setter.begin_set_parameter(&self.data.params.sync_on_change);
                setter.set_parameter(&self.data.params.sync_on_change, !v);
                setter.end_set_parameter(&self.data.params.sync_on_change);
            }
            Message::ToggleSyncContinuous => {
                let setter = ParamSetter::new(self.context.as_ref());
                let v = self.data.params.sync_continuous.value();
                setter.begin_set_parameter(&self.data.params.sync_continuous);
                setter.set_parameter(&self.data.params.sync_continuous, !v);
                setter.end_set_parameter(&self.data.params.sync_continuous);
            }
            Message::SetHardResetAuto(v) => {
                let setter = ParamSetter::new(self.context.as_ref());
                setter.begin_set_parameter(&self.data.params.hard_reset_auto);
                setter.set_parameter(&self.data.params.hard_reset_auto, v);
                setter.end_set_parameter(&self.data.params.hard_reset_auto);
            }
            Message::ForceSync => {
                // Lock-free path: bypasses the param system.
                // The audio thread reads this in process() and fires immediately.
                self.data.force_sync_trigger.store(true, Ordering::Release);
            }
            Message::AuditSlots => {
                let _ = self.data.cmd_tx.try_send(NetworkCommand::AuditSlots);
            }
        }
        Command::none()
    }

    fn view(&mut self) -> Element<'_, Message> {
        // ── Read shared state ─────────────────────────────────────────────
        let connected = self.data.conn_status.load(Ordering::Relaxed);
        let now = now_ms();
        let tx_on = {
            let ts = self.data.tx_activity_ts.load(Ordering::Relaxed);
            ts > 0 && now.saturating_sub(ts) < PULSE_MS
        };
        let rx_on = {
            let ts = self.data.rx_activity_ts.load(Ordering::Relaxed);
            ts > 0 && now.saturating_sub(ts) < PULSE_MS
        };

        let host_bpm_f = f32::from_bits(self.data.host_bpm.load(Ordering::Relaxed));
        let host_float = osc::bpm_to_float(host_bpm_f as f64);
        let hw_float = f32::from_bits(self.data.hardware_float.load(Ordering::Relaxed));
        let hw_bpm = osc::float_to_bpm(hw_float);
        let has_hw_data = hw_float > 0.0001;
        let in_sync = has_hw_data && (host_float - hw_float).abs() < 0.001;

        let sync_on_change = self.data.params.sync_on_change.value();
        let sync_continuous = self.data.params.sync_continuous.value();
        let hard_reset_auto = self.data.params.hard_reset_auto.value();
        let current_slot = *self.data.params.fx_slot.lock();
        let compatible = self.data.compatible_slots.lock().clone();

        // ── Colour palette ────────────────────────────────────────────────
        let c_dim = Color::from_rgb8(100, 100, 115);
        let c_white = Color::from_rgb8(210, 210, 222);
        let c_green = Color::from_rgb8(75, 195, 85);
        let c_red = Color::from_rgb8(215, 65, 65);
        let c_amber = Color::from_rgb8(225, 180, 55);
        let c_teal = Color::from_rgb8(55, 185, 175);

        // ── Header ────────────────────────────────────────────────────────
        let conn_str = if connected { "● Connected" } else { "○  Offline " };
        let conn_color = if connected { c_green } else { c_red };
        let tx_str = if tx_on { "⚡" } else { "·" };
        let rx_str = if rx_on { "↓" } else { "·" };

        let header = Row::new()
            .push(Text::new("ETHERTAP").size(19).color(c_white))
            .push(Space::with_width(Length::Fill))
            .push(Text::new(conn_str).size(11).color(conn_color))
            .push(Space::with_width(Length::Units(8)))
            .push(Text::new(format!("{tx_str}TX")).size(11).color(c_amber))
            .push(Space::with_width(Length::Units(4)))
            .push(Text::new(format!("{rx_str}RX")).size(11).color(c_teal))
            .align_items(Alignment::Center);

        // ── Network config ────────────────────────────────────────────────
        let net_row = Row::new()
            .push(Text::new("Target  ").size(11).color(c_dim))
            .push(
                TextInput::new(&mut self.ip_state, "IP address", &self.ip_buf, Message::IpEdited)
                    .size(11)
                    .padding(4)
                    .width(Length::FillPortion(3)),
            )
            .push(Text::new("  :  ").size(11).color(c_dim))
            .push(
                TextInput::new(&mut self.port_state, "Port", &self.port_buf, Message::PortEdited)
                    .size(11)
                    .padding(4)
                    .width(Length::FillPortion(1)),
            )
            .align_items(Alignment::Center);

        // ── Slot selector ─────────────────────────────────────────────────
        let slot_row = self
            .slot_states
            .iter_mut()
            .zip(1u8..=8u8)
            .filter(|(_, slot)| compatible.is_empty() || compatible.contains(slot))
            .fold(
                Row::new()
                    .push(Text::new("FX  ").size(11).color(c_dim))
                    .spacing(4)
                    .align_items(Alignment::Center),
                |row, (state, slot)| {
                    let sel = slot == current_slot;
                    row.push(
                        Button::new(state, Text::new(slot.to_string()).size(10))
                            .on_press(Message::SlotSelected(slot))
                            .style(EtherBtn(if sel { BtnKind::Active } else { BtnKind::Idle }))
                            .padding([3, 7]),
                    )
                },
            );

        // ── Telemetry section ─────────────────────────────────────────────
        let tel_header = Text::new("── TELEMETRY ─────────────────────────────")
            .size(9)
            .color(c_dim);

        let host_line = Row::new()
            .push(Text::new("Host  ").size(11).color(c_dim))
            .push(
                Text::new(if host_bpm_f > 0.0 {
                    format!("{host_bpm_f:>7.2} BPM")
                } else {
                    "     --- BPM".to_owned()
                })
                .size(11)
                .color(c_white),
            )
            .push(Text::new("  →  ").size(11).color(c_dim))
            .push(
                Text::new(if host_bpm_f > 0.0 {
                    format!("{host_float:.4}")
                } else {
                    "------".to_owned()
                })
                .size(11)
                .color(c_white),
            )
            .align_items(Alignment::Center);

        let (hw_bpm_str, hw_float_str) = if has_hw_data {
            (format!("{:>7.2} BPM", hw_bpm), format!("{hw_float:.4}"))
        } else {
            ("     --- BPM".to_owned(), "------".to_owned())
        };
        let sync_led = if !has_hw_data {
            Text::new("  ○ NO DATA").size(11).color(c_dim)
        } else if in_sync {
            Text::new("  ● MATCH").size(11).color(c_green)
        } else {
            Text::new("  ● DRIFT").size(11).color(c_red)
        };

        let hw_line = Row::new()
            .push(Text::new("Mixer ").size(11).color(c_dim))
            .push(Text::new(hw_bpm_str).size(11).color(c_white))
            .push(Text::new("  ←  ").size(11).color(c_dim))
            .push(Text::new(hw_float_str).size(11).color(c_white))
            .push(sync_led)
            .align_items(Alignment::Center);

        // ── Sync mode ─────────────────────────────────────────────────────
        let sync_header = Text::new("── SYNC MODE ─────────────────────────────")
            .size(9)
            .color(c_dim);

        let sync_mode_row = Row::new()
            .push(
                Button::new(
                    &mut self.btn_sync_chg,
                    Text::new("Sync on Change").size(11).color(if sync_on_change {
                        Color::from_rgb8(100, 170, 255)
                    } else {
                        c_dim
                    }),
                )
                .on_press(Message::ToggleSyncOnChange)
                .style(EtherBtn(if sync_on_change { BtnKind::Active } else { BtnKind::Idle }))
                .padding([5, 10]),
            )
            .push(Space::with_width(Length::Units(8)))
            .push(
                Button::new(
                    &mut self.btn_sync_cont,
                    Text::new("Sync Continuous").size(11).color(if sync_continuous {
                        Color::from_rgb8(100, 170, 255)
                    } else {
                        c_dim
                    }),
                )
                .on_press(Message::ToggleSyncContinuous)
                .style(EtherBtn(if sync_continuous { BtnKind::Active } else { BtnKind::Idle }))
                .padding([5, 10]),
            )
            .align_items(Alignment::Center);

        // ── Hard Reset mode + Force ───────────────────────────────────────
        let reset_header = Text::new("── RESET MODE ────────────────────────────")
            .size(9)
            .color(c_dim);

        let reset_row = Row::new()
            .push(
                Button::new(
                    &mut self.btn_hr_manual,
                    Text::new("Manual Only").size(11).color(if !hard_reset_auto {
                        Color::from_rgb8(100, 170, 255)
                    } else {
                        c_dim
                    }),
                )
                .on_press(Message::SetHardResetAuto(false))
                .style(EtherBtn(if !hard_reset_auto { BtnKind::Active } else { BtnKind::Idle }))
                .padding([5, 10]),
            )
            .push(Space::with_width(Length::Units(6)))
            .push(
                Button::new(
                    &mut self.btn_hr_auto,
                    Text::new("Auto + Manual").size(11).color(if hard_reset_auto {
                        Color::from_rgb8(100, 170, 255)
                    } else {
                        c_dim
                    }),
                )
                .on_press(Message::SetHardResetAuto(true))
                .style(EtherBtn(if hard_reset_auto { BtnKind::Active } else { BtnKind::Idle }))
                .padding([5, 10]),
            )
            .push(Space::with_width(Length::Fill))
            .push(
                Button::new(
                    &mut self.btn_force,
                    Text::new("⚡ FORCE SYNC").size(11).color(Color::from_rgb8(240, 150, 60)),
                )
                .on_press(Message::ForceSync)
                .style(EtherBtn(BtnKind::Force))
                .padding([5, 14]),
            )
            .align_items(Alignment::Center);

        // ── Footer ────────────────────────────────────────────────────────
        let audit_hint = if compatible.is_empty() {
            "run Audit to filter compatible slots"
        } else {
            "showing Stereo Delay (DLY) slots only"
        };
        let footer = Row::new()
            .push(
                Button::new(&mut self.btn_audit, Text::new("Audit Slots").size(10).color(c_dim))
                    .on_press(Message::AuditSlots)
                    .style(EtherBtn(BtnKind::Idle))
                    .padding([3, 8]),
            )
            .push(Space::with_width(Length::Fill))
            .push(Text::new(audit_hint).size(10).color(c_dim))
            .align_items(Alignment::Center);

        // ── Assembly ──────────────────────────────────────────────────────
        let content = Column::new()
            .push(header)
            .push(Space::with_height(8.into()))
            .push(net_row)
            .push(Space::with_height(6.into()))
            .push(slot_row)
            .push(Space::with_height(6.into()))
            .push(tel_header)
            .push(Space::with_height(4.into()))
            .push(host_line)
            .push(Space::with_height(3.into()))
            .push(hw_line)
            .push(Space::with_height(6.into()))
            .push(sync_header)
            .push(Space::with_height(4.into()))
            .push(sync_mode_row)
            .push(Space::with_height(6.into()))
            .push(reset_header)
            .push(Space::with_height(4.into()))
            .push(reset_row)
            .push(Space::with_height(Length::Fill))
            .push(footer)
            .padding(16)
            .spacing(0);

        Container::new(content)
            .width(Length::Fill)
            .height(Length::Fill)
            .into()
    }

    fn background_color(&self) -> Color {
        Color::from_rgb8(15, 15, 21)
    }
}
