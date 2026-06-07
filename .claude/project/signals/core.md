# core

## What it does

EtherTap is a zero-audio VST3 plugin (nih-plug, Rust) that bridges DAW BPM to Behringer X32 / Midas M32 delay times via UDP/OSC. The `process()` audio callback dispatches sync commands to the network worker via lock-free channels; it never allocates, blocks, or locks a contended mutex. Sync can fire on BPM settle (500 ms debounce), on every beat, or only on manual trigger. In standalone mode (no DAW host / no `pos_beats` from transport), `process()` falls back to three shared atomics (`standalone_bpm`, `standalone_playing`, `standalone_pos_beats`) driven by the editor transport panel rather than `context.transport()`.

## Artifacts

- `src/lib.rs` — plugin struct (`EtherTap`), `Plugin` impl, `process()` audio callback, BPM settle state machine, quantised Hard Reset scheduler, reconnect-sync logic, standalone transport fallback, phase-sync PS1–PS4 bar/time-sig/loop detection
- `src/params.rs` — `EtherTapParams` (VST3 param block), `SyncMode` enum, persisted config fields; window size conditional: 360×280 in VST3 mode, 500×340 with `standalone` feature

## CLI code

- `src/bin/gui_test.rs` — standalone binary entry point (requires `standalone` feature); wraps `nih_plug::nih_export_standalone`
- `xtask/src/main.rs` — thin xtask shim; delegates to `cargo nih-plug bundle`

## Docs

- `README.md` — install, connect, sync-mode table, BPM math, OSC reference, architecture diagram
- `CLAUDE.md` — RT-safety contract, inter-thread channel table, nih-plug-iced API constraints, OSC quick reference
- `PATCHES.md` — explains vendored baseview / nih-plug patches and why they exist

## Coupling

- Changes to `SyncMode` or param IDs in `src/params.rs` affect automation lanes in existing DAW sessions; also referenced by `src/editor.rs` for UI rendering and `src/lib.rs` for sync dispatch.
- Changes to `EtherTap` struct fields shared as `Arc<Atomic*>` require matching updates in `src/editor.rs` (reads) and `src/network.rs` / `src/midi_clock.rs` (writes).
- `standalone_bpm`, `standalone_playing`, `standalone_pos_beats` atomics are written by the editor transport panel and read by `process()`; adding new standalone transport state requires updating both.
- `process()` RT contract: any new code path in `process()` must not allocate or block; violation breaks RT safety across the entire plugin.

## Conventions worth knowing

- `f32` BPM and delay float are stored in atomics as `u32` bit patterns via `f32::to_bits` / `f32::from_bits`; `standalone_pos_beats` is `f64` stored as `u64` bits.
- BPM↔delay formula: `delay_float = 20.0 / bpm`; `bpm = 20.0 / delay_float` (normalised to 3000 ms range).
- `SETTLE_MS = 500` — BPM must be stable for 500 ms before OnChange fires.
- `BPM_SETTLE_THRESHOLD = 0.01` — minimum delta to restart settle timer. `BPM_MIDI_THRESHOLD = 0.5` — minimum delta to send `ClockMsg::BpmChanged`.
- Hard Reset sequence: mute (0) → 75 ms dwell → set delay → 75 ms dwell → unmute (1); deferred to next beat boundary for rhythmic masking.
- Standalone mode sentinel: `transport.pos_beats()` returning `None` is the reliable indicator (dummy backend sets `transport.tempo` but not position).
- Default params: target IP `192.168.1.100` (standalone: `127.0.0.1`), port `10023`, FX slot 1, rate sync = OnChange, phase sync = Manual.
- All VST3 params use `#[id]`; persisted-but-not-automatable fields use `#[persist]`.
