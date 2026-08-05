# editor

## What it does
- Renders the plugin GUI via `nice-plug-iced` (Elm-style API, iced 0.14): stateless widget functions, `EtherTapEditor::new/update/view/theme/subscription` driven by a `Message` enum and `Task<Message>` returns (`src/editor.rs:715-873`, `1094-2173`).
- Fixed 360×280 layout in VST3 mode (single `plugin_frame_style` container); an additional `#[cfg(feature = "standalone")]` DAW-shell chrome (transport row + dimensioned 360×280 frame + PARAMETERS IN/OUT footer) wraps the same view for the standalone test harness (`src/editor.rs:2153-2456`).
- Two full-window modal overlays replace the main layout in place: network scan-target picker (`src/editor.rs:1133-1240`) and MIDI output-device picker (`src/editor.rs:1246-1334`).

## CLI code
- `src/editor.rs` — entire editor domain: theme/palette, stylesheets, `EditorData` (shared cross-thread state bundle), `EtherTapEditor` (Elm state/update/view), standalone DAW shell and its footer/param-chip helpers.

## Docs
- `docs/design/daw-controls-overhaul.md` — describes a Pause/Stop split, a `\u{2669}` glyph fix, replacing a side `daw_panel` column with a full-width footer, a count-driven wrap/grid, and a MIDI auto-connect toggle. Verified against `src/editor.rs`: all of these are present in current source — `Message::StopStandalone` sets a one-shot `standalone_stop_trigger` atomic exactly as the doc's recommendation describes (`src/editor.rs:1056-1064`); the transport row has separate play/pause (`\u{25b6}`/`\u{2016}`) and stop (`\u{25a0}`) buttons with no `\u{2669}` glyph anywhere in the file; the `daw_shell` footer (`src/editor.rs:2382-2406`) is full-width, not a side column, and uses the `wrap_rows` chunking helper (`src/editor.rs:2571-2590`); `Message::ToggleMidiAutoConnect` and the `midi_auto_connect` param/button exist (`src/editor.rs:935-941, 1841-1862`). The doc file itself carries no "Implementation log"/shipped marker (unlike the other two docs below), so its shipped status is not recorded in the doc — only confirmable by reading the source, which was done here.
- `docs/spec/te-dark-reskin.md` — flat/hairline dark restyle spec; its own "Implementation log" section states shipped and merged into main. Verified against source: `Palette::dark()` (renamed from the doc's `Theme::dark()`, `src/editor.rs:171-232`) has no bevel-pair fields, `BtnKind` styles are single flat fill + hairline (`src/editor.rs:299-375`), `BORDER_RADIUS`/`BORDER_RADIUS_BTN` are split (`src/editor.rs:248-250`), and `PLUGIN_FRAME_PAD` exists (`src/editor.rs:268`, applied at `src/editor.rs:2167, 2414`) — matches the doc's described end state.
- `docs/spec/host-status-params.md` — read-only telemetry params (`sync_status`, `phase_reset_pending`, `hardware_bpm`, `compatible_slot_count`, `midi_bridge_connected`) surfaced in the standalone DAW-shell PARAMETERS OUT footer; its own "Implementation log" states shipped and merged. Verified against source: all five params are read and rendered as footer chips at `src/editor.rs:2364-2380`, with a `sync_status_label` helper mapping the `SyncStatus` enum (`src/editor.rs:2556-2564`).

## Coupling
- **core**: consumes `EtherTapParams`/`Ppq`/`SyncMode` (and, standalone-only, `SyncStatus`) from `src/params.rs`, and builds `EditorData` from a live `crate::EtherTap` plugin instance (`src/editor.rs:747-779`) — adding/removing a plugin param or shared atomic requires updating both `EditorData`'s fields and the `view()`/`update()` code that reads them.
- **network**: reads `DeviceInfo`/`NetworkCommand`/`ScanHealth` and sends `NetworkCommand` values over `cmd_tx` (scan trigger, target updates) — a change to `NetworkCommand`'s variants or `DeviceInfo`'s fields forces an editor-side update.
- **midi**: reads `crate::midi_clock::AtomicClockStats` for the jitter-stats row and `crate::midi_watcher::POLL_INTERVAL_SECS` for the MIDI-picker status string, and sends device-change notifications over `device_change_tx` — changes to either module's public surface propagate here.

## Conventions worth knowing
- All colors live in one `Palette::dark()` (aliased `PALETTE`, `src/editor.rs:171-234`); restyling means editing only that block, per the file's own top-of-file doc comment.
- `t!(expr)` macro (`src/editor.rs:69-73`) wraps every text element in `MONO_FONT`; icon glyphs come from Solar Icon Set Bold PUA codepoints centralized in `mod icon` (`src/editor.rs:77-85`).
- Fonts are resolved by `(family, weight, style)` against bytes registered via `.font()` on the `Application` builder (`src/editor.rs:715-741`) — the family name must match the font file's own baked-in name table, not an arbitrary label.
- `hgap`/`vgap` helpers (`src/editor.rs:275-280`) wrap `Space::new().width/height(...)` since iced 0.14 dropped `Space::with_width`/`with_height`.
- UI→audio parameter changes go through `NiceGuiContext::param_setter()` (`begin_set_parameter`/`set_parameter`/`end_set_parameter`); momentary triggers use the `pulse_param` helper (`src/editor.rs:2205-2210`).
- The `on_frame_stream` subscription (`src/editor.rs:702-713`) is a hand-rolled background thread + `futures::channel::mpsc` ticking every `TICK_MS` (30 ms), because nice-plug-iced's default `thread-pool` executor has no `iced::time::every`-style timer.
- Section frames (MIXER/EFFECTS/MIDI/SYNC) all go through one `section()` helper (`src/editor.rs:2181-2200`) for a consistent titled-border look.
- The standalone-only `wrap_rows` grid helper (`src/editor.rs:2571-2590`) is explicitly scoped to the DAW-shell footer only, not a general-purpose layout component.
