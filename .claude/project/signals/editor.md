# editor

## What it does

Iced-based GUI editor rendered on the GUI thread. Displays connection status, TX/RX/CK LED indicators, hardware telemetry (polled delay float displayed as BPM), sync mode controls, FX slot picker, MIDI device picker, MIDI clock stats row, and device scan results. With the `standalone` feature enabled the window expands to 500×310 and gains a transport panel (play/stop, BPM text input, tap tempo, beat position display) plus a DAW I/O side panel (transport readout, Rate/Phase sync buttons, status). Sends `NetworkCommand` directly to the network worker and reads shared atomics for display.

## CLI code

- `src/editor.rs` — `EtherTapEditor` struct implementing `IcedEditor`; `EditorData` input struct carrying all shared atomics including `standalone_bpm`, `standalone_playing`, `standalone_pos_beats`; theme/colour palette in `Theme::dark()`; icon codepoints for Solar Icon Set Bold (PUA U+E900+); `t!()` macro for monospace text elements; standalone DAW frame fields gated with `#[cfg_attr(not(feature = "standalone"), allow(dead_code))]`

## Docs

- `CLAUDE.md` — nih-plug-iced API constraints (stateful widget API, `view(&mut self)`, `IcedState::from_size`, `Button::new(&mut self.state, ...)`)

## Coupling

- Reads `hardware_float`, `host_bpm`, `conn_status`, `tx_activity_ts`, `rx_activity_ts`, `midi_clock_activity_ts`, `midi_bridge_connected`, `midi_bridge_connecting`, `midi_clock_drop_count` atomics from `EtherTap` (core domain).
- Reads `midi_clock_stats: Arc<Mutex<ClockStats>>` from midi domain.
- Reads `compatible_slots`, `occupied_slots`, `slot_types`, `scan_targets`, `connected_device` mutexes from core.
- Sends `NetworkCommand` via `cmd_tx` on user button actions (connects to network domain).
- Reads `midi_device_rx` receiver for device list updates (from midi_watcher).
- Uses `EtherTapParams` and `SyncMode` from core params for picker state and persistence.
- Writes `standalone_bpm` and `standalone_playing` atomics (read by `process()` in core); `standalone_pos_beats` is written by `process()` and read here for the position display.

## Conventions worth knowing

- Uses the stateful nih-plug-iced API: each interactive widget (`Button`, `TextInput`, `PickList`) requires a corresponding `button::State` / `text_input::State` / `pick_list::State` field.
- All standalone DAW frame fields in `EtherTapEditor` are present unconditionally but gated with `#[cfg_attr(not(feature = "standalone"), allow(dead_code))]`; view code using them is in `#[cfg(feature = "standalone")]` blocks.
- All theme colours live in `Theme::dark()` at the bottom of the "Theme" section; change colours only there.
- `SOLAR_BOLD` font (Solar Icon Set Bold) used for icon glyphs; `MONO_FONT` (JetBrains Mono Regular) for all other text; `LOGO_FONT` (JetBrains Mono Bold) for the plugin name.
- LED pulse logic: LED is lit if `now_ms() - activity_ts < PULSE_MS` (100 ms constant).
- Window size: 360×280 in VST3 mode; 500×310 with `standalone` feature (set in `src/params.rs`).
- `MIDI_OUT_NONE = "— None —"` sentinel in device PickList for "no physical device".
- Tap tempo: `tap_times: Vec<std::time::Instant>` accumulates taps; averaged interval sets `standalone_bpm`.
