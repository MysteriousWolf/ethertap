# editor

## What it does

Iced-based GUI editor (360×280 px, fixed size) rendered on the GUI thread. Displays connection status, TX/RX/CK LED indicators, hardware telemetry (polled delay float displayed as BPM), sync mode controls, FX slot picker, MIDI device picker, MIDI clock stats row, and device scan results. Sends `NetworkCommand` directly to the network worker and reads shared atomics for display.

## CLI code

- `src/editor.rs` — `EtherTapEditor` struct implementing `IcedEditor`; `EditorData` input struct; theme/colour palette in `Theme::dark()`; icon codepoints for Solar Icon Set Bold (PUA U+E900+); `t!()` macro for monospace text elements

## Docs

- `CLAUDE.md` — nih-plug-iced API constraints (stateful widget API, `view(&mut self)`, `IcedState::from_size`, `Button::new(&mut self.state, ...)`)

## Coupling

- Reads `hardware_float`, `host_bpm`, `conn_status`, `tx_activity_ts`, `rx_activity_ts`, `midi_clock_activity_ts`, `midi_bridge_connected`, `midi_bridge_connecting`, `midi_clock_drop_count` atomics from `EtherTap` (core domain).
- Reads `midi_clock_stats: Arc<Mutex<ClockStats>>` from midi domain.
- Reads `compatible_slots`, `occupied_slots`, `slot_types`, `scan_targets`, `connected_device` mutexes from core.
- Sends `NetworkCommand` via `cmd_tx` on user button actions (connects to network domain).
- Reads `midi_device_rx` receiver for device list updates (from midi_watcher).
- Uses `EtherTapParams` and `SyncMode` from core params for picker state and persistence.

## Conventions worth knowing

- Uses the stateful nih-plug-iced API: each interactive widget (`Button`, `TextInput`, `PickList`) requires a corresponding `button::State` / `text_input::State` / `pick_list::State` field.
- All theme colours live in `Theme::dark()` at the bottom of the "Theme" section; change colours only there.
- `SOLAR_BOLD` font (Solar Icon Set Bold) used for icon glyphs; `MONO_FONT` (JetBrains Mono Regular) for all other text; `LOGO_FONT` (JetBrains Mono Bold) for the plugin name.
- LED pulse logic: LED is lit if `now_ms() - activity_ts < PULSE_DURATION_MS`.
- Window size: 360×280, non-resizable (`IcedState::from_size(360, 280)`).
- `MIDI_OUT_NONE = "— None —"` sentinel in device PickList for "no physical device".
